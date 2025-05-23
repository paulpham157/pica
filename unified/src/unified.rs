use crate::domain::{ResponseCrudToMapBuilder, ResponseCrudToMapRequest};
use crate::{
    algebra::jsruntime::JSRuntimeImpl,
    client::CallerClient,
    domain::{RequestCrud, ResponseCrud, UnifiedMetadata, UnifiedMetadataBuilder},
    helper::{match_route, template_route},
};
use bson::doc;
use cache::local::{
    ConnectionCache, ConnectionModelDefinitionCacheIdKey, ConnectionModelDefinitionCacheIdKeyInner,
    ConnectionModelDefinitionDestinationCache, ConnectionModelSchemaCache, LocalCacheExt,
    SecretCache,
};
use chrono::Utc;
use futures::{
    future::{join_all, OptionFuture},
    FutureExt,
};
use handlebars::Handlebars;
use http::{HeaderMap, HeaderName, HeaderValue, Response, StatusCode};
use mongodb::{
    options::{Collation, CollationStrength, FindOneOptions},
    Client,
};
use osentities::{
    algebra::JsonExt,
    api_model_config::{ModelPaths, RequestModelPaths},
    connection_model_definition::{ConnectionModelDefinition, CrudAction, PlatformInfo},
    connection_model_schema::ConnectionModelSchema,
    constant::*,
    database::DatabaseConfig,
    destination::{Action, Destination},
    environment::Environment,
    error::InternalError,
    hashed_secret::HashedSecret,
    id::{prefix::IdPrefix, Id},
    prelude::{MongoStore, TimedExt},
    ApplicationError, Connection, ErrorMeta, PicaError, Secret, SecretExt, Store,
};
use serde_json::{json, Number, Value};
use std::{collections::HashMap, str::FromStr, sync::Arc};
use tracing::error;

pub struct UnifiedResponse {
    pub response: Response<Value>,
    pub metadata: UnifiedMetadata,
}

pub struct SendToDestinationUnified {
    pub action: Action,
    pub passthrough: bool,
    pub headers: HeaderMap,
    pub query_params: HashMap<String, String>,
    pub body: Option<Value>,
}

#[derive(Clone)]
pub struct UnifiedDestination {
    pub connections_cache: ConnectionCache,
    pub connections_store: MongoStore<Connection>,
    pub connection_model_definitions_cache: ConnectionModelDefinitionDestinationCache,
    pub connection_model_definitions_store: MongoStore<ConnectionModelDefinition>,
    pub connection_model_schemas_cache: ConnectionModelSchemaCache,
    pub connection_model_schemas_store: MongoStore<ConnectionModelSchema>,
    pub secrets_client: Arc<dyn SecretExt + Sync + Send>,
    pub secrets_cache: SecretCache,
    pub http_client: reqwest::Client,
}

pub struct UnifiedCacheTTLs {
    pub connection_cache_ttl_secs: u64,
    pub connection_model_definition_cache_ttl_secs: u64,
    pub connection_model_schema_cache_ttl_secs: u64,
    pub secret_cache_ttl_secs: u64,
}

impl UnifiedDestination {
    pub async fn new(
        db_config: DatabaseConfig,
        cache_size: u64,
        secrets_client: Arc<dyn SecretExt + Sync + Send>,
        cache_ttls: UnifiedCacheTTLs,
    ) -> Result<Self, PicaError> {
        let http_client = reqwest::Client::new();
        let connections_cache =
            ConnectionCache::new(cache_size, cache_ttls.connection_cache_ttl_secs);
        let connection_model_definitions_cache = ConnectionModelDefinitionDestinationCache::new(
            cache_size,
            cache_ttls.connection_model_definition_cache_ttl_secs,
        );
        let connection_model_schemas_cache = ConnectionModelSchemaCache::new(
            cache_size,
            cache_ttls.connection_model_schema_cache_ttl_secs,
        );
        let secrets_cache = SecretCache::new(cache_size, cache_ttls.secret_cache_ttl_secs);

        let client = Client::with_uri_str(&db_config.control_db_url)
            .await
            .map_err(|e| {
                InternalError::connection_error(
                    &format!("Failed to create UnifiedDestination client: {e}"),
                    None,
                )
            })?;

        let db = client.database(&db_config.control_db_name);

        let connections_store = MongoStore::new(&db, &Store::Connections).await?;
        let connection_model_definitions_store =
            MongoStore::new(&db, &Store::ConnectionModelDefinitions).await?;
        let connection_model_schemas_store =
            MongoStore::new(&db, &Store::ConnectionModelSchemas).await?;

        Ok(Self {
            connections_cache,
            connections_store,
            connection_model_definitions_cache,
            connection_model_definitions_store,
            connection_model_schemas_cache,
            connection_model_schemas_store,
            secrets_client,
            secrets_cache,
            http_client,
        })
    }

    pub async fn get_connection_model_definition(
        &self,
        destination: &Destination,
        cmd_cache_id_key: ConnectionModelDefinitionCacheIdKey,
    ) -> Result<Option<ConnectionModelDefinition>, PicaError> {
        match &destination.action {
            Action::Passthrough { method, path, id } => match id {
                Some(id) => {
                    let connection_model_definition = cmd_cache_id_key
                        .get_or_insert_with_filter(
                            &ConnectionModelDefinitionCacheIdKeyInner { id: id.to_string() },
                            self.connection_model_definitions_store.clone(),
                            doc! {"_id": id.to_string()},
                            None,
                        )
                        .await?;

                    Ok(Some(connection_model_definition))
                }
                None => {
                    tracing::error!(
                        "No id provided for passthrough action. Destination: {:?}",
                        destination
                    );
                    let connection_model_definitions = self
                        .connection_model_definitions_store
                        .get_many(
                            Some(doc! {
                                "connectionPlatform": destination.platform.as_ref(),
                                "action": method.as_str(),
                                "supported": true
                            }),
                            None,
                            None,
                            None,
                            None,
                        )
                        .await?;

                    let routes =
                        connection_model_definitions
                            .iter()
                            .map(|c| match c.platform_info {
                                PlatformInfo::Api(ref c) => c.path.as_ref(),
                            });

                    let matched_route = match_route(path, routes.clone()).map(|r| r.to_string());

                    let connection_model_definitions: Vec<ConnectionModelDefinition> =
                        connection_model_definitions
                            .clone()
                            .into_iter()
                            .filter(|c| match c.platform_info {
                                PlatformInfo::Api(ref c) => matched_route
                                    .as_ref()
                                    .is_some_and(|mr| c.path.as_str() == mr),
                            })
                            .collect();

                    if connection_model_definitions.len() > 1 {
                        error!("Multiple connection model definitions found for this path. Destination: {:?}, Routes: {:?}", destination, routes);

                        return Err(InternalError::invalid_argument(
                            "Multiple connection model definitions found for this path",
                            None,
                        ));
                    }

                    Ok(connection_model_definitions.first().cloned())
                }
            },
            Action::Unified { name, action, .. } => Ok(self
                .connection_model_definitions_store
                .collection
                .find_one(doc! {
                    "connectionPlatform": destination.platform.as_ref(),
                    "mapping.commonModelName": name.as_ref(),
                    "actionName": action.to_string()
                })
                .with_options(
                    FindOneOptions::builder()
                        .collation(Some(
                            Collation::builder()
                                .strength(CollationStrength::Secondary)
                                .locale("en")
                                .build(),
                        ))
                        .build(),
                )
                .await?),
        }
    }

    pub async fn execute_model_definition_from_request(
        &self,
        config: &ConnectionModelDefinition,
        params: &RequestCrud,
        secret: &Value,
    ) -> Result<reqwest::Response, PicaError> {
        let context = match params.get_body() {
            None | Some(Value::Null) => None,
            _ => Some(serde_json::to_vec(&params.get_body()).map_err(|e| {
                error!(
                    "Failed to convert body to vec. ID: {}, Error: {}",
                    config.id, e
                );

                ApplicationError::bad_request(&e.to_string(), None)
            })?),
        };

        self.execute_model_definition(
            config,
            params.get_headers().to_owned(),
            params.get_query_params(),
            secret,
            context,
        )
        .await
    }

    pub async fn execute_model_definition(
        &self,
        config: &ConnectionModelDefinition,
        headers: HeaderMap,
        query_params: &HashMap<String, String>,
        secret: &Value,
        context: Option<Vec<u8>>,
    ) -> Result<reqwest::Response, PicaError> {
        let renderer = Handlebars::new();

        let config_str = serde_json::to_string(&config)
            .map_err(|e| InternalError::invalid_argument(&e.to_string(), None))?;

        let config = renderer
            .render_template(&config_str, secret)
            .map_err(|e| InternalError::invalid_argument(&e.to_string(), None))?;

        let config: ConnectionModelDefinition = serde_json::from_str(&config)
            .map_err(|e| InternalError::invalid_argument(&e.to_string(), None))?;

        match config.platform_info {
            PlatformInfo::Api(ref c) => {
                let api_caller = CallerClient::new(c, config.action, &self.http_client);

                let response = api_caller
                    .make_request(context, Some(secret), Some(headers), Some(query_params))
                    .await?;

                Ok(response)
            }
        }
    }

    pub async fn dispatch_unified_request(
        &self,
        connection: Arc<Connection>,
        action: Action,
        environment: Environment,
        params: RequestCrud,
        cache: ConnectionModelDefinitionCacheIdKey,
    ) -> Result<UnifiedResponse, PicaError> {
        let mut metadata = UnifiedMetadataBuilder::default();
        let metadata = metadata
            .timestamp(Utc::now().timestamp_millis())
            .platform_rate_limit_remaining(0)
            .rate_limit_remaining(0)
            .host(params.get_header("host"))
            .transaction_key(Id::now(IdPrefix::Transaction))
            .platform(connection.platform.to_string())
            .platform_version(connection.platform_version.to_string())
            .common_model_version("v1")
            .connection_key(connection.key.to_string());

        self.perform_unified_request(connection, action, environment, params, metadata, cache)
            .await
            .map_err(|e| match metadata.build().ok() {
                Some(metadata) => e.set_meta(&metadata.as_value()),
                None => e,
            })
    }

    async fn perform_unified_request(
        &self,
        connection: Arc<Connection>,
        action: Action,
        environment: Environment,
        params: RequestCrud,
        metadata: &mut UnifiedMetadataBuilder,
        cache: ConnectionModelDefinitionCacheIdKey,
    ) -> Result<UnifiedResponse, PicaError> {
        let key = Destination {
            platform: connection.platform.clone(),
            action: action.clone(),
            connection_key: connection.key.clone(),
        };

        match action {
            Action::Unified {
                name,
                action,
                id,
                passthrough: is_passthrough,
            } => {
                // (ConnectionModelDefinition, Secret, ConnectionModelSchema)
                let (config, secret, cms) = self.get_dependencies(&key, &connection, &name, cache).await.inspect_err(|e| {
                    error!("Failed to get dependencies for unified destination. Destination: {:?}, Error: {e}", key.platform);
                })?;
                tracing::info!("Dependencies retrieved for unified destination. Destination: {:?}, Config: {}, ConnectionModelSchema: {}", key.platform, config.id, cms.id);

                let metadata = metadata
                    .action(action.to_string())
                    .common_model(config.mapping.as_ref().map(|m| m.common_model_name.clone()).unwrap_or_default());

                let secret = insert_action_id(secret.as_value()?, id.as_ref());

                // Namespace for js scripts
                let jsruntime = JSRuntimeImpl;
                let crud_namespace = generate_script_namespace(self.secrets_cache.max_capacity(), &config.id.to_string());
                let schema_namespace = generate_script_namespace(self.secrets_cache.max_capacity(), &cms.id.to_string());

                let body = params.get_body();
                let body = match cms.mapping.as_ref().map(|m| m.from_common_model.as_str()) {
                    Some(code) if body.is_some() => {
                        let namespace = schema_namespace.clone() + "_mapFromCommonModel";

                        jsruntime.create("mapFromCommonModel", &namespace, code)?.run::<Option<&Value>, Option<Value>>(&body, &namespace).await?.map(|v| v.drop_nulls())
                    }
                    _ => body.cloned()
                };

                let default_params = params.clone();
                let request_crud: Option<Result<RequestCrud, PicaError>> = OptionFuture::from(config.mapping.as_ref().map(|m| m.from_common_model.to_owned())
                    .map(|code| async {
                        match code {
                            None => Ok(params),
                            Some(code) => {
                                let namespace = crud_namespace.clone() + "_mapFromCrudRequest";
                                let jsruntime = jsruntime.create("mapCrudRequest", &namespace, &code)?;

                                tracing::debug!("Code for mapping crud request ready for unified destination. Code: {code}, Namespace: {namespace}");

                                let payload = prepare_crud_mapping(params, &config, id.as_ref())?;

                                tracing::debug!("Request crud prepared for unified destination. RequestCrud: {:?}", payload);

                                let params: RequestCrud = jsruntime.run(&payload, &namespace).await?;
                                let params: RequestCrud = params.extend_body(body);

                                Ok(params)
                            }
                        }
                    })).await;

                tracing::debug!("Request crud prepared for unified destination. RequestCrud: {:?}", request_crud);

                let params: RequestCrud = request_crud.unwrap_or(Ok(default_params))?;
                let secret: Value = extend_secret(secret, params.get_path_params());

                let body: Option<Value> = insert_body_into_path_object(&config, params.get_body());
                let params: RequestCrud = params.set_body(body);

                tracing::debug!("Request crud prepared for unified destination. RequestCrud: {:?}", params);

                let response: reqwest::Response = self.execute_model_definition_from_request(&config, &params, &secret).timed(|_, duration| {
                    metadata.latency(duration.as_millis() as i32);
                }).await?;

                let status: StatusCode = response.status();
                let headers: HeaderMap = response.headers().clone();

                tracing::info!("Received response for unified destination. Status: {:?}", response.status());

                let error_for_status = if response.status().is_client_error() || response.status().is_server_error() {
                    Err(InternalError::invalid_argument(&format!("Invalid response status: {}", response.status()), None))
                } else {
                    Ok(())
                };

                let body: Result<Value, PicaError> = response.json().await.map_err(|e| {
                    error!("Failed to get json body from successful response. ID: {}, Error: {}", config.id, e);

                    PicaError::from_err_code(status, &e.to_string(), None)
                });

                let body: Option<Value> = match error_for_status {
                    Err(e) => {
                        error!("Failed to execute model definition. ID: {}, Error: {}", config.id, e);

                        let mut response = Response::builder()
                            .status(status)
                            .body(body?)
                            .map_err(|e| {
                                error!("Failed to create response from builder for unsuccessful response. ID: {}, Error: {}", config.id, e);

                                PicaError::from_err_code(status, &e.to_string(), None)
                            })?;
                        *response.headers_mut() = headers;
                        return Ok(UnifiedResponse { response, metadata: metadata.build()? });
                    }
                    Ok(_) => body.ok(),
                };

                tracing::debug!("Final mapped body for unified destination ready. Destination: {:?}, MappedBody: {body:?}", key);

                let passthrough: Option<Value> = if is_passthrough { body.clone() } else { None };
                let pagination: Option<Value> = match &config.action_name {
                    CrudAction::GetMany => {
                        match config.mapping.as_ref().and_then(|m| m.to_common_model.as_ref()) {
                            Some(code) => {
                                tracing::debug!("Code for mapping crud request ready for unified destination. Code: {code}, Namespace: {crud_namespace}");

                                let namespace = crud_namespace.clone() + "_mapToCrudRequest";
                                let jsruntime: JSRuntimeImpl = jsruntime.create("mapCrudRequest", &namespace, code).inspect_err(|e| {
                                    error!("Failed to create request crud mapping script for connection model. ID: {}, Error: {}", config.id, e);
                                })?;

                                let pagination = extract_pagination(&config, &body)?;
                                let res_to_map = ResponseCrudToMapBuilder::default()
                                    .headers(&headers)
                                    .pagination(pagination)
                                    .request(ResponseCrudToMapRequest::new(params.get_query_params()))
                                    .build()?;

                                let response: ResponseCrud = jsruntime.run(&res_to_map, &namespace).await?;

                                response.get_pagination().cloned()
                            }
                            _ => None
                        }
                    }
                    _ => None
                };

                let body = transform_response_with_path(&config, body, &environment);
                let body = match config.action_name {
                    CrudAction::GetMany | CrudAction::GetOne | CrudAction::Create | CrudAction::Upsert => {
                        match cms.mapping.as_ref().map(|m| m.to_common_model.as_str()) {
                            Some(code) => {
                                let namespace = schema_namespace.clone() + "_mapToCommonModel";

                                let jsruntime = jsruntime.create("mapToCommonModel", &namespace, code).inspect_err(|e| {
                                    error!("Failed to create request schema mapping script for connection model schema. ID: {}, Error: {}", config.id, e);
                                })?;

                                let mapped_body = match body {
                                    Ok(Some(Value::Array(arr))) => {
                                        let futures = arr.into_iter().map(|body| {
                                            let jsruntime = &jsruntime;
                                            let namespace = &namespace;
                                            async move {
                                                let mut response = jsruntime.run::<Value, Value>(&body, namespace).await.inspect_err(|e| {
                                                    error!("Failed to run request schema mapping script for connection model schema. ID: {}, Error: {}", config.id, e);
                                                })?.drop_nulls();

                                                if let Value::Object(map) = &mut response {
                                                    if !map.contains_key(MODIFY_TOKEN_KEY) {
                                                        let v = map.get(ID_KEY).cloned().unwrap_or_else(|| json!(""));
                                                        map.insert(MODIFY_TOKEN_KEY.to_owned(), v);
                                                    }
                                                    Ok::<_, PicaError>(Value::Object(map.clone()))
                                                } else {
                                                    Ok(response)
                                                }
                                            }
                                        });
                                        let values = join_all(futures)
                                            .await
                                            .into_iter()
                                            .collect::<Result<Vec<Value>, _>>()?;
                                        Ok(Value::Array(values))
                                    }
                                    Ok(Some(body)) => {
                                        Ok(jsruntime.run::<Value, Value>(&body, &namespace).await.inspect_err(|e| {
                                            error!("Failed to run request schema mapping script for connection model schema. ID: {}, Error: {}", config.id, e);
                                        })?.drop_nulls())
                                    }
                                    Ok(_) if config.action_name == CrudAction::GetMany => Ok(Value::Array(Default::default())),
                                    Err(e) => Err(e),
                                    _ => Ok(Value::Object(Default::default())),
                                };

                                mapped_body.map(Some)
                            }
                            None => Err(InternalError::invalid_argument(
                                &format!(
                                    "No js for schema mapping to common model {name} for {}. ID: {}",
                                    connection.platform, config.id
                                ),
                                None,
                            )
                            )
                        }
                    }
                    CrudAction::GetCount | CrudAction::Custom => body,
                    CrudAction::Update | CrudAction::Delete => Ok(None),
                }?;

                build_unified_response(config, metadata, is_passthrough)(body, pagination, passthrough, params, status, headers)
            }
            Action::Passthrough { method, path, .. } => Err(InternalError::invalid_argument(
                &format!("Passthrough action is not supported for destination {}, in method {method} and path {path}", key.connection_key),
                None,
            )),
        }
    }

    pub async fn dispatch_destination_request(
        &self,
        connection: Option<Arc<Connection>>,
        destination: &Destination,
        headers: HeaderMap,
        query_params: HashMap<String, String>,
        context: Option<Vec<u8>>,
        cache: ConnectionModelDefinitionCacheIdKey,
    ) -> Result<reqwest::Response, PicaError> {
        let connection = if let Some(connection) = connection {
            connection
        } else {
            Arc::new(
                self.connections_cache
                    .get_or_insert_with_filter(
                        &destination.connection_key,
                        self.connections_store.clone(),
                        doc! { "key": destination.connection_key.as_ref() },
                        None,
                    )
                    .await?,
            )
        };

        let config = match self
            .get_connection_model_definition(destination, cache)
            .await
        {
            Ok(Some(c)) => Ok(Arc::new(c)),
            Ok(None) => Err(InternalError::key_not_found(
                "ConnectionModelDefinition",
                None,
            )),
            Err(e) => Err(InternalError::connection_error(
                format!(
                    "Failed to get connection model definition: {}",
                    e.message().as_ref()
                )
                .as_str(),
                None,
            )),
        }?;

        if !config.supported {
            return Err(ApplicationError::not_found(
                "Supported Connection Model Definition",
                None,
            ));
        }

        let secret = self
            .secrets_cache
            .get_or_insert_with_fn(connection.as_ref(), || async {
                match self
                    .secrets_client
                    .get(&connection.secrets_service_id, &connection.ownership.id)
                    .map(|v| Some(v).transpose())
                    .await
                {
                    Ok(Some(c)) => Ok(c),
                    Ok(None) => Err(InternalError::key_not_found("Secrets", None)),
                    Err(e) => Err(InternalError::connection_error(
                        format!("Failed to get secret: {}", e.message().as_ref()).as_str(),
                        None,
                    )),
                }
            })
            .await?;

        // Template the route for passthrough actions
        let templated_config = match &destination.action {
            Action::Passthrough { path, .. } => {
                let mut config_clone = (*config).clone();
                let PlatformInfo::Api(ref mut c) = config_clone.platform_info;
                let template = template_route(c.path.clone(), path.to_string());
                c.path = template;
                config_clone.platform_info = PlatformInfo::Api(c.clone());
                Arc::new(config_clone)
            }
            _ => config.clone(),
        };

        self.execute_model_definition(
            &templated_config,
            headers,
            &query_params,
            &secret.as_value()?,
            context,
        )
        .await
    }

    async fn get_dependencies(
        &self,
        key: &Destination,
        connection: &Connection,
        name: &str,
        cache: ConnectionModelDefinitionCacheIdKey,
    ) -> Result<(ConnectionModelDefinition, Secret, ConnectionModelSchema), PicaError> {
        let config_fut = self
            .connection_model_definitions_cache
            .get_or_insert_with_fn(key, || async {
                match self.get_connection_model_definition(key, cache).await {
                    Ok(Some(c)) => Ok(c),
                    Ok(None) => Err(InternalError::key_not_found("model definition", None)),
                    Err(e) => Err(InternalError::connection_error(
                        format!(
                            "Failed to get connection model definition: {}",
                            e.message().as_ref()
                        )
                        .as_str(),
                        None,
                    )),
                }
            });

        let secret_fut = self
            .secrets_cache
            .get_or_insert_with_fn(connection, || async {
                match self
                    .secrets_client
                    .get(&connection.secrets_service_id, &connection.ownership.id)
                    .map(|v| Some(v).transpose())
                    .await
                {
                    Ok(Some(c)) => Ok(c),
                    Ok(None) => Err(InternalError::key_not_found("secret", None)),
                    Err(e) => Err(InternalError::connection_error(
                        format!("Failed to get secret: {}", e.message().as_ref()).as_str(),
                        None,
                    )),
                }
            });

        let schema_key: (Arc<str>, Arc<str>) = (connection.platform.clone(), name.into());

        let schema_fut = self
            .connection_model_schemas_cache
            .get_or_insert_with_filter(
                &schema_key,
                self.connection_model_schemas_store.clone(),
                doc! {
                    "connectionPlatform": connection.platform.as_ref(),
                    "mapping.commonModelName": name,
                },
                Some(
                    FindOneOptions::builder()
                        .collation(Some(
                            Collation::builder()
                                .strength(CollationStrength::Secondary)
                                .locale("en")
                                .build(),
                        ))
                        .build(),
                ),
            );

        let res = tokio::join!(config_fut, secret_fut, schema_fut);

        match res {
            (Ok(c), Ok(s), Ok(m)) => Ok((c, s, m)),
            (Err(e), _, _) => Err(e),
            (_, Err(e), _) => Err(e),
            (_, _, Err(e)) => Err(e),
        }
    }
}

fn build_unified_response(
    config: ConnectionModelDefinition,
    metadata: &mut UnifiedMetadataBuilder,
    is_passthrough: bool,
) -> impl FnOnce(
    Option<Value>,
    Option<Value>,
    Option<Value>,
    RequestCrud,
    StatusCode,
    HeaderMap,
) -> Result<UnifiedResponse, PicaError>
       + '_ {
    move |body, pagination, passthrough, params, status, headers| {
        let mut response = json!({});

        let response_len = match &body {
            Some(Value::Array(arr)) => arr.len(),
            _ => 0,
        };

        let hash = HashedSecret::try_from(json!({
            "response": &body,
            "action": config.action_name,
            "commonModel": config.mapping.as_ref().map(|m| &m.common_model_name),
        }))?;
        metadata.hash(hash.inner());

        if let Some(body) = body {
            if let Value::Object(ref mut resp) = response {
                if config.action_name == CrudAction::GetCount {
                    resp.insert(UNIFIED_KEY.to_string(), json!({ COUNT_KEY: body }));
                } else {
                    resp.insert(UNIFIED_KEY.to_string(), body);
                }
            }
        } else {
            tracing::info!("No response body to map for this action. ID: {}", config.id);
        }

        let mut builder = Response::builder();

        // Insert passthrough data if needed
        if is_passthrough {
            builder = builder.header(ACTION_KEY.to_string(), config.title.to_string());
            if let Some(passthrough) = passthrough {
                if let Value::Object(ref mut resp) = response {
                    resp.insert(PASSTHROUGH_KEY.to_string(), passthrough);
                }
            }
        }

        if let Some(Value::Object(pagination_obj)) = pagination {
            let mut pagination_obj = pagination_obj.clone(); // Clone the pagination data to modify it
                                                             // Add limit if available in the query params
            if let Some(Ok(limit)) = params
                .get_query_params()
                .get(LIMIT_KEY)
                .map(|s| s.parse::<u32>())
            {
                pagination_obj.insert(LIMIT_KEY.to_string(), Value::Number(Number::from(limit)));
            }
            pagination_obj.insert(
                PAGE_SIZE_KEY.to_string(),
                Value::Number(Number::from(response_len)),
            );

            if let Value::Object(ref mut resp) = response {
                resp.insert(PAGINATION_KEY.to_string(), Value::Object(pagination_obj));
            }
        }

        let metadata_value = metadata.build()?;
        if let Value::Object(ref mut resp) = response {
            resp.insert(META_KEY.to_string(), metadata_value.as_value().clone());
        }

        if status.is_success() {
            builder = builder
                .header::<&'static str, HeaderValue>(STATUS_HEADER_KEY, status.as_u16().into())
                .status(StatusCode::OK);
        } else {
            builder = builder.status(status);
        }

        if let Some(builder_headers) = builder.headers_mut() {
            builder_headers.extend(headers);
        } else {
            return Err(PicaError::from_err_code(
                status,
                "Could not get headers from builder",
                None,
            ));
        }

        let res = builder.body(response).map_err(|e| {
            error!(
                "Failed to create response from builder for successful response. Error: {}",
                e
            );
            PicaError::from_err_code(status, &e.to_string(), None)
        })?;

        Ok(UnifiedResponse {
            response: res,
            metadata: metadata.build()?,
        })
    }
}

fn transform_response_with_path(
    config: &ConnectionModelDefinition,
    model_definition_json: Option<Value>,
    environment: &Environment,
) -> Result<Option<Value>, PicaError> {
    let path = config
        .platform_info
        .config()
        .paths
        .as_ref()
        .and_then(|paths| paths.response.as_ref())
        .and_then(|response| response.object.as_ref());

    match path {
        None => Ok(model_definition_json),
        Some(path) => {
            let wrapped_body = json!({ BODY_KEY: model_definition_json });
            let mut bodies = jsonpath_lib::select(&wrapped_body, path).map_err(|e| {
                error!(
                    "Failed to select body at response path. ID {}, Path {}, Error {}",
                    config.id, path, e
                );

                ApplicationError::bad_request(&e.to_string(), None)
            })?;

            let is_returning_error = !environment.is_production()
                && matches!(config.action_name, CrudAction::GetMany | CrudAction::GetOne);
            let is_parseable_body = !bodies.is_empty() && bodies.len() == 1;

            if bodies.is_empty() && is_returning_error {
                let error_string = format!(
                    "Could not map unified model. 3rd party Connection returned an invalid response. Expected model at path {path} but found none.",
                );

                return Err(PicaError::from_err_code(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    &error_string,
                    None,
                ));
            }

            if bodies.len() != 1 && is_returning_error {
                return Err(InternalError::invalid_argument(
                    &format!(
                        "Invalid number of selected bodies ({}) at response path {} for CMD with ID: {}",
                        bodies.len(),
                        path,
                        config.id
                    ),
                    None,
                ));
            }

            if is_parseable_body {
                Ok(Some(bodies.remove(0).clone()))
            } else {
                Ok(None)
            }
        }
    }
}

fn extract_pagination(
    config: &ConnectionModelDefinition,
    body: &Option<Value>,
) -> Result<Option<Value>, PicaError> {
    let path = match config
        .platform_info
        .config()
        .paths
        .as_ref()
        .and_then(|paths| paths.response.as_ref())
        .and_then(|response| response.cursor.as_ref())
    {
        Some(p) => p,
        None => return Ok(None),
    };

    let body_value = match body {
        Some(b) => b,
        None => return Ok(None),
    };

    let wrapped_body = json!({ BODY_KEY: body_value });

    let mut bodies = jsonpath_lib::select(&wrapped_body, path).map_err(|e| {
        error!(
            "Failed to select cursor at response path. ID: {}, Path: {}, Error: {}",
            config.id, path, e
        );
        ApplicationError::bad_request(&e.to_string(), None)
    })?;

    Ok(if bodies.len() == 1 {
        Some(bodies.remove(0).clone())
    } else {
        Some(Value::Null)
    })
}

fn insert_body_into_path_object(
    config: &ConnectionModelDefinition,
    body: Option<&Value>,
) -> Option<Value> {
    match config.platform_info.config().paths.as_ref() {
        Some(ModelPaths {
            request: Some(RequestModelPaths { object: Some(path) }),
            ..
        }) => {
            if let Some(path) = path.strip_prefix("$.body.") {
                body.map(|body| json!({ path: body }))
            } else {
                body.cloned()
            }
        }
        _ => body.cloned(),
    }
}

fn extend_secret(mut secret: Value, get_path_params: Option<&HashMap<String, String>>) -> Value {
    if let Value::Object(sec) = &mut secret {
        if let Some(path_params) = get_path_params {
            sec.extend(
                path_params
                    .iter()
                    .map(|(lhs, rhs)| (lhs.to_string(), Value::String(rhs.to_string()))),
            );
        }
    }

    secret
}

fn generate_script_namespace(max_capacity: u64, key: &str) -> String {
    if max_capacity == 0 {
        "$".to_string() + &uuid::Uuid::new_v4().simple().to_string()
    } else {
        key.to_string().replace([':', '-'], "_")
    }
}

fn insert_action_id(secret: Value, id: Option<&Arc<str>>) -> Value {
    if let Value::Object(mut sec) = secret {
        if let Some(id) = id {
            sec.insert(ID_KEY.to_string(), Value::String(id.to_string()));
        }
        Value::Object(sec)
    } else {
        secret
    }
}

/// Prepares the CRUD (Create, Read, Update, Delete) mapping by modifying the request's
/// query parameters and headers based on user-defined and connection-specific configurations.
///
/// # Arguments
///
/// * `params` - A `RequestCrud` object containing the initial request parameters and headers.
/// * `config` - A reference to a `ConnectionModelDefinition` object that provides the connection-specific configurations.
///
/// # Returns
///
/// Returns a `Result` containing either:
/// - An updated `RequestCrud` object with modified query parameters and headers.
/// - An `PicaError` if an error occurs during processing.
fn prepare_crud_mapping(
    params: RequestCrud,
    config: &ConnectionModelDefinition,
    id: Option<&Arc<str>>,
) -> Result<RequestCrud, PicaError> {
    // Remove passthroughForward query param and add user-defined + connection-specific query params
    let (params, removed) = params.remove_query_params(PASSTHROUGH_PARAMS);
    let custom_query_params = removed
        .unwrap_or_default()
        .split('&')
        .filter_map(|pair| {
            pair.split_once('=')
                .map(|(a, b)| (a.to_owned(), b.to_owned()))
        })
        .collect::<HashMap<String, String>>();
    let params = params.extend_query_params(custom_query_params);

    // Remove passthroughHeaders query param and add user-defined + connection-specific headers
    let (params, removed) = params.remove_header(PASSTHROUGH_HEADERS);
    let custom_headers: HashMap<HeaderName, HeaderValue> = removed
        .map(|v| v.to_str().map(|s| s.to_string()))
        .map(|s| match s {
            Err(e) => {
                error!(
                    "Failed to convert custom headers to string. ID {:?}, Error: {:?}",
                    config.id, e
                );
                Err(InternalError::invalid_argument(&e.to_string(), None))
            }
            Ok(s) => Ok(s
                .split(';')
                .filter_map(|pair| pair.split_once('='))
                .filter_map(|(a, b)| {
                    match (HeaderName::from_str(a).ok(), HeaderValue::try_from(b).ok()) {
                        (Some(a), Some(b)) => Some((a, b)),
                        _ => None,
                    }
                })
                .collect::<HashMap<HeaderName, HeaderValue>>()),
        })
        .transpose()?
        .unwrap_or_default();

    Ok(params
        .extend_header(custom_headers)
        .add_path_param(ID_KEY.to_string(), id.as_ref().map(|id| id.to_string())))
}

use super::{delete, read, PublicExt, ReadResponse, RequestExt};
use crate::{
    helper::{shape_mongo_filter, DeploymentSpecParams, ServiceName, ServiceSpecParams},
    logic::event_access::get_client_throughput,
    router::ServerResponse,
    server::{AppState, AppStores},
};
use anyhow::{bail, Result};
use axum::{
    extract::{Path, Query, State},
    routing::{delete as axum_delete, get, patch, post},
    Extension, Json, Router,
};
use chrono::Utc;
use envconfig::Envconfig;
use http::HeaderMap;
use k8s_openapi::{
    api::core::v1::{ContainerPort, EnvVar, EnvVarSource, SecretKeySelector, ServicePort},
    apimachinery::pkg::util::intstr::IntOrString,
};
use mongodb::bson::doc;
use osentities::{
    algebra::MongoStore,
    connection_definition::{ConnectionDefinition, ConnectionDefinitionType},
    database::{DatabasePodConfig, PostgresConfig},
    database_secret::DatabaseConnectionSecret,
    domain::configuration::environment::Environment,
    domain::connection::SanitizedConnection,
    event_access::EventAccess,
    id::{prefix::IdPrefix, Id},
    record_metadata::RecordMetadata,
    settings::Settings,
    ApplicationError, Connection, ConnectionIdentityType, ConnectionType, InternalError, PicaError,
    Throughput, APP_LABEL, DATABASE_TYPE_LABEL, DEFAULT_NAMESPACE, JWT_SECRET_REF_KEY,
    JWT_SECRET_REF_NAME,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::Duration,
};
use tracing::error;
use uuid::Uuid;
use validator::Validate;

pub fn get_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", post(create_connection))
        .route("/", get(read::<CreateConnectionPayload, Connection>))
        .route("/:id", patch(update_connection))
        .route("/:id", axum_delete(delete_connection))
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, Validate)]
#[serde(rename_all = "camelCase")]
pub struct CreateConnectionPayload {
    pub connection_definition_id: Id,
    pub auth_form_data: HashMap<String, String>,
    pub active: bool,
    pub identity: Option<String>,
    pub identity_type: Option<ConnectionIdentityType>,
    pub group: Option<String>,
    pub name: Option<String>,
}

async fn test_connection(
    state: &AppState,
    connection_config: &ConnectionDefinition,
    auth_form_data_value: &Value,
) -> Result<()> {
    if let Some(ref test_connection_model_config_id) = connection_config.test_connection {
        let test_connection_model_config = state
            .app_stores
            .model_config
            .get_one_by_id(&test_connection_model_config_id.to_string())
            .await?;

        let test_connection_model_config = match test_connection_model_config {
            Some(config) => Arc::new(config),
            None => {
                return Err(anyhow::anyhow!(
                    "Test connection model config {} not found",
                    test_connection_model_config_id
                ))
            }
        };

        let context = test_connection_model_config
            .test_connection_payload
            .as_ref()
            .map(|test_payload| {
                serde_json::to_vec(test_payload).map_err(|e| {
                    error!(
                        "Failed to convert test_connection_payload to vec. ID: {}, Error: {}",
                        test_connection_model_config.id, e
                    );
                    anyhow::anyhow!("Failed to convert test_connection_payload: {}", e)
                })
            })
            .transpose()?;

        // Wait up to 10 seconds to allow the resource to be created
        if connection_config.r#type == ConnectionDefinitionType::DatabaseSql
            || connection_config.r#type == ConnectionDefinitionType::DatabaseNoSql
        {
            tokio::time::sleep(Duration::from_secs(
                state.config.database_connection_probe_timeout_secs,
            ))
            .await;
        }

        if let Some(delay) = connection_config.test_delay_in_millis {
            if delay < 0 {
                tracing::warn!("test_delay_in_millis is negative, skipping delay");
            } else {
                let delay = u16::try_from(delay).map_err(|e| {
                    error!("Error converting test_delay_in_millis to u64: {:?}", e);

                    InternalError::serialize_error(
                        "Unable to convert test_delay_in_millis to u64",
                        None,
                    )
                })?;

                tokio::time::sleep(Duration::from_millis(delay.into())).await
            }
        }

        let res = state
            .extractor_caller
            .execute_model_definition(
                &test_connection_model_config,
                HeaderMap::new(),
                &HashMap::new(),
                &Arc::new(auth_form_data_value.clone()),
                context,
            )
            .await?;

        if !res.status().is_success() {
            bail!("Test connections failed: {}", res.status());
        }
    }

    Ok(())
}

impl PublicExt<Connection> for CreateConnectionPayload {
    fn public(conn: Connection) -> Value {
        Into::<SanitizedConnection>::into(conn).to_value()
    }
}

impl RequestExt for CreateConnectionPayload {
    type Output = Connection;

    fn get_store(stores: AppStores) -> MongoStore<Self::Output> {
        stores.connection
    }
}

pub async fn create_connection(
    Extension(access): Extension<Arc<EventAccess>>,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<CreateConnectionPayload>,
) -> Result<Json<SanitizedConnection>, PicaError> {
    if let Err(validation_errors) = payload.validate() {
        return Err(ApplicationError::not_found(
            &format!("Invalid payload: {:?}", validation_errors),
            None,
        ));
    }

    if let Some(identity) = &payload.identity {
        if identity.len() > 128 {
            return Err(ApplicationError::bad_request(
                "Identity must not exceed 128 characters",
                None,
            ));
        }
    }

    let connection_config = match state
        .app_stores
        .connection_config
        .get_one(doc! {
            "_id": payload.connection_definition_id.to_string(),
            "deleted": false
        })
        .await
    {
        Ok(Some(data)) => data,
        Ok(None) => {
            return Err(ApplicationError::not_found(
                &format!(
                    "Connection definition with id {} not found",
                    payload.connection_definition_id
                ),
                None,
            ));
        }
        Err(e) => {
            error!(
                "Error fetching connection definition in connection create: {:?}",
                e
            );

            return Err(e);
        }
    };

    let uuid = Uuid::new_v4().to_string().replace('-', "");
    let group = payload.group.clone().unwrap_or_else(|| uuid.clone());
    let identity = payload.identity.clone().unwrap_or_else(|| group.clone());

    let key_suffix = if identity == uuid {
        uuid.clone()
    } else {
        format!("{}|{}", uuid, identity.replace(&[' ', ':'][..], "-"))
    };

    let key = format!(
        "{}::{}::{}::{}",
        access.environment, connection_config.platform, DEFAULT_NAMESPACE, key_suffix
    );

    let throughput = get_client_throughput(&access.ownership.id, &state).await?;

    let auth_form_data = serde_json::to_value(payload.auth_form_data.clone()).map_err(|e| {
        error!("Error serializing auth form data for connection: {:?}", e);

        ApplicationError::bad_request(&format!("Invalid auth form data: {:?}", e), None)
    })?;

    let connection_id = Id::new(IdPrefix::Connection, Utc::now());

    let (secret_value, service, deployment) =
        generate_k8s_specs_and_secret(&connection_id, &state, &connection_config, &auth_form_data)
            .await?;

    if let (Some(service), Some(deployment)) = (service.clone(), deployment.clone()) {
        state.k8s_client.coordinator(service, deployment).await?;
    }

    match test_connection(&state, &connection_config, &secret_value).await {
        Ok(result) => Ok(result),
        Err(e) => {
            error!(
            "Error executing model definition in connections create for connection testing: {:?}",
            e
        );

            if let (Some(service), Some(deployment)) = (service.as_ref(), deployment.as_ref()) {
                state
                    .k8s_client
                    .delete_all(deployment.namespace.clone(), service.name.clone())
                    .await?;
            }

            Err(ApplicationError::bad_request(
                &format!("Invalid connection credentials: {:?}", e),
                None,
            ))
        }
    }?;

    let secret_result = state
        .secrets_client
        .create(&secret_value, &access.ownership.id)
        .await
        .inspect_err(|e| {
            error!("Error creating secret for connection: {:?}", e);
        })?;

    let environment = access.environment;
    let ownership = access.ownership.clone();

    let conn = Connection {
        id: connection_id,
        platform_version: connection_config.clone().platform_version,
        connection_definition_id: payload.connection_definition_id,
        r#type: connection_config.to_connection_type(),
        key: key.clone().into(),
        group,
        identity: Some(identity.to_owned()),
        name: payload.name,
        has_error: false,
        error: None,
        identity_type: payload.identity_type,
        platform: connection_config.platform.into(),
        environment,
        secrets_service_id: secret_result.id(),
        event_access_id: None,
        access_key: None,
        settings: connection_config.settings,
        throughput: Throughput {
            key,
            limit: throughput,
        },
        ownership,
        oauth: None,
        record_metadata: RecordMetadata::default(),
    };

    state
        .app_stores
        .connection
        .create_one(&conn)
        .await
        .inspect_err(|e| {
            error!("Error creating connection: {:?}", e);
        })?;

    Ok(Json(conn.into()))
}

async fn generate_k8s_specs_and_secret(
    connection_id: &Id,
    state: &AppState,
    connection_config: &ConnectionDefinition,
    auth_form_data: &Value,
) -> Result<
    (
        Value,
        Option<ServiceSpecParams>,
        Option<DeploymentSpecParams>,
    ),
    PicaError,
> {
    Ok(match connection_config.to_connection_type() {
        osentities::ConnectionType::DatabaseSql {} => {
            let service_name = ServiceName::from_id(*connection_id)?;
            let namespace = state.config.namespace.clone();

            let mut labels: BTreeMap<String, String> = BTreeMap::new();
            labels.insert(APP_LABEL.to_owned(), service_name.as_ref().to_string());
            labels.insert(
                DATABASE_TYPE_LABEL.to_owned(),
                connection_config.platform.clone(),
            );

            let payload: HashMap<String, String> = serde_json::from_value(auth_form_data.clone())
                .map_err(|e| {
                error!("Error serializing auth form data for connection: {:?}", e);

                ApplicationError::bad_request(&format!("Invalid auth form data: {:?}", e), None)
            })?;

            let database_pod_config = DatabasePodConfig {
                worker_threads: Some(1),
                address: "0.0.0.0:5005".parse().map_err(|_| {
                    InternalError::serialize_error("Unable to convert address to SocketAddr", None)
                })?,
                environment: state.config.environment,
                connections_url: state.config.connections_url.clone(),
                database_connection_type: connection_config.platform.parse().map_err(|_| {
                    InternalError::serialize_error(
                        "Unable to convert database_connection_type to DatabaseConnectionType",
                        None,
                    )
                })?,
                connection_id: connection_id.to_string(),
                jwt_secret: None,
            };

            let secret = DatabaseConnectionSecret {
                service_name: service_name.to_string(),
                namespace: namespace.to_string(),
                connection_id: *connection_id,
                postgres_config: PostgresConfig::init_from_hashmap(&payload).map_err(|e| {
                    error!("Error initializing postgres config for connection: {:?}", e);

                    InternalError::serialize_error(
                        &format!("Unable to initialize postgres config: {:?}", e),
                        None,
                    )
                })?,
            };

            let service = ServiceSpecParams {
                ports: vec![ServicePort {
                    name: Some("http".to_owned()),
                    port: 80,
                    target_port: Some(IntOrString::Int(5005)), // Must match with  the
                    // container port and the one given in the INTERNAL_SERVER_ADDRESS
                    ..Default::default()
                }],
                r#type: "ClusterIP".into(),
                labels: labels.clone(),
                name: service_name.clone(),
                namespace: namespace.clone(),
            };

            let deployment = DeploymentSpecParams {
                replicas: 1,
                labels,
                namespace,
                image: state.config.database_connection_docker_image.clone(),
                env: {
                    let mut env = database_pod_config.as_hashmap().iter().fold(
                        vec![],
                        |mut vars, (key, value)| {
                            vars.push(EnvVar {
                                name: key.to_string(),
                                value: Some(value.to_string()),
                                ..Default::default()
                            });

                            vars
                        },
                    );

                    // JWT_SECRET
                    env.push(EnvVar {
                        name: "JWT_SECRET".to_string(),
                        value_from: Some(EnvVarSource {
                            secret_key_ref: Some(SecretKeySelector {
                                key: JWT_SECRET_REF_KEY.to_string(),
                                name: JWT_SECRET_REF_NAME.to_owned(),
                                optional: Some(false),
                            }),
                            ..Default::default()
                        }),
                        ..Default::default()
                    });

                    env
                },
                ports: vec![ContainerPort {
                    container_port: 5005,
                    ..ContainerPort::default()
                }],
                name: service_name,
            };

            let value = serde_json::to_value(secret).map_err(|e| {
                error!("Error serializing secret for connection: {:?}", e);
                InternalError::serialize_error("Could not serialize secret", None)
            })?;

            (value, Some(service), Some(deployment))
        }
        _ => (auth_form_data.clone(), None, None),
    })
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, Validate)]
#[serde(rename_all = "camelCase")]
pub struct UpdateConnectionPayload {
    pub settings: Option<Settings>,
    pub throughput: Option<Throughput>,
    pub auth_form_data: Option<HashMap<String, String>>,
    pub active: Option<bool>,
    pub identity: Option<String>,
    pub identity_type: Option<ConnectionIdentityType>,
}

pub async fn update_connection(
    Extension(event_access): Extension<Arc<EventAccess>>,
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(req): Json<UpdateConnectionPayload>,
) -> Result<Json<ServerResponse<Value>>, PicaError> {
    let Some(mut connection) = (match state.app_stores.connection.get_one_by_id(&id).await {
        Ok(connection) => connection,
        Err(e) => {
            error!("Error fetching connection for update: {:?}", e);

            return Err(e);
        }
    }) else {
        return Err(ApplicationError::not_found(
            &format!("Connection with id {id} not found"),
            None,
        ));
    };

    if connection.ownership != event_access.ownership
        || connection.environment != event_access.environment
    {
        return Err(ApplicationError::forbidden(
            "You do not have permission to update this connection",
            None,
        ));
    }

    if let Some(settings) = req.settings {
        connection.settings = settings;
    }

    if let Some(throughput) = req.throughput {
        connection.throughput = throughput;
    }

    if let Some(identity) = req.identity {
        connection.identity = Some(identity);
    }

    if let Some(identity_type) = req.identity_type {
        connection.identity_type = Some(identity_type);
    }

    if let Some(auth_form_data) = req.auth_form_data {
        let auth_form_data_value = serde_json::to_value(auth_form_data).map_err(|e| {
            error!(
                "Error serializing auth form data for connection update: {:?}",
                e
            );

            ApplicationError::bad_request(&format!("Invalid auth form data: {:?}", e), None)
        })?;

        let connection_config = match state
            .app_stores
            .connection_config
            .get_one(doc! {
                "_id": connection.connection_definition_id.to_string(),
                "deleted": false
            })
            .await
        {
            Ok(Some(data)) => data,
            Ok(None) => {
                return Err(ApplicationError::not_found(
                    "Connection definition not found",
                    None,
                ));
            }
            Err(e) => {
                error!(
                    "Error fetching connection definition in connection update: {:?}",
                    e
                );

                return Err(e);
            }
        };

        if connection_config.r#type == ConnectionDefinitionType::DatabaseSql {
            return Err(ApplicationError::bad_request(
                "Unsupported platform for SQL connection",
                None,
            ));
        }

        test_connection(&state, &connection_config, &auth_form_data_value)
            .await
            .map_err(|e| {
                error!("Error executing model definition in connections update for connection testing: {:?}", e);

                ApplicationError::bad_request(&format!("Invalid auth form data: {:?}", e), None)
            })?;

        let secret_result = state
            .secrets_client
            .create(&auth_form_data_value, &event_access.ownership.id)
            .await
            .map_err(|e| {
                error!("Error creating secret for connection update: {:?}", e);

                e
            })?;

        connection.secrets_service_id = secret_result.id();
    }

    if let Some(active) = req.active {
        connection.record_metadata.active = active;
    }

    let Ok(document) = bson::to_document(&connection) else {
        error!("Could not serialize connection into document");

        return Err(InternalError::serialize_error(
            "Could not serialize connection into document",
            None,
        ));
    };

    connection
        .record_metadata
        .mark_updated(&event_access.ownership.id);

    match state
        .app_stores
        .connection
        .update_one(
            &id,
            doc! {
                "$set": document
            },
        )
        .await
    {
        Ok(_) => Ok(Json(ServerResponse::new(
            "connection",
            json!({
                id: connection.id,
            }),
        ))),
        Err(e) => {
            error!("Error updating connection: {:?}", e);

            Err(e)
        }
    }
}

pub async fn delete_connection(
    Extension(access): Extension<Arc<EventAccess>>,
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<ServerResponse<Value>>, PicaError> {
    let connection = delete::<CreateConnectionPayload, Connection>(
        Some(Extension(access.clone())),
        Path(id.clone()),
        State(state.clone()),
    )
    .await?;

    match connection.args.r#type {
        ConnectionType::DatabaseSql { .. } if connection.args.record_metadata.active => {
            let service_name = ServiceName::from_id(connection.args.id)?;
            let namespace = state.config.namespace.clone();
            state.k8s_client.delete_all(namespace, service_name).await?;
        }
        _ => (),
    }

    Ok(Json(ServerResponse::new(
        "connection",
        json!({
            id: connection.args.id,
        }),
    )))
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VaultConnection {
    #[serde(rename = "_id")]
    pub id: Id,
    pub platform_version: String,
    pub name: Option<String>,
    pub r#type: ConnectionType,
    pub key: Arc<str>,
    pub environment: Environment,
    pub platform: Arc<str>,
    pub identity: Option<String>,
    pub identity_type: Option<ConnectionIdentityType>,
    pub description: String,
    #[serde(flatten, default)]
    pub record_metadata: RecordMetadata,
}

pub async fn get_vault_connections(
    Extension(access): Extension<Arc<EventAccess>>,
    headers: HeaderMap,
    query: Option<Query<BTreeMap<String, String>>>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<ServerResponse<ReadResponse<VaultConnection>>>, PicaError> {
    let mongo_query = shape_mongo_filter(query, Some(access), Some(headers));

    let connections = state
        .app_stores
        .connection
        .get_many(
            Some(mongo_query.filter.clone()),
            None,
            None,
            Some(mongo_query.limit),
            Some(mongo_query.skip),
        )
        .await
        .map_err(|e| {
            error!("Error fetching connections: {:?}", e);
            e
        })?;

    let mut vault_connections = Vec::new();

    for connection in connections {
        let connection_definition = match state
            .app_stores
            .connection_config
            .get_one_by_id(&connection.connection_definition_id.to_string())
            .await
        {
            Ok(Some(definition)) => definition,
            Ok(None) => {
                // Skip connections with missing definitions
                error!(
                    "Connection definition with id {} not found",
                    connection.connection_definition_id
                );
                continue;
            }
            Err(e) => {
                error!("Error fetching connection definition: {:?}", e);
                continue;
            }
        };

        let description = connection_definition.frontend.spec.description;

        let vault_connection = VaultConnection {
            id: connection.id,
            platform_version: connection.platform_version,
            name: connection.name,
            r#type: connection.r#type,
            key: connection.key,
            environment: connection.environment,
            platform: connection.platform,
            identity: connection.identity,
            identity_type: connection.identity_type,
            description,
            record_metadata: connection.record_metadata,
        };

        vault_connections.push(vault_connection);
    }

    let total = state
        .app_stores
        .connection
        .count(mongo_query.filter, None)
        .await
        .map_err(|e| {
            error!("Error counting connections: {:?}", e);
            e
        })?;

    let response = ReadResponse {
        rows: vault_connections,
        skip: mongo_query.skip,
        limit: mongo_query.limit,
        total,
    };

    Ok(Json(ServerResponse::new("Vault Connections", response)))
}

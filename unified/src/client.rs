use derive_builder::Builder;
use http::HeaderMap;
use indexmap::IndexMap;
use osentities::{
    api_model_config::{ApiModelConfig, AuthMethod, OAuthLegacyHashAlgorithm},
    oauth_secret::OAuthLegacySecret,
    prelude::oauth_secret::OAuthSecret,
    AuthorizationType, InternalError, Nonce, OAuthData, PicaError, SignableRequest,
    SignatureMethod, SigningKey,
};
use reqwest::{Client, Response, Url};
use serde_json::Value;
use std::collections::HashMap;
use std::error::Error;

#[derive(Debug, Clone, Builder)]
pub struct CallerClient<'a> {
    config: &'a ApiModelConfig,
    action: http::Method,
    client: &'a Client,
}

impl<'a> CallerClient<'a> {
    pub fn new(config: &'a ApiModelConfig, action: http::Method, client: &'a Client) -> Self {
        CallerClient {
            config,
            action,
            client,
        }
    }

    pub async fn make_request(
        &self,
        payload: Option<Vec<u8>>,
        secret: Option<&Value>,
        headers: Option<HeaderMap>,
        query_params: Option<&HashMap<String, String>>,
    ) -> Result<Response, PicaError> {
        let endpoint = if self.config.base_url.ends_with('/') || self.config.path.starts_with('/') {
            format!("{}{}", self.config.base_url, self.config.path)
        } else {
            format!("{}/{}", self.config.base_url, self.config.path)
        };

        let mut request_builder = self.client.request(self.action.clone(), &endpoint);

        let mut merged_headers = headers.unwrap_or_default();

        if let Some(model_headers) = &self.config.headers {
            merged_headers.extend(model_headers.clone());
        }

        merged_headers.remove(http::header::CONTENT_LENGTH);
        merged_headers.remove(http::header::ACCEPT_ENCODING);
        merged_headers.remove(http::header::HOST);

        for (key, value) in merged_headers.iter() {
            request_builder = request_builder.header(key, value);
        }

        if let Some(model_query_params) = &self.config.query_params {
            request_builder = request_builder.query(model_query_params);
        }

        if let Some(custom_query_params) = query_params {
            request_builder = request_builder.query(custom_query_params);
        }

        if let Some(payload) = payload {
            request_builder = request_builder.body(payload);
        }

        request_builder = match &self.config.auth_method {
            AuthMethod::BearerToken { value } => request_builder.bearer_auth(value),
            AuthMethod::ApiKey { key, value } => request_builder.header(key, value),
            AuthMethod::QueryParam { key, value } => request_builder.query(&[(key, value)]),
            AuthMethod::BasicAuth { username, password } => {
                request_builder.basic_auth(username, Some(password))
            }
            AuthMethod::OAuthLegacy {
                hash_algorithm,
                realm,
            } => {
                let secret = serde_json::from_value::<OAuthLegacySecret>(
                    secret.cloned().unwrap_or_default(),
                )
                .map_err(|e| {
                    InternalError::invalid_argument(&e.to_string(), Some("oauth_secret"))
                })?;

                let signature_method = match hash_algorithm {
                    OAuthLegacyHashAlgorithm::HmacSha1 => SignatureMethod::HmacSha1,
                    OAuthLegacyHashAlgorithm::HmacSha256 => SignatureMethod::HmacSha256,
                    OAuthLegacyHashAlgorithm::HmacSha512 => SignatureMethod::HmacSha512,
                    OAuthLegacyHashAlgorithm::PlainText => SignatureMethod::PlainText,
                };

                let nonce = Nonce::generate()?;

                let oauth_data = OAuthData {
                    client_id: secret.consumer_key,
                    token: Some(secret.access_token_id),
                    signature_method,
                    nonce,
                };

                let key = SigningKey {
                    client_secret: secret.consumer_secret,
                    token_secret: Some(secret.access_token_secret),
                };

                let uri = Url::parse(&endpoint).map_err(|e| {
                    InternalError::invalid_argument(&e.to_string(), Some("endpoint"))
                })?;

                let mut signable_request_params = IndexMap::new();
                if let Some(custom_query_params) = query_params {
                    signable_request_params.extend(custom_query_params.clone());
                }
                if let Some(model_query_params) = &self.config.query_params {
                    signable_request_params.extend(model_query_params.clone());
                }

                let signable_request = SignableRequest {
                    method: self.action.clone(),
                    uri,
                    parameters: signable_request_params,
                };

                let authorization_header = oauth_data.authorization(
                    signable_request,
                    AuthorizationType::Request,
                    &key,
                    realm.clone(),
                )?;

                request_builder.header(http::header::AUTHORIZATION, authorization_header)
            }

            AuthMethod::OAuth => {
                // convert secret into OAuthSecret
                let secret =
                    serde_json::from_value::<OAuthSecret>(secret.cloned().unwrap_or_default())
                        .map_err(|e| {
                            InternalError::invalid_argument(&e.to_string(), Some("oauth_secret"))
                        })?;

                request_builder.header(
                    http::header::AUTHORIZATION,
                    format!(
                        "{} {}",
                        secret.token_type.unwrap_or("Bearer".into()),
                        secret.access_token
                    ),
                )
            }
            AuthMethod::None => request_builder,
        };

        let res = request_builder.send().await.map_err(|e| {
            tracing::error!("Failed to send request: {}", e.source().unwrap_or(&e));
            InternalError::io_err(
                &format!("Failed to send request: {}", e),
                Some("reqwest::Error"),
            )
        })?;

        Ok(res)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::StatusCode;
    use mockito::Server;
    use osentities::{
        api_model_config::{SamplesInput, SchemasInput},
        connection_model_definition::{
            ConnectionModelDefinition, CrudAction, PlatformInfo, TestConnection,
        },
        id::Id,
    };
    use reqwest::Client;
    use std::str::FromStr;

    #[tokio::test]
    async fn test_success_make_request() {
        let mut mock_server = Server::new_async().await;

        mock_server
            .mock("GET", "/api/customers/cus_OT8j94jEraNXbW")
            .with_status(200)
            .with_body("{\"id\": \"cus_OT8j94jEraNXbW\"}")
            .create_async()
            .await;

        let api_model_config = ApiModelConfig {
            base_url: mock_server.url() + "/api",
            path: "customers/cus_OT8j94jEraNXbW".to_string(),
            auth_method: AuthMethod::BearerToken {
                value: "sample-key".to_string(),
            },
            headers: None,
            query_params: None,
            content: None,
            schemas: SchemasInput {
                headers: None,
                query_params: None,
                path_params: None,
                body: None,
            },
            samples: SamplesInput {
                headers: None,
                query_params: None,
                path_params: None,
                body: None,
            },
            responses: vec![],
            paths: None,
        };

        let stripe_model_config = ConnectionModelDefinition {
            id: Id::from_str("conn_mod_def::AAAAAAAAAAA::AAAAAAAAAAAAAAAAAAAAAA").unwrap(),
            platform_version: "2023-08-16".to_string(),
            connection_platform: "stripe".to_string(),
            connection_definition_id: Id::from_str("conn::AAAAAAAAAAA::AAAAAAAAAAAAAAAAAAAAAA")
                .unwrap(),
            title: "Get Customers".to_string(),
            name: "Get Customer".to_string(),
            model_name: "Customer".to_string(),
            key: "api::stripe::v1::customer::getOne::get_customer".to_string(),
            platform_info: PlatformInfo::Api(api_model_config.clone()),
            action: http::Method::GET,
            action_name: CrudAction::GetMany,
            extractor_config: None,
            test_connection_status: TestConnection::default(),
            test_connection_payload: None,
            record_metadata: Default::default(),
            is_default_crud_mapping: None,
            mapping: None,
            supported: true,
            knowledge: None,
        };

        let client = Client::new();
        let single_api_caller =
            CallerClient::new(&api_model_config, stripe_model_config.action, &client);

        let res = single_api_caller
            .make_request(None, None, None, None)
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);
        let response = res.bytes().await.unwrap();
        assert_eq!(
            response,
            "{\"id\": \"cus_OT8j94jEraNXbW\"}".as_bytes().to_vec()
        );
    }

    #[tokio::test]
    async fn test_failed_make_request() {
        let mut mock_server = Server::new_async().await;

        mock_server
            .mock("GET", "/api/customers/cus_OT8j94jEraNXbW")
            .with_status(404)
            .with_body("Not found")
            .create_async()
            .await;

        let api_model_config = ApiModelConfig {
            base_url: mock_server.url() + "/api",
            path: "customers/cus_OT8j94jEraNXbW".to_string(),
            auth_method: AuthMethod::BearerToken {
                value: "sample-key".to_string(),
            },
            headers: None,
            content: None,
            query_params: None,
            schemas: SchemasInput {
                headers: None,
                query_params: None,
                path_params: None,
                body: None,
            },
            samples: SamplesInput {
                headers: None,
                query_params: None,
                path_params: None,
                body: None,
            },
            responses: vec![],
            paths: None,
        };

        let stripe_model_config = ConnectionModelDefinition {
            id: Id::from_str("conn_mod_def::AAAAAAAAAAA::AAAAAAAAAAAAAAAAAAAAAA").unwrap(),
            platform_version: "2023-08-16".to_string(),
            connection_platform: "stripe".to_string(),
            connection_definition_id: Id::from_str("conn::AAAAAAAAAAA::AAAAAAAAAAAAAAAAAAAAAA")
                .unwrap(),
            title: "Get Customers".to_string(),
            name: "Get Customer".to_string(),
            key: "api::stripe::v1::customer::getOne::get_customer".to_string(),
            model_name: "Customer".to_string(),
            platform_info: PlatformInfo::Api(api_model_config.clone()),
            action: http::Method::GET,
            action_name: CrudAction::GetMany,
            extractor_config: None,
            test_connection_status: TestConnection::default(),
            test_connection_payload: None,
            record_metadata: Default::default(),
            is_default_crud_mapping: None,
            mapping: None,
            supported: true,
            knowledge: None,
        };

        let client = Client::new();
        let single_api_caller =
            CallerClient::new(&api_model_config, stripe_model_config.action, &client);

        let res = single_api_caller
            .make_request(None, None, None, None)
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let response = res.bytes().await.unwrap();
        assert_eq!(response, "Not found".as_bytes().to_vec());
    }
}

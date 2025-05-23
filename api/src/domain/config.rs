use envconfig::Envconfig;
use osentities::{cache::CacheConfig, environment::Environment};
use osentities::{database::DatabaseConfig, secrets::SecretsConfig};
use std::{
    fmt::{Display, Formatter, Result},
    net::SocketAddr,
};
use strum::{AsRefStr, EnumString};

#[derive(Envconfig, Clone)]
pub struct ConnectionsConfig {
    #[envconfig(from = "WORKER_THREADS")]
    pub worker_threads: Option<usize>,
    #[envconfig(from = "INTERNAL_SERVER_ADDRESS", default = "0.0.0.0:3005")]
    pub address: SocketAddr,
    #[envconfig(from = "CACHE_SIZE", default = "100")]
    pub cache_size: u64,
    #[envconfig(from = "ACCESS_KEY_CACHE_TTL_SECS", default = "1800")]
    pub access_key_cache_ttl_secs: u64,
    #[envconfig(from = "ACCESS_KEY_WHITELIST_REFRESH_INTERVAL_SECS", default = "60")]
    pub access_key_whitelist_refresh_interval_secs: u64,
    #[envconfig(from = "CONNECTION_CACHE_TTL_SECS", default = "120")]
    pub connection_cache_ttl_secs: u64,
    #[envconfig(from = "ENGINEERING_ACCOUNT_ID", default = "engineering_account")]
    pub engineering_account_id: String,
    #[envconfig(from = "CONNECTION_DEFINITION_CACHE_TTL_SECS", default = "86400")]
    pub connection_definition_cache_ttl_secs: u64,
    #[envconfig(from = "CONNECTION_OAUTH_DEFINITION_CACHE_TTL_SECS", default = "86400")]
    pub connection_oauth_definition_cache_ttl_secs: u64,
    #[envconfig(from = "CONNECTION_MODEL_SCHEMA_TTL_SECS", default = "86400")]
    pub connection_model_schema_cache_ttl_secs: u64,
    #[envconfig(from = "CONNECTION_MODEL_DEFINITION_CACHE_TTL_SECS", default = "86400")]
    pub connection_model_definition_cache_ttl_secs: u64,
    #[envconfig(from = "SECRET_CACHE_TTL_SECS", default = "300")]
    pub secret_cache_ttl_secs: u64,
    #[envconfig(
        from = "EVENT_ACCESS_PASSWORD",
        default = "32KFFT_i4UpkJmyPwY2TGzgHpxfXs7zS"
    )]
    pub event_access_password: String,
    #[envconfig(from = "EVENT_ACCESS_THROUGHPUT", default = "500")]
    pub event_access_throughput: u64,
    #[envconfig(from = "EVENT_SAVE_BUFFER_SIZE", default = "2048")]
    pub event_save_buffer_size: usize,
    #[envconfig(from = "EVENT_SAVE_TIMEOUT_SECS", default = "30")]
    pub event_save_timeout_secs: u64,
    #[envconfig(from = "METRIC_SAVE_CHANNEL_SIZE", default = "2048")]
    pub metric_save_channel_size: usize,
    #[envconfig(from = "METRIC_SYSTEM_ID", default = "Pica-Internal-System")]
    pub metric_system_id: String,
    #[envconfig(from = "POSTHOG_WRITE_KEY")]
    pub posthog_write_key: Option<String>,
    #[envconfig(from = "POSTHOG_ENDPOINT")]
    pub posthog_endpoint: Option<String>,
    #[envconfig(nested = true)]
    pub secrets_config: SecretsConfig,
    #[envconfig(
        from = "JWT_SECRET",
        default = "2thZ2UiOnsibmFtZSI6IlN0YXJ0dXBsa3NoamRma3NqZGhma3NqZGhma3NqZG5jhYtggfaP9ubmVjdGlvbnMiOjUwMDAwMCwibW9kdWxlcyI6NSwiZW5kcG9pbnRzIjo3b4e05e2-f050-401f-9822-44f43f71753c"
    )]
    /// This is the admin secret for the API. Be sure this value is not the one use to generate
    /// tokens for the users as it gives access to sensitive admin endpoints.
    pub jwt_secret: String,
    #[envconfig(from = "CONNECTIONS_URL", default = "http://localhost:3005")]
    /// Same as self url, but this may vary in a k8s environment hence it's a separate config
    pub connections_url: String,
    #[envconfig(from = "OAUTH_URL", default = "http://platform-oauth")]
    /// Same as oauth url, but this may vary in a k8s environment hence it's a separate config
    pub oauth_url: String,
    /// Burst size limit
    #[envconfig(from = "API_VERSION", default = "v1")]
    pub api_version: String,
    #[envconfig(from = "HTTP_CLIENT_TIMEOUT_SECS", default = "30")]
    pub http_client_timeout_secs: u64,
    #[envconfig(nested = true)]
    pub headers: Headers,
    #[envconfig(nested = true)]
    pub db_config: DatabaseConfig,
    #[envconfig(nested = true)]
    pub cache_config: CacheConfig,
    #[envconfig(from = "RATE_LIMIT_ENABLED", default = "true")]
    pub rate_limit_enabled: bool,
    #[envconfig(from = "ENVIRONMENT", default = "development")]
    pub environment: Environment,
    #[envconfig(from = "DATABASE_CONNECTION_DOCKER_IMAGE", default = "pica-database")]
    pub database_connection_docker_image: String,
    #[envconfig(from = "NAMESPACE", default = "development")]
    pub namespace: String,
    #[envconfig(from = "DATABASE_CONNECTION_PROBE_TIMEOUT_SECS", default = "10")]
    pub database_connection_probe_timeout_secs: u64,
    #[envconfig(from = "K8S_MODE", default = "logger")]
    pub k8s_mode: K8sMode,
    #[envconfig(from = "OTLP_ENDPOINT")]
    pub otlp_endpoint: Option<String>,
}

impl Display for ConnectionsConfig {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        writeln!(f, "WORKER_THREADS: {:?}", self.worker_threads)?;
        writeln!(f, "INTERNAL_SERVER_ADDRESS: {}", self.address)?;
        writeln!(f, "CACHE_SIZE: {}", self.cache_size)?;
        writeln!(
            f,
            "ACCESS_KEY_CACHE_TTL_SECS: {}",
            self.access_key_cache_ttl_secs
        )?;
        writeln!(
            f,
            "ACCESS_KEY_WHITELIST_REFRESH_INTERVAL_SECS: {}",
            self.access_key_whitelist_refresh_interval_secs
        )?;
        writeln!(f, "EVENT_ACCESS_PASSWORD: ***")?;
        writeln!(
            f,
            "EVENT_ACCESS_THROUGHPUT: {}",
            self.event_access_throughput
        )?;
        writeln!(f, "EVENT_SAVE_BUFFER_SIZE: {}", self.event_save_buffer_size)?;
        writeln!(
            f,
            "CONNECTION_CACHE_TTL_SECS: {}",
            self.connection_cache_ttl_secs
        )?;
        writeln!(f, "CONNECTIONS_URL: {}", self.connections_url)?;
        writeln!(
            f,
            "CONNECTION_DEFINITION_CACHE_TTL_SECS: {}",
            self.connection_definition_cache_ttl_secs
        )?;
        writeln!(
            f,
            "CONNECTION_OAUTH_DEFINITION_CACHE_TTL_SECS: {}",
            self.connection_oauth_definition_cache_ttl_secs
        )?;
        writeln!(
            f,
            "EVENT_SAVE_TIMEOUT_SECS: {}",
            self.event_save_timeout_secs
        )?;
        writeln!(
            f,
            "METRIC_SAVE_CHANNEL_SIZE: {}",
            self.metric_save_channel_size
        )?;
        writeln!(f, "OTLP_ENDPOINT: ***")?;
        writeln!(f, "METRIC_SYSTEM_ID: {}", self.metric_system_id)?;
        writeln!(f, "POSTHOG_WRITE_KEY: ***")?;
        writeln!(f, "JWT_SECRET: ***")?;
        write!(f, "{}", self.secrets_config)?;
        writeln!(f, "API_VERSION: {}", self.api_version)?;
        writeln!(f, "{}", self.headers)?;
        writeln!(f, "{}", self.db_config)?;
        writeln!(f, "{}", self.cache_config)?;
        writeln!(f, "RATE_LIMIT_ENABLED: {}", self.rate_limit_enabled)?;
        writeln!(f, "ENVIRONMENT: {}", self.environment)?;
        writeln!(
            f,
            "DATABASE_CONNECTION_DOCKER_IMAGE: {}",
            self.database_connection_docker_image
        )?;
        writeln!(f, "NAMESPACE: {}", self.namespace)
    }
}

#[derive(Envconfig, Default, Clone)]
pub struct Headers {
    #[envconfig(from = "HEADER_AUTH", default = "x-pica-secret")]
    pub auth_header: String,
    #[envconfig(from = "HEADER_CONNECTION", default = "x-pica-connection-key")]
    pub connection_header: String,
    #[envconfig(
        from = "HEADER_ENABLE_PASSTHROUGH",
        default = "x-pica-enable-passthrough"
    )]
    pub enable_passthrough_header: String,
    #[envconfig(from = "HEADER_RATE_LIMIT_LIMIT", default = "x-pica-rate-limit-limit")]
    pub rate_limit_limit: String,
    #[envconfig(
        from = "HEADER_RATE_LIMIT_REMAINING",
        default = "x-pica-rate-limit-remainings"
    )]
    pub rate_limit_remaining: String,
    #[envconfig(from = "HEADER_RATE_LIMIT_REST", default = "x-pica-rate-limit-reset")]
    pub rate_limit_reset: String,
}

impl Headers {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Display for Headers {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        writeln!(f, "HEADER_AUTH: {}", self.auth_header)?;
        writeln!(f, "HEADER_CONNECTION: {}", self.connection_header)?;
        writeln!(
            f,
            "HEADER_INCLUDE_PASSTHROUGH: {}",
            self.enable_passthrough_header
        )?;
        writeln!(f, "HEADER_RATE_LIMIT_LIMIT: {}", self.rate_limit_limit)?;
        writeln!(
            f,
            "HEADER_RATE_LIMIT_REMAINING: {}",
            self.rate_limit_remaining
        )?;
        writeln!(f, "HEADER_RATE_LIMIT_RESET: {}", self.rate_limit_reset)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, AsRefStr)]
#[strum(serialize_all = "kebab-case")]
pub enum K8sMode {
    Real,
    Logger,
}

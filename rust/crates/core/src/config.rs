use std::{collections::HashMap, env, error::Error, fmt, path::Path};

use serde::{Deserialize, Serialize};

/// Default Memoria base URL. Uses `127.0.0.1` instead of `localhost` because
/// Memoria binds to `0.0.0.0` (IPv4 only) and `localhost` may resolve to `::1`
/// on dual-stack systems, causing connection failures.
pub(crate) const DEFAULT_MEMORIA_URL: &str = "http://127.0.0.1:8100";

/// Default max connections for the shared DB pool.
/// Sized for 50 concurrent runs + sweepers + HTTP handlers + WS overhead.
/// Override with `ASTRA_DB_POOL_MAX_CONNECTIONS`.
pub(crate) const DEFAULT_DB_POOL_MAX_CONNECTIONS: u32 = 80;

/// Default min idle connections for the shared DB pool.
/// Override with `ASTRA_DB_POOL_MIN_CONNECTIONS`.
pub(crate) const DEFAULT_DB_POOL_MIN_CONNECTIONS: u32 = 1;

/// Default acquire timeout for the shared DB pool (seconds).
/// Override with `ASTRA_DB_POOL_ACQUIRE_TIMEOUT_SECS`.
pub(crate) const DEFAULT_DB_POOL_ACQUIRE_TIMEOUT_SECS: u64 = 5;

/// Default idle timeout for the shared DB pool (seconds).
/// Override with `ASTRA_DB_POOL_IDLE_TIMEOUT_SECS`.
pub(crate) const DEFAULT_DB_POOL_IDLE_TIMEOUT_SECS: u64 = 60;

/// Default max lifetime for connections in the shared DB pool (seconds).
/// Override with `ASTRA_DB_POOL_MAX_LIFETIME_SECS`.
pub(crate) const DEFAULT_DB_POOL_MAX_LIFETIME_SECS: u64 = 300;

use crate::runtime_limits::{
    DEFAULT_GLOBAL_OUTPUT_LIMIT, DEFAULT_MAX_RETRIEVED, DEFAULT_MAX_TOOL_RETRIES,
    DEFAULT_MAX_TURN_INPUT_TOKENS, DEFAULT_MAX_TURNS, DEFAULT_PLAN_SUBTASK_MAX_TURNS,
    DEFAULT_RETRY_BASE_MS, DEFAULT_TOOL_OUTPUT_LIMIT, DEFAULT_TURN_TIMEOUT_S,
};

/// Read an env var and apply it to an `Option<T>` field if the value parses.
fn apply_env_override<T: std::str::FromStr>(field: &mut Option<T>, env_var: &str) {
    if let Ok(v) = env::var(env_var) {
        match v.parse() {
            Ok(parsed) => *field = Some(parsed),
            Err(_) => {
                tracing::warn!(%v, env_var, "env var parse failed, using default");
            }
        }
    }
}

/// Read an env var and apply it to an `Option<String>` field (no parsing).
fn apply_env_override_str(field: &mut Option<String>, env_var: &str) {
    if let Ok(v) = env::var(env_var) {
        *field = Some(v);
    }
}

// ─── Server Configuration (TOML) ─────────────────────────────────────────────

/// Top-level server configuration loaded from TOML files.
///
/// Provides defaults for operational parameters; environment variables
/// override TOML values. Load via [`ServerConfig::load()`] or parse
/// directly with [`ServerConfig::parse()`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ServerConfig {
    /// Database connection pool settings.
    pub database: DatabaseConfig,
    /// Authentication and JWT configuration.
    pub auth: AuthConfig,
    /// HTTP API server settings.
    pub api: ApiConfig,
    /// Runtime limits and tuning parameters.
    pub runtime: ServerRuntimeConfig,
    /// Deployment profile and tool capability controls.
    pub deployment: DeploymentConfig,
}

/// Deployment-level tool capability controls.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct DeploymentConfig {
    /// Tools disabled at deployment time (checked before dispatch).
    pub disabled_tools: Vec<String>,
}

impl DeploymentConfig {
    fn merge_from(&mut self, other: &Self) {
        if !other.disabled_tools.is_empty() {
            self.disabled_tools = other.disabled_tools.clone();
        }
    }

    /// Apply environment variable overrides for deployment-level settings.
    pub(crate) fn apply_env_overrides(&mut self) {
        if let Ok(val) = std::env::var("ASTRA_DISABLED_TOOLS") {
            let tools: Vec<String> = val
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if !tools.is_empty() {
                self.disabled_tools = tools;
            }
        }
    }
}

/// Database connection pool configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct DatabaseConfig {
    /// Maximum number of connections in the pool.
    pub max_connections: Option<u32>,
    /// Minimum number of idle connections to maintain.
    pub min_connections: Option<u32>,
    /// Connection acquisition timeout in seconds.
    pub connect_timeout_s: Option<u64>,
    /// Maximum lifetime of a connection before recycling (seconds).
    pub max_lifetime_s: Option<u64>,
    /// Idle timeout before closing unused connections (seconds).
    pub idle_timeout_s: Option<u64>,
}

/// Merge `Option<Copy>` fields from `other` into `self`, taking the first `Some`.
macro_rules! merge_option_copy_fields {
    ($self:ident, $other:ident, $($field:ident),+ $(,)?) => {
        $(
            if let Some(v) = $other.$field {
                $self.$field = Some(v);
            }
        )+
    };
}

/// Merge `Option<Clone>` fields from `other` into `self`, cloning the values.
macro_rules! merge_option_clone_fields {
    ($self:ident, $other:ident, $($field:ident),+ $(,)?) => {
        $(
            if let Some(v) = $other.$field.clone() {
                $self.$field = Some(v);
            }
        )+
    };
}

impl DatabaseConfig {
    pub(crate) fn max_connections(&self) -> u32 {
        self.max_connections
            .unwrap_or(DEFAULT_DB_POOL_MAX_CONNECTIONS)
    }
    pub(crate) fn min_connections(&self) -> u32 {
        self.min_connections
            .unwrap_or(DEFAULT_DB_POOL_MIN_CONNECTIONS)
    }
    pub(crate) fn connect_timeout_s(&self) -> u64 {
        self.connect_timeout_s
            .unwrap_or(DEFAULT_DB_POOL_ACQUIRE_TIMEOUT_SECS)
    }
    pub(crate) fn max_lifetime_s(&self) -> u64 {
        self.max_lifetime_s
            .unwrap_or(DEFAULT_DB_POOL_MAX_LIFETIME_SECS)
    }
    pub(crate) fn idle_timeout_s(&self) -> u64 {
        self.idle_timeout_s
            .unwrap_or(DEFAULT_DB_POOL_IDLE_TIMEOUT_SECS)
    }

    /// Merge non-`None` fields from `other` into `self`.
    fn merge_from(&mut self, other: &Self) {
        merge_option_copy_fields!(
            self,
            other,
            max_connections,
            min_connections,
            connect_timeout_s,
            max_lifetime_s,
            idle_timeout_s
        );
    }

    /// Apply env-var overrides to this section.
    fn apply_env_overrides(&mut self) {
        apply_env_override(&mut self.max_connections, "ASTRA_DB_POOL_MAX_CONNECTIONS");
        apply_env_override(&mut self.min_connections, "ASTRA_DB_POOL_MIN_CONNECTIONS");
        apply_env_override(
            &mut self.connect_timeout_s,
            "ASTRA_DB_POOL_ACQUIRE_TIMEOUT_SECS",
        );
        apply_env_override(&mut self.idle_timeout_s, "ASTRA_DB_POOL_IDLE_TIMEOUT_SECS");
        apply_env_override(&mut self.max_lifetime_s, "ASTRA_DB_POOL_MAX_LIFETIME_SECS");
    }
    /// Validate pool configuration values.
    ///
    /// Returns `Err` with a human-readable message if any pool parameter is
    /// out of range:
    /// - `max_connections` must be ≥ 1
    /// - `min_connections` must be ≤ `max_connections`
    /// - `connect_timeout_s`, `idle_timeout_s`, `max_lifetime_s` must be > 0
    pub(crate) fn validate(&self) -> Result<(), String> {
        let max = self.max_connections();
        if max == 0 {
            return Err("max_connections must be ≥ 1 (got 0)".into());
        }
        let min = self.min_connections();
        if min > max {
            return Err(format!(
                "min_connections ({min}) must be ≤ max_connections ({max})"
            ));
        }
        if self.connect_timeout_s() == 0 {
            return Err("connect_timeout_s must be > 0".into());
        }
        if self.idle_timeout_s() == 0 {
            return Err("idle_timeout_s must be > 0".into());
        }
        if self.max_lifetime_s() == 0 {
            return Err("max_lifetime_s must be > 0".into());
        }
        Ok(())
    }
}

/// Authentication and JWT configuration.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct AuthConfig {
    /// JWT signing secret (required in production).
    pub jwt_secret: Option<String>,
    /// JWT algorithm: HS256, HS384, HS512.
    pub jwt_algorithm: Option<String>,
    /// Access token TTL in minutes.
    pub access_ttl_minutes: Option<u64>,
    /// Refresh token TTL in days.
    pub refresh_ttl_days: Option<u64>,
    /// Bridge HMAC secret (required in production).
    pub bridge_secret: Option<String>,
    /// Authentication mode: local_jwt, trusted_moi.
    pub auth_mode: Option<String>,
    /// Fernet key for encrypting LLM API keys.
    pub token_encryption_key: Option<String>,
}

impl fmt::Debug for AuthConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthConfig")
            .field(
                "jwt_secret",
                &self.jwt_secret.as_ref().map(|_| "[REDACTED]"),
            )
            .field("jwt_algorithm", &self.jwt_algorithm)
            .field("access_ttl_minutes", &self.access_ttl_minutes)
            .field("refresh_ttl_days", &self.refresh_ttl_days)
            .field(
                "bridge_secret",
                &self.bridge_secret.as_ref().map(|_| "[REDACTED]"),
            )
            .field("auth_mode", &self.auth_mode)
            .field(
                "token_encryption_key",
                &self.token_encryption_key.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

impl AuthConfig {
    fn merge_from(&mut self, other: &Self) {
        merge_option_clone_fields!(
            self,
            other,
            jwt_secret,
            jwt_algorithm,
            bridge_secret,
            auth_mode,
            token_encryption_key
        );
        merge_option_copy_fields!(self, other, access_ttl_minutes, refresh_ttl_days);
    }
    fn apply_env_overrides(&mut self) {
        apply_env_override_str(&mut self.jwt_secret, "ASTRA_JWT_SECRET");
        apply_env_override_str(&mut self.jwt_algorithm, "ASTRA_JWT_ALGORITHM");
        apply_env_override(&mut self.access_ttl_minutes, "ASTRA_JWT_ACCESS_TTL_MINUTES");
        apply_env_override(&mut self.refresh_ttl_days, "ASTRA_JWT_REFRESH_TTL_DAYS");
        apply_env_override_str(&mut self.bridge_secret, "ASTRA_BRIDGE_SECRET");
        apply_env_override_str(&mut self.auth_mode, "ASTRA_AUTH_MODE");
        apply_env_override_str(&mut self.token_encryption_key, "ASTRA_TOKEN_ENCRYPTION_KEY");
    }
}

/// HTTP API server settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ApiConfig {
    /// Listen address.
    pub host: Option<String>,
    /// Listen port.
    pub port: Option<u16>,
    /// Comma-separated CORS origins (empty = no restriction).
    pub cors_origins: Option<Vec<String>>,
}

impl ApiConfig {
    pub fn host(&self) -> &str {
        self.host.as_deref().unwrap_or("0.0.0.0")
    }
    pub fn port(&self) -> u16 {
        self.port.unwrap_or(8000)
    }
    pub fn cors_origins(&self) -> &[String] {
        self.cors_origins.as_deref().unwrap_or(&[])
    }
}

impl ApiConfig {
    fn merge_from(&mut self, other: &Self) {
        merge_option_clone_fields!(self, other, host, cors_origins);
        merge_option_copy_fields!(self, other, port);
    }
    fn apply_env_overrides(&mut self) {
        apply_env_override_str(&mut self.host, "ASTRA_API_HOST");
        apply_env_override(&mut self.port, "ASTRA_API_PORT");
        // CORS origins: comma-separated string → Vec<String>
        if let Ok(v) = env::var("ASTRA_CORS_ORIGINS") {
            self.cors_origins = Some(v.split(',').map(|s| s.trim().to_string()).collect());
        }
    }
}

/// Runtime limits and tuning parameters.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ServerRuntimeConfig {
    /// Maximum conversation turns per session.
    pub max_turns: Option<usize>,
    /// Per-subtask turn budget for plan execution (0 = use max_turns).
    pub plan_subtask_max_turns: Option<usize>,
    /// Per-turn hard timeout in seconds.
    pub turn_timeout_s: Option<u64>,
    /// Combined tool output truncation limit (bytes).
    pub global_output_limit: Option<usize>,
    /// Per-tool output truncation limit (bytes).
    pub tool_output_limit: Option<usize>,
    /// Maximum transient-error retries per tool invocation.
    pub max_tool_retries: Option<usize>,
    /// Base backoff delay for tool retries (milliseconds).
    pub retry_base_ms: Option<u64>,
    /// Maximum memory/knowledge-base documents retrieved per turn.
    pub max_retrieved: Option<usize>,
    /// Maximum LLM input tokens per turn before forcing wrap-up (0 = unlimited).
    pub max_turn_input_tokens: Option<u64>,
}

impl ServerRuntimeConfig {
    pub(crate) fn max_turns(&self) -> usize {
        self.max_turns.unwrap_or(DEFAULT_MAX_TURNS)
    }
    pub(crate) fn plan_subtask_max_turns(&self) -> usize {
        self.plan_subtask_max_turns
            .unwrap_or(DEFAULT_PLAN_SUBTASK_MAX_TURNS)
    }
    pub(crate) fn turn_timeout_s(&self) -> u64 {
        self.turn_timeout_s.unwrap_or(DEFAULT_TURN_TIMEOUT_S)
    }
    pub(crate) fn global_output_limit(&self) -> usize {
        self.global_output_limit
            .unwrap_or(DEFAULT_GLOBAL_OUTPUT_LIMIT)
    }
    pub(crate) fn tool_output_limit(&self) -> usize {
        self.tool_output_limit.unwrap_or(DEFAULT_TOOL_OUTPUT_LIMIT)
    }
    pub(crate) fn max_tool_retries(&self) -> usize {
        self.max_tool_retries.unwrap_or(DEFAULT_MAX_TOOL_RETRIES)
    }
    pub(crate) fn retry_base_ms(&self) -> u64 {
        self.retry_base_ms.unwrap_or(DEFAULT_RETRY_BASE_MS)
    }
    pub(crate) fn max_retrieved(&self) -> usize {
        self.max_retrieved.unwrap_or(DEFAULT_MAX_RETRIEVED)
    }
    pub(crate) fn max_turn_input_tokens(&self) -> u64 {
        self.max_turn_input_tokens
            .unwrap_or(DEFAULT_MAX_TURN_INPUT_TOKENS)
    }

    /// Validate runtime configuration values.
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.max_turns == Some(0) {
            return Err("runtime.max_turns must be > 0 (or omit for default)".into());
        }
        if self.plan_subtask_max_turns == Some(0) {
            return Err("runtime.plan_subtask_max_turns must be > 0 (or omit for default)".into());
        }
        if self.turn_timeout_s == Some(0) {
            return Err("runtime.turn_timeout_s must be > 0 (or omit for default)".into());
        }
        if self.global_output_limit == Some(0) {
            return Err("runtime.global_output_limit must be > 0 (or omit for default)".into());
        }
        if self.tool_output_limit == Some(0) {
            return Err("runtime.tool_output_limit must be > 0 (or omit for default)".into());
        }
        if self.retry_base_ms == Some(0) {
            return Err("runtime.retry_base_ms must be > 0 (or omit for default)".into());
        }
        // Use resolved values so the check fires even when one field
        // relies on its default (max_turns=None + plan=100 → 100 > 50).
        let max_turns = self.max_turns();
        let plan_turns = self.plan_subtask_max_turns();
        if plan_turns > max_turns {
            return Err(format!(
                "runtime.plan_subtask_max_turns ({plan_turns}) exceeds max_turns ({max_turns})"
            ));
        }
        Ok(())
    }

    fn merge_from(&mut self, other: &Self) {
        merge_option_copy_fields!(
            self,
            other,
            max_turns,
            plan_subtask_max_turns,
            turn_timeout_s,
            global_output_limit,
            tool_output_limit,
            max_tool_retries,
            retry_base_ms,
            max_retrieved,
            max_turn_input_tokens
        );
    }

    fn apply_env_overrides(&mut self) {
        apply_env_override(&mut self.max_turns, "ASTRA_MAX_TURNS");
        apply_env_override(
            &mut self.plan_subtask_max_turns,
            "ASTRA_PLAN_SUBTASK_MAX_TURNS",
        );
        apply_env_override(&mut self.turn_timeout_s, "ASTRA_TURN_TIMEOUT_S");
        apply_env_override(&mut self.global_output_limit, "ASTRA_GLOBAL_OUTPUT_LIMIT");
        apply_env_override(&mut self.tool_output_limit, "ASTRA_TOOL_OUTPUT_LIMIT");
        apply_env_override(&mut self.max_tool_retries, "ASTRA_MAX_TOOL_RETRIES");
        apply_env_override(&mut self.retry_base_ms, "ASTRA_RETRY_BASE_MS");
        apply_env_override(&mut self.max_retrieved, "ASTRA_MAX_RETRIEVED");
        apply_env_override(
            &mut self.max_turn_input_tokens,
            "ASTRA_MAX_TURN_INPUT_TOKENS",
        );
    }
}

impl ServerConfig {
    /// Load configuration from standard locations.
    ///
    /// Checks `/etc/astra/server.toml` (system-level), then
    /// `~/.astra/server.toml` (user-level, for dev/self-hosted).
    /// User overrides system. Environment variables take highest precedence.
    /// Returns defaults if no file exists.
    pub fn load() -> Result<Self, ConfigError> {
        let mut config = Self::default();

        // System-level: /etc/astra/server.toml
        let system_path = Path::new("/etc/astra/server.toml");
        if system_path.exists() {
            match Self::from_file(system_path) {
                Ok(system_config) => config.merge(system_config),
                Err(e) => {
                    tracing::warn!(
                        "Failed to load {path}: {err}; continuing with defaults",
                        path = system_path.display(),
                        err = e
                    );
                }
            }
        }

        // User-level: ~/.astra/server.toml
        if let Ok(home) = std::env::var("HOME") {
            let user_path = std::path::PathBuf::from(home)
                .join(".astra")
                .join("server.toml");
            if user_path.exists() {
                match Self::from_file(&user_path) {
                    Ok(user_config) => config.merge(user_config),
                    Err(e) => {
                        tracing::warn!(
                            "Failed to load {path}: {err}; continuing with defaults",
                            path = user_path.display(),
                            err = e
                        );
                    }
                }
            }
        }

        // Apply environment variable overrides
        config.apply_env_overrides();

        // Validate database config after all overrides are applied.
        config
            .database
            .validate()
            .map_err(ConfigError::InvalidValue)?;

        // Validate runtime config after all overrides are applied.
        config
            .runtime
            .validate()
            .map_err(ConfigError::InvalidValue)?;

        Ok(config)
    }

    /// Load configuration from a specific file path.
    pub(crate) fn from_file(path: &Path) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            ConfigError::InvalidValue(format!("failed to read {}: {}", path.display(), e))
        })?;
        Self::parse(&content)
    }

    /// Parse configuration from a TOML string.
    pub(crate) fn parse(toml_content: &str) -> Result<Self, ConfigError> {
        toml::from_str(toml_content)
            .map_err(|e| ConfigError::InvalidValue(format!("failed to parse TOML: {}", e)))
    }

    /// Merge another config into this one (other takes precedence for non-None fields).
    fn merge(&mut self, other: Self) {
        self.database.merge_from(&other.database);
        self.auth.merge_from(&other.auth);
        self.api.merge_from(&other.api);
        self.runtime.merge_from(&other.runtime);
        self.deployment.merge_from(&other.deployment);
    }

    /// Apply environment variable overrides on top of loaded config.
    pub(crate) fn apply_env_overrides(&mut self) {
        self.database.apply_env_overrides();
        self.auth.apply_env_overrides();
        self.api.apply_env_overrides();
        self.runtime.apply_env_overrides();
        self.deployment.apply_env_overrides();
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct AppSettings {
    pub matrixone: MatrixOneSettings,
    pub jwt: JwtSettings,
    pub api: ApiSettings,
    pub memoria: MemoriaSettings,
    pub bridge_secret: String,
    pub token_encryption_key: Option<String>,
    pub database_bootstrap_catalog: String,
    /// Tools disabled at deployment time (deployment.toml → server.toml → env).
    pub disabled_tools: Vec<String>,
}

impl fmt::Debug for AppSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppSettings")
            .field("matrixone", &self.matrixone)
            .field("jwt", &self.jwt)
            .field("api", &self.api)
            .field("memoria", &self.memoria)
            .field("bridge_secret", &"[REDACTED]")
            .field(
                "token_encryption_key",
                &self.token_encryption_key.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

impl AppSettings {
    pub fn from_env() -> Result<Self, ConfigError> {
        dotenvy::dotenv().ok();
        let server_config = ServerConfig::load()?;
        Self::from_server_config(&server_config)
    }

    pub fn from_server_config(sc: &ServerConfig) -> Result<Self, ConfigError> {
        // Build a lookup that checks ServerConfig first (TOML + env overrides already applied),
        // then falls back to env vars for keys not in ServerConfig.
        let lookup = |key: &str| -> Option<String> {
            match key {
                // Database pool
                "ASTRA_DB_POOL_MAX_CONNECTIONS" => Some(sc.database.max_connections().to_string()),
                "ASTRA_DB_POOL_MIN_CONNECTIONS" => Some(sc.database.min_connections().to_string()),
                "ASTRA_DB_POOL_ACQUIRE_TIMEOUT_SECS" => {
                    Some(sc.database.connect_timeout_s().to_string())
                }
                "ASTRA_DB_POOL_MAX_LIFETIME_SECS" => Some(sc.database.max_lifetime_s().to_string()),
                "ASTRA_DB_POOL_IDLE_TIMEOUT_SECS" => Some(sc.database.idle_timeout_s().to_string()),

                // Auth
                "ASTRA_JWT_SECRET" => sc.auth.jwt_secret.clone(),
                "ASTRA_JWT_ALGORITHM" => sc.auth.jwt_algorithm.clone(),
                "ASTRA_JWT_ACCESS_TTL_MINUTES" => sc.auth.access_ttl_minutes.map(|v| v.to_string()),
                "ASTRA_JWT_REFRESH_TTL_DAYS" => sc.auth.refresh_ttl_days.map(|v| v.to_string()),
                "ASTRA_BRIDGE_SECRET" => sc.auth.bridge_secret.clone(),
                "ASTRA_AUTH_MODE" => sc.auth.auth_mode.clone(),
                "ASTRA_TOKEN_ENCRYPTION_KEY" => sc.auth.token_encryption_key.clone(),
                // API
                "ASTRA_API_HOST" => Some(sc.api.host().to_string()),
                "ASTRA_API_PORT" => Some(sc.api.port().to_string()),
                "ASTRA_CORS_ORIGINS" => {
                    let origins = sc.api.cors_origins();
                    if origins.is_empty() {
                        None
                    } else {
                        Some(origins.join(","))
                    }
                }
                // For all other keys, fall back to env var
                _ => env::var(key).ok(),
            }
        };
        let mut settings = Self::from_lookup(lookup)?;
        settings.disabled_tools = sc.deployment.disabled_tools.clone();
        Ok(settings)
    }

    pub fn from_map(values: &HashMap<String, String>) -> Result<Self, ConfigError> {
        Self::from_lookup(|key| values.get(key).cloned())
    }
    fn disabled_tools_from_lookup<F>(lookup: &F) -> Vec<String>
    where
        F: Fn(&str) -> Option<String>,
    {
        lookup("ASTRA_DISABLED_TOOLS")
            .map(|s| {
                s.split(',')
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn from_lookup<F>(lookup: F) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let matrixone = MatrixOneSettings {
            host: value_or_default(&lookup, "MATRIXONE_HOST", "localhost"),
            port: parse_or_default(&lookup, "MATRIXONE_PORT", 6001)?,
            user: value_or_default(&lookup, "MATRIXONE_USER", "root"),
            password: required_value(&lookup, "MATRIXONE_PASSWORD", "111")?,
            database: resolve_database_name(&lookup),
            db_pool_max_connections: parse_or_default(
                &lookup,
                "ASTRA_DB_POOL_MAX_CONNECTIONS",
                DEFAULT_DB_POOL_MAX_CONNECTIONS,
            )?,
            db_pool_min_connections: parse_or_default(
                &lookup,
                "ASTRA_DB_POOL_MIN_CONNECTIONS",
                DEFAULT_DB_POOL_MIN_CONNECTIONS,
            )?,
            db_pool_acquire_timeout_secs: parse_or_default(
                &lookup,
                "ASTRA_DB_POOL_ACQUIRE_TIMEOUT_SECS",
                DEFAULT_DB_POOL_ACQUIRE_TIMEOUT_SECS,
            )?,
            db_pool_idle_timeout_secs: parse_or_default(
                &lookup,
                "ASTRA_DB_POOL_IDLE_TIMEOUT_SECS",
                DEFAULT_DB_POOL_IDLE_TIMEOUT_SECS,
            )?,
            db_pool_max_lifetime_secs: parse_or_default(
                &lookup,
                "ASTRA_DB_POOL_MAX_LIFETIME_SECS",
                DEFAULT_DB_POOL_MAX_LIFETIME_SECS,
            )?,
        };
        matrixone.validate().map_err(ConfigError::Validation)?;

        Ok(Self {
            matrixone,
            database_bootstrap_catalog: value_or_default(
                &lookup,
                "ASTRA_DATABASE_BOOTSTRAP_CATALOG",
                "mysql",
            ),
            jwt: JwtSettings::from_lookup(&lookup)?,
            api: ApiSettings {
                host: value_or_default(&lookup, "ASTRA_API_HOST", "0.0.0.0"),
                port: parse_or_default(&lookup, "ASTRA_API_PORT", 8000)?,
                cors_origins: lookup("ASTRA_CORS_ORIGINS"),
            },
            memoria: MemoriaSettings {
                base_url: value_or_default(&lookup, "MEMORIA_BASE_URL", DEFAULT_MEMORIA_URL),
                master_key: lookup("MEMORIA_MASTER_KEY"),
            },
            bridge_secret: required_value(
                &lookup,
                "ASTRA_BRIDGE_SECRET",
                "dev-bridge-secret-change-me",
            )?,
            token_encryption_key: lookup("ASTRA_TOKEN_ENCRYPTION_KEY"),
            disabled_tools: Self::disabled_tools_from_lookup(&lookup),
        })
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct MatrixOneSettings {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub database: String,
    /// Max connections in the shared pool (env `ASTRA_DB_POOL_MAX_CONNECTIONS`, default 80).
    pub db_pool_max_connections: u32,
    /// Min idle connections in the shared pool (env `ASTRA_DB_POOL_MIN_CONNECTIONS`, default 1).
    pub db_pool_min_connections: u32,
    /// Acquire timeout in seconds (env `ASTRA_DB_POOL_ACQUIRE_TIMEOUT_SECS`, default 5).
    pub db_pool_acquire_timeout_secs: u64,
    /// Idle timeout in seconds (env `ASTRA_DB_POOL_IDLE_TIMEOUT_SECS`, default 60).
    pub db_pool_idle_timeout_secs: u64,
    /// Max connection lifetime in seconds (env `ASTRA_DB_POOL_MAX_LIFETIME_SECS`, default 300).
    pub db_pool_max_lifetime_secs: u64,
}

impl fmt::Debug for MatrixOneSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MatrixOneSettings")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("password", &"[REDACTED]")
            .field("database", &self.database)
            .field("db_pool_max_connections", &self.db_pool_max_connections)
            .field("db_pool_min_connections", &self.db_pool_min_connections)
            .field(
                "db_pool_acquire_timeout_secs",
                &self.db_pool_acquire_timeout_secs,
            )
            .field("db_pool_idle_timeout_secs", &self.db_pool_idle_timeout_secs)
            .field("db_pool_max_lifetime_secs", &self.db_pool_max_lifetime_secs)
            .finish()
    }
}

/// MySQL CLI connect timeout (seconds) — shared between server and edge tool execution.
pub const MO_CLI_CONNECT_TIMEOUT_SECS: u32 = 5;

impl Default for MatrixOneSettings {
    fn default() -> Self {
        Self {
            host: "localhost".into(),
            port: 6001,
            user: "root".into(),
            password: "".into(),
            database: "astra".into(),
            db_pool_max_connections: DEFAULT_DB_POOL_MAX_CONNECTIONS,
            db_pool_min_connections: DEFAULT_DB_POOL_MIN_CONNECTIONS,
            db_pool_acquire_timeout_secs: DEFAULT_DB_POOL_ACQUIRE_TIMEOUT_SECS,
            db_pool_idle_timeout_secs: DEFAULT_DB_POOL_IDLE_TIMEOUT_SECS,
            db_pool_max_lifetime_secs: DEFAULT_DB_POOL_MAX_LIFETIME_SECS,
        }
    }
}

impl MatrixOneSettings {
    /// Build a `mysql` CLI [`Command`](std::process::Command) pre-configured
    /// with this settings' host/port/user/password.
    ///
    /// Password is passed via `MYSQL_PWD` env var (hidden from `ps`).
    pub fn mysql_cmd(&self, database: Option<&str>) -> std::process::Command {
        let db = database.unwrap_or(&self.database);
        let mut cmd = std::process::Command::new("mysql");
        cmd.arg(format!("-h{}", self.host))
            .arg(format!("-P{}", self.port))
            .arg(format!("-u{}", self.user))
            .env("MYSQL_PWD", &self.password)
            .arg(db)
            .arg(format!("--connect-timeout={MO_CLI_CONNECT_TIMEOUT_SECS}"))
            .arg("--table");
        cmd
    }

    /// Build settings from environment with dev-safe defaults.
    ///
    /// Falls back to `localhost:6001`, `root`, and the bundled dev password.
    /// Suitable for local dev and tests. Call `dotenvy::dotenv().ok()` first
    /// if you need `.env` file loading (the server entry point already does this).
    pub fn from_env() -> Self {
        let lookup = |k: &str| env::var(k).ok();
        Self {
            host: env::var("MATRIXONE_HOST").unwrap_or_else(|_| "localhost".into()),
            port: env::var("MATRIXONE_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(6001),
            user: env::var("MATRIXONE_USER").unwrap_or_else(|_| "root".into()),
            password: env::var("MATRIXONE_PASSWORD").unwrap_or_else(|_| "111".into()),
            database: resolve_database_name(&lookup),
            db_pool_max_connections: env::var("ASTRA_DB_POOL_MAX_CONNECTIONS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_DB_POOL_MAX_CONNECTIONS),
            db_pool_min_connections: env::var("ASTRA_DB_POOL_MIN_CONNECTIONS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_DB_POOL_MIN_CONNECTIONS),
            db_pool_acquire_timeout_secs: env::var("ASTRA_DB_POOL_ACQUIRE_TIMEOUT_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_DB_POOL_ACQUIRE_TIMEOUT_SECS),
            db_pool_idle_timeout_secs: env::var("ASTRA_DB_POOL_IDLE_TIMEOUT_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_DB_POOL_IDLE_TIMEOUT_SECS),
            db_pool_max_lifetime_secs: env::var("ASTRA_DB_POOL_MAX_LIFETIME_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_DB_POOL_MAX_LIFETIME_SECS),
        }
    }

    /// Build settings from environment, **requiring** `MATRIXONE_PASSWORD`.
    ///
    /// Returns `Err` when the password is unset — suitable for production and
    /// any code path that must not silently fall back to a dev password.
    pub fn from_env_strict() -> Result<Self, String> {
        let lookup = |k: &str| env::var(k).ok();
        let settings = Self {
            host: env::var("MATRIXONE_HOST").unwrap_or_else(|_| "localhost".into()),
            port: env::var("MATRIXONE_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(6001),
            user: env::var("MATRIXONE_USER").unwrap_or_else(|_| "root".into()),
            password: env::var("MATRIXONE_PASSWORD")
                .map_err(|_| "MATRIXONE_PASSWORD environment variable is required".to_string())?,
            database: resolve_database_name(&lookup),
            db_pool_max_connections: env::var("ASTRA_DB_POOL_MAX_CONNECTIONS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_DB_POOL_MAX_CONNECTIONS),
            db_pool_min_connections: env::var("ASTRA_DB_POOL_MIN_CONNECTIONS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_DB_POOL_MIN_CONNECTIONS),
            db_pool_acquire_timeout_secs: env::var("ASTRA_DB_POOL_ACQUIRE_TIMEOUT_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_DB_POOL_ACQUIRE_TIMEOUT_SECS),
            db_pool_idle_timeout_secs: env::var("ASTRA_DB_POOL_IDLE_TIMEOUT_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_DB_POOL_IDLE_TIMEOUT_SECS),
            db_pool_max_lifetime_secs: env::var("ASTRA_DB_POOL_MAX_LIFETIME_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_DB_POOL_MAX_LIFETIME_SECS),
        };
        settings.validate()?;
        Ok(settings)
    }

    /// Build settings with a specific database name (other values from env).
    pub fn from_env_with_database(database: impl Into<String>) -> Self {
        let mut s = Self::from_env();
        s.database = database.into();
        s
    }

    /// Fake settings for unit tests that never open a real DB connection.
    #[cfg(any(test, feature = "dev-defaults"))]
    pub fn mock() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 6001,
            user: "test".into(),
            password: "test".into(),
            database: "test".into(),
            db_pool_max_connections: DEFAULT_DB_POOL_MAX_CONNECTIONS,
            db_pool_min_connections: DEFAULT_DB_POOL_MIN_CONNECTIONS,
            db_pool_acquire_timeout_secs: DEFAULT_DB_POOL_ACQUIRE_TIMEOUT_SECS,
            db_pool_idle_timeout_secs: DEFAULT_DB_POOL_IDLE_TIMEOUT_SECS,
            db_pool_max_lifetime_secs: DEFAULT_DB_POOL_MAX_LIFETIME_SECS,
        }
    }

    /// Returns the database URL with password REDACTED — safe for logging.
    ///
    /// Use [`MatrixOneSettings::database_url_with_password`] when an actual
    /// connection string is required (e.g. constructing a sqlx pool).
    pub fn database_url(&self) -> String {
        format!(
            "mysql://{}:[REDACTED]@{}:{}/{}",
            encode_mysql_url_component(&self.user),
            self.host,
            self.port,
            encode_mysql_url_component(&self.database)
        )
    }

    /// Validate pool configuration values.
    ///
    /// Returns `Err` with a human-readable message if any pool parameter is
    /// out of range:
    /// - `max_connections` must be ≥ 1
    /// - `min_connections` must be ≤ `max_connections`
    /// - acquire/idle/lifetime timeouts must be > 0
    pub fn validate(&self) -> Result<(), String> {
        if self.db_pool_max_connections == 0 {
            return Err("db_pool_max_connections must be ≥ 1 (got 0)".into());
        }
        if self.db_pool_min_connections > self.db_pool_max_connections {
            return Err(format!(
                "db_pool_min_connections ({}) must be ≤ db_pool_max_connections ({})",
                self.db_pool_min_connections, self.db_pool_max_connections
            ));
        }
        if self.db_pool_acquire_timeout_secs == 0 {
            return Err("db_pool_acquire_timeout_secs must be > 0".into());
        }
        if self.db_pool_idle_timeout_secs == 0 {
            return Err("db_pool_idle_timeout_secs must be > 0".into());
        }
        if self.db_pool_max_lifetime_secs == 0 {
            return Err("db_pool_max_lifetime_secs must be > 0".into());
        }
        Ok(())
    }

    /// Returns the database URL with the actual password — use ONLY for DB
    /// connection construction. Never log the result.
    pub fn database_url_with_password(&self) -> String {
        format!(
            "mysql://{}:{}@{}:{}/{}",
            encode_mysql_url_component(&self.user),
            encode_mysql_url_component(&self.password),
            self.host,
            self.port,
            encode_mysql_url_component(&self.database)
        )
    }
}

fn encode_mysql_url_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char);
            }
            _ => {
                encoded.push('%');
                encoded.push_str(&format!("{byte:02X}"));
            }
        }
    }
    encoded
}

#[derive(Clone, PartialEq, Eq)]
pub struct JwtSettings {
    pub secret_key: String,
    pub algorithm: String,
    pub access_token_expire_minutes: u32,
    pub refresh_token_expire_days: u32,
}

impl fmt::Debug for JwtSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JwtSettings")
            .field("secret_key", &"[REDACTED]")
            .field("algorithm", &self.algorithm)
            .field(
                "access_token_expire_minutes",
                &self.access_token_expire_minutes,
            )
            .field("refresh_token_expire_days", &self.refresh_token_expire_days)
            .finish()
    }
}

impl JwtSettings {
    fn from_lookup<F>(lookup: &F) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let secret = required_value(lookup, "ASTRA_JWT_SECRET", "change-me-in-production")?;
        let settings = Self {
            secret_key: secret,
            algorithm: value_or_default(lookup, "ASTRA_JWT_ALGORITHM", "HS256"),
            access_token_expire_minutes: parse_or_default(
                lookup,
                "ASTRA_JWT_ACCESS_TTL_MINUTES",
                10080_u32, // 7 days (was 60 min — too short for dev/harness use)
            )?,
            refresh_token_expire_days: parse_or_default(
                lookup,
                "ASTRA_JWT_REFRESH_TTL_DAYS",
                30_u32, // 30 days — must be longer than access token for refresh to work
            )?,
        };

        // Reject dangerously weak secrets in production (insecure-defaults mode skips this).
        let insecure_ok = lookup("ASTRA_ALLOW_INSECURE_DEFAULTS")
            .map(|v| v == "1")
            .unwrap_or(false);
        if !insecure_ok {
            if settings.secret_key == "change-me-in-production" {
                return Err(ConfigError::Validation(
                    "ASTRA_JWT_SECRET is still the default placeholder 'change-me-in-production' — set a real secret".into(),
                ));
            }
            if settings.secret_key.len() < 32 {
                return Err(ConfigError::Validation(format!(
                    "ASTRA_JWT_SECRET must be at least 32 characters (got {})",
                    settings.secret_key.len()
                )));
            }
        }

        Ok(settings)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ApiSettings {
    pub host: String,
    pub port: u16,
    pub cors_origins: Option<String>,
}

impl fmt::Debug for ApiSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiSettings")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("cors_origins", &self.cors_origins)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct MemoriaSettings {
    pub base_url: String,
    pub master_key: Option<String>,
}

impl MemoriaSettings {
    /// Read Memoria connection config from environment.
    pub fn from_env() -> Self {
        Self {
            base_url: env::var("MEMORIA_BASE_URL")
                .unwrap_or_else(|_| DEFAULT_MEMORIA_URL.to_string()),
            master_key: env::var("MEMORIA_MASTER_KEY").ok(),
        }
    }

    /// Returns `true` when a master key is configured (Memoria is usable).
    pub fn is_configured(&self) -> bool {
        self.master_key.as_ref().is_some_and(|k| !k.is_empty())
    }

    /// `Authorization: Bearer <key>` header value, or `None` if unconfigured.
    pub fn bearer_token(&self) -> Option<String> {
        self.master_key
            .as_ref()
            .filter(|k| !k.is_empty())
            .map(|k| format!("Bearer {k}"))
    }
}

impl fmt::Debug for MemoriaSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemoriaSettings")
            .field("base_url", &self.base_url)
            .field(
                "master_key",
                &self.master_key.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigError {
    InvalidInteger { key: &'static str, value: String },
    MissingRequiredKey { name: &'static str },
    InvalidValue(String),
    Validation(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInteger { key, value } => {
                write!(f, "invalid integer for {key}: {value}")
            }
            Self::MissingRequiredKey { name } => {
                write!(
                    f,
                    "required configuration key `{name}` is unset; set the env var or \
                     opt into bundled insecure defaults with ASTRA_ALLOW_INSECURE_DEFAULTS=1 \
                     (NOT for production)"
                )
            }
            Self::InvalidValue(msg) => {
                write!(f, "invalid configuration value: {msg}")
            }
            Self::Validation(msg) => {
                write!(f, "configuration validation failed: {msg}")
            }
        }
    }
}

impl Error for ConfigError {}

/// Resolves the logical database name used in URLs and DDL.
///
/// When `ASTRA_DATABASE_PREFIX` is set and non-empty, the effective name is
/// `{prefix}{ASTRA_DATABASE}` (base name from `ASTRA_DATABASE`, default
/// `database_default`). This lets you keep a shared base name (e.g. `astra_runtime`) and isolate
/// dev/CI/test from production with a prefix (`test_` → `test_astra_runtime`).
pub fn resolve_database_name_or<F>(lookup: &F, database_default: &str) -> String
where
    F: Fn(&str) -> Option<String>,
{
    let base = value_or_default(lookup, "ASTRA_DATABASE", database_default);
    let prefix = lookup("ASTRA_DATABASE_PREFIX").unwrap_or_default();
    if prefix.is_empty() {
        base
    } else {
        format!("{prefix}{base}")
    }
}

/// Same as [`resolve_database_name_or`] with default base name `astra_runtime`.
pub fn resolve_database_name<F>(lookup: &F) -> String
where
    F: Fn(&str) -> Option<String>,
{
    resolve_database_name_or(lookup, "astra_runtime")
}

fn value_or_default<F>(lookup: &F, key: &'static str, default: &str) -> String
where
    F: Fn(&str) -> Option<String>,
{
    lookup(key).unwrap_or_else(|| default.to_string())
}

/// Returns the value from the lookup, or an insecure default if
/// `ASTRA_ALLOW_INSECURE_DEFAULTS=1` is set in the same lookup.
///
/// In production (no opt-in), missing required keys return
/// `Err(ConfigError::MissingRequiredKey)`.
///
/// # Dev escape hatch
/// Set `ASTRA_ALLOW_INSECURE_DEFAULTS=1` to permit the bundled defaults.
/// This emits a `tracing::error!` warning once and MUST NOT be used in production.
fn required_value<F>(
    lookup: &F,
    key: &'static str,
    insecure_default: &str,
) -> Result<String, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    if let Some(val) = lookup(key) {
        return Ok(val);
    }
    if lookup("ASTRA_ALLOW_INSECURE_DEFAULTS")
        .map(|v| v == "1")
        .unwrap_or(false)
    {
        tracing::error!(
            key = key,
            "INSECURE DEFAULTS — DO NOT USE IN PRODUCTION: {} is using bundled default",
            key
        );
        return Ok(insecure_default.to_string());
    }
    Err(ConfigError::MissingRequiredKey { name: key })
}

fn parse_or_default<T, F>(lookup: &F, key: &'static str, default: T) -> Result<T, ConfigError>
where
    T: std::str::FromStr + Copy,
    F: Fn(&str) -> Option<String>,
{
    match lookup(key) {
        Some(value) => value
            .parse::<T>()
            .map_err(|_| ConfigError::InvalidInteger { key, value }),
        None => Ok(default),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn matrixone_settings_build_mysql_url() {
        let settings = MatrixOneSettings {
            host: "db".into(),
            port: 3306,
            user: "alice".into(),
            password: "secret".into(),
            database: "agent".into(),
            db_pool_max_connections: 1,
            db_pool_min_connections: 1,
            db_pool_acquire_timeout_secs: 5,
            db_pool_idle_timeout_secs: 60,
            db_pool_max_lifetime_secs: 300,
        };

        assert_eq!(
            settings.database_url(),
            "mysql://alice:[REDACTED]@db:3306/agent"
        );
        assert_eq!(
            settings.database_url_with_password(),
            "mysql://alice:secret@db:3306/agent"
        );
        assert_eq!(
            settings.database_url_with_password(),
            "mysql://alice:secret@db:3306/agent"
        );
    }

    #[test]
    fn matrixone_settings_escapes_mysql_url_userinfo() {
        let settings = MatrixOneSettings {
            host: "db".into(),
            port: 3306,
            user: "account:user:role".into(),
            password: "p@ss:word/with?chars".into(),
            database: "agent/db".into(),
            db_pool_max_connections: 1,
            db_pool_min_connections: 1,
            db_pool_acquire_timeout_secs: 5,
            db_pool_idle_timeout_secs: 60,
            db_pool_max_lifetime_secs: 300,
        };

        assert_eq!(
            settings.database_url(),
            "mysql://account%3Auser%3Arole:[REDACTED]@db:3306/agent%2Fdb"
        );
        assert_eq!(
            settings.database_url_with_password(),
            "mysql://account%3Auser%3Arole:p%40ss%3Aword%2Fwith%3Fchars@db:3306/agent%2Fdb"
        );
    }

    #[test]
    fn jwt_settings_debug_redacts_secret() {
        let mut m = HashMap::new();
        m.insert("ASTRA_ALLOW_INSECURE_DEFAULTS".into(), "1".into());
        let settings = AppSettings::from_map(&m).unwrap();
        let debug_str = format!("{:?}", settings.jwt);
        assert!(
            !debug_str.contains("change-me-in-production"),
            "secret should be redacted: {debug_str}"
        );
        assert!(
            debug_str.contains("[REDACTED]"),
            "should show [REDACTED]: {debug_str}"
        );
    }

    #[test]
    fn matrixone_settings_debug_redacts_password() {
        let mut m = HashMap::new();
        m.insert("ASTRA_ALLOW_INSECURE_DEFAULTS".into(), "1".into());
        let settings = AppSettings::from_map(&m).unwrap();
        let debug_str = format!("{:?}", settings.matrixone);
        assert!(
            !debug_str.contains("\"111\""),
            "password should be redacted: {debug_str}"
        );
        assert!(
            debug_str.contains("[REDACTED]"),
            "should show [REDACTED]: {debug_str}"
        );
    }

    #[test]
    fn matrixone_settings_database_url_is_masked() {
        let mut m = HashMap::new();
        m.insert("ASTRA_ALLOW_INSECURE_DEFAULTS".into(), "1".into());
        let settings = AppSettings::from_map(&m).unwrap();
        let url = settings.matrixone.database_url();
        assert!(
            !url.contains(":111@"),
            "masked url should not contain password: {url}"
        );
        assert!(
            url.contains("[REDACTED]"),
            "masked url should contain [REDACTED]: {url}"
        );
        let url_with_pw = settings.matrixone.database_url_with_password();
        assert!(
            url_with_pw.contains(":111@"),
            "url_with_password should contain actual password: {url_with_pw}"
        );
    }

    #[test]
    fn app_settings_debug_redacts_optional_secrets() {
        let mut m = HashMap::new();
        m.insert("ASTRA_ALLOW_INSECURE_DEFAULTS".into(), "1".into());
        m.insert("MEMORIA_MASTER_KEY".into(), "memoria-master-key-xyz".into());
        let settings = AppSettings::from_map(&m).unwrap();
        let debug_str = format!("{settings:?}");
        assert!(
            !debug_str.contains("memoria-master-key-xyz"),
            "memoria master_key should be redacted: {debug_str}"
        );
        assert!(
            !debug_str.contains("dev-bridge-secret-change-me"),
            "bridge_secret should be redacted: {debug_str}"
        );
    }

    #[test]
    fn resolve_database_prefix_concat() {
        let mut m: HashMap<String, String> = HashMap::new();
        m.insert("ASTRA_DATABASE".into(), "prod".into());
        m.insert("ASTRA_DATABASE_PREFIX".into(), "ci_".into());
        assert_eq!(resolve_database_name(&|k| m.get(k).cloned()), "ci_prod");
    }

    #[test]
    fn resolve_database_empty_prefix_uses_base_only() {
        let mut m: HashMap<String, String> = HashMap::new();
        m.insert("ASTRA_DATABASE".into(), "astra_runtime".into());
        m.insert("ASTRA_DATABASE_PREFIX".into(), "".into());
        assert_eq!(
            resolve_database_name(&|k| m.get(k).cloned()),
            "astra_runtime"
        );
    }

    #[test]
    fn jwt_settings_match_runtime_defaults_and_padding() {
        let mut m = HashMap::new();
        m.insert("ASTRA_ALLOW_INSECURE_DEFAULTS".into(), "1".into());
        let settings = AppSettings::from_map(&m).expect("defaults should parse");

        assert_eq!(settings.jwt.algorithm, "HS256");
        assert_eq!(settings.jwt.access_token_expire_minutes, 10080);
        assert_eq!(settings.jwt.refresh_token_expire_days, 30);
        // In insecure-defaults mode, the raw placeholder is accepted as-is (23 chars).
        assert!(settings.jwt.secret_key.len() >= 23);
        assert!(
            settings
                .jwt
                .secret_key
                .starts_with("change-me-in-production")
        );
    }

    #[test]
    fn from_lookup_missing_required_keys_returns_error() {
        let m = HashMap::new();
        let result = AppSettings::from_map(&m);
        assert!(result.is_err());
        match result.unwrap_err() {
            ConfigError::MissingRequiredKey { .. } => {}
            other => panic!("expected MissingRequiredKey, got {other:?}"),
        }
    }

    #[test]
    fn from_lookup_explicit_required_values_work() {
        let mut m = HashMap::new();
        m.insert("MATRIXONE_PASSWORD".into(), "testpw".into());
        m.insert(
            "ASTRA_JWT_SECRET".into(),
            "my-test-jwt-secret-key-at-least-32-chars--".into(),
        );
        m.insert("ASTRA_BRIDGE_SECRET".into(), "bridge-secret".into());
        let result = AppSettings::from_map(&m);
        assert!(result.is_ok(), "explicit values should parse: {:?}", result);
        let settings = result.unwrap();
        assert!(
            settings
                .jwt
                .secret_key
                .starts_with("my-test-jwt-secret-key-at-least")
        );
        assert_eq!(settings.matrixone.password, "testpw");
        assert_eq!(settings.bridge_secret, "bridge-secret");
    }

    #[test]
    fn from_lookup_insecure_defaults_allowed_with_opt_in() {
        let mut m = HashMap::new();
        m.insert("ASTRA_ALLOW_INSECURE_DEFAULTS".into(), "1".into());
        let result = AppSettings::from_map(&m);
        assert!(
            result.is_ok(),
            "insecure defaults should parse with opt-in: {:?}",
            result
        );
    }

    #[test]
    fn app_settings_database_includes_prefix() {
        let mut m = HashMap::new();
        m.insert("ASTRA_ALLOW_INSECURE_DEFAULTS".into(), "1".into());
        m.insert("ASTRA_DATABASE".into(), "agent_db".into());
        m.insert("ASTRA_DATABASE_PREFIX".into(), "ci_".into());
        let settings = AppSettings::from_map(&m).expect("parse");
        assert_eq!(settings.matrixone.database, "ci_agent_db");
    }

    #[test]
    fn runtime_config_validate_plan_turns_exceeds_max_turns_with_defaults() {
        let config = ServerRuntimeConfig {
            max_turns: None,
            plan_subtask_max_turns: Some(400),
            ..Default::default()
        };
        let err = config.validate().unwrap_err();
        assert!(
            err.contains("plan_subtask_max_turns (400) exceeds max_turns (300)"),
            "should reject when default max_turns=300 is exceeded: {err}"
        );
    }

    #[test]
    fn runtime_config_validate_plan_turns_exceeds_max_turns_both_explicit() {
        let config = ServerRuntimeConfig {
            max_turns: Some(30),
            plan_subtask_max_turns: Some(31),
            ..Default::default()
        };
        let err = config.validate().unwrap_err();
        assert!(
            err.contains("plan_subtask_max_turns (31) exceeds max_turns (30)"),
            "should reject explicit conflict: {err}"
        );
    }

    #[test]
    fn runtime_config_validate_plan_turns_eq_max_turns_ok() {
        let config = ServerRuntimeConfig {
            max_turns: Some(50),
            plan_subtask_max_turns: Some(50),
            ..Default::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn runtime_config_validate_defaults_pass() {
        let config = ServerRuntimeConfig::default();
        assert!(config.validate().is_ok());
    }
}

/// JSON fixture parity for `AppSettings::from_map` (was `astra-runtime` `config_contract` binary).
#[cfg(test)]
mod settings_contract_tests {
    use std::{collections::HashMap, fs, path::PathBuf};

    use serde::{Deserialize, Serialize};

    use super::AppSettings;

    #[derive(Debug, Deserialize)]
    struct SettingsContract {
        defaults: FlatSettings,
        override_env: HashMap<String, String>,
        expected_override_settings: FlatSettings,
    }

    #[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
    struct FlatSettings {
        matrixone_host: String,
        matrixone_port: u16,
        matrixone_user: String,
        matrixone_password: String,
        matrixone_database: String,
        bridge_secret: String,
    }

    fn load_contract() -> SettingsContract {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../..")
            .join("fixtures/contracts/settings_contract.json");
        let content = fs::read_to_string(path).expect("settings contract fixture should exist");
        serde_json::from_str(&content).expect("settings contract fixture should be valid JSON")
    }

    fn flatten(settings: AppSettings) -> FlatSettings {
        FlatSettings {
            matrixone_host: settings.matrixone.host,
            matrixone_port: settings.matrixone.port,
            matrixone_user: settings.matrixone.user,
            matrixone_password: settings.matrixone.password,
            matrixone_database: settings.matrixone.database,
            bridge_secret: settings.bridge_secret,
        }
    }

    #[test]
    fn defaults_match_shared_contract() {
        let contract = load_contract();
        let mut m = HashMap::new();
        m.insert("ASTRA_ALLOW_INSECURE_DEFAULTS".into(), "1".into());
        let settings = AppSettings::from_map(&m).expect("defaults should parse");

        assert_eq!(flatten(settings), contract.defaults);
    }

    #[test]
    fn overrides_match_shared_contract() {
        let contract = load_contract();
        let settings =
            AppSettings::from_map(&contract.override_env).expect("overrides should parse");

        assert_eq!(flatten(settings), contract.expected_override_settings);
    }
}

// ── ServerConfig Tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod server_config_tests {
    use super::*;

    #[test]
    fn server_config_parse_minimal_toml() {
        let toml = r#"
[database]
max_connections = 20

[api]
port = 9000
"#;
        let config = ServerConfig::parse(toml).unwrap();
        assert_eq!(config.database.max_connections, Some(20));
        assert_eq!(
            config.database.min_connections(),
            DEFAULT_DB_POOL_MIN_CONNECTIONS
        );
        assert_eq!(config.api.port, Some(9000));
        assert_eq!(config.api.host(), "0.0.0.0");
    }

    #[test]
    fn server_config_parse_full_toml() {
        let toml = r#"
[database]
max_connections = 50
min_connections = 5
connect_timeout_s = 60
max_lifetime_s = 3600
idle_timeout_s = 900

[auth]
jwt_secret = "my-secret-key"
jwt_algorithm = "HS512"
access_ttl_minutes = 1440
refresh_ttl_days = 7
bridge_secret = "bridge-secret"
auth_mode = "trusted_moi"
token_encryption_key = "fernet-key"

[api]
host = "127.0.0.1"
port = 8080
cors_origins = ["http://localhost:3000", "https://example.com"]
"#;
        let config = ServerConfig::parse(toml).unwrap();
        assert_eq!(config.database.max_connections, Some(50));
        assert_eq!(config.database.min_connections, Some(5));
        assert_eq!(config.database.connect_timeout_s, Some(60));
        assert_eq!(config.database.max_lifetime_s, Some(3600));
        assert_eq!(config.database.idle_timeout_s, Some(900));
        assert_eq!(config.auth.jwt_secret.as_deref(), Some("my-secret-key"));
        assert_eq!(config.auth.jwt_algorithm.as_deref(), Some("HS512"));
        assert_eq!(config.auth.access_ttl_minutes, Some(1440));
        assert_eq!(config.auth.refresh_ttl_days, Some(7));
        assert_eq!(config.auth.bridge_secret.as_deref(), Some("bridge-secret"));
        assert_eq!(config.auth.auth_mode.as_deref(), Some("trusted_moi"));
        assert_eq!(
            config.auth.token_encryption_key.as_deref(),
            Some("fernet-key")
        );
        assert_eq!(config.api.host.as_deref(), Some("127.0.0.1"));
        assert_eq!(config.api.port, Some(8080));
        assert_eq!(
            config.api.cors_origins.as_deref(),
            Some(
                &[
                    "http://localhost:3000".to_string(),
                    "https://example.com".to_string()
                ][..]
            )
        );
    }

    #[test]
    fn server_config_parse_empty_toml() {
        let config = ServerConfig::parse("").unwrap();
        assert_eq!(config, ServerConfig::default());
    }

    #[test]
    fn server_config_parse_invalid_toml() {
        let result = ServerConfig::parse("[invalid");
        assert!(result.is_err());
    }

    #[test]
    fn server_config_merge_override_takes_precedence() {
        let mut base = ServerConfig::default();
        base.database.max_connections = Some(10);
        base.api.port = Some(8000);
        base.auth.jwt_secret = Some("base-secret".to_string());

        let mut other = ServerConfig::default();
        other.database.max_connections = Some(50);
        other.api.port = Some(9000);
        other.auth.jwt_secret = Some("override-secret".to_string());

        base.merge(other);

        assert_eq!(base.database.max_connections, Some(50));
        assert_eq!(base.api.port, Some(9000));
        assert_eq!(base.auth.jwt_secret.as_deref(), Some("override-secret"));
    }

    #[test]
    fn server_config_merge_preserves_base_when_other_is_default() {
        let mut base = ServerConfig::default();
        base.database.max_connections = Some(25);
        base.api.port = Some(8080);

        let other = ServerConfig::default();
        base.merge(other);

        // Base values preserved since other had defaults
        assert_eq!(base.database.max_connections, Some(25));
        assert_eq!(base.api.port, Some(8080));
    }

    #[test]
    fn server_config_merge_runtime_override_takes_precedence() {
        let mut base = ServerConfig::default();
        base.runtime.max_turns = Some(100);
        base.runtime.turn_timeout_s = Some(120);
        base.runtime.max_retrieved = Some(5);

        let mut other = ServerConfig::default();
        other.runtime.max_turns = Some(200);
        other.runtime.turn_timeout_s = Some(600);
        other.runtime.max_retrieved = Some(20);

        base.merge(other);

        assert_eq!(base.runtime.max_turns, Some(200));
        assert_eq!(base.runtime.turn_timeout_s, Some(600));
        assert_eq!(base.runtime.max_retrieved, Some(20));
    }

    #[test]
    fn server_config_merge_runtime_preserves_base_when_other_is_default() {
        let mut base = ServerConfig::default();
        base.runtime.max_turns = Some(100);
        base.runtime.turn_timeout_s = Some(120);
        base.runtime.max_retrieved = Some(5);

        let other = ServerConfig::default();
        base.merge(other);

        // Base values preserved since other had defaults
        assert_eq!(base.runtime.max_turns, Some(100));
        assert_eq!(base.runtime.turn_timeout_s, Some(120));
        assert_eq!(base.runtime.max_retrieved, Some(5));
    }

    #[test]
    fn server_config_env_override_runtime() {
        temp_env::with_vars(
            [
                ("ASTRA_MAX_TURNS", Some("50")),
                ("ASTRA_TURN_TIMEOUT_S", Some("600")),
                ("ASTRA_MAX_RETRIEVED", Some("10")),
                ("ASTRA_MAX_TOOL_RETRIES", Some("5")),
                ("ASTRA_RETRY_BASE_MS", Some("1000")),
            ],
            || {
                let mut config = ServerConfig::default();
                config.apply_env_overrides();
                assert_eq!(config.runtime.max_turns, Some(50));
                assert_eq!(config.runtime.turn_timeout_s, Some(600));
                assert_eq!(config.runtime.max_retrieved, Some(10));
                assert_eq!(config.runtime.max_tool_retries, Some(5));
                assert_eq!(config.runtime.retry_base_ms, Some(1000));
            },
        );
    }

    #[test]
    fn env_override_respects_precedence() {
        temp_env::with_vars(
            [
                ("ASTRA_DB_POOL_MAX_CONNECTIONS", Some("100")),
                ("ASTRA_DB_POOL_MIN_CONNECTIONS", Some("10")),
                ("ASTRA_DB_POOL_ACQUIRE_TIMEOUT_SECS", Some("10")),
                ("ASTRA_DB_POOL_IDLE_TIMEOUT_SECS", Some("120")),
                ("ASTRA_DB_POOL_MAX_LIFETIME_SECS", Some("600")),
            ],
            || {
                let mut config = ServerConfig::default();
                config.apply_env_overrides();
                assert_eq!(config.database.max_connections, Some(100));
                assert_eq!(config.database.min_connections, Some(10));
                assert_eq!(config.database.connect_timeout_s, Some(10));
                assert_eq!(config.database.idle_timeout_s, Some(120));
                assert_eq!(config.database.max_lifetime_s, Some(600));
            },
        );
    }

    #[test]
    fn server_config_env_override_auth() {
        temp_env::with_vars(
            [
                ("ASTRA_JWT_SECRET", Some("env-jwt-secret")),
                ("ASTRA_BRIDGE_SECRET", Some("env-bridge-secret")),
            ],
            || {
                let mut config = ServerConfig::default();
                config.apply_env_overrides();
                assert_eq!(config.auth.jwt_secret.as_deref(), Some("env-jwt-secret"));
                assert_eq!(
                    config.auth.bridge_secret.as_deref(),
                    Some("env-bridge-secret")
                );
            },
        );
    }

    #[test]
    fn server_config_env_override_api() {
        temp_env::with_vars(
            [
                ("ASTRA_API_HOST", Some("192.168.1.1")),
                ("ASTRA_API_PORT", Some("3000")),
                ("ASTRA_CORS_ORIGINS", Some("http://a.com,http://b.com")),
            ],
            || {
                let mut config = ServerConfig::default();
                config.apply_env_overrides();
                assert_eq!(config.api.host.as_deref(), Some("192.168.1.1"));
                assert_eq!(config.api.port, Some(3000));
                assert_eq!(
                    config.api.cors_origins.as_deref(),
                    Some(&["http://a.com".to_string(), "http://b.com".to_string()][..])
                );
            },
        );
    }

    #[test]
    fn server_config_serialization_roundtrip() {
        let mut config = ServerConfig::default();
        config.database.max_connections = Some(42);
        config.auth.jwt_secret = Some("test-secret".to_string());
        config.api.port = Some(8888);

        let toml_str = toml::to_string(&config).unwrap();
        let parsed = ServerConfig::parse(&toml_str).unwrap();
        assert_eq!(parsed.database.max_connections, Some(42));
        assert_eq!(parsed.auth.jwt_secret.as_deref(), Some("test-secret"));
        assert_eq!(parsed.api.port, Some(8888));
    }

    #[test]
    fn app_settings_from_server_config() {
        temp_env::with_var("MATRIXONE_PASSWORD", Some("test-password"), || {
            let mut sc = ServerConfig::default();
            sc.database.max_connections = Some(30);
            sc.database.min_connections = Some(5);
            sc.database.connect_timeout_s = Some(8);
            sc.database.idle_timeout_s = Some(90);
            sc.database.max_lifetime_s = Some(400);
            sc.api.host = Some("127.0.0.1".to_string());
            sc.api.port = Some(9000);
            sc.auth.jwt_secret = Some("toml-jwt-secret-which-is-long-enough-32".to_string());
            sc.auth.bridge_secret = Some("toml-bridge-secret".to_string());

            let settings = AppSettings::from_server_config(&sc).unwrap();
            assert_eq!(settings.matrixone.db_pool_max_connections, 30);
            assert_eq!(settings.matrixone.db_pool_min_connections, 5);
            assert_eq!(settings.matrixone.db_pool_acquire_timeout_secs, 8);
            assert_eq!(settings.matrixone.db_pool_idle_timeout_secs, 90);
            assert_eq!(settings.matrixone.db_pool_max_lifetime_secs, 400);
            assert_eq!(settings.api.host, "127.0.0.1");
            assert_eq!(settings.api.port, 9000);
        });
    }

    #[test]
    fn deployment_disabled_tools_from_toml() {
        let toml_str = r#"
            [deployment]
            disabled_tools = ["tool_a", "tool_b"]
            "#;
        let config = ServerConfig::parse(toml_str).unwrap();
        assert_eq!(
            config.deployment.disabled_tools,
            vec!["tool_a".to_string(), "tool_b".to_string()]
        );
    }

    #[test]
    fn deployment_disabled_tools_from_env() {
        temp_env::with_var(
            "ASTRA_DISABLED_TOOLS",
            Some("tool_x, tool_y, tool_z"),
            || {
                let mut config = ServerConfig::default();
                config.apply_env_overrides();
                assert_eq!(
                    config.deployment.disabled_tools,
                    vec![
                        "tool_x".to_string(),
                        "tool_y".to_string(),
                        "tool_z".to_string()
                    ]
                );
            },
        );
    }

    #[test]
    fn deployment_disabled_tools_empty_env_noop() {
        temp_env::with_var("ASTRA_DISABLED_TOOLS", Some(""), || {
            let mut config = ServerConfig::default();
            config.deployment.disabled_tools = vec!["from_toml".to_string()];
            config.apply_env_overrides();
            // Empty env value should not overwrite TOML value
            assert_eq!(
                config.deployment.disabled_tools,
                vec!["from_toml".to_string()]
            );
        });
    }

    #[test]
    fn deployment_disabled_tools_env_trims_whitespace() {
        temp_env::with_var("ASTRA_DISABLED_TOOLS", Some("  a , b ,  c  "), || {
            let mut config = ServerConfig::default();
            config.apply_env_overrides();
            assert_eq!(
                config.deployment.disabled_tools,
                vec!["a".to_string(), "b".to_string(), "c".to_string()]
            );
        });
    }
}

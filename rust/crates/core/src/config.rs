use std::{collections::HashMap, env, error::Error, fmt};

use serde::{Deserialize, Serialize};

/// Default Memoria base URL. Uses `127.0.0.1` instead of `localhost` because
/// Memoria binds to `0.0.0.0` (IPv4 only) and `localhost` may resolve to `::1`
/// on dual-stack systems, causing connection failures.
pub const DEFAULT_MEMORIA_URL: &str = "http://127.0.0.1:8100";

/// Tunable skill catalog surfacing: capped per-turn listing plus `discover_skills` when the
/// catalog is larger than `min_catalog_size` and `dynamic_surface` is enabled.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillSearchSettings {
    /// When true and skill count exceeds `min_catalog_size`, use a capped surface + discovery tool.
    pub dynamic_surface: bool,
    /// Below or equal to this count, every skill is listed (no discovery path).
    pub min_catalog_size: usize,
    /// Max skills in the auto-surfaced subset when dynamic mode applies.
    pub surface_cap: usize,
}

impl Default for SkillSearchSettings {
    fn default() -> Self {
        Self {
            dynamic_surface: true,
            min_catalog_size: 8,
            surface_cap: 14,
        }
    }
}

impl SkillSearchSettings {
    /// When true, expose the full catalog (enum listing, no `discover_skills` for this size).
    #[inline]
    pub fn use_full_catalog(&self, skill_count: usize) -> bool {
        !self.dynamic_surface || skill_count <= self.min_catalog_size
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppSettings {
    pub matrixone: MatrixOneSettings,
    pub application: ApplicationSettings,
    pub jwt: JwtSettings,
    pub github_token: Option<String>,
    pub memoria_base_url: String,
    pub memoria_master_key: Option<String>,
    pub chat_turn_bridge_url: Option<String>,
    pub chat_turn_bridge_secret: String,
}

impl AppSettings {
    pub fn from_env() -> Result<Self, ConfigError> {
        dotenvy::dotenv().ok();
        Self::from_lookup(|key| env::var(key).ok())
    }

    pub fn from_map(values: &HashMap<String, String>) -> Result<Self, ConfigError> {
        Self::from_lookup(|key| values.get(key).cloned())
    }

    fn from_lookup<F>(lookup: F) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        Ok(Self {
            matrixone: MatrixOneSettings {
                host: value_or_default(&lookup, "MATRIXONE_HOST", "localhost"),
                port: parse_or_default(&lookup, "MATRIXONE_PORT", 6001)?,
                user: value_or_default(&lookup, "MATRIXONE_USER", "root"),
                password: required_value(&lookup, "MATRIXONE_PASSWORD", "111")?,
                database: resolve_database_name(&lookup),
            },
            application: ApplicationSettings {
                app_env: value_or_default(&lookup, "APP_ENV", "development"),
                log_level: value_or_default(&lookup, "LOG_LEVEL", "DEBUG"),
                secret_key: required_value(
                    &lookup,
                    "SECRET_KEY",
                    "dev-secret-key-change-in-production",
                )?,
            },
            jwt: JwtSettings::from_lookup(&lookup)?,
            github_token: optional_value(&lookup, "GITHUB_TOKEN"),
            memoria_base_url: value_or_default(&lookup, "MEMORIA_BASE_URL", DEFAULT_MEMORIA_URL),
            memoria_master_key: optional_value(&lookup, "MEMORIA_MASTER_KEY"),
            chat_turn_bridge_url: optional_value(&lookup, "CHAT_TURN_BRIDGE_URL"),
            chat_turn_bridge_secret: required_value(
                &lookup,
                "CHAT_TURN_BRIDGE_SECRET",
                "dev-bridge-secret-change-me",
            )?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatrixOneSettings {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub database: String,
}

impl MatrixOneSettings {
    pub fn database_url(&self) -> String {
        format!(
            "mysql://{}:{}@{}:{}/{}",
            self.user, self.password, self.host, self.port, self.database
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplicationSettings {
    pub app_env: String,
    pub log_level: String,
    pub secret_key: String,
}

impl ApplicationSettings {
    pub fn is_development(&self) -> bool {
        self.app_env == "development"
    }

    pub fn is_production(&self) -> bool {
        self.app_env == "production"
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JwtSettings {
    pub secret_key: String,
    pub algorithm: String,
    pub access_token_expire_minutes: u32,
    pub refresh_token_expire_days: u32,
}

impl JwtSettings {
    fn from_lookup<F>(lookup: &F) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let secret = required_value(lookup, "JWT_SECRET_KEY", "change-me-in-production")?;
        Ok(Self {
            secret_key: normalize_jwt_secret(&secret),
            algorithm: value_or_default(lookup, "JWT_ALGORITHM", "HS256"),
            access_token_expire_minutes: parse_or_default(
                lookup,
                "JWT_ACCESS_TOKEN_EXPIRE_MINUTES",
                60_u32,
            )?,
            refresh_token_expire_days: parse_or_default(
                lookup,
                "JWT_REFRESH_TOKEN_EXPIRE_DAYS",
                7_u32,
            )?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigError {
    InvalidInteger { key: &'static str, value: String },
    MissingRequiredKey { name: &'static str },
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
    let prefix = optional_value(lookup, "ASTRA_DATABASE_PREFIX").unwrap_or_default();
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

fn optional_value<F>(lookup: &F, key: &'static str) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    lookup(key)
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

fn normalize_jwt_secret(secret: &str) -> String {
    if secret.len() >= 32 {
        secret.to_string()
    } else {
        let mut padded = secret.to_string();
        padded.extend(std::iter::repeat_n('0', 32 - secret.len()));
        padded
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn application_mode_helpers_match_runtime_modes() {
        let development = ApplicationSettings {
            app_env: "development".into(),
            log_level: "DEBUG".into(),
            secret_key: "secret".into(),
        };
        let production = ApplicationSettings {
            app_env: "production".into(),
            log_level: "INFO".into(),
            secret_key: "secret".into(),
        };

        assert!(development.is_development());
        assert!(!development.is_production());
        assert!(production.is_production());
        assert!(!production.is_development());
    }

    #[test]
    fn matrixone_settings_build_mysql_url() {
        let settings = MatrixOneSettings {
            host: "db".into(),
            port: 3306,
            user: "alice".into(),
            password: "secret".into(),
            database: "agent".into(),
        };

        assert_eq!(
            settings.database_url(),
            "mysql://alice:secret@db:3306/agent"
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
        assert_eq!(settings.jwt.access_token_expire_minutes, 60);
        assert_eq!(settings.jwt.refresh_token_expire_days, 7);
        assert_eq!(settings.jwt.secret_key.len(), 32);
        assert!(
            settings
                .jwt
                .secret_key
                .starts_with("change-me-in-production")
        );
    }

    #[test]
    fn from_lookup_missing_required_keys_returns_error() {
        let m = HashMap::new(); // no ASTRA_ALLOW_INSECURE_DEFAULTS
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
        m.insert("SECRET_KEY".into(), "test-secret".into());
        m.insert("JWT_SECRET_KEY".into(), "my-test-jwt-secret-key-123".into());
        m.insert("CHAT_TURN_BRIDGE_SECRET".into(), "bridge-secret".into());
        let result = AppSettings::from_map(&m);
        assert!(result.is_ok(), "explicit values should parse: {:?}", result);
        let settings = result.unwrap();
        assert!(
            settings
                .jwt
                .secret_key
                .starts_with("my-test-jwt-secret-key-123")
        );
        assert_eq!(settings.matrixone.password, "testpw");
        assert_eq!(settings.application.secret_key, "test-secret");
        assert_eq!(settings.chat_turn_bridge_secret, "bridge-secret");
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
        app_env: String,
        log_level: String,
        secret_key: String,
        github_token: Option<String>,
        chat_turn_bridge_url: Option<String>,
        chat_turn_bridge_secret: String,
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
            app_env: settings.application.app_env,
            log_level: settings.application.log_level,
            secret_key: settings.application.secret_key,
            github_token: settings.github_token,
            chat_turn_bridge_url: settings.chat_turn_bridge_url,
            chat_turn_bridge_secret: settings.chat_turn_bridge_secret,
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

use std::{collections::HashMap, env, error::Error, fmt};

/// Default Memoria base URL. Uses `127.0.0.1` instead of `localhost` because
/// Memoria binds to `0.0.0.0` (IPv4 only) and `localhost` may resolve to `::1`
/// on dual-stack systems, causing connection failures.
pub const DEFAULT_MEMORIA_URL: &str = "http://127.0.0.1:8100";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppSettings {
    pub matrixone: MatrixOneSettings,
    pub redis: RedisSettings,
    pub application: ApplicationSettings,
    pub jwt: JwtSettings,
    pub embedding: EmbeddingSettings,
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
                password: value_or_default(&lookup, "MATRIXONE_PASSWORD", super::runtime_limits::DEV_MATRIXONE_PASSWORD),
                database: value_or_default(&lookup, "MATRIXONE_DATABASE", "dev_agent"),
            },
            redis: RedisSettings {
                host: value_or_default(&lookup, "REDIS_HOST", "localhost"),
                port: parse_or_default(&lookup, "REDIS_PORT", 6379)?,
                password: optional_value(&lookup, "REDIS_PASSWORD"),
            },
            application: ApplicationSettings {
                app_env: value_or_default(&lookup, "APP_ENV", "development"),
                log_level: value_or_default(&lookup, "LOG_LEVEL", "DEBUG"),
                secret_key: value_or_default(
                    &lookup,
                    "SECRET_KEY",
                    "dev-secret-key-change-in-production",
                ),
            },
            jwt: JwtSettings::from_lookup(&lookup)?,
            embedding: EmbeddingSettings::from_lookup(&lookup)?,
            github_token: optional_value(&lookup, "GITHUB_TOKEN"),
            memoria_base_url: value_or_default(&lookup, "MEMORIA_BASE_URL", DEFAULT_MEMORIA_URL),
            memoria_master_key: optional_value(&lookup, "MEMORIA_MASTER_KEY"),
            chat_turn_bridge_url: optional_value(&lookup, "CHAT_TURN_BRIDGE_URL"),
            chat_turn_bridge_secret: value_or_default(
                &lookup,
                "CHAT_TURN_BRIDGE_SECRET",
                "dev-bridge-secret-change-me",
            ),
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
pub struct RedisSettings {
    pub host: String,
    pub port: u16,
    pub password: Option<String>,
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
        let secret = value_or_default(lookup, "JWT_SECRET_KEY", "change-me-in-production");
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
pub struct EmbeddingSettings {
    pub provider: String,
    pub model: String,
    pub dim: u32,
    pub api_key: String,
    pub base_url: Option<String>,
}

impl EmbeddingSettings {
    fn from_lookup<F>(lookup: &F) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let model = value_or_default(lookup, "EMBEDDING_MODEL", "BAAI/bge-m3");
        let configured_dim = parse_or_default(lookup, "EMBEDDING_DIM", 0_u32)?;
        let dim = if configured_dim == 0 {
            infer_embedding_dim(&model)?
        } else {
            configured_dim
        };

        Ok(Self {
            provider: value_or_default(lookup, "EMBEDDING_PROVIDER", "openai"),
            model,
            dim,
            api_key: value_or_default(lookup, "EMBEDDING_API_KEY", ""),
            base_url: optional_value(lookup, "EMBEDDING_BASE_URL"),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigError {
    InvalidInteger { key: &'static str, value: String },
    UnknownEmbeddingDimension { model: String },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInteger { key, value } => {
                write!(f, "invalid integer for {key}: {value}")
            }
            Self::UnknownEmbeddingDimension { model } => write!(
                f,
                "embedding_dim is not set and model {model:?} is not in KNOWN_DIMENSIONS. Please set EMBEDDING_DIM explicitly."
            ),
        }
    }
}

impl Error for ConfigError {}

fn value_or_default<F>(lookup: &F, key: &'static str, default: &str) -> String
where
    F: Fn(&str) -> Option<String>,
{
    lookup(key).unwrap_or_else(|| default.to_string())
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

fn infer_embedding_dim(model: &str) -> Result<u32, ConfigError> {
    known_embedding_dimension(model).ok_or_else(|| ConfigError::UnknownEmbeddingDimension {
        model: model.to_string(),
    })
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

fn known_embedding_dimension(model: &str) -> Option<u32> {
    match model {
        "BAAI/bge-m3" => Some(1024),
        "BAAI/bge-large-en-v1.5" => Some(1024),
        "BAAI/bge-large-zh-v1.5" => Some(1024),
        "BAAI/bge-base-en-v1.5" => Some(768),
        "BAAI/bge-base-zh-v1.5" => Some(768),
        "BAAI/bge-small-en-v1.5" => Some(512),
        "BAAI/bge-small-zh-v1.5" => Some(512),
        "sentence-transformers/all-MiniLM-L6-v2" => Some(384),
        "sentence-transformers/all-MiniLM-L12-v2" => Some(384),
        "sentence-transformers/all-mpnet-base-v2" => Some(768),
        "sentence-transformers/paraphrase-multilingual-MiniLM-L12-v2" => Some(384),
        "sentence-transformers/paraphrase-multilingual-mpnet-base-v2" => Some(768),
        "text-embedding-ada-002" => Some(1536),
        "embed-english-v3.0" => Some(1024),
        "embed-multilingual-v3.0" => Some(1024),
        "embed-english-light-v3.0" => Some(384),
        "embed-multilingual-light-v3.0" => Some(384),
        "jina-embeddings-v2-base-en" => Some(768),
        "jina-embeddings-v3" => Some(1024),
        "nomic-embed-text-v1" => Some(768),
        "nomic-embed-text-v1.5" => Some(768),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
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
    fn jwt_settings_match_runtime_defaults_and_padding() {
        let settings = AppSettings::from_map(&HashMap::new()).expect("defaults should parse");

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
}

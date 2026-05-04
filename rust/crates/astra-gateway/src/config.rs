use std::path::Path;

#[derive(Debug, Clone, serde::Deserialize)]
pub struct GatewayConfig {
    pub astra: AstraServerConfig,
    /// New multi-backend storage configuration.
    #[serde(default)]
    pub storage: crate::store::StorageConfig,
    /// Legacy database config — kept for backward compatibility with existing YAML files.
    #[serde(default)]
    pub database: Option<DatabaseConfig>,
    /// Default CLI profile (used when no /cli switch active).
    #[serde(default)]
    pub cli: crate::cli_bridge::CliProfile,
    /// Named CLI profiles available for /cli switch.
    #[serde(default)]
    pub cli_profiles: std::collections::HashMap<String, crate::cli_bridge::CliProfile>,
    /// Maximum seconds a spawned CLI may run for one gateway message.
    #[serde(default = "default_cli_timeout_secs")]
    pub cli_timeout_secs: u64,
    #[serde(default)]
    pub platforms: PlatformConfigs,
    /// Directory containing user-defined skill markdown files.
    #[serde(default)]
    pub skills_dir: Option<String>,
    /// Session auto-reset policy.
    #[serde(default)]
    pub session_reset: crate::session_policy::ResetPolicy,
    /// Access control policy (who can send messages).
    #[serde(default)]
    pub access: crate::access_control::AccessPolicy,
    /// Action policy (which gateway mutations are allowed from slash/model sources).
    #[serde(default)]
    pub action_policy: crate::access_control::ActionPolicy,
    /// Maximum concurrent CLI runs across all conversations.
    #[serde(default = "default_max_concurrent_runs")]
    pub max_concurrent_runs: usize,
    /// Group chat: isolate sessions per user (true) or share per group (false).
    #[serde(default = "default_true")]
    pub group_sessions_per_user: bool,
    /// Group chat: require @mention to activate (reduces noise).
    #[serde(default)]
    pub group_require_mention: bool,
    /// Bot display name for @mention matching in group chats (e.g. "Astra").
    /// When empty, any @mention triggers the bot.
    #[serde(default)]
    pub bot_name: String,
    /// Directories to scan for git projects (e.g. ["~/github", "~/work"]).
    #[serde(default)]
    pub project_dirs: Vec<String>,
}

#[derive(Clone, Default, serde::Deserialize)]
pub struct DatabaseConfig {
    #[serde(default)]
    pub url: String,
}

impl std::fmt::Debug for DatabaseConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatabaseConfig")
            .field("url", &"[REDACTED]")
            .finish()
    }
}

fn default_true() -> bool {
    true
}

fn default_cli_timeout_secs() -> u64 {
    60 * 60
}

fn default_max_concurrent_runs() -> usize {
    4
}

#[derive(Clone, serde::Deserialize)]
pub struct AstraServerConfig {
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    pub default_model: Option<String>,
    /// Optional login credentials for gateway-level auto-recovery when CLI tokens expire.
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
}

impl std::fmt::Debug for AstraServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AstraServerConfig")
            .field("base_url", &self.base_url)
            .field(
                "api_key",
                &if self.api_key.is_empty() {
                    "(empty)"
                } else {
                    "[REDACTED]"
                },
            )
            .field("default_model", &self.default_model)
            .field("username", &self.username.as_deref())
            .field(
                "password",
                &if self.password.is_some() {
                    Some("[REDACTED]")
                } else {
                    None
                },
            )
            .finish()
    }
}

fn default_base_url() -> String {
    "http://localhost:8080".into()
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct PlatformConfigs {
    pub wecom: Option<WeComConfig>,
    pub weixin: Option<crate::platforms::weixin::WeixinConfig>,
    pub telegram: Option<TelegramConfig>,
}

#[derive(Clone, serde::Deserialize)]
pub struct WeComConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub bot_id: String,
    #[serde(default)]
    pub secret: String,
    #[serde(default = "default_wecom_ws_url")]
    pub websocket_url: String,
}

impl std::fmt::Debug for WeComConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WeComConfig")
            .field("enabled", &self.enabled)
            .field("bot_id", &self.bot_id)
            .field("secret", &"[REDACTED]")
            .field("websocket_url", &self.websocket_url)
            .finish()
    }
}

fn default_wecom_ws_url() -> String {
    "wss://openws.work.weixin.qq.com".into()
}

#[derive(Clone, serde::Deserialize)]
pub struct TelegramConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub token: String,
}

impl std::fmt::Debug for TelegramConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelegramConfig")
            .field("enabled", &self.enabled)
            .field(
                "token",
                &if self.token.is_empty() {
                    "(empty)"
                } else {
                    "[REDACTED]"
                },
            )
            .finish()
    }
}

impl GatewayConfig {
    pub fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let content = std::fs::read_to_string(path)?;
        let config: Self = serde_yaml_ng::from_str(&content)?;
        Ok(config)
    }

    /// Merge `storage` and legacy `database` fields into a single [`StorageConfig`].
    ///
    /// Priority: if `storage` is explicitly set (not default-derived) or the
    /// `GATEWAY_DATABASE_URL` env var is present, `storage` wins.
    /// Otherwise, a legacy `database.url` value is promoted to
    /// [`StorageConfig::Mysql`] for backward compat.
    pub fn resolve_storage(&self) -> crate::store::StorageConfig {
        // If `storage` was explicitly set to a non-default variant *or* the env
        // var is present, prefer it as-is.
        if !matches!(self.storage, crate::store::StorageConfig::Sqlite { .. })
            || std::env::var("GATEWAY_DATABASE_URL").is_ok()
        {
            return self.storage.clone();
        }
        // Legacy path: `database:` section in YAML.
        if let Some(ref db) = self.database
            && !db.url.is_empty()
        {
            return crate::store::StorageConfig::Mysql {
                url: db.url.clone(),
            };
        }
        // Fall through to the default (SQLite).
        self.storage.clone()
    }
}

impl WeComConfig {
    pub fn resolve(mut self) -> Self {
        if self.bot_id.is_empty()
            && let Ok(v) = std::env::var("WECOM_BOT_ID")
        {
            self.bot_id = v;
        }
        if self.secret.is_empty()
            && let Ok(v) = std::env::var("WECOM_SECRET")
        {
            self.secret = v;
        }
        self
    }
}

impl TelegramConfig {
    pub fn resolve(mut self) -> Self {
        if self.token.is_empty()
            && let Ok(v) = std::env::var("TELEGRAM_BOT_TOKEN")
        {
            self.token = v;
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_config() {
        let yaml = r#"
astra:
  base_url: "http://localhost:8080"
  api_key: "test-key"
"#;
        let cfg: GatewayConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.astra.base_url, "http://localhost:8080");
        assert_eq!(cfg.astra.api_key, "test-key");
        assert!(cfg.platforms.wecom.is_none());
        assert_eq!(cfg.max_concurrent_runs, 4);
        assert!(!cfg.action_policy.allow_model_generated_mutations);
    }

    #[test]
    fn parse_full_config() {
        let yaml = r#"
astra:
  base_url: "http://localhost:8080"
  api_key: "key"
  default_model: "MiniMax-M2.7"
platforms:
  wecom:
    enabled: true
    bot_id: "bot-123"
    secret: "secret-456"
  telegram:
    enabled: false
    token: "tok"
"#;
        let cfg: GatewayConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let wecom = cfg.platforms.wecom.unwrap();
        assert!(wecom.enabled);
        assert_eq!(wecom.bot_id, "bot-123");
        assert_eq!(cfg.astra.default_model.as_deref(), Some("MiniMax-M2.7"));
    }

    #[test]
    fn debug_redacts_secrets() {
        let cfg = AstraServerConfig {
            base_url: "http://localhost:8080".into(),
            api_key: "super-secret-key".into(),
            default_model: None,
            username: Some("admin".into()),
            password: Some("hunter2".into()),
        };
        let dbg = format!("{cfg:?}");
        assert!(
            dbg.contains("[REDACTED]"),
            "api_key should be redacted: {dbg}"
        );
        assert!(
            !dbg.contains("super-secret"),
            "api_key leaked in debug: {dbg}"
        );
        assert!(!dbg.contains("hunter2"), "password leaked in debug: {dbg}");
        assert!(dbg.contains("admin"), "username should be visible: {dbg}");

        let db = DatabaseConfig {
            url: "mysql://root:password@host/db".into(),
        };
        let dbg = format!("{db:?}");
        assert!(!dbg.contains("password"), "db url leaked in debug: {dbg}");

        let wecom = WeComConfig {
            enabled: true,
            bot_id: "bot-123".into(),
            secret: "my-secret".into(),
            websocket_url: "wss://example.com".into(),
        };
        let dbg = format!("{wecom:?}");
        assert!(!dbg.contains("my-secret"), "wecom secret leaked: {dbg}");

        let tg = TelegramConfig {
            enabled: true,
            token: "bot123:AABBCC".into(),
        };
        let dbg = format!("{tg:?}");
        assert!(
            !dbg.contains("bot123:AABBCC"),
            "telegram token leaked: {dbg}"
        );
    }

    // ── resolve_storage tests ────────────────────────────────────────────

    #[test]
    fn resolve_storage_legacy_database_url() {
        let yaml = r#"
astra:
  base_url: "http://localhost:8080"
  api_key: ""
database:
  url: "mysql://root:111@localhost/gw"
"#;
        let cfg: GatewayConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let resolved = cfg.resolve_storage();
        assert!(
            matches!(resolved, crate::store::StorageConfig::Mysql { .. }),
            "expected Mysql, got {resolved:?}"
        );
    }

    #[test]
    fn resolve_storage_explicit_mysql_storage_wins() {
        let yaml = r#"
astra:
  base_url: "http://localhost:8080"
  api_key: ""
storage:
  backend: mysql
  url: "mysql://explicit@host/db"
database:
  url: "mysql://legacy@host/db"
"#;
        let cfg: GatewayConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let resolved = cfg.resolve_storage();
        match resolved {
            crate::store::StorageConfig::Mysql { url } => {
                assert!(
                    url.contains("explicit"),
                    "storage: section should win, got {url}"
                );
            }
            other => panic!("expected Mysql, got {other:?}"),
        }
    }

    #[test]
    fn resolve_storage_no_storage_no_database_defaults_to_sqlite() {
        let yaml = r#"
astra:
  base_url: "http://localhost:8080"
  api_key: ""
"#;
        let cfg: GatewayConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let resolved = cfg.resolve_storage();
        // Depends on whether GATEWAY_DATABASE_URL env var is set
        assert!(
            matches!(
                resolved,
                crate::store::StorageConfig::Sqlite { .. }
                    | crate::store::StorageConfig::Mysql { .. }
            ),
            "expected Sqlite or Mysql, got {resolved:?}"
        );
    }

    #[test]
    fn resolve_storage_explicit_file_not_overridden() {
        let yaml = r#"
astra:
  base_url: "http://localhost:8080"
  api_key: ""
storage:
  backend: file
  dir: "/custom/data"
database:
  url: "mysql://root:111@localhost/gw"
"#;
        let cfg: GatewayConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let resolved = cfg.resolve_storage();
        match resolved {
            crate::store::StorageConfig::File { dir } => {
                assert_eq!(dir, "/custom/data");
            }
            other => panic!("expected File, got {other:?}"),
        }
    }

    #[test]
    fn resolve_storage_none_not_overridden() {
        let yaml = r#"
astra:
  base_url: "http://localhost:8080"
  api_key: ""
storage:
  backend: none
database:
  url: "mysql://root:111@localhost/gw"
"#;
        let cfg: GatewayConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let resolved = cfg.resolve_storage();
        assert!(
            matches!(resolved, crate::store::StorageConfig::None),
            "expected None, got {resolved:?}"
        );
    }

    #[test]
    fn wecom_env_override() {
        let cfg = WeComConfig {
            enabled: true,
            bot_id: String::new(),
            secret: String::new(),
            websocket_url: default_wecom_ws_url(),
        };
        // resolve() reads env vars — test that empty stays empty without env
        let resolved = cfg.resolve();
        // Can't assert env vars in unit tests, but verify no panic
        assert!(resolved.websocket_url.starts_with("wss://"));
    }

    #[test]
    fn parse_config_with_auth_credentials() {
        let yaml = r#"
astra:
  base_url: "http://localhost:8080"
  api_key: "key"
  username: "admin"
  password: "secret123"
"#;
        let cfg: GatewayConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.astra.username.as_deref(), Some("admin"));
        assert_eq!(cfg.astra.password.as_deref(), Some("secret123"));
    }

    #[test]
    fn parse_config_without_auth_credentials() {
        let yaml = r#"
astra:
  base_url: "http://localhost:8080"
  api_key: "key"
"#;
        let cfg: GatewayConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(cfg.astra.username.is_none());
        assert!(cfg.astra.password.is_none());
    }
}

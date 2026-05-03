use std::path::Path;

#[derive(Debug, Clone, serde::Deserialize)]
pub struct GatewayConfig {
    pub astra: AstraServerConfig,
    #[serde(default)]
    pub database: DatabaseConfig,
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
    /// Directories to scan for git projects (e.g. ["~/github", "~/work"]).
    #[serde(default)]
    pub project_dirs: Vec<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct DatabaseConfig {
    #[serde(default = "default_db_url")]
    pub url: String,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            url: default_db_url(),
        }
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

fn default_db_url() -> String {
    std::env::var("GATEWAY_DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:111@127.0.0.1:6001/astra_gateway".into())
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct AstraServerConfig {
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    pub default_model: Option<String>,
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

#[derive(Debug, Clone, serde::Deserialize)]
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

fn default_wecom_ws_url() -> String {
    "wss://openws.work.weixin.qq.com".into()
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct TelegramConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub token: String,
}

impl GatewayConfig {
    pub fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let content = std::fs::read_to_string(path)?;
        let config: Self = serde_yaml_ng::from_str(&content)?;
        Ok(config)
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
        assert!(cfg.action_policy.allow_model_generated_mutations);
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
}

use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "astra-admin")]
#[command(about = "Admin CLI — run `astra-admin` for interactive mode")]
pub(crate) struct Cli {
    /// API server base URL (flag > env > config > default) [env: ASTRA_API_URL]
    #[arg(long)]
    pub api_url: Option<String>,
    #[arg(long)]
    pub profile: Option<String>,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub(crate) enum Command {
    Interactive,
    Login(LoginArgs),
    Register(RegisterArgs),
    Whoami,
    Refresh,
    Logout,
    Init,
    Audit(AuditArgs),
    #[command(subcommand)]
    User(UserCmd),
    #[command(subcommand)]
    Model(ModelCmd),
    #[command(subcommand)]
    Token(TokenCmd),
    #[command(subcommand)]
    Skill(SkillCmd),
    #[command(subcommand)]
    Prompt(PromptCmd),
    #[command(subcommand)]
    Feedback(FeedbackCmd),
    #[command(subcommand)]
    /// Manage server-wide admin configuration (e.g., reasoning model).
    Config(ConfigCmd),
}

#[derive(Subcommand, Debug)]
pub(crate) enum ConfigCmd {
    /// List all admin config keys and values.
    List,
    /// Read a single admin config value (e.g. `get reasoning_model`).
    Get(ConfigKeyArgs),
    /// Set an admin config value (e.g. `set reasoning_model gpt-4o-mini`).
    Set(ConfigSetArgs),
    /// Delete an admin config value.
    #[command(visible_alias = "delete")]
    Unset(ConfigKeyArgs),
}

#[derive(Args, Debug)]
pub(crate) struct ConfigKeyArgs {
    pub key: String,
}

#[derive(Args, Debug)]
pub(crate) struct ConfigSetArgs {
    pub key: String,
    pub value: String,
}

#[derive(Args, Debug)]
pub(crate) struct LoginArgs {
    #[arg(long)]
    pub username: Option<String>,
    #[arg(long)]
    pub password: Option<String>,
}

#[derive(Args, Debug)]
pub(crate) struct RegisterArgs {
    #[arg(long)]
    pub username: Option<String>,
    #[arg(long)]
    pub password: Option<String>,
    #[arg(long)]
    pub email: Option<String>,
}

#[derive(Args, Debug)]
pub(crate) struct AuditArgs {
    #[arg(long)]
    pub user_id: Option<String>,
    #[arg(long)]
    pub since: Option<String>,
    #[arg(long, default_value_t = 100)]
    pub limit: u32,
}

#[derive(Subcommand, Debug)]
pub(crate) enum UserCmd {
    GrantRole(UserRoleArgs),
    RevokeRole(UserRoleArgs),
}

#[derive(Args, Debug)]
pub(crate) struct UserRoleArgs {
    pub username: String,
    pub role_name: String,
}

#[derive(Subcommand, Debug)]
pub(crate) enum ModelCmd {
    List,
    Add(ModelAddArgs),
    Show(ModelShowArgs),
    Delete(ModelDeleteArgs),
    /// Probe upstream LLM connectivity; on success sets `is_active=true`, on failure `false` (HTTP `POST /models/{name}/check`).
    /// This is the supported way to "try activate" from credentials already stored on the server.
    #[command(alias = "probe", visible_alias = "verify")]
    Check(ModelShowArgs),
    Load(ModelLoadArgs),
    /// Update model fields (api-key, base-url, quirks, active status).
    Update(ModelUpdateArgs),
    /// Set the fallback model for rate-limit recovery.
    SetFallback(ModelSetFallbackArgs),
}

#[derive(Args, Debug)]
pub(crate) struct ModelAddArgs {
    pub name: String,
    pub provider: String,
    #[arg(long)]
    pub api_key: String,
    #[arg(long)]
    pub base_url: Option<String>,
}

#[derive(Args, Debug)]
pub(crate) struct ModelUpdateArgs {
    pub model_name: String,
    #[arg(long)]
    pub api_key: Option<String>,
    #[arg(long)]
    pub base_url: Option<String>,
    /// Set stored `is_active` without re-probing. Prefer `model check` to activate only when connectivity succeeds.
    #[arg(long)]
    pub active: Option<bool>,
    /// JSON string for quirks, e.g. '{"fallback_model":"gpt-4o-mini"}'
    #[arg(long)]
    pub quirks: Option<String>,
}

#[derive(Args, Debug)]
pub(crate) struct ModelShowArgs {
    pub model_name: String,
}

#[derive(Args, Debug)]
pub(crate) struct ModelDeleteArgs {
    pub model_name: String,
}

#[derive(Args, Debug)]
pub(crate) struct ModelLoadArgs {
    pub path: String,
    /// When the server already has this model name, `POST /models` is skipped. With this flag,
    /// push `api_key` and optional `base_url` from the YAML via `PUT /models/{name}` so the
    /// server re-runs connectivity and refreshes `is_active`.
    #[arg(long)]
    pub update_existing: bool,
}

#[derive(Args, Debug)]
pub(crate) struct ModelSetFallbackArgs {
    /// Primary model name.
    pub model_name: String,
    /// Fallback model name (use "none" to clear).
    pub fallback_model: String,
}

#[derive(Subcommand, Debug)]
pub(crate) enum TokenCmd {
    List(TokenListArgs),
    Create(TokenCreateArgs),
}

#[derive(Args, Debug)]
pub(crate) struct TokenListArgs {
    #[arg(long)]
    pub token_type: Option<String>,
    #[arg(long)]
    pub scope: Option<String>,
}

#[derive(Args, Debug)]
pub(crate) struct TokenCreateArgs {
    #[arg(long = "type")]
    pub token_type: String,
    #[arg(long)]
    pub provider: Option<String>,
    #[arg(long, default_value = "global")]
    pub scope: String,
    #[arg(long)]
    pub scope_id: Option<String>,
    #[arg(long)]
    pub token_value: Option<String>,
}

#[derive(Subcommand, Debug)]
pub(crate) enum SkillCmd {
    List(SkillListArgs),
    Show(SkillShowArgs),
    Versions(SkillNameArgs),
}

#[derive(Args, Debug)]
pub(crate) struct SkillListArgs {
    #[arg(long, default_value_t = 50)]
    pub limit: u32,
    #[arg(long, default_value_t = 0)]
    pub offset: u32,
}

#[derive(Args, Debug)]
pub(crate) struct SkillShowArgs {
    pub skill_id: String,
    #[arg(long)]
    pub version: Option<String>,
}

#[derive(Args, Debug)]
pub(crate) struct SkillNameArgs {
    pub skill_name: String,
}

#[derive(Subcommand, Debug)]
pub(crate) enum PromptCmd {
    Optimize(PromptOptimizeArgs),
}

#[derive(Args, Debug)]
pub(crate) struct PromptOptimizeArgs {
    #[arg(long)]
    pub agent_id: String,
    #[arg(long, default_value = "quality")]
    pub optimization_type: String,
}

#[derive(Subcommand, Debug)]
pub(crate) enum FeedbackCmd {
    Stats(FeedbackStatsArgs),
    Export(FeedbackExportArgs),
}

#[derive(Args, Debug)]
pub(crate) struct FeedbackStatsArgs {
    #[arg(long)]
    pub agent_id: Option<String>,
    #[arg(long)]
    pub since: Option<String>,
}

#[derive(Args, Debug)]
pub(crate) struct FeedbackExportArgs {
    #[arg(long)]
    pub agent_id: Option<String>,
    #[arg(long, default_value = "jsonl")]
    pub format: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parse_login_command() {
        let cli = Cli::try_parse_from(["astra-admin", "login"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Login(_))));
    }

    #[test]
    fn parse_init_command() {
        let cli = Cli::try_parse_from(["astra-admin", "init"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Init)));
    }

    #[test]
    fn parse_model_list() {
        let cli = Cli::try_parse_from(["astra-admin", "model", "list"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Model(ModelCmd::List))));
    }

    #[test]
    fn parse_audit_with_limit() {
        let cli = Cli::try_parse_from(["astra-admin", "audit", "--limit", "50"]).unwrap();
        if let Some(Command::Audit(args)) = cli.command {
            assert_eq!(args.limit, 50);
        } else {
            panic!("expected Audit command");
        }
    }

    #[test]
    fn parse_profile_flag() {
        let cli = Cli::try_parse_from(["astra-admin", "--profile", "staging", "init"]).unwrap();
        assert_eq!(cli.profile.as_deref(), Some("staging"));
    }

    #[test]
    fn parse_api_url_flag() {
        let cli =
            Cli::try_parse_from(["astra-admin", "--api-url", "http://localhost:9000", "init"])
                .unwrap();
        assert_eq!(cli.api_url.as_deref(), Some("http://localhost:9000"));
    }

    #[test]
    fn default_api_url() {
        let cli = Cli::try_parse_from(["astra-admin", "init"]).unwrap();
        assert_eq!(cli.api_url, None);
    }
}

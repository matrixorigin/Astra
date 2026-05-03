//! Access control for gateway — allowlist-based user filtering.
//!
//! Policies:
//! - `open`: anyone can send messages
//! - `allowlist`: only listed user IDs
//! - `disabled`: reject all messages

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AccessPolicy {
    #[default]
    Open,
    Allowlist {
        users: Vec<String>,
    },
    Disabled,
}

impl AccessPolicy {
    pub fn is_allowed(&self, user_id: &str) -> bool {
        match self {
            Self::Open => true,
            Self::Disabled => false,
            Self::Allowlist { users } => users.iter().any(|u| u.trim() == user_id),
        }
    }

    pub fn rejection_message(&self) -> &'static str {
        match self {
            Self::Disabled => "⚠️ 此网关已停用。",
            Self::Allowlist { .. } => "⚠️ 你没有使用此服务的权限。请联系管理员。",
            Self::Open => "",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionSource {
    SlashCommand,
    ModelGenerated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionCapability {
    SessionMutation,
    CronMutation,
    DurableTaskMutation,
    SkillMutation,
    WorkspaceMutation,
    CliMutation,
    ModelMutation,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActionPolicy {
    /// Allow user-issued slash command mutations.
    #[serde(default = "default_allow_slash_mutations")]
    pub allow_slash_mutations: bool,
    /// Allow model-generated [[GATEWAY:...]] mutation tags. Slash commands stay allowed.
    #[serde(default = "default_allow_model_generated_mutations")]
    pub allow_model_generated_mutations: bool,
    /// If non-empty, /workspace and workspace_set may only target these roots.
    #[serde(default)]
    pub workspace_roots: Vec<String>,
}

fn default_allow_model_generated_mutations() -> bool {
    true
}

fn default_allow_slash_mutations() -> bool {
    true
}

impl Default for ActionPolicy {
    fn default() -> Self {
        Self {
            allow_slash_mutations: default_allow_slash_mutations(),
            allow_model_generated_mutations: default_allow_model_generated_mutations(),
            workspace_roots: Vec::new(),
        }
    }
}

impl ActionPolicy {
    pub fn check(&self, source: ActionSource, capability: ActionCapability) -> Result<(), String> {
        if source == ActionSource::SlashCommand
            && !self.allow_slash_mutations
            && capability.is_mutation()
        {
            return Err("🔒 网关策略已禁用 slash 修改操作。请联系管理员。".into());
        }
        if source == ActionSource::ModelGenerated
            && !self.allow_model_generated_mutations
            && capability.is_mutation()
        {
            return Err("🔒 为安全起见，模型生成的修改操作已被网关策略拒绝。请使用对应的 slash 命令手动执行。".into());
        }
        Ok(())
    }

    pub fn workspace_allowed(&self, path: &std::path::Path) -> Result<(), String> {
        if self.workspace_roots.is_empty() {
            return Ok(());
        }
        let canonical = path
            .canonicalize()
            .map_err(|e| format!("⚠️ 无法解析工作目录: {e}"))?;
        let allowed = self.workspace_roots.iter().any(|root| {
            let expanded = expand_home(root);
            std::path::Path::new(&expanded)
                .canonicalize()
                .map(|root| canonical.starts_with(root))
                .unwrap_or(false)
        });
        if allowed {
            Ok(())
        } else {
            Err("🔒 工作目录不在允许的 workspace_roots 内。请联系管理员调整网关配置。".into())
        }
    }
}

impl ActionCapability {
    pub fn is_mutation(self) -> bool {
        matches!(
            self,
            Self::SessionMutation
                | Self::CronMutation
                | Self::DurableTaskMutation
                | Self::SkillMutation
                | Self::WorkspaceMutation
                | Self::CliMutation
                | Self::ModelMutation
        )
    }
}

fn expand_home(path: &str) -> String {
    if path.starts_with('~') {
        let home = std::env::var("HOME").unwrap_or_default();
        path.replacen('~', &home, 1)
    } else {
        path.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_allows_everyone() {
        let policy = AccessPolicy::Open;
        assert!(policy.is_allowed("anyone"));
        assert!(policy.is_allowed(""));
    }

    #[test]
    fn disabled_rejects_everyone() {
        let policy = AccessPolicy::Disabled;
        assert!(!policy.is_allowed("anyone"));
    }

    #[test]
    fn allowlist_exact_match() {
        let policy = AccessPolicy::Allowlist {
            users: vec!["user_a".into(), "user_b".into()],
        };
        assert!(policy.is_allowed("user_a"));
        assert!(policy.is_allowed("user_b"));
        assert!(!policy.is_allowed("user_c"));
    }

    #[test]
    fn allowlist_does_not_allow_partial_match() {
        let policy = AccessPolicy::Allowlist {
            users: vec!["wxid_abc".into()],
        };
        assert!(!policy.is_allowed("prefix_wxid_abc_suffix"));
        assert!(!policy.is_allowed("wxid_xyz"));
    }

    #[test]
    fn allowlist_empty_rejects_all() {
        let policy = AccessPolicy::Allowlist { users: vec![] };
        assert!(!policy.is_allowed("anyone"));
    }

    #[test]
    fn rejection_messages() {
        assert!(!AccessPolicy::Disabled.rejection_message().is_empty());
        assert!(
            !AccessPolicy::Allowlist { users: vec![] }
                .rejection_message()
                .is_empty()
        );
        assert!(AccessPolicy::Open.rejection_message().is_empty());
    }

    #[test]
    fn default_is_open() {
        assert_eq!(AccessPolicy::default(), AccessPolicy::Open);
    }

    #[test]
    fn serde_roundtrip() {
        let policy = AccessPolicy::Allowlist {
            users: vec!["u1".into()],
        };
        let yaml = serde_yaml_ng::to_string(&policy).unwrap();
        let parsed: AccessPolicy = serde_yaml_ng::from_str(&yaml).unwrap();
        assert_eq!(parsed, policy);
    }

    #[test]
    fn action_policy_denies_model_mutations_when_disabled() {
        let policy = ActionPolicy {
            allow_slash_mutations: true,
            allow_model_generated_mutations: false,
            workspace_roots: Vec::new(),
        };
        assert!(
            policy
                .check(ActionSource::SlashCommand, ActionCapability::CronMutation)
                .is_ok()
        );
        assert!(
            policy
                .check(ActionSource::ModelGenerated, ActionCapability::CronMutation)
                .unwrap_err()
                .contains("拒绝")
        );
    }
}

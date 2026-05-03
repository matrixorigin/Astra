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
}

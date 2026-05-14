//! Match target selection for scoped approvals.
//!
//! Approval scope answers "how long / where is this remembered?"
//! [`crate::permission_scope::AllowScope`]. This module answers
//! "what future request does that approval match?" so every caller
//! (TUI preview, turn/session override, project/user persistence,
//! and audit logging) uses the same rule shape.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::approval_fingerprint::ApprovalFingerprint;
use crate::cloud_approval_policy::{CloudGatedToolKind, cloud_gated_tool_kind_with_args};
use crate::parallel_tool_exec::is_read_only_tool_with_args;
use crate::permission_rule_grammar::{PermissionRuleV2, serialize_rule_v2};
use crate::permission_scope::AllowScope;
use crate::tool_argument_hints::{
    command_hint_from_args, normalized_argv_prefix, path_hint_from_args,
};

/// The second dimension of an approval choice: what future tool call
/// should the selected scope apply to?
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum AllowMatchTarget {
    /// Match exactly this command/path-shaped request.
    Exact,
    /// Match every request for this tool.
    Tool,
    /// Match commands beginning with this argv prefix, or paths matching
    /// this glob/prefix for path-shaped tools.
    Prefix(String),
}

impl AllowMatchTarget {
    #[must_use]
    pub fn audit_label(&self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Tool => "tool",
            Self::Prefix(_) => "prefix",
        }
    }
}

/// The default used by legacy one-step Always paths. The TUI now sends
/// explicit match targets, but non-TUI and older paths still need a
/// conservative, content-aware choice.
#[must_use]
pub fn default_match_target(tool_name: &str, args: &Value) -> AllowMatchTarget {
    match cloud_gated_tool_kind_with_args(tool_name, Some(args)) {
        Some(CloudGatedToolKind::Execute) => command_hint_from_args(args)
            .map(|cmd| normalized_argv_prefix(&cmd))
            .filter(|prefix| !prefix.is_empty())
            .map(AllowMatchTarget::Prefix)
            .unwrap_or(AllowMatchTarget::Tool),
        Some(CloudGatedToolKind::Write) => {
            if path_hint_from_args(args).is_some() {
                AllowMatchTarget::Exact
            } else {
                AllowMatchTarget::Tool
            }
        }
        None => AllowMatchTarget::Tool,
    }
}

#[must_use]
pub fn custom_prefix_source(tool_name: &str, args: &Value) -> String {
    match cloud_gated_tool_kind_with_args(tool_name, Some(args)) {
        Some(CloudGatedToolKind::Execute) => command_hint_from_args(args)
            .map(ToOwned::to_owned)
            .unwrap_or_default(),
        Some(CloudGatedToolKind::Write) => path_hint_from_args(args).unwrap_or_default(),
        None => String::new(),
    }
}

#[must_use]
pub fn is_valid_custom_prefix(prefix: &str, source: &str) -> bool {
    if prefix.is_empty() {
        return false;
    }
    source.starts_with(prefix)
}

#[must_use]
pub fn allow_rule_for_match_target(
    tool_name: &str,
    args: &Value,
    target: &AllowMatchTarget,
) -> String {
    match target {
        AllowMatchTarget::Tool => tool_name.to_string(),
        AllowMatchTarget::Exact => {
            exact_rule(tool_name, args).unwrap_or_else(|| tool_name.to_string())
        }
        AllowMatchTarget::Prefix(prefix) => {
            prefix_rule(tool_name, args, prefix).unwrap_or_else(|| tool_name.to_string())
        }
    }
}

#[must_use]
pub fn fingerprint_for_match_target(
    tool_name: &str,
    args: &Value,
    target: &AllowMatchTarget,
) -> ApprovalFingerprint {
    match target {
        AllowMatchTarget::Tool => ApprovalFingerprint::bare(tool_name),
        AllowMatchTarget::Exact => match cloud_gated_tool_kind_with_args(tool_name, Some(args)) {
            Some(CloudGatedToolKind::Execute) => command_hint_from_args(args)
                .map(|cmd| {
                    ApprovalFingerprint::shell_exact(
                        tool_name,
                        &cmd,
                        is_read_only_tool_with_args(tool_name, Some(args)),
                    )
                })
                .unwrap_or_else(|| ApprovalFingerprint::bare(tool_name)),
            Some(CloudGatedToolKind::Write) => {
                ApprovalFingerprint::file_op_exact(tool_name, path_hint_from_args(args).as_deref())
            }
            None => ApprovalFingerprint::bare(tool_name),
        },
        AllowMatchTarget::Prefix(prefix) => {
            match cloud_gated_tool_kind_with_args(tool_name, Some(args)) {
                Some(CloudGatedToolKind::Execute) if !prefix.is_empty() => {
                    ApprovalFingerprint::shell_prefix(
                        tool_name,
                        prefix,
                        is_read_only_tool_with_args(tool_name, Some(args)),
                    )
                }
                Some(CloudGatedToolKind::Write) if !prefix.is_empty() => {
                    ApprovalFingerprint::file_op_pattern(tool_name, Some(prefix))
                }
                _ => ApprovalFingerprint::bare(tool_name),
            }
        }
    }
}

#[must_use]
pub fn match_target_description(
    scope: AllowScope,
    target: &AllowMatchTarget,
    tool_name: &str,
    args: &Value,
) -> String {
    let duration = match scope {
        AllowScope::OnceThisCall => "for this request",
        AllowScope::RestOfTurn => "for the rest of this turn",
        AllowScope::RestOfSession => "in this session",
        AllowScope::Project => "for this project",
        AllowScope::User => "for this user",
    };
    let is_execute = matches!(
        cloud_gated_tool_kind_with_args(tool_name, Some(args)),
        Some(CloudGatedToolKind::Execute)
    );
    match target {
        AllowMatchTarget::Exact if is_execute => {
            format!("Approve exactly this command {duration}.")
        }
        AllowMatchTarget::Exact => {
            format!("Approve exactly this tool request {duration}.")
        }
        AllowMatchTarget::Tool => {
            format!("Approve this tool for all future permission requests {duration}.")
        }
        AllowMatchTarget::Prefix(prefix) if is_execute => {
            format!("Approve commands starting with `{prefix}` {duration}.")
        }
        AllowMatchTarget::Prefix(prefix) => {
            format!("Approve paths matching `{prefix}` {duration}.")
        }
    }
}

fn exact_rule(tool_name: &str, args: &Value) -> Option<String> {
    match cloud_gated_tool_kind_with_args(tool_name, Some(args))? {
        CloudGatedToolKind::Execute => {
            let cmd = command_hint_from_args(args)?;
            Some(serialize_rule_v2(&PermissionRuleV2 {
                tool: display_tool_name(tool_name),
                argv_exact: Some(cmd.to_string()),
                argv_prefix: None,
                path_glob: None,
                op: Some("execute".to_string()),
                cwd_root: None,
                git_branch: None,
                domain: None,
                capability: None,
                extra: Default::default(),
            }))
        }
        CloudGatedToolKind::Write => {
            let path = path_hint_from_args(args)?;
            Some(serialize_rule_v2(&PermissionRuleV2 {
                tool: tool_name.to_string(),
                argv_exact: None,
                argv_prefix: None,
                path_glob: Some(path),
                op: Some("write".to_string()),
                cwd_root: None,
                git_branch: None,
                domain: None,
                capability: None,
                extra: Default::default(),
            }))
        }
    }
}

fn prefix_rule(tool_name: &str, args: &Value, prefix: &str) -> Option<String> {
    if prefix.is_empty() {
        return None;
    }
    match cloud_gated_tool_kind_with_args(tool_name, Some(args))? {
        CloudGatedToolKind::Execute => Some(serialize_rule_v2(&PermissionRuleV2 {
            tool: display_tool_name(tool_name),
            argv_exact: None,
            argv_prefix: Some(prefix.to_string()),
            path_glob: None,
            op: Some("execute".to_string()),
            cwd_root: None,
            git_branch: None,
            domain: None,
            capability: None,
            extra: Default::default(),
        })),
        CloudGatedToolKind::Write => Some(serialize_rule_v2(&PermissionRuleV2 {
            tool: tool_name.to_string(),
            argv_exact: None,
            argv_prefix: None,
            path_glob: Some(prefix.to_string()),
            op: Some("write".to_string()),
            cwd_root: None,
            git_branch: None,
            domain: None,
            capability: None,
            extra: Default::default(),
        })),
    }
}

fn display_tool_name(tool_name: &str) -> String {
    let mut chars = tool_name.chars();
    match chars.next() {
        None => tool_name.to_string(),
        Some(first) => first.to_uppercase().to_string() + chars.as_str(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission_types::{PermissionRule, RuleMatchContext};

    #[test]
    fn exact_bash_rule_does_not_match_extra_args() {
        let args = serde_json::json!({"command": "git commit -m fix"});
        let rule = allow_rule_for_match_target("bash", &args, &AllowMatchTarget::Exact);
        assert_eq!(
            rule,
            r#"Bash(argv_exact="git commit -m fix", op="execute")"#
        );
        let parsed = PermissionRule::parse(&rule);
        assert!(
            parsed.matches_with_context("bash", &RuleMatchContext::from_tool_args("bash", &args))
        );
        assert!(!parsed.matches_with_context(
            "bash",
            &RuleMatchContext::from_tool_args(
                "bash",
                &serde_json::json!({"command": "git commit -m fix --no-verify"})
            )
        ));
    }

    #[test]
    fn tool_rule_matches_any_same_tool() {
        let args = serde_json::json!({"path": "a.md"});
        assert_eq!(
            allow_rule_for_match_target("write_file", &args, &AllowMatchTarget::Tool),
            "write_file"
        );
    }

    #[test]
    fn prefix_rule_uses_user_supplied_value() {
        let args = serde_json::json!({"command": "cargo test -p astra-cli"});
        let rule = allow_rule_for_match_target(
            "bash",
            &args,
            &AllowMatchTarget::Prefix("cargo test".to_string()),
        );
        assert_eq!(rule, r#"Bash(argv_prefix="cargo test", op="execute")"#);
    }

    #[test]
    fn custom_prefix_validation_rejects_non_source_trailing_chars() {
        assert!(is_valid_custom_prefix("git ", "git status --short"));
        assert!(!is_valid_custom_prefix("git  ", "git status --short"));
        assert!(!is_valid_custom_prefix("git sx", "git status --short"));
    }

    #[test]
    fn prefix_rule_preserves_user_supplied_spacing() {
        let args = serde_json::json!({"command": "git status --short"});
        let rule = allow_rule_for_match_target(
            "bash",
            &args,
            &AllowMatchTarget::Prefix("git ".to_string()),
        );
        assert_eq!(rule, r#"Bash(argv_prefix="git ", op="execute")"#);
    }

    #[test]
    fn description_mentions_scope_and_target() {
        let text = match_target_description(
            AllowScope::RestOfSession,
            &AllowMatchTarget::Tool,
            "bash",
            &serde_json::json!({"command": "git status"}),
        );
        assert_eq!(
            text,
            "Approve this tool for all future permission requests in this session."
        );
    }
}

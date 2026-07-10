//! Match target selection for scoped approvals.
//!
//! Approval scope answers "how long / where is this remembered?"
//! [`crate::permission::scope::AllowScope`]. This module answers
//! "what future request does that approval match?" so every caller
//! (TUI preview, turn/session override, project/user persistence,
//! and audit logging) uses the same rule shape.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::approval_fingerprint::ApprovalFingerprint;
use crate::cloud::approval_policy::{
    CloudGatedToolKind, cloud_gated_tool_kind, cloud_gated_tool_kind_with_args,
};
use crate::parallel_tool_exec::is_read_only_tool_with_args;
use crate::permission::memory_profile::{permission_memory_profile, workspace_write_prefix};
use crate::permission::rule_grammar::{PermissionRuleSpec, serialize_rule};
use crate::permission::scope::AllowScope;
use crate::tool::args::hints::{command_hint_from_args, path_hint_from_args};

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

/// Conservative content-aware match target used when the user chooses
/// Always. The UI exposes this as "remember similar commands/actions"
/// instead of raw exact/prefix/tool terminology.
#[must_use]
pub fn default_match_target(tool_name: &str, args: &Value) -> AllowMatchTarget {
    permission_memory_profile(tool_name, args).match_target
}

#[must_use]
pub fn allow_rule_for_match_target(
    tool_name: &str,
    args: &Value,
    target: &AllowMatchTarget,
) -> String {
    match target {
        AllowMatchTarget::Tool => broad_rule(tool_name),
        AllowMatchTarget::Exact => {
            exact_rule(tool_name, args).unwrap_or_else(|| broad_rule(tool_name))
        }
        AllowMatchTarget::Prefix(prefix) => {
            prefix_rule(tool_name, args, prefix).unwrap_or_else(|| broad_rule(tool_name))
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
        AllowMatchTarget::Exact => match cloud_gated_tool_kind(tool_name) {
            Some(CloudGatedToolKind::Execute) => command_hint_from_args(args)
                .map(|cmd| {
                    ApprovalFingerprint::shell_exact(
                        tool_name,
                        cmd,
                        is_read_only_tool_with_args(tool_name, Some(args)),
                    )
                })
                .unwrap_or_else(|| ApprovalFingerprint::bare(tool_name)),
            Some(CloudGatedToolKind::Write) => ApprovalFingerprint::file_op_exact(
                file_write_fingerprint_tool(tool_name),
                path_hint_from_args(args).as_deref(),
            ),
            None => ApprovalFingerprint::bare(tool_name),
        },
        AllowMatchTarget::Prefix(prefix) => match cloud_gated_tool_kind(tool_name) {
            Some(CloudGatedToolKind::Execute) if !prefix.is_empty() => {
                ApprovalFingerprint::shell_prefix(
                    tool_name,
                    prefix,
                    is_read_only_tool_with_args(tool_name, Some(args)),
                )
            }
            Some(CloudGatedToolKind::Write) if !prefix.is_empty() => {
                ApprovalFingerprint::file_op_prefix(file_write_fingerprint_tool(tool_name), prefix)
            }
            _ => ApprovalFingerprint::bare(tool_name),
        },
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
        AllowScope::Project => "for this workspace",
        AllowScope::User => "for this user",
    };
    let is_execute = matches!(
        cloud_gated_tool_kind(tool_name),
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
        AllowMatchTarget::Prefix(prefix)
            if matches!(
                cloud_gated_tool_kind(tool_name),
                Some(CloudGatedToolKind::Write)
            ) && path_hint_from_args(args)
                .and_then(|path| workspace_write_prefix(&path))
                .as_deref()
                == Some(prefix.as_str()) =>
        {
            format!("Approve file edits in this workspace {duration}.")
        }
        AllowMatchTarget::Prefix(prefix) => {
            format!("Approve paths matching `{prefix}` {duration}.")
        }
    }
}

#[must_use]
pub fn remember_preview(tool_name: &str, args: &Value, location: &str) -> String {
    match (
        cloud_gated_tool_kind_with_args(tool_name, Some(args)),
        tool_name,
        default_match_target(tool_name, args),
    ) {
        (_, "bash", AllowMatchTarget::Prefix(prefix)) => {
            format!("the `{prefix}` command family {location}")
        }
        (_, "bash", AllowMatchTarget::Exact) => format!("this shell command {location}"),
        (_, "bash", AllowMatchTarget::Tool) => format!("safe shell commands {location}"),
        (Some(CloudGatedToolKind::Write), _, AllowMatchTarget::Exact) => {
            format!("this file edit {location}")
        }
        (Some(CloudGatedToolKind::Write), _, AllowMatchTarget::Prefix(prefix))
            if path_hint_from_args(args)
                .and_then(|path| workspace_write_prefix(&path))
                .as_deref()
                == Some(prefix.as_str()) =>
        {
            format!("file edits {location}")
        }
        (Some(CloudGatedToolKind::Write), _, AllowMatchTarget::Tool) => {
            format!("file edits {location}")
        }
        (Some(CloudGatedToolKind::Write), _, AllowMatchTarget::Prefix(prefix)) => {
            format!("similar file edits under `{prefix}`")
        }
        (_, _, AllowMatchTarget::Prefix(prefix)) => format!("similar `{prefix}` calls {location}"),
        (_, _, AllowMatchTarget::Exact) => format!("this `{tool_name}` action {location}"),
        (_, _, AllowMatchTarget::Tool) => format!("`{tool_name}` calls {location}"),
    }
}

fn exact_rule(tool_name: &str, args: &Value) -> Option<String> {
    match cloud_gated_tool_kind(tool_name)? {
        CloudGatedToolKind::Execute => {
            let cmd = command_hint_from_args(args)?;
            Some(serialize_rule(&PermissionRuleSpec {
                tool: display_tool_name(tool_name),
                argv_exact: Some(cmd.to_string()),
                argv_prefix: None,
                path_glob: None,
                path_prefix: None,
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
            let rule_tool = file_write_rule_tool(tool_name);
            Some(serialize_rule(&PermissionRuleSpec {
                tool: rule_tool,
                argv_exact: None,
                argv_prefix: None,
                path_glob: Some(path),
                path_prefix: None,
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

fn prefix_rule(tool_name: &str, _args: &Value, prefix: &str) -> Option<String> {
    if prefix.is_empty() {
        return None;
    }
    match cloud_gated_tool_kind(tool_name)? {
        CloudGatedToolKind::Execute => Some(serialize_rule(&PermissionRuleSpec {
            tool: display_tool_name(tool_name),
            argv_exact: None,
            argv_prefix: Some(prefix.to_string()),
            path_glob: None,
            path_prefix: None,
            op: Some("execute".to_string()),
            cwd_root: None,
            git_branch: None,
            domain: None,
            capability: None,
            extra: Default::default(),
        })),
        CloudGatedToolKind::Write => {
            let cwd_root = path_hint_from_args(_args)
                .and_then(|path| workspace_write_prefix(&path))
                .filter(|root| root == prefix);
            let rule_tool = file_write_rule_tool(tool_name);
            Some(serialize_rule(&PermissionRuleSpec {
                tool: rule_tool,
                argv_exact: None,
                argv_prefix: None,
                path_glob: None,
                path_prefix: Some(prefix.to_string()),
                op: Some("write".to_string()),
                cwd_root,
                git_branch: None,
                domain: None,
                capability: None,
                extra: Default::default(),
            }))
        }
    }
}

fn file_write_rule_tool(tool_name: &str) -> String {
    if crate::tool::categories::registry().is_file_op(tool_name) {
        "file_write".to_string()
    } else {
        tool_name.to_string()
    }
}

fn file_write_fingerprint_tool(tool_name: &str) -> &str {
    if crate::tool::categories::registry().is_file_op(tool_name) {
        "file_write"
    } else {
        tool_name
    }
}

fn display_tool_name(tool_name: &str) -> String {
    let mut chars = tool_name.chars();
    match chars.next() {
        None => tool_name.to_string(),
        Some(first) => first.to_uppercase().to_string() + chars.as_str(),
    }
}

fn broad_rule(tool_name: &str) -> String {
    serialize_rule(&PermissionRuleSpec {
        tool: if tool_name == "bash" {
            display_tool_name(tool_name)
        } else {
            tool_name.to_string()
        },
        argv_exact: None,
        argv_prefix: None,
        path_glob: None,
        path_prefix: None,
        op: None,
        cwd_root: None,
        git_branch: None,
        domain: None,
        capability: None,
        extra: Default::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::types::{PermissionRule, RuleMatchContext};

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
            "write_file()"
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
    fn default_bash_target_uses_command_family_not_broad_tool_or_cd_wrapper() {
        let args = serde_json::json!({
            "command": "cd /home/xupeng/github/astra && cargo test -p astra-cli -- --nocapture"
        });
        let target = default_match_target("bash", &args);
        assert_eq!(target, AllowMatchTarget::Prefix("cargo test".to_string()));
        let rule = allow_rule_for_match_target("bash", &args, &target);
        assert_eq!(rule, r#"Bash(argv_prefix="cargo test", op="execute")"#);
    }

    #[test]
    fn default_bash_target_uses_exact_for_unstable_command_family() {
        let args = serde_json::json!({"command": "python -c 'print(1)'"});
        let target = default_match_target("bash", &args);
        assert_eq!(target, AllowMatchTarget::Exact);
        let rule = allow_rule_for_match_target("bash", &args, &target);
        assert_eq!(
            rule,
            r#"Bash(argv_exact="python -c 'print(1)'", op="execute")"#
        );
    }

    #[test]
    fn path_prefix_rule_uses_literal_path_prefix() {
        let args = serde_json::json!({"path": "zzz1.md"});
        let rule = allow_rule_for_match_target(
            "write_file",
            &args,
            &AllowMatchTarget::Prefix("zzz".to_string()),
        );
        assert_eq!(rule, r#"file_write(path_prefix="zzz", op="write")"#);

        let parsed = PermissionRule::parse(&rule);
        assert!(parsed.matches_with_context(
            "write_file",
            &RuleMatchContext::from_tool_args(
                "write_file",
                &serde_json::json!({"path": "zzz2.md"})
            )
        ));
        assert!(parsed.matches_with_context(
            "str_replace",
            &RuleMatchContext::from_tool_args(
                "str_replace",
                &serde_json::json!({"path": "zzz2.md"})
            )
        ));
        assert!(!parsed.matches_with_context(
            "write_file",
            &RuleMatchContext::from_tool_args("write_file", &serde_json::json!({"path": "abc.md"}))
        ));
    }

    #[test]
    fn path_prefix_match_target_covers_later_write_paths() {
        let args = serde_json::json!({"path": "zzz1.md"});
        let approved = fingerprint_for_match_target(
            "write_file",
            &args,
            &AllowMatchTarget::Prefix("zzz".into()),
        );
        let later = crate::approval_fingerprint::ApprovalFingerprint::file_op(
            "file_write",
            Some("zzz2.md"),
        );
        let other =
            crate::approval_fingerprint::ApprovalFingerprint::file_op("file_write", Some("abc.md"));
        let other_tool = crate::approval_fingerprint::ApprovalFingerprint::file_op(
            "str_replace",
            Some("zzz2.md"),
        );
        let mut overrides = crate::approval_fingerprint::FingerprintedOverrides::default();
        overrides.insert(approved, true);

        assert_eq!(overrides.check(&later), Some(true));
        assert_eq!(overrides.check(&other), None);
        assert_eq!(overrides.check(&other_tool), None);
    }

    #[test]
    fn default_write_target_uses_workspace_prefix_for_safe_paths() {
        let args = serde_json::json!({"path": "crates/astra-cli/src/cli/permission_manager.rs"});
        let target = default_match_target("write_file", &args);
        let workspace_root = std::env::current_dir()
            .unwrap()
            .canonicalize()
            .unwrap_or_else(|_| std::env::current_dir().unwrap())
            .to_string_lossy()
            .into_owned();
        assert_eq!(target, AllowMatchTarget::Prefix(workspace_root.clone()));

        let rule = allow_rule_for_match_target("write_file", &args, &target);
        assert_eq!(
            rule,
            format!(
                r#"file_write(path_prefix="{workspace_root}", op="write", cwd_root="{workspace_root}")"#
            )
        );
        let parsed = PermissionRule::parse(&rule);
        assert!(parsed.matches_with_context(
            "write_file",
            &RuleMatchContext::from_tool_args(
                "write_file",
                &serde_json::json!({"path": "web/src/app/page.tsx"})
            )
        ));
        assert!(!parsed.matches_with_context(
            "write_file",
            &RuleMatchContext::from_tool_args(
                "write_file",
                &serde_json::json!({"path": "/tmp/outside.txt"})
            )
        ));
        assert!(parsed.matches_with_context(
            "str_replace",
            &RuleMatchContext::from_tool_args(
                "str_replace",
                &serde_json::json!({"path": "web/src/app/page.tsx"})
            )
        ));
    }

    #[test]
    fn default_write_target_keeps_sensitive_paths_exact() {
        let args = serde_json::json!({"path": ".env"});
        assert_eq!(
            default_match_target("write_file", &args),
            AllowMatchTarget::Exact
        );
    }

    #[test]
    fn default_write_target_keeps_absolute_outside_workspace_exact() {
        let args = serde_json::json!({"path": "/tmp/astra-permission-test.txt"});
        assert_eq!(
            default_match_target("write_file", &args),
            AllowMatchTarget::Exact
        );
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

    #[test]
    fn remember_preview_describes_command_family() {
        let preview = remember_preview(
            "bash",
            &serde_json::json!({"command": "cargo test -p astra-cli"}),
            "in this workspace",
        );
        assert_eq!(preview, "the `cargo test` command family in this workspace");
    }

    #[test]
    fn remember_preview_describes_workspace_writes() {
        let preview = remember_preview(
            "write_file",
            &serde_json::json!({"path": "src/main.rs"}),
            "in this workspace",
        );
        assert_eq!(preview, "file edits in this workspace");
    }

    #[test]
    fn workspace_write_rule_uses_workspace_guard_not_bare_tool() {
        let args = serde_json::json!({"path": "src/main.rs"});
        let target = default_match_target("write_file", &args);
        let workspace_root = workspace_write_prefix("src/main.rs").unwrap();
        assert_eq!(target, AllowMatchTarget::Prefix(workspace_root.clone()));
        assert_eq!(
            allow_rule_for_match_target("write_file", &args, &target),
            format!(
                r#"file_write(path_prefix="{workspace_root}", op="write", cwd_root="{workspace_root}")"#
            )
        );
    }
}

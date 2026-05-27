use serde_json::Value;
use std::path::{Component, Path, PathBuf};

use crate::cloud::approval_policy::{CloudGatedToolKind, cloud_gated_tool_kind};
use crate::parallel_tool_exec::is_read_only_tool_with_args;
use crate::permission::compound_command::{
    is_cd_wrapper_single_command, tokenize_compound_command,
};
use crate::permission::match_target::AllowMatchTarget;
use crate::permission::redact::matches_sensitive_path;
use crate::tool::args::hints::{
    command_hint_from_args, normalized_argv_prefix, path_hint_from_args,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PersistentMemoryBlock {
    CompoundCommand,
    DynamicEval,
    UnsafeRuleShape,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionMemoryProfile {
    pub match_target: AllowMatchTarget,
    pub persistent_block: Option<PersistentMemoryBlock>,
    pub has_stable_target: bool,
}

#[must_use]
pub fn permission_memory_profile(tool_name: &str, args: &Value) -> PermissionMemoryProfile {
    match cloud_gated_tool_kind(tool_name) {
        Some(CloudGatedToolKind::Execute) => execute_memory_profile(tool_name, args),
        Some(CloudGatedToolKind::Write) => write_memory_profile(args),
        None => PermissionMemoryProfile {
            match_target: AllowMatchTarget::Tool,
            persistent_block: None,
            has_stable_target: true,
        },
    }
}

fn execute_memory_profile(tool_name: &str, args: &Value) -> PermissionMemoryProfile {
    let Some(command) = command_hint_from_args(args) else {
        return PermissionMemoryProfile {
            match_target: AllowMatchTarget::Tool,
            persistent_block: Some(PersistentMemoryBlock::UnsafeRuleShape),
            has_stable_target: false,
        };
    };

    let match_target = command_match_target(command);
    let has_stable_target = match match_target {
        AllowMatchTarget::Prefix(ref prefix) => !prefix.trim().is_empty(),
        AllowMatchTarget::Exact => !command.trim().is_empty(),
        AllowMatchTarget::Tool => false,
    };

    let persistent_block = shell_persistent_block(tool_name, args, has_stable_target);
    PermissionMemoryProfile {
        match_target,
        persistent_block,
        has_stable_target,
    }
}

fn command_match_target(command: &str) -> AllowMatchTarget {
    let prefix = normalized_argv_prefix(command);
    if prefix.is_empty() {
        AllowMatchTarget::Exact
    } else {
        AllowMatchTarget::Prefix(prefix)
    }
}

fn shell_persistent_block(
    tool_name: &str,
    args: &Value,
    has_stable_target: bool,
) -> Option<PersistentMemoryBlock> {
    if !has_stable_target {
        return Some(PersistentMemoryBlock::UnsafeRuleShape);
    }

    if !matches!(tool_name, "bash" | "powershell") {
        return None;
    }

    let Some(command) = command_hint_from_args(args) else {
        return Some(PersistentMemoryBlock::UnsafeRuleShape);
    };
    let parsed = tokenize_compound_command(command);
    if parsed.has_dynamic_eval || powershell_dynamic_eval(tool_name, command) {
        return Some(PersistentMemoryBlock::DynamicEval);
    }

    let safe_read_only = is_read_only_tool_with_args(tool_name, Some(args));
    if parsed.steps.len() > 1 && !safe_read_only && !is_cd_wrapper_single_command(&parsed) {
        return Some(PersistentMemoryBlock::CompoundCommand);
    }

    None
}

fn write_memory_profile(args: &Value) -> PermissionMemoryProfile {
    let Some(path) = path_hint_from_args(args) else {
        return PermissionMemoryProfile {
            match_target: AllowMatchTarget::Tool,
            persistent_block: None,
            has_stable_target: true,
        };
    };

    let match_target = if matches_sensitive_path(&path) {
        AllowMatchTarget::Exact
    } else if let Some(workspace_root) = workspace_write_prefix(&path) {
        AllowMatchTarget::Prefix(workspace_root)
    } else {
        AllowMatchTarget::Exact
    };

    PermissionMemoryProfile {
        match_target,
        persistent_block: None,
        has_stable_target: !path.trim().is_empty(),
    }
}

fn powershell_dynamic_eval(tool_name: &str, command: &str) -> bool {
    if tool_name != "powershell" {
        return false;
    }
    command
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '-')
        .any(|token| {
            matches!(
                token.to_ascii_lowercase().as_str(),
                "iex" | "invoke-expression"
            )
        })
}

#[must_use]
pub fn current_workspace_root() -> Option<PathBuf> {
    std::env::current_dir()
        .ok()
        .map(|cwd| canonicalize_path_best_effort(&cwd))
}

#[must_use]
pub fn resolve_write_path_from_cwd(cwd: &Path, path: &str) -> Option<PathBuf> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return None;
    }
    let candidate = {
        let path = Path::new(trimmed);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        }
    };
    Some(canonicalize_path_best_effort(&candidate))
}

#[must_use]
pub fn resolved_write_path(path: &str) -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let resolved = resolve_write_path_from_cwd(&cwd, path)?;
    Some(resolved.to_string_lossy().into_owned())
}

#[must_use]
pub fn workspace_write_prefix(path: &str) -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let workspace_root = current_workspace_root()?;
    let candidate = resolve_write_path_from_cwd(&cwd, path)?;
    if candidate.starts_with(&workspace_root) {
        Some(workspace_root.to_string_lossy().into_owned())
    } else {
        None
    }
}

fn canonicalize_path_best_effort(path: &Path) -> PathBuf {
    let mut existing = Some(path);
    while let Some(candidate) = existing {
        if candidate.exists() {
            let canonical = candidate
                .canonicalize()
                .unwrap_or_else(|_| normalize_path_components(candidate));
            if candidate == path {
                return canonical;
            }
            if let Ok(suffix) = path.strip_prefix(candidate) {
                return normalize_path_components(&canonical.join(suffix));
            }
        }
        existing = candidate.parent();
    }
    normalize_path_components(path)
}

fn normalize_path_components(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::RootDir | Component::Prefix(_) | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_build_cd_wrapper_uses_command_family_without_block() {
        let args = serde_json::json!({
            "command": "cd /home/xupeng/github/astra/rust && cargo build -p astra-turn-core -p astra-cli"
        });
        let profile = permission_memory_profile("bash", &args);
        assert_eq!(
            profile.match_target,
            AllowMatchTarget::Prefix("cargo build".to_string())
        );
        assert_eq!(profile.persistent_block, None);
        assert!(profile.has_stable_target);
    }

    #[test]
    fn quoted_grep_regex_stays_exact_without_compound_block() {
        let args = serde_json::json!({
            "command": r#"cd /home/xupeng/github/astra && grep -n "fn powershell\|fn bash_with_cancel\|execute_with_metadata_responsive" rust/crates/astra-cli/src/edge_tools/shell.rs rust/crates/astra-cli/src/cli/stream_render.rs"#
        });
        let profile = permission_memory_profile("bash", &args);
        assert_eq!(profile.match_target, AllowMatchTarget::Exact);
        assert_eq!(profile.persistent_block, None);
        assert!(profile.has_stable_target);
    }

    #[test]
    fn read_only_pipe_chain_stays_persistable() {
        let args = serde_json::json!({
            "command": r#"cd rust && grep -n "is_unsafe_bare_shell_prefix\|UNSAFE_SHELL\|is_dangerous_bash_allow_shape" crates/astra-cli/src/edge_tools/shell.rs | head -n 20"#
        });
        let profile = permission_memory_profile("bash", &args);
        assert_eq!(profile.persistent_block, None);
        assert!(profile.has_stable_target);
    }

    #[test]
    fn true_multi_step_mutating_shell_stays_blocked() {
        let args = serde_json::json!({"command": "cargo build && cargo test"});
        let profile = permission_memory_profile("bash", &args);
        assert_eq!(
            profile.persistent_block,
            Some(PersistentMemoryBlock::CompoundCommand)
        );
    }

    #[test]
    fn workspace_write_uses_workspace_prefix() {
        let args = serde_json::json!({"path": "rust/crates/astra-turn-core/src/permission_match_target.rs"});
        let profile = permission_memory_profile("write_file", &args);
        assert_eq!(
            profile.match_target,
            AllowMatchTarget::Prefix(
                current_workspace_root()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            )
        );
        assert_eq!(profile.persistent_block, None);
        assert!(profile.has_stable_target);
    }

    #[test]
    fn sensitive_write_stays_exact() {
        let args = serde_json::json!({"path": ".env"});
        let profile = permission_memory_profile("write_file", &args);
        assert_eq!(profile.match_target, AllowMatchTarget::Exact);
        assert_eq!(profile.persistent_block, None);
    }

    #[test]
    fn parent_relative_write_outside_workspace_stays_exact() {
        let args = serde_json::json!({"path": "../outside.txt"});
        let profile = permission_memory_profile("write_file", &args);
        assert_eq!(profile.match_target, AllowMatchTarget::Exact);
    }

    #[test]
    fn powershell_dynamic_eval_stays_blocked() {
        let args = serde_json::json!({"command": "Invoke-Expression $payload"});
        let profile = permission_memory_profile("powershell", &args);
        assert_eq!(
            profile.persistent_block,
            Some(PersistentMemoryBlock::DynamicEval)
        );
    }
}

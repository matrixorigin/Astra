//! Central path-sensitivity model for permission decisions.
//!
//! This module is the single policy boundary for deciding whether a path-like
//! token should trip the sensitive-path gate. Display redaction still lives in
//! `permission::redact`; call sites should not manually combine redaction,
//! sandbox, and internal-artifact rules.

use std::path::{Component, Path, PathBuf};

use astra_sandbox::{InternalPathKind, is_dangerous_file_path};
use astra_services::SessionArtifactStore;
use serde_json::Value;

use crate::parallel_tool_exec::is_read_only_tool_with_args;
use crate::tool_argument_hints::{command_hint_from_args, path_hint_from_args};

use super::redact::matches_sensitive_path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathSensitivity {
    Normal,
    Sensitive,
    InternalArtifactReadOnly(InternalPathKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathAccess {
    DirectRead,
    DirectWrite,
    ShellReference,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SensitivePathMatch {
    pub token: String,
    pub sensitivity: PathSensitivity,
    pub access: PathAccess,
}

pub fn classify_path_sensitivity(path: &str) -> PathSensitivity {
    if let Some(kind) = classify_current_session_artifact_path(path) {
        return PathSensitivity::InternalArtifactReadOnly(kind);
    }
    if is_dangerous_file_path(path) || matches_sensitive_path(path) {
        return PathSensitivity::Sensitive;
    }
    PathSensitivity::Normal
}

fn classify_current_session_artifact_path(path: &str) -> Option<InternalPathKind> {
    let sessions_root = astra_services::local_session_artifact_store()
        .sessions_root()
        .canonicalize()
        .ok()?;
    let candidate = canonicalize_existing_or_nearest(&expand_home_path(path))?;
    let relative = candidate.strip_prefix(&sessions_root).ok()?;
    let mut components = relative.components();

    match (components.next(), components.next(), components.next()) {
        (
            Some(Component::Normal(session_id)),
            Some(Component::Normal(tool_results)),
            Some(Component::Normal(_artifact_file)),
        ) if !session_id.is_empty()
            && tool_results == "tool-results"
            && components.all(|component| matches!(component, Component::Normal(_))) =>
        {
            Some(InternalPathKind::SessionToolResult)
        }
        _ => None,
    }
}

fn expand_home_path(path: &str) -> PathBuf {
    let Some(home) = dirs::home_dir() else {
        return PathBuf::from(path);
    };

    if path == "~" {
        return home;
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home.join(rest);
    }
    if let Some(rest) = path.strip_prefix("$HOME/") {
        return home.join(rest);
    }
    if let Some(rest) = path.strip_prefix("${HOME}/") {
        return home.join(rest);
    }
    PathBuf::from(path)
}

fn canonicalize_existing_or_nearest(path: &Path) -> Option<PathBuf> {
    if let Ok(canonical) = path.canonicalize() {
        return Some(canonical);
    }

    let mut current = path;
    let mut missing = Vec::new();

    loop {
        let file_name = current.file_name()?.to_os_string();
        missing.push(file_name);

        let parent = current.parent()?;
        current = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };

        if let Ok(mut canonical) = current.canonicalize() {
            for component in missing.iter().rev() {
                canonical.push(component);
            }
            return Some(canonical);
        }
    }
}

pub fn path_requires_sensitive_gate(path: &str, access: PathAccess) -> Option<SensitivePathMatch> {
    let sensitivity = classify_path_sensitivity(path);
    let gated = match (sensitivity, access) {
        (PathSensitivity::Normal, _) => false,
        (PathSensitivity::Sensitive, _) => true,
        (PathSensitivity::InternalArtifactReadOnly(_), PathAccess::DirectWrite) => true,
        (
            PathSensitivity::InternalArtifactReadOnly(_),
            PathAccess::DirectRead | PathAccess::ShellReference,
        ) => false,
    };
    gated.then(|| SensitivePathMatch {
        token: path.to_string(),
        sensitivity,
        access,
    })
}

pub fn sensitive_path_match_for_tool_args(
    tool_name: &str,
    args: &Value,
) -> Option<SensitivePathMatch> {
    let is_direct_read = is_read_only_tool_with_args(tool_name, Some(args));

    if let Some(path) = path_hint_from_args(args)
        && !path.is_empty()
    {
        let access = if is_direct_read {
            PathAccess::DirectRead
        } else {
            PathAccess::DirectWrite
        };
        if let Some(hit) = path_requires_sensitive_gate(&path, access) {
            return Some(hit);
        }
    }

    if let Some(command) = command_hint_from_args(args)
        && !command.is_empty()
    {
        for token in shell_like_tokens(command) {
            if let Some(hit) = path_requires_sensitive_gate(&token, PathAccess::ShellReference) {
                return Some(hit);
            }
        }
    }

    None
}

pub fn sensitive_path_token_for_tool_args(tool_name: &str, args: &Value) -> Option<String> {
    sensitive_path_match_for_tool_args(tool_name, args).map(|hit| hit.token)
}

pub fn shell_like_tokens(command: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;

    for ch in command.chars() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            ch if !in_single && !in_double && (ch.is_whitespace() || "|;&<>()".contains(ch)) => {
                push_shell_like_token(&mut tokens, &mut current);
            }
            _ => current.push(ch),
        }
    }
    push_shell_like_token(&mut tokens, &mut current);
    tokens
}

fn push_shell_like_token(tokens: &mut Vec<String>, current: &mut String) {
    let token = current
        .trim()
        .trim_matches(|ch: char| matches!(ch, '\'' | '"' | ',' | ':' | '[' | ']'))
        .to_string();
    if !token.is_empty() {
        tokens.push(token);
    }
    current.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_current_session_artifact() -> (
        tempfile::TempDir,
        astra_services::session_journal::JournalDirGuard,
        std::path::PathBuf,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let sessions_root = temp.path().join("sessions");
        let guard = astra_services::session_journal::JournalDirGuard::new(&sessions_root);
        let artifact_path = sessions_root.join("session-1/tool-results/call_abc.txt");
        std::fs::create_dir_all(artifact_path.parent().unwrap()).unwrap();
        std::fs::write(&artifact_path, "child output").unwrap();
        (temp, guard, artifact_path)
    }

    #[test]
    fn classifies_internal_tool_results_as_read_only_artifacts() {
        let (_temp, _guard, artifact_path) = create_current_session_artifact();
        let artifact_path = artifact_path.to_string_lossy();

        assert_eq!(
            classify_path_sensitivity(&artifact_path),
            PathSensitivity::InternalArtifactReadOnly(InternalPathKind::SessionToolResult)
        );
        assert!(path_requires_sensitive_gate(&artifact_path, PathAccess::DirectRead).is_none());
        assert!(path_requires_sensitive_gate(&artifact_path, PathAccess::ShellReference).is_none());
        assert!(path_requires_sensitive_gate(&artifact_path, PathAccess::DirectWrite).is_some());
    }

    #[test]
    fn arbitrary_astra_tool_results_are_not_permission_internal_artifacts() {
        let temp = tempfile::tempdir().unwrap();
        let artifact_path = temp
            .path()
            .join(".astra/sessions/session-1/tool-results/call_abc.txt");
        std::fs::create_dir_all(artifact_path.parent().unwrap()).unwrap();
        std::fs::write(&artifact_path, "child output").unwrap();
        let artifact_path = artifact_path.to_string_lossy();

        assert_eq!(
            classify_path_sensitivity(&artifact_path),
            PathSensitivity::Sensitive
        );
        let hit =
            path_requires_sensitive_gate(&artifact_path, PathAccess::DirectRead).expect("gate");
        assert_eq!(hit.sensitivity, PathSensitivity::Sensitive);
    }

    #[test]
    fn shell_pipeline_over_internal_artifact_does_not_trip_sensitive_gate() {
        let (_temp, _guard, artifact_path) = create_current_session_artifact();
        std::fs::write(&artifact_path, "{\"ok\":true}").unwrap();
        let artifact_path = artifact_path.to_string_lossy();
        let args = serde_json::json!({
            "command": format!("cat {artifact_path} | python3 -c 'import sys, json; print(json.load(sys.stdin))'")
        });

        assert_eq!(sensitive_path_match_for_tool_args("bash", &args), None);
    }

    #[test]
    fn shell_command_with_mixed_internal_artifact_and_secret_path_is_sensitive() {
        let (_temp, _guard, artifact_path) = create_current_session_artifact();
        let artifact_path = artifact_path.to_string_lossy();
        let args = serde_json::json!({
            "command": format!("cat {artifact_path} ~/.ssh/id_rsa")
        });

        let hit = sensitive_path_match_for_tool_args("bash", &args).expect("sensitive path");
        assert_eq!(hit.token, "~/.ssh/id_rsa");
        assert_eq!(hit.sensitivity, PathSensitivity::Sensitive);
        assert_eq!(hit.access, PathAccess::ShellReference);
    }

    #[test]
    fn direct_write_to_internal_artifact_is_sensitive() {
        let (_temp, _guard, artifact_path) = create_current_session_artifact();
        let artifact_path = artifact_path.to_string_lossy();
        let args = serde_json::json!({"path": artifact_path, "content": "tamper"});

        let hit =
            sensitive_path_match_for_tool_args("write_file", &args).expect("internal write gate");
        assert_eq!(
            hit.sensitivity,
            PathSensitivity::InternalArtifactReadOnly(InternalPathKind::SessionToolResult)
        );
        assert_eq!(hit.access, PathAccess::DirectWrite);
    }
}

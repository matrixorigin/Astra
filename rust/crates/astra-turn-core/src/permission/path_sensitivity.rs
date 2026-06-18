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
    Read,
    Write,
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
        (Some(Component::Normal(journal_file)), None, None)
            if is_session_journal_file(journal_file) =>
        {
            Some(InternalPathKind::SessionJournal)
        }
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

fn is_session_journal_file(file_name: &std::ffi::OsStr) -> bool {
    let Some(file_name) = file_name.to_str() else {
        return false;
    };
    let Some(session_id) = file_name.strip_suffix(".jsonl") else {
        return false;
    };
    astra_services::session_journal::validate_session_id(session_id).is_ok()
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
        (PathSensitivity::InternalArtifactReadOnly(_), PathAccess::Write) => true,
        (PathSensitivity::InternalArtifactReadOnly(_), PathAccess::Read) => false,
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
            PathAccess::Read
        } else {
            PathAccess::Write
        };
        if let Some(hit) = path_requires_sensitive_gate(&path, access) {
            return Some(hit);
        }
    }

    if let Some(command) = command_hint_from_args(args)
        && !command.is_empty()
    {
        for token in shell_like_tokens(command) {
            let extracted = shell_token_path_candidates(&token);
            if extracted.is_empty() {
                let access = classify_shell_token_access(command, &token);
                if let Some(hit) = path_requires_sensitive_gate(&token, access) {
                    return Some(hit);
                }
                continue;
            }
            for candidate in extracted {
                let access = classify_shell_token_access(command, &candidate);
                if let Some(hit) = path_requires_sensitive_gate(&candidate, access) {
                    return Some(hit);
                }
            }
        }
    }

    None
}

pub fn sensitive_path_token_for_tool_args(tool_name: &str, args: &Value) -> Option<String> {
    sensitive_path_match_for_tool_args(tool_name, args).map(|hit| hit.token)
}

/// Classify whether a path token extracted from a shell command is being read
/// or written by that command.
///
/// The read/write distinction only matters for internal artifact paths
/// (agent-owned tool-results / journals): reading those is permitted, mutating
/// them trips the gate. Secrets and other `Sensitive` paths gate on any access,
/// so a conservative `Read` default is safe for them.
///
/// This replaces the previous coarse whole-command read-only check, which
/// misclassified read references embedded in non-read-only commands (pipes to
/// interpreters, `python3 -c`, etc.) as writes and tripped the gate on the
/// agent's own artifacts.
fn classify_shell_token_access(command: &str, candidate: &str) -> PathAccess {
    if shell_token_is_write_target(command, candidate) {
        PathAccess::Write
    } else {
        PathAccess::Read
    }
}

/// True when `candidate` is the target of an output redirection or an argument
/// to a file-mutating shell verb within `command`.
fn shell_token_is_write_target(command: &str, candidate: &str) -> bool {
    for (start, _) in command.match_indices(candidate) {
        if !is_word_boundary_match(command, start, candidate) {
            continue;
        }
        if preceded_by_output_redirection(&command[..start]) {
            return true;
        }
        if segment_has_mutating_verb(command, start, candidate) {
            return true;
        }
    }
    false
}

fn is_word_boundary_match(command: &str, start: usize, candidate: &str) -> bool {
    let before_ok = start == 0
        || command[..start]
            .chars()
            .next_back()
            .map(|ch| !is_path_char(ch))
            .unwrap_or(true);
    let end = start + candidate.len();
    let after_ok = command[end..]
        .chars()
        .next()
        .map(|ch| !is_path_char(ch))
        .unwrap_or(true);
    before_ok && after_ok
}

fn is_path_char(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/' | '~' | '$' | '{' | '}')
}

fn preceded_by_output_redirection(prefix: &str) -> bool {
    let trimmed = prefix.trim_end_matches(|c: char| c.is_whitespace());
    const OPS: &[&str] = &[
        "&>>", "&>", "1>>", "2>>", ">&", "1>", "2>", "&1>", "&2>", ">>", ">",
    ];
    OPS.iter().any(|op| trimmed.ends_with(op))
}

/// Verbs whose file arguments are mutated (truncated, removed, created, or
/// rewritten).
const SHELL_MUTATING_VERBS: &[&str] = &[
    "rm", "rmdir", "mv", "cp", "ln", "truncate", "chmod", "chown", "chgrp", "unlink", "shred",
    "tee", "install", "dd", "mkdir", "mkfifo", "mknod",
];

/// Inspect the pipeline/sequence segment containing `pos` and report whether its
/// leading verb mutates its file arguments.
fn segment_has_mutating_verb(command: &str, pos: usize, candidate: &str) -> bool {
    let seg_start = command[..pos]
        .rfind(['|', ';', '\n', '&'])
        .map(|i| i + 1)
        .unwrap_or(0);
    let seg_end = command[pos..]
        .find(['|', ';', '\n', '&'])
        .map(|i| pos + i)
        .unwrap_or(command.len());
    let segment = &command[seg_start..seg_end];
    if !segment.contains(candidate) {
        return false;
    }
    let Some(verb) = leading_shell_verb(segment) else {
        return false;
    };
    if SHELL_MUTATING_VERBS.contains(&verb.as_str()) {
        return true;
    }
    // sed is mutating only with -i / --in-place.
    if verb == "sed" && (segment.contains(" -i") || segment.contains("--in-place")) {
        return true;
    }
    false
}

fn leading_shell_verb(segment: &str) -> Option<String> {
    for word in segment.split_whitespace() {
        let word = word.trim_matches(|c: char| matches!(c, '"' | '\'' | '`'));
        // Skip env assignments (FOO=bar) and command-modifier prefixes.
        if matches!(
            word,
            "sudo" | "env" | "command" | "exec" | "nice" | "nohup" | "time"
        ) {
            continue;
        }
        if word.is_empty() || (word.contains('=') && !word.starts_with('=')) {
            continue;
        }
        // Handle absolute paths to binaries: /usr/bin/rm -> rm.
        let basename = word.rsplit('/').next().unwrap_or(word);
        return Some(basename.to_string());
    }
    None
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

fn shell_token_path_candidates(token: &str) -> Vec<String> {
    let mut candidates = Vec::new();
    let mut starts = Vec::new();
    for (idx, _) in token.char_indices() {
        if idx > 0
            && let Some(prev) = token[..idx].chars().next_back()
            && !is_shell_path_start_boundary(prev)
        {
            continue;
        }
        let rest = &token[idx..];
        if rest.starts_with('/')
            || rest.starts_with("~/")
            || rest.starts_with("$HOME/")
            || rest.starts_with("${HOME}/")
        {
            starts.push(idx);
        }
    }

    for start in starts {
        let rest = &token[start..];
        let prefix_len = if rest.starts_with("${HOME}/") {
            "${HOME}/".len()
        } else if rest.starts_with("$HOME/") {
            "$HOME/".len()
        } else if rest.starts_with("~/") {
            "~/".len()
        } else {
            1
        };
        let mut end = token.len();
        for (rel_idx, ch) in rest.char_indices() {
            if rel_idx < prefix_len {
                continue;
            }
            if ch.is_whitespace()
                || matches!(
                    ch,
                    '\'' | '"'
                        | '`'
                        | ')'
                        | '('
                        | ','
                        | ';'
                        | '['
                        | ']'
                        | '{'
                        | '}'
                        | '<'
                        | '>'
                        | '|'
                        | '&'
                        | '$'
                        | '\\'
                        | '!'
                        | '#'
                )
            {
                end = start + rel_idx;
                break;
            }
        }
        let candidate = token[start..end]
            .trim_matches(|ch: char| matches!(ch, '\'' | '"' | ',' | ':' | '[' | ']'))
            .to_string();
        if !candidate.is_empty() && !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }

    candidates
}

fn is_shell_path_start_boundary(ch: char) -> bool {
    ch.is_whitespace()
        || matches!(
            ch,
            '\'' | '"' | '`' | '(' | ',' | ';' | '[' | '{' | '<' | '=' | ':' | '|' | '&'
        )
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

    fn create_current_session_journal() -> (
        tempfile::TempDir,
        astra_services::session_journal::JournalDirGuard,
        std::path::PathBuf,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let sessions_root = temp.path().join("sessions");
        let guard = astra_services::session_journal::JournalDirGuard::new(&sessions_root);
        std::fs::create_dir_all(&sessions_root).unwrap();
        let journal_path = sessions_root.join("550e8400-e29b-41d4-a716-446655440000.jsonl");
        std::fs::write(&journal_path, "{}\n").unwrap();
        (temp, guard, journal_path)
    }

    #[test]
    fn classifies_internal_tool_results_as_read_only_artifacts() {
        let (_temp, _guard, artifact_path) = create_current_session_artifact();
        let artifact_path = artifact_path.to_string_lossy();

        assert_eq!(
            classify_path_sensitivity(&artifact_path),
            PathSensitivity::InternalArtifactReadOnly(InternalPathKind::SessionToolResult)
        );
        assert!(path_requires_sensitive_gate(&artifact_path, PathAccess::Read).is_none());
        assert!(path_requires_sensitive_gate(&artifact_path, PathAccess::Write).is_some());
    }

    #[test]
    fn classifies_current_session_journal_as_read_only_artifact() {
        let (_temp, _guard, journal_path) = create_current_session_journal();
        let journal_path = journal_path.to_string_lossy();

        assert_eq!(
            classify_path_sensitivity(&journal_path),
            PathSensitivity::InternalArtifactReadOnly(InternalPathKind::SessionJournal)
        );
        assert!(path_requires_sensitive_gate(&journal_path, PathAccess::Read).is_none());
        assert!(path_requires_sensitive_gate(&journal_path, PathAccess::Write).is_some());

        let grep_args = serde_json::json!({
            "pattern": "str_replace|str replace",
            "path": journal_path.to_string()
        });
        assert_eq!(sensitive_path_match_for_tool_args("grep", &grep_args), None);
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
        let hit = path_requires_sensitive_gate(&artifact_path, PathAccess::Read).expect("gate");
        assert_eq!(hit.sensitivity, PathSensitivity::Sensitive);
    }

    #[test]
    fn arbitrary_astra_session_journals_are_not_permission_internal_artifacts() {
        let temp = tempfile::tempdir().unwrap();
        let journal_path = temp
            .path()
            .join(".astra/sessions/550e8400-e29b-41d4-a716-446655440000.jsonl");
        std::fs::create_dir_all(journal_path.parent().unwrap()).unwrap();
        std::fs::write(&journal_path, "{}\n").unwrap();
        let journal_path = journal_path.to_string_lossy();

        assert_eq!(
            classify_path_sensitivity(&journal_path),
            PathSensitivity::Sensitive
        );
        let hit = path_requires_sensitive_gate(&journal_path, PathAccess::Read).expect("gate");
        assert_eq!(hit.sensitivity, PathSensitivity::Sensitive);
    }

    #[test]
    fn per_session_jsonl_files_are_not_session_journals() {
        let (_temp, _guard, artifact_path) = create_current_session_artifact();
        let session_dir = artifact_path
            .parent()
            .and_then(std::path::Path::parent)
            .expect("session dir");
        let messages_path = session_dir.join("messages.jsonl");
        std::fs::write(&messages_path, "{}\n").unwrap();
        let messages_path = messages_path.to_string_lossy();

        assert!(!matches!(
            classify_path_sensitivity(&messages_path),
            PathSensitivity::InternalArtifactReadOnly(InternalPathKind::SessionJournal)
        ));
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
    fn interpreter_source_string_over_internal_artifact_does_not_trip_sensitive_gate() {
        let (_temp, _guard, artifact_path) = create_current_session_artifact();
        std::fs::write(&artifact_path, "{\"ok\":true}").unwrap();
        let artifact_path = artifact_path.to_string_lossy();
        let args = serde_json::json!({
            "command": format!(
                "python3 -c \"import json; print(json.load(open('{artifact_path}')))\""
            )
        });

        assert_eq!(sensitive_path_match_for_tool_args("bash", &args), None);
    }

    #[test]
    fn shell_path_candidates_stop_at_shell_metacharacters() {
        assert_eq!(
            shell_token_path_candidates("/etc/secret|wc"),
            vec!["/etc/secret".to_string()]
        );
        assert_eq!(
            shell_token_path_candidates("$HOME/.ssh/id_rsa#comment"),
            vec!["$HOME/.ssh/id_rsa".to_string()]
        );
        assert_eq!(
            shell_token_path_candidates("${HOME}/.ssh/id_rsa&&echo"),
            vec!["${HOME}/.ssh/id_rsa".to_string()]
        );
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
        assert_eq!(hit.access, PathAccess::Read);
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
        assert_eq!(hit.access, PathAccess::Write);
    }

    #[test]
    fn mutating_shell_reference_to_internal_artifact_is_sensitive() {
        let (_temp, _guard, artifact_path) = create_current_session_artifact();
        let artifact_path = artifact_path.to_string_lossy();
        let args = serde_json::json!({
            "command": format!("rm -f {artifact_path}")
        });

        let hit = sensitive_path_match_for_tool_args("bash", &args).expect("internal write gate");
        assert_eq!(
            hit.sensitivity,
            PathSensitivity::InternalArtifactReadOnly(InternalPathKind::SessionToolResult)
        );
        assert_eq!(hit.access, PathAccess::Write);
    }
}

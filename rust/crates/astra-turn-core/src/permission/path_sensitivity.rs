//! Central path-sensitivity model for permission decisions.
//!
//! This module is the single policy boundary for deciding whether a path-like
//! token should trip the sensitive-path gate. Display redaction still lives in
//! `permission::redact`; call sites should not manually combine redaction,
//! sandbox, and internal-artifact rules.
//!
//! Shell-command scanning here is intentionally narrow: it only extracts
//! path-like operands that a command is about to read or write, then delegates
//! the actual sensitivity decision back to this module. Sandbox boundary
//! enforcement still owns full command validation.

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
    if let Some(kind) = classify_legacy_tool_result_path(path) {
        return PathSensitivity::InternalArtifactReadOnly(kind);
    }
    if let Some(kind) = classify_current_session_artifact_path(path) {
        return PathSensitivity::InternalArtifactReadOnly(kind);
    }
    if is_dangerous_file_path(path) || matches_sensitive_path(path) {
        return PathSensitivity::Sensitive;
    }
    PathSensitivity::Normal
}

fn classify_legacy_tool_result_path(path: &str) -> Option<InternalPathKind> {
    let root = dirs::home_dir()?
        .join(".astra")
        .join("tool-results")
        .canonicalize()
        .ok()?;
    let candidate = canonicalize_existing_or_nearest(&expand_home_path(path))?;
    let relative = candidate.strip_prefix(&root).ok()?;
    let mut components = relative.components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_artifact_file)), None) => {
            Some(InternalPathKind::SessionToolResult)
        }
        _ => None,
    }
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

/// Inspect tool arguments for a path-like field or shell path operand and decide
/// whether it trips the sensitive-path gate.
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
        return path_requires_sensitive_gate(&path, access);
    }

    if crate::tool::categories::registry().is_shell(tool_name)
        && let Some(command) = command_hint_from_args(args)
    {
        return sensitive_path_match_for_shell_command(command);
    }

    None
}

pub fn sensitive_path_match_for_shell_command(command: &str) -> Option<SensitivePathMatch> {
    let parsed = crate::permission::compound_command::tokenize_compound_command(command);
    for step in parsed.steps {
        if let Some(hit) = sensitive_path_match_for_shell_segment(step.command.as_str()) {
            return Some(hit);
        }
    }
    None
}

fn sensitive_path_match_for_shell_segment(segment: &str) -> Option<SensitivePathMatch> {
    let tokens = shell_tokenize_like_bash(segment);
    if tokens.is_empty() {
        return None;
    }

    if let Some(hit) = redirection_sensitive_path_match(&tokens) {
        return Some(hit);
    }

    let command_index = first_command_token_index(&tokens)?;
    let command = shell_basename(&tokens[command_index]);
    let args_start = command_index + 1;

    match command {
        "grep" | "egrep" | "fgrep" | "rg" | "ag" | "ack" => {
            grep_like_sensitive_path_match(&tokens, args_start)
        }
        "sed" => sed_sensitive_path_match(&tokens, args_start),
        "find" => find_sensitive_path_match(&tokens, args_start),
        "cp" | "install" | "rsync" => copy_like_sensitive_path_match(&tokens, args_start),
        "rm" | "rmdir" | "unlink" | "mv" | "touch" | "mkdir" | "chmod" | "chown" | "chgrp"
        | "truncate" | "tee" => {
            generic_sensitive_path_match(&tokens, args_start, PathAccess::Write)
        }
        "cat" | "head" | "tail" | "less" | "more" | "wc" | "stat" | "ls" | "ll" | "tree"
        | "file" | "du" | "basename" | "dirname" | "realpath" | "readlink" | "test" => {
            generic_sensitive_path_match(&tokens, args_start, PathAccess::Read)
        }
        "echo" | "printf" | "pwd" | "date" | "true" | "false" | "sleep" | "whoami" | "id"
        | "uname" | "hostname" | "env" | "printenv" => None,
        _ if crate::cloud::approval_policy::bash_command_is_read_only(segment) => None,
        _ => generic_sensitive_path_match(&tokens, args_start, PathAccess::Write),
    }
}

fn first_command_token_index(tokens: &[String]) -> Option<usize> {
    let mut idx = 0;
    while idx < tokens.len() {
        let token = tokens[idx].as_str();
        if token.is_empty()
            || looks_like_shell_assignment(token)
            || is_redirection_operator(token).is_some()
        {
            idx += 1;
            continue;
        }

        let command = shell_basename(token);
        match command {
            "command" | "builtin" | "exec" | "nohup" | "noglob" | "nocorrect" | "sudo" | "doas"
            | "time" => {
                idx += 1;
                continue;
            }
            "env" => {
                idx += 1;
                while idx < tokens.len() {
                    let env_token = tokens[idx].as_str();
                    if looks_like_shell_assignment(env_token) {
                        idx += 1;
                        continue;
                    }
                    if env_token.starts_with('-') {
                        idx += 1;
                        continue;
                    }
                    break;
                }
                continue;
            }
            _ => return Some(idx),
        }
    }
    None
}

fn redirection_sensitive_path_match(tokens: &[String]) -> Option<SensitivePathMatch> {
    let mut idx = 0;
    while idx + 1 < tokens.len() {
        let Some(access) = is_redirection_operator(tokens[idx].as_str()) else {
            idx += 1;
            continue;
        };

        if let Some(hit) = shell_operand_sensitive_path_match(tokens[idx + 1].as_str(), access) {
            return Some(hit);
        }
        idx += 2;
    }
    None
}

fn is_redirection_operator(token: &str) -> Option<PathAccess> {
    let without_fd = token.trim_start_matches(|ch: char| ch.is_ascii_digit());
    if without_fd.is_empty() || without_fd.contains('&') {
        return None;
    }
    if without_fd.starts_with(">>") || without_fd.starts_with('>') {
        return Some(PathAccess::Write);
    }
    if without_fd.starts_with("<<") {
        return None;
    }
    if without_fd.starts_with('<') {
        return Some(PathAccess::Read);
    }
    None
}

fn generic_sensitive_path_match(
    tokens: &[String],
    start: usize,
    access: PathAccess,
) -> Option<SensitivePathMatch> {
    let mut stop_parsing_flags = false;
    let mut idx = start;
    while idx < tokens.len() {
        let token = tokens[idx].as_str();
        if is_redirection_operator(token).is_some() {
            idx += 2;
            continue;
        }
        if !stop_parsing_flags && token == "--" {
            stop_parsing_flags = true;
            idx += 1;
            continue;
        }
        if !stop_parsing_flags && token.starts_with('-') {
            idx += 1;
            continue;
        }
        if let Some(hit) = shell_operand_sensitive_path_match(token, access) {
            return Some(hit);
        }
        idx += 1;
    }
    None
}

fn grep_like_sensitive_path_match(tokens: &[String], start: usize) -> Option<SensitivePathMatch> {
    let mut pattern_seen = false;
    let mut next_value: Option<GrepFlagValue> = None;
    let mut stop_parsing_flags = false;
    let mut idx = start;

    while idx < tokens.len() {
        let token = tokens[idx].as_str();
        if is_redirection_operator(token).is_some() {
            idx += 2;
            continue;
        }

        if let Some(kind) = next_value.take() {
            if kind == GrepFlagValue::File
                && let Some(hit) = shell_operand_sensitive_path_match(token, PathAccess::Read)
            {
                return Some(hit);
            }
            idx += 1;
            continue;
        }

        if !stop_parsing_flags && token == "--" {
            stop_parsing_flags = true;
            idx += 1;
            continue;
        }

        if !stop_parsing_flags && token.starts_with('-') {
            if matches!(token, "-e" | "--regexp") {
                next_value = Some(GrepFlagValue::Pattern);
            } else if let Some(_value) = token.strip_prefix("--regexp=") {
                // Pattern text is data, not a file operand.
            } else if matches!(token, "-f" | "--file") {
                next_value = Some(GrepFlagValue::File);
            } else if let Some(value) = token.strip_prefix("--file=")
                && let Some(hit) = shell_operand_sensitive_path_match(value, PathAccess::Read)
            {
                return Some(hit);
            }
            idx += 1;
            continue;
        }

        if !pattern_seen {
            pattern_seen = true;
            idx += 1;
            continue;
        }

        if let Some(hit) = shell_operand_sensitive_path_match(token, PathAccess::Read) {
            return Some(hit);
        }
        idx += 1;
    }

    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GrepFlagValue {
    Pattern,
    File,
}

fn sed_sensitive_path_match(tokens: &[String], start: usize) -> Option<SensitivePathMatch> {
    let mut script_seen = false;
    let mut write = false;
    let mut stop_parsing_flags = false;
    let mut idx = start;

    while idx < tokens.len() {
        let token = tokens[idx].as_str();
        if is_redirection_operator(token).is_some() {
            idx += 2;
            continue;
        }
        if !stop_parsing_flags && token == "--" {
            stop_parsing_flags = true;
            idx += 1;
            continue;
        }
        if !stop_parsing_flags && token.starts_with('-') {
            if token == "-i" || token.starts_with("-i") || token == "--in-place" {
                write = true;
            }
            idx += 1;
            continue;
        }
        if !script_seen {
            script_seen = true;
            idx += 1;
            continue;
        }
        let access = if write {
            PathAccess::Write
        } else {
            PathAccess::Read
        };
        if let Some(hit) = shell_operand_sensitive_path_match(token, access) {
            return Some(hit);
        }
        idx += 1;
    }

    None
}

fn find_sensitive_path_match(tokens: &[String], start: usize) -> Option<SensitivePathMatch> {
    let mut idx = start;
    while idx < tokens.len() {
        let token = tokens[idx].as_str();
        if token == "--" {
            idx += 1;
            continue;
        }
        if token.starts_with('-') {
            break;
        }
        if let Some(hit) = shell_operand_sensitive_path_match(token, PathAccess::Read) {
            return Some(hit);
        }
        idx += 1;
    }
    None
}

fn copy_like_sensitive_path_match(tokens: &[String], start: usize) -> Option<SensitivePathMatch> {
    let operands = shell_path_operands(tokens, start);
    let (last_index, _) = operands.last().copied()?;

    for (idx, token) in operands {
        let access = if idx == last_index {
            PathAccess::Write
        } else {
            PathAccess::Read
        };
        if let Some(hit) = shell_operand_sensitive_path_match(token, access) {
            return Some(hit);
        }
    }
    None
}

fn shell_path_operands(tokens: &[String], start: usize) -> Vec<(usize, &str)> {
    let mut operands = Vec::new();
    let mut stop_parsing_flags = false;
    let mut idx = start;
    while idx < tokens.len() {
        let token = tokens[idx].as_str();
        if is_redirection_operator(token).is_some() {
            idx += 2;
            continue;
        }
        if !stop_parsing_flags && token == "--" {
            stop_parsing_flags = true;
            idx += 1;
            continue;
        }
        if !stop_parsing_flags && token.starts_with('-') {
            idx += 1;
            continue;
        }
        if shell_path_operand(token).is_some() {
            operands.push((idx, token));
        }
        idx += 1;
    }
    operands
}

fn shell_operand_sensitive_path_match(
    token: &str,
    access: PathAccess,
) -> Option<SensitivePathMatch> {
    let path = shell_path_operand(token)?;
    path_requires_sensitive_gate(&path, access)
}

fn shell_path_operand(token: &str) -> Option<String> {
    let trimmed = token
        .trim_matches(|ch| matches!(ch, '"' | '\''))
        .trim_matches(|ch| matches!(ch, '(' | ')' | ',' | ';'));
    if trimmed.is_empty()
        || trimmed == "-"
        || trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
    {
        return None;
    }
    Some(trimmed.to_string())
}

fn shell_basename(token: &str) -> &str {
    token.rsplit('/').next().unwrap_or(token)
}

fn looks_like_shell_assignment(token: &str) -> bool {
    let Some((key, value)) = token.split_once('=') else {
        return false;
    };
    !key.is_empty()
        && !value.is_empty()
        && key
            .chars()
            .all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
        && key
            .chars()
            .next()
            .is_some_and(|ch| ch == '_' || ch.is_ascii_alphabetic())
}

fn shell_tokenize_like_bash(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = input.chars().peekable();
    let mut in_double_quote = false;
    let mut in_single_quote = false;

    while let Some(ch) = chars.next() {
        match ch {
            '"' if !in_single_quote => in_double_quote = !in_double_quote,
            '\'' if !in_double_quote => in_single_quote = !in_single_quote,
            '\\' if !in_single_quote => {
                if let Some(next) = chars.next()
                    && !matches!(next, '\n' | '\r')
                {
                    current.push(next);
                }
            }
            '>' | '<' if !in_double_quote && !in_single_quote => {
                let mut op = String::new();
                if !current.is_empty() && current.chars().all(|c| c.is_ascii_digit()) {
                    op.push_str(&current);
                    current.clear();
                } else if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
                op.push(ch);
                if chars.peek().is_some_and(|next| *next == ch) {
                    op.push(chars.next().unwrap());
                }
                if chars.peek().is_some_and(|next| *next == '&') {
                    op.push(chars.next().unwrap());
                }
                tokens.push(op);
            }
            c if c.is_whitespace() && !in_double_quote && !in_single_quote => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }

    if !current.is_empty() {
        tokens.push(current);
    }

    tokens
}

pub fn sensitive_path_token_for_tool_args(tool_name: &str, args: &Value) -> Option<String> {
    sensitive_path_match_for_tool_args(tool_name, args).map(|hit| hit.token)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EnvGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &std::path::Path) -> Self {
            let previous = std::env::var(key).ok();
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                if let Some(previous) = &self.previous {
                    std::env::set_var(self.key, previous);
                } else {
                    std::env::remove_var(self.key);
                }
            }
        }
    }

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

    fn create_legacy_tool_result() -> (tempfile::TempDir, EnvGuard, std::path::PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let guard = EnvGuard::set("HOME", temp.path());
        let artifact_path = temp.path().join(".astra/tool-results/call_abc.txt");
        std::fs::create_dir_all(artifact_path.parent().unwrap()).unwrap();
        std::fs::write(&artifact_path, "persisted output").unwrap();
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
        assert!(path_requires_sensitive_gate(&artifact_path, PathAccess::Read).is_none());
        assert!(path_requires_sensitive_gate(&artifact_path, PathAccess::Write).is_some());
    }

    #[test]
    fn classifies_legacy_global_tool_results_as_read_only_artifacts() {
        let (_temp, _guard, artifact_path) = create_legacy_tool_result();
        let artifact_path = artifact_path.to_string_lossy();

        assert_eq!(
            classify_path_sensitivity(&artifact_path),
            PathSensitivity::InternalArtifactReadOnly(InternalPathKind::SessionToolResult)
        );
        assert!(path_requires_sensitive_gate(&artifact_path, PathAccess::Read).is_none());
        assert!(path_requires_sensitive_gate(&artifact_path, PathAccess::Write).is_some());

        let args = serde_json::json!({ "path": artifact_path });
        assert_eq!(sensitive_path_match_for_tool_args("read_file", &args), None);

        let args = serde_json::json!({ "path": artifact_path });
        assert_eq!(
            sensitive_path_match_for_tool_args("write_file", &args).map(|hit| hit.access),
            Some(PathAccess::Write),
            "writing into an internal artifact mutates runtime-owned state and must gate"
        );
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
    fn arbitrary_astra_session_journals_are_not_session_journals() {
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
    fn sensitive_path_match_for_tool_args_detects_shell_file_operands() {
        let args = serde_json::json!({
            "command": "cat ~/.ssh/id_rsa"
        });
        let hit = sensitive_path_match_for_tool_args("bash", &args).expect("gate");
        assert_eq!(hit.sensitivity, PathSensitivity::Sensitive);
        assert_eq!(hit.access, PathAccess::Read);
    }

    #[test]
    fn sensitive_path_match_for_tool_args_ignores_grep_pattern_text() {
        let args = serde_json::json!({
            "command": r#"grep -n "~/.ssh/id_rsa\|credentials.json" rust/crates/astra-cli/src/edge_tools/shell.rs"#
        });
        assert_eq!(sensitive_path_match_for_tool_args("bash", &args), None);
    }

    #[test]
    fn sensitive_path_match_for_tool_args_detects_grep_file_operand() {
        let args = serde_json::json!({
            "command": "grep -n needle ~/.ssh/id_rsa"
        });
        let hit = sensitive_path_match_for_tool_args("bash", &args).expect("gate");
        assert_eq!(hit.sensitivity, PathSensitivity::Sensitive);
        assert_eq!(hit.access, PathAccess::Read);
    }

    #[test]
    fn sensitive_path_match_for_shell_command_allows_internal_artifact_reads() {
        let (_temp, _guard, artifact_path) = create_current_session_artifact();
        let artifact_path = artifact_path.to_string_lossy().to_string();
        let command =
            format!("cat {artifact_path} | python3 -c 'import sys; print(sys.stdin.read())'");

        assert_eq!(sensitive_path_match_for_shell_command(&command), None);
    }

    #[test]
    fn sensitive_path_match_for_shell_command_gates_internal_artifact_writes() {
        let (_temp, _guard, artifact_path) = create_current_session_artifact();
        let artifact_path = artifact_path.to_string_lossy().to_string();
        let command = format!("rm -f {artifact_path}");

        let hit = sensitive_path_match_for_shell_command(&command).expect("gate");
        assert_eq!(
            hit.sensitivity,
            PathSensitivity::InternalArtifactReadOnly(InternalPathKind::SessionToolResult)
        );
        assert_eq!(hit.access, PathAccess::Write);
    }

    #[test]
    fn sensitive_path_match_for_tool_args_uses_structured_path_field() {
        let args = serde_json::json!({ "path": "~/.ssh/id_rsa" });
        let hit = sensitive_path_match_for_tool_args("read_file", &args).expect("gate");
        assert_eq!(hit.sensitivity, PathSensitivity::Sensitive);
    }
}

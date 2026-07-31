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

use astra_sandbox::{InternalPathKind, is_dangerous_file_path, is_sensitive_path};
use astra_services::SessionArtifactStore;
use serde_json::Value;

use crate::parallel_tool_exec::is_read_only_tool_with_args;
use crate::tool_argument_hints::{command_hint_from_args, path_hint_from_args};

use super::redact::matches_sensitive_path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathSensitivity {
    Normal,
    /// Reading this path can expose credentials or system-sensitive state.
    Sensitive,
    /// Mutating this path can alter persistent shell/git/editor/agent state,
    /// but read-only inspection is allowed unless the path is also sensitive.
    WriteSensitive,
    InternalArtifactReadOnly(InternalPathKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathAccess {
    Read,
    List,
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
    if is_read_sensitive_path(path) {
        return PathSensitivity::Sensitive;
    }
    if is_agent_skill_content_path(path) {
        return PathSensitivity::Normal;
    }
    if is_write_sensitive_path(path) {
        return PathSensitivity::WriteSensitive;
    }
    PathSensitivity::Normal
}

fn is_read_sensitive_path(path: &str) -> bool {
    let expanded = expand_home_path(path);
    is_sensitive_path(&expanded)
        || matches_sensitive_path(path)
        || matches_sensitive_path(&expanded.to_string_lossy())
}

fn is_write_sensitive_path(path: &str) -> bool {
    let expanded = expand_home_path(path);
    is_read_sensitive_path(path)
        || is_dangerous_file_path(path)
        || is_dangerous_file_path(&expanded.to_string_lossy())
        || is_tilde_hidden_home_app_state_path(path)
        || is_hidden_home_app_state_path(&expanded)
}

fn is_agent_skill_content_path(path: &str) -> bool {
    let expanded = expand_home_path(path);
    is_agent_skill_content_path_components(Path::new(path))
        || is_agent_skill_content_path_components(&expanded)
}

fn is_agent_skill_content_path_components(path: &Path) -> bool {
    let components = normal_component_strings(path);
    components.windows(2).any(|window| {
        matches!(window, [agent_dir, skills_dir]
            if matches!(agent_dir.as_str(), ".astra" | ".claude")
                && skills_dir == "skills")
    })
}

fn normal_component_strings(path: &Path) -> Vec<String> {
    normalize_lexical_path(path)
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy().to_string()),
            _ => None,
        })
        .collect()
}

fn is_tilde_hidden_home_app_state_path(path: &str) -> bool {
    let Some(relative) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) else {
        return false;
    };
    let first = relative.split(['/', '\\']).next().unwrap_or(relative);
    // Strip glob metacharacters for classification — a glob like `~/.xxx/*` targets
    // the same sensitive directory as `~/.xxx/foo`.
    let first = first.trim_end_matches(['*', '?', '[', ']', '{', '}']);
    first.starts_with('.') && first.len() > 1
}

fn is_hidden_home_app_state_path(path: &Path) -> bool {
    let Some(home) = dirs::home_dir() else {
        return false;
    };
    let candidate = normalize_lexical_path(path);
    let home = normalize_lexical_path(&home);
    let Ok(relative) = candidate.strip_prefix(&home) else {
        return false;
    };
    let Some(Component::Normal(first)) = relative.components().next() else {
        return false;
    };
    let first = first.to_string_lossy();
    first.starts_with('.') && first.len() > 1
}

fn normalize_lexical_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn classify_legacy_tool_result_path(path: &str) -> Option<InternalPathKind> {
    let root = astra_runtime_env::local_state_root()
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
    if contains_glob_meta(path) {
        return None;
    }
    let sessions_root = astra_services::local_session_artifact_store()
        .sessions_root()
        .canonicalize()
        .ok()?;
    let candidate = canonicalize_existing_or_nearest(&expand_home_path(path))?;
    let relative = candidate.strip_prefix(&sessions_root).ok()?;
    classify_session_relative_path(relative, false)
}

fn classify_current_session_listing_pattern(path: &str) -> Option<InternalPathKind> {
    if !contains_glob_meta(path) {
        return None;
    }
    let sessions_root = astra_services::local_session_artifact_store()
        .sessions_root()
        .canonicalize()
        .ok()?;
    let candidate = canonicalize_existing_or_nearest(&expand_home_path(path))?;
    let relative = candidate.strip_prefix(&sessions_root).ok()?;
    classify_session_relative_path(relative, true)
}

fn classify_session_relative_path(
    relative: &Path,
    allow_glob_selectors: bool,
) -> Option<InternalPathKind> {
    let components = normal_components(relative)?;
    classify_session_relative_components(&components, allow_glob_selectors)
}

fn normal_components(path: &Path) -> Option<Vec<String>> {
    let mut out = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => out.push(value.to_string_lossy().to_string()),
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(out)
}

fn classify_session_relative_components(
    components: &[String],
    allow_glob_selectors: bool,
) -> Option<InternalPathKind> {
    match components {
        [] => Some(InternalPathKind::SessionRoot),
        [journal_file] if is_session_journal_file_name(journal_file) => {
            Some(InternalPathKind::SessionJournal)
        }
        [session_selector] if is_session_selector(session_selector, allow_glob_selectors) => {
            Some(InternalPathKind::SessionDirectory)
        }
        [session_selector, child]
            if is_session_selector(session_selector, allow_glob_selectors)
                && is_session_diagnostic_file(child) =>
        {
            Some(InternalPathKind::SessionDiagnostic)
        }
        [session_selector, dir] if is_session_selector(session_selector, allow_glob_selectors) => {
            session_artifact_dir_kind(dir)
        }
        [session_selector, dir, rest @ ..]
            if is_session_selector(session_selector, allow_glob_selectors)
                && rest
                    .iter()
                    .all(|component| allow_glob_selectors || !contains_glob_meta(component)) =>
        {
            session_artifact_dir_kind(dir)
        }
        _ => None,
    }
}

fn is_session_selector(component: &str, allow_glob_selectors: bool) -> bool {
    astra_services::session_journal::validate_session_id(component).is_ok()
        || (allow_glob_selectors && contains_glob_meta(component))
}

fn is_session_diagnostic_file(name: &str) -> bool {
    matches!(
        name,
        "workspace.yaml"
            | "conversation_log.jsonl"
            | "step_events.jsonl"
            | "session-memory.md"
            | "session-memory.meta.json"
    ) || (name.starts_with("llm_error_") && name.ends_with(".json"))
}

fn session_artifact_dir_kind(name: &str) -> Option<InternalPathKind> {
    match name {
        "tool-results" => Some(InternalPathKind::SessionToolResult),
        "step_checkpoints" | "checkpoints" | "file_checkpoints" => {
            Some(InternalPathKind::SessionDiagnostic)
        }
        _ => None,
    }
}

fn is_session_journal_file_name(file_name: &str) -> bool {
    let Some(session_id) = file_name.strip_suffix(".jsonl") else {
        return false;
    };
    astra_services::session_journal::validate_session_id(session_id).is_ok()
}

fn contains_glob_meta(path: &str) -> bool {
    path.chars()
        .any(|ch| matches!(ch, '*' | '?' | '[' | ']' | '{' | '}'))
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
    let mut sensitivity = classify_path_sensitivity(path);
    if matches!(sensitivity, PathSensitivity::WriteSensitive)
        && matches!(access, PathAccess::List)
        && let Some(kind) = classify_current_session_listing_pattern(path)
    {
        sensitivity = PathSensitivity::InternalArtifactReadOnly(kind);
    }
    let gated = match (sensitivity, access) {
        (PathSensitivity::Normal, _) => false,
        (PathSensitivity::Sensitive, _) => true,
        (PathSensitivity::WriteSensitive, PathAccess::Write) => true,
        (PathSensitivity::WriteSensitive, PathAccess::Read) if contains_glob_meta(path) => true,
        (PathSensitivity::WriteSensitive, PathAccess::Read | PathAccess::List) => false,
        (PathSensitivity::InternalArtifactReadOnly(_), PathAccess::Write) => true,
        (PathSensitivity::InternalArtifactReadOnly(_), PathAccess::Read | PathAccess::List) => {
            false
        }
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
            direct_read_access(tool_name)
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

fn direct_read_access(tool_name: &str) -> PathAccess {
    match tool_name {
        "list_dir" | "glob" => PathAccess::List,
        _ => PathAccess::Read,
    }
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
        "ls" | "ll" | "tree" => generic_sensitive_path_match(&tokens, args_start, PathAccess::List),
        "cat" | "head" | "tail" | "less" | "more" | "wc" | "stat" | "file" | "du" | "basename"
        | "dirname" | "realpath" | "readlink" | "test" => {
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
    #[serial_test::serial(path_sensitivity_home)]
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
    fn session_root_and_diagnostics_are_read_only_auto_diagnostics() {
        let temp = tempfile::tempdir().unwrap();
        let sessions_root = temp.path().join(".astra/sessions");
        let _guard = astra_services::session_journal::JournalDirGuard::new(&sessions_root);
        let session_id = "550e8400-e29b-41d4-a716-446655440000";
        let session_dir = sessions_root.join(session_id);
        let checkpoint_dir = session_dir.join("step_checkpoints");
        let checkpoint = checkpoint_dir.join("000001-heavy.json");
        std::fs::create_dir_all(&checkpoint_dir).unwrap();
        std::fs::write(sessions_root.join(format!("{session_id}.jsonl")), "{}\n").unwrap();
        std::fs::write(session_dir.join("workspace.yaml"), "session_id: test\n").unwrap();
        std::fs::write(&checkpoint, "{}\n").unwrap();

        assert_eq!(
            classify_path_sensitivity(&sessions_root.to_string_lossy()),
            PathSensitivity::InternalArtifactReadOnly(InternalPathKind::SessionRoot)
        );
        assert_eq!(
            classify_path_sensitivity(&session_dir.to_string_lossy()),
            PathSensitivity::InternalArtifactReadOnly(InternalPathKind::SessionDirectory)
        );
        assert_eq!(
            classify_path_sensitivity(&session_dir.join("workspace.yaml").to_string_lossy()),
            PathSensitivity::InternalArtifactReadOnly(InternalPathKind::SessionDiagnostic)
        );
        assert_eq!(
            classify_path_sensitivity(&checkpoint.to_string_lossy()),
            PathSensitivity::InternalArtifactReadOnly(InternalPathKind::SessionDiagnostic)
        );

        let list_args = serde_json::json!({ "path": sessions_root.to_string_lossy().to_string() });
        assert_eq!(
            sensitive_path_match_for_tool_args("list_dir", &list_args),
            None
        );
        assert_eq!(sensitive_path_match_for_tool_args("glob", &list_args), None);

        assert_eq!(
            sensitive_path_match_for_shell_command(&format!(
                "ls -lt {} | head -20",
                sessions_root.display()
            )),
            None
        );
        assert_eq!(
            sensitive_path_match_for_shell_command(&format!(
                "ls -d {}/*/ | tail -10",
                sessions_root.display()
            )),
            None
        );
        assert_eq!(
            sensitive_path_match_for_shell_command(&format!(
                "ls -lt {}/*-heavy.json | head -3",
                checkpoint_dir.display()
            )),
            None
        );

        let content_glob = sensitive_path_match_for_shell_command(&format!(
            "cat {}/*-heavy.json",
            checkpoint_dir.display()
        ))
        .expect("content reads through globs should still gate");
        assert_eq!(content_glob.access, PathAccess::Read);
        assert_eq!(content_glob.sensitivity, PathSensitivity::WriteSensitive);

        let write_hit =
            path_requires_sensitive_gate(&checkpoint.to_string_lossy(), PathAccess::Write)
                .expect("writes to diagnostics must gate");
        assert_eq!(
            write_hit.sensitivity,
            PathSensitivity::InternalArtifactReadOnly(InternalPathKind::SessionDiagnostic)
        );
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
            PathSensitivity::WriteSensitive
        );
        assert!(path_requires_sensitive_gate(&artifact_path, PathAccess::Read).is_none());
        let hit = path_requires_sensitive_gate(&artifact_path, PathAccess::Write).expect("gate");
        assert_eq!(hit.sensitivity, PathSensitivity::WriteSensitive);
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
            PathSensitivity::WriteSensitive
        );
        assert!(path_requires_sensitive_gate(&journal_path, PathAccess::Read).is_none());
        let hit = path_requires_sensitive_gate(&journal_path, PathAccess::Write).expect("gate");
        assert_eq!(hit.sensitivity, PathSensitivity::WriteSensitive);
    }

    #[test]
    fn hidden_home_app_state_is_readable_but_not_writable_by_default() {
        let log_path = "~/.xxx/logs/session.log";

        assert_eq!(
            classify_path_sensitivity(log_path),
            PathSensitivity::WriteSensitive
        );
        assert!(path_requires_sensitive_gate(log_path, PathAccess::Read).is_none());
        assert!(path_requires_sensitive_gate(log_path, PathAccess::List).is_none());
        assert!(path_requires_sensitive_gate(log_path, PathAccess::Write).is_some());

        let shell_write = sensitive_path_match_for_shell_command("rm -f ~/.yyy/config.toml")
            .expect("mutating hidden home app state should gate");
        assert_eq!(shell_write.sensitivity, PathSensitivity::WriteSensitive);
        assert_eq!(shell_write.access, PathAccess::Write);
    }

    #[test]
    #[serial_test::serial(path_sensitivity_home)]
    fn agent_skill_content_is_editable_hidden_app_content() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("HOME", temp.path());
        let absolute_astra = temp
            .path()
            .join(".astra/skills/code-review/SKILL.md")
            .to_string_lossy()
            .to_string();

        for path in [
            ".astra/skills/code-review/SKILL.md",
            "./.claude/skills/code-review/SKILL.md",
            "~/.astra/skills/code-review/SKILL.md",
            "$HOME/.claude/skills/code-review/SKILL.md",
            absolute_astra.as_str(),
        ] {
            assert_eq!(
                classify_path_sensitivity(path),
                PathSensitivity::Normal,
                "{path}"
            );
            assert!(
                path_requires_sensitive_gate(path, PathAccess::Write).is_none(),
                "{path}"
            );

            let write_args = serde_json::json!({ "path": path, "content": "# Skill\n" });
            assert_eq!(
                sensitive_path_match_for_tool_args("write_file", &write_args),
                None,
                "{path}"
            );
        }
    }

    #[test]
    #[serial_test::serial(path_sensitivity_home)]
    fn agent_skill_credentials_and_control_files_remain_sensitive() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("HOME", temp.path());

        for path in [
            "~/.astra/skills/code-review/.env",
            "~/.claude/skills/code-review/credentials.json",
        ] {
            assert_eq!(
                classify_path_sensitivity(path),
                PathSensitivity::Sensitive,
                "{path}"
            );
            assert!(
                path_requires_sensitive_gate(path, PathAccess::Read).is_some(),
                "{path}"
            );
            assert!(
                path_requires_sensitive_gate(path, PathAccess::Write).is_some(),
                "{path}"
            );
        }

        for path in [
            "~/.astra/permissions.json",
            "~/.astra/config.toml",
            "~/.claude/settings.json",
        ] {
            assert_eq!(
                classify_path_sensitivity(path),
                PathSensitivity::WriteSensitive,
                "{path}"
            );
            assert!(
                path_requires_sensitive_gate(path, PathAccess::Read).is_none(),
                "{path}"
            );
            assert!(
                path_requires_sensitive_gate(path, PathAccess::Write).is_some(),
                "{path}"
            );
        }
    }

    #[test]
    fn shell_writes_to_agent_skill_content_do_not_trip_sensitive_gate() {
        assert_eq!(
            sensitive_path_match_for_shell_command(
                "cat > ~/.astra/skills/code-review/SKILL.md <<'EOF'\n# Skill\nEOF"
            ),
            None
        );
        assert_eq!(
            sensitive_path_match_for_shell_command(
                "mkdir -p .claude/skills/code-review && touch .claude/skills/code-review/SKILL.md"
            ),
            None
        );

        let hit = sensitive_path_match_for_shell_command("cat ~/.claude/skills/code-review/.env")
            .expect("credential file under a skill must still gate");
        assert_eq!(hit.sensitivity, PathSensitivity::Sensitive);
        assert_eq!(hit.access, PathAccess::Read);
    }

    #[test]
    fn credential_files_remain_read_sensitive() {
        for path in [
            "~/.ssh/id_rsa",
            "~/.aws/credentials",
            "~/.xxx/.env",
            "~/.yyy/credentials.json",
            "~/.zzz/token.pem",
            ".env",
            "config/secrets.toml",
        ] {
            let hit = path_requires_sensitive_gate(path, PathAccess::Read).expect("gate");
            assert_eq!(hit.sensitivity, PathSensitivity::Sensitive, "{path}");
        }

        let shell_read = sensitive_path_match_for_shell_command("cat ~/.xxx/.env")
            .expect("credential-shaped files under hidden app state should still gate");
        assert_eq!(shell_read.sensitivity, PathSensitivity::Sensitive);
        assert_eq!(shell_read.access, PathAccess::Read);
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
            "command": r#"grep -n "~/.ssh/id_rsa\|credentials.json" crates/astra-cli/src/edge_tools/shell.rs"#
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

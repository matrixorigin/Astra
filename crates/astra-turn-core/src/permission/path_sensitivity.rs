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
    let first = parsed
        .steps
        .iter()
        .find_map(|step| sensitive_path_match_for_shell_segment(&step.command, PathAccess::Write));
    if first
        .as_ref()
        .is_some_and(|hit| hit.sensitivity != PathSensitivity::WriteSensitive)
    {
        return first;
    }

    let Some(proven_read) = read_only_shell_path_match(command) else {
        return first;
    };
    proven_read.or_else(|| {
        first?;
        // Only downgrade inferred access. Recheck the same segment as well:
        // a false write hit can precede a credential in a compound command.
        parsed.steps.iter().find_map(|step| {
            sensitive_path_match_for_shell_segment(&step.command, PathAccess::Read)
                .filter(|hit| hit.sensitivity != PathSensitivity::WriteSensitive)
        })
    })
}

fn read_only_shell_path_match(command: &str) -> Option<Option<SensitivePathMatch>> {
    let commands = astra_sandbox::parse_bash_commands_for_path_analysis(command)?;
    let mut can_downgrade = true;
    for argv in &commands {
        match read_only_argv_path_match(argv) {
            Some(Some(hit)) => return Some(Some(hit)),
            Some(None) => {}
            None => can_downgrade = false,
        }
    }
    can_downgrade.then_some(None)
}

fn read_only_argv_path_match(argv: &[String]) -> Option<Option<SensitivePathMatch>> {
    // The operand helpers also serve the lexical fallback. A literal argv
    // resembling a redirect must never be silently skipped by those helpers.
    if crate::cloud::approval_policy::bash_argv_approval_reason(argv).is_some()
        || argv
            .iter()
            .any(|token| is_redirection_operator(token).is_some())
    {
        return None;
    }
    let command_index = first_command_token_index(argv)?;
    let hit = if argv[command_index] == "git" {
        git_history_path_match(&argv[command_index + 1..])
    } else {
        known_shell_operand_match(argv, command_index, true)
    }?;
    // The lexical command index can skip wrappers without resolving their
    // option values or working directory.
    (command_index == 0 || hit.is_some()).then_some(hit)
}

fn git_history_path_match(args: &[String]) -> Option<Option<SensitivePathMatch>> {
    let command = args.iter().position(|arg| arg != "--no-pager")?;
    if !matches!(args[command].as_str(), "show" | "log" | "diff") {
        return None;
    }
    let history = args[command] != "diff";
    let metadata_only =
        args[command] == "log" && git_log_outputs_metadata_only(&args[command + 1..]);
    let mut args = args[command + 1..].iter();
    let mut pathspec_only = false;
    let mut protected_pathspec = None;
    while let Some(arg) = args.next() {
        if !pathspec_only && arg.starts_with('-') {
            if arg == "--" {
                pathspec_only = true;
            } else if history
                && crate::cloud::approval_policy::GIT_HISTORY_VALUE_OPTIONS.contains(&arg.as_str())
            {
                args.next()?;
            }
            continue;
        }

        if !arg.starts_with(':')
            && let Some(hit) = path_requires_sensitive_gate(arg, PathAccess::Read)
        {
            return Some(Some(hit));
        }

        if let Some(path) = git_literal_pathspec(arg) {
            if let Some(hit) = path_requires_sensitive_gate(path, PathAccess::Read) {
                return Some(Some(hit));
            }
            // A pathspec can select descendants in a historical tree.
            // Preserve the protected-state rule even for literal pathspecs.
            if !metadata_only {
                protected_pathspec = protected_pathspec
                    .or_else(|| path_requires_sensitive_gate(path, PathAccess::Write));
            }
        }
        if !pathspec_only
            && !arg.starts_with(':')
            && let Some((_, path)) = arg.split_once(':')
            && let Some(hit) = path_requires_sensitive_gate(path, PathAccess::Read)
        {
            return Some(Some(hit));
        }
        if !pathspec_only
            && let Some(path) = arg.strip_prefix(':')
            && !path.starts_with(['(', '/'])
        {
            let path = match path.as_bytes() {
                [b'0'..=b'3', b':', ..] => &path[2..],
                _ => path,
            };
            if let Some(hit) = path_requires_sensitive_gate(path, PathAccess::Read) {
                return Some(Some(hit));
            }
        }
    }
    // Git's selected objects are unknown without inspecting the tree.
    // An argv scan cannot override an existing compound-command gate.
    if metadata_only {
        Some(None)
    } else {
        protected_pathspec.map(Some)
    }
}

fn git_log_outputs_metadata_only(args: &[String]) -> bool {
    let mut args = args.iter().map(String::as_str);
    while let Some(arg) = args.next() {
        match arg {
            "--" => return args.all(|path| git_literal_pathspec(path).is_some()),
            "--oneline" | "-s" | "--no-patch" | "--all" | "--reverse" | "--no-merges"
            | "--merges" | "--first-parent" | "--full-history" | "--date-order"
            | "--topo-order" | "--decorate" | "--no-decorate" | "--pretty" => {}
            value
                if value.split_once('=').is_some_and(|(name, format)| {
                    name == "--pretty" || (name == "--format" && !format.is_empty())
                }) => {}
            value if crate::cloud::approval_policy::GIT_HISTORY_VALUE_OPTIONS.contains(&value) => {
                // Git handles some log switches before consuming revision values.
                if args.next().is_none_or(|value| value.starts_with('-')) {
                    return false;
                }
            }
            value
                if value.strip_prefix('-').is_some_and(|digits| {
                    !digits.is_empty() && digits.bytes().all(|digit| digit.is_ascii_digit())
                }) => {}
            value
                if !value.starts_with('-')
                    && !value.contains(':')
                    && git_literal_pathspec(value).is_some() => {}
            _ => return false,
        }
    }
    true
}

fn git_literal_pathspec(selector: &str) -> Option<&str> {
    let (path, literal) = if let Some(magic) = selector.strip_prefix(":(") {
        let (flags, path) = magic.split_once(')')?;
        if !flags
            .split(',')
            .all(|flag| matches!(flag, "top" | "literal"))
        {
            return None;
        }
        (path, flags.split(',').any(|flag| flag == "literal"))
    } else if let Some(path) = selector.strip_prefix(":/") {
        (path, false)
    } else if selector.starts_with(':') {
        return None;
    } else {
        (selector, false)
    };
    (!path.is_empty() && (literal || !contains_glob_meta(path))).then_some(path)
}

fn direct_read_access(tool_name: &str) -> PathAccess {
    match tool_name {
        "list_dir" | "glob" => PathAccess::List,
        _ => PathAccess::Read,
    }
}

fn sensitive_path_match_for_shell_segment(
    segment: &str,
    inferred_access: PathAccess,
) -> Option<SensitivePathMatch> {
    let tokens = shell_tokenize_like_bash(segment);
    if let Some(hit) = redirection_sensitive_path_match(&tokens) {
        return Some(hit);
    }

    let command_index = first_command_token_index(&tokens)?;
    if let Some(hit) = known_shell_operand_match(&tokens, command_index, false) {
        return hit;
    }
    if shell_basename(&tokens[command_index]) == "date"
        || crate::cloud::approval_policy::bash_command_is_read_only(segment)
    {
        return None;
    }
    generic_sensitive_path_match(&tokens, command_index + 1, inferred_access)
}

/// Outer `None` means the command has no path-operand model here.
fn known_shell_operand_match(
    tokens: &[String],
    command_index: usize,
    require_known: bool,
) -> Option<Option<SensitivePathMatch>> {
    let command = shell_basename(&tokens[command_index]);
    let args_start = command_index + 1;

    Some(match command {
        "grep" | "egrep" | "fgrep" | "rg" | "ag" | "ack" => {
            return grep_like_sensitive_path_match(tokens, args_start, command, require_known);
        }
        "sed" | "find" => {
            let hit = if command == "sed" {
                sed_sensitive_path_match(tokens, args_start)
            } else {
                find_sensitive_path_match(tokens, args_start)
            };
            // These scans cover explicit operands, not every expression or
            // descendant the command may read.
            return hit.map(Some).or_else(|| (!require_known).then_some(None));
        }
        "cp" | "install" | "rsync" => copy_like_sensitive_path_match(tokens, args_start),
        "rm" | "rmdir" | "unlink" | "mv" | "touch" | "mkdir" | "chmod" | "chown" | "chgrp"
        | "truncate" | "tee" => generic_sensitive_path_match(tokens, args_start, PathAccess::Write),
        "ls" | "ll" | "tree" => {
            return generic_read_operand_match(
                &tokens[args_start..],
                command,
                PathAccess::List,
                require_known,
            );
        }
        "cat" | "head" | "tail" | "less" | "more" | "wc" | "stat" | "file" | "du" | "basename"
        | "dirname" | "realpath" | "readlink" | "test" => {
            return generic_read_operand_match(
                &tokens[args_start..],
                command,
                PathAccess::Read,
                require_known,
            );
        }
        "echo" | "printf" | "pwd" | "true" | "false" | "sleep" | "whoami" | "id" | "uname"
        | "hostname" | "env" | "printenv" => None,
        _ => return None,
    })
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

fn generic_read_operand_match(
    args: &[String],
    command: &str,
    access: PathAccess,
    require_known: bool,
) -> Option<Option<SensitivePathMatch>> {
    if let Some(hit) = generic_sensitive_path_match(args, 0, access) {
        return Some(Some(hit));
    }
    if !require_known {
        return Some(None);
    }

    // Skipping an option in the lexical scan does not prove that it has no
    // path operands. Only certify positionals and known numeric limits.
    let path_args = match args {
        [flag, count, rest @ ..]
            if matches!(command, "head" | "tail")
                && flag == "-n"
                && !count.is_empty()
                && count.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            rest
        }
        _ => args,
    };
    path_args
        .iter()
        .take_while(|arg| arg.as_str() != "--")
        .all(|arg| arg == "-" || !arg.starts_with('-'))
        .then_some(None)
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

fn grep_like_sensitive_path_match(
    tokens: &[String],
    start: usize,
    command: &str,
    require_known: bool,
) -> Option<Option<SensitivePathMatch>> {
    let mut positionals = Vec::new();
    let mut recursive = matches!(command, "rg" | "ag" | "ack");
    let mut explicit_pattern = false;
    let mut options_known = true;
    let mut args = grep_argument_words(&tokens[start..]);
    let mut positional_only = false;
    while let Some(arg) = args.next() {
        if !positional_only && arg == "--" {
            positional_only = true;
            continue;
        }
        if !positional_only && arg.starts_with('-') && arg != "-" {
            match grep_option(command, arg, &mut recursive) {
                Some(Some((kind, attached))) => {
                    let value = attached.or_else(|| args.next())?;
                    // GNU grep accepts abbreviated actions. Only exact known
                    // non-traversing actions can rule out descendant reads.
                    recursive |= matches!(kind, GrepFlagValue::Directories)
                        && !matches!(value, "read" | "skip");
                    explicit_pattern |=
                        matches!(kind, GrepFlagValue::Pattern | GrepFlagValue::PatternFile);
                    if matches!(kind, GrepFlagValue::PatternFile | GrepFlagValue::File)
                        && let Some(hit) =
                            shell_operand_sensitive_path_match(value, PathAccess::Read)
                    {
                        return Some(Some(hit));
                    }
                }
                Some(None) => {}
                None if is_gnu_grep(command) => {
                    let hit = grep_ambiguous_operand_match(&tokens[start..], command);
                    return hit.map(Some).or_else(|| (!require_known).then_some(None));
                }
                None => options_known = false,
            }
            continue;
        }

        positionals.push(arg);
    }

    // GNU grep accepts options after operands. Determine the search roots only
    // after consuming options and separating an implicit pattern from paths.
    let skip_pattern = usize::from(!explicit_pattern && !positionals.is_empty());
    let roots = &positionals[skip_pattern..];
    if let Some(hit) = roots
        .iter()
        .find_map(|path| shell_operand_sensitive_path_match(path, PathAccess::Read))
    {
        return Some(Some(hit));
    }

    if recursive {
        if let Some(hit) = roots.iter().find_map(|path| grep_sensitive_root(path)) {
            return Some(Some(hit));
        }
        // A recursive search may open descendants that aren't named in argv.
        // It cannot clear an existing gate through literal operand analysis.
        if require_known {
            return None;
        }
    }
    (!require_known || options_known).then_some(None)
}

fn grep_argument_words(tokens: &[String]) -> impl Iterator<Item = &str> {
    let mut words = tokens.iter().map(String::as_str);
    std::iter::from_fn(move || {
        while let Some(arg) = words.next() {
            if is_redirection_operator(arg).is_none() {
                return Some(arg);
            }
            words.next();
        }
        None
    })
}

fn grep_ambiguous_operand_match(args: &[String], command: &str) -> Option<SensitivePathMatch> {
    // An unmodelled flag can change arity and traversal. No argument role
    // inferred in the main parser is reliable enough to exempt a path here.
    grep_argument_words(args)
        .flat_map(|arg| {
            let attached = if arg.starts_with("--") {
                arg.split_once('=').map(|(_, value)| value)
            } else {
                match grep_option(command, arg, &mut false) {
                    Some(Some((GrepFlagValue::PatternFile, value))) => value,
                    _ => None,
                }
            };
            std::iter::once(arg).chain(attached)
        })
        .find_map(|path| {
            shell_operand_sensitive_path_match(path, PathAccess::Read)
                .or_else(|| grep_sensitive_root(path))
        })
}

fn grep_sensitive_root(path: &str) -> Option<SensitivePathMatch> {
    let mut hit = shell_operand_sensitive_path_match(path, PathAccess::Write)
        .filter(|hit| hit.sensitivity == PathSensitivity::WriteSensitive)?;
    // The protected root may contain credentials. The operation is a read.
    hit.access = PathAccess::Read;
    Some(hit)
}

fn is_gnu_grep(command: &str) -> bool {
    matches!(command, "grep" | "egrep" | "fgrep")
}

#[derive(Clone, Copy)]
enum GrepFlagValue {
    Pattern,
    PatternFile,
    File,
    Data,
    Directories,
}

/// Outer `None` means the option's operand effect is unknown.
fn grep_option<'a>(
    command: &str,
    arg: &'a str,
    recursive: &mut bool,
) -> Option<Option<(GrepFlagValue, Option<&'a str>)>> {
    use GrepFlagValue::{Data, Directories, File, Pattern, PatternFile};
    let gnu = is_gnu_grep(command);
    let ripgrep = command == "rg";
    if let Some(long) = arg.strip_prefix("--") {
        let (name, attached) = match long.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (long, None),
        };
        let kind = match name {
            "regexp" => Pattern,
            "match" if command == "ack" => Pattern,
            "file" if gnu || ripgrep => PatternFile,
            "exclude-from" if gnu => File,
            "ignore-file" if ripgrep => File,
            "directories" if gnu => Directories,
            "recursive" | "dereference-recursive" if gnu => {
                *recursive = true;
                return Some(None);
            }
            "binary-files" | "devices" | "label" | "include" | "exclude" | "exclude-dir"
            | "group-separator"
                if gnu =>
            {
                Data
            }
            "color" | "colors" | "encoding" | "glob" | "iglob" | "replace" if ripgrep => Data,
            "after-context" | "before-context" | "context" | "max-count" => Data,
            "ignore-case" | "invert-match" | "line-number" | "with-filename" | "no-filename"
            | "fixed-strings" | "count" | "quiet" | "word-regexp" | "line-regexp"
            | "byte-offset" | "only-matching" | "color" => {
                return Some(None);
            }
            "colour"
            | "extended-regexp"
            | "basic-regexp"
            | "perl-regexp"
            | "no-ignore-case"
            | "null-data"
            | "no-messages"
            | "silent"
            | "line-buffered"
            | "text"
            | "files-without-match"
            | "files-with-matches"
            | "initial-tab"
            | "null"
            | "no-group-separator"
            | "binary"
                if gnu =>
            {
                return Some(None);
            }
            _ => return None,
        };
        return Some(Some((kind, attached)));
    }

    let short = arg.strip_prefix('-')?;
    for (index, flag) in short.char_indices() {
        let kind = match flag {
            'e' => Pattern,
            'f' if gnu || ripgrep => PatternFile,
            'f' if command == "ag" => continue,
            'A' | 'B' | 'C' | 'm' => Data,
            'd' if gnu => Directories,
            'D' if gnu => Data,
            'r' | 'R' if gnu => {
                *recursive = true;
                continue;
            }
            'E'
            | 'P'
            | 'G'
            | 'a'
            | 'H'
            | 'h'
            | 'L'
            | 's'
            | 'b'
            | 'I'
            | 'T'
            | 'Z'
            | 'z'
            | 'U'
            | 'V'
            | '0'..='9'
                if gnu =>
            {
                continue;
            }
            'E' | 'g' | 'r' if ripgrep => Data,
            'b' | 'H' | 'N' | 'P' | 's' | 'S' | 'u' | 'U' if ripgrep => continue,
            'i' | 'v' | 'n' | 'F' | 'c' | 'q' | 'w' | 'x' | 'l' | 'o' => continue,
            _ => return None,
        };
        // A value-taking letter consumes the rest of the cluster as data.
        if !gnu && !ripgrep && (!matches!(kind, Pattern | PatternFile) || short.len() != 1) {
            return None;
        }
        let rest = &short[index + flag.len_utf8()..];
        return Some(Some((kind, (!rest.is_empty()).then_some(rest))));
    }
    Some(None)
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
    fn read_only_compound_commands_use_operand_access() {
        for command in [
            r#"for p in .claude/settings.json; do stat "$p" | head -n 1; done"#,
            r#"{ stat .claude/settings.json; cat .claude/README.md; }"#,
            r#"{ stat -- .claude/settings.json; cat .claude/README.md; }"#,
            r#"if true; then stat .claude/settings.json; fi"#,
            r#"git log --grep .env -- README.md"#,
            r#"git log --author "example:.env" -- README.md"#,
        ] {
            assert!(
                sensitive_path_match_for_shell_command(command).is_none(),
                "{command}"
            );
        }
        for command in [
            r#"for c in a123; do git show "${c}:.claude/.env"; done"#,
            r#"for p in .env; do cat "$p" | head -n 1; done"#,
            r#"for p in .env; do date -f "$p"; done"#,
            r#"select p in .claude/settings.json; do stat "$p"; done"#,
            r#"{ stat .claude/settings.json; rm .claude/settings.json; }"#,
            r#"{ stat .claude/settings.json; grep -e example .env; }"#,
            r#"{ stat .claude/settings.json; grep -f examples.txt .env; }"#,
            r#"for p in .claude; do unknown_command "$p"; done"#,
            r#"for c in a123; do git show "$c" -- .claude > .claude/settings.json; done"#,
            r#"for c in a123; do git diff --output=.claude/settings.json "$c" -- .claude; done"#,
            r#"for p in .claude; do rm -rf "$p"; done"#,
            r#"for PATH in ./project-bin; do git show HEAD -- .claude; done"#,
            r#"for c in a123; do git diff "$c" HEAD -- .ssh/name:with-colon; done"#,
        ] {
            assert!(
                sensitive_path_match_for_shell_command(command).is_some(),
                "{command}"
            );
        }
    }

    #[test]
    fn unmodelled_options_cannot_clear_compound_path_gate() {
        for command in [
            "{ file --magic-file=.env .claude/README.md; }",
            "{ file -m.env .claude/README.md; }",
            "{ wc --files0-from=.env; stat .claude/README.md; }",
            "{ stat .claude/README.md; wc --files0-from=items; }",
        ] {
            assert!(
                sensitive_path_match_for_shell_command(command).is_some(),
                "{command}"
            );
        }
        let hit = sensitive_path_match_for_shell_command(
            "{ stat .claude/README.md; wc --files0-from=items; cat .env; }",
        )
        .expect("keep confirmed path matches after an unmodelled option");
        assert_eq!(hit.sensitivity, PathSensitivity::Sensitive);
    }

    #[test]
    fn git_history_preserves_protected_pathspecs_and_concrete_credentials() {
        assert!(!git_log_outputs_metadata_only(&[
            "--grep".to_string(),
            "--".to_string(),
        ]));
        for command in [
            r#"git log --oneline -- .claude"#,
            r#"{ git log -3 --oneline HEAD -- .claude; }"#,
            r#"git show ':/release marker'"#,
            r#"git show ':0:.claude/README'"#,
        ] {
            assert!(
                sensitive_path_match_for_shell_command(command).is_none(),
                "{command}"
            );
        }
        for command in [
            r#"{ git show HEAD -- ':(top).pgpass' .claude/README; }"#,
            r#"git show HEAD -- ':(literal,top).pgpass'"#,
            r#"git diff HEAD -- ':/.pgpass'"#,
            r#"git show ':/.pgpass'"#,
            r#"git show ':.pgpass'"#,
            r#"git show ':0:.pgpass'"#,
            r#"{ git show HEAD -- ':(glob)**/*.txt' ':(top).pgpass'; }"#,
        ] {
            let hit =
                sensitive_path_match_for_shell_command(command).expect("protected literal path");
            assert_eq!(hit.sensitivity, PathSensitivity::Sensitive, "{command}");
            assert_eq!(hit.access, PathAccess::Read, "{command}");
        }
        for command in [
            r#"git log --oneline -p -- .claude"#,
            r#"git log --patch --oneline -- .claude"#,
            r#"git show HEAD -- .claude"#,
            r#"{ git show HEAD -- .claude; }"#,
            r#"{ git show HEAD -- ':(literal,top).claude'; }"#,
            r#"for c in a123; do git show "$c" -- .claude | head -n 5; done"#,
            r#"{ git show HEAD -- ':(glob)**/*.txt' .claude/README; }"#,
            r#"{ git show HEAD ':!*.txt' .claude/README; }"#,
            r#"{ git show 'HEAD:docs/*.txt' .claude/README; }"#,
        ] {
            assert!(
                sensitive_path_match_for_shell_command(command).is_some(),
                "{command}"
            );
        }
    }

    #[test]
    fn grep_path_access_distinguishes_pattern_values_and_file_operands() {
        for command in [
            r#"grep -ne.env README.md"#,
            r#"grep -A2e.env README.md"#,
            r#"grep -- .env README.md"#,
            r#"grep -e < /dev/null .env README.md"#,
            r#"grep -b -3 --include='*.md' .env README.md"#,
        ] {
            assert!(
                sensitive_path_match_for_shell_command(command).is_none(),
                "{command}"
            );
        }
        for command in [
            r#"for name in env; do grep -ne. ".$name" README.md; done"#,
            r#"for name in env; do grep -e . ".$name" README.md; done"#,
            r#"for name in env; do grep -f ".$name" README.md; done"#,
            r#"grep -nf.env README.md"#,
            r#"grep --reg -- -nf.env README.md"#,
            r#"grep -n .env -e pattern"#,
            r#"rg -ne. .env README.md"#,
            r#"rg --ignore-file=.env pattern README.md"#,
            r#"for name in env; do rg --no-ignore pattern README.md; cat ".$name"; done"#,
        ] {
            let hit = sensitive_path_match_for_shell_command(command)
                .unwrap_or_else(|| panic!("credential read must gate: {command}"));
            assert_eq!(hit.sensitivity, PathSensitivity::Sensitive, "{command}");
            assert_eq!(hit.access, PathAccess::Read, "{command}");
        }
    }

    #[test]
    fn recursive_grep_protects_search_roots() {
        for command in [
            r#"for p in .claude; do grep -r needle "$p"; done"#,
            "grep -re needle .claude",
            "grep needle .claude --recursive",
            "grep --directories=recurse needle .claude",
            "grep -d recurse needle .claude",
            "grep -d rec needle .claude",
            "grep -dr needle .claude",
            "grep needle .claude --directories=rec",
            "grep --direct=rec needle .claude",
            "grep --rec .claude -e needle",
            "grep .claude --direct=rec --reg -e",
            "grep -d future-action needle .claude",
            "rg needle .claude",
            "ag needle .claude",
            "ag -f needle .claude",
            "ack needle .claude",
            "ack --match=needle .claude",
            "ack --match needle .claude",
        ] {
            let hit = sensitive_path_match_for_shell_command(command).expect(command);
            assert_eq!(
                hit.sensitivity,
                PathSensitivity::WriteSensitive,
                "{command}"
            );
            assert_eq!(hit.access, PathAccess::Read, "{command}");
        }
        for command in [
            "grep needle .claude/README.md",
            "grep -d read needle .claude",
            "grep --directories=skip needle .claude",
            "grep -e .claude README.md",
            "grep -e needle -- README.md -r",
            "rg -r .claude needle README.md",
            "rg needle src",
        ] {
            assert!(
                sensitive_path_match_for_shell_command(command).is_none(),
                "{command}"
            );
        }
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
    fn hidden_project_paths_are_not_application_state() {
        let home = dirs::home_dir().unwrap();
        for path in [
            "~/.cache/projects/app/src/main.rs".to_string(),
            "~/.codex/reviews/workspace/service.conf".to_string(),
            home.join(".codex/reviews/workspace/service.conf")
                .display()
                .to_string(),
            "~/.xxx/logs/session.log".to_string(),
        ] {
            assert_eq!(
                classify_path_sensitivity(&path),
                PathSensitivity::Normal,
                "{path}"
            );
            assert!(path_requires_sensitive_gate(&path, PathAccess::Write).is_none());
        }
        assert!(
            sensitive_path_match_for_shell_command(
                "printf value > ~/.cache/projects/app/service.conf"
            )
            .is_none()
        );
        for path in [
            "~/.ssh/id_rsa",
            "~/.aws/credentials",
            "~/.xxx/.env",
            "~/.astra/permissions.json",
        ] {
            assert!(
                path_requires_sensitive_gate(path, PathAccess::Write).is_some(),
                "{path}"
            );
        }
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

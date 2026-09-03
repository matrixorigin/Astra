//! Shell security hardening for command execution.
//!
//! Provides:
//! - Extglob/extended-glob disabling (prevents malicious filename expansion after validation)
//! - IFS reset (prevents word-splitting attacks)
//! - Stdin redirect to /dev/null (prevents commands from reading spawn's stdin pipe)
//! - Secret scrubbing from subprocess environments
//! - Dangerous file path detection

use std::collections::HashMap;
use std::path::Path;

/// Paths/directories that always require manual approval even in permissive modes.
/// Modifying these can compromise the system, the shell, or the agent itself.
pub const DANGEROUS_FILE_PATHS: &[&str] = &[
    // Git internals
    ".git/",
    ".gitconfig",
    ".gitmodules",
    // Shell configs (modifying = persistent code execution)
    ".bashrc",
    ".bash_profile",
    ".zshrc",
    ".zprofile",
    ".profile",
    ".zshenv",
    // IDE/editor configs
    ".vscode/",
    ".idea/",
    // Agent configs
    ".claude/",
    ".astra/",
    // SSH keys
    ".ssh/",
    // Ripgrep config (can alter search behavior)
    ".ripgreprc",
];

/// Environment variables that contain secrets and must be scrubbed from subprocesses.
pub const SENSITIVE_ENV_VARS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "ACTIONS_ID_TOKEN_REQUEST_TOKEN",
    "ACTIONS_RUNTIME_TOKEN",
    "AZURE_CLIENT_SECRET",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "DATABASE_URL",
    "REDIS_URL",
    "MEMORIA_MASTER_KEY",
    "ASTRA_JWT_SECRET",
    "ASTRA_TOKEN_ENCRYPTION_KEY",
    "ASTRA_RUNTIME_ROOT_SECRET",
    "ENCRYPTION_KEY",
    "FERNET_KEY",
];

/// Configuration for shell hardening features.
#[derive(Debug, Clone)]
pub struct ShellHardeningConfig {
    /// Preserve failure from any segment of a shell pipeline.
    pub pipefail: bool,
    /// Disable extended glob patterns (bash extglob / zsh EXTENDED_GLOB).
    pub disable_extglob: bool,
    /// Reset IFS to default (space, tab, newline).
    pub reset_ifs: bool,
    /// Redirect stdin from /dev/null to prevent commands from reading spawn's stdin.
    pub redirect_stdin: bool,
    /// Scrub secret environment variables from subprocess.
    pub scrub_secrets: bool,
}

impl Default for ShellHardeningConfig {
    fn default() -> Self {
        Self {
            pipefail: true,
            disable_extglob: true,
            reset_ifs: true,
            redirect_stdin: true,
            scrub_secrets: true,
        }
    }
}

/// Build a hardened command string with security preamble.
///
/// Wraps the user command with:
/// 1. Pipefail (preserves upstream pipeline failures)
/// 2. Extglob disable (prevents malicious filename expansion post-validation)
/// 3. IFS reset (prevents word-splitting attacks)
/// 4. Stdin redirect to /dev/null (prevents stdin pipe hijacking)
///
/// The preamble is compatible with both bash and zsh.
pub fn build_hardened_command(config: &ShellHardeningConfig, user_command: &str) -> String {
    let mut parts = Vec::new();

    if config.pipefail {
        // bash: `set -o pipefail`; zsh: `setopt PIPE_FAIL`. Ignore the
        // unsupported form so the same preamble works across supported shells.
        parts.push(
            "{ set -o pipefail 2>/dev/null || setopt PIPE_FAIL 2>/dev/null; } || true".to_string(),
        );
    }

    if config.disable_extglob {
        // Compatible with both bash and zsh:
        // bash: `shopt -u extglob` disables extended glob
        // zsh: `setopt NO_EXTENDED_GLOB` disables extended glob
        // We emit both, suppressing errors for the one that doesn't apply.
        parts.push(
            "{ shopt -u extglob 2>/dev/null || setopt NO_EXTENDED_GLOB 2>/dev/null; } || true"
                .to_string(),
        );
    }

    if config.reset_ifs {
        // Reset IFS to default (space, tab, newline) to prevent word-splitting attacks.
        // An attacker could set IFS to '/' to break path parsing.
        // Keep this POSIX-sh compatible and free of single quotes: isolated execution
        // paths may wrap the full command in a single-quoted shell argument.
        parts.push("IFS=$(printf \" \\011\\012\")".to_string());
    }

    if parts.is_empty() {
        return apply_stdin_redirect(config, user_command);
    }

    parts.push(apply_stdin_redirect(config, user_command));
    parts.join(" && ")
}

fn shell_path_hard_delimiter(ch: char) -> bool {
    matches!(
        ch,
        '\'' | '"' | '`' | ';' | '|' | '&' | '<' | '>' | '{' | '}' | '[' | ']'
    )
}

fn shell_path_start_boundary(ch: Option<char>) -> bool {
    ch.is_none_or(|ch| {
        ch.is_whitespace()
            || matches!(
                ch,
                '\'' | '"' | '`' | '=' | ':' | '(' | '{' | '[' | ',' | '<' | '>'
            )
    })
}

fn windows_drive_path_at(input: &str, index: usize) -> bool {
    let bytes = input.as_bytes();
    bytes.get(index).is_some_and(u8::is_ascii_alphabetic)
        && bytes.get(index + 1) == Some(&b':')
        && matches!(bytes.get(index + 2), Some(b'\\' | b'/'))
}

/// Return true when `input` starts with a Windows drive absolute path.
pub fn is_windows_drive_path(input: &str) -> bool {
    windows_drive_path_at(input, 0)
}

fn shell_home_path_at(input: &str, index: usize) -> bool {
    input[index..].starts_with("~/")
        || input[index..].starts_with("$HOME/")
        || input[index..].starts_with("${HOME}/")
}

/// Return true when `input` starts with a shell home path form.
pub fn is_shell_home_path(input: &str) -> bool {
    !input.is_empty() && shell_home_path_at(input, 0)
}

fn collect_shell_path_token(input: &str, start: usize) -> String {
    let mut end = input.len();
    for (offset, ch) in input[start..].char_indices() {
        let index = start + offset;
        if shell_path_hard_delimiter(ch) {
            end = index;
            break;
        }
        if ch.is_whitespace() {
            let after_whitespace = index + ch.len_utf8();
            if !whitespace_continues_path(input, after_whitespace) {
                end = index;
                break;
            }
        }
    }
    trim_shell_path_token_end(&input[start..end])
}

fn whitespace_continues_path(input: &str, mut index: usize) -> bool {
    while let Some(ch) = input[index..].chars().next() {
        if ch.is_whitespace() {
            index += ch.len_utf8();
        } else {
            break;
        }
    }
    for ch in input[index..].chars() {
        if ch.is_whitespace() || shell_path_hard_delimiter(ch) {
            return false;
        }
        if ch == '/' || ch == '\\' {
            return true;
        }
    }
    false
}

fn trim_shell_path_token_end(token: &str) -> String {
    let mut trimmed = token.trim_end_matches(['.', ',', ':']).to_string();
    loop {
        let closes = trimmed.chars().filter(|ch| *ch == ')').count();
        let opens = trimmed.chars().filter(|ch| *ch == '(').count();
        if closes > opens && trimmed.ends_with(')') {
            trimmed.pop();
            continue;
        }
        break;
    }
    trimmed
}

fn collect_local_workspace_path_token(input: &str, start: usize) -> String {
    const BRACED_HOME: &str = "${HOME}/";
    if input[start..].starts_with(BRACED_HOME) {
        let suffix_start = start + BRACED_HOME.len();
        return format!(
            "{BRACED_HOME}{}",
            collect_shell_path_token(input, suffix_start)
        );
    }
    collect_shell_path_token(input, start)
}

/// Extract absolute local workspace path mentions from shell-ish user text.
///
/// This intentionally recognizes user-machine paths (`~/`, `$HOME`, `/Users`,
/// `/home`, `/Volumes`, Windows drive paths), not arbitrary relative paths.
/// It is used before deciding whether a server-sandbox run can safely own a
/// request or must be routed to an edge workspace.
pub fn extract_local_workspace_path_mentions(command: &str) -> Vec<String> {
    let mut mentions = Vec::new();
    let mut previous = None;
    for (index, ch) in command.char_indices() {
        let at_boundary = shell_path_start_boundary(previous);
        let rest = &command[index..];
        let is_local_path = at_boundary
            && (shell_home_path_at(command, index)
                || rest.starts_with("/Users/")
                || rest.starts_with("/home/")
                || rest.starts_with("/Volumes/")
                || windows_drive_path_at(command, index));
        if is_local_path {
            let token = collect_local_workspace_path_token(command, index);
            if !token.is_empty() && !mentions.iter().any(|existing| existing == &token) {
                mentions.push(token);
            }
        }
        previous = Some(ch);
    }
    mentions
}

/// Scrub secret environment variables from a subprocess environment map.
///
/// Returns the scrubbed map. Also removes `INPUT_<NAME>` variants
/// (GitHub Actions auto-creates these from workflow inputs).
pub fn scrub_secrets_from_env(env: &mut HashMap<String, String>) {
    for &key in SENSITIVE_ENV_VARS {
        env.remove(key);
        // GitHub Actions INPUT_ prefix variant
        let input_key = format!("INPUT_{}", key);
        env.remove(&input_key);
    }
    // Also remove any key that looks like a secret (heuristic).
    let secret_keys: Vec<String> = env
        .keys()
        .filter(|k| {
            let upper = k.to_uppercase();
            (upper.contains("SECRET")
                || upper.contains("PASSWORD")
                || upper.contains("PRIVATE_KEY"))
                && !upper.contains("TOKEN_TYPE")
                && !upper.contains("TOKEN_ENDPOINT")
        })
        .cloned()
        .collect();
    for key in secret_keys {
        env.remove(&key);
    }
}

/// Check if a file path targets a dangerous location that requires manual approval.
///
/// This is a pure check with no exceptions — it only returns `true` if the path
/// matches a known dangerous pattern. Callers should compose this with
/// [`is_internal_safe_path`] to allow agent-generated artifacts.
pub fn is_dangerous_file_path(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    // Check each path component, not just substring, to avoid matching
    // "my.profile.rs" against ".profile".
    for &dangerous in DANGEROUS_FILE_PATHS {
        if dangerous.ends_with('/') {
            // Directory pattern: match the directory component itself or any child path.
            // This catches `.git` as well as `.git/config` without matching `foo.git`.
            let dir = dangerous.trim_end_matches('/');
            if normalized == dir
                || normalized.ends_with(&format!("/{dir}"))
                || normalized.contains(&format!("/{dir}/"))
                || normalized.starts_with(&format!("{dir}/"))
            {
                return true;
            }
        } else {
            // File pattern: must match a full path component.
            // Check if it appears as a component (preceded by / or start, followed by / or end).
            let d = dangerous;
            if normalized == d
                || normalized.ends_with(&format!("/{d}"))
                || normalized.contains(&format!("/{d}/"))
                || normalized.starts_with(&format!("{d}/"))
            {
                return true;
            }
        }
    }
    false
}

/// Check if reading a file at this path requires manual approval.
///
/// This is intentionally narrower than [`is_dangerous_file_path`], which is a
/// write-safety predicate. Reading ordinary hidden app state or shell/git
/// config is allowed; reading credential files and system-sensitive paths is
/// still blocked.
///
/// # Examples
///
/// ```ignore
/// // Safe internal artifacts → allow.
/// assert!(!is_dangerous_read_path("~/.astra/sessions/s1/tool-results/call_abc.txt"));
/// assert!(!is_dangerous_read_path("~/.astra/sessions/s1.jsonl"));
/// // Hidden app/config state is write-sensitive, not read-sensitive.
/// assert!(!is_dangerous_read_path("~/.xxx/logs/session.log"));
/// assert!(!is_dangerous_read_path(".bashrc"));
/// // Credential reads still block.
/// assert!(is_dangerous_read_path("~/.ssh/id_rsa"));
/// assert!(is_dangerous_read_path(".env"));
/// // Not dangerous at all → allow.
/// assert!(!is_dangerous_read_path("src/main.rs"));
/// ```
pub fn is_dangerous_read_path(path: &str) -> bool {
    // Layer 1: recognized first-party diagnostic artifact? → allow reads.
    if is_internal_safe_path(path).is_some() {
        return false;
    }
    // Layer 2: credential/system-sensitive reads still block. Do not reuse
    // the write-safety predicate here, or read-only diagnostics under
    // ~/.xxx-style app state directories will require manual approval.
    crate::policy::is_sensitive_path(Path::new(path)) || is_secret_like_read_path(path)
}

fn is_secret_like_read_path(path: &str) -> bool {
    // Normalize path components to prevent traversal bypass (../ ./)
    let normalized = normalize_lexical_path_str(path);
    let file_name = normalized.rsplit('/').next().unwrap_or(&normalized);
    normalized == ".ssh"
        || normalized.starts_with(".ssh/")
        || normalized.contains("/.ssh/")
        || normalized == ".aws"
        || normalized.starts_with(".aws/")
        || normalized.contains("/.aws/")
        || normalized == ".kube"
        || normalized.starts_with(".kube/")
        || normalized.contains("/.kube/")
        || normalized == ".gnupg"
        || normalized.starts_with(".gnupg/")
        || normalized.contains("/.gnupg/")
        || normalized == ".azure"
        || normalized.starts_with(".azure/")
        || normalized.contains("/.azure/")
        || normalized == ".config/gh"
        || normalized.starts_with(".config/gh/")
        || normalized.contains("/.config/gh/")
        || normalized == ".config/gcloud"
        || normalized.starts_with(".config/gcloud/")
        || normalized.contains("/.config/gcloud/")
        || normalized == ".config/npm"
        || normalized.starts_with(".config/npm/")
        || normalized.contains("/.config/npm/")
        || normalized == ".config/pip"
        || normalized.starts_with(".config/pip/")
        || normalized.contains("/.config/pip/")
        || normalized == ".config/yarn"
        || normalized.starts_with(".config/yarn/")
        || normalized.contains("/.config/yarn/")
        || normalized == ".docker"
        || normalized.starts_with(".docker/")
        || normalized.contains("/.docker/")
        || normalized == ".git-credentials"
        || normalized.starts_with(".git-credentials/")
        || normalized.contains("/.git-credentials/")
        || normalized == ".netrc"
        || normalized.starts_with(".netrc/")
        || normalized.contains("/.netrc/")
        || normalized == ".local/share/keyrings"
        || normalized.starts_with(".local/share/keyrings/")
        || normalized.contains("/.local/share/keyrings/")
        || normalized == ".pki"
        || normalized.starts_with(".pki/")
        || normalized.contains("/.pki/")
        || file_name == ".env"
        || file_name.starts_with(".env.")
        || file_name == ".npmrc"
        || file_name == ".pypirc"
        || file_name == ".netrc"
        || file_name == ".git-credentials"
        || file_name == "credentials.json"
        || file_name == "credentials.db"
        || file_name == "secrets.toml"
        || file_name == "secrets.yaml"
        || file_name == "secrets.yml"
        || file_name == "id_rsa"
        || file_name == "id_ed25519"
        || file_name == "id_ecdsa"
        || file_name == "id_ed25519_sk"
        || file_name == "id_dsa"
        || file_name == ".pgpass"
        || file_name == ".my.cnf"
        || file_name == "htpasswd"
        || file_name == "token.json"
        || file_name == "access_token"
        || file_name == ".terraformrc"
        || file_name == "config.json"
        || file_name == "auth.json"
        || file_name == ".docker-config.json"
        || file_name.ends_with(".pem")
        || file_name.ends_with(".key")
        || file_name.ends_with(".p12")
        || file_name.ends_with(".pfx")
        || file_name.ends_with(".jks")
}

/// Normalize path by resolving `.` and `..` components lexically (no I/O).
/// Prevents path traversal attacks like `~/.ssh/../.aws/credentials`.
fn normalize_lexical_path_str(path: &str) -> String {
    let mut components = Vec::new();
    for part in path.replace('\\', "/").to_ascii_lowercase().split('/') {
        match part {
            "" | "." => {} // skip empty and current-dir
            ".." => {
                components.pop();
            } // pop parent
            _ => components.push(part.to_string()),
        }
    }
    components.join("/")
}

/// Classify a path as a first-party session artifact generated by the agent.
///
/// These paths are safe for read-only diagnostics such as listing, searching,
/// and summarization. Returning `Some` means "this is a recognized internal
/// path, skip read-sensitive checks." It does not mean writes are allowed; callers
/// still apply their own access-mode policy. Returning `None` means "I don't
/// know, continue with normal checks."
///
/// # Examples
///
/// ```ignore
/// // Session artifacts are safe to read back for summarization/diagnostics.
/// assert_eq!(
///     is_internal_safe_path("~/.astra/sessions"),
///     Some(InternalPathKind::SessionRoot)
/// );
/// assert_eq!(
///     is_internal_safe_path("~/.astra/sessions/s1"),
///     Some(InternalPathKind::SessionDirectory)
/// );
/// assert_eq!(
///     is_internal_safe_path("~/.astra/sessions/s1/tool-results/call_abc.txt"),
///     Some(InternalPathKind::SessionToolResult)
/// );
/// assert_eq!(
///     is_internal_safe_path("~/.astra/sessions/s1/step_checkpoints/000001-heavy.json"),
///     Some(InternalPathKind::SessionDiagnostic)
/// );
/// assert_eq!(
///     is_internal_safe_path("~/.astra/sessions/s1.jsonl"),
///     Some(InternalPathKind::SessionJournal)
/// );
/// // But arbitrary .astra/ paths are not safe.
/// assert_eq!(is_internal_safe_path("~/.astra/config.toml"), None);
/// ```
pub fn is_internal_safe_path(path: &str) -> Option<InternalPathKind> {
    let p = Path::new(path);

    // First pass: string-based component check (fast path, no I/O).
    let kind = match_internal_safe_pattern(p)?;

    // Second pass: resolve symlinks to prevent symlink escape attacks.
    // e.g. `.astra/sessions/s1/tool-results/evil.txt -> /etc/passwd`
    // would pass the string check but write to an arbitrary location.
    //
    // Strategy:
    // 1. If the path exists, canonicalize it and re-check the pattern.
    // 2. If the path doesn't exist (new file), canonicalize the nearest
    //    existing ancestor and verify it's within `.astra/sessions/`.
    if let Ok(canonical) = p.canonicalize() {
        // File exists — re-verify the resolved path matches the safe pattern.
        if match_internal_safe_pattern(&canonical).as_ref() != Some(&kind) {
            return None; // Symlink resolved to an unsafe location.
        }
    } else {
        // File doesn't exist yet — walk up to find an existing ancestor and
        // verify it resolves within a `.astra/sessions/` directory.
        let mut ancestor = p.parent();
        while let Some(dir) = ancestor {
            if let Ok(resolved_dir) = dir.canonicalize() {
                let components: Vec<_> = resolved_dir.components().collect();
                let has_astra_sessions = components
                    .windows(2)
                    .any(|w| w[0].as_os_str() == ".astra" && w[1].as_os_str() == "sessions");
                if !has_astra_sessions {
                    return None; // Ancestor resolved outside .astra/sessions/.
                }
                break;
            }
            ancestor = dir.parent();
        }
    }

    Some(kind)
}

/// Pure string-based pattern match for internal safe paths.
/// Checks path components without any filesystem I/O.
fn match_internal_safe_pattern(p: &std::path::Path) -> Option<InternalPathKind> {
    let components: Vec<_> = p.components().collect();

    for i in 0..components.len().saturating_sub(1) {
        let c0 = components[i].as_os_str().to_string_lossy();
        let c1 = components.get(i + 1)?.as_os_str().to_string_lossy();
        if c0 != ".astra" || c1 != "sessions" {
            continue;
        }
        let tail = components
            .iter()
            .skip(i + 2)
            .map(|component| component.as_os_str().to_string_lossy().to_string())
            .collect::<Vec<_>>();
        return match_internal_session_tail(&tail);
    }

    None
}

fn match_internal_session_tail(tail: &[String]) -> Option<InternalPathKind> {
    if tail.is_empty() {
        return Some(InternalPathKind::SessionRoot);
    }

    if tail.len() == 1 {
        let name = tail[0].as_str();
        if looks_like_session_journal_file(name) {
            return Some(InternalPathKind::SessionJournal);
        }
        if astra_core::session_id::validate(name).is_ok() {
            return Some(InternalPathKind::SessionDirectory);
        }
        return None;
    }

    if astra_core::session_id::validate(&tail[0]).is_err() {
        return None;
    }

    let name = tail[1].as_str();
    if tail.len() == 2 {
        if is_session_diagnostic_file(name) {
            return Some(InternalPathKind::SessionDiagnostic);
        }
        return match_session_artifact_dir(name);
    }

    match_session_artifact_dir(name)
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

fn match_session_artifact_dir(name: &str) -> Option<InternalPathKind> {
    match name {
        "tool-results" => Some(InternalPathKind::SessionToolResult),
        "step_checkpoints" | "checkpoints" | "file_checkpoints" => {
            Some(InternalPathKind::SessionDiagnostic)
        }
        _ => None,
    }
}

fn looks_like_session_journal_file(name: &str) -> bool {
    let Some(session_id) = name.strip_suffix(".jsonl") else {
        return false;
    };
    // Delegate to the single source of truth. This previously reimplemented
    // an alphanumeric/length/`-`/`_`/`.` allowlist that drifted from the
    // canonical `astra_core::session_id::validate` used by the permission
    // layer (`path_sensitivity.rs`) and the services layer
    // (`services::session_journal::validate_session_id`). Any divergence
    // would let the sandbox accept journal filenames the permission layer
    // rejects (or vice-versa), so both must consult the same validator.
    astra_core::session_id::validate(session_id).is_ok()
}

/// Kind of recognized internal session path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InternalPathKind {
    /// Session artifact root (e.g. `.astra/sessions`), safe for listing.
    SessionRoot,
    /// Per-session artifact directory (e.g. `.astra/sessions/<session_id>`), safe for listing.
    SessionDirectory,
    /// Top-level session journal (e.g. `.astra/sessions/<session_id>.jsonl`).
    SessionJournal,
    /// Read-only session diagnostics (workspace metadata, checkpoints, memory summaries, LLM dumps).
    SessionDiagnostic,
    /// Session tool-result artifact (e.g. `.astra/sessions/*/tool-results/call_*.txt`).
    SessionToolResult,
}

/// Backward-compat alias: checks if a path references a session tool-result artifact.
pub fn is_session_tool_result_artifact_reference(path: &str) -> bool {
    matches!(
        is_internal_safe_path(path),
        Some(InternalPathKind::SessionToolResult)
    )
}

fn apply_stdin_redirect(config: &ShellHardeningConfig, command: &str) -> String {
    if !config.redirect_stdin {
        return command.to_string();
    }
    // Don't add redirect if command already has stdin redirect or uses heredoc.
    let lower = command.to_lowercase();
    if lower.contains("< ") || lower.contains("<<") || lower.contains("<<<") {
        return command.to_string();
    }
    // Wrap pipe commands in subshell to preserve pipe semantics
    // while still redirecting the overall stdin to /dev/null.
    if command.contains('|') {
        return format!("( {command} ) < /dev/null");
    }
    format!("{command} < /dev/null")
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Hardened command building ---

    #[test]
    fn default_config_adds_all_hardening() {
        let config = ShellHardeningConfig::default();
        let cmd = build_hardened_command(&config, "echo hello");
        assert!(
            cmd.contains("pipefail"),
            "should preserve pipeline failures"
        );
        assert!(cmd.contains("extglob"), "should disable extglob");
        assert!(cmd.contains("IFS="), "should reset IFS");
        assert!(cmd.contains("< /dev/null"), "should redirect stdin");
        assert!(cmd.ends_with("echo hello < /dev/null"));
    }

    #[test]
    fn ifs_reset_is_shell_portable_and_quote_safe() {
        let config = ShellHardeningConfig::default();
        let cmd = build_hardened_command(&config, "echo hello");

        assert!(
            cmd.contains("IFS=$(printf \" \\011\\012\")"),
            "IFS reset must use portable printf form; got: {cmd}"
        );
        assert!(
            !cmd.contains("$'"),
            "ANSI-C quoting is bash-specific and breaks sh wrappers; got: {cmd}"
        );
        assert!(
            !cmd.contains('\''),
            "hardening preamble must not introduce single quotes; got: {cmd}"
        );
    }

    #[test]
    fn no_hardening_returns_raw_command() {
        let config = ShellHardeningConfig {
            pipefail: false,
            disable_extglob: false,
            reset_ifs: false,
            redirect_stdin: false,
            scrub_secrets: false,
        };
        let cmd = build_hardened_command(&config, "echo hello");
        assert_eq!(cmd, "echo hello");
    }

    #[test]
    fn journal_filename_validation_matches_session_id_rules() {
        // Contract: `looks_like_session_journal_file` and the canonical
        // `astra_core::session_id::validate` must agree. Previously these
        // reimplemented overlapping-but-divergent allowlists; any drift would
        // let the sandbox accept journal filenames the permission/services
        // layer rejects (or vice-versa).
        let good = [
            "abc-123.jsonl",
            "550e8400-e29b-41d4-a716-446655440000.jsonl",
            "session_2024.jsonl",
        ];
        let bad = [
            ".jsonl",                              // empty id
            "..jsonl",                             // traversal
            "a/b.jsonl",                           // separator
            "café.jsonl",                          // non-ASCII
            "has\nnewline.jsonl",                  // control char
            &format!("{}.jsonl", "a".repeat(201)), // too long
        ];
        for g in good {
            assert!(
                looks_like_session_journal_file(g),
                "{g:?} should be accepted"
            );
        }
        for b in bad {
            assert!(
                !looks_like_session_journal_file(b),
                "{b:?} should be rejected"
            );
        }
        // Non-.jsonl suffix never matches regardless of id validity.
        assert!(!looks_like_session_journal_file("abc-123.txt"));
    }

    #[test]
    fn stdin_redirect_skipped_for_heredoc() {
        let config = ShellHardeningConfig::default();
        let cmd = build_hardened_command(&config, "cat <<EOF\nhello\nEOF");
        assert!(!cmd.contains("< /dev/null"));
    }

    #[test]
    fn stdin_redirect_skipped_for_existing_redirect() {
        let config = ShellHardeningConfig::default();
        let cmd = build_hardened_command(&config, "wc -l < input.txt");
        assert!(!cmd.ends_with("< /dev/null"));
    }

    #[test]
    fn stdin_redirect_skipped_for_pipe() {
        let config = ShellHardeningConfig::default();
        let cmd = build_hardened_command(&config, "cat file.txt | grep pattern");
        // Pipe commands are now wrapped in subshell to preserve pipe semantics
        // while still redirecting overall stdin to /dev/null.
        assert!(
            cmd.contains("< /dev/null"),
            "pipe commands must have stdin redirect via subshell wrap"
        );
        assert!(
            cmd.contains("(") && cmd.contains(")"),
            "pipe commands must be wrapped in subshell"
        );
    }

    // --- Secret scrubbing ---

    #[test]
    fn scrubs_known_secrets() {
        let mut env = HashMap::new();
        env.insert("PATH".to_string(), "/usr/bin".to_string());
        env.insert("ANTHROPIC_API_KEY".to_string(), "sk-secret".to_string());
        env.insert("AWS_SECRET_ACCESS_KEY".to_string(), "AKIA...".to_string());
        env.insert("HOME".to_string(), "/home/user".to_string());

        scrub_secrets_from_env(&mut env);

        assert!(env.contains_key("PATH"));
        assert!(env.contains_key("HOME"));
        assert!(!env.contains_key("ANTHROPIC_API_KEY"));
        assert!(!env.contains_key("AWS_SECRET_ACCESS_KEY"));
    }

    #[test]
    fn scrubs_input_prefix_variants() {
        let mut env = HashMap::new();
        env.insert(
            "INPUT_ANTHROPIC_API_KEY".to_string(),
            "sk-secret".to_string(),
        );
        scrub_secrets_from_env(&mut env);
        assert!(!env.contains_key("INPUT_ANTHROPIC_API_KEY"));
    }

    #[test]
    fn scrubs_heuristic_secret_keys() {
        let mut env = HashMap::new();
        env.insert("MY_SECRET_VALUE".to_string(), "hidden".to_string());
        env.insert("DB_PASSWORD".to_string(), "pass123".to_string());
        env.insert("GITHUB_PRIVATE_KEY".to_string(), "key".to_string());
        env.insert("NORMAL_VAR".to_string(), "visible".to_string());
        // TOKEN vars are NOT scrubbed by heuristic (gh CLI needs GITHUB_TOKEN).
        env.insert("GITHUB_TOKEN".to_string(), "ghp_xxx".to_string());

        scrub_secrets_from_env(&mut env);

        assert!(!env.contains_key("MY_SECRET_VALUE"));
        assert!(!env.contains_key("DB_PASSWORD"));
        assert!(!env.contains_key("GITHUB_PRIVATE_KEY"));
        assert!(env.contains_key("NORMAL_VAR"));
        assert!(
            env.contains_key("GITHUB_TOKEN"),
            "gh CLI needs GITHUB_TOKEN"
        );
    }

    #[test]
    fn does_not_scrub_token_vars_by_heuristic() {
        let mut env = HashMap::new();
        env.insert("GITHUB_TOKEN".to_string(), "ghp_xxx".to_string());
        env.insert("GH_TOKEN".to_string(), "ghp_yyy".to_string());
        env.insert("TOKEN_TYPE".to_string(), "bearer".to_string());
        env.insert(
            "TOKEN_ENDPOINT".to_string(),
            "https://auth.example.com".to_string(),
        );
        scrub_secrets_from_env(&mut env);
        assert!(env.contains_key("GITHUB_TOKEN"));
        assert!(env.contains_key("GH_TOKEN"));
        assert!(env.contains_key("TOKEN_TYPE"));
        assert!(env.contains_key("TOKEN_ENDPOINT"));
    }

    // --- Dangerous file paths ---

    #[test]
    fn detects_git_internal_paths() {
        assert!(is_dangerous_file_path(".git"));
        assert!(is_dangerous_file_path("/home/user/.git"));
        assert!(is_dangerous_file_path(".git/config"));
        assert!(is_dangerous_file_path("/home/user/.git/hooks/pre-commit"));
        assert!(is_dangerous_file_path(".gitconfig"));
    }

    #[test]
    fn detects_shell_config_paths() {
        assert!(is_dangerous_file_path(".bashrc"));
        assert!(is_dangerous_file_path("/home/user/.zshrc"));
        assert!(is_dangerous_file_path(".profile"));
    }

    #[test]
    fn detects_ssh_paths() {
        assert!(is_dangerous_file_path(".ssh"));
        assert!(is_dangerous_file_path("/home/user/.ssh"));
        assert!(is_dangerous_file_path(".ssh/id_rsa"));
        assert!(is_dangerous_file_path("~/.ssh/id_rsa"));
        assert!(is_dangerous_file_path("/home/user/.ssh/authorized_keys"));
    }

    #[test]
    fn allows_normal_paths() {
        assert!(!is_dangerous_file_path("src/main.rs"));
        assert!(!is_dangerous_file_path("README.md"));
        assert!(!is_dangerous_file_path("Cargo.toml"));
    }

    #[test]
    fn does_not_match_partial_filenames() {
        // "my.profile.rs" should NOT match ".profile"
        assert!(!is_dangerous_file_path("my.profile.rs"));
        assert!(!is_dangerous_file_path("user.profile_settings"));
        assert!(!is_dangerous_file_path("src/bashrc_utils.rs"));
        // But actual .bashrc should still match
        assert!(is_dangerous_file_path("/home/user/.bashrc"));
        assert!(is_dangerous_file_path(".bashrc"));
    }

    #[test]
    fn dangerous_read_path_allows_diagnostics_and_hidden_app_logs_but_blocks_secrets() {
        let temp = tempfile::tempdir().unwrap();
        let artifact_path = temp
            .path()
            .join(".astra/sessions/s1/tool-results/call_abc.txt");
        std::fs::create_dir_all(artifact_path.parent().unwrap()).unwrap();
        std::fs::write(&artifact_path, "tool output").unwrap();

        // Session tool-results are write-sensitive but not read-sensitive.
        assert!(!is_dangerous_read_path(&artifact_path.to_string_lossy()));
        let journal_path = temp.path().join(".astra/sessions/s1.jsonl");
        std::fs::write(&journal_path, "{}\n").unwrap();
        assert_eq!(
            is_internal_safe_path(&journal_path.to_string_lossy()),
            Some(InternalPathKind::SessionJournal)
        );
        assert!(!is_dangerous_read_path(&journal_path.to_string_lossy()));
        assert!(!is_dangerous_read_path(
            temp.path()
                .join(".astra/sessions")
                .to_string_lossy()
                .as_ref()
        ));
        assert!(!is_dangerous_read_path(
            temp.path()
                .join(".astra/sessions/s1")
                .to_string_lossy()
                .as_ref()
        ));
        assert!(!is_dangerous_read_path(
            temp.path()
                .join(".astra/sessions/s1/workspace.yaml")
                .to_string_lossy()
                .as_ref()
        ));
        assert!(!is_dangerous_read_path(
            temp.path()
                .join(".astra/sessions/s1/step_checkpoints/000001-heavy.json")
                .to_string_lossy()
                .as_ref()
        ));
        // Other hidden app state is write-sensitive but not read-sensitive.
        assert!(!is_dangerous_read_path("/Users/test/.astra/config.toml"));
        assert!(!is_dangerous_read_path(
            "/Users/test/.astra/sessions/s1/messages.jsonl"
        ));
        assert!(!is_dangerous_read_path("/Users/test/.xxx/logs/session.log"));
        // Credential paths are still read-sensitive.
        assert!(is_dangerous_read_path(".ssh/id_rsa"));
        assert!(is_dangerous_read_path("~/.ssh/config"));
        assert!(is_dangerous_read_path(".env"));
        assert!(is_dangerous_read_path("/Users/test/.xxx/.env"));
        assert!(is_dangerous_read_path("/Users/test/.xxx/credentials.json"));
        assert!(is_dangerous_read_path("config/secrets.toml"));
        // Non-secret persistent config is write-sensitive, not read-sensitive.
        assert!(!is_dangerous_read_path(".bashrc"));
        assert!(!is_dangerous_read_path(".git/config"));
    }

    #[test]
    fn is_dangerous_file_path_still_flags_tool_results_for_writes() {
        // Write-sensitive: tool-results are still flagged for writes.
        assert!(is_dangerous_file_path(
            "/Users/test/.astra/sessions/s1/tool-results/call_abc.txt"
        ));
        assert!(is_dangerous_file_path(
            "/Users/test/.astra/sessions/s1.jsonl"
        ));
        assert!(is_dangerous_file_path("/Users/test/.astra/config.toml"));
    }

    // --- scrub_secrets edge cases ---

    #[test]
    fn scrub_secrets_removes_password_variants() {
        let mut env = HashMap::from([
            ("DB_PASSWORD".into(), "secret".into()),
            ("MY_SECRET_KEY".into(), "hidden".into()),
            ("SSH_PRIVATE_KEY".into(), "pk".into()),
            ("SAFE_VAR".into(), "ok".into()),
        ]);
        scrub_secrets_from_env(&mut env);
        assert!(!env.contains_key("DB_PASSWORD"));
        assert!(!env.contains_key("MY_SECRET_KEY"));
        assert!(!env.contains_key("SSH_PRIVATE_KEY"));
        assert!(env.contains_key("SAFE_VAR"));
    }

    #[test]
    fn scrub_secrets_preserves_token_type_and_endpoint() {
        let mut env = HashMap::from([
            ("TOKEN_TYPE".into(), "bearer".into()),
            ("TOKEN_ENDPOINT".into(), "https://auth.example.com".into()),
        ]);
        scrub_secrets_from_env(&mut env);
        assert!(env.contains_key("TOKEN_TYPE"));
        assert!(env.contains_key("TOKEN_ENDPOINT"));
    }

    #[test]
    fn scrub_secrets_case_insensitive_heuristic() {
        let mut env = HashMap::from([
            ("my_secret".into(), "v1".into()),
            ("My_Password".into(), "v2".into()),
        ]);
        scrub_secrets_from_env(&mut env);
        assert!(!env.contains_key("my_secret"));
        assert!(!env.contains_key("My_Password"));
    }

    #[test]
    fn scrub_secrets_empty_env() {
        let mut env = HashMap::new();
        scrub_secrets_from_env(&mut env);
        assert!(env.is_empty());
    }

    // --- is_dangerous_file_path edge cases ---

    #[test]
    fn dangerous_path_with_backslashes() {
        // Windows-style paths should be normalized
        assert!(is_dangerous_file_path("C:\\Users\\.ssh\\id_rsa"));
    }

    #[test]
    fn dangerous_path_empty_string() {
        assert!(!is_dangerous_file_path(""));
    }

    #[test]
    fn dangerous_path_git_internal_nested() {
        assert!(is_dangerous_file_path("repo/.git/config"));
        assert!(is_dangerous_file_path(".git/HEAD"));
        assert!(!is_dangerous_file_path("repo/foo.git/config"));
    }

    // --- build_hardened_command edge cases ---

    #[test]
    fn hardened_command_only_extglob() {
        let config = ShellHardeningConfig {
            pipefail: false,
            disable_extglob: true,
            reset_ifs: false,
            redirect_stdin: false,
            scrub_secrets: false,
        };
        let cmd = build_hardened_command(&config, "echo hello");
        assert!(!cmd.contains("pipefail"));
        assert!(cmd.contains("extglob"));
        assert!(!cmd.contains("IFS="));
        assert!(!cmd.contains("< /dev/null"));
    }

    #[test]
    fn hardened_command_only_ifs() {
        let config = ShellHardeningConfig {
            pipefail: false,
            disable_extglob: false,
            reset_ifs: true,
            redirect_stdin: false,
            scrub_secrets: false,
        };
        let cmd = build_hardened_command(&config, "ls");
        assert!(!cmd.contains("pipefail"));
        assert!(cmd.contains("IFS="));
        assert!(!cmd.contains("extglob"));
    }

    #[test]
    fn stdin_redirect_skipped_for_pipe_chain() {
        // Pipe commands now get subshell wrapping with stdin redirect
        // to prevent right-side commands from reading spawn stdin.
        let config = ShellHardeningConfig::default();
        let cmd = build_hardened_command(&config, "echo hello | grep h | wc -l");
        // Pipeline should be wrapped: ( ... ) < /dev/null
        assert!(
            cmd.contains("< /dev/null"),
            "pipe commands must have stdin redirect via subshell wrap; \
             got: '{cmd}'"
        );
        assert!(
            cmd.contains("( echo hello | grep h | wc -l )"),
            "expected subshell wrapper; got: '{cmd}'"
        );
    }

    #[test]
    fn stdin_redirect_wraps_pipe_with_disabled_extras() {
        let config = ShellHardeningConfig {
            pipefail: false,
            disable_extglob: false,
            reset_ifs: false,
            redirect_stdin: true,
            scrub_secrets: false,
        };
        let cmd = build_hardened_command(&config, "echo hello | grep hello");
        // Even with only redirect_stdin enabled, pipe commands get subshell wrap.
        assert!(
            cmd.contains("< /dev/null"),
            "pipe commands must have stdin redirect; got: '{cmd}'"
        );
    }

    #[test]
    fn default_config_preserves_pipeline_failures_and_stdin_guard() {
        let config = ShellHardeningConfig::default();
        let cmd = build_hardened_command(&config, "cargo test 2>&1 | tail -20");

        assert!(
            cmd.contains("pipefail") || cmd.contains("PIPE_FAIL"),
            "pipeline hardening must enable pipefail; got: '{cmd}'"
        );
        assert!(
            cmd.contains("( cargo test 2>&1 | tail -20 ) < /dev/null"),
            "pipeline hardening must wrap the whole pipeline for stdin guard; got: '{cmd}'"
        );
    }

    #[test]
    fn internal_safe_path_recognizes_session_diagnostics() {
        let root = std::env::temp_dir().join(format!(
            "astra-shell-hardening-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time should be after UNIX_EPOCH")
                .as_nanos()
        ));
        let artifact_dir = root.join(".astra/sessions/s1/tool-results");
        let artifact = artifact_dir.join("call_abc.txt");
        std::fs::create_dir_all(&artifact_dir).expect("create test artifact directory");
        std::fs::write(&artifact, "result").expect("write test artifact");

        let artifact_path = artifact.to_string_lossy();
        assert_eq!(
            is_internal_safe_path(&artifact_path),
            Some(InternalPathKind::SessionToolResult)
        );
        assert_eq!(
            is_internal_safe_path(&format!("cat {artifact_path}")),
            Some(InternalPathKind::SessionToolResult)
        );
        let journal = root.join(".astra/sessions/s1.jsonl");
        std::fs::write(&journal, "journal").expect("write test journal");
        assert_eq!(
            is_internal_safe_path(&journal.to_string_lossy()),
            Some(InternalPathKind::SessionJournal)
        );
        assert_eq!(
            is_internal_safe_path(&format!("grep pattern {}", journal.to_string_lossy())),
            Some(InternalPathKind::SessionJournal)
        );
        assert_eq!(
            is_internal_safe_path(&root.join(".astra/sessions").to_string_lossy()),
            Some(InternalPathKind::SessionRoot)
        );
        assert_eq!(
            is_internal_safe_path(&root.join(".astra/sessions/s1").to_string_lossy()),
            Some(InternalPathKind::SessionDirectory)
        );
        assert_eq!(
            is_internal_safe_path(&artifact_dir.to_string_lossy()),
            Some(InternalPathKind::SessionToolResult)
        );
        assert_eq!(
            is_internal_safe_path(
                &root
                    .join(".astra/sessions/s1/step_checkpoints/000001-heavy.json")
                    .to_string_lossy()
            ),
            Some(InternalPathKind::SessionDiagnostic)
        );
        assert_eq!(
            is_internal_safe_path(
                &root
                    .join(".astra/sessions/s1/messages.jsonl")
                    .to_string_lossy()
            ),
            None
        );
        assert_eq!(
            is_internal_safe_path(&root.join(".astra/config.toml").to_string_lossy()),
            None
        );

        std::fs::remove_dir_all(&root).expect("remove test artifact directory");
    }

    #[test]
    fn local_workspace_path_mentions_preserve_spaces_and_parentheses() {
        assert_eq!(
            extract_local_workspace_path_mentions("cd /home/example/astra && git status"),
            vec!["/home/example/astra"]
        );
        assert_eq!(
            extract_local_workspace_path_mentions("fix /Users/test/project (v2)/src/main.rs"),
            vec!["/Users/test/project (v2)/src/main.rs"]
        );
        assert_eq!(
            extract_local_workspace_path_mentions(
                "compare /Users/test/My Project/src/lib.rs with README"
            ),
            vec!["/Users/test/My Project/src/lib.rs"]
        );
    }

    #[test]
    fn local_workspace_path_helpers_detect_home_and_windows_paths() {
        assert!(is_shell_home_path("~/repo"));
        assert!(is_shell_home_path("$HOME/repo"));
        assert!(is_shell_home_path("${HOME}/repo"));
        assert!(!is_shell_home_path(""));
        assert!(is_windows_drive_path("C:\\Users\\test\\repo"));
        assert!(is_windows_drive_path("D:/Users/test/repo"));
        assert!(!is_windows_drive_path("/Users/test/repo"));
    }
}

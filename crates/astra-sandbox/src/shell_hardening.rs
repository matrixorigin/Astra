//! Shell security hardening for command execution.
//!
//! Provides:
//! - Extglob/extended-glob disabling (prevents malicious filename expansion after validation)
//! - IFS reset (prevents word-splitting attacks)
//! - Secret scrubbing from subprocess environments
//! - Dangerous file path detection

use std::collections::HashMap;

/// Paths/directories that always require manual approval even in permissive modes.
/// Modifying these can compromise the system, the shell, or the agent itself.
pub const DANGEROUS_FILE_PATHS: &[&str] = &[
    // Git/agent authority metadata and credentials. Ordinary project settings,
    // including IDE and shell configuration, follow workspace write authority.
    ".git/", ".claude/", ".astra/", ".ssh/",
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
    /// Scrub secret environment variables from subprocess.
    pub scrub_secrets: bool,
}

impl Default for ShellHardeningConfig {
    fn default() -> Self {
        Self {
            pipefail: true,
            disable_extglob: true,
            reset_ifs: true,
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
///
/// Stdin is selected by the process owner, never rewritten into command text.
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
        return user_command.to_string();
    }

    parts.push(user_command.to_string());
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
/// matches a known dangerous pattern.
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
        assert!(cmd.ends_with("echo hello"));
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
            scrub_secrets: false,
        };
        let cmd = build_hardened_command(&config, "echo hello");
        assert_eq!(cmd, "echo hello");
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
        assert!(!is_dangerous_file_path(".gitconfig"));
    }

    #[test]
    fn ordinary_project_configuration_is_not_protected_metadata() {
        for path in [
            ".bashrc",
            ".zshrc",
            ".profile",
            ".gitmodules",
            ".gitconfig",
            ".vscode/settings.json",
            ".idea/workspace.xml",
            ".ripgreprc",
        ] {
            assert!(!is_dangerous_file_path(path), "{path}");
        }
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
        assert!(!is_dangerous_file_path("/home/user/.bashrc"));
        assert!(!is_dangerous_file_path(".bashrc"));
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
            scrub_secrets: false,
        };
        let cmd = build_hardened_command(&config, "ls");
        assert!(!cmd.contains("pipefail"));
        assert!(cmd.contains("IFS="));
        assert!(!cmd.contains("extglob"));
    }

    #[test]
    fn hardening_preserves_the_user_pipeline_and_process_input() {
        let script = "cargo test 2>&1 | tail -20";
        let cmd = build_hardened_command(&ShellHardeningConfig::default(), script);
        assert!(cmd.contains("pipefail"));
        assert!(cmd.ends_with(script));
        assert!(!cmd.contains("< /dev/null"));
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

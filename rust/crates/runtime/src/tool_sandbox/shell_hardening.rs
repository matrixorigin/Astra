//! Shell security hardening for command execution.
//!
//! Inspired by Claude Code's `bashProvider.ts`, `subprocessEnv.ts`, and `shellQuote.ts`.
//! Provides:
//! - Extglob/extended-glob disabling (prevents malicious filename expansion after validation)
//! - IFS reset (prevents word-splitting attacks)
//! - Stdin redirect to /dev/null (prevents commands from reading spawn's stdin pipe)
//! - Secret scrubbing from subprocess environments
//! - Dangerous file path detection

use std::collections::HashMap;

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
    ".kiro/",
    // SSH keys
    ".ssh/",
    // Ripgrep config (can alter search behavior)
    ".ripgreprc",
];

/// Environment variables that contain secrets and must be scrubbed from subprocesses.
/// Inspired by Claude Code's `subprocessEnv.ts` scrub list.
pub const SENSITIVE_ENV_VARS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "ACTIONS_ID_TOKEN_REQUEST_TOKEN",
    "ACTIONS_RUNTIME_TOKEN",
    "AZURE_CLIENT_SECRET",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "DATABASE_URL",
    "REDIS_URL",
    "MEMORIA_MASTER_KEY",
    "ASTRA_SECRET_KEY",
    "JWT_SECRET_KEY",
    "ENCRYPTION_KEY",
    "FERNET_KEY",
];

/// Configuration for shell hardening features.
#[derive(Debug, Clone)]
pub struct ShellHardeningConfig {
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
/// 1. Extglob disable (prevents malicious filename expansion post-validation)
/// 2. IFS reset (prevents word-splitting attacks)
/// 3. Stdin redirect to /dev/null (prevents stdin pipe hijacking)
///
/// The preamble is compatible with both bash and zsh.
pub fn build_hardened_command(config: &ShellHardeningConfig, user_command: &str) -> String {
    let mut parts = Vec::new();

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
        parts.push("IFS=$' \\t\\n'".to_string());
    }

    if parts.is_empty() {
        return apply_stdin_redirect(config, user_command);
    }

    parts.push(apply_stdin_redirect(config, user_command));
    parts.join(" && ")
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
pub fn is_dangerous_file_path(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    // Check each path component, not just substring, to avoid matching
    // "my.profile.rs" against ".profile".
    for &dangerous in DANGEROUS_FILE_PATHS {
        if dangerous.ends_with('/') {
            // Directory pattern: substring match is correct (e.g. ".git/" in any position).
            if normalized.contains(dangerous) {
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

fn apply_stdin_redirect(config: &ShellHardeningConfig, command: &str) -> String {
    if !config.redirect_stdin {
        return command.to_string();
    }
    // Don't add redirect if command already has stdin redirect or uses heredoc.
    let lower = command.to_lowercase();
    if lower.contains("< ") || lower.contains("<<") || lower.contains("<<<") {
        return command.to_string();
    }
    // Don't add redirect to pipe commands (would break the pipe).
    if command.contains('|') {
        return command.to_string();
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
        assert!(cmd.contains("extglob"), "should disable extglob");
        assert!(cmd.contains("IFS="), "should reset IFS");
        assert!(cmd.contains("< /dev/null"), "should redirect stdin");
        assert!(cmd.ends_with("echo hello < /dev/null"));
    }

    #[test]
    fn no_hardening_returns_raw_command() {
        let config = ShellHardeningConfig {
            disable_extglob: false,
            reset_ifs: false,
            redirect_stdin: false,
            scrub_secrets: false,
        };
        let cmd = build_hardened_command(&config, "echo hello");
        assert_eq!(cmd, "echo hello");
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
        assert!(!cmd.contains("< /dev/null"));
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
        assert!(is_dangerous_file_path(".ssh/id_rsa"));
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
    }

    // --- build_hardened_command edge cases ---

    #[test]
    fn hardened_command_only_extglob() {
        let config = ShellHardeningConfig {
            disable_extglob: true,
            reset_ifs: false,
            redirect_stdin: false,
            scrub_secrets: false,
        };
        let cmd = build_hardened_command(&config, "echo hello");
        assert!(cmd.contains("extglob"));
        assert!(!cmd.contains("IFS="));
        assert!(!cmd.contains("< /dev/null"));
    }

    #[test]
    fn hardened_command_only_ifs() {
        let config = ShellHardeningConfig {
            disable_extglob: false,
            reset_ifs: true,
            redirect_stdin: false,
            scrub_secrets: false,
        };
        let cmd = build_hardened_command(&config, "ls");
        assert!(cmd.contains("IFS="));
        assert!(!cmd.contains("extglob"));
    }

    #[test]
    fn stdin_redirect_skipped_for_herestring() {
        let config = ShellHardeningConfig::default();
        let cmd = build_hardened_command(&config, "cat <<< 'hello'");
        assert!(!cmd.ends_with("< /dev/null"));
    }

    #[test]
    fn stdin_redirect_skipped_for_pipe_chain() {
        let config = ShellHardeningConfig {
            disable_extglob: false,
            reset_ifs: false,
            redirect_stdin: true,
            scrub_secrets: false,
        };
        let cmd = build_hardened_command(&config, "echo hello | grep hello");
        assert!(!cmd.contains("< /dev/null"));
    }
}

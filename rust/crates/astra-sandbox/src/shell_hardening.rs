//! Shell security hardening for command execution.
//!
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
    "ASTRA_BRIDGE_SECRET",
    "ASTRA_EXTERNAL_JWT_SECRET",
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

/// Returns true when the text points at a generated per-session tool result.
///
/// `.astra/` remains dangerous for writes because it stores agent state and
/// configuration. Tool-result files are different: they are append-only run
/// artifacts that the agent must be able to read back in Auto mode to
/// summarize fanout output.
pub fn is_session_tool_result_artifact_reference(text: &str) -> bool {
    let normalized = text.replace('\\', "/");
    normalized.contains(".astra/sessions/") && normalized.contains("/tool-results/")
}

/// Returns true when a shell command appears to mutate a session tool result.
pub fn command_mutates_session_tool_result_artifact(command: &str) -> bool {
    if !is_session_tool_result_artifact_reference(command) {
        return false;
    }
    let lower = command.to_ascii_lowercase();
    [
        "rm ",
        "rm\t",
        "mv ",
        "mv\t",
        "cp ",
        "cp\t",
        "truncate ",
        "chmod ",
        "chown ",
        "shred ",
        "tee ",
        "sed -i",
        ">",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
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
    fn hardened_command_building() {
        // Default config adds all hardening
        let config = ShellHardeningConfig::default();
        let cmd = build_hardened_command(&config, "echo hello");
        assert!(cmd.contains("extglob"));
        assert!(cmd.contains("IFS="));
        assert!(cmd.contains("< /dev/null"));
        assert!(cmd.ends_with("echo hello < /dev/null"));

        // No hardening
        let config = ShellHardeningConfig {
            disable_extglob: false,
            reset_ifs: false,
            redirect_stdin: false,
            scrub_secrets: false,
        };
        assert_eq!(build_hardened_command(&config, "echo hello"), "echo hello");

        // Only extglob
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

        // Only IFS
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
    fn stdin_redirect_skipped_when_unnecessary() {
        let config = ShellHardeningConfig::default();
        let skip_cases: &[&str] = &[
            "cat <<EOF\nhello\nEOF",       // heredoc
            "wc -l < input.txt",           // existing redirect
            "cat file.txt | grep pattern", // pipe
            "cat <<< 'hello'",             // herestring
        ];
        for cmd in skip_cases {
            let result = build_hardened_command(&config, cmd);
            assert!(
                !result.ends_with("< /dev/null"),
                "should skip stdin for: {cmd}"
            );
        }
        // Pipe chain with only redirect_stdin enabled
        let config2 = ShellHardeningConfig {
            disable_extglob: false,
            reset_ifs: false,
            redirect_stdin: true,
            scrub_secrets: false,
        };
        assert!(
            !build_hardened_command(&config2, "echo hello | grep hello").contains("< /dev/null")
        );
    }
    #[test]
    fn secret_scrubbing_rules() {
        // Known secrets: API_KEY/_SECRET patterns
        let mut env = HashMap::from([
            ("PATH".into(), "/usr/bin".into()),
            ("HOME".into(), "/home/user".into()),
            ("ANTHROPIC_API_KEY".into(), "sk-secret".into()),
            ("AWS_SECRET_ACCESS_KEY".into(), "AKIA...".into()),
            ("INPUT_ANTHROPIC_API_KEY".into(), "sk-secret".into()),
        ]);
        scrub_secrets_from_env(&mut env);
        assert!(env.contains_key("PATH"));
        assert!(env.contains_key("HOME"));
        assert!(!env.contains_key("ANTHROPIC_API_KEY"));
        assert!(!env.contains_key("AWS_SECRET_ACCESS_KEY"));
        assert!(!env.contains_key("INPUT_ANTHROPIC_API_KEY"));

        // Heuristic: _SECRET/_PASSWORD/_PRIVATE_KEY, case-insensitive
        let mut env = HashMap::from([
            ("MY_SECRET_VALUE".into(), "v".into()),
            ("DB_PASSWORD".into(), "p".into()),
            ("GITHUB_PRIVATE_KEY".into(), "k".into()),
            ("NORMAL_VAR".into(), "visible".into()),
            ("my_secret".into(), "v1".into()),
            ("My_Password".into(), "v2".into()),
        ]);
        scrub_secrets_from_env(&mut env);
        assert!(!env.contains_key("MY_SECRET_VALUE"));
        assert!(!env.contains_key("DB_PASSWORD"));
        assert!(!env.contains_key("GITHUB_PRIVATE_KEY"));
        assert!(!env.contains_key("my_secret"));
        assert!(!env.contains_key("My_Password"));
        assert!(env.contains_key("NORMAL_VAR"));

        // Token vars preserved (gh CLI needs them)
        let mut env = HashMap::from([
            ("GITHUB_TOKEN".into(), "ghp_xxx".into()),
            ("GH_TOKEN".into(), "ghp_yyy".into()),
            ("TOKEN_TYPE".into(), "bearer".into()),
            ("TOKEN_ENDPOINT".into(), "https://auth.example.com".into()),
        ]);
        scrub_secrets_from_env(&mut env);
        assert_eq!(env.len(), 4);

        // Empty env
        let mut env = HashMap::new();
        scrub_secrets_from_env(&mut env);
        assert!(env.is_empty());
    }
    #[test]
    fn dangerous_file_path_detection() {
        // Git internals (including nested)
        for path in [
            ".git/config",
            "/home/user/.git/hooks/pre-commit",
            ".gitconfig",
            "repo/.git/config",
            ".git/HEAD",
        ] {
            assert!(is_dangerous_file_path(path), "should detect: {path}");
        }

        // Shell configs
        for path in [".bashrc", "/home/user/.zshrc", ".profile"] {
            assert!(is_dangerous_file_path(path), "should detect: {path}");
        }

        // SSH paths (including Windows backslash)
        for path in [
            ".ssh/id_rsa",
            "/home/user/.ssh/authorized_keys",
            r"C:\Users\.ssh\id_rsa",
        ] {
            assert!(is_dangerous_file_path(path), "should detect: {path}");
        }

        // Normal paths are allowed
        for path in ["src/main.rs", "README.md", "Cargo.toml", ""] {
            assert!(!is_dangerous_file_path(path), "should allow: {path:?}");
        }

        // Partial matches should not false-positive
        for path in [
            "my.profile.rs",
            "user.profile_settings",
            "src/bashrc_utils.rs",
        ] {
            assert!(
                !is_dangerous_file_path(path),
                "should not match partial: {path}"
            );
        }
        // But actual partial is still matched
        assert!(is_dangerous_file_path("/home/user/.bashrc"));
        assert!(is_dangerous_file_path(".bashrc"));
    }

    #[test]
    fn session_tool_result_artifact_detection() {
        for text in [
            "/Users/test/.astra/sessions/s1/tool-results/call_abc.txt",
            "cat ~/.astra/sessions/s1/tool-results/call_abc.txt",
        ] {
            assert!(
                is_session_tool_result_artifact_reference(text),
                "should detect session tool result artifact: {text}"
            );
        }

        for text in [
            "/Users/test/.astra/config.toml",
            "/Users/test/.astra/sessions/s1/messages.jsonl",
        ] {
            assert!(
                !is_session_tool_result_artifact_reference(text),
                "should not treat agent config/session journal as tool result artifact: {text}"
            );
        }

        assert!(!command_mutates_session_tool_result_artifact(
            "cat ~/.astra/sessions/s1/tool-results/call_abc.txt | python3 -c 'print(1)'"
        ));
        for command in [
            "rm -f ~/.astra/sessions/s1/tool-results/call_abc.txt",
            "cat ~/.astra/sessions/s1/tool-results/call_abc.txt | tee /tmp/out",
        ] {
            assert!(
                command_mutates_session_tool_result_artifact(command),
                "should flag mutating command: {command}"
            );
        }
    }
}

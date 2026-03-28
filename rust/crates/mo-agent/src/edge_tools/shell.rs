use super::*;

fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

const DEFAULT_SEARCH_EXCLUDE_DIRS: &[&str] = &[
    ".git",
    "target",
    "dist",
    "build",
    "coverage",
    "htmlcov",
    "node_modules",
    "vendor",
    ".venv",
    "venv",
    "__pycache__",
    ".next",
    ".nuxt",
    ".cache",
    "out",
];

fn append_default_grep_excludes(cmd: &mut Command) {
    cmd.arg("--binary-files=without-match");
    cmd.arg("--devices=skip");
    for dir in DEFAULT_SEARCH_EXCLUDE_DIRS {
        cmd.arg("--exclude-dir").arg(dir);
    }
}

fn default_find_prune_clause() -> String {
    let joined = DEFAULT_SEARCH_EXCLUDE_DIRS
        .iter()
        .map(|dir| format!("-name {}", shell_escape(dir)))
        .collect::<Vec<_>>()
        .join(" -o ");
    format!("\\( -type d \\( {joined} \\) -prune \\)")
}

/// SSRF protection: check if a URL targets internal/private networks.
/// Returns Some(reason) if blocked, None if safe.
fn is_ssrf_target(url: &str) -> Option<&'static str> {
    // Extract host from URL (simple parsing, handles http://host:port/path)
    let after_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let authority = after_scheme.split('/').next()?;
    // Handle userinfo@ prefix
    let host_port = authority.split('@').last()?;
    // Handle IPv6 brackets: [::1]:port → extract [::1]
    let host = if host_port.starts_with('[') {
        // IPv6: take everything up to and including the closing bracket
        host_port
            .split(']')
            .next()
            .map(|s| format!("{s}]"))
            .unwrap_or_default()
    } else {
        host_port.split(':').next().unwrap_or("").to_string()
    };
    let lower = host.to_ascii_lowercase();

    // Block localhost variants
    if lower == "localhost"
        || lower == "127.0.0.1"
        || lower == "0.0.0.0"
        || lower == "::1"
        || lower == "[::1]"
        || lower.ends_with(".localhost")
    {
        return Some("localhost access blocked");
    }
    // Block AWS/cloud metadata endpoints
    if lower == "169.254.169.254" || lower == "metadata.google.internal" {
        return Some("cloud metadata endpoint blocked");
    }
    // Block private IP ranges (RFC 1918 + link-local)
    if lower.starts_with("10.")
        || lower.starts_with("192.168.")
        || lower.starts_with("172.") && is_private_172(&lower)
        || lower.starts_with("169.254.")
        || lower.starts_with("fc")
        || lower.starts_with("fd")
    {
        return Some("private network access blocked");
    }
    None
}

/// Check if a 172.x.x.x address is in the private range 172.16-31.x.x
fn is_private_172(host: &str) -> bool {
    if let Some(second) = host.strip_prefix("172.").and_then(|r| r.split('.').next()) {
        if let Ok(n) = second.parse::<u8>() {
            return (16..=31).contains(&n);
        }
    }
    false
}

impl ToolExecutor {
    pub(crate) fn run_shell_output(
        &self,
        command: &str,
        timeout_secs: f64,
    ) -> Result<std::process::Output, String> {
        // If sandbox is active, wrap command with resource limits
        let effective_command = if let Some(ref policy) = self.sandbox_policy {
            if !matches!(policy.mode, SandboxMode::Permissive) {
                wrap_command_with_limits(policy, command)
            } else {
                command.to_string()
            }
        } else {
            command.to_string()
        };

        let mut child_cmd = Command::new("bash");
        child_cmd
            .arg("-c")
            .arg(&effective_command)
            .current_dir(&self.project_root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Apply sandbox environment filtering
        if let Some(ref policy) = self.sandbox_policy
            && !matches!(policy.mode, SandboxMode::Permissive)
            && let Err(e) = sandbox_command(policy, &mut child_cmd)
        {
            eprintln!("[sandbox] failed to apply policy: {e}");
            return Err(format!("Error: sandbox policy application failed: {e}"));
        }

        let mut child = child_cmd.spawn().map_err(|e| format!("Error: {e}"))?;

        let deadline = std::time::Instant::now() + Duration::from_secs_f64(timeout_secs);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if std::time::Instant::now() > deadline {
                        if let Err(e) = child.kill() {
                            eprintln!("[shell] failed to kill timed-out child: {e}");
                        }
                        // Reap the zombie process to prevent resource leak
                        let _ = child.wait();
                        return Err(format!("Error: command timed out after {timeout_secs}s"));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(format!("Error: {e}")),
            }
        }

        child.wait_with_output().map_err(|e| format!("Error: {e}"))
    }

    pub(crate) fn bash(&self, args: &Value) -> String {
        let command = match args.get("command").and_then(Value::as_str) {
            Some(c) => c,
            None => return "Error: missing 'command'".to_string(),
        };
        let timeout_secs = args.get("timeout").and_then(Value::as_f64).unwrap_or(30.0);

        match self.run_shell_output(command, timeout_secs) {
            Err(error) => error,
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                let mut result = String::new();
                if !stdout.is_empty() {
                    result.push_str(&stdout);
                }
                if !stderr.is_empty() {
                    if !result.is_empty() {
                        result.push('\n');
                    }
                    result.push_str(&stderr);
                }
                if result.is_empty() {
                    if out.status.success() {
                        "(no output)".to_string()
                    } else {
                        format!("(exit code {})", out.status.code().unwrap_or(-1))
                    }
                } else {
                    if result.len() > 20_000 {
                        result.truncate(20_000);
                        result.push_str("\n[truncated]");
                    }
                    result
                }
            }
        }
    }

    pub(crate) fn grep(&self, args: &Value) -> String {
        let pattern = match args.get("pattern").and_then(Value::as_str) {
            Some(p) => p,
            None => return "Error: missing 'pattern'".to_string(),
        };
        let search_path = args
            .get("path")
            .and_then(Value::as_str)
            .map(|p| self.resolve(p))
            .unwrap_or_else(|| self.project_root.clone());

        // Validate search path exists before spawning grep
        if !search_path.exists() {
            return format!(
                "Error: path '{}' does not exist. Use list_dir to see available files/directories.",
                search_path.display()
            );
        }

        let include = args.get("include").and_then(Value::as_str).unwrap_or("*");
        let case_sensitive = args
            .get("case_sensitive")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let mut cmd = Command::new("grep");
        cmd.arg("-rn");
        if !case_sensitive {
            cmd.arg("-i");
        }
        append_default_grep_excludes(&mut cmd);
        cmd.arg("--include").arg(include);
        cmd.arg(pattern).arg(&search_path);
        cmd.current_dir(&self.project_root);

        match cmd.output() {
            Ok(out) => {
                let text = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                match out.status.code() {
                    Some(0) => {
                        if text.len() > 20_000 {
                            let mut t = text[..20_000].to_string();
                            t.push_str("\n[truncated]");
                            t
                        } else {
                            text.to_string()
                        }
                    }
                    Some(1) => {
                        // stderr may contain "No such file or directory" warnings
                        // even when exit code is 1 (grep found no matches but
                        // some paths were bad)
                        let warn = stderr.trim();
                        if warn.is_empty() {
                            "No matches found".to_string()
                        } else {
                            format!("No matches found (warnings: {warn})")
                        }
                    }
                    _ => {
                        let detail = stderr.trim();
                        if detail.is_empty() {
                            "Error: grep failed".to_string()
                        } else {
                            format!("Error: {detail}")
                        }
                    }
                }
            }
            Err(e) => format!("Error: {e}"),
        }
    }

    pub(crate) fn glob(&self, args: &Value) -> String {
        let pattern = match args.get("pattern").and_then(Value::as_str) {
            Some(p) => p,
            None => return "Error: missing 'pattern'".to_string(),
        };
        let base = args
            .get("path")
            .and_then(Value::as_str)
            .map(|p| self.resolve(p))
            .unwrap_or_else(|| self.project_root.clone());

        // Validate base path exists
        if !base.exists() {
            return format!(
                "Error: path '{}' does not exist. Use list_dir to see available files/directories.",
                base.display()
            );
        }

        let shell_cmd = format!(
            "cd {} && find . {} -o -name {} -print | sed 's|^./||' | head -200",
            shell_escape(base.to_string_lossy().as_ref()),
            default_find_prune_clause(),
            shell_escape(pattern.split('/').next_back().unwrap_or(pattern))
        );
        let out = Command::new("bash").arg("-c").arg(&shell_cmd).output();
        match out {
            Ok(o) => {
                let text = String::from_utf8_lossy(&o.stdout).to_string();
                if text.trim().is_empty() {
                    "No files found".to_string()
                } else {
                    text.to_string()
                }
            }
            Err(e) => format!("Error: {e}"),
        }
    }

    pub(crate) fn git_run(&self, args: &[&str]) -> String {
        match Command::new("git")
            .args(args)
            .current_dir(&self.project_root)
            .output()
        {
            Ok(out) => {
                let s = String::from_utf8_lossy(&out.stdout).to_string();
                let e = String::from_utf8_lossy(&out.stderr).to_string();
                if s.is_empty() && !e.is_empty() { e } else { s }
            }
            Err(e) => format!("Error: {e}"),
        }
    }

    pub(crate) fn git_diff(&self, args: &Value) -> String {
        let staged = args.get("staged").and_then(Value::as_bool).unwrap_or(false);
        let git_ref = args.get("ref").and_then(Value::as_str);
        let mut cmd_args = vec!["diff"];
        if staged {
            cmd_args.push("--staged");
        }
        let ref_owned;
        if let Some(r) = git_ref {
            ref_owned = r.to_string();
            cmd_args.push(&ref_owned);
        }
        let out = self.git_run(&cmd_args);
        if out.len() > 20_000 {
            let mut t = out[..20_000].to_string();
            t.push_str("\n[truncated]");
            t
        } else {
            out
        }
    }

    pub(crate) fn git_log(&self, args: &Value) -> String {
        let n = args.get("n").and_then(Value::as_u64).unwrap_or(10);
        self.git_run(&["log", "--oneline", &format!("-{n}")])
    }

    /// Show a specific commit's diff, message, and metadata.
    pub(crate) fn git_show(&self, args: &Value) -> String {
        let commit = match args.get("commit").and_then(Value::as_str) {
            Some(c) => c.to_string(),
            None => return "Error: missing 'commit' (SHA, branch, or tag)".to_string(),
        };
        // Validate: no shell metacharacters
        if commit.contains(|c: char| {
            !c.is_alphanumeric()
                && c != '-'
                && c != '_'
                && c != '.'
                && c != '/'
                && c != '~'
                && c != '^'
        }) {
            return "Error: invalid commit reference".to_string();
        }
        let stat_only = args
            .get("stat_only")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let file = args.get("file").and_then(Value::as_str);

        let mut cmd_args = vec!["show", "--no-color"];
        if stat_only {
            cmd_args.push("--stat");
        }
        cmd_args.push(&commit);
        // Optionally scope to a specific file
        if let Some(f) = file {
            cmd_args.push("--");
            cmd_args.push(f);
        }
        let out = self.git_run(&cmd_args);
        // Auto-truncate large diffs
        if out.len() > 30_000 {
            let mut t = out[..30_000].to_string();
            t.push_str("\n[truncated — use stat_only:true or file param to narrow]");
            t
        } else {
            out
        }
    }

    /// Fetch a URL and return its content (text or HTML→text).
    pub(crate) fn web_fetch(&self, args: &Value) -> String {
        let url = match args.get("url").and_then(Value::as_str) {
            Some(u) => u,
            None => return "Error: missing 'url'".to_string(),
        };
        // Basic URL validation
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return "Error: url must start with http:// or https://".to_string();
        }
        // SSRF protection: block internal/private IP ranges
        if let Some(reason) = is_ssrf_target(url) {
            return format!("Error: blocked URL ({reason})");
        }
        let max_bytes = args
            .get("max_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(10_000) as usize;
        let timeout_secs = args.get("timeout").and_then(Value::as_u64).unwrap_or(10);

        let mut cmd = Command::new("curl");
        cmd.args([
            "-sS",
            "-L",
            "--max-time",
            &timeout_secs.to_string(),
            "--max-filesize",
            &(max_bytes * 2).to_string(), // allow 2x for pre-truncation
            "-H",
            "User-Agent: mo-agent/0.1",
            url,
        ])
        .current_dir(&self.project_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

        // Apply sandbox environment filtering (same as bash)
        if let Some(ref policy) = self.sandbox_policy
            && !matches!(policy.mode, SandboxMode::Permissive)
            && let Err(e) = sandbox_command(policy, &mut cmd)
        {
            return format!("Error: sandbox policy application failed: {e}");
        }

        match cmd.output() {
            Ok(out) => {
                let status = out.status;
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                if !status.success() && stdout.is_empty() {
                    return format!("Error: {stderr}");
                }
                let mut result = stdout.to_string();
                if result.len() > max_bytes {
                    result.truncate(max_bytes);
                    result.push_str("\n[truncated]");
                }
                result
            }
            Err(e) => format!("Error: curl not available — {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_executor() -> ToolExecutor {
        ToolExecutor::new(std::env::temp_dir())
    }

    #[test]
    fn shell_escape_simple() {
        assert_eq!(shell_escape("hello"), "'hello'");
    }

    #[test]
    fn shell_escape_with_quotes() {
        assert_eq!(shell_escape("it's"), "'it'\\''s'");
    }

    #[test]
    fn bash_missing_command_returns_error() {
        let executor = test_executor();
        let result = executor.bash(&serde_json::json!({}));
        assert!(result.contains("Error"), "got: {result}");
    }

    #[test]
    fn bash_echo_returns_output() {
        let executor = test_executor();
        let result = executor.bash(&serde_json::json!({"command": "echo hello"}));
        assert!(result.trim().contains("hello"), "got: {result}");
    }

    #[test]
    fn bash_timeout_kills_process() {
        let executor = test_executor();
        let result = executor.bash(&serde_json::json!({"command": "sleep 10", "timeout": 0.2}));
        assert!(result.contains("timed out"), "got: {result}");
    }

    #[test]
    fn bash_failed_command_returns_output() {
        let executor = test_executor();
        let result = executor.bash(&serde_json::json!({"command": "echo err >&2 && false"}));
        assert!(result.contains("err"), "got: {result}");
    }

    #[test]
    fn grep_missing_pattern_returns_error() {
        let executor = test_executor();
        let result = executor.grep(&serde_json::json!({}));
        assert!(result.contains("Error"), "got: {result}");
    }

    #[test]
    fn grep_nonexistent_path_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        let result = executor.grep(&serde_json::json!({
            "pattern": "hello",
            "path": "src-tauri/src"
        }));
        assert!(result.contains("Error"), "should error on missing path, got: {result}");
        assert!(result.contains("does not exist"), "should mention path doesn't exist, got: {result}");
        assert!(result.contains("list_dir"), "should suggest list_dir, got: {result}");
    }

    #[test]
    fn grep_nonexistent_absolute_path_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        let result = executor.grep(&serde_json::json!({
            "pattern": "hello",
            "path": "/nonexistent/fake/directory"
        }));
        assert!(result.contains("Error"), "should error on missing path, got: {result}");
        assert!(result.contains("does not exist"), "got: {result}");
    }

    #[test]
    fn grep_finds_pattern_in_file() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        std::fs::write(dir.path().join("test.txt"), "hello world\nfoo bar").unwrap();

        let result = executor.grep(&serde_json::json!({"pattern": "foo", "path": "."}));
        assert!(result.contains("foo bar"), "got: {result}");
    }

    #[test]
    fn grep_no_match_returns_message() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        std::fs::write(dir.path().join("test.txt"), "hello").unwrap();

        let result = executor.grep(&serde_json::json!({"pattern": "zzzzz", "path": "."}));
        assert!(result.contains("No matches"), "got: {result}");
    }

    #[test]
    fn grep_skips_default_generated_directories() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join("dist")).unwrap();
        std::fs::write(dir.path().join("src").join("app.rs"), "needle in source").unwrap();
        std::fs::write(dir.path().join("dist").join("bundle.js"), "needle in build").unwrap();

        let result = executor.grep(&serde_json::json!({"pattern": "needle", "path": "."}));
        assert!(result.contains("src/app.rs"), "got: {result}");
        assert!(
            !result.contains("dist/bundle.js"),
            "default grep should skip bulky dirs: {result}"
        );
    }

    #[test]
    fn glob_skips_default_generated_directories() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("src").join("main.rs"), "").unwrap();
        std::fs::write(dir.path().join("target").join("cached.rs"), "").unwrap();

        let result = executor.glob(&serde_json::json!({"pattern": "*.rs", "path": "."}));
        assert!(result.contains("src/main.rs"), "got: {result}");
        assert!(
            !result.contains("target/cached.rs"),
            "default glob should skip bulky dirs: {result}"
        );
    }

    #[test]
    fn glob_nonexistent_path_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        let result = executor.glob(&serde_json::json!({
            "pattern": "*.py",
            "path": "nonexistent/directory"
        }));
        assert!(result.contains("Error"), "should error on missing path, got: {result}");
        assert!(result.contains("does not exist"), "got: {result}");
        assert!(result.contains("list_dir"), "should suggest list_dir, got: {result}");
    }

    #[test]
    fn git_log_returns_something_in_git_repo() {
        // This test runs in the actual repo
        let executor =
            ToolExecutor::new(std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir()));
        let result = executor.git_log(&serde_json::json!({"n": 3}));
        // May or may not be in a git repo, just verify no panic
        assert!(!result.is_empty());
    }

    // ── str_replace diff preview ─────────────────────────────────────────────

    #[test]
    fn str_replace_shows_diff_preview() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        std::fs::write(
            dir.path().join("code.rs"),
            "fn main() {\n    println!(\"old\");\n}\n",
        )
        .unwrap();

        let result = executor.str_replace(&serde_json::json!({
            "path": "code.rs",
            "old_str": "println!(\"old\")",
            "new_str": "println!(\"new\")"
        }));
        assert!(result.contains("Replaced successfully"), "got: {result}");
        assert!(result.contains("- "), "should have - line: {result}");
        assert!(result.contains("+ "), "should have + line: {result}");
        assert!(result.contains("old"), "should show old text: {result}");
        assert!(result.contains("new"), "should show new text: {result}");
    }

    #[test]
    fn str_replace_large_diff_shows_summary() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        // Create a file with many lines
        let old_block: String = (0..20).map(|i| format!("line {i}\n")).collect();
        let new_block: String = (0..25).map(|i| format!("new_line {i}\n")).collect();
        let content = format!("header\n{old_block}footer\n");
        std::fs::write(dir.path().join("big.txt"), &content).unwrap();

        let result = executor.str_replace(&serde_json::json!({
            "path": "big.txt",
            "old_str": old_block.trim_end(),
            "new_str": new_block.trim_end()
        }));
        assert!(result.contains("Replaced successfully"), "got: {result}");
        assert!(
            result.contains("lines →"),
            "large diff should show summary: {result}"
        );
    }

    // ── resolve_checked sandbox ──────────────────────────────────────────────

    #[test]
    fn resolve_checked_with_permissive_sandbox_allows_all() {
        use mo_agent_runtime::tool_sandbox::SandboxPolicy;
        let dir = tempfile::tempdir().unwrap();
        let mut executor = ToolExecutor::new(dir.path());
        // Permissive policy → all paths allowed
        executor.sandbox_policy = Some(SandboxPolicy::permissive(dir.path()));
        let result = executor.resolve_checked("/etc/passwd");
        assert!(result.is_ok(), "should allow with permissive: {result:?}");
    }

    #[test]
    fn resolve_checked_with_sandbox_blocks_escape() {
        use mo_agent_runtime::tool_sandbox::SandboxPolicy;
        let dir = tempfile::tempdir().unwrap();
        let mut executor = ToolExecutor::new(dir.path());
        let mut policy = SandboxPolicy::strict(dir.path());
        policy.allowed_paths.clear();
        executor.sandbox_policy = Some(policy);

        let result = executor.resolve_checked("/etc/passwd");
        assert!(result.is_err(), "should block path escape: {result:?}");
        assert!(
            result.unwrap_err().contains("Sandbox"),
            "should mention sandbox"
        );
    }

    // ── SSRF protection ─────────────────────────────────────────────────────

    #[test]
    fn ssrf_blocks_localhost() {
        assert!(is_ssrf_target("http://127.0.0.1:8080/secret").is_some());
        assert!(is_ssrf_target("http://localhost/admin").is_some());
        assert!(is_ssrf_target("http://0.0.0.0:3000").is_some());
        assert!(is_ssrf_target("http://[::1]/api").is_some());
    }

    #[test]
    fn ssrf_blocks_private_networks() {
        assert!(is_ssrf_target("http://10.0.0.1/internal").is_some());
        assert!(is_ssrf_target("http://192.168.1.1/router").is_some());
        assert!(is_ssrf_target("http://172.16.0.1/service").is_some());
        assert!(is_ssrf_target("http://172.31.255.1/db").is_some());
        // 172.15 and 172.32 are NOT private
        assert!(is_ssrf_target("http://172.15.0.1/ok").is_none());
        assert!(is_ssrf_target("http://172.32.0.1/ok").is_none());
    }

    #[test]
    fn ssrf_blocks_cloud_metadata() {
        assert!(is_ssrf_target("http://169.254.169.254/latest/meta-data/").is_some());
        assert!(is_ssrf_target("http://metadata.google.internal/computeMetadata/v1/").is_some());
    }

    #[test]
    fn ssrf_allows_public_urls() {
        assert!(is_ssrf_target("https://github.com/matrixorigin/matrixone").is_none());
        assert!(is_ssrf_target("https://api.github.com/repos").is_none());
        assert!(is_ssrf_target("http://example.com").is_none());
        assert!(is_ssrf_target("https://docs.rs/tokio/latest").is_none());
    }

    // ── git_show ──────────────────────────────────────────────────────────────

    #[test]
    fn git_show_missing_commit_returns_error() {
        let executor = test_executor();
        let result = executor.git_show(&serde_json::json!({}));
        assert!(result.contains("Error"), "got: {result}");
    }

    #[test]
    fn git_show_invalid_ref_returns_error() {
        let executor = test_executor();
        let result = executor.git_show(&serde_json::json!({"commit": "abc; rm -rf /"}));
        assert!(result.contains("invalid"), "got: {result}");
    }

    #[test]
    fn git_show_head_returns_content() {
        // Run in actual repo
        let executor =
            ToolExecutor::new(std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir()));
        let result = executor.git_show(&serde_json::json!({"commit": "HEAD"}));
        // Either shows commit info or git error (if not in repo)
        assert!(!result.is_empty());
    }

    #[test]
    fn git_show_stat_only() {
        let executor =
            ToolExecutor::new(std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir()));
        let result = executor.git_show(&serde_json::json!({"commit": "HEAD", "stat_only": true}));
        assert!(!result.is_empty());
    }

    // ── web_fetch ─────────────────────────────────────────────────────────────

    #[test]
    fn web_fetch_missing_url_returns_error() {
        let executor = test_executor();
        let result = executor.web_fetch(&serde_json::json!({}));
        assert!(result.contains("Error"), "got: {result}");
    }

    #[test]
    fn web_fetch_invalid_scheme_returns_error() {
        let executor = test_executor();
        let result = executor.web_fetch(&serde_json::json!({"url": "ftp://example.com"}));
        assert!(result.contains("http"), "got: {result}");
    }
}

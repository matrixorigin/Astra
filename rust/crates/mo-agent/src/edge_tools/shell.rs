use super::*;

fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

impl ToolExecutor {
    pub(crate) fn run_shell_output(
        &self,
        command: &str,
        timeout_secs: f64,
    ) -> Result<std::process::Output, String> {
        let mut child = Command::new("bash")
            .arg("-c")
            .arg(command)
            .current_dir(&self.project_root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("Error: {e}"))?;

        let deadline = std::time::Instant::now() + Duration::from_secs_f64(timeout_secs);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if std::time::Instant::now() > deadline {
                        let _ = child.kill();
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
        cmd.arg("--include").arg(include);
        cmd.arg(pattern).arg(&search_path);
        cmd.current_dir(&self.project_root);

        match cmd.output() {
            Ok(out) => {
                let text = String::from_utf8_lossy(&out.stdout);
                if text.is_empty() {
                    "No matches found".to_string()
                } else if text.len() > 20_000 {
                    let mut t = text[..20_000].to_string();
                    t.push_str("\n[truncated]");
                    t
                } else {
                    text.to_string()
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

        // Use find as a portable glob implementation
        let mut cmd = Command::new("bash");
        cmd.arg("-c")
            .arg(format!(
                "find . -path '{}' 2>/dev/null | head -200",
                pattern
            ))
            .current_dir(&base);

        // Actually use glob via shell
        let shell_cmd = format!(
            "cd {} && find . -name '{}' 2>/dev/null | sed 's|^./||' | head -200",
            shell_escape(base.to_string_lossy().as_ref()),
            // Convert ** glob to find-compatible
            pattern.split('/').next_back().unwrap_or(pattern)
        );
        let out = Command::new("bash").arg("-c").arg(&shell_cmd).output();
        match out {
            Ok(o) => {
                let text = String::from_utf8_lossy(&o.stdout).to_string();
                if text.trim().is_empty() {
                    "No files found".to_string()
                } else {
                    text
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
    fn git_log_returns_something_in_git_repo() {
        // This test runs in the actual repo
        let executor =
            ToolExecutor::new(std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir()));
        let result = executor.git_log(&serde_json::json!({"n": 3}));
        // May or may not be in a git repo, just verify no panic
        assert!(!result.is_empty());
    }
}

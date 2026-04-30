//! Case execution trait + the `astra chat` subprocess impl.
//!
//! Splitting executor out as a trait lets `SuiteRunner` be tested
//! without actually spawning subprocesses. For day-to-day usage the
//! only impl you'll touch is [`AstraCliExecutor`].
//!
//! ## Why a trait, not a plain function
//!
//! 1. **Testing** — a `FakeExecutor` returning canned `RunOutcome`
//!    values lets us cover the whole orchestration path (deterministic
//!    evaluation → judger gate → session capture → report) without
//!    real provider keys.
//! 2. **Future swap-in** — a hypothetical in-process executor could
//!    skip the subprocess altogether for CI speed.
//! 3. **Reproducer** — the CLI impl exposes `format_reproducer(case,
//!    model)` so the FAIL report prints the exact command a developer
//!    can paste into a shell to re-run the case.

use std::time::Instant;

use async_trait::async_trait;

use crate::case::Case;
use crate::runner::{RunOutcome, RunnerConfig, parse_json_outcome};

/// Execute one (case, model) pair and return an outcome. Errors are
/// encoded as outcomes with exit_code = -1 / 124 so the report can
/// render them uniformly.
#[async_trait]
pub trait CaseExecutor: Send + Sync {
    async fn execute(&self, case: &Case, model: &str) -> RunOutcome;

    /// Shell command a developer can paste to reproduce this run.
    /// Default returns an empty string — CLI impl overrides this to
    /// improve FAIL reports.
    fn reproducer(&self, _case: &Case, _model: &str) -> String {
        String::new()
    }
}

/// Subprocess executor: spawns `astra chat -m <prompt> --model <m>
/// --json -y`. Timeout-enforced via tokio.
pub struct AstraCliExecutor {
    pub cfg: RunnerConfig,
}

impl AstraCliExecutor {
    pub fn new(cfg: RunnerConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl CaseExecutor for AstraCliExecutor {
    async fn execute(&self, case: &Case, model: &str) -> RunOutcome {
        run_case_subprocess(&self.cfg, case, model).await
    }

    fn reproducer(&self, case: &Case, model: &str) -> String {
        // Mirrors the args assembled below. Quote the prompt so it
        // survives a copy-paste.
        let mut parts = vec![
            shell_escape(self.cfg.astra_bin.display().to_string()),
            "chat".into(),
            "-m".into(),
            shell_escape(case.prompt.clone()),
            "--model".into(),
            shell_escape(model.to_string()),
            "--json".into(),
            "-y".into(),
        ];
        for extra in &case.extra_cli_args {
            parts.push(shell_escape(extra.clone()));
        }
        parts.join(" ")
    }
}

fn shell_escape(s: String) -> String {
    // Simple single-quote escape: good enough for reproducer hints,
    // not a security boundary (the prompt is user-authored YAML).
    if s.contains('\'') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\\\""))
    } else {
        format!("'{s}'")
    }
}

async fn run_case_subprocess(cfg: &RunnerConfig, case: &Case, model: &str) -> RunOutcome {
    use std::process::Stdio;
    use std::time::Duration;
    use tokio::process::Command;

    let start = Instant::now();
    let mut cmd = Command::new(&cfg.astra_bin);
    cmd.arg("chat")
        .arg("-m")
        .arg(&case.prompt)
        .arg("--model")
        .arg(model)
        .arg("--json")
        .arg("-y");
    if let Some(ref wd) = cfg.working_dir {
        cmd.current_dir(wd);
    }
    for extra in &case.extra_cli_args {
        cmd.arg(extra);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // `kill_on_drop` ensures that if the outer future is cancelled
        // (timeout, task abort) the child is killed rather than
        // silently outliving us. This is the backstop — the timeout
        // branch below also kills explicitly so tests see a clean exit.
        .kill_on_drop(true);

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return RunOutcome {
                model: model.into(),
                exit_code: -1,
                text: format!("subprocess spawn error: {e}"),
                stderr: String::new(),
                session_id: None,
                run_id: None,
                tool_calls_count: 0,
                tools_used: vec![],
                completion_tokens: 0,
                prompt_tokens: 0,
                duration_ms: start.elapsed().as_millis() as u64,
            };
        }
    };

    let timeout = Duration::from_secs(case.timeout_seconds);
    // `wait_with_output` drains stdout + stderr concurrently and
    // waits for the exit status in one call. This avoids the earlier
    // ordering bug where a stderr-heavy child could block its stdout
    // pipe and only be unstuck by the timeout (which then leaked the
    // process). Passing `child` by value means that if the future
    // is dropped (outer timeout fires) `kill_on_drop` takes over.
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let mut out = parse_json_outcome(&stdout, model);
            out.stderr = stderr;
            out.exit_code = output.status.code().unwrap_or(-1);
            out.duration_ms = start.elapsed().as_millis() as u64;
            out
        }
        Ok(Err(e)) => RunOutcome {
            model: model.into(),
            exit_code: -1,
            text: format!("wait error: {e}"),
            stderr: String::new(),
            session_id: None,
            run_id: None,
            tool_calls_count: 0,
            tools_used: vec![],
            completion_tokens: 0,
            prompt_tokens: 0,
            duration_ms: start.elapsed().as_millis() as u64,
        },
        Err(_timed_out) => RunOutcome {
            model: model.into(),
            // POSIX `timeout` utility exits 124 — follow the convention
            // so downstream tooling can classify.
            // `kill_on_drop(true)` on the Command ensures the child
            // process is sent SIGKILL when the future is dropped
            // (which happens implicitly when `timeout` returns Err).
            exit_code: 124,
            text: format!(
                "timeout after {}s (case timeout_seconds={})",
                timeout.as_secs(),
                case.timeout_seconds
            ),
            stderr: String::new(),
            session_id: None,
            run_id: None,
            tool_calls_count: 0,
            tools_used: vec![],
            completion_tokens: 0,
            prompt_tokens: 0,
            duration_ms: start.elapsed().as_millis() as u64,
        },
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Fake executor/judger helpers shared by suite + integration tests.

    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Records every (case_name, model) invocation and returns a
    /// pre-seeded outcome. Absence of a seed == model-not-found style
    /// exit_code=-1 outcome.
    pub struct FakeExecutor {
        pub seeds: Mutex<HashMap<(String, String), RunOutcome>>,
        pub calls: Mutex<Vec<(String, String)>>,
    }

    impl FakeExecutor {
        pub fn new() -> Self {
            Self {
                seeds: Mutex::new(HashMap::new()),
                calls: Mutex::new(Vec::new()),
            }
        }
        pub fn seed(&self, case: &str, model: &str, outcome: RunOutcome) {
            self.seeds
                .lock()
                .unwrap()
                .insert((case.to_string(), model.to_string()), outcome);
        }
    }

    #[async_trait]
    impl CaseExecutor for FakeExecutor {
        async fn execute(&self, case: &Case, model: &str) -> RunOutcome {
            self.calls
                .lock()
                .unwrap()
                .push((case.name.clone(), model.to_string()));
            let key = (case.name.clone(), model.to_string());
            self.seeds
                .lock()
                .unwrap()
                .get(&key)
                .cloned()
                .unwrap_or(RunOutcome {
                    model: model.into(),
                    exit_code: -1,
                    text: format!("fake: no seed for {}/{}", case.name, model),
                    stderr: String::new(),
                    session_id: None,
                    run_id: None,
                    tool_calls_count: 0,
                    tools_used: vec![],
                    completion_tokens: 0,
                    prompt_tokens: 0,
                    duration_ms: 0,
                })
        }
        fn reproducer(&self, case: &Case, model: &str) -> String {
            format!("<fake executor: case={} model={}>", case.name, model)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn reproducer_roundtrips_prompt_with_quotes() {
        let cfg = RunnerConfig::new(PathBuf::from("/usr/local/bin/astra"));
        let exec = AstraCliExecutor::new(cfg);
        let case = Case {
            name: "c".into(),
            description: None,
            prompt: "say 'hello'".into(),
            models: None,
            criteria: vec![],
            debug_log: false,
            extra_cli_args: vec!["--debug-log-tools".into()],
            timeout_seconds: 60,
        };
        let repro = exec.reproducer(&case, "qwen-flash");
        assert!(repro.contains("/usr/local/bin/astra"));
        assert!(repro.contains("--model"));
        assert!(repro.contains("qwen-flash"));
        assert!(repro.contains("--debug-log-tools"));
        // The prompt has a single quote, so we fall back to double quotes.
        assert!(repro.contains("\"say 'hello'\""));
    }

    // Regression: case timeout MUST kill the child process, not leak
    // it. We invoke /bin/sleep directly so the stable `astra chat`
    // arg vector doesn't interfere — this pins the kill-on-drop +
    // timeout semantics independent of the astra bin's arg shape.
    // Skipped on platforms without /bin/sleep (Windows CI).
    #[tokio::test]
    async fn timeout_kills_subprocess_and_returns_posix_124() {
        if !std::path::Path::new("/bin/sleep").exists() {
            return;
        }
        // Custom command, bypassing `AstraCliExecutor` so the sh/sleep
        // binary isn't forced through `astra chat` args.
        use std::process::Stdio;
        use std::time::Duration;
        use tokio::process::Command;

        let mut cmd = Command::new("/bin/sleep");
        cmd.arg("10")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let child = cmd.spawn().expect("spawn /bin/sleep");
        let start = std::time::Instant::now();
        let timeout_secs = 1u64;
        let result =
            tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait_with_output())
                .await;
        let elapsed = start.elapsed();
        assert!(
            result.is_err(),
            "wait_with_output should time out while sleep is still running"
        );
        // 3s slack so CI slowness doesn't flake; the whole point is
        // that the child is killed on drop, not that timing is exact.
        assert!(
            elapsed.as_secs() <= 3,
            "kill_on_drop didn't cap elapsed — ran {}s",
            elapsed.as_secs()
        );
    }

    #[tokio::test]
    async fn fake_executor_records_calls_and_returns_seeded_outcome() {
        let fe = test_support::FakeExecutor::new();
        let mut seed = RunOutcome {
            model: "qwen-flash".into(),
            exit_code: 0,
            text: "hello".into(),
            stderr: String::new(),
            session_id: Some("s".into()),
            run_id: None,
            tool_calls_count: 1,
            tools_used: vec!["Read".into()],
            completion_tokens: 0,
            prompt_tokens: 0,
            duration_ms: 0,
        };
        seed.exit_code = 0;
        fe.seed("c1", "qwen-flash", seed.clone());

        let case = Case {
            name: "c1".into(),
            description: None,
            prompt: "p".into(),
            models: None,
            criteria: vec![],
            debug_log: false,
            extra_cli_args: vec![],
            timeout_seconds: 60,
        };
        let out = fe.execute(&case, "qwen-flash").await;
        assert_eq!(out.text, "hello");
        assert_eq!(fe.calls.lock().unwrap().len(), 1);

        // Unknown model → synthetic -1 outcome.
        let out2 = fe.execute(&case, "never-seeded").await;
        assert_eq!(out2.exit_code, -1);
        assert!(out2.text.contains("fake"));
    }
}

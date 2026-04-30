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
    use tokio::io::AsyncReadExt;
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
        .stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
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

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let timeout = Duration::from_secs(case.timeout_seconds);
    let wait_fut = async move {
        let mut stdout_buf = String::new();
        if let Some(mut p) = stdout_pipe {
            let _ = p.read_to_string(&mut stdout_buf).await;
        }
        let mut stderr_buf = String::new();
        if let Some(mut p) = stderr_pipe {
            let _ = p.read_to_string(&mut stderr_buf).await;
        }
        let status = child.wait().await;
        (status, stdout_buf, stderr_buf)
    };

    match tokio::time::timeout(timeout, wait_fut).await {
        Ok((Ok(status), stdout, stderr)) => {
            let mut out = parse_json_outcome(&stdout, model);
            out.stderr = stderr;
            out.exit_code = status.code().unwrap_or(-1);
            out.duration_ms = start.elapsed().as_millis() as u64;
            out
        }
        Ok((Err(e), stdout, stderr)) => RunOutcome {
            model: model.into(),
            exit_code: -1,
            text: format!("wait error: {e}; stdout={stdout}"),
            stderr,
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

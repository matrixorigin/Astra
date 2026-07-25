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
        let has_session_id_in_extras = has_session_id(&case.extra_cli_args);
        let mut parts = vec![
            shell_escape(self.cfg.astra_bin.display().to_string()),
            "chat".into(),
            "-m".into(),
            shell_escape(case.prompt.clone()),
        ];
        if !has_session_id_in_extras {
            parts.push("--no-resume".into());
        }
        parts.extend([
            "--model".into(),
            shell_escape(model.to_string()),
            "--json".into(),
            "-y".into(),
        ]);
        for extra in &case.extra_cli_args {
            parts.push(shell_escape(extra.clone()));
        }
        parts.join(" ")
    }
}

fn shell_escape(s: String) -> String {
    // POSIX single-quote escape: wrap in `'…'` and replace any inner
    // `'` with `'\''` (close, escaped-quote, re-open). This produces
    // a string that a POSIX shell ingests byte-for-byte — every other
    // character is literal inside single quotes, including `"`, `$`,
    // backticks, newlines. Our earlier "fall back to double quotes
    // when the string has `'`" approach lost fidelity on mixed-quote
    // prompts because inside `"…"` the shell still expands `$(…)`
    // and backticks.
    //
    // Not a security boundary — cases are developer-authored YAML —
    // but the reproducer promises "paste this into a shell to re-run"
    // and it should actually work.
    let empty = s.is_empty();
    let escaped = s.replace('\'', "'\\''");
    if empty {
        "''".to_string()
    } else {
        format!("'{escaped}'")
    }
}

fn has_session_id(args: &[String]) -> bool {
    args.iter()
        .any(|arg| arg == "--session-id" || arg.starts_with("--session-id="))
}

async fn run_case_subprocess(cfg: &RunnerConfig, case: &Case, model: &str) -> RunOutcome {
    use std::process::Stdio;
    use std::time::Duration;
    use tokio::process::Command;

    // Step-event files are append-only for a session. A continuation command
    // must report only the events it added, otherwise SuiteRunner sums the
    // entire prior transcript once per follow-up turn.
    let prior_step_stats = explicit_session_id(&case.extra_cli_args)
        .and_then(crate::session_capture::load_step_event_stats);
    let start = Instant::now();
    let mut cmd = Command::new(&cfg.astra_bin);
    if let Some(ref profile) = cfg.profile {
        cmd.arg("--profile").arg(profile);
    }
    cmd.arg("chat").arg("-m").arg(&case.prompt);
    // A missing --session-id deliberately means "create a session".  The
    // server, not the harness, owns session identity: inventing a UUID here
    // turns the first turn into an explicit resume request, which a correctly
    // strict server must reject because that session does not exist yet.
    //
    // SuiteRunner reads the server-issued id from this turn's JSON envelope
    // and adds --session-id only to follow-up turns.  Keep that one protocol
    // for every provider and transport rather than relying on a local session
    // creation side effect or a permissive server fallback.
    // `astra chat` normally resumes the most recent one-shot session, so make
    // the root run explicitly isolated as well.  Do not add this on follow-up
    // turns: there `--session-id` is the authoritative continuation request.
    if !has_session_id(&case.extra_cli_args) {
        cmd.arg("--no-resume");
    }
    cmd.arg("--model").arg(model).arg("--json").arg("-y");
    if let Some(ref wd) = cfg.working_dir {
        cmd.current_dir(wd);
    }
    for extra in &case.extra_cli_args {
        cmd.arg(extra);
    }
    for (k, v) in &case.env {
        cmd.env(k, v);
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
                final_state: None,
                interruption_kind: None,
                tool_result_class_counts: std::collections::BTreeMap::new(),
                tool_calls_count: 0,
                tools_used: vec![],
                completion_tokens: 0,
                prompt_tokens: 0,
                cached_input_tokens: 0,
                cache_creation_tokens: 0,
                duration_ms: start.elapsed().as_millis() as u64,
                turn_rounds: 0,
                cache_hits: 0,
                total_tool_calls: 0,
                ttft_ms: 0,
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
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            let mut out = parse_json_outcome(&stdout, model);
            out.stderr = stderr;
            out.exit_code = output.status.code().unwrap_or(-1);
            out.duration_ms = start.elapsed().as_millis() as u64;
            // Extract turn_rounds and cache_hits from step_events if available.
            if let Some(ref sid) = out.session_id
                && let Some(stats) = crate::session_capture::load_step_event_stats(sid)
            {
                let stats = match prior_step_stats.as_ref() {
                    Some(prior) => stats.since(prior),
                    None => stats,
                };
                out.turn_rounds = stats.turn_rounds;
                out.cache_hits = stats.cache_hits;
                out.total_tool_calls = stats.total_tool_calls;
            }
            out
        }
        Ok(Err(e)) => RunOutcome {
            model: model.into(),
            exit_code: -1,
            text: format!("wait error: {e}"),
            stderr: String::new(),
            session_id: None,
            run_id: None,
            final_state: None,
            interruption_kind: None,
            tool_result_class_counts: std::collections::BTreeMap::new(),
            tool_calls_count: 0,
            tools_used: vec![],
            completion_tokens: 0,
            prompt_tokens: 0,
            cached_input_tokens: 0,
            cache_creation_tokens: 0,
            duration_ms: start.elapsed().as_millis() as u64,
            turn_rounds: 0,
            cache_hits: 0,
            total_tool_calls: 0,
            ttft_ms: 0,
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
            final_state: Some("interrupted".into()),
            interruption_kind: Some("timeout".into()),
            tool_result_class_counts: std::collections::BTreeMap::new(),
            tool_calls_count: 0,
            tools_used: vec![],
            completion_tokens: 0,
            prompt_tokens: 0,
            cached_input_tokens: 0,
            cache_creation_tokens: 0,
            duration_ms: start.elapsed().as_millis() as u64,
            turn_rounds: 0,
            cache_hits: 0,
            total_tool_calls: 0,
            ttft_ms: 0,
        },
    }
}

fn explicit_session_id(args: &[String]) -> Option<&str> {
    let id = args.iter().enumerate().find_map(|(index, arg)| {
        if arg == "--session-id" {
            args.get(index + 1).map(String::as_str)
        } else {
            arg.strip_prefix("--session-id=")
        }
    })?;
    // Do not let a malformed extra argument turn observability into a panic:
    // the child CLI will report the user-facing argument error, while the
    // harness simply has no prior-session snapshot to subtract.
    astra_services::session_journal::validate_session_id(id)
        .ok()
        .map(|_| id)
}

// ── External command executor adapter ────────────────────────────────

/// Executor that delegates case execution to an external process.
///
/// Usage: `--executor-cmd "python3 my_agent.py"`
///
/// The external process receives the case JSON on stdin:
/// ```json
/// {"name": "...", "prompt": "...", "model": "...", "timeout_seconds": 180}
/// ```
/// And must return a RunOutcome-compatible JSON on stdout:
/// ```json
/// {"exit_code": 0, "text": "...", "tools_used": [...], ...}
/// ```
pub struct ExternalCmdExecutor {
    cmd: String,
    timeout_seconds: u64,
}

impl ExternalCmdExecutor {
    pub fn new(cmd: impl Into<String>, timeout_seconds: u64) -> Self {
        Self {
            cmd: cmd.into(),
            timeout_seconds,
        }
    }
}

#[async_trait::async_trait]
impl CaseExecutor for ExternalCmdExecutor {
    async fn execute(&self, case: &Case, model: &str) -> RunOutcome {
        use tokio::process::Command;

        let input = serde_json::json!({
            "protocol_version": "1.1",
            "case": {
                "name": case.name,
                "description": case.description,
                "prompt": case.prompt,
                "capability": case.capability,
                "difficulty": case.difficulty,
                "weight": case.weight,
                "setup_cmd": case.setup_cmd,
                "teardown_cmd": case.teardown_cmd,
                "timeout_seconds": case.timeout_seconds,
                "extra_cli_args": case.extra_cli_args,
            },
            "model": model,
            "run_index": 0,
        });

        if self.cmd.trim().is_empty() {
            return RunOutcome {
                model: model.into(),
                exit_code: -1,
                text: "executor-cmd is empty".into(),
                ..Default::default()
            };
        }

        let start = std::time::Instant::now();
        let child = Command::new("sh")
            .arg("-c")
            .arg(&self.cmd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn();

        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                return RunOutcome {
                    model: model.into(),
                    exit_code: -1,
                    text: format!("spawn executor-cmd {}: {e}", self.cmd),
                    duration_ms: start.elapsed().as_millis() as u64,
                    ..Default::default()
                };
            }
        };

        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            let payload = serde_json::to_vec(&input).unwrap();
            let _ = stdin.write_all(&payload).await;
            drop(stdin);
        }

        let timeout = std::time::Duration::from_secs(self.timeout_seconds);
        let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                return RunOutcome {
                    model: model.into(),
                    exit_code: -1,
                    text: format!("executor-cmd wait: {e}"),
                    duration_ms: start.elapsed().as_millis() as u64,
                    ..Default::default()
                };
            }
            Err(_) => {
                return RunOutcome {
                    model: model.into(),
                    exit_code: 124,
                    text: format!("executor-cmd timed out after {}s", self.timeout_seconds),
                    duration_ms: start.elapsed().as_millis() as u64,
                    ..Default::default()
                };
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let mut out = parse_json_outcome(&stdout, model);
        out.stderr = stderr;
        out.exit_code = output.status.code().unwrap_or(-1);
        out.duration_ms = start.elapsed().as_millis() as u64;
        out
    }

    fn reproducer(&self, case: &Case, model: &str) -> String {
        format!(
            "echo '{{\"name\":\"{}\",\"prompt\":\"...\",\"model\":\"{}\"}}' | {}",
            case.name, model, self.cmd
        )
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
                    cached_input_tokens: 0,
                    cache_creation_tokens: 0,
                    duration_ms: 0,
                    turn_rounds: 0,
                    cache_hits: 0,
                    total_tool_calls: 0,
                    ttft_ms: 0,
                    final_state: None,
                    interruption_kind: None,
                    tool_result_class_counts: std::collections::BTreeMap::new(),
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
            extra_cli_args: vec!["--verbose".into()],
            timeout_seconds: 60,
            capability: None,
            required_cache_scope: None,
            difficulty: None,
            weight: 1.0,
            steps: vec![],
            env: std::collections::HashMap::new(),
            setup_cmd: None,
            teardown_cmd: None,
            cleanup_memory_records: false,
        };
        let repro = exec.reproducer(&case, "qwen-flash");
        assert!(repro.contains("/usr/local/bin/astra"));
        assert!(
            !repro.contains("--session-id"),
            "the first turn must let the server create the session: {repro}"
        );
        assert!(repro.contains("--no-resume"));
        assert!(repro.contains("--model"));
        assert!(repro.contains("qwen-flash"));
        assert!(repro.contains("--verbose"));
        // POSIX single-quote escape: `'say '\''hello'\'''` preserves
        // the original bytes without relying on double-quote semantics
        // (which would still expand $ and backticks). A prompt with
        // apostrophes round-trips exactly.
        assert!(
            repro.contains(r"'say '\''hello'\'''"),
            "POSIX single-quote escape expected: {repro}"
        );
    }

    #[tokio::test]
    async fn root_turn_does_not_invent_a_session_id_but_follow_up_preserves_one() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        use crate::test_support::write_executable_shim;
        let tmp = tempfile::tempdir().expect("tempdir");
        let args_path = tmp.path().join("args");
        let shim = tmp.path().join("fake-astra");
        write_executable_shim(
            &shim,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$HARNESS_ARGS_PATH\"\nprintf '%s\\n' '{\"session_id\":\"server-issued\"}'\n",
        )
        .expect("write shim");

        let mut case = simple_case();
        case.env.insert(
            "HARNESS_ARGS_PATH".into(),
            args_path.to_string_lossy().into_owned(),
        );
        let exec = AstraCliExecutor::new(RunnerConfig::new(shim));
        let root = exec.execute(&case, "m").await;
        assert_eq!(root.session_id.as_deref(), Some("server-issued"));
        let root_args = std::fs::read_to_string(&args_path).expect("root args");
        assert!(
            !root_args.lines().any(|arg| arg == "--session-id"),
            "root turn must not fabricate a resumable id: {root_args:?}"
        );
        assert!(root_args.lines().any(|arg| arg == "--no-resume"));

        case.extra_cli_args = vec!["--session-id".into(), "server-issued".into()];
        let follow_up = exec.execute(&case, "m").await;
        assert_eq!(follow_up.session_id.as_deref(), Some("server-issued"));
        let follow_up_args = std::fs::read_to_string(&args_path).expect("follow-up args");
        assert!(
            follow_up_args
                .lines()
                .collect::<Vec<_>>()
                .windows(2)
                .any(|pair| { pair == ["--session-id", "server-issued"] }),
            "follow-up must preserve the server-issued id: {follow_up_args:?}"
        );
        assert!(
            !follow_up_args.lines().any(|arg| arg == "--no-resume"),
            "follow-up must use the explicit server session, not disable resume: {follow_up_args:?}"
        );
    }

    #[test]
    fn shell_escape_mixed_quotes_and_metachars() {
        // Prompt with single-quote, double-quote, dollar, backtick,
        // newline. All five previously risked being either unescaped
        // or getting expanded inside the old double-quote fallback.
        let input = "mix 'a' \"b\" $(echo c) `d`\ne".to_string();
        let got = shell_escape(input);
        // Must open + close with a single quote (the posix wrapping
        // idiom) so the shell reads everything else as literal.
        assert!(got.starts_with('\''), "must start with single quote: {got}");
        assert!(got.ends_with('\''), "must end with single quote: {got}");
        // The two inner apostrophes are each turned into `'\''`.
        assert!(
            got.contains(r"'\''"),
            "inner quotes must be POSIX-escaped: {got}"
        );
        // `$` and backticks must be present LITERALLY (no expansion)
        // because single-quoted strings don't expand.
        assert!(got.contains("$(echo c)"));
        assert!(got.contains("`d`"));
        assert!(got.contains('\n'));
    }

    #[test]
    fn shell_escape_empty_string_is_valid_empty_quoted_literal() {
        // Otherwise a `''` argument collapses into "nothing" on a
        // shell line.
        assert_eq!(shell_escape(String::new()), "''");
    }

    #[test]
    fn explicit_session_id_ignores_invalid_values_without_panicking() {
        let valid = vec![
            "--session-id".to_string(),
            "00000000-0000-0000-0000-000000000001".to_string(),
        ];
        assert_eq!(
            explicit_session_id(&valid),
            Some("00000000-0000-0000-0000-000000000001")
        );
        let invalid = vec!["--session-id=../not-a-session".to_string()];
        assert_eq!(explicit_session_id(&invalid), None);
    }

    #[test]
    fn shell_escape_simple_string_does_not_add_backslashes() {
        assert_eq!(shell_escape("simple".to_string()), "'simple'");
    }

    // Regression: case timeout MUST kill the child process AND
    // surface the synthetic exit=124 / "timeout" outcome through
    // `AstraCliExecutor::execute`. The timeout branch has its own
    // synthetic-outcome construction that was previously untested;
    // this test routes a shim `/bin/sh -c "sleep 10"` script through
    // the real executor path.
    //
    // `#[serial]`: this test is timing-sensitive. The 1-second case
    // timeout races against `tokio::time::timeout` precision and
    // process-spawn latency. Under heavy parallel test load
    // (`cargo test --workspace` spawns 100+ test binaries) the
    // child-process spawn was failing with EAGAIN/ENOMEM, producing
    // exit_code=-1 instead of 124 and tripping the assertion. Running
    // serial removes the contention on a path the test isn't actually
    // exercising. (The other path-1/path-2 spawn-error handling stays
    // covered by `external_executor_spawn_failure_returns_-1` etc.)
    #[tokio::test]
    #[serial_test::serial]
    async fn timeout_kills_subprocess_and_returns_posix_124() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        // Write a shim binary that ignores the `astra chat` arg
        // vector and just sleeps. This lets `AstraCliExecutor`
        // spawn it via its real `chat -m ... --model ... --json -y`
        // arg assembly without the child exiting early.
        use crate::test_support::write_executable_shim;
        let tmp = tempfile::tempdir().expect("tempdir");
        let shim = tmp.path().join("fake-astra");
        write_executable_shim(&shim, "#!/bin/sh\nsleep 10\n").expect("write shim");

        let cfg = RunnerConfig::new(shim.clone());
        let exec = AstraCliExecutor::new(cfg);
        let case = Case {
            name: "timeout_probe".into(),
            description: None,
            prompt: "ignored by the shim — just needs to be non-empty".into(),
            models: Some(vec!["ignored".into()]),
            criteria: vec![],
            debug_log: false,
            extra_cli_args: vec![],
            timeout_seconds: 1,
            capability: None,
            required_cache_scope: None,
            difficulty: None,
            weight: 1.0,
            steps: vec![],
            env: std::collections::HashMap::new(),
            setup_cmd: None,
            teardown_cmd: None,
            cleanup_memory_records: false,
        };
        let start = std::time::Instant::now();
        let outcome = exec.execute(&case, "ignored").await;
        let elapsed = start.elapsed();

        // `kill_on_drop` + explicit timeout capped the elapsed wall
        // time near the 1s budget. 3s slack for CI noise.
        assert!(
            elapsed.as_secs() <= 3,
            "timeout didn't kill subprocess — elapsed {}s",
            elapsed.as_secs()
        );
        // Synthetic outcome: POSIX 124 + explanatory text. This is
        // the contract downstream report rendering + reproducer
        // hinting rely on.
        assert_eq!(
            outcome.exit_code, 124,
            "timeout branch must surface POSIX 124 exit"
        );
        assert!(
            outcome.text.contains("timeout"),
            "timeout text must surface for the report: {}",
            outcome.text
        );
        assert!(
            outcome.duration_ms > 0,
            "duration_ms should be populated on the synthetic outcome"
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
            cached_input_tokens: 0,
            cache_creation_tokens: 0,
            duration_ms: 0,
            turn_rounds: 0,
            cache_hits: 0,
            total_tool_calls: 0,
            ttft_ms: 0,
            final_state: None,
            interruption_kind: None,
            tool_result_class_counts: std::collections::BTreeMap::new(),
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
            capability: None,
            required_cache_scope: None,
            difficulty: None,
            weight: 1.0,
            steps: vec![],
            env: std::collections::HashMap::new(),
            setup_cmd: None,
            teardown_cmd: None,
            cleanup_memory_records: false,
        };
        let out = fe.execute(&case, "qwen-flash").await;
        assert_eq!(out.text, "hello");
        assert_eq!(fe.calls.lock().unwrap_or_else(|e| e.into_inner()).len(), 1);

        // Unknown model → synthetic -1 outcome.
        let out2 = fe.execute(&case, "never-seeded").await;
        assert_eq!(out2.exit_code, -1);
        assert!(out2.text.contains("fake"));
    }

    // ── ExternalCmdExecutor tests ──

    fn simple_case() -> Case {
        Case {
            name: "ext".into(),
            description: None,
            prompt: "test prompt".into(),
            models: Some(vec!["m".into()]),
            criteria: vec![],
            debug_log: false,
            extra_cli_args: vec![],
            timeout_seconds: 60,
            capability: None,
            required_cache_scope: None,
            difficulty: None,
            weight: 1.0,
            steps: vec![],
            env: std::collections::HashMap::new(),
            setup_cmd: None,
            teardown_cmd: None,
            cleanup_memory_records: false,
        }
    }

    #[tokio::test]
    async fn external_executor_happy_path() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        let script = r#"cat <<'REPLY'
{"text":"external-hello","exit_code":0,"session_id":"s1","tool_calls_count":2,"tools_used":["Read","Write"],"completion_tokens":10,"prompt_tokens":20}
REPLY"#;
        let exec = ExternalCmdExecutor::new(script, 10);
        let out = exec.execute(&simple_case(), "m").await;
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.text, "external-hello");
        assert_eq!(out.tools_used, vec!["Read", "Write"]);
        assert!(out.duration_ms < 5000);
    }

    #[tokio::test]
    async fn external_executor_receives_protocol_version_and_case_metadata() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        // Read stdin, verify it contains expected fields, return a
        // signal via the text field.
        let script = r#"
INPUT=$(cat)
OK="yes"
echo "$INPUT" | grep -q '"protocol_version":"1.1"' || OK="no_protocol"
echo "$INPUT" | grep -q '"model":"test-model"' || OK="no_model"
echo "$INPUT" | grep -q '"case"' || OK="no_case"
echo "{\"text\":\"$OK\"}"
"#;
        let exec = ExternalCmdExecutor::new(script, 10);
        let out = exec.execute(&simple_case(), "test-model").await;
        assert_eq!(
            out.text, "yes",
            "external executor must receive protocol_version, model, and case: got {:?}",
            out.text
        );
    }

    #[tokio::test]
    async fn external_executor_empty_cmd_returns_error() {
        let exec = ExternalCmdExecutor::new("  ", 10);
        let out = exec.execute(&simple_case(), "m").await;
        assert_eq!(out.exit_code, -1);
        assert!(out.text.contains("empty"));
    }

    #[tokio::test]
    async fn external_executor_timeout_returns_124() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        let exec = ExternalCmdExecutor::new("sleep 30", 1);
        let start = std::time::Instant::now();
        let out = exec.execute(&simple_case(), "m").await;
        assert_eq!(out.exit_code, 124);
        assert!(out.text.contains("timed out"));
        assert!(start.elapsed().as_secs() <= 3);
    }

    #[tokio::test]
    async fn external_executor_nonzero_exit() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        let exec = ExternalCmdExecutor::new("echo '{}'; exit 42", 10);
        let out = exec.execute(&simple_case(), "m").await;
        assert_eq!(out.exit_code, 42);
    }
}

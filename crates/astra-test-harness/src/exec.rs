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

use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;

use crate::case::Case;
use crate::runner::{
    PROTOCOL_ERROR_MARKER, RunOutcome, RunnerConfig, parse_json_outcome, parse_strict_cli_outcome,
    reconcile_process_exit,
};
use crate::session_identity::{
    cancel_server_session, run_id_from_stream_event, session_id_from_stream_event,
};

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

fn parse_executor_outcome(stdout: &str, model: &str, process_exit: i32) -> RunOutcome {
    let outcome = if stdout.trim().is_empty() {
        // Preserve a real non-zero empty-stdout status for auth/inactive
        // classification; empty success is converted to protocol failure by
        // reconcile_process_exit.
        parse_json_outcome(stdout, model)
    } else {
        match parse_strict_cli_outcome(stdout, model) {
            Ok(outcome) => outcome,
            Err(error) => {
                let mut invalid = parse_json_outcome(stdout, model);
                if !invalid.stderr.starts_with(PROTOCOL_ERROR_MARKER) {
                    invalid.stderr = PROTOCOL_ERROR_MARKER.into();
                    invalid.text = format!("invalid terminal outcome: {error}");
                    invalid.exit_code = -1;
                }
                invalid
            }
        }
    };
    reconcile_process_exit(outcome, stdout, process_exit)
}

/// Reconcile the terminal envelope's session identity with the identity
/// observed in the dedicated machine-event file.  Both are producer-owned
/// handoffs; accepting two different valid UUIDs would let the harness load a
/// different session's journal and certify the wrong durable evidence.
///
/// Returns an observed identity that may be safely cancelled after a protocol
/// failure.  No identity is retained on the outcome in that case, so session
/// criteria cannot inspect untrusted evidence.
fn reconcile_observed_session(
    outcome: &mut RunOutcome,
    observed_session_id: Option<String>,
) -> Option<String> {
    if outcome.stderr.starts_with(PROTOCOL_ERROR_MARKER) {
        outcome.session_id = None;
        return observed_session_id;
    }
    match (
        outcome.session_id.as_deref(),
        observed_session_id.as_deref(),
    ) {
        (Some(terminal), Some(observed)) if terminal != observed => {
            outcome.exit_code = -1;
            outcome.text = format!(
                "invalid terminal outcome: session_id {terminal} disagrees with observed session_id {observed}"
            );
            outcome.stderr = PROTOCOL_ERROR_MARKER.into();
            outcome.session_id = None;
            observed_session_id
        }
        (Some(_), _) => None,
        (None, Some(observed)) => {
            outcome.session_id = Some(observed.to_string());
            None
        }
        (None, None) => None,
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
            "--stream-events".into(),
            "\"$(mktemp -d)/events.jsonl\"".into(),
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

fn load_step_event_stats(
    cfg: &RunnerConfig,
    session_id: &str,
) -> Option<crate::session_capture::StepEventStats> {
    if cfg.artifact_owner_scopes.is_empty() {
        crate::session_capture::load_step_event_stats(session_id)
    } else {
        crate::session_capture::load_step_event_stats_for_owners(
            session_id,
            &cfg.artifact_owner_scopes,
        )
    }
}

fn merge_step_event_stats(out: &mut RunOutcome, stats: crate::session_capture::StepEventStats) {
    // A typed CLI terminal summary spans the complete execution owner. The
    // step-event loader prefers current LlmRoundStarted records and falls
    // back to legacy StepStarted records, so a Server-owned multi-round loop
    // is reported as its actual provider-round count.
    if out.turn_rounds == 0 {
        out.turn_rounds = stats.turn_rounds;
    }
    out.cache_hits = stats.cache_hits;
    out.total_tool_calls = stats.total_tool_calls;
}

// Keep human diagnostics bounded independently of turn duration while
// continuing to drain stderr so the tested CLI cannot block on a full pipe.
const MAX_CAPTURED_STDERR_BYTES: usize = 256 * 1024;
const MAX_STREAM_EVENT_LINE_BYTES: usize = 64 * 1024;
const STDERR_TRUNCATION_NOTICE: &[u8] =
    b"\n[astra-test] stderr capture truncated; further live events omitted\n";

struct BoundedStderrCapture {
    bytes: Vec<u8>,
    truncated: bool,
}

impl BoundedStderrCapture {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            truncated: false,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        let retained = MAX_CAPTURED_STDERR_BYTES.saturating_sub(STDERR_TRUNCATION_NOTICE.len());
        if self.bytes.len() < retained {
            let take = (retained - self.bytes.len()).min(chunk.len());
            self.bytes.extend_from_slice(&chunk[..take]);
            self.truncated |= take != chunk.len();
        } else {
            self.truncated |= !chunk.is_empty();
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.truncated {
            self.bytes.extend_from_slice(STDERR_TRUNCATION_NOTICE);
        }
        self.bytes
    }
}

async fn collect_stderr(stderr: tokio::process::ChildStderr) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;

    let mut stderr = stderr;
    let mut capture = BoundedStderrCapture::new();
    let mut chunk = [0_u8; 8 * 1024];
    loop {
        let read = stderr.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        capture.push(&chunk[..read]);
    }
    Ok(capture.finish())
}

#[derive(Default)]
struct MachineEventObservation {
    session_id: Option<String>,
    run_id: Option<String>,
    event_count: u64,
    invalid: Option<String>,
}

impl MachineEventObservation {
    fn observe_line(&mut self, line: &str) {
        if self.invalid.is_some() {
            return;
        }
        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(value @ serde_json::Value::Object(_)) => value,
            _ => {
                self.invalid = Some("machine event file contains a non-JSON-object line".into());
                return;
            }
        };
        let Some(event_type) = value.get("type").and_then(serde_json::Value::as_str) else {
            self.invalid = Some("machine event JSON object lacks a string type".into());
            return;
        };
        self.event_count = self.event_count.saturating_add(1);
        let observed = match event_type {
            "session_bound" => session_id_from_stream_event(line).map(|id| (true, id)),
            "run_bound" => run_id_from_stream_event(line).map(|id| (false, id)),
            _ => return,
        };
        let Some((is_session, id)) = observed else {
            self.invalid = Some(format!(
                "machine {event_type} event has an invalid identity"
            ));
            return;
        };
        let slot = if is_session {
            &mut self.session_id
        } else {
            &mut self.run_id
        };
        match slot.as_deref() {
            None => *slot = Some(id),
            Some(existing) if existing == id => {}
            Some(_) => {
                self.invalid = Some(format!(
                    "machine event file contains conflicting {event_type} identities"
                ));
            }
        }
    }
}

async fn observe_machine_event_file(
    path: std::path::PathBuf,
    observation: Arc<Mutex<MachineEventObservation>>,
    done: tokio_util::sync::CancellationToken,
) -> std::io::Result<()> {
    use tokio::io::AsyncReadExt;

    let mut file = loop {
        match tokio::fs::File::open(&path).await {
            Ok(file) => break file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tokio::select! {
                    _ = done.cancelled() => {
                        // The child may create and close a short-run event
                        // file while this observer is parked in its NotFound
                        // backoff. Reconcile the path once after process exit
                        // before declaring evidence absent.
                        match tokio::fs::File::open(&path).await {
                            Ok(file) => break file,
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                                if let Ok(mut observation) = observation.lock() {
                                    observation.invalid =
                                        Some("machine event file was not created".into());
                                }
                                return Ok(());
                            }
                            Err(error) => return Err(error),
                        }
                    },
                    _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
                }
            }
            Err(error) => return Err(error),
        }
    };
    let mut partial = Vec::new();
    let mut chunk = [0_u8; 8 * 1024];
    let mut stopping = false;
    loop {
        let read = file.read(&mut chunk).await?;
        if read == 0 {
            if stopping {
                break;
            }
            tokio::select! {
                _ = done.cancelled() => stopping = true,
                _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
            }
            continue;
        }
        for &byte in &chunk[..read] {
            if byte == b'\n' {
                let line = std::str::from_utf8(&partial).ok();
                if let Ok(mut observation) = observation.lock() {
                    match line {
                        Some(line) if !line.is_empty() => observation.observe_line(line),
                        _ => {
                            observation.invalid = Some(
                                "machine event file contains an invalid empty/UTF-8 line".into(),
                            )
                        }
                    }
                }
                partial.clear();
            } else if partial.len() < MAX_STREAM_EVENT_LINE_BYTES {
                partial.push(byte);
            } else if let Ok(mut observation) = observation.lock() {
                observation.invalid = Some("machine event line exceeds the size bound".into());
            }
        }
    }
    if !partial.is_empty()
        && let Ok(mut observation) = observation.lock()
    {
        observation.invalid = Some("machine event file ends with a partial line".into());
    }
    if let Ok(mut observation) = observation.lock()
        && observation.invalid.is_none()
        && observation.event_count == 0
    {
        observation.invalid = Some("machine event file is empty".into());
    }
    Ok(())
}

#[cfg(unix)]
async fn kill_process_group_and_reap(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id()
        && pid <= i32::MAX as u32
    {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
}

#[cfg(not(unix))]
async fn kill_process_group_and_reap(child: &mut tokio::process::Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}

async fn join_output_reader(
    reader: tokio::task::JoinHandle<std::io::Result<Vec<u8>>>,
) -> Result<Vec<u8>, String> {
    reader
        .await
        .map_err(|error| format!("stdout reader task failed: {error}"))?
        .map_err(|error| format!("stdout reader failed: {error}"))
}

async fn join_stderr_reader(
    reader: tokio::task::JoinHandle<std::io::Result<Vec<u8>>>,
) -> Result<Vec<u8>, String> {
    reader
        .await
        .map_err(|error| format!("stderr reader task failed: {error}"))?
        .map_err(|error| format!("stderr reader failed: {error}"))
}

async fn finish_machine_event_observer(
    done: tokio_util::sync::CancellationToken,
    reader: tokio::task::JoinHandle<std::io::Result<()>>,
    observation: &Arc<Mutex<MachineEventObservation>>,
) -> Result<(Option<String>, Option<String>), String> {
    done.cancel();
    reader
        .await
        .map_err(|error| format!("machine event reader task failed: {error}"))?
        .map_err(|error| format!("machine event reader failed: {error}"))?;
    let observation = observation
        .lock()
        .map_err(|_| "machine event observation lock was poisoned".to_string())?;
    if let Some(error) = observation.invalid.as_ref() {
        return Err(error.clone());
    }
    Ok((observation.session_id.clone(), observation.run_id.clone()))
}

async fn run_case_subprocess(cfg: &RunnerConfig, case: &Case, model: &str) -> RunOutcome {
    use std::process::Stdio;
    use std::time::Duration;
    use tokio::process::Command;

    // Step-event files are append-only for a session. A continuation command
    // must report only the events it added, otherwise SuiteRunner sums the
    // entire prior transcript once per follow-up turn.
    let prior_step_stats = explicit_session_id(&case.extra_cli_args)
        .and_then(|session_id| load_step_event_stats(cfg, session_id));
    let start = Instant::now();
    let stream_event_dir = match tempfile::Builder::new()
        .prefix("astra-machine-events-")
        .tempdir()
    {
        Ok(dir) => dir,
        Err(error) => {
            return RunOutcome {
                model: model.into(),
                exit_code: -1,
                text: format!("failed to allocate machine-event directory: {error}"),
                duration_ms: start.elapsed().as_millis() as u64,
                ..Default::default()
            };
        }
    };
    let stream_event_path = stream_event_dir.path().join("events.jsonl");
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
    cmd.arg("--model")
        .arg(model)
        .arg("--json")
        // Exact lifecycle evidence used only for safe timeout convergence.
        .arg("--stream-events")
        .arg(&stream_event_path)
        .arg("-y");
    if let Some(ref wd) = cfg.working_dir {
        cmd.current_dir(wd);
    }
    for extra in &case.extra_cli_args {
        cmd.arg(extra);
    }
    for (k, v) in &case.cli_env {
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
    // Test prompts can legitimately spawn shell commands and child agents. A
    // timeout must kill their process group too, otherwise a grandchild can
    // keep stdout/stderr open after the CLI parent has exited.
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }

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

    let stdout = child.stdout.take().expect("piped stdout is present");
    let stderr = child.stderr.take().expect("piped stderr is present");
    let machine_observation = Arc::new(Mutex::new(MachineEventObservation::default()));
    let machine_observer_done = tokio_util::sync::CancellationToken::new();
    let machine_event_reader = tokio::spawn(observe_machine_event_file(
        stream_event_path,
        Arc::clone(&machine_observation),
        machine_observer_done.clone(),
    ));
    let stdout_reader = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;

        let mut stdout = stdout;
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).await?;
        Ok::<_, std::io::Error>(bytes)
    });
    let stderr_reader = tokio::spawn(collect_stderr(stderr));

    let timeout = Duration::from_secs(case.timeout_seconds);
    // Drain both pipes and observe the dedicated machine-event file
    // concurrently. The file carries the server-issued binding before final
    // JSON exists, so a timeout can cancel the exact run without treating
    // human stderr diagnostics as protocol data.
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => {
            let stdout = join_output_reader(stdout_reader).await;
            let stderr = join_stderr_reader(stderr_reader).await;
            let machine = finish_machine_event_observer(
                machine_observer_done,
                machine_event_reader,
                &machine_observation,
            )
            .await;
            let (stdout, stderr, (observed_session_id, _observed_run_id)) = match (
                stdout, stderr, machine,
            ) {
                (Ok(stdout), Ok(stderr), Ok(machine)) => (stdout, stderr, machine),
                (stdout_error, stderr_error, machine_error) => {
                    return RunOutcome {
                        model: model.into(),
                        exit_code: -1,
                        text: format!(
                            "subprocess evidence collection failed: stdout={stdout_error:?}; stderr={stderr_error:?}; machine_events={machine_error:?}"
                        ),
                        duration_ms: start.elapsed().as_millis() as u64,
                        ..Default::default()
                    };
                }
            };
            let stdout = String::from_utf8_lossy(&stdout).into_owned();
            let stderr = String::from_utf8_lossy(&stderr).into_owned();
            let process_exit = status.code().unwrap_or(-1);
            let mut out = parse_executor_outcome(&stdout, model, process_exit);
            if out.stderr.starts_with(PROTOCOL_ERROR_MARKER) && !stderr.is_empty() {
                out.stderr = format!("{}\n{stderr}", out.stderr);
            } else if !out.stderr.starts_with(PROTOCOL_ERROR_MARKER) {
                out.stderr = stderr;
            }
            out.duration_ms = start.elapsed().as_millis() as u64;
            let cleanup_identity = reconcile_observed_session(&mut out, observed_session_id);
            if let Some(identity) = cleanup_identity {
                let cleanup = cleanup_observed_session(cfg, Some(&identity)).await;
                if !cleanup.is_empty() {
                    if !out.stderr.is_empty() {
                        out.stderr.push('\n');
                    }
                    out.stderr.push_str(cleanup.trim_start_matches('\n'));
                }
            }
            // Extract turn_rounds and cache_hits from step_events if available.
            if let Some(ref sid) = out.session_id
                && let Some(stats) = load_step_event_stats(cfg, sid)
            {
                let stats = match prior_step_stats.as_ref() {
                    Some(prior) => stats.since(prior),
                    None => stats,
                };
                merge_step_event_stats(&mut out, stats);
            }
            out
        }
        Ok(Err(error)) => {
            kill_process_group_and_reap(&mut child).await;
            let _stdout = join_output_reader(stdout_reader).await.unwrap_or_default();
            let stderr = join_stderr_reader(stderr_reader).await.unwrap_or_default();
            let machine = finish_machine_event_observer(
                machine_observer_done,
                machine_event_reader,
                &machine_observation,
            )
            .await;
            let (session_id, _) = machine.clone().unwrap_or_default();
            let cleanup = cleanup_observed_session(cfg, session_id.as_deref()).await;
            RunOutcome {
                model: model.into(),
                exit_code: -1,
                text: format!(
                    "wait error: {error}{cleanup}{}",
                    machine
                        .err()
                        .map(|error| format!("; invalid machine events: {error}"))
                        .unwrap_or_default()
                ),
                stderr: String::from_utf8_lossy(&stderr).into_owned(),
                session_id,
                duration_ms: start.elapsed().as_millis() as u64,
                ..Default::default()
            }
        }
        Err(_timed_out) => {
            kill_process_group_and_reap(&mut child).await;
            let _stdout = join_output_reader(stdout_reader).await.unwrap_or_default();
            let stderr = join_stderr_reader(stderr_reader).await.unwrap_or_default();
            let machine = finish_machine_event_observer(
                machine_observer_done,
                machine_event_reader,
                &machine_observation,
            )
            .await;
            let (session_id, observed_run_id) = machine.clone().unwrap_or_default();
            let cleanup = cleanup_observed_session(cfg, session_id.as_deref()).await;
            let stderr = String::from_utf8_lossy(&stderr).into_owned();
            let mut out = RunOutcome {
                model: model.into(),
                // POSIX `timeout` utility exits 124 — follow the convention.
                // Cancellation only converges resources; it never turns the
                // timed-out product run into a passing harness result.
                exit_code: 124,
                text: format!(
                    "timeout after {}s (case timeout_seconds={}){cleanup}{}",
                    timeout.as_secs(),
                    case.timeout_seconds,
                    machine
                        .err()
                        .map(|error| format!("; invalid machine events: {error}"))
                        .unwrap_or_default()
                ),
                stderr: stderr.clone(),
                session_id,
                // There is no terminal JSON envelope on an outer timeout.
                // Preserve the typed run-bound identity emitted before model
                // work so invocation scoping keeps this run's durable events.
                run_id: observed_run_id,
                final_state: Some("interrupted".into()),
                interruption_kind: Some("timeout".into()),
                duration_ms: start.elapsed().as_millis() as u64,
                ..Default::default()
            };
            // A timeout does not erase typed progress that was durably
            // emitted before cancellation. Merge the same owner-scoped
            // step-event counters used by the normal exit path so the
            // classifier can distinguish "provider never started" from a
            // model/runtime loop that consumed the case budget. This is
            // intentionally evidence-only; it cannot make a timed-out case
            // pass any hard criterion.
            if let Some(ref sid) = out.session_id
                && let Some(stats) = load_step_event_stats(cfg, sid)
            {
                let stats = match prior_step_stats.as_ref() {
                    Some(prior) => stats.since(prior),
                    None => stats,
                };
                merge_step_event_stats(&mut out, stats);
            }
            out
        }
    }
}

async fn cleanup_observed_session(cfg: &RunnerConfig, session_id: Option<&str>) -> String {
    match session_id {
        Some(session_id) => {
            match cancel_server_session(&cfg.astra_bin, cfg.profile.as_deref(), session_id).await {
                Ok(()) => "\n[astra-test] observed session cancelled".to_string(),
                Err(error) => format!("\n[astra-test] session cleanup failed: {error}"),
            }
        }
        None => "\n[astra-test] no server-issued session identity observed before interruption"
            .to_string(),
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
        let process_exit = output.status.code().unwrap_or(-1);
        let mut out = parse_executor_outcome(&stdout, model, process_exit);
        if out.stderr.starts_with(PROTOCOL_ERROR_MARKER) && !stderr.is_empty() {
            out.stderr = format!("{}\n{stderr}", out.stderr);
        } else if !out.stderr.starts_with(PROTOCOL_ERROR_MARKER) {
            out.stderr = stderr;
        }
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
    fn stderr_capture_stays_bounded_without_treating_diagnostics_as_machine_events() {
        let mut capture = BoundedStderrCapture::new();
        capture.push(&vec![b'x'; MAX_STREAM_EVENT_LINE_BYTES + 1]);
        capture.push(
            b"\n{\"type\":\"session_bound\",\"session_id\":\"550e8400-e29b-41d4-a716-446655440000\"}\n",
        );
        capture.push(&vec![b'y'; MAX_CAPTURED_STDERR_BYTES]);

        let stderr = capture.finish();
        assert!(stderr.len() <= MAX_CAPTURED_STDERR_BYTES);
        assert!(
            String::from_utf8_lossy(&stderr).contains("stderr capture truncated"),
            "bounded capture must disclose loss of diagnostic output"
        );
    }

    #[test]
    fn machine_event_observation_is_strict_and_identity_bound() {
        let mut observation = MachineEventObservation::default();
        observation.observe_line(
            r#"{"type":"session_bound","session_id":"550e8400-e29b-41d4-a716-446655440000"}"#,
        );
        observation.observe_line(
            r#"{"type":"run_bound","run_id":"8a0dcb50-38a7-4402-bef3-2c1aee9a4e85"}"#,
        );
        assert!(observation.invalid.is_none());
        assert_eq!(
            observation.session_id.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );

        observation.observe_line("permissive mode warning: command allowed");
        assert!(
            observation
                .invalid
                .as_deref()
                .is_some_and(|error| error.contains("non-JSON-object")),
            "diagnostic contamination must invalidate machine evidence"
        );
    }

    #[tokio::test]
    async fn machine_event_file_observer_reads_dedicated_jsonl_before_shutdown() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.jsonl");
        let observation = Arc::new(Mutex::new(MachineEventObservation::default()));
        let done = tokio_util::sync::CancellationToken::new();
        let reader = tokio::spawn(observe_machine_event_file(
            path.clone(),
            Arc::clone(&observation),
            done.clone(),
        ));
        tokio::fs::write(
            path,
            concat!(
                "{\"type\":\"session_bound\",\"session_id\":\"550e8400-e29b-41d4-a716-446655440000\"}\n",
                "{\"type\":\"run_bound\",\"run_id\":\"8a0dcb50-38a7-4402-bef3-2c1aee9a4e85\"}\n",
            ),
        )
        .await
        .unwrap();

        let (session_id, run_id) = finish_machine_event_observer(done, reader, &observation)
            .await
            .unwrap();

        assert_eq!(
            session_id.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
        assert_eq!(
            run_id.as_deref(),
            Some("8a0dcb50-38a7-4402-bef3-2c1aee9a4e85")
        );
    }

    #[test]
    fn session_identity_mismatch_is_protocol_failure_and_never_evidence() {
        let mut outcome = RunOutcome {
            exit_code: 0,
            session_id: Some("550e8400-e29b-41d4-a716-446655440000".into()),
            ..Default::default()
        };
        let cleanup = reconcile_observed_session(
            &mut outcome,
            Some("550e8400-e29b-41d4-a716-446655440001".into()),
        );
        assert_eq!(outcome.exit_code, -1);
        assert!(outcome.session_id.is_none());
        assert!(outcome.stderr.starts_with(PROTOCOL_ERROR_MARKER));
        assert_eq!(
            cleanup.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440001")
        );
    }

    #[test]
    fn invalid_terminal_protocol_does_not_reuse_untrusted_session_id() {
        let mut outcome = RunOutcome {
            exit_code: -1,
            session_id: Some("550e8400-e29b-41d4-a716-446655440002".into()),
            stderr: PROTOCOL_ERROR_MARKER.into(),
            ..Default::default()
        };
        let cleanup = reconcile_observed_session(
            &mut outcome,
            Some("550e8400-e29b-41d4-a716-446655440003".into()),
        );
        assert!(outcome.session_id.is_none());
        assert_eq!(
            cleanup.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440003")
        );
    }

    #[test]
    fn typed_terminal_rounds_are_not_flattened_by_local_step_events() {
        let mut outcome = RunOutcome {
            turn_rounds: 3,
            ..Default::default()
        };
        merge_step_event_stats(
            &mut outcome,
            crate::session_capture::StepEventStats {
                turn_rounds: 1,
                cache_hits: 2,
                total_tool_calls: 4,
            },
        );

        assert_eq!(outcome.turn_rounds, 3);
        assert_eq!(outcome.cache_hits, 2);
        assert_eq!(outcome.total_tool_calls, 4);
    }

    #[test]
    fn local_step_rounds_fill_an_absent_terminal_summary() {
        let mut outcome = RunOutcome::default();
        merge_step_event_stats(
            &mut outcome,
            crate::session_capture::StepEventStats {
                turn_rounds: 2,
                ..Default::default()
            },
        );

        assert_eq!(outcome.turn_rounds, 2);
    }

    #[test]
    fn reproducer_roundtrips_prompt_with_quotes() {
        let cfg = RunnerConfig::new(PathBuf::from("/usr/local/bin/astra"));
        let exec = AstraCliExecutor::new(cfg);
        let case = Case {
            name: "c".into(),
            description: None,
            prompt: "say 'hello'".into(),
            prompt_variants: vec![],
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
            cli_env: std::collections::HashMap::new(),
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
            concat!(
                "#!/bin/sh\n",
                "printf '%s\\n' \"$@\" > \"$HARNESS_ARGS_PATH\"\n",
                "events=; next_is_events=0\n",
                "for arg in \"$@\"; do\n",
                "  if [ \"$next_is_events\" = 1 ]; then events=$arg; next_is_events=0;\n",
                "  elif [ \"$arg\" = --stream-events ]; then next_is_events=1; fi\n",
                "done\n",
                "printf '%s\\n' '{\"type\":\"session_bound\",\"session_id\":\"550e8400-e29b-41d4-a716-446655440000\"}' > \"$events\"\n",
                "printf '%s\\n' '{\"trace_id\":null,\"request_id\":null,\"run_id\":\"run-1\",\"session_id\":\"550e8400-e29b-41d4-a716-446655440000\",\"text\":\"ok\",\"final_state\":\"completed\",\"interruption_kind\":null,\"tool_result_class_counts\":{},\"prompt_tokens\":0,\"fresh_prompt_tokens\":0,\"cache\":{\"hit\":false,\"read_tokens\":0,\"creation_tokens\":0},\"completion_tokens\":0,\"llm_rounds\":0,\"tool_calls_count\":0,\"tools_used\":[],\"persistence_error\":null,\"exit_code\":0,\"success\":true,\"error_kind\":null}'\n",
            ),
        )
        .expect("write shim");

        let mut case = simple_case();
        case.cli_env.insert(
            "HARNESS_ARGS_PATH".into(),
            args_path.to_string_lossy().into_owned(),
        );
        let exec = AstraCliExecutor::new(RunnerConfig::new(shim));
        let root = exec.execute(&case, "m").await;
        let root_args = std::fs::read_to_string(&args_path).expect("root args");
        assert_eq!(
            root.session_id.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000"),
            "root outcome: {root:?}; args={root_args:?}"
        );
        assert!(
            !root_args.lines().any(|arg| arg == "--session-id"),
            "root turn must not fabricate a resumable id: {root_args:?}"
        );
        assert!(root_args.lines().any(|arg| arg == "--no-resume"));

        case.extra_cli_args = vec![
            "--session-id".into(),
            "550e8400-e29b-41d4-a716-446655440000".into(),
        ];
        let follow_up = exec.execute(&case, "m").await;
        assert_eq!(
            follow_up.session_id.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
        let follow_up_args = std::fs::read_to_string(&args_path).expect("follow-up args");
        assert!(
            follow_up_args
                .lines()
                .collect::<Vec<_>>()
                .windows(2)
                .any(|pair| { pair == ["--session-id", "550e8400-e29b-41d4-a716-446655440000"] }),
            "follow-up must preserve the server-issued id: {follow_up_args:?}"
        );
        assert!(
            !follow_up_args.lines().any(|arg| arg == "--no-resume"),
            "follow-up must use the explicit server session, not disable resume: {follow_up_args:?}"
        );
    }

    #[tokio::test]
    async fn astra_executor_rejects_empty_stdout_after_success() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        use crate::test_support::write_executable_shim;
        let tmp = tempfile::tempdir().expect("tempdir");
        let shim = tmp.path().join("fake-astra-empty");
        write_executable_shim(&shim, "#!/bin/sh\nexit 0\n").expect("write shim");
        let exec = AstraCliExecutor::new(RunnerConfig::new(shim));
        let out = exec.execute(&simple_case(), "m").await;
        assert_eq!(
            out.exit_code, -1,
            "successful execution still needs an envelope"
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
    // `AstraCliExecutor::execute`, preserve the server-issued session id, and
    // cancel precisely that session. The timeout branch has its own
    // synthetic-outcome construction that was previously untested; this test
    // routes a shim `/bin/sh -c "sleep 10"` script through the real executor
    // path.
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
        // The chat invocation emits the exact server session binding then
        // sleeps. The cleanup invocation succeeds only when it receives that
        // same id through `session cancel`; an inferred/malformed id would
        // take the sleeping branch and fail the elapsed-time assertion.
        use crate::test_support::write_executable_shim;
        let tmp = tempfile::tempdir().expect("tempdir");
        let shim = tmp.path().join("fake-astra");
        write_executable_shim(
            &shim,
            concat!(
                "#!/bin/sh\n",
                "if [ \"$1\" = session ] && [ \"$2\" = cancel ] && [ \"$3\" = 550e8400-e29b-41d4-a716-446655440000 ]; then\n",
                "  printf '%s\\n' '{\"status\":\"cancelled\"}'\n",
                "  exit 0\n",
                "fi\n",
                "events=; next_is_events=0\n",
                "for arg in \"$@\"; do\n",
                "  if [ \"$next_is_events\" = 1 ]; then events=$arg; next_is_events=0;\n",
                "  elif [ \"$arg\" = --stream-events ]; then next_is_events=1; fi\n",
                "done\n",
                "printf '%s\\n' '{\"type\":\"session_bound\",\"session_id\":\"550e8400-e29b-41d4-a716-446655440000\"}' > \"$events\"\n",
                "printf '%s\\n' '{\"type\":\"run_bound\",\"run_id\":\"550e8400-e29b-41d4-a716-446655440001\"}' >> \"$events\"\n",
                "sleep 10\n",
            ),
        )
        .expect("write shim");

        let cfg = RunnerConfig::new(shim.clone());
        let exec = AstraCliExecutor::new(cfg);
        let case = Case {
            name: "timeout_probe".into(),
            description: None,
            prompt: "ignored by the shim — just needs to be non-empty".into(),
            prompt_variants: vec![],
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
            cli_env: std::collections::HashMap::new(),
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
            outcome.text.contains("observed session cancelled"),
            "timeout must cancel the observed server session: {}",
            outcome.text
        );
        assert_eq!(
            outcome.session_id.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000"),
            "timeout must preserve the exact server-issued identity"
        );
        assert_eq!(
            outcome.run_id.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440001"),
            "timeout must preserve the run identity needed to scope durable evidence"
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
            prompt_variants: vec![],
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
            cli_env: std::collections::HashMap::new(),
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
            prompt_variants: vec![],
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
            cli_env: std::collections::HashMap::new(),
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
{"trace_id":null,"request_id":null,"run_id":"run-1","session_id":"550e8400-e29b-41d4-a716-446655440001","text":"external-hello","final_state":"completed","interruption_kind":null,"tool_result_class_counts":{},"prompt_tokens":20,"fresh_prompt_tokens":20,"cache":{"hit":false,"read_tokens":0,"creation_tokens":0},"completion_tokens":10,"llm_rounds":1,"tool_calls_count":2,"tools_used":["Read","Write"],"persistence_error":null,"exit_code":0,"success":true,"error_kind":null}
REPLY"#;
        let exec = ExternalCmdExecutor::new(script, 10);
        let out = exec.execute(&simple_case(), "m").await;
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.text, "external-hello");
        assert_eq!(out.tools_used, vec!["Read", "Write"]);
        assert!(out.duration_ms < 5000);
    }

    #[tokio::test]
    async fn external_executor_rejects_zero_exit_with_invalid_json_envelope() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        let exec = ExternalCmdExecutor::new("printf '{}'", 10);
        let out = exec.execute(&simple_case(), "m").await;
        assert_eq!(
            out.exit_code, -1,
            "protocol failure must survive process exit 0"
        );
        assert!(out.text.contains("invalid JSON outcome envelope"));
    }

    #[tokio::test]
    async fn external_executor_rejects_empty_stdout_after_success() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        let exec = ExternalCmdExecutor::new("true", 10);
        let out = exec.execute(&simple_case(), "m").await;
        assert_eq!(
            out.exit_code, -1,
            "successful execution still needs an envelope"
        );
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
        echo "{\"trace_id\":null,\"request_id\":null,\"run_id\":\"run-1\",\"session_id\":\"550e8400-e29b-41d4-a716-446655440002\",\"text\":\"$OK\",\"final_state\":\"completed\",\"interruption_kind\":null,\"tool_result_class_counts\":{},\"prompt_tokens\":0,\"fresh_prompt_tokens\":0,\"cache\":{\"hit\":false,\"read_tokens\":0,\"creation_tokens\":0},\"completion_tokens\":0,\"llm_rounds\":0,\"tool_calls_count\":0,\"tools_used\":[],\"persistence_error\":null,\"exit_code\":0,\"success\":true,\"error_kind\":null}"
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
        let exec = ExternalCmdExecutor::new(
            r#"printf '%s\n' '{"trace_id":null,"request_id":null,"run_id":"run-1","session_id":"550e8400-e29b-41d4-a716-446655440003","text":"failed","final_state":"interrupted","interruption_kind":"provider_error","tool_result_class_counts":{},"prompt_tokens":0,"fresh_prompt_tokens":0,"cache":{"hit":false,"read_tokens":0,"creation_tokens":0},"completion_tokens":0,"llm_rounds":0,"tool_calls_count":0,"tools_used":[],"persistence_error":null,"exit_code":42,"success":false,"error_kind":"api_error"}'; exit 42"#,
            10,
        );
        let out = exec.execute(&simple_case(), "m").await;
        assert_eq!(out.exit_code, 42);
    }

    #[tokio::test]
    async fn external_executor_rejects_invalid_envelope_even_on_nonzero_exit() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        let exec = ExternalCmdExecutor::new("echo '{}'; exit 42", 10);
        let out = exec.execute(&simple_case(), "m").await;
        assert_eq!(
            out.exit_code, -1,
            "invalid protocol dominates process status"
        );
    }

    #[tokio::test]
    async fn external_executor_rejects_bidirectional_process_exit_mismatch() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        let success_envelope = r#"printf '%s\n' '{"trace_id":null,"request_id":null,"run_id":"run-1","session_id":"550e8400-e29b-41d4-a716-446655440004","text":"ok","final_state":"completed","interruption_kind":null,"tool_result_class_counts":{},"prompt_tokens":0,"fresh_prompt_tokens":0,"cache":{"hit":false,"read_tokens":0,"creation_tokens":0},"completion_tokens":0,"llm_rounds":0,"tool_calls_count":0,"tools_used":[],"persistence_error":null,"exit_code":0,"success":true,"error_kind":null}'; exit 42"#;
        let out = ExternalCmdExecutor::new(success_envelope, 10)
            .execute(&simple_case(), "m")
            .await;
        assert_eq!(out.exit_code, -1);

        let failure_envelope = r#"printf '%s\n' '{"trace_id":null,"request_id":null,"run_id":"run-1","session_id":"550e8400-e29b-41d4-a716-446655440005","text":"failed","final_state":"interrupted","interruption_kind":"provider_error","tool_result_class_counts":{},"prompt_tokens":0,"fresh_prompt_tokens":0,"cache":{"hit":false,"read_tokens":0,"creation_tokens":0},"completion_tokens":0,"llm_rounds":0,"tool_calls_count":0,"tools_used":[],"persistence_error":null,"exit_code":42,"success":false,"error_kind":"api_error"}'; exit 0"#;
        let out = ExternalCmdExecutor::new(failure_envelope, 10)
            .execute(&simple_case(), "m")
            .await;
        assert_eq!(out.exit_code, -1);
    }

    #[tokio::test]
    async fn external_executor_does_not_call_an_unexplained_empty_exit_authentication() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        let exec = ExternalCmdExecutor::new("exit 3", 10);
        let out = exec.execute(&simple_case(), "m").await;
        assert_eq!(out.exit_code, 3);
        assert!(out.text.is_empty());
        assert_eq!(
            crate::classify::classify(&out, &[]),
            crate::classify::FailureClass::Unknown
        );
    }
}

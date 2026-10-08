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

/// One launch boundary for execution and its subsequent inspection command.
/// An implicit profile must stay implicit, particularly for native MOI auth.
pub(crate) fn configured_astra_command(cfg: &RunnerConfig, case: &Case) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(&cfg.astra_bin);
    if let Some(profile) = &cfg.profile {
        command.args(["--profile", profile]);
    }
    if let Some(directory) = &cfg.working_dir {
        command.current_dir(directory);
    }
    command.envs(&case.cli_env);
    command
}

#[async_trait]
impl CaseExecutor for AstraCliExecutor {
    async fn execute(&self, case: &Case, model: &str) -> RunOutcome {
        let mut outcome = run_case_subprocess(&self.cfg, case, model).await;
        if crate::criteria::requires_execution_capture(&case.criteria) {
            let capture = crate::execution_capture::load(&self.cfg, case, &outcome).await;
            if let Some(stream) = outcome.stream_capture.as_mut() {
                match capture {
                    Ok(capture) => stream.execution = Some(capture),
                    Err(_) => stream.diagnose("canonical_execution_capture_failed"),
                }
            }
        }
        outcome
    }

    fn reproducer(&self, case: &Case, model: &str) -> String {
        let mut parts = vec![shell_escape(self.cfg.astra_bin.display().to_string())];
        if let Some(profile) = &self.cfg.profile {
            parts.extend(["--profile".into(), shell_escape(profile.clone())]);
        }
        parts.extend(
            case_cli_arguments(case, model, "events.jsonl")
                .into_iter()
                .map(shell_escape),
        );
        parts.join(" ")
    }
}

/// One argument owner for execution and reproductions.
fn case_cli_arguments(case: &Case, model: &str, events_path: &str) -> Vec<String> {
    let mut args = vec!["--model".into(), model.into(), "-y".into()];
    args.push("chat".into());
    args.push("--json".into());
    if !case
        .extra_cli_args
        .iter()
        .any(|arg| arg == "--explain" || arg.starts_with("--explain="))
    {
        args.push("--explain=on".into());
    }
    args.extend(["--stream-events".into(), events_path.into()]);
    if !has_session_id(&case.extra_cli_args) {
        args.push("--no-resume".into());
    }
    if let Some(seconds) = case.cli_wall_time_override_for(case.timeout_seconds) {
        args.extend(["--max-wall-time-seconds".into(), seconds.to_string()]);
    }
    args.extend(case.extra_cli_args.clone());
    args.extend(["-m".into(), case.prompt.clone()]);
    args
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
// A canonical snapshot is one JSONL envelope, not one line per fact. Allow
// envelope headroom beyond the archive budget, still bounded before parsing.
// Archive overflow is diagnostic loss, not invalid lifecycle binding.
const MAX_STREAM_EVENT_LINE_BYTES: usize = 2 * crate::explain_capture::MAX_BYTES;
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
    explain: crate::explain_capture::ExplainCapture,
    stream: Option<crate::runner::StreamCapture>,
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
        self.explain.observe(&value);
        if let Some(stream) = &mut self.stream {
            stream.observe(&value, self.event_count);
        }
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
pub(crate) async fn kill_process_group_and_reap(
    child: &mut tokio::process::Child,
    group_id: Option<u32>,
) {
    if let Some(pid) = group_id
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

async fn wait_for_user_cancel(flag: Option<&Arc<std::sync::atomic::AtomicBool>>) {
    let Some(flag) = flag else {
        std::future::pending::<()>().await;
        return;
    };
    while !flag.load(std::sync::atomic::Ordering::SeqCst) {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[cfg(not(unix))]
pub(crate) async fn kill_process_group_and_reap(
    child: &mut tokio::process::Child,
    _group_id: Option<u32>,
) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}

async fn join_output_reader(
    reader: &mut tokio::task::JoinHandle<std::io::Result<Vec<u8>>>,
) -> Result<Vec<u8>, String> {
    reader
        .await
        .map_err(|error| format!("stdout reader task failed: {error}"))?
        .map_err(|error| format!("stdout reader failed: {error}"))
}

async fn join_stderr_reader(
    reader: &mut tokio::task::JoinHandle<std::io::Result<Vec<u8>>>,
) -> Result<Vec<u8>, String> {
    reader
        .await
        .map_err(|error| format!("stderr reader task failed: {error}"))?
        .map_err(|error| format!("stderr reader failed: {error}"))
}

async fn finish_machine_event_observer(
    done: tokio_util::sync::CancellationToken,
    reader: &mut tokio::task::JoinHandle<std::io::Result<()>>,
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

type CollectedCaseStreams = (Result<Vec<u8>, String>, Result<Vec<u8>, String>);

async fn collect_case_streams(
    stdout_reader: &mut tokio::task::JoinHandle<std::io::Result<Vec<u8>>>,
    stderr_reader: &mut tokio::task::JoinHandle<std::io::Result<Vec<u8>>>,
    stdout_result: &mut Option<Result<Vec<u8>, String>>,
    stderr_result: &mut Option<Result<Vec<u8>, String>>,
    deadline: tokio::time::Instant,
) -> Result<CollectedCaseStreams, &'static str> {
    while stdout_result.is_none() || stderr_result.is_none() {
        tokio::select! {
            result = join_output_reader(stdout_reader), if stdout_result.is_none() => *stdout_result = Some(result),
            result = join_stderr_reader(stderr_reader), if stderr_result.is_none() => *stderr_result = Some(result),
            _ = tokio::time::sleep_until(deadline) => {
                stdout_reader.abort();
                stderr_reader.abort();
                return Err("timeout while draining subprocess evidence");
            }
        }
    }
    Ok((
        stdout_result.take().expect("stdout reader completed"),
        stderr_result.take().expect("stderr reader completed"),
    ))
}

async fn finish_machine_event_observer_bounded(
    done: tokio_util::sync::CancellationToken,
    reader: &mut tokio::task::JoinHandle<std::io::Result<()>>,
    observation: &Arc<Mutex<MachineEventObservation>>,
    deadline: tokio::time::Instant,
) -> Result<(Option<String>, Option<String>), String> {
    match tokio::time::timeout_at(
        deadline,
        finish_machine_event_observer(done, reader, observation),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => {
            reader.abort();
            Err("machine event drain timed out".into())
        }
    }
}

fn observed_machine_identity(
    observation: &Arc<Mutex<MachineEventObservation>>,
) -> (Option<String>, Option<String>) {
    observation.lock().ok().map_or((None, None), |observed| {
        if observed.invalid.is_some() {
            (None, None)
        } else {
            (observed.session_id.clone(), observed.run_id.clone())
        }
    })
}

async fn finish_failed_evidence_outcome(
    cfg: &RunnerConfig,
    observation: &Arc<Mutex<MachineEventObservation>>,
    mut outcome: RunOutcome,
) -> RunOutcome {
    // Capture integrity and cleanup authority are separate. A failed final
    // read cannot certify the run, but an earlier unambiguous server-issued
    // session identity still needs to be cancelled before returning.
    let (session_id, _) = observed_machine_identity(observation);
    outcome
        .stderr
        .push_str(&cleanup_observed_session(cfg, session_id.as_deref()).await);
    retain_machine_capture(&mut outcome, observation);
    outcome
}

async fn run_case_subprocess(cfg: &RunnerConfig, case: &Case, model: &str) -> RunOutcome {
    use std::process::Stdio;
    use std::time::Duration;

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
    let mut cmd = configured_astra_command(cfg, case);
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
    cmd.args(case_cli_arguments(
        case,
        model,
        &stream_event_path.to_string_lossy(),
    ));
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

    if cfg
        .cancel_flag
        .as_ref()
        .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::SeqCst))
    {
        return RunOutcome {
            model: model.into(),
            exit_code: 130,
            text: "cancelled before CLI subprocess admission".into(),
            final_state: Some("interrupted".into()),
            interruption_kind: Some("cancelled".into()),
            duration_ms: start.elapsed().as_millis() as u64,
            ..Default::default()
        };
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
                error_kind: None,
                explain_capture: None,
                stream_capture: None,
                tool_result_class_counts: std::collections::BTreeMap::new(),
                tool_calls_count: 0,
                tools_used: vec![],
                completion_tokens: 0,
                prompt_tokens: 0,
                cached_input_tokens: 0,
                cache_creation_tokens: 0,
                token_usage_coverage: None,
                duration_ms: start.elapsed().as_millis() as u64,
                turn_rounds: 0,
                cache_hits: 0,
                total_tool_calls: 0,
                ttft_ms: 0,
            };
        }
    };

    let child_group_id = child.id();
    let stdout = child.stdout.take().expect("piped stdout is present");
    let stderr = child.stderr.take().expect("piped stderr is present");
    let machine_observation = Arc::new(Mutex::new(MachineEventObservation {
        stream: (cfg.artifacts_dir.is_some()
            || crate::criteria::requires_execution_capture(&case.criteria))
        .then(Default::default),
        ..Default::default()
    }));
    let machine_observer_done = tokio_util::sync::CancellationToken::new();
    let mut machine_event_reader = tokio::spawn(observe_machine_event_file(
        stream_event_path,
        Arc::clone(&machine_observation),
        machine_observer_done.clone(),
    ));
    let mut stdout_reader = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;

        let mut stdout = stdout;
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).await?;
        Ok::<_, std::io::Error>(bytes)
    });
    let mut stderr_reader = tokio::spawn(collect_stderr(stderr));
    // A cancellation can interrupt the wait after either reader completes.
    // Keep that result outside the wait future so no JoinHandle is polled twice.
    let mut stdout_result = None;
    let mut stderr_result = None;

    let timeout = Duration::from_secs(case.timeout_seconds);
    let deadline = tokio::time::Instant::from_std(start + timeout);
    // Keep the child unreaped until both pipes close. If a descendant holds a
    // pipe open, the unreaped child still owns its process-group ID when the
    // deadline fires; a cached integer after wait/reap would not be safe to
    // signal on a busy multi-session host.
    let (stream_result, cancelled_during_drain) = tokio::select! {
        biased;
        streams = collect_case_streams(&mut stdout_reader, &mut stderr_reader, &mut stdout_result, &mut stderr_result, deadline) => (Some(streams), false),
        _ = wait_for_user_cancel(cfg.cancel_flag.as_ref()) => (None, true),
    };
    let mut drained_streams = None;
    let mut drain_error = None;
    let (wait_result, user_cancelled) = match stream_result {
        Some(Ok(streams)) => {
            drained_streams = Some(streams);
            tokio::select! {
                biased;
                status = child.wait() => (Some(status), false),
                _ = wait_for_user_cancel(cfg.cancel_flag.as_ref()) => (None, true),
                _ = tokio::time::sleep_until(deadline) => (None, false),
            }
        }
        Some(Err(error)) => {
            drain_error = Some(error);
            (None, false)
        }
        None => (None, cancelled_during_drain),
    };
    let mut outcome = match wait_result {
        Some(Ok(status)) => {
            let (stdout, stderr) = drained_streams.expect("pipes closed before child was reaped");
            let machine = finish_machine_event_observer_bounded(
                machine_observer_done.clone(),
                &mut machine_event_reader,
                &machine_observation,
                deadline,
            )
            .await;
            let (stdout, stderr, (observed_session_id, _observed_run_id)) = match (
                stdout, stderr, machine,
            ) {
                (Ok(stdout), Ok(stderr), Ok(machine)) => (stdout, stderr, machine),
                (Ok(stdout), Ok(stderr), Err(error)) => {
                    // Keep terminal diagnostics, but never use an unbound
                    // identity to load a journal or certify task success.
                    let mut out = parse_executor_outcome(
                        &String::from_utf8_lossy(&stdout),
                        model,
                        status.code().unwrap_or(-1),
                    );
                    out.exit_code = -1;
                    out.session_id = None;
                    out.run_id = None;
                    out.stderr = format!(
                        "{PROTOCOL_ERROR_MARKER} invalid machine events: {error}\n{}",
                        String::from_utf8_lossy(&stderr)
                    );
                    out.duration_ms = start.elapsed().as_millis() as u64;
                    return finish_failed_evidence_outcome(cfg, &machine_observation, out).await;
                }
                (stdout_error, stderr_error, machine_error) => {
                    let out = RunOutcome {
                        model: model.into(),
                        exit_code: -1,
                        text: format!(
                            "subprocess evidence collection failed: stdout={stdout_error:?}; stderr={stderr_error:?}; machine_events={machine_error:?}"
                        ),
                        duration_ms: start.elapsed().as_millis() as u64,
                        ..Default::default()
                    };
                    return finish_failed_evidence_outcome(cfg, &machine_observation, out).await;
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
        Some(Err(error)) => {
            kill_process_group_and_reap(&mut child, child_group_id).await;
            let stderr = if let Some((_stdout, stderr)) = drained_streams {
                stderr.unwrap_or_default()
            } else {
                collect_case_streams(
                    &mut stdout_reader,
                    &mut stderr_reader,
                    &mut stdout_result,
                    &mut stderr_result,
                    tokio::time::Instant::now() + Duration::from_secs(2),
                )
                .await
                .ok()
                .and_then(|(_stdout, stderr)| stderr.ok())
                .unwrap_or_default()
            };
            let machine = finish_machine_event_observer_bounded(
                machine_observer_done.clone(),
                &mut machine_event_reader,
                &machine_observation,
                tokio::time::Instant::now() + Duration::from_secs(2),
            )
            .await;
            let (session_id, _) = machine
                .clone()
                .unwrap_or_else(|_| observed_machine_identity(&machine_observation));
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
        None => {
            kill_process_group_and_reap(&mut child, child_group_id).await;
            let stderr = if let Some((_stdout, stderr)) = drained_streams {
                stderr.unwrap_or_default()
            } else {
                collect_case_streams(
                    &mut stdout_reader,
                    &mut stderr_reader,
                    &mut stdout_result,
                    &mut stderr_result,
                    tokio::time::Instant::now() + Duration::from_secs(2),
                )
                .await
                .ok()
                .and_then(|(_stdout, stderr)| stderr.ok())
                .unwrap_or_default()
            };
            let machine = finish_machine_event_observer_bounded(
                machine_observer_done.clone(),
                &mut machine_event_reader,
                &machine_observation,
                tokio::time::Instant::now() + Duration::from_secs(2),
            )
            .await;
            let (session_id, observed_run_id) = machine
                .clone()
                .unwrap_or_else(|_| observed_machine_identity(&machine_observation));
            let cleanup = cleanup_observed_session(cfg, session_id.as_deref()).await;
            let stderr = String::from_utf8_lossy(&stderr).into_owned();
            let mut out = RunOutcome {
                model: model.into(),
                // Both paths remain non-successful after resource convergence.
                exit_code: if user_cancelled { 130 } else { 124 },
                text: format!(
                    "{}{cleanup}{}",
                    if user_cancelled {
                        "cancelled by user".to_string()
                    } else if let Some(error) = drain_error {
                        error.to_string()
                    } else {
                        format!(
                            "timeout after {}s (case timeout_seconds={})",
                            timeout.as_secs(),
                            case.timeout_seconds
                        )
                    },
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
                interruption_kind: Some(
                    if user_cancelled {
                        "cancelled"
                    } else {
                        "timeout"
                    }
                    .into(),
                ),
                duration_ms: start.elapsed().as_millis() as u64,
                ..Default::default()
            };
            // An interruption does not erase typed progress that was durably
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
    };
    retain_machine_capture(&mut outcome, &machine_observation);
    outcome
}

fn retain_machine_capture(
    outcome: &mut RunOutcome,
    observation: &Arc<Mutex<MachineEventObservation>>,
) {
    let mut capture = crate::explain_capture::ExplainCapture::default();
    if let Ok(mut observed) = observation.lock() {
        capture = observed.explain.clone();
        if observed.invalid.is_some() {
            capture.diagnose("invalid_machine_stream");
        }
        let binding_matches = outcome.session_id.is_some()
            && outcome.session_id == observed.session_id
            && outcome.run_id == observed.run_id;
        capture.bind(if binding_matches {
            outcome.run_id.as_deref()
        } else {
            None
        });
        if let Some(mut stream) = observed.stream.take() {
            stream.session_id = observed.session_id.clone();
            stream.root_run_id = observed.run_id.clone();
            stream.identity_verified =
                observed.invalid.is_none() && binding_matches && observed.run_id.is_some();
            if observed.invalid.is_some() {
                stream.diagnose("invalid_machine_stream");
            }
            if !stream.identity_verified {
                stream.diagnose("unverified_run_scope");
            }
            if outcome.exit_code != 0 || outcome.final_state.as_deref() != Some("completed") {
                stream.diagnose("execution_incomplete");
            }
            outcome.stream_capture = Some(stream);
        }
    } else {
        capture.diagnose("capture_reader_unavailable");
    }
    if outcome.exit_code != 0 || outcome.final_state.as_deref() != Some("completed") {
        capture.diagnose("execution_incomplete");
    }
    outcome.explain_capture = Some(capture);
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

    pub(crate) fn cache_outcome(run: &str, read: u64, creation: u64) -> crate::runner::RunOutcome {
        cache_request_outcome(run, "t", &[(100, read, creation)])
    }

    pub(crate) fn cache_request_outcome(
        run: &str,
        turn: &str,
        usages: &[(u64, u64, u64)],
    ) -> crate::runner::RunOutcome {
        let mut out = crate::runner::RunOutcome::new("m");
        out.run_id = Some(run.into());
        out.session_id = Some("cache-session".into());
        let mut capture = crate::explain_capture::ExplainCapture::default();
        let mut nodes = vec![("turn".to_owned(), "turn", None, None)];
        for (index, usage) in usages.iter().enumerate() {
            nodes.push((
                format!("model-{index}"),
                "model_round",
                Some(index as u32),
                None,
            ));
            nodes.push((
                format!("request-{index}"),
                "provider_attempt",
                Some(index as u32),
                Some(*usage),
            ));
        }
        for (node, kind, round, usage) in nodes {
            let mut start = serde_json::json!({"schema_version":1,
                "event_id":format!("{node}-start"),"run_id":run,"turn_id":turn,
                "node_id":node,"producer_id":"p","clock_domain_id":"c",
                "kind":kind,"label":node,"transition":"started","elapsed_ms":0});
            if let Some(round) = round {
                start["round_index"] = serde_json::json!(round);
                start["attempt_index"] = serde_json::json!(0);
                start["parent_node_id"] = serde_json::json!(if kind == "model_round" {
                    "turn".to_owned()
                } else {
                    format!("model-{round}")
                });
            }
            let mut finish = start.clone();
            finish["event_id"] = serde_json::json!(format!("{node}-finish"));
            finish["transition"] = serde_json::json!("finished");
            finish["outcome"] = serde_json::json!("completed");
            finish["start_elapsed_ms"] = serde_json::json!(0);
            finish["duration_ms"] = serde_json::json!(0);
            if let Some((fresh, read, creation)) = usage {
                finish["usage"] = serde_json::json!({"basis":"provider_exact",
                    "fresh_input_tokens":fresh,"cache_read_tokens":read,"cache_creation_tokens":creation});
            }
            for fact in [start, finish] {
                let fact: astra_turn_types::ExplainAnalyzeEventV1 =
                    serde_json::from_value(fact).unwrap();
                assert!(fact.is_valid());
                capture.events.push(fact);
            }
        }
        capture.bind(Some(run));
        out.explain_capture = Some(capture);
        out
    }

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
                    token_usage_coverage: None,
                    duration_ms: 0,
                    turn_rounds: 0,
                    cache_hits: 0,
                    total_tool_calls: 0,
                    ttft_ms: 0,
                    final_state: None,
                    interruption_kind: None,
                    error_kind: None,
                    explain_capture: None,
                    stream_capture: None,
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
        assert!(
            observation.stream.is_none(),
            "raw capture requires artifact opt-in"
        );
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

    #[test]
    fn incomplete_execution_retains_explain_facts_without_certifying_identity() {
        let mut observed = MachineEventObservation {
            stream: Some(Default::default()),
            ..Default::default()
        };
        observed.observe_line(r#"{"type":"explain_analyze","schema_version":1,"event_id":"e","run_id":"foreign","turn_id":"t","node_id":"n","producer_id":"p","clock_domain_id":"c","kind":"admission","label":"Admission","transition":"started","elapsed_ms":0}"#);
        observed.observe_line(r#"{"type":"agent_live","event":{"run_id":"child","agent_id":"agent","kind":{"type":"output_delta","model_item_id":null,"text":"partial\n"}}}"#);
        observed.observe_line("corrupt trailing evidence");
        let mut outcome = RunOutcome {
            exit_code: 124,
            ..Default::default()
        };
        retain_machine_capture(&mut outcome, &Arc::new(Mutex::new(observed)));
        let stream = outcome.stream_capture.unwrap();
        assert_eq!(stream.records.len(), 1);
        assert!(!stream.identity_verified);
        assert!(
            stream
                .diagnostics
                .contains(&"invalid_machine_stream".into())
        );
        assert!(stream.diagnostics.contains(&"execution_incomplete".into()));
        let capture = outcome.explain_capture.unwrap();
        assert_eq!(capture.events.len(), 1);
        assert!(!capture.identity_verified);
        assert!(
            capture
                .diagnostics
                .contains(&"invalid_machine_stream".into())
        );
        assert!(capture.diagnostics.contains(&"execution_incomplete".into()));
    }

    #[test]
    fn stream_capture_bounds_and_gaps_do_not_invalidate_explain() {
        use crate::runner::{MAX_STREAM_CAPTURE_BYTES, MAX_STREAM_CAPTURE_RECORDS, StreamCapture};

        let mut observed = MachineEventObservation {
            stream: Some(Default::default()),
            ..Default::default()
        };
        observed.explain.observe(&serde_json::json!({
            "type":"explain_analyze_snapshot", "delivery_degraded":false,
            "events":[{"schema_version":1,"event_id":"e","run_id":"r","turn_id":"t",
                "node_id":"n","producer_id":"p","clock_domain_id":"c",
                "kind":"admission","label":"Admission","transition":"started","elapsed_ms":0}]
        }));
        observed.explain.bind(Some("r"));
        let mut wire = serde_json::json!({"type":"agent_live","event":{
            "run_id":"child","agent_id":"agent","kind":{
                "type":"output_delta","model_item_id":"item","text":"界".repeat(MAX_STREAM_CAPTURE_BYTES / 6)
            }
        }});
        for _ in 0..3 {
            observed.observe_line(&wire.to_string());
        }
        observed.observe_line(r#"{"type":"agent_live","event":{}}"#);
        observed.observe_line(r#"{"type":"agent_live_gap","gap":{"run_id":"child","agent_id":"agent","dropped_event_count":1}}"#);
        let stream = observed.stream.unwrap();
        assert!(stream.diagnostics.contains(&"capture_truncated".into()));
        assert!(stream.diagnostics.contains(&"invalid_agent_live".into()));
        assert!(stream.diagnostics.contains(&"agent_live_gap".into()));
        assert!(serde_json::to_vec(&stream).unwrap().len() <= MAX_STREAM_CAPTURE_BYTES);
        assert!(observed.invalid.is_none());
        assert!(observed.explain.canonical_graph().is_some());

        wire["event"]["kind"]["text"] = serde_json::json!("x");
        let mut stream = StreamCapture::default();
        for index in 0..=MAX_STREAM_CAPTURE_RECORDS {
            stream.observe(&wire, index as u64);
        }
        assert_eq!(stream.records.len(), MAX_STREAM_CAPTURE_RECORDS);
        assert!(stream.diagnostics.contains(&"capture_truncated".into()));
        assert!(serde_json::to_vec(&stream).unwrap().len() <= MAX_STREAM_CAPTURE_BYTES);
    }

    #[tokio::test]
    async fn failed_evidence_cleanup_uses_verified_session_but_not_conflicting_identity() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let shim = tmp.path().join("fake-astra");
        crate::test_support::write_executable_shim(
            &shim,
            concat!(
                "#!/bin/sh\n",
                "if [ \"$1\" = session ] && [ \"$2\" = cancel ] && [ \"$3\" = 550e8400-e29b-41d4-a716-446655440000 ]; then\n",
                "  printf '%s\\n' '{\"session_id\":\"550e8400-e29b-41d4-a716-446655440000\",\"status\":\"cancelled\",\"execution_settled\":true}'\n",
                "  exit 0\n",
                "fi\n",
                "exit 88\n",
            ),
        )
        .expect("write shim");
        let cfg = RunnerConfig::new(shim);
        let observation = Arc::new(Mutex::new(MachineEventObservation::default()));
        observation.lock().unwrap().observe_line(
            r#"{"type":"session_bound","session_id":"550e8400-e29b-41d4-a716-446655440000"}"#,
        );

        let failed_capture = || RunOutcome {
            exit_code: -1,
            text: "subprocess evidence collection failed".into(),
            ..Default::default()
        };
        let recovered = finish_failed_evidence_outcome(&cfg, &observation, failed_capture()).await;
        assert!(recovered.stderr.contains("observed session cancelled"));
        assert!(
            recovered.session_id.is_none(),
            "failed capture cannot certify a run"
        );

        observation.lock().unwrap().observe_line(
            r#"{"type":"session_bound","session_id":"550e8400-e29b-41d4-a716-446655440001"}"#,
        );
        let conflicting =
            finish_failed_evidence_outcome(&cfg, &observation, failed_capture()).await;
        assert!(
            conflicting
                .stderr
                .contains("no server-issued session identity observed")
        );
        assert!(!conflicting.stderr.contains("observed session cancelled"));
    }

    #[tokio::test]
    async fn machine_event_file_observer_reads_dedicated_jsonl_before_shutdown() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.jsonl");
        let observation = Arc::new(Mutex::new(MachineEventObservation {
            stream: Some(Default::default()),
            ..Default::default()
        }));
        let done = tokio_util::sync::CancellationToken::new();
        let mut reader = tokio::spawn(observe_machine_event_file(
            path.clone(),
            Arc::clone(&observation),
            done.clone(),
        ));
        let owner = serde_json::json!({"account_id":"account", "profile_name":"profile", "api_origin":"https://example.invalid"});
        let mut events = [
            serde_json::json!({"type":"session_bound", "session_id":"550e8400-e29b-41d4-a716-446655440000", "owner":owner}),
            serde_json::json!({"type":"run_bound", "run_id":"8a0dcb50-38a7-4402-bef3-2c1aee9a4e85", "owner":owner}),
        ].iter().map(|event| event.to_string()).collect::<Vec<_>>().join("\n") + "\n";
        for (run_id, kind) in [
            (
                "child-a",
                serde_json::json!({"type":"signal","signal":"run_started",
                "parent_run_id":"8a0dcb50-38a7-4402-bef3-2c1aee9a4e85","depth":1,
                "spawn_tool_call_id":"spawn-a","transcript_location":"durable_server"}),
            ),
            (
                "child-a",
                serde_json::json!({"type":"output_delta","model_item_id":"item-a","text":"4"}),
            ),
            (
                "child-a",
                serde_json::json!({"type":"thinking_delta","model_item_id":null,"text":"excluded-secret"}),
            ),
            (
                "child-b",
                serde_json::json!({"type":"signal","signal":"run_started",
                "parent_run_id":"child-a","depth":2,"spawn_tool_call_id":"spawn-b",
                "transcript_location":"local_journal"}),
            ),
            (
                "child-b",
                serde_json::json!({"type":"output_delta","model_item_id":"item-b","text":"other"}),
            ),
            (
                "child-a",
                serde_json::json!({"type":"output_delta","model_item_id":"item-a","text":"4"}),
            ),
            (
                "child-a",
                serde_json::json!({"type":"output_delta","model_item_id":"item-c","text":"2\n"}),
            ),
            (
                "child-a",
                serde_json::json!({"type":"status","text":"excluded-secret"}),
            ),
            (
                "child-a",
                serde_json::json!({"type":"tool_completed","name":"read_file",
                "description":"excluded-secret","status":"completed","duration_ms":0,
                "output_summary":null,"output":"excluded-secret","tool_use_id":"tool"}),
            ),
            (
                "child-a",
                serde_json::json!({"type":"signal","signal":"ask_user_prompted",
                "request_id":"ask","prompt":{"text":"excluded-secret"}}),
            ),
            (
                "child-a",
                serde_json::json!({"type":"signal","signal":"transcript_committed",
                "model_item_id":null,"source_event_id":"response:child-a:turn-1",
                "transcript_location":"durable_server"}),
            ),
            (
                "child-a",
                serde_json::json!({"type":"agent_terminated","termination":"completed",
                "duration_ms":3,"reason":null}),
            ),
        ] {
            events.push_str(
                &serde_json::json!({"type":"agent_live","event":{
                    "run_id":run_id,"agent_id":"shared-agent","kind":kind
                }})
                .to_string(),
            );
            events.push('\n');
        }
        tokio::fs::write(path, events).await.unwrap();

        let (session_id, run_id) = finish_machine_event_observer(done, &mut reader, &observation)
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
        let mut outcome = RunOutcome::new("m")
            .with_session_id(session_id.unwrap())
            .with_final_state("completed");
        outcome.run_id = run_id;
        retain_machine_capture(&mut outcome, &observation);
        directory.close().unwrap();
        let stream = outcome.stream_capture.as_ref().unwrap();
        assert!(stream.identity_verified);
        assert!(stream.diagnostics.is_empty());
        let records = serde_json::to_value(&stream.records).unwrap();
        assert_eq!(stream.records.len(), 8);
        assert_eq!(records[2]["event"]["kind"]["parent_run_id"], "child-a");
        assert_eq!(records[3]["event"]["run_id"], "child-b");
        assert_eq!(
            records[1]["event"]["kind"]["text"],
            records[4]["event"]["kind"]["text"]
        );
        assert_eq!(records[5]["event"]["kind"]["model_item_id"], "item-c");
        assert_eq!(records[5]["event"]["kind"]["text"], "2\n");
        assert_eq!(
            records[6]["event"]["kind"]["source_event_id"],
            "response:child-a:turn-1"
        );
        assert!(records[6]["event"]["kind"]["model_item_id"].is_null());
        assert_eq!(records[7]["event"]["kind"]["termination"], "completed");
        assert!(
            !serde_json::to_string(stream)
                .unwrap()
                .contains("excluded-secret")
        );
        assert!(
            serde_json::to_value(&outcome)
                .unwrap()
                .get("stream_capture")
                .is_none()
        );
        let mut mismatched = outcome.clone();
        mismatched.run_id = Some("foreign".into());
        observation.lock().unwrap().stream = outcome.stream_capture.clone();
        retain_machine_capture(&mut mismatched, &observation);
        let stream = mismatched.stream_capture.unwrap();
        assert!(!stream.identity_verified);
        assert_eq!(
            stream.root_run_id.as_deref(),
            Some("8a0dcb50-38a7-4402-bef3-2c1aee9a4e85")
        );
        assert_eq!(stream.records.len(), 8);
    }

    #[tokio::test]
    async fn machine_file_observer_accepts_snapshot_larger_than_old_line_limit() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.jsonl");
        let events: Vec<_> = (0..400)
            .map(|id| {
                serde_json::json!({
                    "schema_version":1,"event_id":format!("e{id}"),"run_id":"r",
                    "turn_id":"t","node_id":format!("n{id}"),"producer_id":"p",
                    "clock_domain_id":"c","kind":"admission","label":"Admission",
                    "transition":"started","elapsed_ms":0
                })
            })
            .collect();
        let line = serde_json::json!({"type":"explain_analyze_snapshot",
            "events":events,"delivery_degraded":false})
        .to_string()
            + "\n";
        assert!(line.len() > 64 * 1024);
        tokio::fs::write(&path, line).await.unwrap();
        let observation = Arc::new(Mutex::new(MachineEventObservation::default()));
        let done = tokio_util::sync::CancellationToken::new();
        let mut reader = tokio::spawn(observe_machine_event_file(
            path,
            observation.clone(),
            done.clone(),
        ));
        finish_machine_event_observer(done, &mut reader, &observation)
            .await
            .unwrap();
        let observed = observation.lock().unwrap();
        assert!(observed.invalid.is_none());
        assert_eq!(observed.explain.events.len(), 400);
        assert!(observed.explain.diagnostics.is_empty());
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
            timeout_seconds: 180,
            cli_wall_time_seconds: Some(150),
            capability: None,
            required_cache_scope: None,
            difficulty: None,
            weight: 1.0,
            steps: vec![],
            cli_env: std::collections::HashMap::new(),
            setup_cmd: None,
            teardown_cmd: None,
            cleanup_memory_records: false,
            requires_memoria: false,
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
        assert!(repro.contains("'--max-wall-time-seconds' '150'"));
        let mut default_case = simple_case();
        assert!(
            !exec
                .reproducer(&default_case, "qwen-flash")
                .contains("--max-wall-time-seconds"),
            "unconfigured cases must retain their full outer watchdog budget"
        );
        default_case.timeout_seconds = 180;
        assert!(
            !exec
                .reproducer(&default_case, "qwen-flash")
                .contains("--max-wall-time-seconds"),
            "case duration alone must not truncate tool execution"
        );
        // POSIX single-quote escape: `'say '\''hello'\'''` preserves
        // the original bytes without relying on double-quote semantics
        // (which would still expand $ and backticks). A prompt with
        // apostrophes round-trips exactly.
        assert!(
            repro.contains(r"'say '\''hello'\'''"),
            "POSIX single-quote escape expected: {repro}"
        );
    }

    #[test]
    fn explicit_explain_mode_overrides_capture_default() {
        let ordinary = Case::from_path(std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/cases/runtime_observation_recovery/ordinary_previous_run.yaml"
        )))
        .unwrap();
        assert_eq!(ordinary.extra_cli_args, ["--explain=off"]);
        assert!(
            !case_cli_arguments(&ordinary, "test-model", "events.jsonl")
                .contains(&"--explain=on".into())
        );
        for explicit in [vec![], vec!["--explain=off"], vec!["--explain", "off"]] {
            let mut case = simple_case();
            case.extra_cli_args = explicit.iter().map(|arg| (*arg).into()).collect();
            crate::case::validate_extra_cli_args(&case.extra_cli_args).unwrap();
            let args = case_cli_arguments(&case, "test-model", "events.jsonl");
            assert_eq!(
                args.iter()
                    .filter(|arg| *arg == "--explain" || arg.starts_with("--explain="))
                    .count(),
                1
            );
            assert_eq!(args.contains(&"--explain=on".into()), explicit.is_empty());
            for arg in explicit {
                assert!(args.contains(&arg.to_string()));
            }
        }
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
        case.timeout_seconds = 180;
        case.cli_env.insert(
            "HARNESS_ARGS_PATH".into(),
            args_path.to_string_lossy().into_owned(),
        );
        let exec = AstraCliExecutor::new(RunnerConfig::new(shim));
        {
            case.extra_cli_args.clear();
            let root = exec.execute(&case, "m").await;
            let root_args = std::fs::read_to_string(&args_path).expect("root args");
            let args: Vec<_> = root_args.lines().collect();
            assert_eq!(&args[..3], &["--model", "m", "-y"]);
            assert_eq!(args[3], "chat");
            assert_eq!(args[args.len() - 2], "-m");
            assert_eq!(args.last().copied(), Some(case.prompt.as_str()));
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
            assert!(
                !root_args
                    .lines()
                    .any(|arg| arg == "--max-wall-time-seconds"),
                "unconfigured cases must preserve the outer watchdog budget: {root_args:?}"
            );

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
                    .any(|pair| {
                        pair == ["--session-id", "550e8400-e29b-41d4-a716-446655440000"]
                    }),
                "follow-up must preserve the server-issued id: {follow_up_args:?}"
            );
            assert!(
                !follow_up_args.lines().any(|arg| arg == "--no-resume"),
                "follow-up must use the explicit server session, not disable resume: {follow_up_args:?}"
            );
        }
    }

    #[tokio::test]
    async fn empty_machine_evidence_preserves_terminal_diagnostics_but_not_identity() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let shim = tmp.path().join("fake-astra");
        crate::test_support::write_executable_shim(
            &shim,
            r#"#!/bin/sh
next_is_events=0
for arg in "$@"; do
  if [ "$next_is_events" = 1 ]; then : > "$arg"; next_is_events=0;
  elif [ "$arg" = --stream-events ]; then next_is_events=1; fi
done
printf '%s\n' '{"trace_id":null,"request_id":null,"run_id":"run-1","session_id":"550e8400-e29b-41d4-a716-446655440000","text":"ACK","final_state":"completed","interruption_kind":null,"tool_result_class_counts":{},"prompt_tokens":448,"fresh_prompt_tokens":448,"cache":{"hit":true,"read_tokens":8320,"creation_tokens":0},"completion_tokens":31,"llm_rounds":1,"tool_calls_count":0,"tools_used":[],"persistence_error":null,"exit_code":0,"success":true,"error_kind":null}'
"#,
        )
        .unwrap();
        let out = AstraCliExecutor::new(RunnerConfig::new(shim))
            .execute(&simple_case(), "m")
            .await;
        assert_eq!(out.exit_code, -1);
        assert_eq!(out.text, "ACK");
        assert_eq!(out.prompt_tokens, 448);
        assert_eq!(out.cached_input_tokens, 8320);
        assert_eq!(out.completion_tokens, 31);
        assert_eq!(out.final_state.as_deref(), Some("completed"));
        assert!(out.session_id.is_none());
        assert!(out.run_id.is_none());
        assert!(out.stderr.starts_with(PROTOCOL_ERROR_MARKER));
        assert_eq!(
            crate::classify::classify(&out, &[]),
            crate::classify::FailureClass::BehaviorContractViolation
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn canonical_capture_without_archiving_reuses_launch_context_and_checks_account_before_spawn()
     {
        let tmp = tempfile::tempdir().unwrap();
        let shim = tmp.path().join("astra-protocol-fixture");
        let session = "550e8400-e29b-41d4-a716-446655440000";
        let root = "550e8400-e29b-41d4-a716-446655440001";
        let mut capture = crate::execution_capture::tests::capture();
        capture.session_id = session.into();
        capture.run_tree.session_id = session.into();
        capture.reflection.session_id = session.into();
        let page = capture.transcript.as_mut().unwrap();
        page.session_id = session.into();
        for item in &mut page.items {
            item.session_id = session.into();
        }
        capture.run_tree.runs[0].run_id = root.into();
        for run in &mut capture.run_tree.runs {
            run.root_run_id = Some(root.into());
            if run.parent_run_id.is_some() {
                run.parent_run_id = Some(root.into());
            }
        }
        capture
            .reflection
            .model_requests
            .terminal
            .as_mut()
            .unwrap()
            .groups[0]
            .parent_run_id = Some(root.into());
        for fact in capture.reflection.graph_slice.nodes[0]
            .metadata
            .as_mut()
            .unwrap()["execution_spine"]["facts"]
            .as_array_mut()
            .unwrap()
        {
            if fact["run_id"] == "root" {
                fact["run_id"] = serde_json::json!(root);
            }
            fact["parent_run_id"] = serde_json::json!(root);
        }
        let capture_path = tmp.path().join("capture.json");
        std::fs::write(&capture_path, serde_json::to_vec(&capture).unwrap()).unwrap();
        let pending_path = tmp.path().join("pending-capture.json");
        let mut pending = capture.clone();
        pending.reflection.graph_slice.nodes[0]
            .metadata
            .as_mut()
            .unwrap()["execution_spine"]["facts"] = serde_json::json!([]);
        std::fs::write(&pending_path, serde_json::to_vec(&pending).unwrap()).unwrap();
        let bindings = [
            serde_json::json!({"type":"session_bound", "session_id":session, "owner":capture.owner}),
            serde_json::json!({"type":"run_bound", "run_id":root, "owner":capture.owner}),
        ].iter().map(|event| event.to_string()).collect::<Vec<_>>().join("\n") + "\n";
        let bindings_path = tmp.path().join("bindings.jsonl");
        std::fs::write(&bindings_path, bindings).unwrap();
        let terminal_path = tmp.path().join("terminal.json");
        std::fs::write(
            &terminal_path,
            serde_json::to_vec(&serde_json::json!({
                "trace_id":null,"request_id":null,"run_id":root,"session_id":session,
                "text":"observed-value","final_state":"completed","interruption_kind":null,
                "tool_result_class_counts":{},"prompt_tokens":1,"fresh_prompt_tokens":1,
                "cache":{"hit":false,"read_tokens":0,"creation_tokens":0},"completion_tokens":1,
                "llm_rounds":1,"tool_calls_count":0,"tools_used":[],"persistence_error":null,
                "exit_code":0,"success":true,"error_kind":null,
            }))
            .unwrap(),
        )
        .unwrap();
        let log = tmp.path().join("inspection-calls");
        crate::test_support::write_executable_shim(
            &shim,
            r#"#!/bin/sh
test "$PROBE_VALUE" = expected || exit 20
test "$PWD" = "$EXPECTED_CWD" || exit 21
test "$1" != --profile || exit 22
if [ "$1" = session ]; then
  if [ "$HANG_SECOND" = 1 ]; then
    printf 'inspection\n' >> "$PROBE_LOG"
    if [ -f "$HANG_SEEN" ]; then
      printf '%s' "$$" > "$HANG_SEEN.pid"
      sleep 60; exit 0
    fi
    : > "$HANG_SEEN"
    cat "$PENDING_CAPTURE_FILE"
    exit 0
  fi
  if [ -f "$PROBE_LOG" ]; then cat "$CAPTURE_FILE";
  else cat "$PENDING_CAPTURE_FILE"; fi
  printf 'inspection\n' >> "$PROBE_LOG"
  exit 0
fi
next=0
for arg in "$@"; do
  if [ "$next" = 1 ]; then cat "$BINDINGS_FILE" > "$arg"; next=0;
  elif [ "$arg" = --stream-events ]; then next=1; fi
done
cat "$TERMINAL_FILE"
"#,
        )
        .unwrap();
        let mut case = simple_case();
        case.criteria = vec![crate::criteria::Criterion::ExecutionChildResultsAdopted {
            children: vec![crate::criteria::ChildExecutionExpectation {
                expected_result: crate::criteria::ChildResultExpectation::Text(
                    "observed-value".into(),
                ),
                model: "test-model".into(),
                initial_thinking: None,
                answered_question: false,
                workspace_mutation: None,
                logical_rounds: None,
                slot_index: None,
            }],
            fanout_group: None,
        }];
        for (key, value) in [
            ("PROBE_VALUE", "expected".to_string()),
            (
                "EXPECTED_CWD",
                tmp.path().canonicalize().unwrap().display().to_string(),
            ),
            ("PROBE_LOG", log.display().to_string()),
            ("CAPTURE_FILE", capture_path.display().to_string()),
            ("PENDING_CAPTURE_FILE", pending_path.display().to_string()),
            ("BINDINGS_FILE", bindings_path.display().to_string()),
            ("TERMINAL_FILE", terminal_path.display().to_string()),
        ] {
            case.cli_env.insert(key.into(), value);
        }
        let mut cfg = RunnerConfig::new(shim);
        cfg.working_dir = Some(tmp.path().into());
        cfg.artifact_owner_scopes = vec![astra_services::OwnerScope::user("account").unwrap()];
        assert!(cfg.profile.is_none() && cfg.artifacts_dir.is_none());
        let outcome = AstraCliExecutor::new(cfg.clone())
            .execute(&case, "test-model")
            .await;
        assert_eq!(outcome.exit_code, 0, "{outcome:?}");
        let stream = outcome.stream_capture.as_ref().unwrap();
        assert!(stream.identity_verified);
        assert!(crate::criteria::evaluate_deterministic(&case.criteria, &outcome)[0].passed);
        case.criteria = vec![crate::criteria::Criterion::ExecutionChildResultsAdopted {
            children: vec![crate::criteria::ChildExecutionExpectation {
                expected_result: crate::criteria::ChildResultExpectation::Text(
                    "wrong-result".into(),
                ),
                model: "test-model".into(),
                initial_thinking: None,
                answered_question: false,
                workspace_mutation: None,
                logical_rounds: None,
                slot_index: None,
            }],
            fanout_group: None,
        }];
        let wrong_result = AstraCliExecutor::new(cfg.clone())
            .execute(&case, "test-model")
            .await;
        let results = crate::criteria::evaluate_deterministic(&case.criteria, &wrong_result);
        assert!(!results[0].passed);
        assert_eq!(
            crate::classify::classify(&wrong_result, &results),
            crate::classify::FailureClass::BehaviorContractViolation
        );
        let foreign_path = tmp.path().join("foreign-capture.json");
        let mut foreign = capture.clone();
        foreign.owner.account_id = "other-account".into();
        std::fs::write(&foreign_path, serde_json::to_vec(&foreign).unwrap()).unwrap();
        case.cli_env
            .insert("CAPTURE_FILE".into(), foreign_path.display().to_string());
        let foreign_result = AstraCliExecutor::new(cfg.clone())
            .execute(&case, "test-model")
            .await;
        assert!(
            foreign_result
                .stream_capture
                .as_ref()
                .unwrap()
                .execution
                .is_none()
        );
        cfg.artifact_owner_scopes =
            vec![astra_services::OwnerScope::user("other-account").unwrap()];
        let rejected = AstraCliExecutor::new(cfg.clone())
            .execute(&case, "test-model")
            .await;
        assert!(
            rejected
                .stream_capture
                .as_ref()
                .unwrap()
                .execution
                .is_none()
        );
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "inspection\ninspection\ninspection\ninspection\n",
            "pending observation retries once; wrong result and foreign owner do not retry; unauthorized account does not inspect"
        );
        cfg.artifact_owner_scopes = vec![astra_services::OwnerScope::user("account").unwrap()];
        case.cli_env
            .insert("CAPTURE_FILE".into(), pending_path.display().to_string());
        case.cli_env.insert("HANG_SECOND".into(), "1".into());
        case.cli_env.insert(
            "HANG_SEEN".into(),
            tmp.path()
                .join("hanging-observer-started")
                .display()
                .to_string(),
        );
        // Exercise the same observer with a short absolute deadline, not a
        // twenty-second wall-clock wait inside a fifteen-second test budget.
        let partial = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            crate::execution_capture::load_until(
                &cfg,
                &case,
                &outcome,
                tokio::time::Instant::now() + std::time::Duration::from_secs(3),
            ),
        )
        .await
        .expect("observation deadline must include all retries and waits")
        .expect("the last owner-bound partial capture must survive");
        let mut unresolved = outcome.clone();
        unresolved.stream_capture.as_mut().unwrap().execution = Some(partial);
        let reader_pid = std::fs::read_to_string(tmp.path().join("hanging-observer-started.pid"))
            .unwrap()
            .parse::<i32>()
            .unwrap();
        assert_eq!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(reader_pid), None),
            Err(nix::errno::Errno::ESRCH),
            "the owned inspection process must be reaped before returning"
        );
        let stream = unresolved.stream_capture.as_ref().unwrap();
        assert!(
            stream
                .execution
                .as_ref()
                .unwrap()
                .awaiting_completion_evidence(Some(root)),
            "last partial snapshot must survive the deadline"
        );
        let results = crate::criteria::evaluate_deterministic(&case.criteria, &unresolved);
        assert_eq!(
            crate::classify::classify(&unresolved, &results),
            crate::classify::FailureClass::InfraVerificationUnavailable
        );
        assert_eq!(
            std::fs::read_to_string(log).unwrap().lines().count(),
            6,
            "deadline must reap the second reader without launching a third"
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
    // `#[serial]`: leave enough startup time for the shim to publish its
    // server-issued session/run identity under parallel test load. The child
    // still sleeps for 10 seconds, so the 5-second case deadline exercises
    // timeout cancellation. Spawn-error handling remains covered by
    // `external_executor_spawn_failure_returns_-1` and related tests.
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
                "  printf '%s\\n' '{\"session_id\":\"550e8400-e29b-41d4-a716-446655440000\",\"status\":\"cancelled\",\"execution_settled\":true}'\n",
                "  exit 0\n",
                "fi\n",
                "events=; next_is_events=0\n",
                "for arg in \"$@\"; do\n",
                "  if [ \"$next_is_events\" = 1 ]; then events=$arg; next_is_events=0;\n",
                "  elif [ \"$arg\" = --stream-events ]; then next_is_events=1; fi\n",
                "done\n",
                "printf '%s\\n' '{\"type\":\"session_bound\",\"session_id\":\"550e8400-e29b-41d4-a716-446655440000\"}' > \"$events\"\n",
                "printf '%s\\n' '{\"type\":\"run_bound\",\"run_id\":\"550e8400-e29b-41d4-a716-446655440001\"}' >> \"$events\"\n",
                r#"printf '%s\n' '{"type":"explain_analyze","schema_version":1,"event_id":"e","run_id":"550e8400-e29b-41d4-a716-446655440001","turn_id":"t","node_id":"n","producer_id":"p","clock_domain_id":"c","kind":"admission","label":"Admission","transition":"started","elapsed_ms":0}' >> "$events""#,
                "\n",
                r#"printf '%s\n' '{"type":"agent_live","event":{"run_id":"child","agent_id":"agent","kind":{"type":"output_delta","model_item_id":"item","text":"partial"}}}' >> "$events""#,
                "\n",
                "sleep 10\n",
            ),
        )
        .expect("write shim");

        let mut cfg = RunnerConfig::new(shim.clone());
        cfg.artifacts_dir = Some(tmp.path().join("artifacts"));
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
            timeout_seconds: 5,
            cli_wall_time_seconds: None,
            capability: None,
            required_cache_scope: None,
            difficulty: None,
            weight: 1.0,
            steps: vec![],
            cli_env: std::collections::HashMap::new(),
            setup_cmd: None,
            teardown_cmd: None,
            cleanup_memory_records: false,
            requires_memoria: false,
        };
        let start = std::time::Instant::now();
        let outcome = exec.execute(&case, "ignored").await;
        let elapsed = start.elapsed();

        let capture = outcome
            .explain_capture
            .as_ref()
            .expect("timeout retains capture");
        assert_eq!(capture.events.len(), 1);
        assert!(capture.identity_verified);
        assert!(capture.snapshot_pending);
        assert!(capture.diagnostics.contains(&"execution_incomplete".into()));

        // `kill_on_drop` + explicit timeout capped the elapsed wall
        // time near the 5s budget. 2s slack for CI scheduling noise.
        assert!(
            elapsed.as_secs() <= 7,
            "timeout didn't kill subprocess — elapsed {}s",
            elapsed.as_secs()
        );
        let stream = outcome
            .stream_capture
            .as_ref()
            .expect("timeout retains child evidence");
        assert_eq!(stream.records.len(), 1);
        assert!(stream.identity_verified);
        assert!(stream.diagnostics.contains(&"execution_incomplete".into()));
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
    #[serial_test::serial]
    async fn user_cancel_kills_active_cli_and_settles_observed_session() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        use crate::test_support::write_executable_shim;
        let tmp = tempfile::tempdir().expect("tempdir");
        let shim = tmp.path().join("fake-astra");
        let ready = tmp.path().join("ready");
        write_executable_shim(
            &shim,
            concat!(
                "#!/bin/sh\n",
                "if [ \"$1\" = session ] && [ \"$2\" = cancel ] && [ \"$3\" = 550e8400-e29b-41d4-a716-446655440000 ]; then\n",
                "  printf '%s\\n' '{\"session_id\":\"550e8400-e29b-41d4-a716-446655440000\",\"status\":\"cancelled\",\"execution_settled\":true}'\n",
                "  exit 0\n",
                "fi\n",
                "events=; next_is_events=0\n",
                "for arg in \"$@\"; do\n",
                "  if [ \"$next_is_events\" = 1 ]; then events=$arg; next_is_events=0;\n",
                "  elif [ \"$arg\" = --stream-events ]; then next_is_events=1; fi\n",
                "done\n",
                "printf '%s\\n' '{\"type\":\"session_bound\",\"session_id\":\"550e8400-e29b-41d4-a716-446655440000\"}' > \"$events\"\n",
                "printf '%s\\n' '{\"type\":\"run_bound\",\"run_id\":\"550e8400-e29b-41d4-a716-446655440001\"}' >> \"$events\"\n",
                r#"printf '%s\n' '{"type":"agent_live","event":{"run_id":"child","agent_id":"agent","kind":{"type":"output_delta","model_item_id":"item","text":"partial"}}}' >> "$events""#,
                "\n",
                "touch \"$HARNESS_READY_PATH\"\n",
                "sleep 10\n",
            ),
        )
        .expect("write shim");

        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut cfg = RunnerConfig::new(shim);
        cfg.cancel_flag = Some(flag.clone());
        cfg.artifacts_dir = Some(tmp.path().join("artifacts"));
        let exec = AstraCliExecutor::new(cfg);
        let mut case = simple_case();
        case.timeout_seconds = 15;
        case.cli_env.insert(
            "HARNESS_READY_PATH".into(),
            ready.to_string_lossy().into_owned(),
        );
        let start = std::time::Instant::now();
        let running = tokio::spawn(async move { exec.execute(&case, "ignored").await });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !ready.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("CLI shim must publish the session binding before cancellation");
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
        let outcome = running.await.expect("executor task");
        assert!(start.elapsed().as_secs() < 7, "cancel did not reap the CLI");
        assert_eq!(outcome.exit_code, 130);
        assert_eq!(outcome.interruption_kind.as_deref(), Some("cancelled"));
        assert_eq!(outcome.final_state.as_deref(), Some("interrupted"));
        assert_eq!(
            outcome.session_id.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
        assert_eq!(
            outcome.run_id.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440001")
        );
        assert!(outcome.text.contains("observed session cancelled"));
        let stream = outcome
            .stream_capture
            .as_ref()
            .expect("cancel retains stream evidence");
        assert_eq!(stream.records.len(), 1);
        assert!(stream.identity_verified);
        assert!(stream.diagnostics.contains(&"execution_incomplete".into()));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn exited_cli_with_open_descendant_pipe_settles_timeout_and_cancel() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        use crate::test_support::write_executable_shim;
        let tmp = tempfile::tempdir().expect("tempdir");
        let shim = tmp.path().join("fake-astra");
        write_executable_shim(
            &shim,
            concat!(
                "#!/bin/sh\n",
                "if [ \"$1\" = session ] && [ \"$2\" = cancel ]; then\n",
                "  printf '%s\\n' '{\"session_id\":\"550e8400-e29b-41d4-a716-446655440000\",\"status\":\"cancelled\",\"execution_settled\":true}'\n",
                "  exit 0\n",
                "fi\n",
                "events=; next_is_events=0\n",
                "for arg in \"$@\"; do\n",
                "  if [ \"$next_is_events\" = 1 ]; then events=$arg; next_is_events=0;\n",
                "  elif [ \"$arg\" = --stream-events ]; then next_is_events=1; fi\n",
                "done\n",
                "printf '%s\\n' '{\"type\":\"session_bound\",\"session_id\":\"550e8400-e29b-41d4-a716-446655440000\"}' > \"$events\"\n",
                "sleep 5 &\n",
                "if [ -n \"$HARNESS_READY_PATH\" ]; then touch \"$HARNESS_READY_PATH\"; fi\n",
                "exit 0\n",
            ),
        )
        .expect("write shim");
        let exec = AstraCliExecutor::new(RunnerConfig::new(shim.clone()));
        let mut case = simple_case();
        case.timeout_seconds = 2;
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(4),
            exec.execute(&case, "ignored"),
        )
        .await
        .expect("case watchdog must cover stdout/stderr drain after CLI exit");
        assert_eq!(outcome.exit_code, 124);
        assert_eq!(outcome.interruption_kind.as_deref(), Some("timeout"));
        assert!(
            outcome
                .text
                .contains("timeout while draining subprocess evidence")
        );
        assert!(outcome.text.contains("observed session cancelled"));

        let ready = tmp.path().join("drain-ready");
        case.timeout_seconds = 10;
        case.cli_env.insert(
            "HARNESS_READY_PATH".into(),
            ready.to_string_lossy().into_owned(),
        );
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut cfg = RunnerConfig::new(shim);
        cfg.cancel_flag = Some(flag.clone());
        let exec = AstraCliExecutor::new(cfg);
        let running = tokio::spawn(async move { exec.execute(&case, "ignored").await });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !ready.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("CLI shim must enter the open-pipe drain window");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
        let cancelled = tokio::time::timeout(std::time::Duration::from_secs(4), running)
            .await
            .expect("cancel must interrupt pipe drain promptly")
            .expect("executor task");
        assert_eq!(cancelled.exit_code, 130);
        assert_eq!(cancelled.interruption_kind.as_deref(), Some("cancelled"));
        assert!(cancelled.text.contains("observed session cancelled"));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn one_open_pipe_does_not_repoll_completed_reader_on_timeout_or_cancel() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        use crate::test_support::write_executable_shim;
        let tmp = tempfile::tempdir().expect("tempdir");
        for (pipe, child_command) in [
            ("stdout", "sleep 5 2>/dev/null &"),
            ("stderr", "sleep 5 >/dev/null &"),
        ] {
            let shim = tmp.path().join(format!("fake-astra-{pipe}"));
            let script = format!(
                "#!/bin/sh\n\
                 if [ \"$1\" = session ] && [ \"$2\" = cancel ]; then\n\
                   printf '%s\\n' '{{\"session_id\":\"550e8400-e29b-41d4-a716-446655440000\",\"status\":\"cancelled\",\"execution_settled\":true}}'\n\
                   exit 0\n\
                 fi\n\
                 events=; next_is_events=0\n\
                 for arg in \"$@\"; do\n\
                   if [ \"$next_is_events\" = 1 ]; then events=$arg; next_is_events=0;\n\
                   elif [ \"$arg\" = --stream-events ]; then next_is_events=1; fi\n\
                 done\n\
                 printf '%s\\n' '{{\"type\":\"session_bound\",\"session_id\":\"550e8400-e29b-41d4-a716-446655440000\"}}' > \"$events\"\n\
                 {child_command}\n\
                 if [ -n \"$HARNESS_READY_PATH\" ]; then touch \"$HARNESS_READY_PATH\"; fi\n\
                 exit 0\n"
            );
            write_executable_shim(&shim, &script).expect("write shim");

            let mut case = simple_case();
            case.timeout_seconds = 2;
            let timed_out = tokio::time::timeout(
                std::time::Duration::from_secs(4),
                AstraCliExecutor::new(RunnerConfig::new(shim.clone())).execute(&case, "ignored"),
            )
            .await
            .expect("case deadline must settle open pipe");
            assert_eq!(timed_out.exit_code, 124, "{pipe}: {}", timed_out.text);
            assert!(timed_out.text.contains("observed session cancelled"));

            let ready = tmp.path().join(format!("ready-{pipe}"));
            case.timeout_seconds = 10;
            case.cli_env.insert(
                "HARNESS_READY_PATH".into(),
                ready.to_string_lossy().into_owned(),
            );
            let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let mut cfg = RunnerConfig::new(shim);
            cfg.cancel_flag = Some(flag.clone());
            let running =
                tokio::spawn(
                    async move { AstraCliExecutor::new(cfg).execute(&case, "ignored").await },
                );
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while !ready.exists() {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("shim must enter pipe drain");
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            let cancelled = tokio::time::timeout(std::time::Duration::from_secs(4), running)
                .await
                .expect("cancel must settle open pipe")
                .expect("executor task");
            assert_eq!(cancelled.exit_code, 130, "{pipe}: {}", cancelled.text);
            assert!(cancelled.text.contains("observed session cancelled"));
        }
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
            token_usage_coverage: None,
            duration_ms: 0,
            turn_rounds: 0,
            cache_hits: 0,
            total_tool_calls: 0,
            ttft_ms: 0,
            final_state: None,
            interruption_kind: None,
            error_kind: None,
            explain_capture: None,
            stream_capture: None,
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
            cli_wall_time_seconds: None,
            capability: None,
            required_cache_scope: None,
            difficulty: None,
            weight: 1.0,
            steps: vec![],
            cli_env: std::collections::HashMap::new(),
            setup_cmd: None,
            teardown_cmd: None,
            cleanup_memory_records: false,
            requires_memoria: false,
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
            cli_wall_time_seconds: None,
            capability: None,
            required_cache_scope: None,
            difficulty: None,
            weight: 1.0,
            steps: vec![],
            cli_env: std::collections::HashMap::new(),
            setup_cmd: None,
            teardown_cmd: None,
            cleanup_memory_records: false,
            requires_memoria: false,
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

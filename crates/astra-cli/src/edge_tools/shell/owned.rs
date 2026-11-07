//! One shell invocation, independent of the foreground presentation lifetime.
use super::*;
use astra_sandbox::{ProcessEnd, ProcessIoLimits, ProcessOutput};
use std::sync::Arc;

// Keep this presentation projection bounded and derived from canonical facts;
// output truncation and capture failure must not erase execution acknowledgements.
fn terminal_facts(outcome: &super::super::ToolExecutionOutcome) -> String {
    let fields = outcome.tool_result_fields.as_ref();
    let terminal: serde_json::Map<String, Value> = [
        "execution_started",
        "execution_fact",
        "exit_code",
        "result_class",
        "error_kind",
        "stage",
        "cancelled",
        "timed_out",
        "side_effects_maybe",
        "retryable",
        "cleanup_authoritative",
        "background_children_reaped",
    ]
    .into_iter()
    .filter_map(|key| fields?.get(key).map(|value| (key.into(), value.clone())))
    .collect();
    format!("[bash terminal facts: {}]", Value::Object(terminal))
}

pub(super) struct OwnedShellInvocation {
    pub(super) config: ShellRunConfig,
    pub(super) scope: Arc<ShellScopeLease>,
    pub(super) capture: BashOutputCapture,
    pub(super) stdout: astra_tools::shell_output_capture::CaptureWriter,
    pub(super) stderr: astra_tools::shell_output_capture::CaptureWriter,
    pub(super) renderer: BashOutputRenderer,
    pub(super) args: Value,
    pub(super) source_preimages: Option<astra_tools::source_preimage::PreparedSourcePreimages>,
    pub(super) workdir: astra_tools::shell_ops::PreparedBashWorkdir,
    pub(super) output_limit: usize,
    pub(super) retain_for_task: bool,
}

impl OwnedShellInvocation {
    pub(super) async fn run(
        mut self,
        cancel: tokio_util::sync::CancellationToken,
        input: Option<tokio::sync::watch::Sender<Option<astra_sandbox::ProcessInput>>>,
    ) -> super::super::ToolExecutionOutcome {
        self.config.cancel_token = Some(cancel.clone());
        if cancel.is_cancelled() {
            return super::super::cancelled_tool_execution_outcome("bash", false);
        }
        if self
            .scope
            .observation
            .as_ref()
            .is_some_and(|lease| !lease.coordination_integrity_valid())
        {
            return bash_preparation_rejection(
                "Workspace changed before execution; no command was run.".into(),
            );
        }
        let (command, owner) = match prepare_shell_process(&self.config) {
            Ok(prepared) => prepared,
            Err(error) => {
                if cancel.is_cancelled() {
                    return super::super::cancelled_tool_execution_outcome("bash", false);
                }
                return bash_preparation_rejection(error.message);
            }
        };
        let interactive = self
            .args
            .get("stdin")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut process = match owner.spawn_streamed(
            command,
            ProcessIoLimits {
                max_frame_bytes: 32 * 1024,
                max_queued_frames: 4,
                max_stderr_bytes: 0,
                timeout: Some(Duration::from_secs_f64(self.config.timeout_secs)),
            },
            cancel,
            interactive,
            self.scope.clone(),
        ) {
            Ok(process) => process,
            Err(error) => {
                return bash_preparation_rejection(format!("Cannot start command: {error}"));
            }
        };
        if let (Some(input), Some(process_input)) = (input, process.input()) {
            input.send_replace(Some(process_input));
        }
        let preview_limit = self.output_limit.saturating_mul(2).max(16_384);
        let (mut stdout_preview, mut stderr_preview) = (Vec::new(), Vec::new());
        let mut capture_failed = false;
        let mut streams_complete = false;
        #[cfg(all(test, target_os = "linux"))]
        let mut test_stdout = self
            .config
            .supervisor_test_helper
            .as_ref()
            .map(|_| Vec::new());
        while let Some(output) = process.recv_output().await {
            #[cfg(all(test, target_os = "linux"))]
            let mut output = output;
            #[cfg(all(test, target_os = "linux"))]
            if let (ProcessOutput::Stdout(bytes), Some(buffer)) = (&mut output, &mut test_stdout) {
                buffer.append(bytes);
                if !strip_supervisor_test_prelude(buffer, false) {
                    continue;
                }
                *bytes = std::mem::take(buffer);
                test_stdout = None;
            }
            let (bytes, preview, writer) = match output {
                ProcessOutput::Stdout(bytes) => (bytes, &mut stdout_preview, &mut self.stdout),
                ProcessOutput::Stderr(bytes) => (bytes, &mut stderr_preview, &mut self.stderr),
            };
            if let Some(sink) = &self.config.progress_sink {
                sink.record_chunk(&bytes);
            }
            let keep = bytes.len().min(preview_limit.saturating_sub(preview.len()));
            preview.extend_from_slice(&bytes[..keep]);
            if !capture_failed && writer.write(&bytes).await.is_err() {
                capture_failed = true;
                process.cancel();
            }
        }
        #[cfg(all(test, target_os = "linux"))]
        if let Some(bytes) = test_stdout.filter(|bytes| !bytes.is_empty()) {
            if let Some(sink) = &self.config.progress_sink {
                sink.record_chunk(&bytes);
            }
            let keep = bytes
                .len()
                .min(preview_limit.saturating_sub(stdout_preview.len()));
            stdout_preview.extend_from_slice(&bytes[..keep]);
            if !capture_failed && self.stdout.write(&bytes).await.is_err() {
                capture_failed = true;
                process.cancel();
            }
        }
        let terminal = process.wait().await;
        capture_failed |= self.stdout.finish().await.is_err();
        capture_failed |= self.stderr.finish().await.is_err();
        // A cut credential cannot be safely redacted from a partial line.
        for preview in [&mut stdout_preview, &mut stderr_preview] {
            if preview.len() == preview_limit
                || !terminal
                    .as_ref()
                    .is_ok_and(|outcome| matches!(outcome.end, ProcessEnd::Exited))
            {
                let end = preview
                    .iter()
                    .rposition(|byte| *byte == b'\n')
                    .map_or(0, |index| index + 1);
                preview.truncate(end);
            }
        }
        // A text result cannot represent arbitrary bytes faithfully, even
        // when the stream is small enough to fit inline.
        let non_utf8_output = std::str::from_utf8(&stdout_preview).is_err()
            || std::str::from_utf8(&stderr_preview).is_err();
        let whitespace_only_output = (!stdout_preview.is_empty() || !stderr_preview.is_empty())
            && String::from_utf8_lossy(&stdout_preview).trim().is_empty()
            && String::from_utf8_lossy(&stderr_preview).trim().is_empty();
        let (mut outcome, ownership, execution_started, rendered_truncated) = match terminal {
            Ok(terminal) => {
                // The driver proves full pipe drain only on natural exit.
                // Cancellation/timeout retains partial bytes without claiming
                // they are the complete output of an interrupted process.
                streams_complete = matches!(terminal.end, ProcessEnd::Exited);
                capture_failed |= matches!(
                    terminal.end,
                    ProcessEnd::OutputFailed(_)
                        | ProcessEnd::StderrFailed(_)
                        | ProcessEnd::OutputDrainTimedOut
                );
                let ownership = terminal.settlement.map(|settlement| settlement.ownership);
                let started = terminal.target_released != Some(false);
                let exit_code = terminal.status.and_then(|status| status.code());
                let cancelled = matches!(terminal.end, ProcessEnd::Cancelled);
                let timed_out = matches!(terminal.end, ProcessEnd::TimedOut);
                let (mut outcome, truncated) = if let (ProcessEnd::Exited, Some(status)) =
                    (&terminal.end, terminal.status)
                {
                    self.renderer.render_bash_outcome(
                        &self.config.command,
                        std::process::Output {
                            status,
                            stdout: stdout_preview,
                            stderr: stderr_preview,
                        },
                        terminal
                            .settlement
                            .is_some_and(|settlement| settlement.descendants_terminated),
                    )
                } else {
                    let (kind, stage, detail) = match terminal.end {
                        ProcessEnd::Cancelled if !capture_failed => (
                            astra_core::ErrorKind::Cancelled,
                            "cancellation",
                            "Command cancelled".to_string(),
                        ),
                        ProcessEnd::TimedOut => (
                            astra_core::ErrorKind::ToolTimeout,
                            "execution",
                            format!("Command timed out after {}s", self.config.timeout_secs),
                        ),
                        ProcessEnd::OwnershipFailed(error) => (
                            astra_core::ErrorKind::ToolOutcomeUnknown,
                            "ownership",
                            error.to_string(),
                        ),
                        ProcessEnd::WaitFailed(error) => (
                            astra_core::ErrorKind::ToolOutcomeUnknown,
                            "wait",
                            error.to_string(),
                        ),
                        ProcessEnd::InputFailed(error) => (
                            astra_core::ErrorKind::ToolUnavailable,
                            "input",
                            error.to_string(),
                        ),
                        ProcessEnd::OutputFailed(error) | ProcessEnd::StderrFailed(error) => (
                            astra_core::ErrorKind::ToolUnavailable,
                            "output",
                            error.to_string(),
                        ),
                        ProcessEnd::OutputDrainTimedOut => (
                            astra_core::ErrorKind::ToolUnavailable,
                            "output",
                            "Output did not finish draining after process exit".into(),
                        ),
                        _ => (
                            astra_core::ErrorKind::ToolUnavailable,
                            "output",
                            "Output capture did not complete".into(),
                        ),
                    };
                    // A physical command may already have changed user state.
                    // Generic transport retry advice cannot authorize replay.
                    let mut outcome = super::super::ToolExecutionOutcome::error_with_evidence(
                        detail,
                        astra_core::ToolFailureEvidence::new(
                            kind,
                            if timed_out {
                                astra_core::ToolFailureCause::CommandFailed
                            } else {
                                astra_core::ToolFailureEvidence::from_error_kind(kind).cause
                            },
                            false,
                            vec![astra_core::ToolRecoveryAction::InspectStructuredFailure],
                        ),
                    );
                    for (label, bytes) in [("stdout", &stdout_preview), ("stderr", &stderr_preview)]
                    {
                        if !bytes.is_empty() {
                            let text = String::from_utf8_lossy(bytes);
                            let (safe, _) = astra_text_utils::credential_redaction::redact_credentials_for_display(&text);
                            outcome.output.push_str(&format!("\n{label}:\n{safe}"));
                        }
                    }
                    let fields = outcome.tool_result_fields.get_or_insert_default();
                    fields.insert("stage".into(), json!(stage));
                    if started {
                        fields.insert("disposition".into(), json!("executed"));
                        astra_tools::execution_outcome::insert_started_execution_fact(fields);
                    } else {
                        astra_tools::execution_outcome::insert_not_executed_fact(fields);
                    }
                    if terminal.target_released.is_none() || (started && ownership.is_none()) {
                        if terminal.target_released.is_none() {
                            fields.remove("execution_started");
                        }
                        fields.insert("execution_fact".into(), json!("unknown"));
                        fields.insert("side_effects_maybe".into(), json!(true));
                    }
                    fields.insert("retryable".into(), json!(false));
                    (outcome, false)
                };
                let fields = outcome.tool_result_fields.get_or_insert_default();
                if cancelled {
                    fields.insert("cancelled".into(), json!(true));
                }
                if timed_out {
                    fields.insert("timed_out".into(), json!(true));
                }
                if let Some(code) = exit_code {
                    fields.insert("exit_code".into(), json!(code));
                }
                fields.insert(
                    "cleanup_authoritative".into(),
                    json!(ownership.is_some_and(|owner| owner.is_authoritative())),
                );
                (outcome, ownership, started, truncated)
            }
            Err(error) => (
                super::super::ToolExecutionOutcome::error_with_evidence(
                    format!("Process owner failed: {error}"),
                    astra_core::ToolFailureEvidence::from_error_kind(
                        astra_core::ErrorKind::ToolOutcomeUnknown,
                    ),
                ),
                None,
                true,
                false,
            ),
        };
        let unsettled = execution_started
            && (ownership.is_none()
                || self
                    .scope
                    .observation
                    .as_ref()
                    .is_some_and(|lease| !lease.coordination_integrity_valid()));
        outcome = finish_bash_ownership(
            &self.config.effective_project_root,
            outcome,
            unsettled,
            self.scope.observation.as_ref(),
        );
        outcome = attach_bash_workdir_outcome(
            attach_source_preimage_outcome(outcome, self.source_preimages),
            &self.config.effective_project_root,
            &self.workdir,
            &self.args,
        );
        if capture_failed {
            mark_bash_capture_unavailable(&mut outcome);
        }
        let publish = !streams_complete
            || capture_failed
            || rendered_truncated
            || non_utf8_output
            || whitespace_only_output
            || self.capture.captured_bytes()
                > self
                    .output_limit
                    .min(astra_core::runtime_limits::DEFAULT_TOOL_RESULT_INLINE_CHARS)
                    as u64
            || outcome.output.len() > astra_core::runtime_limits::DEFAULT_TOOL_RESULT_INLINE_CHARS;
        if publish {
            match self
                .capture
                .publish_to_workspace(
                    &self.config.effective_project_root,
                    streams_complete && !capture_failed,
                )
                .await
            {
                Ok(receipt) => {
                    if !receipt.complete {
                        mark_bash_capture_unavailable(&mut outcome);
                    }
                    outcome.output = receipt.present(
                        &outcome.output,
                        &terminal_facts(&outcome),
                        self.output_limit,
                    );
                    outcome
                        .tool_result_fields
                        .get_or_insert_default()
                        .extend(receipt.result_fields());
                }
                Err(error) => {
                    mark_bash_capture_unavailable(&mut outcome);
                    outcome.tool_result_fields.get_or_insert_default().insert(
                        "bash_output_capture".into(),
                        json!({"complete":false,"failure":error}),
                    );
                    outcome.output = format!(
                        "[bash output capture failed: {error}; no complete output file]\n{}\n\n{}",
                        terminal_facts(&outcome),
                        outcome.output,
                    );
                }
            }
        }
        if self.retain_for_task {
            self.capture.retain_for_task();
        }
        outcome
    }
}

impl ToolExecutor {
    pub(super) async fn wait_for_shell_task(
        &self,
        mut task: super::super::background_tasks::ShellTaskSubscription,
        args: &Value,
        cancellation: Option<&tokio_util::sync::CancellationToken>,
        nested: bool,
    ) -> super::super::ToolExecutionOutcome {
        let mut cancel_on_drop = CancelShellOnDrop(Some(task.cancel.clone()));
        let can_release = self.background_tasks_shared && !nested;
        let mut foreground = can_release.then(|| self.foreground_wait_control.begin_wait());
        let wait_ms = args
            .get("yield_time_ms")
            .and_then(Value::as_u64)
            .unwrap_or(10_000)
            .min(30_000);
        let deadline =
            can_release.then(|| tokio::time::Instant::now() + Duration::from_millis(wait_ms));
        let result = tokio::select! {
            biased;
            outcome = task.wait() => Some(outcome),
            _ = async { match cancellation { Some(cancel) => cancel.cancelled().await, None => std::future::pending().await } } => {
                task.cancel.cancel();
                Some(task.wait().await)
            }
            _ = async { match foreground.as_mut() { Some(wait) => wait.released().await, None => std::future::pending().await } } => None,
            _ = async { match deadline { Some(deadline) => tokio::time::sleep_until(deadline).await, None => std::future::pending().await } } => None,
        };
        if let Some(result) = result {
            return result
                .map(|outcome| (*outcome).clone())
                .unwrap_or_else(|error| {
                    super::super::ToolExecutionOutcome::error(error.to_string())
                });
        }
        if cancellation.is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
            task.cancel.cancel();
            return task
                .wait()
                .await
                .map(|outcome| (*outcome).clone())
                .unwrap_or_else(|error| {
                    super::super::ToolExecutionOutcome::error(error.to_string())
                });
        }
        let mut registry = self.background_tasks.clone();
        match registry.promote(&task.id) {
            Ok(true) => {
                cancel_on_drop.0 = None;
                let mut fields = serde_json::Map::new();
                fields.insert("bash_detached".into(), json!(true));
                fields.insert("background_task_id".into(), json!(&task.id));
                fields.insert("task_id".into(), json!(&task.id));
                if let Some(context) = registry.model_context(&task.id) {
                    fields.insert("task".into(), context);
                }
                let wake = if registry.has_event_delivery() {
                    WorkUnitWakePolicy::OnAttentionOrTerminal
                } else {
                    WorkUnitWakePolicy::None
                };
                WorkUnitObservation::new(
                    &task.id,
                    "shell",
                    WorkUnitStatus::Running,
                    1,
                    WorkUnitObservationMode::Transition,
                )
                .expect("owned task identity")
                .with_wake_policy(wake)
                .insert_into(&mut fields);
                super::super::ToolExecutionOutcome {
                    output: render_bash_detached_marker(&task.id),
                    tool_result_fields: Some(fields),
                    is_error: false,
                }
            }
            Ok(false) => task
                .wait()
                .await
                .map(|outcome| (*outcome).clone())
                .unwrap_or_else(|error| {
                    super::super::ToolExecutionOutcome::error(error.to_string())
                }),
            Err(error) => super::super::ToolExecutionOutcome::error(error.to_string()),
        }
    }
}

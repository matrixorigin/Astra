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
        "cleanup_unsettled",
        "background_children_reaped",
    ]
    .into_iter()
    .filter_map(|key| fields?.get(key).map(|value| (key.into(), value.clone())))
    .collect();
    format!("[bash terminal facts: {}]", Value::Object(terminal))
}

pub(super) struct OwnedShellInvocation {
    pub(super) config: ShellRunConfig,
    pub(super) workspace: Arc<Option<tokio::sync::OwnedRwLockReadGuard<()>>>,
    pub(super) capture: Option<(
        BashOutputCapture,
        astra_tools::shell_output_capture::CaptureWriter,
        astra_tools::shell_output_capture::CaptureWriter,
    )>,
    pub(super) renderer: BashOutputRenderer,
    pub(super) args: Value,
    pub(super) workdir: astra_tools::shell_ops::PreparedBashWorkdir,
    pub(super) output_limit: usize,
    pub(super) retain_for_task: bool,
    pub(super) task_output_available: bool,
    pub(super) admission_deadline: Option<std::time::Instant>,
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
        if let Some(outcome) = bash_admission_deadline_outcome(self.admission_deadline) {
            return outcome;
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
        #[cfg(unix)]
        {
            let inspection = self.workdir.inspection();
            let root_matches = self
                .config
                .effective_project_root
                .canonicalize()
                .ok()
                .is_some_and(|root| inspection.matches_root_path(&root).unwrap_or(false));
            if !root_matches || inspection.working_directory_relative().is_err() {
                return bash_preparation_rejection(
                    "Error: the selected workspace or working directory changed before execution; no command was run".into(),
                );
            }
        }
        if let Some(outcome) = bash_admission_deadline_outcome(self.admission_deadline) {
            return outcome;
        }
        let mut process = match owner.spawn_streamed(
            command,
            ProcessIoLimits {
                max_frame_bytes: 32 * 1024,
                max_queued_frames: 4,
                max_stderr_bytes: 0,
                timeout: Some(Duration::from_secs_f64(self.config.timeout_secs)),
            },
            cancel.clone(),
            interactive,
            self.workspace.clone(),
        ) {
            Ok(process) => process,
            Err(error) => {
                if cancel.is_cancelled() {
                    return super::super::cancelled_tool_execution_outcome("bash", false);
                }
                return bash_preparation_rejection(format!("Cannot start command: {error}"));
            }
        };
        if let (Some(input), Some(process_input)) = (input, process.input()) {
            input.send_replace(Some(process_input));
        }
        let preview_limit = self.output_limit.saturating_mul(2).max(16_384);
        let retain_tail = self.capture.is_none();
        let (mut stdout_preview, mut stderr_preview) = (
            astra_text_utils::live_redaction::OutputPreview::new(preview_limit, retain_tail),
            astra_text_utils::live_redaction::OutputPreview::new(preview_limit, retain_tail),
        );
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
            let (bytes, preview, stdout_stream) = match output {
                ProcessOutput::Stdout(bytes) => (bytes, &mut stdout_preview, true),
                ProcessOutput::Stderr(bytes) => (bytes, &mut stderr_preview, false),
            };
            if let Some(sink) = &self.config.progress_sink {
                sink.record_chunk(&bytes);
            }
            preview.append(&bytes);
            if let Some((_, stdout, stderr)) = self.capture.as_mut() {
                let writer = if stdout_stream { stdout } else { stderr };
                capture_failed |= writer.write(&bytes).await.is_err();
            }
        }
        #[cfg(all(test, target_os = "linux"))]
        if let Some(bytes) = test_stdout.filter(|bytes| !bytes.is_empty()) {
            if let Some(sink) = &self.config.progress_sink {
                sink.record_chunk(&bytes);
            }
            stdout_preview.append(&bytes);
            if let Some((_, stdout, _)) = self.capture.as_mut() {
                capture_failed |= stdout.write(&bytes).await.is_err();
            }
        }
        let terminal = process.wait().await;
        let capture = if let Some((capture, stdout, stderr)) = self.capture.take() {
            capture_failed |= stdout.finish().await.is_err();
            capture_failed |= stderr.finish().await.is_err();
            Some(capture)
        } else {
            None
        };
        let incomplete = !terminal.as_ref().is_ok_and(|outcome| {
            matches!(outcome.end, ProcessEnd::Exited)
                && outcome.status.is_some_and(|status| {
                    status.code().is_some_and(|code| {
                        !matches!(
                            astra_tools::exit_semantics::classify_exit(&self.config.command, code),
                            astra_tools::exit_semantics::ExitSemantics::Signaled
                                | astra_tools::exit_semantics::ExitSemantics::PipelineTruncated
                        )
                    })
                })
        });
        let (mut stdout_preview, stdout_capped, stdout_non_utf8) =
            stdout_preview.finish(incomplete);
        let (mut stderr_preview, stderr_capped, stderr_non_utf8) =
            stderr_preview.finish(incomplete);
        // A text result cannot represent arbitrary bytes faithfully, even
        // when the stream is small enough to fit inline.
        let non_utf8_output = stdout_non_utf8
            || stderr_non_utf8
            || std::str::from_utf8(&stdout_preview).is_err()
            || std::str::from_utf8(&stderr_preview).is_err()
            || (retain_tail
                && (stdout_capped || stderr_capped)
                && (String::from_utf8_lossy(&stdout_preview).contains('\u{fffd}')
                    || String::from_utf8_lossy(&stderr_preview).contains('\u{fffd}')));
        let display_budget = self
            .output_limit
            .min(astra_core::runtime_limits::DEFAULT_TOOL_RESULT_INLINE_CHARS / 2)
            .saturating_sub(1024);
        let presentation_truncated = retain_tail
            && (stdout_capped
                || stderr_capped
                || stdout_preview.len().saturating_add(stderr_preview.len()) > display_budget);
        if presentation_truncated {
            // Bound each stream separately before combining them so a long
            // stdout cannot hide a later stderr diagnostic, or vice versa.
            let budget = display_budget / 2;
            for preview in [&mut stdout_preview, &mut stderr_preview] {
                let text = String::from_utf8_lossy(preview);
                let safe =
                    astra_text_utils::credential_redaction::redact_credentials_for_display(&text).0;
                *preview = astra_text_utils::credential_redaction::truncate_redacted_head_tail(
                    &safe, budget,
                )
                .into_bytes();
            }
        }
        let whitespace_only_output = (!stdout_preview.is_empty() || !stderr_preview.is_empty())
            && String::from_utf8_lossy(&stdout_preview).trim().is_empty()
            && String::from_utf8_lossy(&stderr_preview).trim().is_empty();
        let (mut outcome, ownership, execution_started, rendered_truncated) = match terminal {
            Ok(terminal) => {
                // The driver proves full pipe drain only on natural exit.
                // Cancellation/timeout retains partial bytes without claiming
                // they are the complete output of an interrupted process.
                streams_complete = !incomplete;
                capture_failed |= matches!(
                    terminal.end,
                    ProcessEnd::OutputFailed(_)
                        | ProcessEnd::StderrFailed(_)
                        | ProcessEnd::OutputDrainTimedOut
                );
                let ownership = terminal.settlement.map(|settlement| settlement.ownership);
                let started = terminal.target_released != Some(false);
                let exit_code = terminal
                    .status
                    .map(|status| astra_tools::shell_ops::exit_code_from_status(&status));
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
                        ProcessEnd::Cancelled => (
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
                            fields.remove("disposition");
                        }
                        astra_tools::execution_outcome::insert_unknown_execution_fact(fields);
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
        let unsettled = execution_started && ownership.is_none();
        outcome = finish_bash_ownership(outcome, unsettled);
        outcome = attach_bash_workdir_outcome(
            outcome,
            &self.config.effective_project_root,
            &self.workdir,
            &self.args,
        );
        if capture_failed {
            mark_bash_capture_unavailable(&mut outcome);
        }
        let capture_truncated = capture.as_ref().is_some_and(BashOutputCapture::truncated);
        let needs_more_output = !streams_complete
            || capture_failed
            || capture_truncated
            || presentation_truncated
            || stdout_capped
            || stderr_capped
            || rendered_truncated
            || non_utf8_output
            || whitespace_only_output
            || capture.as_ref().is_some_and(|capture| {
                capture.captured_bytes()
                    > self
                        .output_limit
                        .min(astra_core::runtime_limits::DEFAULT_TOOL_RESULT_INLINE_CHARS)
                        as u64
            })
            || outcome.output.len() > astra_core::runtime_limits::DEFAULT_TOOL_RESULT_INLINE_CHARS;
        if needs_more_output {
            let fields = outcome.tool_result_fields.get_or_insert_default();
            if capture.is_some() {
                fields.insert(
                    "bash_output_capture".into(),
                    json!({"complete": streams_complete && !capture_failed && !capture_truncated}),
                );
            }
            if self.task_output_available {
                fields.insert("task_output_retained".into(), json!(true));
            }
            let note = if capture_failed {
                "Output capture failed."
            } else if capture_truncated {
                "Captured output reached its 50 MiB limit."
            } else if !streams_complete {
                "Command output is incomplete."
            } else {
                "Command output is truncated."
            };
            let preview = astra_text_utils::credential_redaction::truncate_redacted_head_tail(
                &outcome.output,
                self.output_limit
                    .min(astra_core::runtime_limits::DEFAULT_TOOL_RESULT_INLINE_CHARS / 2),
            );
            outcome.output = format!("[{note}]\n{}\n\n{preview}", terminal_facts(&outcome));
            if !self.task_output_available {
                outcome
                    .output
                    .insert_str(0, "[additional output is unavailable]\n");
            }
            if non_utf8_output {
                outcome.output.insert_str(
                    0,
                    "[Output may contain non-UTF-8 bytes; text display may be lossy.]\n",
                );
            }
        }
        if self.retain_for_task
            && let Some(capture) = capture
        {
            capture.retain_for_task();
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
        can_release: bool,
    ) -> super::super::ToolExecutionOutcome {
        let mut cancel_on_drop = CancelShellOnDrop {
            cancel: Some(task.cancel.clone()),
            registry: self.background_tasks.clone(),
            task_id: task.id.clone(),
        };
        let mut foreground = can_release.then(|| self.foreground_wait_control.begin_wait());
        let deadline = args
            .get("yield_time_ms")
            .and_then(Value::as_u64)
            .or_else(|| (args.get("stdin").and_then(Value::as_bool) == Some(true)).then_some(0))
            .filter(|_| can_release)
            .map(|wait_ms| {
                tokio::time::Instant::now() + Duration::from_millis(wait_ms.min(30_000))
            });
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
            cancel_on_drop.cancel = None;
            return delivered_shell_outcome(&self.background_tasks, &task, result);
        }
        if cancellation.is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
            task.cancel.cancel();
            let result = task.wait().await;
            cancel_on_drop.cancel = None;
            return delivered_shell_outcome(&self.background_tasks, &task, result);
        }
        let mut registry = self.background_tasks.clone();
        match registry.promote(&task.id) {
            Ok(true) => {
                cancel_on_drop.cancel = None;
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
            Ok(false) => {
                let result = task.wait().await;
                cancel_on_drop.cancel = None;
                delivered_shell_outcome(&registry, &task, result)
            }
            Err(error) => super::super::ToolExecutionOutcome::error(error.to_string()),
        }
    }
}

fn delivered_shell_outcome(
    registry: &super::super::background_tasks::BackgroundTaskRegistry,
    task: &super::super::background_tasks::ShellTaskSubscription,
    result: Result<
        Arc<astra_tools::execution_outcome::ToolExecutionOutcome>,
        crate::background_task_error::BackgroundTaskError,
    >,
) -> super::super::ToolExecutionOutcome {
    match result {
        Ok(outcome) => {
            let mut delivered = (*outcome).clone();
            if outcome.tool_result_fields.as_ref().is_some_and(|fields| {
                fields.get("task_output_retained").and_then(Value::as_bool) == Some(true)
            }) {
                if registry.retain_completed_output(&task.id) {
                    delivered.output = format!(
                        "[Captured output: task_output(task_id=\"{}\") while retained; output may expire or be incomplete.]\n{}",
                        task.id, delivered.output
                    );
                    delivered
                        .tool_result_fields
                        .get_or_insert_default()
                        .insert("task_id".into(), json!(task.id));
                } else {
                    delivered
                        .output
                        .insert_str(0, "[additional output is unavailable]\n");
                    delivered
                        .tool_result_fields
                        .get_or_insert_default()
                        .remove("task_output_retained");
                }
            }
            if astra_tools::execution_outcome::outcome_is_unknown(
                outcome.tool_result_fields.as_ref(),
            ) {
                // Until the result reaches its consumer, retain an
                // inspectable owner for effects that may have occurred.
                registry.abandon_foreground(&task.id);
            } else {
                registry.acknowledge_terminal(&task.id);
            }
            delivered
        }
        Err(error) => super::super::ToolExecutionOutcome::error(error.to_string()),
    }
}

//! Plan execution progress rendering and blocking monitor loop.

use crate::chat_stream;
use crate::cli_formatting;
use crate::durable_bridge;
use crate::effects;
use crate::plan_executor;
use crate::repl_state::ReplState;
use crate::stream_render;
use crate::streaming_md;
use crate::theme;
use crossterm::style::Stylize;

/// Shown after Ctrl+C pauses plan auto-execution (interrupt is not sent to the model).
fn eprint_plan_execution_paused_hints() {
    eprintln!("{}", "  What you can do:".dim());
    eprintln!(
        "    {}",
        "continue · resume · next · go · 继续 — resume execution from this point".dim()
    );
    eprintln!(
        "    {}",
        "Lines starting with / — run a slash command; the paused plan stays in memory".dim()
    );
    eprintln!(
        "    {}",
        "Any other message — abandons the plan and sends it as a normal chat turn".dim()
    );
    eprintln!(
        "    {}",
        "Step-by-step mode: at \"Execute this subtask?\", use skip to defer one subtask".dim()
    );
    eprintln!(
        "    {}",
        "correct … / note … / adjust … — stack guidance for upcoming subtasks (correct clear to drop)"
            .dim()
    );
    eprintln!(
        "    {}",
        "rewind N · restart N · redo from N — reset step N and all later steps (1-based list order)"
            .dim()
    );
    eprintln!(
        "    {}",
        "rewind <id-prefix> — same, anchored by subtask id (prefix must match exactly one id)"
            .dim()
    );
}

/// Format a progress bar line for plan execution.
///
/// Example: `[████████░░░░] 3/7 (42%) · ~2m14s remaining`
pub(crate) fn format_plan_progress(
    done: usize,
    total: usize,
    avg_duration: Option<std::time::Duration>,
    elapsed: std::time::Duration,
) -> String {
    let bar_width = 16;
    let pct = if total > 0 {
        (done as f64 / total as f64 * 100.0) as u32
    } else {
        0
    };
    let filled = (done * bar_width).checked_div(total).unwrap_or(0);
    let empty = bar_width - filled;
    let bar = format!("{}{}", "█".repeat(filled), "░".repeat(empty),);

    let elapsed_str = format_duration_short(elapsed);

    let eta_str = if done > 0 {
        if let Some(avg) = avg_duration {
            let remaining = total.saturating_sub(done);
            let eta = avg * remaining as u32;
            format!(" · ~{} remaining", format_duration_short(eta))
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    format!("[{bar}] {done}/{total} ({pct}%) · {elapsed_str} elapsed{eta_str}")
}

/// Format a Duration as a short human-readable string (e.g., "1m32s", "45s", "2h5m").
pub(crate) fn format_duration_short(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs >= 3600 {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        format!("{h}h{m}m")
    } else if secs >= 60 {
        let m = secs / 60;
        let s = secs % 60;
        format!("{m}m{s}s")
    } else {
        format!("{secs}s")
    }
}

/// Format a duration in milliseconds as a compact human-readable string.
fn format_duration_ms(ms: u64) -> String {
    if ms >= 60_000 {
        let m = ms / 60_000;
        let s = (ms % 60_000) / 1000;
        format!("{m}m{s}s")
    } else if ms >= 1000 {
        let s = ms as f64 / 1000.0;
        format!("{s:.1}s")
    } else {
        format!("{ms}ms")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlanMonitorOutcome {
    /// Normal drain — more updates may follow.
    Continue,
    /// `PlanPaused` received — executor is waiting for Resume/Cancel.
    Paused,
    /// `PlanCompleted` or `PlanError` received — executor has exited.
    Finished,
}

/// Wrapper enum for the different spinner types used in plan mode.
/// Provides a uniform interface so the plan monitor can swap spinner styles.
enum PlanSpinner {
    /// Plan-specific spinner: `[subtask] Ns Label ⣾`
    Activity(effects::PlanActivitySpinner),
    /// Chat-style TTFT spinner: `Ns Waiting for stream ⣾`
    Ttft(effects::TtftWaitLineSpinner),
    /// Chat-style tool spinner: `Ns Running… description ⣾`
    Tool(effects::ToolRunningLineSpinner),
}

impl PlanSpinner {
    fn stop_clear(self) {
        match self {
            Self::Activity(s) => s.stop_clear(),
            Self::Ttft(s) => s.stop_clear(),
            Self::Tool(s) => s.stop_clear(),
        }
    }
}

/// Drain plan updates from the executor channel and display them via
/// `eprintln!`. Returns the monitor outcome so the caller can decide
/// whether to keep polling.
fn display_plan_updates_live(
    state: &mut ReplState,
    plan_spinner: &mut Option<PlanSpinner>,
    current_subtask_tag: &mut String,
) -> PlanMonitorOutcome {
    use plan_executor::PlanUpdate;
    let mut outcome = PlanMonitorOutcome::Continue;

    /// Finish any in-flight markdown stream: clear spinner, finalize renderer, newline.
    fn finalize_plan_stream(
        in_stream: &mut bool,
        spinner: &mut Option<PlanSpinner>,
        md: &mut Option<streaming_md::StreamingMarkdown>,
        thinking_pane: &mut Option<effects::ThinkingPreviewPane>,
    ) {
        // Finalize thinking pane before any other output
        if let Some(mut pane) = thinking_pane.take() {
            let summary = pane.summary_line();
            pane.clear();
            eprintln!("{summary}");
        }
        if *in_stream {
            *in_stream = false;
            if let Some(s) = spinner.take() {
                s.stop_clear();
            }
            if let Some(renderer) = md {
                renderer.finish();
            }
            *md = None;
            eprintln!();
        }
    }

    /// Clear active plan spinner (if any), finalize token/md stream, then print a line.
    fn print_plan_monitor_line(
        spinner: &mut Option<PlanSpinner>,
        in_stream: &mut bool,
        md: &mut Option<streaming_md::StreamingMarkdown>,
        thinking_pane: &mut Option<effects::ThinkingPreviewPane>,
        msg: String,
    ) {
        finalize_plan_stream(in_stream, spinner, md, thinking_pane);
        if let Some(s) = spinner.take() {
            s.stop_clear();
        }
        eprintln!("{msg}");
    }

    let handle = match state.plan_handle.as_mut() {
        Some(h) => h,
        None => return outcome,
    };

    while let Some(update) = handle.try_recv() {
        enum PostSpinner {
            None,
            Ttft,
            Tool(String),
            Activity(String),
        }
        let (msg, post_spinner): (String, PostSpinner) = match update {
            PlanUpdate::SubtaskStarted {
                id,
                title,
                index,
                total,
                ..
            } => {
                *current_subtask_tag = id;
                (
                    format!(
                        "\n  {} {} {}",
                        format!("▸ [{index}/{total}]").bold().cyan(),
                        title.bold(),
                        ""
                    ),
                    PostSpinner::Ttft,
                )
            }
            PlanUpdate::SubtaskCompleted {
                id,
                verification_passed,
                elapsed,
                ..
            } => {
                let dur = elapsed
                    .map(|d| format!(" ({})", format_duration_short(d)))
                    .unwrap_or_default();
                if verification_passed {
                    (
                        format!(
                            "  {} {} {}{}",
                            theme::icon_ok(),
                            "done".bold(),
                            id.dim(),
                            dur.dim()
                        ),
                        PostSpinner::Activity("Next subtask".to_string()),
                    )
                } else {
                    (
                        format!(
                            "  {} {} — {}{}",
                            theme::icon_warn(),
                            id,
                            "verification failed".yellow(),
                            dur.dim()
                        ),
                        PostSpinner::Activity("Next subtask".to_string()),
                    )
                }
            }
            PlanUpdate::SubtaskTurnResult {
                subtask_id,
                prompt_tokens,
                completion_tokens,
                session_id,
                ..
            } => {
                state.total_prompt_tokens += prompt_tokens;
                state.total_completion_tokens += completion_tokens;
                state.turn += 1;
                state.current_plan_subtask_id = Some(subtask_id);
                if let Some(sid) = session_id {
                    if state.session_id.is_none() {
                        state.session_id = Some(sid);
                    }
                }
                continue;
            }
            PlanUpdate::SubtaskStatusSync { id, status } => {
                if let Some(ref mut plan) = state.executing_plan {
                    if let Some(st) = plan.subtasks.iter_mut().find(|s| s.id == id) {
                        st.status = status;
                    }
                }
                if let Some(ref mut ps) = state.plan_mode {
                    if let Some(st) = ps.plan.subtasks.iter_mut().find(|s| s.id == id) {
                        st.status = status;
                    }
                }
                continue;
            }
            PlanUpdate::DurableStateReturn(durable) => {
                state.durable_task_state = Some(*durable);
                continue;
            }
            PlanUpdate::PlanProgress {
                done,
                total,
                elapsed,
                eta,
            } => {
                if state.plan_run_task_id.is_some() {
                    let pct = (done * 100).checked_div(total).unwrap_or(0) as u32;
                    state.plan_run_task_last_progress = Some((pct, done as u32, total as u32));
                }
                if let Some(PlanSpinner::Activity(spinner)) = plan_spinner.as_ref() {
                    spinner.set_eta_secs(eta.map(|d| d.as_secs()).unwrap_or(0));
                }
                let _ = (elapsed, eta);
                continue;
            }
            PlanUpdate::PlanCompleted { pct, elapsed } => {
                if let Some(s) = plan_spinner.take() {
                    s.stop_clear();
                }
                if let Some(mut h) = state.plan_handle.take() {
                    while let Some(trailing) = h.try_recv() {
                        apply_trailing_update(trailing, state);
                    }
                }
                let msg = format!(
                    "\n🏁  Plan complete — {pct}% verified in {}",
                    format_duration_short(elapsed),
                );
                state.executing_plan = None;
                state.current_plan_subtask_id = None;
                if let Some(tx) = state.pending_approval.take() {
                    let _ = tx.send(false);
                }
                print_plan_monitor_line(
                    plan_spinner,
                    &mut state.plan_in_token_stream,
                    &mut state.plan_md_renderer,
                    &mut state.plan_thinking_pane,
                    msg,
                );
                if let Some(ref report) = state.last_delivery_report {
                    eprintln!();
                    durable_bridge::display_delivery_report(report);
                }
                if state.plan_mode.is_some() {
                    eprintln!();
                    eprintln!(
                        "{}",
                        "  ✓ Plan completed! Type exit for normal chat, or describe next goal."
                            .dim()
                    );
                }
                return PlanMonitorOutcome::Finished;
            }
            PlanUpdate::PlanError { error } => {
                state.plan_run_task_last_error = Some(error.clone());
                if let Some(s) = plan_spinner.take() {
                    s.stop_clear();
                }
                if let Some(mut h) = state.plan_handle.take() {
                    while let Some(trailing) = h.try_recv() {
                        apply_trailing_update(trailing, state);
                    }
                }
                let msg = format!("\n❌  Plan error: {error}");
                state.executing_plan = None;
                state.current_plan_subtask_id = None;
                if let Some(tx) = state.pending_approval.take() {
                    let _ = tx.send(false);
                }
                print_plan_monitor_line(
                    plan_spinner,
                    &mut state.plan_in_token_stream,
                    &mut state.plan_md_renderer,
                    &mut state.plan_thinking_pane,
                    msg,
                );
                if state.plan_mode.is_some() {
                    eprintln!();
                    eprintln!("{}  {}", "📋".cyan(), "Recovery options:".bold());
                    eprintln!("{}", "    resume      — retry from where it stopped".dim());
                    eprintln!(
                        "{}",
                        "    rewind N    — reset subtask N and try again".dim()
                    );
                    eprintln!("{}", "    correct ... — add guidance before resuming".dim());
                    eprintln!("{}", "    show        — display current plan state".dim());
                    eprintln!("{}", "    exit        — leave plan mode".dim());
                }
                return PlanMonitorOutcome::Finished;
            }
            PlanUpdate::PlanPaused {
                pct,
                remaining,
                elapsed,
                blocked_ids,
            } => {
                outcome = PlanMonitorOutcome::Paused;
                // Surface blocked ids when the executor supplies them so the
                // user can see *which* subtasks are gating progress instead
                // of just a count. Empty blocked_ids means a Ctrl+C / normal
                // interrupt pause where the full pending queue is "remaining".
                let base = format!(
                    "\n⏸  Plan paused — {pct}% done, {remaining} remaining ({})",
                    format_duration_short(elapsed),
                );
                let msg = if blocked_ids.is_empty() {
                    base
                } else {
                    // Cap to a reasonable number so a 50-subtask deadlock
                    // doesn't overflow the terminal.
                    let shown: Vec<String> = blocked_ids.iter().take(8).cloned().collect();
                    let suffix = if blocked_ids.len() > shown.len() {
                        format!(" (+{} more)", blocked_ids.len() - shown.len())
                    } else {
                        String::new()
                    };
                    format!("{base}\n    blocked by: {}{suffix}", shown.join(", "))
                };
                (msg, PostSpinner::None)
            }
            PlanUpdate::GlobalVerificationFailed => (
                "  ⚠ Global verification failed".to_string(),
                PostSpinner::None,
            ),
            PlanUpdate::JournalEvent(event) => {
                if let Some(ref journal) = state.journal {
                    let _ = journal.append(&event);
                }
                continue;
            }
            PlanUpdate::HistoryEntry {
                user_msg,
                assistant_msg,
            } => {
                state.history.push((user_msg, assistant_msg));
                continue;
            }
            PlanUpdate::DeliveryReport(report) => {
                state.last_delivery_report = Some(report);
                continue;
            }
            PlanUpdate::VerificationReport(report) => {
                finalize_plan_stream(
                    &mut state.plan_in_token_stream,
                    plan_spinner,
                    &mut state.plan_md_renderer,
                    &mut state.plan_thinking_pane,
                );
                if let Some(s) = plan_spinner.take() {
                    s.stop_clear();
                }
                durable_bridge::display_verification_report(&report);
                *plan_spinner = Some(PlanSpinner::Activity(effects::PlanActivitySpinner::start(
                    current_subtask_tag,
                    "Continuing",
                )));
                continue;
            }
            PlanUpdate::SubtaskRetry {
                id,
                retries_exhausted,
                attempt,
                max_retries,
                failure_hint,
                ..
            } => {
                let attempt_str = if max_retries > 0 {
                    format!(" ({attempt}/{max_retries})")
                } else {
                    String::new()
                };
                let hint_str = failure_hint.map(|h| format!(": {h}")).unwrap_or_default();
                if retries_exhausted {
                    (
                        format!("  ⚠ {id} — verification failed{attempt_str}{hint_str}"),
                        PostSpinner::None,
                    )
                } else {
                    (
                        format!("  ↻ {id} — verification failed{attempt_str}{hint_str}, retrying…"),
                        PostSpinner::Ttft,
                    )
                }
            }
            PlanUpdate::StreamingEvent { event, .. } => {
                use chat_stream::StreamEvent;
                match event {
                    StreamEvent::ToolStarted { name, description } => {
                        let styled = stream_render::style_tool_description(&name, &description);
                        (
                            format!("  {} {} …", "⬢".cyan(), styled),
                            PostSpinner::Tool(description),
                        )
                    }
                    StreamEvent::ToolCompleted {
                        name,
                        description,
                        status,
                        duration_ms,
                        output_summary,
                    } => {
                        let dur = cli_formatting::format_duration_suffix(duration_ms);
                        let icon = if status == "error" {
                            theme::icon_err()
                        } else {
                            theme::icon_ok()
                        };
                        let styled = stream_render::style_tool_description(&name, &description);
                        let summary = output_summary
                            .map(|s| format!("\n    {}", s.dim()))
                            .unwrap_or_default();
                        (
                            format!("  {icon} {styled}{}{summary}", dur.dim()),
                            PostSpinner::None,
                        )
                    }
                    StreamEvent::WaitingForModel => {
                        finalize_plan_stream(
                            &mut state.plan_in_token_stream,
                            plan_spinner,
                            &mut state.plan_md_renderer,
                            &mut state.plan_thinking_pane,
                        );
                        if let Some(s) = plan_spinner.take() {
                            s.stop_clear();
                        }
                        *plan_spinner =
                            Some(PlanSpinner::Ttft(effects::TtftWaitLineSpinner::start()));
                        continue;
                    }
                    StreamEvent::ModelResponding => {
                        finalize_plan_stream(
                            &mut state.plan_in_token_stream,
                            plan_spinner,
                            &mut state.plan_md_renderer,
                            &mut state.plan_thinking_pane,
                        );
                        if let Some(s) = plan_spinner.take() {
                            s.stop_clear();
                        }
                        *plan_spinner =
                            Some(PlanSpinner::Activity(effects::PlanActivitySpinner::start(
                                current_subtask_tag,
                                "Model responding",
                            )));
                        continue;
                    }
                    StreamEvent::Thinking(true) => {
                        if state.plan_thinking_pane.is_none() {
                            finalize_plan_stream(
                                &mut state.plan_in_token_stream,
                                plan_spinner,
                                &mut state.plan_md_renderer,
                                &mut state.plan_thinking_pane,
                            );
                            if let Some(s) = plan_spinner.take() {
                                s.stop_clear();
                            }
                            use std::io::IsTerminal;
                            let rows = effects::thinking_viewport_rows();
                            let tw = crossterm::terminal::size()
                                .map(|(w, _)| w as usize)
                                .unwrap_or(80);
                            if rows > 0 && std::io::stdout().is_terminal() {
                                state.plan_thinking_pane =
                                    Some(effects::ThinkingPreviewPane::new(rows, tw));
                            } else {
                                *plan_spinner = Some(PlanSpinner::Activity(
                                    effects::PlanActivitySpinner::start(
                                        current_subtask_tag,
                                        "Thinking",
                                    ),
                                ));
                            }
                        }
                        continue;
                    }
                    StreamEvent::ThinkingChunk(text) => {
                        if let Some(ref mut pane) = state.plan_thinking_pane {
                            pane.push_chunk(&text);
                        }
                        continue;
                    }
                    StreamEvent::Thinking(false) => {
                        continue;
                    }
                    StreamEvent::Token(text) => {
                        if let Some(mut pane) = state.plan_thinking_pane.take() {
                            let summary = pane.summary_line();
                            pane.clear();
                            eprintln!("{summary}");
                        }
                        if !state.plan_in_token_stream {
                            if let Some(s) = plan_spinner.take() {
                                s.stop_clear();
                            }
                            state.plan_in_token_stream = true;
                            let tw = crossterm::terminal::size()
                                .map(|(w, _)| w as usize)
                                .unwrap_or(80);
                            state.plan_md_renderer = Some(streaming_md::StreamingMarkdown::new(tw));
                        }
                        if let Some(ref mut md) = state.plan_md_renderer {
                            md.push(&text);
                        }
                        continue;
                    }
                    StreamEvent::StatusLine(line) => {
                        finalize_plan_stream(
                            &mut state.plan_in_token_stream,
                            plan_spinner,
                            &mut state.plan_md_renderer,
                            &mut state.plan_thinking_pane,
                        );
                        if let Some(s) = plan_spinner.take() {
                            s.stop_clear();
                        }
                        eprintln!("    {line}");
                        continue;
                    }
                }
            }
            PlanUpdate::ApprovalNeeded {
                tool,
                header,
                detail,
                reason,
                response_tx,
            } => {
                if let Some(s) = plan_spinner.take() {
                    s.stop_clear();
                }
                let msg = format!(
                    "\n{}  {} — {}\n   {}\n   Reason: {}",
                    theme::icon_warn(),
                    tool,
                    header,
                    detail.as_deref().unwrap_or(""),
                    reason,
                );
                print_plan_monitor_line(
                    plan_spinner,
                    &mut state.plan_in_token_stream,
                    &mut state.plan_md_renderer,
                    &mut state.plan_thinking_pane,
                    msg,
                );
                state.pending_approval = Some(response_tx);
                continue;
            }
            _ => continue,
        };

        print_plan_monitor_line(
            plan_spinner,
            &mut state.plan_in_token_stream,
            &mut state.plan_md_renderer,
            &mut state.plan_thinking_pane,
            msg,
        );

        match post_spinner {
            PostSpinner::Ttft => {
                *plan_spinner = Some(PlanSpinner::Ttft(effects::TtftWaitLineSpinner::start()));
            }
            PostSpinner::Tool(desc) => {
                *plan_spinner = Some(PlanSpinner::Tool(effects::ToolRunningLineSpinner::start(
                    desc,
                )));
            }
            PostSpinner::Activity(label) => {
                *plan_spinner = Some(PlanSpinner::Activity(effects::PlanActivitySpinner::start(
                    current_subtask_tag,
                    &label,
                )));
            }
            PostSpinner::None => {}
        }
    }

    if let Some(ref mut pane) = state.plan_thinking_pane {
        pane.tick();
    }
    outcome
}

/// Push latest plan progress to [`ReplState::task_service`] for `/task list`.
pub(crate) async fn sync_plan_run_task_progress(state: &mut ReplState) {
    let Some(ref tid) = state.plan_run_task_id else {
        return;
    };
    let Some((pct, done, total)) = state.plan_run_task_last_progress else {
        return;
    };
    let Some(ref svc) = state.task_service else {
        return;
    };
    use astra_services::TaskService;
    let _ = svc.update_progress(tid, pct, done, total).await;
}

/// Terminal sync: `/task list` stays `pending` unless we mark the row completed here.
pub(crate) async fn finalize_plan_run_task_after_executor(state: &mut ReplState) {
    let Some(tid) = state.plan_run_task_id.clone() else {
        return;
    };
    let Some(ref svc) = state.task_service else {
        return;
    };
    use astra_services::{TaskService, task_orchestrator::TaskOutcome};
    if let Some(ref err) = state.plan_run_task_last_error {
        let err = err.clone();
        let _ = svc.fail_task(&tid, &err).await;
    } else if let Some(ref report) = state.last_delivery_report {
        let (outcome, pct, done, total) =
            durable_bridge::plan_run_finish_from_delivery_report(report);
        let _ = svc.complete_plan_run(&tid, pct, done, total, outcome).await;
    } else if let Some((pct, done, total)) = state.plan_run_task_last_progress {
        let _ = svc
            .complete_plan_run(&tid, pct, done, total, TaskOutcome::Success)
            .await;
    } else {
        let _ = svc.complete_task(&tid).await;
    }
    state.plan_run_task_id = None;
    state.plan_run_task_last_progress = None;
    state.plan_run_task_last_error = None;
}

/// Returns `true` when the executor sent a terminal event (`PlanCompleted` / `PlanError`).
pub(crate) fn flush_plan_updates_between_prompts(state: &mut ReplState) -> bool {
    if state.plan_handle.is_none() {
        return false;
    }

    let mut plan_spinner: Option<PlanSpinner> = None;
    let mut current_subtask_tag = state.current_plan_subtask_id.clone().unwrap_or_default();
    let outcome = display_plan_updates_live(state, &mut plan_spinner, &mut current_subtask_tag);
    if let Some(spinner) = plan_spinner.take() {
        spinner.stop_clear();
    }
    outcome == PlanMonitorOutcome::Finished
}

/// Clear REPL state when the plan update channel closed without `PlanCompleted` / `PlanError`.
/// Emits structured journal events so the failure is observable in telemetry.
fn cleanup_orphan_plan_executor(state: &mut ReplState, plan_spinner: &mut Option<PlanSpinner>) {
    if let Some(s) = plan_spinner.take() {
        s.stop_clear();
    }
    if let Some(mut pane) = state.plan_thinking_pane.take() {
        pane.clear();
    }
    if let Some(mut h) = state.plan_handle.take() {
        while h.try_recv().is_some() {}
    }

    // Emit structured failure events before clearing state.
    if let Some(ref journal) = state.journal {
        // 1. plan_progress with action=plan_failed
        if let Some(ref plan) = state.executing_plan {
            let goal = state.executing_plan_goal.as_deref().unwrap_or("unknown");
            let total = plan.subtasks.len();
            let done = plan
                .subtasks
                .iter()
                .filter(|s| s.status == astra_services::task_orchestrator::TaskStatus::Completed)
                .count();
            let event = astra_services::session_journal::JournalEvent::plan_progress(
                state.session_id.as_deref(),
                state.turn,
                state
                    .current_plan_subtask_id
                    .as_deref()
                    .unwrap_or("unknown"),
                goal,
                "plan_failed",
                plan.progress_pct(),
                total,
                done,
            );
            let _ = journal.append(&event);
            super::repl_turn::enqueue_ingestion_pub(state, &event);
        }
        // 2. interruption_recorded for the crash
        let interruption = astra_services::session_journal::JournalEvent::interruption_recorded(
            state.session_id.as_deref(),
            state.turn,
            serde_json::json!({
                "kind": "plan_executor_crash",
                "reason": "Plan executor channel closed without terminal status",
                "resumable": false,
            }),
        );
        let _ = journal.append(&interruption);
        super::repl_turn::enqueue_ingestion_pub(state, &interruption);
    }

    state.executing_plan = None;
    state.current_plan_subtask_id = None;
    if let Some(tx) = state.pending_approval.take() {
        let _ = tx.send(false);
    }
    eprintln!(
        "\n{}  Plan executor stopped without a final status (channel closed). State cleared.",
        theme::icon_warn()
    );
}

/// Block the REPL until the plan executor finishes, pauses, or errors.
///
/// Replaces the old "fire and forget" background model: the user cannot type
/// at the prompt while a plan is running. First Ctrl+C sends Pause; a second
/// Ctrl-C within two seconds sends Cancel. Approval prompts are read from stdin inline.
pub(crate) async fn run_blocking_plan_monitor(state: &mut ReplState) {
    let mut plan_spinner: Option<PlanSpinner> = None;
    let mut current_subtask_tag = state.current_plan_subtask_id.clone().unwrap_or_default();
    let mut last_ctrl_c: Option<std::time::Instant> = None;
    const CTRL_C_CANCEL_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

    loop {
        let outcome = display_plan_updates_live(state, &mut plan_spinner, &mut current_subtask_tag);

        sync_plan_run_task_progress(state).await;

        match outcome {
            PlanMonitorOutcome::Finished => {
                finalize_plan_run_task_after_executor(state).await;
                break;
            }
            PlanMonitorOutcome::Paused => {
                break;
            }
            PlanMonitorOutcome::Continue => {}
        }

        if state.plan_handle.as_ref().is_some_and(|h| h.is_finished()) {
            cleanup_orphan_plan_executor(state, &mut plan_spinner);
            sync_plan_run_task_progress(state).await;
            if state.plan_run_task_id.is_some() {
                state.plan_run_task_last_error.get_or_insert(
                    "Plan executor stopped without PlanCompleted/PlanError (channel closed)."
                        .into(),
                );
                finalize_plan_run_task_after_executor(state).await;
            }
            break;
        }

        if state.pending_approval.is_some() {
            let approved = tokio::task::spawn_blocking(|| {
                use std::io::IsTerminal;
                if std::io::stdin().is_terminal() {
                    let ch = crate::permission_manager::PermissionManager::prompt_approval(
                        crate::permission_manager::ApprovalPromptKind::LocalStandard,
                    );
                    matches!(ch, 'y' | 'a' | '!')
                } else {
                    use std::io::Write;
                    let _ = std::io::stderr().flush();
                    eprint!("   Approve? [y/N]: ");
                    let _ = std::io::stderr().flush();
                    let mut line = String::new();
                    if std::io::stdin().read_line(&mut line).is_err() {
                        return false;
                    }
                    let t = line.trim().to_lowercase();
                    t == "y" || t == "yes"
                }
            })
            .await
            .unwrap_or(false);
            if let Some(tx) = state.pending_approval.take() {
                let _ = tx.send(approved);
            }
            continue;
        }

        tokio::select! {
            biased;
            _ = tokio::signal::ctrl_c() => {
                if let Some(ref handle) = state.plan_handle {
                    let now = std::time::Instant::now();
                    let second_in_window = last_ctrl_c
                        .is_some_and(|t| now.duration_since(t) < CTRL_C_CANCEL_WINDOW);
                    last_ctrl_c = Some(now);
                    if second_in_window {
                        let _ = handle.send_command(plan_executor::PlanCommand::Cancel);
                        if let Some(s) = plan_spinner.take() {
                            s.stop_clear();
                        }
                        if let Some(mut pane) = state.plan_thinking_pane.take() {
                            pane.clear();
                        }
                        eprintln!(
                            "\n{}  Second interrupt — cancelling plan.",
                            "⏹".yellow()
                        );
                        break;
                    } else {
                        let _ = handle.send_command(plan_executor::PlanCommand::Pause);
                        if let Some(s) = plan_spinner.take() {
                            s.stop_clear();
                        }
                        if let Some(mut pane) = state.plan_thinking_pane.take() {
                            pane.clear();
                        }
                        eprintln!(
                            "\n{}  Pausing plan… (current subtask will finish first). Press Ctrl-C again within {}s to cancel.",
                            "⏸".yellow(),
                            CTRL_C_CANCEL_WINDOW.as_secs(),
                        );
                    }
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(30)) => {}
        }
    }

    if let Some(s) = plan_spinner.take() {
        s.stop_clear();
    }
    if let Some(mut pane) = state.plan_thinking_pane.take() {
        pane.clear();
    }

    if state.plan_handle.is_some() {
        eprint_plan_execution_paused_hints();
    }
}

/// Apply a single trailing update from the plan executor channel.
/// Called when draining remaining messages after PlanCompleted/PlanError.
fn apply_trailing_update(update: plan_executor::PlanUpdate, state: &mut ReplState) {
    use plan_executor::PlanUpdate;
    match update {
        PlanUpdate::HistoryEntry {
            user_msg,
            assistant_msg,
        } => {
            state.history.push((user_msg, assistant_msg));
        }
        PlanUpdate::JournalEvent(event) => {
            if let Some(ref journal) = state.journal {
                let _ = journal.append(&event);
            }
        }
        PlanUpdate::DeliveryReport(report) => {
            state.last_delivery_report = Some(report);
        }
        PlanUpdate::SubtaskTurnResult {
            subtask_id,
            prompt_tokens,
            completion_tokens,
            session_id,
            ..
        } => {
            state.total_prompt_tokens += prompt_tokens;
            state.total_completion_tokens += completion_tokens;
            state.turn += 1;
            state.current_plan_subtask_id = Some(subtask_id);
            if let Some(sid) = session_id {
                if state.session_id.is_none() {
                    state.session_id = Some(sid);
                }
            }
        }
        PlanUpdate::SubtaskStatusSync { id, status } => {
            sync_subtask_status(state, &id, status);
        }
        PlanUpdate::DurableStateReturn(durable) => {
            state.durable_task_state = Some(*durable);
        }
        _ => {}
    }
}

/// Update all in-memory plan copies so background execution stays observable
/// after plan mode exits.
fn sync_subtask_status(
    state: &mut ReplState,
    subtask_id: &str,
    status: astra_services::task_orchestrator::TaskStatus,
) {
    if let Some(ref mut plan) = state.executing_plan {
        if let Some(st) = plan.subtasks.iter_mut().find(|s| s.id == subtask_id) {
            st.status = status;
        }
    }
    if let Some(ref mut ps) = state.plan_mode {
        if let Some(st) = ps.plan.subtasks.iter_mut().find(|s| s.id == subtask_id) {
            st.status = status;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::task_orchestrator::{SubtaskPlan, TaskPlan, TaskStatus};

    #[test]
    fn flush_plan_updates_syncs_status_into_executing_plan() {
        let mut state = ReplState::default();
        state.executing_plan = Some(TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "s1".into(),
                title: "one".into(),
                status: TaskStatus::Pending,
                ..Default::default()
            }],
            ..Default::default()
        });

        let (handle, update_tx, _cmd_rx) = plan_executor::create_plan_channels();
        state.plan_handle = Some(handle);
        let _ = update_tx.send(plan_executor::PlanUpdate::SubtaskStatusSync {
            id: "s1".into(),
            status: TaskStatus::InProgress,
        });

        let terminal = flush_plan_updates_between_prompts(&mut state);
        assert!(!terminal);
        assert_eq!(
            state
                .executing_plan
                .as_ref()
                .expect("plan retained")
                .subtasks[0]
                .status,
            TaskStatus::InProgress
        );
    }
}

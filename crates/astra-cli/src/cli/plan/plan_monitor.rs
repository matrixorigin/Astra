//! Plan lifecycle projection shared by the executor and TUI event loop.

use crate::cli::cli_config::cli_utils::append_journal_event_or_warn;
use crate::cli::plan::plan_executor;
use crate::cli::session::session_state::SessionState;

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
    let bar = format!("{}{}", "█".repeat(filled), "░".repeat(empty));
    let elapsed_str = format_duration_short(elapsed);
    let eta_str = if done > 0 {
        avg_duration.map_or_else(String::new, |avg| {
            let remaining = total.saturating_sub(done);
            format!(
                " · ~{} remaining",
                format_duration_short(avg * remaining as u32)
            )
        })
    } else {
        String::new()
    };

    format!("[{bar}] {done}/{total} ({pct}%) · {elapsed_str} elapsed{eta_str}")
}

/// Format a duration as a short human-readable string.
pub(crate) fn format_duration_short(duration: std::time::Duration) -> String {
    let secs = duration.as_secs();
    if secs >= 3600 {
        let hours = secs / 3600;
        let minutes = (secs % 3600) / 60;
        format!("{hours}h{minutes}m")
    } else if secs >= 60 {
        let minutes = secs / 60;
        let seconds = secs % 60;
        format!("{minutes}m{seconds}s")
    } else {
        format!("{secs}s")
    }
}

/// Emit a structured plan-lifecycle journal event with common counters.
pub(crate) fn emit_plan_lifecycle_event(
    journal: Option<&astra_services::session_journal::JournalWriter>,
    session_id: Option<&str>,
    executing_plan: Option<&astra_services::task_orchestrator::TaskPlan>,
    description: &str,
    stage: &str,
    mut extra: serde_json::Map<String, serde_json::Value>,
) {
    if let Some(journal) = journal {
        let (items_done, items_total) = executing_plan.map_or((0, 0), |plan| {
            (plan.items_done(), plan.subtasks.len() as u32)
        });
        extra.insert(
            "stage".to_string(),
            serde_json::Value::String(stage.to_string()),
        );
        extra.insert("items_done".to_string(), serde_json::json!(items_done));
        extra.insert("items_total".to_string(), serde_json::json!(items_total));
        let event = astra_services::session_journal::JournalEvent::plan_lifecycle(
            session_id,
            description,
            Some(serde_json::Value::Object(extra)),
        );
        append_journal_event_or_warn(journal, session_id, &event, "plan_monitor:emit_plan_event");
    }
}

/// Apply one update left in the executor channel after a terminal boundary.
pub(crate) fn apply_trailing_update(update: plan_executor::PlanUpdate, state: &mut SessionState) {
    use crate::cli::plan::plan_executor::PlanUpdate;
    match update {
        PlanUpdate::HistoryEntry {
            user_msg,
            assistant_msg,
        } => state.history.push((user_msg, assistant_msg)),
        PlanUpdate::JournalEvent(event) => {
            if let Some(ref journal) = state.journal {
                append_journal_event_or_warn(
                    journal,
                    state.session_id.as_deref(),
                    &event,
                    "plan_monitor:trailing_journal_event",
                );
            }
        }
        PlanUpdate::DeliveryReport(report) => state.last_delivery_report = Some(report),
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
            if let Some(session_id) = session_id
                && state.session_id.is_none()
            {
                state.set_session_id(session_id);
            }
        }
        PlanUpdate::SubtaskStatusSync { id, status } => {
            sync_subtask_status(state, &id, status);
        }
        PlanUpdate::DurableStateReturn(durable) => state.durable_task_state = Some(*durable),
        PlanUpdate::Advisory { title, detail } => {
            tracing::warn!(%title, %detail, "plan executor advisory arrived after monitor shutdown");
        }
        _ => {}
    }
}

/// Update all in-memory plan projections after an executor transition.
pub(crate) fn sync_subtask_status(
    state: &mut SessionState,
    subtask_id: &str,
    status: astra_services::task_orchestrator::TaskStatus,
) {
    if let Some(ref mut plan) = state.executing_plan
        && let Some(subtask) = plan
            .subtasks
            .iter_mut()
            .find(|subtask| subtask.id == subtask_id)
    {
        subtask.status = status;
    }
    if let Some(ref mut plan_state) = state.cloud_plan_mirror
        && let Some(subtask) = plan_state
            .plan
            .subtasks
            .iter_mut()
            .find(|subtask| subtask.id == subtask_id)
    {
        subtask.status = status;
    }
}

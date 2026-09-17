//! Compose a compact execution-state summary for injection into the model's
//! system prompt and the `introspect(session)` snapshot.
//!
//! This is intentionally tiny and stable. It should tell the model:
//! - whether a paused plan can be resumed,
//! - whether a plan is being authored or executed,
//! - whether the last turn was interrupted,
//! - what durable verification state exists,
//! - and what the last lifecycle event was.
//!
//! Canonical Work state is observed through the typed Work projection and is
//! deliberately absent from this append-only compatibility summary.

use astra_runtime::plan::{PlanModeState, plan_resume_digest};
use astra_services::session_journal::{JournalEvent, JournalEventType};

pub(crate) struct ExecutionStateSummaryInput<'a> {
    pub model: Option<&'a str>,
    pub last_turn_interrupted: bool,
    pub session_persistence_error: Option<&'a str>,
    pub plan_mode_active: bool,
    pub plan_mode: Option<&'a PlanModeState>,
    pub last_turn_event: Option<&'a JournalEvent>,
}

pub(crate) fn format_for_session_state(
    state: &crate::cli::session::session_state::SessionState,
) -> Option<String> {
    format_summary(ExecutionStateSummaryInput {
        model: state.model.as_deref(),
        last_turn_interrupted: state.last_turn_interrupted,
        session_persistence_error: state.session_persistence_error.as_deref(),
        plan_mode_active: state.plan_mode_active(),
        plan_mode: state.cloud_plan_mirror.as_ref(),
        last_turn_event: state.last_turn_event.as_ref(),
    })
}

pub(crate) fn format_summary(input: ExecutionStateSummaryInput<'_>) -> Option<String> {
    let mut lifecycle_lines = Vec::new();

    if input.last_turn_interrupted {
        lifecycle_lines.push(
            "turn state: last turn was interrupted; inspect partial work before resuming".into(),
        );
    }
    if let Some(error) = input
        .session_persistence_error
        .map(str::trim)
        .filter(|error| !error.is_empty())
    {
        lifecycle_lines.push(format!(
            "session persistence: degraded · {}",
            preview(error, 160)
        ));
    }
    if let Some(plan_mode) = input.plan_mode.filter(|_| input.plan_mode_active) {
        let mut line = format!(
            "plan authoring: {}",
            plan_resume_digest(plan_mode)
                .unwrap_or_else(|| format!("goal=\"{}\"", preview(&plan_mode.goal, 160)))
        );
        if plan_mode.modified {
            line.push_str(" · modified");
        }
        lifecycle_lines.push(line);
    }
    if let Some(event_line) = input.last_turn_event.and_then(render_last_event) {
        lifecycle_lines.push(event_line);
    }

    if lifecycle_lines.is_empty() {
        return None;
    }

    let mut block = Vec::new();
    block.push("### Turn-start session execution state".to_string());
    if let Some(model) = input.model.map(str::trim).filter(|model| !model.is_empty()) {
        block.push(format!("model: {model}"));
    }
    block.extend(lifecycle_lines);
    Some(block.join("\n"))
}

fn render_last_event(event: &JournalEvent) -> Option<String> {
    let detail = match event.event_type {
        JournalEventType::PlanProgress => event.metadata.as_ref().and_then(|meta| {
            Some(format!(
                "action={} · subtask=\"{}\"",
                meta.get("action")?.as_str()?,
                preview(meta.get("subtask_title")?.as_str()?, 80)
            ))
        }),
        JournalEventType::PlanLifecycle => event
            .metadata
            .as_ref()
            .and_then(|meta| meta.get("summary"))
            .and_then(|value| value.as_str())
            .map(|summary| preview(summary, 160)),
        JournalEventType::PlanEdit => event
            .metadata
            .as_ref()
            .and_then(|meta| meta.get("action"))
            .and_then(|value| value.as_str())
            .map(|action| format!("action={}", preview(action, 120))),
        JournalEventType::TurnError => event.error.as_deref().map(|error| preview(error, 160)),
        JournalEventType::TurnGuardVerdict => event
            .metadata
            .as_ref()
            .and_then(|meta| meta.get("avoid_reason_summary"))
            .and_then(|value| value.as_str())
            .map(|summary| preview(summary, 160))
            .or_else(|| {
                event.metadata.as_ref().and_then(|meta| {
                    meta.get("severity")
                        .and_then(|value| value.as_str())
                        .map(|severity| format!("severity={severity}"))
                })
            }),
        _ => event
            .metadata
            .as_ref()
            .and_then(|meta| meta.get("summary"))
            .and_then(|value| value.as_str())
            .map(|summary| preview(summary, 160))
            .or_else(|| event.error.as_deref().map(|error| preview(error, 160))),
    }?;

    Some(format!(
        "last event: {} · {}",
        event_type_label(event.event_type.clone()),
        detail
    ))
}

fn event_type_label(event_type: JournalEventType) -> &'static str {
    match event_type {
        JournalEventType::Turn => "turn",
        JournalEventType::TurnError => "turn_error",
        JournalEventType::PlanProgress => "plan_progress",
        JournalEventType::PlanEdit => "plan_edit",
        JournalEventType::PlanLifecycle => "plan_lifecycle",
        JournalEventType::TurnGuardVerdict => "turn_guard_verdict",
        _ => "session_event",
    }
}

fn preview(value: &str, max_chars: usize) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max_chars)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{ExecutionStateSummaryInput, format_for_session_state, format_summary};

    #[test]
    fn summary_reports_plan_authoring_without_inventing_execution_state() {
        let plan = astra_runtime::plan::PlanModeState::new("Ship auth".into());
        let summary = format_summary(ExecutionStateSummaryInput {
            model: Some("deepseek-v4-flash"),
            last_turn_interrupted: false,
            session_persistence_error: None,
            plan_mode_active: true,
            plan_mode: Some(&plan),
            last_turn_event: None,
        })
        .expect("summary");

        assert!(summary.contains("plan authoring"));
        assert!(!summary.contains("plan execution"));
    }

    #[test]
    fn inactive_plan_mirror_is_not_an_execution_fact() {
        let mut state = crate::cli::session::session_state::SessionState::default();
        state.cloud_plan_mirror = Some(astra_runtime::plan::PlanModeState::new("stale".into()));

        assert!(format_for_session_state(&state).is_none());
    }
}

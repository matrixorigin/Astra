//! Compose a compact execution-state summary for injection into the model's
//! system prompt and the `introspect(session)` snapshot.
//!
//! This is intentionally tiny and stable. It should tell the model:
//! - whether a paused plan can be resumed,
//! - whether a plan is being authored or executed,
//! - whether the last turn was interrupted,
//! - what durable verification state exists,
//! - what the last lifecycle event was,
//! - and what the session task board currently says.

use astra_runtime::plan::{PlanModeState, plan_resume_digest};
use astra_services::{
    durable_task::{SubtaskStage, TaskContract},
    session_journal::{JournalEvent, JournalEventType},
    task_orchestrator::{TaskPlan, TaskStatus},
};
use astra_tools::task_mgmt::SessionTask;

pub(crate) struct ExecutionStateSummaryInput<'a> {
    pub model: Option<&'a str>,
    pub last_turn_interrupted: bool,
    pub session_persistence_error: Option<&'a str>,
    pub plan_mode_active: bool,
    pub plan_mode: Option<&'a PlanModeState>,
    pub executing_plan: Option<&'a TaskPlan>,
    pub executing_plan_goal: Option<&'a str>,
    pub plan_execution_rounds: usize,
    pub plan_execution_corrections: &'a [String],
    pub durable_contract: Option<&'a TaskContract>,
    pub last_turn_event: Option<&'a JournalEvent>,
    pub tasks: &'a [SessionTask],
}

pub(crate) fn format_for_session_state(
    state: &crate::cli::session::session_state::SessionState,
    tasks: &[SessionTask],
) -> Option<String> {
    format_summary(ExecutionStateSummaryInput {
        model: state.model.as_deref(),
        last_turn_interrupted: state.last_turn_interrupted,
        session_persistence_error: state.session_persistence_error.as_deref(),
        plan_mode_active: state.plan_mode_active(),
        plan_mode: state.cloud_plan_mirror.as_ref(),
        executing_plan: state.executing_plan.as_ref(),
        executing_plan_goal: state.executing_plan_goal.as_deref(),
        plan_execution_rounds: state.plan_execution_rounds,
        plan_execution_corrections: &state.plan_execution_corrections,
        durable_contract: state
            .durable_task_state
            .as_ref()
            .map(|state| &state.contract),
        last_turn_event: state.last_turn_event.as_ref(),
        tasks,
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
    if let Some(plan) = input.executing_plan {
        lifecycle_lines.push(render_executing_plan(
            plan,
            input.executing_plan_goal,
            input.plan_execution_rounds,
            input.plan_execution_corrections,
        ));
    }
    if let Some(contract) = input.durable_contract {
        lifecycle_lines.push(render_durable_contract(contract));
    }
    if let Some(event_line) = input.last_turn_event.and_then(render_last_event) {
        lifecycle_lines.push(event_line);
    }

    let task_block = crate::cli::task::task_summary::format_summary(input.tasks);
    if lifecycle_lines.is_empty() {
        return task_block;
    }

    let mut sections = Vec::new();
    let mut block = Vec::new();
    block.push("### Turn-start session execution state".to_string());
    if let Some(model) = input.model.map(str::trim).filter(|model| !model.is_empty()) {
        block.push(format!("model: {model}"));
    }
    block.extend(lifecycle_lines);
    sections.push(block.join("\n"));
    if let Some(task_block) = task_block {
        sections.push(task_block);
    }
    Some(sections.join("\n\n"))
}

fn render_executing_plan(
    plan: &TaskPlan,
    goal: Option<&str>,
    rounds: usize,
    corrections: &[String],
) -> String {
    let total = plan.subtasks.len();
    let done = plan.items_done();
    let open = plan
        .subtasks
        .iter()
        .filter(|subtask| !subtask.status.is_terminal() && subtask.status != TaskStatus::InProgress)
        .count();
    let in_progress = plan
        .subtasks
        .iter()
        .find(|subtask| subtask.status == TaskStatus::InProgress)
        .map(|subtask| format!("in_progress=\"{}\"", preview(&subtask.title, 80)));
    let next = if in_progress.is_none() {
        plan.subtasks
            .iter()
            .find(|subtask| subtask.status == TaskStatus::Pending)
            .map(|subtask| format!("next=\"{}\"", preview(&subtask.title, 80)))
    } else {
        None
    };

    let mut fields = Vec::new();
    if let Some(goal) = goal.map(str::trim).filter(|goal| !goal.is_empty()) {
        fields.push(format!("goal=\"{}\"", preview(goal, 160)));
    }
    if let Some(in_progress) = in_progress {
        fields.push(in_progress);
    } else if let Some(next) = next {
        fields.push(next);
    }
    if total > 0 {
        fields.push(format!("open={open}"));
        fields.push(format!("done={done}/{total}"));
        fields.push(format!("progress={}%", plan.progress_pct()));
    }
    if rounds > 0 {
        fields.push(format!("rounds={rounds}"));
    }
    if !corrections.is_empty() {
        fields.push(format!("corrections={}", corrections.len()));
    }

    format!("plan execution: {}", fields.join(" · "))
}

fn render_durable_contract(contract: &TaskContract) -> String {
    let verified = contract
        .subtasks
        .iter()
        .filter(|subtask| subtask.stage.is_success())
        .count();
    let total = contract.subtasks.len();
    let current = contract
        .subtasks
        .iter()
        .find(|subtask| {
            matches!(
                subtask.stage,
                SubtaskStage::Executing
                    | SubtaskStage::AwaitingVerification
                    | SubtaskStage::Verifying
                    | SubtaskStage::VerificationFailed { .. }
                    | SubtaskStage::ExecutionFailed { .. }
                    | SubtaskStage::Blocked { .. }
            )
        })
        .or_else(|| {
            contract
                .subtasks
                .iter()
                .find(|subtask| matches!(subtask.stage, SubtaskStage::Pending))
        });

    let mut fields = vec![format!("status={}", contract.status.as_str())];
    if total > 0 {
        fields.push(format!("verified={verified}/{total}"));
    }
    if let Some(subtask) = current {
        fields.push(format!("subtask=\"{}\"", preview(&subtask.title, 80)));
        fields.push(format!("stage={}", subtask.stage.as_str()));
    }

    format!("durable verification: {}", fields.join(" · "))
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
    use super::{ExecutionStateSummaryInput, format_summary};
    use astra_runtime::plan::PlanModeState;
    use astra_services::VerifierKind;
    use astra_services::durable_task::{
        ContractStatus, DurableSubtask, SubtaskStage, TaskContract, TaskScope,
        VerificationCriterion,
    };
    use astra_services::session_journal::JournalEvent;
    use astra_services::task_orchestrator::{TaskPlan, TaskStatus};
    use astra_tools::task_mgmt::{SessionSubtask, SessionTask};

    fn task(id: &str, title: &str, status: &str) -> SessionTask {
        SessionTask {
            id: id.into(),
            title: title.into(),
            description: None,
            status: status.into(),
            subtasks: Vec::new(),
            created_at: "now".into(),
            updated_at: "now".into(),
            active_form: None,
            owner: None,
            metadata: None,
            blocks: Vec::new(),
            blocked_by: Vec::new(),
        }
    }

    fn subtask(id: &str, title: &str, status: &str) -> SessionSubtask {
        SessionSubtask {
            id: id.into(),
            title: title.into(),
            description: None,
            status: status.into(),
            depends_on: Vec::new(),
            owner: None,
        }
    }

    fn durable_contract() -> TaskContract {
        TaskContract {
            contract_id: "contract-12345678".into(),
            task_id: "task-123".into(),
            goal: "Ship auth flow".into(),
            scope: TaskScope::default(),
            subtasks: vec![
                DurableSubtask {
                    id: "sub-1".into(),
                    title: "Model auth state".into(),
                    stage: SubtaskStage::Completed,
                    ..Default::default()
                },
                DurableSubtask {
                    id: "sub-2".into(),
                    title: "Verify auth API".into(),
                    stage: SubtaskStage::AwaitingVerification,
                    criteria: vec![VerificationCriterion {
                        id: "criterion-1".into(),
                        description: "auth API should pass".into(),
                        verifier: VerifierKind::FileExists {
                            paths: vec!["src/auth.rs".into()],
                        },
                        required: true,
                        timeout_sec: 30,
                        global_only: false,
                    }],
                    ..Default::default()
                },
            ],
            global_verification: Vec::new(),
            version: 1,
            status: ContractStatus::Active,
            created_at: "now".into(),
            updated_at: "now".into(),
            domain_hint: None,
            task_type: None,
            last_global_results: Vec::new(),
        }
    }

    #[test]
    fn summary_includes_resume_authoring_execution_durable_and_last_event() {
        let mut plan_mode = PlanModeState::new("Harden auth".into());
        plan_mode.modified = true;
        plan_mode
            .plan
            .subtasks
            .push(astra_services::task_orchestrator::SubtaskPlan {
                id: "draft-1".into(),
                title: "Write auth plan".into(),
                status: TaskStatus::Pending,
                ..Default::default()
            });

        let executing_plan = TaskPlan {
            subtasks: vec![
                astra_services::task_orchestrator::SubtaskPlan {
                    id: "exec-1".into(),
                    title: "Model auth state".into(),
                    status: TaskStatus::Completed,
                    ..Default::default()
                },
                astra_services::task_orchestrator::SubtaskPlan {
                    id: "exec-2".into(),
                    title: "Verify auth API".into(),
                    status: TaskStatus::InProgress,
                    ..Default::default()
                },
            ],
            notes: None,
        };

        let event = JournalEvent::plan_progress(
            Some("sess-1"),
            7,
            "exec-2",
            "Verify auth API",
            "started",
            50,
            2,
            1,
        );
        let mut parent = task("task-1", "Ship auth flow", "in_progress");
        parent.subtasks = vec![
            subtask("sub-1", "Model auth state", "completed"),
            subtask("sub-2", "Verify auth API", "in_progress"),
        ];
        let durable = durable_contract();

        let out = format_summary(ExecutionStateSummaryInput {
            model: Some("gpt-5.4"),
            last_turn_interrupted: true,
            session_persistence_error: None,
            plan_mode_active: true,
            plan_mode: Some(&plan_mode),
            executing_plan: Some(&executing_plan),
            executing_plan_goal: Some("Ship auth flow"),
            plan_execution_rounds: 4,
            plan_execution_corrections: &["add regression coverage".into()],
            durable_contract: Some(&durable),
            last_turn_event: Some(&event),
            tasks: &[parent],
        })
        .expect("summary");

        assert!(
            out.contains("### Turn-start session execution state"),
            "{out}"
        );
        assert!(out.contains("model: gpt-5.4"), "{out}");
        assert!(
            out.contains("plan authoring: [plan-resume] goal=\"Harden auth\""),
            "{out}"
        );
        assert!(out.contains("open=1"), "{out}");
        assert!(out.contains("done=0/1"), "{out}");
        assert!(out.contains("modified"), "{out}");
        assert!(
            out.contains("plan execution: goal=\"Ship auth flow\""),
            "{out}"
        );
        assert!(out.contains("in_progress=\"Verify auth API\""), "{out}");
        assert!(out.contains("rounds=4"), "{out}");
        assert!(out.contains("corrections=1"), "{out}");
        assert!(
            out.contains("durable verification: status=active · verified=1/2"),
            "{out}"
        );
        assert!(out.contains("stage=awaiting_verification"), "{out}");
        assert!(
            out.contains(
                "last event: plan_progress · action=started · subtask=\"Verify auth API\""
            ),
            "{out}"
        );
        assert!(out.contains("### Active task board"), "{out}");
    }

    #[test]
    fn summary_returns_task_block_when_no_lifecycle_state_exists() {
        let out = format_summary(ExecutionStateSummaryInput {
            model: Some("gpt-5.4"),
            last_turn_interrupted: false,
            session_persistence_error: None,
            plan_mode_active: false,
            plan_mode: None,
            executing_plan: None,
            executing_plan_goal: None,
            plan_execution_rounds: 0,
            plan_execution_corrections: &[],
            durable_contract: None,
            last_turn_event: None,
            tasks: &[task("task-1", "Implement checkout", "pending")],
        })
        .expect("task block");

        assert!(
            !out.contains("### Turn-start session execution state"),
            "{out}"
        );
        assert!(out.contains("### Active task board"), "{out}");
    }

    #[test]
    fn summary_uses_next_subtask_when_plan_is_not_started() {
        let executing_plan = TaskPlan {
            subtasks: vec![
                astra_services::task_orchestrator::SubtaskPlan {
                    id: "exec-1".into(),
                    title: "Model auth state".into(),
                    status: TaskStatus::Pending,
                    ..Default::default()
                },
                astra_services::task_orchestrator::SubtaskPlan {
                    id: "exec-2".into(),
                    title: "Verify auth API".into(),
                    status: TaskStatus::Pending,
                    ..Default::default()
                },
            ],
            notes: None,
        };

        let out = format_summary(ExecutionStateSummaryInput {
            model: None,
            last_turn_interrupted: false,
            session_persistence_error: None,
            plan_mode_active: false,
            plan_mode: None,
            executing_plan: Some(&executing_plan),
            executing_plan_goal: Some("Ship auth flow"),
            plan_execution_rounds: 0,
            plan_execution_corrections: &[],
            durable_contract: None,
            last_turn_event: None,
            tasks: &[],
        })
        .expect("summary");

        assert!(out.contains("next=\"Model auth state\""), "{out}");
        assert!(out.contains("done=0/2"), "{out}");
    }

    #[test]
    fn summary_hides_stale_plan_authoring_when_plan_mode_is_inactive() {
        let plan_mode = PlanModeState::new("Stale plan".into());
        let executing_plan = TaskPlan {
            subtasks: vec![astra_services::task_orchestrator::SubtaskPlan {
                id: "exec-1".into(),
                title: "Verify stale plan is hidden".into(),
                status: TaskStatus::InProgress,
                ..Default::default()
            }],
            notes: None,
        };

        let out = format_summary(ExecutionStateSummaryInput {
            model: Some("gpt-5.4"),
            last_turn_interrupted: false,
            session_persistence_error: None,
            plan_mode_active: false,
            plan_mode: Some(&plan_mode),
            executing_plan: Some(&executing_plan),
            executing_plan_goal: Some("Keep executing plan visible"),
            plan_execution_rounds: 2,
            plan_execution_corrections: &[],
            durable_contract: None,
            last_turn_event: None,
            tasks: &[task("task-1", "Implement checkout", "pending")],
        })
        .expect("summary");

        assert!(
            !out.contains("plan authoring:"),
            "inactive plan mode must not leak stale plan-authoring summary: {out}"
        );
        assert!(
            out.contains("plan execution: goal=\"Keep executing plan visible\""),
            "executing-plan summary must remain visible when only plan authoring is stale: {out}"
        );
    }

    #[test]
    fn summary_surfaces_session_persistence_degradation() {
        let out = format_summary(ExecutionStateSummaryInput {
            model: Some("gpt-5.4"),
            last_turn_interrupted: false,
            session_persistence_error: Some(
                "failed to append turn event: Is a directory (os error 21)",
            ),
            plan_mode_active: false,
            plan_mode: None,
            executing_plan: None,
            executing_plan_goal: None,
            plan_execution_rounds: 0,
            plan_execution_corrections: &[],
            durable_contract: None,
            last_turn_event: None,
            tasks: &[],
        })
        .expect("summary");

        assert!(out.contains("session persistence: degraded"), "{out}");
        assert!(out.contains("failed to append turn event"), "{out}");
    }
}

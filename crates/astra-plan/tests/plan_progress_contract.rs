//! Public-API contract for plan progress, readiness and phase serialization.

use astra_plan::{
    InMemoryPlanRepository, PlanListFilter, PlanModeState, PlanPhase, PlanRepository, SubtaskPlan,
    TaskStatus,
};

fn plan(statuses: &[TaskStatus]) -> PlanModeState {
    let mut state = PlanModeState::new("goal".into());
    state.plan.subtasks = statuses
        .iter()
        .enumerate()
        .map(|(index, status)| SubtaskPlan {
            id: format!("s{index}"),
            status: *status,
            ..Default::default()
        })
        .collect();
    state
}

#[test]
fn boundary_phases() {
    assert_eq!(plan(&[]).infer_phase(), PlanPhase::Planning);
    assert_eq!(
        plan(&[TaskStatus::Pending]).infer_phase(),
        PlanPhase::Refining
    );
    assert_eq!(
        plan(&[TaskStatus::Completed; 3]).infer_phase(),
        PlanPhase::Completed
    );
    assert_eq!(
        plan(&[TaskStatus::InProgress]).infer_phase(),
        PlanPhase::Executing
    );
    assert_eq!(
        plan(&[TaskStatus::Pending, TaskStatus::InProgress]).infer_phase(),
        PlanPhase::Executing
    );
    assert_eq!(
        plan(&[TaskStatus::Completed, TaskStatus::Pending]).infer_phase(),
        PlanPhase::Executing
    );
    // An interrupted or failed first step keeps the plan executing (#1172).
    for first in [
        TaskStatus::Paused,
        TaskStatus::Failed,
        TaskStatus::Cancelled,
    ] {
        assert_eq!(
            plan(&[first, TaskStatus::Pending]).infer_phase(),
            PlanPhase::Executing,
            "{first:?} first step"
        );
        assert_eq!(plan(&[first]).infer_phase(), PlanPhase::Executing);
    }
}

async fn phase_counts(repo: &InMemoryPlanRepository, phase: &str) -> usize {
    repo.list_for_user(
        "u-1",
        PlanListFilter {
            phase: Some(phase),
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .len()
}

#[tokio::test]
async fn interrupted_first_step_stays_in_executing_list() {
    for interrupted in [
        TaskStatus::Paused,
        TaskStatus::Failed,
        TaskStatus::Cancelled,
    ] {
        let repo = InMemoryPlanRepository::new();
        let mut state = plan(&[TaskStatus::InProgress, TaskStatus::Pending]);
        state.created_by = Some("u-1".into());
        repo.save("u-1", "plan-1", &mut state, None).await.unwrap();
        assert_eq!(phase_counts(&repo, "executing").await, 1);

        let mut loaded = repo.load("u-1", "plan-1").await.unwrap();
        let version = loaded.version;
        loaded.plan.subtasks[0].status = interrupted;
        repo.save("u-1", "plan-1", &mut loaded, Some(version))
            .await
            .unwrap();
        assert_eq!(
            phase_counts(&repo, "executing").await,
            1,
            "{interrupted:?} first step left the executing list"
        );
        assert_eq!(phase_counts(&repo, "refining").await, 0);
    }
}

#[test]
fn progress_is_floored_and_zero_for_empty_plans() {
    assert_eq!(plan(&[]).plan.progress_pct(), 0);
    let state = plan(&[
        TaskStatus::Completed,
        TaskStatus::Completed,
        TaskStatus::Pending,
    ]);
    assert_eq!(state.plan.progress_pct(), 66);
    assert_eq!(state.plan.items_done(), 2);
}

#[test]
fn readiness_requires_existing_completed_dependencies() {
    let mut state = plan(&[TaskStatus::Failed, TaskStatus::Pending, TaskStatus::Pending]);
    state.plan.subtasks[1].depends_on = vec!["s0".into()];
    state.plan.subtasks[2].depends_on = vec!["missing".into()];
    assert!(state.plan.ready_subtasks().is_empty());

    state.plan.subtasks[0].status = TaskStatus::Completed;
    let ready: Vec<_> = state
        .plan
        .ready_subtasks()
        .iter()
        .map(|s| s.id.clone())
        .collect();
    assert_eq!(ready, ["s1"]);
}

#[test]
fn terminal_statuses_and_parsing_round_trip() {
    for status in [
        TaskStatus::Pending,
        TaskStatus::InProgress,
        TaskStatus::Paused,
        TaskStatus::Completed,
        TaskStatus::Failed,
        TaskStatus::Cancelled,
    ] {
        assert_eq!(TaskStatus::parse_status(status.as_str()), Some(status));
        assert_eq!(
            status.is_terminal(),
            matches!(
                status,
                TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
            )
        );
    }
    assert_eq!(TaskStatus::parse_status("IN_PROGRESS"), None);
}

#[test]
fn phase_serialization_is_lowercase_and_strict() {
    for phase in [
        PlanPhase::Planning,
        PlanPhase::Refining,
        PlanPhase::Executing,
        PlanPhase::Completed,
    ] {
        let json = serde_json::to_string(&phase).unwrap();
        assert_eq!(json, format!("\"{phase}\""));
        assert_eq!(serde_json::from_str::<PlanPhase>(&json).unwrap(), phase);
    }
    assert!(serde_json::from_str::<PlanPhase>("\"Executing\"").is_err());
}

#[test]
fn version_defaults_to_one_when_absent() {
    let state: PlanModeState =
        serde_json::from_str(r#"{"goal":"g","plan":{"subtasks":[],"notes":null}}"#).unwrap();
    assert_eq!(state.version, 1);
    assert!(state.created_by.is_none());
    assert_eq!(
        PlanModeState::new_with_owner("g".into(), "u".into())
            .created_by
            .as_deref(),
        Some("u")
    );
}

//! Astra Plan — shared plan state, execution helpers, and persistence boundaries.

pub mod execution;
pub mod repository;
pub mod resume;
pub mod state;

pub use execution::{
    FileConflict, ParallelGroups, analyze_parallelism, format_subtask_prompt_with_operator_notes,
    subtask_requires_browser_verification,
};
pub use repository::{
    CloudPlanRepository, FinalizeStepRun, InMemoryPlanRepository, NewStepRun, PlanListFilter,
    PlanLoadError, PlanRepository, PlanStepRun, RecordCompletedStepRun, SavedPlanInfo,
};
pub use resume::{
    PlanResumeSnapshot, plan_mode_authoring_active, plan_resume_digest,
    plan_resume_hint_for_session, plan_resume_prompt_hint, plan_resume_snapshot_for_session,
};
pub use state::{
    ExecutionTimeline, PlanExecutionConfig, PlanModeState, PlanPhase, TimelineEvent,
    TimelineEventKind,
};

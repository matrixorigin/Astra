//! Astra Plan — shared plan state and persistence boundaries.

pub mod model;
pub mod repository;
pub mod resume;
pub mod state;

pub use model::{SubtaskPlan, TaskPlan, TaskStatus};
pub use repository::{
    CloudPlanRepository, InMemoryPlanRepository, PlanListFilter, PlanLoadError, PlanRepository,
    SavedPlanInfo,
};
pub use resume::{
    PlanResumeSnapshot, plan_mode_authoring_active, plan_resume_digest,
    plan_resume_hint_for_session, plan_resume_prompt_hint, plan_resume_snapshot_for_plan,
    plan_resume_snapshot_for_session,
};
pub use state::{PlanModeState, PlanPhase};

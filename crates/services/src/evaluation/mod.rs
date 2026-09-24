pub mod api;
pub mod assessment;
pub mod bootstrap;
pub mod database;
pub mod durable;
pub mod execution;
pub mod execution_config;
pub mod experiment;
pub mod materialization;
pub mod measurement_profile;
pub mod noop;
pub mod projection;
pub mod report;
pub mod service;
pub mod task_assessment;
pub mod task_verifier;
pub mod types;
pub mod utils;
pub mod workspace_evidence;

pub use api::{
    EvaluationExperimentCreateRequest, EvaluationExperimentPrepareRequest,
    EvaluationExperimentPrepareResponse, EvaluationPrepareCase, EvaluationPrepareRevision,
    EvaluationPrepareTarget, EvaluationPrepareWorkspace, EvaluationReportQuery,
    EvaluationTrialStartRequest, EvaluationTrialStartResponse,
};
pub use assessment::{
    ASSESSMENT_SCHEMA_VERSION, CausalStrength, ComparisonArm, ComparisonReport,
    EvidenceAvailability, EvidenceKind, EvidenceRef, JudgmentExecutionObservation,
    JudgmentExecutionStatus, Measurement, MeasurementStatus, TrialObservation, TrialStatus,
    build_comparison_for_plan, render_markdown,
};
pub use bootstrap::{
    EVALUATION_ADAPTER_PROFILE_VERSION, EvaluationBootstrapError, EvaluationTrialStartPlan,
    PreparedSkillIdentity, build_prepared_experiment_spec, prepare_trial_start,
    prepared_cache_policy_identity, prepared_experiment_id, prepared_request_matches_spec,
};
pub use database::DatabaseEvaluationService;
pub use durable::{
    DatabaseEvaluationPlanStore, EvaluationExperimentRecord, EvaluationPersistenceError,
    EvaluationTrialBindingRecord,
};
pub use execution::{
    DatabaseEvaluationObservationStore, EVALUATION_EXECUTION_SCHEMA_VERSION,
    EvaluationAdmissionMarker, EvaluationExecutionError, EvaluationObservationRecord,
    EvaluationObservationRequest, EvaluationPolicyFingerprintInput, EvaluationRunAdmission,
    EvaluationSkillRevision, NO_SKILL_CONTENT_HASH, NO_SKILL_REVISION_ID, apply_context_evidence,
    apply_inference_evidence, apply_tool_outcome_evidence, content_fingerprint,
    evaluation_component_idempotency_key, evaluation_policy_fingerprint, is_no_skill_revision,
    prompt_context_fingerprint, prompt_only_snapshot_envelope, prompt_policy_fingerprint,
    terminal_run_observation,
};
pub use execution_config::{
    EVALUATION_EXECUTION_CONFIG_SCHEMA_VERSION, EVALUATION_RUNTIME_CONTRACT_VERSION,
    EvaluationExecutionConfig, FrozenRoundBudget, InstructionOnlyRuntimeConfig,
};
pub use experiment::{
    DataIsolation, EXPERIMENT_SCHEMA_VERSION, EvaluationBudget, EvaluationCase,
    EvaluationJudgmentPolicy, EvaluationTarget, EvaluationTargetKind, ExperimentSpec,
    FrozenConditions, FrozenSkillRoutingPolicy, FrozenWorkspaceExecution,
    JUDGMENT_POLICY_SCHEMA_VERSION, MemoryIsolation, RevisionRef, SNAPSHOT_ENVELOPE_SCHEMA_VERSION,
    SnapshotEnvelope, TrialOrder, TrialUnit,
};
pub use materialization::{
    DatabaseMaterializationReceiptStore, MATERIALIZATION_RECEIPT_SCHEMA_VERSION,
    MaterializationComponentKind, MaterializationOutcome, MaterializationReceiptError,
    MaterializationReceiptRecord, MaterializationReceiptRequest, MaterializationValidationError,
    TrustedMaterializerContext, required_components_for_spec, validate_receipt_set,
};
pub use noop::UnconfiguredEvaluationService;
pub use projection::{
    DatabaseEvaluationProjectionStore, EvaluationExperimentProjection, EvaluationProjectionError,
    EvaluationTrialLifecycle, EvaluationTrialProjection,
};
pub use report::{
    EVALUATION_REPORT_RENDERER_VERSION, EVALUATION_REPORT_SCHEMA_VERSION, EvaluationReportArtifact,
    EvaluationReportCoverage, EvaluationReportJudgmentSummary, EvaluationReportJudgmentTrial,
    EvaluationReportManifest, EvaluationReportObservationRef, build_report_artifact,
    validate_report_label,
};
pub use service::EvaluationService;
pub use types::*;

pub use task_assessment::{
    TASK_ASSESSMENT_SCHEMA_VERSION, TOOL_INVOCATION_COVERAGE_SCHEMA_VERSION, TaskAssessmentError,
    TaskAssessmentOutcome, TaskAssessmentRecord, TaskAssessmentResult,
    TaskAssessmentUnavailableReason, ToolInvocationCoverageProof,
};

#[cfg(test)]
pub(crate) mod test_support {
    use crate as services;
    include!("../../tests/fixtures/evaluation_execution_config.rs");
}

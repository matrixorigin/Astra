pub mod api;
pub mod assessment;
pub mod database;
pub mod durable;
pub mod execution;
pub mod experiment;
pub mod materialization;
pub mod noop;
pub mod projection;
pub mod report;
pub mod service;
pub mod types;
pub mod utils;

pub use api::{EvaluationExperimentCreateRequest, EvaluationReportQuery};
pub use assessment::{
    ASSESSMENT_SCHEMA_VERSION, CausalStrength, ComparisonArm, ComparisonReport,
    EvidenceAvailability, EvidenceKind, EvidenceRef, Measurement, MeasurementStatus,
    TrialObservation, TrialStatus, build_comparison_for_plan, render_markdown,
};
pub use database::DatabaseEvaluationService;
pub use durable::{
    DatabaseEvaluationPlanStore, EvaluationExperimentRecord, EvaluationPersistenceError,
    EvaluationTrialBindingRecord,
};
pub use execution::{
    DatabaseEvaluationObservationStore, EVALUATION_EXECUTION_SCHEMA_VERSION,
    EvaluationAdmissionMarker, EvaluationExecutionError, EvaluationObservationRecord,
    EvaluationObservationRequest, EvaluationRunAdmission, EvaluationSkillRevision,
    content_fingerprint, evaluation_component_idempotency_key, prompt_context_fingerprint,
    prompt_only_snapshot_envelope, prompt_policy_fingerprint, terminal_run_observation,
};
pub use experiment::{
    DataIsolation, EXPERIMENT_SCHEMA_VERSION, EvaluationBudget, EvaluationCase, EvaluationTarget,
    EvaluationTargetKind, ExperimentSpec, FrozenConditions, MemoryIsolation, RevisionRef,
    SNAPSHOT_ENVELOPE_SCHEMA_VERSION, SnapshotEnvelope, TrialOrder, TrialUnit,
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
    EvaluationReportCoverage, EvaluationReportManifest, EvaluationReportObservationRef,
    build_report_artifact, validate_report_label,
};
pub use service::EvaluationService;
pub use types::*;

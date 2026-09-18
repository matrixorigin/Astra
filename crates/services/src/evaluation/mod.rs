pub mod assessment;
pub mod database;
pub mod durable;
pub mod experiment;
pub mod materialization;
pub mod noop;
pub mod service;
pub mod types;
pub mod utils;

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
pub use service::EvaluationService;
pub use types::*;

//! Generic, execution-independent evaluation specifications.
//!
//! This is the first boundary of the Eval system: it freezes what is being
//! compared and how many controlled trial units may be dispatched. It does
//! not run an agent, own a worker, or decide whether a candidate is adopted.
//! Durable scheduling can therefore reuse Work/Run without inventing a
//! Skill-specific loop.

use super::assessment::ComparisonArm;
use astra_core::composite_snapshot::CompositeSnapshot;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use uuid::Uuid;

pub const EXPERIMENT_SCHEMA_VERSION: u32 = 1;
pub const SNAPSHOT_ENVELOPE_SCHEMA_VERSION: u32 = 1;
const MAX_CASES: usize = 10_000;
const MAX_REPETITIONS: u32 = 100;
const MAX_TRIALS: u64 = 100_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationTargetKind {
    Prompt,
    Skill,
    ToolPolicy,
    ModelProvider,
    MemoryPolicy,
    Workflow,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevisionRef {
    pub revision_id: String,
    pub content_hash: String,
    /// Frozen execution material for adapters that need bytes at start time.
    /// Prompt preparation stores the exact text here; Skill preparation keeps
    /// this empty and resolves the owner-scoped immutable version by ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationTarget {
    pub kind: EvaluationTargetKind,
    pub baseline: RevisionRef,
    pub candidate: RevisionRef,
    /// Owner-scoped Skill name for the instruction-only Skill adapter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationCase {
    pub case_id: String,
    pub input_snapshot_ref: String,
    pub input_content_hash: String,
    pub verifier_id: String,
    pub verifier_version: String,
    pub holdout: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_verifier: Option<super::task_verifier::TaskVerifierSpec>,
    /// Frozen text for the first prompt/Skill adapter. Generic adapters may
    /// use only the content hash until they provide their own materializer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_content: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenConditions {
    pub execution_config: super::execution_config::EvaluationExecutionConfig,
    /// The executor must reject side effects outside the declared isolation
    /// profile instead of silently comparing different environments.
    pub isolation_profile: String,
    pub model_binding: String,
    pub provider_binding: String,
    pub context_snapshot_hash: String,
    pub tool_policy_hash: String,
    pub cache_policy: String,
    pub memory_isolation: MemoryIsolation,
    pub data_isolation: DataIsolation,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MemoryIsolation {
    Disabled,
    BranchPerTrial { base_snapshot_ref: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DataIsolation {
    Disabled,
    MatrixOneBranchPerTrial { base_snapshot_ref: String },
}

/// A verified identity envelope for the state a trial actually starts from.
///
/// The envelope is metadata around the existing [`CompositeSnapshot`]; it does
/// not duplicate checkpoints or grant access to any component. A materializer
/// must attach owner/trial-scoped receipts before a Memory or Data component is
/// considered isolated. `snapshot_id` is an address and coordination key;
/// `snapshot_fingerprint` is the content identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotEnvelope {
    pub schema_version: u32,
    /// UUIDv7 address for cross-edge lookup and idempotency.
    pub snapshot_id: String,
    pub owner_id: String,
    pub experiment_id: String,
    pub trial_id: Option<String>,
    pub composite: CompositeSnapshot,
    pub context_snapshot_hash: String,
    pub policy_snapshot_hash: String,
    pub snapshot_fingerprint: String,
}

impl SnapshotEnvelope {
    /// Wrap a composite snapshot with an owner/trial-scoped immutable identity.
    /// The envelope address is intentionally separate from the composite's
    /// existing snapshot index identity; wrapping must not invalidate old
    /// snapshot diffs or checkpoint references.
    pub fn new(
        owner_id: impl Into<String>,
        experiment_id: impl Into<String>,
        trial_id: Option<String>,
        composite: CompositeSnapshot,
        context_snapshot_hash: impl Into<String>,
        policy_snapshot_hash: impl Into<String>,
    ) -> Result<Self, String> {
        let snapshot_id = Uuid::now_v7().to_string();
        let mut envelope = Self {
            schema_version: SNAPSHOT_ENVELOPE_SCHEMA_VERSION,
            snapshot_id,
            owner_id: owner_id.into(),
            experiment_id: experiment_id.into(),
            trial_id,
            composite,
            context_snapshot_hash: context_snapshot_hash.into(),
            policy_snapshot_hash: policy_snapshot_hash.into(),
            snapshot_fingerprint: String::new(),
        };
        envelope.validate_without_fingerprint()?;
        envelope.snapshot_fingerprint = envelope.computed_fingerprint()?;
        Ok(envelope)
    }

    /// Validate identity and references without asserting external materialization.
    pub fn validate(&self) -> Result<(), String> {
        self.validate_without_fingerprint()?;
        let computed = self.computed_fingerprint()?;
        if self.snapshot_fingerprint != computed {
            return Err(format!(
                "snapshot fingerprint mismatch: expected {computed}, found {}",
                self.snapshot_fingerprint
            ));
        }
        Ok(())
    }

    /// Validate the envelope against the caller's expected ownership and
    /// execution binding. This proves identity agreement, not provider-side
    /// authorization; repositories and materializers must still enforce ACLs.
    pub fn validate_for(
        &self,
        expected_owner_id: &str,
        expected_experiment_id: &str,
        expected_trial_id: Option<&str>,
        expected_session_id: Option<&str>,
    ) -> Result<(), String> {
        self.validate()?;
        if self.owner_id != expected_owner_id {
            return Err("snapshot owner does not match the request owner".to_string());
        }
        if self.experiment_id != expected_experiment_id {
            return Err("snapshot experiment does not match the request experiment".to_string());
        }
        if self.trial_id.as_deref() != expected_trial_id {
            return Err("snapshot trial does not match the requested trial".to_string());
        }
        if let Some(expected_session_id) = expected_session_id
            && self.composite.session_id != expected_session_id
        {
            return Err("snapshot session does not match the requested session".to_string());
        }
        Ok(())
    }

    pub fn computed_fingerprint(&self) -> Result<String, String> {
        let mut refs = self
            .composite
            .refs
            .iter()
            .cloned()
            .map(|reference| match reference {
                astra_core::composite_snapshot::SnapshotRef::DataSnapshot(mut data) => {
                    // A creation timestamp helps operations locate a database
                    // snapshot, but is not part of its content identity.
                    data.timestamp = None;
                    astra_core::composite_snapshot::SnapshotRef::DataSnapshot(data)
                }
                other => other,
            })
            .collect::<Vec<_>>();
        refs.sort_by_key(snapshot_ref_rank);
        let payload = serde_json::json!({
            "schema_version": self.schema_version,
            "owner_id": self.owner_id,
            "experiment_id": self.experiment_id,
            "trial_id": self.trial_id,
            "composite": {
                "session_id": self.composite.session_id,
                "turn": self.composite.turn,
                "refs": refs,
            },
            "context_snapshot_hash": self.context_snapshot_hash,
            "policy_snapshot_hash": self.policy_snapshot_hash,
        });
        let canonical = astra_core::canonical_json_string(&payload);
        let digest = Sha256::digest(canonical.as_bytes());
        Ok(format!("sha256:{digest:x}"))
    }

    fn validate_without_fingerprint(&self) -> Result<(), String> {
        if self.schema_version != SNAPSHOT_ENVELOPE_SCHEMA_VERSION {
            return Err(format!(
                "unsupported snapshot envelope schema version {}",
                self.schema_version
            ));
        }
        let parsed_id = Uuid::parse_str(&self.snapshot_id)
            .map_err(|error| format!("snapshot_id must be a UUIDv7: {error}"))?;
        if parsed_id.get_version_num() != 7 {
            return Err("snapshot_id must be a UUIDv7".to_string());
        }
        for (field, value) in [
            ("owner_id", &self.owner_id),
            ("experiment_id", &self.experiment_id),
            ("context_snapshot_hash", &self.context_snapshot_hash),
            ("policy_snapshot_hash", &self.policy_snapshot_hash),
        ] {
            if value.trim().is_empty() {
                return Err(format!("{field} must not be empty"));
            }
        }
        validate_identifier("experiment_id", &self.experiment_id)?;
        if let Some(trial_id) = &self.trial_id
            && (trial_id.is_empty()
                || trial_id.len() > 128
                || !trial_id.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':')
                }))
        {
            return Err("trial_id contains an unsupported character".to_string());
        }
        if self.composite.snapshot_id.trim().is_empty() {
            return Err("composite snapshot_id must not be empty".to_string());
        }
        if self.composite.session_id.trim().is_empty() {
            return Err("composite session_id must not be empty".to_string());
        }
        if self.composite.created_at.trim().is_empty() {
            return Err("composite created_at must not be empty".to_string());
        }
        let mut dimensions = HashSet::new();
        for reference in &self.composite.refs {
            let dimension = match reference {
                astra_core::composite_snapshot::SnapshotRef::SessionState(_) => "session",
                astra_core::composite_snapshot::SnapshotRef::DataSnapshot(_) => "data",
                astra_core::composite_snapshot::SnapshotRef::MemorySnapshot(_) => "memory",
                astra_core::composite_snapshot::SnapshotRef::GitCommit(_) => "git",
                astra_core::composite_snapshot::SnapshotRef::WorkspaceState(_) => "workspace",
            };
            if !dimensions.insert(dimension) {
                return Err(format!("duplicate snapshot dimension `{dimension}`"));
            }
        }
        Ok(())
    }
}

fn snapshot_ref_rank(reference: &astra_core::composite_snapshot::SnapshotRef) -> u8 {
    match reference {
        astra_core::composite_snapshot::SnapshotRef::SessionState(_) => 0,
        astra_core::composite_snapshot::SnapshotRef::DataSnapshot(_) => 1,
        astra_core::composite_snapshot::SnapshotRef::MemorySnapshot(_) => 2,
        astra_core::composite_snapshot::SnapshotRef::GitCommit(_) => 3,
        astra_core::composite_snapshot::SnapshotRef::WorkspaceState(_) => 4,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationBudget {
    pub max_trials: u32,
    pub max_concurrency: u16,
    pub max_wall_time_secs: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TrialOrder {
    BaselineFirst,
    CandidateFirst,
    /// Deterministically interleave pairs using a recorded seed. This is
    /// reproducible without depending on process-local randomness.
    Balanced {
        seed: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentSpec {
    pub schema_version: u32,
    pub experiment_id: String,
    pub target: EvaluationTarget,
    pub cases: Vec<EvaluationCase>,
    pub repetitions: u32,
    pub order: TrialOrder,
    pub conditions: FrozenConditions,
    pub budget: EvaluationBudget,
    /// Present only when the server's user-intent prepare adapter produced
    /// this spec. Raw registration intentionally has no preparation marker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter_profile_version: Option<String>,
    /// Frozen metric requirements for every planned trial.
    pub measurement_profile: super::measurement_profile::MeasurementProfile,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrialUnit {
    pub trial_id: String,
    pub sequence: u32,
    pub spec_fingerprint: String,
    pub experiment_id: String,
    pub case_id: String,
    pub arm: ComparisonArm,
    pub repetition: u32,
    pub input_snapshot_ref: String,
    pub input_content_hash: String,
    pub verifier_id: String,
    pub verifier_version: String,
    pub holdout: bool,
    pub memory_base_snapshot_ref: Option<String>,
    pub data_base_snapshot_ref: Option<String>,
}

impl ExperimentSpec {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != EXPERIMENT_SCHEMA_VERSION {
            return Err(format!(
                "unsupported experiment schema version {}",
                self.schema_version
            ));
        }
        validate_identifier("experiment_id", &self.experiment_id)?;
        validate_revision("baseline", &self.target.baseline)?;
        validate_revision("candidate", &self.target.candidate)?;
        match (&self.target.kind, self.target.skill_name.as_deref()) {
            (EvaluationTargetKind::Skill, Some(name)) if !name.trim().is_empty() => {}
            (EvaluationTargetKind::Skill, _) => {
                return Err("Skill targets require an owner-scoped skill_name".to_string());
            }
            (EvaluationTargetKind::Prompt, Some(_)) => {
                return Err("Prompt targets must not carry skill_name".to_string());
            }
            (_, _) => {}
        }
        if self.cases.is_empty() {
            return Err("at least one evaluation case is required".to_string());
        }
        if self.cases.len() > MAX_CASES {
            return Err(format!("too many evaluation cases (max {MAX_CASES})"));
        }
        if self.repetitions == 0 || self.repetitions > MAX_REPETITIONS {
            return Err(format!(
                "repetitions must be between 1 and {MAX_REPETITIONS}"
            ));
        }
        let mut case_ids = HashSet::with_capacity(self.cases.len());
        for case in &self.cases {
            validate_identifier("case_id", &case.case_id)?;
            if !case_ids.insert(&case.case_id) {
                return Err(format!("duplicate evaluation case `{}`", case.case_id));
            }
            for (field, value) in [
                ("input_snapshot_ref", &case.input_snapshot_ref),
                ("input_content_hash", &case.input_content_hash),
                ("verifier_id", &case.verifier_id),
                ("verifier_version", &case.verifier_version),
            ] {
                if value.trim().is_empty() {
                    return Err(format!(
                        "{field} must not be empty for case `{}`",
                        case.case_id
                    ));
                }
            }
            if let Some(content) = case.input_content.as_deref()
                && content.trim().is_empty()
            {
                return Err(format!(
                    "input_content must not be empty for case `{}`",
                    case.case_id
                ));
            }
            if let Some(verifier) = &case.task_verifier {
                verifier.validate()?;
                if verifier.implementation_id != case.verifier_id
                    || verifier.implementation_version != case.verifier_version
                {
                    return Err("case verifier identity does not match frozen task verifier".into());
                }
            }
        }
        self.conditions.execution_config.validate()?;
        let config = &self.conditions.execution_config;
        if config.runtime.round_budget_by_case.len() != case_ids.len()
            || !config
                .runtime
                .round_budget_by_case
                .keys()
                .all(|id| case_ids.contains(id))
        {
            return Err(
                "execution config round budgets must exactly cover evaluation cases".into(),
            );
        }
        if config.model.offering_id != self.conditions.model_binding
            || config.model.provider != self.conditions.provider_binding
            || super::bootstrap::prepared_cache_policy_identity(
                &config.model.provider,
                config.model.cache_capability.as_ref(),
            ) != self.conditions.cache_policy
        {
            return Err("frozen model bindings must match execution config".into());
        }
        for (field, value) in [
            ("isolation_profile", &self.conditions.isolation_profile),
            ("model_binding", &self.conditions.model_binding),
            ("provider_binding", &self.conditions.provider_binding),
            (
                "context_snapshot_hash",
                &self.conditions.context_snapshot_hash,
            ),
            ("tool_policy_hash", &self.conditions.tool_policy_hash),
            ("cache_policy", &self.conditions.cache_policy),
        ] {
            if value.trim().is_empty() {
                return Err(format!("{field} must not be empty"));
            }
        }
        if let MemoryIsolation::BranchPerTrial { base_snapshot_ref } =
            &self.conditions.memory_isolation
            && base_snapshot_ref.trim().is_empty()
        {
            return Err("memory branch base_snapshot_ref must not be empty".to_string());
        }
        if let DataIsolation::MatrixOneBranchPerTrial { base_snapshot_ref } =
            &self.conditions.data_isolation
            && base_snapshot_ref.trim().is_empty()
        {
            return Err("MatrixOne branch base_snapshot_ref must not be empty".to_string());
        }
        let expected = self
            .cases
            .len()
            .checked_mul(self.repetitions as usize)
            .and_then(|count| count.checked_mul(2))
            .ok_or_else(|| "trial count overflow".to_string())?;
        if expected == 0 || expected as u64 > MAX_TRIALS {
            return Err(format!("too many planned trials (max {MAX_TRIALS})"));
        }
        if self.budget.max_trials < expected as u32 {
            return Err(format!(
                "budget max_trials {} is below planned trial count {expected}",
                self.budget.max_trials
            ));
        }
        if self.budget.max_trials == 0 || self.budget.max_trials as u64 > MAX_TRIALS {
            return Err(format!(
                "budget max_trials must be between 1 and {MAX_TRIALS}"
            ));
        }
        if self.budget.max_concurrency == 0 {
            return Err("budget max_concurrency must be greater than zero".to_string());
        }
        if self.budget.max_wall_time_secs == 0 {
            return Err("budget max_wall_time_secs must be greater than zero".to_string());
        }
        if let Some(version) = self.adapter_profile_version.as_deref()
            && version.trim().is_empty()
        {
            return Err("adapter_profile_version must not be empty".to_string());
        }
        let max_wall_time_secs = i64::try_from(self.budget.max_wall_time_secs).map_err(|_| {
            "budget max_wall_time_secs exceeds the supported duration range".to_string()
        })?;
        if chrono::Duration::try_seconds(max_wall_time_secs).is_none() {
            return Err(
                "budget max_wall_time_secs exceeds the supported duration range".to_string(),
            );
        }
        Ok(())
    }

    pub fn planned_trial_count(&self) -> Result<usize, String> {
        self.validate()?;
        Ok(self.cases.len() * self.repetitions as usize * 2)
    }

    /// Fingerprint the complete validated specification. Persistence can use
    /// this to reject reusing an experiment ID for a different definition.
    pub fn spec_fingerprint(&self) -> Result<String, String> {
        self.validate()?;
        let canonical = serde_json::to_vec(self)
            .map_err(|error| format!("failed to encode experiment specification: {error}"))?;
        let digest = Sha256::digest(canonical);
        Ok(format!("sha256:{digest:x}"))
    }

    /// Expand the frozen specification into stable trial identities. The
    /// durable scheduler must persist these identities before creating a run
    /// and enforce the fingerprint; planning alone does not prevent duplicate
    /// execution or billing.
    pub fn plan_trials(&self) -> Result<Vec<TrialUnit>, String> {
        self.plan_trials_internal(None)
    }

    /// Expand trials while bounding the work and serialized payload retained
    /// by a durable caller. The budget is checked as each trial is built, so a
    /// legal specification with very large repeated case fields cannot first
    /// allocate an unbounded vector and only then be rejected.
    pub fn plan_trials_with_limits(
        &self,
        max_trials: usize,
        max_serialized_bytes: usize,
    ) -> Result<Vec<TrialUnit>, String> {
        self.plan_trials_internal(Some((max_trials, max_serialized_bytes)))
    }

    /// Validate one persisted trial against the frozen specification without
    /// expanding and sorting every other trial. Runtime admission and
    /// settlement call this hot path for each Run; the full plan expansion is
    /// reserved for registration/list integrity checks.
    pub fn validate_trial_identity(&self, trial: &TrialUnit) -> Result<(), String> {
        self.validate()?;
        let spec_fingerprint = self.spec_fingerprint()?;
        let planned_trial_count = self.planned_trial_count()?;
        if trial.sequence == 0 || trial.sequence as usize > planned_trial_count {
            return Err(format!(
                "trial {} has sequence {} outside the planned range",
                trial.trial_id, trial.sequence
            ));
        }
        if trial.experiment_id != self.experiment_id || trial.spec_fingerprint != spec_fingerprint {
            return Err(format!(
                "trial {} is not bound to this experiment identity",
                trial.trial_id
            ));
        }
        let case = self
            .cases
            .iter()
            .find(|case| case.case_id == trial.case_id)
            .ok_or_else(|| format!("trial {} references an unknown case", trial.trial_id))?;
        if trial.repetition >= self.repetitions {
            return Err(format!(
                "trial {} repetition {} is outside the experiment",
                trial.trial_id, trial.repetition
            ));
        }
        let expected_trial_id = trial_id(
            &spec_fingerprint,
            &self.experiment_id,
            &trial.case_id,
            trial.repetition,
            &trial.arm,
        );
        if trial.trial_id != expected_trial_id {
            return Err(format!(
                "trial {} does not match its case/repetition/arm identity",
                trial.trial_id
            ));
        }
        let expected_memory_base = match &self.conditions.memory_isolation {
            MemoryIsolation::Disabled => None,
            MemoryIsolation::BranchPerTrial { base_snapshot_ref } => {
                Some(base_snapshot_ref.as_str())
            }
        };
        let expected_data_base = match &self.conditions.data_isolation {
            DataIsolation::Disabled => None,
            DataIsolation::MatrixOneBranchPerTrial { base_snapshot_ref } => {
                Some(base_snapshot_ref.as_str())
            }
        };
        if trial.input_snapshot_ref != case.input_snapshot_ref
            || trial.input_content_hash != case.input_content_hash
            || trial.verifier_id != case.verifier_id
            || trial.verifier_version != case.verifier_version
            || trial.holdout != case.holdout
            || trial.memory_base_snapshot_ref.as_deref() != expected_memory_base
            || trial.data_base_snapshot_ref.as_deref() != expected_data_base
        {
            return Err(format!(
                "trial {} does not match its frozen case or isolation conditions",
                trial.trial_id
            ));
        }
        Ok(())
    }

    /// Return the deterministic sequence assigned by `plan_trials` without
    /// allocating the complete `TrialUnit` vector. Balanced ordering still
    /// scans the bounded case/repetition space, but retains only the target's
    /// rank and therefore cannot duplicate the persisted plan payload.
    pub fn canonical_trial_sequence(&self, trial: &TrialUnit) -> Result<u32, String> {
        self.validate_trial_identity(trial)?;
        let case_index = self
            .cases
            .iter()
            .position(|case| case.case_id == trial.case_id)
            .ok_or_else(|| format!("trial {} references an unknown case", trial.trial_id))?;
        let arm_index = match &trial.arm {
            ComparisonArm::Baseline => 0_usize,
            ComparisonArm::Candidate => 1_usize,
        };
        let target_original_index =
            (case_index * self.repetitions as usize + trial.repetition as usize) * 2 + arm_index;
        let rank = match &self.order {
            TrialOrder::BaselineFirst | TrialOrder::CandidateFirst => {
                let case_rank = self
                    .cases
                    .iter()
                    .filter(|case| case.case_id < trial.case_id)
                    .count();
                let arm_rank = match (&self.order, &trial.arm) {
                    (TrialOrder::BaselineFirst, ComparisonArm::Baseline)
                    | (TrialOrder::CandidateFirst, ComparisonArm::Candidate) => 0,
                    _ => 1,
                };
                (case_rank * self.repetitions as usize + trial.repetition as usize) * 2 + arm_rank
            }
            TrialOrder::Balanced { seed } => {
                let target_pair_key =
                    stable_pair_key(*seed, &trial.case_id, trial.repetition, b"pair");
                let target_arm_rank = {
                    let candidate_first =
                        stable_pair_key(*seed, &trial.case_id, trial.repetition, b"orientation")
                            & 1
                            == 1;
                    match (&trial.arm, candidate_first) {
                        (ComparisonArm::Baseline, false) | (ComparisonArm::Candidate, true) => 0,
                        _ => 1,
                    }
                };
                let mut preceding = 0_usize;
                for (other_case_index, case) in self.cases.iter().enumerate() {
                    for repetition in 0..self.repetitions {
                        let pair_key = stable_pair_key(*seed, &case.case_id, repetition, b"pair");
                        let candidate_first =
                            stable_pair_key(*seed, &case.case_id, repetition, b"orientation") & 1
                                == 1;
                        for (other_arm_index, other_arm) in
                            [ComparisonArm::Baseline, ComparisonArm::Candidate]
                                .into_iter()
                                .enumerate()
                        {
                            let arm_rank = match (&other_arm, candidate_first) {
                                (ComparisonArm::Baseline, false)
                                | (ComparisonArm::Candidate, true) => 0,
                                _ => 1,
                            };
                            let other_original_index = (other_case_index
                                * self.repetitions as usize
                                + repetition as usize)
                                * 2
                                + other_arm_index;
                            if (pair_key, arm_rank) < (target_pair_key, target_arm_rank)
                                || ((pair_key, arm_rank) == (target_pair_key, target_arm_rank)
                                    && other_original_index < target_original_index)
                            {
                                preceding += 1;
                            }
                        }
                    }
                }
                preceding
            }
        };
        u32::try_from(rank + 1).map_err(|_| "trial sequence exceeds u32".to_string())
    }

    /// The earlier arm of this exact frozen case/repetition pair. Sequence
    /// ordering is derived through the same canonical algorithm as planning;
    /// an adjacent trial from another pair is never a predecessor.
    pub fn paired_predecessor(&self, trial: &TrialUnit) -> Result<Option<TrialUnit>, String> {
        if self.canonical_trial_sequence(trial)? != trial.sequence {
            return Err("trial sequence does not match the frozen plan".into());
        }
        let mut paired = trial.clone();
        paired.arm = match trial.arm {
            ComparisonArm::Baseline => ComparisonArm::Candidate,
            ComparisonArm::Candidate => ComparisonArm::Baseline,
        };
        paired.trial_id = trial_id(
            &trial.spec_fingerprint,
            &self.experiment_id,
            &trial.case_id,
            trial.repetition,
            &paired.arm,
        );
        paired.sequence = self.canonical_trial_sequence(&paired)?;
        Ok((paired.sequence < trial.sequence).then_some(paired))
    }

    fn plan_trials_internal(
        &self,
        limits: Option<(usize, usize)>,
    ) -> Result<Vec<TrialUnit>, String> {
        self.validate()?;
        let spec_fingerprint = self.spec_fingerprint()?;
        let memory_base_snapshot_ref = match &self.conditions.memory_isolation {
            MemoryIsolation::Disabled => None,
            MemoryIsolation::BranchPerTrial { base_snapshot_ref } => {
                Some(base_snapshot_ref.clone())
            }
        };
        let data_base_snapshot_ref = match &self.conditions.data_isolation {
            DataIsolation::Disabled => None,
            DataIsolation::MatrixOneBranchPerTrial { base_snapshot_ref } => {
                Some(base_snapshot_ref.clone())
            }
        };
        let planned_trial_count = self.planned_trial_count()?;
        if let Some((max_trials, _)) = limits
            && planned_trial_count > max_trials
        {
            return Err(format!(
                "planned trial count {planned_trial_count} exceeds expansion limit {max_trials}"
            ));
        }
        let mut trials = Vec::with_capacity(planned_trial_count);
        let mut estimated_serialized_bytes = 0_usize;
        for case in &self.cases {
            for repetition in 0..self.repetitions {
                for arm in [ComparisonArm::Baseline, ComparisonArm::Candidate] {
                    let trial = TrialUnit {
                        trial_id: trial_id(
                            &spec_fingerprint,
                            &self.experiment_id,
                            &case.case_id,
                            repetition,
                            &arm,
                        ),
                        sequence: 0,
                        spec_fingerprint: spec_fingerprint.clone(),
                        experiment_id: self.experiment_id.clone(),
                        case_id: case.case_id.clone(),
                        arm,
                        repetition,
                        input_snapshot_ref: case.input_snapshot_ref.clone(),
                        input_content_hash: case.input_content_hash.clone(),
                        verifier_id: case.verifier_id.clone(),
                        verifier_version: case.verifier_version.clone(),
                        holdout: case.holdout,
                        memory_base_snapshot_ref: memory_base_snapshot_ref.clone(),
                        data_base_snapshot_ref: data_base_snapshot_ref.clone(),
                    };
                    if let Some((_, max_serialized_bytes)) = limits {
                        // Sequence is assigned after ordering. Reserve a
                        // small digit-length margin so the early check stays
                        // conservative without materializing the full plan.
                        let trial_bytes = serde_json::to_vec(&trial)
                            .map_err(|error| format!("failed to size evaluation trial: {error}"))?
                            .len()
                            .checked_add(16)
                            .ok_or_else(|| "evaluation trial size overflow".to_string())?;
                        estimated_serialized_bytes = estimated_serialized_bytes
                            .checked_add(trial_bytes)
                            .ok_or_else(|| "evaluation plan size overflow".to_string())?;
                        if estimated_serialized_bytes > max_serialized_bytes {
                            return Err(format!(
                                "planned trial payload exceeds expansion limit {max_serialized_bytes} bytes"
                            ));
                        }
                    }
                    trials.push(trial);
                }
            }
        }
        match self.order {
            TrialOrder::BaselineFirst => sort_trials(&mut trials, false, None),
            TrialOrder::CandidateFirst => sort_trials(&mut trials, true, None),
            TrialOrder::Balanced { seed } => sort_trials(&mut trials, false, Some(seed)),
        }
        for (sequence, trial) in trials.iter_mut().enumerate() {
            // Sequence is assigned after ordering and is not part of the
            // idempotency identity.
            trial.sequence = sequence as u32 + 1;
        }
        if let Some((_, max_serialized_bytes)) = limits {
            let actual_serialized_bytes = trials.iter().try_fold(0_usize, |total, trial| {
                let trial_bytes = serde_json::to_vec(trial)
                    .map_err(|error| format!("failed to size evaluation trial: {error}"))?
                    .len();
                total
                    .checked_add(trial_bytes)
                    .ok_or_else(|| "evaluation plan size overflow".to_string())
            })?;
            if actual_serialized_bytes > max_serialized_bytes {
                return Err(format!(
                    "planned trial payload is {actual_serialized_bytes} bytes; expansion limit is {max_serialized_bytes}"
                ));
            }
        }
        Ok(trials)
    }
}

fn validate_revision(label: &str, revision: &RevisionRef) -> Result<(), String> {
    validate_identifier(&format!("{label}_revision_id"), &revision.revision_id)?;
    if revision.content_hash.trim().is_empty() {
        return Err(format!("{label} content_hash must not be empty"));
    }
    if let Some(content) = revision.content.as_deref()
        && content.trim().is_empty()
    {
        return Err(format!("{label} content must not be empty"));
    }
    Ok(())
}

fn validate_identifier(label: &str, value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > 128 {
        return Err(format!("{label} must be 1..=128 characters"));
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(format!("{label} contains an unsupported character"));
    }
    Ok(())
}

fn trial_id(
    spec_fingerprint: &str,
    experiment_id: &str,
    case_id: &str,
    repetition: u32,
    arm: &ComparisonArm,
) -> String {
    let arm = match arm {
        ComparisonArm::Baseline => "baseline",
        ComparisonArm::Candidate => "candidate",
    };
    // A digest over length-delimited fields avoids collisions caused by using
    // a human-readable separator in otherwise valid identifiers.
    let canonical = format!(
        "{}:{}{}:{}{}:{}{}:{}:{}",
        spec_fingerprint.len(),
        spec_fingerprint,
        experiment_id.len(),
        experiment_id,
        case_id.len(),
        case_id,
        repetition,
        arm.len(),
        arm
    );
    let digest = Sha256::digest(canonical.as_bytes());
    format!("trial:sha256:{digest:x}")
}

fn sort_trials(trials: &mut [TrialUnit], candidate_first: bool, seed: Option<u64>) {
    trials.sort_by(|left, right| {
        if let Some(seed) = seed {
            let pair_key = |trial: &TrialUnit| {
                stable_pair_key(seed, &trial.case_id, trial.repetition, b"pair")
            };
            let orientation = |trial: &TrialUnit| {
                stable_pair_key(seed, &trial.case_id, trial.repetition, b"orientation") & 1
            };
            let arm_rank = |trial: &TrialUnit| {
                let candidate_first = orientation(trial) == 1;
                match (&trial.arm, candidate_first) {
                    (ComparisonArm::Baseline, false) | (ComparisonArm::Candidate, true) => 0_u8,
                    _ => 1_u8,
                }
            };
            pair_key(left)
                .cmp(&pair_key(right))
                .then_with(|| arm_rank(left).cmp(&arm_rank(right)))
        } else {
            let arm_rank = |trial: &TrialUnit| match (&trial.arm, candidate_first) {
                (ComparisonArm::Baseline, false) | (ComparisonArm::Candidate, true) => 0_u8,
                _ => 1_u8,
            };
            left.case_id
                .cmp(&right.case_id)
                .then_with(|| left.repetition.cmp(&right.repetition))
                .then_with(|| arm_rank(left).cmp(&arm_rank(right)))
        }
    });
}

fn stable_pair_key(seed: u64, case_id: &str, repetition: u32, domain: &[u8]) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(seed.to_be_bytes());
    hasher.update((case_id.len() as u64).to_be_bytes());
    hasher.update(case_id.as_bytes());
    hasher.update(repetition.to_be_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(order: TrialOrder) -> ExperimentSpec {
        ExperimentSpec {
            schema_version: EXPERIMENT_SCHEMA_VERSION,
            experiment_id: "exp-1".to_string(),
            target: EvaluationTarget {
                kind: EvaluationTargetKind::Skill,
                baseline: RevisionRef {
                    revision_id: "skill-v1".to_string(),
                    content_hash: "sha256:old".to_string(),
                    content: None,
                },
                candidate: RevisionRef {
                    revision_id: "skill-v2".to_string(),
                    content_hash: "sha256:new".to_string(),
                    content: None,
                },
                skill_name: Some("sample-skill".to_string()),
            },
            cases: vec![EvaluationCase {
                case_id: "case-a".to_string(),
                input_snapshot_ref: "snapshot-a".to_string(),
                input_content_hash: "sha256:input".to_string(),
                verifier_id: "verifier".to_string(),
                verifier_version: "1".to_string(),
                holdout: false,
                task_verifier: None,
                input_content: None,
            }],
            repetitions: 2,
            order,
            conditions: FrozenConditions {
                execution_config: crate::evaluation::test_support::execution_config(
                    "model-v1",
                    "provider-v1",
                    "case-a",
                ),
                isolation_profile: "prompt_only_private".to_string(),
                model_binding: "model-v1".to_string(),
                provider_binding: "provider-v1".to_string(),
                context_snapshot_hash: "sha256:context".to_string(),
                tool_policy_hash: "sha256:tools".to_string(),
                cache_policy: "provider_default_recorded".to_string(),
                memory_isolation: MemoryIsolation::Disabled,
                data_isolation: DataIsolation::Disabled,
            },
            budget: EvaluationBudget {
                max_trials: 4,
                max_concurrency: 2,
                max_wall_time_secs: 300,
            },
            adapter_profile_version: None,
            measurement_profile:
                crate::evaluation::measurement_profile::MeasurementProfile::InstructionOnlyV1,
        }
    }

    #[test]
    fn execution_config_is_required_and_round_budgets_cover_exact_case_set() {
        let original = spec(TrialOrder::BaselineFirst);
        let mut encoded = serde_json::to_value(&original).unwrap();
        encoded["conditions"]
            .as_object_mut()
            .unwrap()
            .remove("execution_config");
        assert!(serde_json::from_value::<ExperimentSpec>(encoded).is_err());
        let mut changed = original.clone();
        let budget = changed
            .conditions
            .execution_config
            .runtime
            .round_budget_by_case
            .remove("case-a")
            .unwrap();
        assert!(changed.validate().unwrap_err().contains("exactly cover"));
        changed
            .conditions
            .execution_config
            .runtime
            .round_budget_by_case
            .insert("other-case".into(), budget.clone());
        assert!(changed.validate().unwrap_err().contains("exactly cover"));
        changed
            .conditions
            .execution_config
            .runtime
            .round_budget_by_case
            .insert("case-a".into(), budget);
        assert!(changed.validate().unwrap_err().contains("exactly cover"));
        let mut changed = original.clone();
        changed.conditions.execution_config.runtime_contract_version += 1;
        assert!(changed.validate().unwrap_err().contains("version"));
        let mut changed = original.clone();
        changed.conditions.execution_config.model.offering_id = "other-model".into();
        assert!(changed.validate().unwrap_err().contains("model bindings"));
        let mut changed = original.clone();
        changed.conditions.execution_config.pre_turn_compaction_gate =
            astra_turn_types::auxiliary_execution::AuxiliaryCallGate::Disabled;
        assert_ne!(
            original.spec_fingerprint().unwrap(),
            changed.spec_fingerprint().unwrap()
        );
    }

    #[test]
    fn requires_a_supported_measurement_profile() {
        let mut value = serde_json::to_value(spec(TrialOrder::BaselineFirst)).unwrap();
        value.as_object_mut().unwrap().remove("measurement_profile");
        assert!(serde_json::from_value::<ExperimentSpec>(value.clone()).is_err());
        value["measurement_profile"] = serde_json::json!("unsupported");
        assert!(serde_json::from_value::<ExperimentSpec>(value).is_err());
    }

    #[test]
    fn validates_and_expands_pairwise_trials() {
        let spec = spec(TrialOrder::BaselineFirst);
        assert_eq!(spec.planned_trial_count().unwrap(), 4);
        let trials = spec.plan_trials().unwrap();
        assert_eq!(trials.len(), 4);
        assert!(matches!(trials[0].arm, ComparisonArm::Baseline));
        assert!(matches!(trials[1].arm, ComparisonArm::Candidate));
        assert_eq!(trials[0].sequence, 1);
        assert_eq!(trials[1].sequence, 2);
        assert!(
            trials
                .iter()
                .all(|trial| trial.input_content_hash == "sha256:input")
        );
    }

    #[test]
    fn balanced_order_is_reproducible_and_sequence_is_unique() {
        let spec = spec(TrialOrder::Balanced { seed: 42 });
        let first = spec.plan_trials().unwrap();
        let second = spec.plan_trials().unwrap();
        assert_eq!(first, second);
        let ids = first
            .iter()
            .map(|trial| trial.trial_id.as_str())
            .collect::<HashSet<_>>();
        assert_eq!(ids.len(), first.len());
        let fingerprint = spec.spec_fingerprint().unwrap();
        assert!(
            first
                .iter()
                .all(|trial| trial.spec_fingerprint == fingerprint)
        );
        assert!(
            first
                .iter()
                .all(|trial| { spec.canonical_trial_sequence(trial).unwrap() == trial.sequence })
        );
    }

    #[test]
    fn paired_predecessor_uses_frozen_case_repetition_and_order() {
        for order in [
            TrialOrder::BaselineFirst,
            TrialOrder::CandidateFirst,
            TrialOrder::Balanced { seed: 42 },
            TrialOrder::Balanced { seed: 7 },
        ] {
            let mut spec = spec(order);
            let mut other_case = spec.cases[0].clone();
            other_case.case_id = "case-b".into();
            spec.cases.push(other_case);
            let budget = spec
                .conditions
                .execution_config
                .runtime
                .round_budget_by_case["case-a"]
                .clone();
            spec.conditions
                .execution_config
                .runtime
                .round_budget_by_case
                .insert("case-b".into(), budget);
            spec.budget.max_trials = 8;
            let trials = spec.plan_trials().unwrap();
            for trial in &trials {
                let expected = trials.iter().find(|other| {
                    other.case_id == trial.case_id
                        && other.repetition == trial.repetition
                        && other.arm != trial.arm
                        && other.sequence < trial.sequence
                });
                assert_eq!(spec.paired_predecessor(trial).unwrap().as_ref(), expected);
                let mut tampered = trial.clone();
                tampered.sequence = if trial.sequence == 1 { 2 } else { 1 };
                assert!(spec.paired_predecessor(&tampered).is_err());
            }
        }
    }

    #[test]
    fn balanced_order_keeps_each_case_repetition_as_a_pair() {
        let mut spec = spec(TrialOrder::Balanced { seed: 42 });
        spec.cases.push(EvaluationCase {
            case_id: "case-b".to_string(),
            input_snapshot_ref: "snapshot-b".to_string(),
            input_content_hash: "sha256:input-b".to_string(),
            verifier_id: "verifier".to_string(),
            verifier_version: "1".to_string(),
            holdout: true,
            task_verifier: None,
            input_content: None,
        });
        let budget = spec
            .conditions
            .execution_config
            .runtime
            .round_budget_by_case["case-a"]
            .clone();
        spec.conditions
            .execution_config
            .runtime
            .round_budget_by_case
            .insert("case-b".into(), budget);
        spec.budget.max_trials = 8;
        let trials = spec.plan_trials().unwrap();
        for pair in trials.chunks_exact(2) {
            assert_eq!(pair[0].case_id, pair[1].case_id);
            assert_eq!(pair[0].repetition, pair[1].repetition);
            assert_ne!(pair[0].arm, pair[1].arm);
        }
    }

    #[test]
    fn rejects_wall_time_values_that_cannot_be_represented_as_a_duration() {
        let mut spec = spec(TrialOrder::BaselineFirst);
        spec.budget.max_wall_time_secs = u64::MAX;
        let error = spec.validate().expect_err("unrepresentable wall time");
        assert!(error.contains("supported duration range"));

        spec.budget.max_wall_time_secs = i64::MAX as u64;
        let error = spec.validate().expect_err("overflowing wall time");
        assert!(error.contains("supported duration range"));
    }

    #[test]
    fn rejects_duplicate_cases_and_under_budget_specs() {
        let mut duplicate = spec(TrialOrder::CandidateFirst);
        duplicate.cases.push(duplicate.cases[0].clone());
        assert!(duplicate.validate().unwrap_err().contains("duplicate"));

        let mut under_budget = spec(TrialOrder::BaselineFirst);
        under_budget.budget.max_trials = 3;
        assert!(
            under_budget
                .validate()
                .unwrap_err()
                .contains("below planned")
        );
    }

    #[test]
    fn allows_same_revision_for_a_noop_control_and_rejects_unsafe_identifiers() {
        let mut same = spec(TrialOrder::BaselineFirst);
        same.target.candidate = same.target.baseline.clone();
        assert!(same.validate().is_ok());

        let mut unsafe_id = spec(TrialOrder::BaselineFirst);
        unsafe_id.experiment_id = "exp/one".to_string();
        assert!(unsafe_id.validate().unwrap_err().contains("unsupported"));
    }

    #[test]
    fn trial_ids_do_not_collide_when_separator_like_ids_are_used() {
        let mut left = spec(TrialOrder::BaselineFirst);
        left.experiment_id = "a__b".to_string();
        left.cases[0].case_id = "c".to_string();
        let budget = left
            .conditions
            .execution_config
            .runtime
            .round_budget_by_case
            .remove("case-a")
            .unwrap();
        left.conditions
            .execution_config
            .runtime
            .round_budget_by_case
            .insert("c".into(), budget);
        let mut right = spec(TrialOrder::BaselineFirst);
        right.experiment_id = "a".to_string();
        right.cases[0].case_id = "b__c".to_string();
        let budget = right
            .conditions
            .execution_config
            .runtime
            .round_budget_by_case
            .remove("case-a")
            .unwrap();
        right
            .conditions
            .execution_config
            .runtime
            .round_budget_by_case
            .insert("b__c".into(), budget);
        assert_ne!(
            left.plan_trials().unwrap()[0].trial_id,
            right.plan_trials().unwrap()[0].trial_id
        );
    }

    #[test]
    fn memory_branch_is_frozen_and_carried_to_every_trial() {
        let mut spec = spec(TrialOrder::BaselineFirst);
        spec.conditions.memory_isolation = MemoryIsolation::BranchPerTrial {
            base_snapshot_ref: "memoria:snapshot:42".to_string(),
        };
        let trials = spec.plan_trials().unwrap();
        assert!(trials.iter().all(|trial| {
            trial.memory_base_snapshot_ref.as_deref() == Some("memoria:snapshot:42")
        }));
        let mut changed = spec.clone();
        changed.conditions.memory_isolation = MemoryIsolation::BranchPerTrial {
            base_snapshot_ref: "memoria:snapshot:43".to_string(),
        };
        assert_ne!(
            spec.spec_fingerprint().unwrap(),
            changed.spec_fingerprint().unwrap()
        );
    }

    #[test]
    fn matrixone_branch_is_frozen_and_carried_to_every_trial() {
        let mut spec = spec(TrialOrder::BaselineFirst);
        spec.conditions.data_isolation = DataIsolation::MatrixOneBranchPerTrial {
            base_snapshot_ref: "matrixone:snapshot:42".to_string(),
        };
        let trials = spec.plan_trials().unwrap();
        assert!(trials.iter().all(|trial| {
            trial.data_base_snapshot_ref.as_deref() == Some("matrixone:snapshot:42")
        }));
    }

    fn composite_for_snapshot() -> CompositeSnapshot {
        CompositeSnapshot {
            snapshot_id: "legacy-snapshot-id".to_string(),
            session_id: "trial-session".to_string(),
            turn: 3,
            created_at: "2026-09-18T00:00:00Z".to_string(),
            version: 1,
            label: Some("eval baseline".to_string()),
            refs: vec![
                astra_core::composite_snapshot::SnapshotRef::GitCommit(
                    "0123456789abcdef".to_string(),
                ),
                astra_core::composite_snapshot::SnapshotRef::WorkspaceState(
                    "trial-session".to_string(),
                ),
            ],
        }
    }

    #[test]
    fn snapshot_envelope_uses_uuidv7_and_content_fingerprint() {
        let envelope = SnapshotEnvelope::new(
            "user-a",
            "exp-1",
            Some("trial:sha256:abc".to_string()),
            composite_for_snapshot(),
            "sha256:context",
            "sha256:policy",
        )
        .unwrap();
        let id = Uuid::parse_str(&envelope.snapshot_id).unwrap();
        assert_eq!(id.get_version_num(), 7);
        assert_eq!(envelope.composite.snapshot_id, "legacy-snapshot-id");
        assert!(envelope.validate().is_ok());
        assert!(
            envelope
                .validate_for(
                    "user-a",
                    "exp-1",
                    Some("trial:sha256:abc"),
                    Some("trial-session")
                )
                .is_ok()
        );
        let encoded = serde_json::to_string(&envelope).unwrap();
        let restored: SnapshotEnvelope = serde_json::from_str(&encoded).unwrap();
        assert_eq!(restored, envelope);
        assert_eq!(
            envelope.snapshot_fingerprint,
            envelope.computed_fingerprint().unwrap()
        );
        let mut equivalent_composite = composite_for_snapshot();
        equivalent_composite.created_at = "2027-01-01T00:00:00Z".to_string();
        equivalent_composite.version = 99;
        equivalent_composite.label = Some("another display label".to_string());
        equivalent_composite.refs.reverse();
        let equivalent = SnapshotEnvelope::new(
            "user-a",
            "exp-1",
            Some("trial:sha256:abc".to_string()),
            equivalent_composite,
            "sha256:context",
            "sha256:policy",
        )
        .unwrap();
        assert_eq!(
            envelope.snapshot_fingerprint, equivalent.snapshot_fingerprint,
            "address, timestamp, label, version, and ref order are not content identity"
        );
    }

    #[test]
    fn snapshot_envelope_rejects_tampering_and_duplicate_dimensions() {
        let mut envelope = SnapshotEnvelope::new(
            "user-a",
            "exp-1",
            None,
            composite_for_snapshot(),
            "sha256:context",
            "sha256:policy",
        )
        .unwrap();
        envelope.context_snapshot_hash = "sha256:changed".to_string();
        assert!(envelope.validate().unwrap_err().contains("fingerprint"));

        let mut duplicate = SnapshotEnvelope::new(
            "user-a",
            "exp-1",
            None,
            composite_for_snapshot(),
            "sha256:context",
            "sha256:policy",
        )
        .unwrap();
        duplicate
            .composite
            .refs
            .push(astra_core::composite_snapshot::SnapshotRef::GitCommit(
                "fedcba9876543210".to_string(),
            ));
        duplicate.snapshot_fingerprint = duplicate.computed_fingerprint().unwrap();
        assert!(duplicate.validate().unwrap_err().contains("duplicate"));
    }

    #[test]
    fn snapshot_envelope_rejects_non_v7_and_foreign_bindings() {
        let mut envelope = SnapshotEnvelope::new(
            "user-a",
            "exp-1",
            None,
            composite_for_snapshot(),
            "sha256:context",
            "sha256:policy",
        )
        .unwrap();
        envelope.snapshot_id = Uuid::new_v4().to_string();
        assert!(envelope.validate().unwrap_err().contains("UUIDv7"));

        let envelope = SnapshotEnvelope::new(
            "user-a",
            "exp-1",
            None,
            composite_for_snapshot(),
            "sha256:context",
            "sha256:policy",
        )
        .unwrap();
        assert!(
            envelope
                .validate_for("user-b", "exp-1", None, Some("trial-session"))
                .unwrap_err()
                .contains("owner")
        );
        assert!(
            envelope
                .validate_for("user-a", "other-exp", None, Some("trial-session"))
                .unwrap_err()
                .contains("experiment")
        );
        assert!(
            envelope
                .validate_for(
                    "user-a",
                    "exp-1",
                    Some("trial:other"),
                    Some("trial-session")
                )
                .unwrap_err()
                .contains("trial")
        );
        assert!(
            envelope
                .validate_for("user-a", "exp-1", None, Some("other-session"))
                .unwrap_err()
                .contains("session")
        );
    }
}

//! Generic, execution-independent evaluation specifications.
//!
//! This is the first boundary of the Eval system: it freezes what is being
//! compared and how many controlled trial units may be dispatched. It does
//! not run an agent, own a worker, or decide whether a candidate is adopted.
//! Durable scheduling can therefore reuse Work/Run without inventing a
//! Skill-specific loop.

use super::assessment::ComparisonArm;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;

pub const EXPERIMENT_SCHEMA_VERSION: u32 = 1;
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
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationTarget {
    pub kind: EvaluationTargetKind,
    pub baseline: RevisionRef,
    pub candidate: RevisionRef,
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
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenConditions {
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
        {
            if base_snapshot_ref.trim().is_empty() {
                return Err("memory branch base_snapshot_ref must not be empty".to_string());
            }
        }
        if let DataIsolation::MatrixOneBranchPerTrial { base_snapshot_ref } =
            &self.conditions.data_isolation
        {
            if base_snapshot_ref.trim().is_empty() {
                return Err("MatrixOne branch base_snapshot_ref must not be empty".to_string());
            }
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
        let mut trials = Vec::with_capacity(self.planned_trial_count()?);
        for case in &self.cases {
            for repetition in 0..self.repetitions {
                for arm in [ComparisonArm::Baseline, ComparisonArm::Candidate] {
                    trials.push(TrialUnit {
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
                    });
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
        Ok(trials)
    }
}

fn validate_revision(label: &str, revision: &RevisionRef) -> Result<(), String> {
    validate_identifier(&format!("{label}_revision_id"), &revision.revision_id)?;
    if revision.content_hash.trim().is_empty() {
        return Err(format!("{label} content_hash must not be empty"));
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
                },
                candidate: RevisionRef {
                    revision_id: "skill-v2".to_string(),
                    content_hash: "sha256:new".to_string(),
                },
            },
            cases: vec![EvaluationCase {
                case_id: "case-a".to_string(),
                input_snapshot_ref: "snapshot-a".to_string(),
                input_content_hash: "sha256:input".to_string(),
                verifier_id: "verifier".to_string(),
                verifier_version: "1".to_string(),
                holdout: false,
            }],
            repetitions: 2,
            order,
            conditions: FrozenConditions {
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
        }
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
        });
        spec.budget.max_trials = 8;
        let trials = spec.plan_trials().unwrap();
        for pair in trials.chunks_exact(2) {
            assert_eq!(pair[0].case_id, pair[1].case_id);
            assert_eq!(pair[0].repetition, pair[1].repetition);
            assert_ne!(pair[0].arm, pair[1].arm);
        }
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
        let mut right = spec(TrialOrder::BaselineFirst);
        right.experiment_id = "a".to_string();
        right.cases[0].case_id = "b__c".to_string();
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
}

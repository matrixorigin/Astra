//! Generic evidence and comparison primitives for controlled evaluations.
//!
//! This module deliberately has no Skill, prompt, provider, or runner
//! knowledge.  It records what an evaluation observed and what it could not
//! observe.  Product-specific candidate builders and durable trial schedulers
//! should use these types instead of inventing a second report format.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

use super::experiment::ExperimentSpec;

pub const ASSESSMENT_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    Context,
    Trace,
    Journal,
    Artifact,
    Assessment,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceAvailability {
    Available,
    Missing,
    Redacted,
    Revoked,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceRef {
    pub evidence_id: String,
    pub kind: EvidenceKind,
    pub availability: EvidenceAvailability,
    pub content_hash: Option<String>,
    pub locator: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeasurementStatus {
    Observed,
    Missing,
    Unavailable,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Measurement {
    pub name: String,
    pub value: Option<f64>,
    pub unit: String,
    pub status: MeasurementStatus,
    pub basis: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComparisonArm {
    Baseline,
    Candidate,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrialStatus {
    Completed,
    Failed,
    Cancelled,
    TimedOut,
    Unavailable,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrialObservation {
    pub experiment_fingerprint: String,
    pub trial_id: String,
    pub case_id: String,
    pub arm: ComparisonArm,
    pub repetition: u32,
    pub status: TrialStatus,
    pub measurements: Vec<Measurement>,
    pub evidence: Vec<EvidenceRef>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CausalStrength {
    Unknown,
    PairedSupport,
    MechanismSupported,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComparisonReport {
    pub schema_version: u32,
    pub spec_fingerprint: Option<String>,
    pub baseline_label: String,
    pub candidate_label: String,
    /// Number of cases in the frozen plan, or observed cases for the
    /// unbound internal helper.
    pub case_count: usize,
    pub observed_case_count: usize,
    pub paired_case_count: usize,
    pub paired_trial_pair_count: usize,
    pub planned_trial_count: usize,
    pub observed_trial_count: usize,
    pub missing_trial_ids: Vec<String>,
    pub status_counts: BTreeMap<String, usize>,
    /// Per-trial observations are retained so metrics and evidence cannot be
    /// mistaken for one another after aggregation.
    pub observations: Vec<TrialObservation>,
    pub causal_strength: CausalStrength,
    pub unavailable: Vec<String>,
    pub conclusion: String,
}

/// Build a report without dropping failures, cancellations, unknown usage, or
/// unavailable evidence.  A report with no complete baseline/candidate pair is
/// explicitly evidence-incomplete and cannot claim an improvement.
fn build_comparison(
    baseline_label: impl Into<String>,
    candidate_label: impl Into<String>,
    observations: &[TrialObservation],
) -> ComparisonReport {
    let (observations, duplicate_warnings) = deduplicate_observations(observations);
    let mut cases = BTreeMap::<String, BTreeSet<ComparisonArm>>::new();
    let mut case_repetitions = BTreeMap::<(String, u32), BTreeSet<ComparisonArm>>::new();
    let mut status_counts = BTreeMap::new();
    for observation in &observations {
        cases
            .entry(observation.case_id.clone())
            .or_default()
            .insert(observation.arm.clone());
        case_repetitions
            .entry((observation.case_id.clone(), observation.repetition))
            .or_default()
            .insert(observation.arm.clone());
        *status_counts
            .entry(format!("{:?}", observation.status).to_ascii_lowercase())
            .or_insert(0) += 1;
    }
    let paired_trial_pair_count = case_repetitions
        .values()
        .filter(|arms| {
            arms.contains(&ComparisonArm::Baseline) && arms.contains(&ComparisonArm::Candidate)
        })
        .count();
    let paired_case_count = case_repetitions
        .iter()
        .filter(|(_, arms)| {
            arms.contains(&ComparisonArm::Baseline) && arms.contains(&ComparisonArm::Candidate)
        })
        .map(|((case_id, _), _)| case_id)
        .collect::<BTreeSet<_>>()
        .len();
    let mut unavailable = observations
        .iter()
        .filter(|observation| {
            matches!(
                observation.status,
                TrialStatus::Unavailable | TrialStatus::Unknown
            )
        })
        .map(|observation| {
            format!(
                "trial {} has status {:?}",
                observation.trial_id, observation.status
            )
        })
        .collect::<Vec<_>>();
    unavailable.extend(duplicate_warnings);
    // Arm presence is not evidence of a controlled comparison.  The durable
    // trial controller must prove frozen inputs, isolation, ordering, and
    // verifier integrity before a stronger causal classification is allowed.
    let causal_strength = CausalStrength::Unknown;
    let conclusion = if paired_case_count == 0 {
        "No complete baseline/candidate case pair is available; do not claim an improvement."
            .to_string()
    } else if !unavailable.is_empty() {
        "Paired observations exist, but unavailable or unknown trials limit the conclusion."
            .to_string()
    } else {
        "Both arms are present for at least one case; controlled comparability has not been proven."
            .to_string()
    };
    ComparisonReport {
        schema_version: ASSESSMENT_SCHEMA_VERSION,
        spec_fingerprint: None,
        baseline_label: baseline_label.into(),
        candidate_label: candidate_label.into(),
        case_count: cases.len(),
        observed_case_count: cases.len(),
        paired_case_count,
        paired_trial_pair_count,
        planned_trial_count: observations.len(),
        observed_trial_count: observations.len(),
        missing_trial_ids: Vec::new(),
        status_counts,
        observations,
        causal_strength,
        unavailable,
        conclusion,
    }
}

/// Assess observations against the exact frozen plan. This is the pure
/// contract the durable executor will call after settling runs.
pub fn build_comparison_for_plan(
    spec: &ExperimentSpec,
    baseline_label: impl Into<String>,
    candidate_label: impl Into<String>,
    observations: &[TrialObservation],
) -> Result<ComparisonReport, String> {
    let planned = spec.plan_trials()?;
    let fingerprint = spec.spec_fingerprint()?;
    let expected = planned
        .iter()
        .map(|trial| (trial.trial_id.as_str(), trial))
        .collect::<BTreeMap<_, _>>();
    let mut accepted = BTreeMap::<&str, TrialObservation>::new();
    for observation in observations {
        if observation.experiment_fingerprint != fingerprint {
            return Err(format!(
                "trial {} has fingerprint {}, expected {}",
                observation.trial_id, observation.experiment_fingerprint, fingerprint
            ));
        }
        let trial = expected.get(observation.trial_id.as_str()).ok_or_else(|| {
            format!(
                "observation references unknown trial {}",
                observation.trial_id
            )
        })?;
        if trial.case_id != observation.case_id
            || trial.arm != observation.arm
            || trial.repetition != observation.repetition
        {
            return Err(format!(
                "observation {} does not match its planned case, arm, or repetition",
                observation.trial_id
            ));
        }
        if let Some(previous) = accepted.get(observation.trial_id.as_str()) {
            if *previous != *observation {
                return Err(format!(
                    "trial {} has conflicting duplicate observations",
                    observation.trial_id
                ));
            }
            continue;
        }
        accepted.insert(observation.trial_id.as_str(), observation.clone());
    }
    let accepted = accepted.into_values().collect::<Vec<_>>();
    let mut report = build_comparison(baseline_label, candidate_label, &accepted);
    let observed_ids = accepted
        .iter()
        .map(|observation| observation.trial_id.as_str())
        .collect::<BTreeSet<_>>();
    let missing_trial_ids = planned
        .iter()
        .filter(|trial| !observed_ids.contains(trial.trial_id.as_str()))
        .map(|trial| trial.trial_id.clone())
        .collect::<Vec<_>>();
    report.spec_fingerprint = Some(fingerprint);
    report.case_count = spec.cases.len();
    report.observed_case_count = accepted
        .iter()
        .map(|observation| observation.case_id.as_str())
        .collect::<BTreeSet<_>>()
        .len();
    report.planned_trial_count = planned.len();
    report.observed_trial_count = accepted.len();
    report.missing_trial_ids = missing_trial_ids.clone();
    report.unavailable.extend(
        missing_trial_ids
            .iter()
            .map(|trial_id| format!("planned trial {trial_id} has no observation")),
    );
    if !report.missing_trial_ids.is_empty() {
        report.conclusion = if report.observed_trial_count == 0 {
            "No planned trial has an observation; do not claim an improvement.".to_string()
        } else {
            "The comparison is partial because planned trials have no observation; do not claim a complete improvement."
                .to_string()
        };
    }
    Ok(report)
}

fn deduplicate_observations(
    observations: &[TrialObservation],
) -> (Vec<TrialObservation>, Vec<String>) {
    let mut accepted = BTreeMap::<&str, TrialObservation>::new();
    let mut conflicts = Vec::new();
    for observation in observations {
        match accepted.get(observation.trial_id.as_str()) {
            Some(previous) if *previous != *observation => conflicts.push(format!(
                "trial {} has conflicting duplicate observations; first delivery retained",
                observation.trial_id
            )),
            Some(_) => {}
            None => {
                accepted.insert(observation.trial_id.as_str(), observation.clone());
            }
        }
    }
    (accepted.into_values().collect(), conflicts)
}

/// Render the same structured report for Markdown or a terminal preview.
pub fn render_markdown(report: &ComparisonReport) -> String {
    let mut output = format!(
        "# Evaluation comparison\n\nBaseline: `{}`  \nCandidate: `{}`  \nCases: {} planned, {} observed (paired: {} cases / {} trial pairs)  \nTrials: {} / {} observed\n\n",
        report.baseline_label,
        report.candidate_label,
        report.case_count,
        report.observed_case_count,
        report.paired_case_count,
        report.paired_trial_pair_count,
        report.observed_trial_count,
        report.planned_trial_count
    );
    output.push_str("## Trial outcomes\n\n");
    for (status, count) in &report.status_counts {
        output.push_str(&format!("- {status}: {count}\n"));
    }
    output.push_str("\n## Conclusion\n\n");
    output.push_str(&report.conclusion);
    output.push_str(&format!(
        "\nCausal strength: `{:?}`\n",
        report.causal_strength
    ));
    if !report.unavailable.is_empty() {
        output.push_str("\n## Unavailable\n\n");
        for item in &report.unavailable {
            output.push_str(&format!("- {item}\n"));
        }
    }
    output.push_str("\n## Trials and evidence\n\n");
    for observation in &report.observations {
        output.push_str(&format!(
            "### `{}` · {} · {:?} · {:?} · repetition {}\n\n",
            observation.trial_id,
            observation.case_id,
            observation.arm,
            observation.status,
            observation.repetition
        ));
        for measurement in &observation.measurements {
            output.push_str(&format!(
                "- measurement `{}`: {:?} {} ({:?})\n",
                measurement.name, measurement.value, measurement.unit, measurement.status
            ));
        }
        for item in &observation.evidence {
            output.push_str(&format!(
                "- evidence `{}` ({:?}, {:?})\n",
                item.evidence_id, item.kind, item.availability
            ));
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evaluation::experiment::{
        DataIsolation, EXPERIMENT_SCHEMA_VERSION, EvaluationBudget, EvaluationCase,
        EvaluationTarget, EvaluationTargetKind, ExperimentSpec, FrozenConditions, MemoryIsolation,
        RevisionRef, TrialOrder, TrialUnit,
    };

    fn observation(case_id: &str, arm: ComparisonArm, status: TrialStatus) -> TrialObservation {
        TrialObservation {
            experiment_fingerprint: "sha256:test".to_string(),
            trial_id: format!("{case_id}-{arm:?}"),
            case_id: case_id.to_string(),
            arm,
            repetition: 0,
            status,
            measurements: vec![Measurement {
                name: "tokens".to_string(),
                value: None,
                unit: "tokens".to_string(),
                status: MeasurementStatus::Missing,
                basis: None,
            }],
            evidence: vec![EvidenceRef {
                evidence_id: format!("evidence-{case_id}"),
                kind: EvidenceKind::Trace,
                availability: EvidenceAvailability::Available,
                content_hash: None,
                locator: Some(case_id.to_string()),
            }],
        }
    }

    #[test]
    fn keeps_failures_and_unknowns_in_the_report() {
        let report = build_comparison(
            "baseline",
            "candidate",
            &[
                observation("case-1", ComparisonArm::Baseline, TrialStatus::Failed),
                observation("case-1", ComparisonArm::Candidate, TrialStatus::Completed),
                observation("case-2", ComparisonArm::Baseline, TrialStatus::Unknown),
            ],
        );
        assert_eq!(report.status_counts.get("failed"), Some(&1));
        assert_eq!(report.status_counts.get("unknown"), Some(&1));
        assert_eq!(report.paired_case_count, 1);
        assert_eq!(report.paired_trial_pair_count, 1);
        assert!(report.conclusion.contains("unavailable or unknown"));
        assert_eq!(report.causal_strength, CausalStrength::Unknown);
        assert!(render_markdown(&report).contains("evidence-case-1"));
    }

    #[test]
    fn refuses_to_claim_without_a_pair() {
        let report = build_comparison(
            "baseline",
            "candidate",
            &[observation(
                "case-1",
                ComparisonArm::Candidate,
                TrialStatus::Completed,
            )],
        );
        assert_eq!(report.causal_strength, CausalStrength::Unknown);
        assert!(report.conclusion.contains("No complete"));
    }

    #[test]
    fn missing_measurements_are_not_zero() {
        let report = build_comparison(
            "baseline",
            "candidate",
            &[
                observation("case-1", ComparisonArm::Baseline, TrialStatus::Completed),
                observation("case-1", ComparisonArm::Candidate, TrialStatus::Completed),
            ],
        );
        assert!(
            report
                .observations
                .iter()
                .flat_map(|observation| &observation.measurements)
                .all(|measurement| measurement.value.is_none())
        );
    }

    #[test]
    fn duplicate_trial_ids_are_explicitly_unavailable() {
        let sample = observation("case-1", ComparisonArm::Baseline, TrialStatus::Completed);
        let report = build_comparison("baseline", "candidate", &[sample.clone(), sample]);
        assert_eq!(report.status_counts.get("completed"), Some(&1));
        assert!(report.unavailable.is_empty());
    }

    fn plan_spec() -> ExperimentSpec {
        ExperimentSpec {
            schema_version: EXPERIMENT_SCHEMA_VERSION,
            experiment_id: "eval-1".to_string(),
            target: EvaluationTarget {
                kind: EvaluationTargetKind::Workflow,
                baseline: RevisionRef {
                    revision_id: "base".to_string(),
                    content_hash: "sha256:base".to_string(),
                },
                candidate: RevisionRef {
                    revision_id: "candidate".to_string(),
                    content_hash: "sha256:candidate".to_string(),
                },
            },
            cases: vec![EvaluationCase {
                case_id: "case-1".to_string(),
                input_snapshot_ref: "snapshot-1".to_string(),
                input_content_hash: "sha256:input".to_string(),
                verifier_id: "verifier".to_string(),
                verifier_version: "v1".to_string(),
                holdout: true,
            }],
            repetitions: 2,
            order: TrialOrder::BaselineFirst,
            conditions: FrozenConditions {
                isolation_profile: "prompt_only_private".to_string(),
                model_binding: "model-v1".to_string(),
                provider_binding: "provider-v1".to_string(),
                context_snapshot_hash: "sha256:context".to_string(),
                tool_policy_hash: "sha256:tools".to_string(),
                cache_policy: "recorded".to_string(),
                memory_isolation: MemoryIsolation::Disabled,
                data_isolation: DataIsolation::Disabled,
            },
            budget: EvaluationBudget {
                max_trials: 4,
                max_concurrency: 2,
                max_wall_time_secs: 60,
            },
        }
    }

    fn observation_for(trial: &TrialUnit, status: TrialStatus) -> TrialObservation {
        TrialObservation {
            experiment_fingerprint: trial.spec_fingerprint.clone(),
            trial_id: trial.trial_id.clone(),
            case_id: trial.case_id.clone(),
            arm: trial.arm.clone(),
            repetition: trial.repetition,
            status,
            measurements: vec![Measurement {
                name: "wall_time".to_string(),
                value: Some(1.0),
                unit: "seconds".to_string(),
                status: MeasurementStatus::Observed,
                basis: Some("trace".to_string()),
            }],
            evidence: vec![EvidenceRef {
                evidence_id: format!("evidence-{}", trial.trial_id),
                kind: EvidenceKind::Trace,
                availability: EvidenceAvailability::Available,
                content_hash: Some("sha256:evidence".to_string()),
                locator: Some(trial.trial_id.clone()),
            }],
        }
    }

    #[test]
    fn plan_bound_assessment_preserves_missing_and_deduplicates_delivery() {
        let spec = plan_spec();
        let planned = spec.plan_trials().unwrap();
        let first = observation_for(&planned[0], TrialStatus::Completed);
        let second = observation_for(&planned[1], TrialStatus::Failed);
        let report = build_comparison_for_plan(
            &spec,
            "baseline",
            "candidate",
            &[first.clone(), second, first],
        )
        .unwrap();
        assert_eq!(
            report.spec_fingerprint,
            Some(spec.spec_fingerprint().unwrap())
        );
        assert_eq!(report.planned_trial_count, 4);
        assert_eq!(report.observed_trial_count, 2);
        assert_eq!(report.missing_trial_ids.len(), 2);
        assert_eq!(report.case_count, 1);
        assert_eq!(report.observed_case_count, 1);
        assert_eq!(report.status_counts.get("completed"), Some(&1));
        assert_eq!(report.status_counts.get("failed"), Some(&1));
        assert!(render_markdown(&report).contains("baseline"));
    }

    #[test]
    fn plan_bound_assessment_rejects_unknown_and_conflicting_observations() {
        let spec = plan_spec();
        let planned = spec.plan_trials().unwrap();
        let first = observation_for(&planned[0], TrialStatus::Completed);
        let mut conflict = first.clone();
        conflict.status = TrialStatus::Failed;
        assert!(
            build_comparison_for_plan(&spec, "baseline", "candidate", &[first.clone(), conflict])
                .unwrap_err()
                .contains("conflicting duplicate")
        );

        let mut unknown = first;
        unknown.trial_id = "trial:sha256:unknown".to_string();
        assert!(
            build_comparison_for_plan(&spec, "baseline", "candidate", &[unknown])
                .unwrap_err()
                .contains("unknown trial")
        );
    }

    #[test]
    fn pairing_requires_the_same_case_and_repetition() {
        let mut baseline = observation("case-1", ComparisonArm::Baseline, TrialStatus::Completed);
        baseline.repetition = 0;
        let mut candidate = observation("case-1", ComparisonArm::Candidate, TrialStatus::Completed);
        candidate.repetition = 1;
        let report = build_comparison("baseline", "candidate", &[baseline, candidate]);
        assert_eq!(report.paired_case_count, 0);
        assert_eq!(report.paired_trial_pair_count, 0);
    }

    #[test]
    fn plan_bound_report_uses_planned_case_denominator_for_partial_results() {
        let mut spec = plan_spec();
        spec.cases.push(EvaluationCase {
            case_id: "case-2".to_string(),
            input_snapshot_ref: "snapshot-2".to_string(),
            input_content_hash: "sha256:input-2".to_string(),
            verifier_id: "verifier".to_string(),
            verifier_version: "v1".to_string(),
            holdout: false,
        });
        spec.budget.max_trials = 8;
        let planned = spec.plan_trials().unwrap();
        let report = build_comparison_for_plan(
            &spec,
            "baseline",
            "candidate",
            &[
                observation_for(&planned[0], TrialStatus::Completed),
                observation_for(&planned[1], TrialStatus::Completed),
            ],
        )
        .unwrap();
        assert_eq!(report.case_count, 2);
        assert_eq!(report.observed_case_count, 1);
        assert_eq!(report.paired_case_count, 1);
        assert!(report.conclusion.contains("partial"));
    }
}

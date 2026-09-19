//! Deterministic, evidence-preserving report artifacts for controlled Eval.
//!
//! Report generation is pure over a frozen experiment and persisted
//! observations. It never starts a Run, reruns a provider, or mutates an
//! observation. The same facts and renderer version therefore produce the
//! same content identity across TUI, Web, and Server readers.

use super::assessment::{ComparisonReport, build_comparison_for_plan, render_markdown};
use super::durable::EvaluationExperimentRecord;
use super::execution::{EvaluationObservationRecord, content_fingerprint};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

pub const EVALUATION_REPORT_SCHEMA_VERSION: u32 = 3;
pub const EVALUATION_REPORT_RENDERER_VERSION: &str = "evaluation-markdown.v4";
const MAX_REPORT_LABEL_BYTES: usize = 256;

pub fn validate_report_label(name: &str, label: &str) -> Result<(), String> {
    if label.trim().is_empty() {
        return Err(format!("{name} must not be empty"));
    }
    if label.len() > MAX_REPORT_LABEL_BYTES {
        return Err(format!(
            "{name} must be at most {MAX_REPORT_LABEL_BYTES} bytes"
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationReportObservationRef {
    pub observation_id: String,
    pub trial_id: String,
    pub request_fingerprint: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationReportCoverage {
    pub planned_trial_count: usize,
    pub observed_trial_count: usize,
    pub missing_trial_ids: Vec<String>,
    pub unavailable_trial_ids: Vec<String>,
    pub evidence_incomplete: bool,
    pub metric_gaps: Vec<EvaluationMetricGap>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationMetricGap {
    pub trial_id: String,
    pub dimension: String,
    pub metric: String,
    pub expected_unit: String,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationReportManifest {
    pub schema_version: u32,
    pub owner_user_id: String,
    pub experiment_id: String,
    pub spec_fingerprint: String,
    pub observation_refs: Vec<EvaluationReportObservationRef>,
    pub renderer_version: String,
    pub coverage: EvaluationReportCoverage,
    pub report_content_hash: String,
    /// Identity of the complete report artifact, including coverage and the
    /// exact evidence references that produced the rendered content.
    pub artifact_fingerprint: String,
    pub artifact_reference: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationReportArtifact {
    pub manifest: EvaluationReportManifest,
    pub report: ComparisonReport,
    pub markdown: String,
}

impl EvaluationReportArtifact {
    pub fn with_artifact_reference(mut self, artifact_reference: impl Into<String>) -> Self {
        self.manifest.artifact_reference = Some(artifact_reference.into());
        self
    }
}

pub fn build_report_artifact(
    owner_user_id: &str,
    experiment: &EvaluationExperimentRecord,
    observations: &[EvaluationObservationRecord],
    unavailable_trial_ids: &[String],
    baseline_label: impl Into<String>,
    candidate_label: impl Into<String>,
) -> Result<EvaluationReportArtifact, String> {
    if owner_user_id != experiment.owner_user_id {
        return Err("report owner does not match the experiment owner".to_string());
    }
    let baseline_label = baseline_label.into();
    let candidate_label = candidate_label.into();
    for (name, label) in [
        ("baseline_label", &baseline_label),
        ("candidate_label", &candidate_label),
    ] {
        validate_report_label(name, label)?;
    }
    let planned_trial_ids = experiment
        .spec
        .plan_trials()
        .map_err(|error| format!("invalid frozen evaluation plan: {error}"))?
        .into_iter()
        .map(|trial| trial.trial_id)
        .collect::<BTreeSet<_>>();
    let mut normalized_unavailable = BTreeSet::new();
    for trial_id in unavailable_trial_ids {
        if !planned_trial_ids.contains(trial_id) {
            return Err(format!(
                "unavailable trial {trial_id} is not in the frozen evaluation plan"
            ));
        }
        normalized_unavailable.insert(trial_id.clone());
    }
    let mut observation_refs_by_id = BTreeMap::new();
    for record in observations {
        if record.owner_user_id != owner_user_id
            || record.experiment_id != experiment.experiment_id
            || record.observation.experiment_fingerprint != experiment.spec_fingerprint
        {
            return Err(format!(
                "observation {} is outside the report owner/experiment boundary",
                record.observation_id
            ));
        }
        if record.trial_id != record.observation.trial_id {
            return Err(format!(
                "observation {} outer trial identity does not match its content",
                record.observation_id
            ));
        }
        let observation_ref = EvaluationReportObservationRef {
            observation_id: record.observation_id.clone(),
            trial_id: record.trial_id.clone(),
            request_fingerprint: record.request_fingerprint.clone(),
        };
        if let Some(previous) = observation_refs_by_id.get(&record.observation_id) {
            if previous != &observation_ref {
                return Err(format!(
                    "observation {} has conflicting report identity",
                    record.observation_id
                ));
            }
        } else {
            observation_refs_by_id.insert(record.observation_id.clone(), observation_ref);
        }
    }
    let mut report = build_comparison_for_plan(
        &experiment.spec,
        baseline_label,
        candidate_label,
        &observations
            .iter()
            .map(|record| record.observation.clone())
            .collect::<Vec<_>>(),
    )?;
    for trial_id in &normalized_unavailable {
        report
            .unavailable
            .push(format!("planned trial {trial_id} status is unavailable"));
    }
    report.unavailable.sort();
    report.unavailable.dedup();
    if !normalized_unavailable.is_empty() {
        report.conclusion = if report.paired_case_count == 0 {
            "No complete baseline/candidate case pair is available and trial status evidence is unavailable; do not claim an improvement.".to_string()
        } else {
            "Paired observations exist, but unavailable trial status evidence limits the conclusion; do not claim a complete improvement.".to_string()
        };
    }
    let observation_refs = observation_refs_by_id.into_values().collect::<Vec<_>>();
    let mut markdown = render_markdown(&report);
    let mut metric_gaps = Vec::new();
    for trial_id in &planned_trial_ids {
        let observation = report
            .observations
            .iter()
            .find(|item| &item.trial_id == trial_id);
        for &(dimension, metric, unit) in experiment.spec.measurement_profile.requirements() {
            let measurement = observation
                .and_then(|item| item.measurements.iter().find(|item| item.name == metric));
            let reason = match measurement {
                None => "required measurement is missing",
                Some(item) if item.unit != unit => {
                    "measurement unit does not match the frozen requirement"
                }
                Some(item) if item.status != super::assessment::MeasurementStatus::Observed => {
                    "required measurement is not observed"
                }
                // Current observation records contain a textual basis, not
                // a scoped proof of collector/verifier completeness. Never
                // infer coverage from the existence of a numeric value.
                Some(_) if matches!(metric, "prompt_tokens" | "completion_tokens") => {
                    "reported usage subtotal; request and lane coverage is unknown"
                }
                Some(_) => "scoped assessment and completeness evidence is unavailable",
            };
            metric_gaps.push(EvaluationMetricGap {
                trial_id: trial_id.clone(),
                dimension: dimension.into(),
                metric: metric.into(),
                expected_unit: unit.into(),
                reason: reason.into(),
            });
        }
    }
    markdown.push_str("\n## Metric coverage\n\n");
    for gap in &metric_gaps {
        markdown.push_str(&format!(
            "- `{}` / {} / `{}`: {}\n",
            gap.trial_id, gap.dimension, gap.metric, gap.reason
        ));
    }
    let missing_trial_ids = report.missing_trial_ids.clone();
    let unavailable_trial_ids = normalized_unavailable.into_iter().collect::<Vec<_>>();
    let evidence_incomplete = report.observations.iter().any(|observation| {
        !matches!(
            observation.status,
            super::assessment::TrialStatus::Completed
        ) || observation.measurements.iter().any(|measurement| {
            !matches!(
                measurement.status,
                super::assessment::MeasurementStatus::Observed
            )
        }) || observation.evidence.iter().any(|evidence| {
            !matches!(
                evidence.availability,
                super::assessment::EvidenceAvailability::Available
            )
        })
    });
    let coverage = EvaluationReportCoverage {
        planned_trial_count: report.planned_trial_count,
        observed_trial_count: report.observed_trial_count,
        missing_trial_ids,
        unavailable_trial_ids,
        evidence_incomplete: !metric_gaps.is_empty()
            || !report.unavailable.is_empty()
            || evidence_incomplete,
        metric_gaps,
    };
    let report_payload = json!({
        "schema_version": EVALUATION_REPORT_SCHEMA_VERSION,
        "renderer_version": EVALUATION_REPORT_RENDERER_VERSION,
        "report": report.clone(),
        "markdown": markdown.clone(),
        "observation_refs": observation_refs.clone(),
        "coverage": coverage.clone(),
    });
    let report_content_hash = content_fingerprint(
        &serde_json::to_string(&report_payload)
            .map_err(|error| format!("serialize report payload: {error}"))?,
    );
    let artifact_fingerprint = content_fingerprint(
        &serde_json::to_string(&json!({
            "owner_user_id": owner_user_id,
            "experiment_id": experiment.experiment_id,
            "spec_fingerprint": experiment.spec_fingerprint,
            "renderer_version": EVALUATION_REPORT_RENDERER_VERSION,
            "report_content_hash": report_content_hash.clone(),
            "observation_refs": observation_refs.clone(),
            "coverage": coverage.clone(),
        }))
        .map_err(|error| format!("serialize report artifact identity: {error}"))?,
    );
    Ok(EvaluationReportArtifact {
        manifest: EvaluationReportManifest {
            schema_version: EVALUATION_REPORT_SCHEMA_VERSION,
            owner_user_id: owner_user_id.to_string(),
            experiment_id: experiment.experiment_id.clone(),
            spec_fingerprint: experiment.spec_fingerprint.clone(),
            observation_refs,
            renderer_version: EVALUATION_REPORT_RENDERER_VERSION.to_string(),
            coverage,
            report_content_hash,
            artifact_fingerprint,
            artifact_reference: None,
        },
        report,
        markdown,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evaluation::assessment::{
        EvidenceAvailability, EvidenceKind, EvidenceRef, Measurement, MeasurementStatus,
        TrialObservation, TrialStatus,
    };
    use crate::evaluation::experiment::{
        DataIsolation, EvaluationBudget, EvaluationCase, EvaluationTarget, EvaluationTargetKind,
        ExperimentSpec, FrozenConditions, MemoryIsolation, RevisionRef, TrialOrder,
    };
    use crate::evaluation::{EVALUATION_EXECUTION_SCHEMA_VERSION, EvaluationObservationRecord};

    fn fixture() -> (EvaluationExperimentRecord, Vec<EvaluationObservationRecord>) {
        let spec = ExperimentSpec {
            schema_version: 1,
            experiment_id: "report-exp".to_string(),
            target: EvaluationTarget {
                kind: EvaluationTargetKind::Prompt,
                baseline: RevisionRef {
                    revision_id: "base".to_string(),
                    content_hash:
                        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                            .to_string(),
                    content: None,
                },
                candidate: RevisionRef {
                    revision_id: "cand".to_string(),
                    content_hash:
                        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                            .to_string(),
                    content: None,
                },
                skill_name: None,
            },
            cases: vec![EvaluationCase {
                case_id: "case".to_string(),
                input_snapshot_ref: "input://case".to_string(),
                input_content_hash:
                    "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
                        .to_string(),
                verifier_id: "none".to_string(),
                verifier_version: "1".to_string(),
                holdout: false,
                task_verifier: None,
                input_content: None,
            }],
            repetitions: 1,
            order: TrialOrder::BaselineFirst,
            conditions: FrozenConditions {
                execution_config: crate::evaluation::test_support::execution_config(
                    "model", "provider", "case",
                ),
                isolation_profile: "prompt_only_private".to_string(),
                model_binding: "model".to_string(),
                provider_binding: "provider".to_string(),
                context_snapshot_hash:
                    "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
                        .to_string(),
                tool_policy_hash:
                    "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
                        .to_string(),
                cache_policy: "provider_default_recorded".to_string(),
                memory_isolation: MemoryIsolation::Disabled,
                data_isolation: DataIsolation::Disabled,
            },
            budget: EvaluationBudget {
                max_trials: 2,
                max_concurrency: 1,
                max_wall_time_secs: 60,
            },
            adapter_profile_version: None,
            measurement_profile:
                crate::evaluation::measurement_profile::MeasurementProfile::InstructionOnlyV1,
        };
        let fingerprint = spec.spec_fingerprint().expect("spec fingerprint");
        let experiment = EvaluationExperimentRecord {
            owner_user_id: "owner".to_string(),
            experiment_id: spec.experiment_id.clone(),
            spec_fingerprint: fingerprint.clone(),
            spec,
            submission_idempotency_key: "submission".to_string(),
            planned_trial_count: 2,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        };
        let trials = experiment.spec.plan_trials().expect("plan trials");
        let observations = trials
            .into_iter()
            .enumerate()
            .map(|(index, trial)| EvaluationObservationRecord {
                schema_version: EVALUATION_EXECUTION_SCHEMA_VERSION,
                owner_user_id: "owner".to_string(),
                observation_id: format!("observation-{index}"),
                experiment_id: experiment.experiment_id.clone(),
                trial_id: trial.trial_id.clone(),
                session_id: format!("session-{index}"),
                execution_run_id: format!("run-{index}"),
                admission_run_generation: 1,
                execution_run_generation: 1,
                observation: TrialObservation {
                    experiment_fingerprint: fingerprint.clone(),
                    trial_id: trial.trial_id,
                    case_id: trial.case_id,
                    arm: trial.arm,
                    repetition: trial.repetition,
                    status: TrialStatus::Completed,
                    measurements: vec![Measurement {
                        name: "tokens".to_string(),
                        value: Some((index + 1) as f64),
                        unit: "tokens".to_string(),
                        status: MeasurementStatus::Observed,
                        basis: Some("provider".to_string()),
                    }],
                    evidence: vec![EvidenceRef {
                        evidence_id: format!("trace-{index}"),
                        kind: EvidenceKind::Trace,
                        availability: EvidenceAvailability::Available,
                        content_hash: Some(format!("sha256:{}", "a".repeat(64))),
                        locator: Some(format!("run://run-{index}")),
                    }],
                },
                materialization_receipt_ids: vec![format!("receipt-{index}")],
                request_fingerprint: format!("sha256:{}", "b".repeat(64)),
                idempotency_key: format!("observation-key-{index}"),
                created_at: "2026-01-01T00:00:00Z".to_string(),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
            })
            .collect();
        (experiment, observations)
    }

    #[test]
    fn frozen_requirements_cover_every_planned_trial_and_dimension() {
        let (experiment, _) = fixture();
        let observations = Vec::new();
        let artifact =
            build_report_artifact("owner", &experiment, &observations, &[], "base", "cand")
                .unwrap();
        assert!(artifact.manifest.coverage.evidence_incomplete);
        assert_eq!(artifact.manifest.coverage.metric_gaps.len(), 24);
        let dimensions = artifact
            .manifest
            .coverage
            .metric_gaps
            .iter()
            .map(|gap| gap.dimension.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            dimensions,
            BTreeSet::from([
                "task",
                "tool",
                "context",
                "provider",
                "safety",
                "reliability",
                "cost"
            ])
        );
        assert!(
            artifact
                .manifest
                .coverage
                .metric_gaps
                .iter()
                .all(|gap| gap.reason == "required measurement is missing")
        );
    }

    #[test]
    fn reported_usage_does_not_prove_complete_cost() {
        let (experiment, mut observations) = fixture();
        for record in &mut observations {
            record.observation.measurements[0].name = "prompt_tokens".into();
            record.observation.measurements[0].value = Some(0.0);
        }
        let artifact =
            build_report_artifact("owner", &experiment, &observations, &[], "base", "cand")
                .unwrap();
        let gaps = artifact
            .manifest
            .coverage
            .metric_gaps
            .iter()
            .filter(|gap| gap.metric == "prompt_tokens")
            .collect::<Vec<_>>();
        assert_eq!(gaps.len(), 2);
        assert!(gaps.iter().all(|gap| gap.reason.contains("subtotal")));
        assert!(artifact.manifest.coverage.evidence_incomplete);
        assert!(
            artifact
                .report
                .observations
                .iter()
                .all(|item| item.measurements[0].value == Some(0.0))
        );
    }

    #[test]
    fn report_identity_is_stable_when_observation_delivery_order_changes() {
        let (experiment, observations) = fixture();
        let first = build_report_artifact("owner", &experiment, &observations, &[], "base", "cand")
            .expect("report");
        let mut reversed = observations.clone();
        reversed.reverse();
        let second = build_report_artifact("owner", &experiment, &reversed, &[], "base", "cand")
            .expect("report");
        assert_eq!(
            first.manifest.report_content_hash,
            second.manifest.report_content_hash
        );
        assert_eq!(first.markdown, second.markdown);
        assert_eq!(
            first.manifest.observation_refs,
            second.manifest.observation_refs
        );
    }

    #[test]
    fn completed_runs_without_measurements_or_evidence_are_incomplete() {
        for missing_measurements in [true, false] {
            let (experiment, mut observations) = fixture();
            for record in &mut observations {
                if missing_measurements {
                    record.observation.measurements.clear();
                } else {
                    record.observation.evidence.clear();
                }
            }
            let artifact =
                build_report_artifact("owner", &experiment, &observations, &[], "base", "cand")
                    .expect("missing evidence stays reportable");
            assert!(artifact.manifest.coverage.evidence_incomplete);
            assert_eq!(artifact.manifest.coverage.observed_trial_count, 2);
            assert!(artifact.markdown.contains(if missing_measurements {
                "has no measurements"
            } else {
                "has no evidence references"
            }));
            assert_eq!(
                artifact.report.causal_strength,
                super::super::assessment::CausalStrength::Unknown
            );
        }
    }

    #[test]
    fn report_rejects_observed_measurement_without_a_value() {
        let (experiment, mut observations) = fixture();
        observations[0].observation.measurements[0].value = None;
        assert!(
            build_report_artifact("owner", &experiment, &observations, &[], "base", "cand",)
                .expect_err("invalid observed fact")
                .contains("must have a finite value")
        );
    }

    #[test]
    fn report_keeps_partial_and_unknown_facts_visible() {
        let (experiment, mut observations) = fixture();
        observations[0].observation.status = TrialStatus::Unknown;
        observations.pop();
        let unavailable_trial_id = experiment
            .spec
            .plan_trials()
            .expect("plan")
            .into_iter()
            .nth(1)
            .expect("candidate trial")
            .trial_id;
        let report = build_report_artifact(
            "owner",
            &experiment,
            &observations,
            std::slice::from_ref(&unavailable_trial_id),
            "base",
            "cand",
        )
        .expect("partial report");
        assert_eq!(report.manifest.coverage.observed_trial_count, 1);
        assert_eq!(report.manifest.coverage.missing_trial_ids.len(), 1);
        assert!(report.manifest.coverage.evidence_incomplete);
        assert!(report.markdown.contains("Unavailable"));
        assert!(report.markdown.contains(&format!(
            "planned trial {unavailable_trial_id} status is unavailable"
        )));
        assert!(report.markdown.contains("do not claim an improvement"));
    }

    #[test]
    fn report_rejects_unavailable_trial_outside_the_frozen_plan() {
        let (experiment, observations) = fixture();
        let error = build_report_artifact(
            "owner",
            &experiment,
            &observations,
            &["unreadable-trial".to_string()],
            "base",
            "cand",
        )
        .expect_err("foreign unavailable trial must be rejected");
        assert!(error.contains("not in the frozen evaluation plan"));
    }

    #[test]
    fn report_artifact_identity_includes_coverage_and_observation_refs() {
        let (experiment, observations) = fixture();
        let complete =
            build_report_artifact("owner", &experiment, &observations, &[], "base", "cand")
                .expect("complete report");
        let candidate_trial_id = experiment
            .spec
            .plan_trials()
            .expect("plan")
            .into_iter()
            .nth(1)
            .expect("candidate trial")
            .trial_id;
        let partial = build_report_artifact(
            "owner",
            &experiment,
            &observations[..1],
            std::slice::from_ref(&candidate_trial_id),
            "base",
            "cand",
        )
        .expect("partial report");
        assert_ne!(
            complete.manifest.report_content_hash,
            partial.manifest.report_content_hash
        );
        assert_ne!(
            complete.manifest.artifact_fingerprint,
            partial.manifest.artifact_fingerprint
        );
    }
}

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

pub const EVALUATION_REPORT_SCHEMA_VERSION: u32 = 1;
pub const EVALUATION_REPORT_RENDERER_VERSION: &str = "evaluation-markdown.v1";

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
    let markdown = render_markdown(&report);
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
        evidence_incomplete: !report.unavailable.is_empty() || evidence_incomplete,
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
                },
                candidate: RevisionRef {
                    revision_id: "cand".to_string(),
                    content_hash:
                        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                            .to_string(),
                },
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
            }],
            repetitions: 1,
            order: TrialOrder::BaselineFirst,
            conditions: FrozenConditions {
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

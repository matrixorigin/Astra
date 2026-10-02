//! Local, versioned model-routing qualification records. These are review
//! artifacts, not authenticated approvals or a production activation registry.
pub mod rollout;

use astra_turn_types::model_routing::ModelRoutingFeatures;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Seal the evaluation population and its pre-outcome inputs independently of
/// labels, costs, replay completion and feedback. Source/group order is immaterial.
/// Computing this digest does not authorize the sources or attest preregistration.
pub fn router_evaluation_plan_sha256(
    input: &crate::model_routing::offline::RouterDatasetInput,
) -> Result<String, String> {
    #[derive(Serialize)]
    struct PlannedSource<'a> {
        source_id: &'a str,
        owner_id: &'a str,
        decision_at: DateTime<Utc>,
        run_id: &'a str,
        session_id: &'a str,
        group_keys: Vec<&'a str>,
        input_reference: &'a Option<astra_turn_types::FeedbackResponseReference>,
        features: Option<ModelRoutingFeatures>,
        selected_offering_id: &'a str,
        selected_contract_root: &'a str,
    }
    let mut sources: Vec<_> = input
        .sources
        .iter()
        .map(|source| {
            let mut group_keys: Vec<_> = source.group_keys.iter().map(String::as_str).collect();
            group_keys.sort_unstable();
            group_keys.dedup();
            PlannedSource {
                source_id: &source.source_id,
                owner_id: &source.owner_id,
                decision_at: source.decision_at,
                run_id: &source.decision.run_id,
                session_id: &source.decision.session_id,
                group_keys,
                input_reference: &source.decision.input_reference,
                features: source.decision.features,
                selected_offering_id: &source.decision.selected_offering_id,
                selected_contract_root: &source.decision.selected_contract_root,
            }
        })
        .collect();
    sources.sort_by(|a, b| a.source_id.cmp(b.source_id));
    if sources
        .windows(2)
        .any(|pair| pair[0].source_id == pair[1].source_id)
    {
        return Err("Duplicate source in router evaluation plan".into());
    }
    crate::model_routing::offline::content_sha256(&(
        "router-evaluation-plan-v1",
        &input.manifest,
        sources,
    ))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterQualificationProtocol {
    pub schema_version: u8,
    pub job_id: String,
    pub owner_id: String,
    pub dataset_id: String,
    /// Operator-attested roster seal: after all recorded decisions and strictly
    /// before held-out replay starts or supplied outcomes are collected.
    pub registered_at: DateTime<Utc>,
    /// Pins fitting and threshold selection as well as the evaluation criteria.
    pub training_config_sha256: String,
    /// Pins the manifest, source roster, grouping, features and recorded Auto selection.
    pub evaluation_plan_sha256: String,
    pub minimum_test_groups: usize,
    pub minimum_stratum_groups: usize,
    pub minimum_pair_coverage: f64,
    pub maximum_quality_regression: f64,
    pub minimum_cost_saving_fraction: f64,
    /// A prespecified bound; exceeding it rejects the gate, never clips data.
    pub maximum_episode_cost_usd: f64,
    pub maximum_p95_latency_ratio: f64,
    /// Family-wise confidence across all quality and cost comparisons.
    pub confidence: f64,
    /// Predeclared deployment scope. Missing and unexpected strata fail closed.
    pub required_strata: Vec<ModelRoutingFeatures>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouterQualificationStatus {
    Rejected,
    ReadyForShadow,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RouterTuningRecord {
    pub schema_version: u8,
    pub job_id: String,
    pub owner_id: String,
    pub dataset_sha256: String,
    pub candidate_sha256: String,
    pub protocol_sha256: String,
    pub evaluated_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub source_ids: Vec<String>,
    pub status: RouterQualificationStatus,
    /// Neither an offline gate nor shadow predictions authorize activation.
    pub production_qualified: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_routing::offline::{Acceptability, RouterDatasetInput};

    fn input() -> RouterDatasetInput {
        serde_json::from_str(include_str!(
            "../../../fixtures/contracts/model_router_offline.json"
        ))
        .unwrap()
    }

    #[test]
    fn evaluation_plan_ignores_outcomes_and_canonicalizes_roster_order() {
        let mut original = input();
        original.sources[0].group_keys = vec!["b".into(), "a".into()];
        let mut second = original.sources[0].clone();
        second.source_id = "source-2".into();
        original.sources.push(second);
        let expected = router_evaluation_plan_sha256(&original).unwrap();
        let mut changed = original;
        let pair = changed.sources[0].paired.as_mut().unwrap();
        pair.economy.episode.quality.as_mut().unwrap().verdict = Acceptability::Unacceptable;
        pair.economy.episode.cost.as_mut().unwrap().total_usd = 100.0;
        changed.sources[1].paired = None;
        changed.sources[0].group_keys = vec!["a".into(), "b".into(), "a".into()];
        changed.sources.reverse();
        assert_eq!(router_evaluation_plan_sha256(&changed).unwrap(), expected);
    }

    #[test]
    fn evaluation_plan_rejects_ambiguous_source_identity() {
        let mut input = input();
        input.sources.push(input.sources[0].clone());
        assert!(router_evaluation_plan_sha256(&input).is_err());
    }
}

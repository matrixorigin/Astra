//! Durable Auto selection and the semantic admission that justified it.
use astra_turn_types::model_routing::{AutoModelRoutingPolicy, ModelRoutingReason};
use serde::{Deserialize, Serialize};

pub const EVENT_TYPE: &str = "model_routing_decision";
pub const DECISION_KEY: &str = "model-routing-v1";

/// Immutable run fact written before the first primary provider invocation.
/// Subsequent rounds/recovery reuse this choice and reauthorize its Offering.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRoutingDecision {
    pub schema_version: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub features: Option<astra_turn_types::model_routing::ModelRoutingFeatures>,
    /// Full canonical semantic decision, including graph, topology and capabilities.
    pub work_admission: Option<crate::WorkAdmissionDecision>,
    pub work_admission_skill_revision: usize,
    /// The same judgment's child-model requirement, never inferred from its
    /// Work decision after recovery.
    pub delegation_model_requirement: Option<crate::WorkAdmissionTruth>,
    /// Authenticated intent that the requirement described. Rebind only after
    /// matching it to the restored turn; owner generation can change.
    pub delegation_model_source: Option<astra_turn_types::DelegationModelInstructionSource>,
    pub policy_version: String,
    pub policy: AutoModelRoutingPolicy,
    pub run_id: String,
    pub session_id: String,
    pub selected_offering_id: String,
    pub selected_model: String,
    pub selected_contract_root: String,
    pub input_reference: Option<astra_turn_types::FeedbackResponseReference>,
    pub reason: ModelRoutingReason,
    pub assessment: Option<astra_turn_types::TurnAssessment>,
}

impl ModelRoutingDecision {
    pub fn validate_identity(&self, run_id: &str, session_id: &str) -> Result<(), String> {
        if self.schema_version != 2
            || self.run_id != run_id
            || self.session_id != session_id
            || self.delegation_model_requirement.is_some() != self.delegation_model_source.is_some()
            || self.delegation_model_requirement.is_some() && self.work_admission.is_none()
            || self
                .delegation_model_source
                .as_ref()
                .is_some_and(|source| source.run_id != run_id || source.session_id != session_id)
            || self.selected_model.trim().is_empty()
            || self.selected_model.len() > 255
            || (self.selected_offering_id != self.policy.economy_offering_id
                && self.selected_offering_id != self.policy.strong_offering_id)
        {
            return Err("Auto routing decision does not match its execution".into());
        }
        crate::validate_model_offering_id(&self.selected_offering_id)
            .map(|_| ())
            .map_err(|_| "Auto routing decision has an invalid Offering".to_string())
    }
}

/// The fenced event transaction owns both the immutable fact and its effective
/// run identity. Only a new decision may change the baseline, never a replay.
pub(crate) fn selection_for_new_events(
    run: &crate::runs::DurableRunRecord,
    events: &[serde_json::Value],
) -> Result<Option<ModelRoutingDecision>, String> {
    let mut selection = None;
    for event in events
        .iter()
        .filter(|event| event["event_type"] == EVENT_TYPE)
    {
        if selection.is_some() || event["idempotency_key"] != DECISION_KEY {
            return Err("Auto routing requires one immutable decision per run".into());
        }
        let decision: ModelRoutingDecision = serde_json::from_value(event["data"].clone())
            .map_err(|_| "Invalid durable Auto routing decision".to_string())?;
        decision.validate_identity(&run.run_id, &run.session_id)?;
        if run.model_offering_id.as_deref() != Some(&decision.policy.strong_offering_id)
            || run.resolved_model_name.is_none()
        {
            return Err("Auto routing baseline differs from the durable run".into());
        }
        selection = Some(decision);
    }
    Ok(selection)
}

/// Canonical structural predicate shared by online routing and offline validation.
pub fn routing_read_only_primary(decision: Option<&crate::WorkAdmissionDecision>) -> bool {
    decision.is_some_and(|decision| {
        matches!(decision, crate::WorkAdmissionDecision::NotRequired { .. })
            && decision.workspace_mutation()
                == astra_config::user_profile::WorkspaceMutationIntent::ReadOnly
            && decision.execution_topology() == crate::WorkExecutionTopology::Primary
            && decision.required_capabilities().is_empty()
    })
}

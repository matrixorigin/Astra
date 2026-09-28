//! Trusted, invocation-local model requirements for delegated execution.
//! Tool arguments cannot construct this admission; the runtime attaches it
//! after interpreting an authoritative user intent and before durable dispatch.

use serde::{Deserialize, Serialize};

use crate::{ModelSelection, RequestedModelPolicy};

/// Reserved request-context field used only for an authenticated CLI child
/// handoff. The value is a typed, source-bound requirement snapshot; it is not
/// a general-purpose client override.
pub const DELEGATED_MODEL_REQUIREMENTS_CONTEXT_KEY: &str = "__astra_delegated_model_requirements";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegationModelInstructionSource {
    pub user_id: String,
    pub session_id: String,
    pub run_id: String,
    pub turn_chain_id: String,
    pub owner_generation: u64,
    pub control_epoch: usize,
    pub applied_intent_id: Option<String>,
    pub session_turn: u32,
    pub user_intent_digest: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationReasoningEffort {
    Low,
    Medium,
    High,
    Max,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum DelegationReasoningRequirement {
    ModelDefault,
    Off,
    Effort { effort: DelegationReasoningEffort },
    Budget { tokens: u32 },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegationModelSlotConstraint {
    /// Zero-based index in the admitted canonical delegation batch.
    pub slot_index: u32,
    pub model_selection: Option<ModelSelection>,
    /// The requested policy remains distinct from the resolved Offering.
    /// Auto may already have a concrete selection after routing, while its
    /// provenance still needs to be visible on the child run.
    pub requested_model_policy: Option<RequestedModelPolicy>,
    /// Present whenever either the policy or the concrete selection is
    /// present, including before a configured name is batch-resolved.
    pub model_strength: Option<DelegationRequirementStrength>,
    pub reasoning: Option<DelegationReasoningRequirement>,
    pub reasoning_strength: Option<DelegationRequirementStrength>,
    /// Bounded source evidence for explaining task binding; never authority
    /// independent of the runtime-created source and exact slot identity.
    pub task_scope_quote: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DelegationModelAdmissionOutcome {
    ExplicitlyUnconstrained {
        slot_count: u32,
    },
    Constrained {
        slots: Vec<DelegationModelSlotConstraint>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegationModelAdmission {
    pub source: DelegationModelInstructionSource,
    pub invocation_id: String,
    /// Digest of exact canonical logical arguments before runtime correlation.
    pub arguments_digest: String,
    pub outcome: DelegationModelAdmissionOutcome,
    /// Trusted child-state projection for every canonical slot. It is frozen
    /// with the invocation rather than reconstructed from a later parent
    /// context snapshot or child prompt.
    pub child_requirements: Vec<DelegationIntentRequirements>,
}

/// Trusted, command-scoped selection for the local Team execution path.
/// Unlike a Server tool admission, it has no fabricated run, turn-chain,
/// generation, or invocation identity. The executor verifies the authenticated
/// command source and the canonical slot-plan digest before starting children.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectDelegationCommandIdentity {
    pub command_intent_id: String,
    pub session_turn: u32,
}

impl DirectDelegationCommandIdentity {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !is_canonical_command_intent_id(&self.command_intent_id) {
            return Err("direct delegation command identity is invalid");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectDelegationModelPlan {
    pub source: DelegationUserRequirementSource,
    pub slot_plan_digest: String,
    pub outcome: DelegationModelAdmissionOutcome,
    pub child_requirements: Vec<DelegationIntentRequirements>,
}

/// Maximum slots accepted by the shared model-admission contract. The public
/// agent fanout contract is 50 slots; direct Team keeps its narrower
/// `MAX_DIRECT_DELEGATION_SLOTS` boundary because it has a separate command
/// contract.
pub const MAX_MODEL_ADMISSION_SLOTS: usize = 50;
pub const MAX_DIRECT_DELEGATION_SLOTS: usize = 32;

/// Human instruction provenance survives child creation. It is deliberately
/// separate from the current run's dispatch epoch and invocation identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegationUserRequirementSource {
    pub user_id: String,
    pub session_id: String,
    pub session_turn: u32,
    pub applied_intent_id: Option<String>,
    /// Distinguishes a direct authenticated command from another intent in
    /// the same session turn. Server conversational turns leave it absent.
    pub command_intent_id: Option<String>,
    pub user_intent_digest: String,
}

impl DelegationUserRequirementSource {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.user_id.trim().is_empty()
            || self.session_id.trim().is_empty()
            || self.user_intent_digest.trim().is_empty()
        {
            return Err("delegation requirement source is incomplete");
        }
        if self.command_intent_id.as_deref().is_some_and(|command_id| {
            self.applied_intent_id.is_some() || !is_canonical_command_intent_id(command_id)
        }) {
            return Err("delegation command source identity is invalid");
        }
        Ok(())
    }
}

impl DirectDelegationModelPlan {
    #[allow(clippy::too_many_arguments)]
    pub fn validate_identity(
        &self,
        command: &DirectDelegationCommandIdentity,
        user_id: &str,
        session_id: &str,
        task_digest: &str,
        slot_plan_digest: &str,
        expected_slots: usize,
    ) -> Result<(), &'static str> {
        command.validate()?;
        self.source.validate()?;
        if expected_slots == 0 || expected_slots > MAX_DIRECT_DELEGATION_SLOTS {
            return Err("direct delegation plan has invalid slot count");
        }
        if self.source.user_id != user_id
            || self.source.session_id != session_id
            || self.source.session_turn != command.session_turn
            || self.source.command_intent_id.as_deref() != Some(command.command_intent_id.as_str())
            || self.source.applied_intent_id.is_some()
            || self.source.user_intent_digest != task_digest
            || self.slot_plan_digest != slot_plan_digest
            || !is_sha256_digest(&self.slot_plan_digest)
        {
            return Err("direct delegation model plan belongs to another command or slot plan");
        }
        if self.child_requirements.len() != expected_slots {
            return Err("direct delegation child requirements have wrong slot count");
        }
        for child in &self.child_requirements {
            child.validate()?;
            let origin = match child {
                DelegationIntentRequirements::Unassessed => None,
                DelegationIntentRequirements::Unconstrained { source }
                | DelegationIntentRequirements::Unresolved { source, .. }
                | DelegationIntentRequirements::Unavailable { source, .. }
                | DelegationIntentRequirements::CatalogResolutionFailed { source, .. }
                | DelegationIntentRequirements::Requirements { source, .. } => Some(source),
            }
            .ok_or("direct delegation child requirement is unassessed")?;
            if origin.user_id != self.source.user_id
                || origin.session_id != self.source.session_id
                || origin.session_turn != self.source.session_turn
                || origin.applied_intent_id != self.source.applied_intent_id
                || origin.command_intent_id != self.source.command_intent_id
                || origin.user_intent_digest != self.source.user_intent_digest
            {
                return Err("direct delegation child requirement source changed");
            }
        }
        match &self.outcome {
            DelegationModelAdmissionOutcome::ExplicitlyUnconstrained { slot_count }
                if *slot_count as usize == expected_slots => {}
            DelegationModelAdmissionOutcome::ExplicitlyUnconstrained { .. } => {
                return Err("direct delegation plan has invalid slot count");
            }
            DelegationModelAdmissionOutcome::Constrained { slots } => {
                if slots.len() != expected_slots {
                    return Err("direct delegation plan has missing or extra slots");
                }
                let mut has_requirement = false;
                for (index, slot) in slots.iter().enumerate() {
                    if slot.slot_index != index as u32
                        || (slot.model_selection.is_some() || slot.requested_model_policy.is_some())
                            != slot.model_strength.is_some()
                        || slot.reasoning.is_some() != slot.reasoning_strength.is_some()
                    {
                        return Err("direct delegation plan has invalid slot identity or strength");
                    }
                    has_requirement |= slot.model_selection.is_some()
                        || slot.requested_model_policy.is_some()
                        || slot.reasoning.is_some();
                }
                if !has_requirement {
                    return Err("direct delegation plan has no effective constraints");
                }
            }
        }
        Ok(())
    }
}

fn is_sha256_digest(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn is_canonical_command_intent_id(value: &str) -> bool {
    if value.len() != 36 || value.to_ascii_lowercase() != value {
        return false;
    }
    value.bytes().enumerate().all(|(index, byte)| match index {
        8 | 13 | 18 | 23 => byte == b'-',
        _ => byte.is_ascii_hexdigit(),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationRequirementPropagation {
    DirectChildren,
    Descendants,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationRequirementStrength {
    Default,
    Hard,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegationIntentRequirement {
    pub requirement_id: String,
    pub model_selection: Option<ModelSelection>,
    pub requested_model_policy: Option<RequestedModelPolicy>,
    pub reasoning: Option<DelegationReasoningRequirement>,
    /// A task-specific requirement still needs admitted applicability at each
    /// invocation; this quote alone never authorizes a slot assignment.
    pub task_scope_quote: Option<String>,
    pub propagation: DelegationRequirementPropagation,
    pub strength: DelegationRequirementStrength,
}

/// Safe, bounded evidence for a delegated-model lookup that did not resolve
/// to one eligible catalog entry. The count is scoped to the single
/// authorized Chat-catalog snapshot used for that assessment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegationCatalogResolutionFailure {
    /// Zero-based position in the extracted, bounded requirement list.
    pub requirement_index: u32,
    /// Eligible matches in the already-loaded catalog snapshot.
    pub match_count: u32,
}

impl DelegationCatalogResolutionFailure {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.requirement_index >= 16 || self.match_count == 1 {
            return Err("delegation catalog resolution failure is invalid");
        }
        Ok(())
    }

    /// A user-safe explanation derived only from typed counts, never from the
    /// extracted quote, provider response, or catalog rows.
    pub fn safe_message(&self) -> String {
        let requirement = self.requirement_index.saturating_add(1);
        if self.match_count == 0 {
            format!(
                "The requested model is not available in your model catalog (requirement {requirement}); no child was started."
            )
        } else {
            format!(
                "{} available models matched delegated model requirement {requirement}; please choose a model source. No child was started.",
                self.match_count
            )
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum DelegationIntentRequirements {
    #[default]
    Unassessed,
    Unconstrained {
        source: DelegationUserRequirementSource,
    },
    Unresolved {
        source: DelegationUserRequirementSource,
        reason: String,
    },
    Unavailable {
        source: DelegationUserRequirementSource,
        reason: String,
        attempts: u8,
    },
    CatalogResolutionFailed {
        source: DelegationUserRequirementSource,
        failure: DelegationCatalogResolutionFailure,
    },
    Requirements {
        source: DelegationUserRequirementSource,
        requirements: Vec<DelegationIntentRequirement>,
    },
}

impl DelegationIntentRequirements {
    pub fn validate(&self) -> Result<(), &'static str> {
        let source = match self {
            Self::Unassessed => return Ok(()),
            Self::Unconstrained { source }
            | Self::Unresolved { source, .. }
            | Self::Unavailable { source, .. }
            | Self::CatalogResolutionFailed { source, .. }
            | Self::Requirements { source, .. } => source,
        };
        source.validate()?;
        match self {
            Self::Unresolved { reason, .. } if reason.trim().is_empty() => {
                Err("unresolved delegation requirement has no reason")
            }
            Self::Unavailable {
                reason, attempts, ..
            } if reason.trim().is_empty() || !(1..=2).contains(attempts) => {
                Err("unavailable delegation assessment has invalid retry state")
            }
            Self::CatalogResolutionFailed { failure, .. } => failure.validate(),
            Self::Requirements { requirements, .. } => {
                if requirements.is_empty() || requirements.len() > 16 {
                    return Err("delegation requirement set has invalid size");
                }
                let mut ids = std::collections::BTreeSet::new();
                for item in requirements {
                    if item.requirement_id.trim().is_empty()
                        || !ids.insert(item.requirement_id.as_str())
                        || (item.model_selection.is_none()
                            && item.requested_model_policy.is_none()
                            && item.reasoning.is_none())
                        || item
                            .model_selection
                            .as_ref()
                            .is_some_and(|model| model.offering_id.trim().is_empty())
                        || item
                            .task_scope_quote
                            .as_ref()
                            .is_some_and(|scope| scope.trim().is_empty())
                        || matches!(
                            item.reasoning,
                            Some(DelegationReasoningRequirement::Budget { tokens: 0 })
                        )
                    {
                        return Err("delegation requirement is incomplete or duplicated");
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Project only restrictions explicitly admitted to apply beyond this
    /// run's direct children. An empty projection is explicit, not a missing
    /// inheritance record.
    pub fn for_child_descendants(&self) -> Result<Self, &'static str> {
        self.validate()?;
        Ok(match self {
            Self::Requirements {
                source,
                requirements,
            } => {
                let inherited = requirements
                    .iter()
                    .filter(|item| {
                        item.propagation == DelegationRequirementPropagation::Descendants
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                if inherited.is_empty() {
                    Self::Unconstrained {
                        source: source.clone(),
                    }
                } else {
                    Self::Requirements {
                        source: source.clone(),
                        requirements: inherited,
                    }
                }
            }
            other => other.clone(),
        })
    }
}

impl DelegationModelAdmission {
    pub fn validate_identity(
        &self,
        invocation_id: &str,
        arguments_digest: &str,
        control_epoch: usize,
        expected_slots: usize,
    ) -> Result<(), &'static str> {
        if self.invocation_id != invocation_id
            || self.arguments_digest != arguments_digest
            || self.source.control_epoch != control_epoch
        {
            return Err("delegation model admission belongs to another invocation or intent");
        }
        if self.child_requirements.len() != expected_slots {
            return Err("delegation child requirement projection has wrong slot count");
        }
        for child in &self.child_requirements {
            child.validate()?;
            let origin = match child {
                DelegationIntentRequirements::Unassessed => None,
                DelegationIntentRequirements::Unconstrained { source }
                | DelegationIntentRequirements::Unresolved { source, .. }
                | DelegationIntentRequirements::Unavailable { source, .. }
                | DelegationIntentRequirements::CatalogResolutionFailed { source, .. }
                | DelegationIntentRequirements::Requirements { source, .. } => Some(source),
            };
            if origin.is_some_and(|origin| {
                origin.user_id != self.source.user_id
                    || origin.session_id != self.source.session_id
                    || origin.applied_intent_id != self.source.applied_intent_id
                    || origin.user_intent_digest != self.source.user_intent_digest
            }) {
                return Err("delegation child requirement source changed owner");
            }
        }
        match &self.outcome {
            DelegationModelAdmissionOutcome::ExplicitlyUnconstrained { slot_count } => {
                if *slot_count == 0
                    || *slot_count as usize > MAX_MODEL_ADMISSION_SLOTS
                    || *slot_count as usize != expected_slots
                {
                    return Err("delegation model admission has invalid slot count");
                }
            }
            DelegationModelAdmissionOutcome::Constrained { slots } => {
                if slots.is_empty()
                    || slots.len() > MAX_MODEL_ADMISSION_SLOTS
                    || slots.len() != expected_slots
                {
                    return Err("delegation model admission has invalid slots");
                }
                for (index, slot) in slots.iter().enumerate() {
                    if slot.slot_index != index as u32 {
                        return Err("delegation model admission has missing or reordered slots");
                    }
                    if (slot.model_selection.is_some() || slot.requested_model_policy.is_some())
                        != slot.model_strength.is_some()
                        || slot.reasoning.is_some() != slot.reasoning_strength.is_some()
                    {
                        return Err("delegation model requirement strength is missing or orphaned");
                    }
                }
                if slots.iter().all(|slot| {
                    slot.model_selection.is_none()
                        && slot.requested_model_policy.is_none()
                        && slot.reasoning.is_none()
                }) {
                    return Err("constrained delegation has no model requirements");
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_rejects_wrong_identity_and_swapped_slots() {
        let source = DelegationModelInstructionSource {
            user_id: "u".into(),
            session_id: "s".into(),
            run_id: "r".into(),
            turn_chain_id: "t".into(),
            owner_generation: 1,
            control_epoch: 2,
            applied_intent_id: None,
            session_turn: 3,
            user_intent_digest: "digest".into(),
        };
        let mut admission = DelegationModelAdmission {
            source,
            invocation_id: "call".into(),
            arguments_digest: "args".into(),
            child_requirements: vec![Default::default(); 2],
            outcome: DelegationModelAdmissionOutcome::Constrained {
                slots: vec![
                    DelegationModelSlotConstraint {
                        slot_index: 0,
                        model_selection: Some(ModelSelection {
                            offering_id: "offer-b".into(),
                        }),
                        requested_model_policy: None,
                        model_strength: Some(DelegationRequirementStrength::Hard),
                        reasoning: None,
                        reasoning_strength: None,
                        task_scope_quote: Some("review".into()),
                    },
                    DelegationModelSlotConstraint {
                        slot_index: 1,
                        model_selection: None,
                        requested_model_policy: None,
                        model_strength: None,
                        reasoning: None,
                        reasoning_strength: None,
                        task_scope_quote: None,
                    },
                ],
            },
        };
        assert!(admission.validate_identity("call", "args", 2, 2).is_ok());
        assert!(admission.validate_identity("call", "args", 2, 1).is_err());
        assert!(admission.validate_identity("other", "args", 2, 2).is_err());
        assert!(
            admission
                .validate_identity("call", "changed", 2, 2)
                .is_err()
        );
        assert!(admission.validate_identity("call", "args", 3, 2).is_err());
        if let DelegationModelAdmissionOutcome::Constrained { slots } = &mut admission.outcome {
            slots.swap(0, 1);
        }
        assert!(admission.validate_identity("call", "args", 2, 2).is_err());
    }

    #[test]
    fn shared_admission_uses_the_public_fanout_bound() {
        let source = DelegationModelInstructionSource {
            user_id: "u".into(),
            session_id: "s".into(),
            run_id: "r".into(),
            turn_chain_id: "t".into(),
            owner_generation: 1,
            control_epoch: 2,
            applied_intent_id: None,
            session_turn: 3,
            user_intent_digest: "digest".into(),
        };
        let admission = |slot_count: usize| DelegationModelAdmission {
            source: source.clone(),
            invocation_id: "call".into(),
            arguments_digest: "args".into(),
            child_requirements: vec![Default::default(); slot_count],
            outcome: DelegationModelAdmissionOutcome::ExplicitlyUnconstrained {
                slot_count: slot_count as u32,
            },
        };
        assert!(
            admission(MAX_MODEL_ADMISSION_SLOTS)
                .validate_identity("call", "args", 2, MAX_MODEL_ADMISSION_SLOTS,)
                .is_ok()
        );
        assert!(
            admission(MAX_MODEL_ADMISSION_SLOTS + 1)
                .validate_identity("call", "args", 2, MAX_MODEL_ADMISSION_SLOTS + 1,)
                .is_err()
        );
    }

    #[test]
    fn direct_team_plan_is_bound_to_command_task_and_ordered_slots() {
        let command_id = "6bca9f9c-6d18-4579-bce1-2b45f573a098";
        let command = DirectDelegationCommandIdentity {
            command_intent_id: command_id.into(),
            session_turn: 4,
        };
        let source = DelegationUserRequirementSource {
            user_id: "user".into(),
            session_id: "session".into(),
            session_turn: 4,
            applied_intent_id: None,
            command_intent_id: Some(command_id.into()),
            user_intent_digest: "sha256:task-digest".into(),
        };
        let child = DelegationIntentRequirements::Unconstrained {
            source: source.clone(),
        };
        let mut plan = DirectDelegationModelPlan {
            source,
            slot_plan_digest: format!("sha256:{}", "a".repeat(64)),
            outcome: DelegationModelAdmissionOutcome::Constrained {
                slots: vec![DelegationModelSlotConstraint {
                    slot_index: 0,
                    model_selection: Some(ModelSelection {
                        offering_id: "authorized-offering".into(),
                    }),
                    requested_model_policy: None,
                    model_strength: Some(DelegationRequirementStrength::Hard),
                    reasoning: None,
                    reasoning_strength: None,
                    task_scope_quote: None,
                }],
            },
            child_requirements: vec![child],
        };
        let plan_digest = plan.slot_plan_digest.clone();
        assert!(
            plan.validate_identity(
                &command,
                "user",
                "session",
                "sha256:task-digest",
                &plan_digest,
                1,
            )
            .is_ok()
        );
        assert!(
            plan.validate_identity(
                &command,
                "other-user",
                "session",
                "sha256:task-digest",
                &plan_digest,
                1,
            )
            .is_err()
        );
        assert!(
            plan.validate_identity(
                &command,
                "user",
                "session",
                "sha256:changed-task",
                &plan_digest,
                1,
            )
            .is_err()
        );
        assert!(
            plan.validate_identity(
                &command,
                "user",
                "session",
                "sha256:task-digest",
                &plan_digest,
                2,
            )
            .is_err()
        );
        plan.slot_plan_digest = "sha256:truncated".into();
        assert!(
            plan.validate_identity(
                &command,
                "user",
                "session",
                "sha256:task-digest",
                &plan_digest,
                1,
            )
            .is_err()
        );
        plan.slot_plan_digest = plan_digest.clone();
        let other_command = DirectDelegationCommandIdentity {
            command_intent_id: "a6f7e88f-7dd5-4cfb-b4b2-3c1db1a8e72c".into(),
            session_turn: 4,
        };
        assert!(
            plan.validate_identity(
                &other_command,
                "user",
                "session",
                "sha256:task-digest",
                &plan_digest,
                1,
            )
            .is_err(),
            "a plan cannot be replayed under a different authenticated command"
        );
    }

    #[test]
    fn child_projection_consumes_direct_only_requirement_without_losing_source() {
        let source = DelegationUserRequirementSource {
            user_id: "user".into(),
            session_id: "session".into(),
            session_turn: 1,
            applied_intent_id: None,
            command_intent_id: None,
            user_intent_digest: "digest".into(),
        };
        let requirement = |id: &str, propagation| DelegationIntentRequirement {
            requirement_id: id.into(),
            model_selection: Some(ModelSelection {
                offering_id: "offer".into(),
            }),
            requested_model_policy: None,
            reasoning: None,
            task_scope_quote: None,
            propagation,
            strength: DelegationRequirementStrength::Hard,
        };
        let direct = DelegationIntentRequirements::Requirements {
            source: source.clone(),
            requirements: vec![requirement(
                "direct",
                DelegationRequirementPropagation::DirectChildren,
            )],
        };
        assert_eq!(
            direct.for_child_descendants().unwrap(),
            DelegationIntentRequirements::Unconstrained {
                source: source.clone()
            }
        );
        let mixed = DelegationIntentRequirements::Requirements {
            source: source.clone(),
            requirements: vec![
                requirement("direct", DelegationRequirementPropagation::DirectChildren),
                requirement("subtree", DelegationRequirementPropagation::Descendants),
            ],
        };
        assert_eq!(
            mixed.for_child_descendants().unwrap(),
            DelegationIntentRequirements::Requirements {
                source,
                requirements: vec![requirement(
                    "subtree",
                    DelegationRequirementPropagation::Descendants
                )]
            }
        );
        let malformed = DelegationIntentRequirements::Requirements {
            source: DelegationUserRequirementSource {
                user_id: "user".into(),
                session_id: "session".into(),
                session_turn: 1,
                applied_intent_id: None,
                command_intent_id: None,
                user_intent_digest: "digest".into(),
            },
            requirements: Vec::new(),
        };
        assert!(malformed.for_child_descendants().is_err());
    }

    #[test]
    fn catalog_resolution_failure_is_bounded_and_survives_durable_encoding() {
        let failure = DelegationIntentRequirements::CatalogResolutionFailed {
            source: DelegationUserRequirementSource {
                user_id: "user".into(),
                session_id: "session".into(),
                session_turn: 4,
                applied_intent_id: None,
                command_intent_id: None,
                user_intent_digest: "digest".into(),
            },
            failure: DelegationCatalogResolutionFailure {
                requirement_index: 2,
                match_count: 3,
            },
        };
        failure.validate().unwrap();
        assert_eq!(failure.for_child_descendants().unwrap(), failure);
        let encoded = serde_json::to_value(&failure).unwrap();
        let restored: DelegationIntentRequirements = serde_json::from_value(encoded).unwrap();
        assert_eq!(restored, failure);

        for (requirement_index, match_count) in [(16, 0), (0, 1)] {
            let invalid = DelegationIntentRequirements::CatalogResolutionFailed {
                source: DelegationUserRequirementSource {
                    user_id: "user".into(),
                    session_id: "session".into(),
                    session_turn: 4,
                    applied_intent_id: None,
                    command_intent_id: None,
                    user_intent_digest: "digest".into(),
                },
                failure: DelegationCatalogResolutionFailure {
                    requirement_index,
                    match_count,
                },
            };
            assert!(invalid.validate().is_err());
        }
    }
}

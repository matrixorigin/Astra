//! Bounded verification evidence carried by an unfinished execution handoff.
//! Decoding these facts does not authorize execution. A raw snapshot still
//! requires exact invocation-ledger validation; an atomically adopted
//! execution handoff may use its exact locked checkpoint as provenance for the
//! historical prefix, while current execution authority remains separate.

use crate::{DispatchCertainty, StopHook, ToolInvocationCompletionRef, ToolInvocationState};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationEvidence {
    pub ordinal: u64,
    pub invocation: ToolInvocationCompletionRef,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoundVerificationFrontier {
    pub canonical_turn_chain_id: String,
    pub contract: Vec<StopHook>,
    pub processed_through: u64,
    #[serde(deserialize_with = "crate::completion_settlement::deserialize_required_option")]
    pub mutation: Option<VerificationEvidence>,
    pub proofs: Vec<Option<VerificationEvidence>>,
    #[serde(deserialize_with = "crate::completion_settlement::deserialize_required_option")]
    pub workspace_root: Option<String>,
}

impl BoundVerificationFrontier {
    /// Every retained authority reference, including literal-artifact ancestry.
    /// Callers deduplicate by invocation identity before exact ledger reads.
    pub fn evidence(&self) -> impl Iterator<Item = &VerificationEvidence> {
        self.mutation.iter().chain(self.proofs.iter().flatten())
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.canonical_turn_chain_id.trim().is_empty()
            || self.contract.iter().any(|hook| !hook.authoritative)
            || self.proofs.len() != self.contract.len()
        {
            return Err("verification frontier chain or contract is invalid".into());
        }
        if (self.contract.is_empty() && self.mutation.is_some())
            || (self.mutation.is_none() && self.proofs.iter().any(Option::is_some))
        {
            return Err("verification proofs without a source mutation".into());
        }
        let mut ordinals = std::collections::BTreeMap::new();
        let mut identities = std::collections::BTreeMap::new();
        for evidence in self.evidence() {
            if evidence.ordinal == 0
                || evidence.ordinal > self.processed_through
                || evidence.invocation.identity.turn_chain_id != self.canonical_turn_chain_id
            {
                return Err("verification evidence is outside its execution frontier".into());
            }
            let reference = &evidence.invocation;
            if !reference.state.is_terminal()
                || reference.completion_source.is_some()
                || reference.dispatch_certainty != reference.state.required_dispatch_certainty()
                || (reference.state == ToolInvocationState::OutcomeUnknown)
                    != reference.outcome_digest.is_none()
            {
                return Err("verification evidence has an invalid completion state".into());
            }
            if let Some(digest) = &reference.outcome_digest {
                crate::tool_invocation::validate_sha256_content_id("outcome_digest", digest)
                    .map_err(|error| error.to_string())?;
            }
            if ordinals
                .insert(evidence.ordinal, reference)
                .is_some_and(|old| old != reference)
                || identities
                    .insert(reference.identity.storage_key(), evidence.ordinal)
                    .is_some_and(|old| old != evidence.ordinal)
            {
                return Err("verification evidence ordinal and identity disagree".into());
            }
        }
        for proof in self.proofs.iter().flatten() {
            if self
                .mutation
                .as_ref()
                .is_none_or(|mutation| proof.ordinal <= mutation.ordinal)
                || proof.invocation.state != ToolInvocationState::Succeeded
                || proof.invocation.dispatch_certainty != DispatchCertainty::Dispatched
                || proof.invocation.completion_source.is_some()
                || proof.invocation.outcome_digest.is_none()
            {
                return Err(
                    "verification proof is not a successful post-mutation execution".into(),
                );
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub enum VerificationUnavailable {
    MissingTurnChain,
    UnboundMutation,
    UnboundProof { hook_index: usize },
    InvalidEvidence,
    HistoryUnavailable,
}

/// Unavailable preserves custody, but cannot stand in for an empty proof set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum VerificationHandoff {
    Bound {
        snapshot: Box<BoundVerificationFrontier>,
    },
    Unavailable {
        detail: VerificationUnavailable,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DurableToolReference, ToolInvocationFingerprint, ToolInvocationIdentity};

    fn snapshot() -> BoundVerificationFrontier {
        let evidence = |id: &str, ordinal| VerificationEvidence {
            ordinal,
            invocation: ToolInvocationCompletionRef {
                identity: ToolInvocationIdentity::new("user", "session", "run", "chain", id)
                    .unwrap(),
                fingerprint: ToolInvocationFingerprint::new(
                    DurableToolReference::built_in("bash", "v1").unwrap(),
                    &serde_json::json!({}),
                    "policy",
                )
                .unwrap(),
                state: ToolInvocationState::Succeeded,
                dispatch_certainty: DispatchCertainty::Dispatched,
                completion_source: None,
                outcome_digest: Some(format!("sha256:{}", "a".repeat(64))),
            },
        };
        let hook = StopHook {
            label: "verify".into(),
            command: "check".into(),
            working_dir: None,
            depends_on: vec![],
            timeout_secs: None,

            authoritative: true,
        };
        BoundVerificationFrontier {
            canonical_turn_chain_id: "chain".into(),
            contract: vec![hook.clone(), hook],
            processed_through: 2,
            mutation: Some(evidence("write", 1)),
            proofs: vec![Some(evidence("check", 2)); 2],
            workspace_root: None,
        }
    }

    #[test]
    fn bound_verification_shape_rejects_forged_order_state_and_digest() {
        let valid = snapshot();
        valid.validate().unwrap(); // One invocation can satisfy two hooks.
        for state in [
            ToolInvocationState::Prepared,
            ToolInvocationState::Dispatched,
        ] {
            let mut invalid = valid.clone();
            invalid.mutation.as_mut().unwrap().invocation.state = state;
            assert!(invalid.validate().is_err());
        }
        let mut unknown = valid.clone();
        let reference = &mut unknown.mutation.as_mut().unwrap().invocation;
        reference.state = ToolInvocationState::OutcomeUnknown;
        reference.dispatch_certainty = reference.state.required_dispatch_certainty();
        reference.outcome_digest = None;
        unknown.validate().unwrap();
        let mut invalid = valid.clone();
        invalid.proofs[1].as_mut().unwrap().ordinal = 1;
        assert!(invalid.validate().is_err());
        let mut invalid = valid.clone();
        invalid.proofs[1]
            .as_mut()
            .unwrap()
            .invocation
            .outcome_digest = Some(format!("sha256:{}", "b".repeat(64)));
        assert!(invalid.validate().is_err());
        let mut invalid = valid.clone();
        invalid.mutation.as_mut().unwrap().invocation.outcome_digest = Some(String::new());
        assert!(invalid.validate().is_err());
        let mut invalid = valid;
        invalid.contract.clear();
        invalid.proofs.clear();
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn retired_workspace_observation_snapshot_is_rejected_without_migration() {
        let mut wire = serde_json::to_value(snapshot()).unwrap();
        wire["workspace"] = serde_json::json!({"barrier":null,"proof":null,"latest_source":null});
        assert!(serde_json::from_value::<BoundVerificationFrontier>(wire).is_err());
    }

    #[test]
    fn verification_handoff_requires_explicit_shape_and_nullable_mutation() {
        let wire = serde_json::to_value(VerificationHandoff::Bound {
            snapshot: Box::new(snapshot()),
        })
        .unwrap();
        let mut missing = wire.clone();
        missing["snapshot"]
            .as_object_mut()
            .unwrap()
            .remove("mutation");
        assert!(serde_json::from_value::<VerificationHandoff>(missing).is_err());
        assert!(serde_json::from_value::<VerificationHandoff>(serde_json::json!({})).is_err());
        let decoded: VerificationHandoff = serde_json::from_value(wire).unwrap();
        assert!(matches!(decoded, VerificationHandoff::Bound { .. }));
    }
}

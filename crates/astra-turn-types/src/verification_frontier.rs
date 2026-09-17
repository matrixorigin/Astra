//! Bounded verification evidence carried by an unfinished execution handoff.
//! Decoding these facts does not authorize execution: references must still be
//! resolved against the invocation ledger under the recovered run's authority.

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
pub struct WorkspaceMutationSource {
    pub evidence: VerificationEvidence,
    /// None preserves a newer writer that cannot authorize an older artifact.
    #[serde(deserialize_with = "crate::completion_settlement::deserialize_required_option")]
    pub delivered_path: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceObservationProof {
    pub evidence: VerificationEvidence,
    /// Executing a literal artifact additionally needs its preceding delivery.
    #[serde(deserialize_with = "crate::completion_settlement::deserialize_required_option")]
    pub literal_source: Option<WorkspaceMutationSource>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoundWorkspaceObservation {
    #[serde(deserialize_with = "crate::completion_settlement::deserialize_required_option")]
    pub barrier: Option<VerificationEvidence>,
    #[serde(deserialize_with = "crate::completion_settlement::deserialize_required_option")]
    pub proof: Option<WorkspaceObservationProof>,
    #[serde(deserialize_with = "crate::completion_settlement::deserialize_required_option")]
    pub latest_source: Option<WorkspaceMutationSource>,
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
    pub workspace: BoundWorkspaceObservation,
}

impl BoundVerificationFrontier {
    /// Every retained authority reference, including literal-artifact ancestry.
    /// Callers deduplicate by invocation identity before exact ledger reads.
    pub fn evidence(&self) -> impl Iterator<Item = &VerificationEvidence> {
        self.mutation
            .iter()
            .chain(self.proofs.iter().flatten())
            .chain(self.workspace.barrier.iter())
            .chain(self.workspace.proof.iter().map(|proof| &proof.evidence))
            .chain(
                self.workspace
                    .latest_source
                    .iter()
                    .map(|source| &source.evidence),
            )
            .chain(
                self.workspace
                    .proof
                    .iter()
                    .filter_map(|proof| proof.literal_source.as_ref())
                    .map(|source| &source.evidence),
            )
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
        if let Some(proof) = &self.workspace.proof {
            if self
                .workspace
                .barrier
                .as_ref()
                .is_none_or(|barrier| proof.evidence.ordinal < barrier.ordinal)
                || proof.evidence.invocation.state != ToolInvocationState::Succeeded
            {
                return Err(
                    "workspace observation is not a successful post-mutation execution".into(),
                );
            }
            if let Some(source) = &proof.literal_source
                && (source.evidence.ordinal >= proof.evidence.ordinal
                    || source.delivered_path.is_none()
                    || self.workspace.latest_source.as_ref().is_none_or(|latest| {
                        latest.evidence.ordinal < source.evidence.ordinal
                            || (latest.evidence.ordinal == source.evidence.ordinal
                                && latest != source)
                            || (latest.evidence.ordinal > source.evidence.ordinal
                                && latest.evidence.ordinal < proof.evidence.ordinal)
                    }))
            {
                return Err("literal observation has no preceding artifact delivery".into());
            }
        }
        for source in self.workspace.latest_source.iter().chain(
            self.workspace
                .proof
                .iter()
                .filter_map(|proof| proof.literal_source.as_ref()),
        ) {
            if source.delivered_path.as_ref().is_some_and(|path| {
                path.trim().is_empty()
                    || source.evidence.invocation.state != ToolInvocationState::Succeeded
            }) {
                return Err("workspace delivery is not a successful named artifact".into());
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
    UnboundWorkspaceEvidence,
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
            cache_key: None,
            authoritative: true,
        };
        BoundVerificationFrontier {
            canonical_turn_chain_id: "chain".into(),
            contract: vec![hook.clone(), hook],
            processed_through: 2,
            mutation: Some(evidence("write", 1)),
            proofs: vec![Some(evidence("check", 2)); 2],
            workspace_root: None,
            workspace: BoundWorkspaceObservation {
                barrier: None,
                proof: None,
                latest_source: None,
            },
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
    fn workspace_frontier_requires_successful_proof_and_preceding_literal_source() {
        let mut valid = snapshot();
        valid.workspace = BoundWorkspaceObservation {
            barrier: valid.mutation.clone(),
            proof: Some(WorkspaceObservationProof {
                evidence: valid.proofs[0].clone().unwrap(),
                literal_source: Some(WorkspaceMutationSource {
                    evidence: valid.mutation.clone().unwrap(),
                    delivered_path: Some("/app/solution.py".into()),
                }),
            }),
            latest_source: Some(WorkspaceMutationSource {
                evidence: valid.mutation.clone().unwrap(),
                delivered_path: Some("/app/solution.py".into()),
            }),
        };
        valid.workspace_root = Some("/app".into());
        valid.validate().unwrap();
        // The observing compound call may itself become the latest writer.
        let mut compound = valid.clone();
        compound.workspace.latest_source = Some(WorkspaceMutationSource {
            evidence: compound.workspace.proof.as_ref().unwrap().evidence.clone(),
            delivered_path: None,
        });
        compound.validate().unwrap();
        // A DIFFERENT writer between delivery and observation contradicts
        // the assertion that this observation used the latest delivery.
        let mut intervening = valid.clone();
        intervening.processed_through = 3;
        for proof in intervening.proofs.iter_mut().flatten() {
            proof.ordinal = 3;
        }
        intervening
            .workspace
            .proof
            .as_mut()
            .unwrap()
            .evidence
            .ordinal = 3;
        let latest = intervening.workspace.latest_source.as_mut().unwrap();
        latest.evidence.ordinal = 2;
        latest.evidence.invocation.identity.invocation_id = "intervening-writer".into();
        latest.delivered_path = None;
        assert!(intervening.validate().is_err());
        // A source after the proof is not an intervening writer. Whether it
        // creates fresh debt is owned by the separate general barrier rule.
        intervening
            .workspace
            .latest_source
            .as_mut()
            .unwrap()
            .evidence
            .ordinal = 4;
        intervening.processed_through = 4;
        intervening.validate().unwrap();
        let mut invalid = valid.clone();
        invalid.workspace.barrier = None;
        assert!(invalid.validate().is_err());
        let mut invalid = valid.clone();
        invalid
            .workspace
            .proof
            .as_mut()
            .unwrap()
            .literal_source
            .as_mut()
            .unwrap()
            .delivered_path = None;
        assert!(invalid.validate().is_err());
        let mut invalid = valid.clone();
        invalid
            .workspace
            .proof
            .as_mut()
            .unwrap()
            .literal_source
            .as_mut()
            .unwrap()
            .evidence = invalid.proofs[0].clone().unwrap();
        assert!(invalid.validate().is_err());
        let wire = serde_json::to_value(&valid).unwrap();
        for field in ["workspace_root", "workspace"] {
            let mut missing = wire.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<BoundVerificationFrontier>(missing).is_err());
        }
        for field in ["barrier", "proof", "latest_source"] {
            let mut missing = wire.clone();
            missing["workspace"].as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<BoundVerificationFrontier>(missing).is_err());
        }
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

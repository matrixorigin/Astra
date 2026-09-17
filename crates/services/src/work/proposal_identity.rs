//! Shared invocation identity for runtime-authored Work proposals and their
//! durable execution barriers. Argument identity is separate from proposal ID.

use super::{WorkChangeRef, WorkProposalId, WorkProposalKind};
use sha2::{Digest, Sha256};

#[derive(Clone, Copy)]
pub struct WorkProposalInvocationIdentity<'a> {
    pub owner_id: &'a str,
    pub session_id: &'a str,
    pub work_id: &'a str,
    pub branch_id: &'a str,
    pub run_id: &'a str,
    pub turn_chain_id: &'a str,
    pub tool_call_id: &'a str,
}

impl WorkProposalInvocationIdentity<'_> {
    fn digest(&self, kind: WorkProposalKind) -> Result<String, String> {
        let domain = match kind {
            WorkProposalKind::PlanPatch => "work-plan-invocation-v1",
            WorkProposalKind::CriteriaSet => "work-criteria-invocation-v1",
        };
        let mut hasher = Sha256::new();
        for segment in [
            domain,
            self.owner_id,
            self.session_id,
            self.work_id,
            self.branch_id,
            self.run_id.trim(),
            self.turn_chain_id.trim(),
            self.tool_call_id.trim(),
        ] {
            if segment.is_empty() {
                return Err("Work proposal invocation identity is incomplete".into());
            }
            hasher.update((segment.len() as u64).to_be_bytes());
            hasher.update(segment.as_bytes());
        }
        Ok(format!("{:x}", hasher.finalize()))
    }

    pub fn proposal_id(&self, kind: WorkProposalKind) -> Result<WorkProposalId, String> {
        let digest = self.digest(kind)?;
        WorkProposalId::parse(format!("model-{}", &digest[..48])).map_err(|e| e.to_string())
    }

    pub fn derive(
        &self,
        kind: WorkProposalKind,
        canonical_arguments: &[u8],
    ) -> Result<(WorkProposalId, WorkChangeRef), String> {
        let invocation_digest = self.digest(kind)?;
        let proposal_id = WorkProposalId::parse(format!("model-{}", &invocation_digest[..48]))
            .map_err(|e| e.to_string())?;
        let argument_domain: &[u8] = match kind {
            WorkProposalKind::PlanPatch => b"work-plan-arguments-v1",
            WorkProposalKind::CriteriaSet => b"work-criteria-arguments-v1",
        };
        let mut hasher = Sha256::new();
        hasher.update(argument_domain);
        hasher.update(invocation_digest.as_bytes());
        hasher.update((canonical_arguments.len() as u64).to_be_bytes());
        hasher.update(canonical_arguments);
        let source = WorkChangeRef::parse(format!("tool-invocation-{:x}", hasher.finalize()))
            .map_err(|e| e.to_string())?;
        Ok((proposal_id, source))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proposal_marker_and_semantic_identity_share_one_invocation() {
        let identity = WorkProposalInvocationIdentity {
            owner_id: "owner",
            session_id: "session",
            work_id: "work",
            branch_id: "branch",
            run_id: "run",
            turn_chain_id: "turn",
            tool_call_id: "call",
        };
        let first = identity
            .derive(WorkProposalKind::PlanPatch, b"first")
            .unwrap();
        let revised = identity
            .derive(WorkProposalKind::PlanPatch, b"revised")
            .unwrap();
        assert_eq!(
            first.0,
            identity.proposal_id(WorkProposalKind::PlanPatch).unwrap()
        );
        assert_eq!(first.0, revised.0);
        assert_ne!(first.1, revised.1);
        assert_ne!(
            first.0,
            identity.proposal_id(WorkProposalKind::CriteriaSet).unwrap()
        );
    }
}

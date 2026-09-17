use astra_services::work::{
    WorkChangeRef, WorkProposalId, WorkProposalInvocationIdentity, WorkProposalKind,
};
use astra_tools::tool_engine::ToolInvocationMetadata;

use super::runtime_tool_executor::WorkRuntimeBinding;

#[derive(Clone, Copy)]
pub(super) enum RuntimeWorkProposalKind {
    Plan,
    Criteria,
}

/// Derive stable proposal/source identities from trusted invocation metadata
/// and canonical typed arguments. The scheduler uses the same identity owner.
pub(super) fn invocation_identity(
    binding: &WorkRuntimeBinding,
    invocation: ToolInvocationMetadata<'_>,
    kind: RuntimeWorkProposalKind,
    canonical_arguments: &[u8],
) -> Result<(WorkProposalId, WorkChangeRef), ()> {
    WorkProposalInvocationIdentity {
        owner_id: binding.owner_id.as_str(),
        session_id: binding.session_id.as_str(),
        work_id: binding.work_id.as_str(),
        branch_id: binding.branch_id.as_str(),
        run_id: invocation.run_id.ok_or(())?,
        turn_chain_id: invocation.turn_chain_id.ok_or(())?,
        tool_call_id: invocation.tool_call_id.ok_or(())?,
    }
    .derive(
        match kind {
            RuntimeWorkProposalKind::Plan => WorkProposalKind::PlanPatch,
            RuntimeWorkProposalKind::Criteria => WorkProposalKind::CriteriaSet,
        },
        canonical_arguments,
    )
    .map_err(|_| ())
}

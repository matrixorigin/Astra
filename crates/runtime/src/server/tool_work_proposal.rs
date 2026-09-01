use astra_services::work::{WorkChangeRef, WorkProposalId};
use astra_tools::tool_engine::ToolInvocationMetadata;
use sha2::{Digest, Sha256};

use super::runtime_tool_executor::WorkRuntimeBinding;

#[derive(Clone, Copy)]
pub(super) enum RuntimeWorkProposalKind {
    Plan,
    Criteria,
}

fn required_identity(value: Option<&str>) -> Result<&str, ()> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or(())
}

impl RuntimeWorkProposalKind {
    const fn identity_domain(self) -> &'static str {
        match self {
            Self::Plan => "work-plan-invocation-v1",
            Self::Criteria => "work-criteria-invocation-v1",
        }
    }

    const fn argument_domain(self) -> &'static [u8] {
        match self {
            Self::Plan => b"work-plan-arguments-v1",
            Self::Criteria => b"work-criteria-arguments-v1",
        }
    }
}

/// Derive stable proposal/source identities from trusted invocation metadata
/// and canonical typed arguments. User text is deliberately not an input.
pub(super) fn invocation_identity(
    binding: &WorkRuntimeBinding,
    invocation: ToolInvocationMetadata<'_>,
    kind: RuntimeWorkProposalKind,
    canonical_arguments: &[u8],
) -> Result<(WorkProposalId, WorkChangeRef), ()> {
    let run_id = required_identity(invocation.run_id)?;
    let turn_chain_id = required_identity(invocation.turn_chain_id)?;
    let tool_call_id = required_identity(invocation.tool_call_id)?;
    let mut hasher = Sha256::new();
    for segment in [
        kind.identity_domain(),
        binding.owner_id.as_str(),
        binding.session_id.as_str(),
        binding.work_id.as_str(),
        binding.branch_id.as_str(),
        run_id,
        turn_chain_id,
        tool_call_id,
    ] {
        hasher.update(
            u64::try_from(segment.len())
                .expect("bounded invocation identity length fits u64")
                .to_be_bytes(),
        );
        hasher.update(segment.as_bytes());
    }
    let invocation_digest = format!("{:x}", hasher.finalize());
    let proposal_id = WorkProposalId::parse(format!("model-{}", &invocation_digest[..48]))
        .expect("a hex digest is a valid proposal identity");
    let mut semantic_hasher = Sha256::new();
    semantic_hasher.update(kind.argument_domain());
    semantic_hasher.update(invocation_digest.as_bytes());
    semantic_hasher.update(
        u64::try_from(canonical_arguments.len())
            .expect("bounded canonical arguments length fits u64")
            .to_be_bytes(),
    );
    semantic_hasher.update(canonical_arguments);
    let semantic_digest = format!("{:x}", semantic_hasher.finalize());
    let source_ref = WorkChangeRef::parse(format!("tool-invocation-{semantic_digest}"))
        .expect("a hex digest is a valid change reference");
    Ok((proposal_id, source_ref))
}

//! Durable, provider-neutral evidence for deferred-tool invocation.

use serde::{Deserialize, Serialize};

use crate::ResolvedToolDescriptorRef;

/// Evidence that a deferred tool was explicitly selected from the prompt's
/// current catalog.
///
/// This is not an execution grant. It binds a public tool name to the exact
/// compact schema the model inspected; server hosts additionally retain the
/// resolved provider descriptor before checkpointing it. Every
/// invocation must still re-resolve current binding, policy, Work role, and
/// provider offer before execution.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeferredToolActivation {
    pub name: String,
    pub schema_digest: String,
    /// Exact provider-owned descriptor admitted for the request. A new request
    /// may rebind unchanged contract knowledge to its current admitted provider;
    /// emitted calls stay pinned through approval and dispatch. Hosts
    /// that cannot establish this identity must fail closed before dispatch;
    /// `None` remains useful for provider-neutral core tests and is never an
    /// execution grant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub descriptor: Option<ResolvedToolDescriptorRef>,
}

//! Delegation lookup trait — abstracts access to the delegation hierarchy.
//!
//! The router needs to resolve `Parent` targets and record sub-runs, but the
//! actual `DelegationTracker` lives in runtime. This trait allows the messaging
//! crate to remain decoupled from the runtime's delegation engine.

use async_trait::async_trait;

/// Information about a sub-run in a delegation hierarchy.
#[derive(Clone, Debug)]
pub struct SubRunInfo {
    pub run_id: String,
    pub parent_run_id: String,
    pub delegation_id: String,
    pub agent_id: String,
    pub depth: u32,
}

/// Trait for looking up delegation relationships.
///
/// Implemented by `DelegationTracker` in the runtime crate.
#[async_trait]
pub trait DelegationLookup: Send + Sync {
    /// Get the parent run_id for a child run.
    async fn get_parent(&self, run_id: &str) -> Option<String>;

    /// Get the agent_id for a run.
    async fn get_agent_id(&self, run_id: &str) -> Option<String>;

    /// Get the delegation depth for a run.
    async fn get_depth(&self, run_id: &str) -> Option<u32>;

    /// Record a sub-run relationship.
    async fn record_sub_run(&self, info: SubRunInfo);
}

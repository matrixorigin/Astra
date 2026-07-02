//! Multi-agent coordination: edge registry, cross-pod dispatch, and task
//! lease claiming.
//!
//! Each concern lives in its own submodule to keep the codebase navigable.
//! All public symbols are re-exported here so existing callers continue to
//! work with `astra_services::multi_agent::*`.

pub mod edge_dispatch;
pub mod edge_registry;
pub mod hold_cache;
pub mod lease_renewal;
pub mod metrics;
pub mod task_lease;

// Explicit re-exports — callers can still use `astra_services::multi_agent::*`
// but the module boundaries are now clear.
pub use edge_dispatch::{
    DatabaseEdgeDispatchService, EdgeDispatchRow, EdgeDispatchService,
    UnconfiguredEdgeDispatchService, refresh_edge_dispatch_backlog_metrics,
};
pub use edge_registry::{
    DatabaseEdgeRegistryService, EdgeAgentRecord, EdgeRegistryService,
    UnconfiguredEdgeRegistryService,
};
pub use hold_cache::TaskLeaseHoldCache;
pub use lease_renewal::{LeaseRenewalConfig, LeaseRenewalTask};
pub use metrics::{MetricTarget, MultiAgentMetrics, SharedMultiAgentMetrics, shared_metrics};
pub use task_lease::{
    DEFAULT_TASKS_PACK_LIMIT, DatabaseTaskLeaseService, LeaseClaimResult,
    NextClaimableLeaseClaimResult, TaskLeaseService, TaskLeaseView, TasksPackPushResult,
    UnconfiguredTaskLeaseService, pull_tasks_pack_mysql, pull_tasks_pack_mysql_with_limit,
    push_tasks_pack_held_mysql,
};

//! Multi-agent coordination: edge registry and cross-pod dispatch.
//!
//! Each concern lives in its own submodule to keep the codebase navigable.
//! All public symbols are re-exported here so existing callers continue to
//! work with `astra_services::multi_agent::*`.

pub mod edge_dispatch;
pub mod edge_registry;
pub mod metrics;

// Explicit re-exports — callers can still use `astra_services::multi_agent::*`
// but the module boundaries are now clear.
pub use edge_dispatch::{
    DatabaseEdgeDispatchService, EdgeDirectDispatchAdmission, EdgeDispatchAdmission,
    EdgeDispatchAdmissionError, EdgeDispatchIdentity, EdgeDispatchRow, EdgeDispatchService,
    UnconfiguredEdgeDispatchService, refresh_edge_dispatch_backlog_metrics,
};
pub use edge_registry::{
    DatabaseEdgeRegistryService, EdgeAgentRecord, EdgeRegistrationLease, EdgeRegistryService,
    HeartbeatError, UnconfiguredEdgeRegistryService,
};
pub use metrics::{MetricTarget, MultiAgentMetrics, SharedMultiAgentMetrics, shared_metrics};

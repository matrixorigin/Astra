//! In-memory pool of live edge agent WebSocket connections.
//!
//! Each entry maps `{user_id}:{edge_agent_id}` to a channel sender that can
//! push [`EdgeServerMessage`] frames to the connected edge agent. The pool is
//! stored in [`AppState`] and queried by the tool routing layer to decide
//! whether to route tool calls to a remote edge or fall back to the server.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dashmap::{DashMap, mapref::entry::Entry};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::edge_ws_protocol::{
    EDGE_TOOL_RESULT_GRACE_SECS, EDGE_TOOL_TIMEOUT_SECS, EdgeServerMessage,
    MAX_EDGE_TOOL_TIMEOUT_SECS, RuntimeProcessAuthorizationContext, ToolInvocationIdentity,
};

/// Maximum number of inflight dispatched tool requests tracked for dedup.
/// When exceeded, the oldest entry (by dispatch time) is evicted before inserting.
const MAX_PENDING_REQUESTS: usize = 1000;
/// Maximum inflight dispatched tool requests one user may hold. This prevents a
/// single edge account from exhausting the global pending-request pool.
const MAX_PENDING_REQUESTS_PER_USER: usize = 100;

/// Each dispatched request lives at most this long in the pending set before
/// being purged by `cleanup_stale`. Set to 3× the edge tool timeout as a
/// generous safety margin (normal cleanup happens in execute_tool's
/// success/timeout paths).
const PENDING_REQUEST_TTL_SECS: u64 =
    (MAX_EDGE_TOOL_TIMEOUT_SECS + EDGE_TOOL_RESULT_GRACE_SECS) * 3;

fn edge_result_wait_timeout(execution_timeout_secs: u64) -> Duration {
    Duration::from_secs(execution_timeout_secs.saturating_add(EDGE_TOOL_RESULT_GRACE_SECS))
}

/// Maximum capacity for the channel between the tool router and an edge agent's
/// WebSocket write loop. When full, senders apply backpressure to prevent OOM.
pub const EDGE_WS_CHANNEL_CAPACITY: usize = 256;

/// Sender half that pushes frames into an edge agent's WebSocket write loop.
pub type EdgeWsSender = mpsc::Sender<EdgeServerMessage>;

/// Request-scoped values needed to deliver one already-admitted invocation.
/// Grouping them keeps the transport API explicit without a long positional
/// argument list whose adjacent string fields are easy to swap.
pub struct DurablyAdmittedEdgeInvocation<'a> {
    pub connection_user_id: &'a str,
    pub identity: &'a ToolInvocationIdentity,
    pub edge_agent_id: &'a str,
    pub tool: &'a str,
    pub args: &'a serde_json::Value,
    pub runtime_process_authorization:
        Option<&'a astra_services::runs::RuntimeProcessAuthorizationContext>,
    pub timeout_secs: u64,
    pub cancel_token: Option<&'a CancellationToken>,
}

/// Information about a tool request dispatched to an edge, stored for
/// reconnection deduplication. When an edge reconnects, cloud can check
/// its pending requests against completed request IDs reported by the edge.
#[derive(Debug, Clone, Serialize)]
pub struct DispatchedToolRequest {
    pub request_id: String,
    pub tool_name: String,
    pub args: serde_json::Value,
    /// When the request was dispatched (for stale cleanup).
    /// Not serialized to edge — only for internal use.
    #[serde(skip)]
    pub dispatched_at: Instant,
}

/// Metadata about a connected edge agent.
#[derive(Debug)]
pub struct EdgeConnection {
    /// Monotonic in-process connection incarnation. Cleanup from an older
    /// socket may remove only the exact generation it registered.
    pub generation: u64,
    pub user_id: String,
    pub edge_agent_id: String,
    pub hostname: Option<String>,
    pub workspace_dir: Option<String>,
    pub capabilities: Option<Value>,
    /// Workspace that owns this edge agent, captured at connect time from the
    /// edge registration token's `provider_scope_id`.  Used to authorize
    /// cross-user lookups: only workspace members may dispatch to a shared
    /// edge agent (e.g. a sandbox edge that connected via a service account).
    pub workspace_id: Option<String>,
    pub sender: EdgeWsSender,
    pub connected_at: std::time::Instant,
    /// Pending tool call responses: request_id → oneshot sender.
    pending_results: Arc<DashMap<String, PendingEdgeResult>>,
}

#[derive(Debug)]
struct PendingEdgeResult {
    delivery_generation: u64,
    sender: oneshot::Sender<EdgeToolResult>,
}

/// In-progress transactional reconnect for one edge key.
///
/// Created by [`EdgeConnectionPool::begin_reconnect`] BEFORE the reconnect's
/// async DB registration; the previous connection is left active and selectable
/// throughout. On success [`EdgeConnectionPool::commit_reconnect`] atomically
/// swaps in the new connection (inheriting the pending-result map); on failure
/// [`EdgeConnectionPool::abort_reconnect`] leaves the previous connection
/// untouched. Either way the reservation's `Drop` clears the intent marker as a
/// backstop against an early return or panic.
#[must_use = "a reconnect reservation must be committed or aborted"]
pub struct ReconnectReservation {
    key: String,
    /// Pending-result map captured when the reservation was opened (if a
    /// connection existed), held alive across the DB await so a concurrent
    /// cleanup of the previous connection cannot drop its waiters.
    pending: Option<Arc<DashMap<String, PendingEdgeResult>>>,
    connections: Arc<DashMap<String, EdgeConnection>>,
    intents: Arc<DashMap<String, ()>>,
}

impl Drop for ReconnectReservation {
    fn drop(&mut self) {
        // Resolve the reservation and clear its intent under the same
        // connection-shard lock used by registration and cleanup. If another
        // registration won the race but did not inherit this reservation's
        // pending map, those waiters have no delivery owner and must fail now.
        match self.connections.entry(self.key.clone()) {
            Entry::Occupied(connection) => {
                let inherited = self
                    .pending
                    .as_ref()
                    .is_some_and(|pending| Arc::ptr_eq(pending, &connection.get().pending_results));
                if !inherited && let Some(pending) = &self.pending {
                    pending.clear();
                }
                self.intents.remove(&self.key);
            }
            Entry::Vacant(_) => {
                if let Some(pending) = &self.pending {
                    pending.clear();
                }
                self.intents.remove(&self.key);
            }
        }
    }
}

/// Result from an edge tool execution.
#[derive(Debug, Clone)]
pub struct EdgeToolResult {
    pub output: String,
    pub is_error: bool,
    pub duration_ms: Option<u64>,
    pub tool_result_fields: Option<serde_json::Map<String, serde_json::Value>>,
}

/// Pool key: `{user_id}:{edge_agent_id}`.
fn pool_key(user_id: &str, edge_agent_id: &str) -> String {
    format!("{user_id}:{edge_agent_id}")
}

/// Thread-safe pool of live edge WebSocket connections.
#[derive(Debug, Clone)]
pub struct EdgeConnectionPool {
    connections: Arc<DashMap<String, EdgeConnection>>,
    /// Pending tool requests dispatched to edges, keyed by request_id.
    /// Used for reconnection dedup: when an edge reconnects, cloud can
    /// check its pending requests against completed IDs reported by the edge.
    pending_requests: Arc<DashMap<String, PendingRequestEntry>>,
    /// Per-user request index for fast reconnection lookup without scanning
    /// every in-flight request in the process.
    pending_request_ids_by_user: Arc<DashMap<String, VecDeque<String>>>,
    /// Global FIFO of dispatched request IDs for O(1)-amortized eviction when
    /// the pending set reaches capacity. Stale IDs are lazily skipped.
    pending_request_order: Arc<Mutex<VecDeque<String>>>,
    /// Maximum number of inflight dispatched tool requests. When exceeded,
    /// the oldest entry is evicted before insertion.
    max_pending: usize,
    /// Maximum number of inflight dispatched tool requests per user.
    max_pending_per_user: usize,
    next_connection_generation: Arc<AtomicU64>,
    next_delivery_generation: Arc<AtomicU64>,
    /// Keys with a reconnect in progress. While a key is present, a concurrent
    /// cleanup of the previous connection must NOT clear its pending-result map
    /// (the reconnect will inherit it on commit, or fail it on abort). Enables
    /// the transactional reconnect: the previous connection stays active and
    /// selectable throughout the reconnect's async DB registration.
    reconnect_intents: Arc<DashMap<String, ()>>,
    /// Per-key async lock serializing concurrent reconnects for the same edge
    /// key across the whole begin→DB→commit/abort region, so two handlers cannot
    /// interleave their DB registration and pool commit (which could leave the
    /// DB pointing at one incarnation and the pool at another).
    reconnect_locks: Arc<DashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

#[derive(Debug, Clone)]
struct PendingRequestEntry {
    user_id: String,
    request: DispatchedToolRequest,
}

impl EdgeConnectionPool {
    pub fn new() -> Self {
        Self {
            connections: Arc::new(DashMap::new()),
            pending_requests: Arc::new(DashMap::new()),
            pending_request_ids_by_user: Arc::new(DashMap::new()),
            pending_request_order: Arc::new(Mutex::new(VecDeque::new())),
            max_pending: MAX_PENDING_REQUESTS,
            max_pending_per_user: MAX_PENDING_REQUESTS_PER_USER,
            next_connection_generation: Arc::new(AtomicU64::new(0)),
            next_delivery_generation: Arc::new(AtomicU64::new(0)),
            reconnect_intents: Arc::new(DashMap::new()),
            reconnect_locks: Arc::new(DashMap::new()),
        }
    }

    /// Acquire the per-key reconnect lock. The edge WS handler holds this across
    /// its entire reconnect (begin → DB registration → commit/abort) so
    /// concurrent reconnects for the same key are serialized.
    pub fn reconnect_lock(
        &self,
        user_id: &str,
        edge_agent_id: &str,
    ) -> Arc<tokio::sync::Mutex<()>> {
        self.reconnect_locks
            .entry(pool_key(user_id, edge_agent_id))
            .or_default()
            .clone()
    }

    /// Drop the per-key reconnect lock once no reconnect is using it, bounding
    /// the lock map's growth. Called by the handler after its reconnect finishes
    /// (with its own `Arc` clone still alive). Removes the entry only when the
    /// sole strong references are the map's and the caller's — a concurrent
    /// waiter holds its own clone, raising the count above 2, so it is never
    /// removed out from under an in-progress reconnect. The `remove_if` predicate
    /// runs under the shard lock, making this atomic with a racing
    /// `reconnect_lock`.
    pub fn gc_reconnect_lock(&self, user_id: &str, edge_agent_id: &str) {
        self.reconnect_locks
            .remove_if(&pool_key(user_id, edge_agent_id), |_, lock| {
                Arc::strong_count(lock) <= 2
            });
    }

    /// Register a new edge connection. Replaces any existing connection for the same key.
    /// Returns the generation ID assigned to this connection.
    pub fn register(
        &self,
        user_id: &str,
        edge_agent_id: &str,
        hostname: Option<String>,
        workspace_dir: Option<String>,
        sender: EdgeWsSender,
    ) -> u64 {
        self.register_with_capabilities(
            user_id,
            edge_agent_id,
            hostname,
            workspace_dir,
            None,
            None,
            sender,
        )
    }

    /// Register a new edge connection with its structured runtime capability advertisement.
    /// Replaces any existing connection for the same key.
    /// Returns the generation ID assigned to this connection for use with
    /// `unregister_generation` during cleanup.
    ///
    /// `workspace_id` is the owning workspace, captured from the edge registration
    /// token's `provider_scope_id`.  Pass `None` for internal (non-moi) tokens.
    #[allow(clippy::too_many_arguments)]
    pub fn register_with_capabilities(
        &self,
        user_id: &str,
        edge_agent_id: &str,
        hostname: Option<String>,
        workspace_dir: Option<String>,
        capabilities: Option<Value>,
        workspace_id: Option<String>,
        sender: EdgeWsSender,
    ) -> u64 {
        let key = pool_key(user_id, edge_agent_id);
        let generation = self.next_generation();
        let mut connection = self.new_connection(
            generation,
            user_id,
            edge_agent_id,
            hostname,
            workspace_dir,
            capabilities,
            workspace_id,
            sender,
        );
        match self.connections.entry(key) {
            Entry::Occupied(mut entry) => {
                // A reconnect changes delivery ownership, not invocation
                // identity. Preserve exact pending generations so a replayed
                // durable result can still release its original waiter.
                connection.pending_results = entry.get().pending_results.clone();
                entry.insert(connection);
            }
            Entry::Vacant(entry) => {
                entry.insert(connection);
            }
        }
        generation
    }

    fn next_generation(&self) -> u64 {
        self.next_connection_generation
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1)
    }

    #[allow(clippy::too_many_arguments)]
    fn new_connection(
        &self,
        generation: u64,
        user_id: &str,
        edge_agent_id: &str,
        hostname: Option<String>,
        workspace_dir: Option<String>,
        capabilities: Option<Value>,
        workspace_id: Option<String>,
        sender: EdgeWsSender,
    ) -> EdgeConnection {
        EdgeConnection {
            generation,
            user_id: user_id.to_string(),
            edge_agent_id: edge_agent_id.to_string(),
            hostname,
            workspace_dir,
            capabilities,
            workspace_id,
            sender,
            connected_at: std::time::Instant::now(),
            pending_results: Arc::new(DashMap::new()),
        }
    }

    /// Open a transactional reconnect for `(user_id, edge_agent_id)`.
    ///
    /// This does NOT touch the pool: any existing connection stays active and
    /// selectable while the caller performs its async DB registration. The
    /// returned reservation marks the key so a concurrent cleanup of the
    /// previous connection preserves (rather than clears) its pending-result
    /// map, and captures that map so it survives even if the previous entry is
    /// removed mid-reconnect. Finish with [`Self::commit_reconnect`] on success
    /// or [`Self::abort_reconnect`] on failure.
    pub fn begin_reconnect(&self, user_id: &str, edge_agent_id: &str) -> ReconnectReservation {
        let key = pool_key(user_id, edge_agent_id);
        // Set the intent and capture the pending map atomically w.r.t. cleanup:
        // hold the connections shard lock (via `entry`) across BOTH operations.
        // `unregister_generation` reads the intent under the same lock, so a
        // cleanup can never interleave between "set intent" and "capture map" —
        // which would otherwise skip the clear yet leave the reservation with no
        // map, orphaning the in-flight waiters.
        let pending = match self.connections.entry(key.clone()) {
            Entry::Occupied(occupied) => {
                let pending = occupied.get().pending_results.clone();
                self.reconnect_intents.insert(key.clone(), ());
                Some(pending)
            }
            Entry::Vacant(_) => {
                self.reconnect_intents.insert(key.clone(), ());
                None
            }
        };
        ReconnectReservation {
            key,
            pending,
            connections: self.connections.clone(),
            intents: self.reconnect_intents.clone(),
        }
    }

    /// Commit a reconnect: atomically install the new connection, inheriting the
    /// pending-result map from the still-present previous connection or, if it
    /// was cleaned up during the reconnect, from the reservation. Returns the new
    /// generation.
    #[allow(clippy::too_many_arguments)]
    pub fn commit_reconnect(
        &self,
        reservation: ReconnectReservation,
        user_id: &str,
        edge_agent_id: &str,
        hostname: Option<String>,
        workspace_dir: Option<String>,
        capabilities: Option<Value>,
        workspace_id: Option<String>,
        sender: EdgeWsSender,
    ) -> u64 {
        let key = pool_key(user_id, edge_agent_id);
        let generation = self.next_generation();
        let mut connection = self.new_connection(
            generation,
            user_id,
            edge_agent_id,
            hostname,
            workspace_dir,
            capabilities,
            workspace_id,
            sender,
        );
        // Inherit the pending map: prefer a live previous connection's map, else
        // the one captured by the reservation (previous was cleaned up mid-
        // reconnect but the intent kept its waiters alive). Take a clone and drop
        // the read guard before insert() to avoid a same-shard lock conflict.
        let inherited = self
            .connections
            .get(&key)
            .map(|previous| previous.pending_results.clone())
            .or_else(|| reservation.pending.clone());
        if let Some(pending) = inherited {
            connection.pending_results = pending;
        }
        self.connections.insert(key, connection);
        // `reservation` drops here, clearing the intent.
        generation
    }

    /// Abort a reconnect: leave any still-active previous connection untouched.
    /// If the previous connection was cleaned up during the reconnect window,
    /// its waiters were preserved by the intent but will now never be delivered
    /// (the reconnect failed), so fail them here.
    pub fn abort_reconnect(
        &self,
        reservation: ReconnectReservation,
        user_id: &str,
        edge_agent_id: &str,
    ) {
        debug_assert_eq!(reservation.key, pool_key(user_id, edge_agent_id));
        // `ReconnectReservation::drop` performs the atomic abort and clears the
        // intent. Keeping the failure behavior on the reservation also covers
        // early returns and panics that bypass this explicit method.
        drop(reservation);
    }

    /// Remove only the connection incarnation registered by the caller.
    /// Returns false when a newer socket already replaced it.
    pub fn unregister_generation(
        &self,
        user_id: &str,
        edge_agent_id: &str,
        generation: u64,
    ) -> bool {
        let key = pool_key(user_id, edge_agent_id);
        // Hold the connections shard lock (via `entry`) while reading the intent
        // so this is atomic with `begin_reconnect` (which sets the intent under
        // the same lock). Preserve the pending map while a reconnect is in
        // progress — the reconnect will inherit it on commit, or fail it on
        // abort. Clearing here would orphan tool calls admitted before it.
        match self.connections.entry(key.clone()) {
            Entry::Occupied(occupied) if occupied.get().generation == generation => {
                let reconnecting = self.reconnect_intents.contains_key(&key);
                let (_, connection) = occupied.remove_entry();
                if !reconnecting {
                    connection.pending_results.clear();
                }
                true
            }
            _ => false,
        }
    }

    /// Check if a user has any connected edge agent.
    pub fn has_connected_edge(&self, user_id: &str) -> bool {
        self.connections
            .iter()
            .any(|entry| entry.value().user_id == user_id && !entry.value().sender.is_closed())
    }

    /// Find a connected edge agent by its agent ID across all users.
    ///
    /// Returns the owner user_id and connection info, or `None` if the agent
    /// is not connected or the workspace does not match.
    ///
    /// `workspace_id` is the requesting workspace.  Workspace isolation is
    /// fail-closed: a workspace-bound edge (edge.workspace_id is Some) is only
    /// reachable from a request that carries the matching workspace_id.  A
    /// request without workspace context can only reach edges that are also
    /// unscoped (edge.workspace_id is None).  This prevents a request that
    /// lacks workspace context (e.g. workspace_record: None on a
    /// provider-authorized turn) from silently resolving a workspace-bound
    /// sandbox edge.
    pub fn find_edge_by_agent_id(
        &self,
        edge_agent_id: &str,
        workspace_id: Option<&str>,
    ) -> Option<(String, EdgeConnectionInfo)> {
        self.connections
            .iter()
            .find(|entry| {
                let conn = entry.value();
                if conn.edge_agent_id != edge_agent_id || conn.sender.is_closed() {
                    return false;
                }
                // Fail-closed workspace isolation:
                //   request has workspace_id  → edge must have the same workspace_id
                //   request has no workspace_id → edge must also be unscoped (None)
                match (workspace_id, conn.workspace_id.as_deref()) {
                    (Some(req_ws), Some(edge_ws)) => req_ws == edge_ws,
                    (None, None) => true,
                    _ => false,
                }
            })
            .map(|entry| {
                let conn = entry.value();
                let info = EdgeConnectionInfo {
                    edge_agent_id: conn.edge_agent_id.clone(),
                    hostname: conn.hostname.clone(),
                    workspace_dir: conn.workspace_dir.clone(),
                    capabilities: conn.capabilities.clone(),
                    connected_at: conn.connected_at,
                    workspace_id: conn.workspace_id.clone(),
                };
                (conn.user_id.clone(), info)
            })
    }

    /// Get all connected edge agents for a user, regardless of workspace.
    /// Use this for status/display queries; use [`get_user_edges`] for dispatch
    /// to enforce workspace isolation.
    pub fn get_all_user_edges(&self, user_id: &str) -> Vec<EdgeConnectionInfo> {
        self.connections
            .iter()
            .filter(|entry| entry.value().user_id == user_id && !entry.value().sender.is_closed())
            .map(|entry| {
                let conn = entry.value();
                EdgeConnectionInfo {
                    edge_agent_id: conn.edge_agent_id.clone(),
                    hostname: conn.hostname.clone(),
                    workspace_dir: conn.workspace_dir.clone(),
                    capabilities: conn.capabilities.clone(),
                    connected_at: conn.connected_at,
                    workspace_id: conn.workspace_id.clone(),
                }
            })
            .collect()
    }

    /// Get connected edge agents for a user, filtered by workspace.
    ///
    /// Workspace isolation is fail-closed (same semantics as
    /// [`find_edge_by_agent_id`]):
    /// - `workspace_id = Some(ws)` → only edges whose `workspace_id == Some(ws)`
    /// - `workspace_id = None`     → only edges whose `workspace_id` is `None`
    pub fn get_user_edges(
        &self,
        user_id: &str,
        workspace_id: Option<&str>,
    ) -> Vec<EdgeConnectionInfo> {
        self.connections
            .iter()
            .filter(|entry| {
                let conn = entry.value();
                if conn.user_id != user_id || conn.sender.is_closed() {
                    return false;
                }
                match (workspace_id, conn.workspace_id.as_deref()) {
                    (Some(req_ws), Some(edge_ws)) => req_ws == edge_ws,
                    (None, None) => true,
                    _ => false,
                }
            })
            .map(|entry| {
                let conn = entry.value();
                EdgeConnectionInfo {
                    edge_agent_id: conn.edge_agent_id.clone(),
                    hostname: conn.hostname.clone(),
                    workspace_dir: conn.workspace_dir.clone(),
                    capabilities: conn.capabilities.clone(),
                    connected_at: conn.connected_at,
                    workspace_id: conn.workspace_id.clone(),
                }
            })
            .collect()
    }

    /// Deliver an invocation whose exact identity, edge owner, and payload
    /// were already admitted by the durable dispatch authority.
    pub async fn execute_durably_admitted_invocation_with_cancel(
        &self,
        identity: &ToolInvocationIdentity,
        edge_agent_id: &str,
        tool: &str,
        args: &serde_json::Value,
        cancel_token: Option<&CancellationToken>,
    ) -> Option<EdgeToolResult> {
        self.execute_durably_admitted_invocation_on_connection_with_cancel(
            DurablyAdmittedEdgeInvocation {
                connection_user_id: &identity.user_id,
                identity,
                edge_agent_id,
                tool,
                args,
                runtime_process_authorization: None,
                timeout_secs: EDGE_TOOL_TIMEOUT_SECS,
                cancel_token,
            },
        )
        .await
    }

    /// Execute a logical invocation through a connection owned by a distinct
    /// authenticated principal.
    ///
    /// Sandbox edges may connect with a workspace-scoped service account while
    /// the invocation identity remains owned by the end user. The connection
    /// owner selects the socket; the durable identity remains unchanged in the
    /// request and result protocol.
    pub async fn execute_durably_admitted_invocation_on_connection_with_cancel(
        &self,
        invocation: DurablyAdmittedEdgeInvocation<'_>,
    ) -> Option<EdgeToolResult> {
        let DurablyAdmittedEdgeInvocation {
            connection_user_id,
            identity,
            edge_agent_id,
            tool,
            args,
            runtime_process_authorization,
            timeout_secs,
            cancel_token,
        } = invocation;
        self.execute_durably_admitted_invocation_on_connection_with_authorization_timeout_and_cancel(
            connection_user_id,
            identity,
            edge_agent_id,
            tool,
            args,
            runtime_process_authorization,
            timeout_secs,
            cancel_token,
        )
        .await
    }

    /// As above, but preserves the execution deadline selected by the runtime
    /// policy.  Database admission is complete before this method starts its
    /// socket wait; the deadline therefore governs only the edge invocation,
    /// never a checked-out database connection or transaction.
    pub async fn execute_durably_admitted_invocation_on_connection_with_timeout_and_cancel(
        &self,
        connection_user_id: &str,
        identity: &ToolInvocationIdentity,
        edge_agent_id: &str,
        tool: &str,
        args: &serde_json::Value,
        timeout_secs: u64,
        cancel_token: Option<&CancellationToken>,
    ) -> Option<EdgeToolResult> {
        self.execute_durably_admitted_invocation_on_connection_with_authorization_timeout_and_cancel(
            connection_user_id,
            identity,
            edge_agent_id,
            tool,
            args,
            None,
            timeout_secs,
            cancel_token,
        )
        .await
    }

    async fn execute_durably_admitted_invocation_on_connection_with_authorization_timeout_and_cancel(
        &self,
        connection_user_id: &str,
        identity: &ToolInvocationIdentity,
        edge_agent_id: &str,
        tool: &str,
        args: &serde_json::Value,
        runtime_process_authorization: Option<
            &astra_services::runs::RuntimeProcessAuthorizationContext,
        >,
        timeout_secs: u64,
        cancel_token: Option<&CancellationToken>,
    ) -> Option<EdgeToolResult> {
        if cancel_token.is_some_and(CancellationToken::is_cancelled) {
            return None;
        }
        // Public callers include server integrations outside the runtime plan.
        // Clamp defensively here too so a malformed policy can neither create
        // a zero-duration invocation nor outlive the callback custody window.
        let timeout_secs = timeout_secs.clamp(1, MAX_EDGE_TOOL_TIMEOUT_SECS);
        let key = pool_key(connection_user_id, edge_agent_id);
        let (pending_results, sender) = {
            let Some(entry) = self.connections.get(&key) else {
                tracing::warn!(
                    target: "astra_runtime::edge_dispatch_diag",
                    key = %key,
                    "edge_dispatch: execute_tool_with_cancel no pool entry for key"
                );
                return None;
            };
            let conn = entry.value();
            if conn.sender.is_closed() {
                tracing::warn!(
                    target: "astra_runtime::edge_dispatch_diag",
                    key = %key,
                    "edge_dispatch: execute_tool_with_cancel pool entry sender already closed"
                );
                drop(entry);
                self.connections.remove(&key);
                return None;
            }
            (conn.pending_results.clone(), conn.sender.clone())
        };

        let request_id = identity.storage_key();
        let delivery_generation = self
            .next_delivery_generation
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        let (tx, rx) = oneshot::channel();
        pending_results.insert(
            request_id.clone(),
            PendingEdgeResult {
                delivery_generation,
                sender: tx,
            },
        );

        let msg = EdgeServerMessage::ToolRequest {
            request_id: request_id.clone(),
            identity: Box::new(identity.clone()),
            delivery_generation,
            tool: tool.to_string(),
            args: args.clone(),
            runtime_process_authorization: runtime_process_authorization.map(|context| {
                Box::new(RuntimeProcessAuthorizationContext {
                    authorization: context.authorization.clone(),
                })
            }),
            runtime_process_authorization_required: runtime_process_authorization.is_some(),
            timeout_secs,
        };

        // Store in dispatched set for reconnection dedup
        let dispatched = DispatchedToolRequest {
            request_id: request_id.clone(),
            tool_name: tool.to_string(),
            args: args.clone(),
            dispatched_at: Instant::now(),
        };
        self.insert_pending_request(&identity.user_id, &request_id, dispatched);

        if let Err(e) = sender.send(msg).await {
            tracing::warn!(
                target: "astra_runtime::edge_dispatch_diag",
                key = %key,
                request_id = %request_id,
                error = %e,
                "edge_dispatch: execute_tool_with_cancel channel send failed"
            );
            pending_results.remove(&request_id);
            self.remove_pending_request(&request_id);
            return None;
        }
        tracing::info!(
            target: "astra_runtime::edge_dispatch_diag",
            key = %key,
            request_id = %request_id,
            tool = %tool,
            "edge_dispatch: execute_tool_with_cancel queued tool request into edge channel, awaiting result"
        );

        // Helper: notify the edge that it should abort the in-flight request
        // and drop any partial result. Best-effort: if the send fails (edge
        // already gone), we still proceed with local cleanup.
        let cancel_edge = || {
            let _ = sender.try_send(EdgeServerMessage::ToolCancel {
                request_id: request_id.clone(),
                delivery_generation,
            });
        };

        // The edge enforces `timeout_secs` while executing.  Keep the server
        // waiter alive briefly longer for the durable result callback, so a
        // tool that ends at its deadline cannot lose its receiver in the same
        // instant it reports the timeout/completion result.
        let timeout_dur = edge_result_wait_timeout(timeout_secs);
        let result = if let Some(token) = cancel_token {
            tokio::select! {
                _ = token.cancelled() => {
                    cancel_edge();
                    pending_results.remove(&request_id);
                    self.remove_pending_request(&request_id);
                    return None;
                }
                result = tokio::time::timeout(timeout_dur, rx) => result,
            }
        } else {
            tokio::time::timeout(timeout_dur, rx).await
        };
        match result {
            Ok(Ok(result)) => {
                self.remove_pending_request(&request_id);
                Some(result)
            }
            _ => {
                // Timed out waiting for the edge. Tell it to abort so the
                // (possibly still running) tool does not write to a file or
                // run a destructive command after the caller has given up.
                tracing::warn!(
                    target: "astra_runtime::edge_dispatch_diag",
                    key = %key,
                    request_id = %request_id,
                    timeout_secs,
                    "edge_dispatch: execute_tool_with_cancel timed out waiting for edge tool result"
                );
                cancel_edge();
                pending_results.remove(&request_id);
                self.remove_pending_request(&request_id);
                None
            }
        }
    }

    /// Deliver a tool result from an edge agent (called from the edge WS read loop).
    pub fn deliver_tool_result(
        &self,
        user_id: &str,
        edge_agent_id: &str,
        request_id: &str,
        delivery_generation: u64,
        result: EdgeToolResult,
    ) -> bool {
        let key = pool_key(user_id, edge_agent_id);
        if let Some(entry) = self.connections.get(&key)
            && let Some(pending) = entry.value().pending_results.get(request_id)
        {
            if pending.delivery_generation != delivery_generation {
                tracing::warn!(
                    user_id = %user_id,
                    edge_agent_id = %edge_agent_id,
                    request_id = %request_id,
                    expected_generation = pending.delivery_generation,
                    delivery_generation,
                    "edge tool result generation did not match pending delivery"
                );
                return false;
            }
            drop(pending);
            if let Some((_, pending)) = entry.value().pending_results.remove(request_id) {
                return pending.sender.send(result).is_ok();
            }
        }
        // No pending entry: the request was already cancelled, timed out, or the
        // connection was reset. Log so operators can correlate late edge results
        // (e.g. a bash command completing after the caller already gave up and
        // issued a conflicting write) instead of silently dropping them.
        tracing::warn!(
            user_id = %user_id,
            edge_agent_id = %edge_agent_id,
            request_id = %request_id,
            is_error = result.is_error,
            "edge tool result delivered with no pending receiver; dropping"
        );
        false
    }

    /// Remove stale connections (sender closed) and expired pending requests.
    pub fn cleanup_stale(&self) {
        self.connections.retain(|_, conn| !conn.sender.is_closed());

        let deadline = Instant::now() - Duration::from_secs(PENDING_REQUEST_TTL_SECS);
        let stale_ids: Vec<String> = self
            .pending_requests
            .iter()
            .filter(|entry| entry.value().request.dispatched_at <= deadline)
            .map(|entry| entry.key().clone())
            .collect();
        for request_id in stale_ids {
            self.remove_pending_request(&request_id);
        }
    }

    /// Insert a dispatched request into the pending set, enforcing the capacity
    /// limit by evicting the oldest entry if necessary.
    ///
    /// Holds `pending_request_order` across the global capacity check AND all
    /// three structural inserts so that concurrent inserters are serialized:
    /// they cannot both observe `len < max_pending` and then both push, which
    /// would overshoot the cap. It also keeps the order deque consistent with
    /// `pending_requests` under panic (the push_back happens last, under the
    /// same critical section that decided capacity).
    fn insert_pending_request(&self, user_id: &str, request_id: &str, req: DispatchedToolRequest) {
        // Per-user cap: evict first, OUTSIDE the order lock —
        // `evict_oldest_pending_for_user` itself locks `pending_request_order`,
        // so holding it here would re-enter and deadlock.
        while self.pending_count_for_user(user_id) >= self.max_pending_per_user.max(1) {
            if !self.evict_oldest_pending_for_user(user_id) {
                break;
            }
        }

        // Critical section: serialize all concurrent inserters w.r.t. the
        // global capacity invariant and the three-structure consistency.
        let mut order = self
            .pending_request_order
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        // Global capacity eviction under the lock. `pending_request_order` may
        // contain stale ids (entries already removed via remove_pending_request,
        // which does not pop the order deque); pop_front until we evict a live
        // entry or the deque empties.
        while self.pending_requests.len() >= self.max_pending {
            let Some(oldest_request_id) = order.pop_front() else {
                break;
            };
            if self.remove_pending_request(&oldest_request_id).is_some() {
                break;
            }
            // Stale id already gone from pending_requests; keep evicting.
        }

        self.pending_requests.insert(
            request_id.to_string(),
            PendingRequestEntry {
                user_id: user_id.to_string(),
                request: req,
            },
        );
        self.pending_request_ids_by_user
            .entry(user_id.to_string())
            .or_default()
            .push_back(request_id.to_string());
        // Order deque push is last, while still holding the lock that
        // authorized the insert — no window for another inserter to sneak in.
        order.push_back(request_id.to_string());
    }

    fn pending_count_for_user(&self, user_id: &str) -> usize {
        self.pending_request_ids_by_user
            .get(user_id)
            .map(|entry| {
                entry
                    .iter()
                    .filter(|request_id| self.pending_requests.contains_key(request_id.as_str()))
                    .count()
            })
            .unwrap_or(0)
    }

    fn evict_oldest_pending_for_user(&self, user_id: &str) -> bool {
        let request_id = self
            .pending_request_order
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|request_id| {
                self.pending_requests
                    .get(request_id.as_str())
                    .map(|entry| entry.user_id == user_id)
                    .unwrap_or(false)
            })
            .cloned();
        let Some(request_id) = request_id else {
            return false;
        };
        self.remove_pending_request(&request_id).is_some()
    }

    fn remove_pending_request(&self, request_id: &str) -> Option<PendingRequestEntry> {
        let removed = self
            .pending_requests
            .remove(request_id)
            .map(|(_, entry)| entry)?;
        self.remove_user_pending_index(&removed.user_id, request_id);
        Some(removed)
    }

    fn remove_user_pending_index(&self, user_id: &str, request_id: &str) {
        let mut remove_user_bucket = false;
        if let Some(mut entry) = self.pending_request_ids_by_user.get_mut(user_id) {
            entry.retain(|id| id != request_id);
            remove_user_bucket = entry.is_empty();
        }
        if remove_user_bucket {
            self.pending_request_ids_by_user.remove(user_id);
        }
    }

    /// Number of active connections.
    pub fn connection_count(&self) -> usize {
        self.connections.len()
    }

    /// Gracefully shut down all edge connections: send `Closing` to each,
    /// wait a short grace period for clean disconnect, then clear the pool.
    /// Already-closed senders are silently skipped.
    ///
    /// Returns the number of connections that were still alive before drain.
    pub async fn drain(&self) -> usize {
        use crate::edge_ws_protocol::EdgeServerMessage;

        let closing = EdgeServerMessage::Closing {
            reason: "server is shutting down".into(),
        };

        // Per-sender drain: send Closing, then unregister.
        // DashMap retain takes &K, &mut V — we copy keys inline so we can call
        // unregister (which requires a shared reference to self).
        let connections: Vec<(String, String, u64)> = self
            .connections
            .iter()
            .filter(|entry| !entry.value().sender.is_closed())
            .map(|entry| {
                (
                    entry.value().user_id.clone(),
                    entry.value().edge_agent_id.clone(),
                    entry.value().generation,
                )
            })
            .collect();
        let count = connections.len();

        for (user_id, edge_agent_id, _) in &connections {
            if let Some(entry) = self.connections.get(&pool_key(user_id, edge_agent_id)) {
                let _ = entry.sender.try_send(closing.clone());
            }
        }

        // Generation fencing prevents a replacement that raced the drain
        // snapshot from being removed.
        for (user_id, edge_agent_id, generation) in &connections {
            self.unregister_generation(user_id, edge_agent_id, *generation);
        }

        count
    }

    /// Get pending tool requests for a user. Only returns requests that are
    /// still in the pending set — completed requests should already have been
    /// removed via [`ack_completed_for_user`] before calling this method.
    pub fn get_pending_requests_for_user(&self, user_id: &str) -> Vec<DispatchedToolRequest> {
        let Some(index) = self.pending_request_ids_by_user.get(user_id) else {
            return Vec::new();
        };
        index
            .iter()
            .filter_map(|request_id| {
                self.pending_requests
                    .get(request_id)
                    .map(|entry| entry.value().request.clone())
            })
            .collect()
    }

    /// Remove dispatched requests that the edge has confirmed as completed.
    /// Called when the cloud heartbeat handler receives `last_seen_request_ids`
    /// from the edge, confirming those tools have been executed.
    pub fn ack_completed_for_user(&self, user_id: &str, completed_request_ids: &[String]) {
        for id in completed_request_ids {
            let matches_user = self
                .pending_requests
                .get(id)
                .map(|entry| entry.value().user_id == user_id)
                .unwrap_or(false);
            if matches_user {
                self.remove_pending_request(id);
            }
        }
    }
}

impl Default for EdgeConnectionPool {
    fn default() -> Self {
        Self::new()
    }
}

/// Public info about a connected edge (no sender exposed).
#[derive(Debug, Clone)]
pub struct EdgeConnectionInfo {
    pub edge_agent_id: String,
    pub hostname: Option<String>,
    pub workspace_dir: Option<String>,
    pub capabilities: Option<Value>,
    pub connected_at: std::time::Instant,
    /// Owning workspace, captured from the edge registration token at connect
    /// time. Used to enforce workspace isolation on the same-user hot path.
    pub workspace_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn admitted_identity(call_id: &str) -> ToolInvocationIdentity {
        ToolInvocationIdentity::new("user-1", "session", "run", "turn", call_id).unwrap()
    }

    #[test]
    fn register_and_check_connected() {
        let pool = EdgeConnectionPool::new();
        assert!(!pool.has_connected_edge("user-1"));

        let (tx, _rx) = mpsc::channel(1);
        pool.register("user-1", "edge-a", Some("laptop".into()), None, tx);

        assert!(pool.has_connected_edge("user-1"));
        assert!(!pool.has_connected_edge("user-2"));
        assert_eq!(pool.connection_count(), 1);
    }

    #[test]
    fn unregister_generation_removes_only_the_registered_incarnation() {
        let pool = EdgeConnectionPool::new();
        let (old_tx, _old_rx) = mpsc::channel(1);
        let old_generation = pool.register("user-1", "edge-a", None, None, old_tx);
        let (new_tx, _new_rx) = mpsc::channel(1);
        let new_generation = pool.register("user-1", "edge-a", None, None, new_tx);
        assert!(pool.has_connected_edge("user-1"));

        assert!(!pool.unregister_generation("user-1", "edge-a", old_generation));
        assert!(pool.has_connected_edge("user-1"));
        assert!(pool.unregister_generation("user-1", "edge-a", new_generation));
        assert!(!pool.has_connected_edge("user-1"));
        assert_eq!(pool.connection_count(), 0);
    }

    #[tokio::test]
    async fn replacement_connection_preserves_exact_pending_result_waiters() {
        let pool = EdgeConnectionPool::new();
        let (old_tx, mut old_rx) = mpsc::channel(1);
        let old_generation = pool.register("user-1", "edge-a", None, None, old_tx);
        let identity = admitted_identity("replacement-result");
        let caller_pool = pool.clone();
        let caller = tokio::spawn(async move {
            caller_pool
                .execute_durably_admitted_invocation_with_cancel(
                    &identity,
                    "edge-a",
                    "bash",
                    &json!({"command":"effect"}),
                    None,
                )
                .await
        });
        let request = old_rx.recv().await.expect("old socket request");
        let (request_id, delivery_generation) = match request {
            EdgeServerMessage::ToolRequest {
                request_id,
                delivery_generation,
                ..
            } => (request_id, delivery_generation),
            other => panic!("expected tool request, got {other:?}"),
        };

        let (replacement_tx, _replacement_rx) = mpsc::channel(1);
        pool.register("user-1", "edge-a", None, None, replacement_tx);
        assert!(!pool.unregister_generation("user-1", "edge-a", old_generation));
        assert!(pool.deliver_tool_result(
            "user-1",
            "edge-a",
            &request_id,
            delivery_generation,
            EdgeToolResult {
                output: "durable-replay".to_string(),
                is_error: false,
                duration_ms: Some(1),
                tool_result_fields: None,
            }
        ));
        assert_eq!(
            caller.await.unwrap().unwrap().output,
            "durable-replay",
            "connection replacement must not strand the admitted caller"
        );
    }

    /// Admit a durable invocation on `edge-a` and return its (request_id,
    /// delivery_generation) once the ToolRequest reaches the socket.
    async fn admit_on_edge_a(
        pool: &EdgeConnectionPool,
        label: &str,
        socket_rx: &mut mpsc::Receiver<EdgeServerMessage>,
    ) -> (tokio::task::JoinHandle<Option<EdgeToolResult>>, String, u64) {
        let identity = admitted_identity(label);
        let caller_pool = pool.clone();
        let caller = tokio::spawn(async move {
            caller_pool
                .execute_durably_admitted_invocation_with_cancel(
                    &identity,
                    "edge-a",
                    "bash",
                    &json!({ "command": "effect" }),
                    None,
                )
                .await
        });
        let request = socket_rx.recv().await.expect("socket request");
        match request {
            EdgeServerMessage::ToolRequest {
                request_id,
                delivery_generation,
                ..
            } => (caller, request_id, delivery_generation),
            other => panic!("expected tool request, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn explicit_execution_timeout_reaches_edge_and_is_bounded() {
        let pool = EdgeConnectionPool::new();
        let (tx, mut rx) = mpsc::channel(1);
        pool.register("user-1", "edge-a", None, None, tx);
        let identity = admitted_identity("policy-deadline");
        let caller_pool = pool.clone();
        let caller = tokio::spawn(async move {
            caller_pool
                .execute_durably_admitted_invocation_on_connection_with_timeout_and_cancel(
                    "user-1",
                    &identity,
                    "edge-a",
                    "bash",
                    &json!({ "command": "effect" }),
                    MAX_EDGE_TOOL_TIMEOUT_SECS + 1,
                    None,
                )
                .await
        });

        let request = rx.recv().await.expect("socket request");
        let (request_id, delivery_generation, timeout_secs) = match request {
            EdgeServerMessage::ToolRequest {
                request_id,
                delivery_generation,
                timeout_secs,
                ..
            } => (request_id, delivery_generation, timeout_secs),
            other => panic!("expected tool request, got {other:?}"),
        };
        assert_eq!(timeout_secs, MAX_EDGE_TOOL_TIMEOUT_SECS);
        assert_eq!(
            edge_result_wait_timeout(timeout_secs),
            Duration::from_secs(MAX_EDGE_TOOL_TIMEOUT_SECS + EDGE_TOOL_RESULT_GRACE_SECS),
            "server callback custody must outlive the edge execution deadline"
        );
        assert!(pool.deliver_tool_result(
            "user-1",
            "edge-a",
            &request_id,
            delivery_generation,
            EdgeToolResult {
                output: "completed".to_string(),
                is_error: false,
                duration_ms: Some(1),
                tool_result_fields: None,
            }
        ));
        assert_eq!(caller.await.unwrap().unwrap().output, "completed");
    }

    #[tokio::test]
    async fn reconnect_commit_preserves_pending_across_cleanup_window() {
        // Transactional reconnect: begin → the previous connection is cleaned up
        // mid-window (must NOT clear its pending map because the intent is set) →
        // commit inherits the preserved map → the admitted caller is delivered.
        let pool = EdgeConnectionPool::new();
        let (old_tx, mut old_rx) = mpsc::channel(1);
        let old_generation = pool.register("user-1", "edge-a", None, None, old_tx);
        let (caller, request_id, delivery_generation) =
            admit_on_edge_a(&pool, "reconnect-window-result", &mut old_rx).await;

        // Handler opens the reconnect before its DB await.
        let reconnect = pool.begin_reconnect("user-1", "edge-a");

        // Previous connection's cleanup runs during the window: entry removed,
        // but the pending map must be preserved (intent set).
        assert!(pool.unregister_generation("user-1", "edge-a", old_generation));

        // DB succeeds → commit installs the new connection, inheriting the map.
        let (new_tx, _new_rx) = mpsc::channel(1);
        pool.commit_reconnect(
            reconnect, "user-1", "edge-a", None, None, None, None, new_tx,
        );

        assert!(pool.deliver_tool_result(
            "user-1",
            "edge-a",
            &request_id,
            delivery_generation,
            EdgeToolResult {
                output: "reconnect-replay".to_string(),
                is_error: false,
                duration_ms: Some(1),
                tool_result_fields: None,
            }
        ));
        assert_eq!(
            caller.await.unwrap().unwrap().output,
            "reconnect-replay",
            "commit after a cleanup window must preserve pending waiters"
        );
    }

    #[tokio::test]
    async fn reconnect_keeps_previous_connection_selectable_until_commit() {
        // During the reconnect window the previous connection stays active and
        // selectable; commit then atomically swaps in the new one.
        let pool = EdgeConnectionPool::new();
        let (old_tx, _old_rx) = mpsc::channel(1);
        pool.register_with_capabilities(
            "user-1",
            "edge-a",
            Some("old-host".into()),
            None,
            None,
            None,
            old_tx,
        );

        let reconnect = pool.begin_reconnect("user-1", "edge-a");
        // Old connection is still the one selected for dispatch mid-window.
        let during = pool.get_user_edges("user-1", None);
        assert_eq!(during.len(), 1);
        assert_eq!(during[0].hostname.as_deref(), Some("old-host"));

        let (new_tx, _new_rx) = mpsc::channel(1);
        pool.commit_reconnect(
            reconnect,
            "user-1",
            "edge-a",
            Some("new-host".into()),
            None,
            None,
            None,
            new_tx,
        );
        let after = pool.get_user_edges("user-1", None);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].hostname.as_deref(), Some("new-host"));
    }

    #[tokio::test]
    async fn reconnect_abort_leaves_previous_connection_active() {
        // DB failure → abort must leave the still-active previous connection in
        // place (no zombie, no rollback) and keep its waiters deliverable.
        let pool = EdgeConnectionPool::new();
        let (old_tx, mut old_rx) = mpsc::channel(1);
        pool.register_with_capabilities(
            "user-1",
            "edge-a",
            Some("old-host".into()),
            None,
            None,
            None,
            old_tx,
        );
        let (caller, request_id, delivery_generation) =
            admit_on_edge_a(&pool, "abort-result", &mut old_rx).await;

        let reconnect = pool.begin_reconnect("user-1", "edge-a");
        pool.abort_reconnect(reconnect, "user-1", "edge-a");

        let edges = pool.get_user_edges("user-1", None);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].hostname.as_deref(), Some("old-host"));
        assert!(pool.deliver_tool_result(
            "user-1",
            "edge-a",
            &request_id,
            delivery_generation,
            EdgeToolResult {
                output: "still-alive".to_string(),
                is_error: false,
                duration_ms: Some(1),
                tool_result_fields: None,
            }
        ));
        assert_eq!(caller.await.unwrap().unwrap().output, "still-alive");
    }

    #[tokio::test]
    async fn reconnect_abort_fails_waiters_when_previous_was_cleaned_up() {
        // If the previous connection was cleaned up during the window and the
        // reconnect then fails, the preserved waiters must be failed (not left
        // hanging until timeout).
        let pool = EdgeConnectionPool::new();
        let (old_tx, mut old_rx) = mpsc::channel(1);
        let old_generation = pool.register("user-1", "edge-a", None, None, old_tx);
        let (caller, _request_id, _delivery_generation) =
            admit_on_edge_a(&pool, "abort-cleanup-result", &mut old_rx).await;

        let reconnect = pool.begin_reconnect("user-1", "edge-a");
        assert!(pool.unregister_generation("user-1", "edge-a", old_generation));
        pool.abort_reconnect(reconnect, "user-1", "edge-a");

        assert!(!pool.has_connected_edge("user-1"));
        // The caller resolves to a failure promptly (sender dropped), not a hang.
        assert!(caller.await.unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reconnect_abort_racing_with_plain_registration_never_strands_waiters() {
        for iteration in 0..64 {
            let pool = EdgeConnectionPool::new();
            let (old_tx, mut old_rx) = mpsc::channel(1);
            let old_generation = pool.register("user-1", "edge-a", None, None, old_tx);
            let (caller, _, _) =
                admit_on_edge_a(&pool, &format!("abort-race-{iteration}"), &mut old_rx).await;

            let reservation = pool.begin_reconnect("user-1", "edge-a");
            assert!(pool.unregister_generation("user-1", "edge-a", old_generation));

            let barrier = Arc::new(std::sync::Barrier::new(2));
            let abort_barrier = barrier.clone();
            let abort = std::thread::spawn(move || {
                abort_barrier.wait();
                drop(reservation);
            });

            let register_pool = pool.clone();
            let register_barrier = barrier.clone();
            let (new_tx, _new_rx) = mpsc::channel(1);
            let register = std::thread::spawn(move || {
                register_barrier.wait();
                register_pool
                    .register_with_capabilities("user-1", "edge-a", None, None, None, None, new_tx)
            });

            abort.join().expect("abort thread");
            register.join().expect("registration thread");
            let result = tokio::time::timeout(Duration::from_secs(1), caller)
                .await
                .expect("aborted waiter must resolve")
                .expect("tool task must not panic");
            assert!(
                result.is_none(),
                "iteration {iteration}: a non-inheriting registration cannot own the old waiter"
            );
        }
    }

    #[tokio::test]
    async fn first_connection_abort_leaves_pool_empty() {
        // A first-time connection (no previous) whose DB step fails leaves the
        // pool empty.
        let pool = EdgeConnectionPool::new();
        let reconnect = pool.begin_reconnect("user-1", "edge-a");
        pool.abort_reconnect(reconnect, "user-1", "edge-a");
        assert!(!pool.has_connected_edge("user-1"));
    }

    #[test]
    fn gc_reconnect_lock_removes_idle_entry() {
        // With no other reconnect holding the lock, GC drops the map entry so a
        // long-lived process cannot accumulate one mutex per historical edge id.
        let pool = EdgeConnectionPool::new();
        let l1 = pool.reconnect_lock("user-1", "edge-a");
        pool.gc_reconnect_lock("user-1", "edge-a");
        let l2 = pool.reconnect_lock("user-1", "edge-a");
        assert!(
            !Arc::ptr_eq(&l1, &l2),
            "an idle reconnect lock must be GC'd and re-created fresh"
        );
    }

    #[test]
    fn gc_reconnect_lock_keeps_entry_held_by_a_waiter() {
        // A concurrent reconnect holding its own clone raises the strong count
        // above the map's + caller's, so GC must NOT remove the shared lock.
        let pool = EdgeConnectionPool::new();
        let l1 = pool.reconnect_lock("user-1", "edge-a");
        let _waiter = pool.reconnect_lock("user-1", "edge-a");
        pool.gc_reconnect_lock("user-1", "edge-a");
        let l2 = pool.reconnect_lock("user-1", "edge-a");
        assert!(
            Arc::ptr_eq(&l1, &l2),
            "a reconnect lock still held by a waiter must not be GC'd"
        );
    }

    #[test]
    fn get_user_edges_returns_info() {
        let pool = EdgeConnectionPool::new();
        let (tx1, _rx1) = mpsc::channel(1);
        let (tx2, _rx2) = mpsc::channel(1);
        pool.register("user-1", "edge-a", Some("laptop".into()), None, tx1);
        pool.register("user-1", "edge-b", Some("desktop".into()), None, tx2);

        let edges = pool.get_user_edges("user-1", None);
        assert_eq!(edges.len(), 2);
        let names: Vec<&str> = edges.iter().map(|e| e.edge_agent_id.as_str()).collect();
        assert!(names.contains(&"edge-a"));
        assert!(names.contains(&"edge-b"));
    }

    #[test]
    fn get_user_edges_returns_structured_capabilities() {
        let pool = EdgeConnectionPool::new();
        let (tx, _rx) = mpsc::channel(1);
        pool.register_with_capabilities(
            "user-1",
            "edge-a",
            Some("laptop".into()),
            Some("/workspace".into()),
            Some(json!({
                "schema_version": 1,
                "binding": {
                    "runtime": {"provider": "host_process"},
                    "capabilities": {"runtime": {"shell": true, "git": true}}
                }
            })),
            None,
            tx,
        );

        let edges = pool.get_user_edges("user-1", None);

        assert_eq!(edges.len(), 1);
        assert_eq!(
            edges[0].capabilities.as_ref().unwrap()["binding"]["runtime"]["provider"],
            "host_process"
        );
        assert_eq!(
            edges[0].capabilities.as_ref().unwrap()["binding"]["capabilities"]["runtime"]["git"],
            true
        );
    }

    #[test]
    fn closed_sender_detected_as_disconnected() {
        let pool = EdgeConnectionPool::new();
        let (tx, rx) = mpsc::channel(1);
        pool.register("user-1", "edge-a", None, None, tx);
        drop(rx); // close the receiver, which closes the sender
        assert!(!pool.has_connected_edge("user-1"));
    }

    #[test]
    fn cleanup_stale_removes_closed() {
        let pool = EdgeConnectionPool::new();
        let (tx, rx) = mpsc::channel(1);
        pool.register("user-1", "edge-stale", None, None, tx);
        drop(rx);
        assert_eq!(pool.connection_count(), 1);
        pool.cleanup_stale();
        assert_eq!(pool.connection_count(), 0);
    }

    #[tokio::test]
    async fn deliver_tool_result_completes_pending() {
        let pool = EdgeConnectionPool::new();
        let (tx, mut rx) = mpsc::channel(1);
        pool.register("user-1", "edge-a", None, None, tx);

        let identity = admitted_identity("deliver-result");
        let pool_clone = pool.clone();
        let handle = tokio::spawn(async move {
            pool_clone
                .execute_durably_admitted_invocation_with_cancel(
                    &identity,
                    "edge-a",
                    "bash",
                    &json!({"command": "ls"}),
                    None,
                )
                .await
        });

        // Wait a bit for the request to be sent
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Read the tool request from the receiver
        let msg = rx.recv().await.unwrap();
        let (request_id, delivery_generation) = match msg {
            EdgeServerMessage::ToolRequest {
                request_id,
                delivery_generation,
                ..
            } => (request_id, delivery_generation),
            _ => panic!("expected ToolRequest"),
        };

        // Deliver the result
        let delivered = pool.deliver_tool_result(
            "user-1",
            "edge-a",
            &request_id,
            delivery_generation,
            EdgeToolResult {
                output: "file1.txt\nfile2.txt".into(),
                is_error: false,
                duration_ms: Some(10),
                tool_result_fields: Some(serde_json::Map::from_iter([(
                    "exit_code".to_string(),
                    serde_json::json!(0),
                )])),
            },
        );
        assert!(delivered);

        // The execute_tool should now complete
        let result = handle.await.unwrap();
        assert!(result.is_some());
        let result = result.unwrap();
        assert_eq!(result.output, "file1.txt\nfile2.txt");
        assert!(!result.is_error);
        assert_eq!(
            result
                .tool_result_fields
                .as_ref()
                .and_then(|fields| fields.get("exit_code"))
                .and_then(serde_json::Value::as_i64),
            Some(0)
        );
    }

    #[tokio::test]
    async fn durably_admitted_invocation_returns_none_for_missing_edge() {
        let pool = EdgeConnectionPool::new();
        let identity = admitted_identity("missing-edge");
        let result = pool
            .execute_durably_admitted_invocation_with_cancel(
                &identity,
                "nonexistent",
                "bash",
                &json!({}),
                None,
            )
            .await;
        assert!(result.is_none());
    }

    #[test]
    fn pending_requests_are_isolated_by_exact_user_id() {
        let pool = EdgeConnectionPool::new();
        pool.insert_pending_request(
            "alice",
            "req-1",
            DispatchedToolRequest {
                request_id: "req-1".into(),
                tool_name: "bash".into(),
                args: json!({"command":"pwd"}),
                dispatched_at: Instant::now(),
            },
        );
        pool.insert_pending_request(
            "alice:eve",
            "req-2",
            DispatchedToolRequest {
                request_id: "req-2".into(),
                tool_name: "bash".into(),
                args: json!({"command":"whoami"}),
                dispatched_at: Instant::now(),
            },
        );

        let alice = pool.get_pending_requests_for_user("alice");
        assert_eq!(alice.len(), 1);
        assert_eq!(alice[0].request_id, "req-1");
    }

    #[test]
    fn pending_request_eviction_updates_user_index() {
        let mut pool = EdgeConnectionPool::new();
        pool.max_pending = 2;

        for request_id in ["req-1", "req-2", "req-3"] {
            pool.insert_pending_request(
                "alice",
                request_id,
                DispatchedToolRequest {
                    request_id: request_id.into(),
                    tool_name: "bash".into(),
                    args: json!({}),
                    dispatched_at: Instant::now(),
                },
            );
        }

        let pending = pool.get_pending_requests_for_user("alice");
        let ids: Vec<&str> = pending.iter().map(|req| req.request_id.as_str()).collect();
        assert_eq!(ids, vec!["req-2", "req-3"]);
    }

    #[test]
    fn per_user_pending_request_eviction_preserves_other_users() {
        let mut pool = EdgeConnectionPool::new();
        pool.max_pending = 10;
        pool.max_pending_per_user = 2;

        for request_id in ["alice-1", "alice-2", "bob-1", "alice-3"] {
            let user_id = if request_id.starts_with("alice") {
                "alice"
            } else {
                "bob"
            };
            pool.insert_pending_request(
                user_id,
                request_id,
                DispatchedToolRequest {
                    request_id: request_id.into(),
                    tool_name: "bash".into(),
                    args: json!({}),
                    dispatched_at: Instant::now(),
                },
            );
        }

        let alice = pool.get_pending_requests_for_user("alice");
        let alice_ids: Vec<&str> = alice.iter().map(|req| req.request_id.as_str()).collect();
        assert_eq!(alice_ids, vec!["alice-2", "alice-3"]);

        let bob = pool.get_pending_requests_for_user("bob");
        let bob_ids: Vec<&str> = bob.iter().map(|req| req.request_id.as_str()).collect();
        assert_eq!(bob_ids, vec!["bob-1"]);
    }

    #[test]
    fn ack_completed_only_removes_matching_user_requests() {
        let pool = EdgeConnectionPool::new();
        pool.insert_pending_request(
            "alice",
            "req-1",
            DispatchedToolRequest {
                request_id: "req-1".into(),
                tool_name: "bash".into(),
                args: json!({}),
                dispatched_at: Instant::now(),
            },
        );
        pool.insert_pending_request(
            "bob",
            "req-2",
            DispatchedToolRequest {
                request_id: "req-2".into(),
                tool_name: "bash".into(),
                args: json!({}),
                dispatched_at: Instant::now(),
            },
        );

        pool.ack_completed_for_user("alice", &["req-2".into(), "req-1".into()]);

        assert!(pool.get_pending_requests_for_user("alice").is_empty());
        assert_eq!(pool.get_pending_requests_for_user("bob").len(), 1);
    }

    // ── drain ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn drain_empty_pool_returns_zero() {
        let pool = EdgeConnectionPool::new();
        let count = pool.drain().await;
        assert_eq!(count, 0);
        assert_eq!(pool.connection_count(), 0);
    }

    #[tokio::test]
    async fn drain_closes_live_connections_and_clears_pool() {
        let pool = EdgeConnectionPool::new();
        let (tx, mut rx) = mpsc::channel(1);
        pool.register("user-1", "edge-a", None, None, tx);
        assert_eq!(pool.connection_count(), 1);

        let count = pool.drain().await;
        assert_eq!(count, 1);
        assert_eq!(pool.connection_count(), 0);

        // Edge should receive a Closing frame.
        let msg = rx.try_recv().unwrap();
        assert!(matches!(msg, EdgeServerMessage::Closing { .. }));
    }

    #[tokio::test]
    async fn drain_skips_already_closed_senders() {
        let pool = EdgeConnectionPool::new();

        // Live connection
        let (tx1, _rx1) = mpsc::channel(1);
        pool.register("user-1", "edge-a", None, None, tx1);

        // Already-closed connection
        let (tx2, rx2) = mpsc::channel(1);
        pool.register("user-1", "edge-b", None, None, tx2);
        drop(rx2); // close

        assert_eq!(pool.connection_count(), 2);

        let count = pool.drain().await;

        // Only the live connection counts; the closed sender is skipped.
        assert_eq!(count, 1);
        // Closed connections are intentionally left in the pool (they cannot
        // be drained); call cleanup_stale to remove them.
        assert_eq!(pool.connection_count(), 1);
        pool.cleanup_stale();
        assert_eq!(pool.connection_count(), 0);
    }

    #[tokio::test]
    async fn drain_clears_in_flight_results_for_live_connections() {
        let pool = EdgeConnectionPool::new();
        let (tx, mut rx) = mpsc::channel(1);
        pool.register("user-1", "edge-a", None, None, tx);

        // Dispatch a tool call so there's an in-flight oneshot.
        let identity = admitted_identity("drain-inflight");
        let pool_clone = pool.clone();
        let handle = tokio::spawn(async move {
            pool_clone
                .execute_durably_admitted_invocation_with_cancel(
                    &identity,
                    "edge-a",
                    "bash",
                    &json!({"command":"ls"}),
                    None,
                )
                .await
        });

        // Read the ToolRequest so the oneshot is stored in pending_results.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _msg = rx.recv().await.unwrap();

        // Drain should unregister the connection, dropping the oneshot sender.
        let count = pool.drain().await;
        assert_eq!(count, 1);

        // The execute_tool caller gets None (oneshot was dropped).
        let result = handle.await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn drain_tolerates_concurrent_drain_calls() {
        let pool = EdgeConnectionPool::new();
        let (tx, _rx) = mpsc::channel(1);
        pool.register("user-1", "edge-a", None, None, tx);

        let p1 = pool.clone();
        let p2 = pool.clone();
        let (c1, c2) = tokio::join!(p1.drain(), p2.drain());

        // Both should succeed; pool ends empty.
        assert!(c1 <= 1);
        assert!(c2 <= 1);
        assert_eq!(pool.connection_count(), 0);
    }

    // ── generation race condition ─────────────────────────────────────

    /// Register gen-1 then gen-2 for the same edge; cleaning up with gen-1 must
    /// leave gen-2 intact. Only a subsequent gen-2 cleanup removes the entry.
    #[test]
    fn unregister_generation_skips_stale_cleanup() {
        let pool = EdgeConnectionPool::new();

        let (tx1, _rx1) = mpsc::channel(1);
        let gen1 = pool.register("user-1", "edge-a", None, None, tx1);

        // Second connection replaces the first in the pool.
        let (tx2, _rx2) = mpsc::channel(1);
        let gen2 = pool.register("user-1", "edge-a", None, None, tx2);

        assert!(gen2 > gen1, "generations must be strictly increasing");

        // Old connection's cleanup fires with gen-1: must be a no-op.
        let removed = pool.unregister_generation("user-1", "edge-a", gen1);
        assert!(
            !removed,
            "stale gen-1 cleanup must not remove the gen-2 entry"
        );
        assert!(
            pool.has_connected_edge("user-1"),
            "gen-2 connection must still be in the pool"
        );

        // Current connection's cleanup fires with gen-2: must remove the entry.
        let removed = pool.unregister_generation("user-1", "edge-a", gen2);
        assert!(removed, "gen-2 cleanup must remove the entry");
        assert!(
            !pool.has_connected_edge("user-1"),
            "pool must be empty after gen-2 cleanup"
        );
    }

    /// A single connection registered and cleaned up with its own generation
    /// removes the entry normally.
    #[test]
    fn unregister_generation_removes_own_entry() {
        let pool = EdgeConnectionPool::new();
        let (tx, _rx) = mpsc::channel(1);
        let my_gen = pool.register("user-1", "edge-a", None, None, tx);

        let removed = pool.unregister_generation("user-1", "edge-a", my_gen);
        assert!(removed);
        assert!(!pool.has_connected_edge("user-1"));
    }

    /// Three rapid reconnects: gen-1 and gen-2 cleanups must both be no-ops;
    /// only gen-3 cleanup drains the pool.
    #[test]
    fn unregister_generation_handles_multiple_replacements() {
        let pool = EdgeConnectionPool::new();

        let (tx1, _rx1) = mpsc::channel(1);
        let gen1 = pool.register("user-1", "edge-a", None, None, tx1);

        let (tx2, _rx2) = mpsc::channel(1);
        let gen2 = pool.register("user-1", "edge-a", None, None, tx2);

        let (tx3, _rx3) = mpsc::channel(1);
        let gen3 = pool.register("user-1", "edge-a", None, None, tx3);

        assert!(!pool.unregister_generation("user-1", "edge-a", gen1));
        assert!(pool.has_connected_edge("user-1"));

        assert!(!pool.unregister_generation("user-1", "edge-a", gen2));
        assert!(pool.has_connected_edge("user-1"));

        assert!(pool.unregister_generation("user-1", "edge-a", gen3));
        assert!(!pool.has_connected_edge("user-1"));
    }

    #[test]
    fn find_edge_by_agent_id_matches_connected_edge() {
        let pool = EdgeConnectionPool::new();
        let (tx, _rx) = mpsc::channel(1);
        pool.register_with_capabilities(
            "user-a",
            "edge-x",
            None,
            None,
            None,
            Some("ws-1".into()),
            tx,
        );

        let result = pool.find_edge_by_agent_id("edge-x", Some("ws-1"));
        assert!(result.is_some());
        let (owner, info) = result.unwrap();
        assert_eq!(owner, "user-a");
        assert_eq!(info.edge_agent_id, "edge-x");
    }

    #[test]
    fn find_edge_by_agent_id_workspace_mismatch_returns_none() {
        let pool = EdgeConnectionPool::new();
        let (tx, _rx) = mpsc::channel(1);
        pool.register_with_capabilities(
            "user-a",
            "edge-x",
            None,
            None,
            None,
            Some("ws-1".into()),
            tx,
        );

        // Requesting workspace differs from the edge's registered workspace.
        let result = pool.find_edge_by_agent_id("edge-x", Some("ws-2"));
        assert!(result.is_none(), "different workspace must not match");
    }

    #[test]
    fn find_edge_by_agent_id_workspace_mismatch_request_has_ws_edge_unscoped_returns_none() {
        let pool = EdgeConnectionPool::new();
        let (tx, _rx) = mpsc::channel(1);
        // Unscoped edge (no workspace binding).
        pool.register_with_capabilities("user-a", "edge-x", None, None, None, None, tx);

        // A request that carries a workspace_id must NOT silently resolve an
        // unscoped edge — fail-closed prevents workspace confusion.
        let result = pool.find_edge_by_agent_id("edge-x", Some("ws-any"));
        assert!(
            result.is_none(),
            "scoped request must not resolve an unscoped edge"
        );
    }

    #[test]
    fn find_edge_by_agent_id_no_workspace_context_resolves_unscoped_edge() {
        let pool = EdgeConnectionPool::new();
        let (tx, _rx) = mpsc::channel(1);
        pool.register_with_capabilities("user-a", "edge-x", None, None, None, None, tx);

        // Neither side has workspace_id — legacy / single-tenant path is allowed.
        let result = pool.find_edge_by_agent_id("edge-x", None);
        assert!(
            result.is_some(),
            "unscoped request must resolve unscoped edge"
        );
    }

    #[test]
    fn find_edge_by_agent_id_no_workspace_context_does_not_resolve_scoped_edge() {
        let pool = EdgeConnectionPool::new();
        let (tx, _rx) = mpsc::channel(1);
        pool.register_with_capabilities(
            "user-a",
            "edge-x",
            None,
            None,
            None,
            Some("ws-sandbox".to_string()),
            tx,
        );

        // A request without workspace context must NOT resolve a workspace-bound
        // sandbox edge (fail-closed: workspace_record: None on provider turn).
        let result = pool.find_edge_by_agent_id("edge-x", None);
        assert!(
            result.is_none(),
            "unscoped request must not resolve a workspace-bound edge"
        );
    }

    /// Cleanups for different edges must be fully independent.
    #[test]
    fn unregister_generation_keeps_different_edges_independent() {
        let pool = EdgeConnectionPool::new();

        let (tx_a1, _rx_a1) = mpsc::channel(1);
        let gen_a1 = pool.register("user-1", "edge-a", None, None, tx_a1);

        let (tx_a2, _rx_a2) = mpsc::channel(1);
        let _gen_a2 = pool.register("user-1", "edge-a", None, None, tx_a2);

        let (tx_b, _rx_b) = mpsc::channel(1);
        let gen_b = pool.register("user-1", "edge-b", None, None, tx_b);

        // Stale cleanup for edge-a must not touch edge-b.
        assert!(!pool.unregister_generation("user-1", "edge-a", gen_a1));
        assert!(pool.has_connected_edge("user-1")); // edge-b is still present

        // Cleaning up edge-b with its own generation succeeds.
        assert!(pool.unregister_generation("user-1", "edge-b", gen_b));

        // edge-a's gen-2 is still present.
        assert!(pool.has_connected_edge("user-1"));
    }

    // ── workspace isolation in get_user_edges ─────────────────────────

    #[test]
    fn get_user_edges_same_user_two_workspaces_isolation() {
        let pool = EdgeConnectionPool::new();
        let (tx_a, _rx_a) = mpsc::channel(1);
        let (tx_b, _rx_b) = mpsc::channel(1);
        pool.register_with_capabilities(
            "user-1",
            "edge-ws-a",
            None,
            None,
            None,
            Some("ws-a".into()),
            tx_a,
        );
        pool.register_with_capabilities(
            "user-1",
            "edge-ws-b",
            None,
            None,
            None,
            Some("ws-b".into()),
            tx_b,
        );

        let ws_a_edges = pool.get_user_edges("user-1", Some("ws-a"));
        assert_eq!(ws_a_edges.len(), 1);
        assert_eq!(ws_a_edges[0].edge_agent_id, "edge-ws-a");

        let ws_b_edges = pool.get_user_edges("user-1", Some("ws-b"));
        assert_eq!(ws_b_edges.len(), 1);
        assert_eq!(ws_b_edges[0].edge_agent_id, "edge-ws-b");

        // unscoped request must not see workspace-bound edges
        let unscoped = pool.get_user_edges("user-1", None);
        assert!(
            unscoped.is_empty(),
            "unscoped request must not see workspace-bound edges"
        );
    }

    #[test]
    fn get_all_user_edges_returns_across_workspaces() {
        let pool = EdgeConnectionPool::new();
        let (tx_a, _rx_a) = mpsc::channel(1);
        let (tx_b, _rx_b) = mpsc::channel(1);
        pool.register_with_capabilities(
            "user-1",
            "edge-ws-a",
            None,
            None,
            None,
            Some("ws-a".into()),
            tx_a,
        );
        pool.register_with_capabilities(
            "user-1",
            "edge-ws-b",
            None,
            None,
            None,
            Some("ws-b".into()),
            tx_b,
        );

        let all = pool.get_all_user_edges("user-1");
        assert_eq!(all.len(), 2, "status query must see all workspace edges");
    }

    #[test]
    fn get_user_edges_workspace_filter_does_not_cross_users() {
        let pool = EdgeConnectionPool::new();
        let (tx1, _rx1) = mpsc::channel(1);
        let (tx2, _rx2) = mpsc::channel(1);
        pool.register_with_capabilities(
            "user-1",
            "edge-a",
            None,
            None,
            None,
            Some("ws-shared".into()),
            tx1,
        );
        pool.register_with_capabilities(
            "user-2",
            "edge-b",
            None,
            None,
            None,
            Some("ws-shared".into()),
            tx2,
        );

        // Same workspace but different user — must not see other user's edge.
        let edges = pool.get_user_edges("user-1", Some("ws-shared"));
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].edge_agent_id, "edge-a");
    }
}

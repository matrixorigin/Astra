//! WebSocket handler for remote edge agent connections.
//!
//! Provides the `GET /edge/ws` endpoint where edge agent binaries connect,
//! authenticate, and then receive tool execution requests from the server.
//! Results are sent back over the same WebSocket.

use super::*;
use astra_runtime_env::{CapacityProvider, ToolTransportKind};
use astra_server_types::edge_connection_pool::{
    ADMITTED_EXECUTION_DEADLINE_EXPIRED, EdgeConnectionPool, EdgeOutbound, EdgeToolResult,
    admitted_work_deadline,
};
use astra_server_types::edge_ws_protocol::*;
use astra_services::multi_agent::edge_dispatch::EDGE_RELAY_OWNER_LIMIT;
use astra_services::multi_agent::edge_registry::{
    WORK_ATTESTATION_CAPABILITY, WORK_ATTESTATION_TOOL,
};
use astra_services::multi_agent::{
    EdgeDispatchDelivery, EdgeDispatchDeliveryError, EdgeDispatchIdentity, EdgeDispatchRow,
    EdgeDispatchService, HeartbeatError,
};
use astra_thin_client::ToolResultRequest;

use super::edge_ws_transport::{self, EdgeWebSocket, WriteProgress};
use axum::extract::Request;
use axum::response::IntoResponse;
use edge_websocket::tungstenite::Message;
use edge_websocket::tungstenite::protocol::{CloseFrame, frame::coding::CloseCode};
use futures_util::StreamExt;
use futures_util::stream::{SplitSink, SplitStream};
use std::collections::HashMap;
use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::task::Poll;
use std::time::Duration;
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

/// Maximum concurrent edge WebSocket connections.
const MAX_EDGE_WS_CONNECTIONS: usize = EDGE_RELAY_OWNER_LIMIT;
const EDGE_HEARTBEAT_STORAGE_FAILURE_BUDGET: usize = 3;
const EDGE_HEARTBEAT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);
const EDGE_RESULT_SLOTS_PER_SOCKET: usize = 8;
const EDGE_RESULT_CONFIRMATION_BUDGET: Duration = Duration::from_secs(1);
const RELAY_WRITE_IDLE_LIMIT: Duration = Duration::from_secs(30);
const RELAY_SETTLEMENT_LIMIT: Duration = Duration::from_secs(30);
const EDGE_REGISTRY_RELEASE_RETRY_BASE: Duration = Duration::from_secs(1);
const EDGE_REGISTRY_RELEASE_RETRY_MAX: Duration = Duration::from_secs(30);
const EDGE_REGISTRY_RELEASE_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(5);

struct EdgeSocketWriter {
    sink: tokio::sync::Mutex<Option<SplitSink<EdgeWebSocket, Message>>>,
    closed: AtomicBool,
    write_progress: Arc<WriteProgress>,
}

type SharedEdgeSocketWriter = Arc<EdgeSocketWriter>;
type SocketAuthority<'a> = (
    &'a astra_server_types::edge_connection_pool::EdgeConnectionPool,
    &'a str,
    &'a str,
    u64,
);

type EdgeRegistryReleaseReconciliation = Pin<Box<dyn Future<Output = bool> + Send>>;

fn reconcile_edge_registry_release(
    registry: Arc<dyn astra_services::multi_agent::EdgeRegistryService>,
    lease: astra_services::multi_agent::EdgeRegistrationLease,
) -> EdgeRegistryReleaseReconciliation {
    Box::pin(async move {
        let mut retry_delay = EDGE_REGISTRY_RELEASE_RETRY_BASE;
        loop {
            match tokio::time::timeout(
                EDGE_REGISTRY_RELEASE_ATTEMPT_TIMEOUT,
                registry.release_registration(&lease),
            )
            .await
            {
                Ok(Ok(settled)) => return settled,
                Ok(Err(error)) => {
                    tracing::warn!(
                        target: "astra_runtime::edge_ws",
                        user_id = %lease.current.user_id,
                        edge_agent_id = %lease.current.edge_agent_id,
                        %error,
                        "edge WebSocket: durable registration publication remains unresolved"
                    );
                }
                Err(_) => {
                    tracing::warn!(
                        target: "astra_runtime::edge_ws",
                        user_id = %lease.current.user_id,
                        edge_agent_id = %lease.current.edge_agent_id,
                        timeout_ms = EDGE_REGISTRY_RELEASE_ATTEMPT_TIMEOUT.as_millis(),
                        "edge WebSocket: durable registration publication attempt timed out"
                    );
                }
            }
            tokio::time::sleep(retry_delay).await;
            retry_delay = retry_delay
                .saturating_mul(2)
                .min(EDGE_REGISTRY_RELEASE_RETRY_MAX);
        }
    })
}

/// Global counter of active edge WebSocket connections.
static EDGE_WS_CONNECTION_COUNT: AtomicUsize = AtomicUsize::new(0);
// Disconnect storms must not consume the entire database pool with cleanup.
static EDGE_REGISTRY_CLEANUP_LIMIT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);
static EDGE_REGISTRY_CLEANUP_OWNERS: std::sync::LazyLock<Arc<tokio::sync::Semaphore>> =
    std::sync::LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(MAX_EDGE_WS_CONNECTIONS)));

// Bound parallel receipt ownership across all sockets in this process.
static EDGE_RESULT_OWNERS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(64);

struct EdgeWsConnectionPermit;

impl Drop for EdgeWsConnectionPermit {
    fn drop(&mut self) {
        EDGE_WS_CONNECTION_COUNT.fetch_sub(1, Ordering::Release);
    }
}

struct EdgeGenerationCleanup {
    started: AtomicBool,
    registry: Arc<dyn astra_services::multi_agent::EdgeRegistryService>,
    user_id: String,
    edge_agent_id: String,
    edge_id: String,
    cleanup_permits: u32,
    ownership_limit: Arc<tokio::sync::Semaphore>,
    registration_release: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    live_permit: Weak<EdgeWsConnectionPermit>,
}

impl EdgeGenerationCleanup {
    fn start(self: &Arc<Self>) {
        // Capture before electing the owner: teardown can otherwise drop its
        // last strong reference between the election and this upgrade.
        let live_permit = self.live_permit.upgrade();
        if self.started.swap(true, Ordering::AcqRel) {
            return;
        }
        let cleanup = Arc::clone(self);
        tokio::spawn(async move {
            let release = cleanup.registration_release.lock().unwrap().take();
            if let Some(release) = release {
                release.abort();
                let _ = release.await;
            }
            // While waiting for bounded cleanup capacity, retain the old
            // connection slot. Once transferred, reconnects need not wait for
            // this generation's database cleanup.
            let Ok(_owner) = cleanup.ownership_limit.acquire().await else {
                return;
            };
            drop(live_permit);
            let mut delay = Duration::from_millis(100);
            loop {
                let permit = EDGE_REGISTRY_CLEANUP_LIMIT
                    .acquire_many(cleanup.cleanup_permits)
                    .await;
                let Ok(permit) = permit else { return };
                let result = cleanup
                    .registry
                    .unregister_generation(
                        &cleanup.user_id,
                        &cleanup.edge_agent_id,
                        &cleanup.edge_id,
                    )
                    .await;
                drop(permit);
                match result {
                    Ok(_) => return,
                    Err(error) => tracing::warn!(
                        user_id = %cleanup.user_id,
                        edge_agent_id = %cleanup.edge_agent_id,
                        %error,
                        ?delay,
                        "durable Edge cleanup remains unresolved; retrying"
                    ),
                }
                tokio::time::sleep(delay).await;
                delay = delay.saturating_mul(2).min(Duration::from_secs(30));
            }
        });
    }
}

struct EdgeGenerationCleanupGuard {
    cleanup: Arc<EdgeGenerationCleanup>,
    pool: astra_server_types::edge_connection_pool::EdgeConnectionPool,
    generation: u64,
    forward: tokio::task::AbortHandle,
    live_permit: Arc<EdgeWsConnectionPermit>,
}

impl Drop for EdgeGenerationCleanupGuard {
    fn drop(&mut self) {
        self.forward.abort();
        self.pool.unregister_generation(
            &self.cleanup.user_id,
            &self.cleanup.edge_agent_id,
            self.generation,
        );
        self.cleanup.start();
    }
}

fn try_acquire_edge_ws_connection() -> Option<EdgeWsConnectionPermit> {
    loop {
        let current = EDGE_WS_CONNECTION_COUNT.load(Ordering::Acquire);
        if current >= MAX_EDGE_WS_CONNECTIONS {
            return None;
        }
        if EDGE_WS_CONNECTION_COUNT
            .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return Some(EdgeWsConnectionPermit);
        }
    }
}

/// Axum handler for edge WebSocket upgrade.
pub(crate) async fn edge_ws_handler(
    State(state): State<AppState>,
    request: Request,
) -> impl IntoResponse {
    let (mut parts, _) = request.into_parts();
    let (response, upgrade) = match edge_ws_transport::prepare_upgrade(&mut parts).await {
        Ok(upgrade) => upgrade,
        Err(rejection) => return rejection.into_response(),
    };
    let headers = &parts.headers;
    let token = match headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(token) => token.to_string(),
        None => {
            return (axum::http::StatusCode::UNAUTHORIZED, "missing bearer token").into_response();
        }
    };
    let principal = match state
        .auth_service
        .current_principal_for_request(
            headers,
            astra_services::ProviderRequestDescriptor {
                method: "GET".to_string(),
                path: "/edge/ws".to_string(),
                route: Some("/edge/ws".to_string()),
                request_id: None,
                body_digest: None,
            },
        )
        .await
    {
        Ok(principal) => principal,
        Err(error) => return error.into_response(),
    };
    let Some(permit) = try_acquire_edge_ws_connection() else {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "too many edge WebSocket connections",
        )
            .into_response();
    };
    tokio::spawn(async move {
        match edge_ws_transport::open_socket(upgrade).await {
            Ok((socket, write_progress)) => {
                handle_edge_connection(socket, write_progress, state, permit, principal, token)
                    .await;
            }
            Err(error) => {
                tracing::warn!(target: "astra::edge_ws", %error, "edge WebSocket upgrade failed");
            }
        }
    });
    response
}

/// Main edge WebSocket connection loop.
async fn handle_edge_connection(
    socket: EdgeWebSocket,
    write_progress: Arc<WriteProgress>,
    state: AppState,
    permit: EdgeWsConnectionPermit,
    principal: astra_services::AuthPrincipal,
    token: String,
) {
    let (ws_sink, mut ws_stream) = socket.split();
    let ws_sink = Arc::new(EdgeSocketWriter {
        sink: tokio::sync::Mutex::new(Some(ws_sink)),
        closed: AtomicBool::new(false),
        write_progress,
    });

    // ── Phase 1: Authenticate ────────────────────────────────────────
    let auth_timeout = Duration::from_secs(EDGE_AUTH_TIMEOUT_SECS);
    let auth_result = tokio::time::timeout(auth_timeout, async {
        while let Some(Ok(msg)) = ws_stream.next().await {
            if let Message::Text(text) = msg {
                match serde_json::from_str::<EdgeClientMessage>(&text) {
                    Ok(EdgeClientMessage::Auth {
                        edge_agent_id,
                        materialization_id,
                        interaction_api_major,
                        hostname,
                        workspace_dir,
                        capabilities,
                    }) => {
                        if interaction_api_major
                            != astra_server_types::AGENT_INTERACTION_API_MAJOR
                        {
                            let _ = send_edge_msg(
                                &ws_sink,
                                EdgeServerMessage::AuthError {
                                    message: format!(
                                        "incompatible interaction contract: expected {}, received {}",
                                        astra_server_types::AGENT_INTERACTION_API_MAJOR,
                                        interaction_api_major,
                                    ),
                                },
                            )
                            .await;
                            return None;
                        }
                        return Some((
                            edge_agent_id,
                            materialization_id,
                            hostname,
                            workspace_dir,
                            capabilities,
                        ));
                    }
                    _ => {
                        let _ = send_edge_msg(
                            &ws_sink,
                            EdgeServerMessage::AuthError {
                                message: "first message must be edge_auth".into(),
                            },
                        )
                        .await;
                        return None;
                    }
                }
            }
        }
        None
    })
    .await;

    let (edge_agent_id, materialization_id, hostname, workspace_dir, capabilities) =
        match auth_result {
            Ok(Some(auth)) => auth,
            _ => {
                tracing::warn!(
                    target: "astra_runtime::edge_ws",
                    "edge WebSocket auth timeout or closed before edge_auth"
                );
                let _ = send_edge_msg(
                    &ws_sink,
                    EdgeServerMessage::AuthError {
                        message: "auth timeout or connection closed".into(),
                    },
                )
                .await;
                return;
            }
        };

    if !astra_runtime_env::is_valid_provider_id(&edge_agent_id)
        || materialization_id.len() > 128
        || !astra_runtime_env::is_valid_provider_id(&materialization_id)
    {
        tracing::warn!(
            target: "astra_runtime::edge_ws",
            edge_agent_id = %edge_agent_id,
            "edge WebSocket auth failed: invalid edge or materialization identity"
        );
        let _ = send_edge_msg(
            &ws_sink,
            EdgeServerMessage::AuthError {
                message: "invalid edge or materialization identity".into(),
            },
        )
        .await;
        return;
    }

    let user_id = principal.user.user_id.clone();

    // ── Phase 1.5: Verify self-reported edge_agent_id matches token binding ──
    // Resolve the edge binding from one of two sources, then run a SINGLE
    // verification below so the check can never be silently skipped:
    //
    //   • Fast path — a provider-authorized principal already carries the
    //     binding resolved during Phase 1 (no second provider call).
    //   • Compatibility path — AuthService impls that expose the binding only
    //     through `edge_registration_binding()` (the default `current_principal`
    //     returns an Internal origin) are still honored via a fallback call.
    //     Without this, such backends would fall through and accept any
    //     self-reported edge_agent_id with no workspace binding.
    //
    // The returned `workspace_id` is `provider_scope_id`; the MOI provider maps
    // its workspace identity 1:1 onto that opaque scope key. If a future
    // provider uses a different mapping, revisit it in AuthService, not here.
    let binding: Option<(String, String)> = match &principal.origin {
        astra_services::AuthPrincipalOrigin::ProviderAuthorizedRequest(ctx) => {
            match ctx.edge_agent_id.as_deref() {
                Some(bound_edge_agent_id) => Some((
                    bound_edge_agent_id.to_string(),
                    ctx.provider_scope_id.clone(),
                )),
                None => {
                    // Provider-authorized but no edge_agent_id binding means this is
                    // a runtime token (server-to-server), which must not be accepted
                    // on the WebSocket path — only edge-registration tokens are valid here.
                    tracing::warn!(
                        target: "astra_runtime::edge_ws",
                        edge_agent_id = %edge_agent_id,
                        "edge WebSocket auth failed: provider token has no edge_agent_id binding (runtime token on WS path)"
                    );
                    let _ = send_edge_msg(
                        &ws_sink,
                        EdgeServerMessage::AuthError {
                            message: "edge registration token required; runtime token not accepted on WebSocket path".into(),
                        },
                    )
                    .await;
                    return;
                }
            }
        }
        _ => {
            // Not provider-authorized in the principal: consult the explicit
            // edge-binding contract, which distinguishes a first-party token
            // (accept, no binding) from a runtime token with no agent binding
            // (reject) — a plain `None` would conflate the two and fail open.
            match state.auth_service.edge_registration_binding(&token).await {
                Ok(astra_services::EdgeTokenBinding::Bound {
                    edge_agent_id: bound_edge_agent_id,
                    workspace_id,
                }) => Some((bound_edge_agent_id, workspace_id)),
                Ok(astra_services::EdgeTokenBinding::NotEdgeToken) => None,
                Ok(astra_services::EdgeTokenBinding::MissingBinding) => {
                    // Recognized provider token without an edge_agent_id binding
                    // (runtime server-to-server token): not valid on the WS path.
                    tracing::warn!(
                        target: "astra_runtime::edge_ws",
                        edge_agent_id = %edge_agent_id,
                        "edge WebSocket auth failed: token has no edge_agent_id binding (runtime token on WS path)"
                    );
                    let _ = send_edge_msg(
                        &ws_sink,
                        EdgeServerMessage::AuthError {
                            message: "edge registration token required; runtime token not accepted on WebSocket path".into(),
                        },
                    )
                    .await;
                    return;
                }
                Err(_) => {
                    tracing::warn!(
                        target: "astra_runtime::edge_ws",
                        edge_agent_id = %edge_agent_id,
                        "edge WebSocket auth failed: edge token binding verification failed"
                    );
                    let _ = send_edge_msg(
                        &ws_sink,
                        EdgeServerMessage::AuthError {
                            message: "edge token binding verification failed".into(),
                        },
                    )
                    .await;
                    return;
                }
            }
        }
    };

    let workspace_id: Option<String> = match binding {
        Some((bound_edge_agent_id, workspace_id)) => {
            if bound_edge_agent_id != edge_agent_id {
                tracing::warn!(
                    target: "astra_runtime::edge_ws",
                    edge_agent_id = %edge_agent_id,
                    bound_edge_agent_id = %bound_edge_agent_id,
                    "edge WebSocket auth failed: self-reported edge_agent_id does not match token binding"
                );
                let _ = send_edge_msg(
                    &ws_sink,
                    EdgeServerMessage::AuthError {
                        message: "edge_agent_id does not match token binding".into(),
                    },
                )
                .await;
                return;
            }
            Some(workspace_id)
        }
        // First-party (astra-native JWT) tokens carry no binding and are
        // accepted without an edge_agent_id check.
        None => None,
    };

    // ── Phase 1a: Validate & sanitize self-reported capabilities ─────
    // Edge nodes self-report their capabilities; a malicious edge could
    // fabricate tool names or claim capabilities it doesn't possess.
    // We validate against the server-side registry and strip anything
    // that doesn't check out.
    let capabilities = match validate_edge_capabilities(
        capabilities,
        &edge_agent_id,
        ToolTransportKind::EdgeWebSocket,
    )
    .and_then(|mut capabilities| {
        if capabilities.get("capacity_rpc_v1").is_some() {
            if let Some(origin) = &state.execution.edge_capacity_relay_origin {
                capabilities["capacity_relay_v1"] = serde_json::json!({"origin": origin});
            } else if state.execution.edge_registry_service.has_durable_routing() {
                return Err("capacity node routing is not configured".to_string());
            }
        }
        Ok(capabilities)
    }) {
        Ok(capabilities) => Some(capabilities),
        Err(message) => {
            tracing::warn!(
                target: "astra_runtime::edge_ws",
                edge_agent_id = %edge_agent_id,
                error = %message,
                "edge WebSocket capability admission failed"
            );
            let _ = send_edge_msg(&ws_sink, EdgeServerMessage::AuthError { message }).await;
            return;
        }
    };

    // Subscribe before publishing this connection. A wake is only a hint: the
    // durable observer also finds work committed before this subscription.
    let dispatch_wakeup = match state
        .execution
        .edge_dispatch_service
        .subscribe_pending_wakeup(&user_id, &edge_agent_id)
        .await
    {
        Ok(wakeup) => wakeup,
        Err(error) => {
            tracing::error!(
                user_id = %user_id,
                edge_agent_id = %edge_agent_id,
                %error,
                "edge dispatch subscription failed"
            );
            close_unavailable_edge(&ws_sink, "edge dispatch notifications unavailable").await;
            return;
        }
    };
    if dispatch_wakeup.as_ref().is_some_and(|receiver| {
        receiver.has_changed().is_err()
            || state
                .execution
                .edge_dispatch_service
                .pending_limits()
                .is_none()
    }) {
        close_unavailable_edge(&ws_sink, "edge dispatch notifications unavailable").await;
        return;
    }

    tracing::info!(
        user_id = %user_id,
        edge_agent_id = %edge_agent_id,
        hostname = ?hostname,
        "Edge agent connected"
    );

    // ── Phase 2: Begin a transactional reconnect ─────────────────────
    // Serialize the whole reconnect (begin → DB → commit/abort) for this key so
    // two concurrent reconnects cannot interleave their DB registration and pool
    // commit (which could leave the DB pointing at one incarnation and the pool
    // at another). The guard is released right after commit/abort, NOT held for
    // the connection's lifetime — a later reconnect must be able to supersede.
    let edge_registry = state.execution.edge_registry_service.clone();
    let edge_id_for_registry = format!("ws-{}", uuid::Uuid::new_v4());
    let reconnect_mutex = state
        .edge_connection_pool
        .reconnect_lock(&user_id, &edge_agent_id);
    // Watch for the socket closing WHILE queued for the lock: a connection that
    // died in the queue must not go on to overwrite a healthy peer.
    let reconnect_guard = loop {
        tokio::select! {
            guard = reconnect_mutex.lock() => break guard,
            frame = ws_stream.next() => {
                if !handle_edge_setup_frame(frame, &ws_sink).await {
                    // Socket closed before we acquired the lock; nothing was touched.
                    state
                        .edge_connection_pool
                        .gc_reconnect_lock(&user_id, &edge_agent_id);
                    return;
                }
            }
        }
    };

    // If the socket already closed before we could register, bail out before
    // touching the pool or the DB so a dead connection never evicts a peer.
    if edge_socket_appears_closed(&mut ws_stream, &ws_sink).await {
        drop(reconnect_guard);
        state
            .edge_connection_pool
            .gc_reconnect_lock(&user_id, &edge_agent_id);
        return;
    }

    // Do NOT touch the pool yet: any existing connection stays active and
    // selectable throughout the DB registration below, so it is never a
    // half-ready/zombie entry and dispatch never selects a connection whose
    // forward loop is not yet running. The reservation marks the key so a
    // concurrent cleanup of the previous connection preserves (rather than
    // clears) its pending-result map, and captures that map so it survives even
    // if the previous entry is removed mid-reconnect.
    let reconnect = state
        .edge_connection_pool
        .begin_reconnect(&user_id, &edge_agent_id);

    // ── Phase 2a: Register in DB edge registry for cross-pod routing ─
    // B4 counterpart: UnconfiguredEdgeRegistryService always returns Ok here,
    // so single-pod deployments without a DB registry are unaffected.
    //
    // The durable registry returns a generation lease containing the exact row
    // replaced by this connection. Its DB implementation uses an edge_id CAS,
    // so this predecessor remains correct even when another pod reconnects the
    // same edge concurrently.
    let mut registration = Box::pin(
        edge_registry.register_or_update_with_lease_and_materialization(
            &user_id,
            &edge_agent_id,
            &edge_id_for_registry,
            hostname.as_deref(),
            workspace_dir.as_deref(),
            capabilities.clone(),
            workspace_id.as_deref(),
            Some(&materialization_id),
        ),
    );
    let mut socket_closed_during_registration = false;
    let registration_result = loop {
        tokio::select! {
            result = &mut registration => break result,
            frame = ws_stream.next() => {
                if !handle_edge_setup_frame(frame, &ws_sink).await {
                    // Keep driving the DB future to a known outcome. Dropping it
                    // here could leave an outcome-unknown write with no lease to
                    // reconcile conditionally.
                    socket_closed_during_registration = true;
                    break registration.await;
                }
            }
        }
    };
    let registration_lease = match registration_result {
        Ok(lease) => lease,
        Err(e) => {
            tracing::warn!(
                target: "astra_runtime::edge_ws",
                user_id = %user_id,
                edge_agent_id = %edge_agent_id,
                error = %e,
                "edge WebSocket: DB registry registration failed; rejecting connection"
            );
            // Abort the reconnect: leave any still-active previous connection in
            // place (no zombie, no rollback), fail its waiters only if it was
            // cleaned up during the window.
            state
                .edge_connection_pool
                .abort_reconnect(reconnect, &user_id, &edge_agent_id);
            drop(reconnect_guard);
            state
                .edge_connection_pool
                .gc_reconnect_lock(&user_id, &edge_agent_id);
            close_unavailable_edge(&ws_sink, "edge registry unavailable").await;
            return;
        }
    };

    // If the socket closed during DB registration, do NOT commit (which would
    // evict a healthy peer). Abort the reconnect, keeping the previous in-memory
    // connection, and reconcile the DB row we just overwrote:
    //   • If a previous connection owned the row, restore its edge_id so its next
    //     heartbeat is not superseded — merely deleting our row would strand it.
    //   • Otherwise (we were the first registration) delete our row.
    if socket_closed_during_registration
        || edge_socket_appears_closed(&mut ws_stream, &ws_sink).await
    {
        state
            .edge_connection_pool
            .abort_reconnect(reconnect, &user_id, &edge_agent_id);
        if let Err(error) = edge_registry
            .rollback_registration(&registration_lease)
            .await
        {
            tracing::error!(
                target: "astra_runtime::edge_ws",
                user_id = %user_id,
                edge_agent_id = %edge_agent_id,
                %error,
                "edge WebSocket: failed to roll back durable registration after dead reconnect"
            );
        }
        drop(reconnect_guard);
        state
            .edge_connection_pool
            .gc_reconnect_lock(&user_id, &edge_agent_id);
        return;
    }

    // Publish the durable generation while retaining the DB claim. Existing
    // routing fields stayed untouched during claim acquisition; finalize moves
    // the row to a non-routable handoff state until pool commit releases it.
    let mut finalization = Box::pin(edge_registry.finalize_registration(&registration_lease));
    let mut socket_closed_during_finalization = false;
    let finalization_result = loop {
        tokio::select! {
            result = &mut finalization => break result,
            frame = ws_stream.next() => {
                if !handle_edge_setup_frame(frame, &ws_sink).await {
                    socket_closed_during_finalization = true;
                    break finalization.await;
                }
            }
        }
    };
    if !matches!(&finalization_result, Ok(true)) {
        state
            .edge_connection_pool
            .abort_reconnect(reconnect, &user_id, &edge_agent_id);
        if let Err(error) = edge_registry
            .rollback_registration(&registration_lease)
            .await
        {
            tracing::error!(
                target: "astra_runtime::edge_ws",
                user_id = %user_id,
                edge_agent_id = %edge_agent_id,
                %error,
                "edge WebSocket: failed to roll back rejected durable finalization"
            );
        }
        drop(reconnect_guard);
        state
            .edge_connection_pool
            .gc_reconnect_lock(&user_id, &edge_agent_id);
        if let Err(error) = finalization_result {
            tracing::warn!(
                target: "astra_runtime::edge_ws",
                user_id = %user_id,
                edge_agent_id = %edge_agent_id,
                %error,
                "edge WebSocket: durable registration finalization failed"
            );
        }
        close_unavailable_edge(&ws_sink, "edge registry unavailable").await;
        return;
    }

    if socket_closed_during_finalization
        || edge_socket_appears_closed(&mut ws_stream, &ws_sink).await
    {
        state
            .edge_connection_pool
            .abort_reconnect(reconnect, &user_id, &edge_agent_id);
        if let Err(error) = edge_registry
            .rollback_registration(&registration_lease)
            .await
        {
            tracing::error!(
                target: "astra_runtime::edge_ws",
                user_id = %user_id,
                edge_agent_id = %edge_agent_id,
                %error,
                "edge WebSocket: failed to roll back finalized dead reconnect"
            );
        }
        drop(reconnect_guard);
        state
            .edge_connection_pool
            .gc_reconnect_lock(&user_id, &edge_agent_id);
        return;
    }

    // The client requires edge_auth_ok to be the first server application
    // frame. Send it before publishing the pool sender so ToolRequest can never
    // overtake authentication while claim release is awaiting the database.
    if send_edge_msg(
        &ws_sink,
        EdgeServerMessage::AuthOk {
            user_id: user_id.clone(),
            edge_id: edge_id_for_registry.clone(),
            interaction_api_major: astra_server_types::AGENT_INTERACTION_API_MAJOR.to_string(),
        },
    )
    .await
    .is_err()
    {
        state
            .edge_connection_pool
            .abort_reconnect(reconnect, &user_id, &edge_agent_id);
        if let Err(error) = edge_registry
            .rollback_registration(&registration_lease)
            .await
        {
            tracing::error!(
                target: "astra_runtime::edge_ws",
                user_id = %user_id,
                edge_agent_id = %edge_agent_id,
                %error,
                "edge WebSocket: failed to roll back after auth_ok send failure"
            );
        }
        drop(reconnect_guard);
        state
            .edge_connection_pool
            .gc_reconnect_lock(&user_id, &edge_agent_id);
        return;
    }

    // ── Phase 2b: Start the forward loop BEFORE publishing the sender ─
    // The forward task drains pool_rx into the socket. Spawning it before the
    // pool commit guarantees the connection is never selectable for dispatch
    // while its forward loop is not yet running, so a message admitted right
    // after commit cannot sit undrained (and be lost on teardown).
    let (pool_tx, mut pool_rx) = mpsc::channel::<EdgeOutbound>(
        astra_server_types::edge_connection_pool::EDGE_WS_CHANNEL_CAPACITY,
    );
    let control_tx = pool_tx.clone();
    let pong_wake = Arc::new(tokio::sync::Notify::new());
    let forward_pong = pong_wake.clone();
    let ws_sink_fwd = ws_sink.clone();
    let forward_pool = state.edge_connection_pool.clone();
    let forward_user = user_id.clone();
    let forward_edge = edge_agent_id.clone();
    let (forward_generation_tx, forward_generation_rx) = tokio::sync::oneshot::channel();
    let mut forward_task = tokio::spawn(async move {
        let Ok(generation) = forward_generation_rx.await else {
            return;
        };
        let Some(cancel_wake) =
            forward_pool.cancellation_wake(&forward_user, &forward_edge, generation)
        else {
            return;
        };
        // Reconnect inspects retained cancellation once. Ordinary frames do
        // not scan every pending invocation on every socket write.
        cancel_wake.notify_one();
        loop {
            let msg = tokio::select! {
                _ = cancel_wake.notified() => {
            for (request_id, delivery_generation) in forward_pool
                .pending_cancellations_for_connection(&forward_user, &forward_edge, generation)
            {
                let sent = send_edge_msg(
                        &ws_sink_fwd,
                        EdgeServerMessage::ToolCancel {
                            request_id: request_id.clone(),
                            delivery_generation,
                        },
                    ).await;
                if sent.is_err() {
                    return;
                }
                forward_pool.cancellation_forwarded(&request_id, delivery_generation, generation);
            }
                    continue;
                },
                _ = forward_pong.notified() => Some(EdgeServerMessage::Pong {}.into()),
                msg = pool_rx.recv() => msg,
            };
            let Some(mut msg) = msg else {
                break;
            };
            let request_delivery = match msg.message() {
                EdgeServerMessage::ToolRequest {
                    request_id,
                    delivery_generation,
                    ..
                } => {
                    if !forward_pool.queued_request_is_live(
                        &forward_user,
                        &forward_edge,
                        generation,
                        request_id,
                        *delivery_generation,
                    ) {
                        continue;
                    }
                    Some((request_id.clone(), *delivery_generation))
                }
                _ => None,
            };
            let direct_timeout = request_delivery
                .as_ref()
                .map(|_| astra_turn_types::PROVIDER_STAGE_INPUT_ACK_TIMEOUT);
            let authority = request_delivery.as_ref().map(|_| {
                (
                    &forward_pool,
                    forward_user.as_str(),
                    forward_edge.as_str(),
                    generation,
                )
            });
            let text = match serde_json::to_string(msg.message()) {
                Ok(text) => text,
                Err(_) => break,
            };
            match send_edge_frame_if(
                &ws_sink_fwd,
                Message::Text(text.into()),
                direct_timeout,
                authority,
                || {
                    let live = request_delivery.as_ref().is_none_or(
                        |(request_id, delivery_generation)| {
                            forward_pool.queued_request_is_live(
                                &forward_user,
                                &forward_edge,
                                generation,
                                request_id,
                                *delivery_generation,
                            )
                        },
                    );
                    if live {
                        msg.mark_attempted();
                    }
                    live
                },
            )
            .await
            {
                Ok(true) => {}
                Ok(false) => continue,
                Err(()) => break,
            }
            if let Some((request_id, delivery_generation)) = request_delivery {
                forward_pool.request_forwarded(
                    &forward_user,
                    &forward_edge,
                    generation,
                    &request_id,
                    delivery_generation,
                );
            }
        }
    });

    // ── Phase 2c: Commit — atomically install the new connection ─────
    // Only after DB success and with the forward loop already running: the new
    // sender becomes selectable inheriting the pending map. Release the reconnect
    // lock immediately afterward so it never spans the connection's lifetime.
    let pool_generation = state
        .edge_connection_pool
        .commit_reconnect_with_registry_and_materialization_id(
            reconnect,
            &user_id,
            &edge_agent_id,
            hostname.clone(),
            workspace_dir.clone(),
            capabilities,
            workspace_id.clone(),
            Some(registration_lease.current.registry_id.clone()),
            registration_lease.current.materialization_id.clone(),
            Some(edge_id_for_registry.clone()),
            pool_tx,
        );
    let _ = forward_generation_tx.send(pool_generation);
    // Publication runs independently of message handlers; cleanup owns and
    // joins its cancellation before unregistering this generation.
    let (mut registration_release, release_task) = if registration_lease.claim_id.is_some() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let release =
            reconcile_edge_registry_release(edge_registry.clone(), registration_lease.clone());
        let task = tokio::spawn(async move {
            let _ = tx.send(release.await);
        });
        (Some(rx), Some(task))
    } else {
        (None, None)
    };
    drop(reconnect_guard);
    // Release the per-key reconnect lock entry now that the reconnect is done.
    state
        .edge_connection_pool
        .gc_reconnect_lock(&user_id, &edge_agent_id);

    let pool_size = state
        .shared_pool
        .as_ref()
        .map_or(5, |pool| pool.get().options().get_max_connections());
    let cleanup_concurrency = pool_size.saturating_sub(1).clamp(1, 4);
    let permit = Arc::new(permit);
    let cleanup = EdgeGenerationCleanupGuard {
        cleanup: Arc::new(EdgeGenerationCleanup {
            started: AtomicBool::new(false),
            registry: state.execution.edge_registry_service.clone(),
            user_id: user_id.clone(),
            edge_agent_id: edge_agent_id.clone(),
            edge_id: edge_id_for_registry.clone(),
            cleanup_permits: 4_u32.div_ceil(cleanup_concurrency),
            ownership_limit: EDGE_REGISTRY_CLEANUP_OWNERS.clone(),
            registration_release: std::sync::Mutex::new(release_task),
            live_permit: Arc::downgrade(&permit),
        }),
        pool: state.edge_connection_pool.clone(),
        generation: pool_generation,
        forward: forward_task.abort_handle(),
        live_permit: permit,
    };

    // ── Phase 2b: Spawn the event-driven durable dispatch relay ───────
    let inflight_dispatches = InflightEdgeDispatchTracker::default();
    let dispatch_user_id = user_id.clone();
    let dispatch_agent_id = edge_agent_id.clone();
    let dispatch_svc = state.execution.edge_dispatch_service.clone();
    // The relay may outlive the socket while it owns database work. It only
    // retains the writer while actually sending a frame.
    let dispatch_sink = Arc::downgrade(&ws_sink);
    let dispatch_inflight = inflight_dispatches.clone();
    let relay_cleanup = Arc::clone(&cleanup.cleanup);
    let relay_pool = state.edge_connection_pool.clone();
    let forward_abort = forward_task.abort_handle();
    let relay_attestation_edge_id = registration_lease
        .current
        .supports_work_attestation()
        .then(|| registration_lease.current.edge_id.clone());
    let relay_materialization_id = registration_lease.current.materialization_id.clone();
    let (dispatch_cancel_tx, mut dispatch_cancel_rx) = tokio::sync::watch::channel(());
    let (relay_failed_tx, mut relay_failed_rx) = tokio::sync::oneshot::channel();
    let mut dispatch_task = Some(tokio::spawn(async move {
        let mut relay_failed_tx = Some(relay_failed_tx);
        let Some(mut dispatch_wakeup) = dispatch_wakeup else {
            // An unconfigured durable service may still support live capacity.
            let _ = dispatch_cancel_rx.changed().await;
            return;
        };
        let mut next_claim = None;
        'dispatch: loop {
            let prepared = next_claim.take();
            if prepared.is_none() {
                tokio::select! {
                    _ = dispatch_cancel_rx.changed() => break,
                    wakeup = dispatch_wakeup.changed() => {
                        if wakeup.is_err() {
                            tracing::error!(
                                user_id = %dispatch_user_id,
                                edge_agent_id = %dispatch_agent_id,
                                "edge dispatch wake subscription closed"
                            );
                            break;
                        }
                    }
                }
            }
            if !dispatch_wakeup.borrow().may_have_work() {
                continue;
            }
            let Some(limits) = dispatch_svc.pending_limits() else {
                break;
            };
            let owns_relay = |cancel: &tokio::sync::watch::Receiver<()>| {
                !cancel.has_changed().unwrap_or(true)
                    && relay_pool
                        .with_current_generation(
                            &dispatch_user_id,
                            &dispatch_agent_id,
                            pool_generation,
                            || (),
                        )
                        .is_some()
            };
            // Capacity waits must observe empty observations without
            // consuming work that arrived after the probe.
            let mut capacity_wakeup = dispatch_wakeup.clone();
            // Own any final unsent receipts before claiming work that can
            // outlive this socket. A saturated backlog leaves the row pending.
            let Some(Ok(relay_owner)) = wait_for_pending_relay(
                limits.relay_owners.clone().acquire_owned(),
                &mut capacity_wakeup,
                &mut dispatch_cancel_rx,
            )
            .await
            else {
                continue;
            };
            let prepared = match prepared {
                Some(prepared) => prepared,
                None => {
                    let Some(Ok(probe_permit)) = wait_for_pending_relay(
                        limits.claim.acquire(),
                        &mut capacity_wakeup,
                        &mut dispatch_cancel_rx,
                    )
                    .await
                    else {
                        continue;
                    };
                    if !owns_relay(&dispatch_cancel_rx) {
                        break;
                    }
                    if !dispatch_wakeup.borrow_and_update().may_have_work() {
                        continue;
                    }
                    // Keep the permit until the submitted query returns.
                    let probe = dispatch_svc
                        .prepare_poll_pending(&dispatch_user_id, &dispatch_agent_id)
                        .await;
                    drop(probe_permit);
                    match probe {
                        Ok(Some(prepared)) => prepared,
                        Ok(None) => continue,
                        Err(error) => {
                            tracing::warn!(%error, user_id = %dispatch_user_id,
                                edge_agent_id = %dispatch_agent_id, "edge dispatch probe failed");
                            continue;
                        }
                    }
                }
            };
            let Some(Ok(capacity)) = wait_for_pending_relay(
                limits.reserve_relay_request(),
                &mut capacity_wakeup,
                &mut dispatch_cancel_rx,
            )
            .await
            else {
                continue;
            };
            let Some(Ok(claim_permit)) = wait_for_pending_relay(
                limits.claim.acquire(),
                &mut capacity_wakeup,
                &mut dispatch_cancel_rx,
            )
            .await
            else {
                continue;
            };
            if !owns_relay(&dispatch_cancel_rx) {
                break;
            }
            if !dispatch_wakeup.borrow().may_have_work() {
                continue;
            }
            let claim = prepared.claim(capacity).await;
            drop(claim_permit);
            match claim {
                Ok((rows, remainder)) if !rows.is_empty() => {
                    next_claim = remainder;
                    let claimed_at = tokio::time::Instant::now();
                    let mut rows: Vec<_> = rows.into_iter().map(Some).collect();
                    let settle = |owner, receipts: Vec<_>| {
                        let dispatch = dispatch_svc.clone();
                        let metrics = state.multi_agent_metrics.clone();
                        let edge = dispatch_agent_id.clone();
                        tokio::spawn(async move {
                            let _owner = owner;
                            let limits = dispatch.pending_limits().expect("relay dispatch limits");
                            settle_unsent_dispatches(
                                dispatch.as_ref(),
                                &metrics,
                                &limits.settlement,
                                &edge,
                                &receipts,
                            )
                            .await;
                        });
                    };
                    let receipt_grace = Duration::from_secs(EDGE_TOOL_RESULT_GRACE_SECS / 2);
                    let mut flush_at = claimed_at;
                    let mut suffix_deadlines = None;
                    let mut receipts = Vec::new();
                    let mut connection_failed = false;
                    for idx in 0..rows.len() {
                        if dispatch_cancel_rx.has_changed().unwrap_or(true) {
                            append_unsent_receipts(
                                &mut receipts,
                                rows[idx..].iter().flatten(),
                                UnsentReason::Disconnected,
                            );
                            connection_failed = true;
                            break;
                        }
                        let Some(mut row) = rows[idx].take() else {
                            continue;
                        };
                        let text = std::mem::take(&mut row.payload_json);
                        let mut msg = match decode_relay_edge_dispatch_payload(&text) {
                            Ok(msg) => msg,
                            Err(error) => {
                                tracing::warn!(
                                    target: "astra_runtime::edge_ws",
                                    user_id = %row.user_id,
                                    edge_agent_id = %row.edge_agent_id,
                                    request_id = %row.request_id,
                                    error = %error,
                                    "Edge dispatch relay failed to decode claimed payload; marking dispatch failed"
                                );
                                append_unsent_receipts(
                                    &mut receipts,
                                    std::iter::once(&row),
                                    UnsentReason::Invalid("the dispatch payload was invalid"),
                                );
                                continue;
                            }
                        };
                        if let Err(reason) = validate_and_strip_attestation_marker(
                            &mut msg,
                            relay_attestation_edge_id.as_deref(),
                            relay_materialization_id.as_deref(),
                        ) {
                            tracing::warn!(
                                target: "astra_runtime::edge_ws",
                                user_id = %row.user_id,
                                edge_agent_id = %row.edge_agent_id,
                                request_id = %row.request_id,
                                reason,
                                "Edge dispatch relay rejected workspace attestation for this socket"
                            );
                            append_unsent_receipts(
                                &mut receipts,
                                std::iter::once(&row),
                                UnsentReason::Invalid(reason),
                            );
                            continue;
                        }
                        let queued = Duration::from_micros(row.pending_wait_us)
                            .saturating_add(claimed_at.elapsed());
                        let deadline = match bind_relay_execution_deadline(&mut msg, queued) {
                            Ok(deadline) if deadline > tokio::time::Instant::now() => deadline,
                            budget => {
                                let reason = match budget {
                                    Ok(_) => UnsentReason::Expired,
                                    Err(reason) => UnsentReason::Invalid(reason),
                                };
                                append_unsent_receipts(
                                    &mut receipts,
                                    std::iter::once(&row),
                                    reason,
                                );
                                continue;
                            }
                        };
                        // The claimed payload is immutable in storage. Send the
                        // same tool arguments with the remaining execution budget.
                        drop(text);
                        let text = serde_json::to_string(&msg)
                            .expect("decoded Edge tool request is serializable");
                        drop(msg);
                        let inflight = InflightEdgeDispatch {
                            identity: row.identity(),
                            edge_agent_id: row.edge_agent_id.clone(),
                        };
                        if let Err(inflight) = dispatch_inflight.track(inflight).await {
                            tracing::warn!(
                                target: "astra_runtime::edge_ws",
                                user_id = %inflight.identity.user_id,
                                edge_agent_id = %inflight.edge_agent_id,
                                request_id = %inflight.identity.request_id,
                                "Edge dispatch relay rejected claimed dispatch because websocket cleanup is closing"
                            );
                            append_unsent_receipts(
                                &mut receipts,
                                std::iter::once(&row).chain(rows[idx + 1..].iter().flatten()),
                                UnsentReason::Disconnected,
                            );
                            connection_failed = true;
                            break;
                        }
                        let send = async {
                            match dispatch_sink.upgrade() {
                                Some(sink) => {
                                    send_relay_frame(
                                        &sink,
                                        &mut dispatch_cancel_rx,
                                        text,
                                        deadline,
                                        (
                                            &relay_pool,
                                            &dispatch_user_id,
                                            &dispatch_agent_id,
                                            pool_generation,
                                        ),
                                    )
                                    .await
                                }
                                None => RelaySend::Failed { attempted: false },
                            }
                        };
                        tokio::pin!(send);
                        let first = poll_fn(|cx| Poll::Ready(send.as_mut().poll(cx))).await;
                        let send = match first {
                            Poll::Ready(result) => result,
                            Poll::Pending => {
                                // Only inspect queued deadlines once a frame actually blocks.
                                // The projection skips tool arguments and is cached for this batch.
                                let deadlines = suffix_deadlines.get_or_insert_with(|| {
                                    rows.iter()
                                        .map(|row| {
                                            row.as_ref().and_then(|row| {
                                                relay_suffix_deadline(row, claimed_at)
                                            })
                                        })
                                        .collect::<Vec<_>>()
                                });
                                loop {
                                    let now = tokio::time::Instant::now();
                                    let next_deadline = expire_unsent_suffix(
                                        &mut rows[idx + 1..],
                                        &deadlines[idx + 1..],
                                        &mut receipts,
                                        now,
                                    );
                                    if next_deadline.is_none() && receipts.is_empty() {
                                        break send.await;
                                    }
                                    let spare_owner = async {
                                        tokio::time::sleep_until(flush_at).await;
                                        limits
                                            .relay_owners
                                            .clone()
                                            .acquire_owned()
                                            .await
                                            .expect("relay settlement semaphore is open")
                                    };
                                    tokio::select! {
                                        biased;
                                        result = &mut send => break result,
                                        owner = spare_owner, if !receipts.is_empty() => {
                                            settle(owner, std::mem::take(&mut receipts));
                                            flush_at = tokio::time::Instant::now() + receipt_grace;
                                        }
                                        _ = tokio::time::sleep_until(next_deadline.unwrap_or(now)), if next_deadline.is_some() => {}
                                    }
                                }
                            }
                        };
                        if matches!(send, RelaySend::Expired) {
                            dispatch_inflight.remove(&row.request_id).await;
                            append_unsent_receipts(
                                &mut receipts,
                                std::iter::once(&row),
                                UnsentReason::Expired,
                            );
                            continue;
                        }
                        if let RelaySend::Failed { attempted } = send {
                            forward_abort.abort();
                            relay_pool.quiesce_generation(
                                &dispatch_user_id,
                                &dispatch_agent_id,
                                pool_generation,
                            );
                            relay_cleanup.start();
                            if let Some(sender) = relay_failed_tx.take() {
                                let _ = sender.send(());
                            }
                            dispatch_inflight.remove(&row.request_id).await;
                            tracing::warn!(
                                target: "astra_runtime::edge_ws",
                                user_id = %row.user_id,
                                edge_agent_id = %row.edge_agent_id,
                                request_id = %row.request_id,
                                attempted,
                                "Edge dispatch connection is unavailable"
                            );
                            // The Edge may already have received and executed
                            // an attempted request. Keep it claimed for result
                            // replay; settle only work not yet sent.
                            append_unsent_receipts(
                                &mut receipts,
                                (!attempted)
                                    .then_some(&row)
                                    .into_iter()
                                    .chain(rows[idx + 1..].iter().flatten()),
                                UnsentReason::Disconnected,
                            );
                            connection_failed = true;
                            break;
                        }
                    }
                    // Only compact receipts are needed if the database is
                    // unavailable; do not retain the claimed payloads.
                    drop(rows);
                    if !receipts.is_empty() {
                        settle(relay_owner, receipts);
                    }
                    if connection_failed {
                        break 'dispatch;
                    }
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(
                    user_id = %dispatch_user_id,
                    edge_agent_id = %dispatch_agent_id,
                    %error,
                    "edge dispatch claim outcome is unavailable; rows may be pending or dispatched"
                ),
            }
        }
    }));

    // ── Phase 3: Bidirectional message loop ──────────────────────────
    // Inference has a bounded control worker. The reader never awaits its DB
    // enrollment, grant, evidence, or custody writes. A queue overflow closes
    // the connection without ACK; the device retains and replays its journal.
    let (inference_tx, inference_task) =
        match astra_turn_types::runner_inference::RunnerInferenceId::new(edge_agent_id.clone()) {
            Ok(runner_id) => {
                let (sender, receiver) = mpsc::channel(super::runner_inference::CONTROL_CAPACITY);
                let connection =
                    astra_services::runner_model_bindings::AuthenticatedRunnerConnection {
                        user_id: user_id.clone(),
                        runner_id,
                        edge_id: edge_id_for_registry.clone(),
                    };
                let task = tokio::spawn(super::runner_inference::run(
                    state.shared_pool.clone(),
                    state.edge_connection_pool.clone(),
                    connection,
                    pool_generation,
                    receiver,
                ));
                (Some(sender), Some(task))
            }
            Err(_) => (None, None),
        };

    let user_id_cleanup = user_id.clone();
    let edge_agent_id_cleanup = edge_agent_id.clone();
    let read_inflight = inflight_dispatches.clone();

    let dispatch_svc = state.execution.edge_dispatch_service.as_ref();
    let mut result_tasks = tokio::task::JoinSet::<EdgeReceiptCompletion>::new();
    // A connection can still make one fresh attempt while retries occupy the
    // shared slots. That attempt is owned by the connection's live permit.
    let spare_result_slot = Arc::new(Semaphore::new(1));
    let retire_results = CancellationToken::new();
    let offer_receipt = |tasks: &mut tokio::task::JoinSet<EdgeReceiptCompletion>,
                         receipt: EdgeSocketReceipt| {
        let owner = (tasks.len() < EDGE_RESULT_SLOTS_PER_SOCKET)
            .then(|| EDGE_RESULT_OWNERS.try_acquire().ok())
            .flatten();
        let spare = if owner.is_none() {
            let Ok(spare) = spare_result_slot.clone().try_acquire_owned() else {
                return Some(receipt);
            };
            Some((spare, cleanup.live_permit.clone(), retire_results.clone()))
        } else {
            None
        };
        let dispatch = state.execution.edge_dispatch_service.clone();
        let pool = state.edge_connection_pool.clone();
        tasks.spawn(async move {
            let _owner = owner;
            let retire = spare.as_ref().map(|(_, _, retire)| retire);
            receipt.persist(dispatch.as_ref(), &pool, retire).await
        });
        None
    };
    let mut last_receipt = None;
    let mut receipt_admission_deadline = tokio::time::Instant::now();
    let acknowledge_result = async |delivery, delivery_generation, request_id: String| {
        if delivery == EdgeDispatchDelivery::Rejected {
            return Ok(());
        }
        read_inflight.remove(&request_id).await;
        let message = if delivery == EdgeDispatchDelivery::Retired {
            EdgeServerMessage::ToolResultRetired {
                request_id,
                delivery_generation,
            }
        } else {
            EdgeServerMessage::ToolResultAck {
                request_id,
                delivery_generation,
            }
        };
        queue_edge_msg(&control_tx, message)
    };

    // Keep one heartbeat future scoped to this socket. Poll it beside reads,
    // so a slow registry cannot hold up tool receipts or disconnect cleanup.
    let read_loop = async {
        let heartbeat = async {
            let cadence = Duration::from_secs(EDGE_HEARTBEAT_INTERVAL_SECS);
            let mut ticks =
                tokio::time::interval_at(tokio::time::Instant::now() + cadence, cadence);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut failures = 0;
            loop {
                ticks.tick().await;
                // Coalesce hints while the writer owns a frame.
                pong_wake.notify_one();
                let result = tokio::time::timeout(
                    EDGE_HEARTBEAT_ATTEMPT_TIMEOUT,
                    edge_registry.heartbeat(
                        &user_id,
                        &edge_agent_id,
                        &edge_id_for_registry,
                        registration_lease.claim_id.as_deref(),
                    ),
                )
                .await
                .unwrap_or_else(|_| {
                    Err(HeartbeatError::StorageFailure("attempt timed out".into()))
                });
                match result {
                    Ok(()) => failures = 0,
                    Err(HeartbeatError::Superseded) => {
                        tracing::info!(
                            target: "astra_runtime::edge_ws",
                            %user_id, %edge_agent_id,
                            "edge WebSocket: connection superseded by newer registration; closing"
                        );
                        return None;
                    }
                    Err(HeartbeatError::StorageFailure(error)) => {
                        failures += 1;
                        tracing::warn!(
                            target: "astra_runtime::edge_ws",
                            %user_id, %edge_agent_id, %error, failures,
                            budget = EDGE_HEARTBEAT_STORAGE_FAILURE_BUDGET,
                            "edge WebSocket: heartbeat DB failure"
                        );
                        if failures >= EDGE_HEARTBEAT_STORAGE_FAILURE_BUDGET {
                            return Some("edge registry heartbeat unavailable");
                        }
                    }
                }
            }
        };
        tokio::pin!(heartbeat);

        loop {
            if let Some(receipt) = last_receipt.take() {
                last_receipt = offer_receipt(&mut result_tasks, receipt);
                if last_receipt.is_some()
                    && tokio::time::Instant::now() >= receipt_admission_deadline
                {
                    tracing::warn!(target: "astra_runtime::edge_ws", %user_id, %edge_agent_id,
                        "Edge WS: retiring socket at receipt capacity");
                    break;
                }
            }
            tokio::select! {
                biased;
                Some(task) = result_tasks.join_next(), if !result_tasks.is_empty() => {
                    let result = match task {
                        Ok(result) => result,
                        Err(error) => {
                            tracing::warn!(target: "astra_runtime::edge_ws", %user_id, %edge_agent_id,
                                %error, "Edge WS: receipt task failed");
                            break;
                        }
                    };
                    match result {
                        Ok((delivery, generation, request_id)) => {
                            if acknowledge_result(delivery, generation, request_id).await.is_err() {
                                break;
                            }
                        }
                        // On exhaustion a reconnect replays any journaled results.
                        Err(error) if error.is_retryable() => break,
                        Err(_) => {}
                    }
                }
                _ = retire_results.cancelled() => break,
                reason = &mut heartbeat => {
                    if let Some(reason) = reason {
                        send_edge_closing(&ws_sink, reason).await;
                    }
                    break;
                }
                released = async {
                    match registration_release.as_mut() {
                        Some(receiver) => receiver.await,
                        None => std::future::pending().await,
                    }
                } => {
                    registration_release = None;
                    let released = match released {
                        Ok(released) => released,
                        Err(error) => {
                            tracing::error!(
                                target: "astra_runtime::edge_ws",
                                user_id = %user_id,
                                edge_agent_id = %edge_agent_id,
                                %error,
                                "edge WebSocket: registration publication task failed"
                            );
                            send_edge_closing(&ws_sink, "edge registry publication unavailable").await;
                            break;
                        }
                    };
                    if released {
                        tracing::info!(
                            target: "astra_runtime::edge_ws",
                            user_id = %user_id,
                            edge_agent_id = %edge_agent_id,
                            "edge WebSocket: reconciled durable registration publication"
                        );
                    } else {
                        tracing::info!(
                            target: "astra_runtime::edge_ws",
                            user_id = %user_id,
                            edge_agent_id = %edge_agent_id,
                            "edge WebSocket: durable registration was superseded during publication reconciliation"
                        );
                        send_edge_closing(&ws_sink, "edge registry registration claim lost").await;
                        break;
                    }
                }
                result = &mut forward_task => {
                    tracing::warn!(user_id = %user_id, edge_agent_id = %edge_agent_id, ?result,
                        "edge socket writer stopped; closing the connection");
                    break;
                }
                result = async {
                    match dispatch_task.as_mut() {
                        Some(task) => task.await,
                        None => std::future::pending().await,
                    }
                } => {
                    dispatch_task = None;
                    tracing::error!(
                        target: "astra_runtime::edge_ws",
                        user_id = %user_id,
                        edge_agent_id = %edge_agent_id,
                        ?result,
                        "edge dispatch relay stopped; closing the connection"
                    );
                    break;
                }
                _ = &mut relay_failed_rx => {
                    tracing::warn!(
                        target: "astra_runtime::edge_ws",
                        user_id = %user_id,
                        edge_agent_id = %edge_agent_id,
                        "edge dispatch relay is closing while its batch settles"
                    );
                    break;
                }
                _ = async { tokio::time::sleep_until(receipt_admission_deadline).await },
                    if last_receipt.is_some() => {}
                msg = ws_stream.next(), if last_receipt.is_none() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            match serde_json::from_str::<EdgeClientMessage>(&text) {
                                Ok(EdgeClientMessage::ToolResult {
                                    request_id,
                                    identity,
                                    delivery_generation,
                                    output,
                                    is_error,
                                    duration_ms,
                                    tool_result_fields,
                                }) => {
                                    if request_id != identity.storage_key() {
                                        tracing::warn!(
                                            target: "astra_runtime::edge_ws",
                                            user_id = %user_id,
                                            edge_agent_id = %edge_agent_id,
                                            request_id = %request_id,
                                            "Edge WS: rejected result with mismatched durable identity"
                                        );
                                        continue;
                                    }
                                    let inflight = read_inflight.get(&request_id).await;
                                    let dispatch_identity =
                                        astra_services::multi_agent::EdgeDispatchIdentity::new(
                                            &user_id,
                                            &identity.session_id,
                                            &identity.run_id,
                                            &identity.turn_chain_id,
                                            &request_id,
                                        );
                                    if inflight.as_ref().is_some_and(|tracked| {
                                        tracked.identity != dispatch_identity
                                            || tracked.edge_agent_id != edge_agent_id
                                    }) {
                                        tracing::warn!(
                                            target: "astra_runtime::edge_ws",
                                            user_id = %user_id,
                                            edge_agent_id = %edge_agent_id,
                                            request_id = %request_id,
                                            "Edge WS: rejected result that conflicts with tracked dispatch identity"
                                        );
                                        continue;
                                    }

                                    // Persist before ACK. The client journal must retain the result
                                    // if this write fails or the connection closes.
                                    let status = if is_error {
                                        "failed".to_string()
                                    } else {
                                        "completed".to_string()
                                    };
                                    let duration = duration_ms.unwrap_or(0);
                                    let tool_result =
                                        astra_thin_client::ToolResultRequest::new_with_hash(
                                            astra_thin_client::ToolResultRequestParts {
                                                session_id: identity.session_id.clone(),
                                                run_id: identity.run_id.clone(),
                                                turn_chain_id: identity.turn_chain_id.clone(),
                                                request_id: request_id.clone(),
                                                edge_agent_id: edge_agent_id.clone(),
                                                status,
                                                output,
                                                duration_ms: duration,
                                                tool_result_fields,
                                            },
                                        );
                                    let receipt = EdgeSocketReceipt {
                                        identity: dispatch_identity,
                                        delivery_generation,
                                        duration_ms,
                                        result: tool_result,
                                    };
                                    last_receipt = offer_receipt(&mut result_tasks, receipt);
                                    if last_receipt.is_some() {
                                        receipt_admission_deadline = tokio::time::Instant::now()
                                            + EDGE_RESULT_CONFIRMATION_BUDGET;
                                    }
                                }
                                Ok(EdgeClientMessage::CapacityResult { request_id, connection_generation, result }) => {
                                    state.edge_connection_pool.complete_capacity_rpc(
                                        &user_id, &edge_agent_id, pool_generation, &request_id,
                                        connection_generation, result,
                                    );
                                }
                                Ok(EdgeClientMessage::ToolInputAck {
                                    request_id,
                                    delivery_generation,
                                    ack,
                                }) => {
                                    if ack.input_id.trim().is_empty()
                                        || ack.input_id != ack.input_id.trim()
                                    {
                                        tracing::warn!(
                                            target: "astra_runtime::edge_ws",
                                            user_id = %user_id,
                                            edge_agent_id = %edge_agent_id,
                                            request_id = %request_id,
                                            "Edge WS: rejected provider input acknowledgement with invalid input identity"
                                        );
                                        continue;
                                    }
                                    let accepted = state
                                        .edge_connection_pool
                                        .deliver_provider_stage_input_ack(
                                            &user_id,
                                            &edge_agent_id,
                                            &request_id,
                                            delivery_generation,
                                            ack,
                                        );
                                    if !accepted {
                                        tracing::debug!(
                                            target: "astra_runtime::edge_ws",
                                            user_id = %user_id,
                                            edge_agent_id = %edge_agent_id,
                                            request_id = %request_id,
                                            delivery_generation,
                                            "Edge WS: provider input acknowledgement had no matching waiter"
                                        );
                                    }
                                }
                                Ok(EdgeClientMessage::ToolInputUnconfirmed {
                                    request_id,
                                    delivery_generation,
                                    input_id,
                                }) => {
                                    if !input_id.is_empty() && input_id == input_id.trim() {
                                        state.edge_connection_pool.mark_provider_stage_input_unconfirmed(
                                            &user_id, &edge_agent_id, &request_id,
                                            delivery_generation, &input_id,
                                        );
                                    }
                                }
                                Ok(EdgeClientMessage::Ping {}) => {
                                    pong_wake.notify_one();
                                }
                                Ok(message @ (EdgeClientMessage::InferenceHello { .. }
                                    | EdgeClientMessage::InferenceBindingPublish { .. }
                                    | EdgeClientMessage::InferenceStartEvidence { .. }
                                    | EdgeClientMessage::InferenceCapacityRetired { .. }
                                    | EdgeClientMessage::InferenceProgress { .. }
                                    | EdgeClientMessage::InferenceTerminal { .. }
                                    | EdgeClientMessage::InferenceResponseChunk { .. }
                                    | EdgeClientMessage::InferenceRequestCredit { .. })) => {
                                    if !state.edge_connection_pool.is_current_inference_connection(&user_id, &edge_agent_id, pool_generation)
                                        || !super::runner_inference::validate_ingress(&message, &user_id, &edge_agent_id, pool_generation) {
                                        break;
                                    }
                                    if let Some(sender) = &inference_tx {
                                        let disposable_progress = matches!(
                                            &message,
                                            EdgeClientMessage::InferenceProgress { .. }
                                        );
                                        if sender.try_send(message).is_err() {
                                            if disposable_progress {
                                                // A full control queue is a normal
                                                // preview gap, not a reason to tear
                                                // down the authenticated socket. The
                                                // terminal transfer has its own
                                                // custody/reconciliation path.
                                                tracing::debug!(
                                                    target: "astra_runtime::edge_ws",
                                                    user_id = %user_id,
                                                    edge_agent_id = %edge_agent_id,
                                                    "dropping Runner progress because the inference queue is full"
                                                );
                                                continue;
                                            }
                                            let _ = queue_edge_msg(&control_tx, EdgeServerMessage::Closing { reason: "inference control capacity unavailable".into() });
                                            break;
                                        }
                                    } else {
                                        let _ = queue_edge_msg(&control_tx, EdgeServerMessage::InferenceHelloAck {
                                            negotiation: astra_turn_types::runner_inference::RunnerInferenceNegotiation::Unavailable {
                                                reason: astra_turn_types::runner_inference::RunnerInferenceRejection::InferenceUnsupported,
                                            },
                                        });
                                    }
                                }
                                Ok(EdgeClientMessage::Auth { .. }) => {
                                    // Already authenticated, ignore duplicate auth
                                }
                                Err(e) => {
                                    let diagnostic = astra_server_types::edge_ws_protocol::EdgeMessageDecodeDiagnostic::from(&e);
                                    tracing::warn!(
                                        user_id = %user_id,
                                        edge_agent_id = %edge_agent_id,
                                        category = diagnostic.category,
                                        line = diagnostic.line,
                                        column = diagnostic.column,
                                        "Edge: invalid message"
                                    );
                                }
                            }
                        }
                        Some(Ok(Message::Close(_))) | None => break,
                        // Tungstenite queues the protocol Pong when it reads Ping.
                        Some(Ok(Message::Ping(_))) => {}
                        _ => {} // ignore binary, pong
                    }
                }
            }
        }
    };

    read_loop.await;
    let last_receipt = last_receipt.map(|receipt| (receipt, cleanup.live_permit.clone()));

    drop(inference_tx);
    if let Some(mut task) = inference_task {
        if tokio::time::timeout(Duration::from_millis(250), &mut task)
            .await
            .is_err()
        {
            task.abort();
        }
    }

    // ── Cleanup ──────────────────────────────────────────────────────
    // Signal the relay before any cleanup can yield, so it cannot start a
    // new send after the reader has stopped.
    drop(dispatch_cancel_tx);
    drop(cleanup);
    drop(ws_stream);
    drop(ws_sink);
    inflight_dispatches.close_to_new_dispatches().await;
    if let Some(mut dispatch_task) = dispatch_task
        && tokio::time::timeout(Duration::from_millis(250), &mut dispatch_task)
            .await
            .is_err()
    {
        tracing::warn!("edge dispatch claim is still settling after disconnect");
        // The relay keeps its ownership slot until it hands off any receipts.
        drop(dispatch_task);
    }
    let disconnected_dispatches = inflight_dispatches.drain().await;
    if !disconnected_dispatches.is_empty() {
        tracing::warn!(
            user_id = %user_id_cleanup,
            edge_agent_id = %edge_agent_id_cleanup,
            count = disconnected_dispatches.len(),
            "Edge disconnected with durable dispatches in flight; preserving them for result replay"
        );
    }
    // Receipt tasks keep ownership of submitted writes after socket cleanup.
    if let Some((receipt, _connection_slot)) = last_receipt {
        let _ = receipt
            .persist(dispatch_svc, &state.edge_connection_pool, None)
            .await;
    }
    while result_tasks.join_next().await.is_some() {}
    tracing::info!(
        user_id = %user_id_cleanup,
        edge_agent_id = %edge_agent_id_cleanup,
        pool_generation,
        "Edge agent disconnected"
    );
}

/// Deliver the accepted receipt or release the caller to observe durable storage.
fn finish_edge_result(
    pool: &EdgeConnectionPool,
    identity: &EdgeDispatchIdentity,
    edge_agent_id: &str,
    delivery_generation: u64,
    duration_ms: Option<u64>,
    delivery: Result<EdgeDispatchDelivery, EdgeDispatchDeliveryError>,
    receipt: impl FnOnce() -> ToolResultRequest,
) -> Result<EdgeDispatchDelivery, EdgeDispatchDeliveryError> {
    let EdgeDispatchIdentity {
        user_id,
        request_id,
        ..
    } = identity;
    if matches!(delivery, Ok(EdgeDispatchDelivery::Accepted)) {
        let ToolResultRequest {
            output,
            status,
            tool_result_fields,
            ..
        } = receipt();
        let direct_accepted = pool.deliver_tool_result(
            user_id,
            edge_agent_id,
            request_id,
            delivery_generation,
            EdgeToolResult {
                output,
                is_error: status == "failed",
                duration_ms,
                tool_result_fields,
            },
        );
        tracing::debug!(target: "astra_runtime::edge_ws", %user_id, %edge_agent_id, %request_id,
            direct_accepted, "Edge WS: result durably accepted");
    } else {
        pool.recover_tool_result(user_id, edge_agent_id, request_id, delivery_generation);
        match &delivery {
            Ok(EdgeDispatchDelivery::Rejected) => tracing::warn!(
                target: "astra_runtime::edge_ws", %user_id, %edge_agent_id, %request_id,
                "Edge WS: dispatch rejected the tool result"
            ),
            Err(error) => {
                tracing::warn!(target: "astra_runtime::edge_ws", %user_id, %edge_agent_id, %request_id, %error,
                    "Edge WS: could not confirm tool receipt persistence"
                )
            }
            _ => {}
        }
    }
    delivery
}

type EdgeReceiptCompletion = Result<(EdgeDispatchDelivery, u64, String), EdgeDispatchDeliveryError>;

struct EdgeSocketReceipt {
    identity: EdgeDispatchIdentity,
    delivery_generation: u64,
    duration_ms: Option<u64>,
    result: ToolResultRequest,
}

impl EdgeSocketReceipt {
    async fn persist(
        self,
        dispatch: &dyn EdgeDispatchService,
        pool: &EdgeConnectionPool,
        retire_on_retry: Option<&CancellationToken>,
    ) -> EdgeReceiptCompletion {
        let Self {
            identity,
            delivery_generation,
            duration_ms,
            result,
        } = self;
        let edge_agent_id = result.edge_agent_id.clone();
        let result_json = serde_json::to_string(&result).expect("typed Edge tool result");
        let mut result = Some(result);
        let write = async {
            let mut delivery = dispatch
                .deliver_socket_result(&identity, &edge_agent_id, &result_json)
                .await;
            // Keep one owner for the exact bytes from the first write until
            // the final retry, including receipts the Edge could not journal.
            for delay_ms in [100, 200, 2_000] {
                if !delivery.as_ref().is_err_and(|error| error.is_retryable()) {
                    break;
                }
                if let Some(retire) = retire_on_retry {
                    retire.cancel();
                }
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                let _permit = match dispatch.pending_limits() {
                    Some(limits) => Some(
                        limits
                            .settlement
                            .acquire()
                            .await
                            .map_err(|_| "Edge settlement is closed")?,
                    ),
                    None => None,
                };
                delivery = dispatch
                    .deliver_socket_result(&identity, &edge_agent_id, &result_json)
                    .await;
            }
            delivery
        };
        tokio::pin!(write);
        let delivery = tokio::select! {
            biased;
            delivery = &mut write => delivery,
            _ = tokio::time::sleep(EDGE_RESULT_CONFIRMATION_BUDGET) => {
                // Observe potentially committed evidence while the worker
                // continues owning the submitted write and its retries.
                result = None;
                pool.observe_tool_result(
                    &identity.user_id, &edge_agent_id, &identity.request_id, delivery_generation,
                );
                write.await
            }
        };
        finish_edge_result(
            pool,
            &identity,
            &edge_agent_id,
            delivery_generation,
            duration_ms,
            delivery,
            || {
                result.unwrap_or_else(|| {
                    serde_json::from_str(&result_json).expect("serialized Edge tool result")
                })
            },
        )
        .map(|delivery| (delivery, delivery_generation, identity.request_id.clone()))
    }
}

#[derive(Debug, Clone)]
struct InflightEdgeDispatch {
    identity: astra_services::multi_agent::EdgeDispatchIdentity,
    edge_agent_id: String,
}

#[derive(Clone)]
struct InflightEdgeDispatchTracker {
    inner: Arc<tokio::sync::Mutex<InflightEdgeDispatchState>>,
}

#[derive(Default)]
struct InflightEdgeDispatchState {
    closing: bool,
    dispatches: HashMap<String, InflightEdgeDispatch>,
}

impl Default for InflightEdgeDispatchTracker {
    fn default() -> Self {
        Self {
            inner: Arc::new(tokio::sync::Mutex::new(InflightEdgeDispatchState::default())),
        }
    }
}

impl InflightEdgeDispatchTracker {
    async fn track(&self, dispatch: InflightEdgeDispatch) -> Result<(), InflightEdgeDispatch> {
        let mut state = self.inner.lock().await;
        if state.closing {
            return Err(dispatch);
        }
        state
            .dispatches
            .insert(dispatch.identity.request_id.clone(), dispatch);
        Ok(())
    }

    async fn remove(&self, request_id: &str) -> Option<InflightEdgeDispatch> {
        self.inner.lock().await.dispatches.remove(request_id)
    }

    async fn get(&self, request_id: &str) -> Option<InflightEdgeDispatch> {
        self.inner.lock().await.dispatches.get(request_id).cloned()
    }

    async fn close_to_new_dispatches(&self) {
        self.inner.lock().await.closing = true;
    }

    async fn drain(&self) -> Vec<InflightEdgeDispatch> {
        self.inner
            .lock()
            .await
            .dispatches
            .drain()
            .map(|(_, dispatch)| dispatch)
            .collect()
    }
}

/// Best-effort, non-blocking check for a socket close/error already surfaced on
/// the read stream. Used during reconnect setup so a connection that has already
/// disconnected does not commit into the pool and evict a healthy peer.
///
/// Process a frame received while the edge handshake is still being installed.
/// Ping/Pong are legal at any point in a WebSocket session. Every application
/// frame, Close, stream error, or EOF ends setup before the connection is
/// published into the pool.
async fn handle_edge_setup_frame(
    frame: Option<Result<Message, edge_websocket::tungstenite::Error>>,
    ws_sink: &SharedEdgeSocketWriter,
) -> bool {
    match frame {
        Some(Ok(Message::Ping(_))) => {
            // tungstenite queues the automatic Pong while reading the Ping;
            // flush the split sink so a long DB registration cannot delay it.
            use futures_util::SinkExt;
            let mut guard = ws_sink.sink.lock().await;
            let Some(mut sink) = guard.take() else {
                return false;
            };
            let ok = sink.flush().await.is_ok() && !ws_sink.closed.load(Ordering::Acquire);
            if ok {
                *guard = Some(sink);
            }
            ok
        }
        Some(Ok(Message::Pong(_))) => true,
        _ => false,
    }
}

/// Drain every control frame that is already ready and report whether setup has
/// observed a close/error. Draining prevents a queued `Ping -> Close` sequence
/// from publishing a dead connection.
async fn edge_socket_appears_closed(
    ws_stream: &mut SplitStream<EdgeWebSocket>,
    ws_sink: &SharedEdgeSocketWriter,
) -> bool {
    use futures_util::future::FutureExt;
    loop {
        match ws_stream.next().now_or_never() {
            Some(frame) => {
                if !handle_edge_setup_frame(frame, ws_sink).await {
                    return true;
                }
            }
            None => return false,
        }
    }
}

fn decode_relay_edge_dispatch_payload(
    payload_json: &str,
) -> Result<EdgeServerMessage, serde_json::Error> {
    serde_json::from_str(payload_json)
}

/// Work attestation binds the tool arguments to an authenticated socket and
/// checkout. Strip the server-only marker before forwarding the arguments.
fn validate_and_strip_attestation_marker(
    message: &mut EdgeServerMessage,
    attestation_edge_id: Option<&str>,
    materialization_id: Option<&str>,
) -> Result<(), &'static str> {
    let EdgeServerMessage::ToolRequest { tool, args, .. } = message else {
        return Ok(());
    };
    let Some(marker) = args
        .as_object_mut()
        .and_then(|args| args.remove("__astra_attestation"))
    else {
        return Ok(());
    };
    if tool != WORK_ATTESTATION_TOOL {
        return Err("edge_attestation_tool_mismatch");
    }
    let (Some(edge_id), Some(materialization_id)) = (attestation_edge_id, materialization_id)
    else {
        return Err("edge_attestation_tool_unavailable");
    };
    let Some(marker) = marker.as_object() else {
        return Err("edge_attestation_marker_invalid");
    };
    if marker.get("edge_id").and_then(serde_json::Value::as_str) != Some(edge_id) {
        return Err("edge_attestation_connection_changed");
    }
    if marker
        .get("materialization_id")
        .and_then(serde_json::Value::as_str)
        != Some(materialization_id)
    {
        return Err("edge_attestation_materialization_mismatch");
    }
    Ok(())
}

/// Helper: serialize and send an EdgeServerMessage over the WebSocket.
fn queue_edge_msg(sender: &mpsc::Sender<EdgeOutbound>, msg: EdgeServerMessage) -> Result<(), ()> {
    sender.try_send(msg.into()).map_err(|_| ())
}

async fn send_edge_closing(sink: &SharedEdgeSocketWriter, reason: &'static str) {
    let _ = tokio::time::timeout(
        Duration::from_millis(250),
        send_edge_msg(
            sink,
            EdgeServerMessage::Closing {
                reason: reason.into(),
            },
        ),
    )
    .await;
}

async fn send_edge_msg(sink: &SharedEdgeSocketWriter, msg: EdgeServerMessage) -> Result<(), ()> {
    let text = serde_json::to_string(&msg).map_err(|_| ())?;
    send_edge_frame(sink, Message::Text(text.into())).await
}

async fn send_edge_frame(sink: &SharedEdgeSocketWriter, message: Message) -> Result<(), ()> {
    send_edge_frame_if(sink, message, None, None, || true)
        .await
        .map(|_| ())
}

async fn send_edge_frame_if(
    sink: &SharedEdgeSocketWriter,
    message: Message,
    send_timeout: Option<Duration>,
    authority: Option<SocketAuthority<'_>>,
    should_send: impl FnOnce() -> bool,
) -> Result<bool, ()> {
    use futures_util::SinkExt;
    let mut guard = sink.sink.lock().await;
    if sink.closed.load(Ordering::Acquire) {
        return Err(());
    }
    // A cancelled send drops this owned sink instead of returning a partially
    // flushed frame to another writer.
    let mut writer = guard.take().ok_or(())?;
    let sent = {
        let send = writer.send(message);
        tokio::pin!(send);
        let mut first = Some(should_send);
        let poll = std::future::poll_fn(|cx| {
            let result = if let Some(check) = first.take() {
                let first_poll = || check().then(|| send.as_mut().poll(cx));
                match authority {
                    Some((pool, user, edge, generation)) => pool
                        .with_current_generation(user, edge, generation, first_poll)
                        .flatten(),
                    None => first_poll(),
                }
            } else {
                Some(send.as_mut().poll(cx))
            };
            match result {
                None => std::task::Poll::Ready(Ok(false)),
                Some(std::task::Poll::Pending) => std::task::Poll::Pending,
                Some(std::task::Poll::Ready(result)) => {
                    std::task::Poll::Ready(result.map(|_| true).map_err(|_| ()))
                }
            }
        });
        match send_timeout {
            Some(timeout) => tokio::time::timeout(timeout, poll).await.map_err(|_| ())?,
            None => poll.await,
        }
    }?;
    if sink.closed.load(Ordering::Acquire) {
        return Err(());
    }
    *guard = Some(writer);
    Ok(sent)
}

async fn wait_for_pending_relay<F: std::future::Future>(
    ready: F,
    wake: &mut tokio::sync::watch::Receiver<
        astra_services::multi_agent::edge_dispatch::EdgeDispatchWake,
    >,
    cancel: &mut tokio::sync::watch::Receiver<()>,
) -> Option<F::Output> {
    tokio::pin!(ready);
    loop {
        tokio::select! {
            biased;
            _ = cancel.changed() => return None,
            changed = wake.changed() => {
                if changed.is_err() || !wake.borrow_and_update().may_have_work() {
                    return None;
                }
            }
            value = &mut ready => return Some(value),
        }
    }
}

#[derive(serde::Deserialize)]
struct QueuedRelayBudget<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    timeout_secs: Option<u64>,
    execution_deadline_unix_ms: Option<u64>,
    execution_timeout_ms: Option<u64>,
}

fn relay_suffix_deadline(
    row: &EdgeDispatchRow,
    claimed_at: tokio::time::Instant,
) -> Option<tokio::time::Instant> {
    let budget: QueuedRelayBudget<'_> = serde_json::from_str(&row.payload_json).ok()?;
    if budget.kind != "edge_tool_request" {
        return None;
    }
    relay_delivery_deadline(
        budget.timeout_secs,
        budget.execution_deadline_unix_ms,
        budget.execution_timeout_ms,
        Duration::from_micros(row.pending_wait_us).saturating_add(claimed_at.elapsed()),
    )
    .ok()
}

fn expire_unsent_suffix(
    rows: &mut [Option<EdgeDispatchRow>],
    deadlines: &[Option<tokio::time::Instant>],
    receipts: &mut Vec<(EdgeDispatchIdentity, String)>,
    now: tokio::time::Instant,
) -> Option<tokio::time::Instant> {
    rows.iter_mut()
        .zip(deadlines)
        .filter_map(|(row, at)| {
            let at = at.filter(|_| row.is_some())?;
            if at > now {
                return Some(at);
            }
            append_unsent_receipts(receipts, row.take().iter(), UnsentReason::Expired);
            None
        })
        .min()
}

fn relay_delivery_deadline(
    timeout_secs: Option<u64>,
    execution_deadline_unix_ms: Option<u64>,
    execution_timeout_ms: Option<u64>,
    queued: Duration,
) -> Result<tokio::time::Instant, &'static str> {
    let remaining_ms = execution_timeout_ms.map(|ms| {
        u64::try_from(Duration::from_millis(ms).saturating_sub(queued).as_millis())
            .unwrap_or(u64::MAX)
    });
    let now = tokio::time::Instant::now();
    let admitted = match admitted_work_deadline(execution_deadline_unix_ms, remaining_ms) {
        Err(ADMITTED_EXECUTION_DEADLINE_EXPIRED) => return Ok(now),
        result => result?,
    };
    let timed = timeout_secs
        .map(|seconds| {
            now.checked_add(Duration::from_secs(seconds).saturating_sub(queued))
                .ok_or("relay timeout is outside clock range")
        })
        .transpose()?;
    admitted
        .into_iter()
        .chain(timed)
        .min()
        .ok_or("missing relay delivery budget")
}

fn bind_relay_execution_deadline(
    msg: &mut EdgeServerMessage,
    queued: Duration,
) -> Result<tokio::time::Instant, &'static str> {
    let EdgeServerMessage::ToolRequest {
        timeout_secs,
        execution_deadline_unix_ms,
        execution_timeout_ms,
        ..
    } = msg
    else {
        return Err("relay payload is not a tool request");
    };
    let wall_now = u64::try_from(chrono::Utc::now().timestamp_millis())
        .map_err(|_| "relay deadline is outside clock range")?;
    let deadline = relay_delivery_deadline(
        *timeout_secs,
        *execution_deadline_unix_ms,
        *execution_timeout_ms,
        queued,
    )?;
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    let remaining_ms =
        u64::try_from(remaining.as_millis()).map_err(|_| "relay timeout is outside clock range")?;
    if remaining_ms == 0 {
        return Ok(tokio::time::Instant::now());
    }
    let wall_deadline = wall_now
        .checked_add(remaining_ms)
        .ok_or("relay deadline is outside clock range")?;
    // Delivery and execution share one cutoff, even for command-only
    // requests. Buffered transport bytes must not renew the allowance.
    *execution_deadline_unix_ms =
        Some(wall_deadline.min(execution_deadline_unix_ms.unwrap_or(wall_deadline)));
    *execution_timeout_ms = Some(remaining_ms);
    Ok(deadline)
}

enum RelaySend {
    Sent,
    Expired,
    Failed { attempted: bool },
}

/// Once polled, cancellation or an error cannot prove non-execution.
async fn send_relay_frame(
    sink: &SharedEdgeSocketWriter,
    cancel: &mut tokio::sync::watch::Receiver<()>,
    text: String,
    deadline: tokio::time::Instant,
    authority: SocketAuthority<'_>,
) -> RelaySend {
    use futures_util::SinkExt;
    let expired = || tokio::time::Instant::now() >= deadline;
    // The same request deadline governs queueing, the writer lock and flush.
    let stop = async {
        tokio::select! {
            _ = sink.write_progress.wait_until_idle(RELAY_WRITE_IDLE_LIMIT) => {}
            _ = tokio::time::sleep_until(deadline) => {}
        }
    };
    tokio::pin!(stop);
    let guard = tokio::select! {
        biased;
        _ = cancel.changed() => None,
        _ = &mut stop => None,
        guard = sink.sink.lock() => Some(guard),
    };
    let Some(mut guard) = guard else {
        if expired() {
            return RelaySend::Expired;
        }
        sink.closed.store(true, Ordering::Release);
        return RelaySend::Failed { attempted: false };
    };
    if sink.closed.load(Ordering::Acquire) {
        return RelaySend::Failed { attempted: false };
    }
    let Some(mut writer) = guard.take() else {
        sink.closed.store(true, Ordering::Release);
        return RelaySend::Failed { attempted: false };
    };
    let (attempted, sent) = poll_relay_send(
        writer.send(Message::Text(text.into())),
        cancel,
        Some(authority),
        deadline,
        stop,
    )
    .await;
    if !sink.closed.load(Ordering::Acquire) {
        if sent {
            *guard = Some(writer);
            return RelaySend::Sent;
        }
        if !attempted && expired() {
            *guard = Some(writer);
            return RelaySend::Expired;
        }
    }
    // Prevent another writer flushing the frame with uncertain delivery.
    sink.closed.store(true, Ordering::Release);
    RelaySend::Failed { attempted }
}

async fn poll_relay_send<F, E>(
    send: F,
    cancel: &mut tokio::sync::watch::Receiver<()>,
    authority: Option<SocketAuthority<'_>>,
    deadline: tokio::time::Instant,
    idle: impl Future<Output = ()>,
) -> (bool, bool)
where
    F: Future<Output = Result<(), E>>,
{
    tokio::pin!(send, idle);
    let mut attempted = false;
    let mut first = true;
    let sent = tokio::select! {
        biased;
        _ = cancel.changed() => false,
        _ = &mut idle => false,
        result = std::future::poll_fn(|cx| {
            let mut poll = || {
                if tokio::time::Instant::now() >= deadline {
                    return std::task::Poll::Ready(false);
                }
                attempted = true;
                send.as_mut().poll(cx).map(|result| result.is_ok())
            };
            let outcome = if first {
                first = false;
                match authority {
                    Some((pool, user, edge, generation)) => {
                        pool.with_current_generation(user, edge, generation, poll)
                    }
                    None => Some(poll()),
                }
            } else {
                Some(poll())
            };
            outcome.unwrap_or(std::task::Poll::Ready(false))
        }) => result,
    };
    (attempted, sent)
}

async fn close_unavailable_edge(sink: &SharedEdgeSocketWriter, reason: &'static str) {
    // A service dependency is unavailable, not an authoritative credential
    // rejection. Let the existing Edge reconnect lifecycle retry.
    let _ = send_edge_frame(
        sink,
        Message::Close(Some(CloseFrame {
            code: CloseCode::Again,
            reason: reason.into(),
        })),
    )
    .await;
}

#[cfg(test)]
mod inflight_dispatch_tracker_tests {
    #[tokio::test]
    #[ignore = "requires isolated MatrixOne and ASTRA_TEST_DB_IT=1"]
    async fn matrixone_observer_withdraws_cancelled_relay_waiters() {
        use astra_services::multi_agent::edge_dispatch::{
            EDGE_DISPATCH_CANCELLED_REASON, RELAY_PAYLOAD_BUDGET_KIB,
        };
        use astra_services::multi_agent::{
            DatabaseEdgeDispatchService, EdgeDispatchIdentity, EdgeDispatchService,
        };

        assert_eq!(std::env::var("ASTRA_TEST_DB_IT").as_deref(), Ok("1"));
        let settings = astra_core::MatrixOneSettings::from_env();
        astra_services::storage::ensure_core_schema(&settings, "mysql")
            .await
            .unwrap();
        let pool = astra_core::SharedPool::new(&settings).await.unwrap();
        let owner = DatabaseEdgeDispatchService::from_shared(&pool);
        let remote = DatabaseEdgeDispatchService::from_shared(&pool);
        let user = format!("cancelled-relay-{}", uuid::Uuid::new_v4());
        let limits = owner.pending_limits().unwrap();
        let request_kib = limits.reserve_relay_request().await.unwrap().num_permits();
        let mut occupied = limits
            .acquire_relay_payload(RELAY_PAYLOAD_BUDGET_KIB)
            .await
            .unwrap();
        let mut rows = Vec::new();
        for i in 0..9 {
            let edge = format!("edge-{i}");
            let identity = EdgeDispatchIdentity::new(&user, "session", "run", "chain", &edge);
            let mut wake = owner
                .subscribe_pending_wakeup(&user, &edge)
                .await
                .unwrap()
                .unwrap();
            remote
                .admit_dispatch(&identity, &edge, &serde_json::json!({"tool":"bash"}))
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    wake.changed().await.unwrap();
                    if wake.borrow_and_update().may_have_work() {
                        break;
                    }
                }
            })
            .await
            .unwrap();
            rows.push((identity, edge, wake));
        }

        let (healthy, _, mut healthy_wake) = rows.pop().unwrap();
        let prepared = owner
            .prepare_poll_pending(&user, &healthy.request_id)
            .await
            .unwrap()
            .unwrap();
        let mut waiters = Vec::new();
        let (_cancel, mut live) = tokio::sync::watch::channel(());
        let cancelled_rows = rows
            .iter()
            .map(|(identity, edge, _)| (identity.clone(), edge.clone()))
            .collect::<Vec<_>>();
        for (_, _, wake) in &mut rows {
            let mut cancelled = live.clone();
            let mut waiter = Box::pin(async move {
                super::wait_for_pending_relay(limits.reserve_relay_request(), wake, &mut cancelled)
                    .await
            });
            std::future::poll_fn(|cx| {
                assert!(std::future::Future::poll(waiter.as_mut(), cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            waiters.push(waiter);
        }
        let mut next = Box::pin(super::wait_for_pending_relay(
            limits.reserve_relay_request(),
            &mut healthy_wake,
            &mut live,
        ));
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(next.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        for (identity, edge) in &cancelled_rows {
            assert!(
                remote
                    .fail_dispatch(identity, edge, EDGE_DISPATCH_CANCELLED_REASON)
                    .await
                    .unwrap()
            );
        }
        for waiter in waiters {
            assert!(
                tokio::time::timeout(Duration::from_secs(5), waiter)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        drop(occupied.split(request_kib));
        let slot = tokio::time::timeout(Duration::from_secs(5), next)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(prepared.claim(slot).await.unwrap().0[0].identity(), healthy);
        sqlx::query("DELETE FROM edge_pending_dispatch WHERE user_id = ?")
            .bind(&user)
            .execute(pool.get())
            .await
            .unwrap();
    }

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn unsent_settlement_reports_retry_exhaustion() {
        let metrics = astra_services::multi_agent::shared_metrics();
        let dispatch = astra_services::multi_agent::UnconfiguredEdgeDispatchService;
        let receipts = [
            (
                astra_services::multi_agent::EdgeDispatchIdentity::new("u", "s", "r", "t", "1"),
                "receipt".to_string(),
            ),
            (
                astra_services::multi_agent::EdgeDispatchIdentity::new("u", "s", "r", "t", "2"),
                "receipt".to_string(),
            ),
        ];
        settle_unsent_dispatches(
            &dispatch,
            &metrics,
            &tokio::sync::Semaphore::new(1),
            "edge",
            &receipts,
        )
        .await;
        assert_eq!(
            metrics
                .dispatch_settlement_unknown_total
                .load(Ordering::Relaxed),
            2
        );
    }

    #[tokio::test]
    async fn cleanup_waits_for_bounded_ownership_before_releasing_connection_slot() {
        let owners = Arc::new(tokio::sync::Semaphore::new(1));
        let occupied = owners.acquire().await.unwrap();
        let live = Arc::new(try_acquire_edge_ws_connection().unwrap());
        let weak = Arc::downgrade(&live);
        let forward = tokio::spawn(std::future::pending::<()>());
        let cleanup = EdgeGenerationCleanupGuard {
            cleanup: Arc::new(EdgeGenerationCleanup {
                started: AtomicBool::new(false),
                registry: Arc::new(astra_services::multi_agent::UnconfiguredEdgeRegistryService),
                user_id: "user".into(),
                edge_agent_id: "edge".into(),
                edge_id: "generation".into(),
                cleanup_permits: 1,
                ownership_limit: owners.clone(),
                registration_release: std::sync::Mutex::new(None),
                live_permit: Arc::downgrade(&live),
            }),
            pool: astra_server_types::edge_connection_pool::EdgeConnectionPool::new(),
            generation: 1,
            forward: forward.abort_handle(),
            live_permit: live,
        };
        drop(cleanup);
        tokio::task::yield_now().await;
        assert!(
            weak.upgrade().is_some(),
            "waiting cleanup must retain its connection slot"
        );
        drop(occupied);
        tokio::time::timeout(Duration::from_secs(2), async {
            while weak.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("ownership transfer must release the closed connection slot");
    }

    #[tokio::test]
    async fn relay_distinguishes_cancellation_before_and_after_the_first_send_poll() {
        let deadline = tokio::time::Instant::now() + RELAY_WRITE_IDLE_LIMIT;
        let (cancel, mut receiver) = tokio::sync::watch::channel(());
        drop(cancel);
        let never_polled = std::future::poll_fn(|_| -> std::task::Poll<Result<(), ()>> {
            panic!("cancelled send must not be polled")
        });
        assert_eq!(
            poll_relay_send(
                never_polled,
                &mut receiver,
                None,
                deadline,
                tokio::time::sleep_until(deadline)
            )
            .await,
            (false, false)
        );

        let (cancel, mut receiver) = tokio::sync::watch::channel(());
        let polled = Arc::new(tokio::sync::Notify::new());
        let notify = polled.clone();
        let pending = std::future::poll_fn(move |_| -> std::task::Poll<Result<(), ()>> {
            notify.notify_one();
            std::task::Poll::Pending
        });
        let send = tokio::spawn(async move {
            poll_relay_send(
                pending,
                &mut receiver,
                None,
                deadline,
                tokio::time::sleep_until(deadline),
            )
            .await
        });
        polled.notified().await;
        drop(cancel);
        assert_eq!(send.await.unwrap(), (true, false));
    }

    #[tokio::test(start_paused = true)]
    async fn relay_write_deadline_retains_first_poll_certainty() {
        let (_cancel, mut receiver) = tokio::sync::watch::channel(());
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        let never_polled = std::future::poll_fn(|_| -> std::task::Poll<Result<(), ()>> {
            panic!("expired send must not be polled")
        });
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(
            poll_relay_send(
                never_polled,
                &mut receiver,
                None,
                deadline,
                std::future::pending()
            )
            .await,
            (false, false)
        );

        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        assert_eq!(
            poll_relay_send(
                std::future::pending::<Result<(), ()>>(),
                &mut receiver,
                None,
                deadline,
                tokio::time::sleep_until(deadline),
            )
            .await,
            (true, false)
        );
    }

    #[tokio::test]
    async fn relay_checks_generation_only_at_the_first_poll() {
        let deadline = tokio::time::Instant::now() + RELAY_WRITE_IDLE_LIMIT;
        let pool = astra_server_types::edge_connection_pool::EdgeConnectionPool::new();
        let (old_tx, _old_rx) = mpsc::channel(1);
        let old = pool.register("user", "edge", None, None, old_tx);
        let (cancel, mut receiver) = tokio::sync::watch::channel(());
        let mut in_progress = Box::pin(poll_relay_send(
            std::future::pending::<Result<(), ()>>(),
            &mut receiver,
            Some((&pool, "user", "edge", old)),
            deadline,
            tokio::time::sleep_until(deadline),
        ));
        std::future::poll_fn(|cx| {
            assert!(in_progress.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        let (new_tx, _new_rx) = mpsc::channel(1);
        pool.register("user", "edge", None, None, new_tx);
        drop(cancel);
        assert_eq!(in_progress.await, (true, false));

        let (_cancel, mut receiver) = tokio::sync::watch::channel(());
        assert_eq!(
            poll_relay_send(
                std::future::pending::<Result<(), ()>>(),
                &mut receiver,
                Some((&pool, "user", "edge", old)),
                deadline,
                tokio::time::sleep_until(deadline),
            )
            .await,
            (false, false)
        );
    }

    fn dispatch(request_id: &str) -> InflightEdgeDispatch {
        InflightEdgeDispatch {
            identity: astra_services::multi_agent::EdgeDispatchIdentity::new(
                "user-a",
                "session-a",
                "run-a",
                "chain-a",
                request_id,
            ),
            edge_agent_id: "edge-a".to_string(),
        }
    }

    #[tokio::test]
    async fn tracker_close_drains_existing_and_rejects_late_dispatches() {
        let tracker = InflightEdgeDispatchTracker::default();
        assert!(tracker.track(dispatch("req-1")).await.is_ok());

        tracker.close_to_new_dispatches().await;

        let rejected = tracker.track(dispatch("req-2")).await.unwrap_err();
        assert_eq!(rejected.identity.request_id, "req-2");

        let drained = tracker.drain().await;
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].identity.request_id, "req-1");
        assert!(tracker.drain().await.is_empty());
    }

    #[tokio::test]
    async fn tracker_remove_is_idempotent_before_and_after_close() {
        let tracker = InflightEdgeDispatchTracker::default();
        assert!(tracker.track(dispatch("req-1")).await.is_ok());

        assert_eq!(
            tracker
                .remove("req-1")
                .await
                .map(|dispatch| dispatch.identity.request_id),
            Some("req-1".to_string())
        );
        assert!(tracker.remove("req-1").await.is_none());

        tracker.close_to_new_dispatches().await;
        assert!(tracker.remove("req-2").await.is_none());
    }

    #[tokio::test]
    async fn tracker_inspection_does_not_consume_authenticated_dispatch() {
        let tracker = InflightEdgeDispatchTracker::default();
        assert!(tracker.track(dispatch("req-1")).await.is_ok());

        let observed = tracker.get("req-1").await.expect("tracked dispatch");
        assert_eq!(observed.identity.request_id, "req-1");
        assert!(
            tracker.get("req-1").await.is_some(),
            "rejecting an invalid result must leave the real invocation live"
        );
        assert!(tracker.remove("req-1").await.is_some());
    }
}

#[derive(Clone, Copy)]
enum UnsentReason<'a> {
    Expired,
    Disconnected,
    Invalid(&'a str),
}

fn append_unsent_receipts<'a>(
    receipts: &mut Vec<(EdgeDispatchIdentity, String)>,
    rows: impl IntoIterator<Item = &'a EdgeDispatchRow>,
    reason: UnsentReason<'_>,
) {
    receipts.extend(rows.into_iter().map(|row| {
        let (cause, guidance, retryable) = match reason {
            UnsentReason::Expired => (
                "its delivery budget expired",
                "Execution through another delivery path has not been ruled out; inspect its effects before retrying.",
                false,
            ),
            UnsentReason::Disconnected => (
                "the Edge disconnected",
                "Retry with a new tool call if the Edge is ready.",
                true,
            ),
            UnsentReason::Invalid(cause) => (
                cause,
                "Check the request and Edge binding before retrying.",
                false,
            ),
        };
        let result = astra_tools::ToolResult::error(format!(
            "Tool request was not sent because {cause}. {guidance}"
        ));
        let expired = matches!(reason, UnsentReason::Expired);
        let mut result = if expired {
            result.with_execution_outcome_unknown()
        } else {
            result.with_execution_not_started()
        };
        let fields = result.metadata.get_or_insert_with(serde_json::Map::new);
        fields.insert("retryable".into(), retryable.into());
        if expired {
            fields.insert("dispatch_fact".into(), "expired_before_send".into());
            fields.insert("error_kind".into(), "tool_timeout".into());
            fields.insert("next_action".into(), "inspect_effects_before_retry".into());
        } else {
            fields.insert("side_effects_maybe".into(), false.into());
        }
        let receipt = astra_thin_client::ToolResultRequest::new_with_hash(
            astra_thin_client::ToolResultRequestParts {
                session_id: row.session_id.clone(),
                run_id: row.run_id.clone(),
                turn_chain_id: row.turn_chain_id.clone(),
                request_id: row.request_id.clone(),
                edge_agent_id: row.edge_agent_id.clone(),
                status: if expired { "failed" } else { "rejected" }.into(),
                output: result.output,
                duration_ms: 0,
                tool_result_fields: result.metadata,
            },
        );
        (
            row.identity(),
            serde_json::to_string(&receipt).expect("typed Edge receipt is serializable"),
        )
    }));
}

async fn settle_unsent_dispatches(
    dispatch: &dyn astra_services::multi_agent::EdgeDispatchService,
    metrics: &astra_services::multi_agent::SharedMultiAgentMetrics,
    limit: &tokio::sync::Semaphore,
    edge_agent_id: &str,
    receipts: &[(astra_services::multi_agent::EdgeDispatchIdentity, String)],
) {
    let Some((first, _)) = receipts.first() else {
        return;
    };
    let mut remaining = RELAY_SETTLEMENT_LIMIT;
    let mut delay = Duration::from_millis(100);
    // Waiting for a SQL slot is backpressure. The bounded owner keeps the
    // receipt until it can submit the update, including on the first attempt.
    while let Ok(permit) = limit.acquire().await {
        let started = tokio::time::Instant::now();
        let result = dispatch.settle_unsent(edge_agent_id, receipts).await;
        drop(permit);
        let error = match result {
            Ok(()) => return,
            // SQLx did not get a connection, so the update was never sent.
            Err(astra_services::multi_agent::EdgeDispatchDeliveryError::Database(
                sqlx::Error::PoolTimedOut,
            )) => continue,
            Err(error) => error,
        };
        // The response may have been lost after commit. Retrying the same
        // conditional terminal update is safe; never return these rows to
        // the claimable queue or invent a winning result.
        if remaining.saturating_sub(started.elapsed()) <= delay {
            break;
        }
        tracing::warn!(
            user_id = %first.user_id,
            edge_agent_id,
            count = receipts.len(),
            %error,
            ?delay,
            "unable to confirm settlement of unsent Edge dispatches; retrying"
        );
        tokio::time::sleep(delay).await;
        remaining = remaining.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break;
        }
        delay = delay.saturating_mul(2).min(Duration::from_secs(2));
    }
    metrics
        .dispatch_settlement_unknown_total
        .fetch_add(receipts.len() as u64, Ordering::Relaxed);
    tracing::warn!(user_id = %first.user_id, edge_agent_id, count = receipts.len(),
        "edge dispatch settlement exhausted retries; claimed outcome remains unknown");
}

/// Validate edge self-reported capabilities against the server-side tool
/// registry. Strips tool names that don't exist in the built-in registry
/// (a malicious edge could fabricate them) and ensures the executor type
/// is consistent with an edge connection.
///
/// Returns the sanitized capabilities JSON. Only the WebSocket route can
/// receive a Work attestation through the relay. Invalid, mismatched, or absent
/// advertisements fail admission before registration. An authenticated
/// Edge may advertise no executable capacity: that is the control-only state
/// used while replaying durable receipts after a local provider disappears.
/// It cannot receive new tool work because it has neither a builtin tool
/// surface nor a provider discovery declaration.
pub(super) fn validate_edge_capabilities(
    capabilities: Option<serde_json::Value>,
    edge_agent_id: &str,
    transport: ToolTransportKind,
) -> Result<serde_json::Value, String> {
    let capabilities = capabilities.ok_or_else(|| "edge capabilities are required".to_string())?;
    let runtime_process_authorization_v1 =
        astra_server_types::edge_ws_protocol::supports_runtime_process_authorization(Some(
            &capabilities,
        ));
    let w3c_trace_context_v1 =
        astra_server_types::edge_ws_protocol::supports_w3c_trace_context(Some(&capabilities));
    let capacity_providers = capabilities
        .get("capacity_rpc_v1")
        .map(|value| {
            use astra_server_types::edge_ws_protocol::{
                EdgeCapacityProvider, valid_capacity_identity,
            };
            use base64::Engine;
            let providers: Vec<EdgeCapacityProvider> = serde_json::from_value(value.clone())
                .map_err(|_| "invalid capacity provider advertisement".to_string())?;
            let mut identities = std::collections::HashSet::new();
            if providers.is_empty()
                || providers.len() > 8
                || providers.iter().any(|provider| {
                    !valid_capacity_identity(&provider.id)
                        || !identities.insert(&provider.id)
                        || !base64::engine::general_purpose::URL_SAFE_NO_PAD
                            .decode(&provider.public_key)
                            .is_ok_and(|key| key.len() == 32)
                })
            {
                return Err("invalid capacity provider identity or public key".to_string());
            }
            Ok(providers)
        })
        .transpose()?;

    let mut advert = astra_runtime_env::RuntimeEnvironmentAdvertisement::decode(capabilities)
        .map_err(|error| error.to_string())?;

    // Ensure the executor is actually an edge agent (not a cloud executor
    // or local CLI masquerading as edge).
    if !advert.binding.executor.is_edge_agent() {
        return Err("edge capabilities must declare an edge-agent executor".to_string());
    }

    if advert.binding.executor.executor_id != edge_agent_id {
        return Err("edge capability executor id does not match authenticated edge id".to_string());
    }

    // Cross-reference tool names against the edge provider contract. Strip
    // registry-only, server-owned, or currently unavailable tools — edge is a
    // runtime executor provider, not a source of server/control-plane capacity.
    let registry = astra_runtime_env::ToolRegistry::builtins();
    let edge_provider = astra_runtime_env::runtime_workspace_provider(
        astra_runtime_env::CapacityProviderType::EdgeCapacity,
        advert.binding.executor.executor_id.clone(),
        &registry,
        advert.binding.runtime.platform,
    );
    let original_count = advert.binding.tool_surface.tool_names.len();
    advert.binding.tool_surface.tool_names.retain(|name| {
        registry.get(name).is_some()
            && edge_provider.declares_tool(name)
            && astra_runtime_env::CapabilityResolver
                .check_tool(&registry, name, &advert.binding.capabilities)
                .is_ok()
    });
    advert
        .binding
        .tool_surface
        .denials
        .retain(|denial| edge_provider.declares_tool(&denial.tool_name));
    let stripped = original_count - advert.binding.tool_surface.tool_names.len();
    if stripped > 0 {
        tracing::warn!(
            target: "astra_runtime::edge_ws",
            edge_agent_id = %edge_agent_id,
            stripped,
            remaining = advert.binding.tool_surface.tool_names.len(),
            "edge advertised tools outside edge provider ownership — stripped"
        );
    }
    let mut sanitized = serde_json::to_value(&advert)
        .map_err(|error| format!("edge capabilities could not be serialized: {error}"))?;
    if let Some(providers) = capacity_providers {
        sanitized["capacity_rpc_v1"] = serde_json::to_value(providers)
            .map_err(|_| "capacity providers could not be serialized".to_string())?;
    }
    let mut accepted = serde_json::Map::new();
    if runtime_process_authorization_v1 {
        accepted.insert(
            astra_server_types::edge_ws_protocol::RUNTIME_PROCESS_AUTHORIZATION_V1_CAPABILITY
                .to_string(),
            serde_json::Value::Bool(true),
        );
    }
    if w3c_trace_context_v1 {
        accepted.insert(
            astra_server_types::edge_ws_protocol::W3C_TRACE_CONTEXT_V1_CAPABILITY.to_string(),
            serde_json::Value::Bool(true),
        );
    }
    if transport == ToolTransportKind::EdgeWebSocket
        && advert.binding.tool_surface.contains(WORK_ATTESTATION_TOOL)
    {
        accepted.insert(
            WORK_ATTESTATION_CAPABILITY.to_string(),
            serde_json::Value::Bool(true),
        );
    }
    if !accepted.is_empty() {
        sanitized["protocol_capabilities"] = serde_json::Value::Object(accepted);
    }
    Ok(sanitized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_runtime_env::{RuntimeEnvironmentAdvertisement, ToolUnavailableReason};

    fn edge_advertisement_with_tools(tool_names: &[&str]) -> serde_json::Value {
        let registry = astra_runtime_env::ToolRegistry::builtins();
        let mut binding = astra_runtime_env::RunBinding::edge_developer("/workspace", &registry);
        binding.tool_surface.tool_names =
            tool_names.iter().map(|name| (*name).to_string()).collect();
        binding.tool_surface.denials = vec![
            astra_runtime_env::ToolDenial {
                tool_name: "ask_user".to_string(),
                reason: ToolUnavailableReason::ExecutorUnavailable(
                    "control_plane_required".to_string(),
                ),
            },
            astra_runtime_env::ToolDenial {
                tool_name: "write_file".to_string(),
                reason: ToolUnavailableReason::PolicyDenied("filesystem_write".to_string()),
            },
        ];
        serde_json::to_value(RuntimeEnvironmentAdvertisement::new(binding))
            .expect("edge advertisement serializes")
    }

    fn edge_advertisement_with_executor_id(
        executor_id: &str,
        tool_names: &[&str],
    ) -> serde_json::Value {
        let mut advert: RuntimeEnvironmentAdvertisement =
            serde_json::from_value(edge_advertisement_with_tools(tool_names))
                .expect("edge advertisement parses");
        advert.binding.executor.executor_id = executor_id.to_string();
        serde_json::to_value(advert).expect("edge advertisement serializes")
    }

    fn provider_only_edge_advertisement() -> serde_json::Value {
        let registry = astra_runtime_env::ToolRegistry::builtins();
        let mut advert = astra_runtime_env::RuntimeEnvironmentAdvertisement::new(
            astra_runtime_env::RunBinding::edge_developer("/workspace", &registry),
        );
        let declaration = astra_turn_types::ProviderToolDeclaration {
            native_tool_id: astra_turn_types::NativeToolId::new("native_codex")
                .expect("provider tool id"),
            native_tool_name: "native_codex".to_string(),
            stable_tool_alias: None,
            title: Some("Native collaborator".to_string()),
            description: Some("provider stage".to_string()),
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: None,
            claims: Default::default(),
            task_support: astra_turn_types::ProviderTaskSupport::Required,
            extension_fields: Default::default(),
        };
        let snapshot = astra_turn_types::ProviderDiscoverySnapshot::new(
            astra_turn_types::ProviderIdentity::new("edge-agent").expect("provider identity"),
            astra_turn_types::ProviderBindingRef::new("edge-binding").expect("binding"),
            astra_turn_types::ProviderProtocolId::new("cli-local").expect("protocol"),
            vec![declaration],
        )
        .expect("provider discovery snapshot");
        advert.binding.tool_surface.tool_names.clear();
        advert.binding.tool_surface.admissions.clear();
        advert.binding.tool_surface.denials.clear();
        advert.provider_discovery = vec![snapshot];
        serde_json::to_value(advert).expect("provider-only edge advertisement serializes")
    }

    fn relay_tool_request(args: serde_json::Value, tool: &str) -> EdgeServerMessage {
        EdgeServerMessage::ToolRequest {
            request_id: "dispatch-request".to_string(),
            identity: Box::new(
                astra_turn_types::ToolInvocationIdentity::new(
                    "user-1",
                    "session-1",
                    "run-1",
                    "chain-1",
                    "invocation-1",
                )
                .expect("test invocation identity"),
            ),
            delivery_generation: 1,
            tool: tool.to_string(),
            args,
            execution_ceiling: None,
            runtime_process_authorization: None,
            runtime_process_authorization_required: false,
            timeout_secs: Some(30),
            execution_deadline_unix_ms: None,
            execution_timeout_ms: None,
            command_timeout_cap_ms: None,

            traceparent: None,
            tracestate: None,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn relay_deadline_charges_durable_queue_age() {
        let message = relay_tool_request(serde_json::json!({}), "bash");
        let mut ready = message.clone();
        let mut elapsed = message;
        let now = tokio::time::Instant::now();
        let earliest_cutoff = chrono::Utc::now().timestamp_millis() as u64 + 5_000;
        let deadline = bind_relay_execution_deadline(&mut ready, Duration::from_secs(25)).unwrap();
        let latest_cutoff = chrono::Utc::now().timestamp_millis() as u64 + 5_000;
        assert!(deadline.saturating_duration_since(now) >= Duration::from_secs(5));
        assert!(
            deadline.saturating_duration_since(tokio::time::Instant::now())
                <= Duration::from_secs(5)
        );
        assert!(matches!(
            ready,
            EdgeServerMessage::ToolRequest {
                timeout_secs: Some(30),
                execution_deadline_unix_ms: Some(cutoff),
                execution_timeout_ms: Some(5_000),
                ..
            } if (earliest_cutoff..=latest_cutoff).contains(&cutoff)
        ));
        let expired = bind_relay_execution_deadline(&mut elapsed, Duration::from_secs(30)).unwrap();
        assert!(expired <= tokio::time::Instant::now());
    }

    #[test]
    fn relay_attestation_strips_its_marker_for_the_authenticated_materialization() {
        let mut message = relay_tool_request(
            serde_json::json!({
                "command": "pwd",
                "__astra_attestation": {
                    "edge_id": "edge-1",
                    "materialization_id": "materialization-1"
                }
            }),
            "bash",
        );

        assert_eq!(
            validate_and_strip_attestation_marker(
                &mut message,
                Some("edge-1"),
                Some("materialization-1")
            ),
            Ok(())
        );
        let EdgeServerMessage::ToolRequest { args, .. } = message else {
            panic!("expected tool request");
        };
        assert_eq!(args, serde_json::json!({"command":"pwd"}));
        astra_tools::schemas::validate_tool_arguments("bash", &args).unwrap();
    }

    #[test]
    fn relay_attestation_rejects_topology_or_materialization_mismatch() {
        let mut wrong_materialization = relay_tool_request(
            serde_json::json!({
                "__astra_attestation": {
                    "edge_id": "edge-1",
                    "materialization_id": "other-materialization"
                }
            }),
            "bash",
        );
        assert_eq!(
            validate_and_strip_attestation_marker(
                &mut wrong_materialization,
                Some("edge-1"),
                Some("materialization-1")
            ),
            Err("edge_attestation_materialization_mismatch")
        );

        let mut wrong_tool = relay_tool_request(
            serde_json::json!({
                "__astra_attestation": {
                    "edge_id": "edge-1",
                    "materialization_id": "materialization-1"
                }
            }),
            "read_file",
        );
        assert_eq!(
            validate_and_strip_attestation_marker(
                &mut wrong_tool,
                Some("edge-1"),
                Some("materialization-1")
            ),
            Err("edge_attestation_tool_mismatch")
        );
    }

    #[test]
    fn relay_attestation_marker_is_optional_for_ordinary_tool_requests() {
        let mut message = relay_tool_request(serde_json::json!({"command": "pwd"}), "bash");
        assert_eq!(
            validate_and_strip_attestation_marker(
                &mut message,
                Some("edge-1"),
                Some("materialization-1")
            ),
            Ok(())
        );
    }

    #[test]
    fn validate_edge_capabilities_strips_non_edge_provider_tools() {
        let mut capabilities = edge_advertisement_with_tools(&[
            "read_file",
            "bash",
            "ask_user",
            "tool_search",
            "memory",
            "mcp__weather",
            "not_registered",
        ]);

        let sanitized = validate_edge_capabilities(
            Some(capabilities.clone()),
            "edge-agent",
            ToolTransportKind::EdgeWebSocket,
        )
        .expect("valid edge advertisement");
        let proof = WORK_ATTESTATION_CAPABILITY;
        assert_eq!(sanitized["protocol_capabilities"][proof], true);
        // REST publishes discovery information. It cannot establish a relay
        // capability, even if it repeats the server's connection advertisement.
        capabilities["protocol_capabilities"] = serde_json::json!({ (proof): true });
        let rest = validate_edge_capabilities(
            Some(capabilities),
            "edge-agent",
            ToolTransportKind::EdgeLedger,
        )
        .unwrap();
        assert!(rest["protocol_capabilities"][proof].is_null());
        let advert: RuntimeEnvironmentAdvertisement =
            serde_json::from_value(sanitized).expect("sanitized advertisement");

        assert!(advert.binding.tool_surface.contains("read_file"));
        assert!(advert.binding.tool_surface.contains("bash"));
        for hidden in [
            "ask_user",
            "tool_search",
            "memory",
            "mcp__weather",
            "not_registered",
        ] {
            assert!(
                !advert.binding.tool_surface.contains(hidden),
                "{hidden} must not be accepted as edge-owned capacity"
            );
        }
        assert!(
            advert
                .binding
                .tool_surface
                .denials
                .iter()
                .all(|denial| denial.tool_name == "write_file"),
            "edge capability denials should only describe edge-owned runtime tools"
        );
    }

    #[test]
    fn validate_edge_capabilities_accepts_provider_only_edge() {
        let sanitized = validate_edge_capabilities(
            Some(provider_only_edge_advertisement()),
            "edge-agent",
            ToolTransportKind::EdgeWebSocket,
        )
        .expect("provider-only edge must be admissible");
        assert!(sanitized["protocol_capabilities"][WORK_ATTESTATION_CAPABILITY].is_null());
        let advert: RuntimeEnvironmentAdvertisement =
            serde_json::from_value(sanitized).expect("sanitized advertisement");

        assert!(advert.binding.tool_surface.tool_names.is_empty());
        assert_eq!(advert.provider_discovery.len(), 1);
        assert_eq!(
            advert.provider_discovery[0].tool_declarations[0].native_tool_name,
            "native_codex"
        );
    }

    #[test]
    fn validate_edge_capabilities_accepts_control_only_edge() {
        let capabilities = edge_advertisement_with_tools(&[]);
        let sanitized = validate_edge_capabilities(
            Some(capabilities),
            "edge-agent",
            ToolTransportKind::EdgeWebSocket,
        )
        .expect("an authenticated control-only edge must be able to recover receipts");
        let advert: RuntimeEnvironmentAdvertisement =
            serde_json::from_value(sanitized).expect("sanitized advertisement");
        assert!(advert.binding.tool_surface.tool_names.is_empty());
        assert!(advert.provider_discovery.is_empty());
    }

    #[test]
    fn validate_edge_capabilities_preserves_known_protocol_versions_only() {
        let mut capabilities = edge_advertisement_with_tools(&["read_file"]);
        capabilities["protocol_capabilities"] = serde_json::json!({
            "runtime_process_authorization_v1": true,
            "w3c_trace_context_v1": true,
            "unrecognized_future_protocol": true,
        });

        let sanitized = validate_edge_capabilities(
            Some(capabilities),
            "edge-agent",
            ToolTransportKind::EdgeWebSocket,
        )
        .expect("valid edge advertisement");

        assert_eq!(
            sanitized["protocol_capabilities"]
                [astra_server_types::edge_ws_protocol::RUNTIME_PROCESS_AUTHORIZATION_V1_CAPABILITY],
            true
        );
        assert_eq!(
            sanitized["protocol_capabilities"]
                [astra_server_types::edge_ws_protocol::W3C_TRACE_CONTEXT_V1_CAPABILITY],
            true
        );
        assert!(
            sanitized["protocol_capabilities"]
                .get("unrecognized_future_protocol")
                .is_none()
        );
    }

    #[test]
    fn validate_edge_capabilities_preserves_valid_capacities_and_rejects_ambiguous_or_secret_metadata()
     {
        use base64::Engine;
        let provider = serde_json::json!({"id":"approved", "public_key":
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([7;32])});
        let mut capabilities = edge_advertisement_with_tools(&["read_file"]);
        capabilities["capacity_rpc_v1"] = serde_json::json!([provider.clone()]);
        capabilities["capacity_relay_v1"] = serde_json::json!({"origin":"http://attacker"});
        let accepted = validate_edge_capabilities(
            Some(capabilities.clone()),
            "edge-agent",
            ToolTransportKind::EdgeWebSocket,
        )
        .unwrap();
        assert_eq!(
            accepted["capacity_rpc_v1"],
            serde_json::json!([provider.clone()])
        );
        assert!(accepted.get("capacity_relay_v1").is_none());
        for invalid in [
            serde_json::json!([]),
            serde_json::json!([provider.clone(), provider.clone()]),
            serde_json::json!([{"id":"approved", "public_key":"invalid"}]),
            serde_json::json!([{"id":"approved", "public_key":provider["public_key"], "endpoint":"http://attacker"}]),
            serde_json::json!([{"id":"approved", "public_key":provider["public_key"], "bearer_key":"secret"}]),
            serde_json::to_value(vec![provider; 9]).unwrap(),
        ] {
            capabilities["capacity_rpc_v1"] = invalid;
            assert!(
                validate_edge_capabilities(
                    Some(capabilities.clone()),
                    "edge-agent",
                    ToolTransportKind::EdgeWebSocket
                )
                .is_err()
            );
        }
    }

    #[test]
    fn validate_edge_capabilities_rejects_executor_id_mismatch() {
        let capabilities =
            edge_advertisement_with_executor_id("edge@forged", &["read_file", "bash"]);

        let sanitized = validate_edge_capabilities(
            Some(capabilities),
            "edge-agent",
            ToolTransportKind::EdgeWebSocket,
        );

        assert!(
            sanitized.is_err(),
            "edge advertisement executor id must match authenticated edge id before tools become offers"
        );
    }

    #[test]
    fn validate_edge_capabilities_reports_unsupported_capability_schema_before_body() {
        for version in [1, u64::from(u32::MAX)] {
            let error = validate_edge_capabilities(
                Some(serde_json::json!({"schema_version": version})),
                "edge-agent",
                ToolTransportKind::EdgeWebSocket,
            )
            .unwrap_err();
            assert!(error.contains(&format!("schema v{version}")), "{error}");
            assert!(error.contains("expects v2"), "{error}");
            assert!(error.contains("upgrade"), "{error}");
            assert!(!error.contains("missing field"), "{error}");
        }
    }

    #[test]
    fn validate_edge_capabilities_rejects_absent_malformed_and_unknown_schema() {
        assert!(
            validate_edge_capabilities(None, "edge-agent", ToolTransportKind::EdgeWebSocket)
                .is_err()
        );
        assert!(
            validate_edge_capabilities(
                Some(serde_json::json!({
                    "schema_version": RuntimeEnvironmentAdvertisement::SCHEMA_VERSION
                })),
                "edge-agent",
                ToolTransportKind::EdgeWebSocket
            )
            .is_err()
        );

        let mut unknown_schema = edge_advertisement_with_tools(&["read_file"]);
        unknown_schema["schema_version"] = serde_json::json!(u32::MAX);
        assert!(
            validate_edge_capabilities(
                Some(unknown_schema),
                "edge-agent",
                ToolTransportKind::EdgeWebSocket
            )
            .is_err()
        );
    }

    fn inflight_dispatch(request_id: &str) -> InflightEdgeDispatch {
        InflightEdgeDispatch {
            identity: astra_services::multi_agent::EdgeDispatchIdentity::new(
                "user-1",
                "session-1",
                "run-1",
                "chain-1",
                request_id,
            ),
            edge_agent_id: "edge-1".to_string(),
        }
    }

    #[tokio::test]
    async fn inflight_tracker_lifecycle_covers_send_fail_deliver_and_disconnect_drain() {
        let tracker = InflightEdgeDispatchTracker::default();

        assert!(
            tracker
                .track(inflight_dispatch("send-failed"))
                .await
                .is_ok()
        );
        let removed = tracker.remove("send-failed").await;
        assert!(
            removed.is_some(),
            "send failure must remove the pre-registered dispatch before claimed-row failure handling"
        );
        assert!(
            tracker.drain().await.is_empty(),
            "send-failed dispatch must not be failed again during disconnect cleanup"
        );

        assert!(tracker.track(inflight_dispatch("delivered")).await.is_ok());
        let delivered = tracker.remove("delivered").await;
        assert!(
            delivered.is_some(),
            "successful edge result delivery must remove the inflight dispatch"
        );
        assert!(
            tracker.drain().await.is_empty(),
            "delivered dispatch must not be treated as orphaned on disconnect"
        );

        assert!(tracker.track(inflight_dispatch("orphaned")).await.is_ok());
        let drained = tracker.drain().await;
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].identity.request_id, "orphaned");
        assert!(
            tracker.drain().await.is_empty(),
            "disconnect cleanup must consume orphaned dispatches exactly once"
        );
    }

    #[tokio::test]
    async fn inflight_tracker_concurrent_remove_and_disconnect_drain_consume_once() {
        let tracker = InflightEdgeDispatchTracker::default();
        assert!(tracker.track(inflight_dispatch("race")).await.is_ok());

        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(3));
        let remove_tracker = tracker.clone();
        let remove_barrier = std::sync::Arc::clone(&barrier);
        let remove_task = tokio::spawn(async move {
            remove_barrier.wait().await;
            remove_tracker.remove("race").await
        });

        let drain_tracker = tracker.clone();
        let drain_barrier = std::sync::Arc::clone(&barrier);
        let drain_task = tokio::spawn(async move {
            drain_barrier.wait().await;
            drain_tracker.drain().await
        });

        barrier.wait().await;
        let removed = remove_task.await.expect("remove task should not panic");
        let drained = drain_task.await.expect("drain task should not panic");
        let consumed_count = usize::from(removed.is_some())
            + drained
                .iter()
                .filter(|dispatch| dispatch.identity.request_id == "race")
                .count();

        assert_eq!(
            consumed_count, 1,
            "delivery and disconnect cleanup racing for the same dispatch must have exactly one winner"
        );
        assert!(
            tracker.drain().await.is_empty(),
            "no inflight dispatch should remain after the race resolves"
        );
    }

    #[test]
    fn edge_ws_connection_permit_has_single_owner_and_releases_on_drop() {
        let previous = EDGE_WS_CONNECTION_COUNT.swap(0, Ordering::AcqRel);
        let permit = try_acquire_edge_ws_connection().expect("permit should be available");
        assert_eq!(EDGE_WS_CONNECTION_COUNT.load(Ordering::Acquire), 1);

        drop(permit);
        assert_eq!(EDGE_WS_CONNECTION_COUNT.load(Ordering::Acquire), 0);
        EDGE_WS_CONNECTION_COUNT.store(previous, Ordering::Release);
    }
}

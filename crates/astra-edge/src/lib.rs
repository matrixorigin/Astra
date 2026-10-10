//! The existing Edge delivery owner, shared by the Edge binary and CLI host.
//! Authentication/configuration stay in the host; this owner alone handles
//! journal custody, generation fencing, cancellation, ACK/replay and settlement.

mod invocation_journal;

use astra_server_types::edge_ws_protocol::{
    EDGE_HEARTBEAT_INTERVAL_SECS, EdgeClientMessage, EdgeServerMessage,
};
use astra_turn_types::{ProviderStageInput, ProviderStageInputAck};
use futures_util::{SinkExt, StreamExt, future::BoxFuture};
use invocation_journal::{DurableEdgeResult, EdgeInvocationJournal, JournalError, PrepareOutcome};
use serde_json::Value;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
    task::JoinSet,
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};
use tokio_util::sync::CancellationToken;

/// Check whether a previously opened invocation journal may need a delivery
/// owner even when the optional provider is currently unavailable. The journal
/// remains the only source of recovery facts; this helper deliberately avoids
/// creating a new journal for an ordinary session with no prior native work.
pub async fn has_pending_invocation_results(path: PathBuf) -> bool {
    let wal_path = path.with_extension("json.wal");
    let state_exists = match tokio::fs::try_exists(&path).await {
        Ok(exists) => exists,
        Err(_) => return true,
    };
    let wal_exists = match tokio::fs::try_exists(&wal_path).await {
        Ok(exists) => exists,
        Err(_) => return true,
    };
    if !state_exists && !wal_exists {
        return false;
    }
    match EdgeInvocationJournal::open(path).await {
        Ok(journal) => journal
            .pending_results()
            .map_or(true, |results| !results.is_empty()),
        Err(_) => true,
    }
}

/// Convert an API/server base URL into the WebSocket endpoint used by Edge
/// owners. CLI hosts use this same conversion so HTTP configuration and native
/// delivery cannot silently diverge.
pub fn edge_ws_url(server_url: &str) -> Result<String, String> {
    let trimmed = server_url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err("server URL must not be empty".to_string());
    }
    let with_ws_scheme = if let Some(rest) = trimmed.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if trimmed.starts_with("ws://") || trimmed.starts_with("wss://") {
        trimmed.to_string()
    } else if trimmed.contains("://") {
        return Err(format!(
            "unsupported server URL scheme in '{trimmed}'; use http(s):// or ws(s)://"
        ));
    } else {
        format!("ws://{trimmed}")
    };

    let mut url = reqwest::Url::parse(&with_ws_scheme)
        .map_err(|_| format!("invalid server URL '{server_url}'"))?;
    if !matches!(url.scheme(), "ws" | "wss") {
        return Err(format!(
            "unsupported edge WebSocket URL scheme '{}'; use ws:// or wss://",
            url.scheme()
        ));
    }
    url.set_path(&normalized_edge_ws_path(url.path()));
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.to_string())
}

fn normalized_edge_ws_path(path: &str) -> String {
    let segments = path
        .trim_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();

    if segments.is_empty() {
        return "/edge/ws".to_string();
    }

    if let Some(index) = segments
        .windows(2)
        .position(|window| window == ["edge", "ws"])
    {
        return format!("/{}", segments[..index + 2].join("/"));
    }

    format!("/{}/edge/ws", segments.join("/"))
}

pub type EdgeConnectionError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub enum EdgeAuthenticationError {
    #[error("Server rejected Edge authentication")]
    Rejected,
    #[error("Server interaction contract is incompatible")]
    IncompatibleContract,
    #[error("Server returned an invalid authenticated account")]
    InvalidAccount,
    #[error("Server authenticated a different account")]
    AccountMismatch,
    #[error("Unexpected Edge authentication response")]
    Protocol,
    #[error("Edge connection closed before authentication completed")]
    ClosedBeforeAuthentication,
    #[error("Edge authentication deadline expired")]
    Timeout,
    #[error("Edge authentication cancelled")]
    Cancelled,
    #[error("Edge authentication transport failed")]
    Transport(#[source] tokio_tungstenite::tungstenite::Error),
    #[error("Edge authentication envelope is invalid")]
    Envelope(#[source] serde_json::Error),
}

impl EdgeAuthenticationError {
    pub fn is_permanent(&self) -> bool {
        matches!(
            self,
            Self::Rejected
                | Self::IncompatibleContract
                | Self::InvalidAccount
                | Self::AccountMismatch
                | Self::Protocol
                | Self::Envelope(_)
        )
    }
}

/// One authentication exchange for both hosts. A failed exchange consumes the
/// socket: callers cannot accidentally continue with a half-authenticated peer.
/// Success establishes identity, not consumer/capacity readiness.
pub async fn authenticate_connection(
    mut socket: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    auth: EdgeClientMessage,
    expected_account: Option<&str>,
    cancellation: &CancellationToken,
) -> Result<
    (
        WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
        String,
        String,
    ),
    EdgeAuthenticationError,
> {
    if !matches!(auth, EdgeClientMessage::Auth { .. }) {
        return Err(EdgeAuthenticationError::Protocol);
    }
    let deadline = tokio::time::Instant::now()
        + Duration::from_secs(astra_server_types::edge_ws_protocol::EDGE_AUTH_TIMEOUT_SECS);
    let exchange = async {
        socket
            .send(Message::Text(
                serde_json::to_string(&auth)
                    .map_err(EdgeAuthenticationError::Envelope)?
                    .into(),
            ))
            .await
            .map_err(EdgeAuthenticationError::Transport)?;
        let response = match socket.next().await {
            Some(Ok(Message::Text(text))) => serde_json::from_str::<EdgeServerMessage>(&text)
                .map_err(|_| EdgeAuthenticationError::Protocol)?,
            Some(Err(error)) => return Err(EdgeAuthenticationError::Transport(error)),
            None | Some(Ok(Message::Close(_))) => {
                return Err(EdgeAuthenticationError::ClosedBeforeAuthentication);
            }
            _ => return Err(EdgeAuthenticationError::Protocol),
        };
        match response {
            EdgeServerMessage::AuthOk {
                user_id,
                edge_id,
                interaction_api_major,
            } => {
                if interaction_api_major != astra_server_types::AGENT_INTERACTION_API_MAJOR {
                    return Err(EdgeAuthenticationError::IncompatibleContract);
                }
                if user_id.trim().is_empty() {
                    return Err(EdgeAuthenticationError::InvalidAccount);
                }
                if edge_id.trim().is_empty() {
                    return Err(EdgeAuthenticationError::Protocol);
                }
                if expected_account.is_some_and(|expected| expected != user_id) {
                    return Err(EdgeAuthenticationError::AccountMismatch);
                }
                Ok((user_id, edge_id))
            }
            EdgeServerMessage::AuthError { .. } => Err(EdgeAuthenticationError::Rejected),
            _ => Err(EdgeAuthenticationError::Protocol),
        }
    };
    let account = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(EdgeAuthenticationError::Cancelled),
        result = tokio::time::timeout_at(deadline, exchange) =>
            result.map_err(|_| EdgeAuthenticationError::Timeout)??,
    };
    let (account, edge_id) = account;
    Ok((socket, account, edge_id))
}

/// Account is the actual AuthOk identity, not a caller-local fallback.
pub struct EdgeConnectionContext {
    pub account_id: String,
    pub edge_agent_id: String,
    pub workspace_dir: PathBuf,
    pub journal_path: PathBuf,
    /// Signals installed consumer readiness after journal recovery/replay.
    pub ready: Option<oneshot::Sender<()>>,
}

/// Exact transport envelope; a host must not reinterpret IDs or renew budget.
pub struct EdgeInvocation {
    pub identity: astra_server_types::edge_ws_protocol::ToolInvocationIdentity,
    pub delivery_generation: u64,
    pub tool: String,
    pub args: Value,
    pub execution_deadline: Instant,
    pub command_timeout_cap_ms: Option<u64>,
    pub execution_ceiling: Option<Box<astra_server_types::edge_ws_protocol::EdgeExecutionCeiling>>,
    pub runtime_process_authorization:
        Option<Box<astra_server_types::edge_ws_protocol::RuntimeProcessAuthorizationContext>>,
    /// Bounded semantic inputs addressed to this invocation. Ordinary tool
    /// executors may ignore the receiver; provider-stage adapters consume it.
    pub input_rx: mpsc::Receiver<EdgeInvocationInput>,
}

/// One input delivered to a running provider invocation. The adapter resolves
/// the acknowledgement only after its provider protocol has accepted or
/// rejected the input, keeping transport delivery distinct from processing.
pub struct EdgeInvocationInput {
    pub input: ProviderStageInput,
    pub ack: oneshot::Sender<ProviderStageInputAck>,
}

pub trait EdgeInvocationExecutor: Send + Sync {
    fn execute(
        &self,
        invocation: EdgeInvocation,
        cancel: CancellationToken,
    ) -> BoxFuture<'_, astra_tools::ToolResult>;
}

const MAX_CONCURRENT_TOOL_EXECUTIONS: usize = 128;
const MAX_PROVIDER_STAGE_INPUTS_PER_INVOCATION: usize = 64;

fn command_deadline(
    deadline: Instant,
    timeout_secs: u64,
    cap_ms: Option<u64>,
    native: bool,
) -> Instant {
    if native {
        deadline
    } else {
        let cap = Duration::from_millis(cap_ms.unwrap_or(u64::MAX))
            .min(Duration::from_secs(timeout_secs));
        deadline.min(Instant::now() + cap)
    }
}

fn deadline_result(tool: &str, mut result: astra_tools::ToolResult) -> astra_tools::ToolResult {
    let observed_output = std::mem::take(&mut result.output);
    result.output = if observed_output.is_empty() {
        format!("Tool '{tool}' exceeded its server-issued execution deadline")
    } else {
        format!(
            "Tool '{tool}' exceeded its server-issued execution deadline. Observed output before the deadline:\n{observed_output}"
        )
    };
    result.is_error = true;
    result.metadata.get_or_insert_with(Default::default).insert(
        "execution_deadline_exceeded".into(),
        serde_json::Value::Bool(true),
    );
    result
}

/// Anchor the immutable server work budget at receipt, before queueing or
/// journal I/O. Relative remaining time must never renew an absolute cutoff.
fn edge_execution_deadline(
    timeout_secs: u64,
    execution_deadline_unix_ms: Option<u64>,
    execution_timeout_ms: Option<u64>,
) -> Result<Instant, &'static str> {
    let now = Instant::now();
    let duration = match (execution_deadline_unix_ms, execution_timeout_ms) {
        (Some(deadline), Some(remaining)) => {
            let unix_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| "Edge clock cannot validate execution deadline")?
                .as_millis()
                .min(u128::from(u64::MAX)) as u64;
            Duration::from_millis(remaining.min(deadline.saturating_sub(unix_ms)))
        }
        (None, None) => Duration::from_secs(timeout_secs),
        _ => return Err("Incomplete server execution budget"),
    };
    if duration.is_zero() {
        return Err("Server-issued execution deadline expired before dispatch");
    }
    now.checked_add(duration)
        .ok_or("Server execution deadline is outside clock range")
}

#[derive(Clone)]
struct EdgeExecutionBudget {
    permits: Arc<Semaphore>,
}

impl EdgeExecutionBudget {
    fn new() -> Self {
        Self {
            permits: Arc::new(Semaphore::new(MAX_CONCURRENT_TOOL_EXECUTIONS)),
        }
    }

    fn try_acquire(&self) -> Option<OwnedSemaphorePermit> {
        self.permits.clone().try_acquire_owned().ok()
    }
}

struct CompletedEdgeInvocation {
    request_id: String,
    generation: u64,
    result: astra_tools::ToolResult,
    duration_ms: u64,
}

struct CompletedProviderStageInput {
    request_id: String,
    generation: u64,
    ack: ProviderStageInputAck,
}

fn rejected_tool_message(
    request_id: String,
    identity: astra_turn_types::ToolInvocationIdentity,
    delivery_generation: u64,
    message: impl Into<String>,
) -> EdgeClientMessage {
    DurableEdgeResult::from_tool_result(astra_tools::ToolResult::error(message.into()), 0)
        .client_message(request_id, identity, delivery_generation)
}

fn valid_runtime_process_authorization(
    tool: &str,
    required: bool,
    context: Option<&astra_server_types::edge_ws_protocol::RuntimeProcessAuthorizationContext>,
) -> bool {
    required == context.is_some()
        && context.is_none_or(|context| {
            astra_server_types::edge_ws_protocol::runtime_process_authorization_applies_to_tool(
                tool,
            ) && !context.authorization.trim().is_empty()
        })
}

struct InFlightEdgeInvocation {
    generation: u64,
    cancel: CancellationToken,
    input_tx: mpsc::Sender<EdgeInvocationInput>,
}

#[derive(Default)]
struct EdgeInvocationTracker {
    in_flight: HashMap<String, InFlightEdgeInvocation>,
}

impl EdgeInvocationTracker {
    fn begin(
        &mut self,
        request_id: &str,
        generation: u64,
    ) -> Result<(CancellationToken, mpsc::Receiver<EdgeInvocationInput>), u64> {
        if let Some(active) = self.in_flight.get(request_id) {
            return Err(active.generation);
        }
        let cancel = CancellationToken::new();
        let (input_tx, input_rx) = mpsc::channel(MAX_PROVIDER_STAGE_INPUTS_PER_INVOCATION);
        self.in_flight.insert(
            request_id.to_string(),
            InFlightEdgeInvocation {
                generation,
                cancel: cancel.clone(),
                input_tx,
            },
        );
        Ok((cancel, input_rx))
    }

    fn cancel_if_current(&self, request_id: &str, generation: u64) -> bool {
        let Some(active) = self.in_flight.get(request_id) else {
            return false;
        };
        if active.generation != generation {
            return false;
        }
        active.cancel.cancel();
        true
    }

    fn send_input(
        &self,
        request_id: &str,
        generation: u64,
        input: EdgeInvocationInput,
    ) -> Result<(), EdgeInvocationInput> {
        let Some(active) = self.in_flight.get(request_id) else {
            return Err(input);
        };
        if active.generation != generation {
            return Err(input);
        }
        active
            .input_tx
            .try_send(input)
            .map_err(|error| error.into_inner())
    }

    fn finish_if_current(&mut self, request_id: &str, generation: u64) -> bool {
        if self
            .in_flight
            .get(request_id)
            .is_none_or(|active| active.generation != generation)
        {
            return false;
        }
        self.in_flight.remove(request_id);
        true
    }

    fn cancel_all(&self) {
        for active in self.in_flight.values() {
            active.cancel.cancel();
        }
    }
}

/// Session-scoped custody for admitted Edge work.
///
/// A WebSocket is only a delivery attachment. The invocation tracker, task
/// handles, input lanes and journal must outlive an individual transport so a
/// reconnect can steer or cancel work that was already admitted. There is one
/// owner for a session attachment; reconnects only call `serve_connection` on
/// this owner and never create a second invocation state machine.
pub struct EdgeInvocationOwner {
    account_id: String,
    edge_agent_id: String,
    workspace_dir: PathBuf,
    journal_path: PathBuf,
    executor: Arc<dyn EdgeInvocationExecutor>,
    execution_budget: EdgeExecutionBudget,
    invocations: EdgeInvocationTracker,
    tasks: JoinSet<()>,
    completed_tx: mpsc::Sender<CompletedEdgeInvocation>,
    completed_rx: mpsc::Receiver<CompletedEdgeInvocation>,
    input_ack_tx: mpsc::Sender<CompletedProviderStageInput>,
    input_ack_rx: mpsc::Receiver<CompletedProviderStageInput>,
    journal: EdgeInvocationJournal,
    journal_writable: bool,
}

impl EdgeInvocationOwner {
    pub async fn open(
        context: &EdgeConnectionContext,
        executor: Arc<dyn EdgeInvocationExecutor>,
    ) -> Result<Self, EdgeConnectionError> {
        let (completed_tx, completed_rx) = mpsc::channel::<CompletedEdgeInvocation>(1_024);
        let (input_ack_tx, input_ack_rx) =
            mpsc::channel::<CompletedProviderStageInput>(MAX_PROVIDER_STAGE_INPUTS_PER_INVOCATION);
        let journal = EdgeInvocationJournal::open(context.journal_path.clone()).await?;
        let journal_status = journal.status();
        tracing::info!(
            target: "astra.edge.invocation_journal",
            records = journal_status.records,
            running = journal_status.running,
            awaiting_ack = journal_status.awaiting_ack,
            state_bytes = journal_status.state_bytes,
            wal_entries = journal_status.wal_entries,
            wal_bytes = journal_status.wal_bytes,
            "edge invocation journal restored"
        );
        Ok(Self {
            account_id: context.account_id.clone(),
            edge_agent_id: context.edge_agent_id.clone(),
            workspace_dir: context.workspace_dir.clone(),
            journal_path: context.journal_path.clone(),
            executor,
            execution_budget: EdgeExecutionBudget::new(),
            invocations: EdgeInvocationTracker::default(),
            tasks: JoinSet::new(),
            completed_tx,
            completed_rx,
            input_ack_tx,
            input_ack_rx,
            journal,
            journal_writable: true,
        })
    }

    /// Replace only the executor used by future invocations. Already admitted
    /// tasks retain the executor they captured at dispatch, so reconnecting or
    /// rediscovering a capability cannot mutate work in flight.
    pub fn replace_executor(&mut self, executor: Arc<dyn EdgeInvocationExecutor>) {
        self.executor = executor;
    }

    /// Whether this owner still needs a transport even when the provider is
    /// currently unavailable. A control-only reconnect is required for active
    /// input/cancel lanes and for durable result replay; it must not advertise
    /// executable capacity while the provider is absent.
    pub fn has_unsettled_work(&self) -> bool {
        !self.invocations.in_flight.is_empty()
            || !self.tasks.is_empty()
            || !self.completed_rx.is_empty()
            || !self.input_ack_rx.is_empty()
            || {
                let status = self.journal.status();
                status.running > 0 || status.awaiting_ack > 0
            }
    }

    /// Serve one authenticated transport attachment. A transport close returns
    /// without cancelling or joining admitted work; the owner remains usable
    /// for the next connection. Keep the owner outside the reconnect loop and
    /// call `settle` before dropping it, not between transport attachments.
    /// The shutdown token belongs to the session, not a socket attachment.
    pub async fn serve_connection(
        &mut self,
        socket: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
        context: EdgeConnectionContext,
        shutdown: CancellationToken,
        drain: Option<CancellationToken>,
    ) -> Result<(), EdgeConnectionError> {
        if context.account_id != self.account_id
            || context.edge_agent_id != self.edge_agent_id
            || context.workspace_dir != self.workspace_dir
            || context.journal_path != self.journal_path
        {
            return Err("Edge connection does not belong to this invocation owner".into());
        }
        if !self.journal_writable {
            return Err("Edge invocation journal requires recovery before reconnect".into());
        }
        let result = serve_connection_on_owner(self, socket, context, shutdown, drain).await;
        // An incomplete WAL append must not be followed by another append,
        // including on a replacement transport or during final settlement.
        if matches!(
            result
                .as_ref()
                .err()
                .and_then(|error| error.downcast_ref::<JournalError>()),
            Some(JournalError::Io { .. } | JournalError::Corrupt { .. })
        ) {
            self.journal_writable = false;
        }
        result
    }

    /// Join admitted work and persist its results before releasing custody.
    /// Cancelling `shutdown` requests adapter cleanup; leaving it uncancelled
    /// drains the work naturally. This future must be polled to completion.
    pub async fn settle(
        &mut self,
        shutdown: &CancellationToken,
    ) -> Result<(), EdgeConnectionError> {
        if shutdown.is_cancelled() {
            self.invocations.cancel_all();
        }
        let result = settle_invocations(
            &mut self.tasks,
            &mut self.completed_rx,
            &mut self.journal,
            self.journal_writable,
            shutdown,
            &self.invocations,
        )
        .await;
        self.invocations.in_flight.clear();
        if result.is_err() {
            self.journal_writable = false;
        }
        result
    }
}

/// Serve an already authenticated socket. Shutdown stops admission, cancels
/// active invocations, persists their actual results and joins owned work.
pub async fn serve_connection(
    socket: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    context: EdgeConnectionContext,
    executor: Arc<dyn EdgeInvocationExecutor>,
    shutdown: CancellationToken,
) -> Result<(), EdgeConnectionError> {
    serve_connection_with_drain(socket, context, executor, shutdown, None).await
}

/// Serve an authenticated socket with an optional capability-withdrawal
/// signal. Withdrawal is intentionally different from shutdown: it stops new
/// tool admission but lets already admitted invocations publish their durable
/// results before the socket is closed. The existing shutdown path remains the
/// owner for user/session teardown and still cancels active work.
pub async fn serve_connection_with_drain(
    socket: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    context: EdgeConnectionContext,
    executor: Arc<dyn EdgeInvocationExecutor>,
    shutdown: CancellationToken,
    drain: Option<CancellationToken>,
) -> Result<(), EdgeConnectionError> {
    if shutdown.is_cancelled() {
        return Ok(());
    }
    // This convenience entry point owns the invocation owner for exactly one
    // attachment.  A protocol/transport error therefore cannot leave admitted
    // work running after the owner is dropped.  Keep the caller's token
    // uncancelled and use a child for the local cleanup decision; the
    // session-scoped owner API below still preserves work across reconnects.
    let cleanup_shutdown = shutdown.child_token();
    let mut owner = EdgeInvocationOwner::open(&context, executor).await?;
    let connection_result = owner
        .serve_connection(socket, context, cleanup_shutdown.clone(), drain)
        .await;
    if connection_result.is_err() {
        cleanup_shutdown.cancel();
    }
    let cleanup_result = owner.settle(&cleanup_shutdown).await;
    if let Err(error) = &cleanup_result {
        tracing::error!(
            component = "edge",
            operation = "settle_invocations",
            stage = "cleanup",
            error = %error,
            "Edge invocation cleanup failed"
        );
    }
    connection_result.and(cleanup_result)
}

async fn serve_connection_on_owner(
    owner: &mut EdgeInvocationOwner,
    socket: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    mut context: EdgeConnectionContext,
    shutdown: CancellationToken,
    drain: Option<CancellationToken>,
) -> Result<(), EdgeConnectionError> {
    if shutdown.is_cancelled() {
        return Ok(());
    }
    let (mut write, mut read) = socket.split();
    let execution_budget = owner.execution_budget.clone();
    let invocations = &mut owner.invocations;
    let tasks = &mut owner.tasks;
    let completed_tx = owner.completed_tx.clone();
    let completed_rx = &mut owner.completed_rx;
    let input_ack_tx = owner.input_ack_tx.clone();
    let input_ack_rx = &mut owner.input_ack_rx;
    let journal = &mut owner.journal;
    let executor = owner.executor.clone();

    // Results remain in the durable outbox until the server acknowledges the
    // exact delivery generation. Reconnect therefore starts by replaying them.
    for pending in journal.pending_results()? {
        let message = pending.result.client_message(
            pending.request_id,
            pending.identity,
            pending.delivery_generation,
        );
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => return Ok(()),
            result = write.send(Message::Text(serde_json::to_string(&message)?.into())) => result?,
        }
    }

    if shutdown.is_cancelled() {
        return Ok(());
    }
    if let Some(ready) = context.ready.take() {
        let _ = ready.send(());
    }

    // Heartbeat ticker
    let mut heartbeat = tokio::time::interval(Duration::from_secs(EDGE_HEARTBEAT_INTERVAL_SECS));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    tracing::info!(
        workspace = %context.workspace_dir.display(),
        "Edge agent ready — waiting for tool calls"
    );

    async {
        let mut draining = false;
        loop {
        if draining && tasks.is_empty() && completed_rx.is_empty() && input_ack_rx.is_empty() {
            break;
        }
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            _ = async {
                if let Some(drain) = drain.as_ref() {
                    drain.cancelled().await;
                } else {
                    std::future::pending::<()>().await;
                }
            }, if !draining => {
                draining = true;
                tracing::info!("Edge capability withdrawal started; draining admitted invocations");
            }
            joined = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Err(error)) = joined {
                    return Err(Box::new(error) as EdgeConnectionError);
                }
            }
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        tracing::debug!(
                            frame_len = text.len(),
                            "edge received server text frame"
                        );
                        match serde_json::from_str::<EdgeServerMessage>(&text) {
                            Ok(EdgeServerMessage::ToolRequest {
                                request_id,
                                identity,
                                delivery_generation,
                                tool,
                                args: tool_args,
                                runtime_process_authorization,
                                runtime_process_authorization_required,
                                timeout_secs,
                                execution_deadline_unix_ms,
                                execution_timeout_ms,
                                command_timeout_cap_ms,
                                execution_ceiling,
                            }) => {
                                let execution_deadline = edge_execution_deadline(timeout_secs, execution_deadline_unix_ms, execution_timeout_ms).map(|deadline| {
                                    command_deadline(deadline, timeout_secs, command_timeout_cap_ms, execution_ceiling.is_some())
                                });
                                if !valid_runtime_process_authorization(
                                    &tool,
                                    runtime_process_authorization_required,
                                    runtime_process_authorization.as_deref(),
                                ) {
                                    let message = rejected_tool_message(
                                        request_id,
                                        *identity,
                                        delivery_generation,
                                        "Runtime process authorization is invalid",
                                    );
                                    write
                                        .send(Message::Text(
                                            serde_json::to_string(&message)?.into(),
                                        ))
                                        .await?;
                                    continue;
                                }
                                // During capability withdrawal, preserve the journal's
                                // identity/replay decision but do not admit a new
                                // execution. Existing Running/Completed records must
                                // still resolve as Active/Replay instead of being
                                // replaced by a transport-level rejection.
                                let execution_permit = (!draining)
                                    .then(|| execution_budget.try_acquire())
                                    .flatten();
                                match journal
                                    .prepare(
                                        &request_id,
                                        &identity,
                                        delivery_generation,
                                        &tool,
                                        &tool_args,
                                        execution_permit.is_some(),
                                        execution_ceiling.as_deref(),
                                    )
                                    .await
                                {
                                    Ok(PrepareOutcome::Replay(result)) => {
                                        let message = result.client_message(
                                            request_id,
                                            *identity,
                                            delivery_generation,
                                        );
                                        write.send(Message::Text(serde_json::to_string(&message)?.into())).await?;
                                        continue;
                                    }
                                    Ok(PrepareOutcome::Active) => {
                                        tracing::warn!(
                                            request_id = %request_id,
                                            delivery_generation,
                                            "Duplicate edge delivery joined the existing invocation"
                                        );
                                        continue;
                                    }
                                    Ok(PrepareOutcome::Execute) => {}
                                    Err(error @ (JournalError::Full | JournalError::WalFull)) => {
                                        let journal_status = journal.status();
                                        tracing::warn!(
                                            target: "astra.edge.invocation_journal",
                                            %error,
                                            records = journal_status.records,
                                            running = journal_status.running,
                                            awaiting_ack = journal_status.awaiting_ack,
                                            state_bytes = journal_status.state_bytes,
                                            wal_entries = journal_status.wal_entries,
                                            wal_bytes = journal_status.wal_bytes,
                                            "edge invocation admission rejected by durable journal capacity"
                                        );
                                        let result = DurableEdgeResult::not_dispatched_rejection(
                                            format!("Edge invocation admission is temporarily saturated: {error}"),
                                        )
                                        .with_journal_status(&journal_status);
                                        let message = result.client_message(
                                            request_id,
                                            *identity,
                                            delivery_generation,
                                        );
                                        write.send(Message::Text(serde_json::to_string(&message)?.into())).await?;
                                        continue;
                                    }
                                    Err(error @ JournalError::IdentityConflict { .. }) => {
                                        let message = rejected_tool_message(
                                            request_id,
                                            *identity,
                                            delivery_generation,
                                            format!(
                                                "Edge invocation identity conflict before dispatch: {error}"
                                            ),
                                        );
                                        write.send(Message::Text(serde_json::to_string(&message)?.into())).await?;
                                        continue;
                                    }
                                    Err(error) => return Err(error.into()),
                                }
                                // Replay returns the durable original result even after
                                // expiry. Only a newly admitted execution is rejected.
                                let execution_deadline = match execution_deadline {
                                    Ok(deadline) => deadline,
                                    Err(reason) => {
                                        let pending = journal.complete(&request_id, delivery_generation, DurableEdgeResult::not_dispatched_rejection(reason)).await?;
                                        let message = pending.result.client_message(request_id, *identity, delivery_generation);
                                        write.send(Message::Text(serde_json::to_string(&message)?.into())).await?;
                                        continue;
                                    }
                                };
                                let execution_permit = execution_permit.ok_or_else(|| {
                                    format!(
                                        "edge invocation journal admitted {request_id} without execution capacity"
                                    )
                                })?;
                                let (cancel, input_rx) = match invocations.begin(&request_id, delivery_generation) {
                                    Ok(value) => value,
                                    Err(active_generation) => {
                                        return Err(format!(
                                            "edge invocation tracker conflicts with durable journal for {request_id}: active generation {active_generation}, incoming {delivery_generation}"
                                        ).into());
                                    }
                                };
                                let executor = executor.clone();
                                let completed_tx = completed_tx.clone();
                                tracing::info!(tool = %tool, request_id = %request_id, generation = delivery_generation, "Executing tool");
                                tasks.spawn(async move {
                                    let _execution_permit = execution_permit;
                                    let start = Instant::now();
                                    let execution = async {
                                        if Instant::now() >= execution_deadline || cancel.is_cancelled() {
                                            return astra_tools::ToolResult::error("Server-issued execution deadline expired before dispatch".into());
                                        }
                                        executor.execute(EdgeInvocation {
                                            identity: *identity,
                                            delivery_generation,
                                            tool: tool.clone(),
                                            args: tool_args,
                                            execution_deadline,
                                            command_timeout_cap_ms,
                                            execution_ceiling,
                                            runtime_process_authorization,
                                            input_rx,
                                        }, cancel.clone()).await
                                    };
                                    // The executor owns asynchronous subprocess cleanup.
                                    // Dropping its future on cancellation strands children.
                                    tokio::pin!(execution);
                                    let result = tokio::select! {
                                        result = &mut execution => result,
                                        _ = tokio::time::sleep_until(tokio::time::Instant::from_std(execution_deadline)) => {
                                            cancel.cancel();
                                            let observed = execution.await;
                                            deadline_result(&tool, observed)
                                        }
                                    };
                                    let completion = CompletedEdgeInvocation {
                                        request_id,
                                        generation: delivery_generation,
                                        result,
                                        duration_ms: start.elapsed().as_millis() as u64,
                                    };
                                    let _ = completed_tx.send(completion).await;
                                });
                            }
                            Ok(EdgeServerMessage::ToolInput {
                                request_id,
                                delivery_generation,
                                input,
                            }) => {
                                let (ack_tx, ack_rx) = oneshot::channel();
                                if let Err(error) = input.validate() {
                                    let ack = ProviderStageInputAck::rejected(
                                        &input,
                                        error.to_string(),
                                    );
                                    write
                                        .send(Message::Text(
                                            serde_json::to_string(&EdgeClientMessage::ToolInputAck {
                                                request_id,
                                                delivery_generation,
                                                ack,
                                            })?
                                            .into(),
                                        ))
                                        .await?;
                                    continue;
                                }
                                let execution_generation = journal
                                    .running_execution_generation(&request_id, delivery_generation);
                                let input = EdgeInvocationInput { input, ack: ack_tx };
                                let sent = match execution_generation {
                                    Some(generation) => invocations.send_input(&request_id, generation, input),
                                    None => Err(input),
                                };
                                if let Err(input) = sent {
                                    let ack = ProviderStageInputAck::rejected(
                                        &input.input,
                                        "provider invocation is no longer accepting input",
                                    );
                                    write
                                        .send(Message::Text(
                                            serde_json::to_string(&EdgeClientMessage::ToolInputAck {
                                                request_id,
                                                delivery_generation,
                                                ack,
                                            })?
                                            .into(),
                                        ))
                                        .await?;
                                    continue;
                                }
                                let input_ack_tx = input_ack_tx.clone();
                                tokio::spawn(async move {
                                    // A dropped adapter acknowledgement is a
                                    // transport failure. Do not invent a
                                    // third business disposition; the pool's
                                    // bounded wait will retry the same input
                                    // identity on the next opportunity.
                                    if let Ok(ack) = ack_rx.await {
                                        let _ = input_ack_tx
                                            .send(CompletedProviderStageInput {
                                                request_id,
                                                generation: delivery_generation,
                                                ack,
                                            })
                                            .await;
                                    }
                                });
                            }
                            Ok(EdgeServerMessage::Pong {}) => {
                                // heartbeat ack
                            }
                            Ok(EdgeServerMessage::ToolCancel { request_id, delivery_generation }) => {
                                let execution_generation = journal
                                    .running_execution_generation(&request_id, delivery_generation);
                                if execution_generation.is_some_and(|generation| {
                                    invocations.cancel_if_current(&request_id, generation)
                                }) {
                                    tracing::info!(
                                        request_id = %request_id,
                                        delivery_generation,
                                        "Cancelled in-flight edge invocation"
                                    );
                                } else {
                                    tracing::debug!(request_id = %request_id, "Ignoring cancellation for non-active edge invocation");
                                }
                            }
                            Ok(EdgeServerMessage::ToolResultAck { request_id, delivery_generation }) => {
                                if !journal.acknowledge(&request_id, delivery_generation).await? {
                                    tracing::warn!(
                                        request_id = %request_id,
                                        delivery_generation,
                                        "Ignoring stale or unknown edge result acknowledgement"
                                    );
                                }
                            }
                            Ok(EdgeServerMessage::Closing { reason }) => {
                                tracing::info!(reason = %reason, "Server closing connection");
                                break;
                            }
                            Ok(EdgeServerMessage::AuthOk { .. } | EdgeServerMessage::AuthError { .. }) => {
                                // ignore duplicate auth
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "Failed to parse server message");
                            }
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        let _ = write.send(Message::Pong(data)).await;
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        tracing::info!("Connection closed");
                        break;
                    }
                    Some(Err(error)) => return Err(error.into()),
                    _ => {}
                }
            }
            Some(completed) = completed_rx.recv() => {
                if !invocations.finish_if_current(&completed.request_id, completed.generation) {
                    tracing::warn!(
                        request_id = %completed.request_id,
                        generation = completed.generation,
                        "Discarding stale edge invocation completion"
                    );
                    continue;
                }
                let pending = persist_completion(journal, completed).await?;
                let result_msg = pending.result.client_message(
                    pending.request_id,
                    pending.identity,
                    pending.delivery_generation,
                );
                write.send(Message::Text(serde_json::to_string(&result_msg)?.into())).await?;
            }
            Some(completed) = input_ack_rx.recv() => {
                let message = EdgeClientMessage::ToolInputAck {
                    request_id: completed.request_id,
                    delivery_generation: completed.generation,
                    ack: completed.ack,
                };
                write.send(Message::Text(serde_json::to_string(&message)?.into())).await?;
            }
            _ = heartbeat.tick() => {
                let ping = EdgeClientMessage::Ping {};
                if write.send(Message::Text(serde_json::to_string(&ping)?.into())).await.is_err() {
                    tracing::warn!("Failed to send heartbeat");
                    break;
                }
            }
        }
        }

        Ok(())
    }.await
}

async fn persist_completion(
    journal: &mut EdgeInvocationJournal,
    completed: CompletedEdgeInvocation,
) -> Result<invocation_journal::PendingResult, JournalError> {
    let pending = journal
        .complete(
            &completed.request_id,
            completed.generation,
            DurableEdgeResult::from_tool_result(completed.result, completed.duration_ms),
        )
        .await?;
    tracing::info!(
        request_id = %completed.request_id,
        generation = completed.generation,
        duration_ms = completed.duration_ms,
        is_error = pending.result.is_error,
        output_len = pending.result.output.len(),
        "Tool execution complete"
    );
    Ok(pending)
}

async fn settle_invocations(
    tasks: &mut JoinSet<()>,
    completed_rx: &mut mpsc::Receiver<CompletedEdgeInvocation>,
    journal: &mut EdgeInvocationJournal,
    mut journal_writable: bool,
    shutdown: &CancellationToken,
    invocations: &EdgeInvocationTracker,
) -> Result<(), EdgeConnectionError> {
    let mut failure: Option<EdgeConnectionError> = None;
    let mut shutdown_observed = shutdown.is_cancelled();
    if shutdown_observed {
        invocations.cancel_all();
    }
    // Spawned tasks continue running while we receive. Drain while joining:
    // queued completions have already released their execution permits, so
    // even a queue larger than the concurrency budget can fill up.
    // The persistent owner keeps its sender for reconnect. Task completion,
    // not channel closure, is therefore the settlement boundary.
    while !tasks.is_empty() || !completed_rx.is_empty() {
        tokio::select! {
            biased;
            _ = shutdown.cancelled(), if !shutdown_observed => {
                shutdown_observed = true;
                invocations.cancel_all();
                continue;
            }
            Some(completed) = completed_rx.recv() => {
                if journal_writable {
                    let request_id = completed.request_id.clone();
                    if let Err(error) = persist_completion(journal, completed).await {
                        tracing::error!(component = "edge", operation = "settle_invocations", stage = "persist_result", request_id = %request_id, error = %error, "Failed to persist completion during connection cleanup");
                        journal_writable = false;
                        failure = Some(Box::new(error));
                    }
                }
            }
            joined = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Err(error)) = joined {
                    tracing::error!(component = "edge", operation = "settle_invocations", stage = "join", error = %error, "Edge invocation task failed during cleanup");
                    if failure.is_none() {
                        failure = Some(Box::new(error));
                    }
                }
            }
        }
    }
    failure.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoDispatchExecutor;
    impl EdgeInvocationExecutor for NoDispatchExecutor {
        fn execute(
            &self,
            _: EdgeInvocation,
            _: CancellationToken,
        ) -> BoxFuture<'_, astra_tools::ToolResult> {
            Box::pin(async { panic!("readiness failure must not dispatch a tool") })
        }
    }

    struct BlockingExecutor {
        started: std::sync::Mutex<Option<oneshot::Sender<()>>>,
        release: Arc<tokio::sync::Notify>,
    }

    impl EdgeInvocationExecutor for BlockingExecutor {
        fn execute(
            &self,
            _: EdgeInvocation,
            _: CancellationToken,
        ) -> BoxFuture<'_, astra_tools::ToolResult> {
            let started = self.started.lock().unwrap().take();
            let release = self.release.clone();
            Box::pin(async move {
                if let Some(started) = started {
                    let _ = started.send(());
                }
                release.notified().await;
                astra_tools::ToolResult::text("finished".to_string())
            })
        }
    }

    #[tokio::test]
    async fn disconnect_reconnect_preserves_admitted_invocation_control() {
        use futures_util::{SinkExt, StreamExt};

        struct ControlledExecutor {
            started: std::sync::Mutex<Option<oneshot::Sender<()>>>,
            executions: std::sync::atomic::AtomicUsize,
            inputs: std::sync::atomic::AtomicUsize,
            cancellations: std::sync::atomic::AtomicUsize,
        }
        impl EdgeInvocationExecutor for ControlledExecutor {
            fn execute(
                &self,
                mut invocation: EdgeInvocation,
                cancel: CancellationToken,
            ) -> BoxFuture<'_, astra_tools::ToolResult> {
                Box::pin(async move {
                    self.executions
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if let Some(started) = self.started.lock().unwrap().take() {
                        let _ = started.send(());
                    }
                    loop {
                        tokio::select! {
                            _ = cancel.cancelled() => {
                                self.cancellations.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                return astra_tools::ToolResult::text("cancelled".into());
                            }
                            Some(input) = invocation.input_rx.recv() => {
                                self.inputs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                let _ = input.ack.send(ProviderStageInputAck::accepted(&input.input, None));
                            }
                        }
                    }
                })
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let journal_path = directory.path().join("journal.json");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = CancellationToken::new();
        let _shutdown_guard = shutdown.clone().drop_guard();
        let (started_tx, started_rx) = oneshot::channel();
        let executor = Arc::new(ControlledExecutor {
            started: std::sync::Mutex::new(Some(started_tx)),
            executions: std::sync::atomic::AtomicUsize::new(0),
            inputs: std::sync::atomic::AtomicUsize::new(0),
            cancellations: std::sync::atomic::AtomicUsize::new(0),
        });
        let server_shutdown = shutdown.clone();
        let server_executor = executor.clone();
        let server_workspace = directory.path().to_owned();
        let server_journal = journal_path.clone();
        let server = tokio::spawn(async move {
            let context = || EdgeConnectionContext {
                account_id: "user".into(),
                edge_agent_id: "edge".into(),
                workspace_dir: server_workspace.clone(),
                journal_path: server_journal.clone(),
                ready: None,
            };
            let mut owner = EdgeInvocationOwner::open(&context(), server_executor)
                .await
                .unwrap();
            for connection in 0..2 {
                if connection == 1 {
                    // Capability refresh changes future admission only. The
                    // running invocation must keep its original executor.
                    owner.replace_executor(Arc::new(NoDispatchExecutor));
                }
                let (stream, _) = listener.accept().await.unwrap();
                let socket = tokio_tungstenite::accept_async(MaybeTlsStream::Plain(stream))
                    .await
                    .unwrap();
                owner
                    .serve_connection(socket, context(), server_shutdown.clone(), None)
                    .await
                    .unwrap();
            }
            owner.settle(&server_shutdown).await.unwrap();
        });
        let (mut first_socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}"))
            .await
            .unwrap();
        let identity = astra_server_types::edge_ws_protocol::ToolInvocationIdentity::new(
            "user", "session", "run", "turn", "call",
        )
        .unwrap();
        let request_id = identity.storage_key();
        let mut request = EdgeServerMessage::ToolRequest {
            request_id: request_id.clone(),
            identity: Box::new(identity),
            delivery_generation: 1,
            tool: "native_provider".into(),
            args: serde_json::json!({}),
            runtime_process_authorization: None,
            runtime_process_authorization_required: false,
            timeout_secs: 60,
            execution_deadline_unix_ms: None,
            execution_timeout_ms: None,
            command_timeout_cap_ms: None,
            execution_ceiling: None,
        };
        first_socket
            .send(Message::Text(
                serde_json::to_string(&request).unwrap().into(),
            ))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), started_rx)
            .await
            .unwrap()
            .unwrap();
        first_socket.close(None).await.unwrap();
        assert_eq!(
            executor
                .cancellations
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );

        let (mut second_socket, _) = tokio::time::timeout(
            Duration::from_secs(3),
            tokio_tungstenite::connect_async(format!("ws://{address}")),
        )
        .await
        .expect("disconnect must release transport without waiting for the invocation")
        .unwrap();
        // Redelivery advances only the delivery generation, not the running
        // invocation's execution generation. Neither input nor cancellation
        // may require a second execution to reach the original adapter.
        if let EdgeServerMessage::ToolRequest {
            delivery_generation,
            ..
        } = &mut request
        {
            *delivery_generation = 2;
        }
        second_socket
            .send(Message::Text(
                serde_json::to_string(&request).unwrap().into(),
            ))
            .await
            .unwrap();
        second_socket
            .send(Message::Text(
                serde_json::to_string(&EdgeServerMessage::ToolCancel {
                    request_id: request_id.clone(),
                    delivery_generation: 1,
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        // These controls are sent on the replacement transport, not through
        // the tracker directly. The production owner must consume them.
        second_socket
            .send(Message::Text(
                serde_json::to_string(&EdgeServerMessage::ToolInput {
                    request_id: request_id.clone(),
                    delivery_generation: 2,
                    input: ProviderStageInput::Text {
                        input_id: "input-1".into(),
                        content: "continue".into(),
                        correlation_id: None,
                        expected_turn_id: None,
                    },
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        let input_ack = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let frame = second_socket.next().await.unwrap().unwrap();
                let Message::Text(text) = frame else { continue };
                if let EdgeClientMessage::ToolInputAck {
                    request_id: returned_id,
                    delivery_generation,
                    ack,
                } = serde_json::from_str::<EdgeClientMessage>(&text).unwrap()
                {
                    assert_eq!(returned_id, request_id);
                    assert_eq!(delivery_generation, 2);
                    assert_eq!(ack.input_id, "input-1");
                    assert!(ack.accepted);
                    break;
                }
            }
        })
        .await;
        assert!(
            input_ack.is_ok(),
            "replacement transport did not receive input ack"
        );
        assert_eq!(executor.inputs.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            executor
                .cancellations
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        second_socket
            .send(Message::Text(
                serde_json::to_string(&EdgeServerMessage::ToolCancel {
                    request_id: request_id.clone(),
                    delivery_generation: 2,
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let frame = second_socket.next().await.unwrap().unwrap();
                let Message::Text(text) = frame else { continue };
                if let EdgeClientMessage::ToolResult {
                    request_id: returned_id,
                    delivery_generation,
                    output,
                    is_error,
                    ..
                } = serde_json::from_str::<EdgeClientMessage>(&text).unwrap()
                {
                    assert_eq!(returned_id, request_id);
                    assert_eq!(delivery_generation, 2);
                    assert_eq!(output, "cancelled");
                    assert!(!is_error);
                    break;
                }
            }
        })
        .await;
        assert!(
            result.is_ok(),
            "replacement transport did not receive completion"
        );
        assert_eq!(
            executor
                .executions
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            executor
                .cancellations
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    #[tokio::test]
    async fn capability_withdrawal_keeps_journal_identity_for_running_work() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        let directory = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (started_tx, started_rx) = oneshot::channel();
        let release = Arc::new(tokio::sync::Notify::new());
        let executor = Arc::new(BlockingExecutor {
            started: std::sync::Mutex::new(Some(started_tx)),
            release: release.clone(),
        });
        let shutdown = CancellationToken::new();
        let withdrawal = CancellationToken::new();
        let (ready, ready_rx) = oneshot::channel();
        let journal_path = directory.path().join("journal.json");
        let server = tokio::spawn({
            let shutdown = shutdown.clone();
            let withdrawal = withdrawal.clone();
            let workspace = directory.path().to_owned();
            async move {
                let (stream, _) = listener.accept().await.unwrap();
                let socket = tokio_tungstenite::accept_async(MaybeTlsStream::Plain(stream))
                    .await
                    .unwrap();
                serve_connection_with_drain(
                    socket,
                    EdgeConnectionContext {
                        account_id: "user".into(),
                        edge_agent_id: "edge".into(),
                        workspace_dir: workspace,
                        journal_path,
                        ready: Some(ready),
                    },
                    executor,
                    shutdown,
                    Some(withdrawal),
                )
                .await
            }
        });
        let (mut client, _) = tokio_tungstenite::connect_async(format!("ws://{address}"))
            .await
            .unwrap();
        ready_rx.await.unwrap();

        let identity = astra_server_types::edge_ws_protocol::ToolInvocationIdentity::new(
            "user", "session", "run", "turn", "call",
        )
        .unwrap();
        let request = EdgeServerMessage::ToolRequest {
            request_id: identity.storage_key(),
            identity: Box::new(identity.clone()),
            delivery_generation: 1,
            tool: "native_provider".into(),
            args: serde_json::json!({}),
            execution_ceiling: None,
            runtime_process_authorization: None,
            runtime_process_authorization_required: false,
            timeout_secs: 30,
            execution_deadline_unix_ms: None,
            execution_timeout_ms: None,
            command_timeout_cap_ms: None,
        };
        client
            .send(Message::Text(
                serde_json::to_string(&request).unwrap().into(),
            ))
            .await
            .unwrap();
        started_rx.await.unwrap();

        withdrawal.cancel();
        let new_identity = astra_server_types::edge_ws_protocol::ToolInvocationIdentity::new(
            "user", "session", "run", "turn", "new-call",
        )
        .unwrap();
        let new_request = EdgeServerMessage::ToolRequest {
            request_id: new_identity.storage_key(),
            identity: Box::new(new_identity.clone()),
            delivery_generation: 1,
            tool: "native_provider".into(),
            args: serde_json::json!({}),
            execution_ceiling: None,
            runtime_process_authorization: None,
            runtime_process_authorization_required: false,
            timeout_secs: 30,
            execution_deadline_unix_ms: None,
            execution_timeout_ms: None,
            command_timeout_cap_ms: None,
        };
        client
            .send(Message::Text(
                serde_json::to_string(&new_request).unwrap().into(),
            ))
            .await
            .unwrap();
        let new_result = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let frame = client.next().await.unwrap().unwrap();
                let Message::Text(text) = frame else { continue };
                let message: EdgeClientMessage = serde_json::from_str(&text).unwrap();
                if let EdgeClientMessage::ToolResult { .. } = message {
                    break message;
                }
            }
        })
        .await
        .unwrap();
        match new_result {
            EdgeClientMessage::ToolResult {
                identity,
                is_error,
                tool_result_fields,
                ..
            } => {
                assert_eq!(identity, new_identity);
                assert!(is_error);
                assert_eq!(
                    tool_result_fields.unwrap()["outcome_certainty"],
                    "not_dispatched"
                );
            }
            other => panic!("expected new work rejection, got {other:?}"),
        }

        // The duplicate is still Running. It must join the journal identity,
        // not be turned into a new not-dispatched result while draining.
        client
            .send(Message::Text(
                serde_json::to_string(&request).unwrap().into(),
            ))
            .await
            .unwrap();
        release.notify_waiters();
        let original_result = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let frame = client.next().await.unwrap().unwrap();
                let Message::Text(text) = frame else { continue };
                let message: EdgeClientMessage = serde_json::from_str(&text).unwrap();
                if let EdgeClientMessage::ToolResult { .. } = message {
                    break message;
                }
            }
        })
        .await
        .unwrap();
        match original_result {
            EdgeClientMessage::ToolResult {
                identity: returned_identity,
                output,
                is_error,
                ..
            } => {
                assert_eq!(returned_identity, identity);
                assert_eq!(output, "finished");
                assert!(!is_error);
            }
            other => panic!("expected original completion, got {other:?}"),
        }
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn failed_recovery_and_cancelled_installation_never_publish_ready() {
        for cancelled in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let journal_path = directory.path().join("journal.json");
            tokio::fs::write(&journal_path, b"invalid journal")
                .await
                .unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                let _ = socket.next().await;
            });
            let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}"))
                .await
                .unwrap();
            let (ready, receiver) = oneshot::channel();
            let shutdown = CancellationToken::new();
            if cancelled {
                shutdown.cancel();
            }
            let result = serve_connection(
                socket,
                EdgeConnectionContext {
                    account_id: "account".into(),
                    edge_agent_id: "edge".into(),
                    workspace_dir: directory.path().to_owned(),
                    journal_path,
                    ready: Some(ready),
                },
                Arc::new(NoDispatchExecutor),
                shutdown,
            )
            .await;
            assert_eq!(result.is_ok(), cancelled);
            assert!(
                receiver.await.is_err(),
                "failure/cancellation cannot install capacity"
            );
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn cancellation_interrupts_authentication_wait_without_returning_a_socket() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (received, request_received) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            assert!(socket.next().await.unwrap().is_ok());
            received.send(()).unwrap();
            let _ = socket.next().await;
        });
        let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}"))
            .await
            .unwrap();
        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let client = tokio::spawn(async move {
            authenticate_connection(
                socket,
                EdgeClientMessage::Auth {
                    edge_agent_id: "edge".into(),
                    materialization_id: "materialization".into(),
                    interaction_api_major: astra_server_types::AGENT_INTERACTION_API_MAJOR.into(),
                    hostname: None,
                    workspace_dir: None,
                    capabilities: None,
                },
                Some("account"),
                &task_cancellation,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(3), request_received)
            .await
            .unwrap()
            .unwrap();
        cancellation.cancel();
        let result = tokio::time::timeout(Duration::from_secs(3), client)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(result, Err(EdgeAuthenticationError::Cancelled)));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn authentication_uses_server_identity_and_rejects_untrusted_acknowledgements() {
        let cases = [
            (
                serde_json::json!({"type":"edge_auth_ok","user_id":"account","edge_id":"ws-account","interaction_api_major":astra_server_types::AGENT_INTERACTION_API_MAJOR}),
                None,
            ),
            (
                serde_json::json!({"type":"edge_auth_ok","user_id":"other","edge_id":"ws-other","interaction_api_major":astra_server_types::AGENT_INTERACTION_API_MAJOR}),
                Some("mismatch"),
            ),
            (
                serde_json::json!({"type":"edge_auth_ok","user_id":" ","edge_id":"ws-invalid-account","interaction_api_major":astra_server_types::AGENT_INTERACTION_API_MAJOR}),
                Some("account"),
            ),
            (
                serde_json::json!({"type":"edge_auth_ok","user_id":"account","edge_id":"ws-invalid-contract","interaction_api_major":"invalid"}),
                Some("contract"),
            ),
            (
                serde_json::json!({"type":"edge_auth_error","message":"private server details"}),
                Some("rejected"),
            ),
            (Value::Null, Some("closed")),
        ];
        for (response, failure) in cases {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                let request = socket.next().await.unwrap().unwrap();
                let Message::Text(text) = request else {
                    panic!("expected authentication envelope")
                };
                assert!(matches!(
                    serde_json::from_str::<EdgeClientMessage>(&text).unwrap(),
                    EdgeClientMessage::Auth { .. }
                ));
                if response.is_null() {
                    socket.send(Message::Close(None)).await.unwrap();
                } else {
                    socket
                        .send(Message::Text(response.to_string().into()))
                        .await
                        .unwrap();
                }
            });
            let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}"))
                .await
                .unwrap();
            let auth = EdgeClientMessage::Auth {
                edge_agent_id: "edge".into(),
                materialization_id: "materialization".into(),
                interaction_api_major: astra_server_types::AGENT_INTERACTION_API_MAJOR.into(),
                hostname: None,
                workspace_dir: None,
                capabilities: None,
            };
            let result =
                authenticate_connection(socket, auth, Some("account"), &CancellationToken::new())
                    .await;
            match failure {
                None => {
                    let (_, account, edge_id) = result.unwrap();
                    assert_eq!(account, "account");
                    assert_eq!(edge_id, "ws-account");
                }
                Some(kind) => {
                    let error = result.unwrap_err();
                    assert_eq!(error.is_permanent(), kind != "closed");
                    assert!(!error.to_string().contains("private server details"));
                    assert!(matches!(
                        (kind, error),
                        ("mismatch", EdgeAuthenticationError::AccountMismatch)
                            | ("account", EdgeAuthenticationError::InvalidAccount)
                            | ("contract", EdgeAuthenticationError::IncompatibleContract)
                            | ("rejected", EdgeAuthenticationError::Rejected)
                            | (
                                "closed",
                                EdgeAuthenticationError::ClosedBeforeAuthentication
                            )
                    ));
                }
            }
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn cleanup_drains_a_full_completion_queue_and_preserves_results() {
        assert_cleanup_drains(false).await;
    }

    #[tokio::test]
    async fn cleanup_drains_senders_even_when_persistence_fails() {
        assert_cleanup_drains(true).await;
    }

    #[tokio::test]
    async fn pending_result_probe_distinguishes_optional_capacity_from_recovery() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("journal.json");
        assert!(!has_pending_invocation_results(path.clone()).await);
        assert!(!tokio::fs::try_exists(&path).await.unwrap());

        let identity = astra_server_types::edge_ws_protocol::ToolInvocationIdentity::new(
            "user", "session", "run", "turn", "call",
        )
        .unwrap();
        let request_id = identity.storage_key();
        let mut journal = EdgeInvocationJournal::open(path.clone()).await.unwrap();
        journal
            .prepare(
                &request_id,
                &identity,
                1,
                "native_provider",
                &serde_json::json!({}),
                false,
                None,
            )
            .await
            .unwrap();
        drop(journal);
        assert!(has_pending_invocation_results(path.clone()).await);

        let mut journal = EdgeInvocationJournal::open(path.clone()).await.unwrap();
        assert!(journal.acknowledge(&request_id, 1).await.unwrap());
        drop(journal);
        assert!(!has_pending_invocation_results(path).await);
    }

    #[tokio::test]
    async fn shutdown_during_settlement_cancels_and_joins_admitted_work() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("journal.json");
        let identity = astra_server_types::edge_ws_protocol::ToolInvocationIdentity::new(
            "user", "session", "run", "turn", "call",
        )
        .unwrap();
        let request_id = identity.storage_key();
        let mut journal = EdgeInvocationJournal::open(path).await.unwrap();
        journal
            .prepare(
                &request_id,
                &identity,
                1,
                "native_provider",
                &serde_json::json!({}),
                true,
                None,
            )
            .await
            .unwrap();

        let mut tracker = EdgeInvocationTracker::default();
        let (cancel, _input_rx) = tracker.begin(&request_id, 1).unwrap();
        let (completed_tx, mut completed_rx) = mpsc::channel(1);
        let mut tasks = JoinSet::new();
        let task_cancel = cancel.clone();
        tasks.spawn(async move {
            task_cancel.cancelled().await;
            completed_tx
                .send(CompletedEdgeInvocation {
                    request_id,
                    generation: 1,
                    result: astra_tools::ToolResult::text("cancelled".to_string()),
                    duration_ms: 1,
                })
                .await
                .unwrap();
        });
        let shutdown = CancellationToken::new();
        let mut settle = Box::pin(settle_invocations(
            &mut tasks,
            &mut completed_rx,
            &mut journal,
            true,
            &shutdown,
            &tracker,
        ));
        tokio::select! {
            result = &mut settle => panic!("settlement completed before shutdown: {result:?}"),
            _ = tokio::time::sleep(Duration::from_millis(1)) => shutdown.cancel(),
        }
        tokio::time::timeout(Duration::from_secs(1), &mut settle)
            .await
            .unwrap()
            .unwrap();
        drop(settle);
        assert!(tasks.is_empty());
        assert!(cancel.is_cancelled());
    }

    async fn assert_cleanup_drains(fail_first_completion: bool) {
        let state = tempfile::tempdir().unwrap();
        let path = state.path().join("journal.json");
        let mut journal = EdgeInvocationJournal::open(path.clone()).await.unwrap();
        let (tx, mut rx) = mpsc::channel(1);
        let mut tasks = JoinSet::new();
        for i in 0..4 {
            let identity = astra_server_types::edge_ws_protocol::ToolInvocationIdentity::new(
                "user",
                "session",
                "run",
                "turn",
                format!("completion-{i}"),
            )
            .unwrap();
            let request_id = identity.storage_key();
            journal
                .prepare(
                    &request_id,
                    &identity,
                    1,
                    "bash",
                    &serde_json::json!({}),
                    true,
                    None,
                )
                .await
                .unwrap();
            let completion = CompletedEdgeInvocation {
                request_id: if fail_first_completion && i == 0 {
                    "missing-record".into()
                } else {
                    request_id
                },
                generation: 1,
                result: astra_tools::ToolResult::text(format!("finished-{i}")),
                duration_ms: i,
            };
            if i == 0 {
                tx.send(completion).await.unwrap();
            } else {
                let tx = tx.clone();
                tasks.spawn(async move {
                    tx.send(completion).await.unwrap();
                });
            }
        }
        assert_eq!(rx.len(), 1);
        drop(tx);
        let shutdown = CancellationToken::new();
        let invocations = EdgeInvocationTracker::default();
        let settled = tokio::time::timeout(
            Duration::from_secs(5),
            settle_invocations(
                &mut tasks,
                &mut rx,
                &mut journal,
                true,
                &shutdown,
                &invocations,
            ),
        )
        .await
        .unwrap();
        assert!(tasks.is_empty());
        if fail_first_completion {
            let error = settled.unwrap_err();
            assert!(matches!(
                error.downcast_ref::<JournalError>(),
                Some(JournalError::Corrupt { .. })
            ));
            assert_eq!(
                journal.status().running,
                4,
                "stop appending after integrity failure"
            );
            drop(journal);
            let restored = EdgeInvocationJournal::open(path).await.unwrap();
            let pending = restored.pending_results().unwrap();
            assert_eq!(pending.len(), 4);
            assert!(pending.iter().all(
                |result| result.result.tool_result_fields.as_ref().unwrap()["outcome_certainty"]
                    == "unknown"
            ));
            return;
        }
        settled.unwrap();
        drop(journal);
        let restored = EdgeInvocationJournal::open(path).await.unwrap();
        let mut outputs = restored
            .pending_results()
            .unwrap()
            .into_iter()
            .map(|pending| {
                assert!(!pending.result.is_error);
                pending.result.output
            })
            .collect::<Vec<_>>();
        outputs.sort();
        assert_eq!(
            outputs,
            ["finished-0", "finished-1", "finished-2", "finished-3"]
        );
    }

    #[test]
    fn edge_work_deadline_is_absolute_and_not_the_generic_command_timeout() {
        let work_deadline = Instant::now() + Duration::from_secs(600);
        assert!(
            command_deadline(work_deadline, 60, Some(10_000), false)
                .saturating_duration_since(Instant::now())
                <= Duration::from_secs(10)
        );
        assert_eq!(
            command_deadline(work_deadline, 60, Some(10_000), true),
            work_deadline
        );
        let unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        assert!(edge_execution_deadline(30, Some(unix_ms - 1), Some(60_000)).is_err());
        assert!(edge_execution_deadline(30, Some(unix_ms + 60_000), None).is_err());
        assert!(edge_execution_deadline(30, None, Some(60_000)).is_err());
        assert!(edge_execution_deadline(30, Some(unix_ms + 60_000), Some(0)).is_err());
        let deadline =
            edge_execution_deadline(30, Some(unix_ms + 86_400_000), Some(43_200_000)).unwrap();
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(remaining > Duration::from_secs(43_199));
        assert!(remaining <= Duration::from_secs(43_200));
        let absolute =
            edge_execution_deadline(30, Some(unix_ms + 1_000), Some(86_400_000)).unwrap();
        assert!(absolute.saturating_duration_since(Instant::now()) <= Duration::from_secs(1));
    }

    #[test]
    fn deadline_result_preserves_observed_output_and_metadata() {
        let result = deadline_result(
            "native_provider",
            astra_tools::ToolResult {
                output: "partial provider output".into(),
                metadata: Some(serde_json::Map::from_iter([(
                    "native_session".into(),
                    serde_json::Value::String("session-1".into()),
                )])),
                is_error: false,
                exit_semantics: None,
            },
        );

        assert!(result.is_error);
        assert!(result.output.contains("partial provider output"));
        assert_eq!(
            result.metadata.as_ref().and_then(|metadata| {
                metadata
                    .get("native_session")
                    .and_then(|value| value.as_str())
            }),
            Some("session-1")
        );
        assert_eq!(
            result
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("execution_deadline_exceeded")),
            Some(&serde_json::Value::Bool(true))
        );
    }

    #[test]
    fn process_authorization_fails_closed_without_live_credential() {
        use astra_server_types::edge_ws_protocol::RuntimeProcessAuthorizationContext;

        let context = RuntimeProcessAuthorizationContext {
            authorization: "Bearer runtime-grant".to_string(),
        };
        assert!(valid_runtime_process_authorization(
            "bash",
            true,
            Some(&context)
        ));
        assert!(valid_runtime_process_authorization("bash", false, None));
        assert!(!valid_runtime_process_authorization("bash", true, None));
        assert!(!valid_runtime_process_authorization(
            "read_file",
            true,
            Some(&context)
        ));
    }

    #[test]
    fn invocation_tracker_deduplicates_and_fences_stale_completions() {
        let mut tracker = EdgeInvocationTracker::default();
        let generation = 7;
        tracker.begin("request-1", generation).unwrap();
        assert_eq!(tracker.begin("request-1", 8).unwrap_err(), generation);
        assert!(!tracker.finish_if_current("request-1", generation + 1));
        assert_eq!(tracker.begin("request-1", 8).unwrap_err(), generation);
        assert!(tracker.finish_if_current("request-1", generation));

        let next_generation = generation + 1;
        tracker.begin("request-1", next_generation).unwrap();
        assert!(!tracker.finish_if_current("request-1", generation));
        assert!(tracker.finish_if_current("request-1", next_generation));
    }

    #[test]
    fn invocation_tracker_routes_cancellation_to_the_exact_active_request() {
        let mut tracker = EdgeInvocationTracker::default();
        let first_generation = 1;
        let first_cancel = tracker.begin("request-1", first_generation).unwrap();
        let second_cancel = tracker.begin("request-2", 2).unwrap();

        assert!(!tracker.cancel_if_current("request-1", first_generation + 1));
        assert!(!first_cancel.0.is_cancelled());
        assert!(tracker.cancel_if_current("request-1", first_generation));
        assert!(first_cancel.0.is_cancelled());
        assert!(!second_cancel.0.is_cancelled());
        assert!(!tracker.cancel_if_current("missing", first_generation));
    }

    #[test]
    fn invocation_tracker_fences_input_to_generation_and_bounds_queue() {
        let mut tracker = EdgeInvocationTracker::default();
        let generation = 4;
        let (_cancel, mut input_rx) = tracker.begin("request-1", generation).unwrap();
        let input = || EdgeInvocationInput {
            input: ProviderStageInput::Text {
                input_id: uuid::Uuid::new_v4().to_string(),
                content: "continue".into(),
                correlation_id: None,
                expected_turn_id: None,
            },
            ack: tokio::sync::oneshot::channel().0,
        };
        assert!(
            tracker
                .send_input("request-1", generation + 1, input())
                .is_err()
        );
        for _ in 0..MAX_PROVIDER_STAGE_INPUTS_PER_INVOCATION {
            assert!(tracker.send_input("request-1", generation, input()).is_ok());
        }
        assert!(
            tracker
                .send_input("request-1", generation, input())
                .is_err()
        );
        assert!(input_rx.try_recv().is_ok());
    }

    #[test]
    fn execution_budget_admits_exactly_the_configured_concurrency() {
        let budget = EdgeExecutionBudget::new();
        let permits = (0..MAX_CONCURRENT_TOOL_EXECUTIONS)
            .map(|_| budget.try_acquire().expect("configured execution permit"))
            .collect::<Vec<_>>();
        assert!(
            budget.try_acquire().is_none(),
            "the first invocation beyond the execution budget must be rejected before dispatch"
        );
        drop(permits);
        assert!(
            budget.try_acquire().is_some(),
            "completed executions must release capacity"
        );
    }
}

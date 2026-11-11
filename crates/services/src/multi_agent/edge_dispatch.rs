//! Edge dispatch relay: cross-pod tool dispatch via DB-backed queue.
//!
//! When the in-memory edge ledger times out or a tool targets an agent connected
//! to a different pod, the dispatch relay persists the request and the
//! owning pod's process-wide wake observer notifies its edge WS handler for
//! delivery. The observer also recovers work committed before subscription.
//! Results flow back through `deliver_result`.
//!
//! Split from the monolithic `multi_agent.rs`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{FutureExt, TryStreamExt};
use sqlx::{MySql, Row};

use super::metrics::{SharedMultiAgentMetrics, saturating_decrement};
use crate::db_row::RowExt as EdgeDispatchDbRow;
use crate::interaction_contract::{InteractionStatus, edge_dispatch_status};

#[derive(Debug)]
pub struct EdgeDispatchRow {
    pub user_id: String,
    pub session_id: String,
    pub run_id: String,
    pub turn_chain_id: String,
    pub edge_agent_id: String,
    pub request_id: String,
    pub payload_json: String,
    pub result_json: Option<String>,
    pub status: String,
    pub pending_wait_us: u64,
}

impl EdgeDispatchRow {
    pub fn identity(&self) -> EdgeDispatchIdentity {
        EdgeDispatchIdentity::new(
            self.user_id.clone(),
            self.session_id.clone(),
            self.run_id.clone(),
            self.turn_chain_id.clone(),
            self.request_id.clone(),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EdgeDispatchIdentity {
    pub user_id: String,
    pub session_id: String,
    pub run_id: String,
    pub turn_chain_id: String,
    pub request_id: String,
}

/// A bounded, read-only observation of a dispatch belonging to one run.
#[derive(Debug, Clone)]
pub struct EdgeDispatchObservation {
    pub edge_owner_user_id: String,
    pub turn_chain_id: String,
    pub request_id: String,
    pub edge_agent_id: String,
    pub status: String,
    pub tool_name: Option<String>,
    /// Present only for an exact read when the stored result fits the limit.
    pub result_json: Option<String>,
    pub result_too_large: bool,
}

impl EdgeDispatchIdentity {
    pub fn new(
        user_id: impl Into<String>,
        session_id: impl Into<String>,
        run_id: impl Into<String>,
        turn_chain_id: impl Into<String>,
        request_id: impl Into<String>,
    ) -> Self {
        Self {
            user_id: user_id.into(),
            session_id: session_id.into(),
            run_id: run_id.into(),
            turn_chain_id: turn_chain_id.into(),
            request_id: request_id.into(),
        }
    }

    pub fn is_complete(&self) -> bool {
        !self.user_id.trim().is_empty()
            && !self.session_id.trim().is_empty()
            && !self.run_id.trim().is_empty()
            && !self.turn_chain_id.trim().is_empty()
            && !self.request_id.trim().is_empty()
    }

    pub fn for_request_id(&self, request_id: impl Into<String>) -> Self {
        Self {
            user_id: self.user_id.clone(),
            session_id: self.session_id.clone(),
            run_id: self.run_id.clone(),
            turn_chain_id: self.turn_chain_id.clone(),
            request_id: request_id.into(),
        }
    }

    fn request_id_json_value(&self) -> serde_json::Value {
        serde_json::json!({
            "user_id": self.user_id,
            "session_id": self.session_id,
            "run_id": self.run_id,
            "turn_chain_id": self.turn_chain_id,
            "request_id": self.request_id,
        })
    }
}

fn json_payloads_match(persisted: &str, replayed: &str) -> Result<bool, String> {
    let persisted = serde_json::from_str::<serde_json::Value>(persisted)
        .map_err(|error| format!("persisted durable payload is invalid JSON: {error}"))?;
    let replayed = serde_json::from_str::<serde_json::Value>(replayed)
        .map_err(|error| format!("replayed durable payload is invalid JSON: {error}"))?;
    Ok(persisted == replayed)
}

fn dispatch_run_owner(
    identity: &EdgeDispatchIdentity,
    invocation_json: Option<&str>,
) -> Result<String, String> {
    let Some(invocation_json) = invocation_json else {
        // The interactive tool-request lane is owned by the run user itself.
        return Ok(identity.user_id.clone());
    };
    let invocation: astra_turn_types::ToolInvocationIdentity =
        serde_json::from_str(invocation_json).map_err(|_| "invalid dispatch identity")?;
    if invocation.session_id != identity.session_id
        || invocation.run_id != identity.run_id
        || invocation.turn_chain_id != identity.turn_chain_id
        || invocation.storage_key() != identity.request_id
    {
        return Err("dispatch invocation identity does not match its key".into());
    }
    Ok(invocation.user_id)
}

async fn rollback_edge_dispatch_tx(tx: sqlx::Transaction<'_, MySql>, context: &'static str) {
    if let Err(error) = tx.rollback().await {
        tracing::warn!(context, %error, "edge_dispatch rollback failed");
    }
}

#[async_trait]
pub trait EdgeDispatchService: Send + Sync {
    /// Atomically admit a durable request, preserving its serialized payload.
    async fn admit_dispatch(
        &self,
        _identity: &EdgeDispatchIdentity,
        _edge_agent_id: &str,
        _payload: &serde_json::Value,
    ) -> Result<EdgeDispatchAdmission, EdgeDispatchAdmissionError> {
        Err(EdgeDispatchAdmissionError::Rejected(
            "durable Edge admission is unavailable".to_string(),
        ))
    }

    /// Atomically admit and claim a request for direct socket delivery.
    async fn admit_and_claim_direct_dispatch(
        &self,
        _identity: &EdgeDispatchIdentity,
        _edge_agent_id: &str,
        _payload: &serde_json::Value,
    ) -> Result<EdgeDirectDispatchAdmission, EdgeDispatchAdmissionError> {
        Err(EdgeDispatchAdmissionError::Rejected(
            "direct Edge admission is unavailable".to_string(),
        ))
    }

    /// Resource limits for durable claim, delivery, and settlement work.
    fn pending_limits(&self) -> Option<&EdgeDispatchLimits> {
        None
    }

    /// Record a direct dispatch that was not sent but whose durable
    /// terminalization could not be confirmed. Adapters exposing dispatch
    /// metrics should override this hook.
    fn record_unconfirmed_direct_settlement(&self) {}

    /// Claim pending dispatches for a (user, agent) pair.
    async fn poll_pending(
        &self,
        user_id: &str,
        edge_agent_id: &str,
    ) -> Result<Vec<EdgeDispatchRow>, String>;

    /// Settle claimed rows the owner has not attempted to send with their
    /// canonical non-execution receipts. Concurrent terminal results win.
    async fn settle_unsent(
        &self,
        _edge_agent_id: &str,
        _receipts: &[(EdgeDispatchIdentity, String)],
    ) -> Result<(), String> {
        Err("settling unsent edge dispatches is unavailable".to_string())
    }

    /// Subscribe to durable-admission wakeups. Database-backed implementations
    /// must discover work admitted by other pods and before subscription.
    /// `None` means durable dispatch is not configured for this connection.
    async fn subscribe_pending_wakeup(
        &self,
        _user_id: &str,
        _edge_agent_id: &str,
    ) -> Result<Option<tokio::sync::watch::Receiver<u64>>, String> {
        Err("edge dispatch wake notifications are unavailable".to_string())
    }

    /// Deliver a tool result (from HTTP callback or WS) — updates status to 'completed'.
    /// The full dispatch identity and `edge_agent_id` must match the dispatch
    /// record to prevent cross-owner, cross-run, or cross-agent injection.
    async fn deliver_result(
        &self,
        identity: &EdgeDispatchIdentity,
        edge_agent_id: &str,
        result_json: &str,
    ) -> Result<bool, String>;

    /// Terminal runs may settle only a previously claimed dispatch or replay
    /// the same completed result. An unclaimed pending request was not sent.
    async fn deliver_terminal_result(
        &self,
        _identity: &EdgeDispatchIdentity,
        _edge_agent_id: &str,
        _result_json: &str,
    ) -> Result<bool, String> {
        Err("terminal dispatch settlement is unavailable".into())
    }

    /// Whether this exact dispatch was already terminalized as a server-owned
    /// cancellation.  The callback endpoint may acknowledge a late
    /// `cancelled` result in that one case: the durable terminal outcome wins,
    /// and accepting it cannot reopen or overwrite the dispatch.  This is
    /// deliberately narrower than a generic terminal-row probe so a divergent
    /// replay of a completed result remains a protocol error.
    async fn is_server_cancelled_dispatch(
        &self,
        _identity: &EdgeDispatchIdentity,
        _edge_agent_id: &str,
    ) -> Result<bool, String> {
        Ok(false)
    }

    /// Move an in-flight dispatch to a failed terminal state.
    async fn fail_dispatch(
        &self,
        identity: &EdgeDispatchIdentity,
        edge_agent_id: &str,
        reason: &str,
    ) -> Result<bool, String>;

    /// Expire a request only if the Edge has not claimed it. A claimed
    /// request may still produce a result after the caller stops waiting.
    async fn expire_pending_dispatch(
        &self,
        _identity: &EdgeDispatchIdentity,
        _edge_agent_id: &str,
    ) -> Result<bool, String> {
        Err("pending dispatch expiration is unavailable".into())
    }

    /// Poll for a specific request's result. Returns Some(result_json) when completed.
    /// Some(zero) performs one exact durable read without subscribing;
    /// Some(nonzero) bounds the wait; None retains the subscription until
    /// resolution or caller cancellation. No path treats cache absence as
    /// evidence that a durable result does not exist.
    async fn wait_result(
        &self,
        identity: &EdgeDispatchIdentity,
        timeout: Option<std::time::Duration>,
    ) -> Result<Option<String>, String>;

    /// Explicitly inspect one run's retained dispatches. This is never used
    /// for ordinary callbacks or normal child result collection.
    async fn inspect_run_dispatches(
        &self,
        _user_id: &str,
        _session_id: &str,
        _run_id: &str,
        _after: Option<(&str, &str)>,
        _exact: Option<(&str, &str)>,
    ) -> Result<Vec<EdgeDispatchObservation>, String> {
        Err("durable tool outcome inspection is unavailable".into())
    }

    /// Resolve a callback's run owner from the exact, authenticated Edge
    /// dispatch. This is only needed when Edge and run owners differ.
    async fn callback_run_owner(
        &self,
        _identity: &EdgeDispatchIdentity,
        _edge_agent_id: &str,
    ) -> Result<Option<String>, String> {
        Err("dispatch callback owner is unavailable".into())
    }

    /// Clean up stale dispatches older than `older_than`.
    async fn cleanup_stale(&self, older_than: std::time::Duration) -> Result<u64, String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EdgeDispatchAdmission {
    Pending,
    Terminal(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EdgeDirectDispatchAdmission {
    Claimed,
    Observing,
    Terminal(String),
}

/// Certainty at the durable admission boundary matters independently from
/// whether the transport operation itself returned an error.
///
/// `Rejected` proves that this requested dispatch was not admitted. In
/// contrast, `OutcomeUnknown` means the durable insert or its verification may
/// have committed before the caller lost the response, so retrying through a
/// different transport could duplicate an external side effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeDispatchFailureStage {
    Begin,
    Insert,
    Verify,
    Claim,
    Commit,
    Rollback,
    TerminalResult,
    Adapter,
}

impl std::fmt::Display for EdgeDispatchFailureStage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Begin => "database transaction start",
            Self::Insert => "database insert",
            Self::Verify => "database verification",
            Self::Claim => "dispatch claim",
            Self::Commit => "database commit",
            Self::Rollback => "database rollback",
            Self::TerminalResult => "terminal result verification",
            Self::Adapter => "dispatch adapter",
        })
    }
}

/// Only fixed, non-secret information may cross the admission boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeDispatchFailureCause {
    Unspecified,
    PoolTimeout,
    PoolClosed,
    Connection,
    Tls,
    AccessDenied,
    Database(Option<u16>),
    InvalidData,
}

impl std::fmt::Display for EdgeDispatchFailureCause {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unspecified => formatter.write_str("unspecified cause"),
            Self::PoolTimeout => formatter.write_str("connection acquisition timed out"),
            Self::PoolClosed => formatter.write_str("connection pool is closed"),
            Self::Connection => formatter.write_str("database connection failed"),
            Self::Tls => formatter.write_str("database TLS failed"),
            Self::AccessDenied => formatter.write_str("database access denied"),
            Self::Database(Some(code)) => write!(formatter, "database error (code {code})"),
            Self::Database(None) => formatter.write_str("database error"),
            Self::InvalidData => formatter.write_str("invalid database response"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EdgeDispatchFailure {
    pub stage: EdgeDispatchFailureStage,
    pub cause: EdgeDispatchFailureCause,
}

impl From<EdgeDispatchFailureStage> for EdgeDispatchFailure {
    fn from(stage: EdgeDispatchFailureStage) -> Self {
        Self {
            stage,
            cause: EdgeDispatchFailureCause::Unspecified,
        }
    }
}

impl std::fmt::Display for EdgeDispatchFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.stage)?;
        if self.cause != EdgeDispatchFailureCause::Unspecified {
            write!(formatter, ": {}", self.cause)?;
        }
        Ok(())
    }
}

fn edge_admission_database_error(
    stage: EdgeDispatchFailureStage,
    error: sqlx::Error,
) -> EdgeDispatchAdmissionError {
    use EdgeDispatchFailureCause as Cause;
    let cause = match &error {
        sqlx::Error::PoolTimedOut => Cause::PoolTimeout,
        sqlx::Error::PoolClosed => Cause::PoolClosed,
        sqlx::Error::Io(_) => Cause::Connection,
        sqlx::Error::Tls(_) => Cause::Tls,
        sqlx::Error::Database(database) => {
            let number = database
                .try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>()
                .map(|error| error.number());
            if matches!(number, Some(1044 | 1045 | 1142 | 1143 | 1227))
                || matches!(database.code().as_deref(), Some("28000" | "42501"))
            {
                Cause::AccessDenied
            } else {
                Cause::Database(number)
            }
        }
        sqlx::Error::ColumnNotFound(_)
        | sqlx::Error::ColumnDecode { .. }
        | sqlx::Error::Decode(_)
        | sqlx::Error::Protocol(_) => Cause::InvalidData,
        _ => Cause::Unspecified,
    };
    EdgeDispatchAdmissionError::OutcomeUnknown(EdgeDispatchFailure { stage, cause })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EdgeDispatchAdmissionError {
    Rejected(String),
    OutcomeUnknown(EdgeDispatchFailure),
}

impl std::fmt::Display for EdgeDispatchAdmissionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(message) => formatter.write_str(message),
            Self::OutcomeUnknown(failure) => write!(formatter, "{failure}"),
        }
    }
}

impl std::error::Error for EdgeDispatchAdmissionError {}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EdgeAdmissionMode {
    Relay,
    Direct,
}

pub struct DatabaseEdgeDispatchService {
    pool: sqlx::Pool<sqlx::MySql>,
    metrics: Option<SharedMultiAgentMetrics>,
    wait_coordinator: Arc<EdgeDispatchWaitCoordinator>,
    wake_hub: Arc<EdgeDispatchWakeHub>,
    limits: EdgeDispatchLimits,
}

/// Per-service limits bound database work and retained settlement receipts.
pub struct EdgeDispatchLimits {
    pub claim: tokio::sync::Semaphore,
    pub receipts: tokio::sync::Semaphore,
    pub settlement: tokio::sync::Semaphore,
}

impl EdgeDispatchLimits {
    pub fn new(pool_size: u32) -> Self {
        let claims = dispatch_claim_concurrency(pool_size) as usize;
        Self {
            claim: tokio::sync::Semaphore::new(claims),
            receipts: tokio::sync::Semaphore::new(128),
            settlement: tokio::sync::Semaphore::new(claims.min(4)),
        }
    }
}

impl DatabaseEdgeDispatchService {
    pub fn new(pool: sqlx::Pool<sqlx::MySql>) -> Self {
        let wait_coordinator = Arc::new(EdgeDispatchWaitCoordinator::new(pool.clone()));
        let wake_hub = Arc::new(EdgeDispatchWakeHub::new(pool.clone()));
        // Reserve pool capacity for heartbeats and normal requests if a slow
        // wake query causes many Edges to probe at once.
        let limits = EdgeDispatchLimits::new(pool.options().get_max_connections());
        Self {
            pool,
            metrics: None,
            wait_coordinator,
            wake_hub,
            limits,
        }
    }

    pub fn from_shared(shared: &astra_core::SharedPool) -> Self {
        Self::new(shared.get().clone())
    }

    pub fn with_metrics(mut self, metrics: SharedMultiAgentMetrics) -> Self {
        let _ = self.wait_coordinator.metrics.set(metrics.clone());
        self.metrics = Some(metrics);
        self
    }

    async fn admit(
        &self,
        identity: &EdgeDispatchIdentity,
        edge_agent_id: &str,
        payload: &serde_json::Value,
        mode: EdgeAdmissionMode,
    ) -> Result<EdgeDirectDispatchAdmission, EdgeDispatchAdmissionError> {
        use EdgeDispatchAdmissionError::{OutcomeUnknown, Rejected};
        use EdgeDispatchFailureStage as Stage;

        if !identity.is_complete() {
            return Err(Rejected("edge dispatch identity is incomplete".to_string()));
        }
        if edge_agent_id.trim().is_empty() {
            return Err(Rejected("edge dispatch owner is required".to_string()));
        }
        let payload_json = payload.to_string();
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| edge_admission_database_error(Stage::Begin, error))?;
        let insert = match mode {
            EdgeAdmissionMode::Relay => {
                "INSERT IGNORE INTO edge_pending_dispatch \
                 (user_id, session_id, run_id, turn_chain_id, edge_agent_id, request_id, payload_json, status) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, 'pending')"
            }
            EdgeAdmissionMode::Direct => {
                "INSERT IGNORE INTO edge_pending_dispatch \
                 (user_id, session_id, run_id, turn_chain_id, edge_agent_id, request_id, payload_json, status, dispatched_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, 'dispatched', NOW(6))"
            }
        };
        let inserted = sqlx::query(insert)
            .bind(&identity.user_id)
            .bind(&identity.session_id)
            .bind(&identity.run_id)
            .bind(&identity.turn_chain_id)
            .bind(edge_agent_id)
            .bind(&identity.request_id)
            .bind(&payload_json)
            .execute(&mut *tx)
            .await
            .map_err(|error| edge_admission_database_error(Stage::Insert, error))?
            .rows_affected()
            > 0;
        let row = sqlx::query(
            "SELECT edge_agent_id, payload_json, status, result_json \
             FROM edge_pending_dispatch \
             WHERE user_id = ? AND session_id = ? AND run_id = ? \
               AND turn_chain_id = ? AND request_id = ? FOR UPDATE",
        )
        .bind(&identity.user_id)
        .bind(&identity.session_id)
        .bind(&identity.run_id)
        .bind(&identity.turn_chain_id)
        .bind(&identity.request_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| edge_admission_database_error(Stage::Verify, error))?
        .ok_or(OutcomeUnknown(Stage::Verify.into()))?;
        let stored_edge: String = row
            .try_get("edge_agent_id")
            .map_err(|error| edge_admission_database_error(Stage::Verify, error))?;
        let stored_payload: String = row
            .try_get("payload_json")
            .map_err(|error| edge_admission_database_error(Stage::Verify, error))?;
        if stored_edge != edge_agent_id || stored_payload != payload_json {
            tx.rollback()
                .await
                .map_err(|error| edge_admission_database_error(Stage::Rollback, error))?;
            return Err(Rejected(format!(
                "edge_dispatch identity {} conflicts with its durable edge owner or payload",
                identity.request_id
            )));
        }
        let status: String = row
            .try_get("status")
            .map_err(|error| edge_admission_database_error(Stage::Verify, error))?;
        let wake = mode == EdgeAdmissionMode::Relay && status == "pending";
        let outcome = match status.as_str() {
            "pending" if mode == EdgeAdmissionMode::Direct => {
                let updated = sqlx::query(
                    "UPDATE edge_pending_dispatch SET status = 'dispatched', dispatched_at = NOW(6) \
                     WHERE user_id = ? AND session_id = ? AND run_id = ? AND turn_chain_id = ? \
                       AND request_id = ? AND edge_agent_id = ? AND status = 'pending'",
                )
                .bind(&identity.user_id)
                .bind(&identity.session_id)
                .bind(&identity.run_id)
                .bind(&identity.turn_chain_id)
                .bind(&identity.request_id)
                .bind(edge_agent_id)
                .execute(&mut *tx)
                .await
                .map_err(|error| edge_admission_database_error(Stage::Claim, error))?;
                if updated.rows_affected() != 1 {
                    return Err(OutcomeUnknown(Stage::Claim.into()));
                }
                EdgeDirectDispatchAdmission::Claimed
            }
            "pending" => EdgeDirectDispatchAdmission::Observing,
            "dispatched" if mode == EdgeAdmissionMode::Direct && inserted => {
                EdgeDirectDispatchAdmission::Claimed
            }
            "dispatched" => EdgeDirectDispatchAdmission::Observing,
            "completed" | "failed" => {
                let result = row
                    .try_get::<Option<String>, _>("result_json")
                    .map_err(|error| edge_admission_database_error(Stage::TerminalResult, error))?
                    .ok_or(OutcomeUnknown(Stage::TerminalResult.into()))?;
                EdgeDirectDispatchAdmission::Terminal(result)
            }
            _ => return Err(OutcomeUnknown(Stage::Verify.into())),
        };
        tx.commit()
            .await
            .map_err(|error| edge_admission_database_error(Stage::Commit, error))?;
        if inserted && let Some(metrics) = &self.metrics {
            metrics.dispatch_queue_depth.fetch_add(1, Ordering::Relaxed);
        }
        if wake {
            self.wake_hub.notify(&identity.user_id, edge_agent_id).await;
        }
        Ok(outcome)
    }
}

fn dispatch_claim_concurrency(pool_size: u32) -> u32 {
    (pool_size / 4).clamp(1, 16)
}

fn notify_live_subscribers(
    subscribers: &mut HashMap<(String, String), tokio::sync::watch::Sender<u64>>,
) {
    subscribers.retain(|_, sender| {
        if sender.receiver_count() == 0 {
            false
        } else {
            sender.send_modify(|generation| *generation = generation.saturating_add(1));
            true
        }
    });
}

struct EdgeDispatchWakeHub {
    subscribers: tokio::sync::Mutex<HashMap<(String, String), tokio::sync::watch::Sender<u64>>>,
    pool: Option<sqlx::Pool<MySql>>,
    running: AtomicBool,
}

struct WakeQueryConnection {
    inner: sqlx::pool::PoolConnection<MySql>,
    completed: bool,
}

#[derive(Debug, thiserror::Error)]
enum WakeQueryError {
    #[error("edge dispatch wake observer has no database pool")]
    Unconfigured,
    #[error("edge dispatch wake pool acquisition: {0}")]
    Acquire(#[source] sqlx::Error),
    #[error("edge dispatch wake query: {0}")]
    Query(#[source] sqlx::Error),
    #[error("edge dispatch wake result decode: {0}")]
    Decode(#[source] sqlx::Error),
}

impl WakeQueryError {
    fn query_outcome_unknown(&self) -> bool {
        matches!(self, Self::Query(error) if !matches!(error, sqlx::Error::Database(_)))
    }
}

impl Drop for WakeQueryConnection {
    fn drop(&mut self) {
        if !self.completed {
            // Dropping a SQLx query does not necessarily stop its server-side
            // execution. Never return an unfinished observer query to the pool.
            self.inner.close_on_drop();
        }
    }
}

impl Default for EdgeDispatchWakeHub {
    fn default() -> Self {
        Self {
            subscribers: tokio::sync::Mutex::new(HashMap::new()),
            pool: None,
            running: AtomicBool::new(false),
        }
    }
}

impl EdgeDispatchWakeHub {
    fn new(pool: sqlx::Pool<MySql>) -> Self {
        Self {
            pool: Some(pool),
            ..Self::default()
        }
    }

    async fn subscribe(
        self: &Arc<Self>,
        user_id: &str,
        edge_agent_id: &str,
    ) -> tokio::sync::watch::Receiver<u64> {
        let key = (user_id.to_owned(), edge_agent_id.to_owned());
        let mut subscribers = self.subscribers.lock().await;
        if let Some(sender) = subscribers.get(&key)
            && sender.receiver_count() > 0
        {
            let receiver = sender.subscribe();
            drop(subscribers);
            self.ensure_cross_pod_observer();
            return receiver;
        }
        let (sender, receiver) = tokio::sync::watch::channel(0);
        subscribers.insert(key, sender);
        drop(subscribers);
        self.ensure_cross_pod_observer();
        receiver
    }

    async fn notify(&self, user_id: &str, edge_agent_id: &str) {
        let key = (user_id.to_owned(), edge_agent_id.to_owned());
        let mut subscribers = self.subscribers.lock().await;
        Self::notify_key(&mut subscribers, &key);
    }

    fn notify_key(
        subscribers: &mut HashMap<(String, String), tokio::sync::watch::Sender<u64>>,
        key: &(String, String),
    ) {
        if let Some(sender) = subscribers.get(key) {
            if sender.receiver_count() > 0 {
                sender.send_modify(|generation| *generation = generation.saturating_add(1));
            } else {
                subscribers.remove(key);
            }
        }
    }

    fn ensure_cross_pod_observer(self: &Arc<Self>) {
        if self.pool.is_none()
            || self
                .running
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return;
        }
        let observer = self.clone();
        tokio::spawn(async move {
            loop {
                match std::panic::AssertUnwindSafe(observer.clone().run())
                    .catch_unwind()
                    .await
                {
                    Ok(Ok(())) => return,
                    Ok(Err(error)) => tracing::warn!(
                        %error,
                        "edge dispatch wake observation lost; retrying after recovery"
                    ),
                    Err(_) => tracing::error!(
                        "edge dispatch wake observer panicked; retrying after recovery"
                    ),
                }
                // A disconnected query may still be executing on MatrixOne.
                // Limit retries while recovery wakes continue to make progress.
                if tokio::time::timeout(
                    EDGE_DISPATCH_WAKE_UNKNOWN_RETRY_DELAY,
                    observer.clone().run_recovery_only(),
                )
                .await
                .is_ok()
                {
                    return;
                }
            }
        });
    }

    async fn run_recovery_only(self: Arc<Self>) {
        loop {
            {
                let mut subscribers = self.subscribers.lock().await;
                notify_live_subscribers(&mut subscribers);
                if subscribers.is_empty() {
                    self.running.store(false, Ordering::Release);
                    return;
                }
            }
            tokio::time::sleep(EDGE_DISPATCH_WAKE_RECOVERY_INTERVAL).await;
        }
    }

    async fn run(self: Arc<Self>) -> Result<(), WakeQueryError> {
        let mut retry_delay = EDGE_DISPATCH_WAKE_POLL_INTERVAL;
        let pool_size = self
            .pool
            .as_ref()
            .map_or(1, |pool| pool.options().get_max_connections());
        if pool_size <= dispatch_claim_concurrency(pool_size) + 1 {
            // A pool this small cannot reserve a connection for both the
            // observer and claims. Use the existing recovery wake instead.
            self.run_recovery_only().await;
            return Ok(());
        }
        loop {
            let targets = {
                let mut subscribers = self.subscribers.lock().await;
                subscribers.retain(|_, sender| sender.receiver_count() > 0);
                if subscribers.is_empty() {
                    // Keep this transition under the subscription lock: a new
                    // subscriber will start a replacement after the lock drops.
                    self.running.store(false, Ordering::Release);
                    return Ok(());
                }
                subscribers.keys().cloned().collect::<Vec<_>>()
            };

            let mut failed = false;
            let mut found_pending = false;
            for batch in targets.chunks(EDGE_DISPATCH_WAKE_BATCH_SIZE) {
                let mut recovery = tokio::time::interval_at(
                    tokio::time::Instant::now() + EDGE_DISPATCH_WAKE_RECOVERY_INTERVAL,
                    EDGE_DISPATCH_WAKE_RECOVERY_INTERVAL,
                );
                recovery.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                let mut stalled = false;
                let query = self.pending_targets(batch);
                tokio::pin!(query);
                let result = loop {
                    tokio::select! {
                        result = &mut query => break result,
                        _ = recovery.tick() => {
                            if !stalled {
                                tracing::warn!("edge dispatch wake observation is slow; using bounded recovery probes");
                                stalled = true;
                            }
                            // Keep the submitted query alive. Closing a client
                            // connection does not guarantee database cancellation.
                            let mut subscribers = self.subscribers.lock().await;
                            notify_live_subscribers(&mut subscribers);
                        }
                    }
                };
                match result {
                    Ok(pending) => {
                        found_pending |= !pending.is_empty();
                        let mut subscribers = self.subscribers.lock().await;
                        for key in pending {
                            Self::notify_key(&mut subscribers, &key);
                        }
                    }
                    Err(error) => {
                        if error.query_outcome_unknown() {
                            return Err(error);
                        }
                        failed = true;
                        tracing::warn!(
                            %error,
                            batch_size = batch.len(),
                            "edge dispatch wake observation failed; will retry"
                        );
                        for (user_id, edge_agent_id) in batch {
                            self.notify(user_id, edge_agent_id).await;
                        }
                    }
                }
            }
            if failed {
                retry_delay = retry_delay
                    .saturating_mul(2)
                    .min(EDGE_DISPATCH_WAKE_RETRY_MAX);
            } else {
                retry_delay = if found_pending {
                    EDGE_DISPATCH_WAKE_POLL_INTERVAL
                } else {
                    EDGE_DISPATCH_WAKE_IDLE_INTERVAL
                };
            }
            tokio::time::sleep(retry_delay).await;
        }
    }

    async fn pending_targets(
        &self,
        targets: &[(String, String)],
    ) -> Result<Vec<(String, String)>, WakeQueryError> {
        let pool = self.pool.as_ref().ok_or(WakeQueryError::Unconfigured)?;
        if !self.has_live_targets(targets).await {
            return Ok(Vec::new());
        }
        let mut query = sqlx::QueryBuilder::<MySql>::new(
            "SELECT user_id, edge_agent_id FROM edge_pending_dispatch
             WHERE status = 'pending' AND user_id IN (",
        );
        let users: HashSet<_> = targets.iter().map(|(user, _)| user.as_str()).collect();
        let edges: HashSet<_> = targets.iter().map(|(_, edge)| edge.as_str()).collect();
        for (index, user_id) in users.into_iter().enumerate() {
            if index > 0 {
                query.push(", ");
            }
            query.push_bind(user_id);
        }
        query.push(") AND edge_agent_id IN (");
        for (index, edge_agent_id) in edges.into_iter().enumerate() {
            if index > 0 {
                query.push(", ");
            }
            query.push_bind(edge_agent_id);
        }
        query.push(") GROUP BY user_id, edge_agent_id");
        let connection = pool.acquire().await.map_err(WakeQueryError::Acquire)?;
        if !self.has_live_targets(targets).await {
            return Ok(Vec::new());
        }
        let mut connection = WakeQueryConnection {
            inner: connection,
            completed: false,
        };
        let wanted: HashSet<_> = targets
            .iter()
            .map(|(user, edge)| (user.as_str(), edge.as_str()))
            .collect();
        let mut pending = Vec::new();
        let mut rows = query.build().fetch(&mut *connection.inner);
        while let Some(row) = rows.try_next().await.map_err(WakeQueryError::Query)? {
            let user_id: String = row.try_get("user_id").map_err(WakeQueryError::Decode)?;
            let edge_agent_id: String = row
                .try_get("edge_agent_id")
                .map_err(WakeQueryError::Decode)?;
            if wanted.contains(&(user_id.as_str(), edge_agent_id.as_str())) {
                pending.push((user_id, edge_agent_id));
            }
        }
        drop(rows);
        connection.completed = true;
        Ok(pending)
    }

    async fn has_live_targets(&self, targets: &[(String, String)]) -> bool {
        let subscribers = self.subscribers.lock().await;
        targets.iter().any(|target| {
            subscribers
                .get(target)
                .is_some_and(|sender| sender.receiver_count() > 0)
        })
    }
}

const EDGE_DISPATCH_WAIT_BATCH_SIZE: usize = 128;
const EDGE_DISPATCH_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const EDGE_DISPATCH_WAIT_UNKNOWN_ERROR_BUDGET: Duration = Duration::from_secs(30);
const EDGE_DISPATCH_WAKE_BATCH_SIZE: usize = 1024;
const EDGE_DISPATCH_WAKE_POLL_INTERVAL: Duration = Duration::from_millis(100);
const EDGE_DISPATCH_WAKE_IDLE_INTERVAL: Duration = Duration::from_millis(500);
const EDGE_DISPATCH_WAKE_RECOVERY_INTERVAL: Duration = Duration::from_secs(2);
const EDGE_DISPATCH_WAKE_RETRY_MAX: Duration = Duration::from_secs(2);
const EDGE_DISPATCH_WAKE_UNKNOWN_RETRY_DELAY: Duration = Duration::from_secs(10);

type WaitResolution = Result<Option<String>, String>;

struct EdgeDispatchWaitCoordinator {
    pool: sqlx::Pool<sqlx::MySql>,
    waiters: tokio::sync::Mutex<HashMap<EdgeDispatchIdentity, EdgeDispatchWaiter>>,
    running: AtomicBool,
    /// Attached by `DatabaseEdgeDispatchService::with_metrics`; unknown-outcome
    /// accounting is best effort when no metrics handle was attached.
    metrics: std::sync::OnceLock<SharedMultiAgentMetrics>,
}

struct EdgeDispatchWaiter {
    sender: tokio::sync::watch::Sender<Option<WaitResolution>>,
    unknown_since: Option<tokio::time::Instant>,
}

impl EdgeDispatchWaiter {
    fn should_retry(&mut self, failure: &ObservationFailure, now: tokio::time::Instant) -> bool {
        match failure {
            ObservationFailure::Permanent => false,
            ObservationFailure::Transient => true,
            ObservationFailure::Unknown => {
                now.duration_since(*self.unknown_since.get_or_insert(now))
                    < EDGE_DISPATCH_WAIT_UNKNOWN_ERROR_BUDGET
            }
        }
    }
}

enum PolledDispatchState {
    Pending,
    Terminal(String),
}

#[derive(Debug, thiserror::Error)]
enum WaitObservationError {
    #[error("edge_dispatch batched wait_result: {0}")]
    Query(#[source] sqlx::Error),
    #[error("{0}")]
    Invalid(String),
}

#[derive(Debug, PartialEq, Eq)]
enum ObservationFailure {
    Permanent,
    Transient,
    Unknown,
}

fn database_observation_failure(state: Option<&str>, number: Option<u16>) -> ObservationFailure {
    // MatrixOne sends its native error number alongside a generic HY000
    // SQLSTATE for both invalid input and temporary service failures.
    if number.is_some_and(|number| (20500..=20508).contains(&number))
        || matches!(number, Some(20634 | 20635))
        || number == Some(1461) // MySQL prepared-statement capacity exhausted.
        || state.is_some_and(|state| state.starts_with("08") || state.starts_with("40"))
    {
        ObservationFailure::Transient
    } else if number == Some(20301)
        || state.is_some_and(|state| state.starts_with("42") || state.starts_with("28"))
    {
        ObservationFailure::Permanent
    } else {
        ObservationFailure::Unknown
    }
}

impl From<String> for WaitObservationError {
    fn from(error: String) -> Self {
        Self::Invalid(error)
    }
}

impl WaitObservationError {
    fn classification(&self) -> ObservationFailure {
        match self {
            Self::Query(sqlx::Error::Database(error)) => database_observation_failure(
                error.code().as_deref(),
                error
                    .try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>()
                    .map(|error| error.number()),
            ),
            Self::Query(
                sqlx::Error::Io(_)
                | sqlx::Error::Tls(_)
                | sqlx::Error::Protocol(_)
                | sqlx::Error::PoolTimedOut
                | sqlx::Error::WorkerCrashed,
            ) => ObservationFailure::Transient,
            _ => ObservationFailure::Permanent,
        }
    }
}

impl EdgeDispatchWaitCoordinator {
    fn new(pool: sqlx::Pool<sqlx::MySql>) -> Self {
        Self {
            pool,
            waiters: tokio::sync::Mutex::new(HashMap::new()),
            running: AtomicBool::new(false),
            metrics: std::sync::OnceLock::new(),
        }
    }

    fn resolve_observation_error(
        &self,
        waiter: EdgeDispatchWaiter,
        failure: &ObservationFailure,
        error: &str,
    ) {
        // One identity can have multiple subscribers. Count the identity only
        // when at least one subscriber is still present to receive the error.
        if waiter.sender.send(Some(Err(error.to_string()))).is_ok()
            && matches!(failure, ObservationFailure::Unknown)
            && let Some(metrics) = self.metrics.get()
        {
            metrics
                .dispatch_observation_unknown_total
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    async fn subscribe(
        self: &Arc<Self>,
        identity: EdgeDispatchIdentity,
    ) -> tokio::sync::watch::Receiver<Option<WaitResolution>> {
        let receiver = {
            let mut waiters = self.waiters.lock().await;
            if let Some(waiter) = waiters.get(&identity) {
                waiter.sender.subscribe()
            } else {
                let (sender, receiver) = tokio::sync::watch::channel(None);
                waiters.insert(
                    identity,
                    EdgeDispatchWaiter {
                        sender,
                        unknown_since: None,
                    },
                );
                receiver
            }
        };
        if self
            .running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let coordinator = self.clone();
            tokio::spawn(async move { coordinator.run().await });
        }
        receiver
    }

    async fn run(self: Arc<Self>) {
        let mut retry_delay = EDGE_DISPATCH_WAIT_POLL_INTERVAL;
        let mut resume_at: Option<(EdgeDispatchIdentity, usize)> = None;
        loop {
            let mut identities = {
                let mut waiters = self.waiters.lock().await;
                waiters.retain(|_, waiter| waiter.sender.receiver_count() > 0);
                waiters.keys().cloned().collect::<Vec<_>>()
            };
            if identities.is_empty() {
                self.running.store(false, Ordering::Release);
                let has_waiters = !self.waiters.lock().await.is_empty();
                if !has_waiters
                    || self
                        .running
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_err()
                {
                    return;
                }
                continue;
            }

            let rotation = resume_at.as_ref().map_or(0, |(resume, fallback)| {
                identities
                    .iter()
                    .position(|identity| identity == resume)
                    .unwrap_or(*fallback % identities.len())
            });
            identities.rotate_left(rotation);
            let mut transient_failure = false;
            let mut pool_timed_out = false;
            for (batch_index, batch) in identities.chunks(EDGE_DISPATCH_WAIT_BATCH_SIZE).enumerate()
            {
                tracing::trace!(
                    waiter_count = identities.len(),
                    batch_size = batch.len(),
                    "edge_dispatch batched result observation"
                );
                match self.poll_batch(batch).await {
                    Ok(states) => {
                        let mut waiters = self.waiters.lock().await;
                        for identity in batch {
                            if let Some(waiter) = waiters.get_mut(identity) {
                                waiter.unknown_since = None;
                            }
                            match states.get(identity) {
                                Some(PolledDispatchState::Pending) => {}
                                Some(PolledDispatchState::Terminal(result)) => {
                                    if let Some(waiter) = waiters.remove(identity) {
                                        let _ = waiter.sender.send(Some(Ok(Some(result.clone()))));
                                    }
                                }
                                None => {
                                    if let Some(waiter) = waiters.remove(identity) {
                                        let _ = waiter.sender.send(Some(Ok(None)));
                                    }
                                }
                            }
                        }
                    }
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            batch_size = batch.len(),
                            "edge_dispatch batched result observation failed"
                        );
                        if matches!(
                            error,
                            WaitObservationError::Query(sqlx::Error::PoolTimedOut)
                        ) {
                            // Other batches share this pool. Do not repeat a
                            // full acquisition timeout for each one. Resume at
                            // this batch so earlier batches cannot monopolize
                            // a briefly available connection.
                            resume_at = batch.first().cloned().map(|identity| {
                                (
                                    identity,
                                    (rotation + batch_index * EDGE_DISPATCH_WAIT_BATCH_SIZE)
                                        % identities.len(),
                                )
                            });
                            pool_timed_out = true;
                            transient_failure = true;
                            break;
                        }
                        let failure = error.classification();
                        let now = tokio::time::Instant::now();
                        let mut waiters = self.waiters.lock().await;
                        for identity in batch {
                            let Some(waiter) = waiters.get_mut(identity) else {
                                continue;
                            };
                            if waiter.should_retry(&failure, now) {
                                transient_failure = true;
                            } else if let Some(waiter) = waiters.remove(identity) {
                                self.resolve_observation_error(
                                    waiter,
                                    &failure,
                                    &error.to_string(),
                                );
                            }
                        }
                    }
                }
            }
            if !pool_timed_out {
                resume_at = None;
            }
            if transient_failure {
                tokio::time::sleep(retry_delay).await;
                retry_delay = retry_delay.saturating_mul(2).min(Duration::from_secs(2));
            } else {
                retry_delay = EDGE_DISPATCH_WAIT_POLL_INTERVAL;
                tokio::time::sleep(EDGE_DISPATCH_WAIT_POLL_INTERVAL).await;
            }
        }
    }

    async fn poll_batch(
        &self,
        identities: &[EdgeDispatchIdentity],
    ) -> Result<HashMap<EdgeDispatchIdentity, PolledDispatchState>, WaitObservationError> {
        let mut query = sqlx::QueryBuilder::<MySql>::new(
            "SELECT user_id, session_id, run_id, turn_chain_id, request_id, \
                    status, result_json \
             FROM edge_pending_dispatch WHERE ",
        );
        for (index, identity) in identities.iter().enumerate() {
            if index > 0 {
                query.push(" OR ");
            }
            query
                .push("(user_id = ")
                .push_bind(&identity.user_id)
                .push(" AND session_id = ")
                .push_bind(&identity.session_id)
                .push(" AND run_id = ")
                .push_bind(&identity.run_id)
                .push(" AND turn_chain_id = ")
                .push_bind(&identity.turn_chain_id)
                .push(" AND request_id = ")
                .push_bind(&identity.request_id)
                .push(")");
        }
        let rows = query
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(WaitObservationError::Query)?;
        let mut states = HashMap::with_capacity(rows.len());
        for row in rows {
            let identity = EdgeDispatchIdentity::new(
                row.try_get::<String, _>("user_id")
                    .map_err(|error| format!("edge_dispatch wait user_id decode: {error}"))?,
                row.try_get::<String, _>("session_id")
                    .map_err(|error| format!("edge_dispatch wait session_id decode: {error}"))?,
                row.try_get::<String, _>("run_id")
                    .map_err(|error| format!("edge_dispatch wait run_id decode: {error}"))?,
                row.try_get::<String, _>("turn_chain_id")
                    .map_err(|error| format!("edge_dispatch wait turn_chain_id decode: {error}"))?,
                row.try_get::<String, _>("request_id")
                    .map_err(|error| format!("edge_dispatch wait request_id decode: {error}"))?,
            );
            let status: String = row
                .try_get("status")
                .map_err(|error| format!("edge_dispatch wait status decode: {error}"))?;
            let result_json = row
                .try_get::<Option<String>, _>("result_json")
                .map_err(|error| format!("edge_dispatch wait result_json decode: {error}"))?;
            let state = match edge_dispatch_status(&status, result_json.as_deref()) {
                InteractionStatus::Pending => PolledDispatchState::Pending,
                InteractionStatus::Resolved
                | InteractionStatus::Expired
                | InteractionStatus::Cancelled => {
                    PolledDispatchState::Terminal(result_json.ok_or_else(|| {
                        format!(
                            "edge_dispatch terminal identity {} has no result evidence",
                            identity.request_id
                        )
                    })?)
                }
            };
            states.insert(identity, state);
        }
        Ok(states)
    }

    async fn resolve(&self, identity: &EdgeDispatchIdentity, result_json: &str) {
        if let Some(waiter) = self.waiters.lock().await.remove(identity) {
            let _ = waiter.sender.send(Some(Ok(Some(result_json.to_string()))));
        }
    }
}

fn edge_dispatch_decode_error(context: &str, column: &'static str, error: sqlx::Error) -> String {
    format!("edge_dispatch {context} decode `{column}`: {error}")
}

fn decode_claimed_dispatch_row(row: &impl EdgeDispatchDbRow) -> Result<EdgeDispatchRow, String> {
    Ok(EdgeDispatchRow {
        user_id: row
            .string_column("user_id")
            .map_err(|e| edge_dispatch_decode_error("poll row", "user_id", e))?,
        session_id: row
            .string_column("session_id")
            .map_err(|e| edge_dispatch_decode_error("poll row", "session_id", e))?,
        run_id: row
            .string_column("run_id")
            .map_err(|e| edge_dispatch_decode_error("poll row", "run_id", e))?,
        turn_chain_id: row
            .string_column("turn_chain_id")
            .map_err(|e| edge_dispatch_decode_error("poll row", "turn_chain_id", e))?,
        edge_agent_id: row
            .string_column("edge_agent_id")
            .map_err(|e| edge_dispatch_decode_error("poll row", "edge_agent_id", e))?,
        request_id: row
            .string_column("request_id")
            .map_err(|e| edge_dispatch_decode_error("poll row", "request_id", e))?,
        payload_json: row
            .string_column("payload_json")
            .map_err(|e| edge_dispatch_decode_error("poll row", "payload_json", e))?,
        result_json: row
            .optional_string_column("result_json")
            .map_err(|e| edge_dispatch_decode_error("poll row", "result_json", e))?,
        status: "dispatched".to_string(),
        pending_wait_us: non_negative_i64_column(row, "pending_wait_us", "poll row")? as u64,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EdgeDispatchBacklogRow {
    pending_rows: u64,
    dispatched_rows: u64,
    oldest_pending_age_ms: u64,
    oldest_dispatched_age_ms: u64,
}

fn edge_dispatch_failed_result_json(request_id: serde_json::Value, output: String) -> String {
    serde_json::json!({
        "request_id": request_id,
        "status": "error",
        "output": output,
        "duration_ms": 0,
    })
    .to_string()
}

impl EdgeDispatchBacklogRow {
    fn empty() -> Self {
        Self {
            pending_rows: 0,
            dispatched_rows: 0,
            oldest_pending_age_ms: 0,
            oldest_dispatched_age_ms: 0,
        }
    }
}

const EDGE_DISPATCH_BACKLOG_SQL: &str = "SELECT \
    CAST(COALESCE(SUM(CASE WHEN status = 'pending' THEN 1 ELSE 0 END), 0) AS SIGNED) AS pending_rows, \
    CAST(COALESCE(SUM(CASE WHEN status = 'dispatched' THEN 1 ELSE 0 END), 0) AS SIGNED) AS dispatched_rows, \
    CAST(CASE \
        WHEN MIN(CASE WHEN status = 'pending' THEN created_at ELSE NULL END) IS NULL \
            OR MIN(CASE WHEN status = 'pending' THEN created_at ELSE NULL END) > NOW(6) THEN 0 \
        ELSE TIMESTAMPDIFF(MICROSECOND, MIN(CASE WHEN status = 'pending' THEN created_at ELSE NULL END), NOW(6)) \
    END AS SIGNED) AS oldest_pending_age_us, \
    CAST(CASE \
        WHEN MIN(CASE WHEN status = 'dispatched' THEN created_at ELSE NULL END) IS NULL \
            OR MIN(CASE WHEN status = 'dispatched' THEN created_at ELSE NULL END) > NOW(6) THEN 0 \
        ELSE TIMESTAMPDIFF(MICROSECOND, MIN(CASE WHEN status = 'dispatched' THEN created_at ELSE NULL END), NOW(6)) \
    END AS SIGNED) AS oldest_dispatched_age_us \
    FROM edge_pending_dispatch \
    WHERE status IN ('pending', 'dispatched')";

fn non_negative_i64_column(
    row: &impl EdgeDispatchDbRow,
    column: &'static str,
    context: &str,
) -> Result<i64, String> {
    let value = row
        .i64_column(column)
        .map_err(|e| edge_dispatch_decode_error(context, column, e))?;
    if value < 0 {
        return Err(format!(
            "edge_dispatch {context} decode `{column}`: negative value {value}"
        ));
    }
    Ok(value)
}

fn micros_to_millis(us: u64) -> u64 {
    us.saturating_add(999) / 1000
}

fn decode_backlog_row(row: &impl EdgeDispatchDbRow) -> Result<EdgeDispatchBacklogRow, String> {
    Ok(EdgeDispatchBacklogRow {
        pending_rows: non_negative_i64_column(row, "pending_rows", "backlog row")? as u64,
        dispatched_rows: non_negative_i64_column(row, "dispatched_rows", "backlog row")? as u64,
        oldest_pending_age_ms: micros_to_millis(non_negative_i64_column(
            row,
            "oldest_pending_age_us",
            "backlog row",
        )? as u64),
        oldest_dispatched_age_ms: micros_to_millis(non_negative_i64_column(
            row,
            "oldest_dispatched_age_us",
            "backlog row",
        )? as u64),
    })
}

fn apply_backlog_metrics(metrics: &SharedMultiAgentMetrics, backlog: EdgeDispatchBacklogRow) {
    metrics
        .dispatch_pending_rows
        .store(backlog.pending_rows, Ordering::Relaxed);
    metrics
        .dispatch_dispatched_rows
        .store(backlog.dispatched_rows, Ordering::Relaxed);
    metrics
        .dispatch_oldest_pending_age_ms
        .store(backlog.oldest_pending_age_ms, Ordering::Relaxed);
    metrics
        .dispatch_oldest_dispatched_age_ms
        .store(backlog.oldest_dispatched_age_ms, Ordering::Relaxed);
}

/// Refresh DB-authoritative edge dispatch backlog gauges.
///
/// Hot-path counters are process-local approximations. This scrape-time query
/// gives operators the cross-pod truth for queue depth and oldest pending age.
pub async fn refresh_edge_dispatch_backlog_metrics(
    shared: &astra_core::SharedPool,
    metrics: &SharedMultiAgentMetrics,
) -> Result<(), String> {
    let row = sqlx::query(EDGE_DISPATCH_BACKLOG_SQL)
        .fetch_optional(shared.get())
        .await
        .map_err(|e| format!("edge_dispatch backlog metrics SELECT: {e}"))?;
    let backlog = match row {
        Some(row) => decode_backlog_row(&row)?,
        None => EdgeDispatchBacklogRow::empty(),
    };
    apply_backlog_metrics(metrics, backlog);
    Ok(())
}

fn validate_claimed_dispatch_update_count(expected: usize, actual: u64) -> Result<(), String> {
    if actual == expected as u64 {
        return Ok(());
    }
    Err(format!(
        "edge_dispatch poll UPDATE claimed {expected} rows but updated {actual}"
    ))
}

impl DatabaseEdgeDispatchService {
    async fn fail_dispatch_matching(
        &self,
        identity: &EdgeDispatchIdentity,
        edge_agent_id: &str,
        reason: &str,
        pending_only: bool,
    ) -> Result<bool, String> {
        let output = format!("edge dispatch {reason}");
        let result_json =
            edge_dispatch_failed_result_json(identity.request_id_json_value(), output);
        let query = if pending_only {
            "UPDATE edge_pending_dispatch \
             SET status = 'failed', result_json = ?, completed_at = NOW(6) \
             WHERE user_id = ? AND session_id = ? AND run_id = ? AND turn_chain_id = ? \
               AND request_id = ? AND edge_agent_id = ? AND status = 'pending'"
        } else {
            "UPDATE edge_pending_dispatch \
             SET status = 'failed', result_json = ?, completed_at = NOW(6) \
             WHERE user_id = ? AND session_id = ? AND run_id = ? AND turn_chain_id = ? \
               AND request_id = ? AND edge_agent_id = ? \
               AND status IN ('pending', 'dispatched')"
        };
        let affected = sqlx::query(query)
            .bind(&result_json)
            .bind(&identity.user_id)
            .bind(&identity.session_id)
            .bind(&identity.run_id)
            .bind(&identity.turn_chain_id)
            .bind(&identity.request_id)
            .bind(edge_agent_id)
            .execute(&self.pool)
            .await
            .map_err(|e| format!("edge_dispatch fail_dispatch: {e}"))?
            .rows_affected()
            > 0;
        if affected && let Some(ref m) = self.metrics {
            saturating_decrement(&m.dispatch_queue_depth);
            m.dispatch_failed_total.fetch_add(1, Ordering::Relaxed);
        }
        if affected {
            self.wait_coordinator.resolve(identity, &result_json).await;
        }
        Ok(affected)
    }

    async fn deliver_result_matching(
        &self,
        identity: &EdgeDispatchIdentity,
        edge_agent_id: &str,
        result_json: &str,
        allow_pending: bool,
    ) -> Result<bool, String> {
        let start = std::time::Instant::now();
        let sql = if allow_pending {
            "UPDATE edge_pending_dispatch \
             SET status = 'completed', result_json = ?, completed_at = NOW(6) \
             WHERE user_id = ? AND session_id = ? AND run_id = ? AND turn_chain_id = ? \
               AND request_id = ? AND edge_agent_id = ? AND status IN ('pending', 'dispatched')"
        } else {
            "UPDATE edge_pending_dispatch \
             SET status = 'completed', result_json = ?, completed_at = NOW(6) \
             WHERE user_id = ? AND session_id = ? AND run_id = ? AND turn_chain_id = ? \
               AND request_id = ? AND edge_agent_id = ? AND status = 'dispatched'"
        };
        let n = sqlx::query(sql)
            .bind(result_json)
            .bind(&identity.user_id)
            .bind(&identity.session_id)
            .bind(&identity.run_id)
            .bind(&identity.turn_chain_id)
            .bind(&identity.request_id)
            .bind(edge_agent_id)
            .execute(&self.pool)
            .await
            .map_err(|e| format!("edge_dispatch deliver_result: {e}"))?;
        let updated = n.rows_affected() > 0;
        // A reconnect may replay a result whose first delivery committed but
        // whose ACK was lost. MatrixOne's JSON column may normalize object key
        // order and whitespace, so compare JSON values rather than storage
        // serialization. A semantically conflicting body remains rejected.
        let accepted = if updated {
            true
        } else {
            let row = sqlx::query(
                "SELECT status, result_json \
                 FROM edge_pending_dispatch \
                 WHERE user_id = ? AND session_id = ? AND run_id = ? AND turn_chain_id = ? \
                   AND request_id = ? AND edge_agent_id = ?",
            )
            .bind(&identity.user_id)
            .bind(&identity.session_id)
            .bind(&identity.run_id)
            .bind(&identity.turn_chain_id)
            .bind(&identity.request_id)
            .bind(edge_agent_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| format!("edge_dispatch verify replayed result: {e}"))?;
            match row {
                None => false,
                Some(row) => {
                    let status = row.try_get::<String, _>("status").map_err(|error| {
                        format!("edge_dispatch replay status decode failed: {error}")
                    })?;
                    if status != "completed" {
                        false
                    } else {
                        let body = row
                            .try_get::<Option<String>, _>("result_json")
                            .map_err(|error| {
                                format!("edge_dispatch replay result_json decode failed: {error}")
                            })?
                            .ok_or_else(|| {
                                "edge_dispatch completed replay row is missing result_json"
                                    .to_string()
                            })?;
                        json_payloads_match(&body, result_json)?
                    }
                }
            }
        };
        if let Some(ref m) = self.metrics {
            m.dispatch_deliver_update_latency.record(start.elapsed());
            if updated {
                saturating_decrement(&m.dispatch_queue_depth);
                m.dispatch_deliver_hits_total
                    .fetch_add(1, Ordering::Relaxed);
            } else if !accepted {
                m.dispatch_deliver_misses_total
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
        if accepted {
            self.wait_coordinator.resolve(identity, result_json).await;
        }
        Ok(accepted)
    }
}

#[async_trait]
impl EdgeDispatchService for DatabaseEdgeDispatchService {
    fn pending_limits(&self) -> Option<&EdgeDispatchLimits> {
        Some(&self.limits)
    }

    fn record_unconfirmed_direct_settlement(&self) {
        if let Some(metrics) = &self.metrics {
            metrics
                .dispatch_settlement_unknown_total
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    async fn admit_dispatch(
        &self,
        identity: &EdgeDispatchIdentity,
        edge_agent_id: &str,
        payload: &serde_json::Value,
    ) -> Result<EdgeDispatchAdmission, EdgeDispatchAdmissionError> {
        match self
            .admit(identity, edge_agent_id, payload, EdgeAdmissionMode::Relay)
            .await?
        {
            EdgeDirectDispatchAdmission::Terminal(result) => {
                Ok(EdgeDispatchAdmission::Terminal(result))
            }
            EdgeDirectDispatchAdmission::Claimed | EdgeDirectDispatchAdmission::Observing => {
                Ok(EdgeDispatchAdmission::Pending)
            }
        }
    }

    async fn admit_and_claim_direct_dispatch(
        &self,
        identity: &EdgeDispatchIdentity,
        edge_agent_id: &str,
        payload: &serde_json::Value,
    ) -> Result<EdgeDirectDispatchAdmission, EdgeDispatchAdmissionError> {
        self.admit(identity, edge_agent_id, payload, EdgeAdmissionMode::Direct)
            .await
    }

    async fn subscribe_pending_wakeup(
        &self,
        user_id: &str,
        edge_agent_id: &str,
    ) -> Result<Option<tokio::sync::watch::Receiver<u64>>, String> {
        Ok(Some(self.wake_hub.subscribe(user_id, edge_agent_id).await))
    }

    #[tracing::instrument(skip(self), fields(user_id = %user_id, edge_agent_id = %edge_agent_id))]
    async fn poll_pending(
        &self,
        user_id: &str,
        edge_agent_id: &str,
    ) -> Result<Vec<EdgeDispatchRow>, String> {
        // Fast path: a non-locking existence probe before opening any transaction.
        // MatrixOne can acquire a table-level lock on SELECT FOR UPDATE even
        // when the result set is empty, adding significant latency (seconds)
        // per 2-second poll cycle when there is nothing to claim. Skipping
        // the transaction when no row exists avoids that entirely. The
        // tiny TOCTOU window (a new row arriving between the probe and FOR UPDATE)
        // is benign: we will pick it up in the next poll interval.
        let pending: Option<i32> = sqlx::query_scalar(
            "SELECT 1 FROM edge_pending_dispatch \
             WHERE user_id = ? AND edge_agent_id = ? AND status = 'pending' LIMIT 1",
        )
        .bind(user_id)
        .bind(edge_agent_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| format!("edge_dispatch poll probe: {e}"))?;

        if pending.is_none() {
            return Ok(vec![]);
        }

        // Atomically claim pending dispatches using SELECT FOR UPDATE
        // within a transaction. This eliminates the race window between
        // poll and mark — two pods polling simultaneously cannot both
        // claim the same rows.
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| format!("edge_dispatch poll begin tx: {e}"))?;

        let selected = async {
            let mut rows = sqlx::query(
                "SELECT user_id, session_id, run_id, turn_chain_id, edge_agent_id, request_id, \
             payload_json, \
             result_json, \
             status, \
             COALESCE(TIMESTAMPDIFF(MICROSECOND, created_at, NOW(6)), 0) AS pending_wait_us \
             FROM edge_pending_dispatch \
             WHERE user_id = ? AND edge_agent_id = ? AND status = 'pending' \
             ORDER BY created_at ASC, request_id ASC LIMIT 50 \
             FOR UPDATE",
            )
            .bind(user_id)
            .bind(edge_agent_id)
            .fetch(&mut *tx);
            let mut claimed = Vec::new();
            while let Some(row) = rows
                .try_next()
                .await
                .map_err(|error| format!("edge_dispatch poll SELECT: {error}"))?
            {
                claimed.push(decode_claimed_dispatch_row(&row)?);
            }
            Ok::<_, String>(claimed)
        }
        .await;
        let claimed_rows = match selected {
            Ok(rows) => rows,
            Err(error) => {
                rollback_edge_dispatch_tx(tx, "poll select or decode").await;
                return Err(error);
            }
        };

        if claimed_rows.is_empty() {
            tx.commit()
                .await
                .map_err(|e| format!("edge_dispatch poll commit (no rows): {e}"))?;
            return Ok(vec![]);
        }

        // Mark claimed rows as dispatched within the same transaction.
        // MatrixOne rejects row-value `IN ((?, ?), ...)`, so spell the identity
        // set as disjunctions over the full turn-scoped primary key.
        let mut update = sqlx::QueryBuilder::<sqlx::MySql>::new(
            "UPDATE edge_pending_dispatch \
             SET status = 'dispatched', dispatched_at = NOW(6) \
             WHERE status = 'pending' AND (",
        );
        let mut first_identity = true;
        for row in &claimed_rows {
            if !first_identity {
                update.push(" OR ");
            }
            first_identity = false;
            update
                .push("(user_id = ")
                .push_bind(&row.user_id)
                .push(" AND session_id = ")
                .push_bind(&row.session_id)
                .push(" AND run_id = ")
                .push_bind(&row.run_id)
                .push(" AND turn_chain_id = ")
                .push_bind(&row.turn_chain_id)
                .push(" AND request_id = ")
                .push_bind(&row.request_id)
                .push(")");
        }
        update.push(")");
        let update_result = match update.build().execute(&mut *tx).await {
            Ok(result) => result,
            Err(e) => {
                rollback_edge_dispatch_tx(tx, "poll update").await;
                return Err(format!("edge_dispatch poll UPDATE: {e}"));
            }
        };
        if let Err(e) = validate_claimed_dispatch_update_count(
            claimed_rows.len(),
            update_result.rows_affected(),
        ) {
            rollback_edge_dispatch_tx(tx, "validate claimed dispatch count").await;
            return Err(e);
        }

        tx.commit()
            .await
            .map_err(|e| format!("edge_dispatch poll commit: {e}"))?;

        if let Some(ref m) = self.metrics {
            m.dispatch_claimed_total
                .fetch_add(claimed_rows.len() as u64, Ordering::Relaxed);
            for row in &claimed_rows {
                m.dispatch_claim_wait_latency
                    .record(std::time::Duration::from_micros(row.pending_wait_us));
            }
        }

        Ok(claimed_rows)
    }

    async fn settle_unsent(
        &self,
        edge_agent_id: &str,
        receipts: &[(EdgeDispatchIdentity, String)],
    ) -> Result<(), String> {
        let Some((first, _)) = receipts.first() else {
            return Ok(());
        };
        if receipts
            .iter()
            .any(|(identity, _)| identity.user_id != first.user_id)
        {
            return Err("cannot settle dispatches from different users in one batch".into());
        }
        let mut update = sqlx::QueryBuilder::<MySql>::new(
            "UPDATE edge_pending_dispatch SET status = 'completed', completed_at = NOW(6), \
             result_json = CASE ",
        );
        for (identity, receipt) in receipts {
            update
                .push("WHEN session_id = ")
                .push_bind(&identity.session_id)
                .push(" AND run_id = ")
                .push_bind(&identity.run_id)
                .push(" AND turn_chain_id = ")
                .push_bind(&identity.turn_chain_id)
                .push(" AND request_id = ")
                .push_bind(&identity.request_id)
                .push(" THEN ")
                .push_bind(receipt);
        }
        update
            .push(" ELSE result_json END WHERE status = 'dispatched' AND edge_agent_id = ")
            .push_bind(edge_agent_id)
            .push(" AND (");
        for (index, (identity, _)) in receipts.iter().enumerate() {
            if index > 0 {
                update.push(" OR ");
            }
            update
                .push("(user_id = ")
                .push_bind(&identity.user_id)
                .push(" AND session_id = ")
                .push_bind(&identity.session_id)
                .push(" AND run_id = ")
                .push_bind(&identity.run_id)
                .push(" AND turn_chain_id = ")
                .push_bind(&identity.turn_chain_id)
                .push(" AND request_id = ")
                .push_bind(&identity.request_id)
                .push(")");
        }
        update.push(")");
        let updated = update
            .build()
            .execute(&self.pool)
            .await
            .map_err(|error| format!("edge_dispatch settle unsent: {error}"))?
            .rows_affected();
        if let Some(ref metrics) = self.metrics {
            for _ in 0..updated {
                saturating_decrement(&metrics.dispatch_queue_depth);
            }
            metrics
                .dispatch_deliver_hits_total
                .fetch_add(updated, Ordering::Relaxed);
        }
        Ok(())
    }

    #[tracing::instrument(skip(self, identity, result_json), fields(user_id = %identity.user_id, session_id = %identity.session_id, run_id = %identity.run_id, turn_chain_id = %identity.turn_chain_id, request_id = %identity.request_id, edge_agent_id = %edge_agent_id))]
    async fn deliver_result(
        &self,
        identity: &EdgeDispatchIdentity,
        edge_agent_id: &str,
        result_json: &str,
    ) -> Result<bool, String> {
        self.deliver_result_matching(identity, edge_agent_id, result_json, true)
            .await
    }

    async fn deliver_terminal_result(
        &self,
        identity: &EdgeDispatchIdentity,
        edge_agent_id: &str,
        result_json: &str,
    ) -> Result<bool, String> {
        self.deliver_result_matching(identity, edge_agent_id, result_json, false)
            .await
    }

    async fn is_server_cancelled_dispatch(
        &self,
        identity: &EdgeDispatchIdentity,
        edge_agent_id: &str,
    ) -> Result<bool, String> {
        let row = sqlx::query(
            "SELECT status, result_json \
             FROM edge_pending_dispatch \
             WHERE user_id = ? AND session_id = ? AND run_id = ? AND turn_chain_id = ? \
               AND request_id = ? AND edge_agent_id = ?",
        )
        .bind(&identity.user_id)
        .bind(&identity.session_id)
        .bind(&identity.run_id)
        .bind(&identity.turn_chain_id)
        .bind(&identity.request_id)
        .bind(edge_agent_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| format!("edge_dispatch inspect cancelled terminal: {e}"))?;
        let Some(row) = row else {
            return Ok(false);
        };
        let status = row
            .try_get::<String, _>("status")
            .map_err(|e| format!("edge_dispatch cancelled terminal status decode: {e}"))?;
        if status != "failed" {
            return Ok(false);
        }
        let Some(body) = row
            .try_get::<Option<String>, _>("result_json")
            .map_err(|e| format!("edge_dispatch cancelled terminal result decode: {e}"))?
        else {
            return Ok(false);
        };
        let value: serde_json::Value = serde_json::from_str(&body).map_err(|e| {
            format!("edge_dispatch cancelled terminal payload is invalid JSON: {e}")
        })?;
        Ok(
            value.get("status").and_then(serde_json::Value::as_str) == Some("error")
                && value.get("output").and_then(serde_json::Value::as_str)
                    == Some("edge dispatch cancelled"),
        )
    }

    #[tracing::instrument(skip(self, identity), fields(user_id = %identity.user_id, session_id = %identity.session_id, run_id = %identity.run_id, turn_chain_id = %identity.turn_chain_id, edge_agent_id = %edge_agent_id, request_id = %identity.request_id, reason = %reason))]
    async fn fail_dispatch(
        &self,
        identity: &EdgeDispatchIdentity,
        edge_agent_id: &str,
        reason: &str,
    ) -> Result<bool, String> {
        self.fail_dispatch_matching(identity, edge_agent_id, reason, false)
            .await
    }

    async fn expire_pending_dispatch(
        &self,
        identity: &EdgeDispatchIdentity,
        edge_agent_id: &str,
    ) -> Result<bool, String> {
        self.fail_dispatch_matching(identity, edge_agent_id, "expired", true)
            .await
    }

    async fn inspect_run_dispatches(
        &self,
        user_id: &str,
        session_id: &str,
        run_id: &str,
        after: Option<(&str, &str)>,
        exact: Option<(&str, &str)>,
    ) -> Result<Vec<EdgeDispatchObservation>, String> {
        if [user_id, session_id, run_id]
            .iter()
            .any(|part| part.trim().is_empty())
            || after.is_some_and(|(turn, request)| turn.is_empty() || request.is_empty())
            || exact.is_some_and(|(turn, request)| turn.is_empty() || request.is_empty())
            || (after.is_some() && exact.is_some())
        {
            return Err("invalid dispatch inspection identity".into());
        }
        let mut query = sqlx::QueryBuilder::<MySql>::new(
            "SELECT user_id, turn_chain_id, request_id, edge_agent_id, status, \
             JSON_UNQUOTE(JSON_EXTRACT(payload_json, '$.identity')) AS invocation_identity, \
             CASE WHEN OCTET_LENGTH(payload_json) <= 65536 THEN JSON_UNQUOTE(JSON_EXTRACT(payload_json, '$.tool')) ELSE NULL END AS tool_name, ",
        );
        if exact.is_some() {
            // Keep the database/network read bounded even when a callback is huge.
            query.push("CASE WHEN OCTET_LENGTH(result_json) <= 65536 THEN result_json ELSE NULL END AS bounded_result, \
                        CASE WHEN OCTET_LENGTH(result_json) > 65536 THEN 1 ELSE 0 END AS result_too_large ");
        } else {
            query.push("NULL AS bounded_result, 0 AS result_too_large ");
        }
        query
            .push("FROM edge_pending_dispatch WHERE session_id = ")
            .push_bind(session_id)
            .push(" AND run_id = ")
            .push_bind(run_id);
        if let Some((turn, request)) = exact {
            query
                .push(" AND turn_chain_id = ")
                .push_bind(turn)
                .push(" AND request_id = ")
                .push_bind(request);
        } else if let Some((turn, request)) = after {
            query
                .push(" AND (turn_chain_id > ")
                .push_bind(turn)
                .push(" OR (turn_chain_id = ")
                .push_bind(turn)
                .push(" AND request_id > ")
                .push_bind(request)
                .push("))");
        }
        if exact.is_none() {
            query.push(" ORDER BY turn_chain_id, request_id, user_id LIMIT 17");
        } else {
            query.push(" LIMIT 2");
        }
        let rows = query
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(|error| format!("edge dispatch inspection failed: {error}"))?;
        let mut observations = Vec::with_capacity(rows.len());
        for row in rows {
            let identity = EdgeDispatchIdentity::new(
                row.try_get::<String, _>("user_id")
                    .map_err(|e| e.to_string())?,
                session_id,
                run_id,
                row.try_get::<String, _>("turn_chain_id")
                    .map_err(|e| e.to_string())?,
                row.try_get::<String, _>("request_id")
                    .map_err(|e| e.to_string())?,
            );
            let invocation_json: Option<String> = row
                .try_get("invocation_identity")
                .map_err(|e| e.to_string())?;
            if dispatch_run_owner(&identity, invocation_json.as_deref())? != user_id {
                return Err("dispatch inspection scope does not match the run owner".into());
            }
            if observations
                .last()
                .is_some_and(|previous: &EdgeDispatchObservation| {
                    previous.turn_chain_id == identity.turn_chain_id
                        && previous.request_id == identity.request_id
                })
            {
                return Err("dispatch identity is ambiguous across Edge owners".into());
            }
            observations.push(EdgeDispatchObservation {
                edge_owner_user_id: identity.user_id,
                turn_chain_id: identity.turn_chain_id,
                request_id: identity.request_id,
                edge_agent_id: row.try_get("edge_agent_id").map_err(|e| e.to_string())?,
                status: row.try_get("status").map_err(|e| e.to_string())?,
                tool_name: row.try_get("tool_name").map_err(|e| e.to_string())?,
                result_json: row.try_get("bounded_result").map_err(|e| e.to_string())?,
                result_too_large: row
                    .try_get::<i32, _>("result_too_large")
                    .map_err(|e| e.to_string())?
                    != 0,
            });
        }
        Ok(observations)
    }

    async fn callback_run_owner(
        &self,
        identity: &EdgeDispatchIdentity,
        edge_agent_id: &str,
    ) -> Result<Option<String>, String> {
        let row = sqlx::query(
            "SELECT JSON_UNQUOTE(JSON_EXTRACT(payload_json, '$.identity')) AS invocation_identity \
             FROM edge_pending_dispatch WHERE user_id = ? AND session_id = ? AND run_id = ? \
             AND turn_chain_id = ? AND request_id = ? AND edge_agent_id = ?",
        )
        .bind(&identity.user_id)
        .bind(&identity.session_id)
        .bind(&identity.run_id)
        .bind(&identity.turn_chain_id)
        .bind(&identity.request_id)
        .bind(edge_agent_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| format!("dispatch callback owner lookup failed: {e}"))?;
        row.map(|row| {
            let invocation_json: Option<String> = row
                .try_get("invocation_identity")
                .map_err(|e| e.to_string())?;
            dispatch_run_owner(identity, invocation_json.as_deref())
        })
        .transpose()
    }

    #[tracing::instrument(skip(self, identity), fields(user_id = %identity.user_id, session_id = %identity.session_id, run_id = %identity.run_id, turn_chain_id = %identity.turn_chain_id, request_id = %identity.request_id, timeout_ms = ?timeout.map(|timeout| timeout.as_millis())))]
    async fn wait_result(
        &self,
        identity: &EdgeDispatchIdentity,
        timeout: Option<std::time::Duration>,
    ) -> Result<Option<String>, String> {
        if timeout.is_some_and(|timeout| timeout.is_zero()) {
            if !identity.is_complete() {
                return Err("edge result read requires complete owner identity".into());
            }
            let row = sqlx::query(
                "SELECT result_json FROM edge_pending_dispatch \
                 WHERE user_id = ? AND session_id = ? AND run_id = ? AND turn_chain_id = ? \
                   AND request_id = ? AND status IN ('completed', 'failed')",
            )
            .bind(&identity.user_id)
            .bind(&identity.session_id)
            .bind(&identity.run_id)
            .bind(&identity.turn_chain_id)
            .bind(&identity.request_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| format!("edge_dispatch exact result read: {error}"))?;
            return row
                .map(|row| row.try_get::<Option<String>, _>("result_json"))
                .transpose()
                .map(Option::flatten)
                .map_err(|error| format!("edge_dispatch result decode: {error}"));
        }
        let mut receiver = self.wait_coordinator.subscribe(identity.clone()).await;
        let wait = async {
            loop {
                if let Some(resolution) = receiver.borrow().clone() {
                    return resolution;
                }
                receiver
                    .changed()
                    .await
                    .map_err(|_| "edge_dispatch wait coordinator stopped".to_string())?;
            }
        };
        let result = match timeout {
            Some(timeout) => tokio::time::timeout(timeout, wait).await.map_err(|_| ()),
            None => Ok(wait.await),
        };
        match result {
            Ok(result) => result,
            Err(_) => {
                tracing::warn!("edge_dispatch: batched wait_result timed out");
                if let Some(ref metrics) = self.metrics {
                    metrics
                        .dispatch_wait_result_timeouts_total
                        .fetch_add(1, Ordering::Relaxed);
                }
                Ok(None)
            }
        }
    }

    async fn cleanup_stale(&self, older_than: std::time::Duration) -> Result<u64, String> {
        let secs = older_than.as_secs() as i64;
        let expired_result_json = edge_dispatch_failed_result_json(
            serde_json::Value::Null,
            "edge dispatch expired".to_string(),
        );
        let expired = sqlx::query(
            "UPDATE edge_pending_dispatch \
             SET status = 'failed', result_json = ?, completed_at = NOW(6) \
             WHERE status IN ('pending', 'dispatched') \
               AND created_at <= DATE_SUB(NOW(6), INTERVAL ? SECOND) \
               AND NOT EXISTS (SELECT 1 FROM agent_runs r \
                   WHERE r.user_id = COALESCE(JSON_UNQUOTE(JSON_EXTRACT(edge_pending_dispatch.payload_json, '$.identity.user_id')), edge_pending_dispatch.user_id) \
                     AND r.session_id = edge_pending_dispatch.session_id \
                     AND r.run_id = edge_pending_dispatch.run_id \
                     AND r.status IN ('running', 'waiting', 'paused'))",
        )
        .bind(expired_result_json)
        .bind(secs)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("edge_dispatch expire stale: {e}"))?
        .rows_affected();
        if expired > 0
            && let Some(ref m) = self.metrics
        {
            for _ in 0..expired {
                saturating_decrement(&m.dispatch_queue_depth);
            }
            m.dispatch_cleanup_expired_total
                .fetch_add(expired, Ordering::Relaxed);
        }

        let deleted = sqlx::query(
            "DELETE FROM edge_pending_dispatch \
             WHERE status IN ('completed', 'failed') \
               AND COALESCE(completed_at, created_at) <= DATE_SUB(NOW(6), INTERVAL ? SECOND) \
               AND NOT EXISTS (SELECT 1 FROM agent_runs r \
                   WHERE r.user_id = COALESCE(JSON_UNQUOTE(JSON_EXTRACT(edge_pending_dispatch.payload_json, '$.identity.user_id')), edge_pending_dispatch.user_id) \
                     AND r.session_id = edge_pending_dispatch.session_id \
                     AND r.run_id = edge_pending_dispatch.run_id \
                     AND r.status IN ('running', 'waiting', 'paused'))",
        )
        .bind(secs)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("edge_dispatch cleanup: {e}"))?;
        if let Some(ref m) = self.metrics {
            m.dispatch_cleanup_deleted_total
                .fetch_add(deleted.rows_affected(), Ordering::Relaxed);
        }
        Ok(expired + deleted.rows_affected())
    }
}

pub struct UnconfiguredEdgeDispatchService;

#[async_trait]
impl EdgeDispatchService for UnconfiguredEdgeDispatchService {
    async fn subscribe_pending_wakeup(
        &self,
        _user_id: &str,
        _edge_agent_id: &str,
    ) -> Result<Option<tokio::sync::watch::Receiver<u64>>, String> {
        Ok(None)
    }

    async fn poll_pending(
        &self,
        _user_id: &str,
        _edge_agent_id: &str,
    ) -> Result<Vec<EdgeDispatchRow>, String> {
        Err("edge dispatch service not configured".to_string())
    }
    async fn deliver_result(
        &self,
        _identity: &EdgeDispatchIdentity,
        _edge_agent_id: &str,
        _result_json: &str,
    ) -> Result<bool, String> {
        Err("edge dispatch service not configured".to_string())
    }
    async fn fail_dispatch(
        &self,
        _identity: &EdgeDispatchIdentity,
        _edge_agent_id: &str,
        _reason: &str,
    ) -> Result<bool, String> {
        Err("edge dispatch service not configured".to_string())
    }
    async fn wait_result(
        &self,
        _identity: &EdgeDispatchIdentity,
        _timeout: Option<std::time::Duration>,
    ) -> Result<Option<String>, String> {
        Err("edge dispatch service not configured".to_string())
    }
    async fn cleanup_stale(&self, _older_than: std::time::Duration) -> Result<u64, String> {
        Err("edge dispatch service not configured".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse_dispatch_payload(json: &str) -> serde_json::Value {
        serde_json::from_str(json).expect("valid dispatch fixture")
    }
    use std::sync::atomic::{AtomicU64, AtomicUsize};
    use uuid::Uuid;

    static EDGE_DISPATCH_SCHEMA: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

    #[test]
    fn admission_error_categorizes_failures_without_exposing_sources() {
        let secret = "fixture-secret-in-database-error";
        let failures = [
            (
                sqlx::Error::PoolTimedOut,
                EdgeDispatchFailureCause::PoolTimeout,
            ),
            (
                sqlx::Error::PoolClosed,
                EdgeDispatchFailureCause::PoolClosed,
            ),
            (
                sqlx::Error::Io(std::io::Error::other(secret)),
                EdgeDispatchFailureCause::Connection,
            ),
            (
                sqlx::Error::Protocol(secret.to_string()),
                EdgeDispatchFailureCause::InvalidData,
            ),
        ];
        for (error, cause) in failures {
            let mapped = edge_admission_database_error(EdgeDispatchFailureStage::Begin, error);
            assert_eq!(
                mapped,
                EdgeDispatchAdmissionError::OutcomeUnknown(EdgeDispatchFailure {
                    stage: EdgeDispatchFailureStage::Begin,
                    cause,
                })
            );
            assert!(!mapped.to_string().contains(secret));
            assert!(!format!("{mapped:?}").contains(secret));
        }
    }

    #[tokio::test]
    async fn admission_reports_connection_acquisition_timeout() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .acquire_timeout(Duration::from_millis(100))
            .connect_lazy(&format!("mysql://user:fixture-secret@{address}/unused"))
            .unwrap();
        let service = DatabaseEdgeDispatchService::new(pool.clone());
        let identity = EdgeDispatchIdentity::new("user", "session", "run", "turn", "request");
        let error = service
            .admit_dispatch(&identity, "edge", &json!({}))
            .await
            .unwrap_err();
        assert_eq!(
            error,
            EdgeDispatchAdmissionError::OutcomeUnknown(EdgeDispatchFailure {
                stage: EdgeDispatchFailureStage::Begin,
                cause: EdgeDispatchFailureCause::PoolTimeout,
            })
        );
        assert!(!error.to_string().contains("fixture-secret"));
        pool.close().await;
    }

    #[tokio::test]
    async fn unconfirmed_direct_settlement_uses_service_metrics() {
        let pool =
            sqlx::MySqlPool::connect_lazy("mysql://unused@127.0.0.1:1/unused").expect("lazy pool");
        let metrics = super::super::metrics::shared_metrics();
        let service = DatabaseEdgeDispatchService::new(pool).with_metrics(metrics.clone());
        service.record_unconfirmed_direct_settlement();
        assert_eq!(
            metrics
                .dispatch_settlement_unknown_total
                .load(Ordering::Relaxed),
            1
        );
    }

    #[tokio::test]
    async fn observation_unknown_counts_identities_with_live_waiters() {
        let pool =
            sqlx::MySqlPool::connect_lazy("mysql://unused@127.0.0.1:1/unused").expect("lazy pool");
        let metrics = super::super::metrics::shared_metrics();
        let coordinator = EdgeDispatchWaitCoordinator::new(pool);
        assert!(coordinator.metrics.set(metrics.clone()).is_ok());

        let (sender, mut first) = tokio::sync::watch::channel(None);
        let mut second = sender.subscribe();
        coordinator.resolve_observation_error(
            EdgeDispatchWaiter {
                sender,
                unknown_since: None,
            },
            &ObservationFailure::Unknown,
            "observation failed",
        );
        first.changed().await.unwrap();
        second.changed().await.unwrap();
        assert_eq!(
            first.borrow().as_ref(),
            Some(&Err("observation failed".into()))
        );
        assert_eq!(second.borrow().as_ref(), first.borrow().as_ref());
        assert_eq!(
            metrics
                .dispatch_observation_unknown_total
                .load(Ordering::Relaxed),
            1
        );

        let (sender, receiver) = tokio::sync::watch::channel(None);
        drop(receiver);
        coordinator.resolve_observation_error(
            EdgeDispatchWaiter {
                sender,
                unknown_since: None,
            },
            &ObservationFailure::Unknown,
            "no subscriber",
        );
        let (sender, _receiver) = tokio::sync::watch::channel(None);
        coordinator.resolve_observation_error(
            EdgeDispatchWaiter {
                sender,
                unknown_since: None,
            },
            &ObservationFailure::Permanent,
            "invalid query",
        );
        assert_eq!(
            metrics
                .dispatch_observation_unknown_total
                .load(Ordering::Relaxed),
            1
        );
    }

    async fn setup_edge_dispatch_db_it() -> astra_core::SharedPool {
        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "set ASTRA_TEST_DB_IT=1 for ignored integration tests"
        );
        let settings = astra_core::MatrixOneSettings::from_env();
        EDGE_DISPATCH_SCHEMA
            .get_or_init(|| async {
                let catalog = std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG")
                    .unwrap_or_else(|_| "mysql".to_string());
                crate::storage::ensure_core_schema(&settings, &catalog)
                    .await
                    .expect("ensure_core_schema");
            })
            .await;
        astra_core::SharedPool::new(&settings)
            .await
            .expect("SharedPool::new")
    }

    async fn cleanup_edge_dispatch_fixture(
        pool: &astra_core::SharedPool,
        identity: &EdgeDispatchIdentity,
    ) {
        sqlx::query(
            "DELETE FROM edge_pending_dispatch \
             WHERE user_id = ? AND session_id = ? AND run_id = ? AND turn_chain_id = ? AND request_id = ?",
        )
        .bind(&identity.user_id)
        .bind(&identity.session_id)
        .bind(&identity.run_id)
        .bind(&identity.turn_chain_id)
        .bind(&identity.request_id)
        .execute(pool.get())
        .await
        .expect("cleanup edge dispatch fixture");
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
    async fn matrixone_waiter_survives_transient_observation_failure() {
        let pool = setup_edge_dispatch_db_it().await;
        let now = tokio::time::Instant::now();
        for statement in ["SELECT column_that_does_not_exist", "SELECT FROM"] {
            let error = sqlx::query(statement)
                .fetch_all(pool.get())
                .await
                .unwrap_err();
            assert_eq!(
                WaitObservationError::Query(error).classification(),
                ObservationFailure::Permanent
            );
        }
        let unknown = sqlx::query("SELECT MO_CTL('XX', 'nonsense', '')")
            .fetch_all(pool.get())
            .await
            .unwrap_err();
        let unknown = WaitObservationError::Query(unknown).classification();
        assert_eq!(unknown, ObservationFailure::Unknown);
        let (sender, _receiver) = tokio::sync::watch::channel(None);
        let mut waiter = EdgeDispatchWaiter {
            sender,
            unknown_since: None,
        };
        assert!(waiter.should_retry(&unknown, now));
        assert!(!waiter.should_retry(&unknown, now + EDGE_DISPATCH_WAIT_UNKNOWN_ERROR_BUDGET));
        let writer = DatabaseEdgeDispatchService::from_shared(&pool);
        let id = Uuid::new_v4().to_string();
        let identity = EdgeDispatchIdentity::new(&id, &id, &id, &id, &id);
        writer
            .admit_dispatch(&identity, "edge", &json!({}))
            .await
            .unwrap();
        assert_eq!(writer.poll_pending(&id, "edge").await.unwrap().len(), 1);

        let mut settings = astra_core::MatrixOneSettings::from_env();
        let upstream = (settings.host.clone(), settings.port);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let unavailable = Arc::new(AtomicBool::new(true));
        let attempts = Arc::new(AtomicUsize::new(0));
        let proxy = {
            let unavailable = unavailable.clone();
            let attempts = attempts.clone();
            tokio::spawn(async move {
                while let Ok((mut client, _)) = listener.accept().await {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    if unavailable.load(Ordering::SeqCst) {
                        continue;
                    }
                    let upstream = upstream.clone();
                    tokio::spawn(async move {
                        if let Ok(mut server) = tokio::net::TcpStream::connect(upstream).await {
                            let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
                        }
                    });
                }
            })
        };
        settings.host = address.ip().to_string();
        settings.port = address.port();
        let observer_pool = sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_millis(250))
            .connect_lazy(&settings.database_url_with_password())
            .unwrap();
        let observer = Arc::new(DatabaseEdgeDispatchService::new(observer_pool.clone()));
        let waiter = {
            let observer = observer.clone();
            let identity = identity.clone();
            tokio::spawn(async move {
                observer
                    .wait_result(&identity, Some(Duration::from_secs(10)))
                    .await
            })
        };
        tokio::time::timeout(Duration::from_secs(3), async {
            while attempts.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the observer must encounter the injected outage");
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            !waiter.is_finished(),
            "a transient read error must not end the caller's wait"
        );
        writer
            .settle_unsent("edge", &[(identity.clone(), "receipt".into())])
            .await
            .unwrap();
        unavailable.store(false, Ordering::SeqCst);
        assert_eq!(waiter.await.unwrap().unwrap().as_deref(), Some("receipt"));
        observer_pool.close().await;
        proxy.abort();
        cleanup_edge_dispatch_fixture(&pool, &identity).await;
    }

    #[test]
    fn observation_classifies_matrixone_busy_separately_from_invalid_input() {
        assert_eq!(
            database_observation_failure(Some("HY000"), Some(20508)),
            ObservationFailure::Transient
        );
        assert_eq!(
            database_observation_failure(Some("HY000"), Some(20301)),
            ObservationFailure::Permanent
        );
        assert_eq!(
            database_observation_failure(Some("42000"), Some(1461)),
            ObservationFailure::Transient
        );
    }

    #[tokio::test]
    #[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
    async fn matrixone_unknown_observation_does_not_starve_other_batches_or_new_waiters() {
        let admin = setup_edge_dispatch_db_it().await;
        let database = format!("astra_wait_{}", Uuid::new_v4().simple());
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE `{database}`")))
            .execute(admin.get())
            .await
            .unwrap();
        let mut settings = astra_core::MatrixOneSettings::from_env();
        settings.database = database.clone();
        let pool = astra_core::SharedPool::new(&settings).await.unwrap();
        sqlx::query(
            "CREATE TABLE base_dispatch (user_id VARCHAR(128), session_id VARCHAR(128), \
             run_id VARCHAR(128), turn_chain_id VARCHAR(128), request_id VARCHAR(128), \
             status VARCHAR(16), result_json TEXT)",
        )
        .execute(pool.get())
        .await
        .unwrap();
        let mut insert = sqlx::QueryBuilder::<MySql>::new(
            "INSERT INTO base_dispatch (user_id, session_id, run_id, turn_chain_id, request_id, status, result_json) ",
        );
        insert.push_values(0..300, |mut row, index| {
            row.push_bind("u")
                .push_bind("s")
                .push_bind("r")
                .push_bind("t")
                .push_bind(format!("q{index:03}"))
                .push_bind("pending")
                .push_bind("waiting");
        });
        insert.build().execute(pool.get()).await.unwrap();
        sqlx::query(
            "CREATE VIEW edge_pending_dispatch AS SELECT user_id, session_id, run_id, \
             turn_chain_id, request_id, status, CASE WHEN result_json = 'error' \
             THEN CAST(SLEEP(-1) AS CHAR) ELSE result_json END AS result_json \
             FROM base_dispatch",
        )
        .execute(pool.get())
        .await
        .unwrap();

        let metrics = super::super::metrics::shared_metrics();
        let service =
            DatabaseEdgeDispatchService::new(pool.get().clone()).with_metrics(metrics.clone());
        let coordinator = service.wait_coordinator.clone();
        let mut receivers = HashMap::new();
        let ordered = {
            let mut waiters = coordinator.waiters.lock().await;
            for index in 0..300 {
                let identity =
                    EdgeDispatchIdentity::new("u", "s", "r", "t", format!("q{index:03}"));
                let (sender, receiver) = tokio::sync::watch::channel(None);
                receivers.insert(identity.clone(), receiver);
                waiters.insert(
                    identity,
                    EdgeDispatchWaiter {
                        sender,
                        unknown_since: None,
                    },
                );
            }
            waiters.keys().cloned().collect::<Vec<_>>()
        };
        let expired = ordered[128].clone();
        let recent = ordered[129].clone();
        let healthy = ordered[256].clone();
        let mut second_expired = coordinator.waiters.lock().await[&expired]
            .sender
            .subscribe();
        coordinator
            .waiters
            .lock()
            .await
            .get_mut(&expired)
            .unwrap()
            .unknown_since =
            Some(tokio::time::Instant::now() - EDGE_DISPATCH_WAIT_UNKNOWN_ERROR_BUDGET);
        for identity in [&expired, &recent] {
            sqlx::query("UPDATE base_dispatch SET result_json = 'error' WHERE request_id = ?")
                .bind(&identity.request_id)
                .execute(pool.get())
                .await
                .unwrap();
        }
        sqlx::query("UPDATE base_dispatch SET status = 'completed', result_json = 'healthy' WHERE request_id = ?")
            .bind(&healthy.request_id)
            .execute(pool.get())
            .await
            .unwrap();
        let task = tokio::spawn(coordinator.clone().run());
        for identity in [&expired, &healthy] {
            tokio::time::timeout(
                Duration::from_secs(5),
                receivers.get_mut(identity).unwrap().changed(),
            )
            .await
            .expect("both failed and later healthy batches must be observed")
            .unwrap();
        }
        second_expired.changed().await.unwrap();
        assert!(receivers[&expired].borrow().as_ref().unwrap().is_err());
        assert!(second_expired.borrow().as_ref().unwrap().is_err());
        // The observer publishes before incrementing the counter, but does
        // both while holding this lock.
        let waiters = coordinator.waiters.lock().await;
        assert_eq!(
            metrics
                .dispatch_observation_unknown_total
                .load(Ordering::Relaxed),
            1
        );
        drop(waiters);
        assert_eq!(
            receivers[&healthy]
                .borrow()
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap()
                .as_deref(),
            Some("healthy")
        );
        assert!(
            receivers[&recent].borrow().is_none(),
            "a newer waiter's budget must not inherit another's: {:?}",
            *receivers[&recent].borrow()
        );
        sqlx::query("UPDATE base_dispatch SET status = 'completed', result_json = 'recovered' WHERE request_id = ?")
            .bind(&recent.request_id)
            .execute(pool.get())
            .await
            .unwrap();
        tokio::time::timeout(
            Duration::from_secs(5),
            receivers.get_mut(&recent).unwrap().changed(),
        )
        .await
        .expect("newer waiter must remain subscribed until recovery")
        .unwrap();
        assert_eq!(
            receivers[&recent]
                .borrow()
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap()
                .as_deref(),
            Some("recovered")
        );
        struct CounterTarget(AtomicU64);
        impl super::super::metrics::MetricTarget for CounterTarget {
            fn register_counter(&self, _: &str, _: &str) {}
            fn register_gauge(&self, _: &str, _: &str) {}
            fn set_gauge(&self, _: &str, _: f64) {}
            fn set_counter(&self, name: &str, value: u64) {
                if name == "astra_edge_dispatch_observation_unknown_total" {
                    self.0.store(value, Ordering::Relaxed);
                }
            }
        }
        let target = CounterTarget(AtomicU64::new(0));
        metrics.register_with(&target);
        metrics.scrape_to(&target);
        assert_eq!(target.0.load(Ordering::Relaxed), 1);
        drop(receivers);
        drop(second_expired);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        pool.get().close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP DATABASE `{database}`")))
            .execute(admin.get())
            .await
            .unwrap();
    }

    struct FakeEdgeDispatchRow {
        failed_column: Option<&'static str>,
        result_json: Option<String>,
    }

    impl FakeEdgeDispatchRow {
        fn complete() -> Self {
            Self {
                failed_column: None,
                result_json: Some(r#"{"status":"completed"}"#.to_string()),
            }
        }

        fn pending_without_result() -> Self {
            Self {
                failed_column: None,
                result_json: None,
            }
        }

        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
                ..Self::complete()
            }
        }
    }

    impl EdgeDispatchDbRow for FakeEdgeDispatchRow {
        fn json_column(&self, column: &str) -> Result<serde_json::Value, sqlx::Error> {
            if self.failed_column == Some(column) {
                return Err(sqlx::Error::ColumnNotFound(column.to_string()));
            }
            match column {
                "payload_json" => Ok(json!({"tool":"agent_fanout"})),
                _ => Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
        }

        fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error> {
            if self.failed_column == Some(column) {
                return Err(sqlx::Error::ColumnNotFound(column.to_string()));
            }
            match column {
                "pending_wait_us" => Ok(1_234),
                "pending_rows" => Ok(3),
                "dispatched_rows" => Ok(2),
                "oldest_pending_age_us" => Ok(1_500),
                "oldest_dispatched_age_us" => Ok(2_001),
                _ => Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
        }

        fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
            if self.failed_column == Some(column) {
                return Err(sqlx::Error::ColumnNotFound(column.to_string()));
            }
            Ok(match column {
                "user_id" => "user-1",
                "session_id" => "session-1",
                "run_id" => "run-1",
                "turn_chain_id" => "turn-chain-1",
                "edge_agent_id" => "edge-1",
                "request_id" => "request-1",
                "payload_json" => r#"{"tool":"agent_fanout"}"#,
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
            .to_string())
        }

        fn optional_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error> {
            if self.failed_column == Some(column) {
                return Err(sqlx::Error::ColumnNotFound(column.to_string()));
            }
            match column {
                "result_json" => Ok(self.result_json.clone()),
                _ => Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
        }
    }

    #[test]
    fn claimed_dispatch_row_decode_preserves_values() {
        let row = decode_claimed_dispatch_row(&FakeEdgeDispatchRow::complete()).unwrap();

        assert_eq!(row.user_id, "user-1");
        assert_eq!(row.session_id, "session-1");
        assert_eq!(row.run_id, "run-1");
        assert_eq!(row.turn_chain_id, "turn-chain-1");
        assert_eq!(row.edge_agent_id, "edge-1");
        assert_eq!(row.request_id, "request-1");
        assert_eq!(row.payload_json, r#"{"tool":"agent_fanout"}"#);
        assert_eq!(
            row.result_json.as_deref(),
            Some(r#"{"status":"completed"}"#)
        );
        assert_eq!(row.status, "dispatched");
        assert_eq!(row.pending_wait_us, 1_234);
    }

    #[tokio::test]
    async fn pending_wakeup_is_owner_and_edge_scoped_and_coalescing() {
        let hub = Arc::new(EdgeDispatchWakeHub::default());
        let mut intended = hub.subscribe("owner-a", "edge-a").await;
        let other_owner = hub.subscribe("owner-b", "edge-a").await;
        let other_edge = hub.subscribe("owner-a", "edge-b").await;

        hub.notify("owner-a", "edge-a").await;
        intended.changed().await.unwrap();
        assert_eq!(*intended.borrow(), 1);
        assert!(!other_owner.has_changed().unwrap());
        assert!(!other_edge.has_changed().unwrap());

        // Watch notifications intentionally coalesce; durable polling still
        // claims every queued row.
        hub.notify("owner-a", "edge-a").await;
        hub.notify("owner-a", "edge-a").await;
        intended.changed().await.unwrap();
        assert_eq!(*intended.borrow(), 3);
    }

    #[tokio::test]
    async fn small_pool_uses_recovery_wakes_without_opening_observer_connections() {
        for size in [1, 2] {
            let pool = sqlx::mysql::MySqlPoolOptions::new()
                .max_connections(size)
                .min_connections(0)
                .connect_lazy("mysql://root:unused@127.0.0.1:1/unused")
                .unwrap();
            let hub = Arc::new(EdgeDispatchWakeHub::new(pool.clone()));
            let mut wake = hub.subscribe("user", "edge").await;
            tokio::time::timeout(Duration::from_secs(1), wake.changed())
                .await
                .expect("small pool must still wake pending dispatches")
                .expect("wake source remains open");
            tokio::time::timeout(Duration::from_secs(3), wake.changed())
                .await
                .expect("small pool must keep recovering new pending dispatches")
                .expect("wake source remains open");
            assert_eq!(pool.size(), 0, "observer must leave the small pool free");
        }
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
    async fn matrixone_pending_observer_filters_cross_pairs() {
        let pool = setup_edge_dispatch_db_it().await;
        let service = DatabaseEdgeDispatchService::from_shared(&pool);
        let unique = Uuid::new_v4().to_string();
        let user_a = format!("wake-a-{unique}");
        let user_b = format!("wake-b-{unique}");
        let user_c = format!("wake-c-{unique}");
        let edge_a = format!("edge-a-{unique}");
        let edge_b = format!("edge-b-{unique}");
        let targets = vec![
            (user_a.clone(), edge_a.clone()),
            (user_b.clone(), edge_b.clone()),
        ];
        let mut receivers = Vec::new();
        {
            let mut subscribers = service.wake_hub.subscribers.lock().await;
            for key in &targets {
                let (sender, receiver) = tokio::sync::watch::channel(0);
                subscribers.insert(key.clone(), sender);
                receivers.push(receiver);
            }
        }
        let rows = [
            (
                EdgeDispatchIdentity::new(&user_a, "s", "r", "t", "cross"),
                &edge_b,
            ),
            (
                EdgeDispatchIdentity::new(&user_c, "s", "r", "t", "unrelated"),
                &edge_a,
            ),
            (
                EdgeDispatchIdentity::new(&user_a, "s", "r", "t", "target-a"),
                &edge_a,
            ),
            (
                EdgeDispatchIdentity::new(&user_b, "s", "r", "t", "target-b"),
                &edge_b,
            ),
        ];
        for (identity, edge) in &rows[..2] {
            service
                .admit_dispatch(identity, edge, &json!({}))
                .await
                .unwrap();
        }
        assert!(
            service
                .wake_hub
                .pending_targets(&targets)
                .await
                .unwrap()
                .is_empty()
        );
        for (identity, edge) in &rows[2..] {
            service
                .admit_dispatch(identity, edge, &json!({}))
                .await
                .unwrap();
        }
        let found = service.wake_hub.pending_targets(&targets).await.unwrap();
        assert_eq!(found.len(), 2);
        assert!(targets.iter().all(|target| found.contains(target)));
        for (identity, _) in &rows {
            cleanup_edge_dispatch_fixture(&pool, identity).await;
        }
        drop(receivers);
    }

    #[tokio::test(flavor = "current_thread")]
    #[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
    async fn matrixone_slow_observer_preserves_recovery_and_pool_capacity() {
        let admin = setup_edge_dispatch_db_it().await;
        let database = format!("astra_test_wake_{}", Uuid::new_v4().simple());
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE `{database}`")))
            .execute(admin.get())
            .await
            .expect("create isolated wake test database");
        let mut settings = astra_core::MatrixOneSettings::from_env();
        settings.database = database.clone();
        settings.db_pool_max_connections = 3;
        settings.db_pool_min_connections = 0;
        let pool = astra_core::SharedPool::new(&settings).await.unwrap();
        sqlx::query(
            "CREATE VIEW edge_pending_dispatch AS \
             SELECT 'slow-user' AS user_id, 'slow-edge' AS edge_agent_id, \
             IF(SLEEP(8) = 0, 'pending', 'error') AS status",
        )
        .execute(pool.get())
        .await
        .expect("create slow read-only observer fixture");

        // A TCP proxy lets this test sever exactly the observer connection,
        // including on MatrixOne versions without SQL KILL CONNECTION.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = listener.local_addr().unwrap();
        let upstream = (settings.host.clone(), settings.port);
        let disconnect = Arc::new(tokio::sync::Notify::new());
        let proxy_disconnect = disconnect.clone();
        let proxy = tokio::spawn(async move {
            let mut first = true;
            loop {
                let (mut client, _) = listener.accept().await.unwrap();
                let mut server = tokio::net::TcpStream::connect(&upstream).await.unwrap();
                let disconnect = first.then(|| proxy_disconnect.clone());
                first = false;
                tokio::spawn(async move {
                    if let Some(disconnect) = disconnect {
                        tokio::select! {
                            _ = tokio::io::copy_bidirectional(&mut client, &mut server) => {},
                            _ = disconnect.notified() => {},
                        }
                    } else {
                        let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
                    }
                });
            }
        });
        settings.host = proxy_address.ip().to_string();
        settings.port = proxy_address.port();
        let observed_pool = sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(3)
            .connect_lazy(&settings.database_url_with_password())
            .unwrap();
        let hub = Arc::new(EdgeDispatchWakeHub::new(observed_pool.clone()));
        let started = tokio::time::Instant::now();
        let mut wakes = vec![hub.subscribe("slow-user", "slow-edge").await];
        for index in 0..512 {
            wakes.push(hub.subscribe("other-user", &format!("edge-{index}")).await);
        }
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let rows = sqlx::query("SHOW FULL PROCESSLIST")
                    .fetch_all(admin.get())
                    .await
                    .expect("inspect the isolated observer query");
                for row in rows {
                    let db = row.try_get::<Option<String>, _>("db").ok().flatten();
                    let info = row.try_get::<Option<String>, _>("info").ok().flatten();
                    if db.as_deref() == Some(database.as_str())
                        && info
                            .as_deref()
                            .is_some_and(|info| info.contains("FROM edge_pending_dispatch"))
                    {
                        return;
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("a real slow observer query must start");
        tokio::time::timeout(Duration::from_secs(8), async {
            for wake in &mut wakes {
                wake.changed()
                    .await
                    .expect("wake subscription remains open");
            }
        })
        .await
        .expect("a slow batch must not block recovery for later batches");
        assert!(
            started.elapsed() >= EDGE_DISPATCH_WAKE_RECOVERY_INTERVAL,
            "the fixture must exercise the slow-cycle path, not an immediate error"
        );
        let mut late_wake = hub.subscribe("late-user", "late-edge").await;
        tokio::time::timeout(Duration::from_secs(3), late_wake.changed())
            .await
            .expect("a connection joining a stalled cycle must also receive recovery")
            .expect("late wake source remains open");
        // The observer owns its slow query until completion, while the pool
        // retains room for a normal request and for recovery claims.
        let mut connection = tokio::time::timeout(Duration::from_secs(3), observed_pool.acquire())
            .await
            .expect("slow observer must not exhaust the pool")
            .expect("available connection");
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "the ordinary request should not wait for the slow observer"
        );
        sqlx::query("SELECT 1")
            .execute(&mut *connection)
            .await
            .expect("ordinary connection works");
        drop(connection);

        // An actual connection loss must trigger temporary recovery, then
        // return to batched observation without restarting the Server.
        disconnect.notify_one();
        sqlx::query("DROP VIEW edge_pending_dispatch")
            .execute(pool.get())
            .await
            .expect("remove slow observer fixture");
        sqlx::query(
            "CREATE VIEW edge_pending_dispatch AS \
             SELECT 'slow-user' AS user_id, 'slow-edge' AS edge_agent_id, 'pending' AS status",
        )
        .execute(pool.get())
        .await
        .expect("replace it with a fast pending dispatch");
        let mut slow_wake = wakes.remove(0);
        let mut unrelated_wake = wakes.remove(0);
        drop(wakes);
        drop(late_wake);
        tokio::time::sleep(EDGE_DISPATCH_WAKE_UNKNOWN_RETRY_DELAY + Duration::from_secs(2)).await;
        slow_wake.borrow_and_update();
        unrelated_wake.borrow_and_update();
        tokio::time::timeout(Duration::from_secs(2), slow_wake.changed())
            .await
            .expect("the recovered observer must find pending work")
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(500), unrelated_wake.changed())
                .await
                .is_err(),
            "a recovered observer must stop broadcasting recovery probes"
        );
        drop(slow_wake);
        drop(unrelated_wake);
        tokio::time::timeout(Duration::from_secs(12), async {
            while hub.running.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("observer must finish its owned query and stop after disconnect");
        observed_pool.close().await;
        proxy.abort();
        pool.get().close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP DATABASE `{database}`")))
            .execute(admin.get())
            .await
            .expect("drop isolated wake test database");
    }

    #[test]
    fn claimed_dispatch_row_decode_preserves_null_result_for_pending_rows() {
        let row = decode_claimed_dispatch_row(&FakeEdgeDispatchRow::pending_without_result())
            .expect("pending row with null result_json is valid");

        assert_eq!(row.result_json, None);
        assert_eq!(row.status, "dispatched");
    }

    #[test]
    fn claimed_dispatch_row_decode_fails_loudly_on_any_column_error() {
        for column in [
            "user_id",
            "session_id",
            "run_id",
            "turn_chain_id",
            "edge_agent_id",
            "request_id",
            "payload_json",
            "result_json",
            "pending_wait_us",
        ] {
            let error =
                decode_claimed_dispatch_row(&FakeEdgeDispatchRow::fail_on(column)).unwrap_err();
            assert!(
                error.contains("edge_dispatch poll row decode") && error.contains(column),
                "decode error should identify poll row column `{column}`: {error}"
            );
        }
    }

    #[test]
    fn backlog_row_decode_preserves_counts_and_rounds_age_up_to_ms() {
        let row = decode_backlog_row(&FakeEdgeDispatchRow::complete()).unwrap();

        assert_eq!(
            row,
            EdgeDispatchBacklogRow {
                pending_rows: 3,
                dispatched_rows: 2,
                oldest_pending_age_ms: 2,
                oldest_dispatched_age_ms: 3,
            }
        );
    }

    #[test]
    fn backlog_row_decode_fails_loudly_on_any_column_error() {
        for column in [
            "pending_rows",
            "dispatched_rows",
            "oldest_pending_age_us",
            "oldest_dispatched_age_us",
        ] {
            let error = decode_backlog_row(&FakeEdgeDispatchRow::fail_on(column)).unwrap_err();
            assert!(
                error.contains("edge_dispatch backlog row decode") && error.contains(column),
                "backlog decode error should identify `{column}`: {error}"
            );
        }
    }

    #[test]
    fn apply_backlog_metrics_updates_cross_pod_backlog_gauges() {
        let metrics = crate::multi_agent::metrics::shared_metrics();

        apply_backlog_metrics(
            &metrics,
            EdgeDispatchBacklogRow {
                pending_rows: 7,
                dispatched_rows: 5,
                oldest_pending_age_ms: 11_000,
                oldest_dispatched_age_ms: 13_000,
            },
        );

        assert_eq!(metrics.dispatch_pending_rows.load(Ordering::Relaxed), 7);
        assert_eq!(metrics.dispatch_dispatched_rows.load(Ordering::Relaxed), 5);
        assert_eq!(
            metrics
                .dispatch_oldest_pending_age_ms
                .load(Ordering::Relaxed),
            11_000
        );
        assert_eq!(
            metrics
                .dispatch_oldest_dispatched_age_ms
                .load(Ordering::Relaxed),
            13_000
        );
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
    async fn matrixone_backlog_metrics_decode_real_aggregate_types() {
        let pool = setup_edge_dispatch_db_it().await;
        let request_id = format!("edge-backlog-{}", Uuid::new_v4());
        let identity = EdgeDispatchIdentity::new(
            format!("user-{request_id}"),
            format!("session-{request_id}"),
            format!("run-{request_id}"),
            format!("chain-{request_id}"),
            &request_id,
        );
        cleanup_edge_dispatch_fixture(&pool, &identity).await;

        DatabaseEdgeDispatchService::from_shared(&pool)
            .admit_dispatch(&identity, "edge-backlog-agent", &json!({"tool":"probe"}))
            .await
            .expect("insert backlog fixture");

        let metrics = crate::multi_agent::metrics::shared_metrics();
        refresh_edge_dispatch_backlog_metrics(&pool, &metrics)
            .await
            .expect("MatrixOne SUM/COALESCE columns must decode as signed integers");
        assert!(
            metrics.dispatch_pending_rows.load(Ordering::Relaxed) >= 1,
            "the inserted pending row must be visible in DB-authoritative gauges"
        );

        cleanup_edge_dispatch_fixture(&pool, &identity).await;
    }

    #[test]
    fn failed_dispatch_result_json_uses_tool_error_status() {
        let identity =
            EdgeDispatchIdentity::new("user-1", "session-1", "run-1", "turn-chain-1", "request-1");
        let result = edge_dispatch_failed_result_json(
            identity.request_id_json_value(),
            "edge dispatch expired".to_string(),
        );
        let value: serde_json::Value =
            serde_json::from_str(&result).expect("failed result should be JSON");

        assert_eq!(value["request_id"], identity.request_id_json_value());
        assert_eq!(value["status"], "error");
        assert_eq!(value["output"], "edge dispatch expired");
        assert_eq!(value["duration_ms"], 0);
    }

    #[test]
    fn durable_payload_comparison_uses_json_semantics_not_database_formatting() {
        let persisted =
            r#"{"duration_ms": 12, "output": "ok", "request_id": "r1", "status": "completed"}"#;
        let reordered_replay =
            r#"{"request_id":"r1","status":"completed","output":"ok","duration_ms":12}"#;

        assert!(json_payloads_match(persisted, reordered_replay).unwrap());
        assert!(
            !json_payloads_match(
                persisted,
                r#"{"request_id":"r1","status":"completed","output":"changed","duration_ms":12}"#
            )
            .unwrap()
        );
        assert!(
            json_payloads_match(persisted, "not-json")
                .unwrap_err()
                .contains("replayed durable payload is invalid JSON")
        );
        assert!(
            json_payloads_match("not-json", reordered_replay)
                .unwrap_err()
                .contains("persisted durable payload is invalid JSON")
        );
    }

    #[test]
    fn dispatch_payload_serialization_is_ordered_and_keeps_numeric_types() {
        let payload: serde_json::Value = serde_json::from_str(
            r#"{"args":{"timestamp":8.949943645992772e17,"zero":-0.0},"tool":"bash"}"#,
        )
        .unwrap();
        let reordered: serde_json::Value = serde_json::from_str(
            r#"{"tool":"bash","args":{"zero":-0.0,"timestamp":8.949943645992772e17}}"#,
        )
        .unwrap();
        assert_eq!(payload.to_string(), reordered.to_string());
        let mut changed = reordered;
        changed["args"]["zero"] = json!(0.0);
        assert_ne!(payload.to_string(), changed.to_string());
    }

    #[test]
    fn claimed_dispatch_update_count_must_match_selected_rows() {
        validate_claimed_dispatch_update_count(2, 2).expect("matching counts are valid");

        let error = validate_claimed_dispatch_update_count(2, 1)
            .expect_err("claim/update mismatch must fail loudly");
        assert!(
            error.contains("claimed 2 rows but updated 1"),
            "error should identify claim/update mismatch: {error}"
        );
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
    async fn matrixone_tool_outcome_inspection_is_scoped_and_bounded() {
        let pool = setup_edge_dispatch_db_it().await;
        let writer = DatabaseEdgeDispatchService::from_shared(&pool);
        let reader = DatabaseEdgeDispatchService::from_shared(&pool);
        let suffix = Uuid::new_v4().to_string();
        let user = format!("inspect-{suffix}");
        let session = format!("session-{suffix}");
        let run = format!("run-{suffix}");
        let edge = format!("edge-{suffix}");
        for index in 0..18 {
            let identity = EdgeDispatchIdentity::new(
                &user,
                &session,
                &run,
                "turn",
                format!("request-{index:02}"),
            );
            writer
                .admit_and_claim_direct_dispatch(
                    &identity,
                    &edge,
                    &json!({"request_id": identity.request_id, "tool":"grep", "args":{}}),
                )
                .await
                .unwrap();
            let result = if index == 0 {
                json!({"output":"large".repeat(20_000)})
            } else {
                json!({"output":format!("result-{index}")})
            };
            assert!(
                writer
                    .deliver_result(&identity, &edge, &result.to_string())
                    .await
                    .unwrap()
            );
        }
        let page = reader
            .inspect_run_dispatches(&user, &session, &run, None, None)
            .await
            .unwrap();
        assert_eq!(page.len(), 17);
        assert!(page.iter().all(|row| row.result_json.is_none()));
        assert!(
            page.iter()
                .all(|row| row.tool_name.as_deref() == Some("grep"))
        );
        let next = reader
            .inspect_run_dispatches(
                &user,
                &session,
                &run,
                Some((&page[15].turn_chain_id, &page[15].request_id)),
                None,
            )
            .await
            .unwrap();
        assert_eq!(next.len(), 2);
        let result = reader
            .inspect_run_dispatches(&user, &session, &run, None, Some(("turn", "request-01")))
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(result[0].result_json.as_ref().unwrap())
                .unwrap()["output"],
            "result-1"
        );
        let large = reader
            .inspect_run_dispatches(&user, &session, &run, None, Some(("turn", "request-00")))
            .await
            .unwrap();
        assert!(large[0].result_too_large);
        assert!(large[0].result_json.is_none());
        assert!(
            reader
                .inspect_run_dispatches("other-user", &session, &run, None, None)
                .await
                .is_err()
        );
        let invocation = astra_turn_types::ToolInvocationIdentity::new(
            &user,
            &session,
            &run,
            "turn",
            "service-account-call",
        )
        .unwrap();
        let edge_owner = format!("edge-owner-{suffix}");
        let cross_owner = EdgeDispatchIdentity::new(
            &edge_owner,
            &session,
            &run,
            "turn",
            invocation.storage_key(),
        );
        let payload = json!({"identity":invocation, "tool":"grep", "args":{}});
        writer
            .admit_and_claim_direct_dispatch(&cross_owner, &edge, &payload)
            .await
            .unwrap();
        let cross_result = reader
            .inspect_run_dispatches(
                &user,
                &session,
                &run,
                None,
                Some((&cross_owner.turn_chain_id, &cross_owner.request_id)),
            )
            .await
            .unwrap();
        assert_eq!(cross_result.len(), 1);
        assert_eq!(cross_result[0].edge_owner_user_id, edge_owner);
        assert_eq!(
            reader
                .callback_run_owner(&cross_owner, &edge)
                .await
                .unwrap()
                .as_deref(),
            Some(user.as_str())
        );
        assert_eq!(
            reader
                .callback_run_owner(&cross_owner, "wrong-edge")
                .await
                .unwrap(),
            None
        );
        assert!(
            reader
                .inspect_run_dispatches(
                    "other-user",
                    &session,
                    &run,
                    None,
                    Some((&cross_owner.turn_chain_id, &cross_owner.request_id)),
                )
                .await
                .is_err()
        );

        let other_owner = EdgeDispatchIdentity::new(
            format!("other-edge-owner-{suffix}"),
            &session,
            &run,
            "turn",
            &cross_owner.request_id,
        );
        writer
            .admit_and_claim_direct_dispatch(&other_owner, &edge, &payload)
            .await
            .unwrap();
        assert!(
            reader
                .inspect_run_dispatches(
                    &user,
                    &session,
                    &run,
                    None,
                    Some((&cross_owner.turn_chain_id, &cross_owner.request_id)),
                )
                .await
                .is_err(),
            "two owners for the same invocation must not be silently selected"
        );
        for identity in [&cross_owner, &other_owner] {
            sqlx::query("DELETE FROM edge_pending_dispatch WHERE user_id = ? AND session_id = ? AND run_id = ?")
                .bind(&identity.user_id).bind(&session).bind(&run).execute(pool.get()).await.unwrap();
        }
        sqlx::query(
            "DELETE FROM edge_pending_dispatch WHERE user_id = ? AND session_id = ? AND run_id = ?",
        )
        .bind(&user)
        .bind(&session)
        .bind(&run)
        .execute(pool.get())
        .await
        .unwrap();
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
    async fn matrixone_direct_admission_is_never_visible_to_relay_polling() {
        let pool = setup_edge_dispatch_db_it().await;
        let direct = DatabaseEdgeDispatchService::from_shared(&pool);
        let relay = DatabaseEdgeDispatchService::from_shared(&pool);
        let unique = Uuid::new_v4().to_string();
        let user_id = format!("direct-user-{unique}");
        let edge_agent_id = format!("direct-edge-{unique}");
        let identity = EdgeDispatchIdentity::new(
            &user_id,
            format!("session-{unique}"),
            format!("run-{unique}"),
            format!("chain-{unique}"),
            format!("request-{unique}"),
        );
        cleanup_edge_dispatch_fixture(&pool, &identity).await;
        let payload = json!({
            "request_id": identity.request_id,
            "tool": "bash",
            "args": {"command": "😀x".repeat(30_000)},
        })
        .to_string();

        let admission = direct
            .admit_and_claim_direct_dispatch(
                &identity,
                &edge_agent_id,
                &parse_dispatch_payload(&payload),
            )
            .await
            .expect("direct admission");

        assert_eq!(admission, EdgeDirectDispatchAdmission::Claimed);
        let persisted = sqlx::query(
            "SELECT edge_agent_id, payload_json, status \
             FROM edge_pending_dispatch \
             WHERE user_id = ? AND session_id = ? AND run_id = ? \
               AND turn_chain_id = ? AND request_id = ?",
        )
        .bind(&identity.user_id)
        .bind(&identity.session_id)
        .bind(&identity.run_id)
        .bind(&identity.turn_chain_id)
        .bind(&identity.request_id)
        .fetch_one(pool.get())
        .await
        .expect("direct admission must persist its claimed row");
        assert_eq!(
            persisted
                .try_get::<String, _>("edge_agent_id")
                .expect("persisted edge owner"),
            edge_agent_id
        );
        assert_eq!(
            persisted
                .try_get::<String, _>("status")
                .expect("persisted dispatch status"),
            "dispatched"
        );
        assert_eq!(
            persisted.try_get::<String, _>("payload_json").unwrap(),
            payload
        );
        assert_eq!(
            direct
                .admit_and_claim_direct_dispatch(
                    &identity,
                    &edge_agent_id,
                    &parse_dispatch_payload(&payload)
                )
                .await
                .unwrap(),
            EdgeDirectDispatchAdmission::Observing
        );
        assert!(matches!(
            direct
                .admit_and_claim_direct_dispatch(&identity, &edge_agent_id, &json!({}))
                .await,
            Err(EdgeDispatchAdmissionError::Rejected(_))
        ));
        assert!(
            relay
                .poll_pending(&user_id, &edge_agent_id)
                .await
                .expect("relay poll")
                .is_empty(),
            "directly claimed credentials-free payload must never reach relay delivery"
        );
        cleanup_edge_dispatch_fixture(&pool, &identity).await;
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
    async fn matrixone_dispatch_round_trip_survives_cross_pod_delivery() {
        let pool = setup_edge_dispatch_db_it().await;
        let pod_a = DatabaseEdgeDispatchService::from_shared(&pool);
        let pod_b = DatabaseEdgeDispatchService::from_shared(&pool);
        let pod_c = DatabaseEdgeDispatchService::from_shared(&pool);

        let user_id = format!("edge-user-{}", Uuid::new_v4());
        let other_user_id = format!("edge-other-{}", Uuid::new_v4());
        let edge_agent_id = format!("edge-agent-{}", Uuid::new_v4());
        let other_edge_agent_id = format!("edge-other-agent-{}", Uuid::new_v4());
        let request_id = format!("edge-req-{}", Uuid::new_v4());
        let identity = EdgeDispatchIdentity::new(
            &user_id,
            format!("session-{request_id}"),
            format!("run-{request_id}"),
            format!("chain-{request_id}"),
            &request_id,
        );
        let other_identity = EdgeDispatchIdentity::new(
            &other_user_id,
            identity.session_id.clone(),
            identity.run_id.clone(),
            identity.turn_chain_id.clone(),
            &request_id,
        );
        cleanup_edge_dispatch_fixture(&pool, &identity).await;
        cleanup_edge_dispatch_fixture(&pool, &other_identity).await;
        let mut cross_pod_wakeup = pod_b
            .subscribe_pending_wakeup(&user_id, &edge_agent_id)
            .await
            .expect("database edge service starts wake subscription")
            .expect("database edge service exposes wake subscription");

        let payload = json!({
            "request_id": request_id,
            "tool": "bash",
            "args": {"cmd": "printf ok", "content": "😀x".repeat(30_000)}
        })
        .to_string();
        assert_eq!(
            pod_a
                .admit_dispatch(&identity, &edge_agent_id, &parse_dispatch_payload(&payload))
                .await
                .expect("admit pending dispatch"),
            EdgeDispatchAdmission::Pending
        );
        pod_a
            .admit_dispatch(&identity, &edge_agent_id, &parse_dispatch_payload(&payload))
            .await
            .expect("duplicate insert should be idempotent");
        tokio::time::timeout(Duration::from_secs(1), cross_pod_wakeup.changed())
            .await
            .expect("replacement pod should observe durable admission without 2s socket polling")
            .expect("wake observer remains active");

        let wrong_agent_rows = pod_b
            .poll_pending(&user_id, &other_edge_agent_id)
            .await
            .expect("wrong edge agent poll");
        assert!(wrong_agent_rows.is_empty());

        assert!(
            !pod_c
                .deliver_result(
                    &other_identity,
                    &edge_agent_id,
                    r#"{"status":"completed","output":"wrong-user"}"#,
                )
                .await
                .expect("wrong owner deliver should not error")
        );
        assert!(
            !pod_c
                .deliver_result(
                    &identity,
                    &other_edge_agent_id,
                    r#"{"status":"completed","output":"wrong-agent"}"#,
                )
                .await
                .expect("wrong agent deliver should not error")
        );

        let claimed = pod_b
            .poll_pending(&user_id, &edge_agent_id)
            .await
            .expect("correct edge agent poll");
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].user_id, user_id);
        assert_eq!(claimed[0].request_id, request_id);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&claimed[0].payload_json)
                .expect("claimed payload should be valid JSON"),
            serde_json::from_str::<serde_json::Value>(&payload).expect("payload should be JSON")
        );
        assert_eq!(claimed[0].status, "dispatched");
        let payload = json!({"tool": "bash", "args": {"cmd": "printf ok"}}).to_string();
        assert!(
            pod_b
                .poll_pending(&user_id, &edge_agent_id)
                .await
                .expect("already claimed poll")
                .is_empty(),
            "claimed dispatch must not be re-claimed by another pod"
        );

        let unsent = identity.for_request_id(format!("unsent-{request_id}"));
        let same_request_other_session = EdgeDispatchIdentity::new(
            &user_id,
            format!("other-session-{request_id}"),
            &identity.run_id,
            &identity.turn_chain_id,
            &unsent.request_id,
        );
        let cancelled = identity.for_request_id(format!("cancelled-{request_id}"));
        let cancelled_after = identity.for_request_id(format!("cancelled-after-{request_id}"));
        let other_edge = identity.for_request_id(format!("other-edge-{request_id}"));
        for (row, edge) in [
            (&unsent, &edge_agent_id),
            (&same_request_other_session, &edge_agent_id),
            (&cancelled, &edge_agent_id),
            (&cancelled_after, &edge_agent_id),
            (&other_edge, &other_edge_agent_id),
        ] {
            pod_a
                .admit_dispatch(row, edge, &parse_dispatch_payload(&payload))
                .await
                .unwrap();
        }
        assert_eq!(
            pod_b
                .poll_pending(&user_id, &edge_agent_id)
                .await
                .unwrap()
                .len(),
            4
        );
        assert_eq!(
            pod_b
                .poll_pending(&user_id, &other_edge_agent_id)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            pod_c
                .fail_dispatch(&cancelled, &edge_agent_id, "cancelled")
                .await
                .unwrap()
        );
        pod_b
            .settle_unsent(
                &edge_agent_id,
                &[
                    (unsent.clone(), r#"{"result":"unsent"}"#.into()),
                    (
                        same_request_other_session.clone(),
                        r#"{"result":"other session"}"#.into(),
                    ),
                    (cancelled.clone(), r#"{"result":"cancelled"}"#.into()),
                    (cancelled_after.clone(), r#"{"result":"settled"}"#.into()),
                    (other_edge.clone(), r#"{"result":"wrong edge"}"#.into()),
                ],
            )
            .await
            .expect("settle the untouched part of a claim in one update");
        assert!(
            !pod_c
                .fail_dispatch(&cancelled_after, &edge_agent_id, "cancelled")
                .await
                .unwrap()
        );
        assert_eq!(
            pod_c
                .wait_result(&unsent, Some(Duration::ZERO))
                .await
                .unwrap()
                .as_deref(),
            Some(r#"{"result":"unsent"}"#)
        );
        assert_eq!(
            pod_c
                .wait_result(&cancelled_after, Some(Duration::ZERO))
                .await
                .unwrap()
                .as_deref(),
            Some(r#"{"result":"settled"}"#)
        );
        assert_eq!(
            pod_c
                .wait_result(&same_request_other_session, Some(Duration::ZERO))
                .await
                .unwrap()
                .as_deref(),
            Some(r#"{"result":"other session"}"#),
            "equal request ids from different sessions must retain their own receipts"
        );
        assert!(
            pod_c
                .poll_pending(&user_id, &edge_agent_id)
                .await
                .unwrap()
                .is_empty(),
            "settled work must never be returned to the pending queue"
        );
        assert!(
            pod_c
                .poll_pending(&user_id, &other_edge_agent_id)
                .await
                .unwrap()
                .is_empty(),
            "settling unsent work must not reset a different Edge's claim"
        );
        assert!(
            pod_c
                .is_server_cancelled_dispatch(&cancelled, &edge_agent_id)
                .await
                .unwrap()
        );

        let wait_identity = identity.clone();
        let wait = tokio::spawn(async move {
            pod_a
                .wait_result(&wait_identity, Some(std::time::Duration::from_secs(5)))
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let result_json = json!({
            "request_id": request_id,
            "status": "completed",
            "output": "ok",
            "duration_ms": 12,
            "tool_result_fields": {
                "nested": {
                    "integral_float": 86400.0,
                    "other_integral_float": 1800.0,
                    "fraction": 0.125,
                    "integer": 1800,
                    "large_integer": 9007199254740993_u64,
                }
            }
        })
        .to_string();
        assert!(
            pod_c
                .deliver_result(&identity, &edge_agent_id, &result_json)
                .await
                .expect("cross-pod deliver result")
        );
        let waited = wait
            .await
            .expect("wait task should join")
            .expect("wait_result should not fail")
            .expect("wait_result should observe completed result");
        let fresh_reader = DatabaseEdgeDispatchService::from_shared(&pool);
        let recovered = fresh_reader
            .wait_result(&identity, Some(std::time::Duration::ZERO))
            .await
            .expect("exact read must not require a prior waiter")
            .expect("completed durable row must survive coordinator recreation");
        assert_eq!(
            recovered, result_json,
            "durable receipts must preserve signed bytes, including numeric representation"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&recovered).unwrap(),
            serde_json::from_str::<serde_json::Value>(&result_json).unwrap()
        );
        assert!(
            fresh_reader
                .wait_result(&other_identity, Some(std::time::Duration::ZERO))
                .await
                .unwrap()
                .is_none(),
            "exact recovery must preserve owner isolation"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&waited)
                .expect("waited result should be JSON"),
            serde_json::from_str::<serde_json::Value>(&result_json).expect("result should be JSON")
        );
        assert!(
            pod_c
                .deliver_result(&identity, &edge_agent_id, &result_json)
                .await
                .expect("exact replay after lost acknowledgement"),
            "an exact terminal replay must be acknowledged idempotently"
        );
        let mut changed: serde_json::Value = serde_json::from_str(&result_json).unwrap();
        changed["tool_result_fields"]["nested"]["fraction"] = json!(0.25);
        assert!(
            !pod_c
                .deliver_result(&identity, &edge_agent_id, &changed.to_string())
                .await
                .unwrap(),
            "a different execution fact cannot replace an accepted receipt"
        );
        assert!(
            !pod_c
                .deliver_result(
                    &identity,
                    &edge_agent_id,
                    r#"{"status":"completed","output":"duplicate"}"#,
                )
                .await
                .expect("duplicate terminal deliver should not error"),
            "terminal result must not be overwritten"
        );

        let row = sqlx::query(
            "SELECT status, result_json
             FROM edge_pending_dispatch
             WHERE user_id = ? AND session_id = ? AND run_id = ? AND turn_chain_id = ? AND request_id = ?",
        )
        .bind(&identity.user_id)
        .bind(&identity.session_id)
        .bind(&identity.run_id)
        .bind(&identity.turn_chain_id)
        .bind(&identity.request_id)
        .fetch_one(pool.get())
        .await
        .expect("load terminal dispatch row");
        let status: String = row.try_get("status").expect("status");
        let stored_result: Option<String> = row.try_get("result_json").expect("result_json");
        assert_eq!(status, "completed");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(
                stored_result.as_deref().expect("stored result_json")
            )
            .expect("stored result_json should be JSON"),
            serde_json::from_str::<serde_json::Value>(&result_json).expect("result should be JSON")
        );

        cleanup_edge_dispatch_fixture(&pool, &identity).await;
        cleanup_edge_dispatch_fixture(&pool, &other_identity).await;
        cleanup_edge_dispatch_fixture(&pool, &unsent).await;
        cleanup_edge_dispatch_fixture(&pool, &same_request_other_session).await;
        cleanup_edge_dispatch_fixture(&pool, &cancelled).await;
        cleanup_edge_dispatch_fixture(&pool, &cancelled_after).await;
        cleanup_edge_dispatch_fixture(&pool, &other_edge).await;
    }
}

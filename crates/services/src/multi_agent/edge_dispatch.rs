//! Edge dispatch relay: cross-pod tool dispatch via DB-backed queue.
//!
//! When the in-memory edge ledger times out or a tool targets an agent connected
//! to a different pod, the dispatch relay persists the request and the
//! owning pod's process-wide wake observer notifies its edge WS handler for
//! delivery. Per-connection polling remains only as a recovery fallback.
//! Results flow back through `deliver_result`.
//!
//! Split from the monolithic `multi_agent.rs`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
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

/// Canonicalize the durable identity of an Edge dispatch payload.
///
/// `runtime_filesystem_boundary` was retired from `edge_tool_request` during
/// the managed Runner rollout. Older servers may already have persisted that
/// field, so it cannot distinguish the same dispatch across a rolling upgrade.
/// Every other message type and field remains part of the exact JSON identity.
pub fn canonicalize_edge_dispatch_payload(
    payload_json: &str,
) -> Result<serde_json::Value, serde_json::Error> {
    let mut payload = serde_json::from_str::<serde_json::Value>(payload_json)?;
    if payload.get("type").and_then(serde_json::Value::as_str) == Some("edge_tool_request")
        && let Some(object) = payload.as_object_mut()
    {
        object.remove("runtime_filesystem_boundary");
    }
    Ok(payload)
}

fn canonical_edge_dispatch_payload_json(payload_json: &str) -> Result<String, serde_json::Error> {
    serde_json::to_string(&canonicalize_edge_dispatch_payload(payload_json)?)
}

fn edge_dispatch_payloads_match(persisted: &str, replayed: &str) -> Result<bool, String> {
    let persisted = canonicalize_edge_dispatch_payload(persisted)
        .map_err(|error| format!("persisted durable payload is invalid JSON: {error}"))?;
    let replayed = canonicalize_edge_dispatch_payload(replayed)
        .map_err(|error| format!("replayed durable payload is invalid JSON: {error}"))?;
    Ok(persisted == replayed)
}

async fn rollback_edge_dispatch_tx(tx: sqlx::Transaction<'_, MySql>, context: &'static str) {
    if let Err(error) = tx.rollback().await {
        tracing::warn!(context, %error, "edge_dispatch rollback failed");
    }
}

#[async_trait]
pub trait EdgeDispatchService: Send + Sync {
    /// Insert a new pending dispatch idempotently inside the full turn boundary.
    async fn insert_dispatch(
        &self,
        identity: &EdgeDispatchIdentity,
        edge_agent_id: &str,
        payload_json: &str,
    ) -> Result<(), String>;

    /// Admit a durable dispatch and report whether the exact identity already
    /// has terminal evidence. Implementations with a durable store must also
    /// reject an existing identity bound to different payload or edge owner.
    /// The default is only suitable when `insert_dispatch` errors prove that
    /// no durable write occurred; stores with ambiguous commit responses must
    /// override this method and return `OutcomeUnknown`.
    async fn admit_dispatch(
        &self,
        identity: &EdgeDispatchIdentity,
        edge_agent_id: &str,
        payload_json: &str,
    ) -> Result<EdgeDispatchAdmission, EdgeDispatchAdmissionError> {
        self.insert_dispatch(identity, edge_agent_id, payload_json)
            .await
            .map_err(EdgeDispatchAdmissionError::Rejected)?;
        Ok(EdgeDispatchAdmission::Pending)
    }

    /// Atomically claim an admitted pending row for direct socket delivery.
    /// False means another relay already owns dispatch and the caller must
    /// observe the durable result instead of sending a duplicate.
    async fn claim_direct_dispatch(
        &self,
        _identity: &EdgeDispatchIdentity,
        _edge_agent_id: &str,
    ) -> Result<bool, String> {
        Ok(true)
    }

    /// Atomically admit a durable row and reserve it for direct socket
    /// delivery. Durable implementations must prevent relay polling from
    /// observing a newly admitted request between these two state changes.
    async fn admit_and_claim_direct_dispatch(
        &self,
        identity: &EdgeDispatchIdentity,
        edge_agent_id: &str,
        payload_json: &str,
    ) -> Result<EdgeDirectDispatchAdmission, EdgeDispatchAdmissionError> {
        match self
            .admit_dispatch(identity, edge_agent_id, payload_json)
            .await?
        {
            EdgeDispatchAdmission::Terminal(result) => {
                Ok(EdgeDirectDispatchAdmission::Terminal(result))
            }
            EdgeDispatchAdmission::Pending => self
                .claim_direct_dispatch(identity, edge_agent_id)
                .await
                .map(|claimed| {
                    if claimed {
                        EdgeDirectDispatchAdmission::Claimed
                    } else {
                        EdgeDirectDispatchAdmission::Observing
                    }
                })
                .map_err(EdgeDispatchAdmissionError::OutcomeUnknown),
        }
    }

    /// Poll for pending dispatches targeting the given (user, agent) pairs.
    /// Returns dispatches that are still 'pending' and not yet dispatched.
    async fn poll_pending(
        &self,
        user_id: &str,
        edge_agent_id: &str,
    ) -> Result<Vec<EdgeDispatchRow>, String>;

    /// Subscribe to process-local durable-admission wakeups. The database
    /// remains the truth and callers must keep a polling fallback because a
    /// dispatch may be admitted by another pod or while no subscriber exists.
    async fn subscribe_pending_wakeup(
        &self,
        _user_id: &str,
        _edge_agent_id: &str,
    ) -> Option<tokio::sync::watch::Receiver<u64>> {
        None
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

    /// Move an in-flight dispatch to a failed terminal state.
    async fn fail_dispatch(
        &self,
        identity: &EdgeDispatchIdentity,
        edge_agent_id: &str,
        reason: &str,
    ) -> Result<bool, String>;

    /// Poll for a specific request's result. Returns Some(result_json) when completed.
    async fn wait_result(
        &self,
        identity: &EdgeDispatchIdentity,
        timeout: std::time::Duration,
    ) -> Result<Option<String>, String>;

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EdgeDispatchAdmissionError {
    Rejected(String),
    OutcomeUnknown(String),
}

impl std::fmt::Display for EdgeDispatchAdmissionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(message) | Self::OutcomeUnknown(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for EdgeDispatchAdmissionError {}

pub struct DatabaseEdgeDispatchService {
    pool: sqlx::Pool<sqlx::MySql>,
    metrics: Option<SharedMultiAgentMetrics>,
    wait_coordinator: Arc<EdgeDispatchWaitCoordinator>,
    wake_hub: Arc<EdgeDispatchWakeHub>,
}

impl DatabaseEdgeDispatchService {
    pub fn new(pool: sqlx::Pool<sqlx::MySql>) -> Self {
        let wait_coordinator = Arc::new(EdgeDispatchWaitCoordinator::new(pool.clone()));
        let wake_hub = Arc::new(EdgeDispatchWakeHub::new(pool.clone()));
        Self {
            pool,
            metrics: None,
            wait_coordinator,
            wake_hub,
        }
    }

    pub fn from_shared(shared: &astra_core::SharedPool) -> Self {
        Self::new(shared.get().clone())
    }

    pub fn with_metrics(mut self, metrics: SharedMultiAgentMetrics) -> Self {
        self.metrics = Some(metrics);
        self
    }
}

struct EdgeDispatchWakeHub {
    subscribers: tokio::sync::Mutex<HashMap<(String, String), tokio::sync::watch::Sender<u64>>>,
    pool: Option<sqlx::Pool<MySql>>,
    running: AtomicBool,
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
        subscribers.retain(|_, sender| sender.receiver_count() > 0);
        if let Some(sender) = subscribers.get(&key) {
            sender.send_modify(|generation| *generation = generation.saturating_add(1));
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
        tokio::spawn(async move { observer.run().await });
    }

    async fn run(self: Arc<Self>) {
        loop {
            let targets = {
                let mut subscribers = self.subscribers.lock().await;
                subscribers.retain(|_, sender| sender.receiver_count() > 0);
                subscribers.keys().cloned().collect::<Vec<_>>()
            };
            if targets.is_empty() {
                self.running.store(false, Ordering::Release);
                let has_subscribers = !self.subscribers.lock().await.is_empty();
                if !has_subscribers
                    || self
                        .running
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_err()
                {
                    return;
                }
                continue;
            }

            for batch in targets.chunks(EDGE_DISPATCH_WAKE_BATCH_SIZE) {
                match self.pending_targets(batch).await {
                    Ok(pending) => {
                        for (user_id, edge_agent_id) in pending {
                            self.notify(&user_id, &edge_agent_id).await;
                        }
                    }
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            batch_size = batch.len(),
                            "edge_dispatch cross-pod wake observation failed; per-connection recovery polling remains active"
                        );
                    }
                }
            }
            tokio::time::sleep(EDGE_DISPATCH_WAKE_POLL_INTERVAL).await;
        }
    }

    async fn pending_targets(
        &self,
        targets: &[(String, String)],
    ) -> Result<Vec<(String, String)>, String> {
        let pool = self
            .pool
            .as_ref()
            .ok_or_else(|| "edge_dispatch wake observer has no database pool".to_owned())?;
        let mut query = sqlx::QueryBuilder::<MySql>::new(
            "SELECT user_id, edge_agent_id FROM edge_pending_dispatch
             WHERE status = 'pending' AND (",
        );
        for (index, (user_id, edge_agent_id)) in targets.iter().enumerate() {
            if index > 0 {
                query.push(" OR ");
            }
            query
                .push("(user_id = ")
                .push_bind(user_id)
                .push(" AND edge_agent_id = ")
                .push_bind(edge_agent_id)
                .push(")");
        }
        query.push(") GROUP BY user_id, edge_agent_id");
        query
            .build()
            .fetch_all(pool)
            .await
            .map_err(|error| format!("edge_dispatch cross-pod wake query: {error}"))?
            .into_iter()
            .map(|row| {
                Ok((
                    row.try_get::<String, _>("user_id")
                        .map_err(|error| format!("edge_dispatch wake user decode: {error}"))?,
                    row.try_get::<String, _>("edge_agent_id")
                        .map_err(|error| format!("edge_dispatch wake agent decode: {error}"))?,
                ))
            })
            .collect()
    }
}

const EDGE_DISPATCH_WAIT_BATCH_SIZE: usize = 128;
const EDGE_DISPATCH_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const EDGE_DISPATCH_WAKE_BATCH_SIZE: usize = 128;
const EDGE_DISPATCH_WAKE_POLL_INTERVAL: Duration = Duration::from_millis(100);

type WaitResolution = Result<Option<String>, String>;

struct EdgeDispatchWaitCoordinator {
    pool: sqlx::Pool<sqlx::MySql>,
    waiters: tokio::sync::Mutex<
        HashMap<EdgeDispatchIdentity, tokio::sync::watch::Sender<Option<WaitResolution>>>,
    >,
    running: AtomicBool,
}

enum PolledDispatchState {
    Pending,
    Terminal(String),
}

impl EdgeDispatchWaitCoordinator {
    fn new(pool: sqlx::Pool<sqlx::MySql>) -> Self {
        Self {
            pool,
            waiters: tokio::sync::Mutex::new(HashMap::new()),
            running: AtomicBool::new(false),
        }
    }

    async fn subscribe(
        self: &Arc<Self>,
        identity: EdgeDispatchIdentity,
    ) -> tokio::sync::watch::Receiver<Option<WaitResolution>> {
        let receiver = {
            let mut waiters = self.waiters.lock().await;
            if let Some(sender) = waiters.get(&identity) {
                sender.subscribe()
            } else {
                let (sender, receiver) = tokio::sync::watch::channel(None);
                waiters.insert(identity, sender);
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
        loop {
            let identities = {
                let mut waiters = self.waiters.lock().await;
                waiters.retain(|_, sender| sender.receiver_count() > 0);
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

            for batch in identities.chunks(EDGE_DISPATCH_WAIT_BATCH_SIZE) {
                tracing::trace!(
                    waiter_count = identities.len(),
                    batch_size = batch.len(),
                    "edge_dispatch batched result observation"
                );
                match self.poll_batch(batch).await {
                    Ok(states) => {
                        let mut waiters = self.waiters.lock().await;
                        for identity in batch {
                            match states.get(identity) {
                                Some(PolledDispatchState::Pending) => {}
                                Some(PolledDispatchState::Terminal(result)) => {
                                    if let Some(sender) = waiters.remove(identity) {
                                        let _ = sender.send(Some(Ok(Some(result.clone()))));
                                    }
                                }
                                None => {
                                    if let Some(sender) = waiters.remove(identity) {
                                        let _ = sender.send(Some(Ok(None)));
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
                        let mut waiters = self.waiters.lock().await;
                        for identity in batch {
                            if let Some(sender) = waiters.remove(identity) {
                                let _ = sender.send(Some(Err(error.clone())));
                            }
                        }
                    }
                }
            }
            tokio::time::sleep(EDGE_DISPATCH_WAIT_POLL_INTERVAL).await;
        }
    }

    async fn poll_batch(
        &self,
        identities: &[EdgeDispatchIdentity],
    ) -> Result<HashMap<EdgeDispatchIdentity, PolledDispatchState>, String> {
        let mut query = sqlx::QueryBuilder::<MySql>::new(
            "SELECT user_id, session_id, run_id, turn_chain_id, request_id, \
                    status, CAST(result_json AS CHAR) AS result_json \
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
            .map_err(|error| format!("edge_dispatch batched wait_result: {error}"))?;
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
        if let Some(sender) = self.waiters.lock().await.remove(identity) {
            let _ = sender.send(Some(Ok(Some(result_json.to_string()))));
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

#[async_trait]
impl EdgeDispatchService for DatabaseEdgeDispatchService {
    #[tracing::instrument(skip(self, identity, payload_json), fields(user_id = %identity.user_id, session_id = %identity.session_id, run_id = %identity.run_id, turn_chain_id = %identity.turn_chain_id, edge_agent_id = %edge_agent_id, request_id = %identity.request_id))]
    async fn insert_dispatch(
        &self,
        identity: &EdgeDispatchIdentity,
        edge_agent_id: &str,
        payload_json: &str,
    ) -> Result<(), String> {
        if !identity.is_complete() {
            return Err("edge_dispatch insert: incomplete dispatch identity".to_string());
        }
        let payload_json = canonical_edge_dispatch_payload_json(payload_json)
            .map_err(|error| format!("edge_dispatch insert payload is invalid JSON: {error}"))?;
        // Idempotent insert inside a full turn boundary: duplicate calls for
        // the same dispatched tool are harmless retries, while same request_id
        // values in other runs/chains remain isolated.
        match sqlx::query(
            "INSERT IGNORE INTO edge_pending_dispatch \
             (user_id, session_id, run_id, turn_chain_id, edge_agent_id, request_id, payload_json, status) \
             VALUES (?, ?, ?, ?, ?, ?, ?, 'pending')",
        )
        .bind(&identity.user_id)
        .bind(&identity.session_id)
        .bind(&identity.run_id)
        .bind(&identity.turn_chain_id)
        .bind(edge_agent_id)
        .bind(&identity.request_id)
        .bind(&payload_json)
        .execute(&self.pool)
        .await
        {
            Ok(r) => {
                if r.rows_affected() > 0
                    && let Some(ref m) = self.metrics
                {
                    m.dispatch_queue_depth.fetch_add(1, Ordering::Relaxed);
                }
                self.wake_hub
                    .notify(&identity.user_id, edge_agent_id)
                    .await;
                Ok(())
            }
            Err(e) => Err(format!("edge_dispatch insert: {e}")),
        }
    }

    async fn admit_dispatch(
        &self,
        identity: &EdgeDispatchIdentity,
        edge_agent_id: &str,
        payload_json: &str,
    ) -> Result<EdgeDispatchAdmission, EdgeDispatchAdmissionError> {
        if !identity.is_complete() {
            return Err(EdgeDispatchAdmissionError::Rejected(
                "edge_dispatch admit: incomplete dispatch identity".to_string(),
            ));
        }
        if edge_agent_id.trim().is_empty() {
            return Err(EdgeDispatchAdmissionError::Rejected(
                "edge_dispatch admit: edge_agent_id is required".to_string(),
            ));
        }
        let payload_json = canonical_edge_dispatch_payload_json(payload_json).map_err(|error| {
            EdgeDispatchAdmissionError::Rejected(format!(
                "edge_dispatch admit: payload is invalid JSON: {error}"
            ))
        })?;
        self.insert_dispatch(identity, edge_agent_id, &payload_json)
            .await
            .map_err(EdgeDispatchAdmissionError::OutcomeUnknown)?;
        let row = sqlx::query(
            "SELECT edge_agent_id, CAST(payload_json AS CHAR) AS payload_json, \
                    status, CAST(result_json AS CHAR) AS result_json \
             FROM edge_pending_dispatch \
             WHERE user_id = ? AND session_id = ? AND run_id = ? \
               AND turn_chain_id = ? AND request_id = ?",
        )
        .bind(&identity.user_id)
        .bind(&identity.session_id)
        .bind(&identity.run_id)
        .bind(&identity.turn_chain_id)
        .bind(&identity.request_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| {
            EdgeDispatchAdmissionError::OutcomeUnknown(format!(
                "edge_dispatch admit SELECT: {error}"
            ))
        })?
        .ok_or_else(|| {
            EdgeDispatchAdmissionError::OutcomeUnknown(
                "edge_dispatch admitted row disappeared before verification".to_string(),
            )
        })?;
        let persisted_edge: String = row.try_get("edge_agent_id").map_err(|error| {
            EdgeDispatchAdmissionError::OutcomeUnknown(format!(
                "edge_dispatch admit edge_agent_id decode: {error}"
            ))
        })?;
        let persisted_payload: String = row.try_get("payload_json").map_err(|error| {
            EdgeDispatchAdmissionError::OutcomeUnknown(format!(
                "edge_dispatch admit payload_json decode: {error}"
            ))
        })?;
        if persisted_edge != edge_agent_id
            || !edge_dispatch_payloads_match(&persisted_payload, &payload_json)
                .map_err(EdgeDispatchAdmissionError::OutcomeUnknown)?
        {
            return Err(EdgeDispatchAdmissionError::Rejected(format!(
                "edge_dispatch identity {} conflicts with its durable edge owner or payload",
                identity.request_id
            )));
        }
        let status: String = row.try_get("status").map_err(|error| {
            EdgeDispatchAdmissionError::OutcomeUnknown(format!(
                "edge_dispatch admit status decode: {error}"
            ))
        })?;
        match status.as_str() {
            "pending" | "dispatched" => Ok(EdgeDispatchAdmission::Pending),
            "completed" | "failed" => row
                .try_get::<Option<String>, _>("result_json")
                .map_err(|error| {
                    EdgeDispatchAdmissionError::OutcomeUnknown(format!(
                        "edge_dispatch admit result_json decode: {error}"
                    ))
                })?
                .map(EdgeDispatchAdmission::Terminal)
                .ok_or_else(|| {
                    EdgeDispatchAdmissionError::OutcomeUnknown(format!(
                        "edge_dispatch terminal identity {} has no result evidence",
                        identity.request_id
                    ))
                }),
            other => Err(EdgeDispatchAdmissionError::OutcomeUnknown(format!(
                "edge_dispatch identity {} has unsupported status {other}",
                identity.request_id
            ))),
        }
    }

    async fn admit_and_claim_direct_dispatch(
        &self,
        identity: &EdgeDispatchIdentity,
        edge_agent_id: &str,
        payload_json: &str,
    ) -> Result<EdgeDirectDispatchAdmission, EdgeDispatchAdmissionError> {
        if !identity.is_complete() {
            return Err(EdgeDispatchAdmissionError::Rejected(
                "edge_dispatch direct admit: incomplete dispatch identity".to_string(),
            ));
        }
        if edge_agent_id.trim().is_empty() {
            return Err(EdgeDispatchAdmissionError::Rejected(
                "edge_dispatch direct admit: edge_agent_id is required".to_string(),
            ));
        }
        let payload_json = canonical_edge_dispatch_payload_json(payload_json).map_err(|error| {
            EdgeDispatchAdmissionError::Rejected(format!(
                "edge_dispatch direct admit: payload is invalid JSON: {error}"
            ))
        })?;

        let mut tx = self.pool.begin().await.map_err(|error| {
            EdgeDispatchAdmissionError::OutcomeUnknown(format!(
                "edge_dispatch direct admit begin: {error}"
            ))
        })?;
        let inserted = match sqlx::query(
            "INSERT IGNORE INTO edge_pending_dispatch \
             (user_id, session_id, run_id, turn_chain_id, edge_agent_id, request_id, \
              payload_json, status, dispatched_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, 'dispatched', NOW(6))",
        )
        .bind(&identity.user_id)
        .bind(&identity.session_id)
        .bind(&identity.run_id)
        .bind(&identity.turn_chain_id)
        .bind(edge_agent_id)
        .bind(&identity.request_id)
        .bind(&payload_json)
        .execute(&mut *tx)
        .await
        {
            Ok(result) => result.rows_affected() > 0,
            Err(error) => {
                rollback_edge_dispatch_tx(tx, "direct admit insert").await;
                return Err(EdgeDispatchAdmissionError::OutcomeUnknown(format!(
                    "edge_dispatch direct admit INSERT: {error}"
                )));
            }
        };
        let row = match sqlx::query(
            "SELECT edge_agent_id, CAST(payload_json AS CHAR) AS payload_json, \
                    status, CAST(result_json AS CHAR) AS result_json \
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
        {
            Ok(Some(row)) => row,
            Ok(None) => {
                rollback_edge_dispatch_tx(tx, "direct admit missing row").await;
                return Err(EdgeDispatchAdmissionError::OutcomeUnknown(
                    "edge_dispatch direct admitted row disappeared".to_string(),
                ));
            }
            Err(error) => {
                rollback_edge_dispatch_tx(tx, "direct admit select").await;
                return Err(EdgeDispatchAdmissionError::OutcomeUnknown(format!(
                    "edge_dispatch direct admit SELECT: {error}"
                )));
            }
        };
        let persisted_edge: String = row.try_get("edge_agent_id").map_err(|error| {
            EdgeDispatchAdmissionError::OutcomeUnknown(format!(
                "edge_dispatch direct admit edge decode: {error}"
            ))
        })?;
        let persisted_payload: String = row.try_get("payload_json").map_err(|error| {
            EdgeDispatchAdmissionError::OutcomeUnknown(format!(
                "edge_dispatch direct admit payload decode: {error}"
            ))
        })?;
        if persisted_edge != edge_agent_id
            || !edge_dispatch_payloads_match(&persisted_payload, &payload_json)
                .map_err(EdgeDispatchAdmissionError::OutcomeUnknown)?
        {
            rollback_edge_dispatch_tx(tx, "direct admit conflict").await;
            return Err(EdgeDispatchAdmissionError::Rejected(format!(
                "edge_dispatch identity {} conflicts with its durable edge owner or payload",
                identity.request_id
            )));
        }
        let status: String = row.try_get("status").map_err(|error| {
            EdgeDispatchAdmissionError::OutcomeUnknown(format!(
                "edge_dispatch direct admit status decode: {error}"
            ))
        })?;
        let outcome = match status.as_str() {
            "pending" => {
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
                .map_err(|error| {
                    EdgeDispatchAdmissionError::OutcomeUnknown(format!(
                        "edge_dispatch direct admit UPDATE: {error}"
                    ))
                })?;
                if updated.rows_affected() != 1 {
                    rollback_edge_dispatch_tx(tx, "direct admit update count").await;
                    return Err(EdgeDispatchAdmissionError::OutcomeUnknown(
                        "edge_dispatch direct admit did not claim exactly one pending row"
                            .to_string(),
                    ));
                }
                EdgeDirectDispatchAdmission::Claimed
            }
            "dispatched" if inserted => EdgeDirectDispatchAdmission::Claimed,
            "dispatched" => EdgeDirectDispatchAdmission::Observing,
            "completed" | "failed" => row
                .try_get::<Option<String>, _>("result_json")
                .map_err(|error| {
                    EdgeDispatchAdmissionError::OutcomeUnknown(format!(
                        "edge_dispatch direct admit result decode: {error}"
                    ))
                })?
                .map(EdgeDirectDispatchAdmission::Terminal)
                .ok_or_else(|| {
                    EdgeDispatchAdmissionError::OutcomeUnknown(format!(
                        "edge_dispatch terminal identity {} has no result evidence",
                        identity.request_id
                    ))
                })?,
            other => {
                rollback_edge_dispatch_tx(tx, "direct admit unsupported status").await;
                return Err(EdgeDispatchAdmissionError::OutcomeUnknown(format!(
                    "edge_dispatch direct admit observed unsupported status {other}"
                )));
            }
        };
        tx.commit().await.map_err(|error| {
            EdgeDispatchAdmissionError::OutcomeUnknown(format!(
                "edge_dispatch direct admit commit: {error}"
            ))
        })?;
        if inserted && let Some(ref metrics) = self.metrics {
            metrics.dispatch_queue_depth.fetch_add(1, Ordering::Relaxed);
        }
        Ok(outcome)
    }

    async fn subscribe_pending_wakeup(
        &self,
        user_id: &str,
        edge_agent_id: &str,
    ) -> Option<tokio::sync::watch::Receiver<u64>> {
        Some(self.wake_hub.subscribe(user_id, edge_agent_id).await)
    }

    async fn claim_direct_dispatch(
        &self,
        identity: &EdgeDispatchIdentity,
        edge_agent_id: &str,
    ) -> Result<bool, String> {
        let claimed = sqlx::query(
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
        .execute(&self.pool)
        .await
        .map_err(|error| format!("edge_dispatch direct claim: {error}"))?;
        if claimed.rows_affected() > 0 {
            return Ok(true);
        }
        let status = sqlx::query(
            "SELECT status FROM edge_pending_dispatch \
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
        .map_err(|error| format!("edge_dispatch verify direct claim: {error}"))?
        .ok_or_else(|| "edge_dispatch direct claim row disappeared".to_string())?
        .try_get::<String, _>("status")
        .map_err(|error| format!("edge_dispatch direct claim status decode: {error}"))?;
        match status.as_str() {
            "dispatched" | "completed" | "failed" => Ok(false),
            other => Err(format!(
                "edge_dispatch direct claim observed unsupported status {other}"
            )),
        }
    }
    #[tracing::instrument(skip(self), fields(user_id = %user_id, edge_agent_id = %edge_agent_id))]
    async fn poll_pending(
        &self,
        user_id: &str,
        edge_agent_id: &str,
    ) -> Result<Vec<EdgeDispatchRow>, String> {
        // Fast path: non-locking COUNT before opening any transaction.
        // MatrixOne can acquire a table-level lock on SELECT FOR UPDATE even
        // when the result set is empty, adding significant latency (seconds)
        // per 2-second poll cycle when there is nothing to claim. Skipping
        // the transaction when the count is zero avoids that entirely. The
        // tiny TOCTOU window (a new row arriving between COUNT and FOR UPDATE)
        // is benign: we will pick it up in the next poll interval.
        let fast_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM edge_pending_dispatch \
             WHERE user_id = ? AND edge_agent_id = ? AND status = 'pending'",
        )
        .bind(user_id)
        .bind(edge_agent_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| format!("edge_dispatch poll COUNT: {e}"))?;

        if fast_count == 0 {
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

        let rows = match sqlx::query(
            "SELECT user_id, session_id, run_id, turn_chain_id, edge_agent_id, request_id, \
             CAST(payload_json AS CHAR) AS payload_json, \
             CAST(result_json AS CHAR) AS result_json, \
             status, \
             COALESCE(TIMESTAMPDIFF(MICROSECOND, created_at, NOW(6)), 0) AS pending_wait_us \
             FROM edge_pending_dispatch \
             WHERE user_id = ? AND edge_agent_id = ? AND status = 'pending' \
             ORDER BY created_at ASC, request_id ASC LIMIT 50 \
             FOR UPDATE",
        )
        .bind(user_id)
        .bind(edge_agent_id)
        .fetch_all(&mut *tx)
        .await
        {
            Ok(r) => r,
            Err(e) => {
                rollback_edge_dispatch_tx(tx, "poll select").await;
                return Err(format!("edge_dispatch poll SELECT: {e}"));
            }
        };

        if rows.is_empty() {
            tx.commit()
                .await
                .map_err(|e| format!("edge_dispatch poll commit (no rows): {e}"))?;
            return Ok(vec![]);
        }

        let claimed_rows: Vec<EdgeDispatchRow> = match rows
            .iter()
            .map(decode_claimed_dispatch_row)
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(rows) => rows,
            Err(e) => {
                rollback_edge_dispatch_tx(tx, "decode claimed rows").await;
                return Err(e);
            }
        };

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

    #[tracing::instrument(skip(self, identity, result_json), fields(user_id = %identity.user_id, session_id = %identity.session_id, run_id = %identity.run_id, turn_chain_id = %identity.turn_chain_id, request_id = %identity.request_id, edge_agent_id = %edge_agent_id))]
    async fn deliver_result(
        &self,
        identity: &EdgeDispatchIdentity,
        edge_agent_id: &str,
        result_json: &str,
    ) -> Result<bool, String> {
        let start = std::time::Instant::now();
        let n = sqlx::query(
            "UPDATE edge_pending_dispatch \
             SET status = 'completed', result_json = ?, completed_at = NOW(6) \
             WHERE user_id = ? AND session_id = ? AND run_id = ? AND turn_chain_id = ? \
               AND request_id = ? AND edge_agent_id = ? AND status IN ('pending', 'dispatched')",
        )
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
                "SELECT status, CAST(result_json AS CHAR) AS result_json \
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

    #[tracing::instrument(skip(self, identity), fields(user_id = %identity.user_id, session_id = %identity.session_id, run_id = %identity.run_id, turn_chain_id = %identity.turn_chain_id, edge_agent_id = %edge_agent_id, request_id = %identity.request_id, reason = %reason))]
    async fn fail_dispatch(
        &self,
        identity: &EdgeDispatchIdentity,
        edge_agent_id: &str,
        reason: &str,
    ) -> Result<bool, String> {
        let output = format!("edge dispatch {reason}");
        let result_json =
            edge_dispatch_failed_result_json(identity.request_id_json_value(), output);
        let n = sqlx::query(
            "UPDATE edge_pending_dispatch \
             SET status = 'failed', result_json = ?, completed_at = NOW(6) \
             WHERE user_id = ? AND session_id = ? AND run_id = ? AND turn_chain_id = ? \
               AND request_id = ? AND edge_agent_id = ? \
               AND status IN ('pending', 'dispatched')",
        )
        .bind(&result_json)
        .bind(&identity.user_id)
        .bind(&identity.session_id)
        .bind(&identity.run_id)
        .bind(&identity.turn_chain_id)
        .bind(&identity.request_id)
        .bind(edge_agent_id)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("edge_dispatch fail_dispatch: {e}"))?;
        let affected = n.rows_affected() > 0;
        if affected && let Some(ref m) = self.metrics {
            saturating_decrement(&m.dispatch_queue_depth);
            m.dispatch_failed_total.fetch_add(1, Ordering::Relaxed);
        }
        if affected {
            self.wait_coordinator.resolve(identity, &result_json).await;
        }
        Ok(affected)
    }

    #[tracing::instrument(skip(self, identity), fields(user_id = %identity.user_id, session_id = %identity.session_id, run_id = %identity.run_id, turn_chain_id = %identity.turn_chain_id, request_id = %identity.request_id, timeout_ms = timeout.as_millis()))]
    async fn wait_result(
        &self,
        identity: &EdgeDispatchIdentity,
        timeout: std::time::Duration,
    ) -> Result<Option<String>, String> {
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
        match tokio::time::timeout(timeout, wait).await {
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
               AND created_at <= DATE_SUB(NOW(6), INTERVAL ? SECOND)",
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
               AND COALESCE(completed_at, created_at) <= DATE_SUB(NOW(6), INTERVAL ? SECOND)",
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
    async fn insert_dispatch(
        &self,
        _identity: &EdgeDispatchIdentity,
        _edge_agent_id: &str,
        _payload_json: &str,
    ) -> Result<(), String> {
        Err("edge dispatch service not configured".to_string())
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
        _timeout: std::time::Duration,
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
    use uuid::Uuid;

    static EDGE_DISPATCH_DB: tokio::sync::OnceCell<astra_core::SharedPool> =
        tokio::sync::OnceCell::const_new();

    async fn setup_edge_dispatch_db_it() -> astra_core::SharedPool {
        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "set ASTRA_TEST_DB_IT=1 for ignored integration tests"
        );
        EDGE_DISPATCH_DB
            .get_or_init(|| async {
                let settings = astra_core::MatrixOneSettings::from_env();
                let catalog = std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG")
                    .unwrap_or_else(|_| "mysql".to_string());
                crate::storage::ensure_core_schema(&settings, &catalog)
                    .await
                    .expect("ensure_core_schema");
                astra_core::SharedPool::new(&settings)
                    .await
                    .expect("SharedPool::new")
            })
            .await
            .clone()
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
            .insert_dispatch(&identity, "edge-backlog-agent", r#"{"tool":"probe"}"#)
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
        let result = edge_dispatch_failed_result_json(
            serde_json::json!("request-1"),
            "edge dispatch expired".to_string(),
        );
        let value: serde_json::Value =
            serde_json::from_str(&result).expect("failed result should be JSON");

        assert_eq!(value["request_id"], "request-1");
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
    fn edge_tool_request_payload_comparison_retires_only_the_legacy_boundary() {
        let boundary_free = serde_json::json!({
            "type": "edge_tool_request",
            "request_id": "request-1",
            "tool": "bash",
            "args": {"command": "pwd"},
            "timeout_secs": 30
        });
        let mut legacy = boundary_free.clone();
        legacy["runtime_filesystem_boundary"] = serde_json::json!({
            "workspace_root": "/sandbox",
            "read_only_paths": ["/sandbox/.moi/runtime/task-1"]
        });

        let legacy = legacy.to_string();
        let boundary_free = boundary_free.to_string();
        assert!(
            !json_payloads_match(&legacy, &boundary_free).unwrap(),
            "generic durable JSON equality must remain strict"
        );
        assert!(edge_dispatch_payloads_match(&legacy, &boundary_free).unwrap());

        let changed_tool = boundary_free.replace("\"bash\"", "\"write_file\"");
        assert!(!edge_dispatch_payloads_match(&legacy, &changed_tool).unwrap());

        let unrelated_with_boundary = serde_json::json!({
            "type": "edge_other_message",
            "runtime_filesystem_boundary": {"workspace_root": "/sandbox"}
        })
        .to_string();
        let unrelated_without_boundary =
            serde_json::json!({"type": "edge_other_message"}).to_string();
        assert!(
            !edge_dispatch_payloads_match(&unrelated_with_boundary, &unrelated_without_boundary)
                .unwrap(),
            "the compatibility exception must not affect unrelated message types"
        );
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
        let payload = json!({"request_id": identity.request_id, "tool": "materialize_attachment"})
            .to_string();

        let admission = direct
            .admit_and_claim_direct_dispatch(&identity, &edge_agent_id, &payload)
            .await
            .expect("direct admission");

        assert_eq!(admission, EdgeDirectDispatchAdmission::Claimed);
        let persisted = sqlx::query(
            "SELECT edge_agent_id, CAST(payload_json AS CHAR) AS payload_json, status \
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
        assert!(
            json_payloads_match(
                &persisted
                    .try_get::<String, _>("payload_json")
                    .expect("persisted dispatch payload"),
                &payload,
            )
            .expect("persisted direct payload is valid JSON")
        );
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
            .expect("database edge service exposes wake subscription");

        let payload = json!({
            "request_id": request_id,
            "tool": "bash",
            "args": {"cmd": "printf ok"}
        })
        .to_string();
        pod_a
            .insert_dispatch(&identity, &edge_agent_id, &payload)
            .await
            .expect("insert pending dispatch");
        pod_a
            .insert_dispatch(&identity, &edge_agent_id, &payload)
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
        assert!(
            pod_b
                .poll_pending(&user_id, &edge_agent_id)
                .await
                .expect("already claimed poll")
                .is_empty(),
            "claimed dispatch must not be re-claimed by another pod"
        );

        let wait_identity = identity.clone();
        let wait = tokio::spawn(async move {
            pod_a
                .wait_result(&wait_identity, std::time::Duration::from_secs(5))
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let result_json = json!({
            "request_id": request_id,
            "status": "completed",
            "output": "ok",
            "duration_ms": 12
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
            "SELECT status, CAST(result_json AS CHAR) AS result_json
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
    }
}

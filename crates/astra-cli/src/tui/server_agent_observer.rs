//! Non-blocking observer for the server-owned durable session run tree.
//!
//! The TUI tick only schedules bounded one-shot requests and reads a small
//! projection. Network/auth work never runs inline with key handling or draw.

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use astra_thin_client::{
    SESSION_RUN_TREE_SCHEMA_VERSION, SessionRunTreeSnapshot, ThinClient, ThinClientError,
};
use futures_util::{FutureExt, StreamExt};

const ACTIVE_POLL_INTERVAL: Duration = Duration::from_secs(1);
const ROOT_ACTIVE_POLL_INTERVAL: Duration = Duration::from_secs(5);
const QUIET_POLL_INTERVAL: Duration = Duration::from_secs(15);
const COVERED_QUIET_POLL_INTERVAL: Duration = Duration::from_secs(60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const NOTICE_FRAME_TIMEOUT: Duration = Duration::from_secs(45);
const NOTICE_CAPABILITY_RETRY_INTERVAL: Duration = Duration::from_secs(300);
const MAX_FAILURE_BACKOFF: Duration = Duration::from_secs(30);
const SESSION_RUN_NODE_LIMIT: u32 = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServerAgentTruthState {
    Unbound,
    Loading,
    Confirmed,
    Stale,
    Unavailable,
}

impl Default for ServerAgentTruthState {
    fn default() -> Self {
        Self::Unbound
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ServerAgentProjection {
    pub sequence: u64,
    pub truth_state: ServerAgentTruthState,
    pub snapshot: Option<SessionRunTreeSnapshot>,
    /// Client-local start of the read that produced `snapshot`.
    pub snapshot_started_at: Option<Instant>,
}

impl ServerAgentProjection {
    fn unbound() -> Self {
        Self {
            sequence: 0,
            truth_state: ServerAgentTruthState::Unbound,
            snapshot: None,
            snapshot_started_at: None,
        }
    }
}

pub(crate) struct ServerAgentObserver {
    inner: Arc<ObserverInner>,
    fetch_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    notice_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

struct ObserverInner {
    api: ThinClient,
    profile: Option<String>,
    state: Mutex<ObserverState>,
}

struct ObserverState {
    session_id: String,
    binding_generation: u64,
    projection: ServerAgentProjection,
    request_in_flight: bool,
    refresh_pending: bool,
    last_fetch: Instant,
    consecutive_failures: u32,
    access_token: Option<String>,
    notice_stream_ready: bool,
    missing_admission_reads: u8,
    pending_admissions: Vec<String>,
}

impl ServerAgentObserver {
    pub(crate) fn new(api: ThinClient, profile: Option<&str>, session_id: Option<&str>) -> Self {
        let session_id = normalized_session_id(session_id);
        let truth_state = if session_id.is_empty() {
            ServerAgentTruthState::Unbound
        } else {
            ServerAgentTruthState::Loading
        };
        Self {
            inner: Arc::new(ObserverInner {
                api,
                profile: profile.map(str::to_string),
                state: Mutex::new(ObserverState {
                    session_id,
                    binding_generation: 0,
                    projection: ServerAgentProjection {
                        truth_state,
                        ..ServerAgentProjection::unbound()
                    },
                    request_in_flight: false,
                    refresh_pending: false,
                    last_fetch: Instant::now()
                        .checked_sub(QUIET_POLL_INTERVAL)
                        .unwrap_or_else(Instant::now),
                    consecutive_failures: 0,
                    access_token: None,
                    notice_stream_ready: false,
                    missing_admission_reads: 0,
                    pending_admissions: Vec::new(),
                }),
            }),
            fetch_task: Mutex::new(None),
            notice_task: Mutex::new(None),
        }
    }

    pub(crate) fn rebind_session(&self, session_id: Option<&str>) {
        let session_id = normalized_session_id(session_id);
        {
            let mut state = lock_state(&self.inner, "rebind_session");
            if state.session_id == session_id {
                return;
            }
            state.session_id = session_id;
            state.binding_generation = state.binding_generation.wrapping_add(1);
            state.notice_stream_ready = false;
            state.missing_admission_reads = 0;
            state.pending_admissions.clear();
            state.request_in_flight = false;
            state.refresh_pending = false;
            state.consecutive_failures = 0;
            state.last_fetch = Instant::now()
                .checked_sub(QUIET_POLL_INTERVAL)
                .unwrap_or_else(Instant::now);
            state.projection.sequence = state.projection.sequence.wrapping_add(1);
            state.projection.truth_state = if state.session_id.is_empty() {
                ServerAgentTruthState::Unbound
            } else {
                ServerAgentTruthState::Loading
            };
            state.projection.snapshot = None;
            state.projection.snapshot_started_at = None;
        }
        self.abort_fetch();
        abort_task(&self.notice_task);
    }

    /// Clone the snapshot only when there is a new projection to apply.
    pub(crate) fn projection_after(&self, sequence: Option<u64>) -> Option<ServerAgentProjection> {
        let state = lock_state(&self.inner, "projection_after");
        (sequence != Some(state.projection.sequence)).then(|| state.projection.clone())
    }

    /// An explicit user refresh may bypass the current backoff.
    pub(crate) fn request_refresh(&self) -> bool {
        self.schedule_refresh(true)
    }

    /// Live hints are coalesced and retain the existing read/backoff limits.
    pub(crate) fn notify_refresh(&self) -> bool {
        self.schedule_refresh(false)
    }

    fn schedule_refresh(&self, explicit: bool) -> bool {
        let mut state = lock_state(&self.inner, "schedule_refresh");
        if state.session_id.is_empty() {
            return false;
        }
        // A hint can follow the read's database snapshot. Keep the read,
        // but coalesce hints into one subsequent authoritative fetch.
        invalidate_observed_tree(&mut state);
        if state.request_in_flight {
            return false;
        }
        if explicit {
            state.last_fetch = Instant::now()
                .checked_sub(MAX_FAILURE_BACKOFF)
                .unwrap_or_else(Instant::now);
        }
        true
    }

    pub(crate) fn maybe_refresh(&self, attached_root: Option<(&str, &str)>) {
        self.maybe_start_notices();
        let (session_id, binding_generation, access_token) = {
            let mut state = lock_state(&self.inner, "maybe_refresh");
            if state.session_id.is_empty() || state.request_in_flight {
                return;
            }
            let elapsed = state.last_fetch.elapsed();
            if elapsed < ACTIVE_POLL_INTERVAL || elapsed < refresh_interval(&state, attached_root) {
                return;
            }
            state.refresh_pending = false;
            state.request_in_flight = true;
            // A first read after an unavailable snapshot is a real state
            // transition, not invisible retry machinery. Cached snapshots
            // remain `Stale` while they refresh so their provenance stays
            // truthful.
            if state.projection.snapshot.is_none()
                && state.projection.truth_state != ServerAgentTruthState::Loading
            {
                state.projection.truth_state = ServerAgentTruthState::Loading;
                state.projection.sequence = state.projection.sequence.wrapping_add(1);
            }
            (
                state.session_id.clone(),
                state.binding_generation,
                state.access_token.clone(),
            )
        };

        let inner = Arc::clone(&self.inner);
        let fetch_session_id = session_id.clone();
        self.spawn_fetch(binding_generation, session_id, async move {
            fetch_snapshot(
                &inner.api,
                inner.profile.as_deref(),
                &fetch_session_id,
                access_token,
            )
            .await
        });
    }

    fn maybe_start_notices(&self) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let mut task = self.notice_task.lock().unwrap_or_else(|e| e.into_inner());
        if task.is_some() {
            return;
        }
        let state = lock_state(&self.inner, "start_notice_stream");
        if state.session_id.is_empty() || state.access_token.is_none() {
            return;
        }
        let generation = state.binding_generation;
        let session_id = state.session_id.clone();
        drop(state);
        let inner = Arc::clone(&self.inner);
        *task = Some(runtime.spawn(async move {
            watch_admissions(inner, generation, session_id).await;
        }));
    }

    fn spawn_fetch<F>(&self, binding_generation: u64, session_id: String, fetch: F)
    where
        F: Future<Output = Result<SnapshotFetchSuccess, SnapshotFetchError>> + Send + 'static,
    {
        let runtime = match tokio::runtime::Handle::try_current() {
            Ok(runtime) => runtime,
            Err(_) => {
                apply_fetch_result(
                    &self.inner,
                    binding_generation,
                    &session_id,
                    Err(SnapshotFetchError::ObserverRuntimeUnavailable),
                );
                return;
            }
        };
        let inner = Arc::clone(&self.inner);
        let handle = runtime.spawn(async move {
            let result = match AssertUnwindSafe(fetch).catch_unwind().await {
                Ok(result) => result,
                Err(_) => Err(SnapshotFetchError::ObserverTaskPanicked),
            };
            apply_fetch_result(&inner, binding_generation, &session_id, result);
        });
        let mut current = match self.fetch_task.lock() {
            Ok(current) => current,
            Err(poisoned) => {
                tracing::warn!("server agent observer fetch handle poisoned; recovering");
                poisoned.into_inner()
            }
        };
        if let Some(previous) = current.replace(handle) {
            previous.abort();
        }
    }

    fn abort_fetch(&self) {
        abort_task(&self.fetch_task);
    }
}

impl Drop for ServerAgentObserver {
    fn drop(&mut self) {
        abort_task(&self.notice_task);
        abort_task(&self.fetch_task);
    }
}

fn abort_task(task: &Mutex<Option<tokio::task::JoinHandle<()>>>) {
    if let Some(handle) = task.lock().unwrap_or_else(|e| e.into_inner()).take() {
        handle.abort();
    }
}

fn settled_for_observation(run: &astra_thin_client::SessionRunNode) -> bool {
    run.status.is_terminal()
        && !(run.parent_run_id.is_some()
            && run.status == astra_thin_client::SessionRunLifecycleStatus::Cancelled)
}

fn covered_quiet_tree(state: &ObserverState) -> bool {
    state.notice_stream_ready
        && state.pending_admissions.is_empty()
        && !state.refresh_pending
        && state.consecutive_failures == 0
        && state.projection.truth_state == ServerAgentTruthState::Confirmed
        && state.projection.snapshot.as_ref().is_some_and(|snapshot| {
            !snapshot.truncated && snapshot.runs.iter().all(settled_for_observation)
        })
}

fn invalidate_observed_tree(state: &mut ObserverState) {
    state.refresh_pending = true;
    if state.projection.snapshot.is_some()
        && state.projection.truth_state == ServerAgentTruthState::Confirmed
    {
        state.projection.truth_state = ServerAgentTruthState::Stale;
        state.projection.sequence = state.projection.sequence.wrapping_add(1);
    }
}

async fn watch_admissions(inner: Arc<ObserverInner>, generation: u64, session_id: String) {
    use astra_thin_client::protocol::StreamEvent;

    use std::hash::{BuildHasher, RandomState};

    let stagger = Duration::from_millis(
        RandomState::new().hash_one(&session_id) % (QUIET_POLL_INTERVAL.as_millis() as u64),
    );
    loop {
        let mut retry_interval = QUIET_POLL_INTERVAL;
        let access_token = {
            let state = lock_state(&inner, "notice_stream_token");
            if state.binding_generation != generation {
                return;
            }
            state.access_token.clone()
        };
        if let Some(token) = access_token {
            let request = tokio::time::timeout(
                REQUEST_TIMEOUT,
                inner.api.watch_session_run_admissions(&token, &session_id),
            )
            .await;
            match request {
                Ok(Ok(mut notices)) => {
                    let mut handshaken = false;
                    while let Ok(Some(Ok(StreamEvent::Other { event_type, raw }))) =
                        tokio::time::timeout(NOTICE_FRAME_TIMEOUT, notices.next()).await
                    {
                        let mut state = lock_state(&inner, "notice_stream_event");
                        if state.binding_generation != generation {
                            return;
                        }
                        match event_type.as_str() {
                            "run_observation_heartbeat" if handshaken => {}
                            "run_observation_ready" => {
                                if handshaken {
                                    break;
                                }
                                handshaken = true;
                                if !state.notice_stream_ready {
                                    state.notice_stream_ready = true;
                                    invalidate_observed_tree(&mut state);
                                }
                            }
                            "run_observation_notice"
                                if raw["event"] == "admitted" && state.notice_stream_ready =>
                            {
                                let Some(id) = raw["run_id"].as_str() else {
                                    break;
                                };
                                if state.pending_admissions.len() >= 32 {
                                    break;
                                }
                                if !state.pending_admissions.iter().any(|pending| pending == id) {
                                    state.pending_admissions.push(id.to_owned());
                                }
                                invalidate_observed_tree(&mut state);
                            }
                            _ => break,
                        }
                    }
                }
                Ok(Err(ThinClientError::Api { status, .. }))
                    if status == reqwest::StatusCode::NOT_IMPLEMENTED
                        || status == reqwest::StatusCode::NOT_FOUND =>
                {
                    retry_interval = NOTICE_CAPABILITY_RETRY_INTERVAL;
                }
                _ => {}
            }
        }
        {
            let mut state = lock_state(&inner, "notice_stream_gap");
            if state.binding_generation != generation {
                return;
            }
            if state.notice_stream_ready {
                let was_quiet = covered_quiet_tree(&state) && !state.request_in_flight;
                state.notice_stream_ready = false;
                invalidate_observed_tree(&mut state);
                if was_quiet {
                    // One failed peer can wake thousands of idle observers.
                    // Spread their first fallback read over the old cadence.
                    state.refresh_pending = false;
                    state.last_fetch = Instant::now()
                        .checked_sub(stagger)
                        .unwrap_or_else(Instant::now);
                }
            }
        }
        tokio::time::sleep(retry_interval + stagger).await;
    }
}

fn normalized_session_id(session_id: Option<&str>) -> String {
    session_id.map(str::trim).unwrap_or_default().to_string()
}

fn refresh_interval(state: &ObserverState, attached_root: Option<(&str, &str)>) -> Duration {
    if state.consecutive_failures > 0 {
        let exponent = state.consecutive_failures.saturating_sub(1).min(5);
        return ACTIVE_POLL_INTERVAL
            .saturating_mul(1_u32 << exponent)
            .min(MAX_FAILURE_BACKOFF);
    }
    if state.refresh_pending {
        // Coalesced hints must not cause a database read for every HTTP response.
        return ACTIVE_POLL_INTERVAL;
    }
    if covered_quiet_tree(state) {
        // A notice stream has no replay or database read watermark. Even
        // with coverage, a snapshot can miss an already committed Run.
        return COVERED_QUIET_POLL_INTERVAL;
    }
    let Some(snapshot) = state.projection.snapshot.as_ref() else {
        return QUIET_POLL_INTERVAL;
    };
    use astra_thin_client::SessionRunLifecycleStatus::Running;
    let running = || snapshot.runs.iter().filter(|run| run.status == Running);
    if running().any(|run| run.parent_run_id.is_some()) {
        return ACTIVE_POLL_INTERVAL;
    }
    let Some(root) = running().next() else {
        return QUIET_POLL_INTERVAL;
    };
    if attached_root == Some((state.session_id.as_str(), root.run_id.as_str()))
        && state.projection.truth_state == ServerAgentTruthState::Confirmed
        && !snapshot.truncated
        && state.pending_admissions.is_empty()
        && snapshot
            .runs
            .iter()
            .all(|run| run.run_id == root.run_id || settled_for_observation(run))
    {
        // The foreground stream owns this root's progress. Keep the ordinary
        // safety read for sibling and cross-client discovery.
        QUIET_POLL_INTERVAL
    } else {
        ROOT_ACTIVE_POLL_INTERVAL
    }
}

#[derive(Debug)]
enum SnapshotFetchError {
    AuthenticationUnavailable,
    Timeout,
    ObserverRuntimeUnavailable,
    ObserverTaskPanicked,
    Transport(ThinClientError),
    SchemaVersion { received: u32 },
    SessionMismatch { received: String },
}

impl SnapshotFetchError {
    fn invalidates_access_token(&self) -> bool {
        matches!(
            self,
            Self::Transport(ThinClientError::Api { status, .. })
                if *status == reqwest::StatusCode::UNAUTHORIZED
                    || *status == reqwest::StatusCode::FORBIDDEN
        )
    }
}

struct SnapshotFetchSuccess {
    snapshot: SessionRunTreeSnapshot,
    access_token: String,
    started_at: Instant,
}

async fn fetch_snapshot(
    api: &ThinClient,
    profile: Option<&str>,
    session_id: &str,
    access_token: Option<String>,
) -> Result<SnapshotFetchSuccess, SnapshotFetchError> {
    let started_at = Instant::now();
    let token = match access_token {
        Some(token) => token,
        None => crate::cli::session::session_runtime::fresh_access_token(api, profile)
            .await
            .ok_or(SnapshotFetchError::AuthenticationUnavailable)?,
    };
    let snapshot = tokio::time::timeout(
        REQUEST_TIMEOUT,
        api.get_session_run_tree(Some(&token), session_id, SESSION_RUN_NODE_LIMIT),
    )
    .await
    .map_err(|_| SnapshotFetchError::Timeout)?
    .map_err(SnapshotFetchError::Transport)?;
    Ok(SnapshotFetchSuccess {
        snapshot: validate_snapshot(snapshot, session_id)?,
        access_token: token,
        started_at,
    })
}

fn validate_snapshot(
    snapshot: SessionRunTreeSnapshot,
    session_id: &str,
) -> Result<SessionRunTreeSnapshot, SnapshotFetchError> {
    if snapshot.schema_version != SESSION_RUN_TREE_SCHEMA_VERSION {
        return Err(SnapshotFetchError::SchemaVersion {
            received: snapshot.schema_version,
        });
    }
    if snapshot.session_id != session_id {
        return Err(SnapshotFetchError::SessionMismatch {
            received: snapshot.session_id,
        });
    }
    Ok(snapshot)
}

fn apply_fetch_result(
    inner: &ObserverInner,
    binding_generation: u64,
    session_id: &str,
    result: Result<SnapshotFetchSuccess, SnapshotFetchError>,
) {
    let mut state = lock_state(inner, "apply_fetch_result");
    if state.binding_generation != binding_generation || state.session_id != session_id {
        return;
    }
    state.request_in_flight = false;
    state.last_fetch = Instant::now();
    match result {
        Ok(success) => {
            let snapshot = success.snapshot;
            if !state.refresh_pending {
                state
                    .pending_admissions
                    .retain(|id| !snapshot.runs.iter().any(|node| &node.run_id == id));
                if snapshot.truncated && !state.pending_admissions.is_empty() {
                    // Keep the existing cadence for partial trees.
                    state.pending_admissions.clear();
                }
            }
            let missing_admission = !state.pending_admissions.is_empty();
            if missing_admission {
                state.missing_admission_reads = state.missing_admission_reads.saturating_add(1);
                if state.missing_admission_reads < 3 {
                    // Coalesce into the same one-second retry path.
                    state.refresh_pending = true;
                }
            } else {
                state.missing_admission_reads = 0;
            }
            let next_truth = if state.refresh_pending || missing_admission {
                ServerAgentTruthState::Stale
            } else {
                ServerAgentTruthState::Confirmed
            };
            // A later stale read can cover a live fact that arrived during a
            // previous read, even when the durable revision is unchanged.
            let changed = state.refresh_pending
                || state.projection.truth_state != next_truth
                || state
                    .projection
                    .snapshot
                    .as_ref()
                    .map(|current| current.snapshot_revision.as_str())
                    != Some(snapshot.snapshot_revision.as_str());
            state.consecutive_failures = 0;
            state.access_token = Some(success.access_token);
            state.projection.truth_state = next_truth;
            state.projection.snapshot = Some(snapshot);
            if changed {
                state.projection.snapshot_started_at = Some(success.started_at);
                state.projection.sequence = state.projection.sequence.wrapping_add(1);
            }
        }
        Err(error) => {
            if error.invalidates_access_token() {
                state.access_token = None;
            }
            state.consecutive_failures = state.consecutive_failures.saturating_add(1);
            let next_truth = if state.projection.snapshot.is_some() {
                ServerAgentTruthState::Stale
            } else {
                ServerAgentTruthState::Unavailable
            };
            if state.projection.truth_state != next_truth {
                state.projection.sequence = state.projection.sequence.wrapping_add(1);
            }
            state.projection.truth_state = next_truth;
            log_fetch_error(session_id, &error, state.consecutive_failures);
        }
    }
}

fn log_fetch_error(session_id: &str, error: &SnapshotFetchError, consecutive_failures: u32) {
    match error {
        SnapshotFetchError::AuthenticationUnavailable => tracing::debug!(
            session_id,
            consecutive_failures,
            "durable agent observer has no current authentication"
        ),
        SnapshotFetchError::Timeout => tracing::warn!(
            session_id,
            consecutive_failures,
            "durable agent snapshot request timed out"
        ),
        SnapshotFetchError::ObserverTaskPanicked => tracing::error!(
            session_id,
            consecutive_failures,
            "durable agent snapshot task panicked"
        ),
        SnapshotFetchError::ObserverRuntimeUnavailable => tracing::error!(
            session_id,
            consecutive_failures,
            "durable agent observer has no async runtime"
        ),
        SnapshotFetchError::Transport(error) => tracing::warn!(
            session_id,
            consecutive_failures,
            error = %error,
            "durable agent snapshot request failed"
        ),
        SnapshotFetchError::SchemaVersion { received } => tracing::error!(
            session_id,
            consecutive_failures,
            received,
            expected = SESSION_RUN_TREE_SCHEMA_VERSION,
            "durable agent snapshot schema mismatch"
        ),
        SnapshotFetchError::SessionMismatch { received } => tracing::error!(
            session_id,
            consecutive_failures,
            received,
            "durable agent snapshot returned a different session"
        ),
    }
}

fn lock_state<'a>(
    inner: &'a ObserverInner,
    context: &'static str,
) -> MutexGuard<'a, ObserverState> {
    match inner.state.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            tracing::warn!(context, "server agent observer state poisoned; recovering");
            poisoned.into_inner()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_thin_client::SessionRunLifecycleStatus;
    use axum::response::{IntoResponse, Response, Sse, sse::Event};
    use std::collections::VecDeque;
    use std::convert::Infallible;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn snapshot(session_id: &str, revision: &str) -> SessionRunTreeSnapshot {
        SessionRunTreeSnapshot {
            schema_version: SESSION_RUN_TREE_SCHEMA_VERSION,
            session_id: session_id.into(),
            snapshot_revision: revision.into(),
            observed_at: "2026-07-11T00:00:00Z".into(),
            node_limit: 200,
            truncated: false,
            runs: vec![astra_thin_client::SessionRunNode {
                run_id: "child-1".into(),
                parent_run_id: Some("root-1".into()),
                root_run_id: Some("root-1".into()),
                depth: 1,
                agent_id: Some("reviewer".into()),
                agent_name: Some("Reviewer".into()),
                status: SessionRunLifecycleStatus::Running,
                waiting_for: None,
                error_code: None,
                error_message: None,
                run_event_high_watermark: 1,
                total_tool_calls: 0,
                runtime: astra_thin_client::SessionRunRuntimeFacts {
                    runtime_profile: Some("server".into()),
                    model_name: Some("gpt-5".into()),
                    ..Default::default()
                },
                available_actions: vec![astra_thin_client::SessionRunAction::Cancel],
                created_at: "2026-07-11T00:00:00Z".into(),
                updated_at: "2026-07-11T00:00:01Z".into(),
            }],
        }
    }

    fn observer(session_id: &str) -> ServerAgentObserver {
        ServerAgentObserver::new(
            ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            None,
            Some(session_id),
        )
    }

    async fn notice_observer(responses: Vec<Response>) -> (ServerAgentObserver, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let route_calls = Arc::clone(&calls);
        let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
        let app = axum::Router::new().route(
            "/sessions/{session_id}/runs/admissions",
            axum::routing::get(move || {
                let route_calls = Arc::clone(&route_calls);
                let responses = Arc::clone(&responses);
                async move {
                    route_calls.fetch_add(1, Ordering::SeqCst);
                    responses.lock().unwrap().pop_front().unwrap()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let observer = ServerAgentObserver::new(
            ThinClient::new(&format!("http://{addr}"), None).unwrap(),
            None,
            Some("session-1"),
        );
        lock_state(&observer.inner, "test").access_token = Some("test-token".into());
        observer.maybe_start_notices();
        (observer, calls)
    }

    fn ready_frame() -> Result<Event, Infallible> {
        Ok(Event::default().data(r#"{"type":"run_observation_ready"}"#))
    }

    async fn wait_for_notice(predicate: impl Fn() -> bool) {
        // Keep Tokio's paused clock at the requested instant while loopback
        // I/O is in flight. Only the test advances retry and frame deadlines.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !predicate() {
            assert!(Instant::now() < deadline, "notice transition timed out");
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn reconnect_reconciles_pending_admissions_without_disabling_safety_reads() {
        use futures_util::stream;
        use tokio::sync::oneshot;

        let (disconnect, disconnected) = oneshot::channel();
        let (admit, admitted) = oneshot::channel();
        let first = Sse::new(
            stream::iter([
                ready_frame(),
                Ok(Event::default().data(
                    r#"{"type":"run_observation_notice","event":"admitted","run_id":"before-gap"}"#,
                )),
            ])
            .chain(stream::once(async move {
                disconnected.await.unwrap();
                Ok(Event::default().data(r#"{"type":"run_observation_gap"}"#))
            })),
        );
        let second = Sse::new(
            stream::iter([ready_frame()])
                .chain(stream::once(async move {
                    admitted.await.unwrap();
                    Ok(Event::default().data(
                        r#"{"type":"run_observation_notice","event":"admitted","run_id":"after-gap"}"#,
                    ))
                }))
                .chain(stream::pending()),
        );
        let (observer, _) =
            notice_observer(vec![first.into_response(), second.into_response()]).await;
        wait_for_notice(|| {
            lock_state(&observer.inner, "test").pending_admissions == ["before-gap"]
        })
        .await;
        disconnect.send(()).unwrap();
        wait_for_notice(|| !lock_state(&observer.inner, "test").notice_stream_ready).await;
        tokio::time::advance(2 * QUIET_POLL_INTERVAL).await;
        wait_for_notice(|| lock_state(&observer.inner, "test").notice_stream_ready).await;

        let settle = |ids: &[&str], raced: bool| {
            let mut tree = snapshot("session-1", "terminal");
            let mut run = tree.runs.pop().unwrap();
            run.status = SessionRunLifecycleStatus::Completed;
            tree.runs.extend(ids.iter().map(|id| {
                let mut run = run.clone();
                run.run_id = (*id).into();
                run
            }));
            lock_state(&observer.inner, "test").refresh_pending = raced;
            apply_fetch_result(
                &observer.inner,
                0,
                "session-1",
                Ok(SnapshotFetchSuccess {
                    snapshot: tree,
                    access_token: "test-token".into(),
                    started_at: Instant::now(),
                }),
            );
            refresh_interval(&lock_state(&observer.inner, "test"), None)
        };
        assert_eq!(settle(&["before-gap"], false), COVERED_QUIET_POLL_INTERVAL);
        assert_eq!(
            settle(&["before-gap", "during-gap"], false),
            COVERED_QUIET_POLL_INTERVAL
        );

        admit.send(()).unwrap();
        wait_for_notice(|| lock_state(&observer.inner, "test").pending_admissions == ["after-gap"])
            .await;
        assert_eq!(
            settle(&["before-gap", "during-gap", "after-gap"], true),
            ACTIVE_POLL_INTERVAL
        );
        assert_eq!(settle(&["before-gap"], false), ACTIVE_POLL_INTERVAL);
        assert_eq!(
            settle(&["before-gap", "during-gap", "after-gap"], false),
            COVERED_QUIET_POLL_INTERVAL
        );
    }

    #[tokio::test(start_paused = true)]
    async fn unsupported_admissions_endpoint_is_reprobed_on_the_same_binding() {
        use futures_util::stream;
        use reqwest::StatusCode;

        let ready = Sse::new(stream::iter([ready_frame()]).chain(stream::pending()));
        let (observer, calls) = notice_observer(vec![
            StatusCode::NOT_FOUND.into_response(),
            StatusCode::NOT_IMPLEMENTED.into_response(),
            ready.into_response(),
        ])
        .await;
        for call in 1..=2 {
            wait_for_notice(|| calls.load(Ordering::SeqCst) == call).await;
            // Give the localhost response time to reach the watcher before
            // advancing the paused clock through its capability cooldown.
            for _ in 0..64 {
                tokio::task::yield_now().await;
            }
            tokio::time::advance(2 * QUIET_POLL_INTERVAL).await;
            assert_eq!(calls.load(Ordering::SeqCst), call);
            tokio::time::advance(NOTICE_CAPABILITY_RETRY_INTERVAL).await;
        }
        wait_for_notice(|| lock_state(&observer.inner, "test").notice_stream_ready).await;
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn safety_reads_discover_runs_missed_by_initial_and_later_snapshots() {
        use crate::tui::chat_widget::ChatWidget;
        use futures_util::stream;

        let mut empty = snapshot("session-1", "older");
        empty.runs.clear();
        let current = snapshot("session-1", "current");
        let reads = Arc::new(AtomicUsize::new(0));
        let route_reads = Arc::clone(&reads);
        let app = axum::Router::new()
            .route(
                "/sessions/{session_id}/runs/admissions",
                axum::routing::get(|| async {
                    Sse::new(stream::iter([ready_frame()]).chain(stream::pending()))
                }),
            )
            .route(
                "/sessions/{session_id}/runs",
                axum::routing::get(move || {
                    let result = if route_reads.fetch_add(1, Ordering::SeqCst).is_multiple_of(2) {
                        empty.clone()
                    } else {
                        current.clone()
                    };
                    async move { axum::Json(result) }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let observer = ServerAgentObserver::new(
            ThinClient::new(&format!("http://{addr}"), None).unwrap(),
            None,
            Some("session-1"),
        );
        lock_state(&observer.inner, "test").access_token = Some("test-token".into());
        observer.maybe_start_notices();
        wait_for_notice(|| lock_state(&observer.inner, "test").notice_stream_ready).await;

        // The server already has the child before subscription. A lagging
        // read omits it at bootstrap, then again after it has been observed.
        let mut widget = ChatWidget::new("");
        for (read, interval, revision) in [
            (1, ACTIVE_POLL_INTERVAL, "older"),
            (2, COVERED_QUIET_POLL_INTERVAL, "current"),
            (3, ACTIVE_POLL_INTERVAL, "older"),
            (4, COVERED_QUIET_POLL_INTERVAL, "current"),
        ] {
            if interval == COVERED_QUIET_POLL_INTERVAL {
                lock_state(&observer.inner, "test").last_fetch =
                    Instant::now() - 2 * QUIET_POLL_INTERVAL;
                observer.maybe_refresh(None);
                assert!(!lock_state(&observer.inner, "test").request_in_flight);
            }
            lock_state(&observer.inner, "test").last_fetch = Instant::now() - interval;
            observer.maybe_refresh(None);
            wait_for_notice(|| !lock_state(&observer.inner, "test").request_in_flight).await;
            assert_eq!(reads.load(Ordering::SeqCst), read);
            let projection = observer.projection_after(None).unwrap();
            assert_eq!(
                projection.snapshot.as_ref().unwrap().snapshot_revision,
                revision
            );
            widget.reconcile_server_agent_projection(&projection);
            if read > 1 {
                let monitor = widget.agent_monitor_snapshot(5);
                let child = monitor
                    .iter()
                    .find(|row| row.run_id.as_deref() == Some("child-1"))
                    .unwrap();
                assert_eq!(
                    child.available_actions.is_empty(),
                    revision == "older",
                    "only a snapshot containing the child can supply its controls",
                );
            }
        }
    }

    #[test]
    fn admission_missing_from_an_older_database_read_retries_and_degrades_boundedly() {
        let observer = observer("session-1");
        let mut empty = snapshot("session-1", "old");
        empty.runs.clear();
        {
            let mut state = lock_state(&observer.inner, "test");
            state.notice_stream_ready = true;
            state.pending_admissions.push("child-1".into());
        }
        for attempt in 1..=3 {
            {
                let mut state = lock_state(&observer.inner, "test");
                state.refresh_pending = false;
            }
            apply_fetch_result(
                &observer.inner,
                0,
                "session-1",
                Ok(SnapshotFetchSuccess {
                    snapshot: empty.clone(),
                    access_token: "token".into(),
                    started_at: Instant::now(),
                }),
            );
            let state = lock_state(&observer.inner, "test");
            assert_eq!(state.projection.truth_state, ServerAgentTruthState::Stale);
            assert!(!covered_quiet_tree(&state));
            assert_eq!(state.refresh_pending, attempt < 3);
            assert_eq!(state.pending_admissions, ["child-1"]);
        }
    }

    #[test]
    fn failure_degrades_confirmed_snapshot_without_erasing_it() {
        let observer = observer("session-1");
        let initial = observer.projection_after(None).unwrap();
        assert!(observer.projection_after(Some(initial.sequence)).is_none());
        apply_fetch_result(
            &observer.inner,
            0,
            "session-1",
            Ok(SnapshotFetchSuccess {
                snapshot: snapshot("session-1", "revision-1"),
                access_token: "token".into(),
                started_at: Instant::now(),
            }),
        );
        let confirmed = observer.projection_after(Some(initial.sequence)).unwrap();
        assert!(
            observer
                .projection_after(Some(confirmed.sequence))
                .is_none()
        );
        let mut same_revision = snapshot("session-1", "revision-1");
        same_revision.observed_at = "2026-07-11T00:01:00Z".into();
        let later_started_at = confirmed.snapshot_started_at.unwrap() + Duration::from_secs(1);
        apply_fetch_result(
            &observer.inner,
            0,
            "session-1",
            Ok(SnapshotFetchSuccess {
                snapshot: same_revision,
                access_token: "token".into(),
                started_at: later_started_at,
            }),
        );
        assert!(
            observer
                .projection_after(Some(confirmed.sequence))
                .is_none()
        );
        apply_fetch_result(
            &observer.inner,
            0,
            "session-1",
            Err(SnapshotFetchError::Timeout),
        );
        let stale = observer.projection_after(Some(confirmed.sequence)).unwrap();

        assert_eq!(confirmed.truth_state, ServerAgentTruthState::Confirmed);
        assert_eq!(stale.truth_state, ServerAgentTruthState::Stale);
        assert_eq!(stale.snapshot_started_at, confirmed.snapshot_started_at);
        assert_eq!(stale.snapshot.unwrap().snapshot_revision, "revision-1");
        assert!(stale.sequence > confirmed.sequence);
    }

    #[test]
    fn repeated_stale_reads_publish_even_when_revision_is_unchanged() {
        let observer = observer("session-1");
        let first_started_at = Instant::now();
        lock_state(&observer.inner, "test").refresh_pending = true;
        apply_fetch_result(
            &observer.inner,
            0,
            "session-1",
            Ok(SnapshotFetchSuccess {
                snapshot: snapshot("session-1", "revision-1"),
                access_token: "token".into(),
                started_at: first_started_at,
            }),
        );
        let first = observer.projection_after(None).unwrap();
        assert_eq!(first.truth_state, ServerAgentTruthState::Stale);
        assert!(observer.projection_after(Some(first.sequence)).is_none());

        let second_started_at = first_started_at + Duration::from_secs(1);
        apply_fetch_result(
            &observer.inner,
            0,
            "session-1",
            Ok(SnapshotFetchSuccess {
                snapshot: snapshot("session-1", "revision-1"),
                access_token: "token".into(),
                started_at: second_started_at,
            }),
        );
        let second = observer.projection_after(Some(first.sequence)).unwrap();
        assert_eq!(second.truth_state, ServerAgentTruthState::Stale);
        assert_eq!(second.snapshot_started_at, Some(second_started_at));
    }

    #[test]
    fn explicit_refresh_bypasses_failure_backoff_without_interrupting_a_live_request() {
        let observer = observer("session-1");
        {
            let mut state = lock_state(&observer.inner, "test");
            state.consecutive_failures = 5;
            state.last_fetch = Instant::now();
            state.projection.truth_state = ServerAgentTruthState::Unavailable;
        }

        assert!(observer.request_refresh());
        let state = lock_state(&observer.inner, "test");
        assert!(
            state.last_fetch.elapsed() >= MAX_FAILURE_BACKOFF,
            "manual refresh must bypass the current retry backoff"
        );
        drop(state);

        lock_state(&observer.inner, "test").request_in_flight = true;
        assert!(
            !observer.request_refresh(),
            "a manual refresh must not start another request while one is in flight"
        );
        assert!(lock_state(&observer.inner, "test").refresh_pending);
    }

    #[test]
    fn only_executing_runs_use_the_active_poll_interval() {
        let observer = observer("session-1");
        let mut running = snapshot("session-1", "running");
        lock_state(&observer.inner, "test").projection.snapshot = Some(running.clone());
        assert_eq!(
            refresh_interval(&lock_state(&observer.inner, "test"), None),
            ACTIVE_POLL_INTERVAL
        );

        running.runs[0].parent_run_id = None;
        lock_state(&observer.inner, "test").projection.snapshot = Some(running.clone());
        assert_eq!(
            refresh_interval(&lock_state(&observer.inner, "test"), None),
            ROOT_ACTIVE_POLL_INTERVAL,
            "the root remains discoverable without duplicating its attached SSE at one query per second"
        );
        running.runs[0].parent_run_id = Some("root-1".into());

        running.runs[0].status = SessionRunLifecycleStatus::Waiting;
        lock_state(&observer.inner, "test").projection.snapshot = Some(running.clone());
        assert_eq!(
            refresh_interval(&lock_state(&observer.inner, "test"), None),
            QUIET_POLL_INTERVAL,
            "waiting work remains observable without creating a permanent one-second poll"
        );

        running.runs[0].status = SessionRunLifecycleStatus::Paused;
        lock_state(&observer.inner, "test").projection.snapshot = Some(running);
        assert_eq!(
            refresh_interval(&lock_state(&observer.inner, "test"), None),
            QUIET_POLL_INTERVAL
        );
    }

    #[tokio::test]
    async fn attached_root_uses_one_read_instead_of_three_and_still_discovers_children() {
        let mut root = snapshot("session-1", "root");
        root.runs[0].run_id = "root-1".into();
        root.runs[0].parent_run_id = None;
        root.runs[0].depth = 0;
        let mut with_child = root.clone();
        with_child
            .runs
            .push(snapshot("session-1", "child").runs.remove(0));
        let reads = Arc::new(AtomicUsize::new(0));
        let route_reads = Arc::clone(&reads);
        let app = axum::Router::new()
            .route(
                "/sessions/{session_id}/runs/admissions",
                axum::routing::get(|| async { axum::http::StatusCode::NOT_IMPLEMENTED }),
            )
            .route(
                "/sessions/{session_id}/runs",
                axum::routing::get(move || {
                    let with_child = with_child.clone();
                    route_reads.fetch_add(1, Ordering::SeqCst);
                    async move { axum::Json(with_child) }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let observer = ServerAgentObserver::new(
            ThinClient::new(&format!("http://{addr}"), None).unwrap(),
            None,
            Some("session-1"),
        );
        {
            let mut state = lock_state(&observer.inner, "test");
            state.access_token = Some("test-token".into());
            state.projection.truth_state = ServerAgentTruthState::Confirmed;
            state.projection.snapshot = Some(root);
        }
        let root = Some(("session-1", "root-1"));
        for elapsed in [ROOT_ACTIVE_POLL_INTERVAL, 2 * ROOT_ACTIVE_POLL_INTERVAL] {
            lock_state(&observer.inner, "test").last_fetch = Instant::now() - elapsed;
            observer.maybe_refresh(root);
            assert!(!lock_state(&observer.inner, "test").request_in_flight);
            assert_eq!(reads.load(Ordering::SeqCst), 0);
        }
        lock_state(&observer.inner, "test").last_fetch = Instant::now() - QUIET_POLL_INTERVAL;
        observer.maybe_refresh(root);
        assert!(lock_state(&observer.inner, "test").request_in_flight);
        wait_for_notice(|| !lock_state(&observer.inner, "test").request_in_flight).await;
        assert_eq!(reads.load(Ordering::SeqCst), 1);

        // A child that wasn't present in the attached stream remains visible
        // via the safety read and restores the live child cadence.
        let state = lock_state(&observer.inner, "test");
        assert_eq!(refresh_interval(&state, root), ACTIVE_POLL_INTERVAL);
        assert_eq!(
            state.projection.truth_state,
            ServerAgentTruthState::Confirmed
        );
        server.abort();
    }

    #[test]
    fn attached_root_requires_a_complete_matching_snapshot() {
        let observer = observer("session-1");
        let mut state = lock_state(&observer.inner, "test");
        let mut root = snapshot("session-1", "root");
        root.runs[0].run_id = "root-1".into();
        root.runs[0].parent_run_id = None;
        state.projection.snapshot = Some(root);
        state.projection.truth_state = ServerAgentTruthState::Confirmed;
        let attached = Some(("session-1", "root-1"));
        assert_eq!(refresh_interval(&state, attached), QUIET_POLL_INTERVAL);
        for uncovered in [
            None,
            Some(("session-2", "root-1")),
            Some(("session-1", "other")),
        ] {
            assert_eq!(
                refresh_interval(&state, uncovered),
                ROOT_ACTIVE_POLL_INTERVAL
            );
        }
        state.projection.snapshot.as_mut().unwrap().truncated = true;
        assert_eq!(
            refresh_interval(&state, attached),
            ROOT_ACTIVE_POLL_INTERVAL
        );
        state.projection.snapshot.as_mut().unwrap().truncated = false;

        state.projection.truth_state = ServerAgentTruthState::Stale;
        assert_eq!(
            refresh_interval(&state, attached),
            ROOT_ACTIVE_POLL_INTERVAL
        );
        state.projection.truth_state = ServerAgentTruthState::Confirmed;
        state.pending_admissions.push("peer-run".into());
        assert_eq!(
            refresh_interval(&state, attached),
            ROOT_ACTIVE_POLL_INTERVAL
        );
        state.pending_admissions.clear();

        let snapshot = state.projection.snapshot.as_mut().unwrap();
        let mut peer = snapshot.runs[0].clone();
        peer.run_id = "peer-run".into();
        snapshot.runs.push(peer);
        assert_eq!(
            refresh_interval(&state, attached),
            ROOT_ACTIVE_POLL_INTERVAL
        );
        state.refresh_pending = true;
        assert_eq!(refresh_interval(&state, attached), ACTIVE_POLL_INTERVAL);
    }

    #[test]
    fn old_session_response_cannot_cross_a_rebind_boundary() {
        let observer = observer("session-a");
        let original = observer.projection_after(None).unwrap();
        observer.rebind_session(Some("session-b"));
        let rebound = observer.projection_after(Some(original.sequence)).unwrap();
        apply_fetch_result(
            &observer.inner,
            0,
            "session-a",
            Ok(SnapshotFetchSuccess {
                snapshot: snapshot("session-a", "late-a"),
                access_token: "token".into(),
                started_at: Instant::now(),
            }),
        );
        assert!(observer.projection_after(Some(rebound.sequence)).is_none());
        let current = observer.projection_after(None).unwrap();
        assert_eq!(current.truth_state, ServerAgentTruthState::Loading);
        assert!(current.snapshot.is_none());

        // A late response for A must also be fenced after A -> B -> A.
        observer.rebind_session(Some("session-a"));
        let current = observer.projection_after(None).unwrap();
        apply_fetch_result(
            &observer.inner,
            0,
            "session-a",
            Ok(SnapshotFetchSuccess {
                snapshot: snapshot("session-a", "late-a"),
                access_token: "token".into(),
                started_at: Instant::now(),
            }),
        );
        assert!(observer.projection_after(Some(current.sequence)).is_none());
        let after_late_response = observer.projection_after(None).unwrap();
        assert_eq!(
            after_late_response.truth_state,
            ServerAgentTruthState::Loading
        );
        assert!(after_late_response.snapshot.is_none());
    }

    #[test]
    fn snapshot_validation_rejects_wrong_schema_and_session() {
        let mut wrong_schema = snapshot("session-1", "r1");
        wrong_schema.schema_version += 1;
        assert!(matches!(
            validate_snapshot(wrong_schema, "session-1"),
            Err(SnapshotFetchError::SchemaVersion { .. })
        ));
        assert!(matches!(
            validate_snapshot(snapshot("session-2", "r2"), "session-1"),
            Err(SnapshotFetchError::SessionMismatch { .. })
        ));
    }

    #[test]
    fn cached_auth_is_retained_for_network_failure_and_cleared_for_auth_rejection() {
        let observer = observer("session-1");
        apply_fetch_result(
            &observer.inner,
            0,
            "session-1",
            Ok(SnapshotFetchSuccess {
                snapshot: snapshot("session-1", "revision-1"),
                access_token: "cached-token".into(),
                started_at: Instant::now(),
            }),
        );
        apply_fetch_result(
            &observer.inner,
            0,
            "session-1",
            Err(SnapshotFetchError::Timeout),
        );
        assert_eq!(
            lock_state(&observer.inner, "test").access_token.as_deref(),
            Some("cached-token")
        );

        apply_fetch_result(
            &observer.inner,
            0,
            "session-1",
            Err(SnapshotFetchError::Transport(ThinClientError::Api {
                status: reqwest::StatusCode::UNAUTHORIZED,
                body: "expired".into(),
            })),
        );
        assert!(lock_state(&observer.inner, "test").access_token.is_none());
    }

    #[test]
    fn missing_async_runtime_releases_in_flight_latch() {
        let observer = observer("session-1");
        lock_state(&observer.inner, "test").request_in_flight = true;

        observer.spawn_fetch(0, "session-1".into(), async move {
            unreachable!("fetch must not be scheduled without a runtime")
        });

        let state = lock_state(&observer.inner, "test");
        assert!(!state.request_in_flight);
        assert_eq!(
            state.projection.truth_state,
            ServerAgentTruthState::Unavailable
        );
        assert_eq!(state.consecutive_failures, 1);
    }

    #[tokio::test]
    async fn panicked_fetch_releases_in_flight_latch_and_degrades_truth() {
        let observer = observer("session-1");
        lock_state(&observer.inner, "test").request_in_flight = true;
        observer.spawn_fetch(0, "session-1".into(), async move {
            panic!("injected observer panic")
        });

        tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                let request_in_flight = { lock_state(&observer.inner, "test").request_in_flight };
                if !request_in_flight {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("panicked request must release its in-flight latch");

        let state = lock_state(&observer.inner, "test");
        assert_eq!(
            state.projection.truth_state,
            ServerAgentTruthState::Unavailable
        );
        assert_eq!(state.consecutive_failures, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refresh_during_inflight_read_fetches_the_newer_snapshot_once() {
        use crate::tui::agent_run_projection::{
            AgentControlTarget, AgentProjectionConfidence, AgentRunState, AgentRunStatus,
            AgentTranscriptTarget,
        };
        use crate::tui::app_event::TuiAppEvent;
        use crate::tui::chat_widget::{ChatWidget, TurnContext, translate};
        use astra_thin_client::SessionRunAction::{Cancel, Pause, Resume};
        use astra_turn_core::agent_live_event::{
            AgentLiveEvent, AgentLiveEventKind, AgentLiveGap, AgentLiveSignal,
        };
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::sync::Notify;

        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let mut before_resume = snapshot("session-1", "paused");
        let paused = &mut before_resume.runs[0];
        paused.run_id = "resumed-child".into();
        paused.status = SessionRunLifecycleStatus::Paused;
        paused.available_actions = vec![Resume, Cancel];
        let mut running = paused.clone();
        running.status = SessionRunLifecycleStatus::Running;
        running.available_actions = vec![Pause, Cancel];
        let app = axum::Router::new().route(
            "/sessions/{session_id}/runs",
            axum::routing::get({
                let (entered, release, calls) = (entered.clone(), release.clone(), calls.clone());
                move || {
                    let (entered, release, calls) =
                        (entered.clone(), release.clone(), calls.clone());
                    let running = running.clone();
                    async move {
                        let call = calls.fetch_add(1, Ordering::SeqCst);
                        let mut tree = snapshot(
                            "session-1",
                            if call == 0 { "before-gap" } else { "after-gap" },
                        );
                        let mut sibling = tree.runs[0].clone();
                        sibling.run_id = "sibling".into();
                        sibling.agent_id = Some("sibling-agent".into());
                        if call > 0 {
                            sibling.status = SessionRunLifecycleStatus::Completed;
                            sibling.run_event_high_watermark = 2;
                            sibling.available_actions.clear();
                        }
                        tree.runs.push(sibling);
                        tree.runs.push(running);
                        if call < 2 {
                            entered.notify_one();
                            release.notified().await;
                        }
                        axum::Json(tree)
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let observer = ServerAgentObserver::new(
            ThinClient::new(&format!("http://{addr}"), None).unwrap(),
            None,
            Some("session-1"),
        );
        let deliver_gap = |widget: &mut ChatWidget, run_id: &str, kind: AgentLiveEventKind| {
            let events = [
                TuiAppEvent::AgentLive(AgentLiveEvent {
                    agent_id: "reviewer".into(),
                    run_id: run_id.into(),
                    kind,
                }),
                TuiAppEvent::AgentLiveGap(AgentLiveGap {
                    agent_id: "reviewer".into(),
                    run_id: run_id.into(),
                    dropped_event_count: 2,
                }),
            ];
            for event in events {
                widget.handle_event(translate(event, TurnContext::default()).unwrap());
            }
            assert!(
                !observer.notify_refresh(),
                "the in-flight read must continue"
            );
        };
        let assert_child_recovered = |widget: &ChatWidget| {
            let monitor = widget.agent_monitor_snapshot(5);
            let child = monitor
                .iter()
                .find(|row| row.run_id.as_deref() == Some("child-1"))
                .unwrap();
            assert_eq!(
                child.transcript_target,
                Some(AgentTranscriptTarget::DurableServer)
            );
            assert_eq!(child.parent_run_id.as_deref(), Some("root-1"));
            assert_eq!(child.state.status, AgentRunStatus::Waiting);
            assert_eq!(
                child.attention_summary.as_deref(),
                Some("Waiting · executor_offline")
            );
            assert!(child.control_target.is_none());
            assert!(child.available_actions.is_empty());
        };
        let mut widget = ChatWidget::new("");
        widget.reconcile_server_agent_projection(&ServerAgentProjection {
            truth_state: ServerAgentTruthState::Confirmed,
            snapshot: Some(before_resume),
            snapshot_started_at: Some(Instant::now()),
            ..ServerAgentProjection::unbound()
        });
        assert_eq!(
            widget.agent_monitor_snapshot(5)[0].available_actions,
            [Resume, Cancel]
        );
        let resumed_target = AgentControlTarget::DurableRun {
            run_id: "resumed-child".into(),
        };
        lock_state(&observer.inner, "test").access_token = Some("test-token".into());
        let mut applied_sequence = None;
        for (call, revision, sibling_status) in [
            (1, "before-gap", AgentRunStatus::Running),
            (2, "after-gap", AgentRunStatus::Completed),
        ] {
            lock_state(&observer.inner, "test").last_fetch = Instant::now() - QUIET_POLL_INTERVAL;
            observer.maybe_refresh(None);
            tokio::time::timeout(Duration::from_secs(5), entered.notified())
                .await
                .unwrap();
            deliver_gap(
                &mut widget,
                "child-1",
                AgentLiveEventKind::Signal(AgentLiveSignal::ExecutionWaiting {
                    reason: "executor_offline".into(),
                }),
            );
            deliver_gap(
                &mut widget,
                "resumed-child",
                AgentLiveEventKind::OutputDelta {
                    model_item_id: None,
                    text: "Resumed work".into(),
                },
            );
            release.notify_one();
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let in_flight = { lock_state(&observer.inner, "test").request_in_flight };
                    if !in_flight {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("the held read must complete");
            let projection = observer.projection_after(applied_sequence).unwrap();
            applied_sequence = Some(projection.sequence);
            assert_eq!(projection.truth_state, ServerAgentTruthState::Stale);
            assert_eq!(
                projection.snapshot.as_ref().unwrap().snapshot_revision,
                revision
            );
            widget.reconcile_server_agent_projection(&projection);
            assert_child_recovered(&widget);
            let monitor = widget.agent_monitor_snapshot(5);
            let resumed = monitor
                .iter()
                .find(|row| row.run_id.as_deref() == Some("resumed-child"))
                .unwrap();
            assert_eq!(
                resumed.state,
                AgentRunState::observed(AgentRunStatus::Running)
            );
            assert!(
                resumed.available_actions.is_empty(),
                "stale Resume must be hidden"
            );
            assert!(
                widget
                    .begin_agent_control("resumed-child", &resumed_target, Resume)
                    .is_none()
            );
            let sibling = widget.agent_run_state("sibling").unwrap();
            assert_eq!(sibling.status, sibling_status);
            if sibling_status == AgentRunStatus::Running {
                assert_eq!(sibling.confidence, AgentProjectionConfidence::Stale);
            }
            assert_eq!(monitor.server_truth_state, ServerAgentTruthState::Stale);

            // Check coalescing without relying on the process being scheduled
            // within the active poll interval.
            lock_state(&observer.inner, "test").last_fetch =
                Instant::now() + Duration::from_secs(60);
            observer.maybe_refresh(None);
            let state = lock_state(&observer.inner, "test");
            assert!(state.refresh_pending);
            assert!(!state.request_in_flight);
            assert_eq!(refresh_interval(&state, None), ACTIVE_POLL_INTERVAL);
            assert_eq!(calls.load(Ordering::SeqCst), call);
        }

        // A new burst just after completion is also coalesced and rate limited.
        assert!(observer.notify_refresh());
        assert!(observer.notify_refresh());
        assert_eq!(
            observer.projection_after(None).unwrap().truth_state,
            ServerAgentTruthState::Stale
        );
        observer.maybe_refresh(None);
        {
            let mut state = lock_state(&observer.inner, "test");
            assert!(state.refresh_pending);
            assert!(!state.request_in_flight);
            assert_eq!(refresh_interval(&state, None), ACTIVE_POLL_INTERVAL);
            state.last_fetch = Instant::now() - ACTIVE_POLL_INTERVAL;
        }
        observer.maybe_refresh(None);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let done = !lock_state(&observer.inner, "test").request_in_flight;
                if done && calls.load(Ordering::SeqCst) == 3 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the post-completion hint must be observed");
        let recovered = observer.projection_after(applied_sequence).unwrap();
        assert_eq!(recovered.truth_state, ServerAgentTruthState::Confirmed);
        widget.reconcile_server_agent_projection(&recovered);
        let monitor = widget.agent_monitor_snapshot(5);
        let child = monitor
            .iter()
            .find(|row| row.run_id.as_deref() == Some("child-1"))
            .unwrap();
        assert_eq!(child.state.confidence, AgentProjectionConfidence::Confirmed);
        assert_eq!(child.available_actions, [Cancel]);
        let resumed = monitor
            .iter()
            .find(|row| row.run_id.as_deref() == Some("resumed-child"))
            .unwrap();
        assert_eq!(resumed.available_actions, [Pause, Cancel]);
        assert!(
            widget
                .begin_agent_control("resumed-child", &resumed_target, Pause)
                .is_some()
        );
        lock_state(&observer.inner, "test").last_fetch = Instant::now() + Duration::from_secs(60);
        observer.maybe_refresh(None);
        assert!(!lock_state(&observer.inner, "test").request_in_flight);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        server.abort();
    }

    #[test]
    fn queued_hint_respects_failure_backoff_and_does_not_cross_rebind() {
        let observer = observer("session-a");
        lock_state(&observer.inner, "test").request_in_flight = true;
        assert!(!observer.notify_refresh());
        apply_fetch_result(
            &observer.inner,
            0,
            "session-a",
            Err(SnapshotFetchError::Timeout),
        );
        observer.maybe_refresh(None);
        {
            let state = lock_state(&observer.inner, "test");
            assert!(state.refresh_pending, "the hint must survive a failed read");
            assert!(!state.request_in_flight, "the retry must wait for backoff");
        }
        observer.rebind_session(Some("session-b"));
        assert!(!lock_state(&observer.inner, "test").refresh_pending);
    }

    #[tokio::test]
    async fn live_hints_respect_failure_backoff_and_retry_once() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/sessions/session-1/runs"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/sessions/session-1/runs"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(snapshot("session-1", "recovered")),
            )
            .expect(1)
            .mount(&server)
            .await;
        let observer = ServerAgentObserver::new(
            ThinClient::new(&server.uri(), None).unwrap(),
            None,
            Some("session-1"),
        );
        lock_state(&observer.inner, "test").access_token = Some("test-token".into());
        observer.maybe_refresh(None);
        tokio::time::timeout(Duration::from_secs(5), async {
            while lock_state(&observer.inner, "test").consecutive_failures == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the first HTTP request must fail");
        {
            let mut state = lock_state(&observer.inner, "test");
            // Model a prolonged outage and leave a wide scheduling margin.
            state.consecutive_failures = 6;
            state.last_fetch = Instant::now() - Duration::from_secs(10);
        }
        assert!(observer.notify_refresh());
        assert!(observer.notify_refresh());
        observer.maybe_refresh(None);
        {
            let mut state = lock_state(&observer.inner, "test");
            assert!(state.refresh_pending);
            assert!(
                !state.request_in_flight,
                "hints must not bypass failure backoff"
            );
            assert_eq!(refresh_interval(&state, None), MAX_FAILURE_BACKOFF);
            state.last_fetch = Instant::now() - MAX_FAILURE_BACKOFF;
        }
        observer.maybe_refresh(None);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let recovered = {
                    let state = lock_state(&observer.inner, "test");
                    !state.request_in_flight
                        && state
                            .projection
                            .snapshot
                            .as_ref()
                            .is_some_and(|tree| tree.snapshot_revision == "recovered")
                };
                if recovered {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the coalesced hint must be retried after backoff");
        lock_state(&observer.inner, "test").last_fetch = Instant::now() + Duration::from_secs(60);
        observer.maybe_refresh(None);
        assert!(!lock_state(&observer.inner, "test").request_in_flight);
        server.verify().await;
    }

    #[tokio::test]
    async fn dropping_observer_aborts_pending_fetch() {
        struct DropSignal(Arc<std::sync::atomic::AtomicBool>);

        impl Drop for DropSignal {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::Release);
            }
        }

        let observer = observer("session-1");
        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fetch_started = Arc::clone(&started);
        let fetch_dropped = Arc::clone(&dropped);
        observer.spawn_fetch(0, "session-1".into(), async move {
            fetch_started.store(true, std::sync::atomic::Ordering::Release);
            let _drop_signal = DropSignal(fetch_dropped);
            std::future::pending().await
        });
        tokio::time::timeout(Duration::from_millis(500), async {
            while !started.load(std::sync::atomic::Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("pending fetch should start");

        drop(observer);
        tokio::time::timeout(Duration::from_millis(500), async {
            while !dropped.load(std::sync::atomic::Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("observer drop should abort and release pending fetch");
    }
}

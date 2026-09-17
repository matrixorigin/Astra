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
use futures_util::FutureExt;

const ACTIVE_POLL_INTERVAL: Duration = Duration::from_secs(1);
const ROOT_ACTIVE_POLL_INTERVAL: Duration = Duration::from_secs(5);
const QUIET_POLL_INTERVAL: Duration = Duration::from_secs(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
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
}

impl ServerAgentProjection {
    fn unbound() -> Self {
        Self {
            sequence: 0,
            truth_state: ServerAgentTruthState::Unbound,
            snapshot: None,
        }
    }
}

pub(crate) struct ServerAgentObserver {
    inner: Arc<ObserverInner>,
    fetch_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
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
    last_fetch: Instant,
    consecutive_failures: u32,
    access_token: Option<String>,
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
                    last_fetch: Instant::now()
                        .checked_sub(QUIET_POLL_INTERVAL)
                        .unwrap_or_else(Instant::now),
                    consecutive_failures: 0,
                    access_token: None,
                }),
            }),
            fetch_task: Mutex::new(None),
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
            state.request_in_flight = false;
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
        }
        self.abort_fetch();
    }

    pub(crate) fn projection(&self) -> ServerAgentProjection {
        lock_state(&self.inner, "projection").projection.clone()
    }

    /// Make the next tick fetch the durable run tree immediately, without
    /// cancelling a request that is already in flight. This is an explicit
    /// user recovery action for a stale/unavailable workbench surface; the
    /// actual request still runs through [`Self::maybe_refresh`] off the UI
    /// task.
    pub(crate) fn request_refresh(&self) -> bool {
        let mut state = lock_state(&self.inner, "request_refresh");
        if state.session_id.is_empty() || state.request_in_flight {
            return false;
        }
        state.last_fetch = Instant::now()
            .checked_sub(MAX_FAILURE_BACKOFF)
            .unwrap_or_else(Instant::now);
        true
    }

    pub(crate) fn maybe_refresh(&self) {
        let (session_id, binding_generation, access_token) = {
            let mut state = lock_state(&self.inner, "maybe_refresh");
            if state.session_id.is_empty() || state.request_in_flight {
                return;
            }
            let interval = refresh_interval(&state);
            if state.last_fetch.elapsed() < interval {
                return;
            }
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
        let mut current = match self.fetch_task.lock() {
            Ok(current) => current,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(task) = current.take() {
            task.abort();
        }
    }
}

impl Drop for ServerAgentObserver {
    fn drop(&mut self) {
        let task = match self.fetch_task.get_mut() {
            Ok(task) => task,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(task) = task.take() {
            task.abort();
        }
    }
}

fn normalized_session_id(session_id: Option<&str>) -> String {
    session_id.map(str::trim).unwrap_or_default().to_string()
}

fn refresh_interval(state: &ObserverState) -> Duration {
    if state.consecutive_failures > 0 {
        let exponent = state.consecutive_failures.saturating_sub(1).min(5);
        return ACTIVE_POLL_INTERVAL
            .saturating_mul(1_u32 << exponent)
            .min(MAX_FAILURE_BACKOFF);
    }
    if state.projection.snapshot.as_ref().is_some_and(|snapshot| {
        snapshot.runs.iter().any(|run| {
            run.parent_run_id.is_some()
                && run.status == astra_thin_client::SessionRunLifecycleStatus::Running
        })
    }) {
        ACTIVE_POLL_INTERVAL
    } else if state.projection.snapshot.as_ref().is_some_and(|snapshot| {
        snapshot
            .runs
            .iter()
            .any(|run| run.status == astra_thin_client::SessionRunLifecycleStatus::Running)
    }) {
        // The chat SSE is the attached root's primary progress authority. A
        // slower safety poll still discovers a run started by another client
        // without multiplying one database query per connected user-second.
        ROOT_ACTIVE_POLL_INTERVAL
    } else {
        QUIET_POLL_INTERVAL
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
}

async fn fetch_snapshot(
    api: &ThinClient,
    profile: Option<&str>,
    session_id: &str,
    access_token: Option<String>,
) -> Result<SnapshotFetchSuccess, SnapshotFetchError> {
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
            let changed = state.projection.truth_state != ServerAgentTruthState::Confirmed
                || state
                    .projection
                    .snapshot
                    .as_ref()
                    .map(|current| current.snapshot_revision.as_str())
                    != Some(snapshot.snapshot_revision.as_str());
            state.consecutive_failures = 0;
            state.access_token = Some(success.access_token);
            state.projection.truth_state = ServerAgentTruthState::Confirmed;
            state.projection.snapshot = Some(snapshot);
            if changed {
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

    #[test]
    fn failure_degrades_confirmed_snapshot_without_erasing_it() {
        let observer = observer("session-1");
        apply_fetch_result(
            &observer.inner,
            0,
            "session-1",
            Ok(SnapshotFetchSuccess {
                snapshot: snapshot("session-1", "revision-1"),
                access_token: "token".into(),
            }),
        );
        let confirmed = observer.projection();
        apply_fetch_result(
            &observer.inner,
            0,
            "session-1",
            Err(SnapshotFetchError::Timeout),
        );
        let stale = observer.projection();

        assert_eq!(confirmed.truth_state, ServerAgentTruthState::Confirmed);
        assert_eq!(stale.truth_state, ServerAgentTruthState::Stale);
        assert_eq!(stale.snapshot.unwrap().snapshot_revision, "revision-1");
        assert!(stale.sequence > confirmed.sequence);
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
            "a manual refresh must not cancel or duplicate an in-flight request"
        );
    }

    #[test]
    fn only_executing_runs_use_the_active_poll_interval() {
        let observer = observer("session-1");
        let mut running = snapshot("session-1", "running");
        lock_state(&observer.inner, "test").projection.snapshot = Some(running.clone());
        assert_eq!(
            refresh_interval(&lock_state(&observer.inner, "test")),
            ACTIVE_POLL_INTERVAL
        );

        running.runs[0].parent_run_id = None;
        lock_state(&observer.inner, "test").projection.snapshot = Some(running.clone());
        assert_eq!(
            refresh_interval(&lock_state(&observer.inner, "test")),
            ROOT_ACTIVE_POLL_INTERVAL,
            "the root remains discoverable without duplicating its attached SSE at one query per second"
        );
        running.runs[0].parent_run_id = Some("root-1".into());

        running.runs[0].status = SessionRunLifecycleStatus::Waiting;
        lock_state(&observer.inner, "test").projection.snapshot = Some(running.clone());
        assert_eq!(
            refresh_interval(&lock_state(&observer.inner, "test")),
            QUIET_POLL_INTERVAL,
            "waiting work remains observable without creating a permanent one-second poll"
        );

        running.runs[0].status = SessionRunLifecycleStatus::Paused;
        lock_state(&observer.inner, "test").projection.snapshot = Some(running);
        assert_eq!(
            refresh_interval(&lock_state(&observer.inner, "test")),
            QUIET_POLL_INTERVAL
        );
    }

    #[test]
    fn old_session_response_cannot_cross_a_rebind_boundary() {
        let observer = observer("session-a");
        observer.rebind_session(Some("session-b"));
        let rebound = observer.projection();
        apply_fetch_result(
            &observer.inner,
            0,
            "session-a",
            Ok(SnapshotFetchSuccess {
                snapshot: snapshot("session-a", "late-a"),
                access_token: "token".into(),
            }),
        );
        let after_late_response = observer.projection();

        assert_eq!(after_late_response.sequence, rebound.sequence);
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

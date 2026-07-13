//! Read-only TUI projection of the durable plan repository.
//!
//! A plan is not copied into `session_todos`. Instead this observer reads the
//! most relevant durable plan for the selected session, converts its steps into
//! stable display rows, and hands those rows to [`TaskBoardObserver`]. The
//! task board therefore remains a projection over facts owned by their
//! respective systems rather than a second writable plan state.

use std::collections::BTreeMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use astra_services::task_orchestrator::{TaskPlan, TaskStatus};
use astra_thin_client::{ThinClient, ThinClientError};
use astra_tools::task_mgmt::{OpenTaskSummary, SessionTask, SessionTaskStatusKind};
use futures_util::FutureExt;
use serde::Deserialize;
use serde_json::{Map, Value};

use super::task_board_observer::ViewMode;

const ACTIVE_POLL_INTERVAL: Duration = Duration::from_secs(1);
const QUIET_POLL_INTERVAL: Duration = Duration::from_secs(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_FAILURE_BACKOFF: Duration = Duration::from_secs(30);
const SESSION_PLAN_LIST_LIMIT: usize = 20;
const ALL_SESSION_PLAN_LIST_LIMIT: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlanTaskTruthState {
    Unbound,
    Loading,
    Confirmed,
    Stale,
    Unavailable,
}

impl Default for PlanTaskTruthState {
    fn default() -> Self {
        Self::Unbound
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct PlanTaskProjection {
    pub sequence: u64,
    pub truth_state: PlanTaskTruthState,
    pub tasks: Vec<SessionTask>,
    pub multi_session: Vec<(String, Vec<OpenTaskSummary>)>,
}

impl PlanTaskProjection {
    fn unbound() -> Self {
        Self::default()
    }
}

/// One bounded, cancellable observer for the selected session's active plan.
/// It intentionally owns no store and exposes no mutation API.
pub(crate) struct PlanTaskObserver {
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
    /// Separates requests for the single-session and all-sessions payload
    /// shapes. Cancelling a task is best effort; a late completion must never
    /// overwrite the newly selected view with the previous shape.
    view_generation: u64,
    view_mode: ViewMode,
    projection: PlanTaskProjection,
    request_in_flight: bool,
    last_fetch: Instant,
    consecutive_failures: u32,
    access_token: Option<String>,
}

impl PlanTaskObserver {
    pub(crate) fn new(api: ThinClient, profile: Option<&str>, session_id: Option<&str>) -> Self {
        let session_id = normalized_session_id(session_id);
        let truth_state = if session_id.is_empty() {
            PlanTaskTruthState::Unbound
        } else {
            PlanTaskTruthState::Loading
        };
        Self {
            inner: Arc::new(ObserverInner {
                api,
                profile: profile.map(str::to_owned),
                state: Mutex::new(ObserverState {
                    session_id,
                    binding_generation: 0,
                    view_generation: 0,
                    view_mode: ViewMode::SingleSession,
                    projection: PlanTaskProjection {
                        truth_state,
                        ..PlanTaskProjection::unbound()
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
            state.access_token = None;
            state.last_fetch = Instant::now()
                .checked_sub(QUIET_POLL_INTERVAL)
                .unwrap_or_else(Instant::now);
            state.projection.sequence = state.projection.sequence.wrapping_add(1);
            state.projection.truth_state = if state.session_id.is_empty() {
                PlanTaskTruthState::Unbound
            } else {
                PlanTaskTruthState::Loading
            };
            state.projection.tasks.clear();
            state.projection.multi_session.clear();
        }
        self.abort_fetch();
    }

    pub(crate) fn projection(&self) -> PlanTaskProjection {
        lock_state(&self.inner, "projection").projection.clone()
    }

    pub(crate) fn set_view_mode(&self, view_mode: ViewMode) {
        {
            let mut state = lock_state(&self.inner, "set_view_mode");
            if state.view_mode == view_mode {
                return;
            }
            state.view_mode = view_mode;
            state.view_generation = state.view_generation.wrapping_add(1);
            state.request_in_flight = false;
            state.last_fetch = Instant::now()
                .checked_sub(QUIET_POLL_INTERVAL)
                .unwrap_or_else(Instant::now);
            state.projection.truth_state = PlanTaskTruthState::Loading;
            state.projection.sequence = state.projection.sequence.wrapping_add(1);
        }
        self.abort_fetch();
    }

    /// Bypass automatic retry backoff once at the user's request. The
    /// observer still owns the network operation and never interrupts an
    /// existing fetch, so a refresh key cannot create competing reads.
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

    /// Schedule a single fetch when due. Key handling and drawing never await
    /// network/auth work; a panic is turned into truthful unavailable/stale
    /// state rather than leaving `request_in_flight` latched forever.
    pub(crate) fn maybe_refresh(&self) {
        let (session_id, binding_generation, view_generation, access_token, view_mode) = {
            let mut state = lock_state(&self.inner, "maybe_refresh");
            if state.session_id.is_empty()
                || state.request_in_flight
                || state.last_fetch.elapsed() < refresh_interval(&state)
            {
                return;
            }
            state.request_in_flight = true;
            (
                state.session_id.clone(),
                state.binding_generation,
                state.view_generation,
                state.access_token.clone(),
                state.view_mode,
            )
        };

        let inner = Arc::clone(&self.inner);
        let request_session_id = session_id.clone();
        self.spawn_fetch(
            binding_generation,
            view_generation,
            view_mode,
            session_id,
            async move {
                match view_mode {
                    ViewMode::SingleSession => {
                        fetch_single_plan_projection(
                            &inner.api,
                            inner.profile.as_deref(),
                            &request_session_id,
                            access_token,
                        )
                        .await
                    }
                    ViewMode::AllSessions => {
                        fetch_all_plan_summaries(&inner.api, inner.profile.as_deref(), access_token)
                            .await
                    }
                }
            },
        );
    }

    fn spawn_fetch<F>(
        &self,
        binding_generation: u64,
        view_generation: u64,
        view_mode: ViewMode,
        session_id: String,
        fetch: F,
    ) where
        F: Future<Output = Result<PlanTaskFetchSuccess, PlanTaskFetchError>> + Send + 'static,
    {
        let runtime = match tokio::runtime::Handle::try_current() {
            Ok(runtime) => runtime,
            Err(_) => {
                apply_fetch_result(
                    &self.inner,
                    binding_generation,
                    view_generation,
                    view_mode,
                    &session_id,
                    Err(PlanTaskFetchError::ObserverRuntimeUnavailable),
                );
                return;
            }
        };
        let inner = Arc::clone(&self.inner);
        let handle = runtime.spawn(async move {
            let result = match AssertUnwindSafe(fetch).catch_unwind().await {
                Ok(result) => result,
                Err(_) => Err(PlanTaskFetchError::ObserverTaskPanicked),
            };
            apply_fetch_result(
                &inner,
                binding_generation,
                view_generation,
                view_mode,
                &session_id,
                result,
            );
        });
        let mut current = match self.fetch_task.lock() {
            Ok(current) => current,
            Err(poisoned) => {
                tracing::warn!("plan task observer fetch handle poisoned; recovering");
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

impl Drop for PlanTaskObserver {
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
    session_id.map(str::trim).unwrap_or_default().to_owned()
}

fn refresh_interval(state: &ObserverState) -> Duration {
    if state.consecutive_failures > 0 {
        let exponent = state.consecutive_failures.saturating_sub(1).min(5);
        return ACTIVE_POLL_INTERVAL
            .saturating_mul(1_u32 << exponent)
            .min(MAX_FAILURE_BACKOFF);
    }
    if state
        .projection
        .tasks
        .iter()
        .any(|task| task.status.is_open_work())
        || state
            .projection
            .multi_session
            .iter()
            .flat_map(|(_, tasks)| tasks)
            .any(|task| task.status.is_open_work())
    {
        ACTIVE_POLL_INTERVAL
    } else {
        QUIET_POLL_INTERVAL
    }
}

#[derive(Debug)]
enum PlanTaskFetchError {
    AuthenticationUnavailable,
    Timeout,
    ObserverRuntimeUnavailable,
    ObserverTaskPanicked,
    Transport(ThinClientError),
    InvalidResponse(String),
}

impl PlanTaskFetchError {
    fn invalidates_access_token(&self) -> bool {
        matches!(
            self,
            Self::Transport(ThinClientError::Api { status, .. })
                if *status == reqwest::StatusCode::UNAUTHORIZED
                    || *status == reqwest::StatusCode::FORBIDDEN
        )
    }
}

struct PlanTaskFetchSuccess {
    payload: PlanTaskFetchPayload,
    access_token: String,
}

enum PlanTaskFetchPayload {
    Single(Vec<SessionTask>),
    All(Vec<(String, Vec<OpenTaskSummary>)>),
}

#[derive(Deserialize)]
struct PlanListResponse {
    plans: Vec<PlanSummary>,
}

#[derive(Deserialize)]
struct PlanSummary {
    plan_id: String,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    goal: String,
    #[serde(default)]
    version: u64,
    #[serde(default)]
    status: String,
}

#[derive(Deserialize)]
struct PlanResponse {
    plan_id: String,
    version: u64,
    plan: Option<TaskPlan>,
}

async fn fetch_single_plan_projection(
    api: &ThinClient,
    profile: Option<&str>,
    session_id: &str,
    access_token: Option<String>,
) -> Result<PlanTaskFetchSuccess, PlanTaskFetchError> {
    let token = match access_token {
        Some(token) => token,
        None => crate::cli::session::session_runtime::fresh_access_token(api, profile)
            .await
            .ok_or(PlanTaskFetchError::AuthenticationUnavailable)?,
    };
    let session_id = session_id.to_owned();
    let response = tokio::time::timeout(REQUEST_TIMEOUT, async {
        let list = api
            .get_plans_query_json(
                &token,
                &[
                    ("session_id", session_id.clone()),
                    ("limit", SESSION_PLAN_LIST_LIMIT.to_string()),
                ],
            )
            .await
            .map_err(PlanTaskFetchError::Transport)?;
        let list = serde_json::from_value::<PlanListResponse>(list)
            .map_err(|error| PlanTaskFetchError::InvalidResponse(error.to_string()))?;
        let Some(summary) = select_display_plan(list.plans) else {
            return Ok(Vec::new());
        };
        let detail = api
            .get_plan_json(&token, &summary.plan_id)
            .await
            .map_err(PlanTaskFetchError::Transport)?;
        let detail = serde_json::from_value::<PlanResponse>(detail)
            .map_err(|error| PlanTaskFetchError::InvalidResponse(error.to_string()))?;
        if detail.plan_id != summary.plan_id {
            return Err(PlanTaskFetchError::InvalidResponse(
                "plan detail did not match the requested plan".to_string(),
            ));
        }
        Ok(project_plan_steps(detail))
    })
    .await
    .map_err(|_| PlanTaskFetchError::Timeout)??;
    Ok(PlanTaskFetchSuccess {
        payload: PlanTaskFetchPayload::Single(response),
        access_token: token,
    })
}

async fn fetch_all_plan_summaries(
    api: &ThinClient,
    profile: Option<&str>,
    access_token: Option<String>,
) -> Result<PlanTaskFetchSuccess, PlanTaskFetchError> {
    let token = match access_token {
        Some(token) => token,
        None => crate::cli::session::session_runtime::fresh_access_token(api, profile)
            .await
            .ok_or(PlanTaskFetchError::AuthenticationUnavailable)?,
    };
    let response = tokio::time::timeout(REQUEST_TIMEOUT, async {
        let list = api
            .get_plans_query_json(
                &token,
                &[("limit", ALL_SESSION_PLAN_LIST_LIMIT.to_string())],
            )
            .await
            .map_err(PlanTaskFetchError::Transport)?;
        let list = serde_json::from_value::<PlanListResponse>(list)
            .map_err(|error| PlanTaskFetchError::InvalidResponse(error.to_string()))?;
        Ok(project_open_plan_summaries(list.plans))
    })
    .await
    .map_err(|_| PlanTaskFetchError::Timeout)??;
    Ok(PlanTaskFetchSuccess {
        payload: PlanTaskFetchPayload::All(response),
        access_token: token,
    })
}

/// `active_plan_id` is an authoring/write-guard overlay and is deliberately
/// cleared after approval. A work surface instead chooses from all durable
/// plans associated with the session, preferring live execution over drafts
/// and terminal history. The server orders equal-priority rows by recency.
fn select_display_plan(plans: Vec<PlanSummary>) -> Option<PlanSummary> {
    plans
        .into_iter()
        .min_by_key(|plan| match plan.status.as_str() {
            "executing" => 0_u8,
            "refining" => 1,
            "planning" => 2,
            "completed" => 3,
            _ => 4,
        })
}

fn project_open_plan_summaries(plans: Vec<PlanSummary>) -> Vec<(String, Vec<OpenTaskSummary>)> {
    let mut per_session = BTreeMap::<String, Vec<OpenTaskSummary>>::new();
    for plan in plans {
        let Some(session_id) = plan
            .session_id
            .filter(|session_id| !session_id.trim().is_empty())
        else {
            continue;
        };
        let status = session_status_from_phase(&plan.status);
        if !status.is_open_work() {
            continue;
        }
        per_session
            .entry(session_id)
            .or_default()
            .push(OpenTaskSummary {
                id: format!("plan:{}", plan.plan_id),
                title: format!("Plan · {}", plan.goal),
                status,
                updated_at: format!("plan-v{}", plan.version),
            });
    }
    per_session.into_iter().collect()
}

fn session_status_from_phase(phase: &str) -> SessionTaskStatusKind {
    match phase {
        "executing" => SessionTaskStatusKind::InProgress,
        "planning" | "refining" => SessionTaskStatusKind::Pending,
        "completed" => SessionTaskStatusKind::Completed,
        _ => SessionTaskStatusKind::Other,
    }
}

fn project_plan_steps(plan: PlanResponse) -> Vec<SessionTask> {
    let Some(task_plan) = plan.plan else {
        return Vec::new();
    };
    task_plan
        .subtasks
        .into_iter()
        .map(|step| {
            let row_id = plan_step_row_id(&plan.plan_id, &step.id);
            let mut metadata = Map::new();
            metadata.insert("source".to_string(), Value::String("plan".to_string()));
            metadata.insert("plan_id".to_string(), Value::String(plan.plan_id.clone()));
            metadata.insert("plan_version".to_string(), Value::from(plan.version));
            metadata.insert("step_id".to_string(), Value::String(step.id.clone()));
            SessionTask {
                id: row_id,
                title: step.title,
                description: step.description,
                status: session_status(step.status),
                subtasks: Vec::new(),
                // The plan API's revision is the only freshness field exposed
                // by the canonical response. It is deliberately carried as a
                // revision marker, not fabricated wall-clock time.
                created_at: String::new(),
                updated_at: format!("plan-v{}", plan.version),
                active_form: None,
                owner: None,
                metadata: Some(metadata),
                blocks: Vec::new(),
                blocked_by: step
                    .depends_on
                    .iter()
                    .map(|dependency| plan_step_row_id(&plan.plan_id, dependency))
                    .collect(),
                archived_at: None,
            }
        })
        .collect()
}

fn plan_step_row_id(plan_id: &str, step_id: &str) -> String {
    format!("plan:{plan_id}:{step_id}")
}

fn session_status(status: TaskStatus) -> SessionTaskStatusKind {
    match status {
        TaskStatus::Pending => SessionTaskStatusKind::Pending,
        TaskStatus::InProgress => SessionTaskStatusKind::InProgress,
        TaskStatus::Paused => SessionTaskStatusKind::Paused,
        TaskStatus::Completed => SessionTaskStatusKind::Completed,
        TaskStatus::Failed => SessionTaskStatusKind::Failed,
        TaskStatus::Cancelled => SessionTaskStatusKind::Cancelled,
    }
}

fn apply_fetch_result(
    inner: &ObserverInner,
    binding_generation: u64,
    view_generation: u64,
    view_mode: ViewMode,
    session_id: &str,
    result: Result<PlanTaskFetchSuccess, PlanTaskFetchError>,
) {
    let mut state = lock_state(inner, "apply_fetch_result");
    if state.binding_generation != binding_generation
        || state.view_generation != view_generation
        || state.view_mode != view_mode
        || state.session_id != session_id
    {
        return;
    }
    state.request_in_flight = false;
    state.last_fetch = Instant::now();
    match result {
        Ok(success) => {
            let changed = match success.payload {
                PlanTaskFetchPayload::Single(tasks) => {
                    let changed = state.projection.truth_state != PlanTaskTruthState::Confirmed
                        || !same_plan_rows(&state.projection.tasks, &tasks);
                    state.projection.tasks = tasks;
                    changed
                }
                PlanTaskFetchPayload::All(multi_session) => {
                    let changed = state.projection.truth_state != PlanTaskTruthState::Confirmed
                        || state.projection.multi_session != multi_session;
                    state.projection.multi_session = multi_session;
                    changed
                }
            };
            state.consecutive_failures = 0;
            state.access_token = Some(success.access_token);
            state.projection.truth_state = PlanTaskTruthState::Confirmed;
            if changed {
                state.projection.sequence = state.projection.sequence.wrapping_add(1);
            }
        }
        Err(error) => {
            if error.invalidates_access_token() {
                state.access_token = None;
            }
            state.consecutive_failures = state.consecutive_failures.saturating_add(1);
            let next_truth =
                if state.projection.tasks.is_empty() && state.projection.multi_session.is_empty() {
                    PlanTaskTruthState::Unavailable
                } else {
                    PlanTaskTruthState::Stale
                };
            if state.projection.truth_state != next_truth {
                state.projection.sequence = state.projection.sequence.wrapping_add(1);
            }
            state.projection.truth_state = next_truth;
            tracing::warn!(error = ?error, %session_id, "durable plan task projection refresh failed");
        }
    }
}

fn same_plan_rows(left: &[SessionTask], right: &[SessionTask]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            left.id == right.id
                && left.title == right.title
                && left.status == right.status
                && left.updated_at == right.updated_at
                && left.blocked_by == right.blocked_by
        })
}

fn lock_state<'a>(
    inner: &'a ObserverInner,
    context: &'static str,
) -> MutexGuard<'a, ObserverState> {
    match inner.state.lock() {
        Ok(state) => state,
        Err(poisoned) => {
            tracing::warn!(context, "plan task observer state poisoned; recovering");
            poisoned.into_inner()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn observer(session_id: Option<&str>) -> PlanTaskObserver {
        PlanTaskObserver::new(
            ThinClient::new("http://127.0.0.1:1", None).expect("test client"),
            None,
            session_id,
        )
    }

    #[test]
    fn explicit_refresh_bypasses_backoff_without_duplicate_plan_reads() {
        let plan_observer = observer(Some("session-1"));
        {
            let mut state = lock_state(&plan_observer.inner, "test");
            state.consecutive_failures = 5;
            state.last_fetch = Instant::now();
            state.projection.truth_state = PlanTaskTruthState::Unavailable;
        }

        assert!(plan_observer.request_refresh());
        {
            let state = lock_state(&plan_observer.inner, "test");
            assert!(
                state.last_fetch.elapsed() >= MAX_FAILURE_BACKOFF,
                "manual refresh must bypass the current retry backoff"
            );
        }

        lock_state(&plan_observer.inner, "test").request_in_flight = true;
        assert!(
            !plan_observer.request_refresh(),
            "manual refresh must not cancel or duplicate an in-flight request"
        );

        assert!(
            !observer(None).request_refresh(),
            "an unbound observer has no canonical plan source to refresh"
        );
    }

    #[test]
    fn late_single_session_fetch_cannot_confirm_the_all_sessions_view() {
        let plan_observer = observer(Some("session-a"));
        let (binding_generation, view_generation) = {
            let state = lock_state(&plan_observer.inner, "test");
            (state.binding_generation, state.view_generation)
        };

        plan_observer.set_view_mode(ViewMode::AllSessions);
        apply_fetch_result(
            &plan_observer.inner,
            binding_generation,
            view_generation,
            ViewMode::SingleSession,
            "session-a",
            Ok(PlanTaskFetchSuccess {
                payload: PlanTaskFetchPayload::Single(Vec::new()),
                access_token: "token".into(),
            }),
        );

        let projection = plan_observer.projection();
        assert_eq!(projection.truth_state, PlanTaskTruthState::Loading);
        assert!(projection.tasks.is_empty());
        assert!(projection.multi_session.is_empty());
    }

    #[test]
    fn canonical_plan_payload_projects_steps_without_todo_semantics() {
        let response: PlanResponse = serde_json::from_value(json!({
            "plan_id": "plan-42",
            "version": 7,
            "plan": {
                "subtasks": [
                    {
                        "id": "prepare",
                        "title": "Prepare migration",
                        "description": "Back up durable state",
                        "depends_on": [],
                        "status": "completed"
                    },
                    {
                        "id": "apply",
                        "title": "Apply migration",
                        "description": null,
                        "depends_on": ["prepare"],
                        "status": "pending"
                    }
                ],
                "notes": null
            }
        }))
        .expect("canonical plan response must decode");

        let rows = project_plan_steps(response);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "plan:plan-42:prepare");
        assert_eq!(rows[0].status, SessionTaskStatusKind::Completed);
        assert_eq!(
            rows[0].metadata.as_ref().and_then(|m| m.get("source")),
            Some(&json!("plan"))
        );
        assert_eq!(rows[1].blocked_by, ["plan:plan-42:prepare"]);
        assert!(rows.iter().all(|row| row.subtasks.is_empty()));
    }

    #[test]
    fn plan_row_identity_includes_plan_and_step_identity() {
        assert_ne!(
            plan_step_row_id("plan-a", "step"),
            plan_step_row_id("plan-b", "step")
        );
        assert_ne!(
            plan_step_row_id("plan-a", "step-a"),
            plan_step_row_id("plan-a", "step-b")
        );
    }

    #[test]
    fn session_plan_selection_does_not_depend_on_authoring_overlay() {
        let selected = select_display_plan(vec![
            PlanSummary {
                plan_id: "draft".to_string(),
                session_id: None,
                goal: String::new(),
                version: 0,
                status: "planning".to_string(),
            },
            PlanSummary {
                plan_id: "running".to_string(),
                session_id: None,
                goal: String::new(),
                version: 0,
                status: "executing".to_string(),
            },
            PlanSummary {
                plan_id: "previous".to_string(),
                session_id: None,
                goal: String::new(),
                version: 0,
                status: "completed".to_string(),
            },
        ])
        .expect("session has a durable plan");
        assert_eq!(selected.plan_id, "running");
    }

    #[test]
    fn all_session_projection_groups_only_open_durable_plans_by_session() {
        let summaries = project_open_plan_summaries(vec![
            PlanSummary {
                plan_id: "running".to_string(),
                session_id: Some("session-a".to_string()),
                goal: "Ship the workbench".to_string(),
                version: 8,
                status: "executing".to_string(),
            },
            PlanSummary {
                plan_id: "done".to_string(),
                session_id: Some("session-a".to_string()),
                goal: "Old work".to_string(),
                version: 3,
                status: "completed".to_string(),
            },
            PlanSummary {
                plan_id: "unbound".to_string(),
                session_id: None,
                goal: "No session".to_string(),
                version: 1,
                status: "executing".to_string(),
            },
        ]);

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].0, "session-a");
        assert_eq!(summaries[0].1.len(), 1);
        assert_eq!(summaries[0].1[0].id, "plan:running");
        assert_eq!(summaries[0].1[0].status, SessionTaskStatusKind::InProgress);
    }
}

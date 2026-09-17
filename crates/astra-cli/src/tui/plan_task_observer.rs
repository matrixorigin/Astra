//! Bounded, read-only TUI observer for a session's canonical Work Task Graph.
//!
//! The observer resolves the exact session binding, follows revision-pinned
//! pages, validates their typed contract, and projects execution and
//! verification as independent facts. It owns no task mutation API.

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use super::work_board_projection::{SessionTask, SessionTaskStatusKind};
use astra_server_types::{
    WORK_TASK_BOARD_CAPABILITY_MAX_BYTES, WORK_TASK_BOARD_MAX_UNAVAILABLE_CAPABILITIES,
    WORK_TASK_BOARD_TEXT_MAX_BYTES, WORK_TASK_BOARD_UPDATE_SCHEMA_VERSION,
    WorkTaskBoardBlockerKindV1, WorkTaskBoardChangeV1, WorkTaskBoardDeclarationStateV1,
    WorkTaskBoardDeliveryStatusV1, WorkTaskBoardExecutionStatusV1, WorkTaskBoardTaskV1,
    WorkTaskBoardUpdateV1,
};
use astra_thin_client::work::{WorkTaskDeclarationStateV2, WorkTaskGraphItemKindV2};
use astra_thin_client::{
    ThinClient, ThinClientError, WorkItemExecutionStatusV2, WorkTaskGraphBasisV2,
    WorkTaskGraphCursorV2, WorkTaskGraphDependencyV2, WorkTaskGraphItemV2, WorkTaskGraphPageV2,
};
use futures_util::FutureExt;
use serde_json::{Map, Value};

use super::task_board_observer::{LiveWorkTaskBoardUpdate, WorkBoardContext};
use super::task_list::task_needs_attention;

const ACTIVE_POLL_INTERVAL: Duration = Duration::from_secs(2);
const QUIET_POLL_INTERVAL: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_FAILURE_BACKOFF: Duration = Duration::from_secs(30);
const WORK_TASK_GRAPH_MAX_PAGES: usize = 32;

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
    pub work: Option<WorkBoardContext>,
    pub tasks: Vec<SessionTask>,
}

impl PlanTaskProjection {
    fn unbound() -> Self {
        Self::default()
    }
}

/// One bounded, cancellable observer for the selected session's active Work.
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
    projection: PlanTaskProjection,
    request_in_flight: bool,
    last_fetch: Instant,
    consecutive_failures: u32,
    access_token: Option<String>,
    /// The session exists but has no canonical Work binding. This is a
    /// stable, successful observation, not an empty Task Graph and not a
    /// transport failure. Automatic polling stays dormant until an explicit
    /// refresh or session rebind invalidates the observation.
    binding_absent: bool,
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
                    binding_absent: false,
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
            state.binding_absent = false;
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
            state.projection.work = None;
        }
        self.abort_fetch();
    }

    pub(crate) fn projection(&self) -> PlanTaskProjection {
        lock_state(&self.inner, "projection").projection.clone()
    }

    /// Clone the render projection only when its monotonic sequence changed.
    /// The TUI calls this on every event-loop tick, while graph refreshes are
    /// intentionally much less frequent; comparing under the lock prevents
    /// unchanged task metadata from being copied at frame rate.
    pub(crate) fn projection_after(&self, sequence: Option<u64>) -> Option<PlanTaskProjection> {
        let state = lock_state(&self.inner, "projection_after");
        (sequence != Some(state.projection.sequence)).then(|| state.projection.clone())
    }

    /// Bypass automatic retry backoff once at the user's request. The
    /// observer still owns the network operation and never interrupts an
    /// existing fetch, so a refresh key cannot create competing reads.
    pub(crate) fn request_refresh(&self) -> bool {
        let mut state = lock_state(&self.inner, "request_refresh");
        if state.session_id.is_empty() || state.request_in_flight {
            return false;
        }
        state.binding_absent = false;
        // A user-requested retry replaces a prior no-data failure with an
        // honest in-flight state. Keep confirmed rows stale while refreshing,
        // but never leave the old "unavailable" error on screen after a new
        // request has actually been accepted.
        if matches!(
            state.projection.truth_state,
            PlanTaskTruthState::Unbound | PlanTaskTruthState::Unavailable
        ) {
            state.projection.truth_state = PlanTaskTruthState::Loading;
            state.projection.sequence = state.projection.sequence.wrapping_add(1);
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
        let (session_id, binding_generation, access_token) = {
            let mut state = lock_state(&self.inner, "maybe_refresh");
            if state.session_id.is_empty()
                || state.binding_absent
                || state.request_in_flight
                || state.last_fetch.elapsed() < refresh_interval(&state)
            {
                return;
            }
            state.request_in_flight = true;
            (
                state.session_id.clone(),
                state.binding_generation,
                state.access_token.clone(),
            )
        };

        let inner = Arc::clone(&self.inner);
        let request_session_id = session_id.clone();
        self.spawn_fetch(binding_generation, session_id, async move {
            fetch_single_plan_projection(
                &inner.api,
                inner.profile.as_deref(),
                &request_session_id,
                access_token,
            )
            .await
        });
    }

    fn spawn_fetch<F>(&self, binding_generation: u64, session_id: String, fetch: F)
    where
        F: Future<Output = Result<PlanTaskFetchSuccess, PlanTaskFetchError>> + Send + 'static,
    {
        let runtime = match tokio::runtime::Handle::try_current() {
            Ok(runtime) => runtime,
            Err(_) => {
                apply_fetch_result(
                    &self.inner,
                    binding_generation,
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
            apply_fetch_result(&inner, binding_generation, &session_id, result);
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
    if state.projection.tasks.iter().any(task_needs_attention) {
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
    projection: PlanTaskFetchProjection,
    access_token: String,
}

enum PlanTaskFetchProjection {
    NotBound,
    Bound(ProjectedWorkGraph),
}

struct ProjectedWorkGraph {
    work: WorkBoardContext,
    tasks: Vec<SessionTask>,
}

/// Decode a server-issued durable Work board event. This protocol boundary
/// never consults assistant prose, tool descriptions, or tool-name matching.
pub(crate) fn live_work_update_from_server_event(
    update: &Value,
) -> Result<LiveWorkTaskBoardUpdate, String> {
    reject_unknown_work_task_board_event_fields(update)?;
    let update: WorkTaskBoardUpdateV1 = serde_json::from_value(update.clone())
        .map_err(|error| format!("invalid Work task-board lifecycle receipt: {error}"))?;
    if update.schema_version != WORK_TASK_BOARD_UPDATE_SCHEMA_VERSION
        || update.work_id.trim().is_empty()
        || update.branch_id.trim().is_empty()
    {
        return Err("unsupported or incomplete Work task-board lifecycle receipt".to_string());
    }
    match update.change {
        WorkTaskBoardChangeV1::Snapshot {
            goal,
            graph_revision,
            criteria_member_count,
            tasks,
        } => {
            if goal.trim().is_empty()
                || goal.len() > WORK_TASK_BOARD_TEXT_MAX_BYTES
                || graph_revision <= 0
                || tasks.is_empty()
                || tasks.len() > 8
            {
                return Err("invalid bounded Work task-board snapshot".to_string());
            }
            let tasks =
                project_live_tasks(&update.work_id, &update.branch_id, graph_revision, tasks)?;
            Ok(LiveWorkTaskBoardUpdate::Snapshot {
                work: WorkBoardContext {
                    work_id: update.work_id,
                    branch_id: update.branch_id,
                    goal,
                    graph_revision,
                    criteria_member_count,
                    milestone_count: 0,
                },
                tasks,
            })
        }
        WorkTaskBoardChangeV1::Upsert {
            graph_revision,
            tasks,
        } => {
            if tasks.is_empty() {
                return Err("empty Work task-board lifecycle update".to_string());
            }
            let tasks = project_live_tasks(
                &update.work_id,
                &update.branch_id,
                graph_revision.unwrap_or_default(),
                tasks,
            )?;
            Ok(LiveWorkTaskBoardUpdate::Upsert {
                work_id: update.work_id,
                branch_id: update.branch_id,
                graph_revision,
                tasks,
            })
        }
    }
}

fn reject_unknown_work_task_board_event_fields(update: &Value) -> Result<(), String> {
    let object = update
        .as_object()
        .ok_or_else(|| "Work task-board event must be an object".to_string())?;
    let kind = object
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| "Work task-board event is missing kind".to_string())?;
    let allowed: &[&str] = match kind {
        "snapshot" => &[
            "schema_version",
            "work_id",
            "branch_id",
            "kind",
            "goal",
            "graph_revision",
            "criteria_member_count",
            "tasks",
        ],
        "upsert" => &[
            "schema_version",
            "work_id",
            "branch_id",
            "kind",
            "graph_revision",
            "tasks",
        ],
        _ => return Err("unsupported Work task-board event kind".to_string()),
    };
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err("Work task-board event contains unknown fields".to_string());
    }
    Ok(())
}

fn project_live_tasks(
    work_id: &str,
    branch_id: &str,
    graph_revision: i64,
    tasks: Vec<WorkTaskBoardTaskV1>,
) -> Result<Vec<SessionTask>, String> {
    let mut item_ids = BTreeSet::new();
    tasks
        .into_iter()
        .map(|task| {
            if task.item_id.trim().is_empty()
                || task.item_revision <= 0
                || task.objective.trim().is_empty()
                || task.expected_result.trim().is_empty()
                || task.objective.len() > WORK_TASK_BOARD_TEXT_MAX_BYTES
                || task.expected_result.len() > WORK_TASK_BOARD_TEXT_MAX_BYTES
                || task
                    .delivery_summary
                    .as_ref()
                    .is_some_and(|summary| summary.len() > WORK_TASK_BOARD_TEXT_MAX_BYTES)
                || task.unavailable_capabilities.len()
                    > WORK_TASK_BOARD_MAX_UNAVAILABLE_CAPABILITIES
                || task.unavailable_capabilities.iter().any(|capability| {
                    capability.is_empty() || capability.len() > WORK_TASK_BOARD_CAPABILITY_MAX_BYTES
                })
                || !item_ids.insert(task.item_id.clone())
            {
                return Err("invalid or duplicate Work task-board item".to_string());
            }
            Ok(project_live_task(work_id, branch_id, graph_revision, task))
        })
        .collect()
}

fn project_live_task(
    work_id: &str,
    branch_id: &str,
    graph_revision: i64,
    task: WorkTaskBoardTaskV1,
) -> SessionTask {
    let declaration_state = match task.declaration_state {
        WorkTaskBoardDeclarationStateV1::Active => "active",
        WorkTaskBoardDeclarationStateV1::Superseded => "superseded",
        WorkTaskBoardDeclarationStateV1::Cancelled => "cancelled",
    };
    let execution_status = match task.execution_status {
        WorkTaskBoardExecutionStatusV1::NotStarted => "not_started",
        WorkTaskBoardExecutionStatusV1::Running => "running",
        WorkTaskBoardExecutionStatusV1::Waiting => "waiting",
        WorkTaskBoardExecutionStatusV1::Paused => "paused",
        WorkTaskBoardExecutionStatusV1::Completed => "completed",
        WorkTaskBoardExecutionStatusV1::Delegated => "delegated",
        WorkTaskBoardExecutionStatusV1::Failed => "failed",
        WorkTaskBoardExecutionStatusV1::Cancelled => "cancelled",
    };
    let delivery_status = match task.delivery_status {
        WorkTaskBoardDeliveryStatusV1::Unreported => "unreported",
        WorkTaskBoardDeliveryStatusV1::Delivered => "delivered",
        WorkTaskBoardDeliveryStatusV1::Blocked => "blocked",
        WorkTaskBoardDeliveryStatusV1::Failed => "failed",
    };
    let status =
        match task.declaration_state {
            WorkTaskBoardDeclarationStateV1::Superseded
            | WorkTaskBoardDeclarationStateV1::Cancelled => SessionTaskStatusKind::Cancelled,
            WorkTaskBoardDeclarationStateV1::Active => match task.execution_status {
                WorkTaskBoardExecutionStatusV1::NotStarted => SessionTaskStatusKind::Pending,
                WorkTaskBoardExecutionStatusV1::Running
                | WorkTaskBoardExecutionStatusV1::Delegated => SessionTaskStatusKind::InProgress,
                WorkTaskBoardExecutionStatusV1::Waiting
                | WorkTaskBoardExecutionStatusV1::Paused => SessionTaskStatusKind::Paused,
                WorkTaskBoardExecutionStatusV1::Completed => SessionTaskStatusKind::Completed,
                WorkTaskBoardExecutionStatusV1::Failed => SessionTaskStatusKind::Failed,
                WorkTaskBoardExecutionStatusV1::Cancelled => SessionTaskStatusKind::Cancelled,
            },
        };
    let mut metadata = Map::new();
    metadata.insert("source".into(), Value::String("work_task_graph".into()));
    metadata.insert("work_id".into(), Value::String(work_id.to_string()));
    metadata.insert("branch_id".into(), Value::String(branch_id.to_string()));
    if graph_revision > 0 {
        metadata.insert("graph_revision".into(), Value::from(graph_revision));
    }
    metadata.insert("item_id".into(), Value::String(task.item_id.clone()));
    metadata.insert("item_revision".into(), Value::from(task.item_revision));
    metadata.insert("item_kind".into(), Value::String("task".into()));
    metadata.insert(
        "declaration_state".into(),
        Value::String(declaration_state.into()),
    );
    metadata.insert(
        "execution_status".into(),
        Value::String(execution_status.into()),
    );
    metadata.insert(
        "execution_terminal".into(),
        Value::Bool(matches!(
            task.execution_status,
            WorkTaskBoardExecutionStatusV1::Completed
                | WorkTaskBoardExecutionStatusV1::Delegated
                | WorkTaskBoardExecutionStatusV1::Failed
                | WorkTaskBoardExecutionStatusV1::Cancelled
        )),
    );
    metadata.insert(
        "delivery_status".into(),
        Value::String(delivery_status.into()),
    );
    metadata.insert(
        "verification_status".into(),
        Value::String("unknown".into()),
    );
    if let Some(summary) = task.delivery_summary {
        metadata.insert("delivery_summary".into(), Value::String(summary));
    }
    if let Some(blocker_kind) = task.blocker_kind {
        let blocker_kind = match blocker_kind {
            WorkTaskBoardBlockerKindV1::CapabilityUnavailable => "capability_unavailable",
            WorkTaskBoardBlockerKindV1::DependencyBlocked => "dependency_blocked",
            WorkTaskBoardBlockerKindV1::PolicyBlocked => "policy_blocked",
            WorkTaskBoardBlockerKindV1::ExternalUnavailable => "external_unavailable",
        };
        metadata.insert(
            "delivery_blocker_kind".into(),
            Value::String(blocker_kind.into()),
        );
    }
    metadata.insert(
        "unavailable_capabilities".into(),
        Value::Array(
            task.unavailable_capabilities
                .into_iter()
                .map(Value::String)
                .collect(),
        ),
    );
    SessionTask {
        id: work_item_row_id(work_id, branch_id, &task.item_id),
        title: task.objective,
        description: Some(task.expected_result),
        status,
        subtasks: Vec::new(),
        created_at: String::new(),
        updated_at: if graph_revision > 0 {
            format!("graph-r{graph_revision}")
        } else {
            "live-work-update".into()
        },
        active_form: None,
        owner: None,
        metadata: Some(metadata),
        blocks: Vec::new(),
        blocked_by: Vec::new(),
    }
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
    let response = tokio::time::timeout(REQUEST_TIMEOUT, async {
        let binding = match api.get_work_session_binding(&token, session_id).await {
            Ok(binding) => binding,
            Err(ThinClientError::Api { status, .. })
                if status == reqwest::StatusCode::NOT_FOUND =>
            {
                return Ok(PlanTaskFetchProjection::NotBound);
            }
            Err(error) => return Err(PlanTaskFetchError::Transport(error)),
        };
        let mut page = fetch_work_graph_page(
            api,
            &token,
            &binding.work_id,
            &binding.branch_id,
            binding.graph_revision,
            0,
            0,
        )
        .await?;
        validate_graph_page(
            &page,
            &binding.work_id,
            &binding.branch_id,
            binding.graph_revision,
            0,
            0,
        )?;
        let item_total = page.items.total;
        let dependency_total = page.dependencies.total;
        let basis = page.basis;
        let mut items = page.items.entries;
        let mut dependencies = page.dependencies.entries;
        let mut next = page.next_cursor.take();
        let mut seen = BTreeSet::new();
        for _ in 0..WORK_TASK_GRAPH_MAX_PAGES {
            let Some(cursor) = next else { break };
            if !seen.insert(cursor) {
                return Err(PlanTaskFetchError::InvalidResponse(
                    "Task Graph returned a repeated cursor".to_string(),
                ));
            }
            let continuation = fetch_work_graph_page(
                api,
                &token,
                &binding.work_id,
                &binding.branch_id,
                binding.graph_revision,
                cursor.item_offset,
                cursor.dependency_offset,
            )
            .await?;
            validate_graph_page(
                &continuation,
                &binding.work_id,
                &binding.branch_id,
                binding.graph_revision,
                cursor.item_offset,
                cursor.dependency_offset,
            )?;
            if continuation.items.total != item_total
                || continuation.dependencies.total != dependency_total
                || continuation.basis.graph_manifest_hash != basis.graph_manifest_hash
            {
                return Err(PlanTaskFetchError::InvalidResponse(
                    "Task Graph continuation changed immutable totals or manifest".to_string(),
                ));
            }
            items.extend(continuation.items.entries);
            dependencies.extend(continuation.dependencies.entries);
            next = continuation.next_cursor;
        }
        if next.is_some() {
            return Err(PlanTaskFetchError::InvalidResponse(
                "Task Graph exceeded the bounded client page budget".to_string(),
            ));
        }
        if items.len() != usize::from(item_total)
            || dependencies.len() != usize::from(dependency_total)
        {
            return Err(PlanTaskFetchError::InvalidResponse(
                "Task Graph pagination ended before declared totals".to_string(),
            ));
        }
        project_work_graph(basis, items, dependencies).map(PlanTaskFetchProjection::Bound)
    })
    .await
    .map_err(|_| PlanTaskFetchError::Timeout)??;
    Ok(PlanTaskFetchSuccess {
        projection: response,
        access_token: token,
    })
}

async fn fetch_work_graph_page(
    api: &ThinClient,
    token: &str,
    work_id: &str,
    branch_id: &str,
    graph_revision: i64,
    item_offset: u16,
    dependency_offset: u16,
) -> Result<WorkTaskGraphPageV2, PlanTaskFetchError> {
    api.get_work_branch_task_graph_page(
        token,
        work_id,
        branch_id,
        Some(graph_revision),
        item_offset,
        dependency_offset,
    )
    .await
    .map_err(PlanTaskFetchError::Transport)
}

fn validate_graph_page(
    page: &WorkTaskGraphPageV2,
    work_id: &str,
    branch_id: &str,
    graph_revision: i64,
    item_offset: u16,
    dependency_offset: u16,
) -> Result<(), PlanTaskFetchError> {
    page.validate()
        .map_err(PlanTaskFetchError::InvalidResponse)?;
    let valid = page.basis.work_id == work_id
        && page.basis.branch_id == branch_id
        && page.basis.graph_revision == graph_revision
        && page.cursor
            == (WorkTaskGraphCursorV2 {
                graph_revision,
                item_offset,
                dependency_offset,
            })
        && page.items.offset == item_offset
        && page.dependencies.offset == dependency_offset;
    if valid {
        Ok(())
    } else {
        Err(PlanTaskFetchError::InvalidResponse(
            "Task Graph page violated its pinned bounded contract".to_string(),
        ))
    }
}

fn project_work_graph(
    basis: WorkTaskGraphBasisV2,
    items: Vec<WorkTaskGraphItemV2>,
    dependencies: Vec<WorkTaskGraphDependencyV2>,
) -> Result<ProjectedWorkGraph, PlanTaskFetchError> {
    let known_ids = items
        .iter()
        .map(|item| item.item_id.as_str())
        .collect::<BTreeSet<_>>();
    if dependencies.iter().any(|edge| {
        !known_ids.contains(edge.predecessor_item_id.as_str())
            || !known_ids.contains(edge.successor_item_id.as_str())
    }) {
        return Err(PlanTaskFetchError::InvalidResponse(
            "Task Graph dependency referenced an unknown item".to_string(),
        ));
    }
    let task_ids = items
        .iter()
        .filter(|item| item.kind == WorkTaskGraphItemKindV2::Task)
        .map(|item| item.item_id.clone())
        .collect::<BTreeSet<_>>();
    let milestone_count = u16::try_from(
        items
            .iter()
            .filter(|item| item.kind == WorkTaskGraphItemKindV2::Milestone)
            .count(),
    )
    .map_err(|error| PlanTaskFetchError::InvalidResponse(error.to_string()))?;
    let task_row_ids = task_ids
        .iter()
        .map(|item_id| {
            (
                item_id.clone(),
                work_item_row_id(&basis.work_id, &basis.branch_id, item_id),
            )
        })
        .collect::<HashMap<_, _>>();
    let mut blockers_by_task = HashMap::<String, Vec<String>>::new();
    let mut blocks_by_task = HashMap::<String, Vec<String>>::new();
    for edge in dependencies {
        let (Some(predecessor), Some(successor)) = (
            task_row_ids.get(&edge.predecessor_item_id),
            task_row_ids.get(&edge.successor_item_id),
        ) else {
            // Milestones describe graph context but are not executable TUI
            // rows, so their edges do not become synthetic task blockers.
            continue;
        };
        blocks_by_task
            .entry(edge.predecessor_item_id.clone())
            .or_default()
            .push(successor.clone());
        blockers_by_task
            .entry(edge.successor_item_id)
            .or_default()
            .push(predecessor.clone());
    }
    let mut rows = Vec::with_capacity(task_ids.len());
    for item in items
        .into_iter()
        .filter(|item| item.kind == WorkTaskGraphItemKindV2::Task)
    {
        let row_id = work_item_row_id(&basis.work_id, &basis.branch_id, &item.item_id);
        let mut metadata = Map::new();
        metadata.insert("source".into(), Value::String("work_task_graph".into()));
        metadata.insert("work_id".into(), Value::String(basis.work_id.clone()));
        metadata.insert("branch_id".into(), Value::String(basis.branch_id.clone()));
        metadata.insert("graph_revision".into(), Value::from(basis.graph_revision));
        metadata.insert("item_id".into(), Value::String(item.item_id.clone()));
        metadata.insert("item_revision".into(), Value::from(item.revision));
        metadata.insert("item_kind".into(), Value::String("task".into()));
        metadata.insert(
            "declaration_state".into(),
            Value::String(
                match item.declaration_state {
                    WorkTaskDeclarationStateV2::Active => "active",
                    WorkTaskDeclarationStateV2::Superseded => "superseded",
                    WorkTaskDeclarationStateV2::Cancelled => "cancelled",
                }
                .into(),
            ),
        );
        metadata.insert(
            "execution_status".into(),
            Value::String(item.execution.status.as_str().into()),
        );
        metadata.insert(
            "execution_terminal".into(),
            Value::Bool(item.execution.terminal),
        );
        metadata.insert(
            "verification_status".into(),
            Value::String(item.verification.status.as_str().into()),
        );
        metadata.insert(
            "delivery_status".into(),
            Value::String(item.delivery.status.as_str().into()),
        );
        if let Some(summary) = item.delivery.summary.as_ref() {
            metadata.insert("delivery_summary".into(), Value::String(summary.clone()));
        }
        if let Some(blocker_kind) = item.delivery.blocker_kind.as_ref() {
            metadata.insert(
                "delivery_blocker_kind".into(),
                Value::String(blocker_kind.as_str().into()),
            );
        }
        metadata.insert(
            "unavailable_capabilities".into(),
            Value::Array(
                item.delivery
                    .unavailable_capabilities
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect(),
            ),
        );
        if let Some(run) = item.execution.run.as_ref() {
            metadata.insert("run_id".into(), Value::String(run.run_id.clone()));
            metadata.insert("attempt_id".into(), Value::String(run.attempt_id.clone()));
            metadata.insert("run_graph_revision".into(), Value::from(run.graph_revision));
            metadata.insert("run_generation".into(), Value::from(run.run_generation));
            metadata.insert("run_last_event_idx".into(), Value::from(run.last_event_idx));
        }
        if let Some(check) = item.verification.latest_check.as_ref() {
            metadata.insert(
                "check_run_id".into(),
                Value::String(check.check_run_id.clone()),
            );
            metadata.insert(
                "check_criterion_id".into(),
                Value::String(check.criterion.criterion_id.clone()),
            );
            metadata.insert(
                "check_criterion_revision".into(),
                Value::from(check.criterion.revision),
            );
            metadata.insert(
                "check_criterion_set_revision".into(),
                Value::from(check.criterion_set_revision),
            );
            metadata.insert(
                "check_graph_revision".into(),
                Value::from(check.graph_revision),
            );
            metadata.insert(
                "check_verifier_kind".into(),
                Value::String(check.verifier_kind.as_str().into()),
            );
            metadata.insert(
                "check_outcome".into(),
                Value::String(check.outcome.as_str().into()),
            );
            metadata.insert(
                "check_freshness".into(),
                Value::String(check.freshness.as_str().into()),
            );
            metadata.insert(
                "check_coverage".into(),
                Value::String(check.coverage.as_str().into()),
            );
            metadata.insert(
                "check_subject_revision".into(),
                Value::String(check.subject_revision.clone()),
            );
            metadata.insert(
                "check_evidence_ref_count".into(),
                Value::from(check.evidence_ref_count),
            );
            metadata.insert(
                "check_produced_at".into(),
                Value::String(check.produced_at.clone()),
            );
            if let Some(expires_at) = check.expires_at.as_ref() {
                metadata.insert("check_expires_at".into(), Value::String(expires_at.clone()));
            }
        }
        let status = projected_execution_status(&item)?;
        let blockers = blockers_by_task.remove(&item.item_id).unwrap_or_default();
        let blocks = blocks_by_task.remove(&item.item_id).unwrap_or_default();
        rows.push(SessionTask {
            id: row_id,
            title: item.objective,
            description: Some(item.expected_result),
            status,
            subtasks: Vec::new(),
            created_at: String::new(),
            updated_at: item
                .execution
                .run
                .as_ref()
                .map(|run| run.updated_at.clone())
                .unwrap_or_else(|| format!("graph-r{}", basis.graph_revision)),
            active_form: None,
            owner: None,
            metadata: Some(metadata),
            blocks,
            blocked_by: blockers,
        });
    }
    Ok(ProjectedWorkGraph {
        work: WorkBoardContext {
            work_id: basis.work_id,
            branch_id: basis.branch_id,
            goal: basis.goal,
            graph_revision: basis.graph_revision,
            criteria_member_count: basis.criteria_member_count,
            milestone_count,
        },
        tasks: rows,
    })
}

fn projected_execution_status(
    item: &WorkTaskGraphItemV2,
) -> Result<SessionTaskStatusKind, PlanTaskFetchError> {
    if item.declaration_state != WorkTaskDeclarationStateV2::Active {
        return Ok(SessionTaskStatusKind::Cancelled);
    }
    match item.execution.status {
        WorkItemExecutionStatusV2::NotStarted => Ok(SessionTaskStatusKind::Pending),
        WorkItemExecutionStatusV2::Running | WorkItemExecutionStatusV2::Delegated => {
            Ok(SessionTaskStatusKind::InProgress)
        }
        WorkItemExecutionStatusV2::Waiting | WorkItemExecutionStatusV2::Paused => {
            Ok(SessionTaskStatusKind::Paused)
        }
        WorkItemExecutionStatusV2::Completed => Ok(SessionTaskStatusKind::Completed),
        WorkItemExecutionStatusV2::Failed => Ok(SessionTaskStatusKind::Failed),
        WorkItemExecutionStatusV2::Cancelled => Ok(SessionTaskStatusKind::Cancelled),
    }
}

fn work_item_row_id(work_id: &str, branch_id: &str, item_id: &str) -> String {
    format!("work:{work_id}:{branch_id}:{item_id}")
}

fn apply_fetch_result(
    inner: &ObserverInner,
    binding_generation: u64,
    session_id: &str,
    result: Result<PlanTaskFetchSuccess, PlanTaskFetchError>,
) {
    let mut state = lock_state(inner, "apply_fetch_result");
    if state.binding_generation != binding_generation || state.session_id != session_id {
        return;
    }
    state.request_in_flight = false;
    state.last_fetch = Instant::now();
    match result {
        Ok(success) => {
            state.consecutive_failures = 0;
            state.access_token = Some(success.access_token);
            match success.projection {
                PlanTaskFetchProjection::NotBound => {
                    let changed = state.projection.truth_state != PlanTaskTruthState::Unbound
                        || state.projection.work.is_some()
                        || !state.projection.tasks.is_empty();
                    state.binding_absent = true;
                    state.projection.work = None;
                    state.projection.tasks.clear();
                    state.projection.truth_state = PlanTaskTruthState::Unbound;
                    if changed {
                        state.projection.sequence = state.projection.sequence.wrapping_add(1);
                    }
                }
                PlanTaskFetchProjection::Bound(projected) => {
                    let changed = state.projection.truth_state != PlanTaskTruthState::Confirmed
                        || state.projection.work.as_ref() != Some(&projected.work)
                        || !same_plan_rows(&state.projection.tasks, &projected.tasks);
                    state.binding_absent = false;
                    state.projection.work = Some(projected.work);
                    state.projection.tasks = projected.tasks;
                    state.projection.truth_state = PlanTaskTruthState::Confirmed;
                    if changed {
                        state.projection.sequence = state.projection.sequence.wrapping_add(1);
                    }
                }
            }
        }
        Err(error) => {
            if error.invalidates_access_token() {
                state.access_token = None;
            }
            state.consecutive_failures = state.consecutive_failures.saturating_add(1);
            let next_truth = if state.projection.work.is_none() && state.projection.tasks.is_empty()
            {
                PlanTaskTruthState::Unavailable
            } else {
                PlanTaskTruthState::Stale
            };
            if state.projection.truth_state != next_truth {
                state.projection.sequence = state.projection.sequence.wrapping_add(1);
            }
            state.projection.truth_state = next_truth;
            tracing::warn!(error = ?error, %session_id, "Work Task Graph projection refresh failed");
        }
    }
}

fn same_plan_rows(left: &[SessionTask], right: &[SessionTask]) -> bool {
    left == right
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
            assert_eq!(state.projection.truth_state, PlanTaskTruthState::Loading);
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
    fn missing_work_binding_is_unbound_and_dormant_until_explicit_refresh() {
        let plan_observer = observer(Some("session-1"));
        let generation = lock_state(&plan_observer.inner, "test").binding_generation;

        apply_fetch_result(
            &plan_observer.inner,
            generation,
            "session-1",
            Ok(PlanTaskFetchSuccess {
                projection: PlanTaskFetchProjection::NotBound,
                access_token: "token".to_string(),
            }),
        );

        {
            let state = lock_state(&plan_observer.inner, "test");
            assert_eq!(state.projection.truth_state, PlanTaskTruthState::Unbound);
            assert!(state.projection.tasks.is_empty());
            assert!(state.binding_absent);
        }

        plan_observer.maybe_refresh();
        assert!(
            !lock_state(&plan_observer.inner, "test").request_in_flight,
            "a successful no-binding observation must not create permanent polling"
        );

        assert!(plan_observer.request_refresh());
        let state = lock_state(&plan_observer.inner, "test");
        assert!(!state.binding_absent);
        assert_eq!(state.projection.truth_state, PlanTaskTruthState::Loading);
    }

    #[test]
    fn unchanged_projection_is_not_cloned_for_event_loop_ticks() {
        let plan_observer = observer(Some("session-1"));
        let first = plan_observer
            .projection_after(None)
            .expect("the initial sequence is observable once");
        assert!(
            plan_observer
                .projection_after(Some(first.sequence))
                .is_none()
        );

        plan_observer.rebind_session(Some("session-2"));
        let rebound = plan_observer
            .projection_after(Some(first.sequence))
            .expect("a session rebind advances the projection sequence");
        assert_ne!(rebound.sequence, first.sequence);
    }

    fn graph_page() -> WorkTaskGraphPageV2 {
        let mut page: WorkTaskGraphPageV2 = serde_json::from_value(json!({
            "schema_version": 2,
            "scope": "declared_work",
            "basis": {
                "work_id": "work-42",
                "work_revision": 3,
                "goal_revision": 2,
                "goal": "Ship a reliable migration",
                "criteria_set_revision": 4,
                "criteria_member_count": 2,
                "criteria_manifest_hash": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "branch_id": "main",
                "branch_revision": 5,
                "branch_goal_revision": 2,
                "branch_criteria_set_revision": 4,
                "branch_basis_graph_revision": 7,
                "graph_revision": 7,
                "graph_item_count": 3,
                "graph_edge_count": 1,
                "graph_manifest_hash": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            },
            "cursor": {"graph_revision": 7, "item_offset": 0, "dependency_offset": 0},
            "next_cursor": null,
            "items": {
                "offset": 0,
                "limit": 8,
                "total": 3,
                "entries": [
                    {
                        "item_id": "prepare",
                        "revision": 1,
                        "kind": "milestone",
                        "objective": "Prepare migration",
                        "expected_result": "Durable state is backed up",
                        "declaration_state": "active",
                        "execution": {
                            "status": "completed",
                            "terminal": true,
                            "run": {
                                "run_id": "run-1",
                                "attempt_id": "attempt-1",
                                "graph_revision": 7,
                                "run_generation": 1,
                                "last_event_idx": 9,
                                "updated_at": "2026-08-03T00:00:00Z"
                            }
                        },
                        "delivery": {"status": "unreported", "summary": null, "blocker_kind": null, "unavailable_capabilities": []},
                        "verification": {"status": "unknown", "latest_check": null}
                    },
                    {
                        "item_id": "apply",
                        "revision": 2,
                        "kind": "task",
                        "objective": "Apply migration",
                        "expected_result": "All records use the new schema",
                        "declaration_state": "active",
                        "execution": {"status": "not_started", "terminal": false, "run": null},
                        "delivery": {"status": "unreported", "summary": null, "blocker_kind": null, "unavailable_capabilities": []},
                        "verification": {"status": "unknown", "latest_check": null}
                    },
                    {
                        "item_id": "inspect",
                        "revision": 1,
                        "kind": "task",
                        "objective": "Inspect migration",
                        "expected_result": "Migration evidence is collected",
                        "declaration_state": "active",
                        "execution": {"status": "not_started", "terminal": false, "run": null},
                        "delivery": {"status": "unreported", "summary": null, "blocker_kind": null, "unavailable_capabilities": []},
                        "verification": {"status": "unknown", "latest_check": null}
                    }
                ]
            },
            "dependencies": {
                "offset": 0,
                "limit": 128,
                "total": 1,
                "entries": [{
                    "predecessor_item_id": "prepare",
                    "successor_item_id": "apply",
                    "kind": "dependency"
                }]
            }
        }))
        .expect("canonical Work Task Graph fixture must decode");
        page.items
            .entries
            .sort_by(|left, right| left.item_id.cmp(&right.item_id));
        page
    }

    #[test]
    fn canonical_work_graph_separates_goal_context_from_executable_tasks() {
        let page = graph_page();
        validate_graph_page(&page, "work-42", "main", 7, 0, 0).expect("pinned graph page is valid");
        let projected =
            project_work_graph(page.basis, page.items.entries, page.dependencies.entries)
                .expect("canonical graph projects");

        assert_eq!(projected.work.goal, "Ship a reliable migration");
        assert_eq!(projected.work.criteria_member_count, 2);
        assert_eq!(projected.work.milestone_count, 1);
        assert_eq!(projected.tasks.len(), 2);
        assert_eq!(projected.tasks[0].id, "work:work-42:main:apply");
        assert_eq!(projected.tasks[0].status, SessionTaskStatusKind::Pending);
        assert_eq!(
            projected.tasks[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("verification_status")),
            Some(&json!("unknown")),
            "verification remains an independent fact"
        );
        assert!(
            projected.tasks[0].blocked_by.is_empty(),
            "a milestone is context, not a synthetic task blocker"
        );
        assert_eq!(projected.tasks[1].id, "work:work-42:main:inspect");
        assert!(projected.tasks.iter().all(|row| row.subtasks.is_empty()));
    }

    #[test]
    fn task_dependencies_project_as_bidirectional_adjacency() {
        let mut page = graph_page();
        page.basis.graph_edge_count = 2;
        page.dependencies.total = 2;
        page.dependencies.entries.push(WorkTaskGraphDependencyV2 {
            predecessor_item_id: "inspect".into(),
            successor_item_id: "apply".into(),
            kind: astra_thin_client::work::WorkTaskGraphDependencyKindV2::Dependency,
        });

        let projected =
            project_work_graph(page.basis, page.items.entries, page.dependencies.entries)
                .expect("valid task adjacency projects");
        let apply = projected
            .tasks
            .iter()
            .find(|task| task.id.ends_with(":apply"))
            .expect("apply task");
        let inspect = projected
            .tasks
            .iter()
            .find(|task| task.id.ends_with(":inspect"))
            .expect("inspect task");
        assert_eq!(
            apply.blocked_by,
            [work_item_row_id("work-42", "main", "inspect")]
        );
        assert_eq!(
            inspect.blocks,
            [work_item_row_id("work-42", "main", "apply")]
        );
    }

    #[test]
    fn graph_row_identity_includes_work_branch_and_item_identity() {
        assert_ne!(
            work_item_row_id("work-a", "main", "step"),
            work_item_row_id("work-b", "main", "step")
        );
        assert_ne!(
            work_item_row_id("work-a", "main", "step"),
            work_item_row_id("work-a", "experiment", "step")
        );
        assert_ne!(
            work_item_row_id("work-a", "main", "step-a"),
            work_item_row_id("work-a", "main", "step-b")
        );
    }

    #[test]
    fn graph_page_rejects_revision_drift_and_unknown_dependency_endpoints() {
        let mut page = graph_page();
        page.cursor.graph_revision = 8;
        assert!(validate_graph_page(&page, "work-42", "main", 7, 0, 0).is_err());

        let mut page = graph_page();
        page.dependencies.entries[0].predecessor_item_id = "missing".into();
        let result = project_work_graph(page.basis, page.items.entries, page.dependencies.entries);
        assert!(matches!(
            result,
            Err(PlanTaskFetchError::InvalidResponse(_))
        ));
    }

    #[test]
    fn graph_item_rejects_incoherent_execution_and_verification_facts() {
        let mut page = graph_page();
        page.items
            .entries
            .iter_mut()
            .find(|item| item.item_id == "prepare")
            .unwrap()
            .execution
            .terminal = false;
        assert!(page.validate().is_err());

        let mut page = graph_page();
        page.items.entries[0].verification.status =
            astra_thin_client::WorkItemVerificationStatusV2::EvidenceAvailable;
        assert!(page.validate().is_err());

        let mut page = graph_page();
        page.items.entries[0].delivery.status =
            astra_thin_client::WorkItemDeliveryStatusV2::Blocked;
        page.items.entries[0].delivery.summary = Some("missing capability".into());
        page.items.entries[0].delivery.blocker_kind =
            Some(astra_thin_client::WorkItemDeliveryBlockerKindV2::CapabilityUnavailable);
        assert!(
            page.validate().is_err(),
            "capability blockers without exact capability refs must fail closed"
        );
    }

    #[test]
    fn live_work_event_projects_immediately_without_model_text_inference() {
        let update = json!({
                "schema_version": 1,
                "work_id": "work-42",
                "branch_id": "main",
                "kind": "snapshot",
                "goal": "Ship a reliable migration",
                "graph_revision": 7,
                "criteria_member_count": 0,
                "tasks": [
                    {
                        "item_id": "apply",
                        "item_revision": 1,
                        "objective": "Apply migration",
                        "expected_result": "All records use the new schema",
                        "declaration_state": "active",
                        "execution_status": "running",
                        "delivery_status": "unreported",
                        "delivery_summary": null,
                        "blocker_kind": null,
                        "unavailable_capabilities": []
                    },
                    {
                        "item_id": "verify",
                        "item_revision": 1,
                        "objective": "Verify migration",
                        "expected_result": "Evidence is recorded",
                        "declaration_state": "active",
                        "execution_status": "not_started",
                        "delivery_status": "unreported",
                        "delivery_summary": null,
                        "blocker_kind": null,
                        "unavailable_capabilities": []
                    }
                ]
        });
        let projected =
            live_work_update_from_server_event(&update).expect("typed server event projects");
        let LiveWorkTaskBoardUpdate::Snapshot { work, tasks } = projected else {
            panic!("start receipt must create a complete live snapshot");
        };
        assert_eq!(work.goal, "Ship a reliable migration");
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].status, SessionTaskStatusKind::InProgress);
        assert_eq!(tasks[1].status, SessionTaskStatusKind::Pending);
        assert_eq!(
            tasks[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("source")),
            Some(&json!("work_task_graph")),
        );
    }

    #[test]
    fn live_work_receipt_rejects_unknown_fields_instead_of_best_effort_parsing() {
        let mut update = json!({
                "schema_version": 1,
                "work_id": "work-42",
                "branch_id": "main",
                "kind": "upsert",
                "graph_revision": null,
                "tasks": [{
                    "item_id": "apply",
                    "item_revision": 1,
                    "objective": "Apply migration",
                    "expected_result": "All records use the new schema",
                    "declaration_state": "active",
                    "execution_status": "completed",
                    "delivery_status": "delivered",
                    "delivery_summary": "done",
                    "blocker_kind": null,
                    "unavailable_capabilities": [],
                    "untrusted_display_hint": "must not be ignored"
                }]
        });
        assert!(
            live_work_update_from_server_event(&update).is_err(),
            "a drifting lifecycle schema must fail closed rather than become a partial board"
        );

        update["tasks"][0]
            .as_object_mut()
            .expect("task object")
            .remove("untrusted_display_hint");
        update["untrusted_top_level_hint"] = json!("must not be ignored");
        assert!(
            live_work_update_from_server_event(&update).is_err(),
            "the versioned event must reject unknown top-level fields too"
        );
    }

    #[test]
    fn live_work_receipt_rejects_unbounded_display_text() {
        let update = json!({
                "schema_version": 1,
                "work_id": "work-42",
                "branch_id": "main",
                "kind": "snapshot",
                "goal": "g".repeat(WORK_TASK_BOARD_TEXT_MAX_BYTES + 1),
                "graph_revision": 1,
                "criteria_member_count": 0,
                "tasks": [{
                    "item_id": "apply",
                    "item_revision": 1,
                    "objective": "Apply migration",
                    "expected_result": "All records use the new schema",
                    "declaration_state": "active",
                    "execution_status": "running",
                    "delivery_status": "unreported",
                    "delivery_summary": null,
                    "blocker_kind": null,
                    "unavailable_capabilities": []
                }]
        });
        assert!(
            live_work_update_from_server_event(&update).is_err(),
            "a live receipt must stay within the transport display budget"
        );
    }
}

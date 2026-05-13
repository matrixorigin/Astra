use astra_core::{ErrorResponse, SharedPool, error_response, error_response_coded};
use async_trait::async_trait;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};
use thiserror::Error;
use uuid::Uuid;

pub const RUN_LIFECYCLE_UNCONFIGURED_ERROR_CODE: &str = "run_lifecycle_unconfigured";
pub const SSE_HEARTBEAT_INTERVAL_SECS: u64 = 15;

pub fn is_run_lifecycle_unconfigured_error(status: StatusCode, error: &ErrorResponse) -> bool {
    status == StatusCode::NOT_IMPLEMENTED
        && error.error_code.as_deref() == Some(RUN_LIFECYCLE_UNCONFIGURED_ERROR_CODE)
}

#[async_trait]
pub trait RunLifecycleService: Send + Sync {
    async fn create_run(
        &self,
        user_id: String,
        request: ChatRequestData,
    ) -> Result<ChatRunRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn stream_chat(
        &self,
        user_id: String,
        request: ChatRequestData,
    ) -> Result<ChatStreamRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn get_run_status(
        &self,
        run_id: String,
        user_id: String,
    ) -> Result<RunStatusRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn stream_run(
        &self,
        run_id: String,
        user_id: String,
        last_index: u32,
    ) -> Result<Vec<serde_json::Value>, (StatusCode, Json<ErrorResponse>)>;

    async fn stream_run_live(
        &self,
        run_id: String,
        user_id: String,
        last_index: u32,
    ) -> Result<ChatStreamRecord, (StatusCode, Json<ErrorResponse>)> {
        let events = self.stream_run(run_id.clone(), user_id, last_index).await?;
        Ok(ChatStreamRecord {
            session_id: String::new(),
            run_id,
            events,
            event_rx: None,
        })
    }

    async fn cancel_run(
        &self,
        run_id: String,
        user_id: String,
    ) -> Result<CancelRunRecord, (StatusCode, Json<ErrorResponse>)>;

    async fn list_runs(
        &self,
        user_id: String,
        limit: u32,
        offset: u32,
    ) -> Result<RunListRecord, (StatusCode, Json<ErrorResponse>)>;

    /// Pause an active run. Default: NOT_IMPLEMENTED.
    async fn pause_run(
        &self,
        _run_id: String,
        _user_id: String,
    ) -> Result<RunMutationRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Pause not supported",
        ))
    }

    /// Resume a paused run. Default: NOT_IMPLEMENTED.
    async fn resume_run(
        &self,
        _run_id: String,
        _user_id: String,
    ) -> Result<RunMutationRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Resume not supported",
        ))
    }

    async fn submit_run_input(
        &self,
        _run_id: String,
        _user_id: String,
        _input: RunInputData,
    ) -> Result<RunInputRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Run input not supported",
        ))
    }

    /// Drain pending tool approval requests for a run.
    ///
    /// Returns JSON objects with `request_id`, `tool`, `args` fields.
    /// The WS handler calls this during its polling loop to forward
    /// approval requests to the client.
    ///
    /// Default: no-op (returns empty vec).
    async fn drain_approval_requests(&self, _run_id: &str) -> Vec<serde_json::Value> {
        vec![]
    }

    /// Drain pending ask_user prompt requests for a run.
    ///
    /// Returns JSON objects with `request_id`, `question`, `choices`, `default`,
    /// and `context` fields. The WS handler calls this during its polling loop to
    /// forward prompts to the client.
    ///
    /// Default: no-op (returns empty vec).
    async fn drain_user_prompt_requests(&self, _run_id: &str) -> Vec<serde_json::Value> {
        vec![]
    }

    /// Drain pending tool progress events for a run.
    ///
    /// Returns JSON objects with `kind` field (`started`, `delta`, `completed`).
    /// The WS handler calls this during its polling loop to forward
    /// progress events to the client.
    ///
    /// Default: no-op (returns empty vec).
    async fn drain_progress_events(&self, _run_id: &str) -> Vec<serde_json::Value> {
        vec![]
    }

    /// Wait for in-flight background tasks to finish during graceful shutdown.
    /// Returns `true` if all tasks drained within the timeout.
    /// Default: no-op (returns true immediately).
    async fn drain_background_tasks(&self, _timeout: std::time::Duration) -> bool {
        true
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmTokenServiceConfig {
    pub url: String,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmTokenServiceRequest {
    pub url: String,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

impl From<LlmTokenServiceRequest> for LlmTokenServiceConfig {
    fn from(value: LlmTokenServiceRequest) -> Self {
        Self {
            url: value.url,
            timeout_ms: value.timeout_ms,
        }
    }
}

impl From<LlmTokenServiceConfig> for LlmTokenServiceRequest {
    fn from(value: LlmTokenServiceConfig) -> Self {
        Self {
            url: value.url,
            timeout_ms: value.timeout_ms,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionBudget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_turns: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hard_turn_limit: Option<u32>,
}

#[derive(Clone, PartialEq)]
pub struct ChatRequestData {
    pub message: String,
    pub session_id: Option<String>,
    pub full_llm_capture: bool,
    pub agent_id: Option<String>,
    pub model: Option<String>,
    pub llm_token_service: Option<LlmTokenServiceConfig>,
    pub skill_search: Option<astra_core::SkillSearchSettings>,
    pub allow_skills: Option<Vec<String>>,
    pub allow_tools: Option<Vec<String>>,
    pub context: Option<serde_json::Map<String, serde_json::Value>>,
    pub forward_headers: std::collections::HashMap<String, String>,
    pub execution_budget: Option<ExecutionBudget>,
    pub explain: bool,
    pub interactive_client: bool,
}

fn redacted_forward_header_names(headers: &std::collections::HashMap<String, String>) -> Vec<&str> {
    let mut names = headers
        .keys()
        .filter(|name| !name.starts_with("__astra_"))
        .map(String::as_str)
        .collect::<Vec<_>>();
    names.sort_unstable();
    names
}

struct RedactedForwardHeadersDebug<'a>(&'a std::collections::HashMap<String, String>);

impl std::fmt::Debug for RedactedForwardHeadersDebug<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names = redacted_forward_header_names(self.0);
        f.debug_struct("RedactedForwardHeaders")
            .field("count", &names.len())
            .field("names", &names)
            .finish()
    }
}

impl std::fmt::Debug for ChatRequestData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatRequestData")
            .field("message", &self.message)
            .field("session_id", &self.session_id)
            .field("agent_id", &self.agent_id)
            .field("model", &self.model)
            .field("llm_token_service", &self.llm_token_service)
            .field("skill_search", &self.skill_search)
            .field("allow_skills", &self.allow_skills)
            .field("allow_tools", &self.allow_tools)
            .field("context", &self.context)
            .field(
                "forward_headers",
                &RedactedForwardHeadersDebug(&self.forward_headers),
            )
            .field("execution_budget", &self.execution_budget)
            .field("explain", &self.explain)
            .field("interactive_client", &self.interactive_client)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChatRunRecord {
    pub session_id: String,
    pub run_id: String,
    pub status: String,
    pub explain: Option<serde_json::Value>,
}

#[derive(Debug)]
pub struct ChatStreamRecord {
    pub session_id: String,
    pub run_id: String,
    /// Batch events (populated after loop completes for persistence).
    pub events: Vec<serde_json::Value>,
    /// When present, SSE events are streamed incrementally through this
    /// channel. The HTTP handler converts this into a streaming response.
    pub event_rx: Option<tokio::sync::mpsc::Receiver<serde_json::Value>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunStatusRecord {
    pub run_id: String,
    pub session_id: String,
    pub status: String,
    pub waiting_for: Option<String>,
    pub events_count: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CancelRunRecord {
    pub run_id: String,
    pub status: String,
}

/// Generic record for run mutations (pause, resume, etc.).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunMutationRecord {
    pub run_id: String,
    pub status: String,
    pub previous_status: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunInputData {
    pub idempotency_key: String,
    pub input: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunInputRecord {
    pub run_id: String,
    pub accepted: bool,
    pub duplicate: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunListRecord {
    pub runs: Vec<RunStatusRecord>,
    pub total: i64,
    pub limit: u32,
    pub offset: u32,
}

// ─── Durable Run State Store ─────────────────────────────────────────────────

/// Persistent record for a durable agent run.
#[derive(Clone, Debug, PartialEq)]
pub struct DurableRunRecord {
    pub run_id: String,
    pub user_id: String,
    pub session_id: String,
    /// Parent run ID for delegation sub-runs.
    pub parent_run_id: Option<String>,
    /// Root run ID for a delegated run tree.
    pub root_run_id: Option<String>,
    /// Slash-delimited ancestor path, for deterministic subtree queries.
    pub ancestor_path: Option<String>,
    /// Depth from root run. Root run depth is 0.
    pub depth: u32,
    /// Delegation ID this run belongs to.
    pub delegation_id: Option<String>,
    /// Agent profile ID executing this run.
    pub agent_id: Option<String>,
    /// If this run is a verification-gate retry, links to the original run.
    pub retry_of: Option<String>,
    /// Retry blast radius: node, subtree, or siblings.
    pub retry_scope: Option<String>,
    pub status: String,
    pub waiting_for: Option<String>,
    pub owner_pod_id: Option<String>,
    pub owner_lease_expires_at: Option<String>,
    pub run_generation: u64,
    pub last_event_idx: i64,
    pub checkpoint_version: Option<String>,
    pub checkpoint_json: Option<String>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    /// Number of verification-gate retry attempts.
    pub retry_count: u32,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub total_tool_calls: u32,
    pub events: Vec<serde_json::Value>,
    pub created_at: String,
    pub updated_at: String,
}

/// Abstraction for durable run persistence.
///
/// Implementations:
/// - `InMemoryRunStateStore` — for tests and single-process deployments
/// - `DatabaseRunStateStore` — MatrixOne-backed persistence
#[async_trait]
pub trait RunStateStore: Send + Sync {
    /// Insert a new run record.
    async fn insert_run(&self, record: DurableRunRecord) -> Result<(), String>;

    /// Load a run by ID.
    async fn load_run(&self, run_id: &str) -> Result<Option<DurableRunRecord>, String>;

    /// Update run status and optional fields.
    async fn update_run_status(
        &self,
        run_id: &str,
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
    ) -> Result<bool, String>;

    /// Update token/tool counts.
    async fn update_run_usage(
        &self,
        run_id: &str,
        prompt_tokens: u64,
        completion_tokens: u64,
        tool_calls: u32,
    ) -> Result<bool, String>;

    /// Save checkpoint JSON for crash recovery.
    async fn save_checkpoint(&self, run_id: &str, checkpoint_json: &str) -> Result<bool, String>;

    /// Append an event to the run's event log.
    async fn append_event(&self, run_id: &str, event: serde_json::Value) -> Result<(), String>;

    /// List runs for a user with pagination.
    async fn list_user_runs(
        &self,
        user_id: &str,
        limit: u32,
        offset: u32,
    ) -> Result<(Vec<DurableRunRecord>, i64), String>;

    /// Find runs in WAITING status (for resume engine).
    async fn find_waiting_runs(&self) -> Result<Vec<DurableRunRecord>, String>;

    /// Find runs in RUNNING status (for crash recovery — mark them failed on restart).
    async fn find_running_runs(&self) -> Result<Vec<DurableRunRecord>, String>;

    /// Find all sub-runs belonging to a delegation.
    async fn find_sub_runs(&self, delegation_id: &str) -> Result<Vec<DurableRunRecord>, String>;

    /// Update the retry count for a run (verification gate retries).
    async fn update_retry_count(&self, run_id: &str, retry_count: u32) -> Result<bool, String>;
}

/// In-memory run state store for tests and single-process deployments.
pub struct InMemoryRunStateStore {
    runs: tokio::sync::RwLock<std::collections::HashMap<String, DurableRunRecord>>,
}

impl InMemoryRunStateStore {
    /// Maximum number of runs kept in memory. When exceeded, the oldest
    /// completed/failed runs are evicted on insert.
    pub const MAX_RUNS: usize = 10_000;

    pub fn new() -> Self {
        Self {
            runs: tokio::sync::RwLock::new(std::collections::HashMap::new()),
        }
    }
}

impl Default for InMemoryRunStateStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RunStateStore for InMemoryRunStateStore {
    async fn insert_run(&self, record: DurableRunRecord) -> Result<(), String> {
        let mut runs = self.runs.write().await;
        runs.insert(record.run_id.clone(), record);

        // Evict oldest completed/failed runs when over capacity
        if runs.len() > Self::MAX_RUNS {
            let terminal = ["completed", "failed", "cancelled"];
            let mut evictable: Vec<_> = runs
                .iter()
                .filter(|(_, r)| terminal.contains(&r.status.as_str()))
                .map(|(id, r)| (id.clone(), r.updated_at.clone()))
                .collect();
            evictable.sort_by(|a, b| a.1.cmp(&b.1));
            let to_remove = runs.len() - Self::MAX_RUNS;
            for (id, _) in evictable.into_iter().take(to_remove) {
                runs.remove(&id);
            }
        }
        Ok(())
    }

    async fn load_run(&self, run_id: &str) -> Result<Option<DurableRunRecord>, String> {
        let runs = self.runs.read().await;
        Ok(runs.get(run_id).cloned())
    }

    async fn update_run_status(
        &self,
        run_id: &str,
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
    ) -> Result<bool, String> {
        let mut runs = self.runs.write().await;
        if let Some(run) = runs.get_mut(run_id) {
            run.status = status.to_string();
            run.waiting_for = waiting_for.map(ToString::to_string);
            if let Some(msg) = error_message {
                run.error_message = Some(msg.to_string());
            }
            run.updated_at = chrono::Utc::now().to_rfc3339();
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn update_run_usage(
        &self,
        run_id: &str,
        prompt_tokens: u64,
        completion_tokens: u64,
        tool_calls: u32,
    ) -> Result<bool, String> {
        let mut runs = self.runs.write().await;
        if let Some(run) = runs.get_mut(run_id) {
            run.total_prompt_tokens = prompt_tokens;
            run.total_completion_tokens = completion_tokens;
            run.total_tool_calls = tool_calls;
            run.updated_at = chrono::Utc::now().to_rfc3339();
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn save_checkpoint(&self, run_id: &str, checkpoint_json: &str) -> Result<bool, String> {
        let mut runs = self.runs.write().await;
        if let Some(run) = runs.get_mut(run_id) {
            run.checkpoint_json = Some(checkpoint_json.to_string());
            run.updated_at = chrono::Utc::now().to_rfc3339();
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn append_event(&self, run_id: &str, event: serde_json::Value) -> Result<(), String> {
        let mut runs = self.runs.write().await;
        if let Some(run) = runs.get_mut(run_id) {
            run.events.push(event);
            run.updated_at = chrono::Utc::now().to_rfc3339();
        }
        Ok(())
    }

    async fn list_user_runs(
        &self,
        user_id: &str,
        limit: u32,
        offset: u32,
    ) -> Result<(Vec<DurableRunRecord>, i64), String> {
        let runs = self.runs.read().await;
        let mut user_runs: Vec<_> = runs
            .values()
            .filter(|r| r.user_id == user_id)
            .cloned()
            .collect();
        user_runs.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        let total = user_runs.len() as i64;
        let page = user_runs
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .collect();
        Ok((page, total))
    }

    async fn find_waiting_runs(&self) -> Result<Vec<DurableRunRecord>, String> {
        let runs = self.runs.read().await;
        Ok(runs
            .values()
            .filter(|r| r.status == "waiting")
            .cloned()
            .collect())
    }

    async fn find_running_runs(&self) -> Result<Vec<DurableRunRecord>, String> {
        let runs = self.runs.read().await;
        Ok(runs
            .values()
            .filter(|r| r.status == "running")
            .cloned()
            .collect())
    }

    async fn find_sub_runs(&self, delegation_id: &str) -> Result<Vec<DurableRunRecord>, String> {
        let runs = self.runs.read().await;
        Ok(runs
            .values()
            .filter(|r| r.delegation_id.as_deref() == Some(delegation_id))
            .cloned()
            .collect())
    }

    async fn update_retry_count(&self, run_id: &str, retry_count: u32) -> Result<bool, String> {
        let mut runs = self.runs.write().await;
        if let Some(run) = runs.get_mut(run_id) {
            run.retry_count = retry_count;
            run.updated_at = chrono::Utc::now().to_rfc3339();
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

// ─── MatrixOne-backed run state store ───────────────────────────────────────

const DEFAULT_RETRY_SCOPE: &str = "node";
const MAX_TOOL_OUTPUT_BATCH_ROWS: usize = 500;
const MAX_TOOL_OUTPUT_BATCH_BYTES: usize = 16 * 1024 * 1024;
const FALLBACK_PREVIEW_BYTES: usize = 400;

#[derive(Clone, Debug)]
struct ToolPreviewContract {
    max_preview_bytes: usize,
    normalize_version: String,
    found: bool,
}

#[derive(Clone, Debug)]
struct ToolOutputPreviewRow {
    payload: String,
    preview_text: String,
    preview_status: String,
    artifact_ref: Option<String>,
    content_hash: String,
    normalize_version: String,
    parent_output_id: Option<String>,
}

#[derive(Debug, Error)]
pub enum DatabaseRunStateStoreError {
    #[error("database operation failed: operation={operation}, entity={entity}, source={source}")]
    Database {
        operation: &'static str,
        entity: String,
        #[source]
        source: sqlx::Error,
    },
    #[error("invalid retry_scope for run {run_id}: {retry_scope}")]
    InvalidRetryScope { run_id: String, retry_scope: String },
    #[error("invalid checkpoint_v1 for run {run_id}: {reason}")]
    InvalidCheckpoint { run_id: String, reason: String },
    #[error("tool output batch too large: run_id={run_id}, rows={rows}, bytes={bytes}")]
    ToolOutputBatchTooLarge {
        run_id: String,
        rows: usize,
        bytes: usize,
    },
    #[error("JSON serialization failed: operation={operation}, entity={entity}, source={source}")]
    Json {
        operation: &'static str,
        entity: String,
        #[source]
        source: serde_json::Error,
    },
}

type DbStoreResult<T> = Result<T, DatabaseRunStateStoreError>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutputBatchItem {
    pub output_id: String,
    pub tool_call_id: Option<String>,
    pub tool_name: String,
    pub output_json: serde_json::Value,
}

/// MatrixOne durable run store.
///
/// Events are append-only in `agent_run_events`; `run_counters` owns event_idx
/// allocation so reconnect and replay never scan `MAX(event_idx)`.
#[derive(Clone)]
pub struct DatabaseRunStateStore {
    pool: SharedPool,
    owner_pod_id: String,
    lease_ttl: Duration,
}

impl DatabaseRunStateStore {
    pub const DEFAULT_LEASE_TTL: Duration = Duration::from_secs(45);

    pub fn new(pool: SharedPool) -> Self {
        Self {
            pool,
            owner_pod_id: default_owner_pod_id(),
            lease_ttl: Self::DEFAULT_LEASE_TTL,
        }
    }

    pub fn with_owner_pod_id(mut self, owner_pod_id: impl Into<String>) -> Self {
        self.owner_pod_id = owner_pod_id.into();
        self
    }

    pub fn with_lease_ttl(mut self, lease_ttl: Duration) -> Self {
        self.lease_ttl = lease_ttl;
        self
    }

    pub fn owner_pod_id(&self) -> &str {
        &self.owner_pod_id
    }

    async fn load_tool_preview_contracts(
        &self,
        items: &[ToolOutputBatchItem],
    ) -> DbStoreResult<HashMap<String, ToolPreviewContract>> {
        let tool_names = items
            .iter()
            .map(|item| item.tool_name.clone())
            .collect::<HashSet<_>>();
        let mut contracts = tool_names
            .iter()
            .map(|tool_name| {
                (
                    tool_name.clone(),
                    ToolPreviewContract {
                        max_preview_bytes: FALLBACK_PREVIEW_BYTES,
                        normalize_version: "raw_v1".to_string(),
                        found: false,
                    },
                )
            })
            .collect::<HashMap<_, _>>();
        if tool_names.is_empty() {
            return Ok(contracts);
        }

        let mut builder = sqlx::QueryBuilder::<sqlx::MySql>::new(
            "SELECT tool_name, max_preview_bytes, normalize_version
             FROM preview_template_registry
             WHERE status = 'active' AND tool_name IN (",
        );
        let mut separated = builder.separated(", ");
        for tool_name in &tool_names {
            separated.push_bind(tool_name);
        }
        separated.push_unseparated(")");
        let rows = builder
            .build()
            .fetch_all(self.pool.get())
            .await
            .map_err(|source| {
                db_error("load_tool_preview_contracts", "preview_templates", source)
            })?;
        for row in rows {
            let tool_name = row.try_get::<String, _>("tool_name").unwrap_or_default();
            let max_preview_bytes = row
                .try_get::<i64, _>("max_preview_bytes")
                .unwrap_or(FALLBACK_PREVIEW_BYTES as i64)
                .max(1) as usize;
            let normalize_version = row
                .try_get::<String, _>("normalize_version")
                .unwrap_or_else(|_| "raw_v1".to_string());
            contracts.insert(
                tool_name,
                ToolPreviewContract {
                    max_preview_bytes,
                    normalize_version,
                    found: true,
                },
            );
        }
        Ok(contracts)
    }

    async fn record_preview_template_missing_for_tools(
        &self,
        session_id: &str,
        run_id: &str,
        user_id: &str,
        contracts: &HashMap<String, ToolPreviewContract>,
    ) -> DbStoreResult<()> {
        let missing = contracts
            .iter()
            .filter_map(|(tool_name, contract)| (!contract.found).then_some(tool_name.clone()))
            .collect::<Vec<_>>();
        if missing.is_empty() {
            return Ok(());
        }
        let mut builder = sqlx::QueryBuilder::<sqlx::MySql>::new(
            "INSERT INTO agent_events
             (event_id, session_id, user_id, event_type, content, metadata, meta_tool_name, created_at) ",
        );
        builder.push_values(missing.iter(), |mut row, tool_name| {
            row.push_bind(Uuid::new_v4().to_string())
                .push_bind(session_id)
                .push_bind(user_id)
                .push_bind("preview_template_missing")
                .push_bind(tool_name)
                .push_bind(
                    serde_json::json!({
                        "run_id": run_id,
                        "tool_name": tool_name,
                        "fallback_max_preview_bytes": FALLBACK_PREVIEW_BYTES,
                    })
                    .to_string(),
                )
                .push_bind(tool_name)
                .push("NOW(6)");
        });
        builder
            .build()
            .execute(self.pool.get())
            .await
            .map_err(|source| {
                db_error("record_preview_template_missing_for_tools", run_id, source)
            })?;
        Ok(())
    }

    pub async fn acquire_owner_lease(
        &self,
        run_id: &str,
        owner_pod_id: &str,
        ttl: Duration,
    ) -> DbStoreResult<bool> {
        let lease_expires_at = chrono::Utc::now()
            + chrono::Duration::from_std(ttl).unwrap_or_else(|_| chrono::Duration::seconds(45));
        let result = sqlx::query(
            "UPDATE run_counters
             SET owner_pod_id = ?,
                 owner_lease_expires_at = ?,
                 run_generation = run_generation + 1,
                 updated_at = NOW(6)
             WHERE run_id = ?
               AND (owner_pod_id IS NULL OR owner_pod_id = ? OR owner_lease_expires_at < NOW(6))",
        )
        .bind(owner_pod_id)
        .bind(lease_expires_at.naive_utc())
        .bind(run_id)
        .bind(owner_pod_id)
        .execute(self.pool.get())
        .await
        .map_err(|source| db_error("acquire_owner_lease", run_id, source))?;

        if result.rows_affected() == 0 {
            return Ok(false);
        }

        sqlx::query(
            "UPDATE agent_runs
             SET owner_pod_id = ?,
                 owner_lease_expires_at = ?,
                 run_generation = run_generation + 1,
                 updated_at = NOW(6)
             WHERE run_id = ?",
        )
        .bind(owner_pod_id)
        .bind(lease_expires_at.naive_utc())
        .bind(run_id)
        .execute(self.pool.get())
        .await
        .map_err(|source| db_error("sync_owner_lease_to_run", run_id, source))?;

        Ok(true)
    }

    pub async fn insert_tool_output_batch(
        &self,
        batch_id: &str,
        session_id: &str,
        run_id: &str,
        user_id: &str,
        items: &[ToolOutputBatchItem],
    ) -> DbStoreResult<()> {
        let payloads = items
            .iter()
            .map(|item| {
                serde_json::to_string(&item.output_json).map_err(|source| {
                    DatabaseRunStateStoreError::Json {
                        operation: "serialize_tool_output",
                        entity: item.output_id.clone(),
                        source,
                    }
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let payload_bytes = payloads.iter().map(String::len).sum::<usize>();
        if items.len() > MAX_TOOL_OUTPUT_BATCH_ROWS || payload_bytes > MAX_TOOL_OUTPUT_BATCH_BYTES {
            return Err(DatabaseRunStateStoreError::ToolOutputBatchTooLarge {
                run_id: run_id.to_string(),
                rows: items.len(),
                bytes: payload_bytes,
            });
        }
        let preview_contracts = self.load_tool_preview_contracts(items).await?;
        self.record_preview_template_missing_for_tools(
            session_id,
            run_id,
            user_id,
            &preview_contracts,
        )
        .await?;
        let preview_rows = items
            .iter()
            .zip(payloads.iter())
            .map(|(item, payload)| {
                let contract = preview_contracts.get(&item.tool_name).cloned().unwrap_or(
                    ToolPreviewContract {
                        max_preview_bytes: FALLBACK_PREVIEW_BYTES,
                        normalize_version: "raw_v1".to_string(),
                        found: false,
                    },
                );
                build_tool_output_preview_row(session_id, item, payload, &contract)
            })
            .collect::<Vec<_>>();

        let mut tx = self
            .pool
            .get()
            .begin()
            .await
            .map_err(|source| db_error("begin_tool_output_batch", run_id, source))?;

        sqlx::query(
            "INSERT INTO session_tool_output_batches
             (batch_id, session_id, run_id, user_id, output_count, payload_bytes, status, created_at)
             VALUES (?, ?, ?, ?, ?, ?, 'committed', NOW(6))
             ON DUPLICATE KEY UPDATE output_count = output_count",
        )
        .bind(batch_id)
        .bind(session_id)
        .bind(run_id)
        .bind(user_id)
        .bind(items.len() as i64)
        .bind(payload_bytes as i64)
        .execute(&mut *tx)
        .await
        .map_err(|source| db_error("insert_tool_output_batch", batch_id, source))?;

        if !items.is_empty() {
            let mut builder = sqlx::QueryBuilder::<sqlx::MySql>::new(
                "INSERT INTO session_tool_outputs
                 (output_id, batch_id, session_id, run_id, user_id, output_idx,
                  parent_output_id, tool_call_id, tool_name, output_json, payload_bytes,
                  preview_text, preview_status, artifact_ref, content_hash, normalize_version,
                  created_at) ",
            );
            builder.push_values(
                items.iter().zip(preview_rows.iter()).enumerate(),
                |mut row, (idx, (item, preview))| {
                    row.push_bind(&item.output_id)
                        .push_bind(batch_id)
                        .push_bind(session_id)
                        .push_bind(run_id)
                        .push_bind(user_id)
                        .push_bind(idx as i64)
                        .push_bind(&preview.parent_output_id)
                        .push_bind(&item.tool_call_id)
                        .push_bind(&item.tool_name)
                        .push_bind(&preview.payload)
                        .push_bind(preview.payload.len() as i64)
                        .push_bind(&preview.preview_text)
                        .push_bind(&preview.preview_status)
                        .push_bind(&preview.artifact_ref)
                        .push_bind(&preview.content_hash)
                        .push_bind(&preview.normalize_version)
                        .push("NOW(6)");
                },
            );
            builder.push(" ON DUPLICATE KEY UPDATE payload_bytes = payload_bytes");
            builder
                .build()
                .execute(&mut *tx)
                .await
                .map_err(|source| db_error("insert_tool_outputs_batch_rows", batch_id, source))?;
        }

        tx.commit()
            .await
            .map_err(|source| db_error("commit_tool_output_batch", batch_id, source))?;
        Ok(())
    }

    async fn append_event_inner(
        &self,
        run_id: &str,
        event: serde_json::Value,
    ) -> DbStoreResult<()> {
        let idempotency_key = extract_optional_string(&event, "idempotency_key");
        if let Some(key) = idempotency_key.as_deref() {
            let existing = sqlx::query(
                "SELECT id FROM agent_run_events WHERE run_id = ? AND idempotency_key = ? LIMIT 1",
            )
            .bind(run_id)
            .bind(key)
            .fetch_optional(self.pool.get())
            .await
            .map_err(|source| db_error("lookup_run_event_idempotency", run_id, source))?;
            if existing.is_some() {
                return Ok(());
            }
        }

        let run = self
            .load_run_metadata(run_id)
            .await?
            .ok_or_else(|| db_error("load_run_for_append", run_id, sqlx::Error::RowNotFound))?;
        let payload_json =
            serde_json::to_string(&event).map_err(|source| DatabaseRunStateStoreError::Json {
                operation: "serialize_run_event",
                entity: run_id.to_string(),
                source,
            })?;
        let event_type = extract_event_type(&event);
        let event_id = extract_optional_string(&event, "event_id")
            .or_else(|| extract_optional_string(&event, "id"))
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let event_hash = sha256_hex(payload_json.as_bytes());

        let event_idx = self.allocate_event_idx(run_id).await?;

        let insert = sqlx::query(
            "INSERT INTO agent_run_events
             (id, run_id, event_idx, user_id, session_id, event_type, event_id, agent_id,
              idempotency_key, event_hash, producer_pod_id, payload_json, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6))",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(run_id)
        .bind(event_idx)
        .bind(&run.user_id)
        .bind(&run.session_id)
        .bind(event_type)
        .bind(event_id)
        .bind(&run.agent_id)
        .bind(idempotency_key)
        .bind(event_hash)
        .bind(&self.owner_pod_id)
        .bind(payload_json)
        .execute(self.pool.get())
        .await;

        if let Err(source) = insert {
            let msg = source.to_string();
            if msg.contains("uq_run_event_idempotency") || msg.contains("Duplicate") {
                return Ok(());
            }
            return Err(db_error("insert_run_event", run_id, source));
        }

        sqlx::query(
            "UPDATE agent_runs SET last_event_idx = ?, updated_at = NOW(6) WHERE run_id = ?",
        )
        .bind(event_idx)
        .bind(run_id)
        .execute(self.pool.get())
        .await
        .map_err(|source| db_error("update_run_last_event_idx", run_id, source))?;

        Ok(())
    }

    async fn load_run_metadata(&self, run_id: &str) -> DbStoreResult<Option<DurableRunRecord>> {
        let row = sqlx::query("SELECT * FROM agent_runs WHERE run_id = ?")
            .bind(run_id)
            .fetch_optional(self.pool.get())
            .await
            .map_err(|source| db_error("load_run_metadata", run_id, source))?;
        row.map(run_record_from_row).transpose()
    }

    async fn allocate_event_idx(&self, run_id: &str) -> DbStoreResult<i64> {
        for _ in 0..32 {
            let row = sqlx::query("SELECT next_event_idx FROM run_counters WHERE run_id = ?")
                .bind(run_id)
                .fetch_one(self.pool.get())
                .await
                .map_err(|source| db_error("select_run_counter", run_id, source))?;
            let next = row.try_get::<i64, _>("next_event_idx").unwrap_or(0);
            let result = sqlx::query(
                "UPDATE run_counters
                 SET next_event_idx = next_event_idx + 1, updated_at = NOW(6)
                 WHERE run_id = ? AND next_event_idx = ?",
            )
            .bind(run_id)
            .bind(next)
            .execute(self.pool.get())
            .await
            .map_err(|source| db_error("cas_increment_run_counter", run_id, source))?;
            if result.rows_affected() == 1 {
                return Ok(next);
            }
            tokio::task::yield_now().await;
        }
        Err(db_error(
            "allocate_event_idx",
            run_id,
            sqlx::Error::Protocol("run counter CAS exhausted".to_string()),
        ))
    }
}

#[async_trait]
impl RunStateStore for DatabaseRunStateStore {
    async fn insert_run(&self, mut record: DurableRunRecord) -> Result<(), String> {
        if (record.root_run_id.is_none() || record.ancestor_path.is_none())
            && let Some(parent_run_id) = record.parent_run_id.as_deref()
            && let Some(parent) = self
                .load_run_metadata(parent_run_id)
                .await
                .map_err(|e| e.to_string())?
        {
            let parent_root = parent.root_run_id.unwrap_or(parent.run_id.clone());
            let parent_path = parent.ancestor_path.unwrap_or(parent.run_id);
            record.root_run_id.get_or_insert(parent_root);
            record
                .ancestor_path
                .get_or_insert_with(|| format!("{parent_path}/{}", record.run_id));
            if record.depth == 0 {
                record.depth = parent.depth.saturating_add(1);
            }
        }
        record
            .root_run_id
            .get_or_insert_with(|| record.run_id.clone());
        record
            .ancestor_path
            .get_or_insert_with(|| record.run_id.clone());
        let retry_scope = record
            .retry_scope
            .as_deref()
            .unwrap_or(DEFAULT_RETRY_SCOPE)
            .to_string();
        validate_retry_scope(&record.run_id, &retry_scope).map_err(|e| e.to_string())?;

        let lease_expires_at = chrono::Utc::now()
            + chrono::Duration::from_std(self.lease_ttl)
                .unwrap_or_else(|_| chrono::Duration::seconds(45));
        let events = std::mem::take(&mut record.events);

        sqlx::query(
            "INSERT INTO agent_runs
             (run_id, user_id, session_id, parent_run_id, root_run_id, ancestor_path, depth,
              delegation_id, agent_id, retry_of, retry_scope, status, waiting_for,
              owner_pod_id, owner_lease_expires_at, run_generation, last_event_idx,
              checkpoint_version, checkpoint_json, error_code, error_message, retry_count,
              total_prompt_tokens, total_completion_tokens, total_tool_calls, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6), NOW(6))
             ON DUPLICATE KEY UPDATE updated_at = NOW(6)",
        )
        .bind(&record.run_id)
        .bind(&record.user_id)
        .bind(&record.session_id)
        .bind(&record.parent_run_id)
        .bind(record.root_run_id.as_deref().unwrap_or(&record.run_id))
        .bind(record.ancestor_path.as_deref().unwrap_or(&record.run_id))
        .bind(record.depth as i64)
        .bind(&record.delegation_id)
        .bind(&record.agent_id)
        .bind(&record.retry_of)
        .bind(retry_scope)
        .bind(&record.status)
        .bind(&record.waiting_for)
        .bind(&self.owner_pod_id)
        .bind(lease_expires_at.naive_utc())
        .bind(record.run_generation as i64)
        .bind(record.last_event_idx)
        .bind(&record.checkpoint_version)
        .bind(&record.checkpoint_json)
        .bind(&record.error_code)
        .bind(&record.error_message)
        .bind(record.retry_count as i64)
        .bind(record.total_prompt_tokens as i64)
        .bind(record.total_completion_tokens as i64)
        .bind(record.total_tool_calls as i64)
        .execute(self.pool.get())
        .await
        .map_err(|source| db_error("insert_run", &record.run_id, source).to_string())?;

        sqlx::query(
            "INSERT INTO run_counters
             (run_id, next_event_idx, owner_pod_id, owner_lease_expires_at, run_generation, created_at, updated_at)
             VALUES (?, 0, ?, ?, ?, NOW(6), NOW(6))
             ON DUPLICATE KEY UPDATE updated_at = NOW(6)",
        )
        .bind(&record.run_id)
        .bind(&self.owner_pod_id)
        .bind(lease_expires_at.naive_utc())
        .bind(record.run_generation as i64)
        .execute(self.pool.get())
        .await
        .map_err(|source| db_error("insert_run_counter", &record.run_id, source).to_string())?;

        for event in events {
            self.append_event_inner(&record.run_id, event)
                .await
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    async fn load_run(&self, run_id: &str) -> Result<Option<DurableRunRecord>, String> {
        let Some(mut run) = self
            .load_run_metadata(run_id)
            .await
            .map_err(|e| e.to_string())?
        else {
            return Ok(None);
        };

        let rows = sqlx::query(
            "SELECT payload_json, event_idx FROM agent_run_events WHERE run_id = ? ORDER BY event_idx ASC",
        )
        .bind(run_id)
        .fetch_all(self.pool.get())
        .await
        .map_err(|source| db_error("load_run_events", run_id, source).to_string())?;

        run.events = rows
            .into_iter()
            .filter_map(|row| {
                let payload = row.try_get::<String, _>("payload_json").ok()?;
                let mut value = serde_json::from_str::<serde_json::Value>(&payload).ok()?;
                if let Some(obj) = value.as_object_mut()
                    && !obj.contains_key("index")
                    && let Ok(event_idx) = row.try_get::<i64, _>("event_idx")
                {
                    obj.insert("index".to_string(), serde_json::json!(event_idx));
                }
                Some(value)
            })
            .collect();
        Ok(Some(run))
    }

    async fn update_run_status(
        &self,
        run_id: &str,
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
    ) -> Result<bool, String> {
        let result = sqlx::query(
            "UPDATE agent_runs
             SET status = ?, waiting_for = ?, error_message = COALESCE(?, error_message), updated_at = NOW(6)
             WHERE run_id = ?",
        )
        .bind(status)
        .bind(waiting_for)
        .bind(error_message)
        .bind(run_id)
        .execute(self.pool.get())
        .await
        .map_err(|source| db_error("update_run_status", run_id, source).to_string())?;
        Ok(result.rows_affected() > 0)
    }

    async fn update_run_usage(
        &self,
        run_id: &str,
        prompt_tokens: u64,
        completion_tokens: u64,
        tool_calls: u32,
    ) -> Result<bool, String> {
        let result = sqlx::query(
            "UPDATE agent_runs
             SET total_prompt_tokens = ?, total_completion_tokens = ?, total_tool_calls = ?, updated_at = NOW(6)
             WHERE run_id = ?",
        )
        .bind(prompt_tokens as i64)
        .bind(completion_tokens as i64)
        .bind(tool_calls as i64)
        .bind(run_id)
        .execute(self.pool.get())
        .await
        .map_err(|source| db_error("update_run_usage", run_id, source).to_string())?;
        Ok(result.rows_affected() > 0)
    }

    async fn save_checkpoint(&self, run_id: &str, checkpoint_json: &str) -> Result<bool, String> {
        validate_checkpoint_v1(run_id, checkpoint_json).map_err(|e| e.to_string())?;
        let result = sqlx::query(
            "UPDATE agent_runs
             SET checkpoint_version = 'checkpoint_v1', checkpoint_json = ?, updated_at = NOW(6)
             WHERE run_id = ?",
        )
        .bind(checkpoint_json)
        .bind(run_id)
        .execute(self.pool.get())
        .await
        .map_err(|source| db_error("save_checkpoint", run_id, source).to_string())?;
        Ok(result.rows_affected() > 0)
    }

    async fn append_event(&self, run_id: &str, event: serde_json::Value) -> Result<(), String> {
        self.append_event_inner(run_id, event)
            .await
            .map_err(|e| e.to_string())
    }

    async fn list_user_runs(
        &self,
        user_id: &str,
        limit: u32,
        offset: u32,
    ) -> Result<(Vec<DurableRunRecord>, i64), String> {
        let total_row = sqlx::query("SELECT COUNT(*) AS total FROM agent_runs WHERE user_id = ?")
            .bind(user_id)
            .fetch_one(self.pool.get())
            .await
            .map_err(|source| db_error("count_user_runs", user_id, source).to_string())?;
        let total = total_row.try_get::<i64, _>("total").unwrap_or(0);
        let rows = sqlx::query(
            "SELECT * FROM agent_runs WHERE user_id = ? ORDER BY updated_at DESC LIMIT ? OFFSET ?",
        )
        .bind(user_id)
        .bind(limit as i64)
        .bind(offset as i64)
        .fetch_all(self.pool.get())
        .await
        .map_err(|source| db_error("list_user_runs", user_id, source).to_string())?;
        let runs = rows
            .into_iter()
            .map(run_record_from_row)
            .collect::<DbStoreResult<Vec<_>>>()
            .map_err(|e| e.to_string())?;
        Ok((runs, total))
    }

    async fn find_waiting_runs(&self) -> Result<Vec<DurableRunRecord>, String> {
        self.find_runs_by_status("waiting").await
    }

    async fn find_running_runs(&self) -> Result<Vec<DurableRunRecord>, String> {
        self.find_runs_by_status("running").await
    }

    async fn find_sub_runs(&self, delegation_id: &str) -> Result<Vec<DurableRunRecord>, String> {
        let rows = sqlx::query(
            "SELECT * FROM agent_runs WHERE delegation_id = ? ORDER BY depth ASC, created_at ASC",
        )
        .bind(delegation_id)
        .fetch_all(self.pool.get())
        .await
        .map_err(|source| db_error("find_sub_runs", delegation_id, source).to_string())?;
        rows.into_iter()
            .map(run_record_from_row)
            .collect::<DbStoreResult<Vec<_>>>()
            .map_err(|e| e.to_string())
    }

    async fn update_retry_count(&self, run_id: &str, retry_count: u32) -> Result<bool, String> {
        let result = sqlx::query(
            "UPDATE agent_runs SET retry_count = ?, updated_at = NOW(6) WHERE run_id = ?",
        )
        .bind(retry_count as i64)
        .bind(run_id)
        .execute(self.pool.get())
        .await
        .map_err(|source| db_error("update_retry_count", run_id, source).to_string())?;
        Ok(result.rows_affected() > 0)
    }
}

impl DatabaseRunStateStore {
    async fn find_runs_by_status(&self, status: &str) -> Result<Vec<DurableRunRecord>, String> {
        let rows = sqlx::query("SELECT * FROM agent_runs WHERE status = ? ORDER BY updated_at ASC")
            .bind(status)
            .fetch_all(self.pool.get())
            .await
            .map_err(|source| db_error("find_runs_by_status", status, source).to_string())?;
        rows.into_iter()
            .map(run_record_from_row)
            .collect::<DbStoreResult<Vec<_>>>()
            .map_err(|e| e.to_string())
    }
}

fn db_error(
    operation: &'static str,
    entity: impl Into<String>,
    source: sqlx::Error,
) -> DatabaseRunStateStoreError {
    DatabaseRunStateStoreError::Database {
        operation,
        entity: entity.into(),
        source,
    }
}

fn default_owner_pod_id() -> String {
    std::env::var("ASTRA_POD_ID")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| format!("astra-runtime-{}", Uuid::new_v4()))
}

fn validate_retry_scope(run_id: &str, retry_scope: &str) -> DbStoreResult<()> {
    match retry_scope {
        "node" | "subtree" | "siblings" => Ok(()),
        other => Err(DatabaseRunStateStoreError::InvalidRetryScope {
            run_id: run_id.to_string(),
            retry_scope: other.to_string(),
        }),
    }
}

fn validate_checkpoint_v1(run_id: &str, checkpoint_json: &str) -> DbStoreResult<()> {
    let value: serde_json::Value = serde_json::from_str(checkpoint_json).map_err(|source| {
        DatabaseRunStateStoreError::Json {
            operation: "parse_checkpoint",
            entity: run_id.to_string(),
            source,
        }
    })?;
    let object =
        value
            .as_object()
            .ok_or_else(|| DatabaseRunStateStoreError::InvalidCheckpoint {
                run_id: run_id.to_string(),
                reason: "checkpoint root must be an object".to_string(),
            })?;
    if object.get("version").and_then(serde_json::Value::as_str) != Some("checkpoint_v1") {
        return Err(DatabaseRunStateStoreError::InvalidCheckpoint {
            run_id: run_id.to_string(),
            reason: "version must be checkpoint_v1".to_string(),
        });
    }
    if !matches!(object.get("graceful"), Some(serde_json::Value::Bool(_))) {
        return Err(DatabaseRunStateStoreError::InvalidCheckpoint {
            run_id: run_id.to_string(),
            reason: "graceful must be boolean".to_string(),
        });
    }
    if object
        .get("last_batch_id")
        .and_then(serde_json::Value::as_str)
        .is_none()
    {
        return Err(DatabaseRunStateStoreError::InvalidCheckpoint {
            run_id: run_id.to_string(),
            reason: "last_batch_id must be string".to_string(),
        });
    }
    let extra = object.get("extra").and_then(serde_json::Value::as_object);
    if extra.is_none() {
        return Err(DatabaseRunStateStoreError::InvalidCheckpoint {
            run_id: run_id.to_string(),
            reason: "extra must be object".to_string(),
        });
    }
    if let Some(partial) = extra
        .and_then(|m| m.get("partial_progress"))
        .and_then(serde_json::Value::as_object)
    {
        for key in ["step_index", "total_steps", "resumable_marker"] {
            if !partial.contains_key(key) {
                return Err(DatabaseRunStateStoreError::InvalidCheckpoint {
                    run_id: run_id.to_string(),
                    reason: format!("extra.partial_progress.{key} is required"),
                });
            }
        }
    }
    Ok(())
}

fn build_tool_output_preview_row(
    session_id: &str,
    item: &ToolOutputBatchItem,
    payload: &str,
    contract: &ToolPreviewContract,
) -> ToolOutputPreviewRow {
    let content_hash = format!("sha256:{}", sha256_hex(payload.as_bytes()));
    let preview_text = truncate_utf8_bytes(payload, contract.max_preview_bytes);
    let explicit_artifact_ref = extract_optional_string(&item.output_json, "artifact_ref")
        .or_else(|| extract_optional_string(&item.output_json, "artifact_uri"));
    let large_payload_ref = (payload.len() > contract.max_preview_bytes).then(|| {
        format!(
            "tool_output://{session_id}/{}@{}",
            item.output_id, content_hash
        )
    });
    let preview_status = if !contract.found {
        "fallback"
    } else if payload.len() > contract.max_preview_bytes {
        "truncated"
    } else {
        "template"
    }
    .to_string();
    ToolOutputPreviewRow {
        payload: payload.to_string(),
        preview_text,
        preview_status,
        artifact_ref: explicit_artifact_ref.or(large_payload_ref),
        content_hash,
        normalize_version: contract.normalize_version.clone(),
        parent_output_id: extract_optional_string(&item.output_json, "parent_output_id"),
    }
}

fn truncate_utf8_bytes(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes.min(value.len());
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    value[..end].to_string()
}

fn run_record_from_row(row: sqlx::mysql::MySqlRow) -> DbStoreResult<DurableRunRecord> {
    let run_id = row
        .try_get::<String, _>("run_id")
        .map_err(|source| db_error("decode_run_row.run_id", "agent_runs".to_string(), source))?;
    Ok(DurableRunRecord {
        user_id: row.try_get("user_id").unwrap_or_default(),
        session_id: row.try_get("session_id").unwrap_or_default(),
        parent_run_id: row.try_get("parent_run_id").ok(),
        root_run_id: row.try_get("root_run_id").ok(),
        ancestor_path: row.try_get("ancestor_path").ok(),
        depth: row.try_get::<i64, _>("depth").unwrap_or(0).max(0) as u32,
        delegation_id: row.try_get("delegation_id").ok(),
        agent_id: row.try_get("agent_id").ok(),
        retry_of: row.try_get("retry_of").ok(),
        retry_scope: row.try_get("retry_scope").ok(),
        status: row.try_get("status").unwrap_or_default(),
        waiting_for: row.try_get("waiting_for").ok(),
        owner_pod_id: row.try_get("owner_pod_id").ok(),
        owner_lease_expires_at: datetime_string(&row, "owner_lease_expires_at"),
        run_generation: row.try_get::<i64, _>("run_generation").unwrap_or(0).max(0) as u64,
        last_event_idx: row.try_get::<i64, _>("last_event_idx").unwrap_or(-1),
        checkpoint_version: row.try_get("checkpoint_version").ok(),
        checkpoint_json: row.try_get("checkpoint_json").ok(),
        error_code: row.try_get("error_code").ok(),
        error_message: row.try_get("error_message").ok(),
        retry_count: row.try_get::<i64, _>("retry_count").unwrap_or(0).max(0) as u32,
        total_prompt_tokens: row
            .try_get::<i64, _>("total_prompt_tokens")
            .unwrap_or(0)
            .max(0) as u64,
        total_completion_tokens: row
            .try_get::<i64, _>("total_completion_tokens")
            .unwrap_or(0)
            .max(0) as u64,
        total_tool_calls: row
            .try_get::<i64, _>("total_tool_calls")
            .unwrap_or(0)
            .max(0) as u32,
        events: Vec::new(),
        created_at: datetime_string(&row, "created_at").unwrap_or_default(),
        updated_at: datetime_string(&row, "updated_at").unwrap_or_default(),
        run_id,
    })
}

fn datetime_string(row: &sqlx::mysql::MySqlRow, column: &str) -> Option<String> {
    row.try_get::<chrono::NaiveDateTime, _>(column)
        .ok()
        .map(|dt| dt.to_string())
        .or_else(|| row.try_get::<String, _>(column).ok())
}

fn extract_event_type(event: &serde_json::Value) -> String {
    extract_optional_string(event, "event_type")
        .or_else(|| extract_optional_string(event, "type"))
        .unwrap_or_else(|| "unknown".to_string())
}

fn extract_optional_string(event: &serde_json::Value, key: &str) -> Option<String> {
    event
        .get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("{digest:x}")
}

/// Known client-facing event `type` values that are safe to surface
/// on the external `/chat/turn` SSE stream. Anything outside this
/// allowlist is dropped — it's either an internal diagnostic event
/// (e.g. `injection_freshness`) whose stability no external
/// consumer should depend on, or a future event that hasn't been
/// explicitly opted into the public surface.
///
/// wip-7 motivation: the pre-allowlist code path pass-through'd any
/// `{"type": ...}`-shaped event unchanged. wip-5's
/// `injection_freshness` event carried raw channel text for
/// observation purposes — that text leaked to any authenticated
/// `/chat/turn` client. The wip-7 bridge emits fingerprints only,
/// but the allowlist is the defence-in-depth so a future diagnostic
/// event can't accidentally leak either.
///
/// Adding a new public event: add its `type` string here AND, if the
/// event also arrives in `{"event_type": ..., "data": ...}` shape
/// from the journal, add a dedicated `match` arm below so the
/// transform shapes it consistently.
const EXTERNAL_CLIENT_ALLOWLIST: &[&str] = &[
    // Streaming assistant content.
    "text_delta",
    "text_done",
    "thinking_delta",
    "thinking_done",
    "reasoning_delta",
    "reasoning_done",
    "reasoning_message_content",
    // Tool-call lifecycle.
    "tool_call",
    "tool_call_start",
    "tool_call_end",
    "tool_request",
    "approval_required",
    "approval_batch_required",
    // Run lifecycle + framing.
    "run_started",
    "run_finished",
    "run_paused",
    "run_resumed",
    "context_meta",
    "session_info",
    "turn_complete",
    "turn_done",
    "user_input",
    "usage",
    "explain",
    "error",
    "ping",
    // Plan / delegation surface (public admin features).
    "plan_created",
    "plan_step_start",
    "plan_step_done",
    "plan_revised",
    "agent_delegated",
    "agent_spawned",
    "agent_progress",
    "agent_completed",
];

pub fn transform_run_event_for_client(event: serde_json::Value) -> serde_json::Value {
    // Already-client-ready shape (`{"type": ..., ...}`): allowlist the
    // `type` value. Drop anything not explicitly safe for external
    // consumption.
    if event
        .get("event_type")
        .and_then(serde_json::Value::as_str)
        .is_none()
        && let Some(client_type) = event.get("type").and_then(serde_json::Value::as_str)
    {
        if EXTERNAL_CLIENT_ALLOWLIST.contains(&client_type) {
            return event;
        }
        // Unknown / internal event — drop.
        return serde_json::Value::Null;
    }

    let event_type = event
        .get("event_type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let data = event
        .get("data")
        .and_then(serde_json::Value::as_object)
        .cloned()
        .unwrap_or_default();

    match event_type {
        "text_delta" => serde_json::json!({
            "type": "text_delta",
            "content": data.get("chunk").cloned().unwrap_or(serde_json::Value::String(String::new())),
        }),
        "assistant_delta" => serde_json::json!({
            "type": "text_delta",
            "content": data
                .get("text")
                .or_else(|| data.get("chunk"))
                .cloned()
                .unwrap_or(serde_json::Value::String(String::new())),
        }),
        "text_done" => serde_json::json!({
            "type": "text_done",
            "full_text": data.get("full_text").cloned().unwrap_or(serde_json::Value::String(String::new())),
        }),
        "reasoning_message_content" => serde_json::json!({
            "type": "reasoning_message_content",
            "content": data.get("content").cloned().unwrap_or(serde_json::Value::String(String::new())),
        }),
        "thinking_delta" => serde_json::json!({
            "type": "thinking_delta",
            "content": data.get("chunk").cloned().unwrap_or(serde_json::Value::String(String::new())),
        }),
        "thinking_done" | "reasoning_done" => serde_json::json!({ "type": event_type }),
        "tool_call_start" => serde_json::json!({
            "type": "tool_call_start",
            "tool": data.get("tool").cloned().unwrap_or(serde_json::Value::String(String::new())),
            "call_id": data.get("call_id").cloned().unwrap_or(serde_json::Value::String(String::new())),
            "arguments": data.get("arguments").cloned().unwrap_or(serde_json::Value::Null),
        }),
        "tool_result" => serde_json::json!({
            "type": "tool_call_end",
            "call_id": data.get("call_id").cloned().unwrap_or(serde_json::Value::String(String::new())),
            "result": data.get("result").cloned().unwrap_or(serde_json::Value::String(String::new())),
        }),
        "run_started" => {
            let mut out = serde_json::json!({ "type": "run_started" });
            if let Some(obj) = out.as_object_mut() {
                if let Some(run_id) = data.get("run_id").cloned() {
                    obj.insert("run_id".to_string(), run_id);
                }
                if let Some(session_id) = data.get("session_id").cloned() {
                    obj.insert("session_id".to_string(), session_id);
                }
            }
            out
        }
        "run_finished" => {
            let mut out = serde_json::json!({ "type": "run_finished" });
            if let Some(obj) = out.as_object_mut() {
                if let Some(run_id) = data.get("run_id").cloned() {
                    obj.insert("run_id".to_string(), run_id);
                }
                if let Some(status) = data.get("status").cloned() {
                    obj.insert("status".to_string(), status);
                }
                if let Some(error) = data.get("error").cloned() {
                    obj.insert("error".to_string(), error);
                }
            }
            out
        }
        "run_error" => serde_json::json!({
            "type": "error",
            "message": data.get("error").cloned().unwrap_or(serde_json::Value::String("Unknown error".to_string())),
            "code": "RUN_ERROR",
        }),
        "approval_request" => {
            let mut out = serde_json::json!({ "type": "approval_required" });
            if let Some(obj) = out.as_object_mut() {
                for (k, v) in &data {
                    obj.insert(k.clone(), v.clone());
                }
            }
            out
        }
        "user_input" => {
            let mut out = serde_json::json!({ "type": "user_input" });
            if let Some(obj) = out.as_object_mut() {
                for (k, v) in &data {
                    obj.insert(k.clone(), v.clone());
                }
            }
            out
        }
        // Turn-completion events carry the authoritative assistant
        // text for client reconciliation (the streaming deltas may be
        // stale if the server recovered mid-turn). Pass through with
        // the full data payload merged into the client-shaped event.
        "turn_complete" | "turn_done" => {
            let mut out = serde_json::json!({ "type": event_type });
            if let Some(obj) = out.as_object_mut() {
                for (k, v) in &data {
                    obj.insert(k.clone(), v.clone());
                }
            }
            out
        }
        "run_paused" => {
            // Pause/resume lifecycle events had been falling through
            // the `_` catch-all in pre-wip-7 code, which relied on
            // passthrough. wip-7's allowlist drops the catch-all, so
            // pause/resume need explicit arms — `run_handlers` also
            // injects `run_id` for these.
            let mut out = serde_json::json!({ "type": "run_paused" });
            if let Some(obj) = out.as_object_mut() {
                for (k, v) in &data {
                    obj.insert(k.clone(), v.clone());
                }
            }
            out
        }
        "run_resumed" => {
            let mut out = serde_json::json!({ "type": "run_resumed" });
            if let Some(obj) = out.as_object_mut() {
                for (k, v) in &data {
                    obj.insert(k.clone(), v.clone());
                }
            }
            out
        }
        "plan_created" => serde_json::json!({
            "type": "plan_created",
            "plan": data.get("plan").cloned().unwrap_or(serde_json::json!({})),
        }),
        "plan_step_start" => serde_json::json!({
            "type": "plan_step_start",
            "step": data.get("step").cloned().unwrap_or(serde_json::Value::String(String::new())),
        }),
        "plan_step_done" => serde_json::json!({
            "type": "plan_step_done",
            "step": data.get("step").cloned().unwrap_or(serde_json::Value::String(String::new())),
            "result": data.get("result").cloned().unwrap_or(serde_json::Value::String(String::new())),
        }),
        "plan_revised" => serde_json::json!({
            "type": "plan_revised",
            "plan": data.get("plan").cloned().unwrap_or(serde_json::json!({})),
        }),
        "agent_delegated" => serde_json::json!({
            "type": "agent_delegated",
            "agent_id": data.get("agent_id").cloned().unwrap_or(serde_json::Value::String(String::new())),
            "task": data.get("task").cloned().unwrap_or(serde_json::Value::String(String::new())),
        }),
        "agent_spawned" => {
            let mut out = serde_json::json!({ "type": "agent_spawned" });
            if let Some(obj) = out.as_object_mut() {
                for (k, v) in &data {
                    obj.insert(k.clone(), v.clone());
                }
            }
            out
        }
        "agent_progress" => {
            let mut out = serde_json::json!({ "type": "agent_progress" });
            if let Some(obj) = out.as_object_mut() {
                for (k, v) in &data {
                    obj.insert(k.clone(), v.clone());
                }
            }
            out
        }
        "agent_completed" => {
            let mut out = serde_json::json!({ "type": "agent_completed" });
            if let Some(obj) = out.as_object_mut() {
                for (k, v) in &data {
                    obj.insert(k.clone(), v.clone());
                }
            }
            out
        }
        "keepalive" => serde_json::json!({ "type": "ping" }),
        _ => {
            // wip-7 allowlist: unknown internal event_types are
            // dropped. Adding a new event type means adding it
            // explicitly above (and, for client-facing events,
            // adding its `type` to `EXTERNAL_CLIENT_ALLOWLIST`).
            serde_json::Value::Null
        }
    }
}

#[derive(Clone, Debug)]
pub struct UnconfiguredRunLifecycleService;

#[async_trait]
impl RunLifecycleService for UnconfiguredRunLifecycleService {
    async fn create_run(
        &self,
        _user_id: String,
        _request: ChatRequestData,
    ) -> Result<ChatRunRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response_coded(
            StatusCode::NOT_IMPLEMENTED,
            "Run lifecycle service not configured",
            RUN_LIFECYCLE_UNCONFIGURED_ERROR_CODE,
        ))
    }

    async fn stream_chat(
        &self,
        _user_id: String,
        _request: ChatRequestData,
    ) -> Result<ChatStreamRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response_coded(
            StatusCode::NOT_IMPLEMENTED,
            "Run lifecycle service not configured",
            RUN_LIFECYCLE_UNCONFIGURED_ERROR_CODE,
        ))
    }

    async fn get_run_status(
        &self,
        _run_id: String,
        _user_id: String,
    ) -> Result<RunStatusRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response_coded(
            StatusCode::NOT_IMPLEMENTED,
            "Run lifecycle service not configured",
            RUN_LIFECYCLE_UNCONFIGURED_ERROR_CODE,
        ))
    }

    async fn stream_run(
        &self,
        _run_id: String,
        _user_id: String,
        _last_index: u32,
    ) -> Result<Vec<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response_coded(
            StatusCode::NOT_IMPLEMENTED,
            "Run lifecycle service not configured",
            RUN_LIFECYCLE_UNCONFIGURED_ERROR_CODE,
        ))
    }

    async fn cancel_run(
        &self,
        _run_id: String,
        _user_id: String,
    ) -> Result<CancelRunRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response_coded(
            StatusCode::NOT_IMPLEMENTED,
            "Run lifecycle service not configured",
            RUN_LIFECYCLE_UNCONFIGURED_ERROR_CODE,
        ))
    }

    async fn list_runs(
        &self,
        _user_id: String,
        _limit: u32,
        _offset: u32,
    ) -> Result<RunListRecord, (StatusCode, Json<ErrorResponse>)> {
        Err(error_response_coded(
            StatusCode::NOT_IMPLEMENTED,
            "Run lifecycle service not configured",
            RUN_LIFECYCLE_UNCONFIGURED_ERROR_CODE,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_event(event_type: &str, data: serde_json::Value) -> serde_json::Value {
        json!({"event_type": event_type, "data": data})
    }

    #[test]
    fn text_delta() {
        let out = transform_run_event_for_client(make_event("text_delta", json!({"chunk": "hi"})));
        assert_eq!(out["type"], "text_delta");
        assert_eq!(out["content"], "hi");
    }

    #[test]
    fn text_delta_missing_chunk() {
        let out = transform_run_event_for_client(make_event("text_delta", json!({})));
        assert_eq!(out["content"], "");
    }

    #[test]
    fn assistant_delta_maps_to_text_delta() {
        let out =
            transform_run_event_for_client(make_event("assistant_delta", json!({"text": "hi"})));
        assert_eq!(out["type"], "text_delta");
        assert_eq!(out["content"], "hi");
    }

    #[test]
    fn text_done() {
        let out =
            transform_run_event_for_client(make_event("text_done", json!({"full_text": "all"})));
        assert_eq!(out["type"], "text_done");
        assert_eq!(out["full_text"], "all");
    }

    #[test]
    fn reasoning_message_content() {
        let out = transform_run_event_for_client(make_event(
            "reasoning_message_content",
            json!({"content": "think"}),
        ));
        assert_eq!(out["type"], "reasoning_message_content");
        assert_eq!(out["content"], "think");
    }

    #[test]
    fn thinking_delta() {
        let out =
            transform_run_event_for_client(make_event("thinking_delta", json!({"chunk": "t"})));
        assert_eq!(out["type"], "thinking_delta");
        assert_eq!(out["content"], "t");
    }

    #[test]
    fn thinking_done() {
        let out = transform_run_event_for_client(make_event("thinking_done", json!({})));
        assert_eq!(out["type"], "thinking_done");
    }

    #[test]
    fn reasoning_done() {
        let out = transform_run_event_for_client(make_event("reasoning_done", json!({})));
        assert_eq!(out["type"], "reasoning_done");
    }

    #[test]
    fn tool_call_start() {
        let out = transform_run_event_for_client(make_event(
            "tool_call_start",
            json!({"tool": "bash", "call_id": "c1", "arguments": "{\"command\":\"ls\"}"}),
        ));
        assert_eq!(out["type"], "tool_call_start");
        assert_eq!(out["tool"], "bash");
        assert_eq!(out["call_id"], "c1");
        assert_eq!(out["arguments"], "{\"command\":\"ls\"}");
    }

    #[test]
    fn tool_result() {
        let out = transform_run_event_for_client(make_event(
            "tool_result",
            json!({"call_id": "c1", "result": "ok"}),
        ));
        assert_eq!(out["type"], "tool_call_end");
        assert_eq!(out["call_id"], "c1");
    }

    #[test]
    fn run_started_and_finished() {
        let started = transform_run_event_for_client(make_event(
            "run_started",
            json!({"run_id": "run-1", "session_id": "sess-1"}),
        ));
        assert_eq!(started["type"], "run_started");
        assert_eq!(started["run_id"], "run-1");
        assert_eq!(started["session_id"], "sess-1");

        let finished = transform_run_event_for_client(make_event(
            "run_finished",
            json!({"run_id": "run-1", "status": "failed", "error": "boom"}),
        ));
        assert_eq!(finished["type"], "run_finished");
        assert_eq!(finished["run_id"], "run-1");
        assert_eq!(finished["status"], "failed");
        assert_eq!(finished["error"], "boom");
    }

    #[test]
    fn run_error_maps_to_error_type() {
        let out = transform_run_event_for_client(make_event("run_error", json!({"error": "boom"})));
        assert_eq!(out["type"], "error");
        assert_eq!(out["message"], "boom");
        assert_eq!(out["code"], "RUN_ERROR");
    }

    #[test]
    fn run_error_default_message() {
        let out = transform_run_event_for_client(make_event("run_error", json!({})));
        assert_eq!(out["message"], "Unknown error");
    }

    #[test]
    fn approval_and_user_input_events_are_client_visible_for_replay() {
        let approval = transform_run_event_for_client(make_event(
            "approval_request",
            json!({"approval_id": "approval-1"}),
        ));
        assert_eq!(approval["type"], "approval_required");
        assert_eq!(approval["approval_id"], "approval-1");

        let input =
            transform_run_event_for_client(make_event("user_input", json!({"text": "approved"})));
        assert_eq!(input["type"], "user_input");
        assert_eq!(input["text"], "approved");
    }

    #[test]
    fn plan_events() {
        let created = transform_run_event_for_client(make_event(
            "plan_created",
            json!({"plan": {"steps": []}}),
        ));
        assert_eq!(created["type"], "plan_created");
        let step_start =
            transform_run_event_for_client(make_event("plan_step_start", json!({"step": "s1"})));
        assert_eq!(step_start["type"], "plan_step_start");
        let step_done = transform_run_event_for_client(make_event(
            "plan_step_done",
            json!({"step": "s1", "result": "ok"}),
        ));
        assert_eq!(step_done["type"], "plan_step_done");
        let revised =
            transform_run_event_for_client(make_event("plan_revised", json!({"plan": {}})));
        assert_eq!(revised["type"], "plan_revised");
    }

    #[test]
    fn agent_events() {
        let delegated = transform_run_event_for_client(make_event(
            "agent_delegated",
            json!({"agent_id": "a1", "task": "t"}),
        ));
        assert_eq!(delegated["type"], "agent_delegated");
        let progress = transform_run_event_for_client(make_event(
            "agent_progress",
            json!({"agent_id": "a1", "progress": "50%"}),
        ));
        assert_eq!(progress["type"], "agent_progress");
        let completed = transform_run_event_for_client(make_event(
            "agent_completed",
            json!({"agent_id": "a1", "result": "done"}),
        ));
        assert_eq!(completed["type"], "agent_completed");
    }

    #[test]
    fn keepalive_maps_to_ping() {
        let out = transform_run_event_for_client(make_event("keepalive", json!({})));
        assert_eq!(out["type"], "ping");
    }

    #[test]
    fn unknown_event_type_dropped_by_allowlist() {
        // wip-7: the transform is an allowlist, not a passthrough —
        // unknown `event_type` values return `Value::Null` so the
        // caller in `run_handlers` can skip them entirely. This
        // blocks future internal events from accidentally leaking to
        // external API clients.
        let out = transform_run_event_for_client(make_event("custom_event", json!({})));
        assert!(
            out.is_null(),
            "unknown event_type must be dropped by the allowlist, got: {out}"
        );
    }

    #[test]
    fn unknown_event_type_with_data_also_dropped() {
        // Even when an unknown event carries structured data we'd
        // want on a passthrough, the allowlist still drops it — the
        // point is that internal events must be explicitly opted in.
        let out = transform_run_event_for_client(make_event(
            "team_prepare",
            json!({"delegation_id": "d1", "phase": "prepare"}),
        ));
        assert!(out.is_null(), "unknown event_type must be dropped");
    }

    #[test]
    fn missing_event_type_and_no_type_is_dropped() {
        // No `event_type` AND no `type` on the wire: treat as malformed
        // / future schema and drop. Pre-wip-7 this returned an empty
        // `type:""` synthetic event; wip-7 drops it per allowlist.
        let out = transform_run_event_for_client(json!({"data": {}}));
        assert!(out.is_null(), "malformed event must be dropped");
    }

    #[test]
    fn missing_data_object() {
        let out = transform_run_event_for_client(json!({"event_type": "text_delta"}));
        assert_eq!(out["type"], "text_delta");
        assert_eq!(out["content"], "");
    }

    #[test]
    fn already_shaped_client_event_not_in_allowlist_is_dropped() {
        // wip-7: already-shaped `{"type": ...}` events go through the
        // external-client allowlist. `injection_freshness` is the
        // motivating case — it's the internal-diagnostic side-channel
        // the bridge emits for the CLI's observability hooks. External
        // API callers should NEVER see it, even when the bridge emits
        // the fingerprint-only wire shape.
        let event = json!({
            "type": "injection_freshness",
            "channels": [
                { "tag": "self_awareness", "hash": 0u64, "bytes": 0u64, "is_empty": true }
            ]
        });
        let out = transform_run_event_for_client(event);
        assert!(
            out.is_null(),
            "client-shaped internal event must be dropped by allowlist, got: {out}"
        );
    }

    #[test]
    fn already_shaped_client_event_in_allowlist_passes_through() {
        // Allowlisted types pass through unchanged so indexing /
        // downstream decoration (e.g. `run_id` injection in
        // `run_handlers`) still works.
        let event = json!({"type": "text_delta", "content": "hello", "index": 3});
        let out = transform_run_event_for_client(event.clone());
        assert_eq!(out, event);
    }

    #[test]
    fn chat_request_data_debug_redacts_forward_header_values() {
        let mut forward_headers = std::collections::HashMap::new();
        forward_headers.insert(
            "authorization".to_string(),
            "Bearer secret-token".to_string(),
        );
        forward_headers.insert("x-workspace-id".to_string(), "ws-123".to_string());
        forward_headers.insert("__astra_connection_tokens".to_string(), "x-hop".to_string());

        let request = ChatRequestData {
            message: "hi".to_string(),
            session_id: Some("sess-1".to_string()),
            agent_id: None,
            model: None,
            llm_token_service: None,
            skill_search: None,
            allow_skills: None,
            allow_tools: None,
            context: None,
            forward_headers,
            execution_budget: Some(ExecutionBudget {
                initial_turns: Some(10),
                hard_turn_limit: Some(18),
            }),
            full_llm_capture: false,
            explain: false,
            interactive_client: false,
        };

        let rendered = format!("{request:?}");
        assert!(rendered.contains("authorization"));
        assert!(rendered.contains("x-workspace-id"));
        assert!(!rendered.contains("Bearer secret-token"));
        assert!(!rendered.contains("ws-123"));
        assert!(!rendered.contains("__astra_connection_tokens"));
    }

    #[tokio::test]
    async fn unconfigured_service_uses_stable_error_code() {
        let service = UnconfiguredRunLifecycleService;
        let err = service
            .create_run(
                "u1".to_string(),
                ChatRequestData {
                    message: "hi".to_string(),
                    session_id: None,
                    agent_id: None,
                    model: None,
                    llm_token_service: None,
                    skill_search: None,
                    allow_skills: None,
                    allow_tools: None,
                    context: None,
                    forward_headers: std::collections::HashMap::new(),
                    execution_budget: Some(ExecutionBudget {
                        initial_turns: Some(25),
                        hard_turn_limit: Some(40),
                    }),
                    full_llm_capture: false,
                    explain: false,
                    interactive_client: false,
                },
            )
            .await
            .expect_err("service should be unconfigured");
        assert!(is_run_lifecycle_unconfigured_error(err.0, &err.1.0));
        assert_eq!(
            err.1.0.error_code.as_deref(),
            Some(RUN_LIFECYCLE_UNCONFIGURED_ERROR_CODE)
        );
    }

    /// U2: InMemoryRunStateStore must evict old completed runs when the
    /// store exceeds its capacity, preventing unbounded memory growth.
    #[tokio::test]
    async fn in_memory_run_store_evicts_completed_runs() {
        let store = InMemoryRunStateStore::new();
        let max = InMemoryRunStateStore::MAX_RUNS;

        // Fill to capacity + 10 with completed runs
        for i in 0..max + 10 {
            let record = DurableRunRecord {
                run_id: format!("run-{i}"),
                session_id: "s1".into(),
                user_id: "u1".into(),
                status: "completed".into(),
                parent_run_id: None,
                root_run_id: None,
                ancestor_path: None,
                depth: 0,
                delegation_id: None,
                agent_id: None,
                retry_of: None,
                retry_scope: None,
                waiting_for: None,
                owner_pod_id: None,
                owner_lease_expires_at: None,
                run_generation: 0,
                last_event_idx: -1,
                checkpoint_version: None,
                checkpoint_json: None,
                error_code: None,
                error_message: None,
                retry_count: 0,
                total_prompt_tokens: 0,
                total_completion_tokens: 0,
                total_tool_calls: 0,
                events: vec![],
                created_at: chrono::Utc::now().to_rfc3339(),
                updated_at: chrono::Utc::now().to_rfc3339(),
            };
            store.insert_run(record).await.unwrap();
        }

        // Store must not exceed max capacity
        let runs = store.runs.read().await;
        assert!(
            runs.len() <= max,
            "store has {} runs, expected ≤ {max}",
            runs.len()
        );
    }
}

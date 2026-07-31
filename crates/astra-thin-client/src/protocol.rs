//! JSON bodies and classified SSE payloads for the thin client protocol.
//!
//! Aligns with `runtime` `ChatRequest` / `http_helpers::sse_json_response` and design doc §5.5
//! (`edge_executor_id`, `capabilities`).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

pub use astra_turn_types::ModelSelection;

/// `POST /chat/stream` body — superset of server `ChatRequest` plus optional edge fields.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ChatStreamRequest {
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    pub model_selection: ModelSelection,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interaction_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<serde_json::Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_budget: Option<ExecutionBudget>,
    #[serde(default)]
    pub explain: bool,
    /// Forwarded into server `context` for stop-hooks (`when: task_completed`) on cloud runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_subtask_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_plan_subtask: Option<bool>,
    /// Design §5.5 — identifies which edge executor should run tool callbacks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge_executor_id: Option<String>,
    /// Tool names this edge instance can run (bash, fs, git, …).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_authority: Option<astra_turn_types::ConversationAuthorityEnvelopeV1>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionBudget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_turns: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hard_turn_limit: Option<u32>,
}

impl ChatStreamRequest {
    pub fn new(message: impl Into<String>, offering_id: impl Into<String>) -> Self {
        Self::with_model_selection(
            message,
            ModelSelection {
                offering_id: offering_id.into(),
            },
        )
    }

    pub fn with_model_selection(
        message: impl Into<String>,
        model_selection: ModelSelection,
    ) -> Self {
        Self {
            message: message.into(),
            parts: Vec::new(),
            attachments: Vec::new(),
            session_id: None,
            agent_id: None,
            model_selection,
            interaction_mode: None,
            context: None,
            execution_budget: None,
            explain: false,
            plan_subtask_id: None,
            is_plan_subtask: None,
            edge_executor_id: None,
            capabilities: Vec::new(),
            conversation_authority: None,
        }
    }
}

/// `POST /sessions` (matches `SessionCreateRequest` on server).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SessionCreateRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Map<String, Value>>,
}

/// `PUT /sessions/{id}` body subset.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SessionUpdateRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata_patch: Option<serde_json::Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

/// `POST /chat/runs/{run_id}/intents` body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunUserIntentRequest {
    pub intent_id: String,
    pub delivery: astra_turn_types::UserIntentDelivery,
    #[serde(default)]
    pub input: Value,
}

/// `POST /chat/runs/{run_id}/intents` response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunUserIntentResponse {
    pub run_id: String,
    pub intent_id: String,
    pub status: astra_turn_types::UserIntentStatus,
    pub duplicate: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionTranscriptItem {
    pub session_id: String,
    pub item_seq: i64,
    pub run_id: Option<String>,
    pub role: String,
    pub content: String,
    #[serde(default)]
    pub reasoning: Option<String>,
    #[serde(default)]
    pub reasoning_status: Option<String>,
    /// Structured tool calls carried by an assistant message. Empty for
    /// ordinary conversation and for servers that have not projected tool
    /// evidence into this page.
    #[serde(default)]
    pub tool_calls: Vec<SessionTranscriptToolCall>,
    /// Structured linkage for a tool-result message.
    #[serde(default)]
    pub tool_result: Option<SessionTranscriptToolResult>,
    /// Structured non-conversational evidence associated with this agent run.
    /// It is visible in the transcript but never part of prompt-facing
    /// user/assistant/tool history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<astra_turn_types::AgentTranscriptEvidence>,
    /// Stable canonical event identity when this item was projected from a
    /// server-side trace event. Local journal items omit it.
    #[serde(default)]
    pub source_event_id: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionTranscriptToolCall {
    pub tool_use_id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionTranscriptToolResult {
    pub tool_use_id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionTranscriptPageRef {
    pub page_seq: i64,
    pub start_item_seq: i64,
    pub end_item_seq: i64,
    pub item_count: i64,
    pub page_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionTranscriptPage {
    pub session_id: String,
    pub items: Vec<SessionTranscriptItem>,
    pub page_refs: Vec<SessionTranscriptPageRef>,
    pub next_before_seq: Option<i64>,
    pub has_more: bool,
}

/// The server-owned projection a transcript reader asks for. `Session` is an
/// explicit audit/debug scope; user-facing root conversations and delegated
/// runs must choose their distinct scopes instead of relying on an omitted
/// `run_id` convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionTranscriptReadScope<'a> {
    Session,
    RootConversation,
    Run(&'a str),
}

/// `POST /tools/result` (§5.5).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolResultRequest {
    pub session_id: String,
    pub run_id: String,
    pub turn_chain_id: String,
    pub request_id: String,
    pub status: String,
    /// The edge agent ID that produced this result.
    /// Required for cross-pod delivery and must match the dispatch row.
    pub edge_agent_id: String,
    pub output: String,
    pub duration_ms: u64,
    /// Hash of the tool result content for idempotent deduplication.
    /// Computed from the full callback identity plus output.
    pub result_hash: String,
    /// Structured tool-result metadata forwarded through the cloud ledger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_result_fields: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolResultRequestParts {
    pub session_id: String,
    pub run_id: String,
    pub turn_chain_id: String,
    pub request_id: String,
    pub edge_agent_id: String,
    pub status: String,
    pub output: String,
    pub duration_ms: u64,
    pub tool_result_fields: Option<Map<String, Value>>,
}

impl ToolResultRequest {
    /// Compute a content-based hash of the scoped tool result identity + output.
    pub fn compute_result_hash(
        session_id: &str,
        run_id: &str,
        turn_chain_id: &str,
        request_id: &str,
        output: &str,
    ) -> String {
        let mut hasher = Sha256::new();
        hasher.update(session_id.as_bytes());
        hasher.update(b":");
        hasher.update(run_id.as_bytes());
        hasher.update(b":");
        hasher.update(turn_chain_id.as_bytes());
        hasher.update(b":");
        hasher.update(request_id.as_bytes());
        hasher.update(b":");
        hasher.update(output.as_bytes());
        hex::encode(hasher.finalize())
    }

    /// Factory: build a `ToolResultRequest` with the result hash pre-computed.
    /// Every call site that posts a tool result should use this to guarantee
    /// the hash is always present — no more scattered `compute_result_hash` calls.
    pub fn new_with_hash(parts: ToolResultRequestParts) -> Self {
        let ToolResultRequestParts {
            session_id,
            run_id,
            turn_chain_id,
            request_id,
            edge_agent_id,
            status,
            output,
            duration_ms,
            tool_result_fields,
        } = parts;
        let result_hash =
            Self::compute_result_hash(&session_id, &run_id, &turn_chain_id, &request_id, &output);
        Self {
            session_id,
            run_id,
            turn_chain_id,
            request_id,
            edge_agent_id,
            status,
            output,
            duration_ms,
            result_hash,
            tool_result_fields,
        }
    }

    /// Parse a dispatch result JSON string back into `(output, is_error)`.
    ///
    /// The JSON is the serialized `ToolResultRequest` produced by
    /// `edge_callback_handlers` / `deliver_result`.  Both the tool-executor
    /// fallback path and the turn-bridge polling path need to extract the
    /// same two fields — this avoids duplicated parsing logic.
    pub fn parse_output_and_error(result_json: &str) -> (String, bool) {
        let v: serde_json::Value = serde_json::from_str(result_json)
            .unwrap_or_else(|_| serde_json::json!({"output": result_json}));
        let output = v
            .get("output")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let is_error = v
            .get("status")
            .and_then(|v| v.as_str())
            .map(|s| matches!(s, "error" | "failed"))
            .unwrap_or(false);
        (output, is_error)
    }
}

/// `POST /approval/respond` (§5.5).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApprovalRespondRequest {
    pub request_id: String,
    pub decision: ApprovalDecision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub session_id: String,
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_kind: Option<ApprovalKind>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Allow,
    Deny,
    AllowSession,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalKind {
    Standard,
    Explicit,
}

/// `POST /user-prompts/respond`.
///
/// The response payload is left as JSON at the transport boundary because
/// thin clients do not depend on the server tool crate. The server parses it
/// into the canonical questionnaire answer type and validates it against the
/// durable prompt before recording any terminal outcome.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserPromptRespondRequest {
    pub request_id: String,
    pub session_id: String,
    pub run_id: String,
    #[serde(default)]
    pub cancelled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answers: Option<Value>,
}

/// `POST /agents/edge` — matches server `EdgeRegisterRequest` (Phase 3 registry).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EdgeRegisterRequest {
    pub edge_agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Value>,
}

impl EdgeRegisterRequest {
    pub fn new(edge_agent_id: impl Into<String>) -> Self {
        Self {
            edge_agent_id: edge_agent_id.into(),
            hostname: None,
            worktree_path: None,
            capabilities: None,
        }
    }
}

/// `POST /agents/edge/heartbeat`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EdgeHeartbeatRequest {
    pub edge_agent_id: String,
    /// Number of in-flight tool requests on this edge executor.
    /// Used by cloud to detect stalled edges (pending > 0 but no results
    /// received for > 2 min → warning; no heartbeat for > 5 min → stale).
    #[serde(default)]
    pub pending_request_count: u32,
    /// Recently completed request IDs for deduplication on reconnection.
    /// Cloud can skip re-issuing tool calls already completed by this edge.
    #[serde(default)]
    pub last_seen_request_ids: Vec<String>,
}

/// Server policy for invocations that remain unresolved across an Edge
/// heartbeat. Pending transport state is not proof that a side effect is safe
/// to execute again.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum EdgeHeartbeatReplayPolicy {
    /// Older servers returned executable `pending_requests` without durable
    /// result evidence. Clients must surface these entries and must not replay
    /// them automatically.
    #[default]
    LegacyPendingPayloadRequiresManualReconciliation,
    /// The server returns identities only. Reconciliation must use the
    /// canonical durable invocation/result protocol.
    DurableResultReconciliationRequired,
    /// A newer server policy unknown to this client. Unknown policy never
    /// grants replay authority; the client surfaces it for reconciliation.
    #[serde(other)]
    Unknown,
}

/// Minimal shape accepted from the retired executable-pending heartbeat
/// response. Extra legacy payload fields are intentionally ignored: they are
/// diagnostic evidence, never execution authority.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LegacyEdgePendingRequest {
    pub request_id: String,
}

/// `POST /agents/edge/heartbeat` response shared by Server and thin clients.
///
/// The response never authorizes tool execution. A non-empty unresolved or
/// legacy pending set requires durable reconciliation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EdgeHeartbeatResponse {
    pub ok: bool,
    pub user_id: String,
    pub edge_id: String,
    pub edge_agent_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unresolved_request_ids: Vec<String>,
    #[serde(default)]
    pub replay_policy: EdgeHeartbeatReplayPolicy,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ack_request_ids: Vec<String>,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        rename = "pending_requests"
    )]
    pub legacy_pending_requests: Vec<LegacyEdgePendingRequest>,
}

impl EdgeHeartbeatResponse {
    pub fn requires_reconciliation(&self) -> bool {
        !self.unresolved_request_ids.is_empty()
            || !self.legacy_pending_requests.is_empty()
            || self.replay_policy == EdgeHeartbeatReplayPolicy::Unknown
    }
}

/// `POST /agent-jobs/{id}/lease/{claim,release,renew}` — matches server lease handlers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct TaskLeaseMutationRequest {
    pub edge_agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_sec: Option<i64>,
}

/// Classified SSE JSON line (`data: …` payload). Unknown `type` values are preserved as [`StreamEvent::Other`].
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    SessionInfo {
        session_id: String,
        run_id: Option<String>,
    },
    TextDelta {
        content: Value,
    },
    TextDone {
        full_text: Value,
    },
    ReasoningMessageContent {
        content: Value,
    },
    ReasoningDelta {
        content: Value,
    },
    ThinkingDelta {
        content: Value,
    },
    ThinkingDone,
    ReasoningDone,
    ToolCallStart {
        tool: Value,
        call_id: Value,
        arguments: Option<Value>,
    },
    ToolCallEnd {
        call_id: Value,
        result: Value,
    },
    /// §5.5 — cloud asks edge to run a tool (forward-compatible).
    ToolRequest {
        session_id: String,
        run_id: String,
        turn_chain_id: String,
        request_id: String,
        tool: String,
        args: Value,
    },
    PlanCreated {
        plan: Value,
    },
    PlanStepStart {
        step: Value,
    },
    PlanStepDone {
        step: Value,
        result: Value,
    },
    PlanRevised {
        plan: Value,
    },
    /// §5.5 — subtask / plan progress (generic bucket).
    PlanUpdate {
        raw: Value,
    },
    AgentDelegated {
        agent_id: Value,
        task: Value,
    },
    AgentCommunication(astra_turn_types::AgentCommunicationEvent),
    AgentSpawned {
        agent_id: String,
        run_id: String,
        parent_run_id: String,
        agent_type: String,
        description: String,
        timestamp: Option<u64>,
        raw: Value,
    },
    AgentProgress {
        agent_id: String,
        status: Option<String>,
        raw: Value,
    },
    AgentCompleted {
        agent_id: String,
        status: Option<String>,
        raw: Value,
    },
    RunStarted {
        run_id: Option<String>,
        session_id: Option<String>,
    },
    RunPaused {
        run_id: Option<String>,
    },
    RunWaiting {
        run_id: Option<String>,
        reason: Option<String>,
    },
    RunResumed {
        run_id: Option<String>,
    },
    RunCancelled {
        run_id: Option<String>,
    },
    RunUserIntentAccepted {
        run_id: String,
        intent_id: String,
        delivery: astra_turn_types::UserIntentDelivery,
        index: u64,
    },
    RunUserIntentApplied {
        run_id: String,
        intent_id: String,
        delivery: astra_turn_types::UserIntentDelivery,
        event_index: u64,
        content: String,
        index: u64,
    },
    RunError {
        message: String,
        error_kind: Option<String>,
        raw: Value,
    },
    RunInterrupted {
        run_id: Option<String>,
        kind: Option<String>,
        resumable: Option<bool>,
        message: Option<String>,
        raw: Value,
    },
    RunFinished {
        run_id: Option<String>,
        status: Option<String>,
        error: Option<String>,
    },
    Usage {
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        cached_input_tokens: Option<u64>,
        cache_creation_tokens: Option<u64>,
        total_tokens: Option<u64>,
        tool_call_count: Option<u64>,
        raw: Value,
    },
    TurnComplete {
        assistant_text: Option<String>,
        followup_suggestion: Option<String>,
        raw: Value,
    },
    Warning {
        message: String,
        claims_failed: Option<u64>,
        raw: Value,
    },
    Explain {
        content: String,
        raw: Value,
    },
    Ping,
    Done {
        tokens_used: Option<u64>,
        raw: Value,
    },
    /// §5.5 — approval gate.
    ApprovalRequired {
        request_id: String,
        tool: String,
        approval_kind: ApprovalKind,
        path: Option<String>,
        detail: Option<String>,
        raw: Value,
    },
    /// A durable ask-user interaction. `prompt` is the canonical question
    /// payload and `run_id` identifies the run that is waiting for input.
    UserPromptRequired {
        request_id: String,
        run_id: Option<String>,
        prompt: Value,
        raw: Value,
    },
    Error {
        message: String,
        code: Option<String>,
        retryable: bool,
        raw: Value,
    },
    /// Server sent a `type` we do not model yet.
    Other {
        event_type: String,
        raw: Value,
    },
}

/// Parse the JSON object from one SSE `data:` line into a [`StreamEvent`].
pub fn classify_stream_event(value: Value) -> Result<StreamEvent, crate::error::ThinClientError> {
    let obj = value
        .as_object()
        .cloned()
        .ok_or_else(|| crate::error::ThinClientError::InvalidSseJson(value.clone()))?;

    let ty = obj
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let raw = Value::Object(obj.clone());

    Ok(match ty.as_str() {
        "session_info" => StreamEvent::SessionInfo {
            session_id: get_str(&obj, "session_id"),
            run_id: obj
                .get("run_id")
                .and_then(|v| v.as_str())
                .map(std::string::ToString::to_string),
        },
        "text_delta" => StreamEvent::TextDelta {
            content: obj.get("content").cloned().unwrap_or(Value::Null),
        },
        "text_done" => StreamEvent::TextDone {
            full_text: obj.get("full_text").cloned().unwrap_or(Value::Null),
        },
        "reasoning_message_content" => StreamEvent::ReasoningMessageContent {
            content: obj.get("content").cloned().unwrap_or(Value::Null),
        },
        "reasoning_delta" => StreamEvent::ReasoningDelta {
            content: obj.get("content").cloned().unwrap_or(Value::Null),
        },
        "thinking_delta" => StreamEvent::ThinkingDelta {
            content: obj.get("content").cloned().unwrap_or(Value::Null),
        },
        "thinking_done" => StreamEvent::ThinkingDone,
        "reasoning_done" => StreamEvent::ReasoningDone,
        "tool_call_start" => StreamEvent::ToolCallStart {
            tool: obj.get("tool").cloned().unwrap_or(Value::Null),
            call_id: obj.get("call_id").cloned().unwrap_or(Value::Null),
            arguments: obj.get("arguments").cloned(),
        },
        "tool_call_end" | "tool_result" => StreamEvent::ToolCallEnd {
            call_id: obj.get("call_id").cloned().unwrap_or(Value::Null),
            result: obj.get("result").cloned().unwrap_or(Value::Null),
        },
        "tool_request" => StreamEvent::ToolRequest {
            session_id: get_str(&obj, "session_id"),
            run_id: get_str(&obj, "run_id"),
            turn_chain_id: get_str(&obj, "turn_chain_id"),
            request_id: get_str(&obj, "request_id"),
            tool: get_str(&obj, "tool"),
            args: obj
                .get("args")
                .cloned()
                .unwrap_or_else(|| Value::Object(Default::default())),
        },
        "plan_created" => StreamEvent::PlanCreated {
            plan: obj
                .get("plan")
                .cloned()
                .unwrap_or_else(|| Value::Object(Default::default())),
        },
        "plan_step_start" => StreamEvent::PlanStepStart {
            step: obj.get("step").cloned().unwrap_or(Value::Null),
        },
        "plan_step_done" => StreamEvent::PlanStepDone {
            step: obj.get("step").cloned().unwrap_or(Value::Null),
            result: obj.get("result").cloned().unwrap_or(Value::Null),
        },
        "plan_revised" => StreamEvent::PlanRevised {
            plan: obj
                .get("plan")
                .cloned()
                .unwrap_or_else(|| Value::Object(Default::default())),
        },
        "plan_update" => StreamEvent::PlanUpdate { raw },
        "agent_delegated" => StreamEvent::AgentDelegated {
            agent_id: obj.get("agent_id").cloned().unwrap_or(Value::Null),
            task: obj.get("task").cloned().unwrap_or(Value::Null),
        },
        "agent_communication" => StreamEvent::AgentCommunication(
            serde_json::from_value(raw.clone())
                .map_err(|_| crate::error::ThinClientError::InvalidSseJson(raw.clone()))?,
        ),
        "agent_spawned" => StreamEvent::AgentSpawned {
            agent_id: get_str(&obj, "agent_id"),
            run_id: get_str(&obj, "run_id"),
            parent_run_id: get_str(&obj, "parent_run_id"),
            agent_type: get_str(&obj, "agent_type"),
            description: get_str(&obj, "description"),
            timestamp: obj.get("timestamp").and_then(|v| v.as_u64()),
            raw,
        },
        "agent_progress" => StreamEvent::AgentProgress {
            agent_id: get_str(&obj, "agent_id"),
            status: optional_str(&obj, "status"),
            raw,
        },
        "agent_completed" => StreamEvent::AgentCompleted {
            agent_id: get_str(&obj, "agent_id"),
            status: optional_str(&obj, "status"),
            raw,
        },
        "run_started" => StreamEvent::RunStarted {
            run_id: optional_str(&obj, "run_id"),
            session_id: optional_str(&obj, "session_id"),
        },
        "run_paused" => StreamEvent::RunPaused {
            run_id: optional_str(&obj, "run_id"),
        },
        "run_waiting" => StreamEvent::RunWaiting {
            run_id: optional_str(&obj, "run_id"),
            reason: optional_str(&obj, "reason"),
        },
        "run_resumed" => StreamEvent::RunResumed {
            run_id: optional_str(&obj, "run_id"),
        },
        "run_cancelled" => StreamEvent::RunCancelled {
            run_id: optional_str(&obj, "run_id"),
        },
        "user_intent_accepted" => {
            let status: astra_turn_types::UserIntentStatus = required_field(&obj, "status", &raw)?;
            if status != astra_turn_types::UserIntentStatus::AcceptedRemote {
                return Err(crate::error::ThinClientError::InvalidSseJson(raw));
            }
            StreamEvent::RunUserIntentAccepted {
                run_id: required_field(&obj, "run_id", &raw)?,
                intent_id: required_field(&obj, "intent_id", &raw)?,
                delivery: required_field(&obj, "delivery", &raw)?,
                index: required_field(&obj, "index", &raw)?,
            }
        }
        "user_intent_applied" => {
            let status: astra_turn_types::UserIntentStatus = required_field(&obj, "status", &raw)?;
            if status != astra_turn_types::UserIntentStatus::Applied {
                return Err(crate::error::ThinClientError::InvalidSseJson(raw));
            }
            StreamEvent::RunUserIntentApplied {
                run_id: required_field(&obj, "run_id", &raw)?,
                intent_id: required_field(&obj, "intent_id", &raw)?,
                delivery: required_field(&obj, "delivery", &raw)?,
                event_index: required_field(&obj, "event_index", &raw)?,
                content: required_field(&obj, "content", &raw)?,
                index: required_field(&obj, "index", &raw)?,
            }
        }
        "run_error" => StreamEvent::RunError {
            message: obj
                .get("message")
                .or_else(|| obj.get("error"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            error_kind: optional_str(&obj, "error_kind"),
            raw,
        },
        "run_interrupted" => StreamEvent::RunInterrupted {
            run_id: optional_str(&obj, "run_id"),
            kind: optional_str(&obj, "kind").or_else(|| optional_str(&obj, "interruption_kind")),
            resumable: obj.get("resumable").and_then(|v| v.as_bool()),
            message: optional_str(&obj, "message").or_else(|| optional_str(&obj, "user_message")),
            raw,
        },
        "run_finished" => StreamEvent::RunFinished {
            run_id: optional_str(&obj, "run_id"),
            status: optional_str(&obj, "status"),
            error: optional_str(&obj, "error"),
        },
        "usage" => StreamEvent::Usage {
            input_tokens: obj.get("input_tokens").and_then(|v| v.as_u64()),
            output_tokens: obj.get("output_tokens").and_then(|v| v.as_u64()),
            cached_input_tokens: obj.get("cached_input_tokens").and_then(|v| v.as_u64()),
            cache_creation_tokens: obj.get("cache_creation_tokens").and_then(|v| v.as_u64()),
            total_tokens: obj.get("total_tokens").and_then(|v| v.as_u64()),
            tool_call_count: obj.get("tool_call_count").and_then(|v| v.as_u64()),
            raw,
        },
        "turn_complete" => StreamEvent::TurnComplete {
            assistant_text: optional_str(&obj, "assistant_text"),
            followup_suggestion: optional_str(&obj, "followup_suggestion"),
            raw,
        },
        "warning" => StreamEvent::Warning {
            message: get_str(&obj, "message"),
            claims_failed: obj.get("claims_failed").and_then(|v| v.as_u64()),
            raw,
        },
        "explain" => StreamEvent::Explain {
            content: get_str(&obj, "content"),
            raw,
        },
        "ping" => StreamEvent::Ping,
        "done" => StreamEvent::Done {
            tokens_used: obj.get("tokens_used").and_then(|v| v.as_u64()).or_else(|| {
                obj.get("tokens_used")
                    .and_then(|v| v.as_i64())
                    .map(|i| i as u64)
            }),
            raw,
        },
        "approval_required" => StreamEvent::ApprovalRequired {
            request_id: get_str(&obj, "request_id"),
            tool: get_str(&obj, "tool"),
            approval_kind: obj
                .get("approval_kind")
                .cloned()
                .and_then(|value| serde_json::from_value::<ApprovalKind>(value).ok())
                .unwrap_or(ApprovalKind::Explicit),
            path: obj
                .get("path")
                .and_then(|v| v.as_str())
                .map(std::string::ToString::to_string),
            detail: obj
                .get("detail")
                .and_then(|v| v.as_str())
                .map(std::string::ToString::to_string)
                .or_else(|| {
                    obj.get("path")
                        .and_then(|v| v.as_str())
                        .map(std::string::ToString::to_string)
                }),
            raw,
        },
        "user_prompt_required" => StreamEvent::UserPromptRequired {
            request_id: get_str(&obj, "request_id"),
            run_id: optional_str(&obj, "run_id"),
            prompt: obj.get("prompt").cloned().unwrap_or(Value::Null),
            raw,
        },
        "error" => StreamEvent::Error {
            message: obj
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            code: obj
                .get("code")
                .and_then(|v| v.as_str())
                .map(std::string::ToString::to_string),
            retryable: obj
                .get("retryable")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            raw,
        },
        "" => StreamEvent::Other {
            event_type: String::new(),
            raw,
        },
        _ => StreamEvent::Other {
            event_type: ty,
            raw,
        },
    })
}

fn get_str(obj: &serde_json::Map<String, Value>, key: &str) -> String {
    obj.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn optional_str(obj: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key)
        .and_then(|v| v.as_str())
        .map(std::string::ToString::to_string)
}

fn required_field<T>(
    obj: &serde_json::Map<String, Value>,
    key: &str,
    raw: &Value,
) -> Result<T, crate::error::ThinClientError>
where
    T: serde::de::DeserializeOwned,
{
    let value = obj
        .get(key)
        .cloned()
        .ok_or_else(|| crate::error::ThinClientError::InvalidSseJson(raw.clone()))?;
    serde_json::from_value(value)
        .map_err(|_| crate::error::ThinClientError::InvalidSseJson(raw.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn heartbeat_response_decodes_current_identity_only_contract() {
        let response: EdgeHeartbeatResponse = serde_json::from_value(json!({
            "ok": true,
            "user_id": "user-1",
            "edge_id": "transport-1",
            "edge_agent_id": "edge-1",
            "unresolved_request_ids": ["invocation-1"],
            "replay_policy": "durable_result_reconciliation_required",
            "ack_request_ids": ["invocation-2"]
        }))
        .expect("current heartbeat response");

        assert!(response.requires_reconciliation());
        assert_eq!(
            response.replay_policy,
            EdgeHeartbeatReplayPolicy::DurableResultReconciliationRequired
        );
        assert!(response.legacy_pending_requests.is_empty());
    }

    #[test]
    fn heartbeat_response_decodes_legacy_pending_payload_as_non_executable_evidence() {
        let response: EdgeHeartbeatResponse = serde_json::from_value(json!({
            "ok": true,
            "user_id": "user-1",
            "edge_id": "transport-1",
            "edge_agent_id": "edge-1",
            "pending_requests": [{
                "request_id": "legacy-request",
                "tool_name": "bash",
                "args": {"cmd": "echo must-not-run"}
            }]
        }))
        .expect("legacy heartbeat response");

        assert!(response.requires_reconciliation());
        assert_eq!(
            response.replay_policy,
            EdgeHeartbeatReplayPolicy::LegacyPendingPayloadRequiresManualReconciliation
        );
        assert_eq!(
            response.legacy_pending_requests,
            vec![LegacyEdgePendingRequest {
                request_id: "legacy-request".to_string(),
            }]
        );
    }

    #[test]
    fn heartbeat_response_unknown_policy_fails_safe_without_breaking_heartbeat() {
        let response: EdgeHeartbeatResponse = serde_json::from_value(json!({
            "ok": true,
            "user_id": "user-1",
            "edge_id": "transport-1",
            "edge_agent_id": "edge-1",
            "replay_policy": "future_provider_attested_reconciliation"
        }))
        .expect("unknown policy must remain forward compatible");

        assert_eq!(response.replay_policy, EdgeHeartbeatReplayPolicy::Unknown);
        assert!(
            response.requires_reconciliation(),
            "unknown policy must never imply automatic replay authority"
        );
    }

    #[test]
    fn chat_stream_request_serde_roundtrip() {
        let r = ChatStreamRequest {
            message: "hi".into(),
            conversation_authority: None,
            parts: vec![json!({"type": "text", "text": "hi"})],
            attachments: vec![json!({"id": "att-1", "kind": "file"})],
            session_id: Some("s-1".into()),
            agent_id: None,
            model_selection: ModelSelection {
                offering_id: "offer-m".into(),
            },
            interaction_mode: Some("auto".into()),
            context: None,
            execution_budget: Some(ExecutionBudget {
                initial_turns: Some(3),
                hard_turn_limit: Some(6),
            }),
            explain: true,
            plan_subtask_id: Some("t1".into()),
            is_plan_subtask: Some(true),
            edge_executor_id: Some("edge-1".into()),
            capabilities: vec!["bash".into(), "fs".into()],
        };
        let j = serde_json::to_value(&r).unwrap();
        let back: ChatStreamRequest = serde_json::from_value(j).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn chat_stream_request_requires_model_selection_and_defaults_leave_execution_budget_unset() {
        let missing_model = serde_json::json!({"message":"x"});
        serde_json::from_value::<ChatStreamRequest>(missing_model)
            .expect_err("model_selection is required in the thin client wire type");

        let j = serde_json::json!({"message":"x","model_selection":{"offering_id":"offer-m"}});
        let r: ChatStreamRequest = serde_json::from_value(j).unwrap();
        assert!(r.execution_budget.is_none());
        assert_eq!(r.model_selection.offering_id, "offer-m");
    }

    #[test]
    fn chat_stream_request_roundtrip_preserves_execution_budget() {
        let j = serde_json::json!({
            "message": "x",
            "model_selection": {"offering_id": "offer-m"},
            "execution_budget": {"initial_turns": 4, "hard_turn_limit": 9}
        });
        let r: ChatStreamRequest = serde_json::from_value(j).unwrap();
        assert_eq!(
            r.execution_budget,
            Some(ExecutionBudget {
                initial_turns: Some(4),
                hard_turn_limit: Some(9),
            })
        );
    }

    #[test]
    fn approval_respond_request_roundtrip_preserves_context() {
        let req = ApprovalRespondRequest {
            request_id: "ap-1".into(),
            decision: ApprovalDecision::Allow,
            reason: Some("looks good".into()),
            session_id: "sess-1".into(),
            run_id: "run-1".into(),
            tool_name: Some("write_file".into()),
            approval_kind: Some(ApprovalKind::Standard),
        };
        let json = serde_json::to_value(&req).unwrap();
        let back: ApprovalRespondRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn user_prompt_response_and_required_stream_event_preserve_typed_identity() {
        let response = UserPromptRespondRequest {
            request_id: "prompt-1".into(),
            session_id: "sess-1".into(),
            run_id: "run-1".into(),
            cancelled: false,
            answers: Some(json!({"answers": []})),
        };
        let round_trip: UserPromptRespondRequest =
            serde_json::from_value(serde_json::to_value(&response).unwrap()).unwrap();
        assert_eq!(round_trip, response);

        match classify_stream_event(json!({
            "type": "user_prompt_required",
            "request_id": "prompt-1",
            "run_id": "run-1",
            "prompt": {"questions": [{"question": "Continue?"}]}
        }))
        .unwrap()
        {
            StreamEvent::UserPromptRequired {
                request_id,
                run_id,
                prompt,
                ..
            } => {
                assert_eq!(request_id, "prompt-1");
                assert_eq!(run_id.as_deref(), Some("run-1"));
                assert_eq!(prompt["questions"][0]["question"], "Continue?");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn approval_respond_request_requires_session_and_run_id() {
        let json = serde_json::json!({
            "request_id": "ap-legacy",
            "decision": "deny",
            "reason": "no"
        });
        assert!(serde_json::from_value::<ApprovalRespondRequest>(json).is_err());
    }

    #[test]
    fn approval_respond_request_rejects_missing_session_id() {
        let json = serde_json::json!({
            "request_id": "ap-minimal",
            "decision": "deny",
            "reason": "no",
            "run_id": "run-minimal"
        });
        assert!(serde_json::from_value::<ApprovalRespondRequest>(json).is_err());
    }

    #[test]
    fn classify_session_info() {
        let v = serde_json::json!({"type":"session_info","session_id":"a","run_id":"b"});
        match classify_stream_event(v).unwrap() {
            StreamEvent::SessionInfo { session_id, run_id } => {
                assert_eq!(session_id, "a");
                assert_eq!(run_id.as_deref(), Some("b"));
            }
            e => panic!("unexpected {e:?}"),
        }
    }

    #[test]
    fn classify_session_info_without_run_id() {
        let v = serde_json::json!({"type":"session_info","session_id":"a"});
        match classify_stream_event(v).unwrap() {
            StreamEvent::SessionInfo { session_id, run_id } => {
                assert_eq!(session_id, "a");
                assert_eq!(run_id, None);
            }
            e => panic!("unexpected {e:?}"),
        }
    }

    #[test]
    fn classify_tool_request_design_shape() {
        let v = serde_json::json!({
            "type": "tool_request",
            "session_id": "s1",
            "run_id": "r1",
            "turn_chain_id": "chain1",
            "request_id": "tr-1",
            "tool": "bash",
            "args": {"command": "ls"}
        });
        match classify_stream_event(v).unwrap() {
            StreamEvent::ToolRequest {
                session_id,
                run_id,
                turn_chain_id,
                request_id,
                tool,
                args,
            } => {
                assert_eq!(session_id, "s1");
                assert_eq!(run_id, "r1");
                assert_eq!(turn_chain_id, "chain1");
                assert_eq!(request_id, "tr-1");
                assert_eq!(tool, "bash");
                assert_eq!(args["command"], "ls");
            }
            e => panic!("unexpected {e:?}"),
        }
    }

    #[test]
    fn classify_approval_required_preserves_approval_kind() {
        let v = serde_json::json!({
            "type": "approval_required",
            "request_id": "ap-1",
            "tool": "bash",
            "approval_kind": "explicit",
            "detail": "rm -rf tmp"
        });
        match classify_stream_event(v).unwrap() {
            StreamEvent::ApprovalRequired {
                request_id,
                tool,
                approval_kind,
                detail,
                ..
            } => {
                assert_eq!(request_id, "ap-1");
                assert_eq!(tool, "bash");
                assert_eq!(approval_kind, ApprovalKind::Explicit);
                assert_eq!(detail.as_deref(), Some("rm -rf tmp"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn classify_approval_required_without_kind_defaults_to_explicit() {
        let v = serde_json::json!({
            "type": "approval_required",
            "request_id": "ap-legacy",
            "tool": "write_file",
            "path": "src/lib.rs"
        });
        match classify_stream_event(v).unwrap() {
            StreamEvent::ApprovalRequired {
                approval_kind,
                detail,
                ..
            } => {
                assert_eq!(approval_kind, ApprovalKind::Explicit);
                assert_eq!(detail.as_deref(), Some("src/lib.rs"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn classify_tool_call_start_preserves_arguments() {
        let value = serde_json::json!({
            "type":"tool_call_start",
            "tool":"bash",
            "call_id":"c1",
            "arguments":"{\"command\":\"ls\"}"
        });
        match classify_stream_event(value).unwrap() {
            StreamEvent::ToolCallStart {
                tool,
                call_id,
                arguments,
            } => {
                assert_eq!(tool, "bash");
                assert_eq!(call_id, "c1");
                assert_eq!(
                    arguments,
                    Some(Value::String("{\"command\":\"ls\"}".to_string()))
                );
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn classify_tool_call_end_and_legacy_tool_result() {
        for value in [
            serde_json::json!({"type":"tool_call_end","call_id":"c1","result":"ok"}),
            serde_json::json!({"type":"tool_result","call_id":"c2","result":"legacy"}),
        ] {
            match classify_stream_event(value).unwrap() {
                StreamEvent::ToolCallEnd { call_id, result } => {
                    assert!(call_id == "c1" || call_id == "c2");
                    assert!(result == "ok" || result == "legacy");
                }
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    #[test]
    fn classify_reasoning_done() {
        match classify_stream_event(serde_json::json!({"type":"reasoning_done"})).unwrap() {
            StreamEvent::ReasoningDone => {}
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn classify_reasoning_delta() {
        match classify_stream_event(serde_json::json!({
            "type":"reasoning_delta",
            "content":"thinking"
        }))
        .unwrap()
        {
            StreamEvent::ReasoningDelta { content } => assert_eq!(content, "thinking"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn classify_agent_events() {
        let spawned = serde_json::json!({
            "type": "agent_spawned",
            "agent_id": "agent-1",
            "run_id": "run-1",
            "parent_run_id": "root-1",
            "agent_type": "worker",
            "description": "Investigate",
            "timestamp": 123
        });
        match classify_stream_event(spawned).unwrap() {
            StreamEvent::AgentSpawned {
                agent_id,
                run_id,
                parent_run_id,
                agent_type,
                description,
                timestamp,
                raw,
            } => {
                assert_eq!(agent_id, "agent-1");
                assert_eq!(run_id, "run-1");
                assert_eq!(parent_run_id, "root-1");
                assert_eq!(agent_type, "worker");
                assert_eq!(description, "Investigate");
                assert_eq!(timestamp, Some(123));
                assert_eq!(raw["type"], "agent_spawned");
            }
            other => panic!("unexpected {other:?}"),
        }

        let progress = serde_json::json!({
            "type": "agent_progress",
            "agent_id": "agent-1",
            "status": "started",
            "description": "Reading files",
            "timestamp": 456
        });
        match classify_stream_event(progress).unwrap() {
            StreamEvent::AgentProgress {
                agent_id,
                status,
                raw,
            } => {
                assert_eq!(agent_id, "agent-1");
                assert_eq!(status.as_deref(), Some("started"));
                assert_eq!(raw["description"], "Reading files");
                assert_eq!(raw["timestamp"], 456);
            }
            other => panic!("unexpected {other:?}"),
        }

        let completed = serde_json::json!({
            "type": "agent_completed",
            "agent_id": "agent-1",
            "status": "failed",
            "error": "boom",
            "timestamp": 789
        });
        match classify_stream_event(completed).unwrap() {
            StreamEvent::AgentCompleted {
                agent_id,
                status,
                raw,
            } => {
                assert_eq!(agent_id, "agent-1");
                assert_eq!(status.as_deref(), Some("failed"));
                assert_eq!(raw["error"], "boom");
                assert_eq!(raw["timestamp"], 789);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn classify_agent_communication_preserves_typed_identity() {
        let value = serde_json::json!({
            "type": "agent_communication",
            "schema_version": "astra.agent_communication.v1",
            "observed_by": {"run_id": "run-review", "agent_id": "reviewer"},
            "direction": "received",
            "message_id": "msg-1",
            "from": {"run_id": "run-code", "agent_id": "coder"},
            "to": {"kind": "direct", "address": {"run_id": "run-review", "agent_id": "reviewer"}},
            "payload_kind": "text",
            "summary": "Please review the patch",
            "timestamp_ms": 42,
            "requires_ack": true
        });

        let StreamEvent::AgentCommunication(event) = classify_stream_event(value).unwrap() else {
            panic!("expected typed agent communication event");
        };
        assert_eq!(event.observed_by.run_id, "run-review");
        assert_eq!(event.observed_by.agent_id, "reviewer");
        assert_eq!(event.from.run_id, "run-code");
        assert_eq!(
            event.direction,
            astra_turn_types::AgentCommunicationDirection::Received
        );
        assert_eq!(event.summary.as_deref(), Some("Please review the patch"));
    }

    #[test]
    fn classify_usage() {
        let value = serde_json::json!({
            "type": "usage",
            "input_tokens": 10,
            "output_tokens": 4,
            "cached_input_tokens": 1,
            "cache_creation_tokens": 3,
            "total_tokens": 18,
            "tool_call_count": 2,
        });
        match classify_stream_event(value).unwrap() {
            StreamEvent::Usage {
                input_tokens,
                output_tokens,
                cached_input_tokens,
                cache_creation_tokens,
                total_tokens,
                tool_call_count,
                raw,
            } => {
                assert_eq!(input_tokens, Some(10));
                assert_eq!(output_tokens, Some(4));
                assert_eq!(cached_input_tokens, Some(1));
                assert_eq!(cache_creation_tokens, Some(3));
                assert_eq!(total_tokens, Some(18));
                assert_eq!(tool_call_count, Some(2));
                assert_eq!(raw["type"], "usage");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn classify_turn_complete_warning_and_explain() {
        match classify_stream_event(serde_json::json!({
            "type": "turn_complete",
            "assistant_text": "Recovered final text",
            "followup_suggestion": "Try /plan"
        }))
        .unwrap()
        {
            StreamEvent::TurnComplete {
                assistant_text,
                followup_suggestion,
                raw,
            } => {
                assert_eq!(assistant_text.as_deref(), Some("Recovered final text"));
                assert_eq!(followup_suggestion.as_deref(), Some("Try /plan"));
                assert_eq!(raw["type"], "turn_complete");
            }
            other => panic!("unexpected {other:?}"),
        }

        match classify_stream_event(serde_json::json!({
            "type": "warning",
            "message": "approaching limit",
            "claims_failed": 2
        }))
        .unwrap()
        {
            StreamEvent::Warning {
                message,
                claims_failed,
                raw,
            } => {
                assert_eq!(message, "approaching limit");
                assert_eq!(claims_failed, Some(2));
                assert_eq!(raw["type"], "warning");
            }
            other => panic!("unexpected {other:?}"),
        }

        match classify_stream_event(serde_json::json!({
            "type": "explain",
            "content": "why this happened"
        }))
        .unwrap()
        {
            StreamEvent::Explain { content, raw } => {
                assert_eq!(content, "why this happened");
                assert_eq!(raw["type"], "explain");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn classify_run_lifecycle_events() {
        let started = serde_json::json!({
            "type": "run_started",
            "run_id": "run-1",
            "session_id": "sess-1"
        });
        match classify_stream_event(started).unwrap() {
            StreamEvent::RunStarted { run_id, session_id } => {
                assert_eq!(run_id.as_deref(), Some("run-1"));
                assert_eq!(session_id.as_deref(), Some("sess-1"));
            }
            other => panic!("unexpected {other:?}"),
        }

        let finished = serde_json::json!({
            "type": "run_finished",
            "run_id": "run-1",
            "status": "failed",
            "error": "boom"
        });
        match classify_stream_event(finished).unwrap() {
            StreamEvent::RunFinished {
                run_id,
                status,
                error,
            } => {
                assert_eq!(run_id.as_deref(), Some("run-1"));
                assert_eq!(status.as_deref(), Some("failed"));
                assert_eq!(error.as_deref(), Some("boom"));
            }
            other => panic!("unexpected {other:?}"),
        }

        let run_error = serde_json::json!({
            "type": "run_error",
            "message": "boom",
            "error_kind": "tool_failure"
        });
        match classify_stream_event(run_error).unwrap() {
            StreamEvent::RunError {
                message,
                error_kind,
                ..
            } => {
                assert_eq!(message, "boom");
                assert_eq!(error_kind.as_deref(), Some("tool_failure"));
            }
            other => panic!("unexpected {other:?}"),
        }

        let waiting = serde_json::json!({
            "type": "run_waiting",
            "run_id": "run-1",
            "reason": "waiting: executor_offline"
        });
        match classify_stream_event(waiting).unwrap() {
            StreamEvent::RunWaiting { run_id, reason } => {
                assert_eq!(run_id.as_deref(), Some("run-1"));
                assert_eq!(reason.as_deref(), Some("waiting: executor_offline"));
            }
            other => panic!("unexpected {other:?}"),
        }

        let interrupted = serde_json::json!({
            "type": "run_interrupted",
            "run_id": "run-1",
            "kind": "budget_exhausted",
            "resumable": true,
            "message": "You can continue in the next message."
        });
        match classify_stream_event(interrupted).unwrap() {
            StreamEvent::RunInterrupted {
                run_id,
                kind,
                resumable,
                message,
                ..
            } => {
                assert_eq!(run_id.as_deref(), Some("run-1"));
                assert_eq!(kind.as_deref(), Some("budget_exhausted"));
                assert_eq!(resumable, Some(true));
                assert_eq!(
                    message.as_deref(),
                    Some("You can continue in the next message.")
                );
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn classify_run_user_intent_lifecycle_preserves_run_and_event_identity() {
        let accepted = classify_stream_event(serde_json::json!({
            "type": "user_intent_accepted",
            "run_id": "run-child",
            "intent_id": "intent-7",
            "delivery": "guide_current_run",
            "status": "accepted_remote",
            "index": 12
        }))
        .unwrap();
        assert!(matches!(
            accepted,
            StreamEvent::RunUserIntentAccepted {
                run_id,
                intent_id,
                index: 12,
                ..
            } if run_id == "run-child" && intent_id == "intent-7"
        ));

        let applied = classify_stream_event(serde_json::json!({
            "type": "user_intent_applied",
            "run_id": "run-child",
            "intent_id": "intent-7",
            "delivery": "guide_current_run",
            "status": "applied",
            "event_index": 7,
            "content": "inspect the failing test",
            "index": 13
        }))
        .unwrap();
        assert!(matches!(
            applied,
            StreamEvent::RunUserIntentApplied {
                run_id,
                intent_id,
                event_index: 7,
                content,
                index: 13,
                ..
            } if run_id == "run-child"
                && intent_id == "intent-7"
                && content == "inspect the failing test"
        ));
    }

    #[test]
    fn malformed_run_user_intent_event_is_not_downgraded_to_other() {
        let error = classify_stream_event(serde_json::json!({
            "type": "user_intent_applied",
            "run_id": "run-child",
            "intent_id": "intent-7",
            "delivery": "guide_current_run",
            "status": "accepted_remote",
            "event_index": 7,
            "content": "wrong lifecycle status",
            "index": 13
        }))
        .expect_err("applied event with accepted status must fail closed");
        assert!(matches!(
            error,
            crate::error::ThinClientError::InvalidSseJson(_)
        ));
    }

    #[test]
    fn classify_error_event() {
        let v = serde_json::json!({
            "type": "error",
            "message": "nope",
            "code": "AUTH_ERROR",
            "retryable": false
        });
        match classify_stream_event(v).unwrap() {
            StreamEvent::Error {
                message,
                code,
                retryable,
                ..
            } => {
                assert_eq!(message, "nope");
                assert_eq!(code.as_deref(), Some("AUTH_ERROR"));
                assert!(!retryable);
            }
            e => panic!("unexpected {e:?}"),
        }
    }

    #[test]
    fn classify_unknown_type_preserved() {
        let v = serde_json::json!({"type":"future_event","foo": 1});
        match classify_stream_event(v).unwrap() {
            StreamEvent::Other { event_type, raw } => {
                assert_eq!(event_type, "future_event");
                assert_eq!(raw["foo"], 1);
            }
            e => panic!("unexpected {e:?}"),
        }
    }

    // ── ToolResultRequest serialization / parse_output_and_error ──────

    #[test]
    fn tool_result_parse_output_and_error_success() {
        let json = r#"{"request_id":"r1","edge_agent_id":"agt","status":"success","output":"hello","duration_ms":42}"#;
        let (output, is_error) = ToolResultRequest::parse_output_and_error(json);
        assert_eq!(output, "hello");
        assert!(!is_error);
    }

    #[test]
    fn tool_result_parse_output_and_error_error_status() {
        let json = r#"{"request_id":"r1","edge_agent_id":"agt","status":"error","output":"fail"}"#;
        let (output, is_error) = ToolResultRequest::parse_output_and_error(json);
        assert_eq!(output, "fail");
        assert!(is_error);
    }

    #[test]
    fn tool_result_parse_output_and_error_legacy_failed_status() {
        let json = r#"{"request_id":"r1","edge_agent_id":"agt","status":"failed","output":"fail"}"#;
        let (output, is_error) = ToolResultRequest::parse_output_and_error(json);
        assert_eq!(output, "fail");
        assert!(is_error);
    }

    #[test]
    fn tool_result_parse_output_and_error_non_json_fallback() {
        // When input is not JSON, fallback uses the whole string as output
        let (output, is_error) = ToolResultRequest::parse_output_and_error("plain text result");
        assert_eq!(output, "plain text result");
        assert!(!is_error);
    }

    #[test]
    fn tool_result_parse_output_and_error_missing_output() {
        let json = r#"{"request_id":"r1","edge_agent_id":"agt","status":"success"}"#;
        let (output, is_error) = ToolResultRequest::parse_output_and_error(json);
        assert_eq!(output, "");
        assert!(!is_error);
    }

    #[test]
    fn tool_result_new_with_hash_includes_edge_agent_id() {
        let req = ToolResultRequest::new_with_hash(ToolResultRequestParts {
            session_id: "sess-1".into(),
            run_id: "run-1".into(),
            turn_chain_id: "chain-1".into(),
            request_id: "req-1".into(),
            edge_agent_id: "agent-1".into(),
            status: "success".into(),
            output: "done".into(),
            duration_ms: 100,
            tool_result_fields: None,
        });
        assert_eq!(req.session_id, "sess-1");
        assert_eq!(req.run_id, "run-1");
        assert_eq!(req.turn_chain_id, "chain-1");
        assert_eq!(req.request_id, "req-1");
        assert_eq!(req.edge_agent_id, "agent-1");
        assert_eq!(req.output, "done");
        assert!(!req.result_hash.is_empty());
    }

    #[test]
    fn tool_result_new_with_hash_and_fields_preserves_metadata() {
        let fields = Map::from_iter([(
            "runtime_environment_advertisement".to_string(),
            serde_json::json!({"schema_version": 1}),
        )]);
        let req = ToolResultRequest::new_with_hash(ToolResultRequestParts {
            session_id: "sess-1".into(),
            run_id: "run-1".into(),
            turn_chain_id: "chain-1".into(),
            request_id: "req-1".into(),
            edge_agent_id: "agent-1".into(),
            status: "success".into(),
            output: "done".into(),
            duration_ms: 100,
            tool_result_fields: Some(fields),
        });
        assert_eq!(
            req.tool_result_fields
                .as_ref()
                .and_then(|fields| fields.get("runtime_environment_advertisement"))
                .and_then(|value| value.get("schema_version"))
                .and_then(Value::as_u64),
            Some(1)
        );
    }

    #[test]
    fn tool_result_serde_roundtrip_preserves_edge_agent_id() {
        let req = ToolResultRequest::new_with_hash(ToolResultRequestParts {
            session_id: "sess-1".into(),
            run_id: "run-1".into(),
            turn_chain_id: "chain-1".into(),
            request_id: "r1".into(),
            edge_agent_id: "ea-1".into(),
            status: "success".into(),
            output: "ok".into(),
            duration_ms: 10,
            tool_result_fields: None,
        });
        let json = serde_json::to_string(&req).unwrap();
        let back: ToolResultRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn tool_result_deser_missing_identity_is_rejected() {
        let json = r#"{"request_id":"r1","edge_agent_id":"ea-1","status":"success","output":"ok","duration_ms":10,"result_hash":"h"}"#;
        assert!(serde_json::from_str::<ToolResultRequest>(json).is_err());
    }

    #[test]
    fn tool_result_deser_null_edge_agent_id_is_rejected() {
        let json = r#"{"session_id":"s1","run_id":"r1","turn_chain_id":"c1","request_id":"req1","edge_agent_id":null,"status":"success","output":"ok","duration_ms":10,"result_hash":"h"}"#;
        assert!(serde_json::from_str::<ToolResultRequest>(json).is_err());
    }

    #[test]
    fn transcript_item_decodes_legacy_and_typed_tool_evidence() {
        let legacy: SessionTranscriptItem = serde_json::from_value(serde_json::json!({
            "session_id": "session-1",
            "item_seq": 1,
            "run_id": "run-1",
            "role": "assistant",
            "content": "done",
            "created_at": "2026-07-12T00:00:00"
        }))
        .unwrap();
        assert!(legacy.tool_calls.is_empty());
        assert!(legacy.tool_result.is_none());
        assert!(legacy.source_event_id.is_none());

        let typed: SessionTranscriptItem = serde_json::from_value(serde_json::json!({
            "session_id": "session-1",
            "item_seq": -1,
            "run_id": "run-1",
            "role": "assistant",
            "content": "",
            "tool_calls": [{
                "tool_use_id": "call-1",
                "name": "read_file",
                "arguments": "{\"path\":\"src/lib.rs\"}"
            }],
            "source_event_id": "event-call-1",
            "created_at": "2026-07-12T00:00:00"
        }))
        .unwrap();
        assert_eq!(typed.tool_calls[0].name, "read_file");
        assert_eq!(typed.source_event_id.as_deref(), Some("event-call-1"));
    }

    #[test]
    fn session_update_serializes_metadata_patch_without_replacement_metadata() {
        let request = SessionUpdateRequest {
            title: None,
            metadata: None,
            metadata_patch: Some(Map::from_iter([
                ("current_model".to_string(), serde_json::json!("m2")),
                ("workspace_selection".to_string(), Value::Null),
            ])),
            status: None,
        };

        let value = serde_json::to_value(request).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "metadata_patch": {
                    "current_model": "m2",
                    "workspace_selection": null
                }
            })
        );
    }
}

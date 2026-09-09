//! JSON `type` dispatch for astra `/chat/turn` SSE event blocks (blank-line framed).
//!
//! Shared between the CLI stream consumer and any future headless client: updates a structured
//! accumulator and returns terminal UI hints. [`ChatTurnSseFramer`] turns arbitrary byte chunks
//! into complete event blocks via [`astra_inference_adapter::sse::blocks`] and records time-to-first-token.

use astra_core::canonical_names::{normalize_name, normalize_name_list};
use astra_thin_client::ApprovalKind;
use serde_json::Value;
use std::time::Instant;

use crate::compaction_types::{CompactionKind, CompactionTier};
use crate::context_feedback::RuntimeFeedbackFrame;
use crate::tool_ledger_receipt::ToolLedgerReceipt;
use astra_inference_adapter::sse::blocks::{SseBlankLineUtf8Buf, SseUtf8Error};

/// Per-channel fingerprint emitted by the bridge via the
/// `injection_freshness` SSE event. Carries only opaque metadata
/// (content hash, byte length, empty-flag) — never the raw channel
/// text. wip-7 migrated away from the previous raw-text payload because
/// the external `transform_run_event_for_client` transform passed the
/// event through verbatim, leaking learned feedback rules, memoria recall
/// digests and self-awareness summaries to
/// any authenticated `/chat/turn` client.
///
/// The fingerprint is enough for `ObservabilitySession` to detect
/// content change (for the freshness report). The raw preview is
/// derived CLI-side from channels the CLI already owns
/// (`lessons`, `self_awareness`,
/// `recent_arg_hints`, `skill_listing`). Bridge-internal channels
/// (`memoria_prefetch`, `tool_round_guidance`, `volatile`) carry an empty preview in the
/// CLI history — introspect still sees tag + hash + bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InjectionChannelFingerprint {
    pub tag: String,
    pub hash: u64,
    pub bytes: u64,
    pub is_empty: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InjectionFingerprints {
    pub channels: Vec<InjectionChannelFingerprint>,
}

/// One context compaction that changed the provider-visible prompt.
///
/// The bridge may emit `context_meta` more than once for one HTTP turn, so
/// `id` is stable within that turn and lets consumers de-duplicate repeated
/// snapshots without conflating distinct retry compactions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextCompactionEffectiveness {
    /// Older producers did not carry the resolved window policy.
    #[default]
    Unmeasured,
    /// Occupancy landed at least ten percentage points below the trigger.
    Sufficient,
    /// Tokens were removed, but occupancy did not reach the policy target.
    Insufficient,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContextCompactionObservation {
    pub id: String,
    pub kind: CompactionKind,
    pub tier: CompactionTier,
    pub messages_before: u64,
    pub messages_after: u64,
    pub tokens_before: u64,
    pub tokens_after: u64,
    pub tokens_saved: u64,
    #[serde(default)]
    pub post_compaction_target_tokens: Option<u64>,
    #[serde(default)]
    pub effectiveness: ContextCompactionEffectiveness,
}

impl ContextCompactionObservation {
    /// Whether all redundant counters agree with one another.
    ///
    /// This validates only typed structural facts; it never interprets
    /// provider text or human-facing labels.
    #[must_use]
    pub fn is_consistent(&self) -> bool {
        let effectiveness_consistent =
            match (self.post_compaction_target_tokens, self.effectiveness) {
                (None, ContextCompactionEffectiveness::Unmeasured) => true,
                (Some(target), ContextCompactionEffectiveness::Sufficient) => {
                    self.tokens_after <= target
                }
                (Some(target), ContextCompactionEffectiveness::Insufficient) => {
                    self.tokens_after > target
                }
                _ => false,
            };
        !self.id.trim().is_empty()
            && self.messages_before >= self.messages_after
            && self.tokens_before >= self.tokens_after
            && self.tokens_saved == self.tokens_before - self.tokens_after
            && effectiveness_consistent
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum StreamRootAuthority {
    #[default]
    Unbound,
    RunStartedFallback,
    SessionInfo,
}

/// Immutable physical owner identity for one SSE response.
///
/// Per-event `run_id` values may describe fanout descendants. They must never
/// replace this root. `session_info` is authoritative; `run_started` is only a
/// compatibility fallback for replay/legacy streams without the bootstrap.
#[doc(hidden)]
#[derive(Debug, Clone, Default)]
pub struct StreamRootIdentity {
    run_id: Option<String>,
    authority: StreamRootAuthority,
}

impl StreamRootIdentity {
    pub(crate) fn run_id(&self) -> Option<&str> {
        self.run_id.as_deref()
    }

    pub(crate) fn observe_event(&mut self, event: &Value) -> Result<(), String> {
        let Some(run_id) = event
            .get("run_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return Ok(());
        };
        match event.get("type").and_then(Value::as_str) {
            Some("session_info") => match self.authority {
                StreamRootAuthority::SessionInfo
                    if self.run_id.as_deref().is_some_and(|bound| bound != run_id) =>
                {
                    let bound = self.run_id.as_deref().unwrap_or_default();
                    Err(format!(
                        "one SSE stream changed root run identity from `{bound}` to `{run_id}`"
                    ))
                }
                _ => {
                    self.run_id = Some(run_id.to_string());
                    self.authority = StreamRootAuthority::SessionInfo;
                    Ok(())
                }
            },
            Some("run_started") if self.authority == StreamRootAuthority::Unbound => {
                let is_descendant = event
                    .get("parent_run_id")
                    .and_then(Value::as_str)
                    .is_some_and(|parent| !parent.trim().is_empty());
                if !is_descendant {
                    self.run_id = Some(run_id.to_string());
                    self.authority = StreamRootAuthority::RunStartedFallback;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    pub(crate) fn bind_legacy_hint(&mut self, run_id: &str) -> Result<(), String> {
        if self.authority == StreamRootAuthority::Unbound {
            self.run_id = Some(run_id.to_string());
            self.authority = StreamRootAuthority::RunStartedFallback;
            return Ok(());
        }
        if self.run_id.as_deref() == Some(run_id) {
            return Ok(());
        }
        let bound = self.run_id.as_deref().unwrap_or_default();
        Err(format!(
            "SSE root `{bound}` disagrees with accumulated root `{run_id}`"
        ))
    }
}

/// State collected from one `/chat/turn` SSE stream (excluding edge executor bookkeeping).
#[derive(Debug, Clone, Default)]
pub struct ChatTurnSseAccum {
    pub session_id: Option<String>,
    pub run_id: Option<String>,
    /// Durable guidance that the exact physical root incorporated while the
    /// server owned the loop. Thin clients must retain these facts in the
    /// local turn commit; otherwise restart silently reconstructs a different
    /// conversation from the server's canonical transcript.
    pub applied_user_intents: Vec<StreamAppliedUserIntent>,
    #[doc(hidden)]
    pub root_identity: StreamRootIdentity,
    /// Terminal lifecycle fact for the durable run that owns this physical
    /// stream.  `run_finished` is independent of the provider `[DONE]`
    /// marker: cancellation can arrive while an Edge tool is still running,
    /// and clients must not turn it into an ordinary tool failure followed by
    /// another model round.
    pub run_terminal: Option<DurableRunTerminal>,
    pub full_text: String,
    /// Thinking / reasoning chunks (for models that stream reasoning separately).
    pub reasoning_content: String,
    /// Renderer edge state. A provider may emit tens of thousands of
    /// reasoning deltas, but starting the spinner is a state transition, not
    /// a per-chunk event.
    #[doc(hidden)]
    pub thinking_active: bool,
    /// Bedrock reasoning signature — must be passed back unmodified in multi-turn.
    pub reasoning_signature: String,
    pub tool_calls: Vec<Value>,
    /// Index from tool_call id -> position in `tool_calls` for O(1) merges.
    pub tool_call_id_index: std::collections::HashMap<String, usize>,
    pub explain_turns: Vec<Value>,
    pub has_tool_calls: bool,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub has_usage: bool,
    /// `true` when the visible usage is an aggregate rather than one physical
    /// provider exchange (for example a server-owned run or a bounded retry).
    pub usage_is_run_total: bool,
    /// Usage for the most recent physical model request. This is distinct
    /// from aggregate logical-response usage: the latter is useful for
    /// accounting but cannot describe the active context window after a tool
    /// loop or bounded retry.
    pub current_request_usage: Option<astra_turn_types::RequestTokenUsage>,
    pub error_message: Option<String>,
    pub error_code: Option<String>,
    pub error_metadata: Option<Value>,
    pub error_kind: Option<astra_core::ErrorKind>,
    /// A streamed attempt error awaiting the exact durable root outcome.
    /// Only a coherent Completed/Paused owner terminal may supersede it.
    #[doc(hidden)]
    pub pending_attempt_error: bool,
    /// Sealed at an exact-root Completed/Paused durable terminal. A later
    /// error revokes eligibility so `turn_complete` cannot erase an event
    /// that happened after the owner terminal.
    #[doc(hidden)]
    pub attempt_error_supersession_eligible: bool,
    /// System prompt token estimate from runtime (via `context_meta` SSE event).
    pub system_prompt_tokens: Option<u32>,
    /// Detailed system prompt breakdown from runtime (via `context_meta` SSE event).
    pub system_prompt_breakdown: Option<Value>,
    /// Lightweight manifest trace from the shared LLM context assembler.
    pub context_manifest_trace: Option<Value>,
    /// Exact tool names attached to the first physical provider request by
    /// the runtime. Thin clients assemble only the Edge-owned portion of the
    /// request, so their preflight report is not authoritative after the
    /// server merges server-owned tools such as the Work lifecycle surface.
    pub provider_visible_tools: Option<Vec<String>>,
    /// Distinct compactions observed while assembling or retrying this request.
    pub context_compactions: Vec<ContextCompactionObservation>,
    /// Per-turn injection-channel fingerprints captured from the
    /// bridge's `injection_freshness` SSE event. `None` until the
    /// event fires. wip-7 contract: the CLI MUST NOT default this to
    /// an empty bundle when the event is missing — that would mask a
    /// broken observation pipe by reporting every bridge channel as
    /// `Empty` in the freshness report. Stays `None` → downstream
    /// observers skip those channels (history remains `Untracked`).
    pub injection_fingerprints: Option<InjectionFingerprints>,
    /// The lifecycle-effective model `finish_reason` from the final response.
    /// `"stop"` denotes natural completion, `"length"` denotes a typed
    /// output-cap boundary, and `"tool_calls"` denotes a tool request.  When
    /// a provider omitted its reason at an exact wire cap, the raw omission is
    /// retained in the provider exchange capture while this client-facing
    /// field carries the conservative lifecycle interpretation.
    pub finish_reason: Option<String>,
    /// Whether the transport emitted its terminal `[DONE]` marker. This is
    /// intentionally distinct from a model finish reason: once set, later
    /// bytes belong to no valid part of this response and must not mutate the
    /// accumulated answer or queue edge work.
    pub stream_complete: bool,
    /// True only when a typed terminal event states that the Server already
    /// owned every continuation round. Clients use this authority fact to
    /// avoid executing observed tool calls a second time.
    pub server_loop_terminal: bool,
    /// Typed interruption emitted by a Server-owned terminal loop.  This is
    /// distinct from human-facing partial text: clients must preserve the
    /// lifecycle fact even when the server also rendered a useful summary.
    pub server_interruption: Option<Value>,
    /// Authoritative execution summary for a Server-owned continuation loop.
    /// Kept separate from `tool_calls`, which contains pending calls a client
    /// may need to execute and is deliberately cleared at a server terminal.
    pub server_execution_summary: Option<ServerLoopExecutionSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamAppliedUserIntent {
    pub intent_id: String,
    pub delivery: astra_turn_types::UserIntentDelivery,
    pub event_index: usize,
    pub content: String,
}

/// Typed terminal state carried by a durable `run_finished` SSE event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurableRunTerminalStatus {
    Completed,
    Cancelled,
    Failed,
    Delegated,
    Paused,
}

impl DurableRunTerminalStatus {
    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "completed" => Some(Self::Completed),
            "cancelled" => Some(Self::Cancelled),
            "failed" => Some(Self::Failed),
            "delegated" => Some(Self::Delegated),
            "paused" => Some(Self::Paused),
            _ => None,
        }
    }

    #[must_use]
    pub fn is_unsuccessful(self) -> bool {
        matches!(self, Self::Cancelled | Self::Failed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableRunTerminal {
    pub run_id: String,
    /// Exact execution generation that authored this terminal fact.
    pub owner_generation: Option<u64>,
    pub status: DurableRunTerminalStatus,
    pub error: Option<String>,
    /// Exact classified failure kind authored by the durable run owner.
    /// Missing kinds remain `None` so consumers can fail closed instead of
    /// guessing that every failed run was a provider overload.
    pub error_kind: Option<astra_core::ErrorKind>,
}

/// Parse a durable run terminal without mutating stream state.  The SSE
/// consumer also uses this from its read-ahead lane while Edge execution owns
/// the host borrow, so server cancellation remains observable during a long
/// local tool call.
pub fn durable_run_terminal_from_event(
    event: &Value,
) -> Result<Option<DurableRunTerminal>, String> {
    if event.get("type").and_then(Value::as_str) != Some("run_finished") {
        return Ok(None);
    }
    let run_id = event
        .get("run_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "run_finished omitted run_id".to_string())?;
    let status_text = event
        .get("status")
        .and_then(Value::as_str)
        .ok_or_else(|| "run_finished omitted status".to_string())?;
    let status = DurableRunTerminalStatus::from_wire(status_text)
        .ok_or_else(|| format!("run_finished carried unknown status `{status_text}`"))?;
    let error_kind = match event.get("error_kind") {
        None | Some(Value::Null) => None,
        Some(Value::String(tag)) => Some(
            astra_core::ErrorKind::parse_tag(tag)
                .ok_or_else(|| format!("run_finished carried unknown error_kind `{tag}`"))?,
        ),
        Some(_) => return Err("run_finished carried non-string error_kind".to_string()),
    };
    Ok(Some(DurableRunTerminal {
        run_id: run_id.to_string(),
        owner_generation: event.get("owner_generation").and_then(Value::as_u64),
        status,
        error: event
            .get("error")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        error_kind,
    }))
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ServerLoopExecutionSummary {
    pub tool_calls_count: u32,
    pub observation_tool_calls_count: u32,
    pub tools_used: Vec<String>,
    pub llm_rounds: u32,
    /// Fixed-size proof that every remotely attempted tool call reached one
    /// canonical terminal class under the exact run generation.
    pub tool_ledger_receipt: ToolLedgerReceipt,
    /// Coverage for provider-reported token usage across logical model calls.
    /// Token totals remain lower bounds whenever this is partial; consumers
    /// must not interpret an unavailable provider report as a measured zero.
    pub token_usage_coverage: Option<TokenUsageCoverage>,
    /// The exact frame produced by the Server-owned loop. Thin clients must
    /// project this immutable fact instead of reconstructing runtime state
    /// from aggregate terminal counters and their local wrapper state.
    pub runtime_feedback: Option<RuntimeFeedbackFrame>,
}

impl ServerLoopExecutionSummary {
    /// Whether the terminal receipt closes every tool attempt reported by the
    /// same immutable server aggregate. A receipt may be internally complete
    /// for a strict prefix while still lacking authority for the whole run.
    #[must_use]
    pub fn has_complete_tool_ledger(&self) -> bool {
        self.tool_ledger_receipt.is_complete()
            && self.tool_ledger_receipt.attempted == self.tool_calls_count
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsageCoverage {
    pub attempts: u32,
    pub provider_reported: u32,
    pub unavailable: u32,
}

impl TokenUsageCoverage {
    #[must_use]
    pub fn status(self) -> &'static str {
        if self.attempts == 0 || self.provider_reported == 0 {
            "none"
        } else if self.unavailable == 0 {
            "complete"
        } else {
            "partial"
        }
    }

    #[must_use]
    pub fn is_valid(self) -> bool {
        self.provider_reported.checked_add(self.unavailable) == Some(self.attempts)
    }
}

/// Deferred edge work from `tool_request` / `approval_required` events.
///
/// `detail` carries the raw command/path for downstream rule matching
/// (`bash_command_approval_reason`, `ApprovalFingerprint::shell`).
/// `display_label` is the rich UI preview — when present, clients
/// should show it to the user instead of the raw detail. Falls back
/// to `detail` when the emitter didn't populate it (older servers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeApprovalRequest {
    /// Exact durable scope carried by the approval event. These must survive
    /// client-side batching: a session may project concurrent child
    /// interactions onto one live lane, so the consuming stream's fallback
    /// identity is not necessarily the approval owner's identity.
    pub session_id: Option<String>,
    pub run_id: Option<String>,
    pub request_id: String,
    pub tool: String,
    pub approval_kind: ApprovalKind,
    pub detail: Option<String>,
    pub display_label: Option<String>,
}

/// Deferred edge work from `tool_request` / approval SSE events.
#[derive(Debug, Clone)]
pub enum ChatTurnEdgePending {
    ToolRequest {
        session_id: String,
        run_id: String,
        turn_chain_id: String,
        request_id: String,
        schema_admitted_by_server: bool,
        /// Absolute UTC deadline issued by the server.  This remains stable
        /// across batching, local queueing, and SSE replay.
        execution_deadline_unix_ms: u64,
        execution_timeout_ms: u64,
        tool: String,
        args: Value,
    },
    ApprovalRequired {
        session_id: Option<String>,
        run_id: Option<String>,
        request_id: String,
        tool: String,
        approval_kind: ApprovalKind,
        detail: Option<String>,
        display_label: Option<String>,
    },
    ApprovalBatchRequired {
        session_id: Option<String>,
        run_id: Option<String>,
        requests: Vec<EdgeApprovalRequest>,
    },
}

/// Hints for the CLI live renderer (no-op when the consumer sets `quiet`).
#[derive(Debug)]
pub enum SseRenderEffect {
    StopThinkingSpinner,
    StartThinkingSpinner,
    /// Incremental reasoning chunk for a compact terminal preview (CLI).
    ThinkingPreviewChunk(String),
    StreamText(String),
}

fn normalize_tool_call_for_accum(event: &Value) -> Result<Value, &'static str> {
    match event.get("type").and_then(Value::as_str) {
        // Live admitted events carry one exact canonical execution object.
        // Edge delivery projects that same admitted object into the current
        // flat public SSE card (`id`/`name`/`arguments`) before the matching
        // `tool_request`.  This is the sole adapter back into the canonical
        // execution shape; aliases and nested/flat mixtures remain invalid.
        Some("tool_call") => {
            if let Some(tool_call) = event.get("tool_call") {
                if [
                    "id",
                    "call_id",
                    "name",
                    "tool",
                    "arguments",
                    "args",
                    "function",
                ]
                .iter()
                .any(|field| event.get(*field).is_some())
                {
                    return Err("live tool_call mixes wrapper and payload fields");
                }
                let canonical =
                    crate::tool::args::shape::canonicalize_tool_call_for_execution(tool_call)
                        .map_err(|_| "live tool_call payload is malformed")?;
                if canonical != *tool_call {
                    return Err("live tool_call payload is not exact canonical nested shape");
                }
                return Ok(canonical);
            }

            if ["call_id", "tool", "args", "function"]
                .iter()
                .any(|field| event.get(*field).is_some())
            {
                return Err("edge tool_call uses non-canonical flat fields");
            }
            let call_id = event
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty() && id.trim() == *id)
                .ok_or("edge tool_call id is missing or non-exact")?;
            let raw_name = event
                .get("name")
                .and_then(Value::as_str)
                .ok_or("edge tool_call name is missing")?;
            let name = normalize_name(raw_name)
                .filter(|name| *name == raw_name)
                .ok_or("edge tool_call name is non-canonical")?;
            let arguments = event
                .get("arguments")
                .filter(|arguments| arguments.is_object())
                .ok_or("edge tool_call arguments must be an object")?;
            let arguments = serde_json::to_string(arguments)
                .map_err(|_| "edge tool_call arguments cannot be serialized")?;
            Ok(serde_json::json!({
                "id": call_id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": arguments,
                }
            }))
        }
        // Durable client projection is intentionally flat on the wire. This
        // is the single public adapter into the canonical execution shape.
        Some("tool_call_start") => {
            if ["id", "name", "args", "function", "tool_call"]
                .iter()
                .any(|field| event.get(*field).is_some())
            {
                return Err("durable tool_call_start mixes legacy fields");
            }
            let call_id = event
                .get("call_id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty() && id.trim() == *id)
                .ok_or("durable tool_call_start call_id is missing or non-exact")?;
            let raw_name = event
                .get("tool")
                .and_then(Value::as_str)
                .ok_or("durable tool_call_start tool is missing")?;
            let name = normalize_name(raw_name)
                .filter(|name| *name == raw_name)
                .ok_or("durable tool_call_start tool is non-canonical")?;
            let raw_arguments = event
                .get("arguments")
                .ok_or("durable tool_call_start arguments are missing")?;
            let arguments = match raw_arguments {
                Value::Object(_) => raw_arguments.clone(),
                Value::String(raw) => serde_json::from_str::<Value>(raw)
                    .map_err(|_| "durable tool_call_start arguments are malformed")?,
                _ => return Err("durable tool_call_start arguments must be an object"),
            };
            if !arguments.is_object() {
                return Err("durable tool_call_start arguments must be an object");
            }
            let arguments = serde_json::to_string(&arguments)
                .map_err(|_| "durable tool_call_start arguments cannot be serialized")?;
            Ok(serde_json::json!({
                "id": call_id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": arguments,
                }
            }))
        }
        _ => Err("event is not a tool-call event"),
    }
}

fn record_invalid_tool_call(accum: &mut ChatTurnSseAccum, reason: &'static str) {
    accum.error_kind = Some(astra_core::ErrorKind::ContractViolation);
    if accum.error_message.is_none() {
        accum.error_message = Some(format!("Invalid SSE tool-call event: {reason}"));
    }
}

fn approval_kind_from_event(event: &Value) -> ApprovalKind {
    event
        .get("approval_kind")
        .cloned()
        .and_then(|value| serde_json::from_value::<ApprovalKind>(value).ok())
        .unwrap_or(ApprovalKind::Explicit)
}

fn approval_request_from_event(event: &Value) -> Option<EdgeApprovalRequest> {
    let request_id = event
        .get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let tool = event
        .get("tool")
        .and_then(|v| v.as_str())
        .and_then(normalize_name)
        .map(str::to_string);
    let approval_kind = approval_kind_from_event(event);
    let detail = event
        .get("detail")
        .and_then(|v| v.as_str())
        .map(std::string::ToString::to_string)
        .or_else(|| {
            event
                .get("path")
                .and_then(|v| v.as_str())
                .map(std::string::ToString::to_string)
        });
    let display_label = event
        .get("display_label")
        .and_then(|v| v.as_str())
        .map(std::string::ToString::to_string);
    let tool = tool?;
    if request_id.is_empty() {
        return None;
    }
    Some(EdgeApprovalRequest {
        session_id: event
            .get("session_id")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        run_id: event
            .get("run_id")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        request_id,
        tool,
        approval_kind,
        detail,
        display_label,
    })
}

fn apply_one_event(
    event: &Value,
    accum: &mut ChatTurnSseAccum,
    edge_pending: &mut Vec<ChatTurnEdgePending>,
    effects: &mut Vec<SseRenderEffect>,
) {
    let etype = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if accum.root_identity.run_id().is_none()
        && let Some(existing) = accum.run_id.as_deref()
    {
        let _ = accum.root_identity.bind_legacy_hint(existing);
    }
    if matches!(etype, "session_info" | "run_started") {
        match accum.root_identity.observe_event(event) {
            Ok(()) => accum.run_id = accum.root_identity.run_id().map(str::to_string),
            Err(error) => {
                // Preserve the original owner so fatal cleanup cannot target a
                // descendant or conflicting replacement identity.
                accum.run_id = accum.root_identity.run_id().map(str::to_string);
                accum.error_kind = Some(astra_core::ErrorKind::ContractViolation);
                accum.error_message = Some(format!("Invalid SSE root identity: {error}"));
            }
        }
    }
    match etype {
        "text_delta" => {
            if accum.thinking_active {
                accum.thinking_active = false;
                effects.push(SseRenderEffect::StopThinkingSpinner);
            }
            if let Some(content) = event.get("content").and_then(|v| v.as_str()) {
                accum.full_text.push_str(content);
                effects.push(SseRenderEffect::StreamText(content.to_string()));
            }
        }
        "text_done" => {
            if let Some(ft) = event.get("full_text").and_then(|v| v.as_str()) {
                let was_empty = accum.full_text.is_empty();
                // Durable/replayed server streams may contain only the
                // terminal `text_done` event, without the live
                // `text_delta` events that originally produced the answer.
                // Keep the accumulator authoritative even when a bounded
                // provider retry diverged from an already streamed prefix.
                // The render lane only receives a new chunk for an empty
                // accumulator; a changed terminal value is projected by the
                // final stream result, avoiding duplicate append effects.
                accum.full_text = ft.to_string();
                if was_empty && !ft.is_empty() {
                    if accum.thinking_active {
                        accum.thinking_active = false;
                        effects.push(SseRenderEffect::StopThinkingSpinner);
                    }
                    effects.push(SseRenderEffect::StreamText(ft.to_string()));
                }
            }
        }
        "thinking_delta" | "reasoning_delta" | "reasoning_message_content" => {
            if !accum.thinking_active {
                accum.thinking_active = true;
                effects.push(SseRenderEffect::StartThinkingSpinner);
            }
            if let Some(chunk) = event.get("content").and_then(|v| v.as_str()) {
                accum.reasoning_content.push_str(chunk);
                if !chunk.is_empty() {
                    effects.push(SseRenderEffect::ThinkingPreviewChunk(chunk.to_string()));
                }
            }
        }
        "thinking_done" | "reasoning_done" => {
            // Bedrock thinking mode attaches a `signature` that must round-trip
            // to the provider on the next turn inside the assistant message's
            // `reasoningContent` block. Without it, Bedrock rejects with HTTP
            // 400 `messages.N.content.0.thinking.signature: Field required`.
            // Capture here so downstream multi-turn tool-call continuations
            // (headless + delegate paths) can replay it.
            if let Some(sig) = event.get("signature").and_then(|v| v.as_str())
                && !sig.is_empty()
            {
                accum.reasoning_signature.push_str(sig);
            }
            if accum.thinking_active {
                accum.thinking_active = false;
                effects.push(SseRenderEffect::StopThinkingSpinner);
            }
        }
        "user_intent_applied" => {
            let event_run_id = event.get("run_id").and_then(Value::as_str);
            // Descendant control facts may share the physical stream. They
            // must not alter the root conversation reconstructed by the thin
            // client.
            if event_run_id != accum.root_identity.run_id() {
                return;
            }
            let parsed = (|| {
                let status: astra_turn_types::UserIntentStatus =
                    serde_json::from_value(event.get("status").cloned().ok_or("missing status")?)
                        .map_err(|_| "invalid status")?;
                if status != astra_turn_types::UserIntentStatus::Applied {
                    return Err("status was not applied");
                }
                let intent_id = event
                    .get("intent_id")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or("missing intent_id")?
                    .to_string();
                let delivery = serde_json::from_value(
                    event.get("delivery").cloned().ok_or("missing delivery")?,
                )
                .map_err(|_| "invalid delivery")?;
                let event_index = event
                    .get("event_index")
                    .and_then(Value::as_u64)
                    .and_then(|value| usize::try_from(value).ok())
                    .ok_or("invalid event_index")?;
                let content = event
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or("missing content")?
                    .to_string();
                Ok::<_, &'static str>(StreamAppliedUserIntent {
                    intent_id,
                    delivery,
                    event_index,
                    content,
                })
            })();
            match parsed {
                Ok(intent) => {
                    if let Some(existing) = accum
                        .applied_user_intents
                        .iter()
                        .find(|existing| existing.intent_id == intent.intent_id)
                    {
                        if existing != &intent {
                            accum.error_kind = Some(astra_core::ErrorKind::ContractViolation);
                            accum.error_message = Some(format!(
                                "user_intent_applied identity `{}` was replayed with conflicting facts",
                                intent.intent_id
                            ));
                        }
                    } else {
                        accum.applied_user_intents.push(intent);
                    }
                }
                Err(reason) => {
                    accum.error_kind = Some(astra_core::ErrorKind::ContractViolation);
                    accum.error_message =
                        Some(format!("invalid user_intent_applied event: {reason}"));
                }
            }
        }
        "tool_call_start" => {
            if accum.thinking_active {
                accum.thinking_active = false;
                effects.push(SseRenderEffect::StopThinkingSpinner);
            }
            match normalize_tool_call_for_accum(event) {
                Ok(tool_call) => {
                    let id = tool_call
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let idx = accum.tool_calls.len();
                    accum.tool_calls.push(tool_call);
                    if !id.is_empty() {
                        accum.tool_call_id_index.insert(id, idx);
                    }
                }
                Err(reason) => record_invalid_tool_call(accum, reason),
            }
        }
        "tool_call" => match normalize_tool_call_for_accum(event) {
            Ok(tool_call) => {
                let tc_id = tool_call.get("id").and_then(|v| v.as_str()).unwrap_or("");
                if !tc_id.is_empty() {
                    if let Some(&idx) = accum.tool_call_id_index.get(tc_id) {
                        accum.tool_calls[idx] = tool_call;
                    } else {
                        let idx = accum.tool_calls.len();
                        accum.tool_call_id_index.insert(tc_id.to_string(), idx);
                        accum.tool_calls.push(tool_call);
                    }
                } else {
                    accum.tool_calls.push(tool_call);
                }
            }
            Err(reason) => record_invalid_tool_call(accum, reason),
        },
        "tool_request" => {
            let request_id = event
                .get("request_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let tool = event
                .get("tool")
                .and_then(|v| v.as_str())
                .and_then(normalize_name)
                .map(str::to_string);
            let args = event
                .get("args")
                .cloned()
                .unwrap_or_else(|| Value::Object(Default::default()));
            let schema_admitted_by_server = event
                .get("schema_admitted_by_server")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let execution_timeout_ms = event
                .get("execution_timeout_ms")
                .and_then(Value::as_u64)
                .filter(|seconds| *seconds > 0);
            let execution_deadline_unix_ms = event
                .get("execution_deadline_unix_ms")
                .and_then(Value::as_u64)
                .filter(|deadline| *deadline > 0);
            if let Some(tool) = tool
                && !request_id.is_empty()
            {
                if !schema_admitted_by_server {
                    accum.error_kind = Some(astra_core::ErrorKind::ContractViolation);
                    accum.error_message = Some(
                        "Server tool_request omitted wire-schema admission evidence".to_string(),
                    );
                    return;
                }
                let Some(execution_timeout_ms) = execution_timeout_ms else {
                    accum.error_kind = Some(astra_core::ErrorKind::ContractViolation);
                    accum.error_message = Some(
                        "Server tool_request omitted execution deadline authority".to_string(),
                    );
                    return;
                };
                let Some(execution_deadline_unix_ms) = execution_deadline_unix_ms else {
                    accum.error_kind = Some(astra_core::ErrorKind::ContractViolation);
                    accum.error_message =
                        Some("Server tool_request omitted absolute execution deadline".to_string());
                    return;
                };
                edge_pending.push(ChatTurnEdgePending::ToolRequest {
                    session_id: event
                        .get("session_id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    run_id: event
                        .get("run_id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    turn_chain_id: event
                        .get("turn_chain_id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    request_id,
                    schema_admitted_by_server,
                    execution_deadline_unix_ms,
                    execution_timeout_ms,
                    tool,
                    args,
                });
            }
        }
        "approval_required" => {
            if let Some(request) = approval_request_from_event(event) {
                edge_pending.push(ChatTurnEdgePending::ApprovalRequired {
                    session_id: request.session_id,
                    run_id: request.run_id,
                    request_id: request.request_id,
                    tool: request.tool,
                    approval_kind: request.approval_kind,
                    detail: request.detail,
                    display_label: request.display_label,
                });
            }
        }
        "approval_batch_required" => {
            let session_id = event
                .get("session_id")
                .and_then(Value::as_str)
                .map(ToString::to_string);
            let run_id = event
                .get("run_id")
                .and_then(Value::as_str)
                .map(ToString::to_string);
            let mut requests = event
                .get("requests")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(approval_request_from_event)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            for request in &mut requests {
                if request.session_id.is_none() {
                    request.session_id.clone_from(&session_id);
                }
                if request.run_id.is_none() {
                    request.run_id.clone_from(&run_id);
                }
            }
            if !requests.is_empty() {
                edge_pending.push(ChatTurnEdgePending::ApprovalBatchRequired {
                    session_id,
                    run_id,
                    requests,
                });
            }
        }
        "explain" => {
            accum.explain_turns.push(event.clone());
        }
        "turn_complete" | "turn_done" => {
            let mut receipt_failure = None;
            accum.server_loop_terminal =
                event.get("continuation_owner").and_then(Value::as_str) == Some("server");
            if accum.server_loop_terminal {
                if accum.thinking_active {
                    accum.thinking_active = false;
                    effects.push(SseRenderEffect::StopThinkingSpinner);
                }
                accum.has_tool_calls = false;
                accum.tool_calls.clear();
                accum.tool_call_id_index.clear();
                match parse_server_loop_execution_summary(event) {
                    Some(summary) => {
                        receipt_failure = remote_tool_receipt_failure(accum, &summary);
                        accum.server_execution_summary = Some(summary);
                    }
                    None => {
                        let receipt_is_missing_or_invalid = event
                            .get("tool_ledger_receipt")
                            .and_then(|value| {
                                serde_json::from_value::<ToolLedgerReceipt>(value.clone()).ok()
                            })
                            .is_none_or(|receipt| receipt.validate().is_err());
                        if receipt_is_missing_or_invalid {
                            receipt_failure = Some(
                                "server-owned terminal omitted a valid tool ledger receipt"
                                    .to_string(),
                            );
                        } else {
                            accum.error_kind = Some(astra_core::ErrorKind::ContractViolation);
                            accum.error_message = Some(
                                "Server-owned terminal omitted its complete execution summary"
                                    .to_string(),
                            );
                        }
                    }
                }
            } else {
                accum.has_tool_calls = event
                    .get("has_tool_calls")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
            }
            if let Some(assistant_text) = event.get("assistant_text").and_then(|v| v.as_str()) {
                accum.full_text = assistant_text.to_string();
            }
            if accum.server_loop_terminal {
                let execution_state = event.get("execution_state").and_then(Value::as_object);
                let execution_is_interrupted = execution_state
                    .and_then(|state| state.get("status"))
                    .and_then(Value::as_str)
                    == Some("interrupted");
                let interruption = event.get("interruption").filter(|value| value.is_object());
                let kinds_match = execution_state
                    .and_then(|state| state.get("interruption_kind"))
                    .and_then(Value::as_str)
                    .zip(
                        interruption
                            .and_then(|value| value.get("kind"))
                            .and_then(Value::as_str),
                    )
                    .is_some_and(|(execution_kind, record_kind)| execution_kind == record_kind);
                let interruption_projection_coherent =
                    match (execution_is_interrupted, interruption, kinds_match) {
                        (true, Some(interruption), true) => {
                            accum.server_interruption = Some(interruption.clone());
                            true
                        }
                        (false, None, _) => true,
                        _ => {
                            accum.error_kind = Some(astra_core::ErrorKind::ContractViolation);
                            accum.error_message = Some(
                            "Server terminal interruption projection is missing or inconsistent"
                                .to_string(),
                        );
                            false
                        }
                    };
                if let Some(detail) = receipt_failure
                    && accum.error_kind != Some(astra_core::ErrorKind::ContractViolation)
                {
                    if let Some(interruption) = accum.server_interruption.as_mut() {
                        let evidence = format!("additional execution evidence: {detail}");
                        match interruption
                            .get_mut("error_detail")
                            .and_then(|value| value.as_str())
                            .map(str::to_string)
                        {
                            Some(primary) if !primary.is_empty() => {
                                interruption["error_detail"] =
                                    Value::String(format!("{primary}; {evidence}"));
                            }
                            _ => interruption["error_detail"] = Value::String(evidence),
                        }
                    } else {
                        accum.server_interruption =
                            Some(remote_tool_receipt_incomplete_record(detail));
                    }
                    // A missing receipt makes an otherwise successful/paused
                    // remote execution incomplete.  It must not erase an
                    // authoritative failed/cancelled terminal's original
                    // error evidence merely because that terminal also had a
                    // malformed receipt.
                    let receipt_supersedes_attempt_error =
                        accum.run_terminal.as_ref().is_some_and(|terminal| {
                            matches!(
                                terminal.status,
                                DurableRunTerminalStatus::Completed
                                    | DurableRunTerminalStatus::Paused
                            )
                        });
                    if !accum.pending_attempt_error || receipt_supersedes_attempt_error {
                        accum.error_kind = None;
                        accum.error_message = None;
                    }
                }
                let owner_supersedes_attempt_error = accum.pending_attempt_error
                    && accum.attempt_error_supersession_eligible
                    && accum.error_kind != Some(astra_core::ErrorKind::ContractViolation)
                    && accum.server_execution_summary.is_some()
                    && interruption_projection_coherent
                    && accum.run_terminal.as_ref().is_some_and(|terminal| {
                        matches!(
                            terminal.status,
                            DurableRunTerminalStatus::Completed | DurableRunTerminalStatus::Paused
                        )
                    });
                if owner_supersedes_attempt_error {
                    accum.error_message = None;
                    accum.error_code = None;
                    accum.error_metadata = None;
                    accum.error_kind = None;
                    accum.pending_attempt_error = false;
                    accum.attempt_error_supersession_eligible = false;
                }
            }
        }
        "session_info" => {
            if let Some(sid) = event
                .get("session_id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|sid| !sid.is_empty())
            {
                match accum.session_id.as_deref() {
                    None => accum.session_id = Some(sid.to_string()),
                    Some(bound) if bound != sid => {
                        accum.error_kind = Some(astra_core::ErrorKind::ContractViolation);
                        accum.error_message = Some(format!(
                            "Invalid SSE session identity: one SSE stream changed session identity from `{bound}` to `{sid}`"
                        ));
                    }
                    Some(_) => {}
                }
            }
        }
        "usage" => {
            // Canonical wire shape produced by the runtime (see
            // `astra_runtime::turn::token_usage::TokenUsage`). Fields may be
            // either flat on the event or nested under `"usage"`; flat wins.
            let nested = event.get("usage");
            let read_u64 = |field: &str| -> Option<u64> {
                event
                    .get(field)
                    .and_then(|v| v.as_u64())
                    .or_else(|| nested.and_then(|u| u.get(field)).and_then(|v| v.as_u64()))
            };
            let input = read_u64("input_tokens");
            let output = read_u64("output_tokens");
            if input.is_none() && output.is_none() {
                if accum.error_message.is_none() {
                    accum.error_message = Some("Error: invalid usage payload".to_string());
                }
                return;
            }
            // Local accum exposes the legacy field names; map them through.
            // (prompt_tokens stores FRESH input — cache read/creation are
            // counted separately below so the sum is the billable total.)
            accum.prompt_tokens = input.unwrap_or(0);
            accum.completion_tokens = output.unwrap_or(0);
            accum.cache_read_tokens = read_u64("cached_input_tokens").unwrap_or(0);
            accum.cache_creation_tokens = read_u64("cache_creation_tokens").unwrap_or(0);
            accum.has_usage = true;
            // A durable server run terminates with its aggregate usage.  Do
            // not let that aggregate impersonate one provider request in the
            // context rail or cache-rate indicator.
            accum.usage_is_run_total = event
                .get("usage_scope")
                .or_else(|| nested.and_then(|usage| usage.get("usage_scope")))
                .and_then(Value::as_str)
                == Some("run_total");
            if accum.usage_is_run_total {
                // `/chat/stream` carries the final physical exchange inside
                // its terminal accounting event.  The run endpoint expands
                // the same value into a separate `context_usage` event, but
                // accepting the explicit nested form here keeps both
                // transports semantically identical.
                let last_request = event
                    .get("last_request_usage")
                    .or_else(|| nested.and_then(|usage| usage.get("last_request_usage")));
                let physical = last_request.and_then(|usage| {
                    Some(astra_turn_types::RequestTokenUsage {
                        fresh_input_tokens: usage.get("prompt_tokens")?.as_u64()?,
                        cache_read_tokens: usage.get("cache_read_tokens")?.as_u64()?,
                        cache_creation_tokens: usage.get("cache_creation_tokens")?.as_u64()?,
                        output_tokens: usage.get("completion_tokens")?.as_u64()?,
                    })
                });
                if let Some(physical) = physical {
                    accum.current_request_usage = Some(physical);
                }
            } else {
                accum.current_request_usage = Some(astra_turn_types::RequestTokenUsage {
                    fresh_input_tokens: accum.prompt_tokens,
                    cache_read_tokens: accum.cache_read_tokens,
                    cache_creation_tokens: accum.cache_creation_tokens,
                    output_tokens: accum.completion_tokens,
                });
            }
        }
        "context_usage" => {
            // This event is emitted by the server alongside terminal run
            // accounting.  It carries the last physical provider exchange,
            // which is the only authoritative measure of the next context.
            let read_u64 = |field: &str| event.get(field).and_then(|value| value.as_u64());
            let usage = match (
                read_u64("input_tokens"),
                read_u64("cached_input_tokens"),
                read_u64("cache_creation_tokens"),
                read_u64("output_tokens"),
            ) {
                (
                    Some(fresh_input_tokens),
                    Some(cache_read_tokens),
                    Some(cache_creation_tokens),
                    Some(output_tokens),
                ) => Some(astra_turn_types::RequestTokenUsage {
                    fresh_input_tokens,
                    cache_read_tokens,
                    cache_creation_tokens,
                    output_tokens,
                }),
                _ => None,
            };
            // A malformed optional observation must never erase a previously
            // observed physical request.
            if let Some(usage) = usage {
                accum.current_request_usage = Some(usage);
            }
        }
        "error" => {
            // Identity and protocol violations describe the stream itself,
            // not one provider attempt. They are sticky and must never be
            // downgraded by a later attempt-shaped error.
            if accum.error_kind == Some(astra_core::ErrorKind::ContractViolation) {
                accum.attempt_error_supersession_eligible = false;
                return;
            }
            let msg = event
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            accum.error_message = Some(format!("Error: {msg}"));
            accum.error_code = event
                .get("error_code")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            accum.error_metadata = event.get("metadata").cloned();
            accum.error_kind = event
                .get("error_kind")
                .and_then(|v| v.as_str())
                .and_then(astra_core::ErrorKind::parse_tag)
                .or_else(|| {
                    event
                        .get("code")
                        .and_then(|v| v.as_str())
                        .and_then(astra_core::ErrorKind::parse_tag)
                });
            accum.pending_attempt_error = true;
            // An error observed after the durable terminal cannot be
            // retroactively superseded by that earlier terminal.
            accum.attempt_error_supersession_eligible = false;
        }
        "context_meta" => {
            if let Some(t) = event.get("system_prompt_tokens").and_then(|v| v.as_u64()) {
                accum.system_prompt_tokens = Some(t as u32);
            }
            if let Some(b) = event.get("system_prompt_breakdown") {
                accum.system_prompt_breakdown = Some(b.clone());
            }
            if let Some(trace) = event.get("context_manifest_trace") {
                accum.context_manifest_trace = Some(trace.clone());
            }
            if accum.provider_visible_tools.is_none()
                && let Some(tools) = event.get("visible_tools").and_then(Value::as_array)
            {
                let mut names = tools
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                let mut seen = std::collections::HashSet::new();
                names.retain(|name| seen.insert(name.clone()));
                accum.provider_visible_tools = Some(names);
            }
            if let Some(compactions) = event.get("compactions").and_then(Value::as_array) {
                for value in compactions {
                    let Ok(observation) =
                        serde_json::from_value::<ContextCompactionObservation>(value.clone())
                    else {
                        continue;
                    };
                    if !observation.is_consistent() {
                        continue;
                    }
                    if let Some(existing) = accum
                        .context_compactions
                        .iter_mut()
                        .find(|existing| existing.id == observation.id)
                    {
                        *existing = observation;
                    } else {
                        accum.context_compactions.push(observation);
                    }
                }
            }
        }
        "injection_freshness" => {
            // wip-7 wire shape: fingerprints only, no raw text.
            // `channels: [{tag, hash, bytes, is_empty}, ...]`. If the
            // event arrives in any legacy shape (e.g. wip-5's `texts:`
            // container) we deliberately drop it — the transform
            // allowlist already strips that at the external boundary,
            // but we don't want to populate a fingerprint bundle from
            // a raw-text event either (that would suggest the channel
            // was observed when the bridge actually regressed to the
            // old shape). `injection_fingerprints` stays `None`
            // so the CLI observer treats those channels as untracked.
            if let Some(arr) = event.get("channels").and_then(|v| v.as_array()) {
                let mut channels = Vec::with_capacity(arr.len());
                for item in arr {
                    let Some(obj) = item.as_object() else {
                        continue;
                    };
                    let tag = obj
                        .get("tag")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    if tag.is_empty() {
                        continue;
                    }
                    let hash = obj.get("hash").and_then(|v| v.as_u64()).unwrap_or(0);
                    let bytes = obj.get("bytes").and_then(|v| v.as_u64()).unwrap_or(0);
                    // `is_empty` is authoritative when present; else
                    // derive from bytes==0 for robustness.
                    let is_empty = obj
                        .get("is_empty")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(bytes == 0);
                    channels.push(InjectionChannelFingerprint {
                        tag,
                        hash,
                        bytes,
                        is_empty,
                    });
                }
                accum.injection_fingerprints = Some(InjectionFingerprints { channels });
            }
        }
        "run_started" => {}
        "run_finished" => {
            let event_run_id = event
                .get("run_id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty());
            // Root streams intentionally project descendant lifecycle. Scope
            // before strict terminal parsing: child protocol evolution or a
            // malformed child terminal must not poison the physical owner.
            if event_run_id == accum.run_id.as_deref() {
                if accum.thinking_active {
                    accum.thinking_active = false;
                    effects.push(SseRenderEffect::StopThinkingSpinner);
                }
                match durable_run_terminal_from_event(event) {
                    Ok(Some(terminal)) => {
                        accum.attempt_error_supersession_eligible = accum.pending_attempt_error
                            && accum.error_kind != Some(astra_core::ErrorKind::ContractViolation)
                            && matches!(
                                terminal.status,
                                DurableRunTerminalStatus::Completed
                                    | DurableRunTerminalStatus::Paused
                            );
                        accum.run_terminal = Some(terminal);
                    }
                    Ok(None) => {}
                    Err(error) => {
                        accum.attempt_error_supersession_eligible = false;
                        accum.error_kind = Some(astra_core::ErrorKind::ContractViolation);
                        accum.error_message =
                            Some(format!("Invalid durable run terminal: {error}"));
                    }
                }
            } else if event_run_id.is_none() {
                accum.error_kind = Some(astra_core::ErrorKind::ContractViolation);
                accum.error_message = Some(
                    "Invalid durable run terminal: run_finished event omitted its durable run_id"
                        .to_string(),
                );
            }
        }
        _ => {
            // Capture finish_reason from the final SSE chunk when the API
            // streams it as `choices[0].finish_reason`. This is the raw
            // OpenAI-protocol value: "stop", "length", "tool_calls", etc.
            if accum.finish_reason.is_none() {
                if let Some(reason) = event
                    .get("choices")
                    .and_then(|v| v.as_array())
                    .and_then(|choices| choices.first())
                    .and_then(|choice| choice.get("finish_reason"))
                    .and_then(|v| v.as_str())
                {
                    if !reason.is_empty() {
                        accum.finish_reason = Some(reason.to_string());
                    }
                }
            }
        }
    }
}

fn parse_server_loop_execution_summary(event: &Value) -> Option<ServerLoopExecutionSummary> {
    let bounded_u32 = |field: &str| {
        event
            .get(field)
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
    };
    let tool_calls_count = bounded_u32("tool_calls_count")?;
    let observation_tool_calls_count = bounded_u32("observation_tool_calls_count")?;
    let raw_tools = event.get("tools_used")?.as_array()?;
    let mut tools_used = normalize_name_list(
        raw_tools
            .iter()
            .map(Value::as_str)
            .collect::<Option<Vec<_>>>()?
            .into_iter()
            .map(str::to_string),
    );
    tools_used.sort_unstable();
    let llm_rounds = bounded_u32("llm_rounds")?;
    let tool_ledger_receipt =
        serde_json::from_value::<ToolLedgerReceipt>(event.get("tool_ledger_receipt")?.clone())
            .ok()?;
    tool_ledger_receipt.validate().ok()?;
    let token_usage_coverage = match event.get("token_usage_coverage") {
        None => None,
        Some(coverage) => {
            let parsed = TokenUsageCoverage {
                attempts: coverage
                    .get("attempts")
                    .and_then(Value::as_u64)
                    .and_then(|value| u32::try_from(value).ok())?,
                provider_reported: coverage
                    .get("provider_reported")
                    .and_then(Value::as_u64)
                    .and_then(|value| u32::try_from(value).ok())?,
                unavailable: coverage
                    .get("unavailable")
                    .and_then(Value::as_u64)
                    .and_then(|value| u32::try_from(value).ok())?,
            };
            if coverage.get("scope").and_then(Value::as_str) != Some("logical_provider_calls")
                || !parsed.is_valid()
                || parsed.attempts != llm_rounds
                || coverage.get("status").and_then(Value::as_str) != Some(parsed.status())
            {
                return None;
            }
            Some(parsed)
        }
    };
    let execution_is_interrupted = event
        .get("execution_state")
        .and_then(Value::as_object)
        .and_then(|state| state.get("status"))
        .and_then(Value::as_str)
        == Some("interrupted");
    let runtime_feedback = match event.get("runtime_feedback") {
        Some(value) => Some(serde_json::from_value::<RuntimeFeedbackFrame>(value.clone()).ok()?),
        None if execution_is_interrupted => None,
        None => return None,
    };
    let feedback_is_coherent = runtime_feedback.as_ref().is_none_or(|feedback| {
        feedback.is_valid()
            && if execution_is_interrupted {
                // The terminal counter includes the failed provider attempt;
                // runtime feedback describes only the last successfully
                // ingested round. It is therefore a watermark, not an equal
                // counter, on interrupted outcomes.
                feedback.progress.llm_rounds_completed <= llm_rounds
            } else {
                feedback.progress.llm_rounds_completed == llm_rounds
            }
    });
    if observation_tool_calls_count > tool_calls_count
        || llm_rounds == 0
        || (tool_calls_count == 0) != tools_used.is_empty()
        || !feedback_is_coherent
    {
        return None;
    }
    Some(ServerLoopExecutionSummary {
        tool_calls_count,
        observation_tool_calls_count,
        tools_used,
        llm_rounds,
        tool_ledger_receipt,
        token_usage_coverage,
        runtime_feedback,
    })
}

fn remote_tool_receipt_incomplete_record(detail: impl Into<String>) -> Value {
    serde_json::json!({
        "kind": "execution_incomplete",
        "resume_action": "continue_immediately",
        "user_message": "Remote tool execution did not produce a complete terminal receipt.",
        "has_checkpoint": false,
        "tool_calls_completed": 0,
        "turns_completed": 0,
        "remaining_turns": 0,
        "error_detail": detail.into(),
    })
}

fn remote_tool_receipt_failure(
    accum: &ChatTurnSseAccum,
    summary: &ServerLoopExecutionSummary,
) -> Option<String> {
    let receipt = &summary.tool_ledger_receipt;
    if receipt.run_id != accum.run_id.as_deref().unwrap_or_default() {
        return Some("remote tool receipt is bound to a different run".to_string());
    }
    let Some(terminal) = accum.run_terminal.as_ref() else {
        return Some("remote tool receipt has no matching durable run terminal".to_string());
    };
    if terminal.run_id != receipt.run_id
        || terminal.owner_generation != Some(receipt.owner_generation)
    {
        return Some("remote tool receipt disagrees with durable run generation".to_string());
    }
    if !receipt.consistent {
        return Some("remote tool receipt reports conflicting lifecycle facts".to_string());
    }
    if receipt.attempted != summary.tool_calls_count {
        return Some(format!(
            "remote tool receipt covers {} of {} reported attempt(s)",
            receipt.attempted, summary.tool_calls_count
        ));
    }
    if receipt.unresolved > 0 {
        return Some(format!(
            "remote tool receipt has {} unresolved attempt(s)",
            receipt.unresolved
        ));
    }
    None
}

/// Parse one SSE event `block` (may contain multiple `data:` lines), update `accum`, append edge work.
pub fn dispatch_chat_turn_sse_event_block(
    block: &str,
    accum: &mut ChatTurnSseAccum,
    edge_pending: &mut Vec<ChatTurnEdgePending>,
) -> Vec<SseRenderEffect> {
    if accum.stream_complete {
        return Vec::new();
    }
    let mut effects = Vec::new();
    for line in block.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        if data == "[DONE]" {
            if accum.thinking_active {
                accum.thinking_active = false;
                effects.push(SseRenderEffect::StopThinkingSpinner);
            }
            accum.stream_complete = true;
            break;
        }
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            // Synthetic error: protocol parse error should be visible, not silently ignored.
            accum.thinking_active = false;
            effects.push(SseRenderEffect::StopThinkingSpinner);
            if accum.error_message.is_none() {
                accum.error_message = Some("Error: invalid JSON in SSE data".to_string());
            }
            continue;
        };
        apply_one_event(&event, accum, edge_pending, &mut effects);
    }
    effects
}

/// Buffers lossy UTF-8 from a `/chat/turn` body stream, yields complete blank-line SSE blocks, and
/// records [`ChatTurnSseFramer::ttft_ms`] on the first text or reasoning payload
/// (`text_delta`, `content_block_delta`, `thinking_delta`, `reasoning_delta`,
/// `reasoning_message_content`).
#[derive(Debug)]
pub struct ChatTurnSseFramer {
    sse: SseBlankLineUtf8Buf,
    stream_start: Instant,
    pub ttft_ms: Option<u64>,
    first_token_recorded: bool,
}

impl ChatTurnSseFramer {
    pub fn new() -> Self {
        Self {
            sse: SseBlankLineUtf8Buf::new(),
            stream_start: Instant::now(),
            ttft_ms: None,
            first_token_recorded: false,
        }
    }

    fn note_ttft_from_raw_event_text(&mut self, event_block: &str) {
        if self.first_token_recorded {
            return;
        }
        if event_block.contains("\"text_delta\"")
            || event_block.contains("\"content_block_delta\"")
            || event_block.contains("\"thinking_delta\"")
            || event_block.contains("\"reasoning_delta\"")
            || event_block.contains("\"reasoning_message_content\"")
            || event_block.contains("\"tool_call_start\"")
            || event_block.contains("\"tool_call\"")
        {
            self.ttft_ms = Some(self.stream_start.elapsed().as_millis() as u64);
            self.first_token_recorded = true;
        }
    }

    /// Append one HTTP chunk; returns every **complete** UTF-8 SSE event block (may be empty).
    pub fn push_bytes(&mut self, bytes: &[u8]) -> Result<Vec<String>, SseUtf8Error> {
        let blocks = self.sse.push_bytes(bytes)?;
        for b in &blocks {
            self.note_ttft_from_raw_event_text(b);
        }
        Ok(blocks)
    }

    /// After the byte stream ends: run TTFT detection on any trailing bytes, then take the buffer
    /// for a final [`dispatch_chat_turn_sse_event_block`] pass (partial event without `\n\n` yet).
    pub fn take_trailing_dispatch_blob(&mut self) -> Result<String, SseUtf8Error> {
        let tail = self.sse.take_buf()?;
        self.note_ttft_from_raw_event_text(&tail);
        Ok(tail)
    }
}

impl Default for ChatTurnSseFramer {
    fn default() -> Self {
        Self::new()
    }
}

/// Result of parsing a full UTF-8 `/chat/turn`-style SSE body in one shot (tests, fixtures, future headless clients).
#[derive(Debug)]
pub struct ParsedChatTurnSseBody {
    pub accum: ChatTurnSseAccum,
    pub edge_pending: Vec<ChatTurnEdgePending>,
    pub render_effects: Vec<SseRenderEffect>,
    pub ttft_ms: Option<u64>,
}

/// Parse an entire response body as UTF-8.
pub fn parse_chat_turn_sse_utf8_body(body: &str) -> ParsedChatTurnSseBody {
    let mut framer = ChatTurnSseFramer::new();
    let mut accum = ChatTurnSseAccum::default();
    let mut pending = Vec::new();
    let mut render_effects = Vec::new();
    for block in framer
        .push_bytes(body.as_bytes())
        .expect("a Rust str must produce valid UTF-8 SSE blocks")
    {
        render_effects.extend(dispatch_chat_turn_sse_event_block(
            &block,
            &mut accum,
            &mut pending,
        ));
    }
    let tail = framer
        .take_trailing_dispatch_blob()
        .expect("a Rust str must produce valid UTF-8 SSE tail");
    if !tail.trim().is_empty() {
        render_effects.extend(dispatch_chat_turn_sse_event_block(
            &tail,
            &mut accum,
            &mut pending,
        ));
    }
    ParsedChatTurnSseBody {
        accum,
        edge_pending: pending,
        render_effects,
        ttft_ms: framer.ttft_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server_runtime_feedback_fragment(llm_rounds: u32) -> String {
        let frame = crate::introspect::test_runtime_feedback(1, llm_rounds, 8);
        format!(
            ",\"runtime_feedback\":{}",
            serde_json::to_string(&frame).expect("serialize runtime feedback")
        )
    }

    fn tool_receipt_fragment(
        run_id: &str,
        owner_generation: u64,
        attempted: u32,
        terminal: u32,
        consistent: bool,
    ) -> String {
        let receipt = ToolLedgerReceipt::new(
            run_id,
            owner_generation,
            attempted,
            terminal,
            attempted.saturating_sub(terminal),
            crate::tool_ledger_receipt::ToolLedgerResultClassCounts {
                succeeded: terminal,
                ..Default::default()
            },
            u64::from(terminal),
            crate::tool_ledger_receipt::EMPTY_TOOL_LEDGER_ROOT,
            consistent,
        );
        format!(
            ",\"tool_ledger_receipt\":{}",
            serde_json::to_string(&receipt).expect("serialize tool receipt")
        )
    }

    fn bind_server_root(accum: &mut ChatTurnSseAccum, status: &str, owner_generation: u64) {
        for block in [
            sse("session_info", ",\"session_id\":\"s\",\"run_id\":\"r\""),
            sse(
                "run_finished",
                &format!(
                    ",\"run_id\":\"r\",\"status\":\"{status}\",\"owner_generation\":{owner_generation}"
                ),
            ),
        ] {
            dispatch_chat_turn_sse_event_block(&block, accum, &mut vec![]);
        }
    }

    #[test]
    fn parse_utf8_body_roundtrip_text() {
        let body = format!(
            "{}{}",
            sse("text_delta", ",\"content\":\"hi\""),
            sse("text_delta", ",\"content\":\" there\"")
        );
        let p = parse_chat_turn_sse_utf8_body(&body);
        assert_eq!(p.accum.full_text, "hi there");
        assert!(p.edge_pending.is_empty());
    }

    fn sse(event_type: &str, extra: &str) -> String {
        format!("data: {{\"type\":\"{event_type}\"{extra}}}\n\n")
    }

    #[test]
    fn text_delta_accumulates() {
        let mut a = ChatTurnSseAccum::default();
        let block = format!(
            "{}{}",
            sse("text_delta", ",\"content\":\"hello \""),
            sse("text_delta", ",\"content\":\"world\""),
        );
        let efx = dispatch_chat_turn_sse_event_block(&block, &mut a, &mut vec![]);
        assert_eq!(a.full_text, "hello world");
        assert!(!efx.is_empty());
    }

    /// Regression for Bedrock thinking-mode multi-turn:
    /// Bedrock returns `messages.N.content.0.thinking.signature: Field required`
    /// HTTP 400 on turn-2 if the provider signature from turn-1 is not replayed
    /// inside the assistant `reasoningContent` block. The signature hops
    /// bridge→CLI on the `reasoning_done` SSE event; this test pins the
    /// accumulator wire contract so regressions surface as a red unit test
    /// before they reach Bedrock.
    #[test]
    fn reasoning_done_captures_signature_into_accum() {
        let mut a = ChatTurnSseAccum::default();
        let block = format!(
            "{}{}",
            sse("reasoning_delta", ",\"content\":\"let me think\""),
            sse("reasoning_done", ",\"signature\":\"sig_abc123\""),
        );
        dispatch_chat_turn_sse_event_block(&block, &mut a, &mut vec![]);
        assert_eq!(a.reasoning_content, "let me think");
        assert_eq!(
            a.reasoning_signature, "sig_abc123",
            "signature from reasoning_done must land in accum so it can round-trip to Bedrock"
        );
    }

    #[test]
    fn reasoning_done_without_signature_leaves_accum_empty() {
        let mut a = ChatTurnSseAccum::default();
        let block = sse("reasoning_done", "");
        dispatch_chat_turn_sse_event_block(&block, &mut a, &mut vec![]);
        assert!(a.reasoning_signature.is_empty());
    }

    /// Cross-module wire contract: Bedrock thinking-mode tool-call multi-turn.
    ///
    /// This is the integration point that fell through the cracks in PR #284:
    /// the signature captured on the bridge-side `LlmCallResult` must travel
    /// through the SSE boundary to CLI's `ChatTurnSseAccum`, and then be
    /// round-tripped into the *next* assistant message so the subsequent
    /// Bedrock body carries `reasoningContent.reasoningText.signature`.
    ///
    /// If either leg breaks, Bedrock responds with HTTP 400
    /// `messages.N.content.0.thinking.signature: Field required`. This test
    /// exercises the full seam so unit-level green lights can't mask the
    /// regression again.
    #[test]
    fn signature_round_trips_from_sse_into_next_assistant_message() {
        use crate::headless_tool_assembly::{
            EdgeToolRoundRow, openai_assistant_with_tool_calls_message_ext,
        };

        struct Row;
        impl EdgeToolRoundRow for Row {
            fn tool_name(&self) -> &str {
                "noop"
            }
            fn tool_args(&self) -> &Value {
                static NULL: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
                NULL.get_or_init(|| Value::Null)
            }
            fn tool_output(&self) -> &str {
                ""
            }
            fn tool_duration_ms(&self) -> u64 {
                0
            }
        }

        let mut accum = ChatTurnSseAccum::default();
        let stream = format!(
            "{}{}{}",
            sse("reasoning_delta", ",\"content\":\"thinking...\""),
            sse("reasoning_done", ",\"signature\":\"sig_realish_base64\""),
            sse(
                "tool_call",
                ",\"tool_call\":{\"id\":\"tc-1\",\"type\":\"function\",\"function\":{\"name\":\"noop\",\"arguments\":\"{}\"}}"
            ),
        );
        dispatch_chat_turn_sse_event_block(&stream, &mut accum, &mut vec![]);

        assert_eq!(accum.reasoning_signature, "sig_realish_base64");

        let server_tool_calls = vec![serde_json::json!({
            "id": "tc-1",
            "type": "function",
            "function": {"name": "noop", "arguments": "{}"}
        })];
        let msg = openai_assistant_with_tool_calls_message_ext::<Row>(
            &server_tool_calls,
            &[],
            &accum.reasoning_content,
            &accum.reasoning_signature,
            true,
        );
        assert_eq!(
            msg["reasoning_signature"].as_str(),
            Some("sig_realish_base64"),
            "signature must flow: bridge SSE → dispatch accum → next assistant message"
        );
        assert_eq!(msg["reasoning_content"].as_str(), Some("thinking..."));
    }

    #[test]
    fn reasoning_delta_emits_preview_chunks() {
        let mut a = ChatTurnSseAccum::default();
        let block = format!(
            "{}{}",
            sse("reasoning_delta", ",\"content\":\"hello\""),
            sse("reasoning_delta", ",\"content\":\" z\"")
        );
        let efx = dispatch_chat_turn_sse_event_block(&block, &mut a, &mut vec![]);
        assert_eq!(a.reasoning_content, "hello z");
        let chunks: Vec<&str> = efx
            .iter()
            .filter_map(|e| match e {
                SseRenderEffect::ThinkingPreviewChunk(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(chunks, vec!["hello", " z"]);
    }

    #[test]
    fn session_info_captured() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse(
                "session_info",
                ",\"session_id\":\"abc-123\",\"run_id\":\"run-123\"",
            ),
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.session_id.as_deref(), Some("abc-123"));
        assert_eq!(a.run_id.as_deref(), Some("run-123"));
    }

    #[test]
    fn usage_captured() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse("usage", ",\"input_tokens\":100,\"output_tokens\":50"),
            &mut a,
            &mut vec![],
        );
        assert!(a.has_usage);
        assert_eq!(a.prompt_tokens, 100);
        assert_eq!(a.completion_tokens, 50);
    }

    #[test]
    fn error_event_captures_code_and_metadata() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse(
                "error",
                ",\"message\":\"turn mismatch\",\"error_code\":\"session_turn_mismatch\",\"metadata\":{\"actual_session_turn\":1,\"expected_session_turn\":2}",
            ),
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.error_message.as_deref(), Some("Error: turn mismatch"));
        assert_eq!(a.error_code.as_deref(), Some("session_turn_mismatch"));
        let metadata = a.error_metadata.as_ref().expect("metadata");
        assert_eq!(metadata["actual_session_turn"], 1);
        assert_eq!(metadata["expected_session_turn"], 2);
    }

    #[test]
    fn tool_call_collected() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse(
                "tool_call",
                ",\"tool_call\":{\"id\":\"call-1\",\"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":\"{}\"}}",
            ),
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.tool_calls.len(), 1);
        assert_eq!(a.tool_calls[0]["function"]["name"].as_str(), Some("bash"));
    }

    #[test]
    fn blank_tool_call_name_is_not_collected() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse(
                "tool_call",
                ",\"tool_call\":{\"id\":\"call-1\",\"type\":\"function\",\"function\":{\"name\":\"  \",\"arguments\":\"{}\"}}",
            ),
            &mut a,
            &mut vec![],
        );
        assert!(a.tool_calls.is_empty());
        assert_eq!(a.error_kind, Some(astra_core::ErrorKind::ContractViolation));
    }

    #[test]
    fn tool_call_start_collected_in_canonical_shape() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse(
                "tool_call_start",
                ",\"call_id\":\"tc-1\",\"tool\":\"bash\",\"arguments\":\"{\\\"command\\\":\\\"ls\\\"}\"",
            ),
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.tool_calls.len(), 1);
        assert_eq!(a.tool_calls[0]["id"].as_str(), Some("tc-1"));
        assert_eq!(a.tool_calls[0]["function"]["name"].as_str(), Some("bash"));
        assert_eq!(
            a.tool_calls[0]["function"]["arguments"].as_str(),
            Some("{\"command\":\"ls\"}")
        );
    }

    #[test]
    fn turn_complete_tool_calls_flag() {
        // has_tool_calls: true
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse("turn_complete", ",\"has_tool_calls\":true"),
            &mut a,
            &mut vec![],
        );
        assert!(a.has_tool_calls);

        // has_tool_calls: false — stays default
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse("turn_complete", ",\"has_tool_calls\":false"),
            &mut a,
            &mut vec![],
        );
        assert!(!a.has_tool_calls);
    }

    #[test]
    fn server_owned_terminal_does_not_delegate_continuation_back_to_client() {
        let mut a = ChatTurnSseAccum::default();
        for block in [
            sse("session_info", ",\"session_id\":\"s\",\"run_id\":\"r\""),
            sse(
                "run_finished",
                ",\"run_id\":\"r\",\"status\":\"completed\",\"owner_generation\":1",
            ),
        ] {
            dispatch_chat_turn_sse_event_block(&block, &mut a, &mut vec![]);
        }
        dispatch_chat_turn_sse_event_block(
            &sse(
                "tool_call",
                ",\"tool_call\":{\"id\":\"call-1\",\"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":\"{}\"}}",
            ),
            &mut a,
            &mut vec![],
        );
        dispatch_chat_turn_sse_event_block(
            &sse(
                "turn_complete",
                &format!(
                    ",\"has_tool_calls\":true,\"continuation_owner\":\"server\",\"tool_calls_count\":1,\"observation_tool_calls_count\":1,\"tools_used\":[\" agent_fanout \",\"agent_fanout\"],\"llm_rounds\":2,\"token_usage_coverage\":{{\"scope\":\"logical_provider_calls\",\"attempts\":2,\"provider_reported\":1,\"unavailable\":1,\"status\":\"partial\"}}{}{}",
                    server_runtime_feedback_fragment(2),
                    tool_receipt_fragment("r", 1, 1, 1, true),
                ),
            ),
            &mut a,
            &mut vec![],
        );
        assert!(a.server_loop_terminal);
        assert!(!a.has_tool_calls);
        assert!(a.tool_calls.is_empty());
        assert!(a.tool_call_id_index.is_empty());
        assert_eq!(
            a.server_execution_summary,
            Some(ServerLoopExecutionSummary {
                tool_calls_count: 1,
                observation_tool_calls_count: 1,
                tools_used: vec!["agent_fanout".to_string()],
                llm_rounds: 2,
                tool_ledger_receipt: ToolLedgerReceipt::new(
                    "r",
                    1,
                    1,
                    1,
                    0,
                    crate::tool_ledger_receipt::ToolLedgerResultClassCounts {
                        succeeded: 1,
                        ..Default::default()
                    },
                    1,
                    crate::tool_ledger_receipt::EMPTY_TOOL_LEDGER_ROOT,
                    true,
                ),
                token_usage_coverage: Some(TokenUsageCoverage {
                    attempts: 2,
                    provider_reported: 1,
                    unavailable: 1,
                }),
                runtime_feedback: Some(crate::introspect::test_runtime_feedback(1, 2, 8)),
            })
        );
    }

    fn dispatch_bound_server_terminal(
        accum: &mut ChatTurnSseAccum,
        receipt: String,
        terminal_generation: u64,
    ) {
        for block in [
            sse("session_info", ",\"session_id\":\"s\",\"run_id\":\"r\""),
            sse(
                "run_finished",
                &format!(
                    ",\"run_id\":\"r\",\"status\":\"completed\",\"owner_generation\":{terminal_generation}"
                ),
            ),
            sse(
                "turn_complete",
                &format!(
                    ",\"continuation_owner\":\"server\",\"tool_calls_count\":1,\"observation_tool_calls_count\":0,\"tools_used\":[\"bash\"],\"llm_rounds\":1{}{}",
                    server_runtime_feedback_fragment(1),
                    receipt,
                ),
            ),
        ] {
            dispatch_chat_turn_sse_event_block(&block, accum, &mut vec![]);
        }
    }

    #[test]
    fn unresolved_remote_tool_receipt_is_execution_incomplete() {
        let mut accum = ChatTurnSseAccum::default();
        dispatch_bound_server_terminal(&mut accum, tool_receipt_fragment("r", 7, 1, 0, true), 7);

        assert_eq!(
            accum
                .server_interruption
                .as_ref()
                .and_then(|value| value.get("kind"))
                .and_then(Value::as_str),
            Some("execution_incomplete")
        );
        assert_eq!(accum.error_kind, None);
        assert_eq!(
            accum
                .server_execution_summary
                .as_ref()
                .map(|summary| summary.tool_ledger_receipt.unresolved),
            Some(1)
        );
    }

    #[test]
    fn incomplete_receipt_preserves_factual_server_counters_but_not_authority() {
        let mut accum = ChatTurnSseAccum::default();
        for block in [
            sse("session_info", ",\"session_id\":\"s\",\"run_id\":\"r\""),
            sse(
                "run_finished",
                ",\"run_id\":\"r\",\"status\":\"paused\",\"owner_generation\":1",
            ),
            sse(
                "turn_complete",
                &format!(
                    ",\"continuation_owner\":\"server\",\"tool_calls_count\":2,\"observation_tool_calls_count\":2,\"tools_used\":[\"bash\"],\"llm_rounds\":35,\"execution_state\":{{\"status\":\"interrupted\",\"interruption_kind\":\"execution_incomplete\"}},\"interruption\":{{\"kind\":\"execution_incomplete\",\"resume_action\":\"continue_immediately\",\"user_message\":\"partial\",\"error_detail\":\"model ignored wrapup twice\",\"has_checkpoint\":true,\"tool_calls_completed\":1,\"turns_completed\":35,\"remaining_turns\":0}}{}",
                    tool_receipt_fragment("r", 1, 1, 1, true),
                ),
            ),
        ] {
            dispatch_chat_turn_sse_event_block(&block, &mut accum, &mut vec![]);
        }

        let summary = accum
            .server_execution_summary
            .as_ref()
            .expect("factual server aggregate remains observable");
        assert_eq!(summary.llm_rounds, 35);
        assert_eq!(summary.tool_calls_count, 2);
        assert_eq!(summary.tool_ledger_receipt.attempted, 1);
        assert!(!summary.has_complete_tool_ledger());
        assert_eq!(
            accum
                .server_interruption
                .as_ref()
                .and_then(|value| value.get("kind"))
                .and_then(Value::as_str),
            Some("execution_incomplete")
        );
        assert_eq!(accum.error_kind, None);
        let interruption = accum.server_interruption.as_ref().expect("interruption");
        assert_eq!(
            interruption.get("user_message").and_then(Value::as_str),
            Some("partial")
        );
        let detail = interruption
            .get("error_detail")
            .and_then(Value::as_str)
            .expect("combined detail");
        assert!(
            detail.starts_with("model ignored wrapup twice; "),
            "{detail}"
        );
        assert!(
            detail.contains("additional execution evidence:"),
            "{detail}"
        );
        assert!(
            detail.contains("covers 1 of 2 reported attempt(s)"),
            "{detail}"
        );
    }

    #[test]
    fn first_generation_zero_receipt_matches_durable_sse_terminal() {
        let mut accum = ChatTurnSseAccum::default();
        dispatch_bound_server_terminal(&mut accum, tool_receipt_fragment("r", 0, 1, 1, true), 0);

        assert!(accum.server_interruption.is_none());
        assert!(accum.error_kind.is_none());
        assert!(
            accum
                .server_execution_summary
                .as_ref()
                .is_some_and(|summary| summary.tool_ledger_receipt.is_complete())
        );
    }

    #[test]
    fn remote_tool_receipt_generation_mismatch_is_execution_incomplete() {
        let mut accum = ChatTurnSseAccum::default();
        dispatch_bound_server_terminal(&mut accum, tool_receipt_fragment("r", 6, 1, 1, true), 7);

        assert_eq!(
            accum
                .server_interruption
                .as_ref()
                .and_then(|value| value.get("kind"))
                .and_then(Value::as_str),
            Some("execution_incomplete")
        );
        assert_eq!(accum.error_kind, None);
    }

    #[test]
    fn missing_remote_tool_receipt_is_execution_incomplete_not_success() {
        let mut accum = ChatTurnSseAccum::default();
        dispatch_bound_server_terminal(&mut accum, String::new(), 7);

        assert!(accum.server_execution_summary.is_none());
        assert_eq!(
            accum
                .server_interruption
                .as_ref()
                .and_then(|value| value.get("kind"))
                .and_then(Value::as_str),
            Some("execution_incomplete")
        );
        assert_eq!(accum.error_kind, None);
    }

    #[test]
    fn server_owned_terminal_rejects_incoherent_token_usage_coverage() {
        let mut accum = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse(
                "turn_complete",
                &format!(
                    ",\"continuation_owner\":\"server\",\"tool_calls_count\":0,\"observation_tool_calls_count\":0,\"tools_used\":[],\"llm_rounds\":3,\"token_usage_coverage\":{{\"scope\":\"logical_provider_calls\",\"attempts\":2,\"provider_reported\":2,\"unavailable\":0,\"status\":\"complete\"}}{}{}",
                    server_runtime_feedback_fragment(3),
                    tool_receipt_fragment("r", 1, 0, 0, true),
                ),
            ),
            &mut accum,
            &mut vec![],
        );

        assert_eq!(
            accum.error_kind,
            Some(astra_core::ErrorKind::ContractViolation)
        );
        assert!(accum.server_execution_summary.is_none());
    }

    #[test]
    fn token_usage_coverage_rejects_overflow_instead_of_saturating_to_coherence() {
        assert!(
            !TokenUsageCoverage {
                attempts: u32::MAX,
                provider_reported: u32::MAX,
                unavailable: 1,
            }
            .is_valid(),
            "overflowing categories cannot describe exact logical-call coverage"
        );

        let mut accum = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse(
                "turn_complete",
                &format!(
                    ",\"continuation_owner\":\"server\",\"tool_calls_count\":0,\"observation_tool_calls_count\":0,\"tools_used\":[],\"llm_rounds\":4294967296,\"token_usage_coverage\":{{\"scope\":\"logical_provider_calls\",\"attempts\":4294967296,\"provider_reported\":4294967296,\"unavailable\":0,\"status\":\"complete\"}}{}{}",
                    server_runtime_feedback_fragment(u32::MAX),
                    tool_receipt_fragment("r", 1, 0, 0, true),
                ),
            ),
            &mut accum,
            &mut vec![],
        );

        assert_eq!(
            accum.error_kind,
            Some(astra_core::ErrorKind::ContractViolation)
        );
        assert!(accum.server_execution_summary.is_none());
    }

    #[test]
    fn server_owned_terminal_preserves_consistent_interruption() {
        let mut accum = ChatTurnSseAccum::default();
        bind_server_root(&mut accum, "paused", 1);
        dispatch_chat_turn_sse_event_block(
            &sse(
                "turn_complete",
                &format!(
                    ",\"continuation_owner\":\"server\",\"tool_calls_count\":0,\"observation_tool_calls_count\":0,\"tools_used\":[],\"llm_rounds\":3,\"execution_state\":{{\"status\":\"interrupted\",\"interruption_kind\":\"execution_incomplete\"}},\"interruption\":{{\"kind\":\"execution_incomplete\",\"resume_action\":\"continue_immediately\",\"user_message\":\"partial\",\"has_checkpoint\":true,\"tool_calls_completed\":1,\"turns_completed\":3,\"remaining_turns\":0}}{}{}",
                    server_runtime_feedback_fragment(3),
                    tool_receipt_fragment("r", 1, 0, 0, true),
                ),
            ),
            &mut accum,
            &mut vec![],
        );

        assert_eq!(
            accum
                .server_interruption
                .as_ref()
                .and_then(|value| value.get("kind"))
                .and_then(Value::as_str),
            Some("execution_incomplete")
        );
        assert_eq!(accum.error_kind, None);
    }

    #[test]
    fn interrupted_terminal_accepts_last_successful_feedback_watermark() {
        let mut accum = ChatTurnSseAccum::default();
        bind_server_root(&mut accum, "paused", 1);
        dispatch_chat_turn_sse_event_block(
            &sse(
                "turn_complete",
                &format!(
                    ",\"continuation_owner\":\"server\",\"tool_calls_count\":5,\"observation_tool_calls_count\":5,\"tools_used\":[\"bash\"],\"llm_rounds\":5,\"execution_state\":{{\"status\":\"interrupted\",\"interruption_kind\":\"budget_exhausted\"}},\"interruption\":{{\"kind\":\"budget_exhausted\",\"resume_action\":\"continue_immediately\",\"user_message\":\"partial\",\"has_checkpoint\":true,\"tool_calls_completed\":5,\"turns_completed\":5,\"remaining_turns\":0}}{}{}",
                    server_runtime_feedback_fragment(4),
                    tool_receipt_fragment("r", 1, 5, 5, true),
                ),
            ),
            &mut accum,
            &mut vec![],
        );

        let summary = accum.server_execution_summary.expect("valid summary");
        assert_eq!(summary.llm_rounds, 5);
        assert_eq!(summary.tool_calls_count, 5);
        assert_eq!(
            summary
                .runtime_feedback
                .expect("last successful feedback")
                .progress
                .llm_rounds_completed,
            4
        );
        assert_eq!(accum.error_kind, None);
    }

    #[test]
    fn authoritative_paused_terminal_supersedes_attempt_error() {
        let mut accum = ChatTurnSseAccum::default();
        let mut pending = vec![];
        for block in [
            sse("session_info", ",\"session_id\":\"s\",\"run_id\":\"r\""),
            sse(
                "error",
                ",\"message\":\"provider budget\",\"error_kind\":\"budget_exhausted\"",
            ),
            sse(
                "run_finished",
                ",\"run_id\":\"r\",\"status\":\"paused\",\"owner_generation\":1",
            ),
            sse(
                "turn_complete",
                &format!(
                    ",\"continuation_owner\":\"server\",\"tool_calls_count\":5,\"observation_tool_calls_count\":5,\"tools_used\":[\"bash\"],\"llm_rounds\":5,\"execution_state\":{{\"status\":\"interrupted\",\"interruption_kind\":\"budget_exhausted\"}},\"interruption\":{{\"kind\":\"budget_exhausted\",\"resume_action\":\"continue_immediately\",\"user_message\":\"partial\",\"has_checkpoint\":true,\"tool_calls_completed\":5,\"turns_completed\":5,\"remaining_turns\":0}}{}{}",
                    server_runtime_feedback_fragment(4),
                    tool_receipt_fragment("r", 1, 5, 5, true),
                ),
            ),
            "data: [DONE]\n\n".to_string(),
        ] {
            dispatch_chat_turn_sse_event_block(&block, &mut accum, &mut pending);
        }

        assert!(accum.server_loop_terminal);
        assert_eq!(accum.error_kind, None);
        assert_eq!(accum.error_message, None);
        assert!(!accum.pending_attempt_error);
        assert_eq!(
            accum
                .server_interruption
                .as_ref()
                .and_then(|value| value.get("kind"))
                .and_then(Value::as_str),
            Some("budget_exhausted")
        );
        assert_eq!(
            accum
                .server_execution_summary
                .as_ref()
                .map(|summary| summary.tool_calls_count),
            Some(5)
        );
    }

    #[test]
    fn failed_terminal_does_not_supersede_attempt_error() {
        let mut accum = ChatTurnSseAccum::default();
        let mut pending = vec![];
        for block in [
            sse("session_info", ",\"session_id\":\"s\",\"run_id\":\"r\""),
            sse(
                "error",
                ",\"message\":\"provider failed\",\"error_kind\":\"server_error\"",
            ),
            sse(
                "run_finished",
                ",\"run_id\":\"r\",\"status\":\"failed\",\"error_kind\":\"server_error\",\"owner_generation\":1",
            ),
            sse(
                "turn_complete",
                &format!(
                    ",\"continuation_owner\":\"server\",\"tool_calls_count\":0,\"observation_tool_calls_count\":0,\"tools_used\":[],\"llm_rounds\":1{}{}",
                    server_runtime_feedback_fragment(1),
                    tool_receipt_fragment("r", 1, 0, 0, true),
                ),
            ),
        ] {
            dispatch_chat_turn_sse_event_block(&block, &mut accum, &mut pending);
        }

        assert!(accum.pending_attempt_error);
        assert_eq!(accum.error_kind, Some(astra_core::ErrorKind::ServerError));
        assert!(accum.error_message.is_some());
    }

    #[test]
    fn attempt_error_cannot_overwrite_or_clear_protocol_violation() {
        let mut accum = ChatTurnSseAccum::default();
        let mut pending = vec![];
        for block in [
            sse("session_info", ",\"session_id\":\"s\",\"run_id\":\"r\""),
            sse(
                "session_info",
                ",\"session_id\":\"different\",\"run_id\":\"r\"",
            ),
            sse(
                "error",
                ",\"message\":\"provider budget\",\"error_kind\":\"budget_exhausted\"",
            ),
            sse(
                "run_finished",
                ",\"run_id\":\"r\",\"status\":\"paused\",\"owner_generation\":1",
            ),
            sse(
                "turn_complete",
                &format!(
                    ",\"continuation_owner\":\"server\",\"tool_calls_count\":0,\"observation_tool_calls_count\":0,\"tools_used\":[],\"llm_rounds\":1,\"execution_state\":{{\"status\":\"interrupted\",\"interruption_kind\":\"budget_exhausted\"}},\"interruption\":{{\"kind\":\"budget_exhausted\",\"resume_action\":\"continue_immediately\",\"user_message\":\"partial\",\"has_checkpoint\":false,\"tool_calls_completed\":0,\"turns_completed\":1,\"remaining_turns\":0}}{}{}",
                    server_runtime_feedback_fragment(1),
                    tool_receipt_fragment("r", 1, 0, 0, true),
                ),
            ),
        ] {
            dispatch_chat_turn_sse_event_block(&block, &mut accum, &mut pending);
        }

        assert_eq!(
            accum.error_kind,
            Some(astra_core::ErrorKind::ContractViolation)
        );
        assert!(
            accum
                .error_message
                .as_deref()
                .is_some_and(|message| message.contains("changed session identity"))
        );
        assert!(!accum.attempt_error_supersession_eligible);
    }

    #[test]
    fn error_after_owner_terminal_is_not_superseded_by_turn_complete() {
        let mut accum = ChatTurnSseAccum::default();
        let mut pending = vec![];
        for block in [
            sse("session_info", ",\"session_id\":\"s\",\"run_id\":\"r\""),
            sse(
                "run_finished",
                ",\"run_id\":\"r\",\"status\":\"paused\",\"owner_generation\":1",
            ),
            sse(
                "error",
                ",\"message\":\"late transport failure\",\"error_kind\":\"stream_transport\"",
            ),
            sse(
                "turn_complete",
                &format!(
                    ",\"continuation_owner\":\"server\",\"tool_calls_count\":0,\"observation_tool_calls_count\":0,\"tools_used\":[],\"llm_rounds\":1,\"execution_state\":{{\"status\":\"interrupted\",\"interruption_kind\":\"budget_exhausted\"}},\"interruption\":{{\"kind\":\"budget_exhausted\",\"resume_action\":\"continue_immediately\",\"user_message\":\"partial\",\"has_checkpoint\":false,\"tool_calls_completed\":0,\"turns_completed\":1,\"remaining_turns\":0}}{}{}",
                    server_runtime_feedback_fragment(1),
                    tool_receipt_fragment("r", 1, 0, 0, true),
                ),
            ),
        ] {
            dispatch_chat_turn_sse_event_block(&block, &mut accum, &mut pending);
        }

        assert_eq!(
            accum.error_kind,
            Some(astra_core::ErrorKind::StreamTransport)
        );
        assert!(accum.pending_attempt_error);
        assert!(!accum.attempt_error_supersession_eligible);
        assert!(
            accum
                .error_message
                .as_deref()
                .is_some_and(|message| message.contains("late transport failure"))
        );
    }

    #[test]
    fn first_attempt_interruption_accepts_absent_runtime_feedback() {
        let mut accum = ChatTurnSseAccum::default();
        bind_server_root(&mut accum, "paused", 1);
        dispatch_chat_turn_sse_event_block(
            &sse(
                "turn_complete",
                &format!(
                    ",\"continuation_owner\":\"server\",\"tool_calls_count\":0,\"observation_tool_calls_count\":0,\"tools_used\":[],\"llm_rounds\":1,\"execution_state\":{{\"status\":\"interrupted\",\"interruption_kind\":\"budget_exhausted\"}},\"interruption\":{{\"kind\":\"budget_exhausted\",\"resume_action\":\"continue_immediately\",\"user_message\":\"partial\",\"has_checkpoint\":false,\"tool_calls_completed\":0,\"turns_completed\":1,\"remaining_turns\":0}}{}",
                    tool_receipt_fragment("r", 1, 0, 0, true)
                ),
            ),
            &mut accum,
            &mut vec![],
        );

        let summary = accum.server_execution_summary.expect("valid summary");
        assert_eq!(summary.llm_rounds, 1);
        assert_eq!(summary.runtime_feedback, None);
        assert_eq!(accum.error_kind, None);
    }

    #[test]
    fn interrupted_server_terminal_without_complete_record_fails_closed() {
        let mut accum = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse(
                "turn_complete",
                &format!(
                    ",\"continuation_owner\":\"server\",\"tool_calls_count\":0,\"observation_tool_calls_count\":0,\"tools_used\":[],\"llm_rounds\":2,\"execution_state\":{{\"status\":\"interrupted\",\"interruption_kind\":\"token_budget_exceeded\"}}{}",
                    server_runtime_feedback_fragment(2)
                ),
            ),
            &mut accum,
            &mut vec![],
        );

        assert_eq!(accum.server_interruption, None);
        assert_eq!(
            accum.error_kind,
            Some(astra_core::ErrorKind::ContractViolation)
        );
    }

    #[test]
    fn server_terminal_rejects_mismatched_interruption_kind() {
        let mut accum = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse(
                "turn_complete",
                &format!(
                    ",\"continuation_owner\":\"server\",\"tool_calls_count\":0,\"observation_tool_calls_count\":0,\"tools_used\":[],\"llm_rounds\":2,\"execution_state\":{{\"status\":\"interrupted\",\"interruption_kind\":\"token_budget_exceeded\"}},\"interruption\":{{\"kind\":\"execution_incomplete\"}}{}{}",
                    server_runtime_feedback_fragment(2),
                    tool_receipt_fragment("r", 1, 0, 0, true),
                ),
            ),
            &mut accum,
            &mut vec![],
        );

        assert_eq!(accum.server_interruption, None);
        assert_eq!(
            accum.error_kind,
            Some(astra_core::ErrorKind::ContractViolation)
        );
    }

    #[test]
    fn server_owned_terminal_requires_coherent_canonical_runtime_feedback() {
        for suffix in [
            format!(
                ",\"continuation_owner\":\"server\",\"tool_calls_count\":0,\"observation_tool_calls_count\":0,\"tools_used\":[],\"llm_rounds\":2{}",
                tool_receipt_fragment("r", 1, 0, 0, true),
            ),
            format!(
                ",\"continuation_owner\":\"server\",\"tool_calls_count\":0,\"observation_tool_calls_count\":0,\"tools_used\":[],\"llm_rounds\":3{}{}",
                server_runtime_feedback_fragment(2),
                tool_receipt_fragment("r", 1, 0, 0, true),
            ),
        ] {
            let mut accum = ChatTurnSseAccum::default();
            dispatch_chat_turn_sse_event_block(
                &sse("turn_complete", &suffix),
                &mut accum,
                &mut vec![],
            );
            assert!(accum.server_loop_terminal);
            assert!(accum.server_execution_summary.is_none());
            assert_eq!(
                accum.error_kind,
                Some(astra_core::ErrorKind::ContractViolation)
            );
        }
    }

    #[test]
    fn incomplete_server_execution_summary_without_receipt_is_execution_incomplete() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse(
                "turn_complete",
                ",\"continuation_owner\":\"server\",\"tool_calls_count\":1",
            ),
            &mut a,
            &mut vec![],
        );

        assert!(a.server_loop_terminal);
        assert!(a.server_execution_summary.is_none());
        assert_eq!(a.error_kind, None);
        assert_eq!(
            a.server_interruption
                .as_ref()
                .and_then(|record| record.get("kind"))
                .and_then(Value::as_str),
            Some("execution_incomplete")
        );
    }

    #[test]
    fn turn_complete_overrides_full_text_with_authoritative_assistant_text() {
        let mut a = ChatTurnSseAccum {
            full_text: "stale partial".to_string(),
            ..Default::default()
        };
        dispatch_chat_turn_sse_event_block(
            &sse(
                "turn_complete",
                ",\"has_tool_calls\":false,\"assistant_text\":\"recovered final text\"",
            ),
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.full_text, "recovered final text");
    }

    #[test]
    fn error_captured() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse(
                "error",
                ",\"message\":\"rate limited\",\"error_kind\":\"rate_limit\"",
            ),
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.error_message.as_deref(), Some("Error: rate limited"));
        assert_eq!(a.error_kind, Some(astra_core::ErrorKind::RateLimit));
    }

    #[test]
    fn run_started_captures_run_id() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse("run_started", ",\"run_id\":\"run-42\""),
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.run_id.as_deref(), Some("run-42"));
    }

    #[test]
    fn descendant_lifecycle_does_not_rebind_root_run() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &format!(
                "{}{}{}",
                sse("session_info", ",\"run_id\":\"root-run\""),
                sse("run_started", ",\"run_id\":\"child-run\""),
                sse(
                    "agent_progress",
                    ",\"run_id\":\"child-run\",\"message\":\"working\""
                )
            ),
            &mut a,
            &mut vec![],
        );

        assert_eq!(a.run_id.as_deref(), Some("root-run"));
        assert!(a.error_kind.is_none());
    }

    #[test]
    fn session_info_promotes_or_rejects_identity_without_losing_original_owner() {
        let mut promoted = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &format!(
                "{}{}",
                sse("run_started", ",\"run_id\":\"legacy-root\""),
                sse("session_info", ",\"run_id\":\"canonical-root\"")
            ),
            &mut promoted,
            &mut vec![],
        );
        assert_eq!(promoted.run_id.as_deref(), Some("canonical-root"));
        assert!(promoted.error_kind.is_none());

        dispatch_chat_turn_sse_event_block(
            &sse("session_info", ",\"run_id\":\"conflicting-root\""),
            &mut promoted,
            &mut vec![],
        );
        assert_eq!(
            promoted.run_id.as_deref(),
            Some("canonical-root"),
            "fatal cleanup must retain the original physical owner"
        );
        assert_eq!(
            promoted.error_kind,
            Some(astra_core::ErrorKind::ContractViolation)
        );
    }

    #[test]
    fn descendant_started_before_bootstrap_cannot_claim_legacy_owner() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &format!(
                "{}{}",
                sse(
                    "run_started",
                    ",\"run_id\":\"child-run\",\"parent_run_id\":\"root-run\""
                ),
                sse("session_info", ",\"run_id\":\"root-run\"")
            ),
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.run_id.as_deref(), Some("root-run"));
        assert!(a.error_kind.is_none());
    }

    #[test]
    fn run_finished_is_terminal_only_for_the_bound_producer() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &format!(
                "{}{}",
                sse("run_started", ",\"run_id\":\"root-run\""),
                sse(
                    "run_finished",
                    ",\"run_id\":\"child-run\",\"status\":\"cancelled\""
                )
            ),
            &mut a,
            &mut vec![],
        );
        assert!(a.run_terminal.is_none());

        dispatch_chat_turn_sse_event_block(
            &sse(
                "run_finished",
                ",\"run_id\":\"root-run\",\"status\":\"cancelled\"",
            ),
            &mut a,
            &mut vec![],
        );
        let terminal = a.run_terminal.expect("root terminal must be retained");
        assert_eq!(terminal.run_id, "root-run");
        assert_eq!(terminal.status, DurableRunTerminalStatus::Cancelled);
    }

    #[test]
    fn failed_run_terminal_preserves_exact_error_kind() {
        let mut accum = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &format!(
                "{}{}",
                sse("run_started", ",\"run_id\":\"root-run\""),
                sse(
                    "run_finished",
                    ",\"run_id\":\"root-run\",\"status\":\"failed\",\"error\":\"LLM budget elapsed\",\"error_kind\":\"budget_exhausted\""
                )
            ),
            &mut accum,
            &mut vec![],
        );

        let terminal = accum.run_terminal.expect("typed root terminal");
        assert_eq!(terminal.status, DurableRunTerminalStatus::Failed);
        assert_eq!(
            terminal.error_kind,
            Some(astra_core::ErrorKind::BudgetExhausted)
        );
        assert_eq!(terminal.error.as_deref(), Some("LLM budget elapsed"));
    }

    #[test]
    fn unknown_root_terminal_error_kind_fails_closed() {
        let mut accum = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &format!(
                "{}{}",
                sse("run_started", ",\"run_id\":\"root-run\""),
                sse(
                    "run_finished",
                    ",\"run_id\":\"root-run\",\"status\":\"failed\",\"error_kind\":\"future_failure\""
                )
            ),
            &mut accum,
            &mut vec![],
        );

        assert!(accum.run_terminal.is_none());
        assert_eq!(
            accum.error_kind,
            Some(astra_core::ErrorKind::ContractViolation)
        );
        assert!(
            accum
                .error_message
                .as_deref()
                .is_some_and(|message| message.contains("unknown error_kind"))
        );
    }

    #[test]
    fn malformed_descendant_terminal_cannot_poison_root_stream() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &format!(
                "{}{}{}",
                sse("session_info", ",\"run_id\":\"root-run\""),
                sse(
                    "run_finished",
                    ",\"run_id\":\"child-run\",\"status\":\"future_child_state\""
                ),
                sse(
                    "run_finished",
                    ",\"run_id\":\"root-run\",\"status\":\"completed\""
                )
            ),
            &mut a,
            &mut vec![],
        );

        assert!(a.error_kind.is_none());
        assert_eq!(a.run_id.as_deref(), Some("root-run"));
        assert_eq!(
            a.run_terminal.as_ref().map(|terminal| terminal.status),
            Some(DurableRunTerminalStatus::Completed)
        );
    }

    #[test]
    fn run_finished_accepts_every_server_terminal_status() {
        for (wire, expected) in [
            ("completed", DurableRunTerminalStatus::Completed),
            ("cancelled", DurableRunTerminalStatus::Cancelled),
            ("failed", DurableRunTerminalStatus::Failed),
            ("delegated", DurableRunTerminalStatus::Delegated),
            ("paused", DurableRunTerminalStatus::Paused),
        ] {
            let mut accum = ChatTurnSseAccum::default();
            dispatch_chat_turn_sse_event_block(
                &format!(
                    "{}{}",
                    sse("session_info", ",\"run_id\":\"root-run\""),
                    sse(
                        "run_finished",
                        &format!(",\"run_id\":\"root-run\",\"status\":\"{wire}\"")
                    )
                ),
                &mut accum,
                &mut vec![],
            );
            assert_eq!(
                accum.run_terminal.as_ref().map(|terminal| terminal.status),
                Some(expected),
                "status {wire} must remain a legal typed terminal"
            );
            assert!(accum.error_kind.is_none(), "status {wire}");
        }
    }

    #[test]
    fn malformed_bound_run_terminal_fails_closed() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &format!(
                "{}{}",
                sse("run_started", ",\"run_id\":\"root-run\""),
                sse(
                    "run_finished",
                    ",\"run_id\":\"root-run\",\"status\":\"mystery\""
                )
            ),
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.error_kind, Some(astra_core::ErrorKind::ContractViolation));
        assert!(a.run_terminal.is_none());
    }

    #[test]
    fn done_marker_ignored() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block("data: [DONE]\n\n", &mut a, &mut vec![]);
        assert!(a.full_text.is_empty());
    }

    #[test]
    fn invalid_json_ignored() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block("data: {invalid json}\n\n", &mut a, &mut vec![]);
        assert_eq!(
            a.error_message.as_deref(),
            Some("Error: invalid JSON in SSE data")
        );
    }

    #[test]
    fn text_done_fallback_when_no_deltas() {
        let mut a = ChatTurnSseAccum::default();
        let effects = dispatch_chat_turn_sse_event_block(
            &sse("text_done", ",\"full_text\":\"complete answer\""),
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.full_text, "complete answer");
        assert!(
            effects.iter().any(
                |effect| matches!(effect, SseRenderEffect::StreamText(text) if text == "complete answer")
            ),
            "terminal text learned from a replay must enter the render lane"
        );
    }

    #[test]
    fn thinking_delta_captures_reasoning() {
        let mut a = ChatTurnSseAccum::default();
        let block = format!(
            "{}{}",
            sse("thinking_delta", ",\"content\":\"step 1\""),
            sse("thinking_delta", ",\"content\":\" step 2\""),
        );
        dispatch_chat_turn_sse_event_block(&block, &mut a, &mut vec![]);
        assert_eq!(a.reasoning_content, "step 1 step 2");
    }

    #[test]
    fn reasoning_message_content_captures_reasoning() {
        let mut a = ChatTurnSseAccum::default();
        let block = format!(
            "{}{}",
            sse("reasoning_message_content", ",\"content\":\"step 1\""),
            sse("reasoning_message_content", ",\"content\":\" step 2\""),
        );
        let efx = dispatch_chat_turn_sse_event_block(&block, &mut a, &mut vec![]);
        assert_eq!(a.reasoning_content, "step 1 step 2");
        let chunks: Vec<&str> = efx
            .iter()
            .filter_map(|e| match e {
                SseRenderEffect::ThinkingPreviewChunk(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(chunks, vec!["step 1", " step 2"]);
    }

    #[test]
    fn tool_request_enqueues_pending() {
        let mut a = ChatTurnSseAccum::default();
        let mut pending = Vec::new();
        let block = "data: {\"type\":\"tool_request\",\"request_id\":\"tr-1\",\"schema_admitted_by_server\":true,\"execution_timeout_ms\":300000,\"execution_deadline_unix_ms\":1700000300000,\"tool\":\" bash \",\"args\":{\"command\":\"echo x\"}}\n\n";
        dispatch_chat_turn_sse_event_block(block, &mut a, &mut pending);
        assert_eq!(pending.len(), 1);
        match &pending[0] {
            ChatTurnEdgePending::ToolRequest {
                request_id,
                execution_deadline_unix_ms,
                tool,
                args,
                ..
            } => {
                assert_eq!(request_id, "tr-1");
                assert_eq!(*execution_deadline_unix_ms, 1_700_000_300_000);
                assert_eq!(tool, "bash");
                assert_eq!(args["command"], "echo x");
            }
            _ => panic!("expected ToolRequest"),
        }
    }

    #[test]
    fn tool_request_blank_tool_not_pushed() {
        let mut a = ChatTurnSseAccum::default();
        let mut pending = Vec::new();
        dispatch_chat_turn_sse_event_block(
            &sse("tool_request", ",\"request_id\":\"tr-1\",\"tool\":\"  \""),
            &mut a,
            &mut pending,
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn tool_request_without_server_schema_admission_fails_closed() {
        let mut a = ChatTurnSseAccum::default();
        let mut pending = Vec::new();
        dispatch_chat_turn_sse_event_block(
            &sse(
                "tool_request",
                ",\"request_id\":\"tr-1\",\"tool\":\"web_fetch\",\"args\":{\"url\":\"https://example.com\"}",
            ),
            &mut a,
            &mut pending,
        );
        assert!(pending.is_empty());
        assert_eq!(a.error_kind, Some(astra_core::ErrorKind::ContractViolation));
        assert!(
            a.error_message
                .as_deref()
                .is_some_and(|message| message.contains("wire-schema admission"))
        );
    }

    #[test]
    fn tool_request_without_execution_deadline_fails_closed() {
        let mut a = ChatTurnSseAccum::default();
        let mut pending = Vec::new();
        dispatch_chat_turn_sse_event_block(
            &sse(
                "tool_request",
                ",\"request_id\":\"tr-1\",\"schema_admitted_by_server\":true,\"tool\":\"bash\",\"args\":{\"command\":\"echo x\"}",
            ),
            &mut a,
            &mut pending,
        );
        assert!(pending.is_empty());
        assert_eq!(a.error_kind, Some(astra_core::ErrorKind::ContractViolation));
        assert!(
            a.error_message
                .as_deref()
                .is_some_and(|message| { message.contains("execution deadline authority") })
        );
    }

    #[test]
    fn tool_request_without_absolute_execution_deadline_fails_closed() {
        let mut a = ChatTurnSseAccum::default();
        let mut pending = Vec::new();
        dispatch_chat_turn_sse_event_block(
            &sse(
                "tool_request",
                ",\"request_id\":\"r1\",\"schema_admitted_by_server\":true,\"execution_timeout_ms\":300000,\"tool\":\"bash\"",
            ),
            &mut a,
            &mut pending,
        );
        assert!(pending.is_empty());
        assert_eq!(a.error_kind, Some(astra_core::ErrorKind::ContractViolation));
        assert!(
            a.error_message
                .as_deref()
                .is_some_and(|message| message.contains("absolute execution deadline"))
        );
    }

    #[test]
    fn approval_required_enqueues_pending() {
        let mut a = ChatTurnSseAccum::default();
        let mut pending = Vec::new();
        let block = "data: {\"type\":\"approval_required\",\"request_id\":\"ap-1\",\"tool\":\" write_file \",\"approval_kind\":\"standard\",\"path\":\"src/x.rs\",\"detail\":\"src/x.rs\"}\n\n";
        dispatch_chat_turn_sse_event_block(block, &mut a, &mut pending);
        assert_eq!(pending.len(), 1);
        match &pending[0] {
            ChatTurnEdgePending::ApprovalRequired {
                session_id: _,
                run_id: _,
                request_id,
                tool,
                approval_kind,
                detail,
                display_label: _,
            } => {
                assert_eq!(request_id, "ap-1");
                assert_eq!(tool, "write_file");
                assert_eq!(*approval_kind, ApprovalKind::Standard);
                assert_eq!(detail.as_deref(), Some("src/x.rs"));
            }
            _ => panic!("expected ApprovalRequired"),
        }
    }

    #[test]
    fn approval_required_preserves_producer_scope() {
        let mut accum = ChatTurnSseAccum::default();
        let mut pending = Vec::new();
        let block = "data: {\"type\":\"approval_required\",\"session_id\":\"producer-session\",\"run_id\":\"producer-run\",\"request_id\":\"ap-owner\",\"tool\":\"bash\"}\n\n";

        dispatch_chat_turn_sse_event_block(block, &mut accum, &mut pending);

        assert!(matches!(
            pending.as_slice(),
            [ChatTurnEdgePending::ApprovalRequired {
                session_id: Some(session_id),
                run_id: Some(run_id),
                ..
            }] if session_id == "producer-session" && run_id == "producer-run"
        ));
    }

    #[test]
    fn approval_required_blank_tool_not_pushed() {
        let mut a = ChatTurnSseAccum::default();
        let mut pending = Vec::new();
        dispatch_chat_turn_sse_event_block(
            &sse(
                "approval_required",
                ",\"request_id\":\"ap-1\",\"tool\":\"  \"",
            ),
            &mut a,
            &mut pending,
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn approval_required_without_kind_defaults_to_explicit() {
        let mut a = ChatTurnSseAccum::default();
        let mut pending = Vec::new();
        let block = "data: {\"type\":\"approval_required\",\"request_id\":\"ap-1\",\"tool\":\"bash\",\"detail\":\"rm -rf tmp\"}\n\n";
        dispatch_chat_turn_sse_event_block(block, &mut a, &mut pending);
        match &pending[0] {
            ChatTurnEdgePending::ApprovalRequired { approval_kind, .. } => {
                assert_eq!(*approval_kind, ApprovalKind::Explicit);
            }
            other => panic!("expected ApprovalRequired, got {other:?}"),
        }
    }

    #[test]
    fn approval_batch_required_enqueues_pending() {
        let mut a = ChatTurnSseAccum::default();
        let mut pending = Vec::new();
        let block = "data: {\"type\":\"approval_batch_required\",\"requests\":[{\"request_id\":\"ap-1\",\"tool\":\"write_file\",\"approval_kind\":\"standard\",\"detail\":\"src/a.rs\"},{\"request_id\":\"ap-2\",\"tool\":\"write_file\",\"approval_kind\":\"standard\",\"detail\":\"src/b.rs\"}]}\n\n";
        dispatch_chat_turn_sse_event_block(block, &mut a, &mut pending);
        assert_eq!(pending.len(), 1);
        match &pending[0] {
            ChatTurnEdgePending::ApprovalBatchRequired { requests, .. } => {
                assert_eq!(requests.len(), 2);
                assert_eq!(requests[0].request_id, "ap-1");
                assert_eq!(requests[1].detail.as_deref(), Some("src/b.rs"));
                assert_eq!(requests[0].approval_kind, ApprovalKind::Standard);
            }
            other => panic!("expected ApprovalBatchRequired, got {other:?}"),
        }
    }

    #[test]
    fn approval_batch_inherits_scope_without_overwriting_nested_owner() {
        let mut accum = ChatTurnSseAccum::default();
        let mut pending = Vec::new();
        let block = "data: {\"type\":\"approval_batch_required\",\"session_id\":\"batch-session\",\"run_id\":\"batch-run\",\"requests\":[{\"request_id\":\"inherited\",\"tool\":\"bash\"},{\"session_id\":\"nested-session\",\"run_id\":\"nested-run\",\"request_id\":\"owned\",\"tool\":\"bash\"}]}\n\n";

        dispatch_chat_turn_sse_event_block(block, &mut accum, &mut pending);

        let ChatTurnEdgePending::ApprovalBatchRequired { requests, .. } = &pending[0] else {
            panic!("expected ApprovalBatchRequired");
        };
        assert_eq!(requests[0].session_id.as_deref(), Some("batch-session"));
        assert_eq!(requests[0].run_id.as_deref(), Some("batch-run"));
        assert_eq!(requests[1].session_id.as_deref(), Some("nested-session"));
        assert_eq!(requests[1].run_id.as_deref(), Some("nested-run"));
    }

    #[test]
    fn framer_splits_event_across_chunks() {
        let ev = sse("session_info", ",\"session_id\":\"split-id\"");
        let mid = ev.find("session").unwrap();
        let mut f = ChatTurnSseFramer::new();
        assert!(f.push_bytes(&ev.as_bytes()[..mid]).unwrap().is_empty());
        let blocks = f.push_bytes(&ev.as_bytes()[mid..]).unwrap();
        assert_eq!(blocks.len(), 1);
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(&blocks[0], &mut a, &mut vec![]);
        assert_eq!(a.session_id.as_deref(), Some("split-id"));
    }

    #[test]
    fn invalid_usage_payload_sets_error() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block("data: {\"type\":\"usage\"}\n\n", &mut a, &mut vec![]);
        assert_eq!(
            a.error_message.as_deref(),
            Some("Error: invalid usage payload")
        );
        assert!(!a.has_usage);
    }

    #[test]
    fn run_total_usage_cannot_replace_latest_physical_request_context() {
        let mut accum = ChatTurnSseAccum::default();
        let mut pending = vec![];
        dispatch_chat_turn_sse_event_block(
            &sse(
                "usage",
                ",\"input_tokens\":2100000,\"cached_input_tokens\":1700000,\"cache_creation_tokens\":0,\"output_tokens\":34000,\"usage_scope\":\"run_total\"",
            ),
            &mut accum,
            &mut pending,
        );
        assert!(
            accum.has_usage,
            "run totals remain available for accounting"
        );
        assert_eq!(accum.current_request_usage, None);

        dispatch_chat_turn_sse_event_block(
            &sse(
                "context_usage",
                ",\"input_tokens\":17250,\"cached_input_tokens\":85248,\"cache_creation_tokens\":0,\"output_tokens\":901",
            ),
            &mut accum,
            &mut pending,
        );
        assert_eq!(
            accum.current_request_usage,
            Some(astra_turn_types::RequestTokenUsage {
                fresh_input_tokens: 17_250,
                cache_read_tokens: 85_248,
                cache_creation_tokens: 0,
                output_tokens: 901,
            }),
            "the context rail must use the last physical exchange, not run total"
        );
    }

    #[test]
    fn terminal_run_total_embeds_the_physical_context_for_chat_stream() {
        let mut accum = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse(
                "usage",
                ",\"usage\":{\"input_tokens\":73000,\"cached_input_tokens\":149000,\"cache_creation_tokens\":0,\"output_tokens\":2000,\"usage_scope\":\"run_total\",\"last_request_usage\":{\"prompt_tokens\":1700,\"cache_read_tokens\":37000,\"cache_creation_tokens\":0,\"completion_tokens\":500}}",
            ),
            &mut accum,
            &mut vec![],
        );

        assert!(accum.usage_is_run_total);
        assert_eq!(
            accum.prompt_tokens, 73_000,
            "run accounting retains its distinct fresh-input bucket"
        );
        assert_eq!(
            accum.current_request_usage,
            Some(astra_turn_types::RequestTokenUsage {
                fresh_input_tokens: 1_700,
                cache_read_tokens: 37_000,
                cache_creation_tokens: 0,
                output_tokens: 500,
            }),
            "the context UI must read the explicit final physical request, never a multi-run total"
        );
    }

    #[test]
    fn malformed_context_usage_preserves_prior_physical_request() {
        let mut accum = ChatTurnSseAccum {
            current_request_usage: Some(astra_turn_types::RequestTokenUsage {
                fresh_input_tokens: 100,
                cache_read_tokens: 900,
                cache_creation_tokens: 0,
                output_tokens: 10,
            }),
            ..Default::default()
        };
        dispatch_chat_turn_sse_event_block(
            &sse("context_usage", ",\"input_tokens\":123,\"output_tokens\":4"),
            &mut accum,
            &mut vec![],
        );
        assert_eq!(
            accum.current_request_usage,
            Some(astra_turn_types::RequestTokenUsage {
                fresh_input_tokens: 100,
                cache_read_tokens: 900,
                cache_creation_tokens: 0,
                output_tokens: 10,
            })
        );
    }

    #[test]
    fn framer_ttft_on_first_content_block() {
        // TTFT must be recorded for text_delta, reasoning_delta,
        // reasoning_message_content, tool_call_start, and tool_call
        // — whichever arrives first.
        let cases: &[(&str, &str)] = &[
            ("text_delta", ",\"content\":\"x\""),
            ("reasoning_delta", ",\"content\":\"thinking...\""),
            ("reasoning_message_content", ",\"content\":\"thinking...\""),
            ("tool_call_start", ",\"id\":\"call-1\",\"name\":\"bash\""),
            (
                "tool_call",
                ",\"id\":\"call-1\",\"name\":\"bash\",\"arguments\":\"{}\"",
            ),
        ];
        for (event_type, extra) in cases {
            let mut f = ChatTurnSseFramer::new();
            let _ = f.push_bytes(sse(event_type, extra).as_bytes()).unwrap();
            assert!(
                f.ttft_ms.is_some(),
                "ttft must be set when first SSE event is {event_type}"
            );
        }
    }

    #[test]
    fn framer_ttft_not_set_on_usage_only() {
        // usage events alone should not trigger ttft
        let block = sse("usage", ",\"input_tokens\":100,\"output_tokens\":5");
        let mut f = ChatTurnSseFramer::new();
        let _ = f.push_bytes(block.as_bytes()).unwrap();
        assert!(f.ttft_ms.is_none(), "usage-only event must not set ttft");
    }

    // ── Cache token tests ────────────────────────────────────────────────

    #[test]
    fn usage_with_cache_tokens_parsed() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse(
                "usage",
                ",\"input_tokens\":100,\"output_tokens\":50,\"cached_input_tokens\":25,\"cache_creation_tokens\":10",
            ),
            &mut a,
            &mut vec![],
        );
        assert!(a.has_usage);
        assert_eq!(a.prompt_tokens, 100);
        assert_eq!(a.completion_tokens, 50);
        assert_eq!(a.cache_read_tokens, 25);
        assert_eq!(a.cache_creation_tokens, 10);
    }

    #[test]
    fn usage_cache_tokens_default_to_zero_when_missing() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse("usage", ",\"input_tokens\":100,\"output_tokens\":50"),
            &mut a,
            &mut vec![],
        );
        assert!(a.has_usage);
        assert_eq!(a.cache_read_tokens, 0);
        assert_eq!(a.cache_creation_tokens, 0);
    }

    #[test]
    fn usage_cache_tokens_null_treated_as_zero() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse(
                "usage",
                ",\"input_tokens\":100,\"output_tokens\":50,\"cached_input_tokens\":null,\"cache_creation_tokens\":null",
            ),
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.cache_read_tokens, 0);
        assert_eq!(a.cache_creation_tokens, 0);
    }

    #[test]
    fn usage_without_prompt_or_completion_is_error_and_ignores_cache() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse(
                "usage",
                ",\"cached_input_tokens\":500,\"cache_creation_tokens\":100",
            ),
            &mut a,
            &mut vec![],
        );
        // Early return: no prompt/completion → error, cache tokens not parsed
        assert!(!a.has_usage);
        assert_eq!(a.cache_read_tokens, 0);
        assert_eq!(a.cache_creation_tokens, 0);
        assert!(a.error_message.is_some());
    }

    #[test]
    fn usage_second_event_overwrites_cache_tokens() {
        let mut a = ChatTurnSseAccum::default();
        let block = format!(
            "{}{}",
            sse(
                "usage",
                ",\"input_tokens\":100,\"output_tokens\":50,\"cached_input_tokens\":30,\"cache_creation_tokens\":10"
            ),
            sse(
                "usage",
                ",\"input_tokens\":200,\"output_tokens\":80,\"cached_input_tokens\":60,\"cache_creation_tokens\":0"
            ),
        );
        dispatch_chat_turn_sse_event_block(&block, &mut a, &mut vec![]);
        assert_eq!(a.prompt_tokens, 200);
        assert_eq!(a.completion_tokens, 80);
        assert_eq!(a.cache_read_tokens, 60);
        assert_eq!(a.cache_creation_tokens, 0);
    }

    // -----------------------------------------------------------------------
    // Unhappy-path / edge-case tests
    // -----------------------------------------------------------------------

    #[test]
    fn text_delta_missing_content_field_no_panic() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(&sse("text_delta", ""), &mut a, &mut vec![]);
        assert_eq!(a.full_text, "");
    }

    #[test]
    fn text_delta_null_content_ignored() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse("text_delta", ",\"content\":null"),
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.full_text, "");
    }

    #[test]
    fn text_delta_numeric_content_ignored() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse("text_delta", ",\"content\":42"),
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.full_text, "");
    }

    #[test]
    fn tool_request_missing_id_not_pushed() {
        let mut a = ChatTurnSseAccum::default();
        let mut pending = vec![];
        dispatch_chat_turn_sse_event_block(
            &sse("tool_request", ",\"tool\":\"read_file\",\"args\":{}"),
            &mut a,
            &mut pending,
        );
        // Empty request_id → not pushed
        assert!(pending.is_empty());
    }

    #[test]
    fn tool_request_missing_tool_not_pushed() {
        let mut a = ChatTurnSseAccum::default();
        let mut pending = vec![];
        dispatch_chat_turn_sse_event_block(
            &sse("tool_request", ",\"request_id\":\"r1\",\"args\":{}"),
            &mut a,
            &mut pending,
        );
        // Empty tool → not pushed
        assert!(pending.is_empty());
    }

    #[test]
    fn tool_request_missing_args_defaults_to_empty_object() {
        let mut a = ChatTurnSseAccum::default();
        let mut pending = vec![];
        dispatch_chat_turn_sse_event_block(
            &sse(
                "tool_request",
                ",\"request_id\":\"r1\",\"schema_admitted_by_server\":true,\"execution_timeout_ms\":300000,\"execution_deadline_unix_ms\":4102444800000,\"tool\":\"bash\"",
            ),
            &mut a,
            &mut pending,
        );
        assert_eq!(pending.len(), 1);
        if let ChatTurnEdgePending::ToolRequest { args, .. } = &pending[0] {
            assert!(args.is_object());
            assert!(args.as_object().unwrap().is_empty());
        } else {
            panic!("expected ToolRequest");
        }
    }

    #[test]
    fn approval_required_missing_id_not_pushed() {
        let mut a = ChatTurnSseAccum::default();
        let mut pending = vec![];
        dispatch_chat_turn_sse_event_block(
            &sse("approval_required", ",\"tool\":\"write_file\""),
            &mut a,
            &mut pending,
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn approval_required_missing_tool_not_pushed() {
        let mut a = ChatTurnSseAccum::default();
        let mut pending = vec![];
        dispatch_chat_turn_sse_event_block(
            &sse("approval_required", ",\"request_id\":\"r1\""),
            &mut a,
            &mut pending,
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn usage_negative_tokens_treated_as_missing() {
        let mut a = ChatTurnSseAccum::default();
        // as_u64() returns None for negative values
        dispatch_chat_turn_sse_event_block(
            &sse("usage", ",\"input_tokens\":-1,\"output_tokens\":-5"),
            &mut a,
            &mut vec![],
        );
        // Negative i64 fails as_u64() → both None → error
        assert!(a.error_message.is_some());
        assert!(!a.has_usage);
    }

    #[test]
    fn usage_float_tokens_treated_as_zero() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse("usage", ",\"input_tokens\":1.5,\"output_tokens\":2.7"),
            &mut a,
            &mut vec![],
        );
        // as_u64() returns None for floats → falls through to unwrap_or(0)
        // But at least one must be present as integer for has_usage
        assert!(a.error_message.is_some());
    }

    #[test]
    fn usage_missing_cache_tokens_default_to_zero() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse("usage", ",\"input_tokens\":100,\"output_tokens\":50"),
            &mut a,
            &mut vec![],
        );
        assert!(a.has_usage);
        assert_eq!(a.cache_read_tokens, 0);
        assert_eq!(a.cache_creation_tokens, 0);
    }

    #[test]
    fn multiple_errors_last_wins() {
        let mut a = ChatTurnSseAccum::default();
        let block = format!(
            "{}{}",
            sse("error", ",\"message\":\"rate limited\""),
            sse("error", ",\"message\":\"server error\""),
        );
        dispatch_chat_turn_sse_event_block(&block, &mut a, &mut vec![]);
        // Each error overwrites the previous — last error wins.
        assert!(a.error_message.as_ref().unwrap().contains("server error"));
    }

    #[test]
    fn error_event_missing_message_says_unknown() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(&sse("error", ""), &mut a, &mut vec![]);
        assert_eq!(a.error_message.as_deref(), Some("Error: unknown error"));
    }

    #[test]
    fn unknown_event_type_cannot_claim_root_identity() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse("some_future_event", ",\"run_id\":\"run-42\""),
            &mut a,
            &mut vec![],
        );
        assert!(a.run_id.is_none());
    }

    #[test]
    fn unknown_event_without_run_id_is_noop() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse("some_future_event", ",\"data\":123"),
            &mut a,
            &mut vec![],
        );
        assert!(a.run_id.is_none());
        assert!(a.error_message.is_none());
    }

    #[test]
    fn empty_block_is_noop() {
        let mut a = ChatTurnSseAccum::default();
        let efx = dispatch_chat_turn_sse_event_block("", &mut a, &mut vec![]);
        assert!(efx.is_empty());
        assert_eq!(a.full_text, "");
        assert!(a.error_message.is_none());
    }

    #[test]
    fn whitespace_only_block_is_noop() {
        let mut a = ChatTurnSseAccum::default();
        let efx = dispatch_chat_turn_sse_event_block("  \n\n  \n", &mut a, &mut vec![]);
        assert!(efx.is_empty());
    }

    #[test]
    fn done_only_block_is_noop() {
        let mut a = ChatTurnSseAccum::default();
        let efx = dispatch_chat_turn_sse_event_block("data: [DONE]\n\n", &mut a, &mut vec![]);
        assert!(efx.is_empty());
        assert!(!a.has_usage);
    }

    #[test]
    fn invalid_json_sets_error() {
        let mut a = ChatTurnSseAccum::default();
        let efx =
            dispatch_chat_turn_sse_event_block("data: {not valid json}\n\n", &mut a, &mut vec![]);
        assert!(a.error_message.as_ref().unwrap().contains("invalid JSON"));
        // Should also emit StopThinkingSpinner
        assert!(
            efx.iter()
                .any(|e| matches!(e, SseRenderEffect::StopThinkingSpinner))
        );
    }

    #[test]
    fn invalid_json_then_valid_event_still_works() {
        let mut a = ChatTurnSseAccum::default();
        let block = format!(
            "data: {{bad json}}\n\n{}",
            sse("text_delta", ",\"content\":\"ok\""),
        );
        dispatch_chat_turn_sse_event_block(&block, &mut a, &mut vec![]);
        assert_eq!(a.full_text, "ok");
        // Error from first event preserved
        assert!(a.error_message.is_some());
    }

    #[test]
    fn event_missing_type_field_treated_as_unknown() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block("data: {\"run_id\":\"r1\"}\n\n", &mut a, &mut vec![]);
        // Missing/unknown event types have no authority to establish the
        // stream's durable root producer.
        assert!(a.run_id.is_none());
    }

    #[test]
    fn framer_handles_empty_bytes() {
        let mut f = ChatTurnSseFramer::new();
        let blocks = f.push_bytes(&[]).unwrap();
        assert!(blocks.is_empty());
        assert!(f.ttft_ms.is_none());
    }

    #[test]
    fn framer_trailing_blob_on_empty_returns_empty_string() {
        let mut f = ChatTurnSseFramer::new();
        let tail = f.take_trailing_dispatch_blob().unwrap();
        assert_eq!(tail, "");
    }

    #[test]
    fn framer_invalid_utf8_is_rejected() {
        let mut f = ChatTurnSseFramer::new();
        // Invalid UTF-8 sequence: 0xFF is never valid
        let data = b"data: {\"type\":\"text_delta\",\"content\":\"hi\xff\"}\n\n";
        let error = f.push_bytes(data).expect_err("invalid UTF-8 must fail");
        assert!(error.to_string().contains("invalid UTF-8"));
    }

    #[test]
    fn session_info_missing_session_id_is_noop() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse("session_info", ",\"other_field\":\"val\""),
            &mut a,
            &mut vec![],
        );
        assert!(a.session_id.is_none());
    }

    #[test]
    fn turn_complete_missing_has_tool_calls_defaults_false() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(&sse("turn_complete", ""), &mut a, &mut vec![]);
        assert!(!a.has_tool_calls);
    }

    #[test]
    fn thinking_delta_missing_content_no_panic() {
        let mut a = ChatTurnSseAccum::default();
        let efx =
            dispatch_chat_turn_sse_event_block(&sse("thinking_delta", ""), &mut a, &mut vec![]);
        assert_eq!(a.reasoning_content, "");
        assert!(
            efx.iter()
                .any(|e| matches!(e, SseRenderEffect::StartThinkingSpinner))
        );
    }

    #[test]
    fn thinking_delta_empty_string_no_preview_chunk() {
        let mut a = ChatTurnSseAccum::default();
        let efx = dispatch_chat_turn_sse_event_block(
            &sse("thinking_delta", ",\"content\":\"\""),
            &mut a,
            &mut vec![],
        );
        // Empty content: spinner started but no ThinkingPreviewChunk emitted
        assert!(
            efx.iter()
                .any(|e| matches!(e, SseRenderEffect::StartThinkingSpinner))
        );
        assert!(
            !efx.iter()
                .any(|e| matches!(e, SseRenderEffect::ThinkingPreviewChunk(_)))
        );
    }

    #[test]
    fn repeated_thinking_deltas_emit_one_spinner_state_edge() {
        let mut accum = ChatTurnSseAccum::default();
        let first = dispatch_chat_turn_sse_event_block(
            &sse("thinking_delta", ",\"content\":\"a\""),
            &mut accum,
            &mut vec![],
        );
        let second = dispatch_chat_turn_sse_event_block(
            &sse("thinking_delta", ",\"content\":\"b\""),
            &mut accum,
            &mut vec![],
        );

        assert!(
            first
                .iter()
                .any(|effect| matches!(effect, SseRenderEffect::StartThinkingSpinner))
        );
        assert!(
            !second
                .iter()
                .any(|effect| matches!(effect, SseRenderEffect::StartThinkingSpinner)),
            "spinner start is a state edge, not per-chunk progress"
        );
        assert_eq!(accum.reasoning_content, "ab");
    }

    #[test]
    fn root_terminal_stops_active_thinking_once() {
        let mut accum = ChatTurnSseAccum::default();
        let mut pending = vec![];
        dispatch_chat_turn_sse_event_block(
            &sse("session_info", ",\"session_id\":\"s\",\"run_id\":\"r\""),
            &mut accum,
            &mut pending,
        );
        dispatch_chat_turn_sse_event_block(
            &sse("thinking_delta", ",\"content\":\"working\""),
            &mut accum,
            &mut pending,
        );
        let effects = dispatch_chat_turn_sse_event_block(
            &sse("run_finished", ",\"run_id\":\"r\",\"status\":\"paused\""),
            &mut accum,
            &mut pending,
        );

        assert!(!accum.thinking_active);
        assert_eq!(
            effects
                .iter()
                .filter(|effect| matches!(effect, SseRenderEffect::StopThinkingSpinner))
                .count(),
            1
        );
    }

    #[test]
    fn malformed_sse_resets_thinking_edge_state() {
        let mut accum = ChatTurnSseAccum::default();
        let mut pending = vec![];
        dispatch_chat_turn_sse_event_block(
            &sse("thinking_delta", ",\"content\":\"before\""),
            &mut accum,
            &mut pending,
        );
        let stopped =
            dispatch_chat_turn_sse_event_block("data: {not-json}\n\n", &mut accum, &mut pending);
        let restarted = dispatch_chat_turn_sse_event_block(
            &sse("thinking_delta", ",\"content\":\"after\""),
            &mut accum,
            &mut pending,
        );

        assert!(
            stopped
                .iter()
                .any(|effect| matches!(effect, SseRenderEffect::StopThinkingSpinner))
        );
        assert!(
            restarted
                .iter()
                .any(|effect| matches!(effect, SseRenderEffect::StartThinkingSpinner))
        );
        assert!(accum.thinking_active);
    }

    #[test]
    fn text_done_replaces_a_divergent_streamed_prefix() {
        let a_default = ChatTurnSseAccum::default();
        let mut a = ChatTurnSseAccum {
            full_text: "already set".to_string(),
            ..a_default
        };
        let effects = dispatch_chat_turn_sse_event_block(
            &sse("text_done", ",\"full_text\":\"should not overwrite\""),
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.full_text, "should not overwrite");
        assert!(
            effects.is_empty(),
            "text_done after streamed deltas must not duplicate the answer"
        );
    }

    #[test]
    fn text_done_fills_empty_full_text() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse("text_done", ",\"full_text\":\"complete response\""),
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.full_text, "complete response");
    }

    // ── Additional edge-case tests ─────────────────────────────────────────

    #[test]
    fn usage_negative_tokens_treated_as_invalid() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse("usage", ",\"input_tokens\":-5,\"output_tokens\":-10"),
            &mut a,
            &mut vec![],
        );
        // Negative values fail as_u64() → both None → error branch
        assert!(!a.has_usage);
        assert!(a.error_message.is_some());
        assert!(a.error_message.as_ref().unwrap().contains("invalid usage"));
    }

    #[test]
    fn usage_float_tokens_treated_as_error() {
        let mut a = ChatTurnSseAccum::default();
        // Float values cannot be parsed as i64 by serde, so as_i64() returns None.
        dispatch_chat_turn_sse_event_block(
            "data: {\"type\":\"usage\",\"input_tokens\":3.14,\"output_tokens\":2.71}\n\n",
            &mut a,
            &mut vec![],
        );
        // The parser falls through to the "neither prompt nor completion" branch
        // and sets an error, OR it just stores 0. Either way, no panic.
        assert!(a.has_usage || a.error_message.is_some());
    }

    #[test]
    fn session_info_captures_id() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse("session_info", ",\"session_id\":\"sess-abc\""),
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.session_id.as_deref(), Some("sess-abc"));
    }

    #[test]
    fn explain_event_collected() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse("explain", ",\"detail\":\"selection took 5ms\""),
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.explain_turns.len(), 1);
    }

    #[test]
    fn durable_tool_call_start_with_empty_id_fails_closed() {
        let mut a = ChatTurnSseAccum::default();
        // Simulate a model returning a tool_call with empty id
        dispatch_chat_turn_sse_event_block(
            &sse(
                "tool_call_start",
                ",\"call_id\":\"\",\"tool\":\"bash\",\"arguments\":\"{\\\"command\\\":\\\"ls\\\"}\"",
            ),
            &mut a,
            &mut vec![],
        );
        assert!(a.tool_calls.is_empty());
        assert_eq!(a.error_kind, Some(astra_core::ErrorKind::ContractViolation));
    }

    #[test]
    fn durable_tool_call_start_with_missing_id_fails_closed() {
        let mut a = ChatTurnSseAccum::default();
        // No id field at all
        dispatch_chat_turn_sse_event_block(
            &sse("tool_call_start", ",\"tool\":\"grep\",\"arguments\":\"{}\""),
            &mut a,
            &mut vec![],
        );
        assert!(a.tool_calls.is_empty());
        assert_eq!(a.error_kind, Some(astra_core::ErrorKind::ContractViolation));
    }

    /// Durable replay and healthy live delivery merge by the exact call id.
    #[test]
    fn tool_call_merges_into_existing_tool_call_start() {
        let mut a = ChatTurnSseAccum::default();
        let mut p = vec![];
        // The durable wire adapter arrives first.
        dispatch_chat_turn_sse_event_block(
            &sse(
                "tool_call_start",
                ",\"call_id\":\"tc-1\",\"tool\":\"git\",\"arguments\":{\"action\":\"log\",\"n\":5}",
            ),
            &mut a,
            &mut p,
        );
        assert_eq!(a.tool_calls.len(), 1);
        assert_eq!(a.tool_call_id_index.get("tc-1"), Some(&0));
        // The admitted live wrapper carries the same canonical execution fact.
        dispatch_chat_turn_sse_event_block(
            &sse(
                "tool_call",
                ",\"tool_call\":{\"id\":\"tc-1\",\"type\":\"function\",\"function\":{\"name\":\"git\",\"arguments\":\"{\\\"action\\\":\\\"log\\\",\\\"n\\\":5}\"}}",
            ),
            &mut a,
            &mut p,
        );
        // Must still be 1 entry, not 2
        assert_eq!(
            a.tool_calls.len(),
            1,
            "tool_call should merge, not duplicate"
        );
        assert_eq!(a.tool_calls[0]["id"].as_str(), Some("tc-1"));
        assert_eq!(
            a.tool_calls[0]["function"]["arguments"].as_str(),
            Some("{\"action\":\"log\",\"n\":5}")
        );
        assert_eq!(a.tool_call_id_index.get("tc-1"), Some(&0));
    }

    /// tool_call with a new id (no prior tool_call_start) appends normally.
    #[test]
    fn tool_call_without_prior_start_appends() {
        let mut a = ChatTurnSseAccum::default();
        let mut p = vec![];
        dispatch_chat_turn_sse_event_block(
            &sse(
                "tool_call",
                ",\"tool_call\":{\"id\":\"tc-new\",\"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":\"{}\"}}",
            ),
            &mut a,
            &mut p,
        );
        assert_eq!(a.tool_calls.len(), 1);
        assert_eq!(a.tool_calls[0]["id"].as_str(), Some("tc-new"));
        assert_eq!(a.tool_call_id_index.get("tc-new"), Some(&0));
    }

    /// The Edge delivery path intentionally projects the canonical execution
    /// object into a flat public SSE card before emitting the matching
    /// `tool_request`.  The shared client adapter must consume that current
    /// wire shape without weakening the canonical execution boundary.
    #[test]
    fn edge_tool_call_projection_is_adapted_back_to_canonical_accum_shape() {
        let canonical = serde_json::json!({
            "id": "edge-call-1",
            "type": "function",
            "function": {
                "name": "bash",
                "arguments": "{\"command\":\"pwd\"}"
            }
        });
        let event = crate::stream_events::build_edge_tool_call_event(
            canonical.as_object().expect("canonical tool-call object"),
        );
        let block = format!(
            "data: {}\n\n",
            serde_json::to_string(&event).expect("serialize edge event")
        );

        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(&block, &mut a, &mut vec![]);

        assert_eq!(a.tool_calls, vec![canonical]);
        assert_eq!(a.tool_call_id_index.get("edge-call-1"), Some(&0));
        assert_eq!(a.error_kind, None);
        assert_eq!(a.error_message, None);
    }

    // ── Phase-R adversarial regression: usage nested fallback (Bug B) ──

    /// Regression: a provider that nests usage counters under
    /// `"usage": {...}` must still be decoded correctly. Before the fix,
    /// these counters silently zeroed out.
    #[test]
    fn usage_nested_fallback_captures_all_four_counters() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            "data: {\"type\":\"usage\",\"usage\":{\"input_tokens\":101,\"output_tokens\":42,\"cached_input_tokens\":7,\"cache_creation_tokens\":13}}\n\n",
            &mut a,
            &mut vec![],
        );
        assert!(a.has_usage);
        assert_eq!(a.prompt_tokens, 101);
        assert_eq!(a.completion_tokens, 42);
        assert_eq!(a.cache_read_tokens, 7);
        assert_eq!(a.cache_creation_tokens, 13);
        assert!(
            a.error_message.is_none(),
            "no invalid-usage error for nested shape"
        );
    }

    /// Contract pin: when BOTH flat and nested are present, flat wins.
    #[test]
    fn usage_flat_wins_over_nested() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            "data: {\"type\":\"usage\",\"input_tokens\":1,\"output_tokens\":2,\"cached_input_tokens\":3,\"cache_creation_tokens\":4,\"usage\":{\"input_tokens\":999,\"output_tokens\":999,\"cached_input_tokens\":999,\"cache_creation_tokens\":999}}\n\n",
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.prompt_tokens, 1);
        assert_eq!(a.completion_tokens, 2);
        assert_eq!(a.cache_read_tokens, 3);
        assert_eq!(a.cache_creation_tokens, 4);
    }

    /// Mixed shape: flat prompt/completion, nested cache_* still decoded.
    #[test]
    fn usage_mixed_flat_and_nested_per_field() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            "data: {\"type\":\"usage\",\"input_tokens\":50,\"output_tokens\":10,\"usage\":{\"cached_input_tokens\":11,\"cache_creation_tokens\":22}}\n\n",
            &mut a,
            &mut vec![],
        );
        assert_eq!(a.prompt_tokens, 50);
        assert_eq!(a.completion_tokens, 10);
        assert_eq!(a.cache_read_tokens, 11);
        assert_eq!(a.cache_creation_tokens, 22);
    }

    // ── Phase-R adversarial regression: nested function.id (Bug C) ──

    #[test]
    fn live_tool_call_rejects_nested_function_id() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            "data: {\"type\":\"tool_call\",\"tool_call\":{\"function\":{\"id\":\"real-id-42\",\"name\":\"bash\",\"arguments\":\"{}\"}}}\n\n",
            &mut a,
            &mut vec![],
        );
        assert!(a.tool_calls.is_empty());
        assert_eq!(a.error_kind, Some(astra_core::ErrorKind::ContractViolation));
    }

    #[test]
    fn live_tool_call_rejects_nested_function_call_id() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            "data: {\"type\":\"tool_call\",\"tool_call\":{\"function\":{\"call_id\":\"real-call-7\",\"name\":\"bash\",\"arguments\":\"{}\"}}}\n\n",
            &mut a,
            &mut vec![],
        );
        assert!(a.tool_calls.is_empty());
        assert_eq!(a.error_kind, Some(astra_core::ErrorKind::ContractViolation));
    }

    #[test]
    fn live_tool_call_rejects_conflicting_nested_function_id() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            "data: {\"type\":\"tool_call\",\"tool_call\":{\"id\":\"top-id\",\"type\":\"function\",\"function\":{\"id\":\"nested-id\",\"name\":\"bash\",\"arguments\":\"{}\"}}}\n\n",
            &mut a,
            &mut vec![],
        );
        assert!(a.tool_calls.is_empty());
        assert_eq!(a.error_kind, Some(astra_core::ErrorKind::ContractViolation));
    }

    #[test]
    fn live_tool_call_with_no_id_fails_closed() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            "data: {\"type\":\"tool_call\",\"tool_call\":{\"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":\"{}\"}}}\n\n",
            &mut a,
            &mut vec![],
        );
        assert!(a.tool_calls.is_empty());
        assert_eq!(a.error_kind, Some(astra_core::ErrorKind::ContractViolation));
    }

    // ── SSE dispatch contract pins ─────────────────────────────────────

    /// `[DONE]` is a terminal transport marker, not an ordinary ignored
    /// event. A malformed or malicious transport must not be able to append
    /// text or queue edge work after it has declared the stream complete.
    #[test]
    fn done_marker_stops_current_and_later_event_blocks() {
        let mut a = ChatTurnSseAccum::default();
        let mut pending = Vec::new();
        let block = "data: [DONE]\ndata: {\"type\":\"text_delta\",\"content\":\"after-done\"}\n\n";
        dispatch_chat_turn_sse_event_block(block, &mut a, &mut pending);
        dispatch_chat_turn_sse_event_block(
            "data: {\"type\":\"tool_request\",\"request_id\":\"after-done\",\"tool\":\"bash\",\"args\":{}}\n\n",
            &mut a,
            &mut pending,
        );
        assert!(a.stream_complete);
        assert!(a.full_text.is_empty());
        assert!(pending.is_empty());
    }

    /// Contract pin: only the FIRST malformed JSON line sets
    /// `error_message`. Later malformed lines must NOT overwrite the
    /// earlier message — the consumer surfaces the first symptom.
    #[test]
    fn malformed_json_only_first_sets_error_message() {
        let mut a = ChatTurnSseAccum::default();
        let block = "data: {not json one}\ndata: {not json two}\n\n";
        dispatch_chat_turn_sse_event_block(block, &mut a, &mut vec![]);
        assert_eq!(
            a.error_message.as_deref(),
            Some("Error: invalid JSON in SSE data"),
            "first malformed line sets the canonical error"
        );
        // Now simulate a subsequent block with another malformed line and
        // verify the prior error_message is preserved (not overwritten).
        let before = a.error_message.clone();
        dispatch_chat_turn_sse_event_block("data: {still not}\n\n", &mut a, &mut vec![]);
        assert_eq!(a.error_message, before);
    }

    #[test]
    fn live_tool_call_flat_wrapper_fields_fail_closed() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            "data: {\"type\":\"tool_call\",\"id\":\"classic-1\",\"function\":{\"name\":\"bash\",\"arguments\":\"{}\"}}\n\n",
            &mut a,
            &mut vec![],
        );
        assert!(a.tool_calls.is_empty());
        assert_eq!(a.error_kind, Some(astra_core::ErrorKind::ContractViolation));
    }

    #[test]
    fn live_tool_call_mixed_wrapper_and_canonical_payload_fails_closed() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            "data: {\"type\":\"tool_call\",\"id\":\"legacy\",\"tool_call\":{\"id\":\"call-1\",\"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":\"{}\"}}}\n\n",
            &mut a,
            &mut vec![],
        );
        assert!(a.tool_calls.is_empty());
        assert_eq!(a.error_kind, Some(astra_core::ErrorKind::ContractViolation));
    }

    #[test]
    fn durable_tool_call_start_with_partial_arguments_fails_closed() {
        let mut a = ChatTurnSseAccum::default();
        dispatch_chat_turn_sse_event_block(
            &sse(
                "tool_call_start",
                ",\"call_id\":\"call-1\",\"tool\":\"bash\",\"arguments\":\"{\\\"command\\\":\"",
            ),
            &mut a,
            &mut vec![],
        );
        assert!(a.tool_calls.is_empty());
        assert_eq!(a.error_kind, Some(astra_core::ErrorKind::ContractViolation));
    }
}

#[cfg(test)]
mod context_manifest_trace_sse_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn context_meta_sse_parses_context_manifest_trace() {
        let mut accum = ChatTurnSseAccum::default();
        let mut effects = Vec::new();
        let trace = json!({
            "source": "llm_context",
            "wire": {
                "message_count": 3,
                "total_cache_control_count": 2
            }
        });
        let block = format!(
            "data: {{\"type\":\"context_meta\",\"system_prompt_tokens\":42,\"context_manifest_trace\":{trace}}}\n\n"
        );

        dispatch_chat_turn_sse_event_block(&block, &mut accum, &mut effects);

        assert_eq!(accum.system_prompt_tokens, Some(42));
        assert_eq!(accum.context_manifest_trace, Some(trace));
    }

    #[test]
    fn context_meta_sse_captures_first_exact_provider_tool_surface() {
        let mut accum = ChatTurnSseAccum::default();
        let mut effects = Vec::new();
        dispatch_chat_turn_sse_event_block(
            "data: {\"type\":\"context_meta\",\"visible_tools\":[\"start_work\",\"agent\",\"agent\"]}\n\n",
            &mut accum,
            &mut effects,
        );
        dispatch_chat_turn_sse_event_block(
            "data: {\"type\":\"context_meta\",\"visible_tools\":[\"bash\"]}\n\n",
            &mut accum,
            &mut effects,
        );

        assert_eq!(
            accum.provider_visible_tools,
            Some(vec!["start_work".to_string(), "agent".to_string()])
        );
    }

    #[test]
    fn context_meta_sse_deduplicates_compaction_snapshots_but_keeps_distinct_retries() {
        let mut accum = ChatTurnSseAccum::default();
        let mut effects = Vec::new();
        let initial = json!({
            "id": "initial",
            "kind": "wire_assembly",
            "tier": "compact_history",
            "messages_before": 18,
            "messages_after": 10,
            "tokens_before": 12_000,
            "tokens_after": 7_000,
            "tokens_saved": 5_000
        });
        let retry = json!({
            "id": "context_window_retry:2:1",
            "kind": "wire_context_retry",
            "tier": "aggressive_prune",
            "messages_before": 22,
            "messages_after": 8,
            "tokens_before": 16_000,
            "tokens_after": 6_000,
            "tokens_saved": 10_000
        });
        let inconsistent = json!({
            "id": "inconsistent",
            "kind": "wire_context_retry",
            "tier": "aggressive_prune",
            "messages_before": 8,
            "messages_after": 12,
            "tokens_before": 6_000,
            "tokens_after": 7_000,
            "tokens_saved": 99
        });

        for compactions in [
            json!([initial.clone()]),
            json!([initial.clone()]),
            json!([initial, retry.clone()]),
            json!([inconsistent]),
        ] {
            let block = format!(
                "data: {}\n\n",
                json!({"type": "context_meta", "compactions": compactions})
            );
            dispatch_chat_turn_sse_event_block(&block, &mut accum, &mut effects);
        }

        assert_eq!(accum.context_compactions.len(), 2);
        assert_eq!(accum.context_compactions[0].id, "initial");
        assert_eq!(accum.context_compactions[0].tokens_saved, 5_000);
        assert_eq!(
            accum.context_compactions[1],
            serde_json::from_value(retry).unwrap()
        );
    }

    #[test]
    fn root_applied_guidance_is_retained_once_for_conversation_commit() {
        let mut accum = ChatTurnSseAccum::default();
        let mut pending = Vec::new();
        let stream = concat!(
            "data: {\"type\":\"run_started\",\"run_id\":\"root-run\"}\n\n",
            "data: {\"type\":\"user_intent_applied\",\"run_id\":\"root-run\",\"intent_id\":\"intent-1\",\"delivery\":\"guide_current_run\",\"status\":\"applied\",\"event_index\":7,\"content\":\"wait\"}\n\n",
            "data: {\"type\":\"user_intent_applied\",\"run_id\":\"root-run\",\"intent_id\":\"intent-1\",\"delivery\":\"guide_current_run\",\"status\":\"applied\",\"event_index\":7,\"content\":\"wait\"}\n\n",
            "data: {\"type\":\"user_intent_applied\",\"run_id\":\"child-run\",\"intent_id\":\"child-intent\",\"delivery\":\"guide_current_run\",\"status\":\"applied\",\"event_index\":9,\"content\":\"child\"}\n\n"
        );

        dispatch_chat_turn_sse_event_block(stream, &mut accum, &mut pending);

        assert_eq!(
            accum.applied_user_intents,
            vec![StreamAppliedUserIntent {
                intent_id: "intent-1".into(),
                delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                event_index: 7,
                content: "wait".into(),
            }]
        );
        assert!(accum.error_kind.is_none());
    }

    #[test]
    fn conflicting_applied_guidance_replay_fails_closed() {
        let mut accum = ChatTurnSseAccum::default();
        let mut pending = Vec::new();
        let stream = concat!(
            "data: {\"type\":\"run_started\",\"run_id\":\"root-run\"}\n\n",
            "data: {\"type\":\"user_intent_applied\",\"run_id\":\"root-run\",\"intent_id\":\"intent-1\",\"delivery\":\"guide_current_run\",\"status\":\"applied\",\"event_index\":7,\"content\":\"wait\"}\n\n",
            "data: {\"type\":\"user_intent_applied\",\"run_id\":\"root-run\",\"intent_id\":\"intent-1\",\"delivery\":\"guide_current_run\",\"status\":\"applied\",\"event_index\":8,\"content\":\"different\"}\n\n"
        );

        dispatch_chat_turn_sse_event_block(stream, &mut accum, &mut pending);

        assert_eq!(
            accum.error_kind,
            Some(astra_core::ErrorKind::ContractViolation)
        );
    }
}

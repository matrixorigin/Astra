//! Shared Memoria-compaction + LLM-message-assembly primitives.
//!
//! Used by both the server loop host (`ServerAgenticLoopHost::execute_turn`)
//! and the HTTP bridge (`InProcessChatTurnBridge::forward`). Before this
//! module each path had its own inlined copy of the Memoria call and the
//! wire-building logic — the bodies had drifted apart (e.g. the server
//! path discarded `CompactResult.boundary` and so lost the P2 compaction
//! context note) and every cache-annotation tweak had to be mirrored twice.
//!
//! Callers orchestrate three steps per turn:
//!
//!   1. [`MemoriaContext::compact`] (or [`MemoriaContext::compact_with_overrides`]
//!      for the emergency retry path) — async HTTP I/O that returns the
//!      full `CompactResult` (messages + boundary + tier).
//!   2. [`maybe_append_continuation_prompt`] — pure, reads the boundary
//!      signal and decides whether to append a neutral compaction note.
//!   3. [`assemble_llm_messages`] — pure, stitches system messages,
//!      compacted messages, optional post-compaction attachments, and
//!      Anthropic cache annotations into the final wire payload.

use serde_json::Value;

use crate::prompts::{CompactConfig, CompactionTier};
use crate::turn::cloud::compaction::CompactResult;
use crate::turn::cloud::memoria_compact::{
    MemoriaCompactConfig, MemoriaCompactParams, MemoriaPort, compact_with_memoria,
};
use crate::turn::prompt_cache::{PromptCacheConfig, apply_anthropic_cache_metadata};

pub(crate) const REQUIRED_RUNTIME_PREAMBLE_MARKER: &str = "__astra_required_runtime_context";
pub(crate) const RUNTIME_SYSTEM_CONTEXT_MARKER: &str = "__astra_runtime_system_context";
const TOOL_RUNTIME_CONTEXT_PREFIX: &str = "<runtime-context-after-tool>";
const TOOL_RUNTIME_CONTEXT_SUFFIX: &str = "</runtime-context-after-tool>";
const MAX_DERIVED_BUDGET_REFINEMENTS: usize = 8;

/// Convert a Memoria boundary into a typed provider-wire observation.
///
/// The shared estimator covers the fixed prefix, compacted history, and
/// visible tool schemas so server and bridge callers report the same facts.
pub(crate) fn observe_context_compaction(
    id: impl Into<String>,
    kind: astra_turn_core::compaction_types::CompactionKind,
    history_before: &[Value],
    result: &CompactResult,
    fixed_context: &[Value],
    visible_tools: &[Value],
    window_policy: Option<crate::prompts::ContextWindowPolicy>,
) -> Option<astra_turn_core::chat_turn_sse_dispatch::ContextCompactionObservation> {
    result.boundary.as_ref()?;
    if result.messages == history_before {
        return None;
    }

    let estimate = |history: &[Value]| -> u64 {
        fixed_context
            .iter()
            .chain(history)
            .chain(visible_tools)
            .map(crate::prompts::estimate_json_value_tokens)
            .map(|tokens| u64::try_from(tokens).unwrap_or(u64::MAX))
            .fold(0_u64, u64::saturating_add)
    };
    let tokens_before = estimate(history_before);
    let tokens_after = estimate(&result.messages);
    if tokens_after >= tokens_before {
        return None;
    }

    let post_compaction_target_tokens = window_policy
        .map(|policy| u64::try_from(policy.post_compaction_target_tokens()).unwrap_or(u64::MAX));
    let effectiveness = match post_compaction_target_tokens {
        Some(target) if tokens_after <= target => {
            astra_turn_core::chat_turn_sse_dispatch::ContextCompactionEffectiveness::Sufficient
        }
        Some(_) => {
            astra_turn_core::chat_turn_sse_dispatch::ContextCompactionEffectiveness::Insufficient
        }
        None => astra_turn_core::chat_turn_sse_dispatch::ContextCompactionEffectiveness::Unmeasured,
    };
    Some(
        astra_turn_core::chat_turn_sse_dispatch::ContextCompactionObservation {
            id: id.into(),
            kind,
            tier: result.tier,
            messages_before: history_before.len().min(u64::MAX as usize) as u64,
            messages_after: result.messages.len().min(u64::MAX as usize) as u64,
            tokens_before,
            tokens_after,
            tokens_saved: tokens_before - tokens_after,
            post_compaction_target_tokens,
            effectiveness,
        },
    )
}

/// Preflight estimate for the final provider payload.
///
/// This is deliberately observational: provider tokenizers remain the hard
/// authority. The soft target can drive diagnostics without turning an
/// approximation into another destructive compaction trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WireBudgetStatus {
    pub estimated_input_tokens: usize,
    pub requested_output_tokens: usize,
    pub reserved_protocol_tokens: usize,
    pub effective_input_limit: usize,
    pub model_limit: usize,
}

impl WireBudgetStatus {
    #[must_use]
    pub fn with_requested_output_tokens(self, requested_output_tokens: usize) -> Self {
        Self {
            requested_output_tokens,
            ..self
        }
    }

    #[must_use]
    pub fn soft_target_exceeded(self) -> bool {
        self.estimated_input_tokens > self.effective_input_limit
    }

    #[must_use]
    pub fn hard_limit_exceeded(self) -> bool {
        self.estimated_input_tokens
            .saturating_add(self.requested_output_tokens)
            .saturating_add(self.reserved_protocol_tokens)
            > self.model_limit
    }

    #[must_use]
    pub fn to_json(self) -> Value {
        serde_json::json!({
            "estimated_input_tokens": self.estimated_input_tokens,
            "requested_output_tokens": self.requested_output_tokens,
            "reserved_protocol_tokens": self.reserved_protocol_tokens,
            "effective_input_limit": self.effective_input_limit,
            "model_limit": self.model_limit,
            "soft_target_exceeded": self.soft_target_exceeded(),
            "hard_limit_exceeded": self.hard_limit_exceeded(),
            "enforcement": "observational_estimate_provider_authoritative",
        })
    }
}

pub(crate) fn set_manifest_wire_budget(trace: &mut Value, status: WireBudgetStatus) {
    if !trace.is_object() {
        *trace = serde_json::json!({});
    }
    if !trace["wire"].is_object() {
        trace["wire"] = serde_json::json!({});
    }
    trace["wire"]["budget"] = status.to_json();
}

pub(crate) fn wire_budget_status_with_metadata(
    messages: &[Value],
    tools: &[Value],
    model_name: &str,
    context_window: Option<u32>,
    max_completion_tokens: Option<u32>,
    requested_output_tokens: usize,
) -> WireBudgetStatus {
    let tool_tokens = tools
        .iter()
        .map(crate::prompts::estimate_json_value_tokens)
        .sum();
    let estimated_input_tokens =
        crate::prompts::estimate_tokens_cache_aware_split(&[], messages, tool_tokens).total_tokens;
    let budget = crate::prompts::budget_for_model_with_metadata(
        Some(model_name),
        context_window,
        max_completion_tokens,
    );
    let policy = budget.window_policy();
    WireBudgetStatus {
        estimated_input_tokens,
        requested_output_tokens,
        reserved_protocol_tokens: policy.reserved_protocol_tokens,
        effective_input_limit: budget.effective_input_limit(),
        model_limit: budget.model_limit,
    }
}

pub(crate) fn augment_manifest_trace_with_wire_budget_and_metadata(
    trace: &mut Value,
    messages: &[Value],
    tools: &[Value],
    model_name: &str,
    context_window: Option<u32>,
    max_completion_tokens: Option<u32>,
    requested_output_tokens: usize,
) -> WireBudgetStatus {
    let status = wire_budget_status_with_metadata(
        messages,
        tools,
        model_name,
        context_window,
        max_completion_tokens,
        requested_output_tokens,
    );
    set_manifest_wire_budget(trace, status);
    status
}

pub(crate) fn required_runtime_preamble_message(text: &str) -> Option<Value> {
    runtime_system_context_message(text, true)
}

pub(crate) fn runtime_system_context_message(text: &str, required: bool) -> Option<Value> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut message = serde_json::json!({
        "role": "system",
        "content": text,
    });
    message[RUNTIME_SYSTEM_CONTEXT_MARKER] = Value::Bool(true);
    if required {
        message[REQUIRED_RUNTIME_PREAMBLE_MARKER] = Value::Bool(true);
    }
    Some(message)
}

fn runtime_system_context_from_message(mut message: Value) -> Option<Value> {
    let required = is_required_runtime_preamble(&message);
    let content = message.get("content").cloned();
    let empty = match content.as_ref() {
        None | Some(Value::Null) => true,
        Some(Value::String(text)) => text.trim().is_empty(),
        Some(Value::Array(blocks)) => blocks.is_empty(),
        Some(_) => false,
    };
    if empty {
        if required {
            tracing::error!(
                "required runtime system context has empty or missing content; refusing to fabricate replacement text"
            );
        }
        return None;
    }

    let object = message.as_object_mut()?;
    object.insert("role".to_string(), Value::String("system".to_string()));
    object.insert(RUNTIME_SYSTEM_CONTEXT_MARKER.to_string(), Value::Bool(true));
    if required {
        object.insert(
            REQUIRED_RUNTIME_PREAMBLE_MARKER.to_string(),
            Value::Bool(true),
        );
    }
    Some(message)
}

fn current_turn_boundary(messages: &[Value]) -> usize {
    messages
        .iter()
        .rposition(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        .unwrap_or(messages.len())
}

fn current_tail_boundary(messages: &[Value]) -> usize {
    messages
        .iter()
        .rposition(|message| message.get("role").and_then(Value::as_str) != Some("system"))
        .unwrap_or(messages.len())
}

pub(crate) fn insert_runtime_system_context(
    messages: &mut Vec<Value>,
    runtime_messages: Vec<Value>,
    placement: astra_turn_core::cache_placement::VolatilePlacement,
) -> Option<usize> {
    if runtime_messages.is_empty() {
        return None;
    }
    let boundary = if matches!(
        placement,
        astra_turn_core::cache_placement::VolatilePlacement::TailSuffix
    ) {
        current_tail_boundary(messages)
    } else {
        current_turn_boundary(messages)
    };
    messages.splice(boundary..boundary, runtime_messages);
    Some(boundary)
}

pub(crate) fn is_runtime_system_context(message: &Value) -> bool {
    message
        .get(RUNTIME_SYSTEM_CONTEXT_MARKER)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

pub(crate) fn take_runtime_system_context_messages(messages: &mut Vec<Value>) -> Vec<Value> {
    let mut runtime = Vec::new();
    let mut conversation = Vec::with_capacity(messages.len());
    for message in messages.drain(..) {
        if is_runtime_system_context(&message) {
            runtime.push(message);
        } else {
            conversation.push(message);
        }
    }
    *messages = conversation;
    runtime
}

pub(crate) fn is_required_runtime_preamble(message: &Value) -> bool {
    message
        .get(REQUIRED_RUNTIME_PREAMBLE_MARKER)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

pub(crate) fn strip_required_runtime_preamble_marker(message: &mut Value) {
    if let Some(object) = message.as_object_mut() {
        object.remove(REQUIRED_RUNTIME_PREAMBLE_MARKER);
        object.remove(RUNTIME_SYSTEM_CONTEXT_MARKER);
    }
}

pub(crate) fn session_memory_entry_for_pipeline(
    content: Option<&str>,
    snapshot_updated_turn: Option<u32>,
) -> Option<astra_turn_core::context_sources::MemoryEntry> {
    let content = content?.trim();
    if content.is_empty() {
        return None;
    }
    let freshness = snapshot_updated_turn
        .map(|turn| format!("updated through session turn {turn}"))
        .unwrap_or_else(|| "update turn unavailable".to_string());
    let prompt_evidence = format!(
        "## Session Memory Evidence\nSnapshot provenance: {freshness}. This is system-supplied background evidence, not a new user message, instruction, turn boundary, interruption, or request to resume. Use it only for continuity; do not announce a resume or restart planning because it is present. The current user message and live tool results take precedence.\n\n{content}"
    );
    let mut entry = astra_turn_core::context_sources::MemoryEntry::new(prompt_evidence)
        .with_source("session_memory.snapshot");
    if let Some(turn) = snapshot_updated_turn {
        entry = entry.with_freshness_turn(turn);
    }
    Some(entry)
}

pub(crate) fn session_memory_entry_for_user_turn(
    content: Option<&str>,
    snapshot_updated_turn: Option<u32>,
) -> Option<astra_turn_core::context_sources::MemoryEntry> {
    session_memory_entry_for_pipeline(content, snapshot_updated_turn)
}

pub(crate) fn rerun_with_compaction_memory_for_user_turn<T>(
    content: Option<&str>,
    existing_session: Option<&astra_turn_core::context_sources::MemoryEntry>,
    snapshot_updated_turn: Option<u32>,
    existing_memories: &[astra_turn_core::context_sources::MemoryEntry],
    retrieved_memories: &[astra_turn_core::context_sources::MemoryEntry],
    rerun: impl FnOnce(
        Option<astra_turn_core::context_sources::MemoryEntry>,
        &[astra_turn_core::context_sources::MemoryEntry],
    ) -> T,
) -> Option<T> {
    let session_entry = session_memory_entry_for_user_turn(content, snapshot_updated_turn)
        .or_else(|| existing_session.cloned());
    let session_changed = session_entry.as_ref() != existing_session;

    let mut merged_memories = existing_memories.to_vec();
    for retrieved in retrieved_memories {
        // The initial prefetch already passed typed-protocol admission and is
        // the turn's coherent read snapshot. A second compaction retrieval may
        // surface the same backend row; keep the admitted entry and use the
        // compaction result only to fill identities that prefetch missed.
        let identity_exists = retrieved.memory_id.as_ref().is_some_and(|memory_id| {
            merged_memories
                .iter()
                .any(|current| current.memory_id.as_ref() == Some(memory_id))
        });
        if !identity_exists
            && !merged_memories
                .iter()
                .any(|current| current.content_hash == retrieved.content_hash)
        {
            merged_memories.push(retrieved.clone());
        }
    }
    let memories_changed = merged_memories != existing_memories;

    if !session_changed && !memories_changed {
        return None;
    }
    Some(rerun(session_entry, &merged_memories))
}

/// Session-level context that Memoria compaction needs. Bundled into one
/// struct so callers don't pass a long list of positional arguments — each
/// field is named and independently testable.
pub(crate) struct MemoriaContext<'a> {
    /// Session id used for Memoria storage scope + cache-edit pin key.
    pub session_id: &'a str,
    /// Model the main turn is calling — used to size char budgets. Auth
    /// (api_key / base_url / provider / headers) is not plumbed here because
    /// the summary client is constructed by the caller and injected below;
    /// this module stays decoupled from HTTP credentials.
    pub model_name: &'a str,
    /// Registry/model-config context window. `None` means use the generic
    /// 200K default; never infer this from the model name.
    pub context_window: Option<u32>,
    /// Optional HTTP client for Memoria retrieval. `None` = skip retrieval,
    /// fall back to pure truncation.
    pub memoria_client: Option<&'a dyn MemoriaPort>,
    /// Optional summary LLM client. `None` = skip LLM summarization tier.
    pub summary_client: Option<&'a dyn astra_turn_core::cloud_summary::SummaryLlmClient>,
    /// Pipeline-selected compaction tier (authoritative — do NOT re-derive).
    pub tier: CompactionTier,
    /// Optional pre-parsed session facts (bridge path provides these;
    /// server path does not yet).
    pub session_facts: Option<astra_turn_types::session_facts::SessionFacts>,
}

/// Caller-side overrides for Memoria budget knobs that the context-window
/// recovery path needs. The main turn path leaves every field `None` — the
/// `MemoriaContext` then derives sensible defaults from the model budget and
/// the `tier` on `MemoriaContext` itself. The emergency retry path (triggered
/// by a prompt-too-long response) fills these in with tighter values.
#[derive(Default)]
pub(crate) struct BudgetOverrides {
    pub budget_chars: Option<usize>,
    pub keep_chars: Option<usize>,
    pub keep_recent_turns: Option<usize>,
    pub current_tokens: Option<usize>,
    pub tier: Option<CompactionTier>,
}

/// Fully resolved budget values that Memoria needs. Produced either by
/// deriving from the model or by applying caller overrides on top of the
/// derived defaults.
struct ResolvedBudget {
    budget_chars: usize,
    keep_chars: usize,
    keep_recent_turns: usize,
    current_tokens: usize,
    tier: CompactionTier,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SerializedHistoryMeasurement {
    chars: usize,
    bytes: u64,
}

fn serialized_history_measurement(history: &[Value]) -> SerializedHistoryMeasurement {
    history.iter().fold(
        SerializedHistoryMeasurement { chars: 0, bytes: 0 },
        |measurement, message| match serde_json::to_string(message) {
            Ok(encoded) => SerializedHistoryMeasurement {
                chars: measurement.chars.saturating_add(encoded.chars().count()),
                bytes: measurement
                    .bytes
                    .saturating_add(u64::try_from(encoded.len()).unwrap_or(u64::MAX)),
            },
            Err(error) => {
                astra_core::history_work::record_serialization_failure(
                    astra_core::history_work::HistoryWorkSite::HistoryBudgetEstimationSerialization,
                    &error,
                );
                SerializedHistoryMeasurement {
                    chars: measurement.chars.saturating_add(1),
                    ..measurement
                }
            }
        },
    )
}

fn history_budget_chars(
    budget: &crate::prompts::ContextBudget,
    fixed_context_tokens: usize,
    history: &[Value],
    history_tokens: usize,
) -> usize {
    let available_tokens = budget
        .effective_input_limit()
        .saturating_sub(fixed_context_tokens);
    if available_tokens == 0 {
        return 0;
    }
    if history_tokens == 0 {
        return available_tokens.saturating_mul(4);
    }

    let measurement = serialized_history_measurement(history);
    if astra_core::history_work::instrumentation_enabled() {
        astra_core::history_work::record_operation(
            astra_core::history_work::HistoryWorkSite::HistoryBudgetEstimationSerialization,
            measurement.bytes,
            u64::try_from(history.len()).unwrap_or(u64::MAX),
            0,
        );
    }
    let ascii_ceiling = history_tokens.saturating_mul(4);
    available_tokens.saturating_mul(measurement.chars.min(ascii_ceiling)) / history_tokens
}

impl BudgetOverrides {
    fn apply(self, base: ResolvedBudget) -> ResolvedBudget {
        ResolvedBudget {
            budget_chars: self.budget_chars.unwrap_or(base.budget_chars),
            keep_chars: self.keep_chars.unwrap_or(base.keep_chars),
            keep_recent_turns: self.keep_recent_turns.unwrap_or(base.keep_recent_turns),
            current_tokens: self.current_tokens.unwrap_or(base.current_tokens),
            tier: self.tier.unwrap_or(base.tier),
        }
    }
}

impl<'a> MemoriaContext<'a> {
    fn context_budget(&self) -> crate::prompts::ContextBudget {
        crate::prompts::budget_for_model_with_override(Some(self.model_name), self.context_window)
    }

    /// Run Memoria-based history compaction. Returns the full `CompactResult`
    /// so callers can react to `boundary.is_some()` (e.g. for the P2
    /// compaction context note).
    pub async fn compact(
        &self,
        messages: &[Value],
        system_messages: &[Value],
        visible_tools: &[Value],
    ) -> CompactResult {
        self.compact_with_overrides(
            messages,
            system_messages,
            visible_tools,
            BudgetOverrides::default(),
        )
        .await
    }

    /// Same as [`Self::compact`] but accepts budget overrides for emergency
    /// retry after a context-window error. Main-turn callers should prefer
    /// [`Self::compact`] which uses model-derived defaults.
    pub async fn compact_with_overrides(
        &self,
        messages: &[Value],
        system_messages: &[Value],
        visible_tools: &[Value],
        overrides: BudgetOverrides,
    ) -> CompactResult {
        let uses_derived_history_budget = overrides.budget_chars.is_none();
        let budget = self.context_budget();
        // `current_tokens` is a pressure signal for Memoria retrieval; the
        // authoritative compaction tier is `self.tier` (or the override). The
        // cache-aware estimate just tunes retrieval aggressiveness, so we
        // count tool schemas alongside messages for a single total.
        let tool_schema_tokens: usize = visible_tools
            .iter()
            .map(crate::prompts::estimate_json_value_tokens)
            .sum();
        let cache_est = crate::prompts::estimate_tokens_cache_aware_split(
            system_messages,
            messages,
            tool_schema_tokens,
        );

        let resolved = overrides.apply(ResolvedBudget {
            // Memoria controls only the conversation working set. Stable
            // system messages and tool schemas consume the same provider
            // window, so reserve their concrete cost before deriving the
            // history budget.
            budget_chars: history_budget_chars(
                &budget,
                cache_est.cache_eligible_tokens,
                messages,
                cache_est.volatile_tokens,
            ),
            keep_chars: 2_000,
            keep_recent_turns: budget.keep_recent_turns,
            current_tokens: cache_est.total_tokens,
            tier: self.tier,
        });

        let memoria_config = MemoriaCompactConfig::default();
        let memoria_params = MemoriaCompactParams {
            budget_chars: resolved.budget_chars,
            keep_chars: resolved.keep_chars,
            tier: resolved.tier,
            keep_recent_turns: resolved.keep_recent_turns,
            current_tokens: resolved.current_tokens,
            session_facts: self.session_facts.clone(),
        };

        let compact_config = CompactConfig::from_env();

        let mut result = compact_with_memoria(
            messages,
            Some(self.session_id),
            &memoria_config,
            &memoria_params,
            self.memoria_client,
            Some(&compact_config),
            self.summary_client,
        )
        .await;

        // The char budget is derived from the observed chars/token density.
        // Compaction itself can change that density (for example by replacing
        // a large tool result with a short structured projection). Refine
        // against the already-bounded output in the same operation so later
        // turns do not repeatedly erode an unchanged conversation. The first
        // pass is the only one over the original long history; refinements
        // operate on the bounded working set and never repeat Memoria/LLM I/O.
        if uses_derived_history_budget && result.boundary.is_some() {
            for _ in 0..MAX_DERIVED_BUDGET_REFINEMENTS {
                let refined_estimate = crate::prompts::estimate_tokens_cache_aware_split(
                    system_messages,
                    &result.messages,
                    tool_schema_tokens,
                );
                let refined_budget_chars = history_budget_chars(
                    &budget,
                    refined_estimate.cache_eligible_tokens,
                    &result.messages,
                    refined_estimate.volatile_tokens,
                );
                if serialized_history_measurement(&result.messages).chars <= refined_budget_chars {
                    break;
                }

                let mut messages = std::mem::take(&mut result.messages);
                let refined =
                    crate::turn::cloud::compaction_engine::CompactionEngine::compact_tiered(
                        &mut messages,
                        refined_budget_chars,
                        resolved.keep_chars,
                        resolved.tier,
                        resolved.keep_recent_turns,
                    );
                result.messages = refined.messages;
                if refined.boundary.is_none() {
                    break;
                }
                if let Some(boundary) = result.boundary.as_mut() {
                    boundary.messages_after = result.messages.len();
                }
            }
        }

        result
    }
}

/// Post-compaction state-driven attachments that the server path re-injects
/// so the LLM retains skill + file context after history compaction.
///
/// Empty on the bridge path today — the bridge is ephemeral per-request and
/// has no session-state tracking for invoked skills or recently-read files.
#[derive(Default)]
pub(crate) struct PostCompactAttachments<'a> {
    /// Skills that have been invoked earlier in the session, sorted most-
    /// recent-first. Their instructions get re-injected (truncated) so the
    /// LLM can follow them even after the original tool_result was compacted.
    pub invoked_skills: Vec<InvokedSkillRef<'a>>,
    /// Recently-read files `(absolute_path, turn_number)` — restored as
    /// required runtime-system context with truncated content so the LLM
    /// remembers the code it was looking at before compaction.
    pub recent_file_reads: &'a [(String, u32)],
    /// CWD for resolving relative file paths in `recent_file_reads`.
    pub cwd: Option<&'a str>,
}

/// Minimal view of a single invoked skill that `assemble_llm_messages_with_cache_capability` needs.
/// Copied out of the full `SkillInvocationRecord` so this module doesn't pull
/// in the runtime's full state types. The caller is responsible for ordering
/// (most-recent-first); we emit in the supplied order.
pub(crate) struct InvokedSkillRef<'a> {
    pub name: &'a str,
    pub content: &'a str,
}

const COMPACTION_CONTEXT_NOTE: &str = "\
Context was compacted before this point. This runtime note is not a new user \
request and does not authorize resuming old tasks. Use the latest real user \
message plus any current tool result to decide whether to continue, answer a \
status/why question, or stop; do not run tools solely because this note exists.";

/// Queue a neutral runtime-system compaction note when compaction removed
/// messages and the last remaining message is not a real user message.
///
/// Pure function — no I/O. Idempotent when called on messages that already
/// end in a user message.
pub(crate) fn maybe_append_continuation_prompt(
    messages: &mut Vec<Value>,
    compact_boundary_hit: bool,
) {
    if !compact_boundary_hit || messages.len() < 2 {
        return;
    }
    let already_queued = messages.last().is_some_and(|message| {
        is_runtime_system_context(message)
            && message.get("content").and_then(Value::as_str) == Some(COMPACTION_CONTEXT_NOTE)
    });
    if already_queued {
        return;
    }
    let last_is_user = messages
        .last()
        .and_then(|m| m.get("role").and_then(Value::as_str))
        == Some("user");
    if last_is_user {
        return;
    }
    if let Some(message) = runtime_system_context_message(COMPACTION_CONTEXT_NOTE, true) {
        messages.push(message);
    }
}

/// Stitch the final wire-ready `llm_messages` array.
///
/// Order:
///
/// 1. `system_messages` (from the context pipeline).
/// 2. `compacted_messages` (conversation history from Memoria), unchanged.
/// 3. Model-visible runtime context is inserted according to the provider's
///    volatile placement. Auto-prefix providers place it immediately before
///    the current tail message so later tool rounds can reuse the accumulated
///    current-turn prefix. Other non-marker providers keep the current-user
///    boundary. Real user/tool messages remain byte-for-byte unchanged.
/// 4. `strip_stale_reasoning` is applied in place.
/// 5. `apply_anthropic_cache_metadata` (Anthropic path only).
pub(crate) fn assemble_llm_messages_with_cache_capability(
    system_messages: Vec<Value>,
    volatile_preamble: Vec<Value>,
    drained_volatile: Vec<crate::turn::agentic_loop::host::VolatileInjection>,
    mut compacted_messages: Vec<Value>,
    attachments: &PostCompactAttachments<'_>,
    session_id: &str,
    provider: &str,
    model_name: &str,
    thinking: &astra_turn_core::thinking_config::ThinkingConfig,
    cache_capability: Option<astra_turn_core::cache_placement::CacheCapability>,
    cache_cfg: &PromptCacheConfig,
) -> Vec<Value> {
    let cache_cap =
        astra_turn_core::cache_placement::CacheCapability::from_explicit_or_provider_model(
            cache_capability,
            provider,
            model_name,
        );
    let suppress_volatile = matches!(
        cache_cap.volatile_placement,
        astra_turn_core::cache_placement::VolatilePlacement::CurrentUserOnly
    );
    // Structured volatile lane (`state.volatile_pending`): drained upstream,
    // rendered to the provider-specific runtime-system slot.
    // Producers use `state.push_volatile(Kind, content)` and never touch
    // `state.messages[]` for volatile content, so `messages[]` stays byte-
    // stable across rounds. The runtime system message is wire-only and never
    // becomes canonical user/tool history.
    let mut runtime_system_messages = volatile_preamble
        .into_iter()
        .filter(|message| !suppress_volatile || is_required_runtime_preamble(message))
        .filter_map(runtime_system_context_from_message)
        .collect::<Vec<_>>();
    runtime_system_messages.extend(
        render_drained_volatile_messages(&drained_volatile)
            .into_iter()
            .filter(|message| !suppress_volatile || is_required_runtime_preamble(message)),
    );
    runtime_system_messages.extend(
        take_runtime_system_context_messages(&mut compacted_messages)
            .into_iter()
            .filter(|message| !suppress_volatile || is_required_runtime_preamble(message)),
    );

    if !attachments.invoked_skills.is_empty() {
        let mut builder = astra_turn_core::cloud_attachments::AttachmentBuilder::new();
        // Caller supplies `invoked_skills` already in the preferred order
        // (most-recent-first). Emitting in the same order matches legacy
        // output — do not re-sort here; re-sorting would flip bytes.
        for skill in &attachments.invoked_skills {
            builder.add_skill(skill.name, skill.content);
        }
        let built = builder.build();
        runtime_system_messages.extend(built.to_messages().into_iter().filter_map(|message| {
            message
                .get("content")
                .and_then(Value::as_str)
                .and_then(|content| runtime_system_context_message(content, true))
        }));
    }

    if !attachments.recent_file_reads.is_empty() {
        runtime_system_messages.extend(
            astra_turn_core::cloud_attachments::restore_recent_files(
                attachments.recent_file_reads,
                attachments.cwd,
            )
            .into_iter()
            .filter_map(|message| {
                message
                    .get("content")
                    .and_then(Value::as_str)
                    .and_then(|content| runtime_system_context_message(content, true))
            }),
        );
    }
    let mut llm_messages = system_messages;
    llm_messages.extend(compacted_messages);
    let runtime_system_start = if runtime_system_messages.is_empty() {
        None
    } else if matches!(
        cache_cap.volatile_placement,
        astra_turn_core::cache_placement::VolatilePlacement::MarkerIsolated
    ) {
        let start = llm_messages.len();
        llm_messages.extend(runtime_system_messages);
        Some(start)
    } else {
        insert_runtime_system_context(
            &mut llm_messages,
            runtime_system_messages,
            cache_cap.volatile_placement,
        )
    };
    let reasoning_policy = astra_turn_core::edge_ledger::ReasoningReplayPolicy::infer(
        &llm_messages,
        thinking,
        provider,
        model_name,
    );
    astra_turn_core::edge_ledger::strip_stale_reasoning_with_policy(
        &mut llm_messages,
        &reasoning_policy,
    );

    // Keep Anthropic's existing message-level cache boundary on the last stable
    // message before runtime context. This preserves the pre-#629 marker logic;
    // only the runtime message's role and placement change here.
    if cache_cfg.should_annotate() {
        if let Some(prefix_end) = runtime_system_start {
            apply_anthropic_cache_metadata(&mut llm_messages[..prefix_end], cache_cfg, session_id);
        } else {
            apply_anthropic_cache_metadata(&mut llm_messages, cache_cfg, session_id);
        }
    }
    astra_core::history_work::record_serialized_value(
        astra_core::history_work::HistoryWorkSite::ProviderWireAssembly,
        &llm_messages,
    );
    llm_messages
}

pub(crate) fn strip_runtime_context_from_tool_message(message: &mut Value) {
    if message.get("role").and_then(Value::as_str) != Some("tool") {
        return;
    }
    fn strip_suffix(text: &mut String) {
        if let Some(index) = text.rfind(TOOL_RUNTIME_CONTEXT_PREFIX)
            && text[index..]
                .trim_end()
                .ends_with(TOOL_RUNTIME_CONTEXT_SUFFIX)
        {
            let mut end = index;
            while end > 0 && text.as_bytes()[end - 1].is_ascii_whitespace() {
                end -= 1;
            }
            text.truncate(end);
        }
    }

    match message.get_mut("content") {
        Some(Value::String(text)) => strip_suffix(text),
        Some(Value::Array(blocks)) => {
            for block in blocks.iter_mut() {
                for field in ["text", "content"] {
                    if let Some(text) = block.get_mut(field).and_then(|value| value.as_str()) {
                        let mut stripped = text.to_string();
                        strip_suffix(&mut stripped);
                        block[field] = Value::String(stripped);
                    }
                }
            }
            blocks.retain(|block| {
                let text_fields = ["text", "content"]
                    .iter()
                    .filter_map(|field| block.get(*field).and_then(Value::as_str))
                    .collect::<Vec<_>>();
                text_fields.is_empty() || text_fields.iter().any(|text| !text.trim().is_empty())
            });
        }
        _ => {}
    }
}

fn render_drained_volatile_messages(
    drained: &[crate::turn::agentic_loop::host::VolatileInjection],
) -> Vec<Value> {
    let mut out = Vec::new();
    for inj in drained {
        let edge_injection = astra_turn_core::chat_turn_edge_profile::RuntimeVolatileInjection {
            kind: inj.kind.wire_kind(),
            delivery_class: inj.kind.delivery_class(),
            payload: inj.payload.clone(),
            round_index: inj.round_index,
        };
        let Some(text) = edge_injection.render_for_prompt() else {
            continue;
        };
        let required = inj.kind.delivery_class()
            == astra_turn_core::chat_turn_edge_profile::VolatileDeliveryClass::RequiredContext;
        if let Some(message) = runtime_system_context_message(&text, required) {
            out.push(message);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cache_cfg() -> PromptCacheConfig {
        PromptCacheConfig::latch("openai", "gpt-4")
    }

    fn anthropic_cache_cfg() -> PromptCacheConfig {
        PromptCacheConfig::latch("anthropic", "claude-sonnet-4")
    }

    fn message_text(message: &Value) -> String {
        match message.get("content") {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Array(blocks)) => blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        }
    }

    #[test]
    fn context_compaction_observation_preserves_typed_wire_facts() {
        use crate::turn::cloud::compaction::{CompactBoundary, CompactResult, CompactTrigger};
        use astra_turn_core::compaction_types::CompactionKind;

        let before = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "检查约束"}]}),
            json!({"role": "assistant", "content": "analysis ".repeat(800)}),
            json!({"role": "assistant", "tool_calls": [{"id": "call-1", "type": "function", "function": {"name": "lookup", "arguments": "{}"}}]}),
            json!({"role": "tool", "tool_call_id": "call-1", "content": {"rows": [1, 2, 3]}}),
            json!({"role": "user", "content": "continue"}),
        ];
        let after = vec![
            json!({"role": "system", "content": "structured summary"}),
            before.last().expect("latest user").clone(),
        ];
        let result = CompactResult {
            messages: after,
            boundary: Some(CompactBoundary::new(
                CompactTrigger::Auto,
                CompactionTier::CompactHistory,
            )),
            tier: CompactionTier::CompactHistory,
            session_memory_context: None,
            retrieved_memory_entries: Vec::new(),
            runtime_contexts: Vec::new(),
        };

        let observation = observe_context_compaction(
            "wire-1",
            CompactionKind::WireAssembly,
            &before,
            &result,
            &[json!({"role": "system", "content": "fixed"})],
            &[json!({"type": "function", "function": {"name": "lookup"}})],
            Some(
                crate::prompts::budget_for_model_with_metadata(
                    Some("model"),
                    Some(10_000),
                    Some(1_000),
                )
                .window_policy(),
            ),
        )
        .expect("a shrinking boundary is observable");

        assert_eq!(observation.kind, CompactionKind::WireAssembly);
        assert_eq!(observation.tier, CompactionTier::CompactHistory);
        assert_eq!(observation.messages_before, 5);
        assert_eq!(observation.messages_after, 2);
        assert!(observation.tokens_before > observation.tokens_after);
        assert_eq!(
            observation.tokens_saved,
            observation.tokens_before - observation.tokens_after
        );
        assert!(observation.post_compaction_target_tokens.is_some());
        assert_eq!(
            observation.effectiveness,
            astra_turn_core::chat_turn_sse_dispatch::ContextCompactionEffectiveness::Sufficient
        );
        assert!(observation.is_consistent());
    }

    #[test]
    fn context_compaction_observation_requires_boundary_and_prompt_reduction() {
        use crate::turn::cloud::compaction::{CompactResult, CompactTrigger};
        use astra_turn_core::compaction_types::CompactionKind;

        let messages = vec![json!({"role": "user", "content": "stable"})];
        let no_boundary = CompactResult {
            messages: Vec::new(),
            boundary: None,
            tier: CompactionTier::CompactHistory,
            session_memory_context: None,
            retrieved_memory_entries: Vec::new(),
            runtime_contexts: Vec::new(),
        };
        assert!(
            observe_context_compaction(
                "wire-2",
                CompactionKind::WireAssembly,
                &messages,
                &no_boundary,
                &[],
                &[],
                None
            )
            .is_none()
        );

        let unchanged = CompactResult {
            messages: messages.clone(),
            boundary: Some(crate::turn::cloud::compaction::CompactBoundary::new(
                CompactTrigger::Auto,
                CompactionTier::CompactHistory,
            )),
            ..no_boundary
        };
        assert!(
            observe_context_compaction(
                "wire-3",
                CompactionKind::WireContextRetry,
                &messages,
                &unchanged,
                &[],
                &[],
                None
            )
            .is_none()
        );
    }

    #[test]
    fn budget_overrides_default_is_all_none() {
        // Default means "use the context's model-derived budget knobs" — the
        // main path relies on this; a non-None default would silently change
        // main-path behaviour.
        let o = BudgetOverrides::default();
        assert!(o.budget_chars.is_none());
        assert!(o.keep_chars.is_none());
        assert!(o.keep_recent_turns.is_none());
        assert!(o.current_tokens.is_none());
        assert!(o.tier.is_none());
    }

    #[test]
    fn memoria_context_budget_uses_configured_context_window() {
        let ctx = MemoriaContext {
            session_id: "sid-1m",
            model_name: "deepseek-v4-pro-official",
            context_window: Some(1_000_000),
            memoria_client: None,
            summary_client: None,
            tier: CompactionTier::Normal,
            session_facts: None,
        };

        assert_eq!(ctx.context_budget().model_limit, 1_000_000);
    }

    #[test]
    fn history_budget_reserves_system_and_tool_tokens_once() {
        let budget = crate::prompts::budget_for_model_with_override(Some("model"), Some(10_000));
        let ascii_history = vec![json!({"role": "user", "content": "a".repeat(39_970)})];
        let policy = budget.window_policy();
        assert_eq!(policy.reserved_output_tokens, 2_500);
        assert_eq!(policy.reserved_summary_tokens, 1_800);
        assert_eq!(policy.reserved_protocol_tokens, 300);
        assert_eq!(budget.effective_input_limit(), 5_400);
        let available = history_budget_chars(&budget, 1_500, &ascii_history, 10_000);
        assert!(
            (15_500..=15_600).contains(&available),
            "fixed context must be subtracted once after resolving all policy reserves: {available}"
        );
        assert_eq!(
            history_budget_chars(&budget, 20_000, &ascii_history, 10_000),
            0,
            "fixed context larger than the input window must not underflow"
        );
    }

    #[test]
    fn history_budget_uses_observed_token_density_without_language_special_cases() {
        let budget = crate::prompts::budget_for_model_with_override(Some("model"), Some(10_000));
        let dense_history = vec![json!({"role": "user", "content": "界".repeat(9_970)})];

        let available = history_budget_chars(&budget, 1_500, &dense_history, 15_000);
        assert!(
            (2_550..=2_650).contains(&available),
            "character budgets must reflect the estimator's observed density: {available}"
        );
    }

    #[test]
    fn history_budget_measurement_reuses_exact_nested_unicode_serialization() {
        let history = vec![
            json!({
                "role": "user",
                "content": {
                    "text": "你好🚀",
                    "parts": ["alpha", {"nested": true}]
                }
            }),
            json!({"role": "assistant", "content": ["résumé", null, 42]}),
        ];
        let encoded = history
            .iter()
            .map(|message| serde_json::to_string(message).expect("serialize history value"))
            .collect::<Vec<_>>();
        let measurement = serialized_history_measurement(&history);

        assert_eq!(
            measurement.bytes,
            encoded
                .iter()
                .map(|message| message.len() as u64)
                .sum::<u64>()
        );
        assert_eq!(
            measurement.chars,
            encoded
                .iter()
                .map(|message| message.chars().count())
                .sum::<usize>()
        );
        assert!(
            measurement.bytes > measurement.chars as u64,
            "UTF-8 byte accounting must not collapse to character count"
        );
    }

    #[test]
    fn final_wire_budget_status_is_observational_and_counts_block_content() {
        let messages = vec![json!({
            "role": "user",
            "content": [{"type": "text", "text": "你好世界".repeat(400)}]
        })];
        let mut trace = json!({"wire": {"message_count": 1}});
        let status = augment_manifest_trace_with_wire_budget_and_metadata(
            &mut trace,
            &messages,
            &[],
            "model",
            Some(1_000),
            None,
            100,
        );

        assert!(status.soft_target_exceeded());
        assert!(status.hard_limit_exceeded());
        assert_eq!(status.reserved_protocol_tokens, 300);
        assert_eq!(trace["wire"]["message_count"], 1);
        assert_eq!(
            trace["wire"]["budget"]["enforcement"],
            "observational_estimate_provider_authoritative"
        );
    }

    #[tokio::test]
    async fn long_running_compaction_converges_without_protocol_decay() {
        let mut history = vec![json!({
            "role": "user",
            "content": "Inspect, repair, and verify the project without losing the active goal."
        })];
        for round in 0..200 {
            history.push(json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": format!("call-{round}"),
                    "type": "function",
                    "function": {
                        "name": "read_file",
                        "arguments": format!("{{\"path\":\"src/file-{round}.rs\"}}")
                    }
                }]
            }));
            history.push(json!({
                "role": "tool",
                "tool_call_id": format!("call-{round}"),
                "content": format!("round {round}: {}", "realistic tool evidence ".repeat(120))
            }));
        }
        let system = vec![json!({
            "role": "system",
            "content": "Stable execution policy. Preserve the active goal and tool causality."
        })];
        let tools = vec![json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "parameters": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }
            }
        })];
        let ctx = MemoriaContext {
            session_id: "sid-long-running",
            model_name: "model-with-explicit-window",
            context_window: Some(8_000),
            memoria_client: None,
            summary_client: None,
            tier: CompactionTier::AggressivePrune,
            session_facts: None,
        };

        let first = ctx.compact(&history, &system, &tools).await;
        let second = ctx.compact(&first.messages, &system, &tools).await;
        assert_eq!(
            second.messages, first.messages,
            "reapplying compaction to an already-bounded working set must converge"
        );
        assert!(
            first.messages.len() < history.len() / 4,
            "a long execution must keep a bounded live working set"
        );
        assert!(first.messages.iter().any(|message| {
            message.get("content").and_then(Value::as_str)
                == Some("Inspect, repair, and verify the project without losing the active goal.")
        }));
        assert!(first.messages.iter().any(|message| {
            message.get("tool_call_id").and_then(Value::as_str) == Some("call-199")
        }));

        let retained_calls: std::collections::HashSet<&str> = first
            .messages
            .iter()
            .filter_map(|message| message.get("tool_calls").and_then(Value::as_array))
            .flatten()
            .filter_map(|call| call.get("id").and_then(Value::as_str))
            .collect();
        for result in first
            .messages
            .iter()
            .filter(|message| message.get("role").and_then(Value::as_str) == Some("tool"))
        {
            let call_id = result
                .get("tool_call_id")
                .and_then(Value::as_str)
                .expect("tool result id");
            assert!(
                retained_calls.contains(call_id),
                "long-running compaction must never retain orphan result {call_id}"
            );
        }

        let tool_tokens = tools
            .iter()
            .map(crate::prompts::estimate_json_value_tokens)
            .sum();
        let estimate = crate::prompts::estimate_tokens_cache_aware_split(
            &system,
            &first.messages,
            tool_tokens,
        );
        assert!(
            estimate.total_tokens <= ctx.context_budget().effective_input_limit(),
            "bounded wire estimate {} exceeds effective input limit {}",
            estimate.total_tokens,
            ctx.context_budget().effective_input_limit()
        );
    }

    #[tokio::test]
    async fn multilingual_long_running_compaction_respects_the_token_window() {
        let mut history = vec![json!({
            "role": "user",
            "content": "持续检查并修复项目，同时保留当前目标与工具因果关系。"
        })];
        for round in 0..80 {
            history.push(json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": format!("call-cjk-{round}"),
                    "type": "function",
                    "function": {
                        "name": "read_file",
                        "arguments": format!("{{\"path\":\"src/文件-{round}.rs\"}}")
                    }
                }]
            }));
            history.push(json!({
                "role": "tool",
                "tool_call_id": format!("call-cjk-{round}"),
                "content": format!("第 {round} 轮：{}", "真实工具证据".repeat(900))
            }));
        }
        let system = vec![json!({
            "role": "system",
            "content": "稳定执行策略。保留活跃目标与工具因果关系。"
        })];
        let tools = vec![json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "parameters": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }
            }
        })];
        let ctx = MemoriaContext {
            session_id: "sid-long-running-cjk",
            model_name: "model-with-explicit-window",
            context_window: Some(8_000),
            memoria_client: None,
            summary_client: None,
            tier: CompactionTier::AggressivePrune,
            session_facts: None,
        };

        let compacted = ctx.compact(&history, &system, &tools).await;
        let tool_tokens = tools
            .iter()
            .map(crate::prompts::estimate_json_value_tokens)
            .sum();
        let estimate = crate::prompts::estimate_tokens_cache_aware_split(
            &system,
            &compacted.messages,
            tool_tokens,
        );

        assert!(
            estimate.total_tokens <= ctx.context_budget().effective_input_limit(),
            "multilingual bounded wire estimate {} exceeds effective input limit {}",
            estimate.total_tokens,
            ctx.context_budget().effective_input_limit()
        );
    }

    #[test]
    fn budget_overrides_merge_respects_caller_values() {
        // The merge helper is the contract between context defaults and
        // emergency-retry overrides. Each `Some(_)` must win; each `None`
        // must fall through.
        let base = ResolvedBudget {
            budget_chars: 4000,
            keep_chars: 2_000,
            keep_recent_turns: 8,
            current_tokens: 1_234,
            tier: CompactionTier::CompactHistory,
        };
        let overrides = BudgetOverrides {
            budget_chars: Some(3000),
            keep_chars: None,
            keep_recent_turns: Some(4),
            current_tokens: Some(8_888),
            tier: Some(CompactionTier::AggressivePrune),
        };
        let merged = overrides.apply(base);
        assert_eq!(merged.budget_chars, 3000);
        assert_eq!(merged.keep_chars, 2_000, "unset fields fall through");
        assert_eq!(merged.keep_recent_turns, 4);
        assert_eq!(merged.current_tokens, 8_888);
        assert_eq!(merged.tier, CompactionTier::AggressivePrune);
    }

    #[test]
    fn session_memory_evidence_cannot_masquerade_as_a_new_turn() {
        let entry = session_memory_entry_for_pipeline(Some("continue the current task"), Some(7))
            .expect("session memory entry");

        assert_eq!(entry.source.as_deref(), Some("session_memory.snapshot"));
        assert!(
            entry
                .content
                .contains("system-supplied background evidence")
        );
        assert!(entry.content.contains("not a new user message"));
        assert!(
            entry
                .content
                .contains("not a new user message, instruction, turn boundary")
        );
        assert!(
            entry
                .content
                .contains("do not announce a resume or restart planning")
        );
        assert!(entry.content.contains("continue the current task"));
    }

    #[test]
    fn compaction_memory_rerun_skips_identical_context() {
        let current = session_memory_entry_for_pipeline(Some("same memory"), Some(7))
            .expect("current session memory entry");
        let rerun = rerun_with_compaction_memory_for_user_turn(
            Some("same memory"),
            Some(&current),
            Some(7),
            &[],
            &[],
            |_, _| panic!("identical content should not rerun"),
        );
        assert!(rerun.is_none());
    }

    #[test]
    fn compaction_memory_rerun_keeps_changed_session_snapshot() {
        let current = session_memory_entry_for_pipeline(Some("old memory"), Some(7))
            .expect("current session memory entry");
        let rerun = rerun_with_compaction_memory_for_user_turn(
            Some("new memory"),
            Some(&current),
            Some(7),
            &[],
            &[],
            |entry, _| entry,
        )
        .expect("changed session memory should rerun");
        assert_eq!(
            rerun,
            Some(
                session_memory_entry_for_pipeline(Some("new memory"), Some(7))
                    .expect("rerun entry")
            )
        );
    }

    #[test]
    fn compaction_memory_rerun_merges_without_replacing_prefetched_identity() {
        let existing = astra_turn_core::context_sources::MemoryEntry::scored("old", 0.4)
            .with_memory_identity("mem-1", "working");
        let replacement = astra_turn_core::context_sources::MemoryEntry::scored("new", 0.9)
            .with_memory_identity("mem-1", "working");
        let additional = astra_turn_core::context_sources::MemoryEntry::scored("next", 0.8)
            .with_memory_identity("mem-2", "working");

        let rerun = rerun_with_compaction_memory_for_user_turn(
            None,
            None,
            None,
            std::slice::from_ref(&existing),
            &[replacement, additional.clone()],
            |session, memories| (session, memories.to_vec()),
        )
        .expect("retrieved working memories should rerun the pipeline");

        assert!(rerun.0.is_none());
        assert_eq!(rerun.1, vec![existing, additional]);
    }

    #[test]
    fn session_memory_entry_for_user_turn_keeps_memory_for_normal_turn() {
        let entry =
            session_memory_entry_for_user_turn(Some("## Session State\nKeep going"), Some(8))
                .expect("session memory entry");

        assert!(entry.content.contains("updated through session turn 8"));
        assert!(entry.content.ends_with("## Session State\nKeep going"));
        assert_eq!(entry.freshness_turn, Some(8));
        assert_eq!(entry.source.as_deref(), Some("session_memory.snapshot"));
    }

    #[test]
    fn session_memory_unknown_freshness_is_explicit_instead_of_claiming_current_turn() {
        let entry = session_memory_entry_for_user_turn(Some("prior session memory"), None)
            .expect("session memory remains available as evidence");
        assert!(entry.content.contains("update turn unavailable"));
        assert_eq!(entry.freshness_turn, None);
    }

    #[test]
    fn assemble_empty_attachments_matches_simple_concat() {
        let system = vec![json!({"role": "system", "content": "sys"})];
        let compacted = vec![json!({"role": "user", "content": "hi"})];
        let msgs = assemble_llm_messages_with_cache_capability(
            system.clone(),
            Vec::new(),
            Vec::new(),
            compacted.clone(),
            &PostCompactAttachments::default(),
            "s1",
            "openai",
            "gpt-4",
            &astra_turn_core::thinking_config::ThinkingConfig::Off,
            None,
            &cache_cfg(),
        );
        // Expect system first, then compacted. No attachments injected.
        assert_eq!(msgs[0], system[0]);
        assert_eq!(msgs[1], compacted[0]);
        // No trailing attachment markers.
        assert_eq!(msgs.len(), 2);
    }

    #[test]
    fn assemble_injects_invoked_skills_as_runtime_system_before_current_user() {
        let system = vec![json!({"role": "system", "content": "sys"})];
        let compacted = vec![json!({"role": "user", "content": "hi"})];
        let msgs = assemble_llm_messages_with_cache_capability(
            system,
            Vec::new(),
            Vec::new(),
            compacted,
            &PostCompactAttachments {
                invoked_skills: vec![InvokedSkillRef {
                    name: "code-review",
                    content: "review instructions",
                }],
                ..Default::default()
            },
            "s1",
            "openai",
            "gpt-4",
            &astra_turn_core::thinking_config::ThinkingConfig::Off,
            None,
            &cache_cfg(),
        );
        // Skill attachment keeps runtime-system authority and the real user
        // message remains the unmodified current-turn boundary.
        let skill_msg = msgs
            .iter()
            .find(|m| {
                m.get("content")
                    .and_then(Value::as_str)
                    .is_some_and(|c| c.contains("code-review"))
            })
            .expect("skill attachment must be injected");
        let skill_pos = msgs.iter().position(|m| m == skill_msg).unwrap();
        let user_pos = msgs
            .iter()
            .position(|m| m.get("content").and_then(Value::as_str) == Some("hi"))
            .unwrap();
        assert_eq!(skill_msg["role"], "system");
        assert!(is_runtime_system_context(skill_msg));
        assert!(is_required_runtime_preamble(skill_msg));
        assert!(skill_pos < user_pos);
        let user_messages = msgs
            .iter()
            .filter(|message| message.get("role").and_then(Value::as_str) == Some("user"))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            user_messages,
            vec![json!({"role": "user", "content": "hi"})]
        );
    }

    #[test]
    fn compaction_note_appends_when_boundary_set_and_last_is_assistant() {
        let mut msgs = vec![
            json!({"role": "user", "content": "original goal"}),
            json!({"role": "assistant", "content": "partial progress"}),
        ];
        maybe_append_continuation_prompt(&mut msgs, true);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[2]["role"], "system");
        assert!(is_runtime_system_context(&msgs[2]));
        assert!(is_required_runtime_preamble(&msgs[2]));
        let note = msgs[2]["content"].as_str().unwrap();
        assert!(note.contains("Context was compacted"));
        assert!(note.contains("not a new user request"));
        assert!(!note.contains("keep going"));

        maybe_append_continuation_prompt(&mut msgs, true);
        assert_eq!(msgs.len(), 3, "runtime compaction note must be idempotent");
    }

    #[test]
    fn continuation_prompt_noop_when_no_boundary() {
        let before = vec![
            json!({"role": "user", "content": "goal"}),
            json!({"role": "assistant", "content": "response"}),
        ];
        let mut msgs = before.clone();
        maybe_append_continuation_prompt(&mut msgs, false);
        assert_eq!(msgs, before, "no boundary → must not modify messages");
    }

    #[test]
    fn continuation_prompt_noop_when_last_is_user() {
        let before = vec![
            json!({"role": "assistant", "content": "answer"}),
            json!({"role": "user", "content": "follow-up"}),
        ];
        let mut msgs = before.clone();
        maybe_append_continuation_prompt(&mut msgs, true);
        assert_eq!(
            msgs, before,
            "last message already user → no continuation needed"
        );
    }

    #[test]
    fn continuation_prompt_does_not_classify_assistant_completion_prose() {
        let mut msgs = vec![
            json!({"role": "user", "content": "goal"}),
            json!({
                "role": "assistant",
                "content": "All done. Task complete successfully."
            }),
        ];
        maybe_append_continuation_prompt(&mut msgs, true);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[2]["role"], "system");
    }

    #[test]
    fn continuation_prompt_control_is_language_neutral_and_stable() {
        let mut msgs = vec![
            json!({"role": "user", "content": "请帮我重构这段代码 重构这段代码 重构这段代码 重构这段代码 重构这段代码 请帮我重构这段代码"}),
            json!({"role": "assistant", "content": "好的,我开始处理"}),
        ];
        maybe_append_continuation_prompt(&mut msgs, true);
        assert_eq!(msgs.len(), 3);
        let note = msgs[2]["content"].as_str().unwrap();
        assert!(
            note.contains("Context was compacted") && note.contains("not a new user request"),
            "runtime controls must not branch on a guessed user language: {note}"
        );
        assert!(!note.contains("keep going"));
    }

    // ─────────────────────────────────────────────────────────────
    // Cross-caller parity pins
    //
    // Both `ServerAgenticLoopHost::execute_turn` and
    // `InProcessChatTurnBridge::forward` call `assemble_llm_messages_with_cache_capability`.
    // These tests pin the convergence invariants the two callers rely on:
    // any drift here means one caller's wire output no longer matches the
    // other's for the same logical input.
    // ─────────────────────────────────────────────────────────────

    #[test]
    fn parity_bridge_empty_attachments_matches_server_empty_attachments() {
        // The bridge path always supplies an empty `PostCompactAttachments`
        // (no state-backed skill/file re-injection). The server path supplies
        // an empty one too whenever `state.skills.invoked` + `recent_file_reads`
        // are both empty. In that shared case, the output must be IDENTICAL.
        let system = vec![json!({"role": "system", "content": "sys"})];
        let compacted = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": "hello"}),
        ];
        let bridge_msgs = assemble_llm_messages_with_cache_capability(
            system.clone(),
            Vec::new(),
            Vec::new(),
            compacted.clone(),
            &PostCompactAttachments::default(),
            "sid",
            "openai",
            "gpt-4",
            &astra_turn_core::thinking_config::ThinkingConfig::Off,
            None,
            &cache_cfg(),
        );
        let server_msgs = assemble_llm_messages_with_cache_capability(
            system,
            Vec::new(),
            Vec::new(),
            compacted,
            &PostCompactAttachments {
                invoked_skills: Vec::new(),
                recent_file_reads: &[],
                cwd: Some("/tmp"),
            },
            "sid",
            "openai",
            "gpt-4",
            &astra_turn_core::thinking_config::ThinkingConfig::Off,
            None,
            &cache_cfg(),
        );
        assert_eq!(
            bridge_msgs, server_msgs,
            "bridge (default attachments) and server (empty-but-populated attachments) \
             must produce byte-identical output — otherwise caller drift is possible"
        );
    }

    #[test]
    fn parity_continuation_then_assemble_is_deterministic() {
        // The server + bridge call sequence is:
        //   1. memoria.compact() → CompactResult
        //   2. maybe_append_continuation_prompt(&mut result.messages, hit)
        //   3. assemble_llm_messages_with_cache_capability(system, preamble, result.messages, ...)
        //
        // Running the same sequence twice on equal inputs must produce
        // byte-identical outputs — no hidden state, no call-count side effects.
        let system = vec![json!({"role": "system", "content": "sys"})];
        let make_compacted = || {
            vec![
                json!({"role": "user", "content": "original goal"}),
                json!({"role": "assistant", "content": "partial progress"}),
            ]
        };

        let mut first = make_compacted();
        maybe_append_continuation_prompt(&mut first, true);
        let first_out = assemble_llm_messages_with_cache_capability(
            system.clone(),
            Vec::new(),
            Vec::new(),
            first,
            &PostCompactAttachments::default(),
            "sid",
            "openai",
            "gpt-4",
            &astra_turn_core::thinking_config::ThinkingConfig::Off,
            None,
            &cache_cfg(),
        );

        let mut second = make_compacted();
        maybe_append_continuation_prompt(&mut second, true);
        let second_out = assemble_llm_messages_with_cache_capability(
            system,
            Vec::new(),
            Vec::new(),
            second,
            &PostCompactAttachments::default(),
            "sid",
            "openai",
            "gpt-4",
            &astra_turn_core::thinking_config::ThinkingConfig::Off,
            None,
            &cache_cfg(),
        );

        assert_eq!(
            first_out, second_out,
            "compact → continuation → assemble must be deterministic; \
             if this flips, shared assembly has gained hidden state"
        );
    }

    #[test]
    fn parity_server_attachments_preserve_conversation_messages() {
        // Invariant: server-path attachments (invoked_skills, recent_file_reads)
        // use the runtime-system lane and never mutate or masquerade as
        // canonical conversation messages.
        let system = vec![json!({"role": "system", "content": "sys"})];
        let compacted = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": "there"}),
        ];
        let bridge_out = assemble_llm_messages_with_cache_capability(
            system.clone(),
            Vec::new(),
            Vec::new(),
            compacted.clone(),
            &PostCompactAttachments::default(),
            "sid",
            "openai",
            "gpt-4",
            &astra_turn_core::thinking_config::ThinkingConfig::Off,
            None,
            &cache_cfg(),
        );
        let server_out = assemble_llm_messages_with_cache_capability(
            system,
            Vec::new(),
            Vec::new(),
            compacted,
            &PostCompactAttachments {
                invoked_skills: vec![InvokedSkillRef {
                    name: "code-review",
                    content: "review checklist",
                }],
                recent_file_reads: &[],
                cwd: None,
            },
            "sid",
            "openai",
            "gpt-4",
            &astra_turn_core::thinking_config::ThinkingConfig::Off,
            None,
            &cache_cfg(),
        );
        assert!(
            server_out.len() > bridge_out.len(),
            "server with attachments must have strictly more messages"
        );
        let bridge_conversation = bridge_out
            .iter()
            .filter(|message| message.get("role").and_then(Value::as_str) != Some("system"))
            .collect::<Vec<_>>();
        let server_conversation = server_out
            .iter()
            .filter(|message| message.get("role").and_then(Value::as_str) != Some("system"))
            .collect::<Vec<_>>();
        assert_eq!(bridge_conversation, server_conversation);
        assert!(server_out.iter().any(|message| {
            is_runtime_system_context(message)
                && message
                    .get("content")
                    .and_then(Value::as_str)
                    .is_some_and(|content| content.contains("code-review"))
        }));
    }

    #[test]
    fn parity_cache_annotations_are_terminal_step() {
        // `apply_anthropic_cache_metadata` runs after runtime-system placement.
        // Both callers rely on it annotating only the stable prefix before
        // the marker-isolated runtime context.
        //
        // This test pins that ordering by comparing marker placement.
        let system = vec![json!({"role": "system", "content": "sys"})];
        let compacted = vec![json!({"role": "user", "content": "hi"})];

        let bridge_out = assemble_llm_messages_with_cache_capability(
            system.clone(),
            Vec::new(),
            Vec::new(),
            compacted.clone(),
            &PostCompactAttachments::default(),
            "sid",
            "anthropic", // anthropic triggers cache_control annotation
            "claude-sonnet-4",
            &astra_turn_core::thinking_config::ThinkingConfig::Off,
            None,
            &PromptCacheConfig::latch("anthropic", "claude-sonnet-4"),
        );
        let server_out = assemble_llm_messages_with_cache_capability(
            system,
            Vec::new(),
            Vec::new(),
            compacted,
            &PostCompactAttachments {
                invoked_skills: vec![InvokedSkillRef {
                    name: "code-review",
                    content: "checklist",
                }],
                recent_file_reads: &[],
                cwd: None,
            },
            "sid",
            "anthropic",
            "claude-sonnet-4",
            &astra_turn_core::thinking_config::ThinkingConfig::Off,
            None,
            &PromptCacheConfig::latch("anthropic", "claude-sonnet-4"),
        );

        // Both paths must emit well-formed message arrays; the last message
        // differs (it's the user message for bridge, the skill attachment
        // for server) but each of them individually must be a valid message
        // with a `role` field, i.e. the cache-annotation step didn't corrupt
        // structure.
        assert!(bridge_out.last().unwrap().get("role").is_some());
        assert!(server_out.last().unwrap().get("role").is_some());
    }

    #[test]
    fn prefix_only_providers_skip_anthropic_cache_annotations() {
        let msgs = assemble_llm_messages_with_cache_capability(
            vec![json!({"role": "system", "content": "sys"})],
            Vec::new(),
            Vec::new(),
            vec![json!({"role": "user", "content": "hi"})],
            &PostCompactAttachments::default(),
            "sid",
            "openai",
            "gpt-4o",
            &astra_turn_core::thinking_config::ThinkingConfig::Off,
            None,
            &PromptCacheConfig::latch("openai", "gpt-4o"),
        );

        assert!(
            msgs.iter()
                .all(|message| message.get("cache_control").is_none()),
            "prefix-only providers must never receive anthropic cache_control markers"
        );
    }

    /// Regression lock: runtime context must keep system authority while the
    /// complete conversation history remains byte-for-byte unchanged.
    #[test]
    fn prefix_provider_places_runtime_context_at_current_turn_boundary() {
        let stable_sys = vec![json!({"role": "system", "content": "stable core rules only"})];
        let volatile_preamble = vec![json!({"role": "system", "content": "Turn: 5"})];
        let history = vec![
            json!({"role": "user", "content": "first question"}),
            json!({"role": "assistant", "content": "first answer"}),
            json!({"role": "user", "content": "second question"}),
        ];

        let msgs = assemble_llm_messages_with_cache_capability(
            stable_sys,
            volatile_preamble,
            Vec::new(),
            history,
            &PostCompactAttachments::default(),
            "sid",
            "openai",
            "qwen3.5-plus",
            &astra_turn_core::thinking_config::ThinkingConfig::Off,
            None,
            &cache_cfg(),
        );

        assert_eq!(msgs.len(), 5);
        assert_eq!(msgs[0]["content"], "stable core rules only");
        assert_eq!(
            msgs[1],
            json!({"role": "user", "content": "first question"})
        );
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["content"], "first answer");
        assert_eq!(msgs[3]["role"], "system");
        assert!(message_text(&msgs[3]).contains("Turn: 5"));
        assert_eq!(
            msgs[4],
            json!({"role": "user", "content": "second question"})
        );
    }

    #[test]
    fn volatile_preamble_becomes_runtime_system_context() {
        let system = vec![json!({
            "role": "system",
            "content": [{
                "type": "text",
                "text": "sys",
                "cache_control": astra_turn_core::context_serializer::anthropic_ephemeral_cache_control(),
            }],
        })];
        let preamble = vec![json!({"role": "system", "content": "volatile"})];
        let compacted = vec![json!({"role": "user", "content": "hi"})];
        let msgs = assemble_llm_messages_with_cache_capability(
            system,
            preamble,
            Vec::new(),
            compacted,
            &PostCompactAttachments::default(),
            "sid",
            "openai",
            "gpt-4",
            &astra_turn_core::thinking_config::ThinkingConfig::Off,
            None,
            &cache_cfg(),
        );
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "system");
        assert!(message_text(&msgs[1]).contains("volatile"));
        assert_eq!(msgs[2], json!({"role": "user", "content": "hi"}));
    }

    #[test]
    fn runtime_system_context_preserves_block_array_content() {
        let block_content = json!([
            {"type": "text", "text": "runtime evidence"},
            {"type": "document", "source": {"type": "base64", "data": "opaque"}}
        ]);
        let mut runtime = json!({
            "role": "user",
            "content": block_content.clone()
        });
        runtime[REQUIRED_RUNTIME_PREAMBLE_MARKER] = Value::Bool(true);
        let preamble = vec![runtime];
        let msgs = assemble_llm_messages_with_cache_capability(
            vec![json!({"role": "system", "content": "sys"})],
            preamble,
            Vec::new(),
            vec![json!({"role": "user", "content": "hi"})],
            &PostCompactAttachments::default(),
            "sid",
            "openai",
            "gpt-4",
            &astra_turn_core::thinking_config::ThinkingConfig::Off,
            None,
            &cache_cfg(),
        );

        assert_eq!(msgs[1]["role"], "system");
        assert_eq!(msgs[1]["content"], block_content);
        assert!(is_required_runtime_preamble(&msgs[1]));
        assert_eq!(msgs[2], json!({"role": "user", "content": "hi"}));
    }

    #[test]
    fn required_runtime_context_keeps_system_authority() {
        let system = vec![json!({"role": "system", "content": "sys"})];
        let required =
            required_runtime_preamble_message("required resume context").expect("required message");
        let compacted = vec![json!({"role": "user", "content": "hi"})];

        let msgs = assemble_llm_messages_with_cache_capability(
            system,
            vec![required],
            Vec::new(),
            compacted,
            &PostCompactAttachments::default(),
            "sid",
            "openai",
            "gpt-4",
            &astra_turn_core::thinking_config::ThinkingConfig::Off,
            None,
            &cache_cfg(),
        );

        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1]["role"], "system");
        assert_eq!(message_text(&msgs[1]), "required resume context");
        assert_eq!(msgs[2], json!({"role": "user", "content": "hi"}));
    }

    #[test]
    fn self_status_telemetry_does_not_enter_prompt() {
        let system = vec![json!({"role": "system", "content": "sys"})];
        let drained = vec![crate::turn::agentic_loop::host::VolatileInjection {
            kind: crate::turn::agentic_loop::host::VolatileKind::SelfStatus,
            payload: json!("## ⚡ Self-Status\nTurn 9/299 | Cache: 86%"),
            round_index: 9,
        }];
        let compacted = vec![json!({"role": "user", "content": "相关的测试够硬核吗？"})];
        let msgs = assemble_llm_messages_with_cache_capability(
            system,
            Vec::new(),
            drained,
            compacted,
            &PostCompactAttachments::default(),
            "sid",
            "openai",
            "gpt-4",
            &astra_turn_core::thinking_config::ThinkingConfig::Off,
            None,
            &cache_cfg(),
        );

        assert_eq!(msgs.len(), 2);
        let user_text = message_text(&msgs[1]);
        assert!(user_text.contains("相关的测试够硬核吗"));
        assert!(!user_text.contains("Self-Status"));
    }

    #[test]
    fn policy_advisory_volatile_uses_runtime_system_context() {
        let system = vec![json!({"role": "system", "content": "sys"})];
        let drained = vec![crate::turn::agentic_loop::host::VolatileInjection {
            kind: crate::turn::agentic_loop::host::VolatileKind::PolicyAdvisory,
            payload: json!({
                "schema": "policy_advisory.v1",
                "advisories": [{
                    "kind": "stall",
                    "severity": "warning",
                    "recommendation": "consider changing approach"
                }]
            }),
            round_index: 2,
        }];
        let compacted = vec![json!({"role": "user", "content": "fix the failing tests"})];
        let msgs = assemble_llm_messages_with_cache_capability(
            system,
            Vec::new(),
            drained,
            compacted,
            &PostCompactAttachments::default(),
            "sid",
            "openai",
            "gpt-4",
            &astra_turn_core::thinking_config::ThinkingConfig::Off,
            None,
            &cache_cfg(),
        );

        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1]["role"], "system");
        let runtime_text = message_text(&msgs[1]);
        assert!(runtime_text.contains("policy_advisory.v1"));
        assert!(runtime_text.contains("consider changing approach"));
        assert!(runtime_text.contains("<runtime-advisory-evidence>"));
        assert!(runtime_text.contains("\"kind\":\"policy_advisory\""));
        assert!(
            !runtime_text.contains("Do NOT call"),
            "soft policy advisory must not become a hard tool prohibition: {runtime_text}"
        );
        assert_eq!(
            msgs[2],
            json!({"role": "user", "content": "fix the failing tests"})
        );
    }

    #[test]
    fn active_turn_frame_anchors_latest_user_goal_as_runtime_system_context() {
        let system = vec![json!({"role": "system", "content": "sys"})];
        let drained = vec![crate::turn::agentic_loop::host::VolatileInjection {
            kind: crate::turn::agentic_loop::host::VolatileKind::ActiveTurnFrame,
            payload: json!({
                "latest_user_message": "相关的测试够硬核吗？",
                "active_goal": "相关的测试够硬核吗？"
            }),
            round_index: 3,
        }];
        let compacted = vec![
            json!({"role": "user", "content": "一共多少 changes？"}),
            json!({"role": "assistant", "content": "148 files"}),
            json!({"role": "user", "content": "相关的测试够硬核吗？"}),
        ];
        let msgs = assemble_llm_messages_with_cache_capability(
            system,
            Vec::new(),
            drained,
            compacted,
            &PostCompactAttachments::default(),
            "sid",
            "openai",
            "gpt-4",
            &astra_turn_core::thinking_config::ThinkingConfig::Off,
            None,
            &cache_cfg(),
        );

        assert_eq!(msgs[3]["role"], "system");
        let runtime_text = message_text(&msgs[3]);
        assert!(runtime_text.contains("<runtime-required-context>"));
        assert!(runtime_text.contains("\"kind\":\"active_turn_frame\""));
        assert!(
            runtime_text.contains("active_goal"),
            "active goal frame must stay explicit in runtime system context"
        );
        assert_eq!(msgs[1]["content"], "一共多少 changes？");
        assert_eq!(msgs[2]["content"], "148 files");
        assert_eq!(
            msgs[4],
            json!({"role": "user", "content": "相关的测试够硬核吗？"})
        );
    }

    #[test]
    fn tail_suffix_runtime_precedes_current_tail_and_leaves_history_unchanged() {
        let system = vec![json!({"role": "system", "content": "sys"})];
        let preamble = vec![json!({"role": "system", "content": "volatile"})];
        let compacted = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": ""}),
            json!({"role": "tool", "content": "tool output", "tool_call_id": "c1"}),
        ];
        let msgs = assemble_llm_messages_with_cache_capability(
            system,
            preamble,
            Vec::new(),
            compacted,
            &PostCompactAttachments::default(),
            "sid",
            "openai",
            "gpt-4",
            &astra_turn_core::thinking_config::ThinkingConfig::Off,
            None,
            &cache_cfg(),
        );
        assert_eq!(msgs[3]["role"], "system");
        assert!(message_text(&msgs[3]).contains("volatile"));
        assert_eq!(
            msgs[1]["content"], "hi",
            "historical user message must stay unchanged"
        );
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[4]["role"], "tool");
        assert_eq!(message_text(&msgs[4]), "tool output");
        assert_eq!(msgs.len(), 5);
    }

    #[test]
    fn retry_stripping_preserves_non_text_tool_content_blocks() {
        let mut message = json!({
            "role": "tool",
            "tool_call_id": "call-1",
            "content": [
                {"type": "document", "source": {"type": "base64", "data": "opaque"}},
                {
                    "type": "text",
                    "text": "tool evidence\n\n<runtime-context-after-tool>\nvolatile\n</runtime-context-after-tool>"
                }
            ]
        });

        strip_runtime_context_from_tool_message(&mut message);

        let blocks = message["content"].as_array().expect("content blocks");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "document");
        assert_eq!(blocks[1]["text"], "tool evidence");
    }

    #[test]
    fn volatile_preamble_does_not_invent_user_when_history_ends_in_assistant() {
        let system = vec![json!({"role": "system", "content": "sys"})];
        let preamble = vec![json!({"role": "system", "content": "volatile"})];
        let compacted = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": "tail assistant"}),
        ];
        let msgs = assemble_llm_messages_with_cache_capability(
            system,
            preamble,
            Vec::new(),
            compacted,
            &PostCompactAttachments::default(),
            "sid",
            "openai",
            "gpt-4",
            &astra_turn_core::thinking_config::ThinkingConfig::Off,
            None,
            &cache_cfg(),
        );

        assert_eq!(msgs[1]["role"], "system");
        assert!(message_text(&msgs[1]).contains("volatile"));
        assert_eq!(msgs[2], json!({"role": "user", "content": "hi"}));
        assert_eq!(
            msgs[3],
            json!({"role": "assistant", "content": "tail assistant"})
        );
    }

    #[test]
    fn anthropic_marks_conversation_before_runtime_system_context() {
        let system = vec![json!({"role": "system", "content": "sys"})];
        let preamble = vec![json!({"role": "system", "content": "volatile"})];
        let compacted = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": ""}),
            json!({"role": "tool", "content": "tool output", "tool_call_id": "c1"}),
        ];
        let msgs = assemble_llm_messages_with_cache_capability(
            system,
            preamble,
            Vec::new(),
            compacted,
            &PostCompactAttachments::default(),
            "sid",
            "anthropic",
            "claude-sonnet-4",
            &astra_turn_core::thinking_config::ThinkingConfig::Off,
            None,
            &anthropic_cache_cfg(),
        );

        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[4]["role"], "system");
        assert!(
            astra_turn_core::context_serializer::message_has_cache_control(&msgs[3]),
            "cache placement must end on the last conversation message before runtime context",
        );
        assert!(
            !astra_turn_core::context_serializer::message_has_cache_control(&msgs[4]),
            "runtime system context must remain after the stable cache boundary",
        );
        assert!(message_text(&msgs[4]).contains("volatile"));
        assert!(msgs[3]["content"].to_string().contains("tool output"));
    }

    #[test]
    fn anthropic_runtime_system_stays_outside_cache_when_attachments_follow() {
        let msgs = assemble_llm_messages_with_cache_capability(
            vec![json!({"role": "system", "content": "sys"})],
            vec![json!({"role": "user", "content": "<runtime>round-specific</runtime>"})],
            Vec::new(),
            vec![
                json!({"role": "user", "content": "inspect"}),
                json!({"role": "assistant", "content": ""}),
                json!({"role": "tool", "content": "evidence", "tool_call_id": "c1"}),
            ],
            &PostCompactAttachments {
                invoked_skills: vec![InvokedSkillRef {
                    name: "review",
                    content: "stable skill instructions",
                }],
                ..Default::default()
            },
            "sid",
            "anthropic",
            "claude-sonnet-4",
            &astra_turn_core::thinking_config::ThinkingConfig::Off,
            None,
            &anthropic_cache_cfg(),
        );

        assert!(
            astra_turn_core::context_serializer::message_has_cache_control(&msgs[3]),
            "the breakpoint must end on stable conversation history"
        );
        assert!(
            msgs.iter().skip(4).all(|message| {
                !astra_turn_core::context_serializer::message_has_cache_control(message)
            }),
            "neither runtime system context nor later attachments may extend the cached prefix"
        );
        assert!(msgs[3]["content"].to_string().contains("evidence"));
        assert!(message_text(&msgs[4]).contains("round-specific"));
        assert!(
            message_text(msgs.last().expect("skill attachment"))
                .contains("stable skill instructions")
        );
    }

    #[test]
    fn anthropic_keeps_runtime_system_after_user_cache_marker() {
        let system = vec![json!({"role": "system", "content": "sys"})];
        let preamble = vec![json!({"role": "user", "content": "[active-turn-frame:v1]\nlatest"})];
        let compacted = vec![json!({"role": "user", "content": "latest real user"})];
        let msgs = assemble_llm_messages_with_cache_capability(
            system,
            preamble,
            Vec::new(),
            compacted,
            &PostCompactAttachments::default(),
            "sid",
            "anthropic",
            "claude-sonnet-4",
            &astra_turn_core::thinking_config::ThinkingConfig::Off,
            None,
            &anthropic_cache_cfg(),
        );

        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[2]["role"], "system");
        assert_eq!(msgs.len(), 3);
        assert!(
            astra_turn_core::context_serializer::message_has_cache_control(&msgs[1]),
            "real user message must receive the Anthropic cache marker",
        );
        assert!(!astra_turn_core::context_serializer::message_has_cache_control(&msgs[2]));
        assert!(message_text(&msgs[2]).contains("active-turn-frame"));
        assert_eq!(message_text(&msgs[1]), "latest real user");
    }

    #[test]
    fn current_user_only_models_drop_volatile_entirely() {
        let system = vec![json!({"role": "system", "content": "sys"})];
        let preamble = vec![json!({"role": "system", "content": "volatile"})];
        let drained = vec![crate::turn::agentic_loop::host::VolatileInjection {
            kind: crate::turn::agentic_loop::host::VolatileKind::PolicyAdvisory,
            payload: json!("optional policy advisory"),
            round_index: 1,
        }];
        let compacted = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": ""}),
            json!({"role": "tool", "content": "tool output", "tool_call_id": "c1"}),
        ];
        let msgs = assemble_llm_messages_with_cache_capability(
            system,
            preamble,
            drained,
            compacted,
            &PostCompactAttachments::default(),
            "sid",
            "openai",
            "deepseek-v4-flash",
            &astra_turn_core::thinking_config::ThinkingConfig::Off,
            None,
            &cache_cfg(),
        );
        assert_eq!(msgs.len(), 4, "no runtime system message should remain");
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "hi");
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[3]["role"], "tool");
        assert!(
            msgs.iter().all(|message| {
                !message
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .contains("optional policy advisory")
                    && !message
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .contains("tools executed in parallel")
                    && !message
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .contains("volatile")
            }),
            "CurrentUserOnly providers must drop all volatile wire content"
        );
    }

    #[test]
    fn current_user_only_models_keep_required_typed_runtime_as_system() {
        let system = vec![json!({"role": "system", "content": "sys"})];
        let preamble = vec![json!({"role": "system", "content": "volatile"})];
        let drained = vec![crate::turn::agentic_loop::host::VolatileInjection {
            kind: crate::turn::agentic_loop::host::VolatileKind::ActiveTurnFrame,
            payload: json!({"latest_user_goal": "latest user goal"}),
            round_index: 1,
        }];
        let compacted = vec![json!({"role": "user", "content": "hi"})];

        let msgs = assemble_llm_messages_with_cache_capability(
            system,
            preamble,
            drained,
            compacted,
            &PostCompactAttachments::default(),
            "sid",
            "openai",
            "deepseek-v4-flash",
            &astra_turn_core::thinking_config::ThinkingConfig::Off,
            None,
            &cache_cfg(),
        );

        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1]["role"], "system");
        let runtime_text = message_text(&msgs[1]);
        assert!(runtime_text.contains("<runtime-required-context>"));
        assert!(runtime_text.contains("\"kind\":\"active_turn_frame\""));
        assert!(runtime_text.contains("latest user goal"));
        assert!(!runtime_text.contains("volatile"));
        assert_eq!(msgs[2], json!({"role": "user", "content": "hi"}));
    }
}

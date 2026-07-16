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
const TOOL_RUNTIME_CONTEXT_PREFIX: &str = "<runtime-context-after-tool>";
const TOOL_RUNTIME_CONTEXT_SUFFIX: &str = "</runtime-context-after-tool>";

pub(crate) fn required_runtime_preamble_message(text: &str) -> Option<Value> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut message = serde_json::json!({
        "role": "system",
        "content": text,
    });
    message[REQUIRED_RUNTIME_PREAMBLE_MARKER] = Value::Bool(true);
    Some(message)
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
    }
}

pub(crate) fn system_reminder_wrapped_text(text: &str) -> String {
    const SYSTEM_REMINDER_PREFIX: &str = "<system-reminder>";
    const SYSTEM_REMINDER_SUFFIX: &str = "</system-reminder>";
    if text.starts_with(SYSTEM_REMINDER_PREFIX) && text.ends_with(SYSTEM_REMINDER_SUFFIX) {
        text.to_string()
    } else {
        format!("{SYSTEM_REMINDER_PREFIX}\n{text}{SYSTEM_REMINDER_SUFFIX}")
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
        let budget = self.context_budget();
        // `current_tokens` is a pressure signal for Memoria retrieval; the
        // authoritative compaction tier is `self.tier` (or the override). The
        // cache-aware estimate just tunes retrieval aggressiveness, so we
        // count tool schemas alongside messages for a single total.
        let tool_schema_tokens: usize = visible_tools
            .iter()
            .map(|t| {
                serde_json::to_string(t)
                    .map(|s| crate::prompts::estimate_str_tokens(&s))
                    .unwrap_or(50)
            })
            .sum();
        let cache_est = crate::prompts::estimate_tokens_cache_aware_split(
            system_messages,
            messages,
            tool_schema_tokens,
        );

        let resolved = overrides.apply(ResolvedBudget {
            budget_chars: budget.effective_input_limit() * 4,
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

        compact_with_memoria(
            messages,
            Some(self.session_id),
            &memoria_config,
            &memoria_params,
            self.memoria_client,
            Some(&compact_config),
            self.summary_client,
        )
        .await
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
    /// user messages with truncated content so the LLM remembers the code
    /// it was looking at before compaction.
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

const COMPACTION_CONTEXT_NOTE_EN: &str = "\
Context was compacted before this point. This runtime note is not a new user \
request and does not authorize resuming old tasks. Use the latest real user \
message plus any current tool result to decide whether to continue, answer a \
status/why question, or stop; do not run tools solely because this note exists.";

const COMPACTION_CONTEXT_NOTE_ZH: &str = "\
前文上下文已压缩。这是运行时上下文说明，不是新的用户请求，也不授权自动恢复旧任务。\
请根据最新真实用户消息和当前工具结果判断是继续、回答状态/原因问题，还是停止；\
不要仅因为这条说明运行工具。";

/// Append a neutral compaction note when compaction removed messages and the
/// last remaining assistant message doesn't already signal task completion.
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
    let last_is_user = messages
        .last()
        .and_then(|m| m.get("role").and_then(Value::as_str))
        == Some("user");
    if last_is_user {
        return;
    }
    let last_signals_done = messages
        .last()
        .and_then(|m| m.get("content").and_then(Value::as_str))
        .map(is_completion_signal)
        .unwrap_or(false);
    if last_signals_done {
        return;
    }
    // Detect CJK content in recent turns to emit a localised context note.
    let is_cjk = messages
        .iter()
        .rev()
        .take(4)
        .filter_map(|m| m.get("content").and_then(Value::as_str))
        .any(|c| {
            c.chars()
                .take(200)
                .filter(|ch| ('\u{4e00}'..='\u{9fff}').contains(ch))
                .count()
                > 10
        });
    let note = if is_cjk {
        COMPACTION_CONTEXT_NOTE_ZH
    } else {
        COMPACTION_CONTEXT_NOTE_EN
    };
    messages.push(serde_json::json!({
        "role": "user",
        "content": note,
    }));
}

fn is_completion_signal(content: &str) -> bool {
    // Look at the last ~200 chars: "completion" phrases anywhere earlier in
    // a long message don't reliably indicate the agent is done.
    let tail = if content.len() > 200 {
        &content[content.floor_char_boundary(content.len() - 200)..]
    } else {
        content
    };
    let lower = tail.to_ascii_lowercase();
    let has_completion = lower.contains("task complete")
        || lower.contains("all done")
        || lower.contains("finished")
        || lower.contains("completed successfully")
        || lower.contains("任务完成")
        || lower.contains("已完成");
    if !has_completion {
        return false;
    }
    // Negation near the completion phrase: "not yet", "haven't finished", …
    let has_negation = lower.contains("not yet")
        || lower.contains("not complete")
        || lower.contains("not finished")
        || lower.contains("haven't finished")
        || lower.contains("hasn't finished")
        || lower.contains("won't be finished")
        || lower.contains("don't think")
        || lower.contains("not sure")
        || lower.contains("没有完成")
        || lower.contains("尚未完成")
        || lower.contains("except")
        || lower.contains("but ");
    !has_negation
}

/// Stitch the final wire-ready `llm_messages` array.
///
/// Order (matches the legacy server + bridge inline paths byte-for-byte):
///
/// 1. `system_messages` (from the context pipeline).
/// 2. `compacted_messages` (conversation history from Memoria).
/// 3. `volatile_preamble` content is attached as runtime context adjacent to
///    the true tail. If the last message is already `role=user`, append the
///    runtime suffix after the real user text. If it is `role=tool`, append
///    inside that tool result. Otherwise append one synthetic runtime
///    `role=user` message. No assistant acknowledgement is invented.
/// 4. `strip_stale_reasoning` is applied in place.
/// 5. Invoked-skill attachments (server path only).
/// 6. Recent-file attachments (server path only).
/// 7. `apply_anthropic_cache_metadata` (Anthropic path only).
pub(crate) fn assemble_llm_messages_with_cache_capability(
    system_messages: Vec<Value>,
    volatile_preamble: Vec<Value>,
    drained_volatile: Vec<crate::turn::agentic_loop::host::VolatileInjection>,
    compacted_messages: Vec<Value>,
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
    let mut llm_messages = system_messages;
    llm_messages.extend(compacted_messages);

    // Structured volatile lane (`state.volatile_pending`): drained upstream,
    // rendered to the same preamble slot as the historical preamble.
    // Producers use `state.push_volatile(Kind, content)` and never touch
    // `state.messages[]` for volatile content, so `messages[]` stays byte-
    // stable across rounds — the property Anthropic / DeepSeek prompt
    // caches rely on.
    let mut volatile_preamble = volatile_preamble
        .into_iter()
        .filter(|message| !suppress_volatile || is_required_runtime_preamble(message))
        .collect::<Vec<_>>();
    volatile_preamble.extend(
        render_drained_volatile_messages(&drained_volatile)
            .into_iter()
            .filter(|message| !suppress_volatile || is_required_runtime_preamble(message)),
    );

    // Attach runtime content only at the true tail. Required runtime/control
    // frames are structured before this point, but provider chat protocols do
    // not have a hidden runtime lane. On the final wire they therefore ride the
    // same tail suffix as volatile reminders instead of becoming standalone
    // post-prefix system messages.
    let mut synthetic_tail_start: Option<usize> = None;
    let mut synthetic_tail_end: Option<usize> = None;
    let mut tail_user_cache_boundary_applied = false;
    if !volatile_preamble.is_empty() {
        let mut runtime_parts = Vec::new();
        for message in &volatile_preamble {
            let Some(content) = message
                .get("content")
                .and_then(Value::as_str)
                .map(str::trim)
            else {
                continue;
            };
            if content.is_empty() {
                continue;
            }
            runtime_parts.push(content.to_string());
        }

        let runtime_tail_text = runtime_parts.join("\n\n");
        if !runtime_tail_text.is_empty() {
            let tail_role = llm_messages
                .last()
                .and_then(|m| m.get("role").and_then(Value::as_str));
            if tail_role == Some("user") {
                if let Some(tail) = llm_messages.last_mut() {
                    tail_user_cache_boundary_applied = append_volatile_to_tail_user_message(
                        tail,
                        &runtime_tail_text,
                        cache_cfg.should_annotate(),
                    );
                }
            } else if tail_role == Some("tool") {
                let tool_index = llm_messages.len().saturating_sub(1);
                if let Some(tail) = llm_messages.last_mut() {
                    append_runtime_context_to_tail_tool_message(tail, &runtime_tail_text);
                }
                // The tool result now contains per-round runtime bytes. Keep
                // it outside the stable prefix instead of caching volatile
                // content or inventing an assistant acknowledgement.
                synthetic_tail_start = Some(tool_index);
                synthetic_tail_end = Some(llm_messages.len());
            } else {
                synthetic_tail_start = Some(llm_messages.len());
                llm_messages.push(serde_json::json!({
                    "role": "user",
                    "content": runtime_tail_text,
                }));
                synthetic_tail_end = Some(llm_messages.len());
            }
        }
    }
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

    if !attachments.invoked_skills.is_empty() {
        let mut builder = astra_turn_core::cloud_attachments::AttachmentBuilder::new();
        // Caller supplies `invoked_skills` already in the preferred order
        // (most-recent-first). Emitting in the same order matches legacy
        // output — do not re-sort here; re-sorting would flip bytes.
        for skill in &attachments.invoked_skills {
            builder.add_skill(skill.name, skill.content);
        }
        let built = builder.build();
        llm_messages.extend(built.to_messages());
    }

    if !attachments.recent_file_reads.is_empty() {
        let file_messages = astra_turn_core::cloud_attachments::restore_recent_files(
            attachments.recent_file_reads,
            attachments.cwd,
        );
        llm_messages.extend(file_messages);
    }

    // When dynamic runtime context was appended at the final suffix (either as
    // a synthetic user message or inside the tail tool result), place the
    // message-level cache marker on the last stable message before it.
    //
    // User-tail volatile context is appended inside the final user message as
    // a post-marker content block. Inserting it before the latest user would
    // put per-round runtime bytes inside the Anthropic cached prefix and churn
    // historical message hashes on the next turn; appending it as a separate
    // role=system message would violate provider prefix constraints.
    let synthetic_tail_is_final = synthetic_tail_end.is_some_and(|end| end == llm_messages.len());
    if cache_cfg.should_annotate() && !tail_user_cache_boundary_applied {
        if synthetic_tail_is_final {
            if let Some(prefix_end) = synthetic_tail_start {
                apply_anthropic_cache_metadata(
                    &mut llm_messages[..prefix_end],
                    cache_cfg,
                    session_id,
                );
            }
        } else {
            apply_anthropic_cache_metadata(&mut llm_messages, cache_cfg, session_id);
        }
    }
    llm_messages
}

pub(crate) fn append_runtime_context_to_tail_tool_message(message: &mut Value, runtime_text: &str) {
    let Some(object) = message.as_object_mut() else {
        return;
    };
    let runtime_suffix =
        format!("\n\n{TOOL_RUNTIME_CONTEXT_PREFIX}\n{runtime_text}\n{TOOL_RUNTIME_CONTEXT_SUFFIX}");
    match object.get_mut("content") {
        Some(Value::String(text)) => text.push_str(&runtime_suffix),
        Some(Value::Array(blocks)) => {
            let appended = blocks.iter_mut().rev().any(|block| {
                for field in ["text", "content"] {
                    if let Some(text) = block.get_mut(field).and_then(|value| value.as_str()) {
                        let mut combined = text.to_string();
                        combined.push_str(&runtime_suffix);
                        block[field] = Value::String(combined);
                        return true;
                    }
                }
                false
            });
            if !appended {
                blocks.push(serde_json::json!({
                    "type": "text",
                    "text": runtime_suffix.trim_start(),
                }));
            }
        }
        _ => {
            object.insert(
                "content".to_string(),
                Value::String(runtime_suffix.trim_start().to_string()),
            );
        }
    }
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

pub(crate) fn append_volatile_to_tail_user_message(
    message: &mut Value,
    volatile_text: &str,
    mark_cache_boundary_before_volatile: bool,
) -> bool {
    let Some(object) = message.as_object_mut() else {
        return false;
    };
    let volatile_block = serde_json::json!({
        "type": "text",
        "text": volatile_text,
    });

    match object.get_mut("content") {
        Some(Value::String(text)) => {
            let real_user_text = std::mem::take(text);
            if mark_cache_boundary_before_volatile {
                object.insert(
                    "content".to_string(),
                    serde_json::json!([
                        {
                            "type": "text",
                            "text": real_user_text,
                            "cache_control": astra_turn_core::context_serializer::anthropic_ephemeral_cache_control(),
                        },
                        volatile_block,
                    ]),
                );
                true
            } else {
                object.insert(
                    "content".to_string(),
                    Value::String(format!("{real_user_text}\n\n{volatile_text}")),
                );
                false
            }
        }
        Some(Value::Array(blocks)) => {
            let mut marked = false;
            if mark_cache_boundary_before_volatile && let Some(last_real_block) = blocks.last_mut()
            {
                last_real_block["cache_control"] =
                    astra_turn_core::context_serializer::anthropic_ephemeral_cache_control();
                marked = true;
            }
            blocks.push(volatile_block);
            marked
        }
        _ => {
            object.insert(
                "content".to_string(),
                Value::String(volatile_text.to_string()),
            );
            false
        }
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
        let mut message = serde_json::json!({
            "role": "system",
            "content": text,
        });
        if inj.kind.delivery_class()
            == astra_turn_core::chat_turn_edge_profile::VolatileDeliveryClass::RequiredContext
        {
            message[REQUIRED_RUNTIME_PREAMBLE_MARKER] = Value::Bool(true);
        }
        out.push(message);
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
    fn assemble_injects_invoked_skills_after_compacted_messages() {
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
        // Skill attachment appears after the compacted user message.
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
        assert!(
            skill_pos > user_pos,
            "skill attachment must follow the compacted history, not precede it"
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
        assert_eq!(msgs[2]["role"], "user");
        let note = msgs[2]["content"].as_str().unwrap();
        assert!(note.contains("Context was compacted"));
        assert!(note.contains("not a new user request"));
        assert!(!note.contains("keep going"));
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
    fn continuation_prompt_noop_when_assistant_signals_done() {
        let mut msgs = vec![
            json!({"role": "user", "content": "goal"}),
            json!({
                "role": "assistant",
                "content": "All done. Task complete successfully."
            }),
        ];
        let len_before = msgs.len();
        maybe_append_continuation_prompt(&mut msgs, true);
        assert_eq!(
            msgs.len(),
            len_before,
            "completion signal → no continuation appended"
        );
    }

    #[test]
    fn continuation_prompt_still_appends_with_qualified_completion() {
        // "except X" qualifies completion -> negation wins -> note appends.
        let mut msgs = vec![
            json!({"role": "user", "content": "goal"}),
            json!({
                "role": "assistant",
                "content": "All done, except for the migration."
            }),
        ];
        maybe_append_continuation_prompt(&mut msgs, true);
        assert_eq!(msgs.len(), 3);
    }

    #[test]
    fn continuation_prompt_uses_chinese_for_cjk_conversations() {
        let mut msgs = vec![
            json!({"role": "user", "content": "请帮我重构这段代码 重构这段代码 重构这段代码 重构这段代码 重构这段代码 请帮我重构这段代码"}),
            json!({"role": "assistant", "content": "好的,我开始处理"}),
        ];
        maybe_append_continuation_prompt(&mut msgs, true);
        assert_eq!(msgs.len(), 3);
        let note = msgs[2]["content"].as_str().unwrap();
        assert!(
            note.contains("上下文已压缩") && note.contains("不是新的用户请求"),
            "CJK conversation should get the Chinese compaction note: {note}"
        );
        assert!(!note.contains("直接继续"));
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
    fn parity_server_attachments_only_change_tail() {
        // Invariant: server-path attachments (invoked_skills, recent_file_reads)
        // are always APPENDED after the compacted history — they must never
        // mutate or reorder the system prefix + compacted messages that come
        // first. If this breaks, caching invariants break downstream.
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
        // Server output must be a strict prefix extension of bridge output:
        // first N messages identical, then one-or-more extra skill attachments.
        assert!(
            server_out.len() > bridge_out.len(),
            "server with attachments must have strictly more messages"
        );
        for (i, bridge_msg) in bridge_out.iter().enumerate() {
            assert_eq!(
                bridge_msg, &server_out[i],
                "message #{i} diverged between bridge and server paths — \
                 attachments must only append, never reorder or mutate"
            );
        }
    }

    #[test]
    fn parity_cache_annotations_are_terminal_step() {
        // `apply_anthropic_cache_metadata` runs LAST. Both callers rely on
        // this: the cache marker is placed on the final message in the
        // assembled list, and server-path attachments appended *before*
        // annotation means the marker lands on the skill attachment (when
        // present), not the compacted user message.
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

    /// Regression lock: volatile content must not be prepended before the
    /// latest real user intent. Runtime context is appended after that intent
    /// in the same tail user message so provider role ordering remains valid
    /// and prompt-cache markers can sit before the volatile suffix.
    #[test]
    fn prefix_provider_volatile_appends_after_last_user_intent() {
        let stable_sys = vec![json!({"role": "system", "content": "stable core rules only"})];
        let volatile_preamble = vec![
            json!({"role": "user", "content": "<system-reminder>\nTurn: 5 | Tokens: 12000\n</system-reminder>"}),
        ];
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
            "deepseek",
            "deepseek-v4-pro",
            &astra_turn_core::thinking_config::ThinkingConfig::Off,
            None,
            &cache_cfg(),
        );

        // System message is stable
        assert_eq!(msgs[0]["content"], "stable core rules only");

        // History is intact in original order (no preamble pair between them)
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "first question");
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["content"], "first answer");

        assert_eq!(msgs[3]["role"], "user");
        let tail_user = message_text(&msgs[3]);
        assert!(
            tail_user.starts_with("second question"),
            "real user intent must remain first in the tail user message: {tail_user}"
        );
        assert!(
            tail_user.contains("Turn: 5"),
            "volatile runtime context must remain visible after the real user intent: {tail_user}"
        );
        assert!(
            msgs.iter()
                .skip(1)
                .all(|msg| msg.get("role").and_then(Value::as_str) != Some("system")),
            "runtime context must not introduce provider-invalid system messages after history"
        );

        assert_eq!(msgs.len(), 4);
    }

    #[test]
    fn volatile_preamble_appends_to_tail_user_content() {
        let system = vec![json!({"role": "system", "content": "sys"})];
        let preamble =
            vec![json!({"role": "user", "content": "<system-reminder>volatile</system-reminder>"})];
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
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");
        let tail_user = message_text(&msgs[1]);
        assert!(
            tail_user.starts_with("hi"),
            "real user intent must stay first in the tail user message: {tail_user}"
        );
        assert!(
            tail_user.contains("volatile"),
            "volatile runtime context must be appended after real user intent: {tail_user}"
        );
    }

    #[test]
    fn required_runtime_context_uses_protocol_valid_tail_suffix() {
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

        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1]["role"], "user");
        let user_text = message_text(&msgs[1]);
        assert!(
            user_text.starts_with("hi"),
            "real user intent must stay first: {user_text}"
        );
        assert!(user_text.contains("required resume context"));
        assert!(
            msgs.iter()
                .skip(1)
                .all(|msg| msg.get("role").and_then(Value::as_str) != Some("system")),
            "wire payload must not introduce post-prefix system messages: {msgs:#?}"
        );
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
    fn policy_advisory_volatile_reaches_tail_without_overriding_user_intent() {
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

        assert_eq!(msgs.len(), 2);
        let user_text = message_text(&msgs[1]);
        assert!(
            user_text.starts_with("fix the failing tests"),
            "policy advisory must not replace the real user goal: {user_text}"
        );
        assert!(user_text.contains("policy_advisory.v1"));
        assert!(user_text.contains("consider changing approach"));
        assert!(user_text.contains("<runtime-advisory-evidence>"));
        assert!(user_text.contains("\"kind\":\"policy_advisory\""));
        assert!(
            !user_text.contains("Do NOT call"),
            "soft policy advisory must not become a hard tool prohibition: {user_text}"
        );
        assert!(
            msgs.iter()
                .skip(1)
                .all(|msg| msg.get("role").and_then(Value::as_str) != Some("system")),
            "runtime advisory must not introduce provider-invalid post-prefix system frames: {msgs:#?}"
        );
    }

    #[test]
    fn active_turn_frame_anchors_latest_user_goal_after_real_user_content() {
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

        assert_eq!(msgs[1]["content"], "一共多少 changes？");
        assert_eq!(msgs[2]["content"], "148 files");
        assert_eq!(msgs[3]["role"], "user");
        let tail_user = message_text(&msgs[3]);
        assert!(
            tail_user.starts_with("相关的测试够硬核吗？"),
            "real user content must remain first: {tail_user}"
        );
        assert!(tail_user.contains("<runtime-required-context>"));
        assert!(tail_user.contains("\"kind\":\"active_turn_frame\""));
        assert!(
            tail_user.contains("active_goal"),
            "active goal frame must stay explicit in the runtime tail suffix"
        );
        assert!(
            msgs.iter()
                .skip(1)
                .all(|msg| msg.get("role").and_then(Value::as_str) != Some("system")),
            "active turn frame must not introduce post-prefix system messages: {msgs:#?}"
        );
    }

    #[test]
    fn volatile_preamble_appends_protocol_valid_tail_after_tool() {
        let system = vec![json!({"role": "system", "content": "sys"})];
        let preamble =
            vec![json!({"role": "user", "content": "<system-reminder>volatile</system-reminder>"})];
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
        assert_eq!(
            msgs[1]["content"], "hi",
            "historical user message must stay unchanged"
        );
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[3]["role"], "tool");
        let tail_tool = message_text(&msgs[3]);
        assert!(
            tail_tool.starts_with("tool output"),
            "real tool result must remain first: {tail_tool}"
        );
        assert!(
            tail_tool.contains("<system-reminder>volatile</system-reminder>"),
            "volatile reminder should be appended inside the runtime tool tail"
        );
        assert_eq!(
            msgs.len(),
            4,
            "runtime framing must not invent any conversation turn"
        );
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
    fn volatile_preamble_appends_tail_user_when_no_tail_user_exists() {
        let system = vec![json!({"role": "system", "content": "sys"})];
        let preamble =
            vec![json!({"role": "user", "content": "<system-reminder>volatile</system-reminder>"})];
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

        assert_eq!(msgs[1]["content"], "hi");
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["content"], "tail assistant");
        assert_eq!(msgs[3]["role"], "user");
        assert_eq!(
            message_text(&msgs[3]),
            "<system-reminder>volatile</system-reminder>",
            "non-tool tails should append a single synthetic runtime context message"
        );
    }

    #[test]
    fn anthropic_tool_tail_marks_last_real_message_before_synthetic_suffix() {
        let system = vec![json!({"role": "system", "content": "sys"})];
        let preamble =
            vec![json!({"role": "user", "content": "<system-reminder>volatile</system-reminder>"})];
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
        assert!(
            astra_turn_core::context_serializer::message_has_cache_control(&msgs[2]),
            "stable prefix must end before the tool result containing volatile runtime bytes",
        );
        assert!(
            !astra_turn_core::context_serializer::message_has_cache_control(&msgs[3]),
            "dynamic tool/runtime tail must stay unannotated",
        );
        assert!(message_text(&msgs[3]).contains("volatile"));
    }

    #[test]
    fn anthropic_user_tail_keeps_runtime_context_after_user_cache_marker() {
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
        assert_eq!(msgs.len(), 2);
        assert!(
            astra_turn_core::context_serializer::message_has_cache_control(&msgs[1]),
            "final real user message must retain the Anthropic message cache marker",
        );
        let blocks = msgs[1]["content"]
            .as_array()
            .expect("tail user content should be block-shaped");
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].get("cache_control").is_some());
        assert!(
            blocks[0]["text"]
                .as_str()
                .unwrap()
                .contains("latest real user")
        );
        assert!(blocks[1].get("cache_control").is_none());
        assert!(
            blocks[1]["text"]
                .as_str()
                .unwrap()
                .contains("active-turn-frame")
        );
    }

    #[test]
    fn current_user_only_models_drop_volatile_entirely() {
        let system = vec![json!({"role": "system", "content": "sys"})];
        let preamble =
            vec![json!({"role": "user", "content": "<system-reminder>volatile</system-reminder>"})];
        let drained = vec![crate::turn::agentic_loop::host::VolatileInjection {
            kind: crate::turn::agentic_loop::host::VolatileKind::AlreadyFetched,
            payload: json!("## Already Fetched (do NOT re-read/re-grep these)\nFiles: foo.rs"),
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
        assert_eq!(msgs.len(), 4, "no synthetic volatile tail should remain");
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
                    .contains("Already Fetched")
                    && !message
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .contains("tools executed in parallel")
                    && !message
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .contains("<system-reminder>")
            }),
            "CurrentUserOnly providers must drop all volatile wire content"
        );
    }

    #[test]
    fn current_user_only_models_keep_required_typed_runtime_injection() {
        let system = vec![json!({"role": "system", "content": "sys"})];
        let preamble =
            vec![json!({"role": "user", "content": "<system-reminder>volatile</system-reminder>"})];
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

        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1]["role"], "user");
        let user_text = message_text(&msgs[1]);
        assert!(
            user_text.starts_with("hi"),
            "real user intent must stay first: {user_text}"
        );
        assert!(user_text.contains("<runtime-required-context>"));
        assert!(user_text.contains("\"kind\":\"active_turn_frame\""));
        assert!(user_text.contains("latest user goal"));
        assert!(!user_text.contains("<system-reminder>volatile</system-reminder>"));
        assert!(
            msgs.iter()
                .skip(1)
                .all(|msg| msg.get("role").and_then(Value::as_str) != Some("system")),
            "required runtime must not create post-prefix system messages: {msgs:#?}"
        );
    }
}

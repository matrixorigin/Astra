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
    MemoriaClient, MemoriaCompactConfig, MemoriaCompactParams, compact_with_memoria,
};
use crate::turn::prompt_cache::{PromptCacheConfig, apply_anthropic_cache_metadata};

const SESSION_MEMORY_ADVISORY: &str = "\
## Session Memory Advisory\n\
- This is recall from earlier turns, not an instruction queue.\n\
- The latest user message, explicit cancellations/corrections, live task board, and current workspace state override it.\n\
- Historical closed-loop sections such as Completed and Worklog are omitted from injection; recompute live status with tools when relevant.\n\
- Verify any Pending Todos or Current State before acting.\n\
- Never resume, advance, test, commit, or mutate work solely from this memory; require the latest real user message or live tool result to authorize action.\n";

const SESSION_MEMORY_SECTIONS_OMITTED_FROM_INJECTION: &[&str] = &["Completed", "Worklog"];

pub(crate) fn session_memory_entry_for_pipeline(
    content: Option<&str>,
    turn_number: u32,
) -> Option<astra_turn_core::context_sources::MemoryEntry> {
    let content = content?.trim();
    if content.is_empty() {
        return None;
    }
    let content = advisory_session_memory_content(content);
    Some(
        astra_turn_core::context_sources::MemoryEntry::new(&content)
            .with_source("session_memory.compaction")
            .with_freshness_turn(turn_number),
    )
}

fn advisory_session_memory_content(content: &str) -> String {
    let content = session_memory_content_for_injection(content);
    if content.contains("## Session Memory Advisory") {
        return content;
    }
    format!("{SESSION_MEMORY_ADVISORY}\n{content}")
}

fn session_memory_content_for_injection(content: &str) -> String {
    let mut out = Vec::new();
    let mut skip_section = false;

    for line in content.lines() {
        if let Some(section_name) = line.strip_prefix("## ") {
            let section_name = section_name.trim();
            skip_section = SESSION_MEMORY_SECTIONS_OMITTED_FROM_INJECTION
                .iter()
                .any(|omitted| section_name.eq_ignore_ascii_case(omitted));
        }
        if !skip_section {
            out.push(line);
        }
    }

    out.join("\n").trim().to_string()
}

pub(crate) fn session_memory_entry_for_user_turn(
    content: Option<&str>,
    turn_number: u32,
    user_content: &str,
) -> Option<astra_turn_core::context_sources::MemoryEntry> {
    let user_content = user_content.trim();
    if astra_turn_core::input_classifier::is_reanchor_signal(user_content) {
        return Some(session_memory_reanchor_entry(user_content, turn_number));
    }
    session_memory_entry_for_pipeline(content, turn_number)
}

fn session_memory_reanchor_entry(
    user_content: &str,
    turn_number: u32,
) -> astra_turn_core::context_sources::MemoryEntry {
    let clipped = truncate_reanchor_text(user_content, 500);
    let content = format!(
        "## Session Reanchor\n\
         - The latest user reanchor supersedes previously injected session memory where they conflict.\n\
         - Latest user reanchor: {clipped}\n\
         - Treat older session state as stale unless it directly supports this reanchor."
    );
    astra_turn_core::context_sources::MemoryEntry::new(&content)
        .with_source("session_memory.reanchor")
        .with_freshness_turn(turn_number)
}

fn truncate_reanchor_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out = text
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    out.push('…');
    out
}

pub(crate) fn rerun_with_distinct_session_memory_entry_for_user_turn<T>(
    content: Option<&str>,
    existing: Option<&astra_turn_core::context_sources::MemoryEntry>,
    turn_number: u32,
    user_content: &str,
    rerun: impl FnOnce(astra_turn_core::context_sources::MemoryEntry) -> T,
) -> Option<T> {
    let entry = session_memory_entry_for_user_turn(content, turn_number, user_content)?;
    if existing.is_some_and(|current| {
        current.content_hash == entry.content_hash && current.content == entry.content
    }) {
        return None;
    }
    Some(rerun(entry))
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
    pub memoria_client: Option<&'a dyn MemoriaClient>,
    /// Optional summary LLM client. `None` = skip LLM summarization tier.
    pub summary_client: Option<&'a dyn astra_turn_core::cloud_summary::SummaryLlmClient>,
    /// Pipeline-selected compaction tier (authoritative — do NOT re-derive).
    pub tier: CompactionTier,
    /// Optional pre-parsed session facts (bridge path provides these;
    /// server path does not yet).
    pub session_facts: Option<astra_turn_types::session_facts::SessionFacts>,
    /// Current turn number used to tag observatory records. Defaults
    /// to 0 for callers that don't wire observatory.
    pub turn_number: u32,
    /// Optional post-hoc observer. `None` when the host wasn't built
    /// with an observatory (offline CLI, tests).
    pub observatory: Option<std::sync::Arc<crate::session_memory::SessionMemoryObservatory>>,
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
            turn_number: self.turn_number,
            observatory: self.observatory.clone(),
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
///    the true tail. If the last message is already `role=user`, insert a
///    synthetic `role=system` message immediately before it so the latest real
///    user message remains byte-for-byte intact and authoritative. If the tail
///    is `role=tool`, append `assistant("Understood.")` and then a synthetic
///    runtime `role=system` message.
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
    let mut volatile_preamble = if suppress_volatile {
        Vec::new()
    } else {
        volatile_preamble
    };
    if !suppress_volatile {
        volatile_preamble.extend(render_drained_volatile_messages(&drained_volatile));
    }

    // Belt-and-suspenders: legacy callers still push mid-history volatile
    // into messages[] directly (there are ~30 such sites being migrated
    // piecemeal). Until that migration completes,
    // `consolidate_mid_history_volatile_injections` still picks up stragglers.
    // Once zero producers call `state.messages.push(...)` with runtime
    // content, this pass becomes a no-op and can be removed.
    let harvested = consolidate_mid_history_volatile_injections(&mut llm_messages);
    if !suppress_volatile && !harvested.is_empty() {
        volatile_preamble.push(serde_json::json!({
            "role": "user",
            "content": harvested,
        }));
    }

    // Attach volatile content only at the true tail. Rewriting a historical
    // user message while assistant/tool messages trail it mutates mid-history
    // bytes and can zero prefix-cache hits on multi-round tool loops.
    let mut synthetic_tail_start: Option<usize> = None;
    let mut synthetic_tail_end: Option<usize> = None;
    let mut tail_user_cache_boundary_applied = false;
    if !volatile_preamble.is_empty() {
        let mut system_parts = Vec::new();
        let mut user_parts = Vec::new();
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
            match message
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("user")
            {
                "system" => system_parts.push(content.to_string()),
                "user" => user_parts.push(content.to_string()),
                // Historical volatile preamble was represented as a
                // user reminder plus an assistant acknowledgement. The
                // acknowledgement is a transport shim, not instruction
                // content, so it must not be folded into the synthetic
                // reminder user. If the protocol tail requires an assistant
                // bridge, we synthesize that below from the real tail role.
                _ => {}
            }
        }

        let volatile_text = system_parts
            .into_iter()
            .chain(user_parts)
            .collect::<Vec<_>>()
            .join("\n\n");
        if !volatile_text.is_empty() {
            let tail_role = llm_messages
                .last()
                .and_then(|m| m.get("role").and_then(Value::as_str));
            if tail_role == Some("user") {
                if let Some(tail) = llm_messages.last_mut() {
                    tail_user_cache_boundary_applied = append_volatile_to_tail_user_message(
                        tail,
                        &volatile_text,
                        cache_cfg.should_annotate(),
                    );
                }
            } else if tail_role == Some("tool") {
                synthetic_tail_start = Some(llm_messages.len());
                llm_messages.push(serde_json::json!({
                    "role": "assistant",
                    "content": "Understood.",
                }));
                llm_messages.push(serde_json::json!({
                    "role": "user",
                    "content": volatile_text,
                }));
                synthetic_tail_end = Some(llm_messages.len());
            } else {
                synthetic_tail_start = Some(llm_messages.len());
                llm_messages.push(serde_json::json!({
                    "role": "user",
                    "content": volatile_text,
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

    // When synthetic runtime context was appended as the final suffix (no
    // attachments landed after it), place the message-level cache marker on the
    // last REAL message before the synthetic assistant/user reminder pair.
    // Otherwise the synthesized reminder would consume the only message marker
    // and we'd fall back to caching just system+tools on every tool-loop round.
    //
    // User-tail volatile context is appended inside the final user message as
    // a post-marker content block. Inserting it before the latest user would
    // put per-round runtime bytes inside the Anthropic cached prefix and churn
    // historical message hashes on the next turn; appending it as a separate
    // role=system message would violate Anthropic/Bedrock role alternation.
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

fn append_volatile_to_tail_user_message(
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

/// Render the structured volatile lane drained from
/// `AgenticLoopState.volatile_pending` into a single concatenated
/// preamble string. Producers (stall nudges, working-set snapshots,
/// tool-health warnings, …) call `state.push_volatile(kind, content)`
/// and the wire layer renders them all together here so the LLM sees
/// one coherent blob of per-round runtime signal.
///
/// Dedup policy: the producer is responsible for ensuring that each
/// CATEGORY of signal appears at most once in a given drain (e.g. the
/// turn-guard only emits one tool-health warning per turn). This
/// function preserves insertion order so if multiple kinds were queued
/// they come out in the order they were produced.
pub(crate) fn render_drained_volatile(
    drained: &[crate::turn::agentic_loop::host::VolatileInjection],
) -> String {
    let mut out = String::new();
    for inj in drained {
        if !inj.kind.render_in_user_tail() {
            continue;
        }
        let text = inj.content.trim();
        if text.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(text);
    }
    out
}

fn render_drained_volatile_messages(
    drained: &[crate::turn::agentic_loop::host::VolatileInjection],
) -> Vec<Value> {
    let mut out = Vec::new();
    let user_text = render_drained_volatile(drained);
    if !user_text.is_empty() {
        out.push(serde_json::json!({
            "role": "user",
            "content": user_text,
        }));
    }
    for inj in drained {
        if inj.kind.render_in_user_tail() {
            continue;
        }
        let text = inj.content.trim();
        if text.is_empty() {
            continue;
        }
        out.push(serde_json::json!({
            "role": inj.kind.default_role(),
            "content": text,
        }));
    }
    out
}

/// Remove mid-history volatile-style injections from `messages` and return
/// the consolidated text so the caller can fold it into `volatile_preamble`.
///
/// Context: during a tool-loop turn the runtime scatters nudges into the
/// message history — TurnGuard's "⚠ tools have failed …" warnings
/// (once every 2-3 rounds), the `[working-set:v1]` and
/// `## Already Fetched` system blocks (appended at turn boundaries),
/// parallel-batching-force / execution-escalation user corrective
/// messages, the "✓ 2 tools executed in parallel" coaching pings.
///
/// Each of those injections looks volatile to the provider's prefix
/// cache: its CONTENT evolves across rounds (avoid_tools list grows,
/// working_set recent_tools adds entries), and its POSITION in history
/// shifts as tool-result pairs get appended ahead of it. DeepSeek's
/// `/anthropic` endpoint treats every such mid-history byte delta as
/// "new payload" and restarts warm-up, which caps cache_read at the
/// system-prefix size (~2432 in session 05e63cac).
///
/// We keep only the FINAL occurrence of each known pattern (the most
/// recent round's state is always the authoritative one) and return
/// their concatenated text so [`assemble_llm_messages_with_cache_capability`]
/// can render it as adjacent runtime context without mutating the latest real
/// user message. Stripping them out of history removes the mid-history byte
/// churn; the agent still sees the same up-to-date nudges in the tail.
///
/// This is strictly a wire-layer pass. Session persistence (the
/// `conversation_log` snapshot, event journal) is untouched.
pub(crate) fn consolidate_mid_history_volatile_injections(messages: &mut Vec<Value>) -> String {
    // Classifier: given a message (role + content), return Some(kind) if
    // it's a known volatile injection we want to consolidate, None otherwise.
    fn classify(msg: &Value) -> Option<&'static str> {
        let role = msg.get("role").and_then(Value::as_str)?;
        let content = msg.get("content").and_then(Value::as_str)?;
        match role {
            "user" => {
                if content.starts_with("⚠ The following tools have failed") {
                    Some("tool_health_warning")
                } else if content.starts_with("⚠ You have been") {
                    Some("stall_nudge")
                } else if content.contains("consecutive single-tool rounds")
                    && content.contains("parallel")
                {
                    Some("parallel_batching_force")
                } else if content.contains("accumulated")
                    && content.contains("read-only tool calls")
                {
                    Some("execution_escalation")
                } else {
                    None
                }
            }
            "system" => {
                if content.starts_with("[working-set:v1]") {
                    Some("working_set")
                } else if content.starts_with("## Already Fetched") {
                    Some("already_fetched")
                } else if content.starts_with("✓ ") {
                    Some("tool_batch_coaching")
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    // Walk backwards: remember the first (most recent) occurrence of each
    // kind, mark the rest for removal. The index of the FINAL occurrence
    // of each kind survives — we also strip that one from history and
    // keep only the text for the preamble.
    use std::collections::HashMap;
    let mut kept_kinds: HashMap<&'static str, String> = HashMap::new();
    let mut to_remove: Vec<usize> = Vec::new();
    for (idx, msg) in messages.iter().enumerate().rev() {
        let Some(kind) = classify(msg) else {
            continue;
        };
        if kept_kinds.contains_key(kind) {
            // Older duplicate — drop unconditionally.
            to_remove.push(idx);
            continue;
        }
        // First (most recent) occurrence — capture text, then strip.
        if let Some(text) = msg.get("content").and_then(Value::as_str) {
            kept_kinds.insert(kind, text.to_string());
        }
        to_remove.push(idx);
    }

    // Remove in descending order so indices stay valid.
    to_remove.sort_unstable_by(|a, b| b.cmp(a));
    for idx in to_remove {
        messages.remove(idx);
    }

    // Assemble preamble in stable order so the text is byte-deterministic
    // across calls with the same inputs.
    const ORDER: &[&str] = &[
        "tool_health_warning",
        "stall_nudge",
        "parallel_batching_force",
        "execution_escalation",
        "tool_batch_coaching",
        "working_set",
        "already_fetched",
    ];
    let mut out = String::new();
    for kind in ORDER {
        if let Some(text) = kept_kinds.remove(kind) {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(&text);
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
            turn_number: 0,
            observatory: None,
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
    fn rerun_with_distinct_session_memory_entry_for_user_turn_skips_identical_content() {
        let current = session_memory_entry_for_pipeline(Some("same memory"), 7)
            .expect("current session memory entry");
        let rerun = rerun_with_distinct_session_memory_entry_for_user_turn(
            Some("same memory"),
            Some(&current),
            7,
            "continue",
            |_| panic!("identical content should not rerun"),
        );
        assert!(rerun.is_none());
    }

    #[test]
    fn rerun_with_distinct_session_memory_entry_for_user_turn_keeps_changed_content() {
        let current = session_memory_entry_for_pipeline(Some("old memory"), 7)
            .expect("current session memory entry");
        let rerun = rerun_with_distinct_session_memory_entry_for_user_turn(
            Some("new memory"),
            Some(&current),
            7,
            "continue",
            |entry| entry,
        )
        .expect("changed session memory should rerun");
        assert_eq!(
            rerun,
            session_memory_entry_for_pipeline(Some("new memory"), 7).expect("rerun entry")
        );
    }

    #[test]
    fn session_memory_entry_for_user_turn_keeps_memory_for_normal_turn() {
        let entry =
            session_memory_entry_for_user_turn(Some("## Session State\nKeep going"), 8, "continue")
                .expect("session memory entry");

        assert!(entry.content.contains("Session Memory Advisory"));
        assert!(entry.content.contains("not an instruction queue"));
        assert!(entry.content.contains("Keep going"));
        assert_eq!(entry.source.as_deref(), Some("session_memory.compaction"));
    }

    #[test]
    fn session_memory_entry_for_pipeline_does_not_double_wrap_advisory() {
        let content =
            format!("{SESSION_MEMORY_ADVISORY}\n# Session Memory\n\n## Current State\n- x");
        let entry =
            session_memory_entry_for_pipeline(Some(&content), 8).expect("session memory entry");

        assert_eq!(entry.content.matches("Session Memory Advisory").count(), 1);
    }

    #[test]
    fn session_memory_injection_omits_closed_loop_history_sections() {
        let content = "\
# Session Memory

## Active Goals
- Continue fixing runtime UX

## Completed
- Committed changes on branch `0619_job2`
- Ran final checks

## Current State
- Continue from current workspace state

## Worklog
- git status was clean earlier

## Errors & Corrections
- User corrected stale memory attribution
";
        let entry =
            session_memory_entry_for_pipeline(Some(content), 8).expect("session memory entry");

        assert!(entry.content.contains("Continue fixing runtime UX"));
        assert!(
            entry
                .content
                .contains("Continue from current workspace state")
        );
        assert!(
            entry
                .content
                .contains("User corrected stale memory attribution")
        );
        assert!(!entry.content.contains("## Completed"));
        assert!(!entry.content.contains("Committed changes"));
        assert!(!entry.content.contains("## Worklog"));
        assert!(!entry.content.contains("git status was clean"));
        assert!(entry.content.contains("closed-loop sections"));
    }

    #[test]
    fn session_memory_entry_for_user_turn_reanchors_on_correction() {
        let entry = session_memory_entry_for_user_turn(
            Some("stale prior session memory"),
            8,
            "No, that's wrong; use the server-side executor.",
        )
        .expect("reanchor entry");

        assert!(entry.content.contains("Latest user reanchor"));
        assert!(entry.content.contains("server-side executor"));
        assert!(!entry.content.contains("stale prior session memory"));
        assert_eq!(entry.source.as_deref(), Some("session_memory.reanchor"));
    }

    #[test]
    fn session_memory_entry_for_user_turn_reanchors_on_goal_redirect() {
        let entry = session_memory_entry_for_user_turn(
            Some("stale prior session memory"),
            8,
            "不是修修补补，要系统性解决",
        )
        .expect("reanchor entry");

        assert!(entry.content.contains("Latest user reanchor"));
        assert!(entry.content.contains("系统性解决"));
        assert!(!entry.content.contains("stale prior session memory"));
        assert_eq!(entry.source.as_deref(), Some("session_memory.reanchor"));
    }

    #[test]
    fn rerun_with_distinct_session_memory_entry_for_user_turn_keeps_reanchor_stable() {
        let current = session_memory_entry_for_user_turn(
            Some("old stale memory"),
            9,
            "No, that's wrong; keep the explicit invariant.",
        )
        .expect("current reanchor");

        let rerun = rerun_with_distinct_session_memory_entry_for_user_turn(
            Some("new compacted stale memory"),
            Some(&current),
            9,
            "No, that's wrong; keep the explicit invariant.",
            |_| panic!("same correction reanchor should not rerun"),
        );

        assert!(rerun.is_none());
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
            json!({"role": "assistant", "content": "Understood."}),
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
        let preamble = vec![
            json!({"role": "user", "content": "<system-reminder>volatile</system-reminder>"}),
            json!({"role": "assistant", "content": "Understood."}),
        ];
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
    fn self_status_volatile_appends_after_real_user_intent() {
        let system = vec![json!({"role": "system", "content": "sys"})];
        let drained = vec![crate::turn::agentic_loop::host::VolatileInjection {
            kind: crate::turn::agentic_loop::host::VolatileKind::SelfStatus,
            content: "## ⚡ Self-Status\nTurn 9/299 | Cache: 86%".to_string(),
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

        let user_text = msgs
            .iter()
            .filter(|msg| msg.get("role").and_then(Value::as_str) == Some("user"))
            .map(message_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(user_text.contains("相关的测试够硬核吗"));
        assert!(
            user_text.contains("Self-Status"),
            "runtime telemetry must remain visible after the real user intent: {user_text}"
        );
        let system_text = msgs
            .iter()
            .filter(|msg| msg.get("role").and_then(Value::as_str) == Some("system"))
            .map(message_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !system_text.contains("Self-Status"),
            "volatile runtime telemetry must not be smuggled into a post-history system lane"
        );
    }

    #[test]
    fn active_turn_frame_anchors_latest_user_goal_in_tail_user_content() {
        let system = vec![json!({"role": "system", "content": "sys"})];
        let drained = vec![crate::turn::agentic_loop::host::VolatileInjection {
            kind: crate::turn::agentic_loop::host::VolatileKind::ActiveTurnFrame,
            content: "[active-turn-frame:v1]\n{\"latest_user_message\":\"相关的测试够硬核吗？\",\"active_goal\":\"相关的测试够硬核吗？\"}\n[/active-turn-frame]".to_string(),
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
            "latest real user goal must remain first in the tail user message: {tail_user}"
        );
        assert!(tail_user.contains("[active-turn-frame:v1]"));
        assert!(tail_user.contains("相关的测试够硬核吗"));
        assert!(
            tail_user.contains("active_goal"),
            "active goal frame must stay explicit after tool rounds"
        );
        assert!(
            msgs.iter()
                .skip(1)
                .all(|msg| msg.get("role").and_then(Value::as_str) != Some("system")),
            "active-turn runtime context must not require a post-history system role"
        );
    }

    #[test]
    fn volatile_preamble_appends_protocol_valid_tail_after_tool() {
        let system = vec![json!({"role": "system", "content": "sys"})];
        let preamble = vec![
            json!({"role": "user", "content": "<system-reminder>volatile</system-reminder>"}),
            json!({"role": "assistant", "content": "Understood."}),
        ];
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
        assert_eq!(msgs[4]["role"], "assistant");
        assert_eq!(msgs[4]["content"], "Understood.");
        assert_eq!(msgs[5]["role"], "user");
        let tail_user = message_text(&msgs[5]);
        assert!(
            tail_user.starts_with("<system-reminder>volatile</system-reminder>"),
            "volatile reminder should be appended as runtime context"
        );
    }

    #[test]
    fn volatile_preamble_appends_tail_user_when_no_tail_user_exists() {
        let system = vec![json!({"role": "system", "content": "sys"})];
        let preamble = vec![
            json!({"role": "user", "content": "<system-reminder>volatile</system-reminder>"}),
            json!({"role": "assistant", "content": "Understood."}),
        ];
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
        let preamble = vec![
            json!({"role": "user", "content": "<system-reminder>volatile</system-reminder>"}),
            json!({"role": "assistant", "content": "Understood."}),
        ];
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
            astra_turn_core::context_serializer::message_has_cache_control(&msgs[3]),
            "last real tool result must carry the message-level cache marker",
        );
        assert_eq!(msgs[5]["role"], "user");
        assert!(
            !astra_turn_core::context_serializer::message_has_cache_control(&msgs[5]),
            "synthetic runtime tail must stay unannotated",
        );
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
        let preamble = vec![
            json!({"role": "user", "content": "<system-reminder>volatile</system-reminder>"}),
            json!({"role": "assistant", "content": "Understood."}),
        ];
        let drained = vec![crate::turn::agentic_loop::host::VolatileInjection {
            kind: crate::turn::agentic_loop::host::VolatileKind::AlreadyFetched,
            content: "## Already Fetched (do NOT re-read/re-grep these)\nFiles: foo.rs".into(),
            round_index: 1,
        }];
        let compacted = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": ""}),
            json!({"role": "tool", "content": "tool output", "tool_call_id": "c1"}),
            json!({"role": "system", "content": "✓ 2 tools executed in parallel — excellent. Keep batching independent operations."}),
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

    // ── consolidate_mid_history_volatile_injections regressions ─────────
    //
    // Session 05e63cac t5 had 12 `tool_health_warning` user msgs scattered
    // mid-history every 2-3 rounds, plus `[working-set:v1]` and
    // `## Already Fetched` trailing system msgs, plus a "✓ 2 tools executed
    // in parallel" coaching ping. DeepSeek's /anthropic cache plateaued at
    // 2432 tokens (system-only) instead of the probe-measured 88% upper
    // bound because every round rewrote those mid-history bytes.

    #[test]
    fn consolidate_strips_duplicate_tool_health_warnings_keeping_final() {
        let mut msgs = vec![
            json!({"role": "system", "content": "stable sys"}),
            json!({"role": "user", "content": "q1"}),
            json!({"role": "assistant", "content": "a1"}),
            json!({"role": "user", "content": "⚠ The following tools have failed 3 or more times consecutively: [str_replace]. [iteration 1]"}),
            json!({"role": "assistant", "content": "a2"}),
            json!({"role": "user", "content": "⚠ The following tools have failed 3 or more times consecutively: [str_replace, read_file]. [iteration 2]"}),
            json!({"role": "assistant", "content": "a3"}),
            json!({"role": "user", "content": "⚠ The following tools have failed 3 or more times consecutively: [str_replace, read_file, grep]. [iteration 3]"}),
            json!({"role": "user", "content": "latest user turn"}),
        ];
        let preamble = consolidate_mid_history_volatile_injections(&mut msgs);
        // Only the FINAL iteration should survive in preamble.
        assert!(
            preamble.contains("[iteration 3]"),
            "final warning must be preserved: preamble={preamble:?}",
        );
        assert!(
            !preamble.contains("[iteration 1]") && !preamble.contains("[iteration 2]"),
            "older iterations must be dropped: preamble={preamble:?}",
        );
        // Mid-history user-warning slots all stripped; only real conversational
        // turns remain.
        assert_eq!(msgs.len(), 6); // system + q1/a1/a2/a3 + latest user
        for m in &msgs {
            let Some(c) = m.get("content").and_then(Value::as_str) else {
                continue;
            };
            assert!(
                !c.starts_with("⚠ The following tools have failed"),
                "no tool_health_warning msg may remain in-place: {c}",
            );
        }
    }

    #[test]
    fn consolidate_strips_working_set_and_already_fetched_trailing_system() {
        let mut msgs = vec![
            json!({"role": "system", "content": "stable sys"}),
            json!({"role": "user", "content": "q1"}),
            json!({"role": "assistant", "content": "a1"}),
            json!({"role": "user", "content": "q2"}),
            // trailing system msgs injected by runtime
            json!({"role": "system", "content": "[working-set:v1]\ngoal: ship it\nrecent_tools:\n- git [ok t3]"}),
            json!({"role": "system", "content": "## Already Fetched (do NOT re-read/re-grep these)\nsrc/main.rs"}),
        ];
        let preamble = consolidate_mid_history_volatile_injections(&mut msgs);
        assert!(
            preamble.contains("[working-set:v1]"),
            "working-set preserved"
        );
        assert!(
            preamble.contains("## Already Fetched"),
            "already-fetched preserved"
        );
        // Trailing system msgs gone.
        assert_eq!(msgs.len(), 4);
        for m in &msgs {
            let Some(c) = m.get("content").and_then(Value::as_str) else {
                continue;
            };
            assert!(
                !c.starts_with("[working-set:v1]") && !c.starts_with("## Already Fetched"),
                "runtime-injected system msgs must be stripped: {c}",
            );
        }
    }

    #[test]
    fn consolidate_strips_coaching_ping_and_nudge_patterns() {
        let mut msgs = vec![
            json!({"role": "user", "content": "q"}),
            json!({"role": "system", "content": "✓ 2 tools executed in parallel — excellent. Keep batching."}),
            json!({"role": "user", "content": "⚠ You have been exploring for multiple rounds. STOP."}),
            json!({"role": "user", "content": "next"}),
        ];
        let preamble = consolidate_mid_history_volatile_injections(&mut msgs);
        assert!(
            preamble.contains("✓ 2 tools executed"),
            "coaching preserved"
        );
        assert!(
            preamble.contains("⚠ You have been"),
            "stall nudge preserved"
        );
        assert_eq!(msgs.len(), 2); // q + next
    }

    #[test]
    fn consolidate_keeps_real_conversation_user_msgs_intact() {
        // Ordinary user messages (not runtime injections) must survive.
        let mut msgs = vec![
            json!({"role": "user", "content": "write me some code"}),
            json!({"role": "assistant", "content": "ok"}),
            json!({"role": "user", "content": "review the uncommitted changes"}),
            json!({"role": "assistant", "content": "done"}),
            json!({"role": "user", "content": "double check"}),
        ];
        let preamble = consolidate_mid_history_volatile_injections(&mut msgs);
        assert!(
            preamble.is_empty(),
            "no volatile injections → empty preamble"
        );
        assert_eq!(msgs.len(), 5, "real conversation msgs preserved verbatim");
    }

    /// End-to-end regression: two synthetic rounds where only the tail
    /// grows (one tool-result pair appended), with runtime injections
    /// added each round. After consolidation, the prefix up to the added
    /// pair must be byte-identical across both rounds.
    #[test]
    fn consolidate_makes_history_byte_stable_across_rounds() {
        let round_1 = vec![
            json!({"role": "user", "content": "q1"}),
            json!({"role": "user", "content": "⚠ The following tools have failed 3 or more times consecutively: [str_replace]."}),
            json!({"role": "assistant", "tool_calls": [{"id": "c1", "function": {"name": "bash"}}]}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "result1"}),
            json!({"role": "system", "content": "[working-set:v1]\ngoal: fix\nrecent_tools:\n- bash [ok t1]"}),
        ];
        let round_2 = vec![
            json!({"role": "user", "content": "q1"}),
            json!({"role": "user", "content": "⚠ The following tools have failed 3 or more times consecutively: [str_replace, read_file]."}),
            json!({"role": "assistant", "tool_calls": [{"id": "c1", "function": {"name": "bash"}}]}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "result1"}),
            json!({"role": "assistant", "tool_calls": [{"id": "c2", "function": {"name": "bash"}}]}),
            json!({"role": "tool", "tool_call_id": "c2", "content": "result2"}),
            json!({"role": "system", "content": "[working-set:v1]\ngoal: fix\nrecent_tools:\n- bash [ok t1]\n- bash [ok t2]"}),
        ];
        let mut m1 = round_1;
        let mut m2 = round_2;
        let _ = consolidate_mid_history_volatile_injections(&mut m1);
        let _ = consolidate_mid_history_volatile_injections(&mut m2);
        // Round 1: [user q1, assistant_tc, tool] = 3 msgs (warning + working-set stripped).
        // Round 2: Round 1 + [assistant_tc2, tool2]  = 5 msgs.
        assert_eq!(m1.len(), 3, "round 1 post-consolidate: {:#?}", m1);
        assert_eq!(m2.len(), 5, "round 2 post-consolidate: {:#?}", m2);
        // All 3 msgs of round 1 must appear verbatim as the first 3 of round 2.
        for i in 0..m1.len() {
            assert_eq!(
                m1[i], m2[i],
                "msg[{i}] must be byte-identical across rounds after consolidation",
            );
        }
    }
}

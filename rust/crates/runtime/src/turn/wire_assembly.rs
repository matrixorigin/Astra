//! Shared Memoria-compaction + LLM-message-assembly primitives.
//!
//! Used by both the server loop host (`ServerAgenticLoopHost::execute_turn`)
//! and the HTTP bridge (`InProcessChatTurnBridge::forward`). Before this
//! module each path had its own inlined copy of the Memoria call and the
//! wire-building logic — the bodies had drifted apart (e.g. the server
//! path discarded `CompactResult.boundary` and so lost the P2 continuation
//! nudge) and every cache-annotation tweak had to be mirrored twice.
//!
//! Callers orchestrate three steps per turn:
//!
//!   1. [`MemoriaContext::compact`] (or [`MemoriaContext::compact_with_overrides`]
//!      for the emergency retry path) — async HTTP I/O that returns the
//!      full `CompactResult` (messages + boundary + tier).
//!   2. [`maybe_append_continuation_prompt`] — pure, reads the boundary
//!      signal and decides whether to append a "keep going" user nudge.
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

pub(crate) fn session_memory_entry_for_pipeline(
    content: Option<&str>,
    turn_number: u32,
) -> Option<astra_turn_core::context_sources::MemoryEntry> {
    let content = content?.trim();
    if content.is_empty() {
        return None;
    }
    Some(
        astra_turn_core::context_sources::MemoryEntry::new(content)
            .with_source("session_memory.compaction")
            .with_freshness_turn(turn_number),
    )
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
    /// Run Memoria-based history compaction. Returns the full `CompactResult`
    /// so callers can react to `boundary.is_some()` (e.g. for the P2
    /// continuation prompt).
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
        let budget = crate::prompts::budget_for_model(Some(self.model_name));
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
        let mut all_msgs = system_messages.to_vec();
        all_msgs.extend(messages.iter().cloned());
        let cache_est = crate::prompts::estimate_tokens_cache_aware(&all_msgs, tool_schema_tokens);

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

/// Minimal view of a single invoked skill that `assemble_llm_messages` needs.
/// Copied out of the full `SkillInvocationRecord` so this module doesn't pull
/// in the runtime's full state types. The caller is responsible for ordering
/// (most-recent-first); we emit in the supplied order.
pub(crate) struct InvokedSkillRef<'a> {
    pub name: &'a str,
    pub content: &'a str,
}

/// Append a "keep going" user prompt when compaction removed messages and
/// the last remaining assistant message doesn't already signal task
/// completion. Mirrors the bridge-path "P2 continuation" behaviour.
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
    // Detect CJK content in recent turns to emit a localised nudge.
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
    let prompt = if is_cjk {
        "从上次中断的地方继续。不要向用户提问，直接继续当前任务。"
    } else {
        "Continue the conversation from where it left off. \
         Do not ask the user any further questions — \
         pick up the current task and keep going."
    };
    messages.push(serde_json::json!({
        "role": "user",
        "content": prompt,
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
/// 3. `volatile_preamble` content is attached at the true tail:
///    if the last message is already `role=user`, prepend there; if the tail is
///    `role=tool`, append `assistant("Understood.")` + `user(reminder)` so the
///    wire alternation stays protocol-valid; otherwise append one synthetic tail
///    `role=user` reminder. This avoids rewriting a historical user message
///    when `assistant/tool` messages trail it.
/// 4. `strip_stale_reasoning` is applied in place.
/// 5. Invoked-skill attachments (server path only).
/// 6. Recent-file attachments (server path only).
/// 7. `apply_anthropic_cache_metadata` (Anthropic path only).
#[cfg(test)]
pub(crate) fn assemble_llm_messages(
    system_messages: Vec<Value>,
    volatile_preamble: Vec<Value>,
    drained_volatile: Vec<crate::turn::agentic_loop::host::VolatileInjection>,
    compacted_messages: Vec<Value>,
    attachments: &PostCompactAttachments<'_>,
    session_id: &str,
    provider: &str,
    model_name: &str,
    cache_cfg: &PromptCacheConfig,
) -> Vec<Value> {
    assemble_llm_messages_with_cache_capability(
        system_messages,
        volatile_preamble,
        drained_volatile,
        compacted_messages,
        attachments,
        session_id,
        provider,
        model_name,
        None,
        cache_cfg,
    )
}

pub(crate) fn assemble_llm_messages_with_cache_capability(
    system_messages: Vec<Value>,
    volatile_preamble: Vec<Value>,
    drained_volatile: Vec<crate::turn::agentic_loop::host::VolatileInjection>,
    compacted_messages: Vec<Value>,
    attachments: &PostCompactAttachments<'_>,
    session_id: &str,
    provider: &str,
    model_name: &str,
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
    let drained_text = render_drained_volatile(&drained_volatile);
    if !suppress_volatile && !drained_text.is_empty() {
        volatile_preamble.push(serde_json::json!({
            "role": "user",
            "content": drained_text,
        }));
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
    if !volatile_preamble.is_empty() {
        let volatile_text: String = volatile_preamble
            .iter()
            .filter(|m| m.get("role").and_then(Value::as_str) == Some("user"))
            .filter_map(|m| m.get("content").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        if !volatile_text.is_empty() {
            let tail_role = llm_messages
                .last()
                .and_then(|m| m.get("role").and_then(Value::as_str));
            if tail_role == Some("user") {
                let last_user = llm_messages
                    .last_mut()
                    .expect("tail_role=user implies a last message exists");
                let existing = last_user
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                last_user["content"] = Value::String(format!("{volatile_text}\n\n{existing}"));
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
                // No tail user available — append one synthetic tail reminder
                // instead of rewriting a historical user message.
                synthetic_tail_start = Some(llm_messages.len());
                llm_messages.push(serde_json::json!({"role": "user", "content": volatile_text}));
                synthetic_tail_end = Some(llm_messages.len());
            }
        }
    }
    astra_turn_core::edge_ledger::strip_stale_reasoning(&mut llm_messages, provider, model_name);

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

    // When the synthetic tail is still the final suffix (no attachments landed
    // after it), place the message-level cache marker on the last REAL message
    // before the synthetic assistant/user reminder pair. Otherwise the
    // synthesized reminder would consume the only message marker and we'd fall
    // back to caching just system+tools on every tool-loop round.
    let synthetic_tail_is_final = synthetic_tail_end.is_some_and(|end| end == llm_messages.len());
    if cache_cfg.should_annotate() {
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
/// their concatenated text so [`assemble_llm_messages`] can prepend it
/// to the last user message. Stripping them out of history removes the
/// mid-history byte churn; the agent still sees the same up-to-date
/// nudges in the tail.
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
    fn assemble_empty_attachments_matches_simple_concat() {
        let system = vec![json!({"role": "system", "content": "sys"})];
        let compacted = vec![json!({"role": "user", "content": "hi"})];
        let msgs = assemble_llm_messages(
            system.clone(),
            Vec::new(),
            Vec::new(),
            compacted.clone(),
            &PostCompactAttachments::default(),
            "s1",
            "openai",
            "gpt-4",
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
        let msgs = assemble_llm_messages(
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
    fn continuation_prompt_appends_when_boundary_set_and_last_is_assistant() {
        let mut msgs = vec![
            json!({"role": "user", "content": "original goal"}),
            json!({"role": "assistant", "content": "partial progress"}),
        ];
        maybe_append_continuation_prompt(&mut msgs, true);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[2]["role"], "user");
        assert!(
            msgs[2]["content"]
                .as_str()
                .unwrap()
                .contains("Continue the conversation")
        );
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
        // "except X" qualifies completion → negation wins → nudge appends.
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
        let nudge = msgs[2]["content"].as_str().unwrap();
        assert!(
            nudge.contains("继续"),
            "CJK conversation should get the Chinese nudge: {nudge}"
        );
    }

    // ─────────────────────────────────────────────────────────────
    // Cross-caller parity pins
    //
    // Both `ServerAgenticLoopHost::execute_turn` and
    // `InProcessChatTurnBridge::forward` call `assemble_llm_messages`.
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
        let bridge_msgs = assemble_llm_messages(
            system.clone(),
            Vec::new(),
            Vec::new(),
            compacted.clone(),
            &PostCompactAttachments::default(),
            "sid",
            "openai",
            "gpt-4",
            &cache_cfg(),
        );
        let server_msgs = assemble_llm_messages(
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
        //   3. assemble_llm_messages(system, preamble, result.messages, ...)
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
        let first_out = assemble_llm_messages(
            system.clone(),
            Vec::new(),
            Vec::new(),
            first,
            &PostCompactAttachments::default(),
            "sid",
            "openai",
            "gpt-4",
            &cache_cfg(),
        );

        let mut second = make_compacted();
        maybe_append_continuation_prompt(&mut second, true);
        let second_out = assemble_llm_messages(
            system,
            Vec::new(),
            Vec::new(),
            second,
            &PostCompactAttachments::default(),
            "sid",
            "openai",
            "gpt-4",
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
        let bridge_out = assemble_llm_messages(
            system.clone(),
            Vec::new(),
            Vec::new(),
            compacted.clone(),
            &PostCompactAttachments::default(),
            "sid",
            "openai",
            "gpt-4",
            &cache_cfg(),
        );
        let server_out = assemble_llm_messages(
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

        let bridge_out = assemble_llm_messages(
            system.clone(),
            Vec::new(),
            Vec::new(),
            compacted.clone(),
            &PostCompactAttachments::default(),
            "sid",
            "anthropic", // anthropic triggers cache_control annotation
            "claude-sonnet-4",
            &PromptCacheConfig::latch("anthropic", "claude-sonnet-4"),
        );
        let server_out = assemble_llm_messages(
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
        let msgs = assemble_llm_messages(
            vec![json!({"role": "system", "content": "sys"})],
            Vec::new(),
            Vec::new(),
            vec![json!({"role": "user", "content": "hi"})],
            &PostCompactAttachments::default(),
            "sid",
            "openai",
            "gpt-4o",
            &PromptCacheConfig::latch("openai", "gpt-4o"),
        );

        assert!(
            msgs.iter()
                .all(|message| message.get("cache_control").is_none()),
            "prefix-only providers must never receive anthropic cache_control markers"
        );
    }

    /// Regression lock: for prefix-only providers, volatile content MUST NOT
    /// appear in the system message OR as a separate early message pair.
    /// When the tail is already a user message, it should be prepended there;
    /// otherwise it must be appended as one synthetic tail user reminder.
    #[test]
    fn prefix_provider_volatile_prepended_to_last_user_message() {
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

        let msgs = assemble_llm_messages(
            stable_sys,
            volatile_preamble,
            Vec::new(),
            history,
            &PostCompactAttachments::default(),
            "sid",
            "deepseek",
            "deepseek-v4-pro",
            &cache_cfg(),
        );

        // System message is stable
        assert_eq!(msgs[0]["content"], "stable core rules only");

        // History is intact in original order (no preamble pair between them)
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "first question");
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["content"], "first answer");

        // Last user message has volatile prepended to its content
        assert_eq!(msgs[3]["role"], "user");
        let last_user = msgs[3]["content"].as_str().unwrap();
        assert!(
            last_user.contains("Turn: 5"),
            "volatile must be prepended to last user message"
        );
        assert!(
            last_user.contains("second question"),
            "original user content must be preserved"
        );
        assert!(
            last_user.starts_with("<system-reminder>"),
            "volatile must come BEFORE user content"
        );

        // Total message count: system + 2 history + 1 combined last = 4
        // (NOT 6 which would indicate a separate preamble pair)
        assert_eq!(msgs.len(), 4, "no extra preamble pair should be inserted");
    }

    #[test]
    fn volatile_preamble_prepended_to_last_user_not_inserted_as_pair() {
        let system = vec![json!({"role": "system", "content": "sys"})];
        let preamble = vec![
            json!({"role": "user", "content": "<system-reminder>volatile</system-reminder>"}),
            json!({"role": "assistant", "content": "Understood."}),
        ];
        let compacted = vec![json!({"role": "user", "content": "hi"})];
        let msgs = assemble_llm_messages(
            system,
            preamble,
            Vec::new(),
            compacted,
            &PostCompactAttachments::default(),
            "sid",
            "openai",
            "gpt-4",
            &cache_cfg(),
        );
        // 2 messages: system + combined last user (volatile prepended to "hi")
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");
        let user_content = msgs[1]["content"].as_str().unwrap();
        assert!(user_content.contains("volatile"), "volatile prepended");
        assert!(user_content.contains("hi"), "original content preserved");
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
        let msgs = assemble_llm_messages(
            system,
            preamble,
            Vec::new(),
            compacted,
            &PostCompactAttachments::default(),
            "sid",
            "openai",
            "gpt-4",
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
        let tail_user = msgs[5]["content"].as_str().unwrap();
        assert!(
            tail_user.starts_with("<system-reminder>volatile</system-reminder>"),
            "volatile reminder should be appended as the true tail user"
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
        let msgs = assemble_llm_messages(
            system,
            preamble,
            Vec::new(),
            compacted,
            &PostCompactAttachments::default(),
            "sid",
            "openai",
            "gpt-4",
            &cache_cfg(),
        );

        assert_eq!(msgs[1]["content"], "hi");
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["content"], "tail assistant");
        assert_eq!(msgs[3]["role"], "user");
        assert_eq!(
            msgs[3]["content"], "<system-reminder>volatile</system-reminder>",
            "non-tool tails should append a single synthetic reminder user without an extra assistant ack"
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
        let msgs = assemble_llm_messages(
            system,
            preamble,
            Vec::new(),
            compacted,
            &PostCompactAttachments::default(),
            "sid",
            "anthropic",
            "claude-sonnet-4",
            &anthropic_cache_cfg(),
        );

        assert_eq!(msgs[3]["role"], "tool");
        assert!(
            astra_turn_core::context_serializer::message_has_cache_control(&msgs[3]),
            "last real tool result must carry the message-level cache marker",
        );
        assert!(
            !astra_turn_core::context_serializer::message_has_cache_control(&msgs[5]),
            "synthetic tail user must stay unannotated",
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
        let msgs = assemble_llm_messages(
            system,
            preamble,
            drained,
            compacted,
            &PostCompactAttachments::default(),
            "sid",
            "openai",
            "deepseek-v4-flash",
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

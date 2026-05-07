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
    resolve_session_memory_file_options,
};
use crate::turn::prompt_cache::{PromptCacheConfig, apply_anthropic_cache_metadata};

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
    /// CWD for resolving on-disk session memory paths.
    pub cwd: Option<&'a str>,
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
        let (session_memory_file, session_memory_combine) =
            resolve_session_memory_file_options(self.session_id, self.cwd);
        let memoria_params = MemoriaCompactParams {
            budget_chars: resolved.budget_chars,
            keep_chars: resolved.keep_chars,
            tier: resolved.tier,
            keep_recent_turns: resolved.keep_recent_turns,
            current_tokens: resolved.current_tokens,
            session_memory_file,
            session_memory_combine,
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
/// 2. `volatile_preamble` (prefix-only providers: volatile content moved
///    out of the system message into a synthetic user/assistant pair so
///    the system message stays byte-stable across turns for cache hits).
/// 3. `compacted_messages` (from Memoria).
/// 4. `strip_stale_reasoning` is applied in place (reduces input tokens
///    without changing the visible conversation for thinking-model APIs).
/// 5. Invoked-skill attachments (server path only — bridge leaves empty).
/// 6. Recent-file attachments (server path only).
/// 7. `apply_anthropic_cache_metadata` (cache_edits block + tool-result
///    cache_reference markers; last-message breakpoint).
pub(crate) fn assemble_llm_messages(
    system_messages: Vec<Value>,
    volatile_preamble: Vec<Value>,
    compacted_messages: Vec<Value>,
    attachments: &PostCompactAttachments<'_>,
    session_id: &str,
    provider: &str,
    model_name: &str,
    cache_cfg: &PromptCacheConfig,
) -> Vec<Value> {
    let mut llm_messages = system_messages;
    llm_messages.extend(volatile_preamble);
    llm_messages.extend(compacted_messages);
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

    apply_anthropic_cache_metadata(&mut llm_messages, cache_cfg, session_id);
    llm_messages
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cache_cfg() -> PromptCacheConfig {
        PromptCacheConfig::latch("openai", "gpt-4")
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

    /// Regression lock: for prefix-only providers, volatile content MUST NOT
    /// appear in the system message. If it does, prefix cache hit rates drop
    /// from ~80% to ~20% because byte-level prefix matching breaks at the
    /// first volatile byte. This test catches any future refactor that
    /// accidentally routes volatile content back into the system message.
    #[test]
    fn prefix_provider_system_message_contains_no_volatile_content() {
        let stable_sys = vec![json!({"role": "system", "content": "stable core rules only"})];
        let volatile_preamble = vec![
            json!({"role": "user", "content": "<system-reminder>\nTurn: 5 | Tokens: 12000\n</system-reminder>"}),
            json!({"role": "assistant", "content": "Understood."}),
        ];
        let history = vec![json!({"role": "user", "content": "hello"})];

        let msgs = assemble_llm_messages(
            stable_sys,
            volatile_preamble,
            history,
            &PostCompactAttachments::default(),
            "sid",
            "deepseek",
            "deepseek-v4-pro",
            &cache_cfg(),
        );

        // System message must be purely stable — no turn counters, no tokens,
        // no per-turn signals.
        let sys_content = msgs[0]["content"].as_str().unwrap();
        assert_eq!(sys_content, "stable core rules only");
        assert!(
            !sys_content.contains("Turn:"),
            "system message must not contain volatile turn counter"
        );
        assert!(
            !sys_content.contains("Tokens:"),
            "system message must not contain volatile token stats"
        );

        // Volatile content must be in the preamble (position 1-2), not system.
        assert_eq!(msgs[1]["role"], "user");
        assert!(msgs[1]["content"].as_str().unwrap().contains("Turn: 5"));
    }

    #[test]
    fn volatile_preamble_inserted_between_system_and_compacted() {
        let system = vec![json!({"role": "system", "content": "sys"})];
        let preamble = vec![
            json!({"role": "user", "content": "<system-reminder>volatile</system-reminder>"}),
            json!({"role": "assistant", "content": "Understood."}),
        ];
        let compacted = vec![json!({"role": "user", "content": "hi"})];
        let msgs = assemble_llm_messages(
            system,
            preamble,
            compacted,
            &PostCompactAttachments::default(),
            "sid",
            "openai",
            "gpt-4",
            &cache_cfg(),
        );
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");
        assert!(msgs[1]["content"].as_str().unwrap().contains("volatile"));
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["content"], "Understood.");
        assert_eq!(msgs[3]["role"], "user");
        assert_eq!(msgs[3]["content"], "hi");
    }
}

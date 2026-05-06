//! Shared Memoria-compaction + LLM-message-assembly primitives.
//!
//! Used by both the server loop host (`ServerAgenticLoopHost::execute_turn`)
//! and the HTTP bridge (`InProcessChatTurnBridge::forward`). Before this
//! module each path had its own inlined copy of the Memoria call and the
//! wire-building logic — the bodies had drifted apart (e.g. the server
//! path discarded `CompactResult.boundary` and so lost the P2 continuation
//! nudge) and every cache-annotation tweak had to be mirrored twice.
//!
//! Split into three steps that callers orchestrate:
//!
//!   1. [`MemoriaContext::compact`] — async HTTP I/O that returns the
//!      full `CompactResult` (messages + boundary + tier).
//!   2. [`maybe_append_continuation_prompt`] — pure, reads the boundary
//!      signal and decides whether to append a "keep going" user nudge.
//!   3. [`assemble_llm_messages`] — pure, stitches system messages,
//!      compacted messages, optional post-compaction attachments, and
//!      Anthropic cache annotations into the final wire payload.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::Value;

use crate::prompts::{CompactConfig, CompactionTier};
use crate::turn::cloud::compaction::CompactResult;
use crate::turn::cloud::memoria_compact::{
    MemoriaClient, MemoriaCompactConfig, MemoriaCompactParams, SessionMemoryFileCombine,
    compact_with_memoria, resolve_session_memory_file_options,
};
use crate::turn::prompt_cache::{PromptCacheConfig, apply_anthropic_cache_metadata};

/// All inputs `compact_with_memoria` needs, bundled so callers don't thread
/// 8 parameters through their call sites.
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

impl<'a> MemoriaContext<'a> {
    /// Run Memoria-based history compaction. Returns the full `CompactResult`
    /// so callers can react to `boundary.is_some()` (e.g. for the P2
    /// continuation prompt).
    pub async fn compact(
        &self,
        messages: &[Value],
        system_messages: &[Value],
        visible_tools: &[Value],
        header_overrides: &HashMap<String, String>,
        completions_url_override: Option<&str>,
        request_timeout: Option<Duration>,
    ) -> CompactResult {
        let budget = crate::prompts::budget_for_model(Some(self.model_name));
        let budget_chars = budget.effective_input_limit() * 4;

        // `current_tokens` feeds Memoria's budget-pressure knob. `tier` is
        // the authoritative compaction choice (pipeline planner), so this
        // estimate only tunes retrieval aggressiveness.
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

        let memoria_config = MemoriaCompactConfig::default();
        let (session_memory_file, session_memory_combine) =
            resolve_session_memory_file_options(self.session_id, self.cwd);
        let _ = SessionMemoryFileCombine::None; // keep import live
        let memoria_params = MemoriaCompactParams {
            budget_chars,
            keep_chars: 2_000,
            tier: self.tier,
            keep_recent_turns: budget.keep_recent_turns,
            current_tokens: cache_est.total_tokens,
            session_memory_file,
            session_memory_combine,
            session_facts: self.session_facts.clone(),
        };

        let compact_config = CompactConfig::from_env();
        // Summary client is injected from outside — keeping HTTP construction
        // site-local (each caller knows its forwarded auth headers + override
        // URLs) avoids coupling this module to the bridge's
        // `RequestAwareSummaryClient` or the turn-core `HttpSummaryClient`.
        let _ = header_overrides;
        let _ = completions_url_override;
        let _ = request_timeout;

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
/// 2. `compacted_messages` (from Memoria).
/// 3. `strip_stale_reasoning` is applied in place (reduces input tokens
///    without changing the visible conversation for thinking-model APIs).
/// 4. Invoked-skill attachments (server path only — bridge leaves empty).
/// 5. Recent-file attachments (server path only).
/// 6. `apply_anthropic_cache_metadata` (cache_edits block + tool-result
///    cache_reference markers; last-message breakpoint).
pub(crate) fn assemble_llm_messages(
    system_messages: Vec<Value>,
    compacted_messages: Vec<Value>,
    attachments: &PostCompactAttachments<'_>,
    session_id: &str,
    provider: &str,
    model_name: &str,
    cache_cfg: &PromptCacheConfig,
) -> Vec<Value> {
    let mut llm_messages = system_messages;
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
    fn assemble_empty_attachments_matches_simple_concat() {
        let system = vec![json!({"role": "system", "content": "sys"})];
        let compacted = vec![json!({"role": "user", "content": "hi"})];
        let msgs = assemble_llm_messages(
            system.clone(),
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
}

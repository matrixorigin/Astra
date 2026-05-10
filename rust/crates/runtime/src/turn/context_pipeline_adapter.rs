//! Context pipeline adapter — bridges AgenticLoopState to ContextSources.
//!
//! This module is the SOLE place where runtime-specific state is translated
//! into the pipeline's typed inputs. The pipeline never sees AgenticLoopState
//! directly; it only sees the clean ContextSources view.
//!
//! The adapter extracts dynamic prompt fragments from the Host + State,
//! packages them into ExternalSources, and returns a TurnInput ready for
//! `PipelineSession::run_turn_adaptive()`.

use serde_json::Value;

use astra_turn_core::context_sources::{
    EdgeProfile, ExternalSources, MemoryEntry, SessionContext, TurnState,
};
use astra_turn_core::microcompact::ProviderCacheStrategy;
use astra_turn_core::recovery_state::RecoveryState;
use astra_turn_core::token_accounting::TokenAccounting;

use super::agentic_loop_host::AgenticLoopState;

/// Build ExternalSources from the Host's edge_profile + state.
///
/// Single extraction point for all dynamic prompt fragments.
pub(crate) fn build_external_sources(
    edge_profile: &serde_json::Map<String, Value>,
    state: &AgenticLoopState,
    user_content: &str,
    tool_names: &[&str],
    selection_confidence: f64,
    plan_resume_hint: Option<&str>,
) -> ExternalSources {
    // silence: the binder consumes `user_content` via TurnState.last_user_message;
    // here it's only used for future task-type detection if needed.
    let _ = user_content;
    // 1. Self-model (tool-dependent capabilities)
    let self_model_text = if tool_names.is_empty() {
        None
    } else {
        Some(crate::prompts::self_model_section(tool_names))
    };

    // 2. Tool-conditional guidance
    let profile_for_tc = edge_profile
        .get("cwd")
        .and_then(Value::as_str)
        .map(|cwd| format!("cwd: {cwd}"))
        .unwrap_or_default();
    let tool_conditional = if tool_names.is_empty() {
        None
    } else {
        let text = crate::prompts::tool_conditional_section(
            tool_names,
            &profile_for_tc,
            selection_confidence,
        );
        if text.is_empty() { None } else { Some(text) }
    };

    // 3. Environment context — routed split by cache volatility.
    //    Static (Platform/Shell/CWD/Home) → RuntimeIdentity (Session cache).
    //    Volatile (git branch dirty / diff / commits) → RuntimeVolatile.
    //    The legacy `# Project Profile\ncwd:/git_branch:` Markdown block
    //    has been dropped: bind_runtime_identity already emits typed
    //    `Model: / CWD: / Branch:` lines from SessionContext, so
    //    re-emitting cwd/branch as a second header was pure duplicate.
    let env_static = edge_profile
        .get("environment_static")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let env_volatile = edge_profile
        .get("environment_volatile")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    // 4. Effort/agent_type hint
    let effort_hint = {
        let mut hint = String::new();
        if let Some(ref effort) = state.skills.effort {
            hint.push_str(&format!(
                "\n\n## Effort Level\nThe active skill requests effort level: **{effort}**. Adjust thoroughness accordingly.",
            ));
        }
        if let Some(ref agent_type) = state.skills.agent_type {
            hint.push_str(&format!(
                "\n\n## Agent Type\nYou are acting as a **{agent_type}** agent for this skill.",
            ));
        }
        if hint.is_empty() { None } else { Some(hint) }
    };

    // 6. System override (delegation)
    let system_override = edge_profile
        .get("system_prompt_override")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|s| format!("\n\n{s}"));

    // 7. Plan context
    let plan_context = plan_resume_hint.filter(|s| !s.is_empty()).map(String::from);

    // 8. Tool round guidance
    let tool_cfg = astra_config::runtime_config::RuntimeConfig::load().tool_selection;
    let (tool_guidance_text, _signals) = crate::prompts::tool_round_guidance_trace_with(
        &state.messages,
        state.llm_rounds_completed,
        tool_cfg.effective_round_budget_warning(),
        tool_cfg.effective_round_budget_limit(),
    );
    let low_confidence_guidance =
        crate::prompts::low_confidence_tool_selection_section(selection_confidence);
    let tool_guidance = if tool_guidance_text.is_empty() {
        low_confidence_guidance
    } else if let Some(low_confidence) = low_confidence_guidance {
        Some(format!("{tool_guidance_text}{low_confidence}"))
    } else {
        Some(tool_guidance_text)
    };

    // 9. Active skill names as hint
    let _active_skill_names: Vec<&str> = edge_profile
        .get("active_skills")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    let mut extra_dynamic_sections = Vec::new();
    if let Some(ref text) = self_model_text {
        extra_dynamic_sections.push(crate::prompts::PromptSection::dynamic(
            text.clone(),
            crate::prompts::PromptTokenBucket::BasePersona,
        ));
    }
    if let Some(ref text) = tool_conditional {
        extra_dynamic_sections.push(crate::prompts::PromptSection::dynamic(
            text.clone(),
            crate::prompts::PromptTokenBucket::BasePersona,
        ));
    }

    let memory_entries = build_memory_entries_from_edge_profile(edge_profile);

    let spill_backend: Option<std::sync::Arc<dyn astra_turn_core::spill_backend::SpillBackend>> =
        edge_profile
            .get("spill_dir")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(|s| {
                std::sync::Arc::new(astra_turn_core::spill_backend::FileSystemSpillBackend::new(
                    s,
                ))
                    as std::sync::Arc<dyn astra_turn_core::spill_backend::SpillBackend>
            });

    // Skill listing — per-turn ranked shortlist of available skills, based
    // on the current user message. Previously injected post-pipeline as a
    // `role: system` message and folded into `body.system[]` via
    // `consolidate_system_messages`. Routing it through the pipeline's
    // volatile lane keeps the same cache behaviour (it lands in
    // RuntimeVolatile / None scope) but makes the pipeline the single
    // owner of system-block content — simpler to reason about where each
    // byte of the system prompt comes from.
    let skill_listing_extra = state.skills.listing_message.as_ref().and_then(|listing| {
        listing
            .get("content")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(|s| {
                crate::prompts::PromptSection::dynamic(
                    s.to_string(),
                    crate::prompts::PromptTokenBucket::Environment,
                )
            })
    });

    // Environment context — the static half sits in the Session cache,
    // the volatile half (git state) rides the None-scope lane.
    let extra_stable_sections: Vec<crate::prompts::PromptSection> = env_static
        .into_iter()
        .map(|text| {
            crate::prompts::PromptSection::dynamic(
                text,
                crate::prompts::PromptTokenBucket::Environment,
            )
        })
        .collect();
    if let Some(text) = env_volatile {
        extra_dynamic_sections.push(crate::prompts::PromptSection::dynamic(
            text,
            crate::prompts::PromptTokenBucket::Environment,
        ));
    }

    ExternalSources {
        memory_entries,
        spill_dir: None,
        spill_backend,

        effort_hint,
        system_override,
        plan_context,
        tool_guidance,
        // Adapter path (ServerAgenticLoopHost) puts environment_static
        // here so it lands in the cached Session prefix.
        extra_stable_sections,
        // Volatile lane: environment_volatile (git state), skill
        // listing. None-scope so churn doesn't invalidate the cached
        // prefix.
        extra_dynamic_sections: {
            extra_dynamic_sections.extend(skill_listing_extra);
            extra_dynamic_sections
        },
    }
}

fn build_memory_entries_from_edge_profile(
    edge_profile: &serde_json::Map<String, Value>,
) -> Vec<MemoryEntry> {
    if let Some(entries) = edge_profile.get("memory_entries").and_then(Value::as_array) {
        let total = entries.len();
        return entries
            .iter()
            .enumerate()
            .filter_map(|(idx, value)| memory_entry_from_value(value, total, idx))
            .collect();
    }

    edge_profile
        .get("memory_section")
        .and_then(Value::as_str)
        .map(memory_entries_from_section)
        .unwrap_or_default()
}

fn memory_entry_from_value(value: &Value, total: usize, idx: usize) -> Option<MemoryEntry> {
    match value {
        Value::String(content) => {
            let content = content.trim();
            if content.is_empty() {
                return None;
            }
            Some(
                MemoryEntry::scored(content, default_memory_relevance_from_total(total, idx))
                    .with_source("edge_profile.memory_entries"),
            )
        }
        Value::Object(obj) => {
            let content = obj.get("content").and_then(Value::as_str)?.trim();
            if content.is_empty() {
                return None;
            }
            let mut entry = MemoryEntry::scored(
                content,
                obj.get("relevance_score")
                    .and_then(Value::as_f64)
                    .unwrap_or_else(|| default_memory_relevance_from_total(total, idx)),
            )
            .with_source(
                obj.get("source")
                    .and_then(Value::as_str)
                    .unwrap_or("edge_profile.memory_entries"),
            );
            if let Some(tokens) = obj.get("token_estimate").and_then(Value::as_u64) {
                entry = entry.with_token_estimate(tokens.min(u64::from(u32::MAX)) as u32);
            }
            if let Some(turn) = obj.get("freshness_turn").and_then(Value::as_u64) {
                entry = entry.with_freshness_turn(turn.min(u64::from(u32::MAX)) as u32);
            }
            Some(entry)
        }
        _ => None,
    }
}

fn memory_entries_from_section(section: &str) -> Vec<MemoryEntry> {
    let lines: Vec<&str> = section
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && *line != "## User Memories")
        .collect();
    let total = lines.len();
    lines
        .into_iter()
        .enumerate()
        .map(|(idx, line)| {
            MemoryEntry::scored(line, default_memory_relevance_from_total(total, idx))
                .with_source("edge_profile.memory_section")
        })
        .collect()
}

fn default_memory_relevance_from_total(total: usize, idx: usize) -> f64 {
    total.saturating_sub(idx) as f64
}

/// Build TurnState from AgenticLoopState.
pub(crate) fn build_turn_state(state: &AgenticLoopState, user_content: &str) -> TurnState {
    TurnState {
        messages: state.messages.clone(),
        tool_results: vec![],
        tokens: TokenAccounting::from_fields(
            state.total_prompt,
            state.total_cache_read,
            state.total_cache_creation,
            state.total_completion,
        ),
        active_skills: vec![],
        recent_file_reads: std::collections::HashMap::new(),
        // Pull the real per-turn budget from the host state instead of a 20
        // hardcode. Planner uses this to decide when to escalate compaction.
        remaining_turns: state.remaining_turns as u32,
        turn_index: state.llm_rounds_completed,
        // RecoveryState lives on the pipeline session; feeding it freshly
        // here each turn is correct — `run_turn_adaptive` merges it with
        // the session's persisted counters before planning.
        recovery: RecoveryState::default(),
        last_user_message: user_content.to_string(),
    }
}

/// Build SessionContext from Host state.
///
/// The `provider` argument selects the cache policy: Anthropic-family (direct
/// Anthropic, Bedrock Claude) gets `cache_control` semantics; everyone else
/// falls back to prefix-only caching. Pulling this from the actual provider
/// (vs. a hardcoded `anthropic()`) is what prevents the pipeline from emitting
/// unsupported markers to OpenAI-compatible endpoints.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_session_context(
    session_id: &str,
    run_id: Option<&str>,
    model_name: &str,
    max_input_tokens: u64,
    edge_profile: &serde_json::Map<String, Value>,
    provider: &str,
    project_context: Option<&str>,
) -> SessionContext {
    let provider_policy = super::prompt_cache::provider_cache_policy_for(provider, model_name);
    SessionContext {
        session_id: session_id.to_string(),
        run_id: run_id.unwrap_or_default().to_string(),
        model_id: model_name.to_string(),
        model_limit: u32::try_from(max_input_tokens).unwrap_or(u32::MAX),
        provider_policy,
        provider_strategy: ProviderCacheStrategy::default(),
        // Cross-session project context (summaries of prior sessions on
        // this repo) is session-stable — feeding it through the pipeline's
        // `ProjectContext` section puts it in CacheScope::Session behind the
        // 2nd marker instead of runtime-injecting it into `state.messages`
        // AFTER the marker.
        project_context: project_context.unwrap_or("").to_string(),
        edge_profile: EdgeProfile {
            cwd: edge_profile
                .get("cwd")
                .and_then(Value::as_str)
                .map(String::from),
            git_branch: edge_profile
                .get("git_branch")
                .and_then(Value::as_str)
                .map(String::from),
            ..Default::default()
        },
        self_model: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn::agentic_loop_host::tests::make_state;

    #[test]
    fn turn_state_uses_real_remaining_turns_not_hardcoded_20() {
        // The adapter used to hardcode remaining_turns=20 regardless of the
        // actual host budget. That broke the planner's "about to exhaust
        // budget" heuristic and fed wrong signals into compaction escalation.
        let mut state = make_state();
        state.remaining_turns = 7;
        let ts = build_turn_state(&state, "hi");
        assert_eq!(
            ts.remaining_turns, 7,
            "adapter must pass host's remaining_turns through verbatim, \
             not hardcode a default"
        );

        state.remaining_turns = 0;
        let ts = build_turn_state(&state, "hi");
        assert_eq!(ts.remaining_turns, 0, "exhausted budget must surface as 0");
    }

    #[test]
    fn turn_state_tokens_reflect_host_state() {
        let mut state = make_state();
        state.total_prompt = 1000;
        state.total_cache_read = 800;
        state.total_cache_creation = 200;
        state.total_completion = 50;
        let ts = build_turn_state(&state, "user message");
        // TokenAccounting splits prompt_tokens across cache buckets — what
        // matters is the adapter passed the right inputs. Verify via total.
        assert!(
            ts.tokens.total_input() > 0,
            "token accounting must be populated from host fields"
        );
    }

    #[test]
    fn session_context_picks_anthropic_policy_for_anthropic_provider() {
        let ep = serde_json::Map::new();
        let ctx = build_session_context(
            "sid",
            None,
            "claude-sonnet",
            200_000,
            &ep,
            "anthropic",
            None,
        );
        // anthropic policy supports cache_control markers (max_markers > 0).
        assert!(
            ctx.provider_policy.max_markers > 0,
            "anthropic provider must get a policy that supports cache markers"
        );
        assert!(ctx.provider_policy.supports_global_scope);
    }

    #[test]
    fn session_context_picks_anthropic_policy_for_bedrock_provider() {
        let ep = serde_json::Map::new();
        let ctx = build_session_context(
            "sid",
            None,
            "anthropic.claude-sonnet-4-20250514-v1:0",
            200_000,
            &ep,
            "bedrock",
            None,
        );
        // Bedrock Claude translates cache_control → cachePoint downstream,
        // so the pipeline still emits Anthropic-style markers.
        assert!(
            ctx.provider_policy.max_markers > 0,
            "bedrock must use anthropic policy — Bedrock Converse translates cache_control \
             to cachePoint transparently"
        );
    }

    #[test]
    fn session_context_uses_prefix_policy_for_non_claude_bedrock_model() {
        let ep = serde_json::Map::new();
        let ctx = build_session_context(
            "sid",
            None,
            "amazon.titan-text-express-v1",
            200_000,
            &ep,
            "bedrock",
            None,
        );
        assert_eq!(
            ctx.provider_policy.max_markers, 0,
            "non-Claude Bedrock models must not receive Anthropic cache_control markers"
        );
        assert!(!ctx.provider_policy.supports_global_scope);
    }

    #[test]
    fn session_context_saturates_oversized_model_limit() {
        let ep = serde_json::Map::new();
        let ctx = build_session_context("sid", None, "gpt-4o", u64::MAX, &ep, "openai", None);
        assert_eq!(ctx.model_limit, u32::MAX);
    }

    #[test]
    fn session_context_picks_openai_policy_for_openai_provider() {
        let ep = serde_json::Map::new();
        let ctx = build_session_context("sid", None, "gpt-4o", 128_000, &ep, "openai", None);
        // OpenAI uses prefix-only caching — emitting cache_control is a no-op
        // at best and (for some proxies) a 400 Bad Request at worst.
        assert_eq!(
            ctx.provider_policy.max_markers, 0,
            "openai provider must get prefix-only policy — no cache_control markers"
        );
        assert!(!ctx.provider_policy.supports_global_scope);
    }

    #[test]
    fn session_context_defaults_unknown_provider_to_openai_policy() {
        // New providers (DeepSeek, Qwen, MiniMax) come through "openai"
        // protocol; fresh names we don't yet recognize must err on the side
        // of "no markers" — emitting cache_control to an unknown backend
        // risks protocol errors, whereas omitting it just loses caching.
        let ep = serde_json::Map::new();
        let ctx = build_session_context(
            "sid",
            None,
            "unknown-model-v9",
            100_000,
            &ep,
            "unknown",
            None,
        );
        assert_eq!(ctx.provider_policy.max_markers, 0);
    }

    #[test]
    fn session_context_passes_project_context_through() {
        // The Cross-Session Project Context (summaries of prior sessions on
        // this repo) must land in `SessionContext.project_context` so the
        // pipeline's `ProjectContext` section picks it up. Previously this
        // content was runtime-injected into `state.messages` AFTER the cache
        // marker — every turn re-sent it as cache_creation. Now it sits in
        // `CacheScope::Session` behind the 2nd marker and becomes cacheable.
        let ep = serde_json::Map::new();
        let ctx = build_session_context(
            "sid",
            None,
            "claude-sonnet",
            200_000,
            &ep,
            "anthropic",
            Some(
                "1. [active] (2026-05-06, 22 turns, branch: main)\n2. [active] (2026-05-05, 4 turns)",
            ),
        );
        assert!(
            ctx.project_context.contains("22 turns"),
            "project_context must flow through to SessionContext: {}",
            ctx.project_context
        );
    }

    #[test]
    fn session_context_defaults_project_context_to_empty_when_absent() {
        let ep = serde_json::Map::new();
        let ctx = build_session_context("sid", None, "gpt-4o", 128_000, &ep, "openai", None);
        assert!(ctx.project_context.is_empty());
    }

    #[test]
    fn external_sources_carries_prefetched_memory_from_edge_profile() {
        let mut ep = serde_json::Map::new();
        ep.insert(
            "memory_section".into(),
            Value::String("## User Memories\n- prefers Rust\n- hates emojis".into()),
        );
        let state = make_state();
        let sources = build_external_sources(&ep, &state, "hi", &["bash"], 0.8, None);
        assert!(
            !sources.memory_entries.is_empty(),
            "edge_profile.memory_section must flow into ExternalSources.memory_entries"
        );
        assert_eq!(sources.memory_entries.len(), 2);
        assert!(sources.memory_entries[0].content.contains("prefers Rust"));
        assert!(
            sources.memory_entries[0].relevance_score > sources.memory_entries[1].relevance_score,
            "memory_section line order should become production relevance"
        );
    }

    #[test]
    fn external_sources_prefers_structured_memory_entries() {
        let mut ep = serde_json::Map::new();
        ep.insert(
            "memory_section".into(),
            Value::String("## User Memories\nfallback should be ignored".into()),
        );
        ep.insert(
            "memory_entries".into(),
            Value::Array(vec![
                serde_json::json!({
                    "content": "fresh structured memory",
                    "relevance_score": 0.9,
                    "source": "test",
                    "token_estimate": 7,
                    "freshness_turn": 3
                }),
                Value::String("ranked fallback string".into()),
            ]),
        );
        let state = make_state();
        let sources = build_external_sources(&ep, &state, "hi", &["bash"], 0.8, None);

        assert_eq!(sources.memory_entries.len(), 2);
        assert_eq!(sources.memory_entries[0].content, "fresh structured memory");
        assert_eq!(sources.memory_entries[0].source.as_deref(), Some("test"));
        assert_eq!(sources.memory_entries[0].token_estimate, 7);
        assert_eq!(sources.memory_entries[0].freshness_turn, Some(3));
        assert!(
            sources.memory_entries[1]
                .content
                .contains("ranked fallback string")
        );
    }

    #[test]
    fn low_confidence_warning_is_volatile_tool_guidance() {
        let ep = serde_json::Map::new();
        let state = make_state();
        let sources = build_external_sources(&ep, &state, "hi", &["bash"], 0.1, None);

        // tool_conditional field removed — volatile content routes to extra_dynamic_sections only
        assert!(
            sources
                .tool_guidance
                .as_deref()
                .unwrap_or_default()
                .contains("Low-Confidence Tool Selection"),
            "low-confidence warning should route to RuntimeVolatile/tool_guidance"
        );
    }

    // `selected_tool_guidance_is_volatile` was deleted: it asserted on
    // `## Self-Model` + `Explicit Tool Requests` strings that originated
    // from the now-empty `self_model_section` / `tool_conditional_section`
    // bodies (commit a1187f76). The volatile-lane routing contract is
    // covered by the composite integration tests below.

    #[test]
    fn external_sources_empty_memory_when_edge_profile_has_none() {
        let ep = serde_json::Map::new();
        let state = make_state();
        let sources = build_external_sources(&ep, &state, "hi", &["bash"], 0.8, None);
        assert!(sources.memory_entries.is_empty());
    }

    // ── Composite integration tests ─────────────────────────────────────
    //
    // These drive the full adapter path — build_external_sources +
    // build_turn_state + build_session_context — into a real
    // PipelineSession::run_turn_adaptive and assert on the provider-facing
    // output. They guard the post-cutover boundary where the adapter is the
    // sole translator between AgenticLoopState and ContextSources: a
    // regression in adapter construction will otherwise show up only at
    // runtime as a silent cache break or a missing section.

    use astra_turn_core::pipeline_config::PipelineConfig;
    use astra_turn_core::pipeline_session::{AdaptiveTurnInput, PipelineSession};
    use astra_turn_core::section_types::{CacheScope, SectionKind};

    struct CompositeInputs {
        statics: astra_turn_core::context_sources::StaticSections,
        agent: astra_turn_core::context_sources::AgentContext,
        session: astra_turn_core::context_sources::SessionContext,
        turn: astra_turn_core::context_sources::TurnState,
        external: astra_turn_core::context_sources::ExternalSources,
    }

    /// Build all adapter inputs from a populated state + edge profile.
    /// Mirrors the ServerAgenticLoopHost call sequence so regressions in any
    /// one builder surface as a failure here.
    fn build_composite_inputs(
        state: &AgenticLoopState,
        edge_profile: &serde_json::Map<String, Value>,
        provider: &str,
        model_name: &str,
        user_content: &str,
    ) -> CompositeInputs {
        let statics = crate::prompts::build_pipeline_static_sections();
        let agent = astra_turn_core::context_sources::AgentContext {
            tool_schemas: vec![serde_json::json!({
                "type": "function",
                "function": {"name": "bash", "description": "Run a shell command"}
            })],
            ..Default::default()
        };
        let session = build_session_context(
            "composite-sess",
            None,
            model_name,
            200_000,
            edge_profile,
            provider,
            None,
        );
        let turn = build_turn_state(state, user_content);
        let external =
            build_external_sources(edge_profile, state, user_content, &["bash"], 0.8, None);
        CompositeInputs {
            statics,
            agent,
            session,
            turn,
            external,
        }
    }

    #[test]
    fn composite_anthropic_full_path_produces_expected_section_order() {
        // Guards the full cutover contract: adapter-built inputs fed through
        // PipelineSession must emit the canonical section sequence. Drift
        // silently invalidates the Anthropic prompt-cache prefix.
        let mut ep = serde_json::Map::new();
        ep.insert("cwd".into(), Value::String("/tmp/proj".into()));
        ep.insert("git_branch".into(), Value::String("main".into()));
        ep.insert(
            "memory_section".into(),
            Value::String("## User Memories\n- prefers terse answers".into()),
        );
        let state = make_state();
        let ci = build_composite_inputs(&state, &ep, "anthropic", "claude-sonnet-4-6", "hello");

        let sess = PipelineSession::new(PipelineConfig {
            provider_policy: ci.session.provider_policy.clone(),
        });
        let output = sess
            .run_turn_adaptive(AdaptiveTurnInput {
                statics: &ci.statics,
                agent: &ci.agent,
                session: &ci.session,
                turn: &ci.turn,
                external: &ci.external,
                model_id: "claude-sonnet-4-6",
                query_source: "agentic_loop",
            })
            .expect("adapter-built inputs must not abort the pipeline");

        let kinds: Vec<SectionKind> = output.plan.sections.iter().map(|s| s.kind).collect();
        // The planner owns the canonical order; this assertion proves the
        // adapter didn't accidentally drop or reorder a section via missing
        // external fields.
        assert!(kinds.first() == Some(&SectionKind::Identity));
        assert!(kinds.contains(&SectionKind::Constraints));
        assert!(kinds.contains(&SectionKind::RuntimeIdentity));
        assert!(kinds.contains(&SectionKind::RuntimeVolatile));
        assert!(kinds.contains(&SectionKind::Memory));
        let identity_pos = kinds
            .iter()
            .position(|k| *k == SectionKind::Identity)
            .unwrap();
        let runtime_id_pos = kinds
            .iter()
            .position(|k| *k == SectionKind::RuntimeIdentity)
            .unwrap();
        let volatile_pos = kinds
            .iter()
            .position(|k| *k == SectionKind::RuntimeVolatile)
            .unwrap();
        assert!(identity_pos < runtime_id_pos);
        assert!(runtime_id_pos < volatile_pos);
    }

    #[test]
    fn composite_anthropic_full_path_places_cache_markers_at_scope_boundaries() {
        // Cache markers must land at Global→Session and Session→None
        // transitions. The planner assigns scopes; the optimizer places
        // markers; the serializer remaps them to block indices. This test
        // verifies the chain end-to-end with adapter-built inputs.
        let mut ep = serde_json::Map::new();
        ep.insert("cwd".into(), Value::String("/tmp/proj".into()));
        let state = make_state();
        let ci = build_composite_inputs(&state, &ep, "anthropic", "claude-sonnet-4-6", "hello");

        let sess = PipelineSession::new(PipelineConfig {
            provider_policy: ci.session.provider_policy.clone(),
        });
        let output = sess
            .run_turn_adaptive(AdaptiveTurnInput {
                statics: &ci.statics,
                agent: &ci.agent,
                session: &ci.session,
                turn: &ci.turn,
                external: &ci.external,
                model_id: "claude-sonnet-4-6",
                query_source: "agentic_loop",
            })
            .expect("adapter-built inputs must not abort the pipeline");

        assert!(
            !output.serialized.cache_markers.is_empty(),
            "anthropic policy must emit cache markers for adapter-built inputs"
        );
        // Every marker must point at a block that exists and whose scope is
        // strictly more stable than the next block (or is the last block).
        for marker in &output.serialized.cache_markers {
            let block = output
                .serialized
                .system_blocks
                .get(marker.after_section_index)
                .expect("marker must reference a real block index");
            assert!(
                block.cache_control.is_some(),
                "block at marker position must carry cache_control"
            );
            assert!(
                matches!(block.scope, CacheScope::Global | CacheScope::Session),
                "markers must land on Global or Session-scope blocks, not {:?}",
                block.scope
            );
        }
    }

    #[test]
    fn composite_openai_full_path_emits_zero_cache_markers() {
        // OpenAI uses prefix-only caching; emitting cache_control is a no-op
        // at best and a 400 at worst. The adapter+pipeline must agree on
        // "no markers" for non-Anthropic providers.
        let ep = serde_json::Map::new();
        let state = make_state();
        let ci = build_composite_inputs(&state, &ep, "openai", "gpt-4o", "hello");

        let sess = PipelineSession::new(PipelineConfig {
            provider_policy: ci.session.provider_policy.clone(),
        });
        let output = sess
            .run_turn_adaptive(AdaptiveTurnInput {
                statics: &ci.statics,
                agent: &ci.agent,
                session: &ci.session,
                turn: &ci.turn,
                external: &ci.external,
                model_id: "gpt-4o",
                query_source: "agentic_loop",
            })
            .expect("adapter-built inputs must not abort the pipeline");

        assert!(
            output.serialized.cache_markers.is_empty(),
            "openai policy must not emit any cache markers"
        );
        for block in &output.serialized.system_blocks {
            assert!(
                block.cache_control.is_none(),
                "openai blocks must not carry cache_control"
            );
        }
    }

    #[test]
    fn composite_adapter_runtime_identity_sits_in_session_scope() {
        // The cache-locality contract: dynamic-but-session-stable content
        // Session-stable signals (cwd/git_branch typed fields,
        // environment_static via extra_stable_sections, system_override)
        // must land in CacheScope::Session. The adapter packs these into
        // ExternalSources; the binder stitches them into RuntimeIdentity.
        // If adapter output drifts back to CacheScope::None, the 2nd cache
        // marker misses these bytes.
        let mut ep = serde_json::Map::new();
        ep.insert("cwd".into(), Value::String("/tmp/proj".into()));
        ep.insert("git_branch".into(), Value::String("main".into()));
        let state = make_state();
        let ci = build_composite_inputs(&state, &ep, "anthropic", "claude-sonnet-4-6", "hello");

        let sess = PipelineSession::new(PipelineConfig {
            provider_policy: ci.session.provider_policy.clone(),
        });
        let output = sess
            .run_turn_adaptive(AdaptiveTurnInput {
                statics: &ci.statics,
                agent: &ci.agent,
                session: &ci.session,
                turn: &ci.turn,
                external: &ci.external,
                model_id: "claude-sonnet-4-6",
                query_source: "agentic_loop",
            })
            .expect("adapter-built inputs must not abort the pipeline");

        let runtime_id_block = output
            .serialized
            .system_blocks
            .iter()
            .find(|b| b.kind == SectionKind::RuntimeIdentity);
        if let Some(block) = runtime_id_block {
            assert_eq!(
                block.scope,
                CacheScope::Session,
                "RuntimeIdentity must be Session-scoped so the 2nd marker catches it"
            );
            assert!(
                block.text.contains("/tmp/proj") || block.text.contains("main"),
                "RuntimeIdentity should carry adapter-supplied cwd/branch: got {:?}",
                block.text
            );
        }

        // The Session→None boundary must be observed: the last Session block
        // must precede the first None block.
        let last_session = output
            .serialized
            .system_blocks
            .iter()
            .rposition(|b| b.scope == CacheScope::Session);
        let first_none = output
            .serialized
            .system_blocks
            .iter()
            .position(|b| b.scope == CacheScope::None);
        if let (Some(last), Some(first)) = (last_session, first_none) {
            assert!(
                last < first,
                "Session-scope must precede None-scope end-to-end — \
                 adapter leaked turn-volatile content into cached prefix"
            );
        }
    }

    #[test]
    fn composite_memory_section_appears_iff_edge_profile_has_memory() {
        // With memory present → Memory section is planned and bound.
        // Without memory → Memory section is absent (planner skips it so
        // the optimizer doesn't emit an empty block that displaces other
        // sections inside the budget allocator).
        let state = make_state();

        let mut ep_with = serde_json::Map::new();
        ep_with.insert(
            "memory_section".into(),
            Value::String("## User Memories\n- uses rustfmt".into()),
        );
        let ci = build_composite_inputs(&state, &ep_with, "anthropic", "claude-sonnet-4-6", "hi");
        let sess = PipelineSession::new(PipelineConfig {
            provider_policy: ci.session.provider_policy.clone(),
        });
        let out_with = sess
            .run_turn_adaptive(AdaptiveTurnInput {
                statics: &ci.statics,
                agent: &ci.agent,
                session: &ci.session,
                turn: &ci.turn,
                external: &ci.external,
                model_id: "claude-sonnet-4-6",
                query_source: "agentic_loop",
            })
            .expect("adapter must not abort");
        assert!(
            out_with
                .plan
                .sections
                .iter()
                .any(|s| s.kind == SectionKind::Memory),
            "Memory section must be planned when edge_profile carries memory"
        );

        let ep_without = serde_json::Map::new();
        let ci2 =
            build_composite_inputs(&state, &ep_without, "anthropic", "claude-sonnet-4-6", "hi");
        let sess2 = PipelineSession::new(PipelineConfig {
            provider_policy: ci2.session.provider_policy.clone(),
        });
        let out_without = sess2
            .run_turn_adaptive(AdaptiveTurnInput {
                statics: &ci2.statics,
                agent: &ci2.agent,
                session: &ci2.session,
                turn: &ci2.turn,
                external: &ci2.external,
                model_id: "claude-sonnet-4-6",
                query_source: "agentic_loop",
            })
            .expect("adapter must not abort");
        assert!(
            !out_without
                .plan
                .sections
                .iter()
                .any(|s| s.kind == SectionKind::Memory),
            "Memory section must be skipped when edge_profile has no memory"
        );
    }
}

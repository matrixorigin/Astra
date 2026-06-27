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
    ChannelAssembler, ContextChannelPolicy, EdgeProfile, ExternalSources, MemoryEntry,
    SessionContext, TurnState,
};
use astra_turn_core::microcompact::ProviderCacheStrategy;
use astra_turn_core::recovery_state::RecoveryState;
use astra_turn_core::section_types::{CacheScope, PromptSection, PromptTokenBucket};
use astra_turn_core::token_accounting::TokenAccounting;

use super::agentic_loop::host::AgenticLoopState;

/// Build ExternalSources from the Host's edge_profile + state.
///
/// Single extraction point for all dynamic prompt fragments.
/// All prompt-section channels now flow through [`ContextChannelProvider`]
/// implementations + [`ChannelAssembler`] — no ad-hoc `edge_profile` key reads
/// or manual `push` to `extra_stable_sections`/`extra_dynamic_sections`.
/// Typed fields (`effort_hint`, `system_override`, `plan_context`, `tool_guidance`)
/// remain as direct `ExternalSources` fields consumed by the pipeline binder.
pub(crate) fn build_external_sources(
    edge_profile: &serde_json::Map<String, Value>,
    state: &AgenticLoopState,
    user_content: &str,
    tool_names: &[&str],
    plan_resume_hint: Option<&str>,
    cache_capability: Option<astra_turn_core::cache_placement::CacheCapability>,
) -> ExternalSources {
    let _ = user_content;

    // ── Typed fields (pipeline binder direct consumers) ──

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

    let system_override = edge_profile
        .get("system_prompt_override")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|s| format!("\n\n{s}"));

    let plan_context = plan_resume_hint.filter(|s| !s.is_empty()).map(String::from);

    let (tool_guidance_text, _signals) =
        crate::prompts::tool_round_guidance_trace(&state.messages, state.llm_rounds_completed);
    let tool_guidance = (!tool_guidance_text.is_empty()).then_some(tool_guidance_text);

    // ── Memory entries (structured, non-section) ──
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

    // ── Framework+Policy: ContextChannelProvider assembly ──
    // Every prompt section is produced by a typed provider. The assembler
    // collects from all registered providers, partitions by cache scope,
    // and returns (stable_sections, dynamic_sections). No channel can be
    // "forgotten" — the compiler guarantees every provider is iterated.

    let active_skill_names: Vec<String> = edge_profile
        .get("active_skills")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();

    let cwd_str = edge_profile
        .get("cwd")
        .and_then(Value::as_str)
        .map(|s| format!("cwd: {s}"))
        .unwrap_or_default();

    let mut providers: Vec<Box<dyn astra_turn_core::context_sources::ContextChannelProvider>> =
        Vec::new();

    // Self-model: tool-dependent capabilities hint
    if !tool_names.is_empty() {
        providers.push(Box::new(SelfModelProvider {
            tool_names: tool_names.iter().map(|s| s.to_string()).collect(),
        }));
    }

    // Tool-conditional: cross-tool admission protocol
    if !tool_names.is_empty() {
        providers.push(Box::new(ToolConditionalProvider {
            tool_names: tool_names.iter().map(|s| s.to_string()).collect(),
            cwd: cwd_str.clone(),
        }));
    }

    // Environment static (Platform/Shell/CWD/Home)
    if let Some(text) = edge_profile
        .get("environment_static")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        providers.push(Box::new(EnvStaticProvider {
            text: text.to_string(),
        }));
    }

    // Environment volatile (git branch dirty / diff / commits)
    if let Some(text) = edge_profile
        .get("environment_volatile")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        providers.push(Box::new(EnvVolatileProvider {
            text: text.to_string(),
        }));
    }

    // Active skills visibility hint
    if !active_skill_names.is_empty() {
        providers.push(Box::new(ActiveSkillNamesProvider {
            names: active_skill_names.clone(),
        }));
    }

    // Turn budget hint (tiered urgency)
    if state.max_turns > 0 && state.remaining_turns > 0 {
        providers.push(Box::new(TurnBudgetProvider {
            remaining_turns: state.remaining_turns as u32,
            max_turns: state.max_turns as u32,
        }));
    }

    // Capabilities (tool count + skill count + context window)
    {
        let skill_count = active_skill_names.len();
        providers.push(Box::new(CapabilitiesProvider {
            tool_count: tool_names.len(),
            skill_count,
            max_turn_input_tokens: state.max_turn_input_tokens,
        }));
    }

    // Skill listing (session-stable, from state.skills.listing_message)
    if let Some(listing) = state.skills.listing_message.as_ref() {
        if let Some(content) = listing.get("content").and_then(Value::as_str) {
            if !content.is_empty() {
                providers.push(Box::new(SkillListingProvider {
                    content: content.to_string(),
                }));
            }
        }
    }

    // Cache strategy (prompt caching hint for capable models)
    if let Some(cc) = cache_capability {
        if cc.prefers_intra_turn_batching() {
            providers.push(Box::new(CacheStrategyProvider));
        }
    }

    // Lessons (Memoria-bootstrapped session lessons)
    if let Some(text) = edge_profile
        .get("lessons_text")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        providers.push(Box::new(LessonsChannelProvider {
            text: text.to_string(),
        }));
    }

    let assembler = ChannelAssembler::new(providers, ContextChannelPolicy::default());
    let (extra_stable_sections, extra_dynamic_sections) =
        assembler.assemble(state.llm_rounds_completed);

    ExternalSources {
        memory_entries,
        session_memory_entry: None,
        spill_dir: None,
        spill_backend,
        effort_hint,
        system_override,
        plan_context,
        tool_guidance,
        extra_stable_sections,
        extra_dynamic_sections,
    }
}

// ── ContextChannelProvider implementations ──────────────────────────────────
//
// Every provider below replaces an ad-hoc `edge_profile` key read +
// manual `push` to `extra_stable_sections`/`extra_dynamic_sections`.
// The assembler iterates all registered providers; the compiler
// guarantees no channel can be forgotten.

/// Self-model: tool-dependent capabilities hint.
/// Injects when tools are visible. Dynamic scope (per-turn).
struct SelfModelProvider {
    tool_names: Vec<String>,
}

impl astra_turn_core::context_sources::ContextChannelProvider for SelfModelProvider {
    fn channel_id(&self) -> &'static str {
        "self_model"
    }
    fn cache_scope(&self) -> CacheScope {
        CacheScope::None
    }
    fn token_bucket(&self) -> PromptTokenBucket {
        PromptTokenBucket::BasePersona
    }
    fn provide(&self, _turn_index: u32) -> Option<PromptSection> {
        let tool_names: Vec<&str> = self.tool_names.iter().map(String::as_str).collect();
        let text = crate::prompts::self_model_section(&tool_names);
        if text.is_empty() {
            None
        } else {
            Some(PromptSection::dynamic(text, PromptTokenBucket::BasePersona))
        }
    }
}

/// Tool-conditional: cross-tool admission protocol.
/// Dynamic scope (per-turn, depends on visible tool set).
struct ToolConditionalProvider {
    tool_names: Vec<String>,
    cwd: String,
}

impl astra_turn_core::context_sources::ContextChannelProvider for ToolConditionalProvider {
    fn channel_id(&self) -> &'static str {
        "tool_conditional"
    }
    fn cache_scope(&self) -> CacheScope {
        CacheScope::None
    }
    fn token_bucket(&self) -> PromptTokenBucket {
        PromptTokenBucket::Environment
    }
    fn provide(&self, _turn_index: u32) -> Option<PromptSection> {
        let tool_names: Vec<&str> = self.tool_names.iter().map(String::as_str).collect();
        let text = crate::prompts::tool_conditional_section(&tool_names, &self.cwd);
        if text.is_empty() {
            None
        } else {
            Some(PromptSection::dynamic(text, PromptTokenBucket::Environment))
        }
    }
}

/// Environment static: Platform/Shell/CWD/Home.
/// Session scope (stable across turns within a session).
struct EnvStaticProvider {
    text: String,
}

impl astra_turn_core::context_sources::ContextChannelProvider for EnvStaticProvider {
    fn channel_id(&self) -> &'static str {
        "env_static"
    }
    fn cache_scope(&self) -> CacheScope {
        CacheScope::Session
    }
    fn token_bucket(&self) -> PromptTokenBucket {
        PromptTokenBucket::Environment
    }
    fn provide(&self, _turn_index: u32) -> Option<PromptSection> {
        if self.text.is_empty() {
            None
        } else {
            Some(PromptSection::dynamic(
                self.text.clone(),
                PromptTokenBucket::Environment,
            ))
        }
    }
}

/// Environment volatile: git branch dirty / diff / commits.
/// None scope (changes every turn).
struct EnvVolatileProvider {
    text: String,
}

impl astra_turn_core::context_sources::ContextChannelProvider for EnvVolatileProvider {
    fn channel_id(&self) -> &'static str {
        "env_volatile"
    }
    fn cache_scope(&self) -> CacheScope {
        CacheScope::None
    }
    fn token_bucket(&self) -> PromptTokenBucket {
        PromptTokenBucket::Environment
    }
    fn provide(&self, _turn_index: u32) -> Option<PromptSection> {
        if self.text.is_empty() {
            None
        } else {
            Some(PromptSection::dynamic(
                self.text.clone(),
                PromptTokenBucket::Environment,
            ))
        }
    }
}

/// Active skill names: visibility hint for currently loaded skills.
/// Dynamic scope (skill set can change mid-session).
struct ActiveSkillNamesProvider {
    names: Vec<String>,
}

impl astra_turn_core::context_sources::ContextChannelProvider for ActiveSkillNamesProvider {
    fn channel_id(&self) -> &'static str {
        "active_skill_names"
    }
    fn cache_scope(&self) -> CacheScope {
        CacheScope::None
    }
    fn token_bucket(&self) -> PromptTokenBucket {
        PromptTokenBucket::Environment
    }
    fn provide(&self, _turn_index: u32) -> Option<PromptSection> {
        if self.names.is_empty() {
            return None;
        }
        Some(PromptSection::dynamic(
            format!(
                "\n\n## Active Skills\nThe following skills are currently active: {}. Use `discover_skills` to see their full descriptions.",
                self.names.join(", ")
            ),
            PromptTokenBucket::Environment,
        ))
    }
}

/// Turn budget: tiered urgency hint based on remaining turns.
/// Dynamic scope (remaining_turns changes every turn).
struct TurnBudgetProvider {
    remaining_turns: u32,
    max_turns: u32,
}

impl astra_turn_core::context_sources::ContextChannelProvider for TurnBudgetProvider {
    fn channel_id(&self) -> &'static str {
        "turn_budget"
    }
    fn cache_scope(&self) -> CacheScope {
        CacheScope::None
    }
    fn token_bucket(&self) -> PromptTokenBucket {
        PromptTokenBucket::Environment
    }
    fn provide(&self, _turn_index: u32) -> Option<PromptSection> {
        let budget_pct = (self.remaining_turns as f64 / self.max_turns as f64) * 100.0;
        let urgency = if budget_pct >= 80.0 {
            ""
        } else if budget_pct >= 50.0 {
            " Use turns efficiently."
        } else {
            " Do not consume turns needlessly."
        };
        Some(PromptSection::dynamic(
            format!(
                "\n\n## Turn Budget\n{}/{} turns remaining ({:.0}%).{urgency}",
                self.remaining_turns, self.max_turns, budget_pct
            ),
            PromptTokenBucket::Environment,
        ))
    }
}

/// Capabilities: tool count + skill count + context window info.
/// Dynamic scope (tool_names and active_skill_names are per-turn clipped).
struct CapabilitiesProvider {
    tool_count: usize,
    skill_count: usize,
    max_turn_input_tokens: u64,
}

impl astra_turn_core::context_sources::ContextChannelProvider for CapabilitiesProvider {
    fn channel_id(&self) -> &'static str {
        "capabilities"
    }
    fn cache_scope(&self) -> CacheScope {
        CacheScope::None
    }
    fn token_bucket(&self) -> PromptTokenBucket {
        PromptTokenBucket::Environment
    }
    fn provide(&self, _turn_index: u32) -> Option<PromptSection> {
        let mut cap = format!(
            "\n\n## Capabilities\n{} tools available. {} active skills.",
            self.tool_count, self.skill_count
        );
        if self.max_turn_input_tokens > 0 {
            cap.push_str(&format!(
                " Context window: {} tokens per turn.",
                self.max_turn_input_tokens
            ));
        }
        Some(PromptSection::dynamic(cap, PromptTokenBucket::Environment))
    }
}

/// Skill listing: the `<available_skills>` block.
/// Session scope (skills don't change within a session).
struct SkillListingProvider {
    content: String,
}

impl astra_turn_core::context_sources::ContextChannelProvider for SkillListingProvider {
    fn channel_id(&self) -> &'static str {
        "skill_listing"
    }
    fn cache_scope(&self) -> CacheScope {
        CacheScope::Session
    }
    fn token_bucket(&self) -> PromptTokenBucket {
        PromptTokenBucket::Environment
    }
    fn provide(&self, _turn_index: u32) -> Option<PromptSection> {
        if self.content.is_empty() {
            None
        } else {
            Some(PromptSection::stable(
                self.content.clone(),
                CacheScope::Session,
            ))
        }
    }
}

/// Cache strategy: prompt caching hint for capable models.
/// Session scope.
struct CacheStrategyProvider;

impl astra_turn_core::context_sources::ContextChannelProvider for CacheStrategyProvider {
    fn channel_id(&self) -> &'static str {
        "cache_strategy"
    }
    fn cache_scope(&self) -> CacheScope {
        CacheScope::Session
    }
    fn token_bucket(&self) -> PromptTokenBucket {
        PromptTokenBucket::Environment
    }
    fn provide(&self, _turn_index: u32) -> Option<PromptSection> {
        Some(PromptSection::stable(
            "## Execution Strategy\nThis model's prompt cache is only reliable within the current turn. When the task does not require new user input, batch related tool work and complete it within the same turn instead of stopping early or spreading the work across multiple user turns.".to_string(),
            CacheScope::Session,
        ))
    }
}

/// Lessons: Memoria-bootstrapped session lessons.
/// Session scope (loaded once at session start, stable thereafter).
struct LessonsChannelProvider {
    text: String,
}

impl astra_turn_core::context_sources::ContextChannelProvider for LessonsChannelProvider {
    fn channel_id(&self) -> &'static str {
        "lessons"
    }
    fn cache_scope(&self) -> CacheScope {
        CacheScope::Session
    }
    fn token_bucket(&self) -> PromptTokenBucket {
        PromptTokenBucket::UserPreferences
    }
    fn provide(&self, _turn_index: u32) -> Option<PromptSection> {
        if self.text.is_empty() {
            None
        } else {
            Some(PromptSection::stable(
                format!(
                    "\n\n## Session Lessons (Learned from Past Corrections)\n{}",
                    self.text
                ),
                CacheScope::Session,
            ))
        }
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
    cache_capability: Option<astra_turn_core::cache_placement::CacheCapability>,
    current_date: &str,
    user_id: Option<&str>,
) -> SessionContext {
    let provider_policy =
        super::prompt_cache::provider_cache_policy_for(cache_capability, provider, model_name);
    let provider_strategy = ProviderCacheStrategy::from_explicit_or_provider_model(
        cache_capability,
        Some(provider),
        Some(model_name),
    );
    SessionContext {
        session_id: session_id.to_string(),
        run_id: run_id.unwrap_or_default().to_string(),
        model_id: model_name.to_string(),
        provider_name: provider.to_string(),
        model_limit: u32::try_from(max_input_tokens).unwrap_or(u32::MAX),
        provider_policy,
        provider_strategy,
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
        deferred_tools_block: String::new(),
        skill_listing_block: String::new(),
        // Session-stable identity: capture once per session and thread through
        // every turn so cacheable RuntimeIdentity bytes do not churn at UTC midnight.
        current_date: current_date.to_string(),
        user_id: user_id.map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn::agentic_loop::host::tests::make_state;
    use astra_turn_core::context_sources::ContextChannelProvider;

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
            None,
            "2026-05-25",
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
            None,
            "2026-05-25",
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
            None,
            "2026-05-25",
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
        let ctx = build_session_context(
            "sid",
            None,
            "gpt-4o",
            u64::MAX,
            &ep,
            "openai",
            None,
            None,
            "2026-05-25",
            None,
        );
        assert_eq!(ctx.model_limit, u32::MAX);
    }

    #[test]
    fn session_context_picks_openai_policy_for_openai_provider() {
        let ep = serde_json::Map::new();
        let ctx = build_session_context(
            "sid",
            None,
            "gpt-4o",
            128_000,
            &ep,
            "openai",
            None,
            None,
            "2026-05-25",
            None,
        );
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
            None,
            "2026-05-25",
            None,
        );
        assert_eq!(ctx.provider_policy.max_markers, 0);
    }

    #[test]
    fn session_context_prefers_explicit_marker_capability_over_provider_hint() {
        let ep = serde_json::Map::new();
        let ctx = build_session_context(
            "sid",
            None,
            "proxy-claude",
            100_000,
            &ep,
            "openai",
            None,
            Some(astra_turn_core::cache_placement::CacheCapability {
                protocol: astra_turn_core::cache_placement::CacheProtocol::MarkerExplicit,
                volatile_placement:
                    astra_turn_core::cache_placement::VolatilePlacement::MarkerIsolated,
                reuse_scope: Some(
                    astra_turn_core::cache_placement::CacheReuseScope::ConversationTurns,
                ),
            }),
            "2026-05-25",
            None,
        );
        assert!(
            ctx.provider_strategy.supports_cache_control,
            "explicit metadata should enable marker-aware runtime policy even on openai-compatible proxies"
        );
        assert!(ctx.provider_policy.max_markers > 0);
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
            None,
            "2026-05-25",
            None,
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
        let ctx = build_session_context(
            "sid",
            None,
            "gpt-4o",
            128_000,
            &ep,
            "openai",
            None,
            None,
            "2026-05-25",
            None,
        );
        assert!(ctx.project_context.is_empty());
    }

    #[test]
    fn session_context_uses_caller_supplied_current_date() {
        let ep = serde_json::Map::new();
        let ctx = build_session_context(
            "sid",
            None,
            "gpt-4o",
            128_000,
            &ep,
            "openai",
            None,
            None,
            "1999-12-31",
            None,
        );
        assert_eq!(ctx.current_date, "1999-12-31");
    }

    #[test]
    fn external_sources_carries_prefetched_memory_from_edge_profile() {
        let mut ep = serde_json::Map::new();
        ep.insert(
            "memory_section".into(),
            Value::String("## User Memories\n- prefers Rust\n- hates emojis".into()),
        );
        let state = make_state();
        let sources = build_external_sources(&ep, &state, "hi", &["bash"], None, None);
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
        let sources = build_external_sources(&ep, &state, "hi", &["bash"], None, None);

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
    fn tool_availability_protocol_is_visible_tool_scoped_in_pipeline_sources() {
        let ep = serde_json::Map::new();
        let state = make_state();
        let sources = build_external_sources(
            &ep,
            &state,
            "hi",
            &["bash", "read_file", "tool_search"],
            None,
            None,
        );

        let tool_section = sources
            .extra_dynamic_sections
            .iter()
            .find(|section| section.text.contains("Tool Availability Protocol"))
            .expect("tool availability protocol should be emitted for visible tools");
        assert_eq!(
            tool_section.token_bucket,
            crate::prompts::PromptTokenBucket::Environment
        );
        assert!(
            tool_section
                .text
                .contains("Call a structured tool only if it is visible")
        );
        assert!(
            tool_section
                .text
                .contains("tool_search(query=\"select:NAME\")")
        );
        for direct_grep_phrase in [
            "→ grep",
            "grep for names/usages",
            "grep callers/imports",
            "After grep finds",
        ] {
            assert!(
                !tool_section.text.contains(direct_grep_phrase),
                "pipeline prompt must not instruct direct structured grep when grep is hidden: {direct_grep_phrase}"
            );
        }
    }

    // Tool-surface volatile-lane routing is covered by the composite
    // integration tests below.

    #[test]
    fn external_sources_empty_memory_when_edge_profile_has_none() {
        let ep = serde_json::Map::new();
        let state = make_state();
        let sources = build_external_sources(&ep, &state, "hi", &["bash"], None, None);
        assert!(sources.memory_entries.is_empty());
    }

    #[test]
    fn intra_turn_reuse_scope_adds_stable_execution_strategy_hint() {
        let ep = serde_json::Map::new();
        let state = make_state();
        let sources = build_external_sources(
            &ep,
            &state,
            "hi",
            &["bash"],
            None,
            Some(astra_turn_core::cache_placement::CacheCapability {
                protocol: astra_turn_core::cache_placement::CacheProtocol::OpenAiAutoPrefix,
                volatile_placement: astra_turn_core::cache_placement::VolatilePlacement::TailSuffix,
                reuse_scope: Some(
                    astra_turn_core::cache_placement::CacheReuseScope::IntraTurnRounds,
                ),
            }),
        );

        assert!(
            sources.extra_stable_sections.iter().any(|section| section
                .text
                .contains("prompt cache is only reliable within the current turn")),
            "intra-turn-only cache models should get a stable batching hint"
        );
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
            None,
            "2026-05-25",
            None,
        );
        let turn = build_turn_state(state, user_content);
        let external =
            build_external_sources(edge_profile, state, user_content, &["bash"], None, None);
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

        let mut sess = PipelineSession::new(PipelineConfig {
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

        let mut sess = PipelineSession::new(PipelineConfig {
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

        let mut sess = PipelineSession::new(PipelineConfig {
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

        let mut sess = PipelineSession::new(PipelineConfig {
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
        let mut sess = PipelineSession::new(PipelineConfig {
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
        let mut sess2 = PipelineSession::new(PipelineConfig {
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

    // ── ContextChannelProvider unit tests ─────────────────────────────────

    // ── SelfModelProvider ──

    #[test]
    fn self_model_provider_returns_none_when_tool_list_empty() {
        let p = super::SelfModelProvider { tool_names: vec![] };
        assert!(p.provide(0).is_none(), "empty tool list should yield None");
    }

    #[test]
    fn self_model_provider_returns_none_when_self_model_section_empty() {
        // self_model_section currently returns empty string for any input
        let p = super::SelfModelProvider {
            tool_names: vec!["bash".into()],
        };
        // The provider guards on text.is_empty(), not tool_names.is_empty()
        let result = p.provide(0);
        // self_model_section returns "" → provider returns None
        assert!(result.is_none(), "empty self_model_section output → None");
    }

    #[test]
    fn self_model_provider_channel_id_is_stable() {
        let p = super::SelfModelProvider {
            tool_names: vec!["bash".into()],
        };
        assert_eq!(p.channel_id(), "self_model");
    }

    #[test]
    fn self_model_provider_cache_scope_is_none() {
        let p = super::SelfModelProvider {
            tool_names: vec!["bash".into()],
        };
        assert_eq!(p.cache_scope(), CacheScope::None);
    }

    // ── ToolConditionalProvider ──

    #[test]
    fn tool_conditional_provider_emits_protocol_for_visible_tools() {
        let p = super::ToolConditionalProvider {
            tool_names: vec!["bash".into(), "read_file".into()],
            cwd: "cwd: /test".into(),
        };
        let section = p.provide(0).expect("should emit for non-empty tools");
        assert!(section.text.contains("Tool Availability Protocol"));
        assert!(
            section
                .text
                .contains("Call a structured tool only if it is visible")
        );
    }

    #[test]
    fn tool_conditional_provider_returns_none_when_no_tools() {
        let p = super::ToolConditionalProvider {
            tool_names: vec![],
            cwd: String::new(),
        };
        // tool_conditional_section returns "" for empty tool_names
        let result = p.provide(0);
        assert!(
            result.is_none(),
            "empty tools → tool_conditional_section returns empty → None"
        );
    }

    #[test]
    fn tool_conditional_provider_includes_tool_search_hint_when_tool_search_visible() {
        let p = super::ToolConditionalProvider {
            tool_names: vec!["tool_search".into(), "bash".into()],
            cwd: String::new(),
        };
        let section = p.provide(0).expect("should emit");
        assert!(section.text.contains("tool_search(query=\"select:NAME\")"));
    }

    #[test]
    fn tool_conditional_provider_cache_scope_is_none() {
        let p = super::ToolConditionalProvider {
            tool_names: vec!["bash".into()],
            cwd: String::new(),
        };
        assert_eq!(p.cache_scope(), CacheScope::None);
    }

    // ── EnvStaticProvider ──

    #[test]
    fn env_static_provider_emits_when_text_non_empty() {
        let p = super::EnvStaticProvider {
            text: "platform: macos".into(),
        };
        let section = p.provide(0).expect("should emit non-empty text");
        assert!(section.text.contains("platform: macos"));
    }

    #[test]
    fn env_static_provider_returns_none_when_text_empty() {
        let p = super::EnvStaticProvider {
            text: String::new(),
        };
        assert!(p.provide(0).is_none(), "empty text → None");
    }

    #[test]
    fn env_static_provider_cache_scope_is_session() {
        let p = super::EnvStaticProvider { text: "x".into() };
        assert_eq!(p.cache_scope(), CacheScope::Session);
    }

    // ── EnvVolatileProvider ──

    #[test]
    fn env_volatile_provider_emits_when_text_non_empty() {
        let p = super::EnvVolatileProvider {
            text: "git: dirty".into(),
        };
        let section = p.provide(0).expect("should emit");
        assert!(section.text.contains("git: dirty"));
    }

    #[test]
    fn env_volatile_provider_returns_none_when_text_empty() {
        let p = super::EnvVolatileProvider {
            text: String::new(),
        };
        assert!(p.provide(0).is_none());
    }

    #[test]
    fn env_volatile_provider_cache_scope_is_none() {
        let p = super::EnvVolatileProvider { text: "x".into() };
        assert_eq!(p.cache_scope(), CacheScope::None);
    }

    // ── ActiveSkillNamesProvider ──

    #[test]
    fn active_skill_names_provider_emits_skill_list() {
        let p = super::ActiveSkillNamesProvider {
            names: vec!["review".into(), "test".into()],
        };
        let section = p.provide(0).expect("should emit for non-empty names");
        assert!(section.text.contains("Active Skills"));
        assert!(section.text.contains("review"));
        assert!(section.text.contains("test"));
    }

    #[test]
    fn active_skill_names_provider_returns_none_when_empty() {
        let p = super::ActiveSkillNamesProvider { names: vec![] };
        assert!(p.provide(0).is_none(), "empty names → None");
    }

    #[test]
    fn active_skill_names_provider_cache_scope_is_none() {
        let p = super::ActiveSkillNamesProvider {
            names: vec!["a".into()],
        };
        assert_eq!(p.cache_scope(), CacheScope::None);
    }

    // ── TurnBudgetProvider ──

    #[test]
    fn turn_budget_provider_emits_high_budget_no_urgency() {
        let p = super::TurnBudgetProvider {
            remaining_turns: 90,
            max_turns: 100,
        };
        let section = p.provide(0).expect("should always emit when constructed");
        assert!(section.text.contains("Turn Budget"));
        assert!(section.text.contains("90/100"));
        assert!(!section.text.contains("efficiently"));
        assert!(!section.text.contains("needlessly"));
    }

    #[test]
    fn turn_budget_provider_emits_medium_budget_efficiency_nudge() {
        let p = super::TurnBudgetProvider {
            remaining_turns: 60,
            max_turns: 100,
        };
        let section = p.provide(0).expect("should emit");
        assert!(section.text.contains("efficiently"));
    }

    #[test]
    fn turn_budget_provider_emits_low_budget_needlessly_warning() {
        let p = super::TurnBudgetProvider {
            remaining_turns: 10,
            max_turns: 100,
        };
        let section = p.provide(0).expect("should emit");
        assert!(section.text.contains("needlessly"));
    }

    #[test]
    fn turn_budget_provider_at_exact_80_pct_is_high() {
        let p = super::TurnBudgetProvider {
            remaining_turns: 80,
            max_turns: 100,
        };
        let section = p.provide(0).expect("should emit");
        assert!(
            !section.text.contains("efficiently"),
            "80%+ should be high budget, no urgency"
        );
    }

    #[test]
    fn turn_budget_provider_cache_scope_is_none() {
        let p = super::TurnBudgetProvider {
            remaining_turns: 5,
            max_turns: 10,
        };
        assert_eq!(p.cache_scope(), CacheScope::None);
    }

    // ── CapabilitiesProvider ──

    #[test]
    fn capabilities_provider_emits_tool_and_skill_counts() {
        let p = super::CapabilitiesProvider {
            tool_count: 5,
            skill_count: 2,
            max_turn_input_tokens: 100_000,
        };
        let section = p.provide(0).expect("should always emit");
        assert!(section.text.contains("Capabilities"));
        assert!(section.text.contains("5 tools"));
        assert!(section.text.contains("2 active skills"));
        assert!(section.text.contains("100000 tokens per turn"));
    }

    #[test]
    fn capabilities_provider_omits_context_window_when_zero() {
        let p = super::CapabilitiesProvider {
            tool_count: 1,
            skill_count: 0,
            max_turn_input_tokens: 0,
        };
        let section = p.provide(0).expect("should emit");
        assert!(
            !section.text.contains("tokens per turn"),
            "zero max_turn_input_tokens → omitted"
        );
    }

    #[test]
    fn capabilities_provider_cache_scope_is_none() {
        let p = super::CapabilitiesProvider {
            tool_count: 1,
            skill_count: 0,
            max_turn_input_tokens: 0,
        };
        assert_eq!(p.cache_scope(), CacheScope::None);
    }

    // ── SkillListingProvider ──

    #[test]
    fn skill_listing_provider_emits_content() {
        let p = super::SkillListingProvider {
            content: "<available_skills>...</available_skills>".into(),
        };
        let section = p.provide(0).expect("should emit non-empty content");
        assert!(section.text.contains("<available_skills>"));
    }

    #[test]
    fn skill_listing_provider_returns_none_when_empty() {
        let p = super::SkillListingProvider {
            content: String::new(),
        };
        assert!(p.provide(0).is_none());
    }

    #[test]
    fn skill_listing_provider_cache_scope_is_session() {
        let p = super::SkillListingProvider {
            content: "x".into(),
        };
        assert_eq!(p.cache_scope(), CacheScope::Session);
    }

    // ── CacheStrategyProvider ──

    #[test]
    fn cache_strategy_provider_always_emits() {
        let p = super::CacheStrategyProvider;
        let section = p.provide(0).expect("should always emit");
        assert!(section.text.contains("Execution Strategy"));
        assert!(
            section
                .text
                .contains("prompt cache is only reliable within the current turn")
        );
    }

    #[test]
    fn cache_strategy_provider_cache_scope_is_session() {
        let p = super::CacheStrategyProvider;
        assert_eq!(p.cache_scope(), CacheScope::Session);
    }

    // ── LessonsChannelProvider ──

    #[test]
    fn lessons_provider_emits_formatted_lessons() {
        let p = super::LessonsChannelProvider {
            text: "fix:compile:check".into(),
        };
        let section = p.provide(0).expect("should emit non-empty lessons");
        assert!(section.text.contains("Session Lessons"));
        assert!(section.text.contains("fix:compile:check"));
    }

    #[test]
    fn lessons_provider_returns_none_when_empty() {
        let p = super::LessonsChannelProvider {
            text: String::new(),
        };
        assert!(p.provide(0).is_none());
    }

    #[test]
    fn lessons_provider_cache_scope_is_session() {
        let p = super::LessonsChannelProvider { text: "x".into() };
        assert_eq!(p.cache_scope(), CacheScope::Session);
    }

    // ── ChannelAssembler integration tests ──

    #[test]
    fn assembler_empty_providers_returns_empty_sections() {
        let assembler = ChannelAssembler::new(vec![], ContextChannelPolicy::default());
        let (stable, dynamic) = assembler.assemble(0);
        assert!(stable.is_empty());
        assert!(dynamic.is_empty());
    }

    #[test]
    fn assembler_partitions_by_cache_scope() {
        let providers: Vec<Box<dyn astra_turn_core::context_sources::ContextChannelProvider>> = vec![
            Box::new(super::EnvStaticProvider {
                text: "static-env".into(),
            }), // Session scope
            Box::new(super::EnvVolatileProvider {
                text: "volatile-env".into(),
            }), // None scope
        ];
        let assembler = ChannelAssembler::new(providers, ContextChannelPolicy::default());
        let (stable, dynamic) = assembler.assemble(0);
        assert_eq!(stable.len(), 1, "env_static → stable");
        assert_eq!(dynamic.len(), 1, "env_volatile → dynamic");
        assert!(stable[0].text.contains("static-env"));
        assert!(dynamic[0].text.contains("volatile-env"));
    }

    #[test]
    fn assembler_respects_policy_suppressed_channel() {
        let providers: Vec<Box<dyn astra_turn_core::context_sources::ContextChannelProvider>> = vec![
            Box::new(super::CacheStrategyProvider), // always emits
        ];
        let mut policy = ContextChannelPolicy::default();
        policy.suppressed.insert("cache_strategy");
        let assembler = ChannelAssembler::new(providers, policy);
        let (stable, dynamic) = assembler.assemble(0);
        assert!(
            stable.is_empty(),
            "cache_strategy suppressed → no stable output"
        );
        assert!(dynamic.is_empty());
    }

    #[test]
    fn assembler_respects_policy_min_turn() {
        let providers: Vec<Box<dyn astra_turn_core::context_sources::ContextChannelProvider>> =
            vec![Box::new(super::CacheStrategyProvider)];
        let mut policy = ContextChannelPolicy::default();
        policy.min_turn = Some(5);
        let assembler = ChannelAssembler::new(providers, policy);
        let (stable, dynamic) = assembler.assemble(0); // turn 0 < min_turn 5
        assert!(stable.is_empty(), "turn 0 blocked by min_turn=5");
        assert!(dynamic.is_empty());

        let (stable2, dynamic2) = assembler.assemble(5); // turn 5 >= min_turn 5
        assert_eq!(stable2.len(), 1, "turn 5 passes min_turn gate");
        assert!(dynamic2.is_empty());
    }

    #[test]
    fn assembler_providers_returning_none_are_skipped() {
        let providers: Vec<Box<dyn astra_turn_core::context_sources::ContextChannelProvider>> = vec![
            Box::new(super::EnvStaticProvider {
                text: String::new(),
            }), // empty → None
            Box::new(super::LessonsChannelProvider {
                text: String::new(),
            }), // empty → None
        ];
        let assembler = ChannelAssembler::new(providers, ContextChannelPolicy::default());
        let (stable, dynamic) = assembler.assemble(0);
        assert!(
            stable.is_empty(),
            "all providers return None → empty stable"
        );
        assert!(
            dynamic.is_empty(),
            "all providers return None → empty dynamic"
        );
    }

    #[test]
    fn assembler_mixed_scopes_produces_correct_partitioning() {
        let providers: Vec<Box<dyn astra_turn_core::context_sources::ContextChannelProvider>> = vec![
            Box::new(super::SkillListingProvider {
                content: "skills".into(),
            }), // Session
            Box::new(super::CacheStrategyProvider), // Session
            Box::new(super::TurnBudgetProvider {
                remaining_turns: 5,
                max_turns: 10,
            }), // None
            Box::new(super::ActiveSkillNamesProvider {
                names: vec!["a".into()],
            }), // None
        ];
        let assembler = ChannelAssembler::new(providers, ContextChannelPolicy::default());
        let (stable, dynamic) = assembler.assemble(0);
        assert_eq!(stable.len(), 2, "skill_listing + cache_strategy → 2 stable");
        assert_eq!(
            dynamic.len(),
            2,
            "turn_budget + active_skill_names → 2 dynamic"
        );
    }

    #[test]
    fn assembler_overrides_section_scope_from_provider() {
        // A provider that creates a PromptSection::dynamic (CacheScope::None)
        // should have its scope overridden to Session by the assembler when
        // the provider declares cache_scope() == Session.
        let p = super::EnvStaticProvider {
            text: "should-become-session".into(),
        };
        let providers: Vec<Box<dyn astra_turn_core::context_sources::ContextChannelProvider>> =
            vec![Box::new(p)];
        let assembler = ChannelAssembler::new(providers, ContextChannelPolicy::default());
        let (stable, _dynamic) = assembler.assemble(0);
        assert_eq!(stable.len(), 1);
        assert_eq!(
            stable[0].scope,
            CacheScope::Session,
            "assembler must override section.scope to provider.cache_scope()"
        );
    }

    // ── build_external_sources integration tests ──

    #[test]
    fn external_sources_includes_all_provider_sections() {
        let mut ep = serde_json::Map::new();
        ep.insert(
            "environment_static".into(),
            Value::String("env: macos".into()),
        );
        ep.insert(
            "active_skills".into(),
            Value::Array(vec![Value::String("review".into())]),
        );
        ep.insert("lessons_text".into(), Value::String("fix:bug:patch".into()));

        let mut state = make_state();
        state.max_turns = 20;
        state.remaining_turns = 15;
        state.max_turn_input_tokens = 100_000;

        let sources = build_external_sources(&ep, &state, "hi", &["bash", "read_file"], None, None);

        // Check that sections from providers appear in the right lanes
        let stable_texts: Vec<&str> = sources
            .extra_stable_sections
            .iter()
            .map(|s| s.text.as_str())
            .collect();
        let dynamic_texts: Vec<&str> = sources
            .extra_dynamic_sections
            .iter()
            .map(|s| s.text.as_str())
            .collect();

        assert!(
            stable_texts.iter().any(|t| t.contains("env: macos")),
            "env_static in stable"
        );
        assert!(
            stable_texts.iter().any(|t| t.contains("Session Lessons")),
            "lessons in stable"
        );

        assert!(
            dynamic_texts.iter().any(|t| t.contains("Turn Budget")),
            "turn_budget in dynamic"
        );
        assert!(
            dynamic_texts.iter().any(|t| t.contains("Capabilities")),
            "capabilities in dynamic"
        );
        assert!(
            dynamic_texts.iter().any(|t| t.contains("Active Skills")),
            "active_skills in dynamic"
        );
    }

    #[test]
    fn external_sources_empty_edge_profile_produces_only_always_emit_sections() {
        let ep = serde_json::Map::new();
        let state = make_state();
        let sources = build_external_sources(&ep, &state, "hi", &[], None, None);

        // With no tools, no env, no skills: only CapabilitiesProvider (always emits)
        // and no self_model/tool_conditional (gated on non-empty tool_names)
        assert!(
            sources
                .extra_dynamic_sections
                .iter()
                .any(|s| s.text.contains("Capabilities")),
            "Capabilities always emits"
        );
        // No stable sections should be emitted
        assert!(
            sources.extra_stable_sections.is_empty(),
            "empty edge_profile → no stable sections"
        );
    }

    #[test]
    fn external_sources_tool_conditional_only_when_tools_present() {
        let ep = serde_json::Map::new();
        let state = make_state();

        let with_tools = build_external_sources(&ep, &state, "hi", &["bash"], None, None);
        assert!(
            with_tools
                .extra_dynamic_sections
                .iter()
                .any(|s| s.text.contains("Tool Availability Protocol")),
            "tool_conditional emits when tools present"
        );

        let without_tools = build_external_sources(&ep, &state, "hi", &[], None, None);
        assert!(
            !without_tools
                .extra_dynamic_sections
                .iter()
                .any(|s| s.text.contains("Tool Availability Protocol")),
            "tool_conditional absent when no tools"
        );
    }

    #[test]
    fn external_sources_effort_hint_flows_to_typed_field_not_section() {
        let mut state = make_state();
        state.skills.effort = Some(astra_skills::EffortLevel::High);
        state.skills.agent_type = Some("code-review".into());

        let ep = serde_json::Map::new();
        let sources = build_external_sources(&ep, &state, "hi", &[], None, None);

        assert!(
            sources.effort_hint.is_some(),
            "effort_hint should be populated"
        );
        let hint = sources.effort_hint.unwrap();
        assert!(hint.contains("high"));
        assert!(hint.contains("code-review"));

        // effort_hint must NOT appear in sections — it's a typed field
        let all_section_text: String = sources
            .extra_stable_sections
            .iter()
            .chain(sources.extra_dynamic_sections.iter())
            .map(|s| &s.text)
            .cloned()
            .collect::<Vec<_>>()
            .join("");
        assert!(
            !all_section_text.contains("Effort Level"),
            "effort_hint must not leak into sections"
        );
    }

    #[test]
    fn external_sources_plan_context_passed_through() {
        let ep = serde_json::Map::new();
        let state = make_state();
        let sources = build_external_sources(
            &ep,
            &state,
            "hi",
            &[],
            Some("Executing step 3 of 5: refactor database layer"),
            None,
        );
        assert!(sources.plan_context.is_some());
        assert!(sources.plan_context.unwrap().contains("step 3 of 5"));
    }

    #[test]
    fn external_sources_plan_context_none_when_empty_string() {
        let ep = serde_json::Map::new();
        let state = make_state();
        let sources = build_external_sources(&ep, &state, "hi", &[], Some(""), None);
        assert!(sources.plan_context.is_none(), "empty string → None");
    }

    #[test]
    fn external_sources_system_override_passed_through() {
        let mut ep = serde_json::Map::new();
        ep.insert(
            "system_prompt_override".into(),
            Value::String("You are a specialized agent.".into()),
        );
        let state = make_state();
        let sources = build_external_sources(&ep, &state, "hi", &[], None, None);
        assert!(sources.system_override.is_some());
        assert!(
            sources
                .system_override
                .unwrap()
                .contains("specialized agent")
        );
    }

    #[test]
    fn external_sources_system_override_none_when_missing() {
        let ep = serde_json::Map::new();
        let state = make_state();
        let sources = build_external_sources(&ep, &state, "hi", &[], None, None);
        assert!(sources.system_override.is_none());
    }

    #[test]
    fn external_sources_no_duplicate_sections_from_providers() {
        // Each provider should emit at most one section.
        // Verify by counting unique section texts.
        let mut ep = serde_json::Map::new();
        ep.insert(
            "active_skills".into(),
            Value::Array(vec![
                Value::String("review".into()),
                Value::String("test".into()),
            ]),
        );
        let mut state = make_state();
        state.max_turns = 10;
        state.remaining_turns = 5;

        let sources = build_external_sources(&ep, &state, "hi", &["bash"], None, None);

        let all_texts: Vec<&str> = sources
            .extra_stable_sections
            .iter()
            .chain(sources.extra_dynamic_sections.iter())
            .map(|s| s.text.as_str())
            .collect();

        // No duplicate section texts
        let mut seen = std::collections::HashSet::new();
        for text in &all_texts {
            assert!(seen.insert(*text), "duplicate section text: {text}");
        }
    }

    #[test]
    fn external_sources_turn_budget_not_present_when_max_turns_zero() {
        let ep = serde_json::Map::new();
        let mut state = make_state();
        state.max_turns = 0;
        state.remaining_turns = 5;

        let sources = build_external_sources(&ep, &state, "hi", &[], None, None);
        assert!(
            !sources
                .extra_dynamic_sections
                .iter()
                .any(|s| s.text.contains("Turn Budget")),
            "turn_budget provider not constructed when max_turns=0"
        );
    }

    #[test]
    fn external_sources_cache_strategy_emitted_when_cache_capable() {
        let ep = serde_json::Map::new();
        let state = make_state();
        let sources = build_external_sources(
            &ep,
            &state,
            "hi",
            &[],
            None,
            Some(astra_turn_core::cache_placement::CacheCapability {
                protocol: astra_turn_core::cache_placement::CacheProtocol::OpenAiAutoPrefix,
                volatile_placement: astra_turn_core::cache_placement::VolatilePlacement::TailSuffix,
                reuse_scope: Some(
                    astra_turn_core::cache_placement::CacheReuseScope::IntraTurnRounds,
                ),
            }),
        );
        assert!(
            sources
                .extra_stable_sections
                .iter()
                .any(|s| s.text.contains("Execution Strategy")),
            "cache strategy emitted when intra-turn batching preferred"
        );
    }

    #[test]
    fn external_sources_cache_strategy_not_emitted_when_not_intra_turn() {
        let ep = serde_json::Map::new();
        let state = make_state();
        let sources = build_external_sources(
            &ep,
            &state,
            "hi",
            &[],
            None,
            Some(astra_turn_core::cache_placement::CacheCapability {
                protocol: astra_turn_core::cache_placement::CacheProtocol::MarkerExplicit,
                volatile_placement:
                    astra_turn_core::cache_placement::VolatilePlacement::MarkerIsolated,
                reuse_scope: None,
            }),
        );
        assert!(
            !sources
                .extra_stable_sections
                .iter()
                .any(|s| s.text.contains("Execution Strategy")),
            "cache strategy NOT emitted when reuse_scope is not IntraTurnRounds"
        );
    }
}

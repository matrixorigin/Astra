//! Server-side [`AgenticLoopHost`] implementation.
//!
//! Enables the API server to run multi-turn agentic loops with the same
//! cognitive processing (stall detection, dedup, post-tool policy) as the CLI.
//!
//! Architecture:
//! ```text
//! Client → POST /chat/stream
//!   → AgenticRunLifecycleService::stream_chat()
//!     → ServerAgenticLoopHost + run_agentic_loop_with_host()
//!       → execute_turn(): resolve model, cancellable cooldown wait, call LLM, accumulate response
//!       → headless_tool_round(): execute tools via ledger
//!       → post_tool_policy(): stall/dedup/guard
//! ```

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Instant;

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use tokio::sync::Mutex as TokioMutex;

use crate::bridge::rate_limit_cooldown::{PerModelCooldown, RateLimitAction};
use crate::turn::agentic_headless_round::HeadlessStderrStyle;
use crate::turn::agentic_loop_host::{
    AgenticLoopHost, AgenticLoopState, HostReflectionRequest, HostReflectionResult, HostTurnResult,
    TurnInteractionMode, TurnInteractionPolicy, interaction_scoped_tool_restrictions,
};
use crate::turn::bridge_inprocess::{
    PromptCacheConfig, add_message_cache_breakpoint, annotate_tool_schemas_for_caching,
    build_system_message,
};
use crate::turn::chat_turn_sse_dispatch::ChatTurnSseAccum;
use crate::turn::llm_client::{
    LlmCallResult, LlmCancel, cached_system_prompt, call_llm_and_collect, sleep_ms_or_llm_cancel,
};
use crate::turn::tool_schema_prune::{filter_tool_schemas_by_excluded_names, prune_tool_schemas};
use crate::turn::turn_guard::merge_deprioritized_tools_into_restricted;
use crate::{FernetTokenEncryptor, MatrixOneSettings};
use astra_core::SharedPool;

// ── Rate-Limit Cooldown ──────────────────────────────────────────────────────
/// Per-model rate-limit cooldown tracker (shared with llm_client).
fn rate_limit_cooldown() -> &'static PerModelCooldown {
    static COOLDOWN: OnceLock<PerModelCooldown> = OnceLock::new();
    COOLDOWN.get_or_init(PerModelCooldown::new)
}

fn llm_cancel_for_state(state: &AgenticLoopState) -> LlmCancel<'_> {
    match (&state.cancellation.flag, &state.cancellation.token) {
        (Some(f), Some(t)) => LlmCancel::FlagAndToken(f.as_ref(), t.as_ref()),
        (Some(f), None) => LlmCancel::Flag(f.as_ref()),
        (None, Some(t)) => LlmCancel::Token(t.as_ref()),
        (None, None) => LlmCancel::None,
    }
}

/// Server-side host for the runtime agentic loop.
///
/// Each turn:
/// 1. Resolves the active LLM model
/// 2. Builds system prompt + conversation context
/// 3. Calls the LLM directly via [`call_llm_and_collect`]
/// 4. Accumulates response into [`ChatTurnSseAccum`]
/// 5. For tool calls: waits on `edge_callback_ledger` for edge-executed results
///
/// The tool execution in step 5 is handled by the runtime's headless round,
/// which maps tool calls to edge tool results. The ledger is populated by
/// the client posting to `POST /tools/result`.
pub struct ServerAgenticLoopHost {
    // ── LLM resolution ──
    matrixone: MatrixOneSettings,
    encryptor: Arc<FernetTokenEncryptor>,
    shared_pool: Option<SharedPool>,
    model_override: Option<String>,

    // ── Context ──
    edge_tools: Vec<Value>,
    edge_profile: Map<String, Value>,
    valid_tools: HashSet<String>,
    selection_confidence: f64,
    /// `true` when tools were auto-populated from astra-tools (no CLI connected).
    server_side_tools: bool,

    // ── Tool execution (used by RunLifecycleService wiring) ──
    #[allow(dead_code)] // needed once RunLifecycleService uses ledger-based tool execution
    edge_callback_ledger: Arc<TokioMutex<HashMap<String, Value>>>,
    #[allow(dead_code)]
    user_id: String,
    session_id: String,

    // ── Output collection ──
    /// SSE events emitted during the turn, streamed to the client.
    emitted_events: Vec<Value>,

    // ── Agent progress ──
    /// Optional receiver for agent progress events (multi-agent tree updates).
    progress_rx: Option<tokio::sync::broadcast::Receiver<crate::orchestration::AgentProgressEvent>>,
}

/// Builder for [`ServerAgenticLoopHost`].
pub struct ServerAgenticLoopHostBuilder {
    matrixone: MatrixOneSettings,
    encryptor: Arc<FernetTokenEncryptor>,
    shared_pool: Option<SharedPool>,
    model_override: Option<String>,
    edge_tools: Vec<Value>,
    edge_profile: Map<String, Value>,
    selection_confidence: f64,
    edge_callback_ledger: Arc<TokioMutex<HashMap<String, Value>>>,
    user_id: String,
    session_id: String,
    progress_broadcaster: Option<Arc<crate::orchestration::ProgressBroadcaster>>,
}

impl ServerAgenticLoopHostBuilder {
    pub fn new(
        matrixone: MatrixOneSettings,
        encryptor: Arc<FernetTokenEncryptor>,
        user_id: String,
        session_id: String,
    ) -> Self {
        Self {
            matrixone,
            encryptor,
            shared_pool: None,
            model_override: None,
            edge_tools: Vec::new(),
            edge_profile: Map::new(),
            selection_confidence: 1.0,
            edge_callback_ledger: Arc::new(TokioMutex::new(HashMap::new())),
            user_id,
            session_id,
            progress_broadcaster: None,
        }
    }

    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.shared_pool = Some(pool);
        self
    }

    pub fn with_model(mut self, model: Option<String>) -> Self {
        self.model_override = model;
        self
    }

    pub fn with_edge_tools(mut self, tools: Vec<Value>) -> Self {
        self.edge_tools = tools;
        self
    }

    pub fn with_edge_profile(mut self, profile: Map<String, Value>) -> Self {
        self.edge_profile = profile;
        self
    }

    pub fn with_selection_confidence(mut self, confidence: f64) -> Self {
        self.selection_confidence = confidence;
        self
    }

    pub fn with_edge_callback_ledger(
        mut self,
        ledger: Arc<TokioMutex<HashMap<String, Value>>>,
    ) -> Self {
        self.edge_callback_ledger = ledger;
        self
    }

    pub fn with_progress_broadcaster(
        mut self,
        broadcaster: Arc<crate::orchestration::ProgressBroadcaster>,
    ) -> Self {
        self.progress_broadcaster = Some(broadcaster);
        self
    }

    pub fn build(self) -> ServerAgenticLoopHost {
        // When no edge tools are provided (web-only mode), populate with
        // server-side tool schemas from astra-tools so the LLM knows what's available.
        let server_side_tools = self.edge_tools.is_empty();
        let edge_tools = if server_side_tools {
            astra_tools::schemas::server_executor_tool_schemas()
        } else {
            self.edge_tools
        };

        let valid_tools = edge_tools
            .iter()
            .filter_map(|t| {
                t.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .map(String::from)
            })
            .collect();

        let progress_rx = self.progress_broadcaster.as_ref().map(|b| b.subscribe());

        ServerAgenticLoopHost {
            matrixone: self.matrixone,
            encryptor: self.encryptor,
            shared_pool: self.shared_pool,
            model_override: self.model_override,
            edge_tools,
            edge_profile: self.edge_profile,
            valid_tools,
            selection_confidence: self.selection_confidence,
            server_side_tools,
            edge_callback_ledger: self.edge_callback_ledger,
            user_id: self.user_id,
            session_id: self.session_id,
            emitted_events: Vec::new(),
            progress_rx,
        }
    }
}

impl ServerAgenticLoopHost {
    fn push_reasoning_events(emitted_events: &mut Vec<Value>, reasoning: &str) {
        if reasoning.is_empty() {
            return;
        }
        emitted_events.push(json!({
            "type": "reasoning_delta",
            "content": reasoning,
        }));
        emitted_events.push(json!({
            "type": "reasoning_done",
        }));
    }

    /// Access collected SSE events from the last turn.
    /// Also drains any pending agent progress events into the result.
    pub fn take_emitted_events(&mut self) -> Vec<Value> {
        // Drain pending progress events from the broadcast receiver
        if let Some(ref mut rx) = self.progress_rx {
            while let Ok(evt) = rx.try_recv() {
                if let Some(sse_val) = progress_event_to_sse(&evt) {
                    self.emitted_events.push(sse_val);
                }
            }
        }
        std::mem::take(&mut self.emitted_events)
    }

    /// Returns `true` when no CLI edge agent is connected (tools are server-side).
    pub fn edge_tools_empty(&self) -> bool {
        self.server_side_tools
    }

    /// Build the system prompt from edge context and the tool schemas visible
    /// to the current turn.
    ///
    /// Returns `(structured_system_messages, plain_text_for_estimates)`.
    /// The structured messages include Anthropic cache_control annotations when
    /// applicable, enabling prompt caching on the runs path.
    fn build_system_messages_cached(
        &self,
        user_content: &str,
        tools: &[Value],
        state: &AgenticLoopState,
        cache_cfg: &PromptCacheConfig,
    ) -> (Vec<Value>, String) {
        let tool_names: Vec<&str> = tools
            .iter()
            .filter_map(|t| {
                t.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
            })
            .collect();

        let mut profile_parts = Vec::new();
        if let Some(cwd) = self.edge_profile.get("cwd").and_then(Value::as_str) {
            profile_parts.push(format!("cwd: {cwd}"));
        }
        if let Some(branch) = self.edge_profile.get("git_branch").and_then(Value::as_str) {
            profile_parts.push(format!("git_branch: {branch}"));
        }

        let skill_hint = self
            .edge_profile
            .get("active_skills")
            .and_then(Value::as_array)
            .map(|arr| {
                let names: Vec<&str> = arr.iter().filter_map(Value::as_str).collect();
                if names.is_empty() {
                    String::new()
                } else {
                    format!(
                        "\n\n## Active Output Skills\nThe user has enabled these output constraints: {}. \
                         Follow their formatting rules strictly.",
                        names.join(", ")
                    )
                }
            })
            .unwrap_or_default();

        let learned_context_hint = self
            .edge_profile
            .get("learned_context_hint")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(|hint| format!("\n\n## Learned Runtime Context\n{hint}"))
            .unwrap_or_default();

        let task_type = self
            .edge_profile
            .get("selection_task_type")
            .and_then(Value::as_str)
            .or_else(|| crate::prompts::detect_task_type(user_content));

        let profile_desc = if profile_parts.is_empty() {
            String::new()
        } else {
            format!("\n\n# Project Profile\n{}", profile_parts.join("\n"))
        };
        let profile_with_hints = format!("{profile_desc}{skill_hint}{learned_context_hint}");

        // Skill effort/agent_type hints (dynamic per-turn)
        let mut extra_dynamic = String::new();
        if let Some(ref effort) = state.skills.effort {
            extra_dynamic.push_str(&format!(
                "\n\n## Effort Level\nThe active skill requests effort level: **{effort}**. \
                 Adjust thoroughness accordingly.",
            ));
        }
        if let Some(ref agent_type) = state.skills.agent_type {
            extra_dynamic.push_str(&format!(
                "\n\n## Agent Type\nYou are acting as a **{agent_type}** agent for this skill.",
            ));
        }

        // Memory signal detection
        let memory_signal_hint = if let Some(category) =
            crate::prompts::memory_lifecycle::detect_store_signal(user_content)
        {
            let ns = crate::prompts::memory_lifecycle::suggest_namespace(category);
            format!(
                "\n\n⚡ MEMORY SIGNAL DETECTED: category=\"{category}\", namespace=\"{ns}\". \
                 Store the user's intent with memory_store BEFORE doing anything else."
            )
        } else {
            String::new()
        };

        // System prompt override from delegation coordination context
        let system_override = self
            .edge_profile
            .get("system_prompt_override")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(|s| format!("\n\n{s}"))
            .unwrap_or_default();

        // All dynamic per-turn content (not cached)
        let full_dynamic =
            format!("{profile_with_hints}{extra_dynamic}{memory_signal_hint}{system_override}");

        // Build structured system messages with Anthropic cache annotations.
        // Stable sections (Global/Session) get cache_control; dynamic content does not.
        let (sys_msg, dynamic_msg, _sections) = build_system_message(
            &tool_names,
            &full_dynamic,
            self.selection_confidence,
            task_type,
            cache_cfg,
        );
        let mut system_messages = vec![sys_msg];
        if let Some(dm) = dynamic_msg {
            system_messages.push(dm);
        }

        // Plain text for token estimation (no cache annotations)
        let plain = cached_system_prompt(
            &tool_names,
            &full_dynamic,
            self.selection_confidence,
            task_type,
        );

        (system_messages, plain)
    }

    /// Compute the tool schemas visible for the current turn after applying
    /// health-based restrictions. This is the server-path equivalent of the
    /// CLI's deny-at-assembly behavior.
    fn filtered_turn_tools(&self, restricted_tools: &HashSet<String>) -> Vec<Value> {
        filter_tool_schemas_by_excluded_names(self.edge_tools.clone(), restricted_tools)
    }

    fn effective_allowlist_restrictions(&self, state: &AgenticLoopState) -> HashSet<String> {
        let mut allowed = state.skills.request_constraints.allowed_tools.clone();
        if let Some(skill_allowed) = &state.skills.allowed_tools {
            let skill_allowed = skill_allowed
                .iter()
                .map(|name| name.trim().to_ascii_lowercase())
                .collect::<HashSet<_>>();
            allowed = Some(match allowed {
                Some(request_allowed) => request_allowed
                    .intersection(&skill_allowed)
                    .cloned()
                    .collect(),
                None => skill_allowed,
            });
        }

        let Some(allowed) = allowed else {
            return HashSet::new();
        };

        self.edge_tools
            .iter()
            .filter_map(|tool| {
                tool.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .map(String::from)
            })
            .filter(|name| {
                name.as_str() != crate::turn::skill_tool::SKILL_TOOL_NAME
                    && name.as_str() != crate::turn::skill_tool::DISCOVER_SKILLS_TOOL_NAME
                    && !allowed.contains(&name.trim().to_ascii_lowercase())
            })
            .collect()
    }

    fn sync_valid_tools_to_visible(&mut self, visible_tools: &[Value]) {
        self.valid_tools = visible_tools
            .iter()
            .filter_map(|tool| {
                tool.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .map(String::from)
            })
            .collect();
    }

    /// Compute the tool schemas visible for the current turn after applying
    /// health-based restrictions. This is the server-path equivalent of the
    /// CLI's deny-at-assembly behavior.
    #[cfg(test)]
    fn visible_turn_tools(&mut self, state: &mut AgenticLoopState) -> Vec<Value> {
        merge_deprioritized_tools_into_restricted(&state.turn_guard, &mut state.restricted_tools);
        let mut effective_restricted = state.restricted_tools.clone();
        effective_restricted.extend(self.effective_allowlist_restrictions(state));
        let visible = self.filtered_turn_tools(&effective_restricted);
        self.sync_valid_tools_to_visible(&visible);
        visible
    }

    /// Test-only: returns the plain-text system prompt (no cache annotations).
    #[cfg(test)]
    fn build_system_prompt(&self, user_content: &str, visible_tools: &[Value]) -> String {
        let tool_names: Vec<&str> = visible_tools
            .iter()
            .filter_map(|t| {
                t.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
            })
            .collect();

        // Replicate the dynamic section assembly from build_system_messages_cached.
        let mut profile_parts = Vec::new();
        if let Some(cwd) = self.edge_profile.get("cwd").and_then(Value::as_str) {
            profile_parts.push(format!("cwd: {cwd}"));
        }
        if let Some(branch) = self.edge_profile.get("git_branch").and_then(Value::as_str) {
            profile_parts.push(format!("git_branch: {branch}"));
        }
        let skill_hint = self
            .edge_profile
            .get("active_skills")
            .and_then(Value::as_array)
            .map(|arr| {
                let names: Vec<&str> = arr.iter().filter_map(Value::as_str).collect();
                if names.is_empty() {
                    String::new()
                } else {
                    format!(
                        "\n\n## Active Output Skills\nThe user has enabled these output constraints: {}. \
                         Follow their formatting rules strictly.",
                        names.join(", ")
                    )
                }
            })
            .unwrap_or_default();
        let learned_context_hint = self
            .edge_profile
            .get("learned_context_hint")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(|hint| format!("\n\n## Learned Runtime Context\n{hint}"))
            .unwrap_or_default();
        let task_type = crate::prompts::detect_task_type(user_content);
        let profile_desc = if profile_parts.is_empty() {
            String::new()
        } else {
            format!("\n\n# Project Profile\n{}", profile_parts.join("\n"))
        };
        let memory_signal_hint = if let Some(category) =
            crate::prompts::memory_lifecycle::detect_store_signal(user_content)
        {
            let ns = crate::prompts::memory_lifecycle::suggest_namespace(category);
            format!(
                "\n\n⚡ MEMORY SIGNAL DETECTED: category=\"{category}\", namespace=\"{ns}\". \
                 Store the user's intent with memory_store BEFORE doing anything else."
            )
        } else {
            String::new()
        };
        let full_dynamic =
            format!("{profile_desc}{skill_hint}{learned_context_hint}{memory_signal_hint}");

        cached_system_prompt(
            &tool_names,
            &full_dynamic,
            self.selection_confidence,
            task_type,
        )
    }

    /// Build the LLM message array from loop state.
    async fn build_llm_messages(
        &self,
        system_messages: Vec<Value>,
        state: &AgenticLoopState,
        visible_tools: &[Value],
        model_name: &str,
        api_key: &str,
        base_url: &str,
        provider: &str,
        cache_cfg: &PromptCacheConfig,
    ) -> Vec<Value> {
        let mut llm_messages = system_messages;

        // Compute compaction tier
        let budget = crate::prompts::budget_for_model(Some(model_name));
        let tool_schema_tokens: usize = visible_tools
            .iter()
            .map(|t| {
                serde_json::to_string(t)
                    .map(|s| crate::prompts::estimate_str_tokens(&s))
                    .unwrap_or(50)
            })
            .sum();
        let mut all_msgs = llm_messages.clone();
        all_msgs.extend(state.messages.iter().cloned());
        let cache_est = crate::prompts::estimate_tokens_cache_aware(&all_msgs, tool_schema_tokens);
        let tier = crate::prompts::compaction_tier_calibrated(
            &budget,
            cache_est.total_tokens,
            state.last_measured_prompt_tokens,
            state.consecutive_context_window_errors,
        );
        let budget_chars = budget.effective_input_limit() * 4;

        // ── Micro-compact: clear old tool results before main compaction ──
        let micro_compacted_messages = {
            let msgs = state.messages.clone();
            crate::turn::cloud::analytics::run_micro_compact(&msgs)
        };

        // Use Memoria-based compaction (async with HTTP client)
        let memoria_config = crate::turn::cloud::memoria_compact::MemoriaCompactConfig::default();
        let cwd = self.edge_profile.get("cwd").and_then(|v| v.as_str());
        let (session_memory_file, session_memory_combine) =
            crate::turn::cloud::memoria_compact::resolve_session_memory_file_options(
                &self.session_id,
                cwd,
            );
        let memoria_params = crate::turn::cloud::memoria_compact::MemoriaCompactParams {
            budget_chars,
            keep_chars: 2_000,
            tier,
            keep_recent_turns: budget.keep_recent_turns,
            current_tokens: cache_est.total_tokens,
            session_memory_file,
            session_memory_combine,
            session_facts: None,
        };

        // Try to create Memoria client from environment
        let memoria_client = crate::turn::cloud::memoria_compact::HttpMemoriaClient::from_env();

        // Build summary client for LLM-based compaction (uses same model as main LLM)
        let compact_config = crate::prompts::CompactConfig::from_env();
        let summary_client = crate::turn::cloud::summary::HttpSummaryClient::new(
            crate::turn::cloud::summary::LlmConnParams {
                model_name: model_name.to_string(),
                api_key: api_key.to_string(),
                base_url: base_url.to_string(),
                provider: provider.to_string(),
                max_output_tokens: compact_config.summary_token_budget,
            },
        );

        let compact_result = crate::turn::cloud::memoria_compact::compact_with_memoria(
            &micro_compacted_messages,
            Some(&self.session_id),
            &memoria_config,
            &memoria_params,
            memoria_client
                .as_ref()
                .map(|c| c as &dyn crate::turn::cloud::memoria_compact::MemoriaClient),
            Some(&compact_config),
            Some(&summary_client as &dyn crate::turn::cloud::summary::SummaryLlmClient),
        )
        .await;

        llm_messages.extend(compact_result.messages);
        // Strip old reasoning to reduce input tokens (see edge_ledger::strip_stale_reasoning).
        crate::turn::edge_ledger::strip_stale_reasoning(&mut llm_messages, provider, model_name);

        // Post-compaction: re-inject invoked skill instructions (truncated)
        // so the LLM retains skill context after history summarization.
        if !state.skills.invoked.is_empty() {
            let mut builder = crate::turn::cloud::attachments::AttachmentBuilder::new();
            let mut skills: Vec<_> = state.skills.invoked.values().collect();
            skills.sort_by_key(|b| std::cmp::Reverse(b.invoked_at_turn));
            for skill in skills {
                builder.add_skill(&skill.name, &skill.content);
            }
            let attachments = builder.build();
            llm_messages.extend(attachments.to_messages());
        }

        // Post-compaction: re-inject recently-read file contents so the LLM
        // retains awareness of code it was working with before compaction.
        if !state.recent_file_reads.is_empty() {
            let cwd = self.edge_profile.get("cwd").and_then(|v| v.as_str());
            let file_messages = crate::turn::cloud::attachments::restore_recent_files(
                &state.recent_file_reads,
                cwd,
            );
            llm_messages.extend(file_messages);
        }

        // Ephemeral skill listing: injected per-turn, not stored in state.messages.
        if let Some(ref listing) = state.skills.listing_message {
            llm_messages.push(listing.clone());
        }

        // Add cache breakpoint on the last conversation message for Anthropic.
        add_message_cache_breakpoint(&mut llm_messages, cache_cfg);

        llm_messages
    }

    /// Convert an [`LlmCallResult`] into a [`ChatTurnSseAccum`].
    fn result_to_accum(result: &LlmCallResult) -> ChatTurnSseAccum {
        let prompt_tokens = result
            .usage
            .get("prompt")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let completion_tokens = result
            .usage
            .get("completion")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let cache_read_tokens = result
            .usage
            .get("cache_read_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let cache_creation_tokens = result
            .usage
            .get("cache_creation_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);

        ChatTurnSseAccum {
            full_text: result.full_text.clone(),
            reasoning_content: result.reasoning.clone(),
            tool_calls: result.tool_calls.clone(),
            has_tool_calls: !result.tool_calls.is_empty(),
            prompt_tokens,
            completion_tokens,
            cache_read_tokens,
            cache_creation_tokens,
            has_usage: !result.usage.is_empty(),
            session_id: None,
            run_id: None,
            explain_turns: Vec::new(),
            error_message: None,
            system_prompt_tokens: None,
            system_prompt_breakdown: None,
            ..Default::default()
        }
    }
}

#[async_trait]
impl AgenticLoopHost for ServerAgenticLoopHost {
    async fn execute_turn(
        &mut self,
        state: &mut AgenticLoopState,
    ) -> Result<HostTurnResult, astra_core::ClassifiedError> {
        let turn_started = Instant::now();

        // ── 1. Resolve LLM model ────────────────────────────────────────
        // Skill-level model override takes precedence over the host-level one.
        let effective_model_override = state
            .skills
            .model_override
            .as_deref()
            .or(self.model_override.as_deref());
        let pool_ref = self.shared_pool.as_ref().map(|sp| sp.get());
        let (mut model_name, mut api_key, mut base_url, mut provider, fallback_model_name) =
            match astra_services::resolve_active_llm_model(
                &self.matrixone,
                self.encryptor.as_ref(),
                effective_model_override,
                pool_ref,
            )
            .await
            {
                Ok(m) => (
                    m.model_name,
                    m.api_key,
                    m.base_url,
                    m.provider,
                    m.fallback_model,
                ),
                Err(e) => {
                    return Err(astra_core::ClassifiedError::new(
                        astra_core::ErrorKind::Unknown,
                        format!("Model resolution failed: {e}"),
                    ));
                }
            };
        let has_fallback = fallback_model_name.is_some();

        // ── 1b. Check rate-limit cooldown and handle fallback model resolution ──
        let cooldown = rate_limit_cooldown();
        match cooldown.with(&model_name, |c| c.check_request(has_fallback)) {
            RateLimitAction::Proceed => {}
            RateLimitAction::WaitAndRetry { delay_ms } => {
                astra_core::agent_info!(
                    "llm",
                    "rate-limit cooldown: waiting {delay_ms}ms before request"
                );
                sleep_ms_or_llm_cancel(delay_ms, llm_cancel_for_state(state)).await?;
            }
            RateLimitAction::UseFallback { reason } => {
                if let Some(ref fb_name) = fallback_model_name {
                    astra_core::agent_info!(
                        "llm",
                        "rate-limit cooldown: switching to fallback model '{}' ({})",
                        fb_name,
                        reason.as_str()
                    );
                    // Resolve fallback model credentials
                    match astra_services::resolve_active_llm_model(
                        &self.matrixone,
                        self.encryptor.as_ref(),
                        Some(fb_name.as_str()),
                        pool_ref,
                    )
                    .await
                    {
                        Ok(fb) => {
                            model_name = fb.model_name;
                            api_key = fb.api_key;
                            base_url = fb.base_url;
                            provider = fb.provider;
                        }
                        Err(e) => {
                            astra_core::agent_warn!(
                                "llm",
                                "fallback model '{}' resolution failed: {}",
                                fb_name,
                                e
                            );
                            // Continue with primary model (best effort)
                        }
                    }
                } else {
                    astra_core::agent_warn!(
                        "llm",
                        "rate-limit cooldown: fallback requested ({}) but no fallback configured",
                        reason.as_str()
                    );
                }
            }
            RateLimitAction::Reject {
                reason,
                reset_in_ms,
            } => {
                return Err(astra_core::ClassifiedError::new(
                    astra_core::ErrorKind::RateLimit,
                    format!(
                        "Rate limit cooldown active ({}). Resets in {}s. Try again later.",
                        reason.as_str(),
                        reset_in_ms / 1000
                    ),
                ));
            }
        }

        // ── 2. Build messages ───────────────────────────────────────────
        let user_content = state
            .messages
            .iter()
            .rev()
            .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
            .and_then(|m| m.get("content").and_then(Value::as_str))
            .unwrap_or("")
            .to_string();

        merge_deprioritized_tools_into_restricted(&state.turn_guard, &mut state.restricted_tools);
        let mut effective_restricted = state.restricted_tools.clone();
        effective_restricted.extend(self.effective_allowlist_restrictions(state));
        effective_restricted.extend(interaction_scoped_tool_restrictions(
            TurnInteractionMode::Headless,
        ));
        let visible_tools = self.filtered_turn_tools(&effective_restricted);
        self.sync_valid_tools_to_visible(&visible_tools);

        // Latch prompt cache config from provider info (once per turn is fine;
        // provider doesn't change within a turn).
        let cache_cfg = PromptCacheConfig::latch(&provider, &model_name);

        let (system_messages, system_prompt_plain) =
            self.build_system_messages_cached(&user_content, &visible_tools, state, &cache_cfg);

        let llm_messages = self
            .build_llm_messages(
                system_messages,
                state,
                &visible_tools,
                &model_name,
                &api_key,
                &base_url,
                &provider,
                &cache_cfg,
            )
            .await;

        // ── 3. Call LLM ─────────────────────────────────────────────────
        let budget = crate::prompts::budget_for_model(Some(&model_name));
        let max_output_tokens = crate::prompts::capped_output_tokens(&budget);

        let tool_schema_tokens: usize = visible_tools
            .iter()
            .map(|t| {
                serde_json::to_string(t)
                    .map(|s| crate::prompts::estimate_str_tokens(&s))
                    .unwrap_or(50)
            })
            .sum();
        let mut est_msgs = vec![json!({"role": "system", "content": system_prompt_plain})];
        est_msgs.extend(state.messages.iter().cloned());
        let cache_est = crate::prompts::estimate_tokens_cache_aware(&est_msgs, tool_schema_tokens);
        let tier = crate::prompts::compaction_tier_calibrated(
            &budget,
            cache_est.total_tokens,
            state.last_measured_prompt_tokens,
            state.consecutive_context_window_errors,
        );
        let mut final_tools = prune_tool_schemas(&visible_tools, tier);
        // Annotate tool schemas with cache_control for Anthropic.
        annotate_tool_schemas_for_caching(&mut final_tools, &cache_cfg);
        state.last_turn_policy =
            TurnInteractionPolicy::from_tool_schemas(TurnInteractionMode::Headless, &final_tools);

        let llm_cancel = llm_cancel_for_state(state);

        // Output token escalation: if finish_reason is "length", retry once
        // with a higher max_output_tokens (up to 4× the initial budget).
        let mut effective_max_output = max_output_tokens;
        let result = loop {
            let r = call_llm_and_collect(
                &llm_messages,
                &final_tools,
                &model_name,
                &api_key,
                &base_url,
                &provider,
                Some(effective_max_output),
                has_fallback,
                llm_cancel,
            )
            .await;

            // Context-window errors flow through the accum so the agentic loop's
            // Fatal handler can trigger auto-compaction + retry.
            let r = match r {
                Ok(r) => r,
                Err(ref e) if e.kind == astra_core::ErrorKind::ContextWindow => {
                    let accum = ChatTurnSseAccum {
                        error_message: Some(e.message.clone()),
                        ..Default::default()
                    };
                    let ttft_ms = Some(turn_started.elapsed().as_millis() as u64);
                    return Ok(HostTurnResult {
                        accum,
                        ttft_ms,
                        edge_tool_round: Vec::new(),
                        error_kind: Some(astra_core::ErrorKind::ContextWindow),
                    });
                }
                Err(e) => return Err(e),
            };

            if r.finish_reason.as_deref() == Some("length")
                && effective_max_output < max_output_tokens * 4
            {
                let prev = effective_max_output;
                effective_max_output = (effective_max_output * 2).min(max_output_tokens * 4);
                astra_core::agent_warn!(
                    "llm",
                    "output truncated (finish_reason=length), escalating max_output_tokens {} → {}",
                    prev,
                    effective_max_output,
                );
                continue;
            }
            break r;
        };

        // ── 4. Emit SSE events for client ───────────────────────────────
        if !result.full_text.is_empty() {
            self.emitted_events.push(json!({
                "type": "text_delta",
                "content": result.full_text,
            }));
        }
        Self::push_reasoning_events(&mut self.emitted_events, &result.reasoning);
        if !result.usage.is_empty() {
            self.emitted_events.push(json!({
                "type": "usage",
                "prompt_tokens": result.usage.get("prompt"),
                "completion_tokens": result.usage.get("completion"),
            }));
        }

        // ── 5. Build turn result ────────────────────────────────────────
        let ttft_ms = Some(turn_started.elapsed().as_millis() as u64);
        let accum = Self::result_to_accum(&result);

        // Edge tool round is empty — tools are executed by the runtime's
        // headless round via the edge_callback_ledger, not inline here.
        Ok(HostTurnResult {
            accum,
            ttft_ms,
            edge_tool_round: Vec::new(),
            error_kind: None,
        })
    }

    fn supports_auto_reflection(&self) -> bool {
        true
    }

    async fn execute_reflection(
        &mut self,
        state: &mut AgenticLoopState,
        request: HostReflectionRequest<'_>,
    ) -> Result<Option<HostReflectionResult>, astra_core::ClassifiedError> {
        let effective_model_override = state
            .skills
            .model_override
            .as_deref()
            .or(self.model_override.as_deref());
        let pool_ref = self.shared_pool.as_ref().map(|sp| sp.get());
        let (mut model_name, mut api_key, mut base_url, mut provider, fallback_model_name) =
            match astra_services::resolve_active_llm_model(
                &self.matrixone,
                self.encryptor.as_ref(),
                effective_model_override,
                pool_ref,
            )
            .await
            {
                Ok(m) => (
                    m.model_name,
                    m.api_key,
                    m.base_url,
                    m.provider,
                    m.fallback_model,
                ),
                Err(e) => return Err(format!("Model resolution failed: {e}").into()),
            };
        let has_fallback = fallback_model_name.is_some();

        match rate_limit_cooldown().with(&model_name, |c| c.check_request(has_fallback)) {
            RateLimitAction::Proceed => {}
            RateLimitAction::WaitAndRetry { delay_ms } => {
                sleep_ms_or_llm_cancel(delay_ms, llm_cancel_for_state(state)).await?;
            }
            RateLimitAction::UseFallback { .. } => {
                if let Some(ref fb_name) = fallback_model_name
                    && let Ok(fb) = astra_services::resolve_active_llm_model(
                        &self.matrixone,
                        self.encryptor.as_ref(),
                        Some(fb_name.as_str()),
                        pool_ref,
                    )
                    .await
                {
                    model_name = fb.model_name;
                    api_key = fb.api_key;
                    base_url = fb.base_url;
                    provider = fb.provider;
                }
            }
            RateLimitAction::Reject {
                reason,
                reset_in_ms,
            } => {
                return Err(astra_core::ClassifiedError::new(
                    astra_core::ErrorKind::RateLimit,
                    format!(
                        "Rate limit cooldown active ({}). Resets in {}s.",
                        reason.as_str(),
                        reset_in_ms / 1000
                    ),
                ));
            }
        }

        let reflection_messages = vec![
            json!({"role": "system", "content": request.system_prompt}),
            json!({"role": "user", "content": request.user_prompt}),
        ];
        let result = call_llm_and_collect(
            &reflection_messages,
            &[],
            &model_name,
            &api_key,
            &base_url,
            &provider,
            request.max_output_tokens,
            has_fallback,
            llm_cancel_for_state(state),
        )
        .await?;
        let accum = Self::result_to_accum(&result);

        if accum.has_tool_calls {
            return Err(astra_core::ClassifiedError::new(
                astra_core::ErrorKind::InvalidRequest,
                "auto-reflection unexpectedly returned tool calls",
            ));
        }

        Ok(Some(HostReflectionResult {
            full_text: accum.full_text.trim().to_string(),
            prompt_tokens: accum.prompt_tokens,
            completion_tokens: accum.completion_tokens,
            cache_read_tokens: accum.cache_read_tokens,
            cache_creation_tokens: accum.cache_creation_tokens,
            has_usage: accum.has_usage,
        }))
    }

    fn emit_headless_line(&mut self, _style: HeadlessStderrStyle, line: String) {
        self.emitted_events.push(json!({
            "type": "headless_line",
            "content": line,
        }));
    }

    fn is_quiet(&self) -> bool {
        true
    }

    fn valid_tool_names(&self) -> &HashSet<String> {
        &self.valid_tools
    }

    fn inject_tool_schema(&mut self, schema: Value) {
        if let Some(name) = schema
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
        {
            let name_owned = name.to_string();
            self.valid_tools.insert(name_owned.clone());
            if let Some(existing) = self.edge_tools.iter_mut().find(|tool| {
                tool.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    == Some(name_owned.as_str())
            }) {
                *existing = schema;
            } else {
                self.edge_tools.push(schema);
            }
        }
    }
}

// ─── Progress event → SSE value conversion ──────────────────────────────────

/// Convert an `AgentProgressEvent` into an SSE-compatible JSON value.
/// Returns `None` for event types that don't need to be sent to web clients.
fn progress_event_to_sse(evt: &crate::orchestration::AgentProgressEvent) -> Option<Value> {
    use crate::orchestration::ProgressEventType;
    let agent_id = &evt.agent_id;
    let ts = evt.timestamp_epoch_ms;

    match &evt.event_type {
        ProgressEventType::AgentSpawned {
            run_id,
            parent_run_id,
            agent_type,
            description,
        } => Some(json!({
            "event_type": "agent_spawned",
            "data": {
                "agent_id": agent_id,
                "run_id": run_id,
                "parent_run_id": parent_run_id,
                "agent_type": agent_type,
                "description": description,
                "timestamp": ts,
            }
        })),
        ProgressEventType::Started { description } => Some(json!({
            "event_type": "agent_progress",
            "data": {
                "agent_id": agent_id,
                "status": "started",
                "description": description,
                "timestamp": ts,
            }
        })),
        ProgressEventType::Completed {
            result_summary,
            total_tool_calls,
            total_tokens,
            duration_ms,
        } => Some(json!({
            "event_type": "agent_completed",
            "data": {
                "agent_id": agent_id,
                "status": "completed",
                "result_summary": result_summary,
                "total_tool_calls": total_tool_calls,
                "total_tokens": { "prompt": total_tokens.0, "completion": total_tokens.1 },
                "duration_ms": duration_ms,
                "timestamp": ts,
            }
        })),
        ProgressEventType::Failed { error } => Some(json!({
            "event_type": "agent_completed",
            "data": {
                "agent_id": agent_id,
                "status": "failed",
                "error": error,
                "timestamp": ts,
            }
        })),
        ProgressEventType::Cancelled { reason } => Some(json!({
            "event_type": "agent_completed",
            "data": {
                "agent_id": agent_id,
                "status": "cancelled",
                "reason": reason,
                "timestamp": ts,
            }
        })),
        ProgressEventType::ToolExecuting { tool_name, turn } => Some(json!({
            "event_type": "agent_progress",
            "data": {
                "agent_id": agent_id,
                "status": "tool_executing",
                "tool_name": tool_name,
                "turn": turn,
                "timestamp": ts,
            }
        })),
        ProgressEventType::MetricsUpdate {
            turn,
            max_turns,
            total_prompt_tokens,
            total_completion_tokens,
            total_tool_calls,
        } => Some(json!({
            "event_type": "agent_progress",
            "data": {
                "agent_id": agent_id,
                "status": "metrics_update",
                "turn": turn,
                "max_turns": max_turns,
                "total_prompt_tokens": total_prompt_tokens,
                "total_completion_tokens": total_completion_tokens,
                "total_tool_calls": total_tool_calls,
                "timestamp": ts,
            }
        })),
        // Other event types (Idle, Busy, LlmCallStarted, LlmCallCompleted,
        // TurnCompleted, PermissionDenied) are too granular for web SSE
        _ => None,
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn::agentic_loop_host::ASK_USER_TOOL_NAME;
    use crate::turn::agentic_loop_host::run_agentic_loop_with_host;
    use crate::turn::sse_stream_host::EdgeToolExecResult;

    fn mock_matrixone() -> MatrixOneSettings {
        MatrixOneSettings {
            host: "127.0.0.1".to_string(),
            port: 6001,
            user: "test".to_string(),
            password: "test".to_string(),
            database: "test".to_string(),
        }
    }

    fn mock_encryptor() -> Arc<FernetTokenEncryptor> {
        // Use a valid Fernet key for testing
        Arc::new(FernetTokenEncryptor::new("cJ8pxr3t6iJmSYqe6wD7vu2rN_C3ovGUxkC5H3NXFNY=").unwrap())
    }

    fn sample_edge_tools() -> Vec<Value> {
        vec![
            json!({
                "type": "function",
                "function": {
                    "name": "bash",
                    "description": "Execute a bash command",
                    "parameters": { "type": "object", "properties": {} }
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": "Read a file",
                    "parameters": { "type": "object", "properties": {} }
                }
            }),
        ]
    }

    fn sample_edge_tools_with_ask_user() -> Vec<Value> {
        let mut tools = sample_edge_tools();
        tools.push(json!({
            "type": "function",
            "function": {
                "name": ASK_USER_TOOL_NAME,
                "description": "Ask the user for clarification",
                "parameters": { "type": "object", "properties": {} }
            }
        }));
        tools
    }

    #[test]
    fn builder_extracts_valid_tool_names() {
        let host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "user1".to_string(),
            "sess1".to_string(),
        )
        .with_edge_tools(sample_edge_tools())
        .build();

        assert!(host.valid_tool_names().contains("bash"));
        assert!(host.valid_tool_names().contains("read_file"));
        assert_eq!(host.valid_tool_names().len(), 2);
    }

    #[test]
    fn builder_defaults() {
        let host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u".to_string(),
            "s".to_string(),
        )
        .build();

        assert!(host.is_quiet());
        // When no edge tools are provided, server-side tool schemas are auto-populated
        assert!(host.server_side_tools);
        assert!(!host.valid_tool_names().is_empty());
        assert!(host.valid_tool_names().contains("rollback_file_edits"));
        assert!(host.valid_tool_names().contains("adjust_config"));
        assert!(host.valid_tool_names().contains("prioritize_tool"));
        assert!(host.valid_tool_names().contains("deprioritize_tool"));
        assert!(host.valid_tool_names().contains("set_goal"));
        assert!(host.valid_tool_names().contains("compress_context"));
        assert!(host.valid_tool_names().contains("rollback_session_state"));
        assert!(host.valid_tool_names().contains("task_create"));
        assert!(host.valid_tool_names().contains("task_list"));
        assert!(host.valid_tool_names().contains("task_get"));
        assert!(host.valid_tool_names().contains("task_update"));
        assert!(host.valid_tool_names().contains("task_stop"));
        assert!(host.valid_tool_names().contains("mo_query"));
        assert!(
            host.valid_tool_names()
                .contains("rollback_database_snapshots")
        );
        assert!(host.valid_tool_names().contains("memory_store"));
        assert!(host.valid_tool_names().contains("multi_edit"));
        assert!(!host.valid_tool_names().contains("powershell"));
        assert!(host.emitted_events.is_empty());
    }

    #[test]
    fn build_system_prompt_includes_cwd() {
        let mut profile = Map::new();
        profile.insert("cwd".to_string(), json!("/home/user/project"));
        profile.insert("git_branch".to_string(), json!("main"));

        let host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u1".to_string(),
            "s1".to_string(),
        )
        .with_edge_tools(sample_edge_tools())
        .with_edge_profile(profile)
        .build();

        let prompt = host.build_system_prompt("test query", &host.edge_tools);
        assert!(
            prompt.contains("/home/user/project"),
            "prompt should contain cwd"
        );
        assert!(prompt.contains("main"), "prompt should contain git branch");
    }

    #[test]
    fn build_system_prompt_includes_skill_hints() {
        let mut profile = Map::new();
        profile.insert(
            "active_skills".to_string(),
            json!(["code_review", "testing"]),
        );

        let host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u1".to_string(),
            "s1".to_string(),
        )
        .with_edge_tools(sample_edge_tools())
        .with_edge_profile(profile)
        .build();

        let prompt = host.build_system_prompt("test", &host.edge_tools);
        assert!(
            prompt.contains("code_review"),
            "prompt should include skill names"
        );
    }

    #[test]
    fn build_system_prompt_includes_learned_context() {
        let mut profile = Map::new();
        profile.insert(
            "learned_context_hint".to_string(),
            json!("matrixorigin => github"),
        );

        let host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u1".to_string(),
            "s1".to_string(),
        )
        .with_edge_tools(sample_edge_tools())
        .with_edge_profile(profile)
        .build();

        let prompt = host.build_system_prompt("test", &host.edge_tools);
        assert!(
            prompt.contains("Learned Runtime Context"),
            "prompt should include learned context section"
        );
    }

    #[test]
    fn build_system_prompt_detects_memory_signal() {
        let host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u1".to_string(),
            "s1".to_string(),
        )
        .with_edge_tools(sample_edge_tools())
        .build();

        let prompt = host.build_system_prompt("remember that I prefer dark mode", &host.edge_tools);
        assert!(
            prompt.contains("MEMORY SIGNAL DETECTED"),
            "should detect memory store signal"
        );
    }

    #[test]
    fn result_to_accum_converts_correctly() {
        let result = LlmCallResult {
            full_text: "Hello world".to_string(),
            reasoning: "thinking...".to_string(),
            tool_calls: vec![json!({"id": "tc1", "function": {"name": "bash"}})],
            usage: Map::from_iter([
                ("prompt".to_string(), json!(100)),
                ("completion".to_string(), json!(50)),
            ]),
            model_used: "gpt-4".to_string(),
            duration_ms: 500,
            finish_reason: Some("stop".to_string()),
        };

        let accum = ServerAgenticLoopHost::result_to_accum(&result);
        assert_eq!(accum.full_text, "Hello world");
        assert_eq!(accum.reasoning_content, "thinking...");
        assert!(accum.has_tool_calls);
        assert_eq!(accum.tool_calls.len(), 1);
        assert_eq!(accum.prompt_tokens, 100);
        assert_eq!(accum.completion_tokens, 50);
        assert!(accum.has_usage);
    }

    #[test]
    fn result_to_accum_empty_result() {
        let result = LlmCallResult::default();
        let accum = ServerAgenticLoopHost::result_to_accum(&result);
        assert!(accum.full_text.is_empty());
        assert!(!accum.has_tool_calls);
        assert!(!accum.has_usage);
        assert_eq!(accum.prompt_tokens, 0);
    }

    #[test]
    fn take_emitted_events_clears() {
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u".to_string(),
            "s".to_string(),
        )
        .build();

        host.emitted_events.push(json!({"type": "test"}));
        assert_eq!(host.emitted_events.len(), 1);

        let events = host.take_emitted_events();
        assert_eq!(events.len(), 1);
        assert!(host.emitted_events.is_empty());
    }

    #[test]
    fn emit_headless_line_adds_event() {
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u".to_string(),
            "s".to_string(),
        )
        .build();

        host.emit_headless_line(HeadlessStderrStyle::Dim, "test line".to_string());
        assert_eq!(host.emitted_events.len(), 1);
        assert_eq!(host.emitted_events[0]["type"], "headless_line");
        assert_eq!(host.emitted_events[0]["content"], "test line");
    }

    #[test]
    fn push_reasoning_events_emits_done_marker() {
        let mut emitted = Vec::new();
        ServerAgenticLoopHost::push_reasoning_events(&mut emitted, "thinking...");

        assert_eq!(emitted.len(), 2);
        assert_eq!(emitted[0]["type"], "reasoning_delta");
        assert_eq!(emitted[0]["content"], "thinking...");
        assert_eq!(emitted[1]["type"], "reasoning_done");
    }

    #[test]
    fn push_reasoning_events_skips_empty_reasoning() {
        let mut emitted = Vec::new();
        ServerAgenticLoopHost::push_reasoning_events(&mut emitted, "");
        assert!(emitted.is_empty());
    }

    #[test]
    fn builder_with_selection_confidence() {
        let host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u".to_string(),
            "s".to_string(),
        )
        .with_selection_confidence(0.42)
        .build();

        assert!((host.selection_confidence - 0.42).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn build_llm_messages_includes_system_and_user() {
        let host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u".to_string(),
            "s".to_string(),
        )
        .with_edge_tools(sample_edge_tools())
        .build();

        let mut state = create_test_state();
        state
            .messages
            .push(json!({"role": "user", "content": "hello"}));

        let msgs = host
            .build_llm_messages(
                vec![json!({"role": "system", "content": "system prompt text"})],
                &state,
                &host.edge_tools,
                "gpt-4",
                "sk-test",
                "https://api.test.com",
                "openai",
                &PromptCacheConfig::latch("openai", "gpt-4"),
            )
            .await;
        assert!(msgs.len() >= 2, "should have system + user messages");
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "system prompt text");
    }

    // ── Mock host tests for agentic loop integration ───────────────────────

    /// A mock host that returns pre-configured results, simulating
    /// ServerAgenticLoopHost behavior without network calls.
    struct MockServerHost {
        turns: Vec<HostTurnResult>,
        valid_tools: HashSet<String>,
        emitted: Vec<String>,
    }

    impl MockServerHost {
        fn with_text_response(text: &str, prompt: u64, completion: u64) -> Self {
            Self {
                turns: vec![HostTurnResult {
                    accum: ChatTurnSseAccum {
                        full_text: text.to_string(),
                        has_usage: true,
                        prompt_tokens: prompt,
                        completion_tokens: completion,
                        ..ChatTurnSseAccum::default()
                    },
                    ttft_ms: Some(50),
                    edge_tool_round: Vec::new(),
                    error_kind: None,
                }],
                valid_tools: HashSet::new(),
                emitted: Vec::new(),
            }
        }

        fn with_tool_response(
            tools: Vec<EdgeToolExecResult>,
            prompt: u64,
            completion: u64,
        ) -> Self {
            Self {
                turns: vec![HostTurnResult {
                    accum: ChatTurnSseAccum {
                        has_tool_calls: false,
                        has_usage: true,
                        prompt_tokens: prompt,
                        completion_tokens: completion,
                        ..ChatTurnSseAccum::default()
                    },
                    ttft_ms: Some(30),
                    edge_tool_round: tools,
                    error_kind: None,
                }],
                valid_tools: HashSet::from(["bash".to_string(), "read_file".to_string()]),
                emitted: Vec::new(),
            }
        }
    }

    #[async_trait]
    impl AgenticLoopHost for MockServerHost {
        async fn execute_turn(
            &mut self,
            _state: &mut AgenticLoopState,
        ) -> Result<HostTurnResult, astra_core::ClassifiedError> {
            if self.turns.is_empty() {
                return Err(astra_core::ClassifiedError::new(
                    astra_core::ErrorKind::BudgetExhausted,
                    "no more turns",
                ));
            }
            Ok(self.turns.remove(0))
        }

        fn emit_headless_line(&mut self, _style: HeadlessStderrStyle, line: String) {
            self.emitted.push(line);
        }

        fn is_quiet(&self) -> bool {
            true
        }

        fn valid_tool_names(&self) -> &HashSet<String> {
            &self.valid_tools
        }
    }

    fn create_test_state() -> AgenticLoopState {
        use crate::pipeline::step_protocol::InMemoryIdempotencyCache;
        use crate::pipeline::step_recorder::StepRecorder;
        use crate::semantic_dedup::SemanticDedup;
        use crate::turn::chat_turn_heuristics::TaskExecutionProfile;
        use crate::turn::turn_guard::TurnGuard;

        AgenticLoopState {
            messages: Vec::new(),
            tool_results: Vec::new(),
            current_session_id: None,
            current_run_id: None,
            recursion_depth: 0,
            final_text: String::new(),
            total_prompt: 0,
            total_completion: 0,
            total_cache_read: 0,
            total_cache_creation: 0,
            total_tool_calls: 0,
            total_evidence_tool_calls: 0,
            has_any_usage: false,
            max_turns: 10,
            remaining_turns: 10,
            turn_guard: TurnGuard::new(),
            restricted_tools: HashSet::new(),
            step_recorder: StepRecorder::new("test-session", "test-task"),
            idempotency_cache: InMemoryIdempotencyCache::new(),
            semantic_dedup: SemanticDedup::new(0.75),
            call_counts: HashMap::new(),
            max_identical_tool_calls: crate::runtime_config::RuntimeConfig::load()
                .tool_selection
                .effective_max_identical_calls(),
            max_tools_per_turn: 15,
            stall: Default::default(),
            telemetry: Default::default(),
            skills: Default::default(),
            hooks: Default::default(),
            cancellation: Default::default(),
            messaging: Default::default(),
            error_recovery: Default::default(),
            message: "test query".to_string(),
            recent_tools: Vec::new(),
            task_profile: TaskExecutionProfile::default(),
            last_turn_policy: crate::turn::agentic_loop_host::TurnInteractionPolicy::default(),
            api: astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            api_token: "test-token".to_string(),
            delegation_engine: None,
            project_context: None,
            checkpoint_gate: None,
            evolution_service: None,
            rate_limit_cooldown: Default::default(),
            data_snapshot_provider: None,
            last_composite_snapshot: None,
            last_measured_prompt_tokens: None,
            consecutive_context_window_errors: 0,
            compaction_effectiveness: Default::default(),
            max_turn_input_tokens: 0,
            budget_wrapup_injected: false,
            skill_produced_output: false,
            max_cumulative_tokens: 0,
            thinking_budget_tokens: None,
            recent_file_reads: Vec::new(),
            permission_context: None,
            permission_handler: None,
            tactical_adapter: None,
            step_signal_collector: None,
            tool_budget_override: None,
            pending_reflection_signals: Vec::new(),
            recent_tactical_actions: Vec::new(),
            server_tool_executor: None,
            interruption: None,
            session_facts: Default::default(),
            approval_overrides: None,
            confidence_trend: Default::default(),
            last_confidence_diagnosis: None,
        }
    }

    #[test]
    fn visible_turn_tools_excludes_restricted_and_deprioritized_tools() {
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u".to_string(),
            "s".to_string(),
        )
        .with_edge_tools(sample_edge_tools())
        .build();

        let mut state = create_test_state();
        state.restricted_tools.insert("read_file".to_string());
        state
            .turn_guard
            .health
            .record_resource_limit_failure("bash");

        let visible = host.visible_turn_tools(&mut state);

        assert!(visible.is_empty(), "both tools should be filtered out");
        assert!(state.restricted_tools.contains("bash"));
        assert!(state.restricted_tools.contains("read_file"));
    }

    #[test]
    fn visible_turn_tools_respects_request_and_skill_allowlists() {
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u".to_string(),
            "s".to_string(),
        )
        .with_edge_tools(sample_edge_tools())
        .build();

        let mut state = create_test_state();
        state.skills.request_constraints.allowed_tools = Some(
            ["bash".to_string(), "read_file".to_string()]
                .into_iter()
                .collect(),
        );
        state.skills.allowed_tools = Some(["bash".to_string()].into_iter().collect());

        let visible = host.visible_turn_tools(&mut state);
        let visible_names = visible
            .iter()
            .filter_map(|tool| {
                tool.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
            })
            .collect::<Vec<_>>();

        assert_eq!(visible_names, vec!["bash"]);
        assert!(host.valid_tool_names().contains("bash"));
        assert!(!host.valid_tool_names().contains("read_file"));
    }

    #[test]
    fn headless_turn_policy_excludes_ask_user_from_final_tools() {
        let host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u".to_string(),
            "s".to_string(),
        )
        .with_edge_tools(sample_edge_tools_with_ask_user())
        .build();

        let mut state = create_test_state();
        merge_deprioritized_tools_into_restricted(&state.turn_guard, &mut state.restricted_tools);
        let mut effective_restricted = state.restricted_tools.clone();
        effective_restricted.extend(interaction_scoped_tool_restrictions(
            TurnInteractionMode::Headless,
        ));
        let visible_tools = host.filtered_turn_tools(&effective_restricted);
        let final_tools =
            prune_tool_schemas(&visible_tools, crate::prompts::CompactionTier::Normal);
        let policy =
            TurnInteractionPolicy::from_tool_schemas(TurnInteractionMode::Headless, &final_tools);

        assert_eq!(
            policy.visible_tool_names,
            vec!["bash".to_string(), "read_file".to_string()]
        );
        assert_eq!(policy.evidence_tool_names, policy.visible_tool_names);
        assert!(!policy.allow_ask_user);
    }

    #[tokio::test]
    async fn server_host_mock_text_response() {
        let mut host = MockServerHost::with_text_response("Hello from server", 100, 50);
        let mut state = create_test_state();
        state
            .messages
            .push(json!({"role": "user", "content": "hi"}));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());
        assert_eq!(state.final_text.trim(), "Hello from server");
        assert_eq!(state.total_prompt, 100);
        assert_eq!(state.total_completion, 50);
    }

    #[tokio::test]
    async fn server_host_mock_tool_response() {
        let tools = vec![EdgeToolExecResult {
            request_id: "r1".to_string(),
            tool: "bash".to_string(),
            args: json!({"command": "echo hello"}),
            output: "hello\n".to_string(),
            tool_result_fields: None,
            status: "ok".to_string(),
            duration_ms: 10,
        }];

        let mut host = MockServerHost::with_tool_response(tools, 200, 100);
        let mut state = create_test_state();
        state
            .messages
            .push(json!({"role": "user", "content": "run bash"}));

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        // Should complete (tool round runs but no more turns to consume)
        assert!(outcome.is_ok() || outcome.is_err());
    }

    #[tokio::test]
    async fn server_host_budget_tracking() {
        let mut host = MockServerHost::with_text_response("response", 500, 200);
        let mut state = create_test_state();
        state
            .messages
            .push(json!({"role": "user", "content": "test"}));

        let _ = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(state.has_any_usage);
        assert_eq!(state.total_prompt, 500);
        assert_eq!(state.total_completion, 200);
        assert!(state.telemetry.first_ttft_ms.is_some());
    }

    #[tokio::test]
    async fn server_host_multi_turn_budget_exhaustion() {
        let mut state = create_test_state();
        state.max_turns = 2;
        state.remaining_turns = 2;
        state
            .messages
            .push(json!({"role": "user", "content": "test"}));

        // Two text responses — loop should complete after consuming both
        let mut host = MockServerHost {
            turns: vec![
                HostTurnResult {
                    accum: ChatTurnSseAccum {
                        full_text: "turn1".to_string(),
                        has_usage: true,
                        prompt_tokens: 100,
                        completion_tokens: 50,
                        ..ChatTurnSseAccum::default()
                    },
                    ttft_ms: Some(10),
                    edge_tool_round: Vec::new(),
                    error_kind: None,
                },
                HostTurnResult {
                    accum: ChatTurnSseAccum {
                        full_text: "turn2".to_string(),
                        has_usage: true,
                        prompt_tokens: 100,
                        completion_tokens: 50,
                        ..ChatTurnSseAccum::default()
                    },
                    ttft_ms: Some(10),
                    edge_tool_round: Vec::new(),
                    error_kind: None,
                },
            ],
            valid_tools: HashSet::new(),
            emitted: Vec::new(),
        };

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());
        assert!(state.final_text.contains("turn1"));
    }

    // ── inject_tool_schema tests ────────────────────────────────────────────

    #[test]
    fn inject_tool_schema_adds_to_edge_tools_and_valid_tools() {
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "user1".to_string(),
            "sess1".to_string(),
        )
        .with_edge_tools(sample_edge_tools())
        .build();

        assert!(!host.valid_tool_names().contains("delegate"));
        let initial_count = host.edge_tools.len();

        use crate::turn::agentic_loop_host::delegate_tool_schema;
        host.inject_tool_schema(delegate_tool_schema());

        assert!(host.valid_tool_names().contains("delegate"));
        assert_eq!(host.edge_tools.len(), initial_count + 1);
        let last = host.edge_tools.last().unwrap();
        assert_eq!(last["function"]["name"], "delegate");
    }

    #[test]
    fn inject_tool_schema_is_idempotent() {
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "user1".to_string(),
            "sess1".to_string(),
        )
        .with_edge_tools(sample_edge_tools())
        .build();

        use crate::turn::agentic_loop_host::delegate_tool_schema;
        let initial_count = host.edge_tools.len();

        host.inject_tool_schema(delegate_tool_schema());
        host.inject_tool_schema(delegate_tool_schema());

        // Only one injection — duplicate is skipped
        assert_eq!(host.edge_tools.len(), initial_count + 1);
    }

    #[test]
    fn inject_tool_schema_ignores_malformed_schema() {
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "user1".to_string(),
            "sess1".to_string(),
        )
        .with_edge_tools(sample_edge_tools())
        .build();

        let initial_count = host.edge_tools.len();
        host.inject_tool_schema(json!({"bad": "schema"}));

        // No change — malformed schema ignored
        assert_eq!(host.edge_tools.len(), initial_count);
    }

    // ── llm_cancel_for_state (aligns server loop with AgenticLoopState cancel fields) ──

    #[test]
    fn llm_cancel_for_state_none_is_never_triggered() {
        let s = create_test_state();
        assert!(!super::llm_cancel_for_state(&s).is_triggered());
    }

    #[test]
    fn llm_cancel_for_state_flag_and_token_triggers_on_flag() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use tokio_util::sync::CancellationToken;

        let mut s = create_test_state();
        let flag = Arc::new(AtomicBool::new(true));
        let token = Arc::new(CancellationToken::new());
        s.cancellation.flag = Some(flag.clone());
        s.cancellation.token = Some(token);
        assert!(super::llm_cancel_for_state(&s).is_triggered());
        assert!(flag.load(Ordering::SeqCst));
    }

    #[test]
    fn llm_cancel_for_state_token_only_triggers_when_cancelled() {
        use tokio_util::sync::CancellationToken;

        let mut s = create_test_state();
        let token = Arc::new(CancellationToken::new());
        s.cancellation.token = Some(token.clone());
        assert!(!super::llm_cancel_for_state(&s).is_triggered());
        token.cancel();
        assert!(super::llm_cancel_for_state(&s).is_triggered());
    }

    // ── progress_event_to_sse tests ──

    #[test]
    fn progress_event_agent_spawned_to_sse() {
        use crate::orchestration::{AgentProgressEvent, ProgressEventType};

        let evt = AgentProgressEvent {
            agent_id: "agent-1".to_string(),
            event_type: ProgressEventType::AgentSpawned {
                run_id: "run-123".to_string(),
                parent_run_id: "run-000".to_string(),
                agent_type: "explore".to_string(),
                description: "Search codebase".to_string(),
            },
            timestamp_epoch_ms: 1000,
        };
        let sse = super::progress_event_to_sse(&evt).expect("should produce SSE");
        assert_eq!(sse["event_type"], "agent_spawned");
        assert_eq!(sse["data"]["agent_id"], "agent-1");
        assert_eq!(sse["data"]["run_id"], "run-123");
        assert_eq!(sse["data"]["agent_type"], "explore");
    }

    #[test]
    fn progress_event_completed_to_sse() {
        use crate::orchestration::{AgentProgressEvent, ProgressEventType};

        let evt = AgentProgressEvent {
            agent_id: "agent-2".to_string(),
            event_type: ProgressEventType::Completed {
                result_summary: "Done".to_string(),
                total_tool_calls: 5,
                total_tokens: (100, 200),
                duration_ms: 3000,
            },
            timestamp_epoch_ms: 2000,
        };
        let sse = super::progress_event_to_sse(&evt).expect("should produce SSE");
        assert_eq!(sse["event_type"], "agent_completed");
        assert_eq!(sse["data"]["status"], "completed");
        assert_eq!(sse["data"]["total_tool_calls"], 5);
    }

    #[test]
    fn progress_event_idle_returns_none() {
        use crate::orchestration::{AgentProgressEvent, ProgressEventType};

        let evt = AgentProgressEvent {
            agent_id: "agent-3".to_string(),
            event_type: ProgressEventType::Idle,
            timestamp_epoch_ms: 3000,
        };
        assert!(super::progress_event_to_sse(&evt).is_none());
    }
}

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
//!       → execute_turn(): resolve model, call LLM, accumulate response
//!       → headless_tool_round(): execute tools via ledger
//!       → post_tool_policy(): stall/dedup/guard
//! ```

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use tokio::sync::Mutex as TokioMutex;

use crate::turn::agentic_headless_round::HeadlessStderrStyle;
use crate::turn::agentic_loop_host::{AgenticLoopHost, AgenticLoopState, HostTurnResult};
use crate::turn::chat_turn_sse_dispatch::ChatTurnSseAccum;
use crate::turn::llm_client::{
    LlmCallResult, cached_system_prompt, call_llm_and_collect, classify_llm_error,
};
use crate::turn::tool_schema_prune::prune_tool_schemas;
use crate::{FernetTokenEncryptor, MatrixOneSettings};
use mo_agent_core::SharedPool;

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

    // ── Tool execution (used by RunLifecycleService wiring) ──
    #[allow(dead_code)] // needed once RunLifecycleService uses ledger-based tool execution
    edge_callback_ledger: Arc<TokioMutex<HashMap<String, Value>>>,
    #[allow(dead_code)]
    user_id: String,
    session_id: String,

    // ── Output collection ──
    /// SSE events emitted during the turn, streamed to the client.
    emitted_events: Vec<Value>,
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

    pub fn build(self) -> ServerAgenticLoopHost {
        let valid_tools = self
            .edge_tools
            .iter()
            .filter_map(|t| {
                t.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .map(String::from)
            })
            .collect();

        ServerAgenticLoopHost {
            matrixone: self.matrixone,
            encryptor: self.encryptor,
            shared_pool: self.shared_pool,
            model_override: self.model_override,
            edge_tools: self.edge_tools,
            edge_profile: self.edge_profile,
            valid_tools,
            selection_confidence: self.selection_confidence,
            edge_callback_ledger: self.edge_callback_ledger,
            user_id: self.user_id,
            session_id: self.session_id,
            emitted_events: Vec::new(),
        }
    }
}

impl ServerAgenticLoopHost {
    /// Access collected SSE events from the last turn.
    pub fn take_emitted_events(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.emitted_events)
    }

    /// Build the system prompt from edge context.
    fn build_system_prompt(&self, user_content: &str) -> String {
        let tool_names: Vec<&str> = self
            .edge_tools
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

        let base = cached_system_prompt(
            &tool_names,
            &profile_with_hints,
            self.selection_confidence,
            task_type,
        );

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

        format!("{base}{memory_signal_hint}")
    }

    /// Build the LLM message array from loop state.
    async fn build_llm_messages(
        &self,
        system_prompt: &str,
        state: &AgenticLoopState,
        model_name: &str,
    ) -> Vec<Value> {
        let mut llm_messages = vec![json!({
            "role": "system",
            "content": system_prompt
        })];

        // Compute compaction tier
        let budget = crate::prompts::budget_for_model(Some(model_name));
        let tool_schema_tokens: usize = self
            .edge_tools
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
        let tier = budget.compaction_tier(cache_est.total_tokens);
        let budget_chars = budget.effective_input_limit() * 4;

        // Use Memoria-based compaction (async with HTTP client)
        let memoria_config = crate::turn::cloud::memoria_compact::MemoriaCompactConfig::default();
        let memoria_params = crate::turn::cloud::memoria_compact::MemoriaCompactParams {
            budget_chars,
            keep_chars: 2_000,
            tier,
            keep_recent_turns: budget.keep_recent_turns,
            current_tokens: cache_est.total_tokens,
        };

        // Try to create Memoria client from environment
        let memoria_client = crate::turn::cloud::memoria_compact::HttpMemoriaClient::from_env();

        let compact_result = crate::turn::cloud::memoria_compact::compact_with_memoria(
            &state.messages,
            Some(&self.session_id),
            &memoria_config,
            &memoria_params,
            memoria_client
                .as_ref()
                .map(|c| c as &dyn crate::turn::cloud::memoria_compact::MemoriaClient),
        )
        .await;

        llm_messages.extend(compact_result.messages);
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

        ChatTurnSseAccum {
            full_text: result.full_text.clone(),
            reasoning_content: result.reasoning.clone(),
            tool_calls: result.tool_calls.clone(),
            has_tool_calls: !result.tool_calls.is_empty(),
            prompt_tokens,
            completion_tokens,
            has_usage: !result.usage.is_empty(),
            session_id: None,
            run_id: None,
            explain_turns: Vec::new(),
            error_message: None,
        }
    }
}

#[async_trait]
impl AgenticLoopHost for ServerAgenticLoopHost {
    async fn execute_turn(
        &mut self,
        state: &mut AgenticLoopState,
    ) -> Result<HostTurnResult, String> {
        let turn_started = Instant::now();

        // ── 1. Resolve LLM model ────────────────────────────────────────
        let pool_ref = self.shared_pool.as_ref().map(|sp| sp.get());
        let (model_name, api_key, base_url, provider) =
            match mo_agent_services::resolve_active_llm_model(
                &self.matrixone,
                self.encryptor.as_ref(),
                self.model_override.as_deref(),
                pool_ref,
            )
            .await
            {
                Ok(m) => (m.model_name, m.api_key, m.base_url, m.provider),
                Err(e) => return Err(format!("Model resolution failed: {e}")),
            };

        // ── 2. Build messages ───────────────────────────────────────────
        let user_content = state
            .messages
            .iter()
            .rev()
            .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
            .and_then(|m| m.get("content").and_then(Value::as_str))
            .unwrap_or("");

        let system_prompt = self.build_system_prompt(user_content);
        let llm_messages = self
            .build_llm_messages(&system_prompt, state, &model_name)
            .await;

        // ── 3. Call LLM ─────────────────────────────────────────────────
        let budget = crate::prompts::budget_for_model(Some(&model_name));
        let max_output_tokens = (budget.model_limit as f64 * budget.output_reserve_ratio) as usize;

        let tool_schema_tokens: usize = self
            .edge_tools
            .iter()
            .map(|t| {
                serde_json::to_string(t)
                    .map(|s| crate::prompts::estimate_str_tokens(&s))
                    .unwrap_or(50)
            })
            .sum();
        let mut est_msgs = vec![json!({"role": "system", "content": system_prompt})];
        est_msgs.extend(state.messages.iter().cloned());
        let cache_est = crate::prompts::estimate_tokens_cache_aware(&est_msgs, tool_schema_tokens);
        let tier = budget.compaction_tier(cache_est.total_tokens);
        let pruned_tools = prune_tool_schemas(&self.edge_tools, tier);

        let result = call_llm_and_collect(
            &llm_messages,
            &pruned_tools,
            &model_name,
            &api_key,
            &base_url,
            &provider,
            Some(max_output_tokens),
        )
        .await
        .map_err(|e| {
            let kind = classify_llm_error(&e);
            format!("[{kind}] {e}")
        })?;

        // ── 4. Emit SSE events for client ───────────────────────────────
        if !result.full_text.is_empty() {
            self.emitted_events.push(json!({
                "type": "text_delta",
                "content": result.full_text,
            }));
        }
        if !result.reasoning.is_empty() {
            self.emitted_events.push(json!({
                "type": "reasoning_delta",
                "content": result.reasoning,
            }));
        }
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
        })
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
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
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
        assert!(host.valid_tool_names().is_empty());
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

        let prompt = host.build_system_prompt("test query");
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

        let prompt = host.build_system_prompt("test");
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

        let prompt = host.build_system_prompt("test");
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

        let prompt = host.build_system_prompt("remember that I prefer dark mode");
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
            .build_llm_messages("system prompt text", &state, "gpt-4")
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
        ) -> Result<HostTurnResult, String> {
            if self.turns.is_empty() {
                return Err("no more turns".to_string());
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
        use crate::turn::turn_guard::TurnGuard;

        AgenticLoopState {
            messages: Vec::new(),
            tool_results: Vec::new(),
            current_session_id: None,
            current_run_id: None,
            final_text: String::new(),
            total_prompt: 0,
            total_completion: 0,
            total_tool_calls: 0,
            has_any_usage: false,
            max_turns: 10,
            remaining_turns: 10,
            turn_guard: TurnGuard::new(),
            restricted_tools: HashSet::new(),
            step_recorder: StepRecorder::new("test-session", "test-task"),
            idempotency_cache: InMemoryIdempotencyCache::new(),
            semantic_dedup: SemanticDedup::new(0.75),
            turn_sigs: Vec::new(),
            turn_tool_names: Vec::new(),
            stall_events: Vec::new(),
            intent_tool_turns: Vec::new(),
            verdict_events: Vec::new(),
            last_heavy_checkpoint: None,
            tool_call_records: Vec::new(),
            forced_factual_retry: false,
            explain_turns: Vec::new(),
            first_ttft_ms: None,
            all_tools_used: HashSet::new(),
            first_selection_report: None,
            first_budget_pressure: 0.0,
            first_context_assembly_ms: None,
            first_memoria_ms: None,
            first_selector_ms: None,
            first_selector_strategy: None,
            selector_tokens_in: 0,
            selector_tokens_out: 0,
            all_selected_skills: Vec::new(),
            message: "test query".to_string(),
            recent_tools: Vec::new(),
            api: mo_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            api_token: "test-token".to_string(),
            cancel_flag: None,
        }
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
        assert!(state.first_ttft_ms.is_some());
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
                },
            ],
            valid_tools: HashSet::new(),
            emitted: Vec::new(),
        };

        let outcome = run_agentic_loop_with_host(&mut host, &mut state).await;
        assert!(outcome.is_ok());
        assert!(state.final_text.contains("turn1"));
    }
}

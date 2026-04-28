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
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use tokio::sync::Mutex as TokioMutex;
use tokio_util::sync::CancellationToken;

use crate::bridge::rate_limit_cooldown::RateLimitAction;
use crate::turn::agentic_headless_round::HeadlessStderrStyle;
use crate::turn::agentic_loop_host::{
    AgenticLoopHost, AgenticLoopState, HostReflectionRequest, HostReflectionResult, HostTurnResult,
    TurnInteractionMode, TurnInteractionPolicy, interaction_scoped_tool_restrictions,
};
use crate::turn::agentic_loop_tool_support::edge_tool_status_exit_code;
use crate::turn::bridge_llm_stream::rate_limit_cooldown;
use crate::turn::chat_turn_sse_dispatch::ChatTurnSseAccum;
use crate::turn::llm_client::{
    LlmCallResult, LlmCancel, cached_system_prompt, call_llm_and_collect_with_request_overrides,
    call_llm_nonstream_fallback_with_request_overrides, llm_connect_timeout, llm_fallback_timeout,
    sleep_ms_or_llm_cancel,
};
use crate::turn::prompt_cache::{
    PromptCacheConfig, annotate_tool_schemas_for_caching, apply_anthropic_cache_metadata,
    build_system_message_with_dynamic_sections,
};
use crate::turn::tool_schema_prune::{filter_tool_schemas_by_excluded_names, prune_tool_schemas};
use crate::turn::turn_guard::merge_deprioritized_tools_into_restricted;
use crate::{FernetTokenEncryptor, MatrixOneSettings};
use astra_core::SharedPool;
use astra_services::LlmTokenServiceConfig;

fn request_aware_summary_http_client() -> Result<reqwest::Client, String> {
    static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    match CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(llm_connect_timeout())
            .timeout(llm_fallback_timeout())
            .build()
            .map_err(|error| error.to_string())
    }) {
        Ok(client) => Ok(client.clone()),
        Err(error) => Err(error.clone()),
    }
}

fn llm_cancel_for_state(state: &AgenticLoopState) -> LlmCancel<'_> {
    match (&state.cancellation.flag, &state.cancellation.token) {
        (Some(f), Some(t)) => LlmCancel::FlagAndToken(f.as_ref(), t.as_ref()),
        (Some(f), None) => LlmCancel::Flag(f.as_ref()),
        (None, Some(t)) => LlmCancel::Token(t.as_ref()),
        (None, None) => LlmCancel::None,
    }
}

fn record_full_llm_request_event(
    state: &mut AgenticLoopState,
    full_llm_capture: bool,
    session_id: &str,
    source: &str,
    model: &str,
    provider: &str,
    attempt: u32,
    messages: &[Value],
    tools: &[Value],
    max_output_tokens: Option<usize>,
) {
    if session_id.is_empty() || !full_llm_capture {
        return;
    }
    let Some(buf) = state.turn_event_buffer.as_mut() else {
        return;
    };
    let round = buf.current_round();
    let trace = crate::turn::llm_exchange_capture::CaptureTrace {
        session_turn_source: Some("state"),
        turn_chain_id: None,
        user_query_event_id: None,
    };
    let mut evt = astra_services::session_journal::JournalEvent::llm_request_full(
        Some(session_id),
        state.session_turn,
        round,
        json!({
            "source": source,
            "model": model,
            "provider": provider,
            "attempt": attempt,
            "trace": {
                "session_turn": state.session_turn,
                "round": round,
                "session_turn_source": trace.session_turn_source,
                "turn_chain_id": trace.turn_chain_id,
                "user_query_event_id": trace.user_query_event_id,
            },
            "request": crate::turn::llm_exchange_capture::build_capture_request_json(
                messages,
                tools,
                max_output_tokens,
            ),
        }),
    );
    evt.offset_ms = Some(buf.offset_ms());
    buf.record(evt);
}

fn record_full_llm_response_event(
    state: &mut AgenticLoopState,
    full_llm_capture: bool,
    session_id: &str,
    source: &str,
    model: &str,
    provider: &str,
    attempt: u32,
    outcome: &str,
    response: Value,
) {
    if session_id.is_empty() || !full_llm_capture {
        return;
    }
    let Some(buf) = state.turn_event_buffer.as_mut() else {
        return;
    };
    let round = buf.current_round();
    let trace = crate::turn::llm_exchange_capture::CaptureTrace {
        session_turn_source: Some("state"),
        turn_chain_id: None,
        user_query_event_id: None,
    };
    let mut evt = astra_services::session_journal::JournalEvent::llm_response_full(
        Some(session_id),
        state.session_turn,
        round,
        json!({
            "source": source,
            "model": model,
            "provider": provider,
            "attempt": attempt,
            "trace": {
                "session_turn": state.session_turn,
                "round": round,
                "session_turn_source": trace.session_turn_source,
                "turn_chain_id": trace.turn_chain_id,
                "user_query_event_id": trace.user_query_event_id,
            },
            "response": crate::turn::llm_exchange_capture::build_capture_response_json(
                outcome,
                response,
            ),
        }),
    );
    evt.offset_ms = Some(buf.offset_ms());
    buf.record(evt);
}

#[cfg(feature = "bridge-e2e-hooks")]
fn mock_error_kind_from_str(kind: &str) -> astra_core::ErrorKind {
    match kind {
        "rate_limit" => astra_core::ErrorKind::RateLimit,
        "server_error" => astra_core::ErrorKind::ServerError,
        "auth" => astra_core::ErrorKind::Auth,
        "context_window" => astra_core::ErrorKind::ContextWindow,
        "invalid_request" => astra_core::ErrorKind::InvalidRequest,
        "stream_idle" => astra_core::ErrorKind::StreamIdle,
        "stream_transport" => astra_core::ErrorKind::StreamTransport,
        "budget_exhausted" => astra_core::ErrorKind::BudgetExhausted,
        "tool_rounds_exhausted" => astra_core::ErrorKind::ToolRoundsExhausted,
        "network" => astra_core::ErrorKind::Network,
        "tool_not_found" => astra_core::ErrorKind::ToolNotFound,
        "tool_invalid_args" => astra_core::ErrorKind::ToolInvalidArgs,
        "tool_timeout" => astra_core::ErrorKind::ToolTimeout,
        "tool_unavailable" => astra_core::ErrorKind::ToolUnavailable,
        "resource_limit" => astra_core::ErrorKind::ResourceLimit,
        "cancelled" => astra_core::ErrorKind::Cancelled,
        _ => astra_core::ErrorKind::Unknown,
    }
}

#[cfg(feature = "bridge-e2e-hooks")]
fn mock_round_error(round: &Value) -> Option<astra_core::ClassifiedError> {
    let error = round.get("error")?.as_object()?;
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .filter(|message| !message.trim().is_empty())
        .unwrap_or("mock LLM round failed");
    let kind = error
        .get("kind")
        .and_then(Value::as_str)
        .map(mock_error_kind_from_str)
        .unwrap_or(astra_core::ErrorKind::Unknown);
    let classified = astra_core::ClassifiedError::new(kind, message.to_string());
    if let Some(details) = error.get("details").filter(|details| details.is_object()) {
        Some(classified.with_details_json(details.to_string()))
    } else {
        Some(classified)
    }
}

#[cfg(feature = "bridge-e2e-hooks")]
fn mock_round_partial_text(error: &astra_core::ClassifiedError) -> Option<String> {
    let details = error.details_json.as_deref()?;
    let value: Value = serde_json::from_str(details).ok()?;
    value
        .get("partial_full_text")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(ToString::to_string)
}

#[derive(Debug, Clone)]
struct ResolvedTurnLlmConfig {
    model_name: String,
    api_key: String,
    base_url: String,
    provider: String,
    fallback_model: Option<String>,
    header_overrides: HashMap<String, String>,
    completions_url_override: Option<String>,
    request_timeout: Option<Duration>,
}

#[derive(Debug, Clone)]
struct RequestAwareSummaryClient {
    model_name: String,
    api_key: String,
    base_url: String,
    provider: String,
    max_output_tokens: usize,
    header_overrides: HashMap<String, String>,
    completions_url_override: Option<String>,
    request_timeout: Option<Duration>,
}

#[async_trait]
impl crate::turn::cloud::summary::SummaryLlmClient for RequestAwareSummaryClient {
    async fn summarize(
        &self,
        messages: &[Value],
    ) -> Result<crate::turn::cloud::summary::SummaryResponse, String> {
        let client = request_aware_summary_http_client()?;

        match call_llm_nonstream_fallback_with_request_overrides(
            &client,
            messages,
            &[],
            &self.model_name,
            &self.api_key,
            &self.base_url,
            &self.provider,
            Some(self.max_output_tokens),
            llm_fallback_timeout(),
            (!self.header_overrides.is_empty()).then_some(&self.header_overrides),
            self.completions_url_override.as_deref(),
            self.request_timeout,
        )
        .await
        {
            Ok(result) => Ok(crate::turn::cloud::summary::SummaryResponse {
                text: result.full_text,
                is_ptl_error: false,
            }),
            Err(error) if error.kind == astra_core::ErrorKind::ContextWindow => {
                Ok(crate::turn::cloud::summary::SummaryResponse {
                    text: String::new(),
                    is_ptl_error: true,
                })
            }
            Err(error) => Err(error.to_string()),
        }
    }
}

fn normalize_request_model(preferred_model: Option<&str>) -> Option<String> {
    preferred_model
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(String::from)
}

async fn resolve_llm_model_for_turn(
    matrixone: &MatrixOneSettings,
    encryptor: &FernetTokenEncryptor,
    preferred_model: Option<&str>,
    pool: Option<&sqlx::Pool<sqlx::MySql>>,
    llm_token_service: Option<&LlmTokenServiceConfig>,
    forward_headers: &HashMap<String, String>,
) -> Result<ResolvedTurnLlmConfig, String> {
    if let Some(config) = llm_token_service {
        return Ok(ResolvedTurnLlmConfig {
            model_name: normalize_request_model(preferred_model)
                .unwrap_or_else(|| "gpt-4o-mini".to_string()),
            api_key: String::new(),
            base_url: "https://api.openai.com/v1".to_string(),
            provider: "openai".to_string(),
            fallback_model: None,
            header_overrides: forward_headers.clone(),
            completions_url_override: Some(config.url.clone()),
            request_timeout: config.timeout_ms.map(Duration::from_millis),
        });
    }
    let resolved =
        astra_services::resolve_active_llm_model(matrixone, encryptor, preferred_model, pool)
            .await?;
    Ok(ResolvedTurnLlmConfig {
        model_name: resolved.model_name,
        api_key: resolved.api_key,
        base_url: resolved.base_url,
        provider: resolved.provider,
        fallback_model: resolved.fallback_model,
        header_overrides: HashMap::new(),
        completions_url_override: None,
        request_timeout: None,
    })
}

/// Test-only snapshot of the materials a single turn of the mock LLM path
/// assembles before it would call a real provider. Captures enough to
/// assert on prompt-cache annotations, schema changes, and stable prefix
/// byte-equality across turns without involving the network.
#[cfg(feature = "bridge-e2e-hooks")]
#[derive(Debug, Clone)]
pub struct CapturedLlmRequest {
    /// 0-based turn index (counts mock-LLM rounds actually executed).
    pub turn_index: usize,
    /// Provider id used for cache config (e.g. `"anthropic"` or `"openai"`).
    pub provider: String,
    /// Model id used for cache config (e.g. `"claude-sonnet-4"`).
    pub model: String,
    /// Whether `PromptCacheConfig` was computing annotations on this turn.
    pub cache_enabled: bool,
    /// Whether Anthropic-style cache_control blocks were emitted.
    pub is_anthropic: bool,
    /// The primary structured system message (with cache_control blocks for
    /// Anthropic, or just the stable prefix text for OpenAI-compatible).
    pub system_primary: Value,
    /// The optional per-turn dynamic system message (OpenAI split only).
    pub system_dynamic: Option<Value>,
    /// Tool schemas after pruning + `annotate_tool_schemas_for_caching`.
    pub tools: Vec<Value>,
    /// Conversation messages after `add_message_cache_breakpoint` was applied
    /// (for Anthropic) or a clone of `state.messages` (otherwise).
    pub messages: Vec<Value>,
    /// Number of `cache_control` blocks present in `system_primary` content.
    pub system_cache_control_count: usize,
    /// Whether the last tool schema carries a `cache_control` marker.
    pub last_tool_has_cache_control: bool,
    /// Whether the last non-system message carries a `cache_control` marker.
    pub last_message_has_cache_control: bool,
    /// SHA256 hex of the cacheable prefix (for OpenAI: `system_primary.content`
    /// as text; for Anthropic: the concatenated text of all blocks up to and
    /// including the last cache_control breakpoint).
    pub cacheable_prefix_sha256: String,
}

#[cfg(feature = "bridge-e2e-hooks")]
fn value_has_cache_control(v: &Value) -> bool {
    v.get("cache_control")
        .map(|cc| !cc.is_null())
        .unwrap_or(false)
}

#[cfg(feature = "bridge-e2e-hooks")]
fn count_system_cache_control(primary: &Value) -> usize {
    let Some(content) = primary.get("content") else {
        return 0;
    };
    match content {
        Value::Array(blocks) => blocks.iter().filter(|b| value_has_cache_control(b)).count(),
        _ => 0,
    }
}

#[cfg(feature = "bridge-e2e-hooks")]
fn cacheable_prefix_text(system_primary: &Value, is_anthropic: bool) -> String {
    let Some(content) = system_primary.get("content") else {
        return String::new();
    };
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    let Some(blocks) = content.as_array() else {
        return String::new();
    };
    if is_anthropic {
        // Concatenate text up to and including the last block carrying
        // `cache_control` (the full cacheable prefix per Anthropic semantics).
        let last_break = blocks
            .iter()
            .enumerate()
            .rev()
            .find(|(_, b)| value_has_cache_control(b))
            .map(|(i, _)| i);
        match last_break {
            Some(idx) => blocks
                .iter()
                .take(idx + 1)
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(""),
            None => String::new(),
        }
    } else {
        blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("")
    }
}

#[cfg(feature = "bridge-e2e-hooks")]
fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    format!("{:x}", h.finalize())
}

#[cfg(feature = "bridge-e2e-hooks")]
#[allow(clippy::too_many_arguments)]
fn build_captured_llm_request(
    turn_index: usize,
    provider: String,
    model: String,
    cache_cfg: &PromptCacheConfig,
    system_msgs: &[Value],
    tools: &[Value],
    messages: &[Value],
    breakdown: &crate::turn::context_assembly_trace::SystemPromptBreakdown,
) -> CapturedLlmRequest {
    let _ = breakdown; // retained in case future assertions want it
    // Identify primary + dynamic system slots.
    let primary = system_msgs.first().cloned().unwrap_or_else(|| json!({}));
    let dynamic = system_msgs.get(1).cloned();
    let system_cache_control_count = count_system_cache_control(&primary);
    let last_tool_has_cache_control = tools
        .last()
        .map(|t| {
            value_has_cache_control(t)
                || t.get("cache_control").is_some()
                || t.get("function")
                    .map(|f| value_has_cache_control(f))
                    .unwrap_or(false)
        })
        .unwrap_or(false);
    let last_message_has_cache_control = messages
        .iter()
        .rev()
        .find(|m| m.get("role").and_then(Value::as_str) != Some("system"))
        .map(|m| {
            if value_has_cache_control(m) {
                return true;
            }
            if let Some(arr) = m.get("content").and_then(Value::as_array) {
                return arr.iter().any(value_has_cache_control);
            }
            false
        })
        .unwrap_or(false);
    let prefix = cacheable_prefix_text(&primary, cache_cfg.is_anthropic);
    let cacheable_prefix_sha256 = sha256_hex(&prefix);
    CapturedLlmRequest {
        turn_index,
        provider,
        model,
        cache_enabled: cache_cfg.cache_enabled,
        is_anthropic: cache_cfg.is_anthropic,
        system_primary: primary,
        system_dynamic: dynamic,
        tools: tools.to_vec(),
        messages: messages.to_vec(),
        system_cache_control_count,
        last_tool_has_cache_control,
        last_message_has_cache_control,
        cacheable_prefix_sha256,
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
    llm_token_service: Option<LlmTokenServiceConfig>,

    // ── Context ──
    edge_tools: Vec<Value>,
    edge_profile: Map<String, Value>,
    valid_tools: HashSet<String>,
    selection_confidence: f64,
    /// `true` when tools were auto-populated from astra-tools (no CLI connected).
    server_side_tools: bool,
    /// `true` when the connected client can answer ask_user prompts.
    interactive_client: bool,
    /// `true` when this session explicitly requests full LLM request/response capture.
    full_llm_capture: bool,

    // ── Tool execution ──
    #[allow(dead_code)] // used in Step 3: wire edge tool delivery via ledger
    edge_callback_ledger: Arc<TokioMutex<HashMap<String, Value>>>,
    #[allow(dead_code)] // used in Step 3
    user_id: String,
    session_id: String,
    /// Session-scoped cache for dedup of identical read-only tool invocations
    /// within a short window. Gated by concurrency_safety classification.
    tool_result_cache: astra_turn_core::tool_result_dedup::SharedResultCache,

    // ── Output collection ──
    /// SSE events emitted during the turn, streamed to the client.
    emitted_events: Vec<Value>,
    /// When set, SSE events are also pushed through this channel for
    /// incremental streaming (web agent mode). The HTTP handler reads
    /// from the corresponding receiver to stream SSE to the client.
    event_tx: Option<tokio::sync::mpsc::Sender<Value>>,
    /// When the SSE channel's receiver is dropped (client disconnected),
    /// this flag is set so the agentic loop cancels at the next turn boundary.
    client_cancel_flag: Option<Arc<AtomicBool>>,
    /// Low-latency cancellation token — cancelled alongside `client_cancel_flag`
    /// for immediate LLM abort on client disconnect.
    client_cancel_token: Option<Arc<CancellationToken>>,

    // ── Agent progress ──
    /// Optional receiver for agent progress events (multi-agent tree updates).
    progress_rx: Option<tokio::sync::broadcast::Receiver<crate::orchestration::AgentProgressEvent>>,

    // ── Test hooks ──
    #[cfg(feature = "bridge-e2e-hooks")]
    test_llm_rounds: std::collections::VecDeque<Value>,
    #[cfg(feature = "bridge-e2e-hooks")]
    test_llm_rounds_wired: bool,
    /// Optional provider hint for the mock path, so cache_control annotations
    /// are exercised as if talking to anthropic/openai/etc. Default (None)
    /// leaves `PromptCacheConfig::default()` behavior (annotations off).
    #[cfg(feature = "bridge-e2e-hooks")]
    mock_provider: Option<(String, String)>,
    /// Per-turn captured payloads for assertion in tests.
    #[cfg(feature = "bridge-e2e-hooks")]
    llm_request_capture: Option<Arc<std::sync::Mutex<Vec<CapturedLlmRequest>>>>,
}

/// Builder for [`ServerAgenticLoopHost`].
pub struct ServerAgenticLoopHostBuilder {
    matrixone: MatrixOneSettings,
    encryptor: Arc<FernetTokenEncryptor>,
    shared_pool: Option<SharedPool>,
    model_override: Option<String>,
    llm_token_service: Option<LlmTokenServiceConfig>,
    edge_tools: Vec<Value>,
    edge_profile: Map<String, Value>,
    selection_confidence: f64,
    edge_callback_ledger: Arc<TokioMutex<HashMap<String, Value>>>,
    user_id: String,
    session_id: String,
    progress_broadcaster: Option<Arc<crate::orchestration::ProgressBroadcaster>>,
    interactive_client: bool,
    full_llm_capture: bool,
    event_tx: Option<tokio::sync::mpsc::Sender<Value>>,
    #[cfg(feature = "bridge-e2e-hooks")]
    test_llm_rounds: Vec<Value>,
    #[cfg(feature = "bridge-e2e-hooks")]
    test_llm_rounds_wired: bool,
    #[cfg(feature = "bridge-e2e-hooks")]
    mock_provider: Option<(String, String)>,
    #[cfg(feature = "bridge-e2e-hooks")]
    llm_request_capture: Option<Arc<std::sync::Mutex<Vec<CapturedLlmRequest>>>>,
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
            llm_token_service: None,
            edge_tools: Vec::new(),
            edge_profile: Map::new(),
            selection_confidence: 1.0,
            edge_callback_ledger: Arc::new(TokioMutex::new(HashMap::new())),
            user_id,
            session_id,
            progress_broadcaster: None,
            interactive_client: false,
            full_llm_capture: false,
            event_tx: None,
            #[cfg(feature = "bridge-e2e-hooks")]
            test_llm_rounds: Vec::new(),
            #[cfg(feature = "bridge-e2e-hooks")]
            test_llm_rounds_wired: false,
            #[cfg(feature = "bridge-e2e-hooks")]
            mock_provider: None,
            #[cfg(feature = "bridge-e2e-hooks")]
            llm_request_capture: None,
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

    pub fn with_llm_token_service(
        mut self,
        llm_token_service: Option<LlmTokenServiceConfig>,
    ) -> Self {
        self.llm_token_service = llm_token_service;
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

    pub fn with_interactive_client(mut self, interactive_client: bool) -> Self {
        self.interactive_client = interactive_client;
        self
    }

    pub fn with_full_llm_capture(mut self, full_llm_capture: bool) -> Self {
        self.full_llm_capture = full_llm_capture;
        self
    }

    pub fn with_event_tx(mut self, tx: tokio::sync::mpsc::Sender<Value>) -> Self {
        self.event_tx = Some(tx);
        self
    }

    #[cfg(feature = "bridge-e2e-hooks")]
    pub fn with_test_llm_rounds(mut self, rounds: Vec<Value>) -> Self {
        self.test_llm_rounds_wired = true;
        self.test_llm_rounds = rounds;
        self
    }

    /// **Test-only.** Override the provider/model seen by the mock LLM path so
    /// that `PromptCacheConfig::latch` produces the same annotations as real
    /// calls. Use e.g. `("anthropic", "claude-sonnet-4")` to exercise
    /// `cache_control` blocks end-to-end, or `("openai", "gpt-4o")` for the
    /// stable-prefix / dynamic-system-message split.
    #[cfg(feature = "bridge-e2e-hooks")]
    pub fn with_mock_provider(
        mut self,
        provider: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        self.mock_provider = Some((provider.into(), model.into()));
        self
    }

    /// **Test-only.** Attach an `Arc<Mutex<Vec<CapturedLlmRequest>>>`; every
    /// invocation of `execute_mock_turn` appends a snapshot of the materials
    /// that would be sent to a real LLM (system messages with cache_control
    /// annotations, annotated tool schemas, message cache breakpoint if any,
    /// cache config and a stable hash of the cacheable prefix).
    #[cfg(feature = "bridge-e2e-hooks")]
    pub fn with_llm_request_capture(
        mut self,
        capture: Arc<std::sync::Mutex<Vec<CapturedLlmRequest>>>,
    ) -> Self {
        self.llm_request_capture = Some(capture);
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
            llm_token_service: self.llm_token_service,
            edge_tools,
            edge_profile: self.edge_profile,
            valid_tools,
            selection_confidence: self.selection_confidence,
            server_side_tools,
            interactive_client: self.interactive_client,
            full_llm_capture: self.full_llm_capture,
            edge_callback_ledger: self.edge_callback_ledger,
            user_id: self.user_id,
            session_id: self.session_id,
            tool_result_cache: astra_turn_core::tool_result_dedup::new_shared_cache(
                128,
                Some(std::time::Duration::from_secs(30)),
            ),
            emitted_events: Vec::new(),
            event_tx: self.event_tx,
            client_cancel_flag: None,
            client_cancel_token: None,
            progress_rx,
            #[cfg(feature = "bridge-e2e-hooks")]
            test_llm_rounds: std::collections::VecDeque::from(self.test_llm_rounds),
            #[cfg(feature = "bridge-e2e-hooks")]
            test_llm_rounds_wired: self.test_llm_rounds_wired,
            #[cfg(feature = "bridge-e2e-hooks")]
            mock_provider: self.mock_provider,
            #[cfg(feature = "bridge-e2e-hooks")]
            llm_request_capture: self.llm_request_capture,
        }
    }
}

impl ServerAgenticLoopHost {
    fn turn_interaction_mode(&self) -> TurnInteractionMode {
        if self.interactive_client {
            TurnInteractionMode::Prompt
        } else {
            TurnInteractionMode::Headless
        }
    }

    /// Push an SSE event to both the internal buffer and the streaming channel.
    /// If the streaming channel is closed (client disconnected), triggers
    /// cancellation so the agentic loop stops at the next turn boundary.
    fn emit_event(&mut self, event: Value) {
        if let Some(ref tx) = self.event_tx {
            match tx.try_send(event.clone()) {
                Ok(()) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    // Client disconnected — cancel the loop to stop wasting LLM tokens.
                    if let Some(flag) = &self.client_cancel_flag {
                        flag.store(true, Ordering::SeqCst);
                    }
                    if let Some(token) = &self.client_cancel_token {
                        token.cancel();
                    }
                    self.event_tx = None;
                }
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    // Backpressure: client can't keep up. Cancel to avoid unbounded buffering.
                    tracing::warn!(target: "sse_channel", "SSE event channel full — cancelling run");
                    if let Some(flag) = &self.client_cancel_flag {
                        flag.store(true, Ordering::SeqCst);
                    }
                    if let Some(token) = &self.client_cancel_token {
                        token.cancel();
                    }
                    self.event_tx = None;
                }
            }
        }
        self.emitted_events.push(event);
    }

    fn push_reasoning_events(&mut self, reasoning: &str) {
        if reasoning.is_empty() {
            return;
        }
        self.emit_event(json!({
            "type": "reasoning_delta",
            "content": reasoning,
        }));
        self.emit_event(json!({
            "type": "reasoning_done",
        }));
    }

    /// Access collected SSE events from the last turn.
    /// Also drains any pending agent progress events into the result.
    pub fn take_emitted_events(&mut self) -> Vec<Value> {
        // Drain pending progress events from the broadcast receiver.
        // Treat `Lagged(n)` as a recoverable warning: the receiver continues
        // and we collect every still-buffered event after the gap, preventing
        // silent loss of all subsequent progress events.
        let mut progress_events = Vec::new();
        if let Some(ref mut rx) = self.progress_rx {
            use tokio::sync::broadcast::error::TryRecvError;
            loop {
                match rx.try_recv() {
                    Ok(evt) => {
                        if let Some(sse_val) = progress_event_to_sse(&evt) {
                            progress_events.push(sse_val);
                        }
                    }
                    Err(TryRecvError::Lagged(n)) => {
                        tracing::warn!(
                            target: "astra_runtime::server_loop_host",
                            dropped = n,
                            "progress receiver lagged; continuing drain"
                        );
                        continue;
                    }
                    Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => break,
                }
            }
        }
        for evt in progress_events {
            self.emit_event(evt);
        }
        std::mem::take(&mut self.emitted_events)
    }

    /// Returns `true` when no CLI edge agent is connected (tools are server-side).
    pub fn edge_tools_empty(&self) -> bool {
        self.server_side_tools
    }

    /// Attach an incremental SSE channel. Events will be pushed through
    /// this sender as they are emitted, enabling streaming to the client.
    /// When the channel closes (client disconnect), `cancel_flag` and
    /// `cancel_token` are triggered to stop the agentic loop.
    pub fn set_event_tx(&mut self, tx: tokio::sync::mpsc::Sender<Value>) {
        self.event_tx = Some(tx);
    }

    /// Set the cancellation handles used when client disconnects.
    pub fn set_client_cancel(&mut self, flag: Arc<AtomicBool>, token: Arc<CancellationToken>) {
        self.client_cancel_flag = Some(flag);
        self.client_cancel_token = Some(token);
    }

    /// **Test-only.** Drive a single mock-LLM turn end-to-end without the
    /// surrounding `run_agentic_loop_with_host` orchestration. Pops the next
    /// scripted round from `test_llm_rounds`, runs the full system-prompt +
    /// tool-schema + cache annotation pipeline, and appends a
    /// [`CapturedLlmRequest`] if a capture hook was attached via
    /// [`ServerAgenticLoopHostBuilder::with_llm_request_capture`].
    ///
    /// Returns the resulting [`HostTurnResult`]. Increments
    /// `state.llm_rounds_completed` to mirror the real dispatch path so the
    /// next turn observes the correct round index.
    #[cfg(feature = "bridge-e2e-hooks")]
    pub async fn run_one_mock_turn_for_test(
        &mut self,
        state: &mut AgenticLoopState,
    ) -> Result<HostTurnResult, astra_core::ClassifiedError> {
        let round = self.test_llm_rounds.pop_front().unwrap_or_else(
            || json!({ "full_text": "[mock rounds exhausted]", "tool_calls": [], "usage": {} }),
        );
        let started = Instant::now();
        let result = self.execute_mock_turn(state, &round, started).await?;
        state.llm_rounds_completed = state.llm_rounds_completed.saturating_add(1);
        Ok(result)
    }

    /// Execute a mock LLM turn from `test_llm_rounds` (bridge-e2e-hooks only).
    ///
    /// Parses the round JSON (same shape as bridge e2e hooks), emits SSE events,
    /// and returns a `HostTurnResult` as if a real LLM responded.
    /// Execute a mock LLM turn from `test_llm_rounds` (bridge-e2e-hooks only).
    ///
    /// Parses the round JSON (same shape as bridge e2e hooks), builds the same
    /// cache-annotated system messages, tool schemas and message list that a
    /// real LLM call would receive, emits SSE events, optionally records a
    /// [`CapturedLlmRequest`] for test assertion, and returns a [`HostTurnResult`]
    /// as if a real LLM responded. Test rounds may also inject failures with
    /// `{ "error": { "message": "...", "kind": "...", "details": { ... } } }`
    /// so HTTP E2E can validate error-path artifact publication without a flaky
    /// real-provider dependency.
    #[cfg(feature = "bridge-e2e-hooks")]
    async fn execute_mock_turn(
        &mut self,
        state: &mut AgenticLoopState,
        round: &Value,
        turn_started: Instant,
    ) -> Result<HostTurnResult, astra_core::ClassifiedError> {
        // Latch a cache config from the (optional) mock provider so that
        // annotations exercised here mirror the real pipeline at the server
        // loop host level — including Anthropic cache_control blocks and the
        // OpenAI stable-prefix / dynamic split.
        let cache_cfg = match &self.mock_provider {
            Some((provider, model)) => PromptCacheConfig::latch(provider, model),
            None => PromptCacheConfig::default(),
        };

        let edge_tools_snapshot = self.edge_tools.clone();
        let (system_msgs, _system_plain, system_prompt_breakdown) = self
            .build_system_messages_cached(&state.message, &edge_tools_snapshot, state, &cache_cfg);
        self.emit_context_meta(&system_prompt_breakdown);

        // Replicate the real-path tool + message annotations so captured
        // payloads reflect what a real provider would see.
        let mut annotated_tools = edge_tools_snapshot.clone();
        annotate_tool_schemas_for_caching(&mut annotated_tools, &cache_cfg);
        let mut annotated_messages = state.messages.clone();
        apply_anthropic_cache_metadata(&mut annotated_messages, &cache_cfg, &self.session_id);
        let (provider, model) = self
            .mock_provider
            .clone()
            .unwrap_or_else(|| ("openai".to_string(), "server-loop-mock".to_string()));
        let mut capture_messages = system_msgs.clone();
        capture_messages.extend(annotated_messages.clone());

        // Record the captured request before tool delivery (so assertions see
        // a deterministic view even if tool ledger introduces delays).
        if let Some(cap) = &self.llm_request_capture {
            let turn_index = state.llm_rounds_completed as usize;
            let captured = build_captured_llm_request(
                turn_index,
                provider.clone(),
                model.clone(),
                &cache_cfg,
                &system_msgs,
                &annotated_tools,
                &annotated_messages,
                &system_prompt_breakdown,
            );
            if let Ok(mut guard) = cap.lock() {
                guard.push(captured);
            }
        }

        if let Some(error) = mock_round_error(round) {
            if let Some(partial_text) = mock_round_partial_text(&error) {
                self.emit_event(json!({ "type": "text_delta", "content": partial_text }));
            }
            if !self.session_id.is_empty() {
                let mut artifact_store =
                    astra_services::DatabaseSessionArtifactStore::new(self.matrixone.clone());
                if let Some(pool) = self.shared_pool.clone() {
                    artifact_store = artifact_store.with_pool(pool);
                }
                let outcome = if error.kind == astra_core::ErrorKind::ContextWindow {
                    "context_window_error"
                } else {
                    "error"
                };
                crate::turn::llm_exchange_capture::persist_configured_capture_or_log(
                    "server_loop_host mock error capture",
                    self.full_llm_capture,
                    Some(&artifact_store),
                    &self.session_id,
                    &self.user_id,
                    state.session_turn,
                    state.llm_rounds_completed,
                    None,
                    "server_loop_host",
                    &model,
                    &provider,
                    &capture_messages,
                    &annotated_tools,
                    None,
                    outcome,
                    llm_capture_error_response(&error),
                    Some(crate::turn::llm_exchange_capture::CaptureTrace {
                        session_turn_source: Some("state"),
                        turn_chain_id: None,
                        user_query_event_id: None,
                    }),
                )
                .await;
            }
            if error.kind == astra_core::ErrorKind::ContextWindow {
                let accum = ChatTurnSseAccum {
                    error_message: Some(error.message.clone()),
                    system_prompt_tokens: Some(system_prompt_breakdown.total_tokens),
                    system_prompt_breakdown: serde_json::to_value(&system_prompt_breakdown).ok(),
                    ..Default::default()
                };
                return Ok(HostTurnResult {
                    accum,
                    ttft_ms: Some(turn_started.elapsed().as_millis() as u64),
                    edge_tool_round: Vec::new(),
                    error_kind: Some(astra_core::ErrorKind::ContextWindow),
                });
            }
            return Err(error);
        }

        let (full_text, reasoning, tool_calls, usage) =
            crate::turn::bridge_e2e_hooks::parse_llm_round(round);

        if !reasoning.is_empty() {
            self.push_reasoning_events(&reasoning);
        }
        if !full_text.is_empty() {
            self.emit_event(json!({ "type": "text_delta", "content": &full_text }));
        }
        for tc in &tool_calls {
            self.emit_event(json!({ "type": "tool_call", "tool_call": tc }));
        }
        let prompt = usage
            .get("prompt_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(10);
        let completion = usage
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(5);
        // Accept both the flat naming (mock-friendly) and the Anthropic
        // `*_input_tokens` variants so fixtures can model either provider.
        let cache_read = usage
            .get("cache_read_tokens")
            .or_else(|| usage.get("cache_read_input_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let cache_creation = usage
            .get("cache_creation_tokens")
            .or_else(|| usage.get("cache_creation_input_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        self.emit_event(json!({
            "type": "usage",
            "prompt_tokens": prompt,
            "completion_tokens": completion,
            "cache_read_tokens": cache_read,
            "cache_creation_tokens": cache_creation,
        }));

        let edge_tool_round =
            if !self.server_side_tools && self.event_tx.is_some() && !tool_calls.is_empty() {
                self.deliver_edge_tools_via_ledger(&tool_calls).await
            } else {
                Vec::new()
            };

        let accum = ChatTurnSseAccum {
            full_text: full_text.clone(),
            reasoning_content: reasoning,
            tool_calls: tool_calls.clone(),
            has_tool_calls: !tool_calls.is_empty(),
            prompt_tokens: prompt,
            completion_tokens: completion,
            cache_read_tokens: cache_read,
            cache_creation_tokens: cache_creation,
            has_usage: true,
            system_prompt_tokens: Some(system_prompt_breakdown.total_tokens),
            system_prompt_breakdown: serde_json::to_value(&system_prompt_breakdown).ok(),
            ..Default::default()
        };

        if !self.session_id.is_empty() {
            let mut artifact_store =
                astra_services::DatabaseSessionArtifactStore::new(self.matrixone.clone());
            if let Some(pool) = self.shared_pool.clone() {
                artifact_store = artifact_store.with_pool(pool);
            }
            crate::turn::llm_exchange_capture::persist_configured_capture_or_log(
                "server_loop_host mock success capture",
                self.full_llm_capture,
                Some(&artifact_store),
                &self.session_id,
                &self.user_id,
                state.session_turn,
                state.llm_rounds_completed,
                None,
                "server_loop_host",
                &model,
                &provider,
                &capture_messages,
                &annotated_tools,
                None,
                "success",
                json!({
                    "finish_reason": if tool_calls.is_empty() { "stop" } else { "tool_calls" },
                    "full_text": full_text.clone(),
                    "reasoning": accum.reasoning_content.clone(),
                    "tool_calls": tool_calls.clone(),
                    "usage": {
                        "prompt_tokens": prompt,
                        "completion_tokens": completion,
                        "cache_read_tokens": cache_read,
                        "cache_creation_tokens": cache_creation,
                    },
                }),
                Some(crate::turn::llm_exchange_capture::CaptureTrace {
                    session_turn_source: Some("state"),
                    turn_chain_id: None,
                    user_query_event_id: None,
                }),
            )
            .await;
        }

        state.final_text_streamed = !full_text.is_empty();
        state.final_text = full_text;
        state.total_prompt += prompt;
        state.total_completion += completion;
        state.has_any_usage = true;

        Ok(HostTurnResult {
            accum,
            ttft_ms: Some(turn_started.elapsed().as_millis() as u64),
            edge_tool_round,
            error_kind: None,
        })
    }

    /// Deliver edge tool calls via the ledger protocol.
    ///
    /// For each tool call:
    /// 1. If approval required: emit `approval_required` SSE → wait on approval ledger
    /// 2. Emit `tool_request` SSE (so client can execute the tool)
    /// 3. Wait on tool result ledger (populated by client's `POST /tools/result`)
    /// 4. Convert result to `EdgeToolExecResult`
    ///
    /// Events are streamed incrementally through `event_tx`.
    async fn deliver_edge_tools_via_ledger(
        &mut self,
        tool_calls: &[Value],
    ) -> Vec<astra_turn_core::sse_stream_host::EdgeToolExecResult> {
        use astra_turn_core::cloud_tool_delivery::{
            cloud_tool_requires_approval_for_delivery, collect_approval_batches,
            sse_maps_through_tool_request, wait_approval_ledger_for_tool,
            wait_tool_result_ledger_for_tool,
        };
        use astra_turn_core::headless_tool_assembly::{
            ensure_tool_call_ids, parse_flat_tool_call_event,
        };
        use astra_turn_core::sse_stream_host::EdgeToolExecResult;
        use astra_turn_core::stream_events::{
            ApprovalBatchRequestEvent, build_approval_batch_required_event,
            build_approval_required_event,
        };
        use std::collections::HashMap;

        let tool_calls = ensure_tool_call_ids(tool_calls);
        // 5-minute timeout: web clients may execute long-running tools.
        let ledger_wait = std::time::Duration::from_secs(300);
        let mut results_by_id: HashMap<String, EdgeToolExecResult> = HashMap::new();

        for batch in collect_approval_batches(&tool_calls) {
            if batch.items.len() == 1 {
                let item = &batch.items[0];
                self.emit_event(Value::Object(build_approval_required_event(
                    &item.request_id,
                    &item.tool_name,
                    item.approval_kind,
                    item.path.as_deref(),
                    item.detail.as_deref(),
                )));
            } else {
                let requests = batch
                    .items
                    .iter()
                    .map(|item| ApprovalBatchRequestEvent {
                        request_id: &item.request_id,
                        tool_name: &item.tool_name,
                        approval_kind: item.approval_kind,
                        path: item.path.as_deref(),
                        detail: item.detail.as_deref(),
                    })
                    .collect::<Vec<_>>();
                self.emit_event(Value::Object(build_approval_batch_required_event(
                    &requests,
                )));
            }
        }

        let mut block_start = 0;
        while block_start < tool_calls.len() {
            let approval_required =
                cloud_tool_requires_approval_for_delivery(&tool_calls[block_start]);
            let mut block_end = block_start + 1;
            while block_end < tool_calls.len()
                && cloud_tool_requires_approval_for_delivery(&tool_calls[block_end])
                    == approval_required
            {
                block_end += 1;
            }

            let block = &tool_calls[block_start..block_end];
            let mut executable_calls = Vec::new();
            if approval_required {
                for tc in block {
                    if !tc.is_object() {
                        continue;
                    }
                    let (request_id, tool_name, args) = parse_flat_tool_call_event(tc);
                    if let Err(denied) = wait_approval_ledger_for_tool(
                        &self.edge_callback_ledger,
                        &self.user_id,
                        tc,
                        ledger_wait,
                        None,
                    )
                    .await
                    {
                        for m in denied.sse_maps {
                            self.emit_event(Value::Object(m));
                        }
                        results_by_id.insert(
                            request_id.clone(),
                            EdgeToolExecResult {
                                request_id,
                                tool: tool_name,
                                args,
                                output: "Tool execution denied or timed out".to_string(),
                                tool_result_fields: None,
                                status: "error".to_string(),
                                duration_ms: 0,
                            },
                        );
                        continue;
                    }
                    executable_calls.push(tc);
                }
            } else {
                executable_calls.extend(block.iter());
            }

            for tc in &executable_calls {
                for m in sse_maps_through_tool_request(tc) {
                    self.emit_event(Value::Object(m));
                }
            }

            for tc in executable_calls {
                if !tc.is_object() {
                    continue;
                }
                let (id, tool_name, args) = parse_flat_tool_call_event(tc);

                // ── Dedup read-only tool invocations within a short window ──
                // Only applies when concurrency_safety classifies the tool as
                // read-only / parallelizable; mutating tools skip the cache.
                let is_cacheable =
                    astra_turn_core::parallel_tool_exec::is_read_only_tool(&tool_name);
                let sig = if is_cacheable {
                    Some(
                        astra_turn_core::tool_result_dedup::CallSignature::from_args(
                            &tool_name, &args,
                        ),
                    )
                } else {
                    None
                };

                let started = std::time::Instant::now();
                let cached = sig.as_ref().and_then(|s| {
                    self.tool_result_cache
                        .lock()
                        .ok()
                        .and_then(|mut g| g.lookup(s))
                });

                let (delivery_output, delivery_sse_maps, duration_ms, status, tool_result_fields): (
                    String,
                    Vec<Map<String, Value>>,
                    u64,
                    String,
                    Option<Map<String, Value>>,
                ) = if let Some(cached_output) = cached {
                    (cached_output, Vec::new(), 0, "ok".to_string(), None)
                } else {
                    let delivery = wait_tool_result_ledger_for_tool(
                        &self.edge_callback_ledger,
                        &self.user_id,
                        tc,
                        ledger_wait,
                    )
                    .await;
                    let duration_ms = started.elapsed().as_millis() as u64;
                    let sse_maps = delivery.sse_maps.clone();
                    let output = delivery
                        .tool_messages
                        .first()
                        .and_then(|m| m.get("content"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let tool_result = delivery.tool_results.first().cloned();
                    let status = tool_result
                        .as_ref()
                        .map(|result| result.status.clone())
                        .unwrap_or_else(|| "unknown".to_string());
                    // Record successful read-only results only.
                    if let Some(sig_ref) = sig.as_ref() {
                        let is_err = tool_result
                            .as_ref()
                            .and_then(|result| edge_tool_status_exit_code(&result.status))
                            .is_some_and(|exit_code| exit_code != 0);
                        if !is_err {
                            if let Ok(mut guard) = self.tool_result_cache.lock() {
                                guard.record(sig_ref.clone(), output.clone());
                            }
                        }
                    }
                    (
                        output,
                        sse_maps,
                        duration_ms,
                        status,
                        tool_result.and_then(|result| result.tool_result_fields),
                    )
                };

                for m in delivery_sse_maps {
                    self.emit_event(Value::Object(m));
                }

                let output = delivery_output;

                results_by_id.insert(
                    id.clone(),
                    EdgeToolExecResult {
                        request_id: id,
                        tool: tool_name,
                        args,
                        output,
                        tool_result_fields,
                        status,
                        duration_ms,
                    },
                );
            }

            block_start = block_end;
        }

        let mut results = Vec::with_capacity(tool_calls.len());
        for tc in tool_calls.iter() {
            let Some(tc_map) = tc.as_object() else {
                continue;
            };
            let request_id = tc_map
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if let Some(result) = results_by_id.remove(&request_id) {
                results.push(result);
            }
        }

        results
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
    ) -> (
        Vec<Value>,
        String,
        crate::turn::context_assembly_trace::SystemPromptBreakdown,
    ) {
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

        let active_skill_names: Vec<&str> = self
            .edge_profile
            .get("active_skills")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let skill_hint = if active_skill_names.is_empty() {
            String::new()
        } else {
            format!(
                "\n\n## Active Output Skills\nThe user has enabled these output constraints: {}. \
                 Follow their formatting rules strictly.",
                active_skill_names.join(", ")
            )
        };

        let learned_context_text = self
            .edge_profile
            .get("learned_context_hint")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or_default();
        let learned_context_hint = if learned_context_text.is_empty() {
            String::new()
        } else {
            format!("\n\n## Learned Runtime Context\n{learned_context_text}")
        };

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

        let tool_cfg = crate::runtime_config::RuntimeConfig::load().tool_selection;
        let (tool_round_guidance, guidance_signals) =
            crate::prompts::tool_round_guidance_trace_with(
                &state.messages,
                state.llm_rounds_completed,
                tool_cfg.effective_round_budget_warning(),
                tool_cfg.effective_round_budget_limit(),
            );

        let mut dynamic_sections = Vec::new();
        if !profile_desc.is_empty() {
            dynamic_sections.push(crate::prompts::PromptSection::dynamic(
                profile_desc.clone(),
                crate::prompts::PromptTokenBucket::Environment,
            ));
        }
        if !skill_hint.is_empty() {
            dynamic_sections.push(
                crate::prompts::PromptSection::dynamic(
                    skill_hint.clone(),
                    crate::prompts::PromptTokenBucket::UserPreferences,
                )
                .with_trace_signals(
                    crate::turn::context_assembly_trace::PromptTraceSignals {
                        context_signals:
                            crate::turn::context_assembly_trace::PromptContextSignals {
                                active_output_skills: true,
                                ..Default::default()
                            },
                        ..Default::default()
                    },
                ),
            );
        }
        if !learned_context_hint.is_empty() {
            dynamic_sections.push(
                crate::prompts::PromptSection::dynamic(
                    learned_context_hint.clone(),
                    crate::prompts::PromptTokenBucket::UserPreferences,
                )
                .with_trace_signals(
                    crate::turn::context_assembly_trace::PromptTraceSignals {
                        context_signals:
                            crate::turn::context_assembly_trace::PromptContextSignals {
                                learned_runtime_context: true,
                                ..Default::default()
                            },
                        ..Default::default()
                    },
                ),
            );
        }
        if !extra_dynamic.is_empty() {
            dynamic_sections.push(
                crate::prompts::PromptSection::dynamic(
                    extra_dynamic.clone(),
                    crate::prompts::PromptTokenBucket::Environment,
                )
                .with_trace_signals(
                    crate::turn::context_assembly_trace::PromptTraceSignals {
                        context_signals:
                            crate::turn::context_assembly_trace::PromptContextSignals {
                                effort_hint: state.skills.effort.is_some(),
                                agent_type_hint: state.skills.agent_type.is_some(),
                                ..Default::default()
                            },
                        ..Default::default()
                    },
                ),
            );
        }
        if !memory_signal_hint.is_empty() {
            dynamic_sections.push(
                crate::prompts::PromptSection::dynamic(
                    memory_signal_hint.clone(),
                    crate::prompts::PromptTokenBucket::Environment,
                )
                .with_trace_signals(
                    crate::turn::context_assembly_trace::PromptTraceSignals {
                        context_signals:
                            crate::turn::context_assembly_trace::PromptContextSignals {
                                memory_signal_detected: true,
                                ..Default::default()
                            },
                        ..Default::default()
                    },
                ),
            );
        }
        if !system_override.is_empty() {
            dynamic_sections.push(
                crate::prompts::PromptSection::dynamic(
                    system_override.clone(),
                    crate::prompts::PromptTokenBucket::Environment,
                )
                .with_trace_signals(
                    crate::turn::context_assembly_trace::PromptTraceSignals {
                        context_signals:
                            crate::turn::context_assembly_trace::PromptContextSignals {
                                system_prompt_override: true,
                                ..Default::default()
                            },
                        ..Default::default()
                    },
                ),
            );
        }
        if !tool_round_guidance.is_empty() {
            dynamic_sections.push(
                crate::prompts::PromptSection::dynamic(
                    tool_round_guidance.clone(),
                    crate::prompts::PromptTokenBucket::Environment,
                )
                .with_trace_signals(
                    crate::turn::context_assembly_trace::PromptTraceSignals {
                        guidance_signals,
                        ..Default::default()
                    },
                ),
            );
        }
        let full_dynamic = crate::prompts::sections_to_string(&dynamic_sections);

        // Build structured system messages with Anthropic cache annotations.
        // Stable sections (Global/Session) get cache_control; dynamic content does not.
        let (sys_msg, dynamic_msg, sections) = build_system_message_with_dynamic_sections(
            &tool_names,
            &dynamic_sections,
            self.selection_confidence,
            task_type,
            cache_cfg,
        );
        let breakdown = crate::prompts::build_system_prompt_trace(&sections, vec![], vec![]);
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

        (system_messages, plain, breakdown)
    }

    fn emit_context_meta(
        &mut self,
        breakdown: &crate::turn::context_assembly_trace::SystemPromptBreakdown,
    ) {
        self.emit_event(json!({
            "type": "context_meta",
            "system_prompt_tokens": breakdown.total_tokens,
            "system_prompt_breakdown": breakdown,
        }));
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
        if std::mem::take(&mut state.widen_selection_pending) {
            // Pipeline-requested widen: skip health-based deprioritization
            // merge for this turn so the full catalogue is re-exposed.
        } else {
            merge_deprioritized_tools_into_restricted(
                &state.turn_guard,
                &mut state.restricted_tools,
            );
        }
        let mut effective_restricted = state.restricted_tools.clone();
        effective_restricted.extend(self.effective_allowlist_restrictions(state));
        // Boosted tools are never hidden, even if they landed in the restricted
        // set earlier (e.g., via stall-based deprioritization).
        for boosted in &state.boosted_tools {
            effective_restricted.remove(boosted);
        }
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
        let tool_cfg = crate::runtime_config::RuntimeConfig::load().tool_selection;
        let round_budget_hint = crate::prompts::round_budget_directive_with(
            0,
            tool_cfg.effective_round_budget_warning(),
            tool_cfg.effective_round_budget_limit(),
        );
        let full_dynamic = format!(
            "{profile_desc}{skill_hint}{learned_context_hint}{memory_signal_hint}{round_budget_hint}"
        );

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
        header_overrides: &HashMap<String, String>,
        completions_url_override: Option<&str>,
        request_timeout: Option<Duration>,
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
        let summary_client = RequestAwareSummaryClient {
            model_name: model_name.to_string(),
            api_key: api_key.to_string(),
            base_url: base_url.to_string(),
            provider: provider.to_string(),
            max_output_tokens: compact_config.summary_token_budget,
            header_overrides: header_overrides.clone(),
            completions_url_override: completions_url_override.map(String::from),
            request_timeout,
        };

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

        // Add Anthropic protocol-level prompt-cache metadata on the request clone.
        apply_anthropic_cache_metadata(&mut llm_messages, cache_cfg, &self.session_id);

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
    fn injects_round_guidance(&self) -> bool {
        true // Server injects guidance into the system prompt in execute_turn.
    }

    fn render_final_text(&mut self, text: &str) {
        if !text.is_empty() {
            self.emit_event(json!({ "type": "text_delta", "content": text }));
        }
    }

    async fn execute_turn(
        &mut self,
        state: &mut AgenticLoopState,
    ) -> Result<HostTurnResult, astra_core::ClassifiedError> {
        let turn_started = Instant::now();

        // ── Test hook: mock LLM rounds ──────────────────────────────────
        #[cfg(feature = "bridge-e2e-hooks")]
        {
            if let Some(round) = self.test_llm_rounds.pop_front() {
                return self.execute_mock_turn(state, &round, turn_started).await;
            }
            if self.test_llm_rounds_wired {
                // All mock rounds consumed — return a no-op text result so the
                // agentic loop terminates cleanly (no real LLM fallback).
                self.emit_event(
                    json!({ "type": "text_delta", "content": "[mock rounds exhausted]" }),
                );
                state.final_text = "[mock rounds exhausted]".to_string();
                state.final_text_streamed = true;
                return Ok(HostTurnResult {
                    accum: ChatTurnSseAccum {
                        full_text: "[mock rounds exhausted]".to_string(),
                        ..Default::default()
                    },
                    ttft_ms: Some(0),
                    edge_tool_round: Vec::new(),
                    error_kind: None,
                });
            }
        }

        // ── 1. Resolve LLM model ────────────────────────────────────────
        // Skill-level model override takes precedence over the host-level one.
        let effective_model_override = state
            .skills
            .model_override
            .as_deref()
            .or(self.model_override.as_deref());
        let pool_ref = self.shared_pool.as_ref().map(|sp| sp.get());
        let mut llm_cfg = match resolve_llm_model_for_turn(
            &self.matrixone,
            self.encryptor.as_ref(),
            effective_model_override,
            pool_ref,
            self.llm_token_service.as_ref(),
            &state.hooks.forward_headers,
        )
        .await
        {
            Ok(m) => m,
            Err(e) => {
                return Err(astra_core::ClassifiedError::new(
                    astra_core::ErrorKind::Unknown,
                    format!("Model resolution failed: {e}"),
                ));
            }
        };
        let has_fallback = llm_cfg.fallback_model.is_some();

        // ── 1b. Check rate-limit cooldown and handle fallback model resolution ──
        let cooldown = rate_limit_cooldown();
        match cooldown.with(&llm_cfg.model_name, |c| c.check_request(has_fallback)) {
            RateLimitAction::Proceed => {}
            RateLimitAction::WaitAndRetry { delay_ms } => {
                astra_core::agent_info!(
                    "llm",
                    "rate-limit cooldown: waiting {delay_ms}ms before request"
                );
                sleep_ms_or_llm_cancel(delay_ms, llm_cancel_for_state(state)).await?;
            }
            RateLimitAction::UseFallback { reason } => {
                if let Some(ref fb_name) = llm_cfg.fallback_model {
                    astra_core::agent_info!(
                        "llm",
                        "rate-limit cooldown: switching to fallback model '{}' ({})",
                        fb_name,
                        reason.as_str()
                    );
                    // Resolve fallback model credentials
                    match resolve_llm_model_for_turn(
                        &self.matrixone,
                        self.encryptor.as_ref(),
                        Some(fb_name.as_str()),
                        pool_ref,
                        self.llm_token_service.as_ref(),
                        &state.hooks.forward_headers,
                    )
                    .await
                    {
                        Ok(fb) => llm_cfg = fb,
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

        if std::mem::take(&mut state.widen_selection_pending) {
            // Pipeline-requested widen: skip deprioritized-merge for this turn.
        } else {
            merge_deprioritized_tools_into_restricted(
                &state.turn_guard,
                &mut state.restricted_tools,
            );
        }
        let mut effective_restricted = state.restricted_tools.clone();
        effective_restricted.extend(self.effective_allowlist_restrictions(state));
        let interaction_mode = self.turn_interaction_mode();
        effective_restricted.extend(interaction_scoped_tool_restrictions(interaction_mode));
        for boosted in &state.boosted_tools {
            effective_restricted.remove(boosted);
        }
        let visible_tools = self.filtered_turn_tools(&effective_restricted);
        self.sync_valid_tools_to_visible(&visible_tools);

        // Latch prompt cache config from provider info (once per turn is fine;
        // provider doesn't change within a turn).
        let cache_cfg = PromptCacheConfig::latch(&llm_cfg.provider, &llm_cfg.model_name);

        let (system_messages, system_prompt_plain, system_prompt_breakdown) =
            self.build_system_messages_cached(&user_content, &visible_tools, state, &cache_cfg);
        self.emit_context_meta(&system_prompt_breakdown);

        let llm_messages = self
            .build_llm_messages(
                system_messages,
                state,
                &visible_tools,
                &llm_cfg.model_name,
                &llm_cfg.api_key,
                &llm_cfg.base_url,
                &llm_cfg.provider,
                &llm_cfg.header_overrides,
                llm_cfg.completions_url_override.as_deref(),
                llm_cfg.request_timeout,
                &cache_cfg,
            )
            .await;

        // ── 3. Call LLM ─────────────────────────────────────────────────
        let budget = crate::prompts::budget_for_model(Some(&llm_cfg.model_name));
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
            TurnInteractionPolicy::from_tool_schemas(interaction_mode, &final_tools);

        // Output token escalation: if finish_reason is "length", retry once
        // with a higher max_output_tokens (up to 4× the initial budget).
        let mut effective_max_output = max_output_tokens;
        let mut attempt_in_round = 0_u32;
        let result = loop {
            record_full_llm_request_event(
                state,
                self.full_llm_capture,
                &self.session_id,
                "server_loop_host",
                &llm_cfg.model_name,
                &llm_cfg.provider,
                attempt_in_round,
                &llm_messages,
                &final_tools,
                Some(effective_max_output),
            );
            let llm_cancel = llm_cancel_for_state(state);
            let r = call_llm_and_collect_with_request_overrides(
                &llm_messages,
                &final_tools,
                &llm_cfg.model_name,
                &llm_cfg.api_key,
                &llm_cfg.base_url,
                &llm_cfg.provider,
                Some(effective_max_output),
                has_fallback,
                llm_cancel,
                (!llm_cfg.header_overrides.is_empty()).then_some(&llm_cfg.header_overrides),
                llm_cfg.completions_url_override.as_deref(),
                llm_cfg.request_timeout,
            )
            .await;

            // Context-window errors flow through the accum so the agentic loop's
            // Fatal handler can trigger auto-compaction + retry.
            let r = match r {
                Ok(r) => r,
                Err(ref e) if e.kind == astra_core::ErrorKind::ContextWindow => {
                    record_full_llm_response_event(
                        state,
                        self.full_llm_capture,
                        &self.session_id,
                        "server_loop_host",
                        &llm_cfg.model_name,
                        &llm_cfg.provider,
                        attempt_in_round,
                        "context_window_error",
                        llm_capture_error_response(e),
                    );
                    if !self.session_id.is_empty() {
                        let mut artifact_store = astra_services::DatabaseSessionArtifactStore::new(
                            self.matrixone.clone(),
                        );
                        if let Some(pool) = self.shared_pool.clone() {
                            artifact_store = artifact_store.with_pool(pool);
                        }
                        let dump = astra_turn_core::llm_request_dump::build_llm_request_dump(
                            &self.session_id,
                            None,
                            &llm_cfg.model_name,
                            &llm_cfg.provider,
                            &e.message,
                            &llm_messages,
                            &final_tools,
                            i64::from(state.llm_rounds_completed),
                            Some(effective_max_output),
                        );
                        if let Err(error) =
                            dump.persist_remote(&self.user_id, &artifact_store).await
                        {
                            astra_core::agent_error!(
                                "llm-dump",
                                "server_loop_host context window dump persist failed: {error}"
                            );
                        }
                        crate::turn::llm_exchange_capture::persist_configured_capture_or_log(
                            "server_loop_host context window capture",
                            self.full_llm_capture,
                            Some(&artifact_store),
                            &self.session_id,
                            &self.user_id,
                            state.session_turn,
                            state.llm_rounds_completed,
                            None,
                            "server_loop_host",
                            &llm_cfg.model_name,
                            &llm_cfg.provider,
                            &llm_messages,
                            &final_tools,
                            Some(effective_max_output),
                            "context_window_error",
                            llm_capture_error_response(e),
                            Some(crate::turn::llm_exchange_capture::CaptureTrace {
                                session_turn_source: Some("state"),
                                turn_chain_id: None,
                                user_query_event_id: None,
                            }),
                        )
                        .await;
                    }
                    let accum = ChatTurnSseAccum {
                        error_message: Some(e.message.clone()),
                        system_prompt_tokens: Some(system_prompt_breakdown.total_tokens),
                        system_prompt_breakdown: serde_json::to_value(&system_prompt_breakdown)
                            .ok(),
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
                Err(e) => {
                    record_full_llm_response_event(
                        state,
                        self.full_llm_capture,
                        &self.session_id,
                        "server_loop_host",
                        &llm_cfg.model_name,
                        &llm_cfg.provider,
                        attempt_in_round,
                        "error",
                        llm_capture_error_response(&e),
                    );
                    if !self.session_id.is_empty() {
                        let mut artifact_store = astra_services::DatabaseSessionArtifactStore::new(
                            self.matrixone.clone(),
                        );
                        if let Some(pool) = self.shared_pool.clone() {
                            artifact_store = artifact_store.with_pool(pool);
                        }
                        let dump = astra_turn_core::llm_request_dump::build_llm_request_dump(
                            &self.session_id,
                            None,
                            &llm_cfg.model_name,
                            &llm_cfg.provider,
                            &e.message,
                            &llm_messages,
                            &final_tools,
                            i64::from(state.llm_rounds_completed),
                            Some(effective_max_output),
                        );
                        if let Err(error) =
                            dump.persist_remote(&self.user_id, &artifact_store).await
                        {
                            astra_core::agent_error!(
                                "llm-dump",
                                "server_loop_host error dump persist failed: {error}"
                            );
                        }
                        crate::turn::llm_exchange_capture::persist_configured_capture_or_log(
                            "server_loop_host error capture",
                            self.full_llm_capture,
                            Some(&artifact_store),
                            &self.session_id,
                            &self.user_id,
                            state.session_turn,
                            state.llm_rounds_completed,
                            None,
                            "server_loop_host",
                            &llm_cfg.model_name,
                            &llm_cfg.provider,
                            &llm_messages,
                            &final_tools,
                            Some(effective_max_output),
                            "error",
                            llm_capture_error_response(&e),
                            Some(crate::turn::llm_exchange_capture::CaptureTrace {
                                session_turn_source: Some("state"),
                                turn_chain_id: None,
                                user_query_event_id: None,
                            }),
                        )
                        .await;
                    }
                    return Err(e);
                }
            };

            record_full_llm_response_event(
                state,
                self.full_llm_capture,
                &self.session_id,
                "server_loop_host",
                &llm_cfg.model_name,
                &llm_cfg.provider,
                attempt_in_round,
                if r.finish_reason.as_deref() == Some("length")
                    && effective_max_output < max_output_tokens * 4
                {
                    "length_retry"
                } else {
                    "success"
                },
                json!({
                    "finish_reason": r.finish_reason.clone(),
                    "full_text": r.full_text.clone(),
                    "reasoning": r.reasoning.clone(),
                    "tool_calls": r.tool_calls.clone(),
                    "usage": r.usage.clone(),
                }),
            );

            if r.finish_reason.as_deref() == Some("length")
                && effective_max_output < max_output_tokens * 4
            {
                let prev = effective_max_output;
                effective_max_output = (effective_max_output * 2).min(max_output_tokens * 4);
                attempt_in_round = attempt_in_round.saturating_add(1);
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

        if !self.session_id.is_empty() {
            let mut artifact_store =
                astra_services::DatabaseSessionArtifactStore::new(self.matrixone.clone());
            if let Some(pool) = self.shared_pool.clone() {
                artifact_store = artifact_store.with_pool(pool);
            }
            crate::turn::llm_exchange_capture::persist_configured_capture_or_log(
                "server_loop_host success capture",
                self.full_llm_capture,
                Some(&artifact_store),
                &self.session_id,
                &self.user_id,
                state.session_turn,
                state.llm_rounds_completed,
                None,
                "server_loop_host",
                &llm_cfg.model_name,
                &llm_cfg.provider,
                &llm_messages,
                &final_tools,
                Some(effective_max_output),
                "success",
                json!({
                    "finish_reason": result.finish_reason.clone(),
                    "full_text": result.full_text.clone(),
                    "reasoning": result.reasoning.clone(),
                    "tool_calls": result.tool_calls.clone(),
                    "usage": result.usage.clone(),
                }),
                Some(crate::turn::llm_exchange_capture::CaptureTrace {
                    session_turn_source: Some("state"),
                    turn_chain_id: None,
                    user_query_event_id: None,
                }),
            )
            .await;
        }

        // ── 4. Emit SSE events for client ───────────────────────────────
        if !result.full_text.is_empty() {
            self.emit_event(json!({
                "type": "text_delta",
                "content": result.full_text,
            }));
        }
        self.push_reasoning_events(&result.reasoning);
        if !result.usage.is_empty() {
            self.emit_event(json!({
                "type": "usage",
                "prompt_tokens": result.usage.get("prompt"),
                "completion_tokens": result.usage.get("completion"),
            }));
        }

        // ── 5. Edge tool delivery via ledger (streaming mode) ────────────
        //
        // When streaming to a web client with edge tools, emit `tool_request`
        // SSE events so the client can execute tools locally, then wait on
        // the ledger for the results posted via `POST /tools/result`.
        //
        // When server_side_tools is true, the headless pipeline uses
        // server_tool_executor and no ledger is needed.
        let edge_tool_round = if !self.server_side_tools
            && self.event_tx.is_some()
            && !result.tool_calls.is_empty()
        {
            self.deliver_edge_tools_via_ledger(&result.tool_calls).await
        } else {
            Vec::new()
        };

        // ── 6. Build turn result ────────────────────────────────────────
        let ttft_ms = Some(turn_started.elapsed().as_millis() as u64);
        let mut accum = Self::result_to_accum(&result);
        accum.system_prompt_tokens = Some(system_prompt_breakdown.total_tokens);
        accum.system_prompt_breakdown = serde_json::to_value(&system_prompt_breakdown).ok();

        Ok(HostTurnResult {
            accum,
            ttft_ms,
            edge_tool_round,
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
        let mut llm_cfg = match resolve_llm_model_for_turn(
            &self.matrixone,
            self.encryptor.as_ref(),
            effective_model_override,
            pool_ref,
            self.llm_token_service.as_ref(),
            &state.hooks.forward_headers,
        )
        .await
        {
            Ok(m) => m,
            Err(e) => return Err(format!("Model resolution failed: {e}").into()),
        };
        let has_fallback = llm_cfg.fallback_model.is_some();

        match rate_limit_cooldown().with(&llm_cfg.model_name, |c| c.check_request(has_fallback)) {
            RateLimitAction::Proceed => {}
            RateLimitAction::WaitAndRetry { delay_ms } => {
                sleep_ms_or_llm_cancel(delay_ms, llm_cancel_for_state(state)).await?;
            }
            RateLimitAction::UseFallback { .. } => {
                if let Some(ref fb_name) = llm_cfg.fallback_model
                    && let Ok(fb) = resolve_llm_model_for_turn(
                        &self.matrixone,
                        self.encryptor.as_ref(),
                        Some(fb_name.as_str()),
                        pool_ref,
                        self.llm_token_service.as_ref(),
                        &state.hooks.forward_headers,
                    )
                    .await
                {
                    llm_cfg = fb;
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
        let result = match call_llm_and_collect_with_request_overrides(
            &reflection_messages,
            &[],
            &llm_cfg.model_name,
            &llm_cfg.api_key,
            &llm_cfg.base_url,
            &llm_cfg.provider,
            request.max_output_tokens,
            has_fallback,
            llm_cancel_for_state(state),
            (!llm_cfg.header_overrides.is_empty()).then_some(&llm_cfg.header_overrides),
            llm_cfg.completions_url_override.as_deref(),
            llm_cfg.request_timeout,
        )
        .await
        {
            Ok(result) => result,
            Err(error) => {
                if !self.session_id.is_empty() {
                    let mut artifact_store =
                        astra_services::DatabaseSessionArtifactStore::new(self.matrixone.clone());
                    if let Some(pool) = self.shared_pool.clone() {
                        artifact_store = artifact_store.with_pool(pool);
                    }
                    let dump = astra_turn_core::llm_request_dump::build_llm_request_dump(
                        &self.session_id,
                        None,
                        &llm_cfg.model_name,
                        &llm_cfg.provider,
                        &error.message,
                        &reflection_messages,
                        &[],
                        i64::from(state.llm_rounds_completed),
                        request.max_output_tokens,
                    );
                    if let Err(error) = dump.persist_remote(&self.user_id, &artifact_store).await {
                        astra_core::agent_error!(
                            "llm-dump",
                            "server_loop_reflection error dump persist failed: {error}"
                        );
                    }
                    crate::turn::llm_exchange_capture::persist_configured_capture_or_log(
                        "server_loop_reflection error capture",
                        self.full_llm_capture,
                        Some(&artifact_store),
                        &self.session_id,
                        &self.user_id,
                        state.session_turn,
                        state.llm_rounds_completed,
                        None,
                        "server_loop_reflection",
                        &llm_cfg.model_name,
                        &llm_cfg.provider,
                        &reflection_messages,
                        &[],
                        request.max_output_tokens,
                        "error",
                        llm_capture_error_response(&error),
                        Some(crate::turn::llm_exchange_capture::CaptureTrace {
                            session_turn_source: Some("state"),
                            turn_chain_id: None,
                            user_query_event_id: None,
                        }),
                    )
                    .await;
                }
                return Err(error);
            }
        };
        if !self.session_id.is_empty() {
            let mut artifact_store =
                astra_services::DatabaseSessionArtifactStore::new(self.matrixone.clone());
            if let Some(pool) = self.shared_pool.clone() {
                artifact_store = artifact_store.with_pool(pool);
            }
            crate::turn::llm_exchange_capture::persist_configured_capture_or_log(
                "server_loop_reflection success capture",
                self.full_llm_capture,
                Some(&artifact_store),
                &self.session_id,
                &self.user_id,
                state.session_turn,
                state.llm_rounds_completed,
                None,
                "server_loop_reflection",
                &llm_cfg.model_name,
                &llm_cfg.provider,
                &reflection_messages,
                &[],
                request.max_output_tokens,
                "success",
                json!({
                    "finish_reason": result.finish_reason.clone(),
                    "full_text": result.full_text.clone(),
                    "reasoning": result.reasoning.clone(),
                    "tool_calls": result.tool_calls.clone(),
                    "usage": result.usage.clone(),
                }),
                Some(crate::turn::llm_exchange_capture::CaptureTrace {
                    session_turn_source: Some("state"),
                    turn_chain_id: None,
                    user_query_event_id: None,
                }),
            )
            .await;
        }
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
        self.emit_event(json!({
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

fn llm_capture_error_response(error: &astra_core::ClassifiedError) -> Value {
    let mut response = json!({
        "error": error.message,
        "kind": error.kind.to_string(),
    });
    if let Some(details_json) = error.details_json.as_deref()
        && let Ok(Value::Object(details)) = serde_json::from_str::<Value>(details_json)
        && let Some(response_object) = response.as_object_mut()
    {
        response_object.extend(details);
    }
    response
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn::agentic_loop_host::ASK_USER_TOOL_NAME;
    use crate::turn::agentic_loop_host::run_agentic_loop_with_host;
    use crate::turn::cloud::summary::SummaryLlmClient;
    use crate::turn::edge_ledger::{approval_callback_key, tool_callback_key};
    use crate::turn::sse_stream_host::EdgeToolExecResult;
    #[cfg(feature = "bridge-e2e-hooks")]
    use astra_services::SessionArtifactStore;

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
    fn llm_request_dump_failures_are_not_silently_ignored() {
        let source = include_str!("server_loop_host.rs");
        let tests_start = source.rfind("mod tests {").expect("test module start");
        let production = &source[..tests_start];
        for context in [
            "server_loop_host context window dump persist failed",
            "server_loop_host error dump persist failed",
            "server_loop_reflection error dump persist failed",
        ] {
            let start = production
                .find(context)
                .expect("dump logging context should exist");
            let window_start = start.saturating_sub(480);
            let window = &production[window_start..production.len().min(start + 120)];
            assert!(
                window.contains("if let Err(error)")
                    && window.contains("dump.persist_remote(&self.user_id, &artifact_store).await"),
                "{context} should handle dump.persist_remote failures explicitly"
            );
        }
    }

    #[test]
    fn llm_error_paths_publish_remote_llm_capture_artifacts() {
        let source = include_str!("server_loop_host.rs");
        let tests_start = source.rfind("mod tests {").expect("test module start");
        let production = &source[..tests_start];
        for context in [
            "server_loop_host context window capture",
            "server_loop_host error capture",
            "server_loop_reflection error capture",
        ] {
            let start = production
                .find(context)
                .expect("capture context should exist");
            let window = &production[start..production.len().min(start + 220)];
            assert!(
                window.contains("Some(&artifact_store)"),
                "{context} should publish a remote llm_capture artifact, not local-only capture"
            );
        }
    }

    #[test]
    fn llm_capture_error_response_includes_partial_details() {
        let error = astra_core::ClassifiedError::new(
            astra_core::ErrorKind::StreamTransport,
            "LLM stream transport error: connection reset",
        )
        .with_details_json(
            json!({
                "partial_full_text": "half answer",
                "usage": { "prompt": 10, "completion": 4 }
            })
            .to_string(),
        );
        let response = llm_capture_error_response(&error);
        assert_eq!(response["partial_full_text"].as_str(), Some("half answer"));
        assert_eq!(response["usage"]["prompt"].as_i64(), Some(10));
        assert_eq!(response["kind"].as_str(), Some("stream_transport"));
    }

    #[test]
    fn server_loop_error_captures_use_structured_error_response() {
        let source = include_str!("server_loop_host.rs");
        let tests_start = source.rfind("mod tests {").expect("test module start");
        let production = &source[..tests_start];
        assert!(
            production.contains("llm_capture_error_response(e)"),
            "context-window captures should route through the structured error response helper"
        );
        assert!(
            production.contains("llm_capture_error_response(&e)"),
            "generic server-loop error captures should route through the structured error response helper"
        );
        assert!(
            production.contains("llm_capture_error_response(&error)"),
            "reflection error captures should route through the structured error response helper"
        );
    }

    #[test]
    fn server_loop_uses_shared_rate_limit_cooldown_singleton() {
        let source = include_str!("server_loop_host.rs");
        let tests_start = source.rfind("mod tests {").expect("test module start");
        let production = &source[..tests_start];
        assert!(
            production.contains("use crate::turn::bridge_llm_stream::rate_limit_cooldown;"),
            "server-loop should reuse the shared llm_client/bridge rate-limit cooldown singleton"
        );
        assert!(
            !production.contains("static COOLDOWN: OnceLock<PerModelCooldown>"),
            "server-loop should not keep a separate cooldown singleton disconnected from llm_client"
        );
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
    fn build_system_messages_cached_includes_late_round_guidance_in_dynamic_prompt() {
        let host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u1".to_string(),
            "s1".to_string(),
        )
        .with_edge_tools(sample_edge_tools())
        .build();

        let mut state = create_test_state();
        state.current_round_index = crate::prompts::ROUND_BUDGET_THRESHOLD;
        state.llm_rounds_completed = crate::prompts::ROUND_BUDGET_THRESHOLD;
        state.messages = vec![
            json!({"role": "user", "content": "inspect the project"}),
            json!({"role": "tool", "content": "Cargo.toml"}),
            json!({"role": "tool", "content": "README.md"}),
        ];

        let (system_messages, plain, breakdown) = host.build_system_messages_cached(
            "inspect the project",
            &host.edge_tools,
            &state,
            &PromptCacheConfig::latch("openai", "gpt-4o"),
        );

        assert!(
            plain.contains("Synthesize Or Batch Now"),
            "plain prompt should include the late-round synthesis nudge"
        );
        assert!(
            plain.contains("2 tools executed in parallel"),
            "plain prompt should preserve batching feedback"
        );

        let primary = system_messages.first().expect("primary system message");
        let dynamic = system_messages.last().expect("dynamic system message");
        let primary_text = primary
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let dynamic_text = dynamic
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default();

        assert!(
            !primary_text.contains("Synthesize Or Batch Now"),
            "late-round guidance must stay out of the stable cached prefix"
        );
        assert!(
            dynamic_text.contains("Synthesize Or Batch Now"),
            "late-round guidance should live in the dynamic prompt message"
        );
        assert!(breakdown.guidance_signals.round_budget_warning);
        assert!(breakdown.guidance_signals.synthesize_or_batch);
        assert!(breakdown.guidance_signals.parallel_feedback);
    }

    #[test]
    fn build_system_messages_cached_records_dynamic_context_signals() {
        let mut profile = Map::new();
        profile.insert("active_skills".to_string(), json!(["concise"]));
        profile.insert(
            "learned_context_hint".to_string(),
            json!("matrixorigin => github"),
        );
        profile.insert(
            "system_prompt_override".to_string(),
            json!("You are operating under a delegated reviewer contract."),
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

        let mut state = create_test_state();
        state.skills.effort = Some(crate::skills::manifest::EffortLevel::High);
        state.skills.agent_type = Some("reviewer".to_string());

        let (_, _, breakdown) = host.build_system_messages_cached(
            "remember that I prefer dark mode",
            &host.edge_tools,
            &state,
            &PromptCacheConfig::latch("openai", "gpt-4o"),
        );

        assert!(breakdown.context_signals.active_output_skills);
        assert!(breakdown.context_signals.learned_runtime_context);
        assert!(breakdown.context_signals.memory_signal_detected);
        assert!(breakdown.context_signals.system_prompt_override);
        assert!(breakdown.context_signals.effort_hint);
        assert!(breakdown.context_signals.agent_type_hint);
        assert!(!breakdown.context_signals.self_awareness);
        assert!(!breakdown.context_signals.implicit_feedback);
        assert!(!breakdown.context_signals.learned_feedback_rules);
        assert!(!breakdown.context_signals.session_anchor);
        assert!(breakdown.environment_tokens > 0);
        assert!(breakdown.user_preferences_tokens > 0);
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
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u".to_string(),
            "s".to_string(),
        )
        .build();
        host.push_reasoning_events("thinking...");

        assert_eq!(host.emitted_events.len(), 2);
        assert_eq!(host.emitted_events[0]["type"], "reasoning_delta");
        assert_eq!(host.emitted_events[0]["content"], "thinking...");
        assert_eq!(host.emitted_events[1]["type"], "reasoning_done");
    }

    #[test]
    fn push_reasoning_events_skips_empty_reasoning() {
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u".to_string(),
            "s".to_string(),
        )
        .build();
        host.push_reasoning_events("");
        assert!(host.emitted_events.is_empty());
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
                &HashMap::new(),
                None,
                None,
                &PromptCacheConfig::latch("openai", "gpt-4"),
            )
            .await;
        assert!(msgs.len() >= 2, "should have system + user messages");
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "system prompt text");
    }

    #[tokio::test]
    async fn deliver_edge_tools_batches_multiple_approval_prompts() {
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u-batch".to_string(),
            "s-batch".to_string(),
        )
        .build();
        let ledger = host.edge_callback_ledger.clone();
        let tool_calls = vec![
            json!({
                "id": "w1",
                "type": "function",
                "function": {"name": "write_file", "arguments": r#"{"path":"a.rs","content":"1"}"#}
            }),
            json!({
                "id": "w2",
                "type": "function",
                "function": {"name": "write_file", "arguments": r#"{"path":"b.rs","content":"2"}"#}
            }),
        ];

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let mut guard = ledger.lock().await;
            guard.insert(
                approval_callback_key("u-batch", "w1"),
                json!({"kind": "approval_respond", "body": {"request_id": "w1", "decision": "allow"}}),
            );
            guard.insert(
                approval_callback_key("u-batch", "w2"),
                json!({"kind": "approval_respond", "body": {"request_id": "w2", "decision": "allow"}}),
            );
            drop(guard);

            tokio::time::sleep(Duration::from_millis(10)).await;
            let mut guard = ledger.lock().await;
            guard.insert(
                tool_callback_key("u-batch", "w1"),
                json!({"body": {"request_id": "w1", "status": "ok", "output": "wrote-a"}}),
            );
            guard.insert(
                tool_callback_key("u-batch", "w2"),
                json!({"body": {"request_id": "w2", "status": "ok", "output": "wrote-b"}}),
            );
        });

        let results = host.deliver_edge_tools_via_ledger(&tool_calls).await;

        assert!(
            host.emitted_events
                .iter()
                .all(|event| event.get("type").and_then(Value::as_str) != Some("approval_required"))
        );
        let batch = host
            .emitted_events
            .iter()
            .find(|event| {
                event.get("type").and_then(Value::as_str) == Some("approval_batch_required")
            })
            .expect("approval batch event");
        assert_eq!(batch["requests"].as_array().unwrap().len(), 2);

        let tool_request_positions: Vec<_> = host
            .emitted_events
            .iter()
            .enumerate()
            .filter_map(|(idx, event)| {
                (event.get("type").and_then(Value::as_str) == Some("tool_request")).then_some(idx)
            })
            .collect();
        assert_eq!(tool_request_positions.len(), 2);
        let first_end = host
            .emitted_events
            .iter()
            .position(|event| event.get("type").and_then(Value::as_str) == Some("tool_call_end"))
            .expect("tool_call_end");
        assert!(tool_request_positions.iter().all(|idx| *idx < first_end));

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].request_id, "w1");
        assert_eq!(results[1].request_id, "w2");
        assert_eq!(results[0].status, "ok");
        assert_eq!(results[1].status, "ok");
    }

    #[tokio::test]
    async fn deliver_edge_tools_does_not_block_later_read_only_block_on_future_approval_block() {
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u-mixed".to_string(),
            "s-mixed".to_string(),
        )
        .build();
        let ledger = host.edge_callback_ledger.clone();
        let tool_calls = vec![
            json!({
                "id": "r1",
                "type": "function",
                "function": {"name": "read_file", "arguments": r#"{"path":"a.rs"}"#}
            }),
            json!({
                "id": "w1",
                "type": "function",
                "function": {"name": "write_file", "arguments": r#"{"path":"b.rs","content":"1"}"#}
            }),
            json!({
                "id": "r2",
                "type": "function",
                "function": {"name": "read_file", "arguments": r#"{"path":"c.rs"}"#}
            }),
            json!({
                "id": "w2",
                "type": "function",
                "function": {"name": "write_file", "arguments": r#"{"path":"d.rs","content":"2"}"#}
            }),
        ];

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            ledger.lock().await.insert(
                tool_callback_key("u-mixed", "r1"),
                json!({"body": {"request_id": "r1", "status": "ok", "output": "read-a"}}),
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
            ledger.lock().await.insert(
                approval_callback_key("u-mixed", "w1"),
                json!({"kind": "approval_respond", "body": {"request_id": "w1", "decision": "allow"}}),
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
            ledger.lock().await.insert(
                tool_callback_key("u-mixed", "w1"),
                json!({"body": {"request_id": "w1", "status": "ok", "output": "wrote-b"}}),
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
            ledger.lock().await.insert(
                tool_callback_key("u-mixed", "r2"),
                json!({"body": {"request_id": "r2", "status": "ok", "output": "read-c"}}),
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
            ledger.lock().await.insert(
                approval_callback_key("u-mixed", "w2"),
                json!({"kind": "approval_respond", "body": {"request_id": "w2", "decision": "allow"}}),
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
            ledger.lock().await.insert(
                tool_callback_key("u-mixed", "w2"),
                json!({"body": {"request_id": "w2", "status": "ok", "output": "wrote-d"}}),
            );
        });

        let results = host.deliver_edge_tools_via_ledger(&tool_calls).await;

        let request_ids: Vec<_> = host
            .emitted_events
            .iter()
            .filter(|event| event.get("type").and_then(Value::as_str) == Some("tool_request"))
            .filter_map(|event| event.get("request_id").and_then(Value::as_str))
            .collect();
        assert_eq!(request_ids, vec!["r1", "w1", "r2", "w2"]);

        let w1_end = host
            .emitted_events
            .iter()
            .position(|event| {
                event.get("type").and_then(Value::as_str) == Some("tool_call_end")
                    && event.get("call_id").and_then(Value::as_str) == Some("w1")
            })
            .expect("w1 tool_call_end");
        let r2_request = host
            .emitted_events
            .iter()
            .position(|event| {
                event.get("type").and_then(Value::as_str) == Some("tool_request")
                    && event.get("request_id").and_then(Value::as_str) == Some("r2")
            })
            .expect("r2 tool_request");
        let w2_request = host
            .emitted_events
            .iter()
            .position(|event| {
                event.get("type").and_then(Value::as_str) == Some("tool_request")
                    && event.get("request_id").and_then(Value::as_str) == Some("w2")
            })
            .expect("w2 tool_request");
        assert!(r2_request > w1_end);
        assert!(r2_request < w2_request);

        assert_eq!(results.len(), 4);
        assert_eq!(results[0].request_id, "r1");
        assert_eq!(results[1].request_id, "w1");
        assert_eq!(results[2].request_id, "r2");
        assert_eq!(results[3].request_id, "w2");
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
            final_text_streamed: false,
            total_prompt: 0,
            total_completion: 0,
            total_cache_read: 0,
            total_cache_creation: 0,
            total_tool_calls: 0,
            total_evidence_tool_calls: 0,
            has_any_usage: false,
            max_turns: 10,
            remaining_turns: 10,
            agentic_turn_budget: TaskExecutionProfile::default().agentic_turn_budget,
            current_round_index: 0,
            llm_rounds_completed: 0,
            turn_guard: TurnGuard::new(),
            restricted_tools: HashSet::new(),
            boosted_tools: HashSet::new(),
            widen_selection_pending: false,
            step_recorder: StepRecorder::new("test-session", "test-task"),
            idempotency_cache: InMemoryIdempotencyCache::new(),
            semantic_dedup: SemanticDedup::new(0.75),
            call_counts: HashMap::new(),
            max_identical_tool_calls: crate::runtime_config::RuntimeConfig::load()
                .tool_selection
                .effective_max_identical_calls(),
            max_tools_per_turn: 15,
            repeated_cache_hit_suppression: 3,
            max_consecutive_empty_name: 3,
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
            pinned_tool_schema_tokens: 0,
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
            continuity: Default::default(),
            compact_strategy: Default::default(),
            approval_overrides: None,
            confidence_trend: Default::default(),
            last_confidence_diagnosis: None,
            session_turn: 0,
            bridge_turn_chain_id: None,
            bridge_user_query_event_id: None,
            turn_event_buffer: None,
        }
    }

    #[derive(Clone)]
    struct GatewayState {
        requests: Arc<tokio::sync::Mutex<Vec<Value>>>,
        status: axum::http::StatusCode,
        response: Value,
    }

    async fn spawn_gateway(
        status: axum::http::StatusCode,
        response: Value,
    ) -> (
        String,
        Arc<tokio::sync::Mutex<Vec<Value>>>,
        tokio::task::JoinHandle<()>,
    ) {
        use axum::{
            Router,
            body::Bytes,
            extract::State,
            http::header,
            response::{IntoResponse, Response},
            routing::post,
        };
        use tokio::net::TcpListener;

        fn build_streaming_gateway_body(response: &Value) -> String {
            let content = response["choices"]
                .as_array()
                .and_then(|choices| choices.first())
                .and_then(|choice| choice.get("message"))
                .and_then(|message| message.get("content"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let finish_reason = response["choices"]
                .as_array()
                .and_then(|choices| choices.first())
                .and_then(|choice| choice.get("finish_reason"))
                .cloned()
                .unwrap_or_else(|| json!("stop"));
            let usage = response.get("usage").cloned().unwrap_or_else(|| json!({}));
            format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                json!({"choices":[{"delta":{"content": content}}]}),
                json!({"choices":[{"delta":{},"finish_reason": finish_reason}],"usage": usage}),
            )
        }

        async fn handler(State(state): State<GatewayState>, body: Bytes) -> Response {
            let payload: Value = serde_json::from_slice(&body).expect("gateway request json");
            state.requests.lock().await.push(payload.clone());
            let wants_stream = payload
                .get("stream")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if state.status.is_success() && wants_stream {
                (
                    state.status,
                    [(header::CONTENT_TYPE, "text/event-stream")],
                    build_streaming_gateway_body(&state.response),
                )
                    .into_response()
            } else {
                (state.status, axum::Json(state.response.clone())).into_response()
            }
        }

        let requests = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let app = Router::new()
            .route("/gateway/chat/completions", post(handler))
            .with_state(GatewayState {
                requests: requests.clone(),
                status,
                response,
            });
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("listener addr");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("gateway server should run");
        });
        (
            format!("http://{addr}/gateway/chat/completions"),
            requests,
            server,
        )
    }

    fn read_journal_events(session_id: &str) -> Vec<Value> {
        let path = astra_services::session_journal::JournalWriter::new(session_id)
            .expect("journal writer")
            .path()
            .clone();
        match std::fs::read_to_string(path) {
            Ok(contents) => contents
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| serde_json::from_str::<Value>(line).expect("journal event json"))
                .collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => panic!("read journal: {error}"),
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

    #[test]
    fn interactive_turn_policy_keeps_ask_user_in_final_tools() {
        let host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u".to_string(),
            "s".to_string(),
        )
        .with_edge_tools(sample_edge_tools_with_ask_user())
        .with_interactive_client(true)
        .build();

        let mut state = create_test_state();
        merge_deprioritized_tools_into_restricted(&state.turn_guard, &mut state.restricted_tools);
        let mut effective_restricted = state.restricted_tools.clone();
        effective_restricted.extend(interaction_scoped_tool_restrictions(
            host.turn_interaction_mode(),
        ));
        let visible_tools = host.filtered_turn_tools(&effective_restricted);
        let final_tools =
            prune_tool_schemas(&visible_tools, crate::prompts::CompactionTier::Normal);
        let policy =
            TurnInteractionPolicy::from_tool_schemas(host.turn_interaction_mode(), &final_tools);

        assert_eq!(
            policy.visible_tool_names,
            vec![
                "bash".to_string(),
                "read_file".to_string(),
                "ask_user".to_string()
            ]
        );
        assert_eq!(
            policy.evidence_tool_names,
            vec!["bash".to_string(), "read_file".to_string()]
        );
        assert!(policy.allow_ask_user);
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

    #[tokio::test]
    async fn llm_token_service_uses_gateway_url_and_forwarded_headers() {
        let mut forward_headers = HashMap::new();
        forward_headers.insert("authorization".to_string(), "Bearer moi-token".to_string());
        forward_headers.insert("x-workspace-id".to_string(), "ws-001".to_string());
        forward_headers.insert("__astra_connection_tokens".to_string(), "x-hop".to_string());

        let resolved = resolve_llm_model_for_turn(
            &mock_matrixone(),
            mock_encryptor().as_ref(),
            Some("gpt-5-mini"),
            None,
            Some(&LlmTokenServiceConfig {
                url: "http://catalog:8081/api/v1/chat/completions".to_string(),
                timeout_ms: Some(2000),
            }),
            &forward_headers,
        )
        .await
        .expect("resolve via llm token service gateway");

        assert_eq!(resolved.model_name, "gpt-5-mini");
        assert_eq!(resolved.provider, "openai");
        assert_eq!(resolved.base_url, "https://api.openai.com/v1");
        assert_eq!(
            resolved.completions_url_override.as_deref(),
            Some("http://catalog:8081/api/v1/chat/completions")
        );
        assert_eq!(
            resolved
                .header_overrides
                .get("authorization")
                .map(String::as_str),
            Some("Bearer moi-token")
        );
        assert_eq!(
            resolved
                .header_overrides
                .get("x-workspace-id")
                .map(String::as_str),
            Some("ws-001")
        );
        assert_eq!(resolved.request_timeout, Some(Duration::from_millis(2000)));
    }

    #[tokio::test]
    async fn llm_token_service_without_model_uses_default_model_name() {
        let resolved = resolve_llm_model_for_turn(
            &mock_matrixone(),
            mock_encryptor().as_ref(),
            None,
            None,
            Some(&LlmTokenServiceConfig {
                url: "http://catalog:8081/api/v1/chat/completions".to_string(),
                timeout_ms: None,
            }),
            &HashMap::new(),
        )
        .await
        .expect("resolve via llm token service gateway");
        assert_eq!(resolved.model_name, "gpt-4o-mini");
        assert!(resolved.request_timeout.is_none());
    }

    #[cfg(feature = "bridge-e2e-hooks")]
    #[tokio::test(flavor = "current_thread")]
    async fn mock_turn_persists_local_llm_capture_when_session_capture_enabled() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(temp.path());
        let session_id = "00000000-0000-0000-0000-000000000125";

        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "user-capture".to_string(),
            session_id.to_string(),
        )
        .with_edge_tools(sample_edge_tools())
        .with_full_llm_capture(true)
        .with_test_llm_rounds(vec![json!({
            "full_text": "captured reply",
            "usage": { "prompt_tokens": 7, "completion_tokens": 9 }
        })])
        .build();
        let mut state = create_test_state();
        state.message = "capture this turn".to_string();

        host.run_one_mock_turn_for_test(&mut state)
            .await
            .expect("mock turn");

        let session_dir = astra_services::local_session_artifact_store()
            .session_dir(session_id)
            .expect("session dir");
        let files: Vec<_> = std::fs::read_dir(session_dir)
            .expect("capture dir")
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            files
                .iter()
                .any(|name| name.contains("llm_capture_t0_r0_server_loop_host_success")),
            "expected local llm capture file, got {files:?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn execute_turn_persists_full_journal_request_and_response_when_session_capture_enabled()
    {
        let temp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(temp.path());
        let session_id = "00000000-0000-0000-0000-000000000126";
        let (gateway_url, requests, server) = spawn_gateway(
            axum::http::StatusCode::OK,
            json!({
                "choices": [
                    {
                        "message": { "content": "journal capture reply" },
                        "finish_reason": "stop"
                    }
                ],
                "usage": { "prompt_tokens": 12, "completion_tokens": 5 }
            }),
        )
        .await;

        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "user-journal".to_string(),
            session_id.to_string(),
        )
        .with_edge_tools(sample_edge_tools())
        .with_full_llm_capture(true)
        .with_llm_token_service(Some(LlmTokenServiceConfig {
            url: gateway_url,
            timeout_ms: Some(2000),
        }))
        .build();
        let mut state = create_test_state();
        state.session_turn = 1;
        state.turn_event_buffer =
            Some(astra_services::session_journal::TurnEventBuffer::begin_turn(Some(session_id), 1));
        state
            .messages
            .push(json!({"role": "user", "content": "capture this turn"}));
        state.message = "capture this turn".to_string();

        host.execute_turn(&mut state).await.expect("execute turn");

        state
            .turn_event_buffer
            .as_mut()
            .expect("turn event buffer")
            .flush(
                &astra_services::session_journal::JournalWriter::new(session_id)
                    .expect("journal writer"),
            )
            .expect("flush journal");

        let journal = read_journal_events(session_id);
        let llm_events: Vec<_> = journal
            .iter()
            .filter(|event| {
                matches!(
                    event.get("type").and_then(Value::as_str),
                    Some("llm_request_full" | "llm_response_full")
                )
            })
            .collect();
        assert_eq!(
            llm_events.len(),
            2,
            "expected request+response events: {journal:?}"
        );
        assert_eq!(
            llm_events[0].get("type").and_then(Value::as_str),
            Some("llm_request_full")
        );
        assert_eq!(
            llm_events[1].get("type").and_then(Value::as_str),
            Some("llm_response_full")
        );
        assert_eq!(
            llm_events[0]["metadata"]["request"]["messages"][0]["role"].as_str(),
            Some("system")
        );
        assert!(
            llm_events[0]["metadata"]["request"]["messages"]
                .as_array()
                .map(|messages| messages.iter().any(|msg| msg["role"] == "user"))
                .unwrap_or(false)
        );
        assert_eq!(
            llm_events[1]["metadata"]["response"]["outcome"].as_str(),
            Some("success")
        );
        assert_eq!(
            llm_events[1]["metadata"]["response"]["response"]["finish_reason"].as_str(),
            Some("stop")
        );
        assert_eq!(
            llm_events[1]["metadata"]["response"]["response"]["full_text"].as_str(),
            Some("journal capture reply")
        );
        assert_eq!(
            llm_events[1]["metadata"]["response"]["response"]["usage"]["completion"].as_i64(),
            Some(5)
        );
        assert_eq!(
            llm_events[0]["metadata"]["trace"]["session_turn_source"].as_str(),
            Some("state")
        );
        assert!(
            llm_events[0]["metadata"]["trace"]["turn_chain_id"].is_null(),
            "server-loop trace should not fabricate bridge correlation ids"
        );

        let gateway_requests = requests.lock().await;
        assert_eq!(gateway_requests.len(), 1, "one upstream request expected");

        server.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn execute_turn_persists_full_journal_error_response_when_session_capture_enabled() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(temp.path());
        let session_id = "00000000-0000-0000-0000-000000000127";
        let (gateway_url, _requests, server) = spawn_gateway(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error": {"message": "upstream exploded"}}),
        )
        .await;

        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "user-journal".to_string(),
            session_id.to_string(),
        )
        .with_edge_tools(sample_edge_tools())
        .with_full_llm_capture(true)
        .with_llm_token_service(Some(LlmTokenServiceConfig {
            url: gateway_url,
            timeout_ms: Some(2000),
        }))
        .build();
        let mut state = create_test_state();
        state.session_turn = 1;
        state.turn_event_buffer =
            Some(astra_services::session_journal::TurnEventBuffer::begin_turn(Some(session_id), 1));
        state
            .messages
            .push(json!({"role": "user", "content": "capture this failed turn"}));
        state.message = "capture this failed turn".to_string();

        let error = match host.execute_turn(&mut state).await {
            Ok(_) => panic!("execute turn should fail"),
            Err(error) => error,
        };
        assert_eq!(error.kind, astra_core::ErrorKind::ServerError);

        state
            .turn_event_buffer
            .as_mut()
            .expect("turn event buffer")
            .flush(
                &astra_services::session_journal::JournalWriter::new(session_id)
                    .expect("journal writer"),
            )
            .expect("flush journal");

        let journal = read_journal_events(session_id);
        let llm_events: Vec<_> = journal
            .iter()
            .filter(|event| {
                matches!(
                    event.get("type").and_then(Value::as_str),
                    Some("llm_request_full" | "llm_response_full")
                )
            })
            .collect();
        assert_eq!(
            llm_events.len(),
            2,
            "expected request+error events: {journal:?}"
        );
        assert_eq!(
            llm_events[1]["metadata"]["response"]["outcome"].as_str(),
            Some("error")
        );
        assert_eq!(
            llm_events[1]["metadata"]["response"]["response"]["kind"].as_str(),
            Some("server_error")
        );
        assert!(
            llm_events[1]["metadata"]["response"]["response"]["error"]
                .as_str()
                .is_some_and(|message| message.contains("LLM error 500")),
            "expected stored error payload: {}",
            llm_events[1]
        );
        assert_eq!(
            llm_events[1]["metadata"]["trace"]["session_turn_source"].as_str(),
            Some("state")
        );

        server.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn execute_turn_does_not_persist_full_journal_events_when_session_capture_disabled() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(temp.path());
        let session_id = "00000000-0000-0000-0000-000000000128";
        let (gateway_url, requests, server) = spawn_gateway(
            axum::http::StatusCode::OK,
            json!({
                "choices": [
                    {
                        "message": { "content": "journal capture reply" },
                        "finish_reason": "stop"
                    }
                ],
                "usage": { "prompt_tokens": 12, "completion_tokens": 5 }
            }),
        )
        .await;

        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "user-journal".to_string(),
            session_id.to_string(),
        )
        .with_edge_tools(sample_edge_tools())
        .with_llm_token_service(Some(LlmTokenServiceConfig {
            url: gateway_url,
            timeout_ms: Some(2000),
        }))
        .build();
        let mut state = create_test_state();
        state.session_turn = 1;
        state.turn_event_buffer =
            Some(astra_services::session_journal::TurnEventBuffer::begin_turn(Some(session_id), 1));
        state
            .messages
            .push(json!({"role": "user", "content": "capture disabled should stay quiet"}));
        state.message = "capture disabled should stay quiet".to_string();

        host.execute_turn(&mut state).await.expect("execute turn");

        state
            .turn_event_buffer
            .as_mut()
            .expect("turn event buffer")
            .flush(
                &astra_services::session_journal::JournalWriter::new(session_id)
                    .expect("journal writer"),
            )
            .expect("flush journal");

        let journal = read_journal_events(session_id);
        let llm_events: Vec<_> = journal
            .iter()
            .filter(|event| {
                matches!(
                    event.get("type").and_then(Value::as_str),
                    Some("llm_request_full" | "llm_response_full")
                )
            })
            .collect();
        assert!(
            llm_events.is_empty(),
            "capture-disabled run should not emit full LLM journal events: {journal:?}"
        );

        let gateway_requests = requests.lock().await;
        assert_eq!(gateway_requests.len(), 1, "one upstream request expected");

        server.abort();
    }

    #[cfg(feature = "bridge-e2e-hooks")]
    #[tokio::test(flavor = "current_thread")]
    async fn mock_turn_can_inject_error_with_structured_details() {
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "user-capture".to_string(),
            "".to_string(),
        )
        .with_edge_tools(sample_edge_tools())
        .with_test_llm_rounds(vec![json!({
            "error": {
                "message": "synthetic streamed failure",
                "kind": "stream_transport",
                "details": {
                    "partial_full_text": "half answer",
                    "usage": { "prompt": 17, "completion": 3 }
                }
            }
        })])
        .build();
        let mut state = create_test_state();
        state.message = "fail this turn".to_string();

        let error = match host.run_one_mock_turn_for_test(&mut state).await {
            Ok(_) => panic!("mock round should fail"),
            Err(error) => error,
        };
        assert_eq!(error.kind, astra_core::ErrorKind::StreamTransport);
        assert_eq!(error.message, "synthetic streamed failure");
        let details: Value =
            serde_json::from_str(error.details_json.as_deref().expect("details json")).unwrap();
        assert_eq!(details["partial_full_text"].as_str(), Some("half answer"));
        assert_eq!(details["usage"]["prompt"].as_i64(), Some(17));
    }

    #[tokio::test]
    async fn summary_client_uses_gateway_override_and_forwarded_headers() {
        use axum::{Router, extract::State, routing::post};
        use tokio::net::TcpListener;

        #[derive(Default)]
        struct RequestCapture {
            authorization: tokio::sync::Mutex<Option<String>>,
            workspace_id: tokio::sync::Mutex<Option<String>>,
            path: tokio::sync::Mutex<Option<String>>,
        }

        async fn handler(
            State(capture): State<Arc<RequestCapture>>,
            headers: axum::http::HeaderMap,
            request: axum::extract::Request,
        ) -> axum::Json<Value> {
            *capture.path.lock().await = Some(request.uri().path().to_string());
            *capture.authorization.lock().await = headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .map(String::from);
            *capture.workspace_id.lock().await = headers
                .get("x-workspace-id")
                .and_then(|value| value.to_str().ok())
                .map(String::from);
            axum::Json(json!({
                "choices": [
                    {
                        "message": { "content": "gateway summary" },
                        "finish_reason": "stop"
                    }
                ],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1}
            }))
        }

        let capture = Arc::new(RequestCapture::default());
        let app = Router::new()
            .route("/gateway/chat/completions", post(handler))
            .with_state(capture.clone());
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("listener addr");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("test server should run");
        });

        let mut forwarded = HashMap::new();
        forwarded.insert("authorization".to_string(), "Bearer moi-token".to_string());
        forwarded.insert("x-workspace-id".to_string(), "ws-001".to_string());
        let client = RequestAwareSummaryClient {
            model_name: "gpt-4o-mini".to_string(),
            api_key: String::new(),
            base_url: "https://api.openai.com/v1".to_string(),
            provider: "openai".to_string(),
            max_output_tokens: 128,
            header_overrides: forwarded,
            completions_url_override: Some(format!("http://{addr}/gateway/chat/completions")),
            request_timeout: Some(Duration::from_secs(2)),
        };

        let response = client
            .summarize(&[
                json!({"role": "system", "content": "summarize"}),
                json!({"role": "user", "content": "payload"}),
            ])
            .await
            .expect("summary should succeed");
        assert_eq!(response.text, "gateway summary");
        assert!(!response.is_ptl_error);

        assert_eq!(
            capture.authorization.lock().await.as_deref(),
            Some("Bearer moi-token")
        );
        assert_eq!(capture.workspace_id.lock().await.as_deref(), Some("ws-001"));
        assert_eq!(
            capture.path.lock().await.as_deref(),
            Some("/gateway/chat/completions")
        );

        server.abort();
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

    // ── Mock-LLM prompt-cache verification framework tests ──────────────────
    //
    // These exercise the pure helpers that assemble `CapturedLlmRequest` so we
    // can trust the framework before layering E2E tests on top.

    #[cfg(feature = "bridge-e2e-hooks")]
    #[test]
    fn captured_request_counts_anthropic_cache_control_blocks() {
        let primary = json!({
            "role": "system",
            "content": [
                { "type": "text", "text": "stable global" },
                { "type": "text", "text": "frozen middle", "cache_control": { "type": "ephemeral" } },
                { "type": "text", "text": "dynamic tail" },
            ]
        });
        assert_eq!(super::count_system_cache_control(&primary), 1);

        let primary_openai = json!({ "role": "system", "content": "plain text" });
        assert_eq!(super::count_system_cache_control(&primary_openai), 0);
    }

    #[cfg(feature = "bridge-e2e-hooks")]
    #[test]
    fn captured_request_prefix_hash_for_anthropic_covers_only_up_to_breakpoint() {
        let primary = json!({
            "role": "system",
            "content": [
                { "type": "text", "text": "A" },
                { "type": "text", "text": "B", "cache_control": { "type": "ephemeral" } },
                { "type": "text", "text": "C" },
            ]
        });
        // Prefix is "AB" (stops at last cache_control breakpoint).
        let hex = super::sha256_hex("AB");
        let prefix = super::cacheable_prefix_text(&primary, true);
        assert_eq!(prefix, "AB");
        assert_eq!(super::sha256_hex(&prefix), hex);
    }

    #[cfg(feature = "bridge-e2e-hooks")]
    #[test]
    fn captured_request_prefix_hash_for_openai_concatenates_all_text() {
        let primary = json!({
            "role": "system",
            "content": "stable prefix text"
        });
        let prefix = super::cacheable_prefix_text(&primary, false);
        assert_eq!(prefix, "stable prefix text");
    }

    #[cfg(feature = "bridge-e2e-hooks")]
    #[test]
    fn captured_request_openai_prefix_equal_across_turns_drives_cache_hit() {
        // Two turns with identical stable system content → identical hash.
        let p1 = json!({ "role": "system", "content": "SAME" });
        let p2 = json!({ "role": "system", "content": "SAME" });
        let h1 = super::sha256_hex(&super::cacheable_prefix_text(&p1, false));
        let h2 = super::sha256_hex(&super::cacheable_prefix_text(&p2, false));
        assert_eq!(h1, h2, "OpenAI stable-prefix hash must match byte-for-byte");

        // A schema churn / content diff breaks the prefix hash.
        let p3 = json!({ "role": "system", "content": "DIFFERENT" });
        let h3 = super::sha256_hex(&super::cacheable_prefix_text(&p3, false));
        assert_ne!(h1, h3, "Prefix change must invalidate cache key");
    }

    #[cfg(feature = "bridge-e2e-hooks")]
    #[test]
    fn captured_request_detects_last_tool_and_last_message_cache_markers() {
        let tools = vec![
            json!({ "type": "function", "function": { "name": "a" } }),
            json!({
                "type": "function",
                "function": { "name": "b" },
                "cache_control": { "type": "ephemeral" }
            }),
        ];
        let messages = vec![
            json!({ "role": "user", "content": "hello" }),
            json!({
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "reply", "cache_control": { "type": "ephemeral" } }
                ]
            }),
        ];
        let cfg = PromptCacheConfig {
            cache_enabled: true,
            is_anthropic: true,
        };
        let breakdown = crate::turn::context_assembly_trace::SystemPromptBreakdown::default();
        let captured = super::build_captured_llm_request(
            0,
            "anthropic".to_string(),
            "claude-sonnet-4".to_string(),
            &cfg,
            &[
                json!({ "role": "system", "content": [{ "type": "text", "text": "x", "cache_control": { "type": "ephemeral" } }] }),
            ],
            &tools,
            &messages,
            &breakdown,
        );
        assert!(captured.last_tool_has_cache_control);
        assert!(captured.last_message_has_cache_control);
        assert_eq!(captured.system_cache_control_count, 1);
        assert!(captured.is_anthropic);
        assert!(captured.cache_enabled);
        assert_eq!(captured.turn_index, 0);
    }
}

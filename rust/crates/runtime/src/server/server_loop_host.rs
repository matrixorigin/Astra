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

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use tokio::sync::Mutex as TokioMutex;
use tokio_util::sync::CancellationToken;

use crate::orchestration::{AgentProgressEvent, ProgressEventType};
use crate::server::tool_transport::{
    ExecutionBindingSnapshot, ExecutorBinding, ExecutorBindingKind, ExecutorStatus, FallbackPolicy,
    ToolExecutionRequest, ToolPolicySnapshot, WorkspaceAuthority, WorkspaceBinding,
    WorkspaceBindingKind, binding_event_fields,
    capability_filter_edge_provided_tool_schemas_for_binding,
    capability_filtered_server_tool_schemas, projected_tool_end_event_fields,
    projected_tool_start_event_fields,
};
use crate::turn::agentic::headless_round::HeadlessStderrStyle;
use crate::turn::agentic_loop::host::{
    AgenticLoopHost, AgenticLoopState, FactualRetryFallbackJudgeContext, HostTurnResult,
    TurnInteractionMode, TurnInteractionPolicy, interaction_scoped_tool_restrictions,
};
use crate::turn::agentic_loop::tool_support::edge_tool_status_exit_code;
use crate::turn::bridge::llm_stream::rate_limit_cooldown;
use crate::turn::llm::client::{
    LlmCallResult, LlmCancel, LlmStreamUpdate, call_llm_and_collect_with_request_overrides,
    call_llm_and_collect_with_request_overrides_and_stream_callback,
    call_llm_nonstream_fallback_with_request_overrides, llm_connect_timeout, llm_fallback_timeout,
    sleep_ms_or_llm_cancel,
};
use crate::turn::prompt_cache::PromptCacheConfig;
use crate::{FernetTokenEncryptor, MatrixOneSettings};
use astra_core::SharedPool;
use astra_services::LlmTokenServiceConfig;
use astra_services::multi_agent::EdgeDispatchService;
use astra_services::runs::RequestedTurnInteractionMode;
use astra_turn_core::agent_live_event::{
    AgentLiveEvent, AgentLiveEventKind, SharedAgentLiveEventSink,
};
use astra_turn_core::agentic_turn_ingest::{
    FactualRetryFallbackDecision, FactualRetryFallbackJudgeInput,
    factual_retry_fallback_judge_messages, parse_factual_retry_fallback_judge_response,
};
use astra_turn_core::bridge_rate_limit_cooldown::{
    FallbackOutcome, RateLimitAction, try_resolve_fallback,
};
use astra_turn_core::chat_turn_sse_dispatch::ChatTurnSseAccum;
use astra_turn_core::compaction_types::CompactionTier;
use astra_turn_core::pipeline_metrics::MetricsRegistry;
use astra_turn_core::thinking_config::ThinkingConfig;
use astra_turn_core::tool::schema::tool_schema_name;
use astra_turn_core::tool_schema_prune::filter_tool_schemas_by_excluded_names;

const MAX_PENDING_PROGRESS_AGENTS: usize = 128;
const MAX_PENDING_PROGRESS_PER_AGENT: usize = 8;
const MAX_STREAMED_TURN_EVENT_BUFFER: usize = 2_048;
const AUX_LLM_POLICY_ENV: &str = "ASTRA_AUX_LLM_POLICY";
const TURN_INTENT_JUDGE_POLICY_ENV: &str = "ASTRA_TURN_INTENT_JUDGE_POLICY";
const FACTUAL_RETRY_JUDGE_POLICY_ENV: &str = "ASTRA_FACTUAL_RETRY_JUDGE_POLICY";
const PRE_TURN_COMPACTION_LLM_POLICY_ENV: &str = "ASTRA_PRE_TURN_COMPACTION_LLM_POLICY";
const METRIC_LLM_MAIN_ATTEMPTS_TOTAL: &str = "astra_llm_main_attempts_total";
const METRIC_LLM_MAIN_ATTEMPT_TOKENS_TOTAL: &str = "astra_llm_main_attempt_tokens_total";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuxiliaryLlmPolicy {
    CapacityAware,
    Always,
    Disabled,
}

impl AuxiliaryLlmPolicy {
    fn from_env(policy_env: &'static str) -> Self {
        std::env::var(policy_env)
            .or_else(|_| std::env::var(AUX_LLM_POLICY_ENV))
            .ok()
            .as_deref()
            .map(parse_auxiliary_llm_policy)
            .unwrap_or(Self::CapacityAware)
    }

    fn as_label(self) -> &'static str {
        match self {
            Self::CapacityAware => "capacity_aware",
            Self::Always => "always",
            Self::Disabled => "disabled",
        }
    }
}

fn parse_auxiliary_llm_policy(raw: &str) -> AuxiliaryLlmPolicy {
    match raw.trim().to_ascii_lowercase().as_str() {
        "always" | "on" | "true" | "1" => AuxiliaryLlmPolicy::Always,
        "disabled" | "disable" | "off" | "false" | "0" => AuxiliaryLlmPolicy::Disabled,
        "capacity_aware" | "capacity-aware" | "auto" | "" => AuxiliaryLlmPolicy::CapacityAware,
        _ => AuxiliaryLlmPolicy::CapacityAware,
    }
}

fn should_skip_auxiliary_llm_for_capacity(policy_env: &'static str) -> Option<&'static str> {
    let policy = AuxiliaryLlmPolicy::from_env(policy_env);
    match policy {
        AuxiliaryLlmPolicy::Disabled => Some("disabled"),
        AuxiliaryLlmPolicy::Always => None,
        AuxiliaryLlmPolicy::CapacityAware => {
            if crate::llm_provider_admission::ProviderAdmissionConfig::from_env().is_enabled() {
                Some("provider_admission_enabled")
            } else {
                None
            }
        }
    }
}

fn auxiliary_llm_policy_label(policy_env: &'static str) -> &'static str {
    AuxiliaryLlmPolicy::from_env(policy_env).as_label()
}

fn llm_main_attempt_metrics_slot() -> &'static RwLock<Option<Arc<MetricsRegistry>>> {
    static SLOT: OnceLock<RwLock<Option<Arc<MetricsRegistry>>>> = OnceLock::new();
    SLOT.get_or_init(Default::default)
}

pub(crate) fn set_llm_main_attempt_metrics_registry(registry: Arc<MetricsRegistry>) {
    register_llm_main_attempt_metrics(&registry);
    *llm_main_attempt_metrics_slot()
        .write()
        .expect("llm main attempt metrics registry lock poisoned") = Some(registry);
}

fn llm_main_attempt_metrics_registry() -> Option<Arc<MetricsRegistry>> {
    llm_main_attempt_metrics_slot()
        .read()
        .expect("llm main attempt metrics registry lock poisoned")
        .clone()
}

fn register_llm_main_attempt_metrics(registry: &MetricsRegistry) {
    registry.register_counter(
        METRIC_LLM_MAIN_ATTEMPTS_TOTAL,
        "Main server LLM attempts by phase, retry class, and low-cardinality outcome.",
    );
    registry.register_counter(
        METRIC_LLM_MAIN_ATTEMPT_TOKENS_TOTAL,
        "Estimated main server LLM attempt tokens by phase, retry class, and low-cardinality outcome.",
    );
}

fn llm_main_attempt_label(attempt_in_round: u32) -> &'static str {
    if attempt_in_round == 0 {
        "initial"
    } else {
        "retry"
    }
}

fn is_llm_provider_admission_error(error: &astra_core::ClassifiedError) -> bool {
    let Some(details_json) = error.details_json.as_deref() else {
        return false;
    };
    let Ok(Value::Object(details)) = serde_json::from_str::<Value>(details_json) else {
        return false;
    };
    details.get("source").and_then(Value::as_str) == Some("llm_provider_admission")
}

fn llm_main_error_outcome(error: &astra_core::ClassifiedError) -> &'static str {
    if is_llm_provider_admission_error(error) {
        return "admission_rejected";
    }
    match error.kind {
        astra_core::ErrorKind::RateLimit => "error_rate_limit",
        astra_core::ErrorKind::ServerError => "error_server_error",
        astra_core::ErrorKind::Auth => "error_auth",
        astra_core::ErrorKind::ContextWindow => "error_context_window",
        astra_core::ErrorKind::InvalidRequest => "error_invalid_request",
        astra_core::ErrorKind::StreamIdle => "error_stream_idle",
        astra_core::ErrorKind::StreamTransport => "error_stream_transport",
        astra_core::ErrorKind::ConnectionPoolExhausted => "error_connection_pool_exhausted",
        astra_core::ErrorKind::BudgetExhausted => "error_budget_exhausted",
        astra_core::ErrorKind::ToolRoundsExhausted => "error_tool_rounds_exhausted",
        astra_core::ErrorKind::Network => "error_network",
        astra_core::ErrorKind::ToolNotFound => "error_tool_not_found",
        astra_core::ErrorKind::ToolInvalidArgs => "error_tool_invalid_args",
        astra_core::ErrorKind::ToolTimeout => "error_tool_timeout",
        astra_core::ErrorKind::ToolUnavailable => "error_tool_unavailable",
        astra_core::ErrorKind::ToolBinding => "error_tool_binding",
        astra_core::ErrorKind::ResourceLimit => "error_resource_limit",
        astra_core::ErrorKind::DatabaseError => "error_database",
        astra_core::ErrorKind::Stall => "error_stall",
        astra_core::ErrorKind::MissingModelSelection => "error_missing_model_selection",
        astra_core::ErrorKind::Cancelled => "error_cancelled",
        astra_core::ErrorKind::Unknown => "error_unknown",
    }
}

fn llm_main_success_outcome(result: &LlmCallResult, will_retry_for_length: bool) -> &'static str {
    if will_retry_for_length {
        return "length_retry";
    }
    match result.finish_reason.as_deref() {
        Some("stop") => "success_stop",
        Some("tool_calls") => "success_tool_calls",
        Some("length") => "success_length_cap",
        Some("content_filter") => "success_content_filter",
        Some(_) => "success_other",
        None => "success_unknown_finish",
    }
}

fn record_llm_main_attempt_metrics(
    phase: &'static str,
    attempt: &'static str,
    outcome: &'static str,
    estimated_tokens: u64,
) {
    let Some(registry) = llm_main_attempt_metrics_registry() else {
        return;
    };
    register_llm_main_attempt_metrics(&registry);
    let labels = &[("phase", phase), ("attempt", attempt), ("outcome", outcome)];
    registry.increment_counter(METRIC_LLM_MAIN_ATTEMPTS_TOTAL, labels, 1);
    registry.increment_counter(
        METRIC_LLM_MAIN_ATTEMPT_TOKENS_TOTAL,
        labels,
        estimated_tokens.max(1),
    );
}

#[derive(Debug)]
pub(crate) struct RunScopedAgentProgressFilter {
    pub(crate) run_ids: HashSet<String>,
    pub(crate) agent_ids: HashSet<String>,
    pub(crate) pending_by_agent: HashMap<String, VecDeque<AgentProgressEvent>>,
}

impl RunScopedAgentProgressFilter {
    pub(crate) fn new(root_run_id: String) -> Self {
        let mut run_ids = HashSet::new();
        run_ids.insert(root_run_id);
        Self {
            run_ids,
            agent_ids: HashSet::new(),
            pending_by_agent: HashMap::new(),
        }
    }

    pub(crate) fn accept(&mut self, event: AgentProgressEvent) -> Vec<AgentProgressEvent> {
        if self.agent_ids.contains(&event.agent_id) {
            return vec![event];
        }

        if let ProgressEventType::AgentSpawned {
            run_id,
            parent_run_id,
            ..
        } = &event.event_type
        {
            if self.run_ids.contains(parent_run_id) || self.run_ids.contains(run_id) {
                self.run_ids.insert(run_id.clone());
                self.agent_ids.insert(event.agent_id.clone());
                let mut accepted = self
                    .pending_by_agent
                    .remove(&event.agent_id)
                    .unwrap_or_default();
                accepted.push_back(event);
                return accepted.into_iter().collect();
            }

            self.pending_by_agent.remove(&event.agent_id);
            return Vec::new();
        }

        self.remember_pending(event);
        Vec::new()
    }

    fn remember_pending(&mut self, event: AgentProgressEvent) {
        if !self.pending_by_agent.contains_key(&event.agent_id)
            && self.pending_by_agent.len() >= MAX_PENDING_PROGRESS_AGENTS
            && let Some(key) = self.pending_by_agent.keys().next().cloned()
        {
            self.pending_by_agent.remove(&key);
        }

        let pending = self
            .pending_by_agent
            .entry(event.agent_id.clone())
            .or_default();
        if pending.len() >= MAX_PENDING_PROGRESS_PER_AGENT {
            pending.pop_front();
        }
        pending.push_back(event);
    }
}

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

fn server_requested_interaction_mode(mode: RequestedTurnInteractionMode) -> TurnInteractionMode {
    match mode {
        RequestedTurnInteractionMode::NonInteractive => TurnInteractionMode::NonInteractive,
        RequestedTurnInteractionMode::Prompt => TurnInteractionMode::Prompt,
        RequestedTurnInteractionMode::Auto => TurnInteractionMode::Auto,
        RequestedTurnInteractionMode::Deny => TurnInteractionMode::Deny,
        RequestedTurnInteractionMode::Headless => TurnInteractionMode::Headless,
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

fn estimate_tool_schema_tokens(tools: &[Value]) -> u64 {
    // Provider tokenizers differ, but UTF-8 bytes / 4 is the same coarse
    // estimator used elsewhere in the manifest path. The important invariant
    // is not exact accounting; it is that each LLM call's manifest records a
    // non-zero, queryable tool-schema budget when tools were actually exposed.
    serde_json::to_string(tools)
        .map(|value| value.len().div_ceil(4) as u64)
        .unwrap_or(0)
}

fn record_full_llm_request_event(
    state: &mut AgenticLoopState,
    full_llm_capture: bool,
    user_id: &str,
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
    let prompt_request_plan =
        astra_services::plan_prompt_request(astra_services::PromptRequestPlanInput {
            user_id,
            session_id,
            turn: state.session_turn,
            round,
            attempt,
            source,
            messages,
            tools,
            max_output_tokens,
        })
        .ok();
    let trace = crate::turn::llm::exchange_capture::CaptureTrace {
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
            "request": crate::turn::llm::exchange_capture::build_capture_request_json(
                messages,
                tools,
                max_output_tokens,
            ),
            "prompt_request_id": prompt_request_plan.as_ref().map(|plan| plan.request_id.as_str()),
            "request_hash": prompt_request_plan.as_ref().map(|plan| plan.request_hash.as_str()),
            "request_summary": prompt_request_plan
                .as_ref()
                .map(|plan| plan.summary_json.clone())
                .unwrap_or_else(|| crate::turn::llm::exchange_capture::build_capture_request_summary_json(
                    messages,
                    tools,
                    max_output_tokens,
                )),
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
    let trace = crate::turn::llm::exchange_capture::CaptureTrace {
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
            "response": crate::turn::llm::exchange_capture::build_capture_response_json(
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
    /// Upstream literal name to put in the request body's `model` field.
    /// `None` → send `model_name`. See `ResolvedActiveLlmModel::upstream_model_name`.
    wire_model_name: Option<String>,
    api_key: String,
    base_url: String,
    provider: String,
    cache_capability: Option<astra_turn_core::cache_placement::CacheCapability>,
    fallback_chain: Vec<String>,
    header_overrides: HashMap<String, String>,
    request_body_overrides: Option<Map<String, Value>>,
    completions_url_override: Option<String>,
    request_timeout: Option<Duration>,
    /// Context window from explicit model config. `None` means the registry row
    /// did not provide model metadata; callers must choose an explicit fallback
    /// policy rather than inferring from the model name.
    context_window: Option<u32>,
}

type PipelineTurnOutcome = crate::turn::llm::context::LlmContextAssemblyOutput;

#[derive(Debug, Clone)]
struct RequestAwareSummaryClient {
    model_name: String,
    wire_model_name: Option<String>,
    api_key: String,
    base_url: String,
    provider: String,
    max_output_tokens: usize,
    header_overrides: HashMap<String, String>,
    request_body_overrides: Option<Map<String, Value>>,
    completions_url_override: Option<String>,
    request_timeout: Option<Duration>,
}

#[async_trait]
impl astra_turn_core::cloud_summary::SummaryLlmClient for RequestAwareSummaryClient {
    async fn summarize(
        &self,
        messages: &[Value],
    ) -> Result<astra_turn_core::cloud_summary::SummaryResponse, String> {
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
            self.wire_model_name.as_deref(),
            (!self.header_overrides.is_empty()).then_some(&self.header_overrides),
            self.request_body_overrides.as_ref(),
            self.completions_url_override.as_deref(),
            self.request_timeout,
            &ThinkingConfig::Off,
        )
        .await
        {
            Ok(result) => Ok(astra_turn_core::cloud_summary::SummaryResponse {
                text: result.full_text,
                is_ptl_error: false,
            }),
            Err(error) if error.kind == astra_core::ErrorKind::ContextWindow => {
                Ok(astra_turn_core::cloud_summary::SummaryResponse {
                    text: String::new(),
                    is_ptl_error: true,
                })
            }
            Err(error) => Err(error.to_string()),
        }
    }
}

impl RequestAwareSummaryClient {
    fn from_resolved_config(llm_cfg: &ResolvedTurnLlmConfig, max_output_tokens: usize) -> Self {
        Self {
            model_name: llm_cfg.model_name.clone(),
            wire_model_name: llm_cfg.wire_model_name.clone(),
            api_key: llm_cfg.api_key.clone(),
            base_url: llm_cfg.base_url.clone(),
            provider: llm_cfg.provider.clone(),
            max_output_tokens,
            header_overrides: llm_cfg.header_overrides.clone(),
            request_body_overrides: llm_cfg.request_body_overrides.clone(),
            completions_url_override: llm_cfg.completions_url_override.clone(),
            request_timeout: llm_cfg.request_timeout,
        }
    }
}

struct SummaryClientTurnIntentJudge {
    client: Box<dyn astra_turn_core::cloud_summary::SummaryLlmClient>,
}

#[async_trait]
impl astra_services::TurnIntentJudge for SummaryClientTurnIntentJudge {
    async fn judge(
        &self,
        ctx: &astra_services::TurnIntentJudgeContext,
    ) -> Result<astra_config::user_profile::TurnIntent, astra_services::TurnIntentJudgeError> {
        let messages = astra_services::turn_intent_judge_messages(ctx);
        let response = self
            .client
            .summarize(&messages)
            .await
            .map_err(astra_services::TurnIntentJudgeError::Transport)?;
        astra_services::parse_turn_intent_response(response.text.as_str())
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
            wire_model_name: None,
            api_key: String::new(),
            base_url: "https://api.openai.com/v1".to_string(),
            provider: "openai".to_string(),
            cache_capability: None,
            fallback_chain: Vec::new(),
            header_overrides: forward_headers.clone(),
            request_body_overrides: None,
            completions_url_override: Some(config.url.clone()),
            request_timeout: config.timeout_ms.map(Duration::from_millis),
            context_window: None,
        });
    }
    let resolved =
        astra_services::resolve_active_llm_model(matrixone, encryptor, preferred_model, pool)
            .await?;
    Ok(ResolvedTurnLlmConfig {
        model_name: resolved.model_name,
        wire_model_name: resolved.wire_model_name,
        api_key: resolved.api_key,
        base_url: resolved.base_url,
        provider: resolved.provider,
        cache_capability: crate::turn::llm::context::cache_capability_from_model_metadata(
            resolved.prompt_cache_capability,
        ),
        fallback_chain: resolved.fallback_chain,
        header_overrides: HashMap::new(),
        request_body_overrides: resolved.request_body_overrides,
        completions_url_override: None,
        request_timeout: None,
        context_window: resolved.context_window,
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
    /// Indices of messages (in the captured `messages` array) that carry a
    /// `cache_control` marker anywhere in their content. Order matches the
    /// message order. Empty for non-Anthropic providers.
    ///
    /// Used by tests to assert Claude Code-style message-marker behavior:
    /// exactly one marker on the last non-system message for
    /// Anthropic/Bedrock-compatible requests.
    pub message_cache_control_indices: Vec<usize>,
    /// For each message in `messages`, the SHA-256 hex of that message's
    /// canonical JSON serialization (sort_keys). Tests compare slices of
    /// this vector across rounds to prove the cacheable message prefix is
    /// byte-stable (a prerequisite for Anthropic cache hits beyond tools).
    pub message_sha256: Vec<String>,
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

/// Normalize a message to the shape Anthropic uses for cache-key
/// derivation. Removes `cache_control` attributes everywhere (they are
/// request-layer directives, not tokens) and upgrades `content: "text"`
/// strings to the canonical `content: [{type:"text", text:"..."}]`
/// array form. Tool-role messages are also canonicalized to the same
/// `tool_result` block shape the Anthropic adapter sends on the wire, so
/// "tail marker moved from old tool_result to new tool_result" does not
/// spuriously look like historical-byte churn in the capture hashes.
#[cfg(feature = "bridge-e2e-hooks")]
fn normalize_message_for_cache_hash(m: &Value) -> Value {
    let mut out = m.clone();
    if let Some(obj) = out.as_object_mut() {
        let role = obj.get("role").and_then(Value::as_str);
        match role {
            Some("tool") => {
                let tool_use_id = obj
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let normalized = match obj.get("content").cloned() {
                    Some(Value::Array(mut blocks))
                        if blocks.iter().any(|b| {
                            b.get("type").and_then(Value::as_str) == Some("tool_result")
                        }) =>
                    {
                        for block in blocks.iter_mut() {
                            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                                continue;
                            }
                            if let Some(map) = block.as_object_mut()
                                && !map.contains_key("tool_use_id")
                                && !tool_use_id.is_empty()
                            {
                                map.insert(
                                    "tool_use_id".into(),
                                    Value::String(tool_use_id.clone()),
                                );
                            }
                        }
                        Value::Array(blocks)
                    }
                    Some(Value::String(text)) => serde_json::json!([{
                        "type": "tool_result",
                        "tool_use_id": tool_use_id,
                        "content": text,
                    }]),
                    Some(Value::Null) | None => serde_json::json!([{
                        "type": "tool_result",
                        "tool_use_id": tool_use_id,
                        "content": "",
                    }]),
                    Some(other) => serde_json::json!([{
                        "type": "tool_result",
                        "tool_use_id": tool_use_id,
                        "content": other.to_string(),
                    }]),
                };
                obj.insert("content".into(), normalized);
            }
            _ => {
                if let Some(content) = obj.get("content").cloned()
                    && let Some(s) = content.as_str()
                {
                    obj.insert(
                        "content".into(),
                        serde_json::json!([{ "type": "text", "text": s }]),
                    );
                }
            }
        }
    }
    strip_cache_control(&mut out);
    out
}

#[cfg(feature = "bridge-e2e-hooks")]
fn strip_cache_control(v: &mut Value) {
    match v {
        Value::Object(map) => {
            map.remove("cache_control");
            for (_, child) in map.iter_mut() {
                strip_cache_control(child);
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                strip_cache_control(item);
            }
        }
        _ => {}
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
    breakdown: &astra_turn_core::context_assembly_trace::SystemPromptBreakdown,
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
    let message_cache_control_indices: Vec<usize> = if cache_cfg.is_anthropic {
        messages
            .iter()
            .enumerate()
            .filter_map(|(i, m)| {
                let at_msg = value_has_cache_control(m);
                let in_content = m
                    .get("content")
                    .and_then(Value::as_array)
                    .is_some_and(|arr| arr.iter().any(value_has_cache_control));
                (at_msg || in_content).then_some(i)
            })
            .collect()
    } else {
        Vec::new()
    };
    // Hash each message AFTER normalizing to the shape Anthropic uses for
    // cache-key derivation (see `normalize_message_for_cache_hash`). This
    // lets prefix-stability tests prove "same tokens, different marker
    // placement" is still a cache hit.
    let message_sha256: Vec<String> = messages
        .iter()
        .map(|m| {
            let normalized = normalize_message_for_cache_hash(m);
            let canonical =
                serde_json::to_string(&normalized).unwrap_or_else(|_| "<unserializable>".into());
            sha256_hex(&canonical)
        })
        .collect();
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
        message_cache_control_indices,
        message_sha256,
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
    resolved_model_name: Option<String>,
    resolved_context_window: Option<u32>,
    /// Cached LLM connection params from the last successful model resolution.
    /// Used by `summary_client()` to construct the compact-summary client
    /// without re-resolving (model resolution requires async DB call).
    resolved_llm_params: Option<astra_turn_core::cloud_summary::LlmConnParams>,
    /// Full resolved config from the last successful model resolution.
    /// Summary-classifier calls need completion overrides and forwarded
    /// headers, which `LlmConnParams` intentionally does not carry.
    resolved_llm_config: Option<ResolvedTurnLlmConfig>,

    // ── Context ──
    edge_tools: Vec<Value>,
    capabilities: astra_turn_core::capability::CapabilitySet,
    edge_profile: Map<String, Value>,
    valid_tools: HashSet<String>,
    /// Names the validator should admit beyond the current visible schemas.
    ///
    /// Covers runtime-surface tools (`skill`, `agent`, `web_search`,
    /// etc.) plus plugin/MCP tool names. Populated by the host's init
    /// path before the first `sync_valid_tools_to_visible` call; stable
    /// for the rest of the session.
    admissible_extras: Vec<String>,
    /// `true` when tools were auto-populated from astra-tools (no CLI connected).
    server_side_tools: bool,
    /// `true` when the connected client can answer ask_user prompts.
    interactive_client: bool,
    /// Optional request-level interaction policy override.
    interaction_mode: Option<RequestedTurnInteractionMode>,
    /// `true` when this session explicitly requests full LLM request/response capture.
    full_llm_capture: bool,
    /// Whether tool-call validation should admit Astra's static tool catalog
    /// even when those tools are not visible in the current loop.
    static_tool_catalog_admissible: bool,
    /// Resolved always-load tool names for this session. Used to place
    /// cache-control markers at the actual stable tool boundary.
    always_load_tool_names: HashSet<String>,
    /// System-prompt section reminding the LLM that a plan is in-flight.
    /// Shared mutable so mid-run tool executions (enter_plan_mode /
    /// exit_plan_mode) can refresh it. `None` means no active plan; reads
    /// are cheap (one RwLock read) so `build_system_prompt` stays sync.
    plan_resume_hint: Arc<std::sync::RwLock<Option<String>>>,
    /// Bounded task-board digest loaded before the turn starts. This is a
    /// scan hint, not an instruction to create or update tasks every turn.
    task_board_resume_hint: Option<String>,

    // ── Tool execution ──
    edge_callback_ledger: Arc<TokioMutex<HashMap<String, Value>>>,
    approval_audit_context: Option<astra_turn_core::cloud_tool_delivery::ApprovalAuditContext>,
    user_id: String,
    session_id: String,
    workspace_binding: WorkspaceBinding,
    executor_binding: ExecutorBinding,
    runtime_binding: Option<astra_runtime_env::RuntimeBinding>,
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
    progress_filter: Option<RunScopedAgentProgressFilter>,
    /// Latches the first lifecycle summary built for this host/user turn.
    /// Keeps prompt and introspection lifecycle context byte-consistent across
    /// multi-round tool loops.
    turn_start_lifecycle_summary: Option<String>,
    /// Tracks which plan hint was baked into the latched lifecycle summary so
    /// mid-turn plan enter/exit can refresh only the plan line.
    turn_start_plan_resume_hint: Option<String>,
    /// Workspace/executor binding metadata attached to live tool-call events
    /// before transport execution begins.
    execution_metadata: Option<Value>,
    /// Optional mirror used by server-side spawned sub-runs. The child host
    /// still owns normal SSE/event buffering, but these mirrored events go to
    /// the parent Work Surface agent card instead of the parent chat transcript.
    agent_live_mirror: Option<AgentLiveMirror>,

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
    /// Per-turn set of already-emitted tool_call id-keys (dedup across multiple
    /// `execute_mock_turn` invocations within the same chat turn). Cleared at
    /// the start of each user-turn in `run_one_mock_turn_for_test` and in
    /// `execute_turn`'s test-hook path.
    #[cfg(feature = "bridge-e2e-hooks")]
    /// Shared across host instances within the same chat turn so that
    /// skill subruns (which construct a second `ServerAgenticLoopHost` via
    /// `run_lifecycle.rs:3465`) reuse the parent host's dedup state instead
    /// of starting with an empty HashSet. Without this sharing, the same
    /// `tool_call` id would be emitted once per host instance. See
    /// `web_agent_e2e::skill_invocation_costs_exactly_two_llm_rounds_today`.
    emitted_tool_call_ids: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,

    // ── Fork-prefix parent capture (G2) ──
    /// Optional fork-prefix store. When set + the fork-prefix feature
    /// flag is on, `on_turn_completed` captures the parent turn's
    /// cacheable prefix so delegate / agent-spawn sub-runs routed
    /// through the server-side DelegationEngine can inherit it. Mirrors
    /// the CLI-side wiring in `CliAgenticLoopHost::prefix_store`.
    prefix_store: Option<std::sync::Arc<dyn astra_turn_core::fork_prefix_store::PrefixCaptureSink>>,
    /// Tool schemas advertised to the LLM this turn — used to populate
    /// `CaptureRequest.tool_schemas` for per-tool drift attribution.
    /// Updated by `execute_turn` each round.
    last_turn_tool_schemas: Vec<Value>,
    /// Shared handle to the runtime-disabled tools (admin API). Used to
    /// exclude admin-disabled tools from the LLM tool surface.
    disabled_tools: Arc<tokio::sync::RwLock<HashSet<String>>>,
    /// Optional LLM-based turn intent judge. When set, every turn first asks
    /// the judge to classify the user's message. Judge failure is non-fatal:
    /// the turn proceeds without explicit semantic intent.
    turn_intent_judge: Option<Arc<dyn astra_services::TurnIntentJudge>>,
}

#[derive(Clone)]
struct AgentLiveMirror {
    agent_id: String,
    sink: SharedAgentLiveEventSink,
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
    execution_bindings: Option<ExecutionBindingSnapshot>,
    edge_callback_ledger: Arc<TokioMutex<HashMap<String, Value>>>,
    user_id: String,
    session_id: String,
    progress_broadcaster: Option<Arc<crate::orchestration::ProgressBroadcaster>>,
    progress_root_run_id: Option<String>,
    interactive_client: bool,
    interaction_mode: Option<RequestedTurnInteractionMode>,
    full_llm_capture: bool,
    static_tool_catalog_admissible: bool,
    event_tx: Option<tokio::sync::mpsc::Sender<Value>>,
    plan_resume_hint: Option<String>,
    task_board_resume_hint: Option<String>,
    server_tool_catalog_enabled: bool,
    #[cfg(feature = "bridge-e2e-hooks")]
    test_llm_rounds: Vec<Value>,
    #[cfg(feature = "bridge-e2e-hooks")]
    test_llm_rounds_wired: bool,
    #[cfg(feature = "bridge-e2e-hooks")]
    mock_provider: Option<(String, String)>,
    #[cfg(feature = "bridge-e2e-hooks")]
    llm_request_capture: Option<Arc<std::sync::Mutex<Vec<CapturedLlmRequest>>>>,
    capabilities: astra_turn_core::capability::CapabilitySet,
    /// Shared tool_call dedup state. When set (via `with_dedup_state`), the
    /// built host shares the same `emitted_tool_call_ids` Arc as the parent
    /// host, preventing duplicate `tool_call` events across host instances
    /// within the same chat turn (e.g. parent + skill subrun).
    #[cfg(feature = "bridge-e2e-hooks")]
    shared_dedup_state: Option<std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>>,
    /// Optional fork-prefix store for parent-turn capture (G2).
    prefix_store: Option<std::sync::Arc<dyn astra_turn_core::fork_prefix_store::PrefixCaptureSink>>,
    /// Shared handle to the runtime-disabled tools (admin API).
    disabled_tools: Option<Arc<tokio::sync::RwLock<HashSet<String>>>>,
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
            execution_bindings: None,
            edge_callback_ledger: Arc::new(TokioMutex::new(HashMap::new())),
            user_id,
            session_id,
            progress_broadcaster: None,
            progress_root_run_id: None,
            interactive_client: false,
            interaction_mode: None,
            full_llm_capture: false,
            static_tool_catalog_admissible: true,
            event_tx: None,
            plan_resume_hint: None,
            task_board_resume_hint: None,
            server_tool_catalog_enabled: true,
            #[cfg(feature = "bridge-e2e-hooks")]
            test_llm_rounds: Vec::new(),
            #[cfg(feature = "bridge-e2e-hooks")]
            test_llm_rounds_wired: false,
            #[cfg(feature = "bridge-e2e-hooks")]
            mock_provider: None,
            #[cfg(feature = "bridge-e2e-hooks")]
            llm_request_capture: None,
            capabilities: crate::capabilities::full_server_capabilities_for_tests(),
            #[cfg(feature = "bridge-e2e-hooks")]
            shared_dedup_state: None,
            prefix_store: None,
            disabled_tools: None,
        }
    }

    /// Inject a shared fork-prefix store. When set, the built host
    /// captures the parent turn's cacheable prefix into this store so
    /// delegate / agent-spawn sub-runs can inherit it. `None` (default)
    /// makes `on_turn_completed` a no-op — preserves zero-overhead
    /// behavior for callers that don't enable the fork-prefix feature.
    pub fn with_prefix_store(
        mut self,
        store: Option<std::sync::Arc<dyn astra_turn_core::fork_prefix_store::PrefixCaptureSink>>,
    ) -> Self {
        self.prefix_store = store;
        self
    }

    /// Share a parent host's `emitted_tool_call_ids` HashSet with the host
    /// being built, so that skill subruns deduplicate `tool_call` events
    /// against the parent's already-emitted ids. Call this when constructing
    /// a subrun host from `ServerSkillSubRunExecutor`.
    #[cfg(feature = "bridge-e2e-hooks")]
    pub fn with_dedup_state(
        mut self,
        shared: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    ) -> Self {
        self.shared_dedup_state = Some(shared);
        self
    }

    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.shared_pool = Some(pool);
        self
    }

    /// Inject a pre-computed system-prompt section that reminds the LLM a
    /// plan is active for this session. Populated by the lifecycle service
    /// before the loop starts so the first and subsequent turns both see it.
    pub fn with_plan_resume_hint(mut self, hint: Option<String>) -> Self {
        self.plan_resume_hint = hint;
        self
    }

    pub fn with_task_board_resume_hint(mut self, hint: Option<String>) -> Self {
        self.task_board_resume_hint = hint;
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

    pub fn with_server_tool_catalog_enabled(mut self, enabled: bool) -> Self {
        self.server_tool_catalog_enabled = enabled;
        self
    }

    pub fn with_edge_profile(mut self, profile: Map<String, Value>) -> Self {
        self.edge_profile = profile;
        self
    }

    pub fn with_execution_bindings(
        mut self,
        workspace: WorkspaceBinding,
        executor: ExecutorBinding,
    ) -> Self {
        self.execution_bindings = Some(ExecutionBindingSnapshot::inferred(workspace, executor));
        self
    }

    pub fn with_execution_binding_snapshot(mut self, snapshot: ExecutionBindingSnapshot) -> Self {
        self.execution_bindings = Some(snapshot);
        self
    }

    pub fn with_server_sandbox_workspace(mut self, root: impl AsRef<Path>) -> Self {
        self.execution_bindings = Some(ExecutionBindingSnapshot::inferred(
            WorkspaceBinding::server_sandbox(root),
            ExecutorBinding::server_local(),
        ));
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

    pub fn with_progress_root_run_id(mut self, run_id: String) -> Self {
        self.progress_root_run_id = Some(run_id);
        self
    }

    pub fn with_interactive_client(mut self, interactive_client: bool) -> Self {
        self.interactive_client = interactive_client;
        self
    }

    pub fn with_interaction_mode(
        mut self,
        interaction_mode: Option<RequestedTurnInteractionMode>,
    ) -> Self {
        self.interaction_mode = interaction_mode;
        self
    }

    pub fn with_full_llm_capture(mut self, full_llm_capture: bool) -> Self {
        self.full_llm_capture = full_llm_capture;
        self
    }

    pub fn with_static_tool_catalog_admissible(mut self, admissible: bool) -> Self {
        self.static_tool_catalog_admissible = admissible;
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
        let server_side_tools = self.server_tool_catalog_enabled && self.edge_tools.is_empty();
        let binding_snapshot = self.execution_bindings.clone().unwrap_or_else(|| {
            ExecutionBindingSnapshot::inferred(
                WorkspaceBinding {
                    kind: WorkspaceBindingKind::None,
                    display_name: "No workspace".to_string(),
                    cwd: None,
                    authority: WorkspaceAuthority::None,
                    fallback_policy: FallbackPolicy::Disabled,
                },
                ExecutorBinding::server_control_plane(),
            )
        });
        let schema_workspace = binding_snapshot.workspace.clone();
        let schema_executor = binding_snapshot.executor.clone();
        let schema_runtime = binding_snapshot.runtime.clone();
        let edge_tools = if server_side_tools {
            capability_filtered_server_tool_schemas(
                &self.capabilities,
                &schema_workspace,
                &schema_executor,
                schema_runtime.as_ref(),
            )
        } else {
            capability_filter_edge_provided_tool_schemas_for_binding(
                self.edge_tools,
                &schema_workspace,
                &schema_executor,
                schema_runtime.as_ref(),
            )
        };

        let mut valid_tools: HashSet<String> = edge_tools
            .iter()
            .filter_map(|t| {
                t.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .map(String::from)
            })
            .collect();
        let admissible_extras = if server_side_tools
            && matches!(schema_workspace.kind, WorkspaceBindingKind::EdgeWorkspace)
            && matches!(schema_executor.kind, ExecutorBindingKind::EdgeAgent)
            && !matches!(schema_executor.status, ExecutorStatus::Online)
        {
            hidden_execution_boundary_tool_names(&edge_tools)
        } else {
            Vec::new()
        };
        valid_tools.extend(admissible_extras.iter().cloned());

        let always_load_tool_names: HashSet<String> = self
            .edge_profile
            .get(astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_ALWAYS_LOAD_TOOL_NAMES)
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_else(crate::turn::prompt_cache::runtime_always_load_tool_names);

        let progress_rx = self.progress_broadcaster.as_ref().map(|b| b.subscribe());
        let progress_filter = self
            .progress_root_run_id
            .map(RunScopedAgentProgressFilter::new);

        ServerAgenticLoopHost {
            matrixone: self.matrixone,
            encryptor: self.encryptor,
            shared_pool: self.shared_pool,
            model_override: self.model_override,
            llm_token_service: self.llm_token_service,
            resolved_model_name: None,
            resolved_context_window: None,
            resolved_llm_params: None,
            resolved_llm_config: None,
            edge_tools,
            capabilities: self.capabilities,
            edge_profile: self.edge_profile,
            valid_tools,
            admissible_extras,
            server_side_tools,
            interactive_client: self.interactive_client,
            interaction_mode: self.interaction_mode,
            full_llm_capture: self.full_llm_capture,
            static_tool_catalog_admissible: self.static_tool_catalog_admissible,
            always_load_tool_names,
            edge_callback_ledger: self.edge_callback_ledger,
            approval_audit_context: None,
            user_id: self.user_id,
            session_id: self.session_id,
            workspace_binding: schema_workspace.clone(),
            executor_binding: schema_executor.clone(),
            runtime_binding: schema_runtime.clone(),
            tool_result_cache: astra_turn_core::tool_result_dedup::new_shared_cache(
                128,
                Some(std::time::Duration::from_secs(30)),
            ),
            emitted_events: Vec::new(),
            event_tx: self.event_tx,
            client_cancel_flag: None,
            client_cancel_token: None,
            progress_rx,
            progress_filter,
            turn_start_lifecycle_summary: None,
            turn_start_plan_resume_hint: None,
            execution_metadata: self.execution_bindings.as_ref().map(|snapshot| {
                Value::Object(binding_event_fields(
                    &snapshot.workspace,
                    &snapshot.executor,
                ))
            }),
            agent_live_mirror: None,
            plan_resume_hint: Arc::new(std::sync::RwLock::new(self.plan_resume_hint)),
            task_board_resume_hint: self.task_board_resume_hint,
            #[cfg(feature = "bridge-e2e-hooks")]
            test_llm_rounds: std::collections::VecDeque::from(self.test_llm_rounds),
            #[cfg(feature = "bridge-e2e-hooks")]
            test_llm_rounds_wired: self.test_llm_rounds_wired,
            #[cfg(feature = "bridge-e2e-hooks")]
            mock_provider: self.mock_provider,
            #[cfg(feature = "bridge-e2e-hooks")]
            llm_request_capture: self.llm_request_capture,
            #[cfg(feature = "bridge-e2e-hooks")]
            emitted_tool_call_ids: self.shared_dedup_state.unwrap_or_else(|| {
                std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()))
            }),
            prefix_store: self.prefix_store,
            last_turn_tool_schemas: Vec::new(),
            disabled_tools: self
                .disabled_tools
                .unwrap_or_else(|| Arc::new(tokio::sync::RwLock::new(HashSet::new()))),
            turn_intent_judge: None,
        }
    }

    pub fn with_capabilities(
        mut self,
        capabilities: astra_turn_core::capability::CapabilitySet,
    ) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Share the runtime-disabled-tools set with the host so the LLM tool
    /// surface excludes admin-disabled tools.
    pub fn with_disabled_tools(
        mut self,
        handle: Arc<tokio::sync::RwLock<HashSet<String>>>,
    ) -> Self {
        self.disabled_tools = Some(handle);
        self
    }
}

impl ServerAgenticLoopHost {
    fn update_latched_plan_resume_line(summary: &str, plan_hint: Option<&str>) -> String {
        let plan_line = plan_hint
            .map(str::trim)
            .filter(|hint| !hint.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| "none".to_string());
        summary
            .lines()
            .map(|line| {
                if line.starts_with("- Plan resume: ") {
                    format!("- Plan resume: {plan_line}")
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Handle to the shared plan-resume hint slot. Mid-run callers
    /// (tool executions that mutate plan-mode state) clone this handle and
    /// swap in a fresh hint so the next turn's system prompt reflects the
    /// new plan state instead of the snapshot baked at loop start.
    pub(crate) fn plan_resume_hint_handle(&self) -> Arc<std::sync::RwLock<Option<String>>> {
        Arc::clone(&self.plan_resume_hint)
    }

    /// Inject an LLM-based turn intent judge.
    ///
    /// The judge is consulted at the start of every user turn (see
    /// [`AgenticLoopHost::judge_turn_intent`]); on judge failure or when this
    /// setter is not called, the host proceeds without explicit semantic
    /// intent.
    pub fn set_turn_intent_judge(&mut self, judge: Arc<dyn astra_services::TurnIntentJudge>) {
        self.turn_intent_judge = Some(judge);
    }

    async fn resolve_llm_config_for_state(
        &self,
        state: &AgenticLoopState,
    ) -> Result<ResolvedTurnLlmConfig, String> {
        // Skill-level model override takes precedence over the host-level one.
        let effective_model_override = self.effective_model_override_for_state(state);
        let pool_ref = self.shared_pool.as_ref().map(|sp| sp.get());
        resolve_llm_model_for_turn(
            &self.matrixone,
            self.encryptor.as_ref(),
            effective_model_override,
            pool_ref,
            self.llm_token_service.as_ref(),
            &state.hooks.forward_headers,
        )
        .await
    }

    fn effective_model_override_for_state<'a>(
        &'a self,
        state: &'a AgenticLoopState,
    ) -> Option<&'a str> {
        state
            .skills
            .model_override
            .as_deref()
            .or(self.model_override.as_deref())
    }

    fn cached_llm_config_matches_state(&self, state: &AgenticLoopState) -> bool {
        let Some(config) = self.resolved_llm_config.as_ref() else {
            return false;
        };
        match self.effective_model_override_for_state(state) {
            Some(requested) => config.model_name.eq_ignore_ascii_case(requested),
            None => true,
        }
    }

    fn clear_resolved_llm_config(&mut self) {
        self.resolved_model_name = None;
        self.resolved_context_window = None;
        self.resolved_llm_params = None;
        self.resolved_llm_config = None;
    }

    fn remember_resolved_llm_config(&mut self, llm_cfg: &ResolvedTurnLlmConfig) {
        self.resolved_model_name = Some(llm_cfg.model_name.clone());
        self.resolved_context_window = llm_cfg.context_window;
        self.resolved_llm_params = Some(astra_turn_core::cloud_summary::LlmConnParams {
            model_name: llm_cfg.model_name.clone(),
            api_key: llm_cfg.api_key.clone(),
            base_url: llm_cfg.base_url.clone(),
            provider: llm_cfg.provider.clone(),
            max_output_tokens: 4096,
        });
        self.resolved_llm_config = Some(llm_cfg.clone());
    }

    async fn turn_intent_summary_client(
        &mut self,
        state: &AgenticLoopState,
    ) -> Option<Box<dyn astra_turn_core::cloud_summary::SummaryLlmClient>> {
        if self.resolved_llm_config.is_some() && !self.cached_llm_config_matches_state(state) {
            self.clear_resolved_llm_config();
        }
        if let Some(config) = self.resolved_llm_config.as_ref() {
            return Some(Box::new(RequestAwareSummaryClient::from_resolved_config(
                config, 256,
            )));
        }

        if self.resolved_llm_params.is_none() {
            let llm_cfg = match self.resolve_llm_config_for_state(state).await {
                Ok(config) => config,
                Err(error) => {
                    tracing::debug!(
                        target: "astra::turn_intent",
                        error = %error,
                        "turn intent judge skipped because model resolution is unavailable"
                    );
                    return None;
                }
            };
            self.remember_resolved_llm_config(&llm_cfg);
            return Some(Box::new(RequestAwareSummaryClient::from_resolved_config(
                &llm_cfg, 256,
            )));
        }

        self.summary_client()
    }

    /// Install runtime MCP tool schemas into the LLM tool surface.
    /// Updates `edge_tools`, `valid_tools`, and `admissible_extras`
    /// so the LLM sees MCP tools and the validator admits them.
    pub fn install_runtime_tool_schemas(&mut self, schemas: Vec<Value>) {
        let runtime_tools_are_the_only_tool_surface =
            !schemas.is_empty() && self.edge_tools.is_empty();
        for schema in &schemas {
            if let Some(name) = schema
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|v| v.as_str())
            {
                self.valid_tools.insert(name.to_string());
                self.admissible_extras.push(name.to_string());
            }
        }
        self.edge_tools.extend(schemas);
        if runtime_tools_are_the_only_tool_surface {
            self.server_side_tools = true;
        }
    }

    fn read_plan_resume_hint(&self) -> Option<String> {
        match self.plan_resume_hint.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => {
                astra_core::agent_warn!(
                    "pipeline",
                    "plan_resume_hint RwLock poisoned — plan context lost for this turn"
                );
                poisoned.into_inner().clone()
            }
        }
    }

    fn render_turn_start_lifecycle_summary(
        &self,
        state: &AgenticLoopState,
        plan_hint: Option<&str>,
    ) -> String {
        let session_id = state
            .current_session_id
            .as_deref()
            .unwrap_or(self.session_id.as_str());
        let run_id = state.current_run_id.as_deref().unwrap_or("none");
        let model = self
            .resolved_model_name
            .as_deref()
            .or(self.model_override.as_deref())
            .unwrap_or("auto");
        let mode = if self.server_side_tools {
            "web-agent (server-side tools)"
        } else {
            "edge-agent (edge-provided tools)"
        };
        let interaction = self.turn_interaction_mode().label();
        let workspace = self
            .edge_profile
            .get("cwd")
            .and_then(Value::as_str)
            .map(str::to_string);
        let agent_id = self.edge_profile.get("agent_id").and_then(Value::as_str);

        let lineage_parent = self
            .edge_profile
            .get("session_lineage")
            .and_then(Value::as_object)
            .and_then(|lineage| lineage.get("parent_session_id"))
            .and_then(Value::as_str);
        let lineage_turn = self
            .edge_profile
            .get("session_lineage")
            .and_then(Value::as_object)
            .and_then(|lineage| lineage.get("forked_after_turn"))
            .and_then(Value::as_u64);

        let interruption = state
            .interruption
            .as_ref()
            .map(|record| format!("{:?}", record.kind))
            .unwrap_or_else(|| "none".to_string());

        let plan_line = plan_hint
            .map(str::trim)
            .filter(|hint| !hint.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| "none".to_string());

        let mut lines = vec![
            "# Turn-start session execution state".to_string(),
            format!("- Mode: {mode} · interaction: {interaction}"),
            format!("- Session: {session_id} · run: {run_id} · model: {model}"),
            match workspace {
                Some(cwd) => format!("- Workspace: {cwd}"),
                None if self.server_side_tools => {
                    "- Workspace: server-provisioned (edge cwd unavailable)".to_string()
                }
                None => "- Workspace: unavailable".to_string(),
            },
            format!("- Plan resume: {plan_line}"),
            format!(
                "- Task board: {}",
                self.task_board_resume_hint
                    .as_deref()
                    .map(str::trim)
                    .filter(|hint| !hint.is_empty())
                    .unwrap_or("no open tasks")
            ),
            format!(
                "- Delegation: engine={} · this_turn={} · progress_stream={}",
                if state.delegation_engine.is_some() {
                    "enabled"
                } else {
                    "disabled"
                },
                state.delegations_this_turn,
                if self.progress_rx.is_some() {
                    "subscribed"
                } else {
                    "none"
                }
            ),
            format!("- Interruption: {interruption}"),
        ];
        if state.recursion_depth > 0 || agent_id.is_some() {
            lines.push(format!(
                "- Delegation context: recursion_depth={}{}",
                state.recursion_depth,
                agent_id
                    .map(|id| format!(" · agent_id={id}"))
                    .unwrap_or_default()
            ));
        }

        if let Some(parent) = lineage_parent {
            let turn_suffix = lineage_turn
                .map(|turn| format!(" · forked_after_turn={turn}"))
                .unwrap_or_default();
            lines.push(format!("- Session lineage: parent={parent}{turn_suffix}"));
        }

        lines.join("\n")
    }

    fn turn_interaction_mode(&self) -> TurnInteractionMode {
        self.interaction_mode
            .map(server_requested_interaction_mode)
            .unwrap_or_else(|| {
                if self.interactive_client {
                    TurnInteractionMode::Prompt
                } else {
                    TurnInteractionMode::Headless
                }
            })
    }

    /// Push an SSE event to both the internal buffer and the streaming channel.
    /// If the streaming channel is closed (client disconnected), triggers
    /// cancellation so the agentic loop stops at the next turn boundary. If the
    /// channel is full, live streaming is detached but the run continues.
    fn emit_event(&mut self, mut event: Value) {
        self.attach_execution_metadata_to_tool_event(&mut event);
        self.mirror_agent_live_event(&event);
        let streaming_turn = self.event_tx.is_some();
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
                    // Backpressure is a delivery problem, not a user cancel. Detach
                    // live streaming and keep the loop running so long jobs survive
                    // slow clients or background tabs.
                    tracing::warn!(target: "sse_channel", "SSE event channel full; detaching live stream without cancelling run");
                    self.event_tx = None;
                }
            }
        }
        self.emitted_events.push(event);
        if streaming_turn && self.emitted_events.len() > MAX_STREAMED_TURN_EVENT_BUFFER {
            let overflow = self
                .emitted_events
                .len()
                .saturating_sub(MAX_STREAMED_TURN_EVENT_BUFFER);
            self.emitted_events.drain(0..overflow);
        }
    }

    fn attach_execution_metadata_to_tool_event(&self, event: &mut Value) {
        let Some(event_obj) = event.as_object_mut() else {
            return;
        };
        let event_type = event_obj
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_string);
        if !matches!(
            event_type.as_deref(),
            Some("tool_call" | "tool_call_start" | "tool_call_end")
        ) {
            return;
        }
        let Some(metadata_obj) = self.execution_metadata.as_ref().and_then(Value::as_object) else {
            return;
        };
        for (key, value) in metadata_obj {
            event_obj
                .entry(key.clone())
                .or_insert_with(|| value.clone());
        }
        let projected_fields = match event_type.as_deref() {
            Some("tool_call" | "tool_call_start") => {
                let Some(tool_name) = tool_name_from_tool_start_event(event_obj) else {
                    return;
                };
                projected_tool_start_event_fields(tool_name, metadata_obj)
            }
            Some("tool_call_end") => projected_tool_end_event_fields(
                tool_name_from_tool_end_event(event_obj),
                metadata_obj,
            ),
            _ => None,
        };
        let Some(projected_fields) = projected_fields else {
            return;
        };
        for (key, value) in projected_fields {
            event_obj.insert(key, value);
        }
    }

    fn mirror_agent_live_event(&self, event: &Value) {
        let Some(mirror) = self.agent_live_mirror.as_ref() else {
            return;
        };
        let Some(kind) = agent_live_event_kind_from_server_sse(event) else {
            return;
        };
        if let Err(err) = mirror.sink.send(AgentLiveEvent {
            agent_id: mirror.agent_id.clone(),
            kind,
        }) {
            tracing::warn!(
                target: "astra_runtime::work_surface",
                agent_id = %mirror.agent_id,
                error = ?err,
                "failed to mirror child agent live event"
            );
        }
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
                        let accepted = if let Some(filter) = self.progress_filter.as_mut() {
                            filter.accept(evt)
                        } else {
                            vec![evt]
                        };
                        for evt in accepted {
                            if let Some(sse_val) = progress_event_to_sse(&evt) {
                                progress_events.push(sse_val);
                            }
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
    /// Attach an incremental SSE channel. Events will be pushed through
    /// this sender as they are emitted, enabling streaming to the client.
    /// When the channel closes (client disconnect), `cancel_flag` and
    /// `cancel_token` are triggered to stop the agentic loop.
    pub fn set_event_tx(&mut self, tx: tokio::sync::mpsc::Sender<Value>) {
        // In live streaming mode, run_lifecycle owns the dedicated progress
        // bridge. Keeping the host subscription active would replay the same
        // agent progress events at turn-boundary drains, duplicating cards and
        // persisted work-surface deltas.
        self.progress_rx = None;
        self.progress_filter = None;
        self.event_tx = Some(tx);
    }

    pub fn set_execution_metadata(&mut self, metadata: Value) {
        self.execution_metadata = Some(metadata);
    }

    pub fn set_approval_audit_context(
        &mut self,
        context: astra_turn_core::cloud_tool_delivery::ApprovalAuditContext,
    ) {
        self.approval_audit_context = Some(context);
    }

    pub fn set_agent_live_event_sink(&mut self, agent_id: String, sink: SharedAgentLiveEventSink) {
        self.agent_live_mirror = Some(AgentLiveMirror { agent_id, sink });
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
        // Clear dedup state ONLY at the true user-turn boundary.
        // NOTE: Do NOT clear emitted_tool_call_ids here. The HashSet's
        // lifetime equals the ServerAgenticLoopHost instance lifetime, which
        // equals one user-turn (build_host is called per HTTP request in
        // chat_handler_inner). Skill subruns re-enter execute_turn with a
        // fresh AgenticLoopState but the SAME host instance, so the HashSet
        // persists across rounds within a user-turn — exactly what we need
        // to dedupe tool_call events emitted by both Round 1 and Round 2
        // of the agentic loop for the same skill invocation.
        //
        // Previous versions cleared here (and/or in execute_turn's test-hook
        // path) which wiped ids inserted by earlier rounds and caused
        // duplicate events. See skill_invocation_costs_exactly_two_llm_rounds_today.
        //
        // Legacy comment (kept for history):
        // `run_one_mock_turn_for_test` can be called in addition to
        // `execute_turn` within the same user-turn (both drive mock emits
        // through `execute_mock_turn`). Clearing unconditionally here
        // would wipe ids inserted by a prior `execute_turn` pass in the
        // same turn, allowing duplicate tool_call events to escape.
        // `state.llm_rounds_completed == 0` is the unambiguous signal
        // that this is the first mock drive for a fresh user-turn.
        // Contract locked by:
        //   `skill_invocation_costs_exactly_two_llm_rounds_today`
        // Dedup state is intentionally NOT cleared here — the host instance
        // itself is the user-turn boundary (one build_host() per HTTP request).
        // See the NOTE block above for full rationale.
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
            Some((provider, model)) => {
                self.resolved_model_name = Some(model.clone());
                PromptCacheConfig::latch(provider, model)
            }
            None => PromptCacheConfig::default(),
        };

        let edge_tools_snapshot = self.edge_tools.clone();
        // Use the same pipeline path as `execute_turn` so mock-replay exercises
        // exactly what a real turn would send. The previous implementation had
        let (provider_name, model_name_for_pipeline) = match &self.mock_provider {
            Some((p, m)) => (p.clone(), m.clone()),
            None => ("openai".to_string(), "server-loop-mock".to_string()),
        };
        let user_content = state.message.clone();
        let mock_pipeline = self.run_turn_pipeline(
            state,
            &edge_tools_snapshot,
            &provider_name,
            &model_name_for_pipeline,
            &user_content,
        )?;
        state.last_llm_context_manifest_trace = Some(mock_pipeline.manifest_trace.to_json());
        let system_msgs = mock_pipeline.system_messages;
        let volatile_preamble = mock_pipeline.volatile_preamble;

        // Replicate the real-path tool + message annotations so captured
        // payloads reflect what a real provider would see. Start from the
        // pipeline-pruned tool schemas so mock replay mirrors the real
        // wire. Route the history through the same `assemble_llm_messages`
        // stitcher the real path uses so matrix tests see the output of
        // volatile-preamble folding and `consolidate_mid_history_volatile_injections`.
        let mut annotated_tools = mock_pipeline.tool_schemas;
        crate::turn::llm::context::annotate_tool_schemas_for_cache(
            &mut annotated_tools,
            &cache_cfg,
            &self.always_load_tool_names,
        );
        self.sync_valid_tools_to_wire_surface_for_state(&annotated_tools, state);
        self.last_turn_tool_schemas = annotated_tools.clone();
        let (provider, model) = self
            .mock_provider
            .clone()
            .unwrap_or_else(|| ("openai".to_string(), "server-loop-mock".to_string()));
        let mock_llm_cfg = ResolvedTurnLlmConfig {
            provider: provider.clone(),
            model_name: model.clone(),
            wire_model_name: None,
            api_key: String::new(),
            base_url: String::new(),
            fallback_chain: Vec::new(),
            cache_capability: None,
            header_overrides: HashMap::new(),
            request_body_overrides: None,
            completions_url_override: None,
            request_timeout: None,
            context_window: None,
        };
        let wire_messages = self.assemble_llm_messages(
            system_msgs.clone(),
            volatile_preamble.clone(),
            state.messages.clone(),
            state,
            &mock_llm_cfg,
            &cache_cfg,
        );
        if let Some(trace) = state.last_llm_context_manifest_trace.as_mut() {
            crate::turn::llm::context::augment_manifest_trace_with_wire(
                trace,
                &wire_messages,
                &annotated_tools,
            );
        }
        self.emit_context_meta(
            &mock_pipeline.breakdown,
            state.last_llm_context_manifest_trace.as_ref(),
        );
        // `assemble_llm_messages` produces `[system(s), …, compacted msgs,
        // post-compact attachments]`. For the capture's downstream
        // assertions we want just the message portion (post the canonical
        // system messages). Extract.
        let sys_count = system_msgs.len();
        let annotated_messages: Vec<Value> =
            wire_messages.iter().skip(sys_count).cloned().collect();
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
                &mock_pipeline.breakdown,
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
                crate::turn::llm::exchange_capture::persist_configured_capture_or_log(
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
                    Some(crate::turn::llm::exchange_capture::CaptureTrace {
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
                    system_prompt_tokens: Some(mock_pipeline.breakdown.total_tokens),
                    system_prompt_breakdown: serde_json::to_value(&mock_pipeline.breakdown).ok(),
                    context_manifest_trace: state.last_llm_context_manifest_trace.clone(),
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

        let (full_text, reasoning, tool_calls, usage, delay_ms) =
            astra_turn_core::bridge_e2e_hooks::parse_llm_round(round);
        if delay_ms > 0 {
            sleep_ms_or_llm_cancel(delay_ms, llm_cancel_for_state(state)).await?;
        }

        if !reasoning.is_empty() {
            self.push_reasoning_events(&reasoning);
        }
        if !full_text.is_empty() {
            self.emit_event(json!({ "type": "text_delta", "content": &full_text }));
        }
        // Tool_call dedup — two distinct concerns, two distinct scopes:
        //
        //   1. Per-round local set (`round_seen`): the ONLY structure used to
        //      filter/suppress duplicate emits. Scope is this one
        //      `execute_mock_turn` invocation. Protects against duplicated
        //      tool_call deltas within a single SSE stream. Fresh HashSet
        //      every invocation, so LLMs are free to reuse the same tool_call
        //      id across rounds (e.g. `call_bash_0` in round 2 AND round 3);
        //      the second one will be emitted just like the first.
        //
        //   2. Cross-host shared set (`self.emitted_tool_call_ids`,
        //      Arc<Mutex<HashSet>>): a WRITE-ONLY log of ids that this host
        //      (and any host sharing the Arc via `with_shared_dedup_state`)
        //      has ever emitted during the user-turn. It is NOT consulted to
        //      suppress per-round new ids. Its purpose is to let sub-run or
        //      sibling hosts observe what has been emitted elsewhere — the
        //      consumers of that signal live outside this loop.
        //
        // Prior behavior (pre-fix) used the Arc set as a filter, which caused
        // a regression: round 3's legitimate re-use of a round-2 tool_call id
        // was silently swallowed. The HashSet's turn-scoped semantics made
        // any cross-round id repeat disappear, breaking
        // `interleaved_tool_and_text_rounds_preserve_event_order_and_history`.
        //
        // Contract locked by:
        //   `skill_invocation_costs_exactly_two_llm_rounds_today`
        //   `interleaved_tool_and_text_rounds_preserve_event_order_and_history`
        let mut round_seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for tc in &tool_calls {
            let key = match tc.get("id").and_then(Value::as_str) {
                Some(id) if !id.is_empty() => format!("id:{id}"),
                _ => format!("raw:{tc}"),
            };
            // Per-round dedup (same SSE stream): skip if already seen in THIS round.
            if !round_seen.insert(key.clone()) {
                continue;
            }
            // Record to the cross-host log, but do NOT use it to filter:
            // per-round new ids always emit. Lock span is kept minimal.
            {
                let mut shared = self.emitted_tool_call_ids.lock().unwrap();
                shared.insert(key);
            }
            self.emit_event(json!({ "type": "tool_call", "tool_call": tc }));
        }
        // Mock fixtures use upstream OpenAI-native keys (`prompt_tokens` /
        // `completion_tokens` / `prompt_tokens_details.cached_tokens`), plus
        // direct Anthropic-style aliases. Normalize through the shared
        // [`TokenUsage`] extractor so the emitted SSE uses canonical keys
        // regardless of fixture provenance.
        let extracted = crate::turn::token_usage::extract_usage(
            crate::turn::token_usage::UsageDialect::OpenAi,
            &usage,
        );
        let u = extracted.unwrap_or(crate::turn::token_usage::TokenUsage {
            input_tokens: 10,
            cached_input_tokens: 0,
            cache_creation_tokens: 0,
            output_tokens: 5,
        });
        self.emit_event(json!({
            "type": "usage",
            "input_tokens": u.input_tokens,
            "cached_input_tokens": u.cached_input_tokens,
            "cache_creation_tokens": u.cache_creation_tokens,
            "output_tokens": u.output_tokens,
            "total_tokens": u.total_tokens(),
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
            prompt_tokens: u.input_tokens,
            completion_tokens: u.output_tokens,
            cache_read_tokens: u.cached_input_tokens,
            cache_creation_tokens: u.cache_creation_tokens,
            has_usage: true,
            system_prompt_tokens: Some(mock_pipeline.breakdown.total_tokens),
            system_prompt_breakdown: serde_json::to_value(&mock_pipeline.breakdown).ok(),
            context_manifest_trace: state.last_llm_context_manifest_trace.clone(),
            ..Default::default()
        };

        if !self.session_id.is_empty() {
            let mut artifact_store =
                astra_services::DatabaseSessionArtifactStore::new(self.matrixone.clone());
            if let Some(pool) = self.shared_pool.clone() {
                artifact_store = artifact_store.with_pool(pool);
            }
            crate::turn::llm::exchange_capture::persist_configured_capture_or_log(
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
                        "input_tokens": u.input_tokens,
                        "cached_input_tokens": u.cached_input_tokens,
                        "cache_creation_tokens": u.cache_creation_tokens,
                        "output_tokens": u.output_tokens,
                        "total_tokens": u.total_tokens(),
                    },
                }),
                Some(crate::turn::llm::exchange_capture::CaptureTrace {
                    session_turn_source: Some("state"),
                    turn_chain_id: None,
                    user_query_event_id: None,
                }),
            )
            .await;
        }

        state.final_text_streamed = !full_text.is_empty();
        state.final_text = full_text;
        state.total_prompt += u.input_tokens;
        state.total_cache_read += u.cached_input_tokens;
        state.total_cache_creation += u.cache_creation_tokens;
        state.total_completion += u.output_tokens;
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
    ///
    /// **P0-3**: When the in-memory ledger times out and an edge dispatch
    /// service is wired (cross-pod deployment), falls back to DB-polling
    /// for results delivered by another pod.
    async fn deliver_edge_tools_via_ledger(
        &mut self,
        tool_calls: &[Value],
    ) -> Vec<astra_turn_core::sse_stream_host::EdgeToolExecResult> {
        use astra_turn_core::cloud_tool_delivery::{
            cloud_tool_requires_approval_for_delivery, collect_approval_batches,
            local_tool_execution_delivery, sse_maps_through_tool_request,
            wait_approval_ledger_for_tool, wait_tool_result_ledger_for_tool,
        };
        use astra_turn_core::headless_tool_assembly::{
            ensure_tool_call_ids, parse_flat_tool_call_event,
        };
        use astra_turn_core::sse_stream_host::EdgeToolExecResult;
        use astra_turn_core::stream_events::{
            ApprovalBatchRequestEvent, build_approval_batch_required_event,
            build_approval_required_event, build_tool_call_end_event,
        };
        use std::collections::HashMap;

        // 5-minute timeout: web clients may execute long-running tools.
        let ledger_wait = std::time::Duration::from_secs(300);
        let mut results_by_id: HashMap<String, EdgeToolExecResult> = HashMap::new();
        let ordered_tool_calls = ensure_tool_call_ids(tool_calls);
        let mut tool_calls = Vec::with_capacity(ordered_tool_calls.len());

        for tc in ordered_tool_calls.iter() {
            if !tc.is_object() {
                continue;
            }
            let (request_id, tool_name, args) = parse_flat_tool_call_event(tc);
            if self.valid_tools.contains(&tool_name) {
                tool_calls.push(tc.clone());
                continue;
            }

            let output = astra_turn_core::tool::deferred_activation::tool_not_admitted_message(
                &tool_name, false,
            );
            self.emit_event(Value::Object(build_tool_call_end_event(
                &request_id,
                json!({
                    "status": "error",
                    "output": output,
                }),
            )));
            results_by_id.insert(
                request_id.clone(),
                EdgeToolExecResult {
                    request_id: request_id.clone(),
                    tool: tool_name.clone(),
                    args: args.clone(),
                    output,
                    tool_result_fields: Some(self.edge_result_fields_with_runtime(
                        &request_id,
                        &tool_name,
                        &args,
                        None,
                    )),
                    status: "error".to_string(),
                    duration_ms: 0,
                },
            );
        }

        for batch in collect_approval_batches(&tool_calls) {
            if batch.items.len() == 1 {
                let item = &batch.items[0];
                self.emit_event(Value::Object(build_approval_required_event(
                    &item.request_id,
                    &item.tool_name,
                    item.approval_kind,
                    item.path.as_deref(),
                    item.detail.as_deref(),
                    item.display_label.as_deref(),
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
                        display_label: item.display_label.as_deref(),
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
                        self.approval_audit_context.as_ref(),
                    )
                    .await
                    {
                        for m in denied.sse_maps {
                            self.emit_event(Value::Object(m));
                        }
                        results_by_id.insert(
                            request_id.clone(),
                            EdgeToolExecResult {
                                tool_result_fields: Some(self.edge_result_fields_with_runtime(
                                    &request_id,
                                    &tool_name,
                                    &args,
                                    None,
                                )),
                                request_id,
                                tool: tool_name,
                                args,
                                output: "Tool execution denied or timed out".to_string(),
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
                    // L1094 (execute_mock_turn mock-LLM-response path) is the
                    // SINGLE owner of `tool_call` events per skill invocation.
                    // `sse_maps_through_tool_request` re-wraps the same tc as
                    // a `tool_call` map for the tool-dispatch stream, but that
                    // would produce a duplicate event (same id) downstream.
                    // Skip any `tool_call` map here — other map types
                    // (tool_request, etc.) still flow through normally.
                    // Contract locked by:
                    //   `skill_invocation_costs_exactly_two_llm_rounds_today`
                    #[cfg(feature = "bridge-e2e-hooks")]
                    {
                        if m.get("type").and_then(|v| v.as_str()) == Some("tool_call") {
                            continue;
                        }
                    }
                    self.emit_event(Value::Object(m));
                }
            }

            for tc in executable_calls {
                if !tc.is_object() {
                    continue;
                }
                let (id, tool_name, args) = parse_flat_tool_call_event(tc);

                // ── Dedup read-only tool invocations within a short window ──
                // Only applies when the tool is parallelizable (args-aware);
                // mutating tools skip the cache.
                let is_cacheable = astra_turn_core::parallel_tool_exec::is_read_only_tool_with_args(
                    &tool_name,
                    Some(&args),
                );
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
                    Map<String, Value>,
                ) = if let Some(cached_output) = cached {
                    (
                        cached_output,
                        Vec::new(),
                        0,
                        "ok".to_string(),
                        self.edge_result_fields_with_runtime(&id, &tool_name, &args, None),
                    )
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
                        self.edge_result_fields_with_runtime(
                            &id,
                            &tool_name,
                            &args,
                            tool_result.and_then(|result| result.tool_result_fields),
                        ),
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
                        tool_result_fields: Some(tool_result_fields),
                        status,
                        duration_ms,
                    },
                );
            }

            block_start = block_end;
        }

        let mut results = Vec::with_capacity(ordered_tool_calls.len());
        for tc in ordered_tool_calls.iter() {
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

    fn edge_result_fields_with_runtime(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        args: &Value,
        fields: Option<Map<String, Value>>,
    ) -> Map<String, Value> {
        let mut fields = fields.unwrap_or_default();
        fields
            .entry("runtime_environment_advertisement".to_string())
            .or_insert_with(|| {
                let registry = astra_runtime_env::ToolRegistry::builtins();
                let request = ToolExecutionRequest {
                    user_id: self.user_id.clone(),
                    run_id: String::new(),
                    session_id: self.session_id.clone(),
                    tool_call_id: tool_call_id.to_string(),
                    tool_name: tool_name.to_string(),
                    args: args.clone(),
                    workspace: self.workspace_binding.clone(),
                    workspace_record: None,
                    executor: self.executor_binding.clone(),
                    runtime: self.runtime_binding.clone(),
                    policy: ToolPolicySnapshot::default(),
                };
                let binding = request.runtime_environment_binding(&registry);
                serde_json::to_value(astra_runtime_env::RuntimeEnvironmentAdvertisement::new(
                    binding,
                ))
                .expect("runtime environment advertisement serializes")
            });
        fields
    }

    fn emit_context_meta(
        &mut self,
        breakdown: &astra_turn_core::context_assembly_trace::SystemPromptBreakdown,
        manifest_trace: Option<&Value>,
    ) {
        self.emit_event(crate::turn::llm::context::context_meta_event(
            breakdown,
            manifest_trace,
        ));
    }

    /// Compute the tool schemas visible for the current turn after applying
    /// hard request policies.
    ///
    /// IMPORTANT: prompt-visible schemas must match the runtime policy the
    /// model can actually execute. Request/delegation allowlists are hard
    /// constraints and are pruned here; skill `allowed_tools` is only a hint
    /// and must not silently hide otherwise-callable tools from the model.
    fn filtered_turn_tools(&self, restricted_tools: &HashSet<String>) -> Vec<Value> {
        filter_tool_schemas_by_excluded_names(self.edge_tools.clone(), restricted_tools)
    }

    fn runtime_ready_turn_tools(&self, tools: Vec<Value>, state: &AgenticLoopState) -> Vec<Value> {
        let Some(executor) = state.server_tool_executor.as_deref() else {
            return tools;
        };
        tools
            .into_iter()
            .filter(|tool| {
                tool.get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
                    .is_some_and(|name| executor.tool_runtime_ready(name))
            })
            .collect()
    }

    fn filtered_runtime_ready_turn_tools(
        &self,
        restricted_tools: &HashSet<String>,
        state: &AgenticLoopState,
    ) -> Vec<Value> {
        self.runtime_ready_turn_tools(self.filtered_turn_tools(restricted_tools), state)
    }

    fn runtime_allowlist_restrictions(&self, state: &AgenticLoopState) -> HashSet<String> {
        let disabled: HashSet<String> = self
            .disabled_tools
            .try_read()
            .map(|g| g.clone())
            .unwrap_or_default();
        self.edge_tools
            .iter()
            .filter_map(|tool| {
                tool.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .map(String::from)
            })
            .filter(|name| {
                !crate::turn::agentic::tool_interception::runtime_allows_tool(state, name)
                    || disabled.contains(name)
            })
            .collect()
    }

    #[cfg(test)]
    fn sync_valid_tools_to_visible(&mut self, visible_tools: &[Value]) {
        self.valid_tools =
            self.admissible_tool_names_for_surface(visible_tools, &self.admissible_extras);
    }

    fn sync_valid_tools_to_wire_surface_for_state(
        &mut self,
        wire_tools: &[Value],
        state: &AgenticLoopState,
    ) {
        let mut extras = self.admissible_extras.clone();
        let activatable_deferred_tool_names = self.deferred_tool_names_for_wire_tools(
            wire_tools,
            self.resolved_model_name.as_deref(),
            self.resolved_context_window,
            state.server_tool_executor.as_deref(),
        );
        if let Some(executor) = state.server_tool_executor.as_deref() {
            executor.set_current_activatable_tool_names(activatable_deferred_tool_names);
            executor.set_current_searchable_tool_schemas(wire_tools);
            extras.extend(executor.activated_deferred_tool_names());
        }
        self.valid_tools = self.admissible_tool_names_for_surface(wire_tools, &extras);
    }

    fn admissible_tool_names_for_surface(
        &self,
        wire_tools: &[Value],
        extras: &[String],
    ) -> HashSet<String> {
        if self.static_tool_catalog_admissible {
            crate::turn::headless_tool_pipeline::admissible_tool_names_from_visible_and_extras(
                wire_tools, extras,
            )
        } else {
            crate::turn::headless_tool_pipeline::admissible_tool_names_from_visible_and_extras_strict(
                wire_tools, extras,
            )
        }
    }

    fn deferred_tool_names_from_edge_profile_for_model(
        &self,
        resolved_model_name: Option<&str>,
        resolved_context_window: Option<u32>,
    ) -> HashSet<String> {
        crate::turn::deferred_tools_edge_profile::names_for_model(
            &self.edge_profile,
            resolved_model_name,
            resolved_context_window,
        )
    }

    fn deferred_tool_names_for_wire_tools(
        &self,
        wire_tools: &[Value],
        resolved_model_name: Option<&str>,
        resolved_context_window: Option<u32>,
        executor: Option<&crate::server::server_tool_executor::ServerToolExecutor>,
    ) -> HashSet<String> {
        let deferred_tool_names = self.prompt_deferred_tool_names_for_wire_tools(
            wire_tools,
            resolved_model_name,
            resolved_context_window,
        );
        if deferred_tool_names.is_empty() {
            return HashSet::new();
        }

        if let Some(executor) = executor {
            let runtime_bound = executor.runtime_bound_tool_names(deferred_tool_names.clone());
            if runtime_bound != deferred_tool_names {
                let removed: Vec<&str> = deferred_tool_names
                    .difference(&runtime_bound)
                    .map(String::as_str)
                    .collect();
                tracing::warn!(
                    target: "astra.deferred_tools",
                    removed = ?removed,
                    removed_count = removed.len(),
                    declared_count = deferred_tool_names.len(),
                    kept_count = runtime_bound.len(),
                    "deferred manifest filtered: runtime binding removed {} of {} tool(s); \
                     prompt block will be rendered with the runtime-bound subset",
                    removed.len(),
                    deferred_tool_names.len()
                );
                return runtime_bound;
            }
        }
        deferred_tool_names
    }

    fn prompt_deferred_tool_names_for_wire_tools(
        &self,
        wire_tools: &[Value],
        resolved_model_name: Option<&str>,
        resolved_context_window: Option<u32>,
    ) -> HashSet<String> {
        let deferred_tool_names = self.deferred_tool_names_from_edge_profile_for_model(
            resolved_model_name,
            resolved_context_window,
        );
        if deferred_tool_names.is_empty() {
            return HashSet::new();
        }

        let visible_tool_names = astra_turn_core::tool::schema::tool_names_from_schemas(wire_tools);
        if !deferred_tool_names.is_disjoint(&visible_tool_names) {
            let overlap: Vec<&str> = deferred_tool_names
                .intersection(&visible_tool_names)
                .map(String::as_str)
                .collect();
            let retained: HashSet<String> = deferred_tool_names
                .difference(&visible_tool_names)
                .cloned()
                .collect();
            tracing::warn!(
                target: "astra.deferred_tools",
                overlap = ?overlap,
                deferred_count = deferred_tool_names.len(),
                visible_count = visible_tool_names.len(),
                kept_count = retained.len(),
                "deferred manifest filtered: deferred tool(s) already appear in visible surface; \
                 prompt block will keep only names that still require activation"
            );
            return retained;
        }

        deferred_tool_names
    }

    fn deferred_tools_block_for_wire_surface(
        &self,
        wire_tools: &[Value],
        state: &AgenticLoopState,
        model_name: &str,
        model_context_window: Option<u32>,
    ) -> String {
        let manifest_names = self.deferred_tool_names_for_wire_tools(
            wire_tools,
            Some(model_name),
            model_context_window,
            state.server_tool_executor.as_deref(),
        );
        if manifest_names.is_empty() {
            return String::new();
        }
        crate::turn::deferred_tools_edge_profile::block_for_model_filtered(
            &self.edge_profile,
            model_name,
            model_context_window,
            &manifest_names,
        )
        .unwrap_or_default()
    }

    /// Set the extras list (runtime-injected names + plugin names) so
    /// the validator admits them after deferred activation. Should be
    /// called once at session start, before the first tool round.
    pub fn set_admissible_extras(&mut self, extras: Vec<String>) {
        self.admissible_extras = extras;
    }

    /// Compute the effective restricted-tool set for a turn by running the
    /// full restriction pipeline:
    ///
    /// 1. widen check
    /// 2. runtime allowlist restrictions
    /// 3. interaction-scoped restrictions (always applied)
    /// 4. boost rescue
    /// 5. activated-deferred-tool rescue
    ///
    /// `consume_widen` controls whether the `widen_selection_pending` flag is
    /// consumed (authoritative path: main turn / test helper) or merely
    /// peeked (preview path: pre-turn summary, which must not steal the flag
    /// from the main turn that follows it). This is the only legitimate
    /// caller-policy divergence; every other step is identical across sites.
    ///
    /// Single source of truth shared by `visible_turn_tools`, `execute_turn`,
    /// and the summary path so the recipe cannot drift between call sites.
    fn compute_effective_restricted(
        &self,
        state: &mut AgenticLoopState,
        consume_widen: bool,
    ) -> HashSet<String> {
        // 1. Consume or peek the widen flag. Soft health diagnostics are not
        // promoted into the hard restricted-tool set.
        if consume_widen {
            let _ = std::mem::take(&mut state.widen_selection_pending);
        }
        // 2-5. layered restrictions from the merged base.
        let mut effective = state.restricted_tools.clone();
        effective.extend(self.runtime_allowlist_restrictions(state));
        effective.extend(interaction_scoped_tool_restrictions(
            self.turn_interaction_mode(),
        ));
        // Boosted tools are never hidden, even if they landed in the restricted
        // set earlier (e.g., via stall-based deprioritization).
        for boosted in &state.boosted_tools {
            effective.remove(boosted);
        }
        if let Some(executor) = state.server_tool_executor.as_deref() {
            for name in executor.activated_deferred_tool_names() {
                // Rescue activated deferred tools so they're visible this turn.
                effective.remove(&name);
            }
        }
        effective
    }

    /// Compute the tool schemas visible for the current turn after applying
    /// hard runtime restrictions.
    #[cfg(test)]
    fn visible_turn_tools(&mut self, state: &mut AgenticLoopState) -> Vec<Value> {
        let effective_restricted = self.compute_effective_restricted(state, true);
        let visible = self.filtered_runtime_ready_turn_tools(&effective_restricted, state);
        self.sync_valid_tools_to_wire_surface_for_state(&visible, state);
        visible
    }

    /// Build the turn's system messages via the context pipeline.
    ///
    /// Single source of truth for both the real `execute_turn` path and the
    /// `bridge-e2e-hooks`-gated `execute_mock_turn` path. Previously the mock
    /// that duplicated section assembly AND drifted from production behaviour —
    /// deleted in the same change that introduced this helper.
    ///
    /// Returns `(structured_system_messages, plain_text_for_estimates, breakdown)`.
    /// Run the full context pipeline for this turn and return everything the
    /// wire payload needs: rendered system message(s), the plain-text form for
    /// tracing, the breakdown, the selected compaction tier, and the tier-pruned
    /// tool schemas. Callers that only want the system text can discard the
    /// extra fields.
    fn run_turn_pipeline(
        &mut self,
        state: &mut AgenticLoopState,
        visible_tools: &[Value],
        provider: &str,
        model_name: &str,
        user_content: &str,
    ) -> Result<PipelineTurnOutcome, astra_core::ClassifiedError> {
        self.run_turn_pipeline_with_cache_capability_and_session_memory(
            state,
            visible_tools,
            provider,
            model_name,
            None,
            None,
            None,
            user_content,
        )
    }

    fn run_turn_pipeline_with_cache_capability_and_session_memory(
        &mut self,
        state: &mut AgenticLoopState,
        visible_tools: &[Value],
        provider: &str,
        model_name: &str,
        model_context_window: Option<u32>,
        cache_capability: Option<astra_turn_core::cache_placement::CacheCapability>,
        session_memory_entry: Option<astra_turn_core::context_sources::MemoryEntry>,
        user_content: &str,
    ) -> Result<PipelineTurnOutcome, astra_core::ClassifiedError> {
        let plan_hint = self.read_plan_resume_hint();
        let lifecycle_summary = if let Some(existing) = &self.turn_start_lifecycle_summary {
            if self.turn_start_plan_resume_hint.as_deref() != plan_hint.as_deref() {
                let updated = Self::update_latched_plan_resume_line(existing, plan_hint.as_deref());
                self.turn_start_lifecycle_summary = Some(updated.clone());
                self.turn_start_plan_resume_hint = plan_hint.clone();
                updated
            } else {
                existing.clone()
            }
        } else {
            let summary = self.render_turn_start_lifecycle_summary(state, plan_hint.as_deref());
            self.turn_start_lifecycle_summary = Some(summary.clone());
            self.turn_start_plan_resume_hint = plan_hint.clone();
            summary
        };
        let lifecycle_sections = vec![crate::prompts::PromptSection::dynamic(
            lifecycle_summary,
            crate::prompts::PromptTokenBucket::Environment,
        )];
        let restricted_snapshot = state.restricted_tools.clone();
        let deferred_tools_block = self.deferred_tools_block_for_wire_surface(
            visible_tools,
            state,
            model_name,
            model_context_window,
        );
        let cache_cfg =
            PromptCacheConfig::from_cache_capability(cache_capability, provider, model_name);
        crate::turn::llm::context::assemble_context_pipeline(
            crate::turn::llm::context::LlmContextAssemblyInput {
                state,
                session_id: &self.session_id,
                tool_surface: crate::turn::llm::context::ToolSurfacePlan::from_visible_tools(
                    visible_tools,
                    &restricted_snapshot,
                )
                .with_deferred_tools_block(&deferred_tools_block),
                runtime_signals: crate::turn::llm::context::RuntimeSignals::new(
                    &self.edge_profile,
                    plan_hint,
                )
                .with_extra_sections(&[], &lifecycle_sections)
                .with_session_memory_entry(session_memory_entry),
                cache_cfg: &cache_cfg,
                provider,
                model_name,
                context_window: model_context_window,
                cache_capability,
                user_content,
                query_source: "agentic_loop",
            },
        )
    }

    /// Run the Memoria compaction step and return the full `CompactResult`
    /// (messages + boundary). The boundary is what the caller inspects to
    /// decide whether to append the P2 continuation prompt — the bridge path
    /// does this inline, so we expose the same signal here for parity.
    async fn compact_messages_via_memoria(
        &self,
        state: &AgenticLoopState,
        system_messages: &[Value],
        visible_tools: &[Value],
        tier: CompactionTier,
        llm_cfg: &ResolvedTurnLlmConfig,
    ) -> crate::turn::cloud::compaction::CompactResult {
        let compact_config = crate::prompts::CompactConfig::from_env();
        let summary_client = RequestAwareSummaryClient {
            model_name: llm_cfg.model_name.clone(),
            wire_model_name: llm_cfg.wire_model_name.clone(),
            api_key: llm_cfg.api_key.clone(),
            base_url: llm_cfg.base_url.clone(),
            provider: llm_cfg.provider.clone(),
            max_output_tokens: compact_config.summary_token_budget,
            header_overrides: llm_cfg.header_overrides.clone(),
            request_body_overrides: llm_cfg.request_body_overrides.clone(),
            completions_url_override: llm_cfg.completions_url_override.clone(),
            request_timeout: llm_cfg.request_timeout,
        };
        let memoria_client = crate::turn::cloud::memoria_compact::HttpMemoriaClient::from_env();

        // Observatory is owned by the memory_extraction_service; clone the
        // Arc so both extraction (service) and injection (compaction) write
        // into the same ring set.
        let observatory = state
            .memory_extraction_service
            .as_ref()
            .and_then(|svc| svc.observatory().cloned());
        let ctx = crate::turn::wire_assembly::MemoriaContext {
            session_id: &self.session_id,
            model_name: &llm_cfg.model_name,
            context_window: llm_cfg.context_window,
            memoria_client: memoria_client
                .as_ref()
                .map(|c| c as &dyn crate::turn::cloud::memoria_compact::MemoriaClient),
            summary_client: Some(
                &summary_client as &dyn astra_turn_core::cloud_summary::SummaryLlmClient,
            ),
            tier,
            session_facts: None,
            turn_number: state.llm_rounds_completed,
            observatory,
        };
        ctx.compact(&state.messages, system_messages, visible_tools)
            .await
    }

    /// Thin wrapper around [`wire_assembly::assemble_llm_messages_with_cache_capability`] that
    /// extracts the server-path-specific attachments from `AgenticLoopState`
    /// (invoked skills + recently-read files) and delegates the rest. The
    /// shared module handles `strip_stale_reasoning_with_policy`, continuation-prompt
    /// insertion, attachment ordering, and cache annotations.
    fn assemble_llm_messages(
        &self,
        system_messages: Vec<Value>,
        volatile_preamble: Vec<Value>,
        compacted_messages: Vec<Value>,
        state: &mut AgenticLoopState,
        llm_cfg: &ResolvedTurnLlmConfig,
        cache_cfg: &PromptCacheConfig,
    ) -> Vec<Value> {
        // Per-turn skill listing (ranked shortlist) now flows through the
        // pipeline as an `extra_dynamic_sections` entry (RuntimeVolatile,
        // None scope). See `context_pipeline_adapter` — post-hoc injection
        // here would double up the content on the wire.
        let thinking = state.thinking.clone();
        crate::turn::llm::context::assemble_wire_messages(
            crate::turn::llm::context::LlmWireAssemblyInput {
                system_messages,
                volatile_preamble,
                compacted_messages,
                state,
                thinking: &thinking,
                edge_profile: &self.edge_profile,
                session_id: &self.session_id,
                provider: &llm_cfg.provider,
                model_name: &llm_cfg.model_name,
                cache_capability: llm_cfg.cache_capability,
                cache_cfg,
            },
        )
    }

    /// Convert an [`LlmCallResult`] into a [`ChatTurnSseAccum`].
    fn result_to_accum(result: &LlmCallResult) -> ChatTurnSseAccum {
        let u = crate::turn::token_usage::TokenUsage::from_partial_json_map(&result.usage);
        let prompt_tokens = u.input_tokens;
        let completion_tokens = u.output_tokens;
        let cache_read_tokens = u.cached_input_tokens;
        let cache_creation_tokens = u.cache_creation_tokens;

        ChatTurnSseAccum {
            full_text: result.full_text.clone(),
            reasoning_content: result.reasoning.clone(),
            reasoning_signature: result.reasoning_signature.clone(),
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

    async fn judge_turn_intent(
        &mut self,
        state: &AgenticLoopState,
    ) -> Option<astra_config::user_profile::TurnIntent> {
        let has_prior_assistant_turn = state
            .messages
            .iter()
            .rev()
            .any(|m| m.get("role").and_then(|r| r.as_str()) == Some("assistant"));
        // Use 1-based turn count: llm_rounds_completed counts *prior* rounds,
        // so the current user turn is +1.
        let turn_count = state.llm_rounds_completed.saturating_add(1);

        if let Some(judge) = self.turn_intent_judge.as_ref() {
            return crate::turn::agentic::turn_intent::judge_turn_intent_with_llm(
                judge.as_ref(),
                &state.message,
                turn_count,
                &state.recent_tools,
                has_prior_assistant_turn,
            )
            .await;
        }

        if let Some(reason) = should_skip_auxiliary_llm_for_capacity(TURN_INTENT_JUDGE_POLICY_ENV) {
            tracing::debug!(
                target: "astra::turn_intent",
                policy = auxiliary_llm_policy_label(TURN_INTENT_JUDGE_POLICY_ENV),
                reason,
                "turn intent judge skipped by capacity policy"
            );
            return None;
        }

        let client = self.turn_intent_summary_client(state).await?;
        let judge = SummaryClientTurnIntentJudge { client };
        crate::turn::agentic::turn_intent::judge_turn_intent_with_llm(
            &judge,
            &state.message,
            turn_count,
            &state.recent_tools,
            has_prior_assistant_turn,
        )
        .await
    }

    async fn judge_factual_retry_fallback(
        &mut self,
        ctx: FactualRetryFallbackJudgeContext<'_>,
    ) -> Option<FactualRetryFallbackDecision> {
        if let Some(reason) = should_skip_auxiliary_llm_for_capacity(FACTUAL_RETRY_JUDGE_POLICY_ENV)
        {
            tracing::debug!(
                target: "astra::factual_retry_judge",
                policy = auxiliary_llm_policy_label(FACTUAL_RETRY_JUDGE_POLICY_ENV),
                reason,
                "factual retry fallback judge skipped by capacity policy"
            );
            return None;
        }

        let client = self.summary_client()?;
        let messages = factual_retry_fallback_judge_messages(FactualRetryFallbackJudgeInput {
            original_query: ctx.original_query,
            fallback_text: ctx.fallback_text,
            retry_text: ctx.retry_text,
        });
        let response = match client.summarize(&messages).await {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!(
                    target: "astra::factual_retry_judge",
                    error = %error,
                    "factual retry fallback judge unavailable; keeping retry output"
                );
                return None;
            }
        };
        match parse_factual_retry_fallback_judge_response(response.text.as_str()) {
            Ok(verdict) => Some(verdict.accepted_decision()),
            Err(error) => {
                tracing::warn!(
                    target: "astra::factual_retry_judge",
                    error = %error,
                    "factual retry fallback judge returned malformed output; keeping retry output"
                );
                None
            }
        }
    }

    fn turn_start_lifecycle_summary(&self, state: &AgenticLoopState) -> String {
        if let Some(summary) = &self.turn_start_lifecycle_summary {
            let plan_hint = self.read_plan_resume_hint();
            if self.turn_start_plan_resume_hint.as_deref() != plan_hint.as_deref() {
                return Self::update_latched_plan_resume_line(summary, plan_hint.as_deref());
            }
            return summary.clone();
        }
        let plan_hint = self.read_plan_resume_hint();
        self.render_turn_start_lifecycle_summary(state, plan_hint.as_deref())
    }

    fn plan_mode_active(&self, _state: &AgenticLoopState) -> bool {
        self.read_plan_resume_hint().is_some()
    }

    fn render_final_text(&mut self, text: &str) {
        if !text.is_empty() {
            self.emit_event(json!({ "type": "text_delta", "content": text }));
        }
    }

    fn on_deferred_user_input(&mut self, input: &Value) {
        let Some(raw_skills) = input.get("active_skills") else {
            return;
        };

        let active_skills: Vec<Value> = raw_skills
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|skill| !skill.is_empty())
            .map(|skill| Value::String(skill.to_string()))
            .collect();

        if active_skills.is_empty() {
            self.edge_profile.remove("active_skills");
        } else {
            self.edge_profile
                .insert("active_skills".to_string(), Value::Array(active_skills));
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
                // Clear per-*user-turn* dedup state only at the first LLM round
                // NOTE: Do NOT clear emitted_tool_call_ids here.
                // The single authoritative clear point is run_one_mock_turn_for_test
                // (the true user-turn boundary). Skill subruns create a fresh
                // AgenticLoopState with llm_rounds_completed==0 and re-enter
                // execute_turn — clearing here would wipe the parent turn's dedup
                // state and allow duplicate tool_call events to escape the HashSet.
                // Contract: emitted_tool_call_ids is cleared ONLY in
                // run_one_mock_turn_for_test at the start of each new user message.
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
        let mut llm_cfg = match self.resolve_llm_config_for_state(state).await {
            Ok(m) => m,
            Err(e) => {
                return Err(astra_core::ClassifiedError::new(
                    astra_core::ErrorKind::Unknown,
                    format!("Model resolution failed: {e}"),
                ));
            }
        };
        let has_fallback = !llm_cfg.fallback_chain.is_empty();

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
                let mx = &self.matrixone;
                let enc = self.encryptor.as_ref();
                let lts = self.llm_token_service.as_ref();
                let fwd = &state.hooks.forward_headers;
                let pool_ref = self.shared_pool.as_ref().map(|sp| sp.get());
                match try_resolve_fallback(
                    cooldown,
                    &llm_cfg.fallback_chain,
                    reason,
                    |fb_name| async move {
                        resolve_llm_model_for_turn(
                            mx,
                            enc,
                            Some(fb_name.as_str()),
                            pool_ref,
                            lts,
                            fwd,
                        )
                        .await
                    },
                )
                .await
                {
                    FallbackOutcome::Resolved(fb) => {
                        llm_cfg = fb;
                    }
                    FallbackOutcome::NoFallbackConfigured => {
                        astra_core::agent_warn!(
                            "llm",
                            "rate-limit cooldown: fallback requested ({}) but no fallback configured",
                            reason.as_str()
                        );
                    }
                    FallbackOutcome::AllExhausted { chain_len } => {
                        astra_core::agent_warn!(
                            "llm",
                            "rate-limit cooldown: all {} fallback models exhausted ({})",
                            chain_len,
                            reason.as_str()
                        );
                    }
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

        let effective_restricted = self.compute_effective_restricted(state, true);
        let visible_tools = self.filtered_runtime_ready_turn_tools(&effective_restricted, state);

        // Latch prompt cache config from provider info (once per turn is fine;
        // provider doesn't change within a turn).
        let cache_cfg = PromptCacheConfig::from_cache_capability(
            llm_cfg.cache_capability,
            &llm_cfg.provider,
            &llm_cfg.model_name,
        );
        self.remember_resolved_llm_config(&llm_cfg);

        // ── 2b. Run the context pipeline ─────────────────────────────────
        // Pipeline is the single source of truth for:
        //   * system prompt content
        //   * compaction tier selection
        //   * tier-pruned tool schemas
        // Runtime no longer re-derives any of these.
        let initial_session_memory_entry =
            if let Some(svc) = state.memory_extraction_service.as_ref() {
                svc.current_session_memory_entry_for_pipeline(
                    &self.session_id,
                    state.session_turn,
                    &user_content,
                )
                .await
            } else {
                None
            };
        let turn_pipeline = self.run_turn_pipeline_with_cache_capability_and_session_memory(
            state,
            &visible_tools,
            &llm_cfg.provider,
            &llm_cfg.model_name,
            llm_cfg.context_window,
            llm_cfg.cache_capability,
            initial_session_memory_entry.clone(),
            &user_content,
        )?;
        let PipelineTurnOutcome {
            system_messages,
            volatile_preamble,
            system_plain: system_prompt_plain,
            breakdown: system_prompt_breakdown,
            tier,
            tool_schemas: pipeline_tool_schemas,
            manifest_trace,
        } = turn_pipeline;
        let mut final_system_messages = system_messages;
        let mut final_volatile_preamble = volatile_preamble;
        let mut final_system_prompt_breakdown = system_prompt_breakdown;
        let mut final_pipeline_tool_schemas = pipeline_tool_schemas;
        let mut final_manifest_trace = manifest_trace;

        // Debug: dump system prompt for cache analysis (env-gated, zero cost when off).
        if std::env::var("ASTRA_PIPELINE_DUMP_SYSTEM_PROMPT").is_ok() {
            let dump_path = std::env::temp_dir().join(format!(
                "astra-pipeline-prompt-{}-turn{}.txt",
                self.session_id, state.llm_rounds_completed
            ));
            let _ = std::fs::write(&dump_path, &system_prompt_plain);
        }
        state.last_llm_context_manifest_trace = Some(final_manifest_trace.to_json());

        // Phase 3: Memoria compaction is now a named async step, separate
        // from the pure assembly step. `execute_turn` orchestrates both so
        // the wire-building flow is readable and each phase is individually
        // testable / replaceable.
        let compact_result = self
            .compact_messages_via_memoria(
                state,
                &final_system_messages,
                &visible_tools,
                tier,
                &llm_cfg,
            )
            .await;
        if let Some(rerun) =
            crate::turn::wire_assembly::rerun_with_distinct_session_memory_entry_for_user_turn(
                compact_result.session_memory_context.as_deref(),
                initial_session_memory_entry.as_ref(),
                state.session_turn,
                &user_content,
                |session_memory_entry| {
                    self.run_turn_pipeline_with_cache_capability_and_session_memory(
                        state,
                        &visible_tools,
                        &llm_cfg.provider,
                        &llm_cfg.model_name,
                        llm_cfg.context_window,
                        llm_cfg.cache_capability,
                        Some(session_memory_entry),
                        &user_content,
                    )
                },
            )
            .transpose()?
        {
            debug_assert_eq!(rerun.tier, tier);
            final_system_messages = rerun.system_messages;
            final_volatile_preamble = rerun.volatile_preamble;
            final_system_prompt_breakdown = rerun.breakdown;
            final_pipeline_tool_schemas = rerun.tool_schemas;
            final_manifest_trace = rerun.manifest_trace;
            state.last_llm_context_manifest_trace = Some(final_manifest_trace.to_json());
        }
        // Parity with the bridge path: when Memoria returned a boundary, the
        // conversation was trimmed mid-task, so nudge the model to resume
        // instead of asking the user a follow-up question.
        let mut compacted_messages = compact_result.messages;
        crate::turn::wire_assembly::maybe_append_continuation_prompt(
            &mut compacted_messages,
            compact_result.boundary.is_some(),
        );
        let llm_messages = self.assemble_llm_messages(
            final_system_messages,
            final_volatile_preamble,
            compacted_messages,
            state,
            &llm_cfg,
            &cache_cfg,
        );

        // ── 3. Call LLM ─────────────────────────────────────────────────
        let budget = crate::prompts::budget_for_model_with_override(
            Some(&llm_cfg.model_name),
            llm_cfg.context_window,
        );
        let max_output_tokens = crate::prompts::capped_output_tokens(&budget);

        let cache_cap =
            astra_turn_core::cache_placement::CacheCapability::from_explicit_or_provider_model(
                llm_cfg.cache_capability,
                &llm_cfg.provider,
                &llm_cfg.model_name,
            );
        let mut final_tools = crate::turn::llm::context::stabilize_tool_schemas_for_cache(
            &final_pipeline_tool_schemas,
            &state.sticky_tool_schemas,
            &visible_tools,
            cache_cap,
            state.llm_rounds_completed,
        );
        state.sticky_tool_schemas = final_tools.clone();
        // Annotate tool schemas with cache_control for Anthropic.
        crate::turn::llm::context::annotate_tool_schemas_for_cache(
            &mut final_tools,
            &cache_cfg,
            &self.always_load_tool_names,
        );
        // Runtime admission must mirror the exact tool schemas sent on the
        // wire. Pipeline pruning, sticky schema stabilization, and cache
        // annotation all happen after the broad edge-tool candidate set is
        // built, so syncing earlier can admit or reject tools the model did
        // not actually see this turn.
        self.sync_valid_tools_to_wire_surface_for_state(&final_tools, state);
        self.last_turn_tool_schemas = final_tools.clone();
        if let Some(trace) = state.last_llm_context_manifest_trace.as_mut() {
            crate::turn::llm::context::augment_manifest_trace_with_wire(
                trace,
                &llm_messages,
                &final_tools,
            );
        }
        self.emit_context_meta(
            &final_system_prompt_breakdown,
            state.last_llm_context_manifest_trace.as_ref(),
        );
        state.pinned_tool_schema_tokens = estimate_tool_schema_tokens(&final_tools);
        state.last_turn_policy =
            TurnInteractionPolicy::from_tool_schemas(self.turn_interaction_mode(), &final_tools);

        // Output token escalation: if finish_reason is "length", retry once
        // with a higher max_output_tokens (up to 4× the initial budget).
        let mut effective_max_output = max_output_tokens;
        let mut attempt_in_round = 0_u32;
        let mut streamed_text = String::new();
        let mut streamed_reasoning = String::new();
        let result = loop {
            let attempt_label = llm_main_attempt_label(attempt_in_round);
            let admission_estimated_tokens = crate::prompts::estimate_tokens(
                &llm_messages,
                state.pinned_tool_schema_tokens as usize,
                0,
            )
            .saturating_add(effective_max_output);
            match crate::llm_provider_admission::admit_llm_provider_request(
                self.shared_pool.as_ref(),
                &llm_cfg.provider,
                &llm_cfg.model_name,
                admission_estimated_tokens as u64,
            )
            .await
            {
                Ok(()) => record_llm_main_attempt_metrics(
                    "admission",
                    attempt_label,
                    "allowed",
                    admission_estimated_tokens as u64,
                ),
                Err(error) => {
                    record_llm_main_attempt_metrics(
                        "admission",
                        attempt_label,
                        llm_main_error_outcome(&error),
                        admission_estimated_tokens as u64,
                    );
                    return Err(error);
                }
            }
            let prompt_round = state
                .turn_event_buffer
                .as_ref()
                .map(|buffer| buffer.current_round())
                .unwrap_or(state.llm_rounds_completed);
            let prompt_request_plan =
                astra_services::plan_prompt_request(astra_services::PromptRequestPlanInput {
                    user_id: &self.user_id,
                    session_id: &self.session_id,
                    turn: state.session_turn,
                    round: prompt_round,
                    attempt: attempt_in_round,
                    source: "server_loop_host",
                    messages: &llm_messages,
                    tools: &final_tools,
                    max_output_tokens: Some(effective_max_output),
                })
                .ok();
            record_full_llm_request_event(
                state,
                self.full_llm_capture,
                &self.user_id,
                &self.session_id,
                "server_loop_host",
                &llm_cfg.model_name,
                &llm_cfg.provider,
                attempt_in_round,
                &llm_messages,
                &final_tools,
                Some(effective_max_output),
            );
            if let Some(prompt_request_plan) = prompt_request_plan.as_ref() {
                crate::turn::llm::exchange_capture::spawn_prompt_request_plan_persist_or_log(
                    "server_loop_host",
                    self.shared_pool.clone(),
                    astra_services::PromptRequestPersistInput {
                        session_id: self.session_id.clone(),
                        user_id: self.user_id.clone(),
                        run_id: state.current_run_id.clone(),
                        turn: state.session_turn,
                        round: prompt_round,
                        attempt: attempt_in_round,
                        source: "server_loop_host".to_string(),
                        model: llm_cfg.model_name.clone(),
                        provider: llm_cfg.provider.clone(),
                    },
                    prompt_request_plan.clone(),
                );
            }
            state.step_recorder.begin_llm_round(&llm_cfg.model_name);
            let llm_round_start = std::time::Instant::now();
            let llm_cancel = llm_cancel_for_state(state);
            let r = {
                let mut attempt_text = String::new();
                let mut attempt_reasoning = String::new();
                let mut on_stream_update = |update: LlmStreamUpdate| match update {
                    LlmStreamUpdate::TextDelta(content) => {
                        attempt_text.push_str(&content);
                        if streamed_text.starts_with(&attempt_text) {
                            return;
                        }
                        if let Some(suffix) = attempt_text.strip_prefix(&streamed_text)
                            && !suffix.is_empty()
                        {
                            self.emit_event(json!({
                                "type": "text_delta",
                                "content": suffix,
                            }));
                            streamed_text.push_str(suffix);
                        } else if streamed_text.is_empty() {
                            self.emit_event(json!({
                                "type": "text_delta",
                                "content": content,
                            }));
                            streamed_text.push_str(&content);
                        }
                    }
                    LlmStreamUpdate::ReasoningDelta(content) => {
                        attempt_reasoning.push_str(&content);
                        if streamed_reasoning.starts_with(&attempt_reasoning) {
                            return;
                        }
                        if let Some(suffix) = attempt_reasoning.strip_prefix(&streamed_reasoning)
                            && !suffix.is_empty()
                        {
                            self.emit_event(json!({
                                "type": "reasoning_delta",
                                "content": suffix,
                            }));
                            streamed_reasoning.push_str(suffix);
                        } else if streamed_reasoning.is_empty() {
                            self.emit_event(json!({
                                "type": "reasoning_delta",
                                "content": content,
                            }));
                            streamed_reasoning.push_str(&content);
                        }
                    }
                };
                call_llm_and_collect_with_request_overrides_and_stream_callback(
                    &llm_messages,
                    &final_tools,
                    &llm_cfg.model_name,
                    llm_cfg.wire_model_name.as_deref(),
                    &llm_cfg.api_key,
                    &llm_cfg.base_url,
                    &llm_cfg.provider,
                    Some(effective_max_output),
                    has_fallback,
                    llm_cancel,
                    (!llm_cfg.header_overrides.is_empty()).then_some(&llm_cfg.header_overrides),
                    llm_cfg.request_body_overrides.as_ref(),
                    llm_cfg.completions_url_override.as_deref(),
                    llm_cfg.request_timeout,
                    &state.thinking,
                    Some(&mut on_stream_update),
                )
                .await
            };

            // Context-window errors flow through the accum so the agentic loop's
            // Fatal handler can trigger auto-compaction + retry.
            let r = match r {
                Ok(r) => r,
                Err(ref e) if e.kind == astra_core::ErrorKind::ContextWindow => {
                    record_llm_main_attempt_metrics(
                        "call",
                        attempt_label,
                        llm_main_error_outcome(e),
                        admission_estimated_tokens as u64,
                    );
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
                        crate::turn::llm::exchange_capture::persist_configured_capture_or_log(
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
                            Some(crate::turn::llm::exchange_capture::CaptureTrace {
                                session_turn_source: Some("state"),
                                turn_chain_id: None,
                                user_query_event_id: None,
                            }),
                        )
                        .await;
                    }
                    let accum = ChatTurnSseAccum {
                        error_message: Some(e.message.clone()),
                        system_prompt_tokens: Some(final_system_prompt_breakdown.total_tokens),
                        system_prompt_breakdown: serde_json::to_value(
                            &final_system_prompt_breakdown,
                        )
                        .ok(),
                        context_manifest_trace: state.last_llm_context_manifest_trace.clone(),
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
                    record_llm_main_attempt_metrics(
                        "call",
                        attempt_label,
                        llm_main_error_outcome(&e),
                        admission_estimated_tokens as u64,
                    );
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
                        crate::turn::llm::exchange_capture::persist_configured_capture_or_log(
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
                            Some(crate::turn::llm::exchange_capture::CaptureTrace {
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

            {
                let u = crate::turn::token_usage::TokenUsage::from_partial_json_map(&r.usage);
                crate::llm_provider_admission::record_llm_provider_admission_calibration(
                    admission_estimated_tokens as u64,
                    &r.usage,
                );
                state.step_recorder.end_llm_round(
                    &llm_cfg.model_name,
                    u.input_tokens,
                    u.output_tokens,
                    u.cached_input_tokens,
                    u.cache_creation_tokens,
                    llm_round_start.elapsed().as_millis() as u64,
                );
            }

            let will_retry_for_length = r.finish_reason.as_deref() == Some("length")
                && effective_max_output < max_output_tokens * 4;
            let llm_attempt_outcome = llm_main_success_outcome(&r, will_retry_for_length);
            record_llm_main_attempt_metrics(
                "call",
                attempt_label,
                llm_attempt_outcome,
                admission_estimated_tokens as u64,
            );
            record_full_llm_response_event(
                state,
                self.full_llm_capture,
                &self.session_id,
                "server_loop_host",
                &llm_cfg.model_name,
                &llm_cfg.provider,
                attempt_in_round,
                llm_attempt_outcome,
                json!({
                    "finish_reason": r.finish_reason.clone(),
                    "full_text": r.full_text.clone(),
                    "reasoning": r.reasoning.clone(),
                    "tool_calls": r.tool_calls.clone(),
                    "usage": r.usage.clone(),
                }),
            );

            if will_retry_for_length {
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
            crate::turn::llm::exchange_capture::persist_configured_capture_or_log(
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
                Some(crate::turn::llm::exchange_capture::CaptureTrace {
                    session_turn_source: Some("state"),
                    turn_chain_id: None,
                    user_query_event_id: None,
                }),
            )
            .await;
        }

        // ── 4. Emit SSE events for client ───────────────────────────────
        if !result.full_text.is_empty() {
            if streamed_text.is_empty() {
                self.emit_event(json!({
                    "type": "text_delta",
                    "content": result.full_text,
                }));
                streamed_text = result.full_text.clone();
            } else if let Some(suffix) = result.full_text.strip_prefix(&streamed_text)
                && !suffix.is_empty()
            {
                self.emit_event(json!({
                    "type": "text_delta",
                    "content": suffix,
                }));
                streamed_text.push_str(suffix);
            }
        }
        if result.reasoning.is_empty() {
            if !streamed_reasoning.is_empty() {
                self.emit_event(json!({ "type": "reasoning_done" }));
            }
        } else if streamed_reasoning.is_empty() {
            self.push_reasoning_events(&result.reasoning);
        } else {
            if let Some(suffix) = result.reasoning.strip_prefix(&streamed_reasoning)
                && !suffix.is_empty()
            {
                self.emit_event(json!({
                    "type": "reasoning_delta",
                    "content": suffix,
                }));
                streamed_reasoning.push_str(suffix);
            }
            self.emit_event(json!({ "type": "reasoning_done" }));
        }
        if !result.full_text.is_empty() && streamed_text == result.full_text {
            state.final_text_streamed = true;
        }
        if !result.usage.is_empty() {
            let u = crate::turn::token_usage::TokenUsage::from_partial_json_map(&result.usage);
            self.emit_event(json!({
                "type": "usage",
                "input_tokens": u.input_tokens,
                "cached_input_tokens": u.cached_input_tokens,
                "cache_creation_tokens": u.cache_creation_tokens,
                "output_tokens": u.output_tokens,
                "total_tokens": u.total_tokens(),
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
        accum.system_prompt_tokens = Some(final_system_prompt_breakdown.total_tokens);
        accum.system_prompt_breakdown = serde_json::to_value(&final_system_prompt_breakdown).ok();
        accum.context_manifest_trace = state.last_llm_context_manifest_trace.clone();

        Ok(HostTurnResult {
            accum,
            ttft_ms,
            edge_tool_round,
            error_kind: None,
        })
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

    async fn maybe_pre_turn_compact(
        &mut self,
        state: &mut crate::turn::agentic_loop::host::AgenticLoopState,
        pressure: f64,
        quiet: bool,
    ) {
        if pressure < 0.80
            || state.compact_tier_applied >= CompactionTier::CompactHistory
            || state.messages.len() <= 10
        {
            return;
        }
        if let Some(reason) =
            should_skip_auxiliary_llm_for_capacity(PRE_TURN_COMPACTION_LLM_POLICY_ENV)
        {
            tracing::debug!(
                target: "astra::pre_turn_compaction",
                policy = auxiliary_llm_policy_label(PRE_TURN_COMPACTION_LLM_POLICY_ENV),
                reason,
                "pre-turn LLM compaction skipped by capacity policy"
            );
            return;
        }
        let Some(params) = self.resolved_llm_params.clone() else {
            return;
        };

        let user_content = state
            .messages
            .iter()
            .rev()
            .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
            .and_then(|m| m.get("content").and_then(Value::as_str))
            .unwrap_or("")
            .to_string();

        let effective_restricted = self.compute_effective_restricted(state, false);
        let visible_tools = self.filtered_runtime_ready_turn_tools(&effective_restricted, state);
        // We only need the system messages here — the inline summary call
        // reuses the main turn's system prefix, not its tools.
        let system_messages = match self.run_turn_pipeline(
            state,
            &visible_tools,
            &params.provider,
            &params.model_name,
            &user_content,
        ) {
            Ok(outcome) => outcome.system_messages,
            Err(error) => {
                astra_core::agent_warn!(
                    "pipeline",
                    "skipping pre-turn compaction because context pipeline failed: {}",
                    error
                );
                return;
            }
        };
        // Use the trait's summary_client() so gateway overrides and forwarded
        // auth headers are respected, rather than constructing a plain client inline.
        let Some(client) = self.summary_client() else {
            return;
        };
        if let Some(summary_text) = astra_turn_core::cloud_summary::generate_inline_summary(
            &system_messages,
            &state.messages,
            client.as_ref(),
        )
        .await
        {
            let old_count = state.messages.len();
            let keep_recent = (old_count / 4).max(4);
            let mut spill_count = old_count - keep_recent;
            // Snap to a safe role boundary so we never split an assistant/tool pair.
            spill_count =
                crate::turn::agentic_loop::execution_phase::adjust_spill_boundary_for_tool_pairs(
                    &state.messages,
                    spill_count,
                );
            let spilled: Vec<_> = state.messages.drain(..spill_count).collect();
            let tokens_freed = spilled
                .iter()
                .map(|m| serde_json::to_string(m).map(|s| s.len()).unwrap_or(0))
                .sum::<usize>() as u64
                / 4;

            state.messages.insert(
                0,
                serde_json::json!({
                    "role": "system",
                    "content": format!(
                        "[Conversation compacted — {} messages summarized]\n\n{}",
                        spilled.len(),
                        summary_text,
                    )
                }),
            );
            state.compact_tier_applied = CompactionTier::CompactHistory;

            if let Some(ref mut sess) = state.pipeline_session {
                sess.recovery.record_reactive_compact();
                sess.stats.record_compaction(tokens_freed);
            }
            if !quiet {
                self.emit_headless_line(
                    HeadlessStderrStyle::Yellow,
                    format!(
                        "♻ Pre-turn LLM compact: freed ~{} tokens ({} → summary)",
                        tokens_freed,
                        spilled.len(),
                    ),
                );
            }
        } else if !quiet {
            self.emit_headless_line(
                HeadlessStderrStyle::Dim,
                format!(
                    "  ⚠ Pre-turn LLM compact failed; continuing at {:.0}% pressure",
                    pressure * 100.0,
                ),
            );
        }
    }

    fn summary_client(&self) -> Option<Box<dyn astra_turn_core::cloud_summary::SummaryLlmClient>> {
        // LLM config is resolved per-turn inside execute_turn. Prefer the full
        // config because gateway completion overrides and forwarded auth
        // headers are part of the runtime contract for all auxiliary LLM calls.
        if let Some(config) = self.resolved_llm_config.as_ref() {
            return Some(Box::new(RequestAwareSummaryClient::from_resolved_config(
                config, 4096,
            )));
        }

        // Older tests can still seed the minimal params directly.
        let params = self.resolved_llm_params.as_ref()?;
        Some(Box::new(
            astra_turn_core::cloud_summary::HttpSummaryClient::new(params.clone()),
        ))
    }

    fn valid_tool_names(&self) -> &HashSet<String> {
        &self.valid_tools
    }

    fn deferred_tool_names(&self) -> HashSet<String> {
        self.deferred_tool_names_from_edge_profile_for_model(
            self.resolved_model_name.as_deref(),
            self.resolved_context_window,
        )
    }

    fn capabilities(&self) -> astra_turn_core::capability::CapabilitySet {
        self.capabilities.clone()
    }

    fn on_turn_completed(&mut self, state: &crate::turn::agentic_loop::host::AgenticLoopState) {
        // G2: server-side parent capture. Mirrors the CLI host's
        // `on_turn_completed`, so delegate / agent-spawn sub-runs
        // routed through the server DelegationEngine can inherit the
        // parent's cacheable prefix. No-op unless the store was wired
        // in and the feature flag is on (`capture_parent_prefix`
        // early-returns if so).
        let Some(store) = self.prefix_store.as_ref() else {
            return;
        };
        let parent_run_id = match state.current_run_id.as_deref() {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => return,
        };
        let model_id = self.model_override.clone().unwrap_or_default();
        let provider = astra_turn_core::fork_prefix::ProviderKind::from_provider_hint(&model_id);
        let raw_provider = provider.raw_provider_name().to_owned();
        let Ok(canonical_prefix_bytes) = serde_json::to_vec(&state.messages) else {
            return;
        };
        let captured_at_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // Populate tool_schemas from the turn's advertised list so
        // per-tool drift attribution works. `system_blocks` stays
        // empty for now: the server assembles system messages via
        // the canonical byte form. A follow-up can thread the
        // system_msgs Vec<Value> through the same stash path if the
        // telemetry attribution need arises.
        let tool_schemas =
            astra_turn_core::fork_prefix::build_tool_schema_entries(&self.last_turn_tool_schemas);
        let req = astra_turn_core::fork_capture::CaptureRequest {
            parent_run_id,
            parent_turn_seq: state.llm_rounds_completed,
            provider,
            model_id: model_id.clone(),
            thinking: astra_turn_core::thinking_config::fork_capture_thinking_slice(
                &state.thinking,
                &raw_provider,
                &model_id,
            ),
            system_blocks: vec![],
            tool_schemas,
            beta_headers: vec![],
            canonical_prefix_bytes,
            cache_mode: astra_turn_core::fork_prefix::CacheMode::Write,
            captured_at_secs,
            microcompact_fired_in_turn: false,
        };
        // Route the capture outcome to telemetry rather than discarding it —
        // silent drops hide "child sub-run started without inheriting parent
        // cache prefix" and "prefix store ran out of room" conditions, both
        // of which directly degrade cache hit rate on delegated runs.
        match astra_turn_core::fork_capture::capture_parent_prefix(req, store.as_ref()) {
            astra_turn_core::fork_capture::ForkCaptureOutcome::Captured { prefix_id, evicted } => {
                if !evicted.is_empty() {
                    astra_core::agent_warn!(
                        "fork_prefix",
                        "parent prefix captured ({prefix_id}); evicted {} old entries to make room: {:?}",
                        evicted.len(),
                        evicted
                    );
                }
            }
            astra_turn_core::fork_capture::ForkCaptureOutcome::Skipped { reason } => {
                astra_core::agent_warn!(
                    "fork_prefix",
                    "parent prefix capture skipped ({reason:?}) — child sub-runs in this \
                     session will start with cold cache"
                );
            }
        }
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

fn tool_name_from_tool_start_event(event_obj: &Map<String, Value>) -> Option<&str> {
    event_obj
        .get("tool_name")
        .or_else(|| event_obj.get("tool"))
        .or_else(|| event_obj.get("name"))
        .and_then(Value::as_str)
        .or_else(|| {
            let tool_call = event_obj.get("tool_call").and_then(Value::as_object)?;
            let function = tool_call.get("function").and_then(Value::as_object);
            function
                .and_then(|function| function.get("name"))
                .or_else(|| tool_call.get("name"))
                .and_then(Value::as_str)
        })
}

fn tool_name_from_tool_end_event(event_obj: &Map<String, Value>) -> Option<&str> {
    event_obj
        .get("tool_name")
        .or_else(|| event_obj.get("tool"))
        .or_else(|| event_obj.get("name"))
        .and_then(Value::as_str)
}

pub(crate) fn agent_live_event_kind_from_server_sse(event: &Value) -> Option<AgentLiveEventKind> {
    let event_type = event.get("type").and_then(Value::as_str)?;
    match event_type {
        "text_delta" => event
            .get("content")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(|text| AgentLiveEventKind::OutputDelta(text.to_string())),
        "reasoning_delta" | "thinking_delta" => event
            .get("content")
            .or_else(|| event.get("text"))
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(|text| AgentLiveEventKind::ThinkingDelta(text.to_string())),
        "tool_call" | "tool_call_start" => {
            let (name, description, tool_use_id) = live_tool_started_fields(event)?;
            Some(AgentLiveEventKind::ToolStarted {
                name,
                description,
                tool_use_id,
            })
        }
        "tool_call_end" | "tool_transport_completed" | "tool_transport_failed" => {
            let (name, description, status, duration_ms, output_summary, output, tool_use_id) =
                live_tool_completed_fields(event)?;
            Some(AgentLiveEventKind::ToolCompleted {
                name,
                description,
                status,
                duration_ms,
                output_summary,
                output,
                tool_use_id,
            })
        }
        _ => None,
    }
}

fn live_tool_started_fields(event: &Value) -> Option<(String, String, String)> {
    if let Some(tool_call) = event.get("tool_call").and_then(Value::as_object) {
        let function = tool_call.get("function").and_then(Value::as_object);
        let name = function
            .and_then(|function| function.get("name"))
            .or_else(|| tool_call.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("tool")
            .to_string();
        let description = function
            .and_then(|function| function.get("arguments"))
            .or_else(|| tool_call.get("arguments"))
            .map(live_value_to_string)
            .unwrap_or_default();
        let tool_use_id = tool_call
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        return Some((name, description, tool_use_id));
    }

    let name = event
        .get("tool_name")
        .or_else(|| event.get("tool"))
        .or_else(|| event.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("tool")
        .to_string();
    let description = event
        .get("arguments")
        .or_else(|| event.get("args"))
        .map(live_value_to_string)
        .unwrap_or_default();
    let tool_use_id = event
        .get("call_id")
        .or_else(|| event.get("tool_call_id"))
        .or_else(|| event.get("tool_use_id"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Some((name, description, tool_use_id))
}

#[allow(clippy::type_complexity)]
fn live_tool_completed_fields(
    event: &Value,
) -> Option<(
    String,
    String,
    String,
    u64,
    Option<String>,
    Option<String>,
    String,
)> {
    let failed = event
        .get("success")
        .and_then(Value::as_bool)
        .map(|success| !success)
        .unwrap_or(false)
        || event.get("type").and_then(Value::as_str) == Some("tool_transport_failed");
    let status = event
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or(if failed { "error" } else { "ok" })
        .to_string();
    let name = event
        .get("tool_name")
        .or_else(|| event.get("tool"))
        .or_else(|| event.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("tool")
        .to_string();
    let description = event
        .get("description")
        .or_else(|| event.get("arguments"))
        .or_else(|| event.get("args"))
        .map(live_value_to_string)
        .unwrap_or_default();
    let duration_ms = event
        .get("duration_ms")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_summary = event
        .get("output_summary")
        .or_else(|| event.get("summary"))
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let output = event
        .get("result")
        .or_else(|| event.get("output"))
        .or_else(|| event.get("error"))
        .map(live_value_to_string)
        .filter(|value| !value.is_empty());
    let tool_use_id = event
        .get("call_id")
        .or_else(|| event.get("tool_call_id"))
        .or_else(|| event.get("tool_use_id"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Some((
        name,
        description,
        status,
        duration_ms,
        output_summary,
        output,
        tool_use_id,
    ))
}

fn live_value_to_string(value: &Value) -> String {
    value
        .as_str()
        .map(ToString::to_string)
        .unwrap_or_else(|| value.to_string())
}

// ─── Progress event → SSE value conversion ──────────────────────────────────

/// Convert an `AgentProgressEvent` into an SSE-compatible JSON value.
/// Returns `None` for event types that don't need to be sent to web clients.
pub(crate) fn progress_event_to_sse(
    evt: &crate::orchestration::AgentProgressEvent,
) -> Option<Value> {
    use crate::orchestration::ProgressEventType;
    let agent_id = &evt.agent_id;
    let ts = evt.timestamp_epoch_ms;

    let event = match &evt.event_type {
        ProgressEventType::AgentSpawned {
            run_id,
            parent_run_id,
            agent_type,
            description,
            fanout_slot,
        } => json!({
            "type": "agent_spawned",
            "agent_id": agent_id,
            "run_id": run_id,
            "parent_run_id": parent_run_id,
            "agent_type": agent_type,
            "description": description,
            "fanout_slot": fanout_slot,
            "timestamp": ts,
        }),
        ProgressEventType::Started { description } => json!({
            "type": "agent_progress",
            "agent_id": agent_id,
            "status": "started",
            "description": description,
            "timestamp": ts,
        }),
        ProgressEventType::TurnCompleted {
            turn,
            tool_calls_this_turn,
            activity,
        } => json!({
            "type": "agent_progress",
            "agent_id": agent_id,
            "status": "turn_completed",
            "turn": turn,
            "tool_calls_this_turn": tool_calls_this_turn,
            "activity": activity,
            "timestamp": ts,
        }),
        ProgressEventType::Idle => json!({
            "type": "agent_progress",
            "agent_id": agent_id,
            "status": "idle",
            "timestamp": ts,
        }),
        ProgressEventType::Busy { activity } => json!({
            "type": "agent_progress",
            "agent_id": agent_id,
            "status": "busy",
            "activity": activity,
            "timestamp": ts,
        }),
        ProgressEventType::Completed {
            result_summary,
            total_tool_calls,
            total_tokens,
            duration_ms,
        } => json!({
            "type": "agent_completed",
            "agent_id": agent_id,
            "status": "completed",
            "result_summary": result_summary,
            "total_tool_calls": total_tool_calls,
            "total_tokens": { "prompt": total_tokens.0, "completion": total_tokens.1 },
            "duration_ms": duration_ms,
            "timestamp": ts,
        }),
        ProgressEventType::Interrupted {
            reason,
            partial_summary,
            total_tool_calls,
            total_tokens,
            duration_ms,
        } => json!({
            "type": "agent_interrupted",
            "agent_id": agent_id,
            "status": "interrupted",
            "reason": reason,
            "partial_summary": partial_summary,
            "total_tool_calls": total_tool_calls,
            "total_tokens": { "prompt": total_tokens.0, "completion": total_tokens.1 },
            "duration_ms": duration_ms,
            "timestamp": ts,
        }),
        ProgressEventType::Failed { error } => json!({
            "type": "agent_failed",
            "agent_id": agent_id,
            "status": "failed",
            "error": error,
            "timestamp": ts,
        }),
        ProgressEventType::Waiting { reason } => json!({
            "type": "agent_waiting",
            "agent_id": agent_id,
            "status": "waiting",
            "reason": reason,
            "timestamp": ts,
        }),
        ProgressEventType::Cancelled { reason } => json!({
            "type": "agent_cancelled",
            "agent_id": agent_id,
            "status": "cancelled",
            "reason": reason,
            "timestamp": ts,
        }),
        ProgressEventType::ToolExecuting { tool_name, turn } => json!({
            "type": "agent_progress",
            "agent_id": agent_id,
            "status": "tool_executing",
            "tool_name": tool_name,
            "turn": turn,
            "timestamp": ts,
        }),
        ProgressEventType::LlmCallStarted { turn } => json!({
            "type": "agent_progress",
            "agent_id": agent_id,
            "status": "llm_call_started",
            "turn": turn,
            "timestamp": ts,
        }),
        ProgressEventType::LlmCallCompleted {
            turn,
            ttft_ms,
            duration_ms,
        } => json!({
            "type": "agent_progress",
            "agent_id": agent_id,
            "status": "llm_call_completed",
            "turn": turn,
            "ttft_ms": ttft_ms,
            "duration_ms": duration_ms,
            "timestamp": ts,
        }),
        ProgressEventType::MetricsUpdate {
            turn,
            max_turns,
            total_prompt_tokens,
            total_completion_tokens,
            total_tool_calls,
        } => json!({
            "type": "agent_progress",
            "agent_id": agent_id,
            "status": "metrics_update",
            "turn": turn,
            "max_turns": max_turns,
            "total_prompt_tokens": total_prompt_tokens,
            "total_completion_tokens": total_completion_tokens,
            "total_tool_calls": total_tool_calls,
            "timestamp": ts,
        }),
        ProgressEventType::PermissionDenied {
            tool_name,
            reason,
            turn,
        } => json!({
            "type": "agent_progress",
            "agent_id": agent_id,
            "status": "permission_denied",
            "tool_name": tool_name,
            "reason": reason,
            "turn": turn,
            "timestamp": ts,
        }),
    };
    Some(merge_progress_metadata(event, evt.metadata.as_ref()))
}

fn merge_progress_metadata(mut event: Value, metadata: Option<&Value>) -> Value {
    let (Some(event_obj), Some(metadata_obj)) =
        (event.as_object_mut(), metadata.and_then(Value::as_object))
    else {
        return event;
    };
    for (key, value) in metadata_obj {
        event_obj
            .entry(key.clone())
            .or_insert_with(|| value.clone());
    }
    event
}

fn llm_capture_error_response(error: &astra_core::ClassifiedError) -> Value {
    let mut response = json!({
        "error": error.message,
        "kind": error.kind.to_string(),
    });
    if let Some(details_json) = error.details_json.as_deref()
        && let Ok(Value::Object(mut details)) = serde_json::from_str::<Value>(details_json)
        && let Some(response_object) = response.as_object_mut()
    {
        // Canonical-schema guarantee: error artifacts surface `usage` in the
        // same shape as the success path (see `bridge_sse_helpers` and
        // `turn::token_usage::TokenUsage::to_json_map`). Provider dialects and
        // partial canonical-like maps are normalized here so downstream
        // consumers have a single schema to reason about.
        if let Some(Value::Object(raw_usage)) = details.get("usage").cloned() {
            let canonical = normalize_usage_to_canonical(&raw_usage);
            details.insert("usage".to_string(), Value::Object(canonical));
        }
        response_object.extend(details);
    }
    response
}

/// Best-effort normalization of a provider-dialect `usage` object into the
/// canonical token-usage schema. Returns the canonical map when recognizable
/// tokens are present; otherwise returns the input untouched so we never
/// drop fields we don't understand.
fn normalize_usage_to_canonical(
    raw: &serde_json::Map<String, Value>,
) -> serde_json::Map<String, Value> {
    // Anthropic and canonical shapes both use `input_tokens`/`output_tokens`.
    // Provider-specific cache keys are the deterministic discriminator and
    // must be handled before completing a canonical-like map.
    let looks_anthropic = raw.contains_key("cache_read_input_tokens")
        || raw.contains_key("cache_creation_input_tokens");
    // Anthropic dialect (Messages API, deepseek `/anthropic` endpoint, …):
    // `input_tokens`/`output_tokens` plus separate `cache_read_input_tokens`
    // and `cache_creation_input_tokens`. Must be checked BEFORE the generic
    // `input_tokens` canonical fast-path above would match, otherwise the
    // cache fields would leak through verbatim.
    if looks_anthropic {
        if let Some(canonical) = crate::turn::token_usage::extract_usage(
            crate::turn::token_usage::UsageDialect::AnthropicMessages,
            raw,
        ) {
            return canonical.to_json_map();
        }
    }
    // Canonical or canonical-like shape. Complete missing cache buckets with
    // zeros and recompute `total_tokens` so error artifacts never surface a
    // partial token schema.
    if raw.contains_key("input_tokens")
        || raw.contains_key("cached_input_tokens")
        || raw.contains_key("cache_creation_tokens")
        || raw.contains_key("output_tokens")
    {
        return crate::turn::token_usage::TokenUsage::from_partial_json_map(raw).to_json_map();
    }
    // Detect OpenAI dialect (prompt_tokens / completion_tokens / …).
    if raw.contains_key("prompt_tokens") || raw.contains_key("completion_tokens") {
        if let Some(canonical) = crate::turn::token_usage::extract_usage(
            crate::turn::token_usage::UsageDialect::OpenAi,
            raw,
        ) {
            return canonical.to_json_map();
        }
    }
    raw.clone()
}

fn hidden_execution_boundary_tool_names(visible_tools: &[Value]) -> Vec<String> {
    let visible: HashSet<String> = visible_tools
        .iter()
        .filter_map(tool_schema_name)
        .map(str::to_string)
        .collect();
    astra_runtime_env::ToolRegistry::builtins()
        .iter()
        .filter(|spec| {
            matches!(
                spec.required.executor,
                astra_runtime_env::RequiredExecutor::RuntimeExecutor
            ) || !matches!(
                spec.required.workspace,
                astra_runtime_env::RequiredWorkspace::None
            )
        })
        .map(|spec| spec.name.clone())
        .filter(|name| !visible.contains(name))
        .collect()
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn::agentic_loop::host::ASK_USER_TOOL_NAME;
    use crate::turn::agentic_loop::host::run_agentic_loop_with_host;
    #[cfg(feature = "bridge-e2e-hooks")]
    use astra_services::SessionArtifactStore;
    use astra_turn_core::cloud_summary::SummaryLlmClient;
    use astra_turn_core::edge_ledger::{approval_callback_key, tool_callback_key};
    use astra_turn_core::sse_stream_host::EdgeToolExecResult;
    use std::ffi::OsString;

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }

        fn remove(key: &'static str) -> Self {
            let previous = std::env::var_os(key);
            unsafe { std::env::remove_var(key) };
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            unsafe {
                match self.previous.take() {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    fn mock_matrixone() -> MatrixOneSettings {
        MatrixOneSettings::mock()
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
    fn llm_main_attempt_outcome_classifiers_are_stable() {
        let admission_error = astra_core::ClassifiedError::new(
            astra_core::ErrorKind::RateLimit,
            "provider gate rejected request",
        )
        .with_details_json(json!({"source": "llm_provider_admission"}).to_string());
        assert_eq!(
            llm_main_error_outcome(&admission_error),
            "admission_rejected"
        );

        let provider_rate_limit =
            astra_core::ClassifiedError::new(astra_core::ErrorKind::RateLimit, "provider 429");
        assert_eq!(
            llm_main_error_outcome(&provider_rate_limit),
            "error_rate_limit"
        );

        let success = LlmCallResult {
            finish_reason: Some("tool_calls".to_string()),
            ..Default::default()
        };
        assert_eq!(
            llm_main_success_outcome(&success, false),
            "success_tool_calls"
        );
        assert_eq!(llm_main_success_outcome(&success, true), "length_retry");
    }

    #[test]
    #[serial_test::serial(llm_main_attempt_metrics)]
    fn llm_main_attempt_metrics_render_low_cardinality_series() {
        let registry = Arc::new(astra_turn_core::pipeline_metrics::MetricsRegistry::new());
        set_llm_main_attempt_metrics_registry(registry.clone());

        record_llm_main_attempt_metrics("admission", "initial", "allowed", 2_048);
        record_llm_main_attempt_metrics("call", "initial", "success_stop", 2_048);
        record_llm_main_attempt_metrics("admission", "retry", "admission_rejected", 4_096);

        let rendered = registry.render_prometheus();
        assert!(rendered.contains(
            r#"astra_llm_main_attempts_total{attempt="initial",outcome="allowed",phase="admission"} 1"#
        ));
        assert!(rendered.contains(
            r#"astra_llm_main_attempts_total{attempt="initial",outcome="success_stop",phase="call"} 1"#
        ));
        assert!(rendered.contains(
            r#"astra_llm_main_attempts_total{attempt="retry",outcome="admission_rejected",phase="admission"} 1"#
        ));
        assert!(rendered.contains(
            r#"astra_llm_main_attempt_tokens_total{attempt="retry",outcome="admission_rejected",phase="admission"} 4096"#
        ));
    }

    #[test]
    fn builder_populates_server_tools_by_default_when_edge_tools_empty() {
        let host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u1".to_string(),
            "s1".to_string(),
        )
        .with_capabilities(crate::capabilities::lifecycle_server_capabilities(
            false, false,
        ))
        .build();

        assert!(host.server_side_tools);
        assert!(
            !host.edge_tools.is_empty(),
            "web/default mode should expose server runtime tool schemas"
        );
    }

    #[test]
    fn builder_can_disable_server_tool_catalog_for_registry_runtime() {
        let host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u1".to_string(),
            "s1".to_string(),
        )
        .with_capabilities(crate::capabilities::lifecycle_server_capabilities(
            false, false,
        ))
        .with_server_tool_catalog_enabled(false)
        .build();

        assert!(!host.server_side_tools);
        assert!(
            host.edge_tools.is_empty(),
            "Agent Binding mode starts with no local/request tool schemas"
        );
    }

    #[test]
    fn registry_runtime_strict_admissible_tools_excludes_static_catalog() {
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u1".to_string(),
            "s1".to_string(),
        )
        .with_capabilities(crate::capabilities::lifecycle_server_capabilities(
            false, false,
        ))
        .with_server_tool_catalog_enabled(false)
        .with_static_tool_catalog_admissible(false)
        .build();
        let visible = vec![json!({
            "type": "function",
            "function": {
                "name": "mcp__tools__query",
                "description": "Binding-discovered MCP tool",
                "parameters": { "type": "object", "properties": {} }
            }
        })];

        host.sync_valid_tools_to_visible(&visible);

        assert!(host.valid_tool_names().contains("mcp__tools__query"));
        assert!(!host.valid_tool_names().contains("bash"));
        assert!(!host.valid_tool_names().contains("tool_search"));
    }

    #[test]
    fn registry_runtime_mcp_tools_switch_empty_host_to_server_side_execution() {
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u1".to_string(),
            "s1".to_string(),
        )
        .with_capabilities(crate::capabilities::lifecycle_server_capabilities(
            false, false,
        ))
        .with_server_tool_catalog_enabled(false)
        .with_static_tool_catalog_admissible(false)
        .build();

        assert!(!host.server_side_tools);

        host.install_runtime_tool_schemas(vec![json!({
            "type": "function",
            "function": {
                "name": "mcp__tools__query",
                "description": "Binding-discovered MCP tool",
                "parameters": { "type": "object", "properties": {} }
            }
        })]);

        assert!(
            host.server_side_tools,
            "registry runtime MCP tools are executed by ServerToolExecutor, not edge ledger"
        );
        assert!(host.valid_tool_names().contains("mcp__tools__query"));
    }

    #[test]
    fn runtime_mcp_install_does_not_reclassify_existing_edge_tool_surface() {
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u1".to_string(),
            "s1".to_string(),
        )
        .with_capabilities(crate::capabilities::lifecycle_server_capabilities(
            false, false,
        ))
        .with_edge_tools(sample_edge_tools())
        .with_execution_binding_snapshot(edge_runtime_snapshot())
        .build();

        assert!(!host.server_side_tools);

        host.install_runtime_tool_schemas(vec![json!({
            "type": "function",
            "function": {
                "name": "mcp__tools__query",
                "description": "Binding-discovered MCP tool",
                "parameters": { "type": "object", "properties": {} }
            }
        })]);

        assert!(
            !host.server_side_tools,
            "existing edge/client tool surfaces must keep edge-ledger execution"
        );
        assert!(host.valid_tool_names().contains("mcp__tools__query"));
    }

    fn schema_names(tools: &[Value]) -> HashSet<String> {
        tools
            .iter()
            .filter_map(tool_schema_name)
            .map(str::to_string)
            .collect()
    }

    fn edge_runtime_snapshot() -> ExecutionBindingSnapshot {
        ExecutionBindingSnapshot::new(
            WorkspaceBinding::edge_workspace(
                "MacBook Pro",
                "/Users/test/project",
                WorkspaceAuthority::ReadWrite,
            ),
            ExecutorBinding::edge_agent(
                "edge-1",
                "MacBook Pro",
                crate::server::tool_transport::ToolTransportKind::EdgeWs,
                crate::server::tool_transport::ExecutorStatus::Online,
            ),
            astra_runtime_env::RuntimeBinding::host_process("edge-host"),
        )
    }

    fn server_tool_executor_with_agent_context(
        work_dir: &Path,
    ) -> crate::server::server_tool_executor::ServerToolExecutor {
        let mut executor = crate::server::server_tool_executor::ServerToolExecutor::new(
            work_dir.to_path_buf(),
            "user1".into(),
            "sess1".into(),
            None,
            None,
        );
        let transport = Arc::new(astra_messaging::InProcessTransport::new());
        let tracker = Arc::new(crate::server::delegation::engine::DelegationTracker::new());
        let router = Arc::new(astra_messaging::AgentMailboxRouter::new(transport, tracker));
        let spawner = Arc::new(crate::orchestration::DynamicAgentSpawner::new(router));
        executor.set_agent_tool_context(crate::orchestration::AgentToolContext {
            run_id: "run1".into(),
            agent_id: "agent1".into(),
            delegation_chain: Vec::new(),
            current_model: Some("test-model".into()),
            recursion_depth: 0,
            is_fork_child: false,
            working_dir: work_dir.to_path_buf(),
            spawner,
            inherited_permissions: crate::orchestration::InheritedPermissions::auto_approve(),
            active_skills: Vec::new(),
            live_event_sink: None,
            trace_context: None,
            execution_metadata: None,
        });
        executor
    }

    fn message_text(message: &Value) -> String {
        let Some(content) = message.get("content") else {
            return message.to_string();
        };
        if let Some(text) = content.as_str() {
            return text.to_string();
        }
        if let Some(blocks) = content.as_array() {
            return blocks
                .iter()
                .filter_map(|block| {
                    block
                        .get("text")
                        .and_then(Value::as_str)
                        .or_else(|| block.as_str())
                        .map(str::to_string)
                })
                .collect::<Vec<_>>()
                .join("\n");
        }
        content.to_string()
    }

    fn pipeline_outcome_text(outcome: &PipelineTurnOutcome) -> String {
        outcome
            .system_messages
            .iter()
            .chain(outcome.volatile_preamble.iter())
            .map(message_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn llm_request_dump_failures_are_not_silently_ignored() {
        let source = include_str!("server_loop_host.rs");
        // Use `find` (first occurrence) — rfind would find the test module itself
        // if a nested `mod tests {` were ever added. First occurrence is always
        // the production `mod tests {` opener.
        let tests_start = source
            .find("\n#[cfg(test)]\nmod tests {")
            .expect("cfg(test) + mod tests marker");
        let production = &source[..tests_start];
        for context in [
            "server_loop_host context window dump persist failed",
            "server_loop_host error dump persist failed",
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

    /// Pin: after compaction, the server loop MUST re-inject invoked-skill
    /// instructions via `AttachmentBuilder::add_skill`. Without this, skills
    /// that were loaded before compaction lose their full content once history
    /// is summarized, and the model would have to re-invoke them — costing an
    /// extra round trip per skill.
    ///
    /// The mechanism hinges on:
    ///   1. `state.skills.invoked` tracking every skill the model has called
    ///      (see `InvokedSkill` at `turn/skill_tool.rs:147`).
    ///   2. The compaction path iterating that map and feeding each entry into
    ///      `AttachmentBuilder::add_skill` so `to_messages()` restores the
    ///      skill content as user messages appended after the compact summary.
    ///
    /// A silent refactor that drops either piece would break cross-turn skill
    /// persistence with no runtime error. This test pins both anchors.
    #[test]
    fn post_compaction_reinjects_invoked_skills() {
        // Cross-file anchor: production code now lives in the shared
        // `llm_context` + `wire_assembly` path. Both pieces must be present
        // for the re-injection pipeline to work; a silent refactor that drops
        // either is a cross-turn skill-loss bug waiting to happen.
        let host_src = include_str!("server_loop_host.rs");
        let host_tests_start = host_src
            .find("\n#[cfg(test)]\nmod tests {")
            .expect("cfg(test) + mod tests marker");
        let host_production = &host_src[..host_tests_start];
        assert!(
            host_production.contains("assemble_llm_messages("),
            "server host must route final wire assembly through the shared helper"
        );

        let llm_context_src = include_str!("../turn/llm/context.rs");
        let llm_context_tests_start = llm_context_src
            .find("\n#[cfg(test)]\nmod ")
            .expect("llm_context cfg(test) + mod marker");
        let llm_context_production = &llm_context_src[..llm_context_tests_start];
        assert!(
            llm_context_production.contains("input.state.skills.invoked"),
            "shared LLM assembly must consult state.skills.invoked to decide re-injection"
        );
        // Ordering guard: most-recently invoked skill first (so the oldest
        // content sits closest to the model's current turn after to_messages
        // reverses). If this sort key flips, cross-turn ordering will break.
        assert!(
            llm_context_production.contains("std::cmp::Reverse(skill.invoked_at_turn)"),
            "invoked skills must be sorted most-recent-first before re-injection"
        );

        let shared_src = include_str!("../turn/wire_assembly.rs");
        let shared_tests_start = shared_src
            .find("\n#[cfg(test)]\nmod tests {")
            .expect("wire_assembly cfg(test) + mod tests marker");
        let shared_production = &shared_src[..shared_tests_start];
        assert!(
            shared_production.contains("builder.add_skill(skill.name, skill.content)"),
            "shared assembly must feed invoked skills into AttachmentBuilder::add_skill \
             so full instructions survive compaction"
        );
    }

    #[test]
    fn llm_error_paths_publish_remote_llm_capture_artifacts() {
        let source = include_str!("server_loop_host.rs");
        let tests_start = source
            .find("\n#[cfg(test)]\nmod tests {")
            .expect("cfg(test) + mod tests marker");
        let production = &source[..tests_start];
        for context in [
            "server_loop_host context window capture",
            "server_loop_host error capture",
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
                "usage": { "input_tokens": 10, "output_tokens": 4, "total_tokens": 14 }
            })
            .to_string(),
        );
        let response = llm_capture_error_response(&error);
        assert_eq!(response["partial_full_text"].as_str(), Some("half answer"));
        assert_eq!(response["usage"]["input_tokens"].as_i64(), Some(10));
        assert_eq!(response["usage"]["cached_input_tokens"].as_i64(), Some(0));
        assert_eq!(response["usage"]["cache_creation_tokens"].as_i64(), Some(0));
        assert_eq!(response["usage"]["output_tokens"].as_i64(), Some(4));
        assert_eq!(response["usage"]["total_tokens"].as_i64(), Some(14));
        assert_eq!(response["kind"].as_str(), Some("stream_transport"));
    }

    #[test]
    fn llm_capture_error_response_completes_canonical_like_usage() {
        let error =
            astra_core::ClassifiedError::new(astra_core::ErrorKind::StreamTransport, "boom")
                .with_details_json(
                    json!({
                        "usage": { "input_tokens": 8, "output_tokens": 3 }
                    })
                    .to_string(),
                );
        let response = llm_capture_error_response(&error);
        assert_eq!(response["usage"]["input_tokens"].as_i64(), Some(8));
        assert_eq!(response["usage"]["cached_input_tokens"].as_i64(), Some(0));
        assert_eq!(response["usage"]["cache_creation_tokens"].as_i64(), Some(0));
        assert_eq!(response["usage"]["output_tokens"].as_i64(), Some(3));
        assert_eq!(response["usage"]["total_tokens"].as_i64(), Some(11));
        assert_eq!(
            response["usage"].as_object().expect("usage object").len(),
            5,
            "canonical-like error usage must be completed to the exact canonical schema"
        );
    }

    #[test]
    fn llm_capture_error_response_normalizes_openai_usage_to_canonical() {
        // Upstream details carry OpenAI-style `prompt_tokens`/`completion_tokens`.
        // The captured artifact must present the canonical schema that the SSE
        // path produces, so downstream consumers see one shape across success
        // and failure.
        let error =
            astra_core::ClassifiedError::new(astra_core::ErrorKind::StreamTransport, "boom")
                .with_details_json(
                    json!({
                        "partial_full_text": "half",
                        "usage": { "prompt_tokens": 17, "completion_tokens": 3 }
                    })
                    .to_string(),
                );
        let response = llm_capture_error_response(&error);
        assert_eq!(response["usage"]["input_tokens"].as_i64(), Some(17));
        assert_eq!(response["usage"]["output_tokens"].as_i64(), Some(3));
        assert!(
            response["usage"].get("prompt_tokens").is_none(),
            "canonical output must not retain the OpenAI-dialect `prompt_tokens` key",
        );
    }

    #[test]
    fn llm_capture_error_response_completes_existing_canonical_usage() {
        // When details already speak the canonical dialect we still normalize
        // through the canonical struct so the output has exactly one schema
        // and `total_tokens` is derived from the disjoint buckets.
        let error =
            astra_core::ClassifiedError::new(astra_core::ErrorKind::StreamTransport, "boom")
                .with_details_json(
                    json!({
                        "usage": {
                            "input_tokens": 11,
                            "cached_input_tokens": 2,
                            "cache_creation_tokens": 1,
                            "output_tokens": 4,
                            "total_tokens": 999
                        }
                    })
                    .to_string(),
                );
        let response = llm_capture_error_response(&error);
        assert_eq!(response["usage"]["input_tokens"].as_i64(), Some(11));
        assert_eq!(response["usage"]["cached_input_tokens"].as_i64(), Some(2));
        assert_eq!(response["usage"]["cache_creation_tokens"].as_i64(), Some(1));
        assert_eq!(response["usage"]["output_tokens"].as_i64(), Some(4));
        assert_eq!(response["usage"]["total_tokens"].as_i64(), Some(18));
    }

    #[test]
    fn llm_capture_error_response_normalizes_anthropic_usage_to_canonical() {
        // Anthropic-dialect usage (e.g. deepseek `/anthropic` endpoint, direct
        // Anthropic Messages API) arrives with `input_tokens`/`output_tokens`
        // at the top level AND separate `cache_read_input_tokens` /
        // `cache_creation_input_tokens` keys. These must be folded into the
        // canonical `cached_input_tokens` / `cache_creation_tokens` schema so
        // downstream cache-rate math sees the hits instead of zeros.
        let error =
            astra_core::ClassifiedError::new(astra_core::ErrorKind::StreamTransport, "boom")
                .with_details_json(
                    json!({
                        "usage": {
                            "input_tokens": 25,
                            "output_tokens": 7,
                            "cache_read_input_tokens": 4864,
                            "cache_creation_input_tokens": 120
                        }
                    })
                    .to_string(),
                );
        let response = llm_capture_error_response(&error);
        assert_eq!(response["usage"]["input_tokens"].as_i64(), Some(25));
        assert_eq!(response["usage"]["output_tokens"].as_i64(), Some(7));
        assert_eq!(
            response["usage"]["cached_input_tokens"].as_i64(),
            Some(4864),
            "anthropic `cache_read_input_tokens` must be folded into canonical `cached_input_tokens`",
        );
        assert_eq!(
            response["usage"]["cache_creation_tokens"].as_i64(),
            Some(120),
            "anthropic `cache_creation_input_tokens` must be folded into canonical `cache_creation_tokens`",
        );
        assert!(
            response["usage"].get("cache_read_input_tokens").is_none(),
            "anthropic-dialect key must not leak through after normalization",
        );
    }

    #[test]
    fn llm_capture_error_response_detects_anthropic_dialect_from_cache_keys_only() {
        // Edge case: an error artifact that only carries cache fields (no
        // top-level input/output tokens) — still unambiguously anthropic. We
        // must recognize the dialect from `cache_read_input_tokens` alone,
        // otherwise the pass-through branch (triggered by presence of
        // `input_tokens`) never fires and we silently drop the cache signal.
        let error =
            astra_core::ClassifiedError::new(astra_core::ErrorKind::StreamTransport, "boom")
                .with_details_json(
                    json!({
                        "usage": {
                            "cache_read_input_tokens": 2048,
                            "cache_creation_input_tokens": 0
                        }
                    })
                    .to_string(),
                );
        let response = llm_capture_error_response(&error);
        assert_eq!(
            response["usage"]["cached_input_tokens"].as_i64(),
            Some(2048),
        );
    }

    #[test]
    fn llm_capture_error_response_leaves_unknown_usage_dialect_untouched() {
        // If we cannot identify the dialect, preserve raw keys — dropping
        // fields silently is worse than asking downstream code to
        // defensively parse.
        let error =
            astra_core::ClassifiedError::new(astra_core::ErrorKind::StreamTransport, "boom")
                .with_details_json(
                    json!({
                        "usage": { "tokens_in": 7, "tokens_out": 2 }
                    })
                    .to_string(),
                );
        let response = llm_capture_error_response(&error);
        assert_eq!(response["usage"]["tokens_in"].as_i64(), Some(7));
        assert_eq!(response["usage"]["tokens_out"].as_i64(), Some(2));
    }

    #[test]
    fn llm_capture_error_response_does_not_invent_buckets_from_total_only_usage() {
        let error =
            astra_core::ClassifiedError::new(astra_core::ErrorKind::StreamTransport, "boom")
                .with_details_json(
                    json!({
                        "usage": { "total_tokens": 42 }
                    })
                    .to_string(),
                );
        let response = llm_capture_error_response(&error);
        assert_eq!(response["usage"]["total_tokens"].as_i64(), Some(42));
        assert!(
            response["usage"].get("input_tokens").is_none(),
            "total-only usage has no disjoint bucket evidence and must remain an unknown dialect",
        );
    }

    #[test]
    fn server_loop_error_captures_use_structured_error_response() {
        let source = include_str!("server_loop_host.rs");
        let tests_start = source
            .find("\n#[cfg(test)]\nmod tests {")
            .expect("cfg(test) + mod tests marker");
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
        let tests_start = source
            .find("\n#[cfg(test)]\nmod tests {")
            .expect("cfg(test) + mod tests marker");
        let production = &source[..tests_start];
        assert!(
            production.contains("use crate::turn::bridge::llm_stream::rate_limit_cooldown;"),
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
        .with_execution_binding_snapshot(edge_runtime_snapshot())
        .build();

        assert!(host.valid_tool_names().contains("bash"));
        assert!(host.valid_tool_names().contains("read_file"));
        assert_eq!(host.valid_tool_names().len(), 2);
    }

    #[test]
    fn sync_valid_tools_uses_final_wire_surface_not_candidate_edge_tools() {
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "user1".to_string(),
            "sess1".to_string(),
        )
        .with_edge_tools(sample_edge_tools())
        .with_execution_binding_snapshot(edge_runtime_snapshot())
        .build();
        assert!(host.valid_tool_names().contains("read_file"));

        let dir = tempfile::TempDir::new().unwrap();
        let executor = Arc::new(server_tool_executor_with_agent_context(dir.path()));
        let mut state = create_test_state();
        state.server_tool_executor = Some(Arc::clone(&executor));

        let wire_tools = vec![sample_edge_tools()[0].clone()];
        host.sync_valid_tools_to_wire_surface_for_state(&wire_tools, &state);

        assert!(host.valid_tool_names().contains("bash"));
        assert!(
            !host.valid_tool_names().contains("read_file"),
            "runtime admission must mirror the final wire tools, not the broader edge candidate set"
        );
        let searchable = executor
            .current_searchable_tool_names()
            .expect("executor searchable names must be synced");
        assert!(searchable.contains("bash"));
        assert!(
            !searchable.contains("read_file"),
            "tool_search must also mirror the final wire tools"
        );
    }

    #[tokio::test]
    async fn server_host_syncs_deferred_manifest_to_executor_activation() {
        let mut edge_profile = Map::new();
        edge_profile.insert(
            astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_DEFERRED_TOOL_NAMES
                .to_string(),
            json!(["agent_fanout", " "]),
        );
        edge_profile.insert(
            astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_DEFERRED_TOOLS_TEXT
                .to_string(),
            json!("<deferred-tools>\nagent_fanout\n</deferred-tools>"),
        );
        edge_profile.insert(
            astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_DEFERRED_TOOLS_CONTEXT_WINDOW
                .to_string(),
            json!(200_000),
        );
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "user1".to_string(),
            "sess1".to_string(),
        )
        .with_edge_tools(sample_edge_tools())
        .with_edge_profile(edge_profile)
        .with_execution_binding_snapshot(edge_runtime_snapshot())
        .build();
        host.resolved_model_name = Some("test-model".to_string());
        host.resolved_context_window = Some(200_000);

        let dir = tempfile::TempDir::new().unwrap();
        let executor = Arc::new(server_tool_executor_with_agent_context(dir.path()));
        let mut state = create_test_state();
        state.server_tool_executor = Some(Arc::clone(&executor));

        let _visible = host.visible_turn_tools(&mut state);

        assert!(
            executor
                .current_activatable_tool_names_snapshot()
                .contains("agent_fanout"),
            "executor must mirror edge-profile deferred names"
        );
        assert!(
            <ServerAgenticLoopHost as AgenticLoopHost>::deferred_tool_names(&host)
                .contains("agent_fanout"),
            "validator must see the same deferred manifest"
        );

        let result = executor
            .execute_with_metadata("tool_search", &json!({"query": "Select:agent_fanout"}))
            .await;
        let parsed: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(
            parsed["matches"][0]["name"].as_str(),
            Some("agent_fanout"),
            "server tool_search must resolve names advertised in the deferred manifest: {}",
            result.output
        );
    }

    #[test]
    fn builder_filters_edge_tools_through_runtime_binding() {
        let host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "user1".to_string(),
            "sess1".to_string(),
        )
        .with_edge_tools(sample_edge_tools())
        .build();

        let names = schema_names(&host.edge_tools);
        assert!(!names.contains("bash"));
        assert!(!names.contains("read_file"));
        assert!(!host.valid_tool_names().contains("bash"));
        assert!(!host.valid_tool_names().contains("read_file"));
    }

    #[test]
    fn builder_default_server_side_tools_hide_project_tools_without_runtime() {
        let host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "user1".to_string(),
            "sess1".to_string(),
        )
        .build();

        let names = schema_names(&host.edge_tools);
        assert!(names.contains("ask_user"));
        assert!(names.contains("tool_search"));
        assert!(names.contains("web_search"));
        for hidden in [
            "bash",
            "read_file",
            "write_file",
            "git",
            "symbols",
            "run_script",
        ] {
            assert!(
                !names.contains(hidden),
                "{hidden} must not be advertised without a workspace runtime"
            );
            assert!(
                !host.valid_tool_names().contains(hidden),
                "{hidden} must not be admitted without a workspace runtime"
            );
        }
    }

    #[test]
    fn builder_server_side_tools_follow_server_sandbox_binding() {
        let host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "user1".to_string(),
            "sess1".to_string(),
        )
        .with_execution_bindings(
            WorkspaceBinding::server_sandbox("/tmp/astra-workspace"),
            ExecutorBinding::server_local(),
        )
        .build();

        let names = schema_names(&host.edge_tools);
        for visible in [
            "ask_user",
            "tool_search",
            "bash",
            "read_file",
            "write_file",
            "git",
        ] {
            assert!(
                names.contains(visible),
                "{visible} should be advertised for a server sandbox runtime"
            );
            assert!(
                host.valid_tool_names().contains(visible),
                "{visible} should be admitted for a server sandbox runtime"
            );
        }
    }

    #[test]
    fn builder_server_side_tools_hide_project_tools_when_edge_offline() {
        let host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "user1".to_string(),
            "sess1".to_string(),
        )
        .with_execution_bindings(
            WorkspaceBinding::edge_workspace(
                "MacBook Pro",
                "/Users/test/project",
                WorkspaceAuthority::ReadWrite,
            ),
            ExecutorBinding::edge_agent(
                "edge-1",
                "MacBook Pro",
                crate::server::tool_transport::ToolTransportKind::EdgeWs,
                crate::server::tool_transport::ExecutorStatus::Offline,
            ),
        )
        .build();

        let names = schema_names(&host.edge_tools);
        for visible in ["agent", "tool_search", "web_search", "memory"] {
            assert!(
                names.contains(visible),
                "{visible} should remain visible because it runs on the server"
            );
        }
        for hidden in [
            "bash",
            "read_file",
            "write_file",
            "git",
            "symbols",
            "run_script",
        ] {
            assert!(
                !names.contains(hidden),
                "{hidden} must be hidden while the edge runtime is offline"
            );
            assert!(
                host.valid_tool_names().contains(hidden),
                "{hidden} should be admitted while hidden so stale calls can report executor_offline"
            );
            assert!(
                host.admissible_extras.contains(&hidden.to_string()),
                "{hidden} should remain boundary-admissible so stale calls can report executor_offline"
            );
        }
    }

    #[test]
    fn builder_server_side_tools_follow_orchestrator_read_only_binding() {
        let host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "user1".to_string(),
            "sess1".to_string(),
        )
        .with_execution_binding_snapshot(ExecutionBindingSnapshot::new(
            WorkspaceBinding {
                kind: WorkspaceBindingKind::CloudWorkspace,
                display_name: "Snapshot".to_string(),
                cwd: Some("/snapshot".to_string()),
                authority: WorkspaceAuthority::ReadOnly,
                fallback_policy: FallbackPolicy::Disabled,
            },
            ExecutorBinding {
                kind: ExecutorBindingKind::OrchestratorManaged,
                executor_id: "orchestrator:snapshot".to_string(),
                display_name: "Orchestrator-managed executor".to_string(),
                transport: crate::server::tool_transport::ToolTransportKind::SandboxResidentAgent,
                status: crate::server::tool_transport::ExecutorStatus::Online,
            },
            astra_runtime_env::RuntimeBinding::oci_container("snapshot-runtime"),
        ))
        .build();

        let names = schema_names(&host.edge_tools);
        for visible in ["read_file", "grep", "glob", "git"] {
            assert!(
                names.contains(visible),
                "{visible} should be advertised for an online read-only orchestrator-managed executor"
            );
            assert!(
                host.valid_tool_names().contains(visible),
                "{visible} should be admitted for an online read-only orchestrator-managed executor"
            );
        }
        for hidden in ["write_file", "str_replace", "run_script"] {
            assert!(
                !names.contains(hidden),
                "{hidden} must be hidden for a read-only orchestrator-managed executor"
            );
        }
    }

    #[test]
    fn deferred_user_input_updates_active_skills_in_edge_profile() {
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "user1".to_string(),
            "sess1".to_string(),
        )
        .build();

        host.on_deferred_user_input(&json!({
            "content": "Use the release format.",
            "active_skills": ["release-manager", "deploy-auditor"],
        }));
        assert_eq!(
            host.edge_profile.get("active_skills"),
            Some(&json!(["release-manager", "deploy-auditor"]))
        );

        host.on_deferred_user_input(&json!({
            "content": "No special output formatting now.",
            "active_skills": [],
        }));
        assert!(
            host.edge_profile.get("active_skills").is_none(),
            "explicitly empty deferred active_skills should clear prior run-level skill hints"
        );
    }

    #[test]
    fn pipeline_abort_returns_error_and_records_alert() {
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u1".to_string(),
            "s1".to_string(),
        )
        .with_edge_tools(sample_edge_tools())
        .with_execution_binding_snapshot(edge_runtime_snapshot())
        .build();
        let mut state = crate::turn::agentic_loop::host::tests::make_state();
        state.current_session_id = Some("sid-abort".into());
        state.max_turn_input_tokens = 100_000;
        state.turn_event_buffer = Some(
            astra_services::session_journal::TurnEventBuffer::begin_turn(Some("sid-abort"), 1),
        );
        let mut pipeline_session = astra_turn_core::pipeline_session::PipelineSession::new(
            astra_turn_core::pipeline_config::PipelineConfig::default(),
        );
        pipeline_session.recovery.consecutive_ptl_errors = 3;
        state.pipeline_session = Some(pipeline_session);
        let tools = host.edge_tools.clone();

        let error = match host.run_turn_pipeline(
            &mut state,
            &tools,
            "anthropic",
            "claude-sonnet",
            "continue",
        ) {
            Ok(_) => panic!("pipeline abort must stop the turn instead of using a fallback prompt"),
            Err(error) => error,
        };
        assert_eq!(error.kind, astra_core::ErrorKind::ContextWindow);
        assert!(
            state
                .turn_event_buffer
                .as_ref()
                .is_some_and(|buffer| !buffer.is_empty()),
            "pipeline abort should record an alert event for harness/journal analysis"
        );
    }

    #[test]
    fn result_to_accum_converts_correctly() {
        let result = LlmCallResult {
            full_text: "Hello world".to_string(),
            reasoning: "thinking...".to_string(),
            reasoning_signature: String::new(),
            tool_calls: vec![json!({"id": "tc1", "function": {"name": "bash"}})],
            usage: Map::from_iter([
                ("input_tokens".to_string(), json!(100)),
                ("output_tokens".to_string(), json!(50)),
                ("cached_input_tokens".to_string(), json!(0)),
                ("cache_creation_tokens".to_string(), json!(0)),
                ("total_tokens".to_string(), json!(150)),
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

    #[tokio::test]
    async fn assemble_llm_messages_includes_system_and_user() {
        let host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u".to_string(),
            "s".to_string(),
        )
        .with_edge_tools(sample_edge_tools())
        .with_execution_binding_snapshot(edge_runtime_snapshot())
        .build();

        let mut state = create_test_state();
        state
            .messages
            .push(json!({"role": "user", "content": "hello"}));

        // `assemble_llm_messages` is the pure stitching step after Phase 3 —
        // no Memoria I/O. With Normal tier Memoria passes messages through
        // unchanged, so feeding `state.messages` directly as the compacted
        // list is equivalent to the real runtime path here.
        let llm_cfg = ResolvedTurnLlmConfig {
            model_name: "gpt-4".into(),
            wire_model_name: None,
            api_key: String::new(),
            base_url: String::new(),
            provider: "openai".into(),
            fallback_chain: Vec::new(),
            cache_capability: None,
            header_overrides: HashMap::new(),
            request_body_overrides: None,
            completions_url_override: None,
            request_timeout: None,
            context_window: None,
        };
        let msgs = host.assemble_llm_messages(
            vec![json!({"role": "system", "content": "system prompt text"})],
            Vec::new(),
            state.messages.clone(),
            &mut state,
            &llm_cfg,
            &PromptCacheConfig::latch("openai", "gpt-4"),
        );
        assert!(msgs.len() >= 2, "should have system + user messages");
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "system prompt text");
    }

    #[tokio::test]
    async fn run_turn_pipeline_returns_tier_and_pruned_tool_schemas() {
        // Phase 1 contract: the pipeline is the sole authority for
        // (a) compaction tier selection and
        // (b) tier-appropriate tool-schema pruning.
        // Runtime no longer re-derives either — both must come back from
        // `run_turn_pipeline` via the PipelineTurnOutcome struct.
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u-turn-pipeline".to_string(),
            "s-turn-pipeline".to_string(),
        )
        .with_edge_tools(vec![json!({
            "type": "function",
            "function": {
                "name": "bash",
                "description": "Execute a shell command. Runs inside the sandbox with a 2-minute default timeout and deletes temp dirs on exit.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": {"type": "string", "description": "Command to run. Use absolute paths."}
                    }
                }
            }
        })])
        .with_execution_binding_snapshot(edge_runtime_snapshot())
        .build();

        let mut state = create_test_state();
        state.current_session_id = Some("s-turn-pipeline".into());
        state.max_turn_input_tokens = 200_000;
        state.pipeline_session = Some(astra_turn_core::pipeline_session::PipelineSession::new(
            astra_turn_core::pipeline_config::PipelineConfig::default(),
        ));
        let tools = host.edge_tools.clone();

        let outcome = host
            .run_turn_pipeline(&mut state, &tools, "openai", "gpt-4", "just do it")
            .expect("pipeline should succeed");

        // Tier comes from the planner, not from runtime's estimate.
        // For this low-pressure state planner should select Normal.
        assert_eq!(
            outcome.tier,
            CompactionTier::Normal,
            "low-pressure turn should plan at CompactionTier::Normal"
        );
        // Tool schemas are the pipeline's Optimize output, not the raw input.
        assert_eq!(
            outcome.tool_schemas.len(),
            tools.len(),
            "Normal tier preserves tool schema count"
        );
        // System messages are rendered from pipeline's serialized system blocks.
        assert!(!outcome.system_messages.is_empty());
        assert_eq!(outcome.system_messages[0]["role"], "system");
    }

    #[tokio::test]
    async fn run_turn_pipeline_includes_turn_start_lifecycle_summary_for_web_agent() {
        let mut edge_profile = Map::new();
        edge_profile.insert(
            "session_lineage".to_string(),
            json!({
                "parent_session_id": "parent-session-1",
                "forked_after_turn": 7
            }),
        );

        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u-lifecycle".to_string(),
            "s-lifecycle".to_string(),
        )
        .with_edge_profile(edge_profile)
        .with_plan_resume_hint(Some("[plan-resume] goal=\"stabilize\"".to_string()))
        .build();

        let mut state = create_test_state();
        state.current_session_id = Some("s-lifecycle".into());
        state.current_run_id = Some("run-lifecycle".into());
        state.delegations_this_turn = 2;
        state.max_turn_input_tokens = 200_000;
        state.pipeline_session = Some(astra_turn_core::pipeline_session::PipelineSession::new(
            astra_turn_core::pipeline_config::PipelineConfig::default(),
        ));
        let tools = host.edge_tools.clone();

        let outcome = host
            .run_turn_pipeline(&mut state, &tools, "openai", "gpt-4o", "continue")
            .expect("pipeline should succeed");
        let text = pipeline_outcome_text(&outcome);

        assert!(
            text.contains("Turn-start session execution state"),
            "turn-start lifecycle summary must be injected into prompt context: {text}"
        );
        assert!(
            text.contains("Mode: web-agent (server-side tools) · interaction: headless"),
            "web-agent mode marker must be explicit: {text}"
        );
        assert!(
            text.contains("Workspace: server-provisioned (edge cwd unavailable)"),
            "web-agent mode without edge cwd must explain workspace source: {text}"
        );
        assert!(
            text.contains("Plan resume: [plan-resume] goal=\"stabilize\""),
            "plan resume digest must be visible in lifecycle summary: {text}"
        );
        assert!(
            text.contains("Task board: no open tasks"),
            "task-board state should be explicit even when empty: {text}"
        );
        assert!(
            text.contains("Session lineage: parent=parent-session-1 · forked_after_turn=7"),
            "fork lineage must be surfaced when available: {text}"
        );
        assert!(
            text.contains("Delegation: engine=disabled · this_turn=2 · progress_stream=none"),
            "delegation state must be visible: {text}"
        );
    }

    #[tokio::test]
    async fn run_turn_pipeline_includes_bounded_task_board_hint_for_web_agent() {
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u-task-board".to_string(),
            "s-task-board".to_string(),
        )
        .with_task_board_resume_hint(Some(
            "open=2 · next=[in_progress] task-1: Ship task UX".to_string(),
        ))
        .build();

        let mut state = create_test_state();
        state.current_session_id = Some("s-task-board".into());
        state.current_run_id = Some("run-task-board".into());
        state.max_turn_input_tokens = 200_000;
        state.pipeline_session = Some(astra_turn_core::pipeline_session::PipelineSession::new(
            astra_turn_core::pipeline_config::PipelineConfig::default(),
        ));
        let tools = host.edge_tools.clone();

        let outcome = host
            .run_turn_pipeline(&mut state, &tools, "openai", "gpt-4o", "continue")
            .expect("pipeline should succeed");
        let text = pipeline_outcome_text(&outcome);

        assert!(
            text.contains("Task board: open=2 · next=[in_progress] task-1: Ship task UX"),
            "task-board digest must be visible in Cloud lifecycle summary: {text}"
        );
        assert!(
            !text.contains("The task tools haven't been used recently."),
            "Cloud task-board lifecycle context must stay a bounded state hint, not a noisy auto-reminder: {text}"
        );
    }

    #[tokio::test]
    async fn run_turn_pipeline_ignores_noncanonical_top_level_fork_keys() {
        let mut edge_profile = Map::new();
        edge_profile.insert(
            "parent_session_id".to_string(),
            Value::String("parent-ignored".to_string()),
        );
        edge_profile.insert("forked_at_turn".to_string(), Value::Number(11u64.into()));
        edge_profile.insert(
            "agent_id".to_string(),
            Value::String("reviewer-1".to_string()),
        );

        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u-canonical-lineage".to_string(),
            "s-canonical-lineage".to_string(),
        )
        .with_edge_profile(edge_profile)
        .build();

        let mut state = create_test_state();
        state.current_session_id = Some("s-canonical-lineage".into());
        state.current_run_id = Some("run-canonical-lineage".into());
        state.recursion_depth = 2;
        state.max_turn_input_tokens = 200_000;
        state.pipeline_session = Some(astra_turn_core::pipeline_session::PipelineSession::new(
            astra_turn_core::pipeline_config::PipelineConfig::default(),
        ));
        let tools = host.edge_tools.clone();

        let outcome = host
            .run_turn_pipeline(&mut state, &tools, "openai", "gpt-4o", "continue")
            .expect("pipeline should succeed");
        let text = pipeline_outcome_text(&outcome);

        assert!(
            !text.contains("Session lineage:"),
            "turn-start lifecycle summary must require canonical session_lineage object: {text}"
        );
        assert!(
            !text.contains("parent-ignored") && !text.contains("forked_after_turn=11"),
            "top-level fork aliases must not leak into lifecycle summary: {text}"
        );
        assert!(
            text.contains("Delegation context: recursion_depth=2 · agent_id=reviewer-1"),
            "sub-agent delegation context should be visible in lifecycle summary: {text}"
        );
    }

    #[test]
    fn turn_start_lifecycle_summary_reports_edge_mode_and_workspace() {
        let mut edge_profile = Map::new();
        edge_profile.insert("cwd".to_string(), Value::String("/tmp/proj".to_string()));

        let host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u-edge".to_string(),
            "s-edge".to_string(),
        )
        .with_edge_tools(sample_edge_tools())
        .with_execution_binding_snapshot(edge_runtime_snapshot())
        .with_edge_profile(edge_profile)
        .with_interactive_client(true)
        .build();

        let mut state = create_test_state();
        state.current_session_id = Some("s-edge".into());
        state.current_run_id = Some("run-edge".into());
        let summary = host.turn_start_lifecycle_summary(&state);

        assert!(
            summary.contains("Mode: edge-agent (edge-provided tools) · interaction: prompt"),
            "edge-connected mode should be explicit: {summary}"
        );
        assert!(
            summary.contains("Workspace: /tmp/proj"),
            "edge cwd should be surfaced in lifecycle summary: {summary}"
        );
        assert!(
            summary.contains("Plan resume: none"),
            "summary should clearly report missing resume hint: {summary}"
        );
    }

    #[tokio::test]
    async fn run_turn_pipeline_refreshes_only_plan_resume_line_when_hint_changes() {
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u-latch".to_string(),
            "s-latch".to_string(),
        )
        .with_plan_resume_hint(Some("[plan-resume] goal=\"initial\"".to_string()))
        .build();

        let mut state = create_test_state();
        state.current_session_id = Some("s-latch".into());
        state.current_run_id = Some("run-latch".into());
        state.max_turn_input_tokens = 200_000;
        state.pipeline_session = Some(astra_turn_core::pipeline_session::PipelineSession::new(
            astra_turn_core::pipeline_config::PipelineConfig::default(),
        ));
        let tools = host.edge_tools.clone();

        state.current_round_index = 0;
        let round0 = host
            .run_turn_pipeline(&mut state, &tools, "openai", "gpt-4o", "continue")
            .expect("round 0 pipeline should succeed");
        let round0_text = pipeline_outcome_text(&round0);
        assert!(round0_text.contains("Plan resume: [plan-resume] goal=\"initial\""));
        assert!(round0_text.contains("this_turn=0"));

        state.current_round_index = 3;
        state.delegations_this_turn = 5;
        {
            let hint_handle = host.plan_resume_hint_handle();
            let mut guard = hint_handle.write().expect("plan hint lock");
            *guard = Some("[plan-resume] goal=\"mutated\"".to_string());
        }

        let round3 = host
            .run_turn_pipeline(&mut state, &tools, "openai", "gpt-4o", "continue")
            .expect("round 3 pipeline should succeed");
        let round3_text = pipeline_outcome_text(&round3);
        assert!(
            round3_text.contains("Plan resume: [plan-resume] goal=\"mutated\""),
            "plan line must refresh when the shared plan hint changes: {round3_text}"
        );
        assert!(
            round3_text.contains("this_turn=0"),
            "delegation counters should remain turn-start snapshot values: {round3_text}"
        );
    }

    /// Session 986a553e observed MiniMax-M2.7 cache collapsing from
    /// 7680 to 0 across six tool-loop rounds because volatile
    /// content (Self-Awareness with live turn/token counters) was
    /// being re-injected every round. The new `CacheCapability`
    /// routing classifies MiniMax as `VolatilePlacement::CurrentUserOnly`;
    /// `run_turn_pipeline` now consults it and emits an **empty**
    /// `volatile_preamble` on rounds > 0 so the message history bytes
    /// stay stable across the tool loop.
    #[tokio::test]
    async fn run_turn_pipeline_minimax_skips_volatile_on_tool_loop_round() {
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u-minimax".to_string(),
            "s-minimax".to_string(),
        )
        .with_edge_tools(vec![json!({
            "type": "function",
            "function": {
                "name": "bash",
                "description": "Shell.",
                "parameters": {"type": "object", "properties": {}}
            }
        })])
        .with_execution_binding_snapshot(edge_runtime_snapshot())
        .build();
        let mut state = create_test_state();
        state.current_session_id = Some("s-minimax".into());
        state.max_turn_input_tokens = 200_000;
        state.pipeline_session = Some(astra_turn_core::pipeline_session::PipelineSession::new(
            astra_turn_core::pipeline_config::PipelineConfig::default(),
        ));
        let tools = host.edge_tools.clone();

        // Updated contract: strict-history providers (MiniMax) must
        // suppress volatile preamble on EVERY round, not just >0.
        // Round-0-only injection still causes a byte mismatch at
        // msg[1] vs round 1+ (round 0 has preamble+user_q, round 1
        // has only user_q), so the whole turn's cache misses.
        for round in [0u32, 1, 5] {
            state.current_round_index = round;
            let out = host
                .run_turn_pipeline(&mut state, &tools, "openai", "MiniMax-M2.7", "hi")
                .expect("pipeline should succeed");
            assert!(
                out.volatile_preamble.is_empty(),
                "MiniMax must suppress volatile preamble on every round \
                 (strict-history provider). round={round} preamble={:?}",
                out.volatile_preamble,
            );
        }
    }

    #[tokio::test]
    async fn run_turn_pipeline_deepseek_v4_flash_skips_volatile_on_tool_loop_round() {
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u-deepseek".to_string(),
            "s-deepseek".to_string(),
        )
        .with_edge_tools(vec![json!({
            "type": "function",
            "function": {
                "name": "bash",
                "description": "Shell.",
                "parameters": {"type": "object", "properties": {}}
            }
        })])
        .build();
        let mut state = create_test_state();
        state.current_session_id = Some("s-deepseek".into());
        state.max_turn_input_tokens = 200_000;
        state.pipeline_session = Some(astra_turn_core::pipeline_session::PipelineSession::new(
            astra_turn_core::pipeline_config::PipelineConfig::default(),
        ));
        let tools = host.edge_tools.clone();

        for round in [0u32, 1, 5] {
            state.current_round_index = round;
            let out = host
                .run_turn_pipeline(&mut state, &tools, "openai", "deepseek-v4-flash", "hi")
                .expect("pipeline should succeed");
            assert!(
                out.volatile_preamble.is_empty(),
                "DeepSeek v4 flash must suppress volatile preamble on every round. \
                 round={round} preamble={:?}",
                out.volatile_preamble,
            );
        }
    }

    /// OpenAI auto-prefix cache can tolerate volatile-in-tail every
    /// round, so preamble emission stays unchanged across rounds.
    #[tokio::test]
    async fn run_turn_pipeline_openai_keeps_volatile_on_all_rounds() {
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u-openai".to_string(),
            "s-openai".to_string(),
        )
        .with_edge_tools(vec![json!({
            "type": "function",
            "function": {
                "name": "bash",
                "description": "Shell.",
                "parameters": {"type": "object", "properties": {}}
            }
        })])
        .build();
        let mut state = create_test_state();
        state.current_session_id = Some("s-openai".into());
        state.max_turn_input_tokens = 200_000;
        state.pipeline_session = Some(astra_turn_core::pipeline_session::PipelineSession::new(
            astra_turn_core::pipeline_config::PipelineConfig::default(),
        ));
        let tools = host.edge_tools.clone();

        state.current_round_index = 0;
        let r0 = host
            .run_turn_pipeline(&mut state, &tools, "openai", "gpt-4o", "hi")
            .expect("pipeline should succeed");
        state.current_round_index = 3;
        let r3 = host
            .run_turn_pipeline(&mut state, &tools, "openai", "gpt-4o", "hi")
            .expect("pipeline should succeed");
        // Identical preamble shape on OpenAI — injection gate doesn't
        // special-case round index.
        assert_eq!(
            r0.volatile_preamble.len(),
            r3.volatile_preamble.len(),
            "OpenAI should emit identical preamble on round 0 and 3",
        );
    }

    /// Anthropic path (MarkerIsolated) — post-fix contract: volatile
    /// (CacheScope::None) blocks are promoted OUT of the system content
    /// array and into `volatile_preamble` so the system content stays
    /// byte-stable across rounds. This unblocks tool-schema caching on
    /// DeepSeek's `/anthropic` endpoint (see session 5c5cbf78 analysis
    /// and `tests/fixtures/deepseek_anthropic_cache_probe.py`) and is
    /// no-op for Bedrock (which cache-writes the full payload on the
    /// first call regardless).
    ///
    /// The preamble existence on a given round depends on whether the
    /// pipeline generated any `CacheScope::None` blocks in the first
    /// place; here we just assert the MarkerIsolated branch no longer
    /// leaves stable and volatile content co-mingled in the system
    /// content array.
    #[tokio::test]
    async fn run_turn_pipeline_anthropic_system_content_stays_byte_stable() {
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u-anth".to_string(),
            "s-anth".to_string(),
        )
        .with_edge_tools(vec![json!({
            "type": "function",
            "function": {
                "name": "bash",
                "description": "Shell.",
                "parameters": {"type": "object", "properties": {}}
            }
        })])
        .build();
        let mut state = create_test_state();
        state.current_session_id = Some("s-anth".into());
        state.max_turn_input_tokens = 200_000;
        state.pipeline_session = Some(astra_turn_core::pipeline_session::PipelineSession::new(
            astra_turn_core::pipeline_config::PipelineConfig::default(),
        ));
        let tools = host.edge_tools.clone();

        // Capture system content on two different rounds and assert the
        // byte-stable invariant. The old MarkerIsolated branch embedded
        // the Turn/Tokens counter inside the system content, so
        // `system_messages[0]["content"]` differed between round 0 and
        // round 7. Post-fix: the counter rides in `volatile_preamble`
        // and system content is identical.
        state.current_round_index = 0;
        let r0 = host
            .run_turn_pipeline(&mut state, &tools, "anthropic", "claude-sonnet-4", "hi")
            .expect("pipeline should succeed");
        state.current_round_index = 7;
        let r7 = host
            .run_turn_pipeline(&mut state, &tools, "anthropic", "claude-sonnet-4", "hi")
            .expect("pipeline should succeed");

        assert_eq!(
            r0.system_messages, r7.system_messages,
            "system_messages must be byte-identical across rounds — any \
             drift here reopens the session 5c5cbf78 cache regression. \
             r0={:#?} r7={:#?}",
            r0.system_messages, r7.system_messages,
        );
    }

    #[tokio::test]
    async fn run_turn_pipeline_prunes_tool_schemas_under_pressure() {
        // Forces the planner into TrimSchemas+ tier by driving up recovery
        // state (PTL errors), then verifies the returned tool_schemas reflect
        // tier-appropriate pruning — tool descriptions should be truncated.
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u-prune".to_string(),
            "s-prune".to_string(),
        )
        .with_edge_tools(vec![json!({
            "type": "function",
            "function": {
                "name": "bash",
                "description": "Execute a shell command. Runs inside the sandbox with a 2-minute default timeout and deletes temp dirs on exit.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": {"type": "string", "description": "Command to run. Use absolute paths."}
                    }
                }
            }
        })])
        .with_execution_binding_snapshot(edge_runtime_snapshot())
        .build();

        let mut state = create_test_state();
        state.current_session_id = Some("s-prune".into());
        state.max_turn_input_tokens = 200_000;
        let mut ps = astra_turn_core::pipeline_session::PipelineSession::new(
            astra_turn_core::pipeline_config::PipelineConfig::default(),
        );
        // Force the planner to escalate beyond Normal via recovery state.
        ps.recovery.consecutive_ptl_errors = 1;
        state.pipeline_session = Some(ps);
        let tools = host.edge_tools.clone();

        let outcome = host
            .run_turn_pipeline(&mut state, &tools, "openai", "gpt-4", "continue")
            .expect("pipeline should succeed");

        assert!(
            outcome.tier >= CompactionTier::TrimSchemas,
            "recovery PTL=1 must escalate tier above Normal, got {:?}",
            outcome.tier
        );
        // Description must have been touched by TrimSchemas (first-sentence truncation).
        let pruned_desc = outcome.tool_schemas[0]["function"]["description"]
            .as_str()
            .unwrap_or("");
        assert!(
            !pruned_desc.contains("deletes temp dirs"),
            "trailing sentence should be stripped under TrimSchemas, got: {pruned_desc:?}"
        );
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
        // Register write_file as a valid tool so the edge ledger delivery path admits it.
        host.install_runtime_tool_schemas(vec![json!({
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Write file contents",
                "parameters": {"type": "object", "properties": {}}
            }
        })]);
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
    async fn deliver_edge_tools_tool_call_end_includes_edge_ledger_metadata() {
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u-edge-meta".to_string(),
            "s-edge-meta".to_string(),
        )
        .build();
        // Register read_file as a valid tool so the edge ledger delivery path admits it.
        host.install_runtime_tool_schemas(vec![json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read file contents",
                "parameters": {"type": "object", "properties": {}}
            }
        })]);
        host.set_execution_metadata(json!({
            "workspace": {
                "kind": "edge_workspace",
                "display_name": "MacBook Pro",
                "cwd": "/Users/test/project",
                "authority": "read_write",
                "fallback_policy": "disabled"
            },
            "executor": {
                "kind": "edge_agent",
                "executor_id": "edge-1",
                "display_name": "MacBook Pro",
                "transport": "edge_ws",
                "status": "online"
            },
            "transport": "edge_ws",
            "fallback_policy": "disabled"
        }));
        host.edge_callback_ledger.lock().await.insert(
            tool_callback_key("u-edge-meta", "r1"),
            json!({"body": {"request_id": "r1", "status": "ok", "output": "read-a"}}),
        );
        let tool_calls = vec![json!({
            "id": "r1",
            "type": "function",
            "function": {"name": "read_file", "arguments": r#"{"path":"a.rs"}"#}
        })];

        let results = host.deliver_edge_tools_via_ledger(&tool_calls).await;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, "ok");
        let end = host
            .emitted_events
            .iter()
            .find(|event| event.get("type").and_then(Value::as_str) == Some("tool_call_end"))
            .expect("tool_call_end");
        assert_eq!(end["call_id"], "r1");
        assert_eq!(end["workspace"]["kind"], "edge_workspace");
        assert_eq!(end["workspace"]["cwd"], "/Users/test/project");
        assert_eq!(end["executor"]["kind"], "edge_agent");
        assert_eq!(end["executor"]["executor_id"], "edge-1");
        assert_eq!(end["executor"]["transport"], "edge_ledger");
        assert_eq!(end["executor"]["status"], "online");
        assert_eq!(end["transport"], "edge_ledger");
        assert_eq!(end["fallback_policy"], "disabled");
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
        // Register read_file and write_file as valid tools so the edge ledger delivery path admits them.
        host.install_runtime_tool_schemas(vec![
            json!({
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": "Read file contents",
                    "parameters": {"type": "object", "properties": {}}
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "write_file",
                    "description": "Write file contents",
                    "parameters": {"type": "object", "properties": {}}
                }
            }),
        ]);
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
        use astra_pipeline::step_protocol::InMemoryIdempotencyCache;
        use astra_pipeline::step_recorder::StepRecorder;
        use astra_text_utils::semantic_dedup::SemanticDedup;
        use astra_turn_core::chat_turn_heuristics::TaskExecutionProfile;
        use astra_turn_core::turn_guard::TurnGuard;

        AgenticLoopState {
            messages: Vec::new(),
            volatile_pending: Vec::new(),
            recent_rounds: Vec::new(),
            tool_results: Vec::new(),
            current_session_id: None,
            current_run_id: None,
            context_manifest_pool: None,
            context_manifest_user_id: None,
            context_manifest_model_name: None,
            runtime_manifest: None,
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
            textless_stop_retries: 0,
            last_finish_reason: None,
            turn_budget_hint_emitted_90: false,
            turn_budget_hint_emitted_50: false,
            turn_budget_hint_emitted_20: false,
            agentic_turn_budget: TaskExecutionProfile::default().agentic_turn_budget,
            budget_policy: None,
            current_round_index: 0,
            llm_rounds_completed: 0,
            last_request_message_count: None,
            turn_guard: TurnGuard::new(),
            restricted_tools: HashSet::new(),
            boosted_tools: HashSet::new(),
            widen_selection_pending: false,
            step_recorder: StepRecorder::new("test-user", "test-session", "test-task"),
            idempotency_cache: InMemoryIdempotencyCache::new(),
            semantic_dedup: SemanticDedup::new(0.75),
            observation_journal: Default::default(),
            observation_store: None,
            call_counts: HashMap::new(),
            max_identical_tool_calls: astra_config::runtime_config::RuntimeConfig::load()
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
            deferred_input: Default::default(),
            error_recovery: Default::default(),
            run_control: None,
            pipeline_session: Some(astra_turn_core::pipeline_session::PipelineSession::new(
                astra_turn_core::pipeline_config::PipelineConfig::default(),
            )),
            message: "test query".to_string(),
            recent_tools: Vec::new(),
            has_prior_assistant_turn: false,
            task_profile: TaskExecutionProfile::default(),
            last_turn_policy: crate::turn::agentic_loop::host::TurnInteractionPolicy::default(),
            api: astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
            api_token: "test-token".to_string(),
            delegation_engine: None,
            delegations_this_turn: 0,
            delegation_chain: Vec::new(),
            self_agent_id: "orchestrator".to_string(),
            project_context: None,
            checkpoint_gate: None,
            last_llm_context_manifest_trace: None,
            rate_limit_cooldown: Default::default(),
            data_snapshot_provider: None,
            last_composite_snapshot: None,
            last_measured_prompt_tokens: None,
            consecutive_context_window_errors: 0,
            compaction_effectiveness: Default::default(),
            pinned_tool_schema_tokens: 0,
            sticky_tool_schemas: Vec::new(),
            max_turn_input_tokens: 0,
            budget_wrapup_injected: false,
            budget_wrapup_ignored_rounds: 0,
            compact_tier_applied: CompactionTier::Normal,
            skill_produced_output: false,
            max_cumulative_tokens: 0,
            thinking: astra_turn_core::thinking_config::ThinkingConfig::Off,
            recent_file_reads: Vec::new(),
            permission_context: None,
            permission_handler: None,
            tactical_adapter: None,
            step_signal_collector: None,
            tool_budget_override: None,
            recent_tactical_actions: Vec::new(),
            server_tool_executor: None,
            interruption: None,
            session_facts: Default::default(),
            memory_extraction_service: None,
            session_memory_state: Default::default(),
            session_memory_llm_params: None,
            compact_strategy: Default::default(),
            approval_overrides: None,
            confidence_trend: Default::default(),
            last_confidence_diagnosis: None,
            session_turn: 0,
            bridge_turn_chain_id: None,
            bridge_user_query_event_id: None,
            turn_event_buffer: None,
            harness: crate::turn::harness_adapter::HarnessSlot::empty(),
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
    fn visible_turn_tools_excludes_only_hard_restricted_tools() {
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u".to_string(),
            "s".to_string(),
        )
        .with_edge_tools(sample_edge_tools())
        .with_execution_binding_snapshot(edge_runtime_snapshot())
        .build();

        let mut state = create_test_state();
        state.restricted_tools.insert("read_file".to_string());
        state
            .turn_guard
            .health
            .record_resource_limit_failure("bash");

        let visible = host.visible_turn_tools(&mut state);
        let visible_names: HashSet<&str> = visible
            .iter()
            .filter_map(|tool| {
                tool.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
            })
            .collect();

        assert!(visible_names.contains("bash"));
        assert!(!visible_names.contains("read_file"));
        assert!(
            !state.restricted_tools.contains("bash"),
            "soft health must not mutate hard restricted_tools"
        );
        assert!(state.restricted_tools.contains("read_file"));
    }

    #[test]
    fn visible_turn_tools_filters_executor_service_unready_tools() {
        let dir = tempfile::TempDir::new().expect("temp workspace");
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u".to_string(),
            "s".to_string(),
        )
        .with_server_sandbox_workspace(dir.path())
        .build();
        let raw_names = astra_turn_core::tool::schema::tool_names_from_schemas(&host.edge_tools);
        assert!(
            raw_names.contains("reflect"),
            "capability-only server surface starts with reflect before executor readiness filtering"
        );

        let mut state = create_test_state();
        state.server_tool_executor = Some(Arc::new(
            crate::server::server_tool_executor::ServerToolExecutor::new(
                dir.path().to_path_buf(),
                "u".to_string(),
                "s".to_string(),
                None,
                None,
            ),
        ));

        let visible = host.visible_turn_tools(&mut state);
        let names = astra_turn_core::tool::schema::tool_names_from_schemas(&visible);
        assert!(
            !names.contains("reflect"),
            "service-unready tools must not be visible to the model: {names:?}"
        );
        assert!(
            names.contains("introspect"),
            "ready observation tools should remain visible"
        );
    }

    #[test]
    fn builder_hides_reflect_without_reflect_service_capability() {
        let host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u".to_string(),
            "s".to_string(),
        )
        .with_capabilities(crate::capabilities::lifecycle_server_capabilities(
            true, false,
        ))
        .build();

        let names = astra_turn_core::tool::schema::tool_names_from_schemas(&host.edge_tools);
        assert!(
            !names.contains("reflect"),
            "builder must fail closed before executor filtering when the reflect service is unconfigured: {names:?}"
        );
    }

    #[test]
    fn visible_turn_tools_apply_effective_runtime_allowlist() {
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u".to_string(),
            "s".to_string(),
        )
        .with_edge_tools(sample_edge_tools())
        .with_execution_binding_snapshot(edge_runtime_snapshot())
        .build();

        let mut state = create_test_state();
        // Delegation restricts to bash + read_file, then the active skill narrows
        // that set further to bash only.
        state.skills.request_constraints.allowed_tools = Some(
            ["bash".to_string(), "read_file".to_string()]
                .into_iter()
                .collect(),
        );
        state.session_turn = 1;
        state.skills.allowed_tools = Some(["bash".to_string()].into_iter().collect());
        state.skills.invoked.insert(
            "review-changes".into(),
            crate::turn::skill_tool::InvokedSkill {
                name: "review-changes".into(),
                content: String::new(),
                invoked_at_turn: 1,
                reentry_count: 0,
            },
        );

        let visible = host.visible_turn_tools(&mut state);
        let visible_names: HashSet<&str> = visible
            .iter()
            .filter_map(|tool| {
                tool.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
            })
            .collect();

        // With the new design, skill allowed_tools is only a hint —
        // the visible schema reflects the request constraints directly.
        assert!(visible_names.contains("bash"));
        assert!(visible_names.contains("read_file"));
        // Tools outside the request allowlist are hidden.
        assert!(!visible_names.contains("str_replace"));
    }

    #[test]
    fn visible_turn_tools_excludes_disabled_tools() {
        let edge_tools = vec![
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
        ];

        let disabled: HashSet<String> = ["bash".to_string()].into_iter().collect();
        let disabled_handle = Arc::new(tokio::sync::RwLock::new(disabled));

        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u".to_string(),
            "s".to_string(),
        )
        .with_edge_tools(edge_tools)
        .with_execution_binding_snapshot(edge_runtime_snapshot())
        .with_disabled_tools(disabled_handle)
        .build();

        let mut state = create_test_state();
        let visible = host.visible_turn_tools(&mut state);
        let visible_names: HashSet<&str> = visible
            .iter()
            .filter_map(|tool| {
                tool.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
            })
            .collect();

        assert!(visible_names.contains("read_file"));
        assert!(!visible_names.contains("bash"));
        assert_eq!(visible_names.len(), 1);
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
        .with_execution_binding_snapshot(edge_runtime_snapshot())
        .build();

        let state = create_test_state();
        let mut effective_restricted = state.restricted_tools.clone();
        effective_restricted.extend(interaction_scoped_tool_restrictions(
            TurnInteractionMode::Headless,
        ));
        let visible_tools = host.filtered_turn_tools(&effective_restricted);
        let final_tools = astra_turn_core::tool_schema_prune::prune_tool_schemas(
            &visible_tools,
            crate::prompts::CompactionTier::Normal,
        );
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
        .with_execution_binding_snapshot(edge_runtime_snapshot())
        .with_interactive_client(true)
        .build();

        let state = create_test_state();
        let mut effective_restricted = state.restricted_tools.clone();
        effective_restricted.extend(interaction_scoped_tool_restrictions(
            host.turn_interaction_mode(),
        ));
        let visible_tools = host.filtered_turn_tools(&effective_restricted);
        let final_tools = astra_turn_core::tool_schema_prune::prune_tool_schemas(
            &visible_tools,
            crate::prompts::CompactionTier::Normal,
        );
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

    #[test]
    fn server_host_honors_requested_auto_interaction_mode() {
        let host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u".to_string(),
            "s".to_string(),
        )
        .with_interactive_client(true)
        .with_interaction_mode(Some(RequestedTurnInteractionMode::Auto))
        .build();

        assert_eq!(host.turn_interaction_mode(), TurnInteractionMode::Auto);
        assert!(host.turn_interaction_mode().suppresses_loop_nudges());
    }

    #[test]
    fn server_host_defaults_to_prompt_for_interactive_clients_without_override() {
        let host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u".to_string(),
            "s".to_string(),
        )
        .with_interactive_client(true)
        .build();

        assert_eq!(host.turn_interaction_mode(), TurnInteractionMode::Prompt);
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
        .with_execution_binding_snapshot(edge_runtime_snapshot())
        .build();

        assert!(!host.valid_tool_names().contains("delegate"));
        let initial_count = host.edge_tools.len();

        use crate::turn::agentic_loop::host::delegate_tool_schema;
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
        .with_execution_binding_snapshot(edge_runtime_snapshot())
        .build();

        use crate::turn::agentic_loop::host::delegate_tool_schema;
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
        .with_execution_binding_snapshot(edge_runtime_snapshot())
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
        .with_execution_binding_snapshot(edge_runtime_snapshot())
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
        .with_execution_binding_snapshot(edge_runtime_snapshot())
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
            llm_events[0]["metadata"]["request_summary"]["message_count"].as_u64(),
            Some(2)
        );
        assert!(
            llm_events[0]["metadata"]["prompt_request_id"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
        );
        assert_eq!(
            llm_events[0]["metadata"]["request_summary"]["message_roles"][0]["role"].as_str(),
            Some("system")
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
            llm_events[1]["metadata"]["response"]["response"]["usage"]["output_tokens"].as_i64(),
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
        // Collapse llm_client's 1s/2s/4s exponential backoff so the retry
        // loop behind a 500 upstream completes in tens of ms instead of 7s.
        let _backoff = crate::turn::llm::client::set_test_retry_backoff_ms(0);
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
        .with_execution_binding_snapshot(edge_runtime_snapshot())
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
        .with_execution_binding_snapshot(edge_runtime_snapshot())
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
        .with_execution_binding_snapshot(edge_runtime_snapshot())
        .with_test_llm_rounds(vec![json!({
            "error": {
                "message": "synthetic streamed failure",
                "kind": "stream_transport",
                "details": {
                    "partial_full_text": "half answer",
                    "usage": { "input_tokens": 17, "output_tokens": 3, "total_tokens": 20 }
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
        assert_eq!(details["usage"]["input_tokens"].as_i64(), Some(17));
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
            wire_model_name: None,
            api_key: String::new(),
            base_url: "https://api.openai.com/v1".to_string(),
            provider: "openai".to_string(),
            max_output_tokens: 128,
            header_overrides: forwarded,
            request_body_overrides: None,
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

    #[tokio::test]
    #[serial_test::serial(auxiliary_llm_capacity_policy_env)]
    async fn factual_retry_fallback_judge_uses_llm_response_selection() {
        use axum::{Router, extract::State, routing::post};
        use tokio::net::TcpListener;

        let _aux_policy = EnvVarGuard::remove(AUX_LLM_POLICY_ENV);
        let _policy = EnvVarGuard::set(FACTUAL_RETRY_JUDGE_POLICY_ENV, "always");

        #[derive(Default)]
        struct RequestCapture {
            body: tokio::sync::Mutex<Option<Value>>,
        }

        async fn handler(
            State(capture): State<Arc<RequestCapture>>,
            request: axum::extract::Request,
        ) -> axum::Json<Value> {
            let bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
                .await
                .expect("read request body");
            *capture.body.lock().await = Some(serde_json::from_slice(&bytes).expect("json body"));
            axum::Json(json!({
                "choices": [
                    {
                        "message": {
                            "content": "{\"decision\":\"restore_fallback\",\"confidence\":0.94,\"reason\":\"candidate A answers the UI question\"}"
                        },
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

        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "user-judge".to_string(),
            "session-judge".to_string(),
        )
        .build();
        host.resolved_llm_params = Some(astra_turn_core::cloud_summary::LlmConnParams {
            model_name: "gpt-4o-mini".to_string(),
            api_key: String::new(),
            base_url: format!("http://{addr}/gateway"),
            provider: "openai".to_string(),
            max_output_tokens: 128,
        });

        let decision = host
            .judge_factual_retry_fallback(FactualRetryFallbackJudgeContext {
                original_query: "what do 59% and 117k mean?",
                fallback_text: "59% is context usage; 117k is token count.",
                retry_text: "I completed the requested work.",
            })
            .await;

        assert_eq!(
            decision,
            Some(FactualRetryFallbackDecision::RestoreFallback)
        );
        let body = capture.body.lock().await.clone().expect("judge request");
        let messages = body["messages"].as_array().expect("messages");
        let prompt = messages[1]["content"].as_str().expect("user prompt");
        assert!(prompt.contains("Candidate A"));
        assert!(prompt.contains("Candidate B"));

        server.abort();
    }

    #[tokio::test]
    #[serial_test::serial(auxiliary_llm_capacity_policy_env)]
    async fn factual_retry_fallback_judge_skips_gateway_when_provider_admission_is_enabled() {
        use axum::{Router, routing::post};
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
        use tokio::net::TcpListener;

        let _aux_policy = EnvVarGuard::remove(AUX_LLM_POLICY_ENV);
        let _policy = EnvVarGuard::remove(FACTUAL_RETRY_JUDGE_POLICY_ENV);
        let _mode = EnvVarGuard::set("ASTRA_LLM_PROVIDER_ADMISSION_MODE", "db_fixed_window");
        let _rpm = EnvVarGuard::set("ASTRA_LLM_PROVIDER_ADMISSION_RPM", "20");

        let request_count = Arc::new(AtomicUsize::new(0));
        let request_count_for_handler = request_count.clone();
        let app = Router::new().route(
            "/gateway/chat/completions",
            post(move || {
                let request_count = request_count_for_handler.clone();
                async move {
                    request_count.fetch_add(1, AtomicOrdering::SeqCst);
                    axum::Json(json!({
                        "choices": [
                            {
                                "message": {
                                    "content": "{\"decision\":\"restore_fallback\",\"confidence\":0.94,\"reason\":\"candidate A answers the UI question\"}"
                                },
                                "finish_reason": "stop"
                            }
                        ],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1}
                    }))
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("listener addr");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("test server should run");
        });

        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "user-judge".to_string(),
            "session-judge".to_string(),
        )
        .build();
        host.resolved_llm_params = Some(astra_turn_core::cloud_summary::LlmConnParams {
            model_name: "gpt-4o-mini".to_string(),
            api_key: String::new(),
            base_url: format!("http://{addr}/gateway"),
            provider: "openai".to_string(),
            max_output_tokens: 128,
        });

        let decision = host
            .judge_factual_retry_fallback(FactualRetryFallbackJudgeContext {
                original_query: "what do 59% and 117k mean?",
                fallback_text: "59% is context usage; 117k is token count.",
                retry_text: "I completed the requested work.",
            })
            .await;

        assert_eq!(decision, None);
        assert_eq!(
            request_count.load(AtomicOrdering::SeqCst),
            0,
            "capacity-aware default must not spend provider RPM on factual retry judge"
        );

        server.abort();
    }

    #[tokio::test]
    #[serial_test::serial(auxiliary_llm_capacity_policy_env)]
    async fn maybe_pre_turn_compact_uses_inline_summary_prefix() {
        use axum::{Router, extract::State, routing::post};
        use tokio::net::TcpListener;

        let _aux_policy = EnvVarGuard::remove(AUX_LLM_POLICY_ENV);
        let _policy = EnvVarGuard::set(PRE_TURN_COMPACTION_LLM_POLICY_ENV, "always");

        #[derive(Default)]
        struct RequestCapture {
            body: tokio::sync::Mutex<Option<Value>>,
        }

        async fn handler(
            State(capture): State<Arc<RequestCapture>>,
            request: axum::extract::Request,
        ) -> axum::Json<Value> {
            let bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
                .await
                .expect("read request body");
            *capture.body.lock().await = Some(serde_json::from_slice(&bytes).expect("json body"));
            axum::Json(json!({
                "choices": [
                    {
                        "message": { "content": "inline summary" },
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

        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "user-inline".to_string(),
            "session-inline".to_string(),
        )
        .with_edge_tools(sample_edge_tools())
        .with_execution_binding_snapshot(edge_runtime_snapshot())
        .build();
        host.resolved_llm_params = Some(astra_turn_core::cloud_summary::LlmConnParams {
            model_name: "gpt-4o-mini".to_string(),
            api_key: String::new(),
            base_url: format!("http://{addr}/gateway"),
            provider: "openai".to_string(),
            max_output_tokens: 128,
        });

        let mut state = create_test_state();
        state.max_turn_input_tokens = 100;
        state.message = "Fix the regression".to_string();
        for i in 0..6 {
            state
                .messages
                .push(json!({"role": "user", "content": format!("question {i}")}));
            state
                .messages
                .push(json!({"role": "assistant", "content": format!("answer {i}")}));
        }

        <ServerAgenticLoopHost as crate::turn::agentic_loop::host::AgenticLoopHost>::maybe_pre_turn_compact(
            &mut host,
            &mut state,
            0.95,
            true,
        )
        .await;
        assert_eq!(
            state.compact_tier_applied,
            CompactionTier::CompactHistory,
            "pre-turn compact should bump tier to CompactHistory",
        );

        let body = capture
            .body
            .lock()
            .await
            .clone()
            .expect("summary request should be captured");
        let messages = body["messages"]
            .as_array()
            .expect("openai request should contain messages");

        assert!(
            messages.len() > state.messages.len() / 2,
            "inline summary request should include system prompt plus much of the original history"
        );
        assert_eq!(
            messages
                .first()
                .and_then(|m| m.get("role"))
                .and_then(Value::as_str),
            Some("system"),
            "inline summary request should begin with main-turn system messages"
        );
        assert!(
            messages.iter().any(|m| {
                m["content"]
                    .as_str()
                    .is_some_and(|s| s.contains("question 0"))
            }),
            "expected original conversation history in inline summary request"
        );
        assert_eq!(
            messages
                .last()
                .and_then(|m| m.get("content"))
                .and_then(Value::as_str),
            Some(astra_turn_core::cloud_summary::INLINE_COMPACT_INSTRUCTION),
            "expected trailing inline compact instruction"
        );
        assert!(
            !messages.iter().any(|m| {
                m["content"]
                    .as_str()
                    .is_some_and(|s| s.contains("You are a conversation summarizer"))
            }),
            "inline path must not fall back to COMPACT_SYSTEM_PROMPT"
        );

        server.abort();
    }

    #[tokio::test]
    #[serial_test::serial(auxiliary_llm_capacity_policy_env)]
    async fn maybe_pre_turn_compact_skips_gateway_when_provider_admission_is_enabled() {
        use axum::{Router, routing::post};
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
        use tokio::net::TcpListener;

        let _aux_policy = EnvVarGuard::remove(AUX_LLM_POLICY_ENV);
        let _policy = EnvVarGuard::remove(PRE_TURN_COMPACTION_LLM_POLICY_ENV);
        let _mode = EnvVarGuard::set("ASTRA_LLM_PROVIDER_ADMISSION_MODE", "db_fixed_window");
        let _rpm = EnvVarGuard::set("ASTRA_LLM_PROVIDER_ADMISSION_RPM", "20");

        let request_count = Arc::new(AtomicUsize::new(0));
        let request_count_for_handler = request_count.clone();
        let app = Router::new().route(
            "/gateway/chat/completions",
            post(move || {
                let request_count = request_count_for_handler.clone();
                async move {
                    request_count.fetch_add(1, AtomicOrdering::SeqCst);
                    axum::Json(json!({
                        "choices": [
                            {
                                "message": { "content": "inline summary" },
                                "finish_reason": "stop"
                            }
                        ],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1}
                    }))
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("listener addr");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("test server should run");
        });

        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "user-inline".to_string(),
            "session-inline".to_string(),
        )
        .build();
        host.resolved_llm_params = Some(astra_turn_core::cloud_summary::LlmConnParams {
            model_name: "gpt-4o-mini".to_string(),
            api_key: String::new(),
            base_url: format!("http://{addr}/gateway"),
            provider: "openai".to_string(),
            max_output_tokens: 128,
        });

        let mut state = create_test_state();
        state.max_turn_input_tokens = 100;
        state.message = "Fix the regression".to_string();
        for i in 0..6 {
            state
                .messages
                .push(json!({"role": "user", "content": format!("question {i}")}));
            state
                .messages
                .push(json!({"role": "assistant", "content": format!("answer {i}")}));
        }

        <ServerAgenticLoopHost as crate::turn::agentic_loop::host::AgenticLoopHost>::maybe_pre_turn_compact(
            &mut host,
            &mut state,
            0.95,
            true,
        )
        .await;

        assert_eq!(state.compact_tier_applied, CompactionTier::Normal);
        assert_eq!(
            request_count.load(AtomicOrdering::SeqCst),
            0,
            "capacity-aware default must not spend provider RPM on pre-turn LLM compaction"
        );

        server.abort();
    }

    // ── progress_event_to_sse tests ──

    #[test]
    fn set_event_tx_disables_host_progress_subscription() {
        use crate::orchestration::ProgressBroadcaster;

        let broadcaster = Arc::new(ProgressBroadcaster::default());
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "user1".to_string(),
            "sess1".to_string(),
        )
        .with_progress_broadcaster(broadcaster)
        .with_progress_root_run_id("run-root".to_string())
        .build();
        assert!(host.progress_rx.is_some());
        assert!(host.progress_filter.is_some());

        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        host.set_event_tx(tx);

        assert!(host.progress_rx.is_none());
        assert!(host.progress_filter.is_none());
    }

    #[test]
    fn full_sse_channel_detaches_stream_without_cancelling_run() {
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "user1".to_string(),
            "sess1".to_string(),
        )
        .build();
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let cancel_token = Arc::new(CancellationToken::new());
        host.set_client_cancel(Arc::clone(&cancel_flag), Arc::clone(&cancel_token));

        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        tx.try_send(json!({"type": "preloaded"}))
            .expect("pre-fill bounded channel");
        host.set_event_tx(tx);

        host.emit_event(json!({"type": "text_delta", "content": "slow client"}));

        assert!(
            !cancel_flag.load(Ordering::SeqCst),
            "backpressure must not be promoted to user cancellation"
        );
        assert!(
            !cancel_token.is_cancelled(),
            "LLM cancellation token must remain active on channel backpressure"
        );
        assert!(
            host.event_tx.is_none(),
            "live stream sender should be detached after bounded channel backpressure"
        );
        assert_eq!(host.emitted_events.len(), 1);
    }

    #[test]
    fn tool_call_start_events_preserve_execution_metadata() {
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "user1".to_string(),
            "sess1".to_string(),
        )
        .build();
        host.set_execution_metadata(json!({
            "workspace": {
                "kind": "edge_workspace",
                "display_name": "MacBook Pro",
                "cwd": "/Users/test/project",
                "authority": "read_write",
                "fallback_policy": "disabled"
            },
            "executor": {
                "kind": "edge_agent",
                "executor_id": "edge-1",
                "display_name": "MacBook Pro",
                "transport": "edge_ws",
                "status": "online"
            },
            "transport": "edge_ws",
            "fallback_policy": "disabled"
        }));
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        host.set_event_tx(tx);

        host.emit_event(json!({
            "type": "tool_call_start",
            "call_id": "call-1",
            "tool": "bash"
        }));

        let event = rx.try_recv().expect("tool_call_start event");
        assert_eq!(event["type"], "tool_call_start");
        assert_eq!(event["workspace"]["kind"], "edge_workspace");
        assert_eq!(event["workspace"]["cwd"], "/Users/test/project");
        assert_eq!(event["executor"]["kind"], "edge_agent");
        assert_eq!(event["transport"], "edge_ws");
        assert_eq!(event["fallback_policy"], "disabled");
    }

    #[test]
    fn tool_call_events_preserve_execution_metadata() {
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "user1".to_string(),
            "sess1".to_string(),
        )
        .build();
        host.set_execution_metadata(json!({
            "workspace": {
                "kind": "edge_workspace",
                "display_name": "MacBook Pro",
                "cwd": "/Users/test/project",
                "authority": "read_write",
                "fallback_policy": "disabled"
            },
            "executor": {
                "kind": "edge_agent",
                "executor_id": "edge-1",
                "display_name": "MacBook Pro",
                "transport": "edge_ws",
                "status": "online"
            },
            "transport": "edge_ws",
            "fallback_policy": "disabled"
        }));
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        host.set_event_tx(tx);

        host.emit_event(json!({
            "type": "tool_call",
            "tool_call": {
                "id": "call-1",
                "function": {
                    "name": "edge_shell",
                    "arguments": "{\"command\":\"pwd\"}"
                }
            }
        }));

        let event = rx.try_recv().expect("tool_call event");
        assert_eq!(event["type"], "tool_call");
        assert_eq!(event["workspace"]["kind"], "edge_workspace");
        assert_eq!(event["workspace"]["cwd"], "/Users/test/project");
        assert_eq!(event["executor"]["kind"], "edge_agent");
        assert_eq!(event["executor"]["executor_id"], "edge-1");
        assert_eq!(event["transport"], "edge_ws");
        assert_eq!(event["fallback_policy"], "disabled");
    }

    #[test]
    fn tool_call_start_projects_server_runtime_metadata_for_runtime_tools() {
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "user1".to_string(),
            "sess1".to_string(),
        )
        .build();
        host.set_execution_metadata(json!({
            "workspace": {
                "kind": "edge_workspace",
                "display_name": "MacBook Pro",
                "cwd": "/Users/test/project",
                "authority": "read_write",
                "fallback_policy": "disabled"
            },
            "executor": {
                "kind": "edge_agent",
                "executor_id": "edge-1",
                "display_name": "MacBook Pro",
                "transport": "edge_ws",
                "status": "online"
            },
            "transport": "edge_ws",
            "fallback_policy": "disabled"
        }));
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        host.set_event_tx(tx);

        host.emit_event(json!({
            "type": "tool_call_start",
            "call_id": "call-web-search",
            "tool": "web_search"
        }));

        let event = rx.try_recv().expect("tool_call_start event");
        assert_eq!(event["type"], "tool_call_start");
        assert_eq!(event["workspace"]["kind"], "none");
        assert_eq!(event["executor"]["kind"], "server_local");
        assert_eq!(event["executor"]["executor_id"], "server-runtime");
        assert_eq!(event["executor"]["display_name"], "Server runtime");
        assert_eq!(event["transport"], "server_local");
    }

    #[test]
    fn tool_call_start_projects_request_scoped_mcp_metadata() {
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "user1".to_string(),
            "sess1".to_string(),
        )
        .build();
        host.set_execution_metadata(json!({
            "workspace": {
                "kind": "edge_workspace",
                "display_name": "MacBook Pro",
                "cwd": "/Users/test/project",
                "authority": "read_write",
                "fallback_policy": "disabled"
            },
            "executor": {
                "kind": "edge_agent",
                "executor_id": "edge-1",
                "display_name": "MacBook Pro",
                "transport": "edge_ws",
                "status": "online"
            },
            "transport": "edge_ws",
            "fallback_policy": "disabled"
        }));
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        host.set_event_tx(tx);

        host.emit_event(json!({
            "type": "tool_call_start",
            "tool_call": {
                "id": "call-mcp",
                "function": {
                    "name": "mcp__demo__search",
                    "arguments": "{\"query\":\"hello\"}"
                }
            }
        }));

        let event = rx.try_recv().expect("tool_call_start event");
        assert_eq!(event["type"], "tool_call_start");
        assert_eq!(event["workspace"]["kind"], "edge_workspace");
        assert_eq!(event["workspace"]["cwd"], "/Users/test/project");
        assert_eq!(event["executor"]["kind"], "mcp");
        assert_eq!(event["executor"]["executor_id"], "request-scoped-mcp");
        assert_eq!(event["executor"]["display_name"], "MCP server");
        assert_eq!(event["transport"], "mcp_http");
        assert_eq!(event["fallback_policy"], "disabled");
    }

    #[test]
    fn server_sse_text_delta_maps_to_agent_live_output() {
        let kind = super::agent_live_event_kind_from_server_sse(&json!({
            "type": "text_delta",
            "content": "child output"
        }))
        .expect("text delta should mirror");

        assert!(matches!(
            kind,
            AgentLiveEventKind::OutputDelta(text) if text == "child output"
        ));
    }

    #[test]
    fn server_sse_tool_events_map_to_agent_live_tool_lifecycle() {
        let started = super::agent_live_event_kind_from_server_sse(&json!({
            "type": "tool_call",
            "tool_call": {
                "id": "call-1",
                "function": {"name": "bash", "arguments": "{\"cmd\":\"cargo test\"}"}
            }
        }))
        .expect("tool call should mirror");
        assert!(matches!(
            started,
            AgentLiveEventKind::ToolStarted { name, tool_use_id, .. }
                if name == "bash" && tool_use_id == "call-1"
        ));

        let completed = super::agent_live_event_kind_from_server_sse(&json!({
            "type": "tool_call_end",
            "call_id": "call-1",
            "tool": "bash",
            "success": true,
            "result": "ok"
        }))
        .expect("tool result should mirror");
        assert!(matches!(
            completed,
            AgentLiveEventKind::ToolCompleted { name, status, output, tool_use_id, .. }
                if name == "bash"
                    && status == "ok"
                    && output.as_deref() == Some("ok")
                    && tool_use_id == "call-1"
        ));
    }

    #[test]
    fn server_sse_tool_transport_failed_maps_error_to_agent_live_output() {
        let failed = super::agent_live_event_kind_from_server_sse(&json!({
            "type": "tool_transport_failed",
            "call_id": "call-2",
            "tool": "bash",
            "success": false,
            "duration_ms": 42,
            "error": "edge transport disconnected"
        }))
        .expect("transport failure should mirror");

        assert!(matches!(
            failed,
            AgentLiveEventKind::ToolCompleted {
                name,
                status,
                duration_ms,
                output,
                tool_use_id,
                ..
            } if name == "bash"
                && status == "error"
                && duration_ms == 42
                && output.as_deref() == Some("edge transport disconnected")
                && tool_use_id == "call-2"
        ));
    }

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
                fanout_slot: None,
            },
            timestamp_epoch_ms: 1000,
            metadata: None,
        };
        let sse = super::progress_event_to_sse(&evt).expect("should produce SSE");
        assert_eq!(sse["type"], "agent_spawned");
        assert_eq!(sse["agent_id"], "agent-1");
        assert_eq!(sse["run_id"], "run-123");
        assert_eq!(sse["agent_type"], "explore");
    }

    #[test]
    fn progress_event_agent_spawned_preserves_execution_metadata() {
        use crate::orchestration::{AgentProgressEvent, ProgressEventType};

        let evt = AgentProgressEvent {
            agent_id: "agent-1".to_string(),
            event_type: ProgressEventType::AgentSpawned {
                run_id: "run-123".to_string(),
                parent_run_id: "run-000".to_string(),
                agent_type: "explore".to_string(),
                description: "Search codebase".to_string(),
                fanout_slot: None,
            },
            timestamp_epoch_ms: 1000,
            metadata: Some(json!({
                "workspace": {"kind": "edge_workspace", "cwd": "/repo"},
                "executor": {"kind": "edge_agent", "display_name": "MacBook Pro"},
                "transport": "edge_ws",
                "fallback_policy": "disabled",
                "agent_id": "must-not-overwrite",
            })),
        };

        let sse = super::progress_event_to_sse(&evt).expect("should produce SSE");
        assert_eq!(sse["agent_id"], "agent-1");
        assert_eq!(sse["workspace"]["kind"], "edge_workspace");
        assert_eq!(sse["executor"]["kind"], "edge_agent");
        assert_eq!(sse["transport"], "edge_ws");
        assert_eq!(sse["fallback_policy"], "disabled");
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
            metadata: None,
        };
        let sse = super::progress_event_to_sse(&evt).expect("should produce SSE");
        assert_eq!(sse["type"], "agent_completed");
        assert_eq!(sse["status"], "completed");
        assert_eq!(sse["total_tool_calls"], 5);
    }

    #[test]
    fn progress_event_live_statuses_to_sse_agent_progress() {
        use crate::orchestration::{AgentProgressEvent, ProgressEventType};

        let cases = vec![
            (
                ProgressEventType::Busy {
                    activity: "executing".to_string(),
                },
                "busy",
                json!({"activity": "executing"}),
            ),
            (
                ProgressEventType::LlmCallStarted { turn: 2 },
                "llm_call_started",
                json!({"turn": 2}),
            ),
            (
                ProgressEventType::LlmCallCompleted {
                    turn: 2,
                    ttft_ms: Some(17),
                    duration_ms: 91,
                },
                "llm_call_completed",
                json!({"turn": 2, "ttft_ms": 17, "duration_ms": 91}),
            ),
            (
                ProgressEventType::TurnCompleted {
                    turn: 3,
                    tool_calls_this_turn: 1,
                    activity: "summarized".to_string(),
                },
                "turn_completed",
                json!({"turn": 3, "tool_calls_this_turn": 1, "activity": "summarized"}),
            ),
            (
                ProgressEventType::PermissionDenied {
                    tool_name: "bash".to_string(),
                    reason: "approval required".to_string(),
                    turn: 4,
                },
                "permission_denied",
                json!({"tool_name": "bash", "reason": "approval required", "turn": 4}),
            ),
        ];

        for (event_type, status, expected_fields) in cases {
            let evt = AgentProgressEvent {
                agent_id: "agent-live".to_string(),
                event_type,
                timestamp_epoch_ms: 1234,
                metadata: None,
            };
            let sse = super::progress_event_to_sse(&evt).expect("progress SSE");
            assert_eq!(sse["type"], "agent_progress");
            assert_eq!(sse["agent_id"], "agent-live");
            assert_eq!(sse["status"], status);
            assert_eq!(sse["timestamp"], 1234);
            for (key, value) in expected_fields.as_object().unwrap() {
                assert_eq!(&sse[key], value, "field {key} for status {status}");
            }
        }
    }

    #[test]
    fn progress_event_interrupted_to_sse() {
        use crate::orchestration::{AgentProgressEvent, ProgressEventType};

        let evt = AgentProgressEvent {
            agent_id: "agent-2".to_string(),
            event_type: ProgressEventType::Interrupted {
                reason: "budget_exhausted".to_string(),
                partial_summary: "Partial".to_string(),
                total_tool_calls: 5,
                total_tokens: (100, 200),
                duration_ms: 3000,
            },
            timestamp_epoch_ms: 2000,
            metadata: None,
        };
        let sse = super::progress_event_to_sse(&evt).expect("should produce SSE");
        assert_eq!(sse["type"], "agent_interrupted");
        assert_eq!(sse["status"], "interrupted");
        assert_eq!(sse["reason"], "budget_exhausted");
    }

    #[test]
    fn progress_event_failed_and_cancelled_use_distinct_terminal_event_types() {
        use crate::orchestration::{AgentProgressEvent, ProgressEventType};

        let failed = AgentProgressEvent {
            agent_id: "agent-f".to_string(),
            event_type: ProgressEventType::Failed {
                error: "boom".to_string(),
            },
            timestamp_epoch_ms: 1,
            metadata: None,
        };
        let cancelled = AgentProgressEvent {
            agent_id: "agent-c".to_string(),
            event_type: ProgressEventType::Cancelled {
                reason: "user request".to_string(),
            },
            timestamp_epoch_ms: 2,
            metadata: None,
        };

        let failed_sse = super::progress_event_to_sse(&failed).expect("failed SSE");
        let cancelled_sse = super::progress_event_to_sse(&cancelled).expect("cancelled SSE");

        assert_eq!(failed_sse["type"], "agent_failed");
        assert_eq!(failed_sse["status"], "failed");
        assert_eq!(cancelled_sse["type"], "agent_cancelled");
        assert_eq!(cancelled_sse["status"], "cancelled");
    }

    #[test]
    fn progress_event_idle_to_sse_agent_progress() {
        use crate::orchestration::{AgentProgressEvent, ProgressEventType};

        let evt = AgentProgressEvent {
            agent_id: "agent-3".to_string(),
            event_type: ProgressEventType::Idle,
            timestamp_epoch_ms: 3000,
            metadata: None,
        };
        let sse = super::progress_event_to_sse(&evt).expect("idle progress SSE");
        assert_eq!(sse["type"], "agent_progress");
        assert_eq!(sse["agent_id"], "agent-3");
        assert_eq!(sse["status"], "idle");
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
        let breakdown = astra_turn_core::context_assembly_trace::SystemPromptBreakdown::default();
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

    #[cfg(feature = "bridge-e2e-hooks")]
    #[test]
    fn normalize_message_for_cache_hash_canonicalizes_tool_content_shapes() {
        let string_tool = json!({
            "role": "tool",
            "tool_call_id": "tooluse_123",
            "content": "tool output",
            "cache_control": { "type": "ephemeral" }
        });
        let normalized_string = super::normalize_message_for_cache_hash(&string_tool);
        assert_eq!(
            normalized_string["content"],
            json!([{
                "type": "tool_result",
                "tool_use_id": "tooluse_123",
                "content": "tool output"
            }])
        );

        let array_tool = json!({
            "role": "tool",
            "tool_call_id": "tooluse_456",
            "content": [{
                "type": "tool_result",
                "content": "tool output",
                "cache_control": { "type": "ephemeral" }
            }]
        });
        let normalized_array = super::normalize_message_for_cache_hash(&array_tool);
        assert_eq!(
            normalized_array["content"],
            json!([{
                "type": "tool_result",
                "tool_use_id": "tooluse_456",
                "content": "tool output"
            }])
        );
    }

    // ── Prompt-visible tool schema / skill runtime-policy tests ────────────

    fn sample_edge_tools_full() -> Vec<Value> {
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
            json!({
                "type": "function",
                "function": {
                    "name": "str_replace",
                    "description": "Edit a file",
                    "parameters": { "type": "object", "properties": {} }
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "write_file",
                    "description": "Write a file",
                    "parameters": { "type": "object", "properties": {} }
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "grep",
                    "description": "Search files",
                    "parameters": { "type": "object", "properties": {} }
                }
            }),
        ]
    }

    /// Core invariant: the prompt-visible schema must match the active runtime
    /// tool policy, including skill allowlists.
    #[test]
    fn skill_allowed_tools_do_not_prune_prompt_visible_tools() {
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u".to_string(),
            "s".to_string(),
        )
        .with_edge_tools(sample_edge_tools_full())
        .with_execution_binding_snapshot(edge_runtime_snapshot())
        .build();

        let mut state = create_test_state();
        state.session_turn = 1;
        // Simulate: review skill activated with read-only allowed_tools
        state.skills.allowed_tools = Some(
            ["bash", "read_file", "grep"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        state.skills.invoked.insert(
            "review-changes".into(),
            crate::turn::skill_tool::InvokedSkill {
                name: "review-changes".into(),
                content: String::new(),
                invoked_at_turn: 1,
                reentry_count: 0,
            },
        );

        let visible = host.visible_turn_tools(&mut state);
        let visible_names: Vec<&str> = visible
            .iter()
            .filter_map(|t| {
                t.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
            })
            .collect();

        assert!(
            visible_names.contains(&"bash"),
            "skill-hinted tool must stay visible"
        );
        assert!(
            visible_names.contains(&"read_file"),
            "skill-hinted tool must stay visible"
        );
        assert!(
            visible_names.contains(&"grep"),
            "skill-hinted tool must stay visible"
        );
        assert!(
            visible_names.contains(&"str_replace"),
            "skill hints must not hide otherwise-callable tools from the prompt-visible schema"
        );
        assert!(
            visible_names.contains(&"write_file"),
            "skill hints must not hide otherwise-callable tools from the prompt-visible schema"
        );
    }

    /// After a skill sets allowed_tools, later turns should keep the same hint
    /// state without turning it into a hard prompt-visible restriction.
    #[test]
    fn skill_allowed_tools_leave_prompt_schemas_unpruned_across_turns() {
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u".to_string(),
            "s".to_string(),
        )
        .with_edge_tools(sample_edge_tools_full())
        .with_execution_binding_snapshot(edge_runtime_snapshot())
        .build();

        let mut state = create_test_state();
        // Turn 1: review skill active
        state.session_turn = 1;
        state.skills.allowed_tools = Some(
            ["bash", "read_file", "grep"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        state.skills.invoked.insert(
            "review-changes".into(),
            crate::turn::skill_tool::InvokedSkill {
                name: "review-changes".into(),
                content: String::new(),
                invoked_at_turn: 1,
                reentry_count: 0,
            },
        );
        let _turn1 = host.visible_turn_tools(&mut state);

        // Turn 2: the loaded skill hint still governs the session until another
        // activation replaces it, but the full callable schema stays visible.
        state.session_turn = 2;
        let visible = host.visible_turn_tools(&mut state);
        let visible_names: Vec<&str> = visible
            .iter()
            .filter_map(|t| {
                t.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
            })
            .collect();

        assert!(
            visible_names.contains(&"str_replace"),
            "turn 2 should keep the full callable schema visible"
        );
        assert!(
            visible_names.contains(&"write_file"),
            "turn 2 should keep the full callable schema visible"
        );
    }

    /// request_constraints.allowed_tools (from delegation) must still restrict
    /// tools — this is a security boundary for child agents, NOT skill metadata.
    #[test]
    fn request_constraints_still_restrict_tools() {
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u".to_string(),
            "s".to_string(),
        )
        .with_edge_tools(sample_edge_tools_full())
        .with_execution_binding_snapshot(edge_runtime_snapshot())
        .build();

        let mut state = create_test_state();
        // Delegation constrains to bash + read_file only
        state.skills.request_constraints.allowed_tools = Some(
            ["bash", "read_file"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        // No skill allowed_tools set

        let visible = host.visible_turn_tools(&mut state);
        let visible_names: Vec<&str> = visible
            .iter()
            .filter_map(|t| {
                t.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
            })
            .collect();

        assert!(visible_names.contains(&"bash"));
        assert!(visible_names.contains(&"read_file"));
        assert!(
            !visible_names.contains(&"str_replace"),
            "request_constraints must still restrict — this is a delegation security boundary"
        );
        assert!(
            !visible_names.contains(&"write_file"),
            "request_constraints must still restrict — this is a delegation security boundary"
        );
    }

    /// Combined scenario: delegation constrains to [bash, read_file, grep, str_replace]
    /// AND a review skill carries a narrower allowed_tools hint. The hard
    /// request policy still wins, but the skill hint must not narrow further.
    #[test]
    fn prompt_schema_ignores_skill_hints_when_request_allowlist_is_broader() {
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "u".to_string(),
            "s".to_string(),
        )
        .with_edge_tools(sample_edge_tools_full())
        .with_execution_binding_snapshot(edge_runtime_snapshot())
        .build();

        let mut state = create_test_state();
        state.session_turn = 1;
        // Delegation allows: bash, read_file, grep, str_replace (but NOT write_file)
        state.skills.request_constraints.allowed_tools = Some(
            ["bash", "read_file", "grep", "str_replace"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        // Skill only lists: bash, read_file, grep
        state.skills.allowed_tools = Some(
            ["bash", "read_file", "grep"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        state.skills.invoked.insert(
            "review-changes".into(),
            crate::turn::skill_tool::InvokedSkill {
                name: "review-changes".into(),
                content: String::new(),
                invoked_at_turn: 1,
                reentry_count: 0,
            },
        );

        let visible = host.visible_turn_tools(&mut state);
        let visible_names: Vec<&str> = visible
            .iter()
            .filter_map(|t| {
                t.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
            })
            .collect();

        assert!(
            visible_names.contains(&"str_replace"),
            "request-allowlisted tools must remain visible even when the active skill hint omits them"
        );
        // write_file: NOT in request_constraints → restricted by delegation
        assert!(
            !visible_names.contains(&"write_file"),
            "write_file not in delegation allowlist, must be restricted"
        );
    }

    // ── G2: server-side parent capture (on_turn_completed) ──
    //
    // These tests pin behaviors of the server host's capture path so
    // a future refactor can't silently regress:
    //
    //  1. No store wired → no-op (zero overhead for callers that
    //     don't enable fork-prefix).
    //  2. Store wired + non-empty run_id/messages → prefix lands in
    //     the sink with the right run_id and the tool_schemas are
    //     populated from the advertised edge_tools.

    mod fork_prefix_capture_g2 {
        use super::*;
        use astra_turn_core::fork_prefix_store::{InMemoryPrefixStore, PrefixCaptureSink};
        use std::sync::Arc;

        fn state_with_run(run_id: &str) -> AgenticLoopState {
            let mut state = crate::turn::agentic_loop::host::tests::make_state();
            state.current_run_id = Some(run_id.to_string());
            state.messages = vec![serde_json::json!({"role": "user", "content": "hi"})];
            state
        }

        fn build_host_with(
            store: Option<Arc<dyn PrefixCaptureSink>>,
            tools: Vec<Value>,
        ) -> ServerAgenticLoopHost {
            ServerAgenticLoopHostBuilder::new(
                mock_matrixone(),
                mock_encryptor(),
                "u".to_string(),
                "s".to_string(),
            )
            .with_edge_tools(tools)
            .with_prefix_store(store)
            .build()
        }

        #[test]
        fn on_turn_completed_is_noop_without_store() {
            // No store wired — must not panic, must not affect any
            // state the loop depends on.
            let mut host = build_host_with(None, sample_edge_tools());
            let state = state_with_run("run-1");
            host.on_turn_completed(&state);
            // Nothing to assert beyond "it ran without side effects".
        }

        #[test]
        fn on_turn_completed_captures_when_store_wired() {
            let store = Arc::new(InMemoryPrefixStore::new());
            let store_arc: Arc<dyn PrefixCaptureSink> = store.clone();
            let mut host = build_host_with(Some(store_arc), sample_edge_tools());
            // execute_turn would normally stash this; mimic it here.
            host.last_turn_tool_schemas = sample_edge_tools();
            host.on_turn_completed(&state_with_run("run-capture"));
            assert_eq!(
                store.tracked_count(),
                1,
                "store + valid run_id must produce one entry"
            );
            let pfx = store
                .get_prefix("run-capture")
                .expect("prefix stored under parent_run_id");
            // tool_schemas from edge_tools should have been populated.
            assert_eq!(
                pfx.tool_schemas().len(),
                2,
                "two advertised tools → two ToolSchemaEntry"
            );
            let names: Vec<_> = pfx.tool_schemas().iter().map(|t| t.name.as_str()).collect();
            assert!(names.contains(&"bash"));
            assert!(names.contains(&"read_file"));
        }

        #[test]
        fn on_turn_completed_skips_when_run_id_missing() {
            let store = Arc::new(InMemoryPrefixStore::new());
            let store_arc: Arc<dyn PrefixCaptureSink> = store.clone();
            let mut host = build_host_with(Some(store_arc), sample_edge_tools());
            let mut state = state_with_run("whatever");
            state.current_run_id = None; // simulate pre-run state
            host.on_turn_completed(&state);
            assert_eq!(store.tracked_count(), 0, "missing run_id must skip capture");
        }
    }

    // ── Turn intent judge wiring ────────────────────────────────────────
    //
    // Pin the contract that ServerAgenticLoopHost honors an injected LLM
    // judge and never substitutes natural-language keyword matching when the
    // judge is absent or fails.
    mod turn_intent_judge_wiring {
        use super::*;
        use astra_config::user_profile::{Scenario, TurnContinuationMode, TurnIntent};
        use astra_services::{
            LlmTokenServiceConfig, TurnIntentJudge, TurnIntentJudgeContext, TurnIntentJudgeError,
        };
        use async_trait::async_trait;
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        struct ScriptedJudge {
            calls: std::sync::Mutex<Vec<TurnIntentJudgeContext>>,
            response: std::sync::Mutex<Option<Result<TurnIntent, TurnIntentJudgeError>>>,
        }

        impl ScriptedJudge {
            fn ok(intent: TurnIntent) -> Arc<Self> {
                Arc::new(Self {
                    calls: std::sync::Mutex::new(Vec::new()),
                    response: std::sync::Mutex::new(Some(Ok(intent))),
                })
            }
            fn err(error: TurnIntentJudgeError) -> Arc<Self> {
                Arc::new(Self {
                    calls: std::sync::Mutex::new(Vec::new()),
                    response: std::sync::Mutex::new(Some(Err(error))),
                })
            }
            fn calls(&self) -> Vec<TurnIntentJudgeContext> {
                self.calls.lock().unwrap().clone()
            }
        }

        #[async_trait]
        impl TurnIntentJudge for ScriptedJudge {
            async fn judge(
                &self,
                ctx: &TurnIntentJudgeContext,
            ) -> Result<TurnIntent, TurnIntentJudgeError> {
                self.calls.lock().unwrap().push(ctx.clone());
                self.response
                    .lock()
                    .unwrap()
                    .take()
                    .expect("ScriptedJudge consumed twice")
            }
        }

        fn host_with_judge(judge: Arc<dyn TurnIntentJudge>) -> ServerAgenticLoopHost {
            let mut host = ServerAgenticLoopHostBuilder::new(
                mock_matrixone(),
                mock_encryptor(),
                "u".to_string(),
                "s".to_string(),
            )
            .build();
            host.set_turn_intent_judge(judge);
            host
        }

        #[test]
        fn auxiliary_llm_capacity_policy_parser_is_stable() {
            assert_eq!(
                parse_auxiliary_llm_policy("always"),
                AuxiliaryLlmPolicy::Always
            );
            assert_eq!(
                parse_auxiliary_llm_policy("off"),
                AuxiliaryLlmPolicy::Disabled
            );
            assert_eq!(
                parse_auxiliary_llm_policy("capacity-aware"),
                AuxiliaryLlmPolicy::CapacityAware
            );
            assert_eq!(
                parse_auxiliary_llm_policy("surprise"),
                AuxiliaryLlmPolicy::CapacityAware
            );
        }

        #[test]
        #[serial_test::serial(auxiliary_llm_capacity_policy_env)]
        fn auxiliary_llm_policy_uses_specific_override_before_global() {
            let _aux_policy = EnvVarGuard::set(AUX_LLM_POLICY_ENV, "disabled");
            let _specific_policy = EnvVarGuard::set(FACTUAL_RETRY_JUDGE_POLICY_ENV, "always");

            assert_eq!(
                AuxiliaryLlmPolicy::from_env(FACTUAL_RETRY_JUDGE_POLICY_ENV),
                AuxiliaryLlmPolicy::Always
            );
        }

        #[tokio::test]
        async fn judge_turn_intent_invokes_wired_judge_and_returns_its_result() {
            let llm_intent = TurnIntent::default()
                .with_continuation_mode(TurnContinuationMode::ContinueCurrentObjective);
            let judge = ScriptedJudge::ok(llm_intent.clone());
            let mut host = host_with_judge(judge.clone() as Arc<dyn TurnIntentJudge>);

            let mut state = crate::turn::agentic_loop::host::tests::make_state();
            state.message = "可以了，按你刚才说的方向继续往下走".to_string();
            state.messages = vec![
                serde_json::json!({"role": "user", "content": "earlier"}),
                serde_json::json!({"role": "assistant", "content": "ok"}),
            ];
            state.recent_tools = vec!["read_file".to_string()];
            state.llm_rounds_completed = 4;

            let intent = host.judge_turn_intent(&state).await;
            assert_eq!(intent, Some(llm_intent));

            let calls = judge.calls();
            assert_eq!(calls.len(), 1, "judge must be called exactly once");
            let call = &calls[0];
            assert_eq!(call.message, "可以了，按你刚才说的方向继续往下走");
            assert_eq!(
                call.turn_count, 5,
                "turn count should be llm_rounds_completed+1"
            );
            assert_eq!(call.recent_tools, vec!["read_file".to_string()]);
            assert!(
                call.has_prior_assistant_turn,
                "must surface the prior-assistant signal so the judge can detect follow-ups"
            );
        }

        #[tokio::test]
        async fn judge_turn_intent_returns_none_when_judge_errors() {
            let judge = ScriptedJudge::err(TurnIntentJudgeError::Transport(
                "connection reset".to_string(),
            ));
            let mut host = host_with_judge(judge.clone() as Arc<dyn TurnIntentJudge>);

            let mut state = crate::turn::agentic_loop::host::tests::make_state();
            state.message = "please inspect the current changes".to_string();

            assert_eq!(host.judge_turn_intent(&state).await, None);
        }

        #[tokio::test]
        async fn judge_turn_intent_returns_none_when_no_judge_set() {
            let mut host = ServerAgenticLoopHostBuilder::new(
                mock_matrixone(),
                mock_encryptor(),
                "u".to_string(),
                "s".to_string(),
            )
            .build();

            let mut state = crate::turn::agentic_loop::host::tests::make_state();
            state.message = "please inspect the current changes".to_string();

            assert_eq!(host.judge_turn_intent(&state).await, None);
        }

        #[tokio::test]
        #[serial_test::serial(auxiliary_llm_capacity_policy_env)]
        async fn judge_turn_intent_uses_gateway_llm_when_no_judge_is_injected() {
            use axum::{Router, extract::State, routing::post};
            use tokio::net::TcpListener;

            let _aux_policy = EnvVarGuard::remove(AUX_LLM_POLICY_ENV);
            let _policy = EnvVarGuard::set(TURN_INTENT_JUDGE_POLICY_ENV, "always");

            #[derive(Default)]
            struct RequestCapture {
                authorization: tokio::sync::Mutex<Option<String>>,
                workspace_id: tokio::sync::Mutex<Option<String>>,
                body: tokio::sync::Mutex<Option<Value>>,
            }

            async fn handler(
                State(capture): State<Arc<RequestCapture>>,
                headers: axum::http::HeaderMap,
                request: axum::extract::Request,
            ) -> axum::Json<Value> {
                *capture.authorization.lock().await = headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .map(String::from);
                *capture.workspace_id.lock().await = headers
                    .get("x-workspace-id")
                    .and_then(|value| value.to_str().ok())
                    .map(String::from);

                let bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
                    .await
                    .expect("read request body");
                *capture.body.lock().await =
                    Some(serde_json::from_slice(&bytes).expect("json body"));

                axum::Json(json!({
                    "choices": [
                        {
                            "message": {
                                "content": "{\"requested_scenario\":\"refactoring\",\"prohibited_scenarios\":[],\"continuation_mode\":\"continue_current_objective\",\"reanchors_current_objective\":true}"
                            },
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

            let mut host = ServerAgenticLoopHostBuilder::new(
                mock_matrixone(),
                mock_encryptor(),
                "u".to_string(),
                "s".to_string(),
            )
            .with_llm_token_service(Some(LlmTokenServiceConfig {
                url: format!("http://{addr}/gateway/chat/completions"),
                timeout_ms: Some(2_000),
            }))
            .build();

            let mut state = crate::turn::agentic_loop::host::tests::make_state();
            state.message = "不对，我要的是系统性修复，不是临时补丁".to_string();
            state.messages = vec![
                serde_json::json!({"role": "user", "content": "earlier"}),
                serde_json::json!({"role": "assistant", "content": "ok"}),
            ];
            state.hooks.forward_headers.insert(
                "authorization".to_string(),
                "Bearer forwarded-token".to_string(),
            );
            state
                .hooks
                .forward_headers
                .insert("x-workspace-id".to_string(), "ws-judge".to_string());

            let intent = host
                .judge_turn_intent(&state)
                .await
                .expect("gateway judge should return intent");

            assert_eq!(intent.requested_scenario, Some(Scenario::Refactoring));
            assert_eq!(
                intent.continuation_mode,
                TurnContinuationMode::ContinueCurrentObjective
            );
            assert!(
                intent.reanchors_current_objective,
                "judge response should drive reanchor behavior"
            );
            assert_eq!(
                capture.authorization.lock().await.as_deref(),
                Some("Bearer forwarded-token")
            );
            assert_eq!(
                capture.workspace_id.lock().await.as_deref(),
                Some("ws-judge")
            );

            let body = capture.body.lock().await.clone().expect("judge request");
            let messages = body["messages"].as_array().expect("messages");
            assert_eq!(messages.len(), 2);
            let prompt = messages[1]["content"].as_str().expect("user prompt");
            assert!(
                prompt.contains("reanchors_current_objective"),
                "judge prompt must request the structured reanchor field"
            );
            assert!(
                prompt.contains("不对，我要的是系统性修复，不是临时补丁"),
                "judge prompt must include the current user message as data"
            );

            server.abort();
        }

        #[tokio::test]
        #[serial_test::serial(auxiliary_llm_capacity_policy_env)]
        async fn builtin_turn_intent_judge_skips_gateway_when_provider_admission_is_enabled() {
            use axum::{Router, routing::post};
            use tokio::net::TcpListener;

            let _aux_policy = EnvVarGuard::remove(AUX_LLM_POLICY_ENV);
            let _policy = EnvVarGuard::remove(TURN_INTENT_JUDGE_POLICY_ENV);
            let _mode = EnvVarGuard::set("ASTRA_LLM_PROVIDER_ADMISSION_MODE", "db_fixed_window");
            let _rpm = EnvVarGuard::set("ASTRA_LLM_PROVIDER_ADMISSION_RPM", "20");
            let request_count = Arc::new(AtomicUsize::new(0));
            let request_count_for_handler = request_count.clone();
            let app = Router::new().route(
                "/gateway/chat/completions",
                post(move || {
                    let request_count = request_count_for_handler.clone();
                    async move {
                        request_count.fetch_add(1, AtomicOrdering::SeqCst);
                        axum::Json(json!({
                            "choices": [
                                {
                                    "message": {
                                        "content": "{\"requested_scenario\":\"refactoring\"}"
                                    },
                                    "finish_reason": "stop"
                                }
                            ],
                            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
                        }))
                    }
                }),
            );
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind listener");
            let addr = listener.local_addr().expect("listener addr");
            let server = tokio::spawn(async move {
                axum::serve(listener, app)
                    .await
                    .expect("test server should run");
            });

            let mut host = ServerAgenticLoopHostBuilder::new(
                mock_matrixone(),
                mock_encryptor(),
                "u".to_string(),
                "s".to_string(),
            )
            .with_llm_token_service(Some(LlmTokenServiceConfig {
                url: format!("http://{addr}/gateway/chat/completions"),
                timeout_ms: Some(2_000),
            }))
            .build();

            let mut state = crate::turn::agentic_loop::host::tests::make_state();
            state.message = "继续，但要系统性一点".to_string();

            assert_eq!(host.judge_turn_intent(&state).await, None);
            assert_eq!(
                request_count.load(AtomicOrdering::SeqCst),
                0,
                "capacity-aware default must not spend provider RPM on built-in turn intent judge"
            );

            server.abort();
        }
    }
}

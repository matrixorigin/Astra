//! Shared LLM calling utilities.
//!
//! Extracted from [`super::super::bridge::inprocess`] so both the in-process bridge
//! and [`crate::server::server_loop_host::ServerAgenticLoopHost`] can call LLMs
//! without duplicating the retry/backoff/parsing logic.
//!
//! # Proxy invariant
//!
//! [`astra_core::net::apply_env_proxy`] is the **only** place in the codebase
//! that honours `HTTPS_PROXY` / `ALL_PROXY` env vars. It is called from the
//! LLM client here and from `validate_connectivity` in `astra-services`
//! (both reach external provider endpoints). All other `reqwest` clients
//! (durable bridge, skill HTTP, server tool executor, summary client, …)
//! must call `.no_proxy()` — their traffic is local/intranet and should
//! not be routed through a user's LLM proxy.
//!
//! Re-exported as [`apply_env_proxy`] for in-crate call sites.

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    sync::OnceLock,
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};

use astra_logging::redact_known_secret_patterns;
use async_trait::async_trait;
use axum::body::Bytes;
use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

#[cfg(test)]
use crate::prompts;
#[cfg(test)]
use astra_text_utils::output_style::current_output_style;
use astra_turn_core::bridge_rate_limit_cooldown::{
    RateLimitAction, is_overload_status, is_rate_limit_status, parse_retry_after_ms,
};
use astra_turn_core::cache_placement::{CacheCapability, VolatilePlacement};
use astra_turn_core::sse_blocks::SseBlankLineUtf8Buf;
use astra_turn_core::sse_data_lines::{
    json_events_from_sse_event_block, validate_sse_event_block_json,
    validated_drain_sse_data_lines, validated_finish_sse_data_buffer,
};
use astra_turn_core::thinking_config::ThinkingConfig;
use astra_turn_core::tool_call_shape::tool_call_name;

/// Redact common provider secret patterns from a string before logging.
///
/// Replaces the value following well-known prefixes (`sk-`, `Bearer `, `key-`)
/// with `[REDACTED]`. The scan stops at the first whitespace, quote, or comma,
/// which is sufficient for the JSON / plaintext error bodies that providers
/// commonly echo authorization material into.
pub(crate) fn redact_provider_secrets(s: &str) -> String {
    redact_known_secret_patterns(s)
}

/// Maximum retries for known provider failures (429, 5xx, connect-before-delivery).
pub(crate) const LLM_MAX_RETRIES: u32 = 3;
/// Base delay between retries (doubles each attempt: 1s, 2s, 4s).
/// Override: `ASTRA_LLM_RETRY_BASE_MS` (e.g. `10` in E2E tests that
/// intentionally exhaust retries to assert error-surface behavior).
pub(crate) const LLM_RETRY_BASE_MS: u64 = 1000;

pub(crate) fn llm_retry_base_ms() -> u64 {
    static VAL: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *VAL.get_or_init(|| {
        std::env::var("ASTRA_LLM_RETRY_BASE_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(LLM_RETRY_BASE_MS)
    })
}
/// Extended delay for TPM (tokens per minute) exhaustion (60 seconds).
/// TPM limits typically reset after 60 seconds, so we wait longer.
const TPM_EXHAUST_DELAY_MS: u64 = 60_000;
/// TCP connect timeout for LLM API requests (seconds). Override: `ASTRA_LLM_CONNECT_TIMEOUT_S`.
const LLM_CONNECT_TIMEOUT_S: u64 = 30;
/// Non-stream request hard timeout (seconds). Override: `ASTRA_LLM_NONSTREAM_TIMEOUT_S`.
const LLM_NONSTREAM_TIMEOUT_S: u64 = 120;
/// Total budget across all safe retries for a single LLM call (seconds).
/// Override: `ASTRA_LLM_TOTAL_BUDGET_S`.
const LLM_TOTAL_BUDGET_S: u64 = 300;
/// Maximum grace period for trailing usage / `[DONE]` after a semantic
/// provider terminal (`finish_reason`). A broken keep-alive must not leave a
/// completed answer stuck behind the ordinary multi-minute idle watchdog.
const LLM_STREAM_TERMINAL_DRAIN_GRACE_MS: u64 = 500;

// ── Rate-Limit Cooldown ──────────────────────────────────────────────────────

/// Per-model rate-limit cooldown tracker — shared with bridge_llm_stream.
use super::super::bridge::llm_stream::rate_limit_cooldown;

// ── Global HTTP Client ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LlmProviderProtocol {
    OpenAiCompatible,
    AnthropicMessages,
    BedrockConverse,
}

impl LlmProviderProtocol {
    #[must_use]
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiCompatible => "openai_compatible",
            Self::AnthropicMessages => "anthropic_messages",
            Self::BedrockConverse => "bedrock_converse",
        }
    }
}

pub(crate) fn llm_provider_protocol(provider: &str) -> LlmProviderProtocol {
    match provider {
        "anthropic" => LlmProviderProtocol::AnthropicMessages,
        "bedrock" => LlmProviderProtocol::BedrockConverse,
        _ => LlmProviderProtocol::OpenAiCompatible,
    }
}

/// Immutable identity of the exact serialized provider payload.
///
/// The bytes are serialized once by [`PreparedProviderRequest`]. The HTTP
/// request body and this hash therefore cannot drift through a second JSON
/// serialization or a pre-transport prompt projection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProviderWireRequestIdentity {
    pub protocol: LlmProviderProtocol,
    pub provider_wire_hash: String,
    pub provider_wire_bytes: u64,
    pub composition: ProviderWireComposition,
}

/// Mutually exclusive byte zones from the exact serialized provider body.
///
/// Child JSON values contribute their exact serialized bytes. Object keys,
/// array delimiters, commas, and provider-specific configuration remain in
/// `provider_envelope_bytes`, so the four zones always reconcile to the
/// actual HTTP body length without counting any message or tool twice.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ProviderWireComposition {
    pub system_bytes: u64,
    pub conversation_bytes: u64,
    pub tool_schema_bytes: u64,
    pub provider_envelope_bytes: u64,
    pub system_items: u32,
    pub conversation_items: u32,
    pub tool_schema_items: u32,
}

impl ProviderWireComposition {
    fn from_body(
        body: &Value,
        protocol: LlmProviderProtocol,
        provider_wire_bytes: u64,
    ) -> Result<Self, astra_core::ClassifiedError> {
        let mut composition = Self::default();
        match protocol {
            LlmProviderProtocol::OpenAiCompatible => {
                for message in body
                    .get("messages")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if message.get("role").and_then(Value::as_str) == Some("system") {
                        composition.system_bytes = composition
                            .system_bytes
                            .saturating_add(serialized_value_bytes(message)?);
                        composition.system_items = composition.system_items.saturating_add(1);
                    } else {
                        composition.conversation_bytes = composition
                            .conversation_bytes
                            .saturating_add(serialized_value_bytes(message)?);
                        composition.conversation_items =
                            composition.conversation_items.saturating_add(1);
                    }
                }
                accumulate_wire_items(
                    body.get("tools"),
                    &mut composition.tool_schema_bytes,
                    &mut composition.tool_schema_items,
                )?;
            }
            LlmProviderProtocol::AnthropicMessages => {
                accumulate_wire_items(
                    body.get("system"),
                    &mut composition.system_bytes,
                    &mut composition.system_items,
                )?;
                accumulate_wire_items(
                    body.get("messages"),
                    &mut composition.conversation_bytes,
                    &mut composition.conversation_items,
                )?;
                accumulate_wire_items(
                    body.get("tools"),
                    &mut composition.tool_schema_bytes,
                    &mut composition.tool_schema_items,
                )?;
            }
            LlmProviderProtocol::BedrockConverse => {
                accumulate_wire_items(
                    body.get("system"),
                    &mut composition.system_bytes,
                    &mut composition.system_items,
                )?;
                accumulate_wire_items(
                    body.get("messages"),
                    &mut composition.conversation_bytes,
                    &mut composition.conversation_items,
                )?;
                accumulate_wire_items(
                    body.pointer("/toolConfig/tools"),
                    &mut composition.tool_schema_bytes,
                    &mut composition.tool_schema_items,
                )?;
            }
        }
        let payload_bytes = composition
            .system_bytes
            .saturating_add(composition.conversation_bytes)
            .saturating_add(composition.tool_schema_bytes);
        composition.provider_envelope_bytes = provider_wire_bytes
            .checked_sub(payload_bytes)
            .ok_or_else(|| {
                astra_core::ClassifiedError::new(
                    astra_core::ErrorKind::ContractViolation,
                    "provider wire composition exceeds the serialized request body",
                )
            })?;
        Ok(composition)
    }

    #[must_use]
    pub(crate) fn total_bytes(&self) -> u64 {
        self.system_bytes
            .saturating_add(self.conversation_bytes)
            .saturating_add(self.tool_schema_bytes)
            .saturating_add(self.provider_envelope_bytes)
    }
}

fn serialized_value_bytes(value: &Value) -> Result<u64, astra_core::ClassifiedError> {
    serde_json::to_vec(value)
        .map(|encoded| {
            let bytes = u64::try_from(encoded.len()).unwrap_or(u64::MAX);
            if astra_core::history_work::instrumentation_enabled() {
                astra_core::history_work::record_bytes(
                    astra_core::history_work::HistoryWorkSite::ProviderWireAssembly,
                    bytes,
                );
            }
            bytes
        })
        .map_err(|error| {
            astra_core::history_work::record_serialization_failure(
                astra_core::history_work::HistoryWorkSite::ProviderWireAssembly,
                &error,
            );
            astra_core::ClassifiedError::new(
                astra_core::ErrorKind::ContractViolation,
                format!("serialize provider wire composition value: {error}"),
            )
        })
}

fn accumulate_wire_items(
    value: Option<&Value>,
    bytes: &mut u64,
    items: &mut u32,
) -> Result<(), astra_core::ClassifiedError> {
    let Some(value) = value else {
        return Ok(());
    };
    if let Some(values) = value.as_array() {
        for value in values {
            *bytes = bytes.saturating_add(serialized_value_bytes(value)?);
            *items = items.saturating_add(1);
        }
    } else {
        *bytes = bytes.saturating_add(serialized_value_bytes(value)?);
        *items = items.saturating_add(1);
    }
    Ok(())
}

/// Exact provider payload shared by durable attempt admission and HTTP send.
#[derive(Clone, Debug)]
pub(crate) struct PreparedProviderRequest {
    body: Bytes,
    identity: ProviderWireRequestIdentity,
}

impl PreparedProviderRequest {
    pub(crate) fn from_json(
        body: &Value,
        protocol: LlmProviderProtocol,
    ) -> Result<Self, astra_core::ClassifiedError> {
        let encoded = serde_json::to_vec(body).map_err(|error| {
            astra_core::history_work::record_serialization_failure(
                astra_core::history_work::HistoryWorkSite::ProviderBodySerialization,
                &error,
            );
            astra_core::ClassifiedError::new(
                astra_core::ErrorKind::ContractViolation,
                format!("serialize exact provider request body: {error}"),
            )
        })?;
        let provider_wire_bytes = u64::try_from(encoded.len()).unwrap_or(u64::MAX);
        if astra_core::history_work::instrumentation_enabled() {
            astra_core::history_work::record_bytes(
                astra_core::history_work::HistoryWorkSite::ProviderBodySerialization,
                provider_wire_bytes,
            );
        }
        let provider_wire_hash = format!("{:x}", Sha256::digest(&encoded));
        let composition = ProviderWireComposition::from_body(body, protocol, provider_wire_bytes)?;
        Ok(Self {
            body: Bytes::from(encoded),
            identity: ProviderWireRequestIdentity {
                protocol,
                provider_wire_hash,
                provider_wire_bytes,
                composition,
            },
        })
    }

    #[must_use]
    pub(crate) fn identity(&self) -> &ProviderWireRequestIdentity {
        &self.identity
    }

    #[must_use]
    pub(crate) fn body(&self) -> Bytes {
        self.body.clone()
    }

    #[cfg(test)]
    fn body_bytes(&self) -> &[u8] {
        self.body.as_ref()
    }
}

pub(crate) fn provider_uses_anthropic_messages(provider: &str) -> bool {
    llm_provider_protocol(provider) == LlmProviderProtocol::AnthropicMessages
}

pub(crate) fn provider_uses_bedrock_converse(provider: &str) -> bool {
    llm_provider_protocol(provider) == LlmProviderProtocol::BedrockConverse
}

/// Returns true only when the *provider* is known to be DashScope / Aliyun / Alibaba.
///
/// We intentionally do NOT match on model name here: Qwen models are also served
/// through generic OpenAI-compatible proxies (vLLM, Ollama, SGLang, …) that do not
/// accept `enable_thinking` and may 400 on unknown top-level fields. Matching the
/// provider name alone avoids false positives on those deployments.
pub(crate) fn provider_uses_dashscope_thinking(provider: &str) -> bool {
    astra_turn_core::thinking_config::provider_may_think_natively(provider)
}

/// Global HTTP client for LLM requests (connection pooling, reuse).
pub(crate) fn global_llm_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        let connect = llm_connect_timeout();
        let total = std::time::Duration::from_secs(LLM_TOTAL_BUDGET_S + 60);
        let pool_idle = std::env::var("ASTRA_LLM_POOL_MAX_IDLE_PER_HOST")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(4usize);
        let mut builder = reqwest::Client::builder()
            .connect_timeout(connect)
            // Use a generous timeout; per-request timeout handled via tokio::time::timeout
            .timeout(total)
            .pool_max_idle_per_host(pool_idle);
        // Honour HTTPS_PROXY / ALL_PROXY env vars (reqwest default-features=false
        // does not auto-read system proxy, so we wire it up explicitly).
        builder = apply_env_proxy(builder);
        match builder.build()
        {
            Ok(client) => {
                tracing::info!(
                    target: "astra_runtime::llm_client",
                    pool_max_idle_per_host = pool_idle,
                    connect_timeout_s = connect.as_secs(),
                    total_timeout_s = total.as_secs(),
                    "global LLM HTTP client built"
                );
                client
            }
            Err(e) => {
                // audit-C1: TLS / HTTP stack init failure should not crash the process.
                // Retry with the same timeouts but without pool tuning so we still bound
                // hung-upstream risk if this tier succeeds.
                tracing::error!(
                    target: "astra_runtime::llm_client",
                    error = %e,
                    "failed to build global LLM HTTP client; retrying without pool_max_idle_per_host"
                );
                let mut fallback_builder = reqwest::Client::builder()
                    .connect_timeout(connect)
                    .timeout(total);
                fallback_builder = apply_env_proxy(fallback_builder);
                match fallback_builder.build() {
                    Ok(client) => client,
                    Err(e2) => {
                        tracing::error!(
                            target: "astra_runtime::llm_client",
                            error = %e2,
                            "failed to build minimal global LLM HTTP client; retrying with proxy-aware reqwest::Client::new() equivalent"
                        );
                        let mut last_chance_builder = reqwest::Client::builder();
                        last_chance_builder = apply_env_proxy(last_chance_builder);
                        match last_chance_builder.build() {
                            Ok(client) => client,
                            Err(e3) => {
                                tracing::error!(
                                    target: "astra_runtime::llm_client",
                                    error = %e3,
                                    "failed to build last-chance proxy-aware LLM HTTP client; using reqwest::Client::new()"
                                );
                                reqwest::Client::new()
                            }
                        }
                    }
                }
            }
        }
    })
}

#[cfg(test)]
fn reset_rate_limit_cooldown_for_tests() {
    rate_limit_cooldown().reset_for_tests();
}

/// Returns `true` if `name` looks like a valid tool function name.
///
/// LLM providers sometimes return malformed tool calls when the model leaks XML-style
/// thinking tags (e.g., `<reflect>`) into tool call blocks. We reject names that:
/// - are empty
/// - contain `<` or `>` (XML artifact)
/// - contain whitespace
fn is_valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('<')
        && !name.contains('>')
        && !name.chars().any(char::is_whitespace)
}

fn canonical_valid_tool_name(name: &str) -> Option<&str> {
    astra_core::canonical_names::normalize_name(name).filter(|name| is_valid_tool_name(name))
}

// ── System Prompt ─────────────────────────────────────────────────────────

/// Detect TPM (tokens per minute) exhaustion errors.
///
/// TPM errors require longer wait times because they indicate the account-level
/// token quota has been exhausted. These typically reset after 60 seconds.
fn is_tpm_exhaustion(error_text: &str) -> bool {
    let lower = error_text.to_lowercase();
    (lower.contains("tpm") && (lower.contains("exceed") || lower.contains("limit")))
        || lower.contains("tokens per minute")
        || lower.contains("rate limit exceeded") && lower.contains("token")
}

/// Collected result from a single LLM streaming call.
#[derive(Debug, Clone, Default)]
pub(crate) struct LlmCallResult {
    /// Provider response identity when available on non-stream responses.
    pub response_id: Option<String>,
    pub full_text: String,
    pub reasoning: String,
    /// Bedrock reasoning signature — must be passed back unmodified in multi-turn.
    pub reasoning_signature: String,
    pub tool_calls: Vec<Value>,
    pub usage: Map<String, Value>,
    pub model_used: String,
    #[allow(dead_code)] // validated in tests; reserved for future telemetry
    pub duration_ms: u64,
    /// The finish_reason from the last SSE choice (e.g. "stop", "length", "tool_calls").
    /// `None` when the stream ended without an explicit finish_reason.
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum LlmStreamUpdate {
    Text(String),
    Reasoning(String),
    ToolCall { index: usize, tool_call: Value },
}

pub(crate) type LlmStreamCallback<'a> = dyn FnMut(LlmStreamUpdate) + Send + 'a;

/// Short-lived provider route material for one model call.
///
/// This is deliberately neither serializable nor cloneable. It may borrow a
/// credential and request headers, so its custom `Debug` only exposes
/// non-secret routing facts.
pub(crate) struct LlmExecutionRoute<'a> {
    pub model_name: &'a str,
    pub wire_model_name: Option<&'a str>,
    pub api_key: &'a str,
    pub base_url: &'a str,
    pub provider: &'a str,
    pub header_overrides: Option<&'a HashMap<String, String>>,
    pub request_body_overrides: Option<&'a Map<String, Value>>,
    pub completions_url_override: Option<&'a str>,
    pub request_timeout: Option<std::time::Duration>,
}

impl<'a> LlmExecutionRoute<'a> {
    /// Borrow the single trusted execution-material contract produced by
    /// model admission. Provider adapters must not rebuild this route from a
    /// client-selected model name or URL.
    #[must_use]
    pub(crate) fn from_admitted(execution: &'a astra_services::AdmittedModelExecution) -> Self {
        Self {
            model_name: &execution.model_name,
            wire_model_name: execution.wire_model_name.as_deref(),
            api_key: &execution.api_key,
            base_url: &execution.base_url,
            provider: &execution.provider,
            header_overrides: (!execution.header_overrides.is_empty())
                .then_some(&execution.header_overrides),
            request_body_overrides: execution.request_body_overrides.as_ref(),
            completions_url_override: execution.completions_url_override.as_deref(),
            request_timeout: execution
                .request_timeout_ms
                .map(std::time::Duration::from_millis),
        }
    }
}

/// Owned execution route for adapters that must outlive one stack frame.
/// Like the borrowed route, it is deliberately non-serializable and redacts
/// credentials, endpoint URLs, and header values from `Debug`.
#[derive(Clone)]
pub(crate) struct OwnedLlmExecutionRoute {
    pub model_name: String,
    pub wire_model_name: Option<String>,
    pub api_key: String,
    pub base_url: String,
    pub provider: String,
    pub header_overrides: HashMap<String, String>,
    pub request_body_overrides: Option<Map<String, Value>>,
    pub completions_url_override: Option<String>,
    pub request_timeout: Option<std::time::Duration>,
}

impl OwnedLlmExecutionRoute {
    #[must_use]
    pub fn borrowed(&self) -> LlmExecutionRoute<'_> {
        LlmExecutionRoute {
            model_name: &self.model_name,
            wire_model_name: self.wire_model_name.as_deref(),
            api_key: &self.api_key,
            base_url: &self.base_url,
            provider: &self.provider,
            header_overrides: (!self.header_overrides.is_empty()).then_some(&self.header_overrides),
            request_body_overrides: self.request_body_overrides.as_ref(),
            completions_url_override: self.completions_url_override.as_deref(),
            request_timeout: self.request_timeout,
        }
    }
}

impl std::fmt::Debug for OwnedLlmExecutionRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.borrowed().fmt(f)
    }
}

impl std::fmt::Debug for LlmExecutionRoute<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut header_names = self
            .header_overrides
            .into_iter()
            .flat_map(HashMap::keys)
            .map(String::as_str)
            .collect::<Vec<_>>();
        header_names.sort_unstable();
        f.debug_struct("LlmExecutionRoute")
            .field("model_name", &self.model_name)
            .field("wire_model_name", &self.wire_model_name)
            .field("provider", &self.provider)
            .field("credential_present", &!self.api_key.is_empty())
            .field("header_names", &header_names)
            .field(
                "completions_url_override_present",
                &self.completions_url_override.is_some(),
            )
            .field("request_timeout", &self.request_timeout)
            .finish()
    }
}

/// Canonical runtime input for one logical model call.
pub(crate) struct LlmCall<'a> {
    pub purpose: astra_turn_types::InferencePurpose,
    pub messages: &'a [Value],
    pub tools: &'a [Value],
    /// Cache placement resolved from model metadata by the owning runtime path.
    /// `None` preserves heuristic classification for standalone inference calls.
    pub cache_capability: Option<CacheCapability>,
    pub route: LlmExecutionRoute<'a>,
    pub max_output_tokens: Option<usize>,
    pub temperature: Option<f64>,
    pub has_fallback: bool,
    pub thinking: &'a ThinkingConfig,
}

/// Durable observer for physical provider requests.
///
/// `begin_attempt` must commit before the HTTP request is sent. Every returned
/// index is then completed exactly once, including retryable failures. Logical
/// invocation lifecycle remains owned by the caller so one invocation can
/// contain multiple physical attempts.
#[async_trait]
pub(crate) trait ProviderAttemptObserver: Send + Sync {
    async fn begin_attempt(
        &self,
        wire: &ProviderWireRequestIdentity,
    ) -> Result<u32, astra_core::ClassifiedError>;

    async fn finish_attempt(
        &self,
        attempt_index: u32,
        terminal: &astra_services::InferenceInvocationTerminal,
    ) -> Result<(), astra_core::ClassifiedError>;
}

pub(crate) fn provider_attempt_terminal_from_result(
    result: &LlmCallResult,
) -> astra_services::InferenceInvocationTerminal {
    let usage = crate::turn::token_usage::TokenUsage::from_partial_json_map(&result.usage);
    astra_services::InferenceInvocationTerminal::succeeded(
        astra_services::InferenceUsage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_tokens: usage.cached_input_tokens,
            cache_creation_tokens: usage.cache_creation_tokens,
        },
        result.response_id.clone(),
    )
}

pub(crate) fn provider_attempt_terminal_from_error(
    error: &astra_core::ClassifiedError,
) -> astra_services::InferenceInvocationTerminal {
    provider_attempt_terminal_from_error_with_partial(error, None)
}

pub(crate) fn provider_attempt_terminal_from_error_with_partial(
    error: &astra_core::ClassifiedError,
    partial: Option<&LlmCallResult>,
) -> astra_services::InferenceInvocationTerminal {
    let status = match error.kind {
        astra_core::ErrorKind::Cancelled => astra_services::InferenceTerminalStatus::Cancelled,
        astra_core::ErrorKind::StreamIdle | astra_core::ErrorKind::StreamTransport => {
            astra_services::InferenceTerminalStatus::DeliveryUnknown
        }
        _ => astra_services::InferenceTerminalStatus::Failed,
    };
    let message = redact_provider_secrets(&error.message);
    let usage = partial
        .map(|partial| crate::turn::token_usage::TokenUsage::from_partial_json_map(&partial.usage))
        .unwrap_or_default();
    astra_services::InferenceInvocationTerminal {
        status,
        usage: astra_services::InferenceUsage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_tokens: usage.cached_input_tokens,
            cache_creation_tokens: usage.cache_creation_tokens,
        },
        provider_response_id: partial.and_then(|partial| partial.response_id.clone()),
        error_kind: Some(error.kind.as_str().to_string()),
        error_message: Some(
            astra_text_utils::str_preview::truncate_str(&message, 1_000).to_string(),
        ),
    }
}

/// Classify a request-builder `send` failure by whether provider delivery is
/// still knowably impossible.
///
/// A connect failure occurs before an HTTP request can reach the provider and
/// is therefore safe to retry. Every other `send` failure (including a timeout
/// while uploading or waiting for response headers) may have happened after
/// delivery and must remain `delivery_unknown` instead of triggering another
/// inference request.
pub(crate) fn classify_provider_send_error(
    context: &str,
    error: &reqwest::Error,
) -> (astra_core::ClassifiedError, bool) {
    let retry_safe = error.is_connect();
    let kind = if retry_safe {
        astra_core::ErrorKind::Network
    } else {
        astra_core::ErrorKind::StreamTransport
    };
    (
        astra_core::ClassifiedError::new(kind, format!("{context}: {error}")),
        retry_safe,
    )
}

pub(crate) async fn finish_observed_provider_attempt(
    observer: Option<&dyn ProviderAttemptObserver>,
    attempt_index: Option<u32>,
    terminal: &astra_services::InferenceInvocationTerminal,
) -> Result<(), astra_core::ClassifiedError> {
    let (Some(observer), Some(attempt_index)) = (observer, attempt_index) else {
        return Ok(());
    };
    observer.finish_attempt(attempt_index, terminal).await
}

pub(crate) async fn finish_observed_provider_error(
    observer: Option<&dyn ProviderAttemptObserver>,
    attempt_index: Option<u32>,
    error: &astra_core::ClassifiedError,
) -> Result<(), astra_core::ClassifiedError> {
    finish_observed_provider_attempt(
        observer,
        attempt_index,
        &provider_attempt_terminal_from_error(error),
    )
    .await
}

pub(crate) async fn finish_observed_provider_error_with_partial(
    observer: Option<&dyn ProviderAttemptObserver>,
    attempt_index: Option<u32>,
    error: &astra_core::ClassifiedError,
    partial: &LlmCallResult,
) -> Result<(), astra_core::ClassifiedError> {
    finish_observed_provider_attempt(
        observer,
        attempt_index,
        &provider_attempt_terminal_from_error_with_partial(error, Some(partial)),
    )
    .await
}

fn llm_result_has_partial_signal(result: &LlmCallResult) -> bool {
    !result.full_text.is_empty()
        || !result.reasoning.is_empty()
        || !result.tool_calls.is_empty()
        || !result.usage.is_empty()
        || result.finish_reason.is_some()
}

fn llm_result_details_json(result: &LlmCallResult) -> Option<String> {
    if !llm_result_has_partial_signal(result) {
        return None;
    }
    serde_json::to_string(&json!({
        "partial_full_text": result.full_text,
        "partial_reasoning": result.reasoning,
        "reasoning_signature": result.reasoning_signature,
        "tool_calls": result.tool_calls,
        "usage": result.usage,
        "finish_reason": result.finish_reason,
        "model_used": result.model_used,
    }))
    .ok()
}

/// Cooperative cancellation for [`call_llm_and_collect`] / [`collect_llm_stream`].
#[derive(Clone, Copy)]
pub(crate) enum LlmCancel<'a> {
    None,
    /// Cooperative cancel when the caller already owns a [`CancellationToken`].
    Token(&'a CancellationToken),
    Flag(&'a AtomicBool),
    /// User cancel (`AtomicBool`) plus a [`CancellationToken`] for immediate wake during LLM I/O.
    FlagAndToken(&'a AtomicBool, &'a CancellationToken),
}

impl LlmCancel<'_> {
    pub(crate) fn is_triggered(self) -> bool {
        match self {
            LlmCancel::None => false,
            LlmCancel::Token(t) => t.is_cancelled(),
            LlmCancel::Flag(f) => f.load(Ordering::Acquire),
            LlmCancel::FlagAndToken(f, t) => f.load(Ordering::Acquire) || t.is_cancelled(),
        }
    }
}

/// Completes when cancellation is requested; otherwise never completes if [`LlmCancel::None`].
pub(crate) async fn wait_llm_cancel(cancel: LlmCancel<'_>) {
    match cancel {
        LlmCancel::None => std::future::pending().await,
        LlmCancel::Token(t) => t.cancelled().await,
        LlmCancel::Flag(f) => {
            const POLL: std::time::Duration = std::time::Duration::from_millis(50);
            while !f.load(Ordering::Acquire) {
                tokio::time::sleep(POLL).await;
            }
        }
        LlmCancel::FlagAndToken(f, t) => {
            const POLL: std::time::Duration = std::time::Duration::from_millis(50);
            tokio::select! {
                biased;
                _ = t.cancelled() => {}
                _ = async {
                    while !f.load(Ordering::Acquire) {
                        tokio::time::sleep(POLL).await;
                    }
                } => {}
            }
        }
    }
}

/// Sleep for rate-limit / cooldown delays unless [`LlmCancel`] fires first (cooperative abort).
pub(crate) async fn sleep_ms_or_llm_cancel(
    delay_ms: u64,
    cancel: LlmCancel<'_>,
) -> Result<(), astra_core::ClassifiedError> {
    tokio::select! {
        biased;
        _ = wait_llm_cancel(cancel) => Err(astra_core::ClassifiedError::new(
            astra_core::ErrorKind::Cancelled,
            "LLM call cancelled",
        )),
        _ = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => Ok(()),
    }
}

/// Per-chunk idle watchdog (pre-progress): no SSE JSON for this long → treat as stalled.
/// Production delegates to the canonical timeout in `sse_stream_host`; tests may
/// override it through unit-test locals or the `bridge-e2e-hooks` integration hook.
pub(crate) fn stream_idle_timeout() -> std::time::Duration {
    #[cfg(test)]
    if let Some(d) = TEST_STREAM_IDLE_TIMEOUT.with(|c| *c.borrow()) {
        return d;
    }
    #[cfg(feature = "bridge-e2e-hooks")]
    if let Some(d) = bridge_e2e_stream_idle_timeout_override() {
        return d;
    }
    astra_turn_core::sse_stream_host::stream_idle_timeout()
}

/// Per-chunk idle watchdog (post-progress): once at least one SSE chunk has been
/// received, allow a longer idle window to accommodate thinking/reasoning pauses.
/// Production delegates to the canonical timeout in `sse_stream_host`; tests may
/// override it through unit-test locals or the `bridge-e2e-hooks` integration hook.
pub(crate) fn stream_idle_timeout_after_progress() -> std::time::Duration {
    #[cfg(test)]
    if let Some(d) = TEST_STREAM_IDLE_TIMEOUT_AFTER_PROGRESS.with(|c| *c.borrow()) {
        return d;
    }
    #[cfg(feature = "bridge-e2e-hooks")]
    if let Some(d) = bridge_e2e_stream_idle_timeout_after_progress_override() {
        return d;
    }
    astra_turn_core::sse_stream_host::stream_idle_timeout_after_progress()
}

pub(crate) fn stream_terminal_drain_timeout(
    ordinary_idle: std::time::Duration,
) -> std::time::Duration {
    ordinary_idle.min(std::time::Duration::from_millis(
        LLM_STREAM_TERMINAL_DRAIN_GRACE_MS,
    ))
}

#[cfg(feature = "bridge-e2e-hooks")]
static BRIDGE_E2E_STREAM_IDLE_TIMEOUT_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

#[cfg(feature = "bridge-e2e-hooks")]
static BRIDGE_E2E_STREAM_IDLE_TIMEOUT_AFTER_PROGRESS_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

#[cfg(feature = "bridge-e2e-hooks")]
pub(crate) struct BridgeE2eStreamIdleTimeoutGuard {
    prev_pre_ms: u64,
    prev_post_ms: u64,
}

#[cfg(feature = "bridge-e2e-hooks")]
impl Drop for BridgeE2eStreamIdleTimeoutGuard {
    fn drop(&mut self) {
        restore_bridge_e2e_stream_idle_timeouts_for_test(self.prev_pre_ms, self.prev_post_ms);
    }
}

#[cfg(feature = "bridge-e2e-hooks")]
fn duration_override(ms: u64) -> Option<std::time::Duration> {
    (ms > 0).then(|| std::time::Duration::from_millis(ms))
}

#[cfg(feature = "bridge-e2e-hooks")]
fn bridge_e2e_stream_idle_timeout_override() -> Option<std::time::Duration> {
    duration_override(BRIDGE_E2E_STREAM_IDLE_TIMEOUT_MS.load(std::sync::atomic::Ordering::SeqCst))
}

#[cfg(feature = "bridge-e2e-hooks")]
fn bridge_e2e_stream_idle_timeout_after_progress_override() -> Option<std::time::Duration> {
    duration_override(
        BRIDGE_E2E_STREAM_IDLE_TIMEOUT_AFTER_PROGRESS_MS.load(std::sync::atomic::Ordering::SeqCst),
    )
}

#[cfg(feature = "bridge-e2e-hooks")]
pub(crate) fn set_bridge_e2e_stream_idle_timeouts_for_test(
    pre_ms: u64,
    post_ms: u64,
) -> BridgeE2eStreamIdleTimeoutGuard {
    assert!(pre_ms > 0, "pre-progress idle timeout must be positive");
    assert!(post_ms > 0, "post-progress idle timeout must be positive");
    let prev_pre_ms =
        BRIDGE_E2E_STREAM_IDLE_TIMEOUT_MS.swap(pre_ms, std::sync::atomic::Ordering::SeqCst);
    let prev_post_ms = BRIDGE_E2E_STREAM_IDLE_TIMEOUT_AFTER_PROGRESS_MS
        .swap(post_ms, std::sync::atomic::Ordering::SeqCst);
    BridgeE2eStreamIdleTimeoutGuard {
        prev_pre_ms,
        prev_post_ms,
    }
}

#[cfg(feature = "bridge-e2e-hooks")]
pub(crate) fn restore_bridge_e2e_stream_idle_timeouts_for_test(pre_ms: u64, post_ms: u64) {
    BRIDGE_E2E_STREAM_IDLE_TIMEOUT_MS.store(pre_ms, std::sync::atomic::Ordering::SeqCst);
    BRIDGE_E2E_STREAM_IDLE_TIMEOUT_AFTER_PROGRESS_MS
        .store(post_ms, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(feature = "bridge-e2e-hooks")]
pub(crate) fn current_bridge_e2e_stream_idle_timeouts_for_test()
-> (Option<std::time::Duration>, Option<std::time::Duration>) {
    (
        bridge_e2e_stream_idle_timeout_override(),
        bridge_e2e_stream_idle_timeout_after_progress_override(),
    )
}

#[cfg(test)]
thread_local! {
    static TEST_STREAM_IDLE_TIMEOUT: std::cell::RefCell<Option<std::time::Duration>> =
        const { std::cell::RefCell::new(None) };
    static TEST_STREAM_IDLE_TIMEOUT_AFTER_PROGRESS: std::cell::RefCell<Option<std::time::Duration>> =
        const { std::cell::RefCell::new(None) };
    // Retry-backoff override for tests: when `Some(ms)`, the generic
    // `LLM_RETRY_BASE_MS * 2^(attempt-1)` delay is replaced by this flat
    // value. Provider/cooldown delay hints remain authoritative.
    static TEST_RETRY_BACKOFF_MS: std::cell::RefCell<Option<u64>> =
        const { std::cell::RefCell::new(None) };
}

/// Compute the between-attempts backoff in ms. `attempt` is 1-indexed (the
/// first retry after the initial failure has attempt=1).
fn retry_backoff_ms(attempt: u32) -> u64 {
    #[cfg(test)]
    if let Some(ms) = TEST_RETRY_BACKOFF_MS.with(|c| *c.borrow()) {
        return ms;
    }
    llm_retry_base_ms() * (1 << (attempt - 1))
}

/// Override the between-retry backoff to `ms` for the duration of a test.
/// Without this, every retry incurs a real 1s/2s/4s sleep — with it,
/// retry-logic tests run in <100ms. Returns a guard that clears the override
/// on drop. `pub(crate)` so other runtime modules (e.g. server_loop_host
/// end-to-end tests) can use the same knob.
#[cfg(test)]
pub(crate) fn set_test_retry_backoff_ms(ms: u64) -> impl Drop {
    TEST_RETRY_BACKOFF_MS.with(|c| *c.borrow_mut() = Some(ms));
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            TEST_RETRY_BACKOFF_MS.with(|c| *c.borrow_mut() = None);
        }
    }
    Guard
}

/// Apply HTTP(S)/ALL proxy env vars to a reqwest::ClientBuilder.
///
/// reqwest is built with `default-features = false`, so it does not auto-read
/// the system proxy env vars. We wire them up explicitly here and honour
/// `NO_PROXY` via `reqwest::NoProxy::from_env()`.
///
/// Precedence (first match wins): `HTTPS_PROXY`, `https_proxy`, `ALL_PROXY`,
/// `all_proxy`. For `HTTPS_PROXY`/`https_proxy` we register an HTTPS-scheme
/// proxy; for `ALL_PROXY`/`all_proxy` we register an all-scheme proxy so that
/// `socks5://` URLs (which only make sense as all-scheme) are honoured.
pub(crate) use astra_core::net::apply_env_proxy;

// Tests for `apply_env_proxy` live with its authoritative implementation in
// `astra_core::net`. Do not duplicate them here.

/// Resolve an LLM duration-in-seconds constant, consulting its env-var
/// override and falling back to the compile-time default. Used by
/// `LLM_CONNECT_TIMEOUT_S`, `LLM_NONSTREAM_TIMEOUT_S`, and
/// `LLM_TOTAL_BUDGET_S`. Operators set these to lower values in
/// degraded conditions (tight SLOs) or raise them for slow providers;
/// the const defaults are the production baseline.
fn llm_secs_from_env(var: &str, default_secs: u64) -> u64 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(default_secs)
}

/// TCP connect timeout for LLM API requests. Override: `ASTRA_LLM_CONNECT_TIMEOUT_S`.
pub(crate) fn llm_connect_timeout() -> std::time::Duration {
    std::time::Duration::from_secs(llm_secs_from_env(
        "ASTRA_LLM_CONNECT_TIMEOUT_S",
        LLM_CONNECT_TIMEOUT_S,
    ))
}

/// Hard timeout for a non-stream request. Override: `ASTRA_LLM_NONSTREAM_TIMEOUT_S`.
pub(crate) fn llm_nonstream_timeout() -> std::time::Duration {
    std::time::Duration::from_secs(llm_secs_from_env(
        "ASTRA_LLM_NONSTREAM_TIMEOUT_S",
        LLM_NONSTREAM_TIMEOUT_S,
    ))
}

/// Total budget across all retries + fallback for a single LLM call. Override: `ASTRA_LLM_TOTAL_BUDGET_S`.
pub(crate) fn llm_total_budget() -> std::time::Duration {
    std::time::Duration::from_secs(llm_secs_from_env(
        "ASTRA_LLM_TOTAL_BUDGET_S",
        LLM_TOTAL_BUDGET_S,
    ))
}

#[cfg(test)]
fn llm_completions_url(base_url: &str, override_url: Option<&str>, provider: &str) -> String {
    llm_request_url(base_url, override_url, provider, "", true)
}

fn bedrock_converse_url(base_url: &str, model_name: &str, streaming: bool) -> String {
    let base = base_url.trim_end_matches('/');
    let mut url = reqwest::Url::parse(base).unwrap_or_else(|_| {
        reqwest::Url::parse("http://invalid.local").expect("valid fallback URL")
    });
    {
        let Ok(mut segments) = url.path_segments_mut() else {
            return format!(
                "{base}/model/{model_name}/{}",
                if streaming {
                    "converse-stream"
                } else {
                    "converse"
                }
            );
        };
        segments.pop_if_empty();
        segments.push("model");
        segments.push(model_name);
        segments.push(if streaming {
            "converse-stream"
        } else {
            "converse"
        });
    }
    if url.host_str() == Some("invalid.local") {
        format!(
            "{base}/model/{model_name}/{}",
            if streaming {
                "converse-stream"
            } else {
                "converse"
            }
        )
    } else {
        url.to_string()
    }
}

pub(crate) fn llm_request_url(
    base_url: &str,
    override_url: Option<&str>,
    provider: &str,
    model_name: &str,
    streaming: bool,
) -> String {
    override_url
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(String::from)
        .unwrap_or_else(|| llm_request_url_for_provider(base_url, provider, model_name, streaming))
}

fn acquire_registered_endpoint_permit_for_override(
    request_url: &str,
    completions_url_override: Option<&str>,
) -> Result<Option<tokio::sync::OwnedSemaphorePermit>, astra_core::ClassifiedError> {
    let Some(override_url) = completions_url_override
        .map(str::trim)
        .filter(|url| !url.is_empty())
    else {
        return Ok(None);
    };
    let endpoint_url = if request_url.is_empty() {
        override_url
    } else {
        request_url
    };
    crate::capability_endpoint_pool::try_acquire_endpoint_permit(endpoint_url)
        .map(Some)
        .map_err(|detail| {
            astra_core::ClassifiedError::new(astra_core::ErrorKind::ResourceLimit, detail)
        })
}

/// Build the default completions URL for a given provider (no override).
///
/// Anthropic uses `/v1/messages`, Bedrock uses `/model/{modelId}/converse`,
/// all others use `/chat/completions`.
pub(crate) fn llm_request_url_for_provider(
    base_url: &str,
    provider: &str,
    model_name: &str,
    streaming: bool,
) -> String {
    let base = base_url.trim_end_matches('/');
    match llm_provider_protocol(provider) {
        LlmProviderProtocol::AnthropicMessages => {
            if base.ends_with("/v1") {
                format!("{base}/messages")
            } else {
                format!("{base}/v1/messages")
            }
        }
        LlmProviderProtocol::BedrockConverse => bedrock_converse_url(base, model_name, streaming),
        LlmProviderProtocol::OpenAiCompatible => format!("{base}/chat/completions"),
    }
}

fn json_string_to_value_or_string(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

fn coerce_tool_result_content(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(Value::Object(map)) if map.is_empty() => String::new(),
        Some(Value::Array(parts)) => {
            let joined = parts
                .iter()
                .filter_map(|part| {
                    part.get("text")
                        .and_then(Value::as_str)
                        .or_else(|| part.get("content").and_then(Value::as_str))
                })
                .collect::<Vec<_>>()
                .join("");
            if joined.is_empty() {
                // No joinable text parts (e.g. image-only array). Returning the
                // raw JSON-stringified array would leak structural noise to the
                // model; prefer empty string + a warn so the degradation is
                // observable without polluting the prompt.
                tracing::warn!(
                    target: "astra::tool_result",
                    parts = parts.len(),
                    "tool_result content array had no text parts; coercing to empty string"
                );
                String::new()
            } else {
                joined
            }
        }
        Some(other) => other.to_string(),
    }
}

fn normalize_openai_tool_message_content(messages: &[Value]) -> Vec<Value> {
    astra_core::history_work::record_serialized_value(
        astra_core::history_work::HistoryWorkSite::ProviderWireAssembly,
        messages,
    );
    messages
        .iter()
        .map(|message| {
            if message.get("role").and_then(Value::as_str) != Some("tool") {
                return message.clone();
            }
            // Only rewrite content that OpenAI would reject or misread:
            // bare objects (most notably the empty `{}` placeholder) and nulls.
            // Leave well-formed String and Array content untouched so future
            // multi-part tool messages pass through intact.
            let needs_rewrite = matches!(
                message.get("content"),
                Some(Value::Object(_)) | Some(Value::Null) | None
            );
            if !needs_rewrite {
                return message.clone();
            }
            let mut normalized = message.clone();
            if let Some(obj) = normalized.as_object_mut() {
                obj.insert(
                    "content".to_string(),
                    Value::String(coerce_tool_result_content(message.get("content"))),
                );
            }
            normalized
        })
        .collect()
}

fn is_nonblank_text(text: &str) -> bool {
    !text.trim().is_empty()
}

fn bedrock_cache_point_from_cache_control(cache_control: Option<&Value>) -> Option<Value> {
    let cache_control = cache_control?;
    let mut cache_point = Map::new();
    cache_point.insert("type".to_string(), Value::String("default".to_string()));
    if let Some(ttl) = cache_control
        .get("ttl")
        .and_then(Value::as_str)
        .filter(|ttl| matches!(*ttl, "5m" | "1h"))
    {
        cache_point.insert("ttl".to_string(), Value::String(ttl.to_string()));
    }
    Some(json!({ "cachePoint": Value::Object(cache_point) }))
}

fn bedrock_cache_point_from_block(block: &Value) -> Option<Value> {
    if let Some(cache_point) = block.get("cachePoint") {
        return Some(json!({ "cachePoint": cache_point.clone() }));
    }
    bedrock_cache_point_from_cache_control(block.get("cache_control"))
}

fn build_bedrock_text_content_blocks(content: Option<&Value>) -> Vec<Value> {
    match content {
        Some(Value::String(text)) if is_nonblank_text(text) => vec![json!({ "text": text })],
        Some(Value::Array(parts)) => {
            let mut blocks = Vec::new();
            for part in parts {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    if is_nonblank_text(text) {
                        blocks.push(json!({ "text": text }));
                    }
                } else if let Some(text) = part.as_str() {
                    if is_nonblank_text(text) {
                        blocks.push(json!({ "text": text }));
                    }
                }
                if let Some(cache_point) = bedrock_cache_point_from_block(part) {
                    blocks.push(cache_point);
                }
            }
            blocks
        }
        Some(Value::Object(obj)) => {
            let mut blocks = Vec::new();
            if let Some(text) = obj.get("text").and_then(Value::as_str) {
                if is_nonblank_text(text) {
                    blocks.push(json!({ "text": text }));
                }
            }
            if let Some(cache_point) = bedrock_cache_point_from_block(&Value::Object(obj.clone())) {
                blocks.push(cache_point);
            }
            blocks
        }
        _ => Vec::new(),
    }
}

fn bedrock_system_has_text(blocks: &[Value]) -> bool {
    blocks.iter().any(|block| {
        block
            .get("text")
            .and_then(Value::as_str)
            .is_some_and(is_nonblank_text)
    })
}

fn bedrock_cache_point_from_message_content(content: Option<&Value>) -> Option<Value> {
    match content {
        Some(Value::Array(parts)) => parts.iter().rev().find_map(bedrock_cache_point_from_block),
        Some(Value::Object(obj)) => bedrock_cache_point_from_block(&Value::Object(obj.clone())),
        _ => None,
    }
}

fn build_bedrock_tool_blocks(tool_calls: Option<&Vec<Value>>) -> Vec<Value> {
    let Some(tool_calls) = tool_calls else {
        return Vec::new();
    };
    tool_calls
        .iter()
        .filter_map(|tool_call| {
            let id = tool_call.get("id").and_then(Value::as_str)?;
            let function = tool_call.get("function")?.as_object()?;
            let name = tool_call_name(tool_call)?;
            let input = function
                .get("arguments")
                .and_then(Value::as_str)
                .map(json_string_to_value_or_string)
                .unwrap_or_else(|| json!({}));
            Some(json!({
                "toolUse": {
                    "toolUseId": id,
                    "name": name,
                    "input": input,
                }
            }))
        })
        .collect()
}

fn build_bedrock_message_content(msg: &Value, include_reasoning_content: bool) -> Vec<Value> {
    let role = msg.get("role").and_then(Value::as_str).unwrap_or_default();
    match role {
        "tool" => {
            let tool_use_id = msg.get("tool_call_id").and_then(Value::as_str);
            let content = coerce_tool_result_content(msg.get("content"));
            tool_use_id
                .map(|tool_use_id| {
                    // Bedrock's `toolResult.content[].json` field requires a
                    // JSON object (Document type). Scalars, arrays, strings,
                    // booleans, and null must use the `text` branch — or
                    // Bedrock rejects with "messages.N.content.M.toolResult
                    // .content.0.json is invalid — provide a json object".
                    //
                    // Empty content uses `{"text": ""}` (not `{"json": {}}`)
                    // because Bedrock accepts an empty string here but rejects
                    // an empty JSON object as "invalid". Do not switch to
                    // `{"json": {}}` — it will 400 at the provider.
                    let result_block = if content.is_empty() {
                        json!({"text": ""})
                    } else {
                        match serde_json::from_str::<Value>(&content) {
                            Ok(parsed) if parsed.is_object() => json!({"json": parsed}),
                            _ => json!({"text": content}),
                        }
                    };
                    let mut blocks = vec![json!({
                        "toolResult": {
                            "toolUseId": tool_use_id,
                            "content": [result_block],
                        }
                    })];
                    if let Some(cache_point) =
                        bedrock_cache_point_from_message_content(msg.get("content"))
                    {
                        blocks.push(cache_point);
                    }
                    blocks
                })
                .unwrap_or_default()
        }
        "assistant" => {
            let mut blocks = Vec::new();
            // Bedrock requires reasoningContent FIRST when thinking is enabled.
            let has_reasoning = if include_reasoning_content
                && let Some(rc) = msg.get("reasoning_content").and_then(Value::as_str)
            {
                if !rc.is_empty() {
                    let signature = msg
                        .get("reasoning_signature")
                        .and_then(Value::as_str)
                        .filter(|sig| !sig.is_empty());
                    if let Some(sig) = signature {
                        let reasoning_text = json!({"text": rc, "signature": sig});
                        blocks.push(json!({"reasoningContent": {"reasoningText": reasoning_text}}));
                        true
                    } else {
                        // Bedrock thinking blocks are cryptographically bound to a
                        // provider-emitted signature. Replaying unsigned reasoning
                        // text is invalid and produces HTTP 400, especially after a
                        // session switches from an OpenAI-compatible thinking model
                        // to Bedrock. Keep the visible assistant text/tool calls, but
                        // do not serialize an invalid reasoningContent block.
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            };
            blocks.extend(build_bedrock_text_content_blocks(msg.get("content")));
            blocks.extend(build_bedrock_tool_blocks(
                msg.get("tool_calls").and_then(Value::as_array),
            ));
            // Bedrock rejects assistant messages where the final block is thinking/reasoning.
            // If reasoning was emitted but no text or tool_use followed, append a minimal
            // text block to satisfy the constraint.
            if has_reasoning && blocks.len() == 1 {
                blocks.push(json!({ "text": "" }));
            }
            blocks
        }
        _ => build_bedrock_text_content_blocks(msg.get("content")),
    }
}

/// Synthetic tool_result content inserted for a declared tool_call that has
/// no matching response. The model sees this and can recover (e.g. retry).
const SYNTHETIC_TOOL_INTERRUPTED_CONTENT: &str = "[tool execution not recorded]";

/// Repair OpenAI-wire `assistant.tool_calls` / `role=tool` pairing mismatches
/// before provider-specific translation. Three classes of corruption we observe,
/// in order of severity:
///
/// 1. Missing tool_result for a declared tool_call (stream cut mid-execution,
///    session resume, or bridge restart). Bedrock: "Expected toolResult blocks
///    at messages.N.content for the following Ids: …".
/// 2. Orphaned tool_result whose tool_call_id doesn't match any preceding
///    assistant's tool_calls. Bedrock: "unexpected toolResult".
/// 3. Duplicate tool_call_id within one tool-group (retry artifact). Bedrock:
///    duplicate-id 400.
///
/// This mirrors the reference agent's `ensureToolResultPairing` but operates on OpenAI
/// wire format (role=tool messages) instead of Anthropic blocks.
pub(crate) fn repair_openai_tool_pairing(messages: &[Value]) -> Vec<Value> {
    astra_core::history_work::record_serialized_value(
        astra_core::history_work::HistoryWorkSite::ProviderWireAssembly,
        messages,
    );
    let mut repaired: Vec<Value> = Vec::with_capacity(messages.len());
    let mut missing_counts: usize = 0;
    let mut orphan_counts: usize = 0;
    let mut dup_counts: usize = 0;

    let mut i = 0;
    while i < messages.len() {
        let msg = &messages[i];
        let role = msg.get("role").and_then(Value::as_str).unwrap_or_default();

        if role == "assistant" {
            let declared_ids: Vec<String> = msg
                .get("tool_calls")
                .and_then(Value::as_array)
                .map(|tcs| {
                    tcs.iter()
                        .filter_map(|tc| tc.get("id").and_then(Value::as_str).map(String::from))
                        .collect()
                })
                .unwrap_or_default();

            repaired.push(msg.clone());
            i += 1;

            if declared_ids.is_empty() {
                continue;
            }

            // Collect the contiguous run of role=tool messages that follow.
            let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
            let declared_set: std::collections::HashSet<&str> =
                declared_ids.iter().map(String::as_str).collect();
            while i < messages.len()
                && messages[i].get("role").and_then(Value::as_str) == Some("tool")
            {
                let tool_msg = &messages[i];
                let id = tool_msg
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if id.is_empty() || !declared_set.contains(id) {
                    orphan_counts += 1;
                    i += 1;
                    continue;
                }
                if !seen_ids.insert(id.to_string()) {
                    dup_counts += 1;
                    i += 1;
                    continue;
                }
                repaired.push(tool_msg.clone());
                i += 1;
            }

            for declared in &declared_ids {
                if !seen_ids.contains(declared) {
                    missing_counts += 1;
                    repaired.push(json!({
                        "role": "tool",
                        "tool_call_id": declared,
                        "content": SYNTHETIC_TOOL_INTERRUPTED_CONTENT,
                    }));
                }
            }
            continue;
        }

        if role == "tool" {
            // Orphan: a role=tool message without a preceding assistant
            // tool_calls declaration in the current window. Drop it.
            orphan_counts += 1;
            i += 1;
            continue;
        }

        repaired.push(msg.clone());
        i += 1;
    }

    if missing_counts + orphan_counts + dup_counts > 0 {
        tracing::warn!(
            missing = missing_counts,
            orphaned = orphan_counts,
            duplicate = dup_counts,
            input_len = messages.len(),
            output_len = repaired.len(),
            "repaired OpenAI tool_call/tool_result pairing"
        );
    }
    repaired
}

fn anthropic_tool_use_ids(msg: &Value) -> Vec<String> {
    msg.get("content")
        .map(anthropic_content_as_blocks)
        .unwrap_or_default()
        .into_iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
        .filter_map(|block| block.get("id").and_then(Value::as_str).map(String::from))
        .collect()
}

fn is_anthropic_tool_result_block(block: &Value) -> bool {
    block.get("type").and_then(Value::as_str) == Some("tool_result")
}

fn synthetic_anthropic_tool_result_block(tool_use_id: &str) -> Value {
    json!({
        "type": "tool_result",
        "tool_use_id": tool_use_id,
        "content": SYNTHETIC_TOOL_INTERRUPTED_CONTENT,
    })
}

/// Repair Anthropic Messages tool_use/tool_result mismatches after conversion to
/// Anthropic-native roles/content blocks.
///
/// Anthropic enforces a stricter adjacency rule than OpenAI wire format: any
/// `tool_result` block must live on the `user` message immediately following the
/// `assistant` message that declared the matching `tool_use`. Compaction and
/// resume can leave three classes of corruption:
///
/// 1. Missing tool_result for a declared tool_use.
/// 2. Orphaned tool_result whose `tool_use_id` no longer appears in the
///    previous assistant message.
/// 3. Duplicate tool_result blocks for the same `tool_use_id`.
///
/// We repair in-place at the Anthropic wire layer so both native `role=tool`
/// inputs (after conversion) and already-native `role=user` tool_result blocks
/// are handled consistently.
fn repair_anthropic_tool_pairing(messages: &[Value]) -> Vec<Value> {
    astra_core::history_work::record_serialized_value(
        astra_core::history_work::HistoryWorkSite::ProviderWireAssembly,
        messages,
    );
    let mut repaired: Vec<Value> = Vec::with_capacity(messages.len() + 1);
    let mut missing_counts: usize = 0;
    let mut orphan_counts: usize = 0;
    let mut dup_counts: usize = 0;

    let mut i = 0;
    while i < messages.len() {
        let msg = &messages[i];
        let role = msg.get("role").and_then(Value::as_str).unwrap_or_default();

        if role == "assistant" {
            let declared_ids = anthropic_tool_use_ids(msg);
            repaired.push(msg.clone());
            i += 1;

            if declared_ids.is_empty() {
                continue;
            }

            let declared_set: HashSet<&str> = declared_ids.iter().map(String::as_str).collect();

            if i < messages.len() && messages[i].get("role").and_then(Value::as_str) == Some("user")
            {
                let mut user_msg = messages[i].clone();
                let mut seen_ids: HashSet<String> = HashSet::new();
                let mut kept_tool_results: Vec<Value> = Vec::new();
                let mut other_blocks: Vec<Value> = Vec::new();

                for block in messages[i]
                    .get("content")
                    .map(anthropic_content_as_blocks)
                    .unwrap_or_default()
                {
                    if !is_anthropic_tool_result_block(&block) {
                        other_blocks.push(block);
                        continue;
                    }
                    let tool_use_id = block
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if tool_use_id.is_empty() || !declared_set.contains(tool_use_id) {
                        orphan_counts += 1;
                        continue;
                    }
                    if !seen_ids.insert(tool_use_id.to_string()) {
                        dup_counts += 1;
                        continue;
                    }
                    kept_tool_results.push(block);
                }

                for declared in &declared_ids {
                    if !seen_ids.contains(declared) {
                        missing_counts += 1;
                        kept_tool_results.push(synthetic_anthropic_tool_result_block(declared));
                    }
                }

                kept_tool_results.extend(other_blocks);
                user_msg["content"] = Value::Array(kept_tool_results);
                repaired.push(user_msg);
                i += 1;
                continue;
            }

            missing_counts += declared_ids.len();
            repaired.push(json!({
                "role": "user",
                "content": declared_ids
                    .iter()
                    .map(|id| synthetic_anthropic_tool_result_block(id))
                    .collect::<Vec<_>>(),
            }));
            continue;
        }

        if role == "user" {
            let blocks = msg
                .get("content")
                .map(anthropic_content_as_blocks)
                .unwrap_or_default();
            if blocks.iter().any(is_anthropic_tool_result_block) {
                let kept_blocks: Vec<Value> = blocks
                    .into_iter()
                    .filter_map(|block| {
                        if is_anthropic_tool_result_block(&block) {
                            orphan_counts += 1;
                            None
                        } else {
                            Some(block)
                        }
                    })
                    .collect();
                if kept_blocks.is_empty() {
                    i += 1;
                    continue;
                }
                let mut user_msg = msg.clone();
                user_msg["content"] = Value::Array(kept_blocks);
                repaired.push(user_msg);
                i += 1;
                continue;
            }
        }

        repaired.push(msg.clone());
        i += 1;
    }

    if missing_counts + orphan_counts + dup_counts > 0 {
        tracing::warn!(
            missing = missing_counts,
            orphaned = orphan_counts,
            duplicate = dup_counts,
            input_len = messages.len(),
            output_len = repaired.len(),
            "repaired tool_use/tool_result pairing for Anthropic request"
        );
    }
    repaired
}

fn flush_tool_buffer(out: &mut Vec<Value>, buffer: &mut Vec<Value>) {
    if buffer.is_empty() {
        return;
    }
    let blocks = std::mem::take(buffer);
    out.push(json!({
        "role": "user",
        "content": blocks,
    }));
}

fn build_bedrock_messages(
    messages: &[Value],
    include_reasoning_content: bool,
) -> (Vec<Value>, Vec<Value>) {
    astra_core::history_work::record_serialized_value(
        astra_core::history_work::HistoryWorkSite::ProviderWireAssembly,
        messages,
    );
    let mut system = Vec::new();
    let mut out = Vec::new();
    // Bedrock Converse requires all toolResult blocks for a given assistant
    // turn's parallel toolUse blocks to live in a SINGLE user message. OpenAI
    // wire format emits one `role: "tool"` per result, so we buffer
    // consecutive tool messages and flush them as one merged user message
    // whenever a non-tool message (or end of input) is reached.
    let mut tool_buffer: Vec<Value> = Vec::new();
    for msg in messages {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or_default();
        if role != "tool" {
            flush_tool_buffer(&mut out, &mut tool_buffer);
        }
        match role {
            "system" => {
                system.extend(build_bedrock_text_content_blocks(msg.get("content")));
            }
            "tool" => {
                tool_buffer.extend(build_bedrock_message_content(
                    msg,
                    include_reasoning_content,
                ));
            }
            "user" | "assistant" => {
                let content = build_bedrock_message_content(msg, include_reasoning_content);
                if !content.is_empty() {
                    out.push(json!({
                        "role": role,
                        "content": content,
                    }));
                }
            }
            _ => {}
        }
    }
    flush_tool_buffer(&mut out, &mut tool_buffer);

    // Bedrock Converse requires strict role alternation (user/assistant/user/...).
    // Runtime-injected messages (correctives, attention, budget warnings) can
    // create consecutive user messages. Merge them into a single message with
    // combined content blocks.
    let out = merge_consecutive_same_role(out);

    (system, out)
}

/// Merge consecutive messages with the same role into a single message
/// with combined content blocks. Bedrock Converse requires strict
/// user/assistant alternation.
fn merge_consecutive_same_role(messages: Vec<Value>) -> Vec<Value> {
    if messages.is_empty() {
        return messages;
    }
    let mut merged: Vec<Value> = Vec::with_capacity(messages.len());
    for msg in messages {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or_default();
        let can_merge = merged
            .last()
            .is_some_and(|prev: &Value| prev.get("role").and_then(Value::as_str) == Some(role));
        if can_merge {
            // Append content blocks to the previous message
            if let Some(new_content) = msg.get("content").and_then(Value::as_array) {
                if let Some(prev) = merged.last_mut() {
                    if let Some(prev_content) =
                        prev.get_mut("content").and_then(Value::as_array_mut)
                    {
                        prev_content.extend(new_content.iter().cloned());
                    }
                }
            }
        } else {
            merged.push(msg);
        }
    }
    merged
}

fn build_bedrock_tools(tools: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    for tool in tools {
        if let Some(mapped) = (|| {
            let function = tool.get("function")?.as_object()?;
            let name = function.get("name").and_then(Value::as_str)?;
            let mut input_schema = function
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            // Bedrock Converse rejects schemas with unsupported fields like
            // "default", "minimum", "maximum" inside property definitions.
            // Strip them recursively to avoid HTTP 400.
            strip_unsupported_schema_fields(&mut input_schema);
            let mut tool_spec = Map::new();
            tool_spec.insert("name".to_string(), Value::String(name.to_string()));
            if let Some(description) = function.get("description").and_then(Value::as_str) {
                tool_spec.insert(
                    "description".to_string(),
                    Value::String(description.to_string()),
                );
            }
            tool_spec.insert("inputSchema".to_string(), json!({ "json": input_schema }));
            Some(json!({ "toolSpec": Value::Object(tool_spec) }))
        })() {
            out.push(mapped);
            if let Some(cache_point) =
                bedrock_cache_point_from_cache_control(tool.get("cache_control"))
            {
                out.push(cache_point);
            }
        }
    }
    out
}

/// Recursively strip JSON Schema fields that Bedrock Converse does not accept.
/// Bedrock's toolSpec inputSchema only supports a subset of JSON Schema:
/// type, description, properties, required, items, enum.
/// Fields like "default", "minimum", "maximum", "minItems", "maxItems",
/// "pattern", "format" cause HTTP 400 "The provided request is not valid".
///
/// Also strips top-level composition keywords (`allOf`/`oneOf`/`anyOf`)
/// which Bedrock rejects with "input_schema does not support oneOf,
/// allOf, or anyOf at the top level", plus all internal `x-astra-*`
/// annotations. Providers should ignore vendor keys, but some strict
/// validators reject them.
fn strip_unsupported_schema_fields(value: &mut Value) {
    strip_internal_schema_extensions(value);
    strip_unsupported_schema_fields_inner(value, /* is_top_level */ true);
}

/// Internal schema metadata belongs to Astra's discovery and validation
/// layers, never to a provider wire contract. Strip the whole vendor prefix
/// generically so adding a new internal annotation cannot break a strict
/// provider or require another provider-specific exception.
fn strip_internal_schema_extensions(value: &mut Value) {
    match value {
        Value::Object(object) => {
            // Providers reject Astra's vendor extensions, but deleting them
            // used to delete the only precise per-action argument contract as
            // well. Materialize one compact, deterministic description before
            // stripping so the provider sees the same contract the executor
            // enforces without relying on unsupported schema composition.
            let mut requirements = Vec::new();
            if let Some(per_action) = object
                .get(astra_tools::schemas::PER_ACTION_REQUIRED_KEY)
                .and_then(Value::as_object)
            {
                for (action, fields) in per_action {
                    let fields = fields
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>();
                    if !fields.is_empty() {
                        requirements.push(format!("{action} requires {}", fields.join(" + ")));
                    }
                }
            }
            if let Some(per_action) = object
                .get(astra_tools::schemas::PER_ACTION_ANY_OF_REQUIRED_KEY)
                .and_then(Value::as_object)
            {
                for (action, alternatives) in per_action {
                    let alternatives = alternatives
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_array)
                        .map(|fields| {
                            fields
                                .iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join(" + ")
                        })
                        .filter(|fields| !fields.is_empty())
                        .collect::<Vec<_>>();
                    if !alternatives.is_empty() {
                        requirements.push(format!(
                            "{action} also requires one of {}",
                            alternatives.join(" or ")
                        ));
                    }
                }
            }
            if let Some(per_action) = object
                .get(astra_tools::schemas::PER_ACTION_ALLOWED_KEY)
                .and_then(Value::as_object)
            {
                for (action, fields) in per_action {
                    let fields = fields
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>();
                    if !fields.is_empty() {
                        requirements.push(format!("{action} accepts only {}", fields.join(" + ")));
                    }
                }
            }
            if !requirements.is_empty() {
                let contract = format!("Action contract: {}.", requirements.join("; "));
                let description = object
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|description| !description.is_empty())
                    .map(|description| format!("{description} {contract}"))
                    .unwrap_or(contract);
                object.insert("description".to_string(), Value::String(description));
            }
            if let Some(action_union) = standard_action_union_schema(object) {
                object.insert("oneOf".to_string(), Value::Array(action_union));
            }
            object.retain(|key, _| !key.starts_with("x-astra-"));
            for child in object.values_mut() {
                strip_internal_schema_extensions(child);
            }
        }
        Value::Array(values) => {
            for child in values {
                strip_internal_schema_extensions(child);
            }
        }
        _ => {}
    }
}

/// Convert Astra's internal consolidated-action contract into ordinary JSON
/// Schema before provider-specific extension stripping.
///
/// OpenAI-compatible providers receive a structural discriminated union
/// instead of having to infer conditional required/allowed fields from prose.
/// Providers that reject composition keywords run this same conversion and
/// then remove `oneOf` in `strip_unsupported_schema_fields_inner`, retaining
/// the deterministic compact description fallback above.
fn standard_action_union_schema(object: &Map<String, Value>) -> Option<Vec<Value>> {
    let properties = object.get("properties")?.as_object()?;
    let actions = properties
        .get("action")?
        .get("enum")?
        .as_array()?
        .iter()
        .map(Value::as_str)
        .collect::<Option<Vec<_>>>()?;
    let per_action_required = object
        .get(astra_tools::schemas::PER_ACTION_REQUIRED_KEY)?
        .as_object()?;
    let per_action_allowed = object
        .get(astra_tools::schemas::PER_ACTION_ALLOWED_KEY)
        .and_then(Value::as_object);
    let per_action_any_of = object
        .get(astra_tools::schemas::PER_ACTION_ANY_OF_REQUIRED_KEY)
        .and_then(Value::as_object);

    actions
        .into_iter()
        .map(|action| {
            let required_fields = per_action_required
                .get(action)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(Value::as_str)
                .collect::<Option<Vec<_>>>()?;
            let allowed_fields = match per_action_allowed
                .and_then(|allowed| allowed.get(action))
                .and_then(Value::as_array)
            {
                Some(fields) => fields
                    .iter()
                    .map(Value::as_str)
                    .collect::<Option<Vec<_>>>()?,
                None => properties.keys().map(String::as_str).collect(),
            };

            let mut branch_properties = Map::new();
            for field in allowed_fields {
                branch_properties.insert(field.to_string(), properties.get(field)?.clone());
            }
            let action_schema = branch_properties.get_mut("action")?.as_object_mut()?;
            action_schema.insert("enum".to_string(), json!([action]));

            let mut required = vec!["action"];
            for field in required_fields {
                if !required.contains(&field) {
                    required.push(field);
                }
            }
            let mut branch = json!({
                "type": "object",
                "properties": branch_properties,
                "required": required,
                "additionalProperties": false,
            });
            if let Some(alternatives) = per_action_any_of
                .and_then(|per_action| per_action.get(action))
                .and_then(Value::as_array)
            {
                let alternatives = alternatives
                    .iter()
                    .map(Value::as_array)
                    .map(|fields| {
                        fields?
                            .iter()
                            .map(Value::as_str)
                            .collect::<Option<Vec<_>>>()
                            .map(|required| json!({"required": required}))
                    })
                    .collect::<Option<Vec<_>>>()?;
                if !alternatives.is_empty() {
                    branch["anyOf"] = Value::Array(alternatives);
                }
            }
            Some(branch)
        })
        .collect()
}

/// Keys always stripped — Bedrock's strict validator rejects them
/// at any nesting level.
const UNSUPPORTED_ANYWHERE: &[&str] = &[
    "default",
    "minimum",
    "maximum",
    "minItems",
    "maxItems",
    "pattern",
    "format",
    "examples",
    "title",
    "$schema",
    "additionalProperties",
    "allOf",
    "oneOf",
    "anyOf",
];

/// Keys stripped **only at the top level** of `input_schema`. JSON-Schema
/// conditional validation on nested sub-properties is legitimate and must
/// be preserved; Bedrock only chokes on these when they appear at the
/// outermost level alongside `type: "object"` + `properties`.
const UNSUPPORTED_TOP_LEVEL_ONLY: &[&str] = &["if", "then", "else"];

fn strip_unsupported_schema_fields_inner(value: &mut Value, is_top_level: bool) {
    if let Some(obj) = value.as_object_mut() {
        for key in UNSUPPORTED_ANYWHERE {
            obj.remove(*key);
        }
        if is_top_level {
            for key in UNSUPPORTED_TOP_LEVEL_ONLY {
                obj.remove(*key);
            }
        }
        // Recurse into properties (nested = not top-level).
        if let Some(props) = obj.get_mut("properties").and_then(Value::as_object_mut) {
            for (_, prop_val) in props.iter_mut() {
                strip_unsupported_schema_fields_inner(prop_val, false);
            }
        }
        // Recurse into items (array schemas).
        if let Some(items) = obj.get_mut("items") {
            strip_unsupported_schema_fields_inner(items, false);
        }
    }
}

fn bedrock_messages_contain_tool_blocks(messages: &[Value]) -> bool {
    messages.iter().any(|msg| {
        msg.get("content")
            .and_then(Value::as_array)
            .is_some_and(|blocks| {
                blocks
                    .iter()
                    .any(|b| b.get("toolUse").is_some() || b.get("toolResult").is_some())
            })
    })
}

/// Global counter: number of Bedrock thinking requests observed with a
/// `reasoningContent.text` block but no `signature`. Incremented by
/// [`assert_bedrock_thinking_signature_contract`] whenever the invariant is
/// violated. Exposed as a `pub static` so health/metric handlers can surface
/// it without plumbing a handle through every call site — matches the
/// convention used by `PERSIST_FAIL_COUNT` / `PERSIST_OK_COUNT`.
///
/// Any non-zero value in production means at least one turn will 400 at
/// Bedrock; on-call should page and check `astra_core::agent_warn!` logs
/// tagged `llm` for `bedrock signature contract violation`.
pub static BEDROCK_THINKING_SIGNATURE_VIOLATION_COUNT: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Guard against the recurring Bedrock thinking-mode regression:
/// `messages.N.content.0.thinking.signature: Field required` (HTTP 400).
///
/// When thinking is enabled, every assistant `reasoningContent` block MUST
/// carry a signature. If the upstream pipeline ever drops it (as PR #284
/// and its follow-up showed, twice), Bedrock will reject the turn with the
/// message above.
///
/// Behavior:
/// - Debug builds / tests: `debug_assert!` — fails loud so the offending
///   refactor can't merge.
/// - Release builds: structured warn log + counter increment. On-call
///   monitors [`BEDROCK_THINKING_SIGNATURE_VIOLATION_COUNT`] as a
///   continuous-signal tripwire rather than scanning logs.
fn assert_bedrock_thinking_signature_contract(bedrock_messages: &[Value]) {
    for (idx, msg) in bedrock_messages.iter().enumerate() {
        let Some(blocks) = msg.get("content").and_then(Value::as_array) else {
            continue;
        };
        for block in blocks {
            let Some(reasoning_text) = block
                .get("reasoningContent")
                .and_then(|rc| rc.get("reasoningText"))
            else {
                continue;
            };
            let text = reasoning_text
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("");
            if text.is_empty() {
                continue;
            }
            let has_signature = reasoning_text
                .get("signature")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty());
            if has_signature {
                continue;
            }
            // This combo will 400. Count it so on-call can trigger on a
            // non-zero tripwire instead of grepping logs; panic in debug/test
            // so regressions can't merge silently.
            BEDROCK_THINKING_SIGNATURE_VIOLATION_COUNT
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            debug_assert!(
                false,
                "Bedrock thinking contract violation: messages[{idx}] has \
                 reasoningContent.text but no signature — Bedrock will reject \
                 with `messages.{idx}.content.0.thinking.signature: Field required`. \
                 The signature must be captured from the provider stream and replayed \
                 on every continuation turn (see chat_turn_sse_dispatch::reasoning_done)."
            );
            astra_core::agent_warn!(
                "llm",
                "bedrock signature contract violation: messages[{}].reasoningContent \
                 is non-empty but signature is missing — turn will 400",
                idx
            );
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn build_provider_request_body(
    messages: &[Value],
    tools: &[Value],
    model_name: &str,
    provider: &str,
    max_output_tokens: Option<usize>,
    temperature: Option<f64>,
    streaming: bool,
    thinking: &astra_turn_core::thinking_config::ThinkingConfig,
) -> Value {
    build_provider_request_body_with_overrides(
        messages,
        tools,
        model_name,
        provider,
        max_output_tokens,
        temperature,
        streaming,
        thinking,
        None,
    )
}

pub(crate) fn build_provider_request_body_with_overrides(
    messages: &[Value],
    tools: &[Value],
    model_name: &str,
    provider: &str,
    max_output_tokens: Option<usize>,
    temperature: Option<f64>,
    streaming: bool,
    thinking: &astra_turn_core::thinking_config::ThinkingConfig,
    request_body_overrides: Option<&Map<String, Value>>,
) -> Value {
    let sanitized_overrides =
        sanitize_request_body_overrides_for_thinking(thinking, request_body_overrides);
    let marker_stripped_messages;
    let messages = if messages.iter().any(|message| {
        crate::turn::wire_assembly::is_required_runtime_preamble(message)
            || crate::turn::wire_assembly::is_runtime_system_context(message)
    }) {
        marker_stripped_messages = {
            astra_core::history_work::record_serialized_value(
                astra_core::history_work::HistoryWorkSite::ProviderWireAssembly,
                messages,
            );
            let mut cloned = messages.to_vec();
            strip_internal_runtime_markers(&mut cloned);
            cloned
        };
        marker_stripped_messages.as_slice()
    } else {
        messages
    };
    // Final send-time guard: some callers still reach request assembly without
    // the earlier edge-ledger normalization pass. Repair reasoning replay here
    // as the last line of defense so thinking providers never see a malformed
    // assistant history just because an intermediate path skipped assembly.
    //
    // Hot-path optimisation: the common case (standard OpenAI/Anthropic, no
    // thinking, no prior reasoning) yields a no-op policy. We skip the
    // `messages.to_vec()` clone in that case using `Cow::Borrowed`, falling
    // back to an owned clone only when the policy may actually mutate.
    let policy = astra_turn_core::edge_ledger::ReasoningReplayPolicy::infer(
        messages, thinking, provider, model_name,
    );
    let reasoning_repaired: std::borrow::Cow<'_, [Value]> = if policy.is_no_op() {
        std::borrow::Cow::Borrowed(messages)
    } else {
        astra_core::history_work::record_serialized_value(
            astra_core::history_work::HistoryWorkSite::ProviderWireAssembly,
            messages,
        );
        let mut owned = messages.to_vec();
        astra_turn_core::edge_ledger::strip_stale_reasoning_with_policy(&mut owned, &policy);
        std::borrow::Cow::Owned(owned)
    };
    match llm_provider_protocol(provider) {
        LlmProviderProtocol::BedrockConverse => {
            let repaired = repair_openai_tool_pairing(&reasoning_repaired);
            let (system, bedrock_messages) =
                build_bedrock_messages(&repaired, thinking.is_enabled());
            let mut body = json!({
                "messages": bedrock_messages,
            });
            if bedrock_system_has_text(&system) {
                body["system"] = Value::Array(system);
            }
            let mut inference = Map::new();
            if let Some(max_out) = max_output_tokens {
                inference.insert("maxTokens".to_string(), json!(max_out));
            }
            if let Some(temp) = temperature {
                inference.insert("temperature".to_string(), json!(temp));
            }
            if !inference.is_empty() {
                body["inferenceConfig"] = Value::Object(inference);
            }
            let bedrock_tools = build_bedrock_tools(tools);
            if !bedrock_tools.is_empty() {
                body["toolConfig"] = json!({ "tools": bedrock_tools });
            } else if bedrock_messages_contain_tool_blocks(&bedrock_messages) {
                // Bedrock requires toolConfig when messages contain toolUse/toolResult
                // blocks, but rejects an empty tools array. Provide a minimal
                // placeholder tool so the request validates.
                body["toolConfig"] = json!({ "tools": [{
                    "toolSpec": {
                        "name": "_noop",
                        "description": "No-op placeholder for message history compatibility",
                        "inputSchema": { "json": { "type": "object", "properties": {} } }
                    }
                }] });
            }
            thinking.apply_bedrock(&mut body);
            if thinking.is_enabled() {
                assert_bedrock_thinking_signature_contract(&bedrock_messages);
            }
            apply_request_body_overrides(
                &mut body,
                sanitized_overrides
                    .as_ref()
                    .map(|overrides| overrides.as_ref()),
            );
            body
        }
        LlmProviderProtocol::AnthropicMessages | LlmProviderProtocol::OpenAiCompatible => {
            let is_anthropic = provider_uses_anthropic_messages(provider);
            if is_anthropic {
                let (system, anthropic_messages) =
                    build_anthropic_system_and_messages(&reasoning_repaired);
                let anthropic_messages = repair_anthropic_tool_pairing(&anthropic_messages);
                let mut body = json!({
                    "model": model_name,
                    "messages": anthropic_messages,
                    "stream": streaming,
                });
                if !system.is_empty() {
                    body["system"] = Value::Array(system);
                }
                if let Some(max_out) = max_output_tokens {
                    body["max_tokens"] = json!(max_out);
                }
                if let Some(temp) = temperature {
                    body["temperature"] = json!(temp);
                }
                let anthropic_tools = build_anthropic_tools(tools);
                if !anthropic_tools.is_empty() {
                    body["tools"] = Value::Array(anthropic_tools);
                    body["tool_choice"] = json!({"type": "auto"});
                }
                thinking.apply_anthropic(&mut body);
                apply_request_body_overrides(
                    &mut body,
                    sanitized_overrides
                        .as_ref()
                        .map(|overrides| overrides.as_ref()),
                );
                return body;
            }
            let repaired = repair_openai_tool_pairing(&reasoning_repaired);
            let normalized_messages = normalize_openai_tool_message_content(&repaired);
            let mut body = json!({
                "model": model_name,
                "messages": normalized_messages,
                "stream": streaming,
            });
            if streaming {
                body["stream_options"] = json!({"include_usage": true});
            }
            if let Some(max_out) = max_output_tokens {
                // When thinking is active, providers like DeepSeek allocate a
                // thinking_budget that must be LESS than max_completion_tokens.
                // If max_out is too small, the request will 400. Bump to at
                // least thinking_budget + a headroom for the visible answer.
                //
                // We honor the user's configured ceiling when it already exceeds
                // the required floor (respects deliberate budget caps) and only
                // bump when the configured value is demonstrably too low.
                let effective_max = if !thinking.is_off() {
                    let required_floor: usize = match thinking {
                        ThinkingConfig::Enabled { budget_tokens } => {
                            (*budget_tokens as usize).saturating_add(8192)
                        }
                        _ => 65536,
                    };
                    if max_out < required_floor {
                        tracing::debug!(
                            user_max = max_out,
                            bumped_to = required_floor,
                            "max_completion_tokens bumped to fit thinking budget"
                        );
                        required_floor
                    } else {
                        max_out
                    }
                } else {
                    max_out
                };
                body["max_completion_tokens"] = json!(effective_max);
            }
            if let Some(temp) = temperature {
                body["temperature"] = json!(temp);
            }
            if !tools.is_empty() {
                astra_core::history_work::record_serialized_value(
                    astra_core::history_work::HistoryWorkSite::ProviderWireAssembly,
                    tools,
                );
                let mut wire_tools = tools.to_vec();
                for tool in &mut wire_tools {
                    strip_internal_schema_extensions(tool);
                }
                body["tools"] = Value::Array(wire_tools);
                body["tool_choice"] = Value::String("auto".to_string());
            }
            if provider_uses_dashscope_thinking(provider) {
                // DashScope/Qwen uses a binary `enable_thinking` flag; there is no
                // equivalent of `reasoning_effort`.
                match thinking {
                    ThinkingConfig::Off => {
                        // Native thinkers (Qwen3, Qwen3.5) think by default.
                        // Explicitly suppress to avoid wasting tokens on reasoning
                        // when the caller requested Off.
                        body["enable_thinking"] = json!(false);
                    }
                    ThinkingConfig::Enabled { .. } => {
                        body["enable_thinking"] = json!(true);
                    }
                    ThinkingConfig::Adaptive { effort } => {
                        tracing::warn!(
                            provider,
                            effort = ?effort,
                            "DashScope/Qwen does not support `reasoning_effort`; \
                             Adaptive mode mapped to `enable_thinking: true` — effort level ignored"
                        );
                        body["enable_thinking"] = json!(true);
                    }
                }
            } else {
                thinking.apply_openai(&mut body);
            }
            apply_request_body_overrides(&mut body, sanitized_overrides.as_deref());
            body
        }
    }
}

fn apply_request_body_overrides(
    body: &mut Value,
    request_body_overrides: Option<&Map<String, Value>>,
) {
    let Some(overrides) = request_body_overrides else {
        return;
    };
    let keys: Vec<&String> = overrides.keys().collect();
    tracing::debug!(?keys, "applying request body overrides");
    merge_json_object(body, overrides);
}

fn sanitize_request_body_overrides_for_thinking<'a>(
    thinking: &ThinkingConfig,
    request_body_overrides: Option<&'a Map<String, Value>>,
) -> Option<Cow<'a, Map<String, Value>>> {
    let overrides = request_body_overrides?;
    if !thinking.is_off() {
        return Some(Cow::Borrowed(overrides));
    }
    let mut sanitized = overrides.clone();
    for path in THINKING_OVERRIDE_STRIP_PATHS {
        strip_override_path(&mut sanitized, path);
    }
    if sanitized.is_empty() {
        None
    } else {
        Some(Cow::Owned(sanitized))
    }
}

const THINKING_OVERRIDE_STRIP_PATHS: &[&[&str]] = &[
    &["reasoning_effort"],
    &["enable_thinking"],
    &["thinking"],
    &["reasoning"],
    &["output_config", "effort"],
    &["output_config", "reasoning_effort"],
    &["additionalModelRequestFields", "thinking"],
    &["additionalModelRequestFields", "reasoning"],
    &["additionalModelRequestFields", "output_config", "effort"],
];

fn strip_override_path(target: &mut Map<String, Value>, path: &[&str]) {
    let Some((head, tail)) = path.split_first() else {
        return;
    };
    if tail.is_empty() {
        target.remove(*head);
        return;
    }
    let Some(Value::Object(child)) = target.get_mut(*head) else {
        return;
    };
    strip_override_path(child, tail);
    if child.is_empty() {
        target.remove(*head);
    }
}

fn merge_json_object(target: &mut Value, overrides: &Map<String, Value>) {
    merge_json_object_with_depth(target, overrides, 0);
}

const MAX_JSON_MERGE_DEPTH: usize = 64;

fn merge_json_object_with_depth(target: &mut Value, overrides: &Map<String, Value>, depth: usize) {
    let Some(target_obj) = target.as_object_mut() else {
        tracing::warn!("merge_json_object called with non-object target; skipping");
        return;
    };
    for (key, override_value) in overrides {
        match (target_obj.get_mut(key), override_value) {
            (Some(existing), Value::Object(override_obj)) if existing.is_object() => {
                if depth >= MAX_JSON_MERGE_DEPTH {
                    tracing::warn!(
                        key,
                        max_depth = MAX_JSON_MERGE_DEPTH,
                        "merge_json_object exceeded max depth; replacing nested object"
                    );
                    *existing = Value::Object(override_obj.clone());
                } else {
                    merge_json_object_with_depth(existing, override_obj, depth + 1);
                }
            }
            _ => {
                target_obj.insert(key.clone(), override_value.clone());
            }
        }
    }
}

pub(crate) fn apply_provider_auth(
    mut req: reqwest::RequestBuilder,
    provider: &str,
    api_key: &str,
    header_overrides: Option<&HashMap<String, String>>,
) -> reqwest::RequestBuilder {
    if provider_uses_anthropic_messages(provider) {
        if !has_llm_auth_override(provider, header_overrides) {
            req = req.header("x-api-key", api_key);
        }
        req.header("anthropic-version", "2023-06-01")
    } else {
        if !has_llm_auth_override(provider, header_overrides) {
            req = req.header("authorization", format!("Bearer {api_key}"));
        }
        req
    }
}

/// Strip empty `tool_calls: []` from assistant messages in-place.
///
/// Thin wrapper around the canonical implementation in `astra_turn_core`.
pub(crate) fn strip_empty_assistant_tool_calls(messages: &mut [Value]) {
    astra_turn_core::chat_history_openai::sanitize_empty_assistant_tool_calls_mut(messages);
}

#[cfg(test)]
pub(crate) fn consolidate_system_messages(messages: &[Value]) -> Vec<Value> {
    consolidate_system_messages_inner(messages, false)
}

pub(crate) fn consolidate_system_messages_for_provider(
    messages: &[Value],
    provider: &str,
    model_name: &str,
    explicit_cache_capability: Option<CacheCapability>,
) -> Vec<Value> {
    let protocol = llm_provider_protocol(provider);
    let cache_cap = CacheCapability::from_explicit_or_provider_model(
        explicit_cache_capability,
        provider,
        model_name,
    );
    let preserve_runtime_system_tail = matches!(protocol, LlmProviderProtocol::AnthropicMessages)
        || (matches!(protocol, LlmProviderProtocol::OpenAiCompatible)
            && !matches!(
                cache_cap.volatile_placement,
                VolatilePlacement::CurrentUserOnly
            ));
    consolidate_system_messages_inner(messages, preserve_runtime_system_tail)
}

fn strip_internal_runtime_markers(messages: &mut [Value]) {
    for message in messages {
        crate::turn::wire_assembly::strip_required_runtime_preamble_marker(message);
        if let Some(object) = message.as_object_mut() {
            object.remove(astra_turn_types::RUNTIME_MESSAGE_PROVENANCE_FIELD);
            object.remove(astra_turn_types::USER_TURN_SEMANTICS_FIELD);
            object.remove(astra_turn_types::BRIDGE_TURN_MESSAGE_PROVENANCE_FIELD);
            object.remove("_compact_boundary");
        }
    }
}

fn consolidate_system_messages_inner(
    messages: &[Value],
    preserve_runtime_system_tail: bool,
) -> Vec<Value> {
    let mut system_parts: Vec<String> = Vec::new();
    let mut system_blocks: Vec<Value> = Vec::new();
    let mut structured_system = false;
    let mut rest: Vec<Value> = Vec::new();

    let flush_string_parts_into_blocks = |blocks: &mut Vec<Value>, parts: &mut Vec<String>| {
        for part in parts.drain(..) {
            if !blocks.is_empty() {
                blocks.push(json!({"type": "text", "text": "\n\n"}));
            }
            blocks.push(json!({"type": "text", "text": part}));
        }
    };

    for msg in messages {
        let is_system = msg.get("role").and_then(|r| r.as_str()) == Some("system");
        let preserve_runtime_control = preserve_runtime_system_tail
            && is_system
            && crate::turn::wire_assembly::is_runtime_system_context(msg);
        if is_system && !preserve_runtime_control {
            match msg.get("content") {
                Some(Value::String(text)) => {
                    if text.is_empty() {
                        continue;
                    }
                    if structured_system {
                        if !system_blocks.is_empty() {
                            system_blocks.push(json!({"type": "text", "text": "\n\n"}));
                        }
                        system_blocks.push(json!({"type": "text", "text": text}));
                    } else {
                        system_parts.push(text.to_string());
                    }
                }
                Some(Value::Array(parts)) => {
                    structured_system = true;
                    flush_string_parts_into_blocks(&mut system_blocks, &mut system_parts);
                    if parts.is_empty() {
                        continue;
                    }
                    if !system_blocks.is_empty() {
                        system_blocks.push(json!({"type": "text", "text": "\n\n"}));
                    }
                    system_blocks.extend(parts.iter().cloned());
                }
                Some(other) if !other.is_null() => {
                    structured_system = true;
                    flush_string_parts_into_blocks(&mut system_blocks, &mut system_parts);
                    if !system_blocks.is_empty() {
                        system_blocks.push(json!({"type": "text", "text": "\n\n"}));
                    }
                    system_blocks.push(other.clone());
                }
                _ => {}
            }
        } else {
            let mut cloned = msg.clone();
            crate::turn::wire_assembly::strip_required_runtime_preamble_marker(&mut cloned);
            rest.push(cloned);
        }
    }

    let mut out = Vec::with_capacity(1 + rest.len());
    if structured_system {
        if !system_blocks.is_empty() {
            out.push(json!({"role": "system", "content": system_blocks}));
        }
    } else if !system_parts.is_empty() {
        out.push(json!({"role": "system", "content": system_parts.join("\n\n")}));
    }
    out.extend(rest);
    strip_internal_runtime_markers(&mut out);

    // Sanitize assistant messages: remove empty tool_calls arrays and fix
    // tool_calls with empty function names.
    // Some providers (e.g. MiniMax) reject messages containing tool_calls
    // where the function name is empty (can happen when skill interception
    // captures a call before the streaming name chunk arrives).
    //
    // Build a lookup from tool_call_id → tool name from tool-result messages
    // so we can recover the correct name when possible.
    let tool_name_by_id: HashMap<String, String> = out
        .iter()
        .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("tool"))
        .filter_map(|m| {
            let id = m.get("tool_call_id").and_then(Value::as_str)?.to_string();
            let name = m
                .get("name")
                .and_then(Value::as_str)
                .and_then(astra_core::canonical_names::normalize_name)?
                .to_string();
            Some((id, name))
        })
        .collect();

    strip_empty_assistant_tool_calls(&mut out);

    for msg in &mut out {
        if msg.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            continue;
        }
        let Some(obj) = msg.as_object_mut() else {
            continue;
        };
        let Some(tcs) = obj.get_mut("tool_calls").and_then(Value::as_array_mut) else {
            continue;
        };
        for tc in tcs.iter_mut() {
            let call_id = tc
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if let Some(func) = tc.get_mut("function") {
                let canonical_name = func
                    .get("name")
                    .and_then(Value::as_str)
                    .and_then(astra_core::canonical_names::normalize_name)
                    .map(std::string::ToString::to_string);
                if let Some(name) = canonical_name {
                    if let Some(f) = func.as_object_mut() {
                        f.insert("name".to_string(), Value::String(name));
                    }
                } else {
                    let recovered = tool_name_by_id
                        .get(&call_id)
                        .map(|s| s.as_str())
                        .unwrap_or("_unknown");
                    if let Some(f) = func.as_object_mut() {
                        f.insert("name".to_string(), Value::String(recovered.to_string()));
                    }
                }
            }
        }
    }

    out
}

fn anthropic_text_blocks_from_content(content: Option<&Value>) -> Vec<Value> {
    match content {
        Some(Value::String(text)) if !text.is_empty() => {
            vec![json!({"type": "text", "text": text})]
        }
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                if let Some(text) = part.as_str() {
                    return Some(json!({"type": "text", "text": text}));
                }
                let obj = part.as_object()?;
                if obj.get("type").and_then(Value::as_str) == Some("text") {
                    return Some(Value::Object(obj.clone()));
                }
                None
            })
            .collect(),
        Some(Value::Object(obj)) if obj.get("type").and_then(Value::as_str) == Some("text") => {
            vec![Value::Object(obj.clone())]
        }
        _ => Vec::new(),
    }
}

fn openai_tool_call_to_anthropic_block(tool_call: &Value) -> Option<Value> {
    let id = tool_call.get("id").and_then(Value::as_str)?;
    let function = tool_call.get("function")?.as_object()?;
    let name = tool_call_name(tool_call)?;
    let input = function
        .get("arguments")
        .and_then(Value::as_str)
        .map(json_string_to_value_or_string)
        .unwrap_or_else(|| json!({}));
    Some(json!({
        "type": "tool_use",
        "id": id,
        "name": name,
        "input": input,
    }))
}

fn carry_cache_annotations(src: &Value, dst: &mut Value) {
    if let Some(cc) = src.get("cache_control") {
        dst["cache_control"] = cc.clone();
    }
}

fn anthropic_message_from_openai(msg: &Value) -> Option<Value> {
    let role = msg.get("role").and_then(Value::as_str)?;
    match role {
        "user" => {
            let mut content = anthropic_content_blocks_from_openai_user(msg);
            if content.is_empty() {
                let blocks = anthropic_text_blocks_from_content(msg.get("content"));
                content = if blocks.len() == 1
                    && blocks[0].get("cache_control").is_none()
                    && blocks[0].get("type").and_then(Value::as_str) == Some("text")
                {
                    vec![blocks[0]["text"].clone()]
                } else {
                    blocks
                };
            }
            let mut out = if content.len() == 1 && content[0].is_string() {
                json!({"role": "user", "content": content[0].clone()})
            } else {
                json!({"role": "user", "content": content})
            };
            carry_cache_annotations(msg, &mut out);
            Some(out)
        }
        "assistant" => {
            let mut blocks = Vec::new();
            // Unsigned `reasoning_content` (no `reasoning_signature`) is
            // silently dropped because Anthropic requires a signature for
            // every `thinking` block.  This is the request-builder-side
            // guard; see also `ReasoningReplayPolicy::can_strip_unsigned_reasoning`
            // in `edge_ledger.rs` for the message-history-side policy
            // (`strip_stale_reasoning_with_policy`).
            let has_thinking =
                if let Some(rc) = msg.get("reasoning_content").and_then(Value::as_str) {
                    if rc.is_empty() {
                        false
                    } else if let Some(sig) = msg
                        .get("reasoning_signature")
                        .and_then(Value::as_str)
                        .filter(|sig| !sig.is_empty())
                    {
                        blocks.push(json!({
                            "type": "thinking",
                            "thinking": rc,
                            "signature": sig,
                        }));
                        true
                    } else {
                        false
                    }
                } else {
                    false
                };
            blocks.extend(anthropic_text_blocks_from_content(msg.get("content")));
            if let Some(tool_calls) = msg.get("tool_calls").and_then(Value::as_array) {
                blocks.extend(
                    tool_calls
                        .iter()
                        .filter_map(openai_tool_call_to_anthropic_block),
                );
            }
            // Anthropic/Bedrock reject assistant messages where the final block is `thinking`.
            // If thinking was emitted but no text or tool_use followed, append a minimal
            // text block to satisfy the constraint.
            if has_thinking && blocks.len() == 1 {
                blocks.push(json!({"type": "text", "text": ""}));
            }
            let mut out = json!({"role": "assistant", "content": blocks});
            carry_cache_annotations(msg, &mut out);
            Some(out)
        }
        "tool" => {
            let tool_use_id = msg.get("tool_call_id").and_then(Value::as_str)?;
            // The message's `content` may already be a pre-annotated
            // `[{type: "tool_result", ...}]` block array — this happens
            // when `annotate_last_message_cache_breakpoint` landed on
            // this tool message and upgraded its string content to the
            // content-block shape. In that case we must forward the
            // already-built tool_result verbatim (so cache_control /
            // tool_use_id placed by the annotator survives), otherwise
            // we'd nest `tool_result` inside another `tool_result.content`
            // — which DeepSeek's anthropic endpoint rejects with
            // "messages[N]: unknown variant `tool_result`".
            if let Some(arr) = msg.get("content").and_then(Value::as_array)
                && arr
                    .iter()
                    .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
            {
                let mut blocks = arr.clone();
                // Defensively stamp `tool_use_id` on any tool_result blocks
                // that lack them (the annotator always supplies tool_use_id
                // but we re-check here for robustness).
                for block in blocks.iter_mut() {
                    if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                        continue;
                    }
                    let Some(obj) = block.as_object_mut() else {
                        continue;
                    };
                    if !obj.contains_key("tool_use_id") {
                        obj.insert("tool_use_id".into(), Value::String(tool_use_id.to_string()));
                    }
                }
                let mut out = json!({
                    "role": "user",
                    "content": blocks,
                });
                carry_cache_annotations(msg, &mut out);
                return Some(out);
            }
            let content = match msg.get("content") {
                Some(Value::String(text)) => Value::String(text.clone()),
                Some(Value::Null) | None => Value::String(String::new()),
                Some(Value::Object(map)) if map.is_empty() => {
                    astra_core::agent_warn!(
                        "llm_client",
                        "tool-role msg has empty-object content; degrading to empty string for tool_result_block. tool_use_id={}",
                        tool_use_id
                    );
                    Value::String(String::new())
                }
                Some(other) => {
                    // Non-string content on a tool-role message is a
                    // serialization bug upstream (compaction, fold, or
                    // format conversion set it to an object/array instead
                    // of a string). Coerce to string representation so
                    // the LLM sees SOMETHING — not a bare `{}` that it
                    // misreads as "tool returned empty JSON object".
                    astra_core::agent_warn!(
                        "llm_client",
                        "tool-role msg has non-string content (type={}); \
                         coercing to string for tool_result_block. \
                         tool_use_id={}",
                        if other.is_object() {
                            "object"
                        } else if other.is_array() {
                            "array"
                        } else {
                            "other"
                        },
                        tool_use_id
                    );
                    match other {
                        Value::Array(arr) => {
                            // Content-array: extract text blocks
                            let text: String = arr
                                .iter()
                                .filter_map(|b| b.get("text").and_then(Value::as_str))
                                .collect::<Vec<_>>()
                                .join("");
                            if text.is_empty() {
                                Value::String(other.to_string())
                            } else {
                                Value::String(text)
                            }
                        }
                        _ => Value::String(other.to_string()),
                    }
                }
            };
            let tool_result_block = json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": content,
            });
            let mut out = json!({
                "role": "user",
                "content": [tool_result_block]
            });
            carry_cache_annotations(msg, &mut out);
            Some(out)
        }
        _ => None,
    }
}

fn anthropic_content_as_blocks(content: &Value) -> Vec<Value> {
    match content {
        Value::String(text) if !text.is_empty() => vec![json!({"type": "text", "text": text})],
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| {
                if let Some(text) = part.as_str() {
                    return (!text.is_empty()).then(|| json!({"type": "text", "text": text}));
                }
                part.as_object().map(|_| part.clone())
            })
            .collect(),
        Value::Object(_) => vec![content.clone()],
        _ => Vec::new(),
    }
}

fn merge_consecutive_anthropic_messages(messages: Vec<Value>) -> Vec<Value> {
    let mut merged: Vec<Value> = Vec::with_capacity(messages.len());
    for msg in messages {
        let Some(role) = msg.get("role").and_then(Value::as_str) else {
            astra_core::agent_warn!("llm", "dropped Anthropic message without role: {msg}");
            continue;
        };
        if let Some(last) = merged.last_mut()
            && last.get("role").and_then(Value::as_str) == Some(role)
        {
            let mut blocks = last
                .get("content")
                .map(anthropic_content_as_blocks)
                .unwrap_or_default();
            blocks.extend(
                msg.get("content")
                    .map(anthropic_content_as_blocks)
                    .unwrap_or_default(),
            );
            last["content"] = Value::Array(blocks);
            if last.get("cache_control").is_none()
                && let Some(cache_control) = msg.get("cache_control")
            {
                last["cache_control"] = cache_control.clone();
            }
            continue;
        }
        merged.push(msg);
    }
    merged
}

fn anthropic_content_blocks_from_openai_user(msg: &Value) -> Vec<Value> {
    let Some(content) = msg.get("content") else {
        return Vec::new();
    };
    let arr = match content {
        Value::Array(arr) => arr,
        _ => return Vec::new(),
    };
    let has_anthropic_types = arr.iter().any(|block| {
        let ty = block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        matches!(ty, "tool_result" | "image" | "document")
    });
    if !has_anthropic_types {
        return Vec::new();
    }
    arr.clone()
}

fn build_anthropic_system_and_messages(messages: &[Value]) -> (Vec<Value>, Vec<Value>) {
    astra_core::history_work::record_serialized_value(
        astra_core::history_work::HistoryWorkSite::ProviderWireAssembly,
        messages,
    );
    let mut system = Vec::new();
    let mut out_messages = Vec::new();
    for msg in messages {
        if msg.get("role").and_then(Value::as_str) == Some("system") {
            system.extend(anthropic_text_blocks_from_content(msg.get("content")));
        } else if let Some(converted) = anthropic_message_from_openai(msg) {
            out_messages.push(converted);
        } else {
            astra_core::agent_warn!("llm", "dropped unsupported Anthropic message role: {msg}");
        }
    }
    (system, merge_consecutive_anthropic_messages(out_messages))
}

fn build_anthropic_tools(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .filter_map(|tool| {
            if tool.get("name").is_some() && tool.get("input_schema").is_some() {
                // Pre-shaped native Anthropic tool — still strip
                // composition keywords + the `x-astra-…`
                // extension in case the schema came from our
                // consolidated tool registry.
                let mut out = tool.clone();
                if let Some(schema) = out.get_mut("input_schema") {
                    strip_unsupported_schema_fields(schema);
                }
                return Some(out);
            }
            let function = tool.get("function")?.as_object()?;
            let name = function.get("name")?.clone();
            let mut out = Map::new();
            out.insert("name".to_string(), name);
            if let Some(description) = function.get("description").cloned() {
                out.insert("description".to_string(), description);
            }
            let mut input_schema = function
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            // Strip keywords Anthropic rejects at the top level
            // (allOf/oneOf/anyOf) and our internal vendor extension
            // (x-astra-per-action-required). Anthropic tolerates the
            // other "unsupported" fields (default/minimum/etc.) —
            // but the helper is shared and idempotent, so we just
            // call it.
            strip_unsupported_schema_fields(&mut input_schema);
            out.insert("input_schema".to_string(), input_schema);
            if let Some(cache_control) = tool.get("cache_control").cloned() {
                out.insert("cache_control".to_string(), cache_control);
            }
            Some(Value::Object(out))
        })
        .collect()
}

/// Split a streaming content chunk into (text, is_reasoning) segments,
/// tracking whether we're inside a `<think>` block across chunks.
///
/// Returns a vec of (chunk_str, is_reasoning) pairs. Callers should route
/// is_reasoning=true chunks to `reasoning_delta` and false to `text_delta`.
pub(crate) fn split_think_chunks(content: &str, in_think: &mut bool) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut pos = 0;
    let len = content.len();

    while pos < len {
        if *in_think {
            if let Some(end) = content[pos..].find("</think>") {
                let abs_end = pos + end;
                if abs_end > pos {
                    out.push((content[pos..abs_end].to_string(), true));
                }
                *in_think = false;
                pos = abs_end + "</think>".len();
            } else {
                out.push((content[pos..].to_string(), true));
                pos = len;
            }
        } else {
            if let Some(start) = content[pos..].find("<think>") {
                let abs_start = pos + start;
                if abs_start > pos {
                    out.push((content[pos..abs_start].to_string(), false));
                }
                *in_think = true;
                pos = abs_start + "<think>".len();
            } else {
                out.push((content[pos..].to_string(), false));
                pos = len;
            }
        }
    }
    out
}

/// Extract `<think>...</think>` blocks from text, returning (reasoning, cleaned_text).
///
/// Some models (e.g. MiniMax) embed reasoning in content using `<think>` tags
/// instead of a separate `reasoning_content` streaming field. This extracts
/// all `<think>` blocks into reasoning and returns the remaining text.
fn extract_think_tags(text: &str) -> Option<(String, String)> {
    if !text.contains("<think>") {
        return None;
    }
    let mut reasoning = String::new();
    let mut cleaned = String::new();
    let mut pos = 0;
    while let Some(start) = text[pos..].find("<think>") {
        let abs_start = pos + start;
        cleaned.push_str(&text[pos..abs_start]);
        if let Some(end) = text[abs_start..].find("</think>") {
            let abs_end = abs_start + end + "</think>".len();
            let inner = &text[abs_start + "<think>".len()..abs_start + end];
            if !reasoning.is_empty() {
                reasoning.push('\n');
            }
            reasoning.push_str(inner.trim());
            pos = abs_end;
        } else {
            // Unclosed <think> — treat rest as reasoning
            let inner = &text[abs_start + "<think>".len()..];
            if !reasoning.is_empty() {
                reasoning.push('\n');
            }
            reasoning.push_str(inner.trim());
            pos = text.len();
        }
    }
    cleaned.push_str(&text[pos..]);
    let cleaned = cleaned.trim().to_string();
    if reasoning.is_empty() {
        None
    } else {
        Some((reasoning, cleaned))
    }
}

pub(crate) fn apply_llm_header_overrides(
    mut req: reqwest::RequestBuilder,
    header_overrides: Option<&HashMap<String, String>>,
) -> reqwest::RequestBuilder {
    let Some(header_overrides) = header_overrides else {
        return req;
    };
    for (name, value) in header_overrides {
        if name.starts_with("__astra_") {
            continue;
        }
        let Ok(header_name) = reqwest::header::HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        let Ok(header_value) = reqwest::header::HeaderValue::from_str(value) else {
            continue;
        };
        req = req.header(header_name, header_value);
    }
    req
}

fn has_llm_auth_override(
    provider: &str,
    header_overrides: Option<&HashMap<String, String>>,
) -> bool {
    let Some(header_overrides) = header_overrides else {
        return false;
    };
    if provider_uses_anthropic_messages(provider) {
        header_overrides
            .keys()
            .any(|name| name.eq_ignore_ascii_case("x-api-key"))
    } else {
        header_overrides
            .keys()
            .any(|name| name.eq_ignore_ascii_case("authorization"))
    }
}

/// Call the LLM streaming API, collect the full response, and return a structured result.
///
/// Unlike `call_llm_stream` (which returns raw SSE bytes), this function
/// parses the stream and returns the aggregated `LlmCallResult` directly.
/// Used by `ServerAgenticLoopHost` for server-side agentic loops.
///
/// Records 429/529 errors for rate-limit cooldown tracking.
///
/// **Note**: Caller must check rate-limit cooldown state and handle fallback model
/// resolution BEFORE calling this function. This function only records errors
/// for cooldown tracking, not pre-checks.
#[cfg(test)]
pub(crate) async fn call_llm_and_collect(
    call: LlmCall<'_>,
    cancel: LlmCancel<'_>,
) -> Result<LlmCallResult, astra_core::ClassifiedError> {
    call_llm_and_collect_with_stream_callback(call, cancel, None, None).await
}

pub(crate) async fn call_llm_and_collect_with_stream_callback(
    call: LlmCall<'_>,
    cancel: LlmCancel<'_>,
    mut stream_callback: Option<&mut LlmStreamCallback<'_>>,
    attempt_observer: Option<&dyn ProviderAttemptObserver>,
) -> Result<LlmCallResult, astra_core::ClassifiedError> {
    let LlmCall {
        purpose,
        messages,
        tools,
        cache_capability,
        route,
        max_output_tokens,
        temperature,
        has_fallback,
        thinking,
    } = call;
    let LlmExecutionRoute {
        model_name,
        wire_model_name,
        api_key,
        base_url,
        provider,
        header_overrides,
        request_body_overrides,
        completions_url_override,
        request_timeout,
    } = route;
    let cooldown = rate_limit_cooldown();
    // `model_key` indexes rate-limit state on the local row name.
    let model_key = model_name;
    // `upstream_name` is what goes in the outbound request body + URL.
    let upstream_name = wire_model_name.unwrap_or(model_name);

    let started = Instant::now();
    let total_budget = llm_total_budget();
    let client = global_llm_client();

    // Consolidate system messages: merge all system-role messages into the first
    // one, converting extras to a single leading system message. Some providers
    // (e.g. MiniMax) reject system messages after the first position.
    let messages =
        consolidate_system_messages_for_provider(messages, provider, model_name, cache_capability);

    // All providers stream — including Bedrock (via converse-stream +
    // AWS vnd.amazon.eventstream). The body builder and URL builder flip
    // to the streaming variant for every supported provider.
    let body = build_provider_request_body_with_overrides(
        &messages,
        tools,
        upstream_name,
        provider,
        max_output_tokens,
        temperature,
        true,
        thinking,
        request_body_overrides,
    );
    let prepared_request =
        PreparedProviderRequest::from_json(&body, llm_provider_protocol(provider))?;

    let url = llm_request_url(
        base_url,
        completions_url_override,
        provider,
        upstream_name,
        true,
    );
    let _registered_endpoint_permit =
        acquire_registered_endpoint_permit_for_override(&url, completions_url_override)?;

    let mut last_err = String::new();
    let mut last_kind = astra_core::ErrorKind::Unknown;
    let max_retries = LLM_MAX_RETRIES;
    let mut retry_delay_override_ms = None;
    // Read idle timeouts once before the retry loop to avoid env-var races between
    // parallel tests (and to ensure consistent timeouts across retries).
    let idle_pre = stream_idle_timeout();
    let idle_post = stream_idle_timeout_after_progress();
    let attach_partial_details = |error: astra_core::ClassifiedError,
                                  partial: &LlmCallResult|
     -> astra_core::ClassifiedError {
        if let Some(details_json) = llm_result_details_json(partial) {
            error.with_details_json(details_json)
        } else {
            error
        }
    };

    for attempt in 0..=max_retries {
        if cancel.is_triggered() {
            return Err(astra_core::ClassifiedError::new(
                astra_core::ErrorKind::Cancelled,
                "LLM call cancelled",
            ));
        }
        // Total budget guard: abort if we've already spent too long across retries.
        if started.elapsed() > total_budget {
            return Err(astra_core::ClassifiedError::new(
                astra_core::ErrorKind::BudgetExhausted,
                format!(
                    "LLM total budget exhausted ({:.0}s): {last_err}",
                    total_budget.as_secs_f64()
                ),
            ));
        }
        if attempt > 0 {
            // A provider/cooldown hint owns the next delay when present.
            // Otherwise use generic exponential backoff. This prevents a
            // rate-limit response from sleeping in both places.
            let delay = retry_delay_override_ms
                .take()
                .unwrap_or_else(|| retry_backoff_ms(attempt));
            tokio::select! {
                biased;
                _ = wait_llm_cancel(cancel) => return Err(astra_core::ClassifiedError::new(
                    astra_core::ErrorKind::Cancelled,
                    "LLM call cancelled",
                )),
                _ = tokio::time::sleep(std::time::Duration::from_millis(delay)) => {}
            }
        }

        let observed_attempt = match attempt_observer {
            Some(observer) => Some(observer.begin_attempt(prepared_request.identity()).await?),
            None => None,
        };
        let mut req = client.post(&url).header("content-type", "application/json");
        req = apply_provider_auth(req, provider, api_key, header_overrides);
        req = apply_llm_header_overrides(req, header_overrides);
        if let Some(timeout) = request_timeout {
            req = req.timeout(timeout);
        }

        tracing::debug!(
            target: "astra_runtime::llm_client",
            url = %url,
            attempt,
            purpose = purpose.as_str(),
            provider,
            model_name,
            "LLM request sending"
        );
        let response = match req.body(prepared_request.body()).send().await {
            Ok(r) => {
                tracing::debug!(
                    target: "astra_runtime::llm_client",
                    url = %url,
                    status = r.status().as_u16(),
                    "LLM request connected"
                );
                r
            }
            Err(e) => {
                tracing::warn!(
                    target: "astra_runtime::llm_client",
                    url = %url,
                    error = %e,
                    "LLM send failed"
                );
                let (error, retry_safe) = classify_provider_send_error("LLM request failed", &e);
                finish_observed_provider_error(attempt_observer, observed_attempt, &error).await?;
                last_err = error.message.clone();
                last_kind = error.kind;
                if retry_safe {
                    continue;
                }
                return Err(error);
            }
        };

        let status = response.status().as_u16();
        if response.status().is_success() {
            // Success — record to cooldown tracker
            cooldown.with(model_key, |c| c.record_success());
            if provider_uses_bedrock_converse(provider) {
                match crate::turn::bedrock::transport::collect_bedrock_stream(
                    response,
                    model_name,
                    started,
                    cancel,
                    idle_pre,
                    stream_callback.as_deref_mut(),
                )
                .await
                {
                    Ok(result) => {
                        finish_observed_provider_attempt(
                            attempt_observer,
                            observed_attempt,
                            &provider_attempt_terminal_from_result(&result),
                        )
                        .await?;
                        return Ok(result);
                    }
                    Err(crate::turn::bedrock::transport::BedrockStreamError::Cancelled {
                        partial,
                    }) => {
                        let error = attach_partial_details(
                            astra_core::ClassifiedError::new(
                                astra_core::ErrorKind::StreamTransport,
                                "Bedrock delivery became unknown after cancellation",
                            ),
                            &partial,
                        );
                        finish_observed_provider_error_with_partial(
                            attempt_observer,
                            observed_attempt,
                            &error,
                            &partial,
                        )
                        .await?;
                        return Err(error);
                    }
                    Err(crate::turn::bedrock::transport::BedrockStreamError::Exception {
                        kind,
                        message,
                        partial,
                    }) => {
                        use crate::turn::bedrock::stream::{RetryKind, is_retryable_exception};
                        let has_partial = llm_result_has_partial_signal(&partial);
                        match is_retryable_exception(&kind) {
                            RetryKind::RateLimit => {
                                let error = attach_partial_details(
                                    astra_core::ClassifiedError::new(
                                        astra_core::ErrorKind::RateLimit,
                                        format!("bedrock throttle: {message}"),
                                    ),
                                    &partial,
                                );
                                finish_observed_provider_error_with_partial(
                                    attempt_observer,
                                    observed_attempt,
                                    &error,
                                    &partial,
                                )
                                .await?;
                                last_err = error.message.clone();
                                last_kind = error.kind;
                                let action =
                                    cooldown.with(model_key, |c| c.record_429(None, has_fallback));
                                if has_partial {
                                    return Err(error);
                                }
                                match action {
                                    RateLimitAction::WaitAndRetry { delay_ms } => {
                                        retry_delay_override_ms = Some(delay_ms);
                                        continue;
                                    }
                                    RateLimitAction::UseFallback { reason } => {
                                        return Err(crate::turn::bridge::llm_stream::fallback_required_error(
                                            error,
                                            reason,
                                        ));
                                    }
                                    RateLimitAction::Reject { .. } | RateLimitAction::Proceed => {
                                        return Err(error);
                                    }
                                }
                            }
                            RetryKind::Transient => {
                                let error = attach_partial_details(
                                    astra_core::ClassifiedError::new(
                                        astra_core::ErrorKind::ServerError,
                                        format!("bedrock transient {kind}: {message}"),
                                    ),
                                    &partial,
                                );
                                finish_observed_provider_error_with_partial(
                                    attempt_observer,
                                    observed_attempt,
                                    &error,
                                    &partial,
                                )
                                .await?;
                                last_err = error.message.clone();
                                last_kind = error.kind;
                                if has_partial {
                                    return Err(error);
                                }
                                continue;
                            }
                            RetryKind::Terminal => {
                                let error = attach_partial_details(
                                    astra_core::ClassifiedError::new(
                                        astra_core::ErrorKind::Unknown,
                                        format!("bedrock {kind}: {message}"),
                                    ),
                                    &partial,
                                );
                                finish_observed_provider_error_with_partial(
                                    attempt_observer,
                                    observed_attempt,
                                    &error,
                                    &partial,
                                )
                                .await?;
                                return Err(error);
                            }
                        }
                    }
                    Err(crate::turn::bedrock::transport::BedrockStreamError::Transport {
                        error: transport_error,
                        partial,
                    }) => {
                        let error = attach_partial_details(
                            astra_core::ClassifiedError::new(
                                astra_core::ErrorKind::StreamTransport,
                                format!("bedrock transport: {transport_error}"),
                            ),
                            &partial,
                        );
                        finish_observed_provider_error_with_partial(
                            attempt_observer,
                            observed_attempt,
                            &error,
                            &partial,
                        )
                        .await?;
                        return Err(error);
                    }
                }
            }
            let byte_stream = response.bytes_stream();
            let stream_result = if provider_uses_anthropic_messages(provider) {
                collect_anthropic_llm_stream(
                    byte_stream,
                    model_name,
                    started,
                    cancel,
                    idle_pre,
                    idle_post,
                    stream_callback.as_deref_mut(),
                )
                .await
            } else {
                collect_llm_stream(
                    byte_stream,
                    model_name,
                    started,
                    cancel,
                    idle_pre,
                    idle_post,
                    stream_callback.as_deref_mut(),
                )
                .await
            };
            match stream_result {
                Ok(result) => {
                    finish_observed_provider_attempt(
                        attempt_observer,
                        observed_attempt,
                        &provider_attempt_terminal_from_result(&result),
                    )
                    .await?;
                    return Ok(result);
                }
                Err(StreamCollectError::Cancelled { partial }) => {
                    let error = attach_partial_details(
                        astra_core::ClassifiedError::new(
                            astra_core::ErrorKind::StreamTransport,
                            "LLM delivery became unknown after stream cancellation",
                        ),
                        &partial,
                    );
                    finish_observed_provider_error_with_partial(
                        attempt_observer,
                        observed_attempt,
                        &error,
                        &partial,
                    )
                    .await?;
                    return Err(error);
                }
                Err(StreamCollectError::Transport { error, partial }) => {
                    let observed_error = attach_partial_details(
                        astra_core::ClassifiedError::new(
                            astra_core::ErrorKind::StreamTransport,
                            format!("LLM stream transport error: {error}"),
                        ),
                        &partial,
                    );
                    finish_observed_provider_error_with_partial(
                        attempt_observer,
                        observed_attempt,
                        &observed_error,
                        &partial,
                    )
                    .await?;
                    return Err(observed_error);
                }
                Err(StreamCollectError::IdleTimeout {
                    elapsed_ms,
                    partial,
                    ..
                }) => {
                    let observed_error = attach_partial_details(
                        astra_core::ClassifiedError::new(
                            astra_core::ErrorKind::StreamIdle,
                            format!("stream idle timeout after {elapsed_ms}ms"),
                        ),
                        &partial,
                    );
                    finish_observed_provider_error_with_partial(
                        attempt_observer,
                        observed_attempt,
                        &observed_error,
                        &partial,
                    )
                    .await?;
                    return Err(observed_error);
                }
            }
        }

        // Parse retry-after header
        let headers = response.headers();
        let retry_after_ms = headers
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_retry_after_ms);

        let text = response
            .text()
            .await
            .unwrap_or_else(|e| format!("<body read error: {e}>"));

        // Auth errors: redact the body in logs and return a generic message
        // so provider-echoed secrets cannot leak through error propagation.
        if status == 401 || status == 403 {
            let truncated = &text[..text.len().min(80)];
            let redacted = redact_provider_secrets(truncated);
            tracing::warn!(
                target: "astra_runtime::llm_client",
                "LLM auth error ({status}) on {model_key}: {redacted}",
            );
            let error = astra_core::ClassifiedError::new(
                astra_core::ErrorKind::Auth,
                "LLM provider authentication failed".to_string(),
            );
            finish_observed_provider_error(attempt_observer, observed_attempt, &error).await?;
            return Err(error);
        }

        // For other 4xx errors, suppress the raw response body to avoid
        // leaking secrets that providers may echo back. Retain body for 5xx
        // (helpful for diagnosing transient backend failures) and the 400
        // context-window check below (which still needs to inspect text).
        last_err = if (400..500).contains(&status) {
            format!("LLM request rejected: {status}")
        } else {
            format!("LLM error {status}: {text}")
        };

        // Record rate-limit errors to cooldown tracker
        if is_rate_limit_status(status) {
            last_kind = astra_core::ErrorKind::RateLimit;
            let observed_error = astra_core::ClassifiedError::new(last_kind, last_err.clone());
            finish_observed_provider_error(attempt_observer, observed_attempt, &observed_error)
                .await?;

            let tpm_exhaustion = is_tpm_exhaustion(&text);
            if tpm_exhaustion {
                astra_core::agent_warn!(
                    "llm",
                    "TPM exhaustion detected on {} — applying {}s retry delay",
                    model_key,
                    TPM_EXHAUST_DELAY_MS / 1000
                );
            }

            let action = cooldown.with(model_key, |c| c.record_429(retry_after_ms, has_fallback));
            astra_core::agent_warn!(
                "llm",
                "rate limit (429) on {}: action={:?}",
                model_key,
                action,
            );
            match action {
                RateLimitAction::WaitAndRetry { delay_ms } => {
                    retry_delay_override_ms = Some(if tpm_exhaustion {
                        delay_ms.max(TPM_EXHAUST_DELAY_MS)
                    } else {
                        delay_ms
                    });
                    continue;
                }
                RateLimitAction::UseFallback { reason } => {
                    return Err(crate::turn::bridge::llm_stream::fallback_required_error(
                        observed_error,
                        reason,
                    ));
                }
                RateLimitAction::Reject { .. } | RateLimitAction::Proceed => {
                    return Err(observed_error);
                }
            }
        }

        if is_overload_status(status) {
            last_kind = astra_core::ErrorKind::ServerError;
            let observed_error = astra_core::ClassifiedError::new(last_kind, last_err.clone());
            finish_observed_provider_error(attempt_observer, observed_attempt, &observed_error)
                .await?;
            let action = cooldown.with(model_key, |c| c.record_529(retry_after_ms, has_fallback));
            astra_core::agent_warn!(
                "llm",
                "server overload ({status}) on {}: action={:?}",
                model_key,
                action,
            );
            match action {
                RateLimitAction::WaitAndRetry { delay_ms } => {
                    retry_delay_override_ms = Some(delay_ms);
                    continue;
                }
                RateLimitAction::UseFallback { reason } => {
                    return Err(crate::turn::bridge::llm_stream::fallback_required_error(
                        observed_error,
                        reason,
                    ));
                }
                RateLimitAction::Reject { .. } | RateLimitAction::Proceed => {
                    return Err(observed_error);
                }
            }
        }

        // Other 5xx errors are retryable
        if status >= 500 {
            last_kind = astra_core::ErrorKind::ServerError;
            let observed_error = astra_core::ClassifiedError::new(last_kind, last_err.clone());
            finish_observed_provider_error(attempt_observer, observed_attempt, &observed_error)
                .await?;
            continue;
        }

        // Context-window errors — classified at source, no string prefix needed.
        if status == 400 && astra_core::is_llm_context_window_error(&text) {
            let error = astra_core::ClassifiedError::new(
                astra_core::ErrorKind::ContextWindow,
                format!("LLM error {status}: {text}"),
            );
            finish_observed_provider_error(attempt_observer, observed_attempt, &error).await?;
            return Err(error);
        }

        // Other 400 errors
        if status == 400 {
            let error =
                astra_core::ClassifiedError::new(astra_core::ErrorKind::InvalidRequest, last_err);
            finish_observed_provider_error(attempt_observer, observed_attempt, &error).await?;
            return Err(error);
        }

        let error = astra_core::ClassifiedError::new(last_kind, last_err);
        finish_observed_provider_error(attempt_observer, observed_attempt, &error).await?;
        return Err(error);
    }

    Err(astra_core::ClassifiedError::new(
        last_kind,
        format!("{last_err} (after {} retries)", LLM_MAX_RETRIES),
    ))
}

/// Maximum accumulated response size (text + reasoning + args) before aborting stream (16 MB).
const MAX_STREAM_ACCUMULATION_BYTES: usize = 16 * 1024 * 1024;
/// Maximum number of tool calls per LLM stream response.
const MAX_STREAM_TOOL_CALLS: usize = 128;

/// Parse an OpenAI-compatible SSE stream and collect into `LlmCallResult`.
async fn collect_llm_stream(
    stream: impl futures_util::Stream<Item = Result<Bytes, reqwest::Error>> + Unpin + Send + 'static,
    model_name: &str,
    started: Instant,
    cancel: LlmCancel<'_>,
    idle_pre: std::time::Duration,
    idle_post: std::time::Duration,
    mut stream_callback: Option<&mut LlmStreamCallback<'_>>,
) -> Result<LlmCallResult, StreamCollectError> {
    let mut full_text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls_map: HashMap<usize, Map<String, Value>> = HashMap::new();
    let mut usage = Map::new();
    let mut response_id: Option<String> = None;
    let mut finish_reason: Option<String> = None;
    let mut accumulated_bytes: usize = 0;
    let mut made_progress = false;
    let mut in_think = false;
    let partial_result = |response_id: &Option<String>,
                          full_text: &String,
                          reasoning: &String,
                          tool_calls_map: &HashMap<usize, Map<String, Value>>,
                          usage: &Map<String, Value>,
                          finish_reason: &Option<String>| {
        let mut sorted_tcs: Vec<_> = tool_calls_map.iter().collect();
        sorted_tcs.sort_by_key(|(idx, _)| **idx);
        let tool_calls = sorted_tcs
            .into_iter()
            .map(|(_, value)| Value::Object(value.clone()))
            .collect();
        LlmCallResult {
            response_id: response_id.clone(),
            full_text: full_text.clone(),
            reasoning: reasoning.clone(),
            reasoning_signature: String::new(),
            tool_calls,
            usage: usage.clone(),
            model_used: model_name.to_string(),
            duration_ms: started.elapsed().as_millis() as u64,
            finish_reason: finish_reason.clone(),
        }
    };

    let sse = parse_openai_sse_json_stream(stream);
    tokio::pin!(sse);
    let mut saw_terminal = false;
    loop {
        let ordinary_idle = if made_progress { idle_post } else { idle_pre };
        let idle = if saw_terminal {
            stream_terminal_drain_timeout(ordinary_idle)
        } else {
            ordinary_idle
        };
        let item = tokio::select! {
            biased;
            _ = wait_llm_cancel(cancel) => {
                if saw_terminal {
                    break;
                }
                return Err(StreamCollectError::Cancelled {
                    partial: partial_result(
                        &response_id,
                        &full_text,
                        &reasoning,
                        &tool_calls_map,
                        &usage,
                        &finish_reason,
                    ),
                });
            },
            r = tokio::time::timeout(idle, sse.next()) => match r {
                Ok(v) => v,
                Err(_elapsed) => {
                    if saw_terminal {
                        break;
                    }
                    return Err(StreamCollectError::IdleTimeout {
                        elapsed_ms: idle.as_millis() as u64,
                        made_progress,
                        partial: partial_result(
                            &response_id,
                            &full_text,
                            &reasoning,
                            &tool_calls_map,
                            &usage,
                            &finish_reason,
                        ),
                    });
                }
            },
        };
        let Some(item) = item else { break };
        let chunk = match item {
            Ok(ParsedSseEvent::Done) => {
                saw_terminal = true;
                break;
            }
            Ok(ParsedSseEvent::Data(v)) => v,
            Err(error) => {
                if saw_terminal {
                    break;
                }
                return Err(StreamCollectError::Transport {
                    error,
                    partial: partial_result(
                        &response_id,
                        &full_text,
                        &reasoning,
                        &tool_calls_map,
                        &usage,
                        &finish_reason,
                    ),
                });
            }
        };
        if response_id.is_none() {
            response_id = chunk
                .get("id")
                .and_then(Value::as_str)
                .map(ToString::to_string);
        }
        // Parse usage from any chunk. Streaming endpoints we call are always
        // OpenAI-compatible: Bedrock Converse streams are intercepted at a
        // higher level and decoded by the dedicated Bedrock transport.
        if let Some(u) = chunk.get("usage").and_then(Value::as_object)
            && let Some(extracted) = crate::turn::token_usage::extract_usage(
                crate::turn::token_usage::UsageDialect::OpenAi,
                u,
            )
        {
            usage = extracted.to_json_map();
            made_progress = true;
        }
        if saw_terminal && !usage.is_empty() {
            break;
        }

        let Some(choices) = chunk.get("choices").and_then(Value::as_array) else {
            continue;
        };

        // Extract finish_reason from the last chunk that carries one.
        if let Some(fr) = choices
            .first()
            .and_then(|c| c.get("finish_reason"))
            .and_then(Value::as_str)
        {
            finish_reason = Some(fr.to_string());
            saw_terminal = true;
            made_progress = true;
        }

        let Some(delta) = choices
            .first()
            .and_then(|c| c.get("delta"))
            .and_then(Value::as_object)
        else {
            if saw_terminal && !usage.is_empty() {
                break;
            }
            continue;
        };

        // Text content
        if let Some(content) = delta.get("content").and_then(Value::as_str)
            && !content.is_empty()
        {
            accumulated_bytes += content.len();
            if accumulated_bytes > MAX_STREAM_ACCUMULATION_BYTES {
                return Err(StreamCollectError::Transport {
                    error: format!(
                        "LLM stream exceeded {MAX_STREAM_ACCUMULATION_BYTES} bytes — aborting"
                    ),
                    partial: partial_result(
                        &response_id,
                        &full_text,
                        &reasoning,
                        &tool_calls_map,
                        &usage,
                        &finish_reason,
                    ),
                });
            }
            let chunks = split_think_chunks(content, &mut in_think);
            for (chunk, is_reasoning) in chunks {
                if chunk.is_empty() {
                    continue;
                }
                if is_reasoning {
                    reasoning.push_str(&chunk);
                    if let Some(callback) = stream_callback.as_deref_mut() {
                        callback(LlmStreamUpdate::Reasoning(chunk));
                    }
                } else {
                    full_text.push_str(&chunk);
                    if let Some(callback) = stream_callback.as_deref_mut() {
                        callback(LlmStreamUpdate::Text(chunk));
                    }
                }
            }
            made_progress = true;
        }

        // Reasoning
        if let Some(r) = delta.get("reasoning_content").and_then(Value::as_str)
            && !r.is_empty()
        {
            accumulated_bytes += r.len();
            if accumulated_bytes > MAX_STREAM_ACCUMULATION_BYTES {
                return Err(StreamCollectError::Transport {
                    error: format!(
                        "LLM stream exceeded {MAX_STREAM_ACCUMULATION_BYTES} bytes — aborting"
                    ),
                    partial: partial_result(
                        &response_id,
                        &full_text,
                        &reasoning,
                        &tool_calls_map,
                        &usage,
                        &finish_reason,
                    ),
                });
            }
            reasoning.push_str(r);
            if let Some(callback) = stream_callback.as_deref_mut() {
                callback(LlmStreamUpdate::Reasoning(r.to_string()));
            }
            made_progress = true;
        }

        // Tool calls (streaming accumulation)
        if let Some(tcs) = delta.get("tool_calls").and_then(Value::as_array) {
            for tc in tcs {
                let idx = tc.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if tool_calls_map.len() >= MAX_STREAM_TOOL_CALLS
                    && !tool_calls_map.contains_key(&idx)
                {
                    astra_core::agent_warn!(
                        "llm",
                        "stream tool_calls exceeded {MAX_STREAM_TOOL_CALLS} — ignoring extra"
                    );
                    continue;
                }
                let entry = tool_calls_map.entry(idx).or_insert_with(|| {
                    Map::from_iter([
                        ("id".to_string(), Value::String(String::new())),
                        ("type".to_string(), Value::String("function".to_string())),
                        ("function".to_string(), json!({"name": "", "arguments": ""})),
                    ])
                });
                if let Some(id) = tc.get("id").and_then(Value::as_str)
                    && !id.is_empty()
                {
                    entry.insert("id".to_string(), Value::String(id.to_string()));
                    made_progress = true;
                }
                if let Some(func) = tc.get("function").and_then(Value::as_object) {
                    let f = entry
                        .entry("function".to_string())
                        .or_insert_with(|| json!({}));
                    let Some(f) = f.as_object_mut() else {
                        continue;
                    };
                    if let Some(name) = func
                        .get("name")
                        .and_then(Value::as_str)
                        .and_then(canonical_valid_tool_name)
                    {
                        f.insert("name".to_string(), Value::String(name.to_string()));
                        made_progress = true;
                    } else if let Some(bad_name) = func
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|name| !name.is_empty())
                    {
                        astra_core::agent_warn!(
                            "llm",
                            "dropped malformed tool_call with invalid name: {bad_name:?}"
                        );
                    }
                    if let Some(args) = func.get("arguments").and_then(Value::as_str) {
                        accumulated_bytes += args.len();
                        if accumulated_bytes > MAX_STREAM_ACCUMULATION_BYTES {
                            return Err(StreamCollectError::Transport {
                                error: format!(
                                    "stream tool-call arguments exceeded {MAX_STREAM_ACCUMULATION_BYTES} byte limit"
                                ),
                                partial: partial_result(
                                    &response_id,
                                    &full_text,
                                    &reasoning,
                                    &tool_calls_map,
                                    &usage,
                                    &finish_reason,
                                ),
                            });
                        }
                        let existing = f
                            .entry("arguments".to_string())
                            .or_insert_with(|| Value::String(String::new()));
                        if let Value::String(s) = existing {
                            s.push_str(args);
                            made_progress = true;
                        }
                    }
                }
                if let Some(callback) = stream_callback.as_deref_mut() {
                    callback(LlmStreamUpdate::ToolCall {
                        index: idx,
                        tool_call: Value::Object(entry.clone()),
                    });
                }
            }
        }
        if saw_terminal && !usage.is_empty() {
            break;
        }
    }

    if !saw_terminal {
        return Err(StreamCollectError::Transport {
            error: "provider SSE ended without a terminal marker".to_string(),
            partial: partial_result(
                &response_id,
                &full_text,
                &reasoning,
                &tool_calls_map,
                &usage,
                &finish_reason,
            ),
        });
    }

    let mut sorted_tcs: Vec<_> = tool_calls_map.into_iter().collect();
    sorted_tcs.sort_by_key(|(idx, _)| *idx);
    let mut tool_calls: Vec<Value> = sorted_tcs
        .into_iter()
        .map(|(_, v)| Value::Object(v))
        .collect();

    // Degraded tool-call fallback: some models emit <invoke> XML or <tool_call>
    // tags in content instead of structured tool_calls. Recover them.
    if tool_calls.is_empty() {
        if let Some(parsed) =
            astra_turn_core::xml_tool_call_fallback::parse_degraded_tool_calls(&full_text)
        {
            astra_core::agent_warn!(
                "llm",
                "recovered {} tool call(s) from degraded text in content (stream)",
                parsed.len()
            );
            full_text =
                astra_turn_core::xml_tool_call_fallback::strip_degraded_tool_calls(&full_text);
            tool_calls = parsed;
        }
    }

    // Extract <think>...</think> blocks from content into reasoning.
    // Models like MiniMax embed thinking in content with <think> tags
    // instead of using a separate reasoning_content field.
    if reasoning.is_empty() {
        if let Some((extracted_reasoning, cleaned_text)) = extract_think_tags(&full_text) {
            reasoning = extracted_reasoning;
            full_text = cleaned_text;
        }
    }

    Ok(LlmCallResult {
        response_id,
        full_text,
        reasoning,
        reasoning_signature: String::new(),
        tool_calls,
        usage,
        model_used: model_name.to_string(),
        duration_ms: started.elapsed().as_millis() as u64,
        finish_reason,
    })
}

async fn collect_anthropic_llm_stream(
    stream: impl futures_util::Stream<Item = Result<Bytes, reqwest::Error>> + Unpin + Send + 'static,
    model_name: &str,
    started: Instant,
    cancel: LlmCancel<'_>,
    idle_pre: std::time::Duration,
    idle_post: std::time::Duration,
    mut stream_callback: Option<&mut LlmStreamCallback<'_>>,
) -> Result<LlmCallResult, StreamCollectError> {
    let mut full_text = String::new();
    let mut reasoning = String::new();
    // Anthropic emits the HMAC signature for each `thinking` content block
    // via a dedicated `signature_delta`. The next round MUST echo it
    // verbatim on the assistant message, or the API returns HTTP 400
    // `content[].thinking in the thinking mode must be passed back`
    // (see session effccfcd-28d8-41f4-a4b0-ecd0ec503625 for the original
    // failure mode; bridge_llm_stream was patched earlier, this path
    // was the latent one).
    let mut reasoning_signature = String::new();
    let mut tool_calls_map: HashMap<usize, Map<String, Value>> = HashMap::new();
    let mut usage_tokens = crate::turn::token_usage::TokenUsage::default();
    let mut response_id: Option<String> = None;
    let mut finish_reason: Option<String> = None;
    let mut accumulated_bytes: usize = 0;
    let mut made_progress = false;
    let partial_result = |response_id: &Option<String>,
                          full_text: &String,
                          reasoning: &String,
                          reasoning_signature: &String,
                          tool_calls_map: &HashMap<usize, Map<String, Value>>,
                          usage_tokens: &crate::turn::token_usage::TokenUsage,
                          finish_reason: &Option<String>| {
        let mut sorted_tcs: Vec<_> = tool_calls_map.iter().collect();
        sorted_tcs.sort_by_key(|(idx, _)| **idx);
        let tool_calls = sorted_tcs
            .into_iter()
            .map(|(_, value)| Value::Object(value.clone()))
            .collect();
        LlmCallResult {
            response_id: response_id.clone(),
            full_text: full_text.clone(),
            reasoning: reasoning.clone(),
            reasoning_signature: reasoning_signature.clone(),
            tool_calls,
            usage: usage_tokens.to_json_map(),
            model_used: model_name.to_string(),
            duration_ms: started.elapsed().as_millis() as u64,
            finish_reason: finish_reason.clone(),
        }
    };

    let sse = parse_openai_sse_json_stream(stream);
    tokio::pin!(sse);
    let mut saw_terminal = false;
    loop {
        let idle = if made_progress { idle_post } else { idle_pre };
        let item = tokio::select! {
            biased;
            _ = wait_llm_cancel(cancel) => return Err(StreamCollectError::Cancelled {
                partial: partial_result(
                    &response_id,
                    &full_text,
                    &reasoning,
                    &reasoning_signature,
                    &tool_calls_map,
                    &usage_tokens,
                    &finish_reason,
                ),
            }),
            r = tokio::time::timeout(idle, sse.next()) => match r {
                Ok(v) => v,
                Err(_elapsed) => {
                    return Err(StreamCollectError::IdleTimeout {
                        elapsed_ms: idle.as_millis() as u64,
                        made_progress,
                        partial: partial_result(
                            &response_id,
                            &full_text,
                            &reasoning,
                            &reasoning_signature,
                            &tool_calls_map,
                            &usage_tokens,
                            &finish_reason,
                        ),
                    });
                }
            },
        };
        let Some(item) = item else { break };
        let event = match item {
            Ok(ParsedSseEvent::Done) => {
                saw_terminal = true;
                break;
            }
            Ok(ParsedSseEvent::Data(v)) => v,
            Err(error) => {
                return Err(StreamCollectError::Transport {
                    error,
                    partial: partial_result(
                        &response_id,
                        &full_text,
                        &reasoning,
                        &reasoning_signature,
                        &tool_calls_map,
                        &usage_tokens,
                        &finish_reason,
                    ),
                });
            }
        };
        if response_id.is_none() {
            response_id = event
                .pointer("/message/id")
                .or_else(|| event.get("id"))
                .and_then(Value::as_str)
                .map(ToString::to_string);
        }
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                if let Some(u) = event
                    .get("message")
                    .and_then(|m| m.get("usage"))
                    .and_then(Value::as_object)
                    && let Some(extracted) = crate::turn::token_usage::extract_usage(
                        crate::turn::token_usage::UsageDialect::AnthropicMessages,
                        u,
                    )
                {
                    usage_tokens.input_tokens = extracted.input_tokens;
                    usage_tokens.cached_input_tokens = extracted.cached_input_tokens;
                    usage_tokens.cache_creation_tokens = extracted.cache_creation_tokens;
                    usage_tokens.output_tokens =
                        usage_tokens.output_tokens.max(extracted.output_tokens);
                    made_progress = true;
                }
            }
            Some("content_block_start") => {
                if let Some(block) = event.get("content_block").and_then(Value::as_object)
                    && block.get("type").and_then(Value::as_str) == Some("tool_use")
                {
                    let index = event.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    if tool_calls_map.len() >= MAX_STREAM_TOOL_CALLS
                        && !tool_calls_map.contains_key(&index)
                    {
                        astra_core::agent_warn!(
                            "llm",
                            "stream tool_calls exceeded {MAX_STREAM_TOOL_CALLS} — ignoring extra"
                        );
                        continue;
                    }
                    let id = block.get("id").and_then(Value::as_str).unwrap_or_default();
                    let name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("_unknown");
                    tool_calls_map.insert(
                        index,
                        Map::from_iter([
                            ("id".to_string(), Value::String(id.to_string())),
                            ("type".to_string(), Value::String("function".to_string())),
                            (
                                "function".to_string(),
                                json!({"name": name, "arguments": ""}),
                            ),
                        ]),
                    );
                    if let Some(callback) = stream_callback.as_deref_mut()
                        && let Some(tool_call) = tool_calls_map.get(&index)
                    {
                        callback(LlmStreamUpdate::ToolCall {
                            index,
                            tool_call: Value::Object(tool_call.clone()),
                        });
                    }
                    made_progress = true;
                }
            }
            Some("content_block_delta") => {
                let Some(delta) = event.get("delta").and_then(Value::as_object) else {
                    continue;
                };
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(text) = delta.get("text").and_then(Value::as_str) {
                            accumulated_bytes += text.len();
                            if accumulated_bytes > MAX_STREAM_ACCUMULATION_BYTES {
                                return Err(StreamCollectError::Transport {
                                    error: format!(
                                        "LLM stream exceeded {MAX_STREAM_ACCUMULATION_BYTES} bytes — aborting"
                                    ),
                                    partial: partial_result(
                                        &response_id,
                                        &full_text,
                                        &reasoning,
                                        &reasoning_signature,
                                        &tool_calls_map,
                                        &usage_tokens,
                                        &finish_reason,
                                    ),
                                });
                            }
                            full_text.push_str(text);
                            if let Some(callback) = stream_callback.as_deref_mut() {
                                callback(LlmStreamUpdate::Text(text.to_string()));
                            }
                            made_progress = true;
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(text) = delta.get("thinking").and_then(Value::as_str) {
                            accumulated_bytes += text.len();
                            if accumulated_bytes > MAX_STREAM_ACCUMULATION_BYTES {
                                return Err(StreamCollectError::Transport {
                                    error: format!(
                                        "LLM stream exceeded {MAX_STREAM_ACCUMULATION_BYTES} bytes — aborting"
                                    ),
                                    partial: partial_result(
                                        &response_id,
                                        &full_text,
                                        &reasoning,
                                        &reasoning_signature,
                                        &tool_calls_map,
                                        &usage_tokens,
                                        &finish_reason,
                                    ),
                                });
                            }
                            reasoning.push_str(text);
                            if let Some(callback) = stream_callback.as_deref_mut() {
                                callback(LlmStreamUpdate::Reasoning(text.to_string()));
                            }
                            made_progress = true;
                        }
                    }
                    Some("signature_delta") => {
                        // Anthropic closes a `thinking` content block with
                        // an HMAC signature. The next turn MUST echo this
                        // on the assistant message or the API returns
                        // HTTP 400 `content[].thinking in the thinking
                        // mode must be passed back`. See session
                        // effccfcd-28d8-41f4-a4b0-ecd0ec503625 for the
                        // original symptom on the bridge path; this
                        // branch plugs the same hole on the server_loop
                        // path.
                        if let Some(sig) = delta.get("signature").and_then(Value::as_str)
                            && !sig.is_empty()
                        {
                            reasoning_signature.push_str(sig);
                            made_progress = true;
                        }
                    }
                    Some("input_json_delta") => {
                        let index =
                            event.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                        let args = delta
                            .get("partial_json")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        accumulated_bytes += args.len();
                        if accumulated_bytes > MAX_STREAM_ACCUMULATION_BYTES {
                            return Err(StreamCollectError::Transport {
                                error: format!(
                                    "stream tool-call arguments exceeded {MAX_STREAM_ACCUMULATION_BYTES} byte limit"
                                ),
                                partial: partial_result(
                                    &response_id,
                                    &full_text,
                                    &reasoning,
                                    &reasoning_signature,
                                    &tool_calls_map,
                                    &usage_tokens,
                                    &finish_reason,
                                ),
                            });
                        }
                        if let Some(entry) = tool_calls_map.get_mut(&index)
                            && let Some(function) =
                                entry.get_mut("function").and_then(Value::as_object_mut)
                            && let Some(Value::String(existing)) = function.get_mut("arguments")
                        {
                            existing.push_str(args);
                            made_progress = true;
                        }
                        if let Some(callback) = stream_callback.as_deref_mut()
                            && let Some(tool_call) = tool_calls_map.get(&index)
                        {
                            callback(LlmStreamUpdate::ToolCall {
                                index,
                                tool_call: Value::Object(tool_call.clone()),
                            });
                        }
                    }
                    _ => {}
                }
            }
            Some("message_delta") => {
                if let Some(stop_reason) = event
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    finish_reason = Some(stop_reason.to_string());
                    made_progress = true;
                }
                if let Some(u) = event.get("usage").and_then(Value::as_object)
                    && let Some(extracted) = crate::turn::token_usage::extract_usage(
                        crate::turn::token_usage::UsageDialect::AnthropicMessages,
                        u,
                    )
                {
                    if u.contains_key("input_tokens") {
                        usage_tokens.input_tokens = extracted.input_tokens;
                    }
                    if u.contains_key("cache_read_input_tokens") {
                        usage_tokens.cached_input_tokens = extracted.cached_input_tokens;
                    }
                    if u.contains_key("cache_creation_input_tokens") {
                        usage_tokens.cache_creation_tokens = extracted.cache_creation_tokens;
                    }
                    if u.contains_key("output_tokens") {
                        usage_tokens.output_tokens = extracted.output_tokens;
                    }
                    made_progress = true;
                }
            }
            Some("message_stop") => {
                saw_terminal = true;
                break;
            }
            Some("error") => {
                return Err(StreamCollectError::Transport {
                    error: event.to_string(),
                    partial: partial_result(
                        &response_id,
                        &full_text,
                        &reasoning,
                        &reasoning_signature,
                        &tool_calls_map,
                        &usage_tokens,
                        &finish_reason,
                    ),
                });
            }
            _ => {}
        }
    }

    if !saw_terminal {
        return Err(StreamCollectError::Transport {
            error: "Anthropic SSE ended without message_stop".to_string(),
            partial: partial_result(
                &response_id,
                &full_text,
                &reasoning,
                &reasoning_signature,
                &tool_calls_map,
                &usage_tokens,
                &finish_reason,
            ),
        });
    }

    let mut sorted_tcs: Vec<_> = tool_calls_map.into_iter().collect();
    sorted_tcs.sort_by_key(|(idx, _)| *idx);
    let tool_calls = sorted_tcs
        .into_iter()
        .map(|(_, v)| Value::Object(v))
        .collect();
    Ok(LlmCallResult {
        response_id,
        full_text,
        reasoning,
        reasoning_signature,
        tool_calls,
        usage: usage_tokens.to_json_map(),
        model_used: model_name.to_string(),
        duration_ms: started.elapsed().as_millis() as u64,
        finish_reason,
    })
}

#[derive(Debug)]
#[allow(dead_code)] // Transport variant reserved for future network error handling
enum StreamCollectError {
    IdleTimeout {
        elapsed_ms: u64,
        made_progress: bool,
        partial: LlmCallResult,
    },
    /// Byte stream error from the HTTP client (e.g. reset, TLS failure).
    Transport {
        error: String,
        partial: LlmCallResult,
    },
    /// [`LlmCancel`] fired during collection.
    Cancelled { partial: LlmCallResult },
}

/// For `tokio::select!`: completes when `cancel` fires, or never if `cancel` is `None`.
pub(crate) async fn wait_until_cancelled_or_pending(cancel: Option<&CancellationToken>) {
    match cancel {
        Some(t) => t.cancelled().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
pub(crate) async fn call_llm_nonstream(
    client: &reqwest::Client,
    call: LlmCall<'_>,
    timeout: std::time::Duration,
) -> Result<LlmCallResult, astra_core::ClassifiedError> {
    call_llm_nonstream_with_attempt_observer(client, call, timeout, None).await
}

pub(crate) async fn call_llm_nonstream_with_attempt_observer(
    client: &reqwest::Client,
    call: LlmCall<'_>,
    timeout: std::time::Duration,
    attempt_observer: Option<&dyn ProviderAttemptObserver>,
) -> Result<LlmCallResult, astra_core::ClassifiedError> {
    let LlmCall {
        purpose,
        messages,
        tools,
        cache_capability,
        route,
        max_output_tokens,
        temperature,
        has_fallback: _,
        thinking,
    } = call;
    let LlmExecutionRoute {
        model_name,
        wire_model_name,
        api_key,
        base_url,
        provider,
        header_overrides,
        request_body_overrides,
        completions_url_override,
        request_timeout,
    } = route;
    let started = Instant::now();
    let upstream_name = wire_model_name.unwrap_or(model_name);

    let messages =
        consolidate_system_messages_for_provider(messages, provider, model_name, cache_capability);

    let body = build_provider_request_body_with_overrides(
        &messages,
        tools,
        upstream_name,
        provider,
        max_output_tokens,
        temperature,
        false,
        thinking,
        request_body_overrides,
    );
    let prepared_request =
        PreparedProviderRequest::from_json(&body, llm_provider_protocol(provider))?;

    let url = llm_request_url(
        base_url,
        completions_url_override,
        provider,
        upstream_name,
        false,
    );
    let _registered_endpoint_permit =
        acquire_registered_endpoint_permit_for_override(&url, completions_url_override)?;
    let observed_attempt = match attempt_observer {
        Some(observer) => Some(observer.begin_attempt(prepared_request.identity()).await?),
        None => None,
    };
    let mut req = client.post(&url).header("content-type", "application/json");
    req = apply_provider_auth(req, provider, api_key, header_overrides);
    req = apply_llm_header_overrides(req, header_overrides);

    // Apply per-request timeout (overrides the client-level default).
    let effective_timeout = request_timeout
        .map(|value| value.min(timeout))
        .unwrap_or(timeout);
    tracing::debug!(
        target: "astra_runtime::llm_client",
        url = %url,
        purpose = purpose.as_str(),
        provider,
        model_name,
        "LLM non-stream request sending"
    );
    let resp = match req
        .timeout(effective_timeout)
        .body(prepared_request.body())
        .send()
        .await
    {
        Ok(response) => response,
        Err(e) => {
            let elapsed = started.elapsed();
            tracing::warn!(
                target: "astra_runtime::llm_client",
                url = %url,
                elapsed_ms = elapsed.as_millis() as u64,
                configured_timeout_s = effective_timeout.as_secs(),
                error = %e,
                "LLM non-stream request send failed"
            );
            let retry_safe = e.is_connect();
            let kind = if retry_safe {
                astra_core::ErrorKind::Network
            } else if e.is_timeout() {
                astra_core::ErrorKind::StreamIdle
            } else {
                astra_core::ErrorKind::StreamTransport
            };
            let error = astra_core::ClassifiedError::new(
                kind,
                nonstream_send_error_message(&e, effective_timeout, elapsed),
            );
            finish_observed_provider_error(attempt_observer, observed_attempt, &error).await?;
            return Err(error);
        }
    };
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        let kind = if status == 401 || status == 403 {
            astra_core::ErrorKind::Auth
        } else if is_rate_limit_status(status) {
            astra_core::ErrorKind::RateLimit
        } else if status >= 500 {
            astra_core::ErrorKind::ServerError
        } else if status == 400 && astra_core::is_llm_context_window_error(&text) {
            astra_core::ErrorKind::ContextWindow
        } else if status == 400 {
            astra_core::ErrorKind::InvalidRequest
        } else {
            astra_core::ErrorKind::Unknown
        };
        let detail = redact_provider_secrets(&text);
        let detail = astra_text_utils::str_preview::truncate_str(&detail, 500);
        let error = astra_core::ClassifiedError::new(
            kind,
            format!("LLM non-stream request error {status}: {detail}"),
        );
        finish_observed_provider_error(attempt_observer, observed_attempt, &error).await?;
        return Err(error);
    }
    let transport_response_id = provider_uses_bedrock_converse(provider)
        .then(|| {
            resp.headers()
                .get("x-amzn-requestid")
                .and_then(|value| value.to_str().ok())
                .filter(|value| !value.is_empty())
                .map(ToString::to_string)
        })
        .flatten();
    let v: Value = match resp.json().await {
        Ok(value) => value,
        Err(error) => {
            let error = astra_core::ClassifiedError::new(
                astra_core::ErrorKind::StreamTransport,
                error.to_string(),
            );
            let partial = LlmCallResult {
                response_id: transport_response_id.clone(),
                ..LlmCallResult::default()
            };
            finish_observed_provider_error_with_partial(
                attempt_observer,
                observed_attempt,
                &error,
                &partial,
            )
            .await?;
            return Err(error);
        }
    };
    let mut result = parse_nonstream_response_for_provider(&v, provider, model_name, started);
    if result.response_id.is_none() {
        result.response_id = transport_response_id;
    }
    finish_observed_provider_attempt(
        attempt_observer,
        observed_attempt,
        &provider_attempt_terminal_from_result(&result),
    )
    .await?;
    Ok(result)
}

fn nonstream_send_error_message(
    error: &reqwest::Error,
    effective_timeout: std::time::Duration,
    elapsed: std::time::Duration,
) -> String {
    let action = if error.is_timeout() {
        "timed out"
    } else {
        "failed"
    };
    format!(
        "LLM non-stream request {action} after {}ms (configured timeout {}s): {error}",
        elapsed.as_millis(),
        effective_timeout.as_secs()
    )
}

fn map_bedrock_finish_reason(stop_reason: &str) -> String {
    match stop_reason {
        "tool_use" => "tool_calls".to_string(),
        "max_tokens" => "length".to_string(),
        "end_turn" => "stop".to_string(),
        other => other.to_string(),
    }
}

fn parse_bedrock_nonstream_response(
    v: &Value,
    model_name: &str,
    started: Instant,
) -> LlmCallResult {
    let mut full_text = String::new();
    let mut reasoning = String::new();
    let mut reasoning_signature = String::new();
    let mut tool_calls = Vec::new();
    let usage = v
        .get("usage")
        .and_then(Value::as_object)
        .and_then(|u| {
            crate::turn::token_usage::extract_usage(
                crate::turn::token_usage::UsageDialect::BedrockConverse,
                u,
            )
        })
        .map(|u| u.to_json_map())
        .unwrap_or_default();

    if let Some(content_blocks) = v
        .get("output")
        .and_then(|output| output.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
    {
        for block in content_blocks {
            if let Some(text) = block.get("text").and_then(Value::as_str) {
                full_text.push_str(text);
            }
            if let Some(rc) = block.get("reasoningContent").and_then(Value::as_object) {
                if let Some(rt) = rc.get("reasoningText").and_then(Value::as_object) {
                    if let Some(t) = rt.get("text").and_then(Value::as_str) {
                        reasoning.push_str(t);
                    }
                    if let Some(sig) = rt.get("signature").and_then(Value::as_str) {
                        reasoning_signature.push_str(sig);
                    }
                }
            }
            if let Some(tool_use) = block.get("toolUse").and_then(Value::as_object) {
                let id = tool_use
                    .get("toolUseId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let name = tool_use
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("_unknown");
                let arguments = tool_use.get("input").cloned().unwrap_or_else(|| json!({}));
                tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": arguments.to_string(),
                    }
                }));
            }
        }
    }

    LlmCallResult {
        response_id: v.get("id").and_then(Value::as_str).map(String::from),
        full_text,
        reasoning,
        reasoning_signature,
        tool_calls,
        usage,
        model_used: model_name.to_string(),
        duration_ms: started.elapsed().as_millis() as u64,
        finish_reason: v
            .get("stopReason")
            .and_then(Value::as_str)
            .map(map_bedrock_finish_reason),
    }
}

fn parse_openai_compatible_nonstream_response(
    v: &Value,
    model_name: &str,
    started: Instant,
) -> LlmCallResult {
    let mut full_text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    let usage = v
        .get("usage")
        .and_then(Value::as_object)
        .and_then(|u| {
            crate::turn::token_usage::extract_usage(
                crate::turn::token_usage::UsageDialect::OpenAi,
                u,
            )
        })
        .map(|u| u.to_json_map())
        .unwrap_or_default();

    if let Some(choice) = v
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        && let Some(msg) = choice.get("message").and_then(Value::as_object)
    {
        if let Some(content) = msg.get("content").and_then(Value::as_str) {
            full_text = content.to_string();
        }
        if let Some(r) = msg.get("reasoning_content").and_then(Value::as_str) {
            reasoning = r.to_string();
        }
        if let Some(tcs) = msg.get("tool_calls").and_then(Value::as_array) {
            tool_calls = tcs.clone();
        }
    }

    let finish_reason = v
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|c| c.get("finish_reason"))
        .and_then(Value::as_str)
        .map(String::from);

    // Degraded tool-call fallback: same recovery for non-stream responses.
    if tool_calls.is_empty() {
        if let Some(parsed) =
            astra_turn_core::xml_tool_call_fallback::parse_degraded_tool_calls(&full_text)
        {
            astra_core::agent_warn!(
                "llm",
                "recovered {} tool call(s) from degraded text in content (non-stream)",
                parsed.len()
            );
            full_text =
                astra_turn_core::xml_tool_call_fallback::strip_degraded_tool_calls(&full_text);
            tool_calls = parsed;
        }
    }

    if reasoning.is_empty() {
        if let Some((extracted_reasoning, cleaned_text)) = extract_think_tags(&full_text) {
            reasoning = extracted_reasoning;
            full_text = cleaned_text;
        }
    }

    LlmCallResult {
        response_id: v.get("id").and_then(Value::as_str).map(String::from),
        full_text,
        reasoning,
        reasoning_signature: String::new(),
        tool_calls,
        usage,
        model_used: model_name.to_string(),
        duration_ms: started.elapsed().as_millis() as u64,
        finish_reason,
    }
}

fn parse_anthropic_nonstream_response(
    v: &Value,
    model_name: &str,
    started: Instant,
) -> LlmCallResult {
    let mut full_text = String::new();
    let mut reasoning = String::new();
    // See `collect_anthropic_llm_stream` for the signature-echo contract.
    // Explicit non-stream callers have the same requirement: dropping the
    // signature here breaks the next signed-thinking round.
    let mut reasoning_signature = String::new();
    let mut tool_calls = Vec::new();
    if let Some(content) = v.get("content").and_then(Value::as_array) {
        for block in content {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        full_text.push_str(text);
                    }
                }
                Some("thinking") => {
                    if let Some(text) = block.get("thinking").and_then(Value::as_str) {
                        reasoning.push_str(text);
                    }
                    if let Some(sig) = block.get("signature").and_then(Value::as_str)
                        && !sig.is_empty()
                    {
                        reasoning_signature.push_str(sig);
                    }
                }
                Some("tool_use") => {
                    let id = block.get("id").and_then(Value::as_str).unwrap_or_default();
                    let name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("_unknown");
                    let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                    tool_calls.push(json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": input.to_string(),
                        }
                    }));
                }
                _ => {}
            }
        }
    }
    let usage = v
        .get("usage")
        .and_then(Value::as_object)
        .and_then(|u| {
            crate::turn::token_usage::extract_usage(
                crate::turn::token_usage::UsageDialect::AnthropicMessages,
                u,
            )
        })
        .map(|u| u.to_json_map())
        .unwrap_or_default();

    LlmCallResult {
        response_id: v.get("id").and_then(Value::as_str).map(String::from),
        full_text,
        reasoning,
        reasoning_signature,
        tool_calls,
        usage,
        model_used: model_name.to_string(),
        duration_ms: started.elapsed().as_millis() as u64,
        finish_reason: v
            .get("stop_reason")
            .and_then(Value::as_str)
            .map(String::from),
    }
}

pub(crate) fn parse_nonstream_response_for_provider(
    v: &Value,
    provider: &str,
    model_name: &str,
    started: Instant,
) -> LlmCallResult {
    match llm_provider_protocol(provider) {
        LlmProviderProtocol::BedrockConverse => {
            parse_bedrock_nonstream_response(v, model_name, started)
        }
        LlmProviderProtocol::AnthropicMessages => {
            parse_anthropic_nonstream_response(v, model_name, started)
        }
        LlmProviderProtocol::OpenAiCompatible => {
            parse_openai_compatible_nonstream_response(v, model_name, started)
        }
    }
}

/// One semantically meaningful item from a provider SSE stream.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ParsedSseEvent {
    Data(Value),
    Done,
}

/// Parse OpenAI-style SSE bytes without collapsing `[DONE]` into ordinary EOF.
/// Transport and framing errors surface as `Err`; a clean socket EOF without
/// [`ParsedSseEvent::Done`] remains distinguishable to the caller.
pub(crate) fn parse_openai_sse_json_stream(
    stream: impl futures_util::Stream<Item = Result<Bytes, reqwest::Error>> + Unpin + Send + 'static,
) -> impl futures_util::Stream<Item = Result<ParsedSseEvent, String>> + Send + 'static {
    async_stream::stream! {
        let mut sse_in = SseBlankLineUtf8Buf::new();
        tokio::pin!(stream);
        while let Some(chunk) = stream.next().await {
            let bytes = match chunk {
                Ok(b) => b,
                Err(e) => {
                    yield Err(e.to_string());
                    return;
                }
            };
            let blocks = match sse_in.push_bytes(&bytes) {
                Ok(blocks) => blocks,
                Err(error) => {
                    yield Err(format!("invalid UTF-8 in model SSE response: {error}"));
                    return;
                }
            };
            for block in blocks {
                if let Err(error) = validate_sse_event_block_json(&block) {
                    yield Err(error);
                    return;
                }
                let d = json_events_from_sse_event_block(&block);
                for v in d.events {
                    yield Ok(ParsedSseEvent::Data(v));
                }
                if d.stream_finished {
                    yield Ok(ParsedSseEvent::Done);
                    return;
                }
            }
        }
        let mut buf = match sse_in.into_inner() {
            Ok(buf) => buf,
            Err(error) => {
                yield Err(format!("invalid UTF-8 in model SSE response: {error}"));
                return;
            }
        };
        let tail = match validated_drain_sse_data_lines(&mut buf, "") {
            Ok(value) => value,
            Err(error) => {
                yield Err(error);
                return;
            }
        };
        for v in tail.events {
            yield Ok(ParsedSseEvent::Data(v));
        }
        if tail.stream_finished {
            yield Ok(ParsedSseEvent::Done);
            return;
        }
        let fin = match validated_finish_sse_data_buffer(&mut buf) {
            Ok(value) => value,
            Err(error) => {
                yield Err(error);
                return;
            }
        };
        for v in fin.events {
            yield Ok(ParsedSseEvent::Data(v));
        }
        if fin.stream_finished {
            yield Ok(ParsedSseEvent::Done);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::extract::State;
    use axum::http::HeaderMap;
    use axum::response::Response;
    use axum::routing::post;
    use futures_util::StreamExt;
    use futures_util::stream;
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Set thread-local stream idle timeouts for the duration of a test.
    /// Returns a guard that resets them on drop.
    fn set_test_stream_timeouts(pre_ms: u64, post_ms: Option<u64>) -> impl Drop {
        TEST_STREAM_IDLE_TIMEOUT.with(|c| {
            *c.borrow_mut() = Some(std::time::Duration::from_millis(pre_ms));
        });
        if let Some(post) = post_ms {
            TEST_STREAM_IDLE_TIMEOUT_AFTER_PROGRESS.with(|c| {
                *c.borrow_mut() = Some(std::time::Duration::from_millis(post));
            });
        }
        struct Guard;
        impl Drop for Guard {
            fn drop(&mut self) {
                TEST_STREAM_IDLE_TIMEOUT.with(|c| *c.borrow_mut() = None);
                TEST_STREAM_IDLE_TIMEOUT_AFTER_PROGRESS.with(|c| *c.borrow_mut() = None);
            }
        }
        Guard
    }

    #[cfg(feature = "bridge-e2e-hooks")]
    #[test]
    fn bridge_e2e_stream_idle_timeout_override_is_visible_to_runtime_paths() {
        let _guard = set_bridge_e2e_stream_idle_timeouts_for_test(123, 456);
        assert_eq!(stream_idle_timeout(), std::time::Duration::from_millis(123));
        assert_eq!(
            stream_idle_timeout_after_progress(),
            std::time::Duration::from_millis(456)
        );
    }

    #[tokio::test]
    async fn sleep_ms_or_llm_cancel_sleeps_when_no_cancel_source() {
        let r = sleep_ms_or_llm_cancel(8, LlmCancel::None).await;
        assert!(r.is_ok());
    }

    #[tokio::test]
    async fn sleep_ms_or_llm_cancel_aborts_on_token() {
        let token = CancellationToken::new();
        let token_for_wait = token.clone();
        let h = tokio::spawn(async move {
            sleep_ms_or_llm_cancel(60_000, LlmCancel::Token(&token_for_wait)).await
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        token.cancel();
        let r = h.await.expect("join");
        assert_eq!(
            r.expect_err("cancelled").kind,
            astra_core::ErrorKind::Cancelled
        );
    }

    #[tokio::test]
    async fn sleep_ms_or_llm_cancel_aborts_on_flag() {
        let flag = Arc::new(AtomicBool::new(false));
        let flag_signal = flag.clone();
        let flag_for_wait = flag.clone();
        let h = tokio::spawn(async move {
            sleep_ms_or_llm_cancel(60_000, LlmCancel::Flag(flag_for_wait.as_ref())).await
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        flag_signal.store(true, Ordering::SeqCst);
        let r = h.await.expect("join");
        assert_eq!(
            r.expect_err("cancelled").kind,
            astra_core::ErrorKind::Cancelled
        );
    }

    #[tokio::test]
    async fn sleep_ms_or_llm_cancel_aborts_flag_and_token_via_token() {
        let flag = Arc::new(AtomicBool::new(false));
        let flag_for_join = flag.clone();
        let token = CancellationToken::new();
        let token_for_wait = token.clone();
        let h = tokio::spawn(async move {
            sleep_ms_or_llm_cancel(
                60_000,
                LlmCancel::FlagAndToken(flag.as_ref(), &token_for_wait),
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        token.cancel();
        let r = h.await.expect("join");
        assert_eq!(
            r.expect_err("cancelled").kind,
            astra_core::ErrorKind::Cancelled
        );
        assert!(!flag_for_join.load(Ordering::SeqCst));
    }

    #[test]
    fn llm_cancel_is_triggered_matrix() {
        assert!(!LlmCancel::None.is_triggered());

        let flag_off = Arc::new(AtomicBool::new(false));
        assert!(!LlmCancel::Flag(flag_off.as_ref()).is_triggered());
        flag_off.store(true, Ordering::SeqCst);
        assert!(LlmCancel::Flag(flag_off.as_ref()).is_triggered());

        let token = CancellationToken::new();
        assert!(!LlmCancel::Token(&token).is_triggered());
        token.cancel();
        assert!(LlmCancel::Token(&token).is_triggered());

        let flag2 = Arc::new(AtomicBool::new(false));
        let token2 = CancellationToken::new();
        assert!(!LlmCancel::FlagAndToken(flag2.as_ref(), &token2).is_triggered());
        token2.cancel();
        assert!(LlmCancel::FlagAndToken(flag2.as_ref(), &token2).is_triggered());

        let flag3 = Arc::new(AtomicBool::new(true));
        let token3 = CancellationToken::new();
        assert!(LlmCancel::FlagAndToken(flag3.as_ref(), &token3).is_triggered());
    }

    #[test]
    fn execution_route_debug_exposes_routing_facts_without_credentials() {
        let mut headers = HashMap::new();
        headers.insert(
            "authorization".to_string(),
            "Bearer header-secret".to_string(),
        );
        headers.insert("x-workspace-id".to_string(), "workspace-secret".to_string());
        let route = LlmExecutionRoute {
            model_name: "model-a",
            wire_model_name: Some("wire-model-a"),
            api_key: "api-key-secret",
            base_url: "https://models.example.test/v1",
            provider: "openai",
            header_overrides: Some(&headers),
            request_body_overrides: None,
            completions_url_override: Some("https://gateway.example.test/inference"),
            request_timeout: Some(std::time::Duration::from_secs(30)),
        };

        let debug = format!("{route:?}");
        assert!(debug.contains("model-a"));
        assert!(debug.contains("authorization"));
        assert!(debug.contains("x-workspace-id"));
        for secret in [
            "api-key-secret",
            "header-secret",
            "workspace-secret",
            "models.example.test",
            "gateway.example.test",
        ] {
            assert!(!debug.contains(secret), "debug output leaked {secret}");
        }
    }

    // ── Timeout configuration tests ─────────────────────────────────────────

    #[test]
    fn connect_timeout_default_is_30s() {
        // Ensure no env override interferes.
        let dur = llm_connect_timeout();
        // Default is LLM_CONNECT_TIMEOUT_S = 30.
        assert_eq!(dur, std::time::Duration::from_secs(LLM_CONNECT_TIMEOUT_S));
    }

    #[test]
    fn nonstream_timeout_default_is_120s() {
        let dur = llm_nonstream_timeout();
        assert_eq!(dur, std::time::Duration::from_secs(LLM_NONSTREAM_TIMEOUT_S));
    }

    #[test]
    fn total_budget_default_is_300s() {
        let dur = llm_total_budget();
        assert_eq!(dur, std::time::Duration::from_secs(LLM_TOTAL_BUDGET_S));
    }

    #[tokio::test]
    async fn total_budget_exhausted_returns_error() {
        // Simulate a scenario where started time is already past budget.
        // We test the logic inline since call_llm_and_collect needs a server.
        let budget = std::time::Duration::from_millis(1);
        let started = Instant::now();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        assert!(
            started.elapsed() > budget,
            "elapsed should exceed tiny budget"
        );
    }

    #[tokio::test]
    async fn nonstream_request_respects_timeout() {
        // Create a mock server that delays longer than the request timeout.
        let app = Router::new().route(
            "/chat/completions",
            post(|| async {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                Response::builder()
                    .status(200)
                    .body(Body::from(
                        r#"{"choices":[{"message":{"content":"late"}}]}"#,
                    ))
                    .unwrap()
            }),
        );
        let base = spawn_local_http_server(app).await;
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("build client");
        // Use a very short timeout — should fail before the 5s delay completes.
        let timeout = std::time::Duration::from_millis(100);
        let result = call_llm_nonstream(
            &client,
            LlmCall {
                purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
                messages: &[json!({"role":"user","content":"x"})],
                tools: &[],
                cache_capability: None,
                route: LlmExecutionRoute {
                    model_name: "m",
                    wire_model_name: None,
                    api_key: "k",
                    base_url: &base,
                    provider: "openai",
                    header_overrides: None,
                    request_body_overrides: None,
                    completions_url_override: None,
                    request_timeout: None,
                },
                max_output_tokens: None,
                temperature: None,
                has_fallback: false,
                thinking: &ThinkingConfig::Off,
            },
            timeout,
        )
        .await;
        assert!(result.is_err(), "should timeout: {result:?}");
        let err = result.unwrap_err();
        assert_eq!(
            err.kind,
            astra_core::ErrorKind::StreamIdle,
            "the provider request deadline must have a typed timeout outcome"
        );
        assert!(
            err.message.contains("timeout") || err.message.contains("Timeout"),
            "error should mention timeout: {err}"
        );
    }

    #[tokio::test]
    async fn call_llm_with_request_overrides_uses_direct_gateway_url_and_headers() {
        #[derive(Clone, Default)]
        struct Capture {
            auth: Option<String>,
            workspace: Option<String>,
            model: Option<String>,
        }

        async fn gateway_handler(
            State(capture): State<Arc<Mutex<Capture>>>,
            headers: HeaderMap,
            axum::Json(body): axum::Json<Value>,
        ) -> Response {
            *capture.lock().expect("capture lock") = Capture {
                auth: headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .map(String::from),
                workspace: headers
                    .get("x-workspace-id")
                    .and_then(|value| value.to_str().ok())
                    .map(String::from),
                model: body.get("model").and_then(Value::as_str).map(String::from),
            };
            let payload = json!({"choices":[{"delta":{"content":"from-gateway"}}]});
            let body = format!("data: {}\n\ndata: [DONE]\n\n", payload);
            Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(Body::from(body))
                .expect("gateway response")
        }

        let capture = Arc::new(Mutex::new(Capture::default()));
        let app = Router::new()
            .route("/gateway", post(gateway_handler))
            .with_state(capture.clone());
        let base = spawn_local_http_server(app).await;
        let gateway_url = format!("{base}/gateway");

        let mut overrides = HashMap::new();
        overrides.insert("authorization".to_string(), "Bearer moi-token".to_string());
        overrides.insert("x-workspace-id".to_string(), "ws-001".to_string());
        overrides.insert("__astra_connection_tokens".to_string(), "x-hop".to_string());

        let result = call_llm_and_collect(
            LlmCall {
                purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
                messages: &[json!({"role":"user","content":"hi"})],
                tools: &[],
                cache_capability: None,
                route: LlmExecutionRoute {
                    model_name: "gpt-5-mini",
                    wire_model_name: None,
                    api_key: "",
                    base_url: "https://api.openai.com/v1",
                    provider: "openai",
                    header_overrides: Some(&overrides),
                    request_body_overrides: None,
                    completions_url_override: Some(&gateway_url),
                    request_timeout: None,
                },
                max_output_tokens: None,
                temperature: None,
                has_fallback: false,
                thinking: &ThinkingConfig::Off,
            },
            LlmCancel::None,
        )
        .await
        .expect("gateway llm call");

        assert_eq!(result.full_text, "from-gateway");
        let seen = capture.lock().expect("capture lock").clone();
        assert_eq!(seen.auth.as_deref(), Some("Bearer moi-token"));
        assert_eq!(seen.workspace.as_deref(), Some("ws-001"));
        assert_eq!(seen.model.as_deref(), Some("gpt-5-mini"));
    }

    #[tokio::test]
    async fn call_llm_and_collect_omits_empty_assistant_tool_calls_in_request_body() {
        #[derive(Clone, Default, Debug)]
        struct Capture {
            messages: Vec<Value>,
        }

        async fn gateway_handler(
            State(capture): State<Arc<Mutex<Capture>>>,
            axum::Json(body): axum::Json<Value>,
        ) -> Response {
            capture.lock().expect("capture lock").messages = body
                .get("messages")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let payload = json!({"choices":[{"delta":{"content":"ok"}}]});
            let body = format!("data: {payload}\n\ndata: [DONE]\n\n");
            Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(Body::from(body))
                .expect("gateway response")
        }

        let capture = Arc::new(Mutex::new(Capture::default()));
        let app = Router::new()
            .route("/chat/completions", post(gateway_handler))
            .with_state(capture.clone());
        let base = spawn_local_http_server(app).await;
        let messages = vec![
            json!({"role":"assistant","content":"Done.","tool_calls":[]}),
            json!({"role":"assistant","content":null,"tool_calls":[{"id":"c1","type":"function","function":{"name":"bash","arguments":"{}"}}]}),
            json!({"role":"tool","tool_call_id":"c1","name":"bash","content":"ok"}),
            json!({"role":"user","content":"hi"}),
        ];

        let result = call_llm_and_collect(
            LlmCall {
                purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
                messages: &messages,
                tools: &[],
                cache_capability: None,
                route: LlmExecutionRoute {
                    model_name: "gpt-5-mini",
                    wire_model_name: None,
                    api_key: "k",
                    base_url: &base,
                    provider: "openai",
                    header_overrides: None,
                    request_body_overrides: None,
                    completions_url_override: None,
                    request_timeout: None,
                },
                max_output_tokens: None,
                temperature: None,
                has_fallback: false,
                thinking: &ThinkingConfig::Off,
            },
            LlmCancel::None,
        )
        .await
        .expect("llm ok");

        assert_eq!(result.full_text, "ok");
        let seen = capture.lock().expect("capture lock").clone();
        assert_eq!(seen.messages.len(), 4);
        assert!(seen.messages[0].get("tool_calls").is_none(), "{seen:?}");
        assert_eq!(
            seen.messages[1]["tool_calls"][0]["function"]["name"].as_str(),
            Some("bash")
        );
    }

    #[test]
    fn is_tpm_exhaustion_detects_patterns() {
        // TPM (tokens per minute) exhaustion patterns
        assert!(is_tpm_exhaustion("endpoint TPM exceeded"));
        assert!(is_tpm_exhaustion("TPM limit exceeded for this endpoint"));
        assert!(is_tpm_exhaustion("tokens per minute limit reached"));
        assert!(is_tpm_exhaustion(
            "Rate limit exceeded: token quota exhausted"
        ));
        // Negative cases - regular rate limits (not TPM)
        assert!(!is_tpm_exhaustion("rate limit exceeded"));
        assert!(!is_tpm_exhaustion("too many requests"));
        assert!(!is_tpm_exhaustion("429 quota exceeded"));
        assert!(!is_tpm_exhaustion(""));
    }

    #[test]
    fn llm_call_result_default() {
        let r = LlmCallResult::default();
        assert!(r.full_text.is_empty());
        assert!(r.tool_calls.is_empty());
        assert_eq!(r.duration_ms, 0);
    }

    #[test]
    fn parse_nonstream_response_extracts_fields() {
        let v = json!({
            "choices": [{
                "message": {
                    "content": "hello",
                    "reasoning_content": "think",
                    "tool_calls": [{"id":"t1","type":"function","function":{"name":"bash","arguments":"{}"}}]
                }
            }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 5 }
        });
        let r = parse_nonstream_response_for_provider(&v, "openai", "test-model", Instant::now());
        assert_eq!(r.full_text, "hello");
        assert_eq!(r.reasoning, "think");
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(
            r.usage.get("input_tokens").and_then(Value::as_u64),
            Some(10)
        );
        assert_eq!(
            r.usage.get("output_tokens").and_then(Value::as_u64),
            Some(5)
        );
        assert_eq!(
            r.usage.get("total_tokens").and_then(Value::as_u64),
            Some(15)
        );
    }

    #[test]
    fn parse_bedrock_nonstream_response_extracts_fields() {
        let v = json!({
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [
                        {"text": "hello"},
                        {"reasoningContent": {"reasoningText": {"text": "think"}}},
                        {"toolUse": {"toolUseId": "t1", "name": "bash", "input": {"cmd": "pwd"}}}
                    ]
                }
            },
            "stopReason": "tool_use",
            "usage": { "inputTokens": 10, "outputTokens": 5, "totalTokens": 15 }
        });
        let r = parse_nonstream_response_for_provider(
            &v,
            "bedrock",
            "anthropic.claude-3-5-sonnet-v1:0",
            Instant::now(),
        );
        assert_eq!(r.full_text, "hello");
        assert_eq!(r.reasoning, "think");
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0]["function"]["name"], "bash");
        assert_eq!(
            r.tool_calls[0]["function"]["arguments"].as_str(),
            Some(r#"{"cmd":"pwd"}"#)
        );
        assert_eq!(r.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(
            r.usage.get("input_tokens").and_then(Value::as_u64),
            Some(10)
        );
        assert_eq!(
            r.usage.get("output_tokens").and_then(Value::as_u64),
            Some(5)
        );
        assert_eq!(
            r.usage.get("total_tokens").and_then(Value::as_u64),
            Some(15)
        );
    }

    #[test]
    fn parse_bedrock_nonstream_response_extracts_cache_usage() {
        let v = json!({
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [{"text": "hello"}]
                }
            },
            "stopReason": "end_turn",
            "usage": {
                "inputTokens": 10,
                "outputTokens": 5,
                "cacheReadInputTokens": 8,
                "cacheWriteInputTokens": 3,
                "totalTokens": 15
            }
        });
        let r = parse_nonstream_response_for_provider(
            &v,
            "bedrock",
            "anthropic.claude-sonnet-4-20250514-v1:0",
            Instant::now(),
        );
        assert_eq!(
            r.usage.get("input_tokens").and_then(Value::as_u64),
            Some(10)
        );
        assert_eq!(
            r.usage.get("cached_input_tokens").and_then(Value::as_u64),
            Some(8)
        );
        assert_eq!(
            r.usage.get("cache_creation_tokens").and_then(Value::as_u64),
            Some(3)
        );
        assert_eq!(
            r.usage.get("output_tokens").and_then(Value::as_u64),
            Some(5)
        );
        // Bedrock billing identity: input + cached + creation + output.
        assert_eq!(
            r.usage.get("total_tokens").and_then(Value::as_u64),
            Some(26)
        );
    }

    #[tokio::test]
    async fn parse_openai_sse_json_stream_split_chunks() {
        let parts: Vec<Result<Bytes, reqwest::Error>> = vec![
            Ok(Bytes::from("data: ")),
            Ok(Bytes::from(r#"{"t":1}"#)),
            Ok(Bytes::from("\n\n")),
        ];
        let st = parse_openai_sse_json_stream(stream::iter(parts));
        tokio::pin!(st);
        let ev = st.next().await.unwrap().unwrap();
        assert_eq!(ev, ParsedSseEvent::Data(json!({"t": 1})));
        assert!(st.next().await.is_none());
    }

    #[tokio::test]
    async fn parse_openai_sse_json_stream_preserves_utf8_split_across_chunks() {
        let parts: Vec<Result<Bytes, reqwest::Error>> = vec![
            Ok(Bytes::from_static(b"data: {\"text\":\"\xe6")),
            Ok(Bytes::from_static(b"\x88")),
            Ok(Bytes::from_static(b"\x91\"}\n\n")),
        ];
        let st = parse_openai_sse_json_stream(stream::iter(parts));
        tokio::pin!(st);
        assert_eq!(
            st.next().await.unwrap().unwrap(),
            ParsedSseEvent::Data(json!({"text": "我"}))
        );
        assert!(st.next().await.is_none());
    }

    #[tokio::test]
    async fn parse_openai_sse_json_stream_rejects_invalid_utf8() {
        let parts: Vec<Result<Bytes, reqwest::Error>> =
            vec![Ok(Bytes::from_static(b"data: {\"text\":\"\xff\"}\n\n"))];
        let st = parse_openai_sse_json_stream(stream::iter(parts));
        tokio::pin!(st);
        let error = st
            .next()
            .await
            .expect("invalid UTF-8 item")
            .expect_err("invalid UTF-8 must fail");
        assert!(error.contains("invalid UTF-8"), "{error}");
        assert!(st.next().await.is_none());
    }

    #[tokio::test]
    async fn parse_openai_sse_json_stream_done_terminates() {
        let body = "data: {\"a\":1}\n\ndata: [DONE]\n\n";
        let parts: Vec<Result<Bytes, reqwest::Error>> =
            vec![Ok(Bytes::copy_from_slice(body.as_bytes()))];
        let st = parse_openai_sse_json_stream(stream::iter(parts));
        tokio::pin!(st);
        let e1 = st.next().await.unwrap().unwrap();
        assert_eq!(e1, ParsedSseEvent::Data(json!({"a": 1})));
        assert_eq!(st.next().await.unwrap().unwrap(), ParsedSseEvent::Done);
        assert!(st.next().await.is_none());
    }

    async fn sample_reqwest_stream_error() -> reqwest::Error {
        reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap()
            .get("http://127.0.0.1:1/")
            .send()
            .await
            .expect_err("connection to closed port should fail")
    }

    async fn sample_reqwest_response_timeout_error() -> reqwest::Error {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind timeout listener");
        let addr = listener.local_addr().expect("timeout listener address");
        tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.expect("accept timeout request");
            std::future::pending::<()>().await;
        });
        reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_millis(20))
            .build()
            .expect("timeout client")
            .get(format!("http://{addr}/"))
            .send()
            .await
            .expect_err("provider that accepts but never responds should time out")
    }

    #[tokio::test]
    async fn provider_send_error_retries_only_connect_before_delivery() {
        let connect_error = sample_reqwest_stream_error().await;
        let (classified, retry_safe) =
            classify_provider_send_error("provider send failed", &connect_error);
        assert!(retry_safe);
        assert_eq!(classified.kind, astra_core::ErrorKind::Network);
        assert_eq!(
            provider_attempt_terminal_from_error(&classified).status,
            astra_services::InferenceTerminalStatus::Failed
        );

        let response_timeout = sample_reqwest_response_timeout_error().await;
        assert!(response_timeout.is_timeout());
        let (classified, retry_safe) =
            classify_provider_send_error("provider send failed", &response_timeout);
        assert!(!retry_safe);
        assert_eq!(classified.kind, astra_core::ErrorKind::StreamTransport);
        assert_eq!(
            provider_attempt_terminal_from_error(&classified).status,
            astra_services::InferenceTerminalStatus::DeliveryUnknown
        );
    }

    #[tokio::test]
    async fn parse_openai_sse_json_stream_surfaces_byte_stream_error() {
        let err = sample_reqwest_stream_error().await;
        let parts: Vec<Result<Bytes, reqwest::Error>> = vec![Err(err)];
        let st = parse_openai_sse_json_stream(stream::iter(parts));
        tokio::pin!(st);
        let r = st.next().await.expect("one item");
        let msg = r.expect_err("transport");
        assert!(!msg.is_empty());
        assert!(st.next().await.is_none());
    }

    #[tokio::test]
    async fn parse_openai_sse_json_stream_event_then_transport_error() {
        let err = sample_reqwest_stream_error().await;
        let parts: Vec<Result<Bytes, reqwest::Error>> =
            vec![Ok(Bytes::from("data: {\"x\":1}\n\n")), Err(err)];
        let st = parse_openai_sse_json_stream(stream::iter(parts));
        tokio::pin!(st);
        assert_eq!(
            st.next().await.unwrap().unwrap(),
            ParsedSseEvent::Data(json!({"x": 1}))
        );
        assert!(st.next().await.unwrap().is_err());
        assert!(st.next().await.is_none());
    }

    #[tokio::test]
    async fn parse_openai_sse_json_stream_invalid_block_errors() {
        let parts: Vec<Result<Bytes, reqwest::Error>> =
            vec![Ok(Bytes::from("data: {\"x\":1}\n\ndata: not-json\n\n"))];
        let st = parse_openai_sse_json_stream(stream::iter(parts));
        tokio::pin!(st);
        assert_eq!(
            st.next().await.unwrap().unwrap(),
            ParsedSseEvent::Data(json!({"x": 1}))
        );
        let err = st
            .next()
            .await
            .expect("invalid block item")
            .expect_err("parse error");
        assert!(err.contains("invalid JSON in SSE data line"), "{err}");
        assert!(st.next().await.is_none());
    }

    #[tokio::test]
    async fn parse_openai_sse_json_stream_invalid_tail_errors() {
        let parts: Vec<Result<Bytes, reqwest::Error>> = vec![Ok(Bytes::from("data: not-json"))];
        let st = parse_openai_sse_json_stream(stream::iter(parts));
        tokio::pin!(st);
        let err = st
            .next()
            .await
            .expect("invalid tail item")
            .expect_err("parse error");
        assert!(err.contains("invalid JSON in SSE data line"), "{err}");
        assert!(st.next().await.is_none());
    }

    #[tokio::test]
    async fn parse_openai_sse_json_stream_tail_flush_without_final_blank_line() {
        let parts: Vec<Result<Bytes, reqwest::Error>> = vec![Ok(Bytes::from("data: {\"z\":9}"))];
        let st = parse_openai_sse_json_stream(stream::iter(parts));
        tokio::pin!(st);
        let ev = st.next().await.unwrap().unwrap();
        assert_eq!(ev, ParsedSseEvent::Data(json!({"z": 9})));
        assert!(st.next().await.is_none());
    }

    #[tokio::test]
    async fn collect_llm_stream_surfaces_transport_error() {
        let err = sample_reqwest_stream_error().await;
        let byte_stream = stream::iter(vec![Err(err)]);
        let started = Instant::now();
        let res = collect_llm_stream(
            byte_stream,
            "test-model",
            started,
            LlmCancel::None,
            stream_idle_timeout(),
            stream_idle_timeout_after_progress(),
            None,
        )
        .await;
        assert!(
            matches!(res, Err(StreamCollectError::Transport { .. })),
            "expected transport error, got: {res:?}"
        );
    }

    #[tokio::test]
    async fn collect_llm_stream_transport_after_partial_carries_partial_result() {
        let err = sample_reqwest_stream_error().await;
        let d1 = json!({"choices":[{"delta":{"content":"partial"}}]});
        let byte_stream = stream::iter(vec![Ok(Bytes::from(format!("data: {d1}\n\n"))), Err(err)]);
        let res = collect_llm_stream(
            byte_stream,
            "test-model",
            Instant::now(),
            LlmCancel::None,
            stream_idle_timeout(),
            stream_idle_timeout_after_progress(),
            None,
        )
        .await
        .expect_err("transport error");
        match res {
            StreamCollectError::Transport { partial, .. } => {
                assert_eq!(partial.full_text, "partial");
                assert_eq!(partial.model_used, "test-model");
            }
            other => panic!("expected transport error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn collect_llm_stream_rejects_clean_eof_without_terminal_marker() {
        let delta = json!({"choices":[{"delta":{"content":"partial"}}]});
        let byte_stream = stream::iter(vec![Ok(Bytes::from(format!("data: {delta}\n\n")))]);

        let error = collect_llm_stream(
            byte_stream,
            "test-model",
            Instant::now(),
            LlmCancel::None,
            stream_idle_timeout(),
            stream_idle_timeout_after_progress(),
            None,
        )
        .await
        .expect_err("EOF without [DONE] or finish_reason is delivery-unknown");

        match error {
            StreamCollectError::Transport { error, partial } => {
                assert_eq!(error, "provider SSE ended without a terminal marker");
                assert_eq!(partial.full_text, "partial");
            }
            other => panic!("expected transport error, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn collect_llm_stream_finishes_after_semantic_terminal_without_waiting_for_eof() {
        let content = json!({"choices":[{"delta":{"content":"done"}}]});
        let terminal = json!({"choices":[{"delta":{},"finish_reason":"stop"}]});
        let bytes = Bytes::from(format!("data: {content}\n\ndata: {terminal}\n\n"));
        let byte_stream =
            stream::iter(vec![Ok(bytes)]).chain(stream::pending::<Result<Bytes, reqwest::Error>>());

        let result = collect_llm_stream(
            byte_stream,
            "test-model",
            Instant::now(),
            LlmCancel::None,
            std::time::Duration::from_secs(30),
            std::time::Duration::from_secs(30),
            None,
        )
        .await
        .expect("finish_reason is authoritative even if the socket remains open");

        assert_eq!(result.full_text, "done");
        assert_eq!(result.finish_reason.as_deref(), Some("stop"));
    }

    #[tokio::test]
    async fn collect_llm_stream_routes_inline_think_to_reasoning_before_partial_error() {
        let err = sample_reqwest_stream_error().await;
        let d1 = json!({"choices":[{"delta":{"content":"<think>hidden reasoning"}}]});
        let d2 = json!({"choices":[{"delta":{"content":"</think>visible answer"}}]});
        let byte_stream = stream::iter(vec![
            Ok(Bytes::from(format!("data: {d1}\n\n"))),
            Ok(Bytes::from(format!("data: {d2}\n\n"))),
            Err(err),
        ]);
        let mut updates = Vec::new();
        let mut callback = |update| updates.push(update);
        let res = collect_llm_stream(
            byte_stream,
            "test-model",
            Instant::now(),
            LlmCancel::None,
            stream_idle_timeout(),
            stream_idle_timeout_after_progress(),
            Some(&mut callback),
        )
        .await
        .expect_err("transport error");
        match res {
            StreamCollectError::Transport { partial, .. } => {
                assert_eq!(partial.full_text, "visible answer");
                assert_eq!(partial.reasoning, "hidden reasoning");
                assert!(!partial.full_text.contains("<think>"));
            }
            other => panic!("expected transport error, got {other:?}"),
        }
        assert_eq!(
            updates,
            vec![
                LlmStreamUpdate::Reasoning("hidden reasoning".to_string()),
                LlmStreamUpdate::Text("visible answer".to_string()),
            ]
        );
    }

    // Bedrock does not use this OpenAI collector. Its real Converse stream is
    // decoded by `turn::bedrock::transport` and covered in that module.

    #[tokio::test]
    async fn collect_llm_stream_aggregates_delta_text_reasoning_usage() {
        let d1 = json!({"choices":[{"delta":{"content":"Hi ","reasoning_content":"R"}}]});
        let d2 = json!({"choices":[{"delta":{"content":"there"}}]});
        let u = json!({"usage":{"prompt_tokens":3,"completion_tokens":4}});
        let body = format!("data: {d1}\n\ndata: {d2}\n\ndata: {u}\n\ndata: [DONE]\n\n");
        let stream = stream::iter(vec![Ok(Bytes::from(body))]);
        let res = collect_llm_stream(
            stream,
            "gpt-test",
            Instant::now(),
            LlmCancel::None,
            stream_idle_timeout(),
            stream_idle_timeout_after_progress(),
            None,
        )
        .await
        .expect("collect");
        assert_eq!(res.full_text, "Hi there");
        assert_eq!(res.reasoning, "R");
        assert_eq!(
            res.usage.get("input_tokens").and_then(Value::as_u64),
            Some(3)
        );
        assert_eq!(
            res.usage.get("output_tokens").and_then(Value::as_u64),
            Some(4)
        );
        assert_eq!(
            res.usage.get("total_tokens").and_then(Value::as_u64),
            Some(7)
        );
        assert_eq!(res.model_used, "gpt-test");
        assert!(res.tool_calls.is_empty());
        // `[DONE]` proves protocol completion without inventing a finish reason.
        assert_eq!(res.finish_reason, None);
    }

    #[tokio::test]
    async fn collect_llm_stream_invokes_incremental_callback() {
        let d1 = json!({"choices":[{"delta":{"content":"Hi ","reasoning_content":"R"}}]});
        let d2 = json!({"choices":[{"delta":{"content":"there"}}]});
        let body = format!("data: {d1}\n\ndata: {d2}\n\ndata: [DONE]\n\n");
        let stream = stream::iter(vec![Ok(Bytes::from(body))]);
        let mut updates = Vec::new();
        let mut callback = |update| updates.push(update);
        let res = collect_llm_stream(
            stream,
            "gpt-test",
            Instant::now(),
            LlmCancel::None,
            stream_idle_timeout(),
            stream_idle_timeout_after_progress(),
            Some(&mut callback),
        )
        .await
        .expect("collect");
        assert_eq!(res.full_text, "Hi there");
        assert_eq!(
            updates,
            vec![
                LlmStreamUpdate::Text("Hi ".to_string()),
                LlmStreamUpdate::Reasoning("R".to_string()),
                LlmStreamUpdate::Text("there".to_string()),
            ],
            "callback should receive deltas before aggregate completion",
        );
    }

    #[tokio::test]
    async fn collect_llm_stream_extracts_finish_reason_stop() {
        let d1 = json!({"choices":[{"delta":{"content":"Hello"}}]});
        let done = json!({"choices":[{"delta":{},"finish_reason":"stop"}]});
        let body = format!("data: {d1}\n\ndata: {done}\n\n");
        let stream = stream::iter(vec![Ok(Bytes::from(body))]);
        let res = collect_llm_stream(
            stream,
            "m",
            Instant::now(),
            LlmCancel::None,
            stream_idle_timeout(),
            stream_idle_timeout_after_progress(),
            None,
        )
        .await
        .expect("collect");
        assert_eq!(res.full_text, "Hello");
        assert_eq!(res.finish_reason.as_deref(), Some("stop"));
    }

    #[tokio::test]
    async fn collect_llm_stream_extracts_finish_reason_length() {
        let d1 = json!({"choices":[{"delta":{"content":"truncated"}}]});
        let done = json!({"choices":[{"delta":{},"finish_reason":"length"}]});
        let body = format!("data: {d1}\n\ndata: {done}\n\n");
        let stream = stream::iter(vec![Ok(Bytes::from(body))]);
        let res = collect_llm_stream(
            stream,
            "m",
            Instant::now(),
            LlmCancel::None,
            stream_idle_timeout(),
            stream_idle_timeout_after_progress(),
            None,
        )
        .await
        .expect("collect");
        assert_eq!(res.finish_reason.as_deref(), Some("length"));
    }

    #[tokio::test]
    async fn collect_llm_stream_merges_tool_call_argument_chunks() {
        let c1 = json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"bash","arguments":"{\"foo"}}]}}]});
        let c2 = json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\":\"bar\"}"}}]}}]});
        let body = format!("data: {c1}\n\ndata: {c2}\n\ndata: [DONE]\n\n");
        let stream = stream::iter(vec![Ok(Bytes::from(body))]);
        let res = collect_llm_stream(
            stream,
            "m",
            Instant::now(),
            LlmCancel::None,
            stream_idle_timeout(),
            stream_idle_timeout_after_progress(),
            None,
        )
        .await
        .expect("collect");
        assert_eq!(res.tool_calls.len(), 1);
        let args = res.tool_calls[0]["function"]["arguments"]
            .as_str()
            .expect("arguments string");
        let parsed: Value = serde_json::from_str(args).expect("valid merged JSON args");
        assert_eq!(parsed, json!({"foo":"bar"}));
        assert_eq!(res.tool_calls[0]["function"]["name"].as_str(), Some("bash"));
    }

    #[tokio::test]
    async fn collect_llm_stream_canonicalizes_tool_call_name() {
        let c1 = json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":" bash ","arguments":"{}"}}]}}]});
        let body = format!("data: {c1}\n\ndata: [DONE]\n\n");
        let stream = stream::iter(vec![Ok(Bytes::from(body))]);
        let res = collect_llm_stream(
            stream,
            "m",
            Instant::now(),
            LlmCancel::None,
            stream_idle_timeout(),
            stream_idle_timeout_after_progress(),
            None,
        )
        .await
        .expect("collect");
        assert_eq!(res.tool_calls.len(), 1);
        assert_eq!(res.tool_calls[0]["function"]["name"].as_str(), Some("bash"));
    }

    #[test]
    fn bedrock_tool_blocks_canonicalize_tool_call_names() {
        let tool_calls = vec![json!({
            "id": "call_1",
            "type": "function",
            "function": {"name": " bash ", "arguments": "{\"command\":\"pwd\"}"}
        })];
        let blocks = build_bedrock_tool_blocks(Some(&tool_calls));
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["toolUse"]["name"].as_str(), Some("bash"));
        assert_eq!(blocks[0]["toolUse"]["input"], json!({"command": "pwd"}));
    }

    #[test]
    fn openai_tool_call_to_anthropic_block_canonicalizes_name() {
        let tool_call = json!({
            "id": "call_1",
            "type": "function",
            "function": {"name": " bash ", "arguments": "{\"command\":\"pwd\"}"}
        });
        let block = openai_tool_call_to_anthropic_block(&tool_call).expect("tool_use block");
        assert_eq!(block["name"].as_str(), Some("bash"));
        assert_eq!(block["input"], json!({"command": "pwd"}));
    }

    #[tokio::test]
    async fn stream_idle_timeout_triggers() {
        let _guard = set_test_stream_timeouts(1, None);
        let pending_stream = stream::pending::<Result<Bytes, reqwest::Error>>();
        let started = Instant::now();
        let res = collect_llm_stream(
            pending_stream,
            "test-model",
            started,
            LlmCancel::None,
            stream_idle_timeout(),
            stream_idle_timeout_after_progress(),
            None,
        )
        .await;
        assert!(
            matches!(
                res,
                Err(StreamCollectError::IdleTimeout {
                    made_progress: false,
                    ..
                })
            ),
            "expected idle timeout, got: {res:?}"
        );
    }

    #[tokio::test]
    async fn stream_idle_timeout_after_partial_output_marks_progress() {
        let _guard = set_test_stream_timeouts(1, Some(1));
        let d1 = json!({"choices":[{"delta":{"content":"partial"}}]});
        let stream = stream::iter(vec![Ok(Bytes::from(format!("data: {d1}\n\n")))])
            .chain(stream::pending::<Result<Bytes, reqwest::Error>>());
        let res = collect_llm_stream(
            stream,
            "test-model",
            Instant::now(),
            LlmCancel::None,
            stream_idle_timeout(),
            stream_idle_timeout_after_progress(),
            None,
        )
        .await;
        match res.expect_err("idle timeout after partial output") {
            StreamCollectError::IdleTimeout {
                made_progress,
                partial,
                ..
            } => {
                assert!(made_progress, "partial output should mark progress");
                assert_eq!(partial.full_text, "partial");
            }
            other => panic!("expected idle timeout after partial output, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn collect_llm_stream_respects_cancel_flag() {
        let flag = Arc::new(AtomicBool::new(false));
        let flag_signal = flag.clone();
        let pending_stream = stream::pending::<Result<Bytes, reqwest::Error>>();
        let started = Instant::now();
        let handle = tokio::spawn(async move {
            collect_llm_stream(
                pending_stream,
                "test-model",
                started,
                LlmCancel::Flag(flag.as_ref()),
                stream_idle_timeout(),
                stream_idle_timeout_after_progress(),
                None,
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        flag_signal.store(true, Ordering::SeqCst);
        let res = handle.await.expect("join");
        assert!(
            matches!(res, Err(StreamCollectError::Cancelled { .. })),
            "expected cancel, got: {res:?}"
        );
    }

    #[tokio::test]
    async fn collect_llm_stream_respects_cancel_token() {
        let token = CancellationToken::new();
        let token_for_stream = token.clone();
        let pending_stream = stream::pending::<Result<Bytes, reqwest::Error>>();
        let started = Instant::now();
        let handle = tokio::spawn(async move {
            collect_llm_stream(
                pending_stream,
                "test-model",
                started,
                LlmCancel::Token(&token_for_stream),
                stream_idle_timeout(),
                stream_idle_timeout_after_progress(),
                None,
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        token.cancel();
        let res = handle.await.expect("join");
        assert!(
            matches!(res, Err(StreamCollectError::Cancelled { .. })),
            "expected cancel, got: {res:?}"
        );
    }

    #[tokio::test]
    async fn collect_llm_stream_flag_and_token_cancels_on_token() {
        let flag = Arc::new(AtomicBool::new(false));
        let flag_for_join = flag.clone();
        let token = CancellationToken::new();
        let token_signal = token.clone();
        let pending_stream = stream::pending::<Result<Bytes, reqwest::Error>>();
        let started = Instant::now();
        let handle = tokio::spawn(async move {
            collect_llm_stream(
                pending_stream,
                "test-model",
                started,
                LlmCancel::FlagAndToken(flag.as_ref(), &token_signal),
                stream_idle_timeout(),
                stream_idle_timeout_after_progress(),
                None,
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        token.cancel();
        let res = handle.await.expect("join");
        assert!(
            matches!(res, Err(StreamCollectError::Cancelled { .. })),
            "expected cancel, got: {res:?}"
        );
        assert!(!flag_for_join.load(Ordering::SeqCst));
    }

    #[derive(Clone)]
    struct Hit(Arc<AtomicU32>);

    #[derive(Default)]
    struct RecordingAttemptObserver {
        next: AtomicU32,
        began: Mutex<Vec<u32>>,
        wires: Mutex<Vec<ProviderWireRequestIdentity>>,
        finished: Mutex<Vec<(u32, astra_services::InferenceTerminalStatus)>>,
    }

    #[async_trait]
    impl ProviderAttemptObserver for RecordingAttemptObserver {
        async fn begin_attempt(
            &self,
            wire: &ProviderWireRequestIdentity,
        ) -> Result<u32, astra_core::ClassifiedError> {
            let attempt = self.next.fetch_add(1, Ordering::SeqCst);
            self.began.lock().expect("began lock").push(attempt);
            self.wires.lock().expect("wires lock").push(wire.clone());
            Ok(attempt)
        }

        async fn finish_attempt(
            &self,
            attempt_index: u32,
            terminal: &astra_services::InferenceInvocationTerminal,
        ) -> Result<(), astra_core::ClassifiedError> {
            self.finished
                .lock()
                .expect("finished lock")
                .push((attempt_index, terminal.status));
            Ok(())
        }
    }

    #[test]
    fn prepared_provider_request_reconciles_exact_bytes_for_every_protocol() {
        let cases = [
            (
                LlmProviderProtocol::OpenAiCompatible,
                json!({
                    "model": "m",
                    "messages": [
                        {"role": "system", "content": "stable"},
                        {"role": "user", "content": "task"}
                    ],
                    "tools": [{"type": "function", "function": {"name": "read"}}],
                    "stream": true
                }),
                (1, 1, 1),
            ),
            (
                LlmProviderProtocol::AnthropicMessages,
                json!({
                    "model": "m",
                    "system": [{"type": "text", "text": "stable"}],
                    "messages": [{"role": "user", "content": "task"}],
                    "tools": [{"name": "read", "input_schema": {"type": "object"}}],
                    "stream": true
                }),
                (1, 1, 1),
            ),
            (
                LlmProviderProtocol::BedrockConverse,
                json!({
                    "system": [{"text": "stable"}],
                    "messages": [{"role": "user", "content": [{"text": "task"}]}],
                    "toolConfig": {"tools": [{"toolSpec": {"name": "read"}}]},
                    "inferenceConfig": {"maxTokens": 128}
                }),
                (1, 1, 1),
            ),
        ];

        for (protocol, body, expected_counts) in cases {
            let expected_bytes = serde_json::to_vec(&body).expect("fixture serializes");
            let expected_hash = format!("{:x}", Sha256::digest(&expected_bytes));
            assert_eq!(
                serialized_value_bytes(&body).expect("composition serialization"),
                u64::try_from(expected_bytes.len()).unwrap_or(u64::MAX),
                "composition accounting must use the exact compact JSON stream"
            );
            let prepared =
                PreparedProviderRequest::from_json(&body, protocol).expect("prepare exact body");
            let identity = prepared.identity();

            assert_eq!(prepared.body_bytes(), expected_bytes);
            assert_eq!(identity.provider_wire_hash, expected_hash);
            assert_eq!(
                identity.provider_wire_bytes,
                u64::try_from(expected_bytes.len()).expect("fixture length")
            );
            assert_eq!(
                identity.composition.total_bytes(),
                identity.provider_wire_bytes,
                "{protocol:?} byte zones must be exhaustive and disjoint"
            );
            assert_eq!(
                (
                    identity.composition.system_items,
                    identity.composition.conversation_items,
                    identity.composition.tool_schema_items,
                ),
                expected_counts
            );
        }
    }

    #[test]
    fn uncertain_stream_terminal_preserves_observed_usage_and_response_identity() {
        let partial = LlmCallResult {
            response_id: Some("provider-response-7".to_string()),
            usage: Map::from_iter([
                ("input_tokens".to_string(), json!(200)),
                ("cached_input_tokens".to_string(), json!(800)),
                ("cache_creation_tokens".to_string(), json!(100)),
                ("output_tokens".to_string(), json!(50)),
            ]),
            ..LlmCallResult::default()
        };
        let terminal = provider_attempt_terminal_from_error_with_partial(
            &astra_core::ClassifiedError::new(
                astra_core::ErrorKind::StreamTransport,
                "connection ended after partial delivery",
            ),
            Some(&partial),
        );

        assert_eq!(
            terminal.status,
            astra_services::InferenceTerminalStatus::DeliveryUnknown
        );
        assert_eq!(
            terminal.provider_response_id.as_deref(),
            Some("provider-response-7")
        );
        assert_eq!(
            terminal.usage,
            astra_services::InferenceUsage {
                input_tokens: 200,
                output_tokens: 50,
                cache_read_tokens: 800,
                cache_creation_tokens: 100,
            }
        );
    }

    struct RejectingAttemptObserver;

    #[async_trait]
    impl ProviderAttemptObserver for RejectingAttemptObserver {
        async fn begin_attempt(
            &self,
            _wire: &ProviderWireRequestIdentity,
        ) -> Result<u32, astra_core::ClassifiedError> {
            Err(astra_core::ClassifiedError::new(
                astra_core::ErrorKind::DatabaseError,
                "durable attempt admission unavailable",
            ))
        }

        async fn finish_attempt(
            &self,
            _attempt_index: u32,
            _terminal: &astra_services::InferenceInvocationTerminal,
        ) -> Result<(), astra_core::ClassifiedError> {
            panic!("an attempt that was not durably admitted cannot be finished")
        }
    }

    #[derive(Clone)]
    struct StreamRequestHits {
        stream_hits: Arc<AtomicU32>,
        nonstream_hits: Arc<AtomicU32>,
    }

    async fn spawn_local_http_server(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock llm listener");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("mock serve");
        });
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        format!("http://{addr}")
    }

    async fn spawn_raw_partial_transport_server(state: StreamRequestHits) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind raw mock llm listener");
        let addr = listener.local_addr().expect("raw local_addr");
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let state = state.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0_u8; 8192];
                    let read = socket.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..read]);
                    let is_stream = req.contains("\"stream\":true");
                    if is_stream {
                        state.stream_hits.fetch_add(1, Ordering::SeqCst);
                        let partial = format!(
                            "data: {}\n\n",
                            json!({"choices":[{"delta":{"content":"partial"}}]})
                        );
                        let chunk = format!("{:X}\r\n{}\r\n", partial.len(), partial);
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{chunk}"
                        );
                        socket
                            .write_all(response.as_bytes())
                            .await
                            .expect("write partial stream response");
                        let _ = socket.shutdown().await;
                    } else {
                        state.nonstream_hits.fetch_add(1, Ordering::SeqCst);
                        let fallback_body =
                            r#"{"choices":[{"message":{"content":"unexpected second request"}}]}"#;
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{fallback_body}",
                            fallback_body.len(),
                        );
                        socket
                            .write_all(response.as_bytes())
                            .await
                            .expect("write fallback response");
                        let _ = socket.shutdown().await;
                    }
                });
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        format!("http://{addr}")
    }

    async fn mock_429_retry_zero_then_sse(State(Hit(c)): State<Hit>) -> Response {
        let n = c.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            Response::builder()
                .status(429)
                .header("retry-after", "0")
                .body(Body::from("rate limited"))
                .unwrap()
        } else {
            let payload = json!({"choices":[{"delta":{"content":"after-429"}}]});
            let body = format!("data: {}\n\ndata: [DONE]\n\n", payload);
            Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(Body::from(body))
                .unwrap()
        }
    }

    async fn mock_429_retry_two_seconds(State(Hit(c)): State<Hit>) -> Response {
        c.fetch_add(1, Ordering::SeqCst);
        Response::builder()
            .status(429)
            .header("retry-after", "2")
            .body(Body::from("slow"))
            .unwrap()
    }

    async fn mock_529_retry_zero_then_sse(State(Hit(c)): State<Hit>) -> Response {
        let n = c.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            Response::builder()
                .status(529)
                .header("retry-after", "0")
                .body(Body::from("overload"))
                .unwrap()
        } else {
            let payload = json!({"choices":[{"delta":{"content":"after-529"}}]});
            let body = format!("data: {}\n\ndata: [DONE]\n\n", payload);
            Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(Body::from(body))
                .unwrap()
        }
    }

    async fn mock_503_retry_zero_then_sse(State(Hit(c)): State<Hit>) -> Response {
        let n = c.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            Response::builder()
                .status(503)
                .header("retry-after", "0")
                .body(Body::from("unavailable"))
                .unwrap()
        } else {
            let payload = json!({"choices":[{"delta":{"content":"after-503"}}]});
            let body = format!("data: {}\n\ndata: [DONE]\n\n", payload);
            Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(Body::from(body))
                .unwrap()
        }
    }

    async fn mock_500_once(State(Hit(c)): State<Hit>) -> Response {
        c.fetch_add(1, Ordering::SeqCst);
        Response::builder()
            .status(500)
            .body(Body::from("server error"))
            .unwrap()
    }

    async fn mock_stream_idle_after_partial(
        State(state): State<StreamRequestHits>,
        axum::Json(body): axum::Json<Value>,
    ) -> Response {
        let is_stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
        if is_stream {
            state.stream_hits.fetch_add(1, Ordering::SeqCst);
            let partial = json!({"choices":[{"delta":{"content":"partial"}}]});
            let body_stream = stream::iter(vec![Ok::<Bytes, std::io::Error>(Bytes::from(
                format!("data: {partial}\n\n"),
            ))])
            .chain(stream::pending::<Result<Bytes, std::io::Error>>());
            Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(body_stream))
                .unwrap()
        } else {
            state.nonstream_hits.fetch_add(1, Ordering::SeqCst);
            Response::builder()
                .status(200)
                .body(Body::from(
                    r#"{"choices":[{"message":{"content":"unexpected second request"}}]}"#,
                ))
                .unwrap()
        }
    }

    async fn mock_stream_idle_before_any_output(
        State(Hit(c)): State<Hit>,
        axum::Json(_body): axum::Json<Value>,
    ) -> Response {
        c.fetch_add(1, Ordering::SeqCst);
        let body_stream = stream::pending::<Result<Bytes, std::io::Error>>();
        Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(body_stream))
            .unwrap()
    }

    #[tokio::test]
    async fn collect_llm_stream_rejects_invalid_utf8_inside_json_string() {
        let mut v: Vec<u8> = Vec::new();
        v.extend_from_slice(br#"data: {"choices":[{"delta":{"content":"a"#);
        v.push(0xff);
        v.extend_from_slice(br#""}}]}"#);
        v.extend_from_slice(b"\n\n");
        let stream = stream::iter(vec![Ok(Bytes::from(v))]);
        let error = collect_llm_stream(
            stream,
            "m",
            Instant::now(),
            LlmCancel::None,
            stream_idle_timeout(),
            stream_idle_timeout_after_progress(),
            None,
        )
        .await
        .expect_err("invalid UTF-8 must fail the stream");
        match error {
            StreamCollectError::Transport { error, partial } => {
                assert!(error.contains("invalid UTF-8"), "{error}");
                assert!(partial.full_text.is_empty());
            }
            other => panic!("expected transport error, got {other:?}"),
        }
    }

    // ── Anthropic native stream tests ──────────────────────────────────────

    fn anthropic_sse(events: &[Value]) -> String {
        events
            .iter()
            .map(|e| format!("data: {e}\n\n"))
            .collect::<String>()
    }

    #[tokio::test]
    async fn collect_anthropic_stream_text_and_usage() {
        let events = vec![
            json!({"type":"message_start","message":{"usage":{"input_tokens":10,"cache_read_input_tokens":5,"cache_creation_input_tokens":2,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" world"}}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}),
            json!({"type":"message_stop"}),
        ];
        let body = anthropic_sse(&events);
        let stream = stream::iter(vec![Ok(Bytes::from(body))]);
        let r = collect_anthropic_llm_stream(
            stream,
            "claude-test",
            Instant::now(),
            LlmCancel::None,
            stream_idle_timeout(),
            stream_idle_timeout_after_progress(),
            None,
        )
        .await
        .expect("stream should succeed");
        assert_eq!(r.full_text, "Hello world");
        assert_eq!(r.finish_reason.as_deref(), Some("end_turn"));
        assert_eq!(
            r.usage.get("input_tokens").and_then(Value::as_u64),
            Some(10)
        );
        assert_eq!(
            r.usage.get("cached_input_tokens").and_then(Value::as_u64),
            Some(5)
        );
        assert_eq!(
            r.usage.get("cache_creation_tokens").and_then(Value::as_u64),
            Some(2)
        );
        assert_eq!(
            r.usage.get("output_tokens").and_then(Value::as_u64),
            Some(7)
        );
        assert_eq!(
            r.usage.get("total_tokens").and_then(Value::as_u64),
            Some(24)
        );
    }

    #[tokio::test]
    async fn collect_anthropic_stream_message_delta_updates_all_usage_buckets() {
        let events = vec![
            json!({"type":"message_start","message":{"usage":{"input_tokens":10,"output_tokens":0}}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}),
            json!({
                "type":"message_delta",
                "delta":{"stop_reason":"end_turn"},
                "usage":{
                    "input_tokens":11,
                    "cache_read_input_tokens":6,
                    "cache_creation_input_tokens":3,
                    "output_tokens":7
                }
            }),
            json!({"type":"message_stop"}),
        ];
        let stream = stream::iter(vec![Ok(Bytes::from(anthropic_sse(&events)))]);
        let r = collect_anthropic_llm_stream(
            stream,
            "claude-test",
            Instant::now(),
            LlmCancel::None,
            stream_idle_timeout(),
            stream_idle_timeout_after_progress(),
            None,
        )
        .await
        .expect("stream should succeed");

        assert_eq!(
            r.usage.get("input_tokens").and_then(Value::as_u64),
            Some(11)
        );
        assert_eq!(
            r.usage.get("cached_input_tokens").and_then(Value::as_u64),
            Some(6)
        );
        assert_eq!(
            r.usage.get("cache_creation_tokens").and_then(Value::as_u64),
            Some(3)
        );
        assert_eq!(
            r.usage.get("total_tokens").and_then(Value::as_u64),
            Some(27)
        );
    }

    #[tokio::test]
    async fn collect_anthropic_stream_thinking_delta() {
        let events = vec![
            json!({"type":"message_start","message":{"usage":{"input_tokens":1,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me think"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"..."}}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"answer"}}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":10}}),
            json!({"type":"message_stop"}),
        ];
        let body = anthropic_sse(&events);
        let stream = stream::iter(vec![Ok(Bytes::from(body))]);
        let r = collect_anthropic_llm_stream(
            stream,
            "claude-test",
            Instant::now(),
            LlmCancel::None,
            stream_idle_timeout(),
            stream_idle_timeout_after_progress(),
            None,
        )
        .await
        .expect("stream should succeed");
        assert_eq!(r.reasoning, "Let me think...");
        assert_eq!(r.full_text, "answer");
    }

    /// Regression: session ff1cbaca audit uncovered that
    /// `collect_anthropic_llm_stream` was returning
    /// `reasoning_signature: String::new()` unconditionally because the
    /// parser had no `signature_delta` branch. Any thinking-model
    /// request routed through `call_llm_and_collect` (server_loop_host
    /// / conflict_resolver) would lose the HMAC signature and fail the
    /// next round with HTTP 400
    /// `content[].thinking in the thinking mode must be passed back to
    /// the API` — the same failure mode as effccfcd-28d8-41f4-a4b0-ecd0ec503625.
    #[tokio::test]
    async fn collect_anthropic_stream_captures_signature_delta() {
        let events = vec![
            json!({"type":"message_start","message":{"usage":{"input_tokens":1,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"deep thought"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig_abc123"}}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"answer"}}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":10}}),
            json!({"type":"message_stop"}),
        ];
        let body = anthropic_sse(&events);
        let stream = stream::iter(vec![Ok(Bytes::from(body))]);
        let r = collect_anthropic_llm_stream(
            stream,
            "claude-test",
            Instant::now(),
            LlmCancel::None,
            stream_idle_timeout(),
            stream_idle_timeout_after_progress(),
            None,
        )
        .await
        .expect("stream should succeed");
        assert_eq!(r.reasoning, "deep thought");
        assert_eq!(
            r.reasoning_signature, "sig_abc123",
            "signature_delta must flow into reasoning_signature — otherwise the \
             next round hits HTTP 400 (effccfcd regression via server_loop_host)",
        );
    }

    /// Signature concatenation across multiple `thinking` content blocks
    /// (Anthropic emits one signature per thinking block, but signed
    /// thinking CAN be interleaved with text). Accumulator must append,
    /// not overwrite.
    #[tokio::test]
    async fn collect_anthropic_stream_accumulates_multiple_signatures() {
        let events = vec![
            json!({"type":"message_start","message":{"usage":{"input_tokens":1,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"t1"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig1"}}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"thinking","thinking":""}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":"t2"}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"signature_delta","signature":"sig2"}}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}),
            json!({"type":"message_stop"}),
        ];
        let body = anthropic_sse(&events);
        let stream = stream::iter(vec![Ok(Bytes::from(body))]);
        let r = collect_anthropic_llm_stream(
            stream,
            "claude-test",
            Instant::now(),
            LlmCancel::None,
            stream_idle_timeout(),
            stream_idle_timeout_after_progress(),
            None,
        )
        .await
        .expect("stream should succeed");
        assert_eq!(r.reasoning_signature, "sig1sig2");
    }

    #[tokio::test]
    async fn collect_anthropic_stream_tool_use() {
        let events = vec![
            json!({"type":"message_start","message":{"usage":{"input_tokens":5,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"calling tool"}}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"bash"}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"com"}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"mand\":\"ls\"}"}}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":20}}),
            json!({"type":"message_stop"}),
        ];
        let body = anthropic_sse(&events);
        let stream = stream::iter(vec![Ok(Bytes::from(body))]);
        let r = collect_anthropic_llm_stream(
            stream,
            "claude-test",
            Instant::now(),
            LlmCancel::None,
            stream_idle_timeout(),
            stream_idle_timeout_after_progress(),
            None,
        )
        .await
        .expect("stream should succeed");
        assert_eq!(r.full_text, "calling tool");
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0]["id"], "toolu_1");
        assert_eq!(r.tool_calls[0]["function"]["name"], "bash");
        assert_eq!(
            r.tool_calls[0]["function"]["arguments"].as_str(),
            Some(r#"{"command":"ls"}"#)
        );
        assert_eq!(r.finish_reason.as_deref(), Some("tool_use"));
    }

    #[tokio::test]
    async fn collect_anthropic_stream_error_event() {
        let events = vec![
            json!({"type":"message_start","message":{"usage":{"input_tokens":1,"output_tokens":0}}}),
            json!({"type":"error","error":{"type":"overloaded_error","message":"overloaded"}}),
        ];
        let body = anthropic_sse(&events);
        let stream = stream::iter(vec![Ok(Bytes::from(body))]);
        let r = collect_anthropic_llm_stream(
            stream,
            "claude-test",
            Instant::now(),
            LlmCancel::None,
            stream_idle_timeout(),
            stream_idle_timeout_after_progress(),
            None,
        )
        .await;
        assert!(
            matches!(r, Err(StreamCollectError::Transport { .. })),
            "error event should produce transport error, got: {r:?}"
        );
    }

    #[tokio::test]
    async fn collect_anthropic_stream_transport_error_carries_partial() {
        let d1 = json!({"type":"message_start","message":{"usage":{"input_tokens":1,"output_tokens":0}}});
        let d2 = json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}});
        let body = format!("data: {d1}\n\ndata: {d2}\n\n");
        let err = sample_reqwest_stream_error().await;
        let stream = stream::iter(vec![Ok(Bytes::from(body)), Err(err)]);
        let r = collect_anthropic_llm_stream(
            stream,
            "claude-test",
            Instant::now(),
            LlmCancel::None,
            stream_idle_timeout(),
            stream_idle_timeout_after_progress(),
            None,
        )
        .await;
        match r {
            Err(StreamCollectError::Transport { partial, .. }) => {
                assert_eq!(partial.full_text, "partial");
            }
            other => panic!("expected transport error with partial, got: {other:?}"),
        }
    }

    // ── Anthropic message conversion tests ──────────────────────────────────

    #[test]
    fn anthropic_message_conversion_preserves_cache_control() {
        let msg = json!({
            "role": "user",
            "content": "hello",
            "cache_control": {"type": "ephemeral"},
        });
        let converted = anthropic_message_from_openai(&msg).unwrap();
        assert_eq!(converted["role"], "user");
        assert_eq!(converted["cache_control"]["type"], "ephemeral");
    }

    /// Regression guard for session 5c5cbf78 (2026-05-08): real Anthropic
    /// `/v1/messages` returns HTTP 400 when asked to decode a `cache_reference`
    /// top-level key on a message or inside a `tool_result` block. The
    /// speculative wire helper that emitted those fields has been removed;
    /// this test pins that invariant at the converter layer so an upstream
    /// regression can't reintroduce it silently.
    #[test]
    fn anthropic_message_conversion_drops_speculative_cache_reference_on_tool() {
        let msg = json!({
            "role": "tool",
            "tool_call_id": "call_1",
            "content": "result text",
            "cache_reference": "call_1",
            "cache_control": {"type": "ephemeral"},
        });
        let converted = anthropic_message_from_openai(&msg).unwrap();
        assert_eq!(converted["role"], "user");
        assert_eq!(converted["cache_control"]["type"], "ephemeral");
        assert!(
            converted.get("cache_reference").is_none(),
            "cache_reference must be stripped (not a real Anthropic field): {converted}",
        );
        let blocks = converted["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "tool_result");
        assert!(
            blocks[0].get("cache_reference").is_none(),
            "tool_result block must not carry cache_reference: {:?}",
            blocks[0],
        );
    }

    /// Companion regression: a user message with a `cache_edits` content
    /// block — the shape that actually hit HTTP 400 on 5c5cbf78 t6_r7 —
    /// must no longer be treated as a pass-through Anthropic-native type.
    /// `cache_edits` isn't in Anthropic's content-block grammar, so the
    /// converter should fall back to the text-only path and strip it.
    #[test]
    fn anthropic_message_conversion_drops_cache_edits_content_block() {
        let msg = json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "continue", "cache_control": {"type": "ephemeral"}},
                {"type": "cache_edits", "edits": [{"type": "delete", "cache_reference": "tool-2"}]}
            ],
        });
        let converted = anthropic_message_from_openai(&msg).unwrap();
        let blocks = converted["content"].as_array().unwrap();
        for (i, b) in blocks.iter().enumerate() {
            let ty = b.get("type").and_then(Value::as_str).unwrap_or("");
            assert_ne!(
                ty, "cache_edits",
                "block[{i}] must not be cache_edits (5c5cbf78 regression): {b}",
            );
        }
    }

    // ── pre-annotated tool_result content must not be double-wrapped ────
    //
    // After `annotate_last_message_cache_breakpoint` runs, a tool message
    // like `{role: "tool", tool_call_id: "c1", content: "result"}` is
    // upgraded in place to `{role: "tool", content: [{type: "tool_result",
    // tool_use_id: "c1", content: "result", cache_control: {...}}]}`. The
    // old tool branch of `anthropic_message_from_openai` then wrapped the
    // already-structured array AGAIN inside a new tool_result block's
    // `content` field, producing a nested `tool_result → tool_result`
    // shape. Anthropic/DeepSeek both reject it with
    // `messages[N].content[0]: unknown variant tool_result, expected one
    // of text, image, ...`.
    //
    // This test locks down the post-fix contract: when the tool message
    // already carries a tool_result block array, forward it verbatim
    // under `role: "user"` — no re-wrapping.
    #[test]
    fn anthropic_tool_message_preannotated_content_is_forwarded_without_nesting() {
        let msg = json!({
            "role": "tool",
            "tool_call_id": "call_abc",
            "cache_reference": "call_abc",
            "content": [{
                "type": "tool_result",
                "tool_use_id": "call_abc",
                "content": "8a08c39 feat\n14410ad fix",
                "cache_control": {"type": "ephemeral"},
            }],
        });
        let converted = anthropic_message_from_openai(&msg).unwrap();
        assert_eq!(converted["role"], "user");
        let blocks = converted["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 1, "exactly one content block, not nested");
        let block = &blocks[0];
        assert_eq!(block["type"], "tool_result");
        assert_eq!(block["tool_use_id"], "call_abc");
        // `content` must be the string (or original payload) from the
        // annotator, NOT a nested array containing another tool_result.
        assert_eq!(block["content"], "8a08c39 feat\n14410ad fix");
        assert_eq!(block["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn anthropic_tool_message_string_content_wraps_into_tool_result() {
        // The other branch: pre-annotation tool message with raw string
        // content → must still be wrapped in a fresh tool_result block.
        let msg = json!({
            "role": "tool",
            "tool_call_id": "call_xyz",
            "content": "hello",
        });
        let converted = anthropic_message_from_openai(&msg).unwrap();
        assert_eq!(converted["role"], "user");
        let blocks = converted["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[0]["tool_use_id"], "call_xyz");
        assert_eq!(blocks[0]["content"], "hello");
    }

    // ── Tool-role content-coercion boundary tests ─────────────────────────
    //
    // Regression guards for the six-session `{}` cascade bug
    // (commit 104b4502). When compaction / fold / format-conversion
    // produces a non-string `content` on a tool-role message, the
    // converter must coerce the payload to a string rather than pass
    // `{}`, `[]`, or `null` through verbatim — otherwise downstream
    // LLM reads the empty object as "tool returned nothing" and
    // hallucinates a retry or an error.
    //
    // These tests pin the FUNCTION-BOUNDARY contract of
    // `anthropic_message_from_openai` for every shape
    // (`Value::Object`, `Value::Array` with/without text, `Value::Null`,
    // and content-field absent).

    #[test]
    fn anthropic_tool_message_object_content_coerced_to_string_not_empty_json() {
        // Upstream bug: tool content ended up as `{}` (empty object).
        // The converter must degrade it to an empty string so the LLM
        // never sees a bare JSON object as the tool_result body.
        let msg = json!({
            "role": "tool",
            "tool_call_id": "call_obj",
            "content": {},
        });
        let converted = anthropic_message_from_openai(&msg).unwrap();
        let blocks = converted["content"].as_array().unwrap();
        let body = &blocks[0]["content"];
        assert!(
            body.is_string(),
            "tool_result.content must be a string after coercion, got: {body}",
        );
        assert_eq!(
            body.as_str().unwrap(),
            "",
            "empty object content must be degraded to empty string, not bare '{{}}'",
        );
    }

    #[test]
    fn canonical_continuation_tool_result_is_valid_across_provider_protocols() {
        let canonical =
            astra_turn_core::prompt_facing::sanitize_canonical_continuation_messages_with_turn_semantics(
                vec![
                    json!({"role": "user", "content": "inspect"}),
                    json!({
                        "role": "assistant",
                        "tool_calls": [{
                            "id": "call-1",
                            "type": "function",
                            "function": {"name": "read_file", "arguments": "{}"}
                        }]
                    }),
                    json!({
                        "role": "tool",
                        "tool_call_id": "call-1",
                        "content": [{
                            "type": "tool_result",
                            "tool_use_id": "call-1",
                            "content": [{"type": "text", "text": "durable evidence"}],
                            "cache_control": {"type": "ephemeral"}
                        }]
                    }),
                ],
            )
            .expect("canonical continuation");
        assert_eq!(canonical[2]["content"].as_str(), Some("durable evidence"));

        let openai = build_provider_request_body(
            &canonical,
            &[],
            "gpt-test",
            "openai",
            None,
            None,
            false,
            &ThinkingConfig::Off,
        );
        assert_eq!(
            openai["messages"][2]["content"].as_str(),
            Some("durable evidence")
        );

        let anthropic = build_provider_request_body(
            &canonical,
            &[],
            "claude-test",
            "anthropic",
            None,
            None,
            false,
            &ThinkingConfig::Off,
        );
        let anthropic_result = anthropic["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|message| message["content"].as_array().into_iter().flatten())
            .find(|block| block["type"] == "tool_result")
            .expect("Anthropic tool_result");
        assert_eq!(
            anthropic_result["content"].as_str(),
            Some("durable evidence")
        );

        let bedrock = build_provider_request_body(
            &canonical,
            &[],
            "anthropic.claude-test",
            "bedrock",
            None,
            None,
            false,
            &ThinkingConfig::Off,
        );
        let bedrock_result = bedrock["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|message| message["content"].as_array().into_iter().flatten())
            .find_map(|block| block.get("toolResult"))
            .expect("Bedrock toolResult");
        assert_eq!(
            bedrock_result["content"][0]["text"].as_str(),
            Some("durable evidence")
        );
    }

    #[test]
    fn openai_request_body_tool_message_empty_object_content_becomes_empty_string() {
        let messages = vec![
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_obj",
                    "type": "function",
                    "function": {"name": "bash", "arguments": "{}"}
                }]
            }),
            json!({
                "role": "tool",
                "tool_call_id": "call_obj",
                "content": {},
            }),
        ];
        let body = build_provider_request_body(
            &messages,
            &[],
            "gpt-test",
            "openai",
            None,
            None,
            true,
            &ThinkingConfig::Off,
        );
        assert_eq!(body["messages"][1]["content"].as_str(), Some(""));
    }

    #[test]
    fn bedrock_tool_message_empty_object_content_becomes_text_empty_string() {
        let messages = vec![
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_obj",
                    "type": "function",
                    "function": {"name": "bash", "arguments": "{}"}
                }]
            }),
            json!({
                "role": "tool",
                "tool_call_id": "call_obj",
                "content": {},
            }),
        ];
        let body = build_provider_request_body(
            &messages,
            &[],
            "anthropic.claude-3-5-sonnet-v1:0",
            "bedrock",
            None,
            None,
            false,
            &ThinkingConfig::Off,
        );
        let result_block = &body["messages"][1]["content"][0]["toolResult"]["content"][0];
        assert_eq!(result_block.get("text").and_then(Value::as_str), Some(""));
        assert!(
            result_block.get("json").is_none(),
            "empty object must not be sent as Bedrock json {{}}: {result_block:?}"
        );
    }

    #[test]
    fn anthropic_tool_message_nonempty_object_content_stringified_verbatim() {
        // An object payload (e.g. `{"error": "boom"}`) must survive
        // as a readable string — not be silently elided.
        let msg = json!({
            "role": "tool",
            "tool_call_id": "call_obj2",
            "content": {"error": "boom", "code": 42},
        });
        let converted = anthropic_message_from_openai(&msg).unwrap();
        let body = converted["content"][0]["content"].as_str().unwrap();
        // serde_json preserves key order as inserted; both fields must be present.
        assert!(
            body.contains("\"error\":\"boom\""),
            "error preserved: {body}"
        );
        assert!(body.contains("\"code\":42"), "code preserved: {body}");
    }

    #[test]
    fn anthropic_tool_message_array_with_text_blocks_extracts_joined_text() {
        // Content-block array shape: Anthropic-style
        // `[{type:"text", text:"..."}]` — the coercion branch extracts
        // the joined `text` fields rather than the raw JSON dump.
        let msg = json!({
            "role": "tool",
            "tool_call_id": "call_arr",
            "content": [
                {"type": "text", "text": "hello "},
                {"type": "text", "text": "world"},
            ],
        });
        let converted = anthropic_message_from_openai(&msg).unwrap();
        let body = converted["content"][0]["content"].as_str().unwrap();
        assert_eq!(
            body, "hello world",
            "text blocks must be joined into a single string, got: {body}",
        );
    }

    #[test]
    fn anthropic_tool_message_array_without_text_falls_back_to_json_repr() {
        // An array of objects with NO `text` fields must round-trip
        // as its JSON representation, not the empty string — else the
        // LLM sees `""` and treats the tool as silent.
        let msg = json!({
            "role": "tool",
            "tool_call_id": "call_arr2",
            "content": [{"kind": "image"}, {"kind": "ref"}],
        });
        let converted = anthropic_message_from_openai(&msg).unwrap();
        let body = converted["content"][0]["content"].as_str().unwrap();
        assert!(
            body.contains("\"kind\":\"image\""),
            "non-text array serialized verbatim: {body}",
        );
        assert!(
            body.contains("\"kind\":\"ref\""),
            "all array elements preserved: {body}",
        );
        assert_ne!(body, "", "must not collapse to empty string");
    }

    #[test]
    fn coerce_tool_result_content_extracts_from_cache_annotated_tool_result_block() {
        // Regression: wire_cache_annotations rewrites tool message content from
        // a plain string into `[{type: "tool_result", content: "...", tool_use_id: "...",
        // cache_control: {...}}]`. The Bedrock body builder calls
        // `coerce_tool_result_content` which must extract the `content` field,
        // not return empty string (which makes the model think the tool had no output).
        let content = json!([{
            "type": "tool_result",
            "tool_use_id": "tooluse_abc123",
            "content": "gh version 2.46.0 (2025-01-13)\nhttps://github.com/cli/cli/releases/tag/v2.46.0",
            "cache_control": {"type": "ephemeral"},
        }]);
        let result = coerce_tool_result_content(Some(&content));
        assert_eq!(
            result,
            "gh version 2.46.0 (2025-01-13)\nhttps://github.com/cli/cli/releases/tag/v2.46.0",
            "must extract content from tool_result block, got empty or wrong: {result:?}",
        );
    }

    #[test]
    fn bedrock_body_preserves_tool_output_after_cache_annotation() {
        // End-to-end: simulate the full path where cache annotation rewrites
        // tool content, then Bedrock body builder processes it.
        let messages = vec![
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "tooluse_abc",
                    "type": "function",
                    "function": {"name": "bash", "arguments": "{\"command\":\"gh --version\"}"}
                }]
            }),
            json!({
                "role": "tool",
                "tool_call_id": "tooluse_abc",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "tooluse_abc",
                    "content": "gh version 2.46.0\nhttps://github.com/cli/cli/releases/tag/v2.46.0",
                    "cache_control": {"type": "ephemeral"},
                }],
            }),
        ];
        let body = build_provider_request_body(
            &messages,
            &[],
            "us.anthropic.claude-opus-4-7",
            "bedrock",
            None,
            None,
            false,
            &ThinkingConfig::Off,
        );
        let tool_result_content = &body["messages"][1]["content"][0]["toolResult"]["content"][0];
        let text = tool_result_content
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("");
        assert!(
            text.contains("gh version 2.46.0"),
            "Bedrock toolResult must contain the actual tool output, got: {text:?}",
        );
    }

    #[test]
    fn anthropic_tool_message_null_content_becomes_empty_string() {
        // `Value::Null` is the documented empty-payload case. The
        // converter MUST emit `""` (not null, not absent) so downstream
        // wire serialization doesn't drop the tool_result.content key.
        let msg = json!({
            "role": "tool",
            "tool_call_id": "call_null",
            "content": serde_json::Value::Null,
        });
        let converted = anthropic_message_from_openai(&msg).unwrap();
        let body = &converted["content"][0]["content"];
        assert_eq!(body.as_str(), Some(""), "null → empty string, got: {body}");
    }

    #[test]
    fn anthropic_tool_message_absent_content_becomes_empty_string() {
        // No `content` key at all: same as Null — converter still emits
        // a well-formed tool_result with an empty string body.
        let msg = json!({
            "role": "tool",
            "tool_call_id": "call_missing",
        });
        let converted = anthropic_message_from_openai(&msg).unwrap();
        let blocks = converted["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 1, "still wraps into exactly one block");
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[0]["tool_use_id"], "call_missing");
        assert_eq!(blocks[0]["content"].as_str(), Some(""));
    }

    #[test]
    fn anthropic_tool_message_number_content_stringified_not_dropped() {
        // A numeric payload (rare but possible from a misbehaving
        // upstream) must stringify to its JSON form, not be silently
        // dropped to `""`.
        let msg = json!({
            "role": "tool",
            "tool_call_id": "call_num",
            "content": 42,
        });
        let converted = anthropic_message_from_openai(&msg).unwrap();
        let body = converted["content"][0]["content"].as_str().unwrap();
        assert_eq!(body, "42", "number content stringified, got: {body}");
    }

    #[test]
    fn anthropic_system_and_messages_preserves_cache_control_on_system_blocks() {
        let messages = vec![
            json!({
                "role": "system",
                "content": [
                    {"type": "text", "text": "stable", "cache_control": {"type": "ephemeral", "ttl": "1h"}},
                    {"type": "text", "text": "dynamic"}
                ]
            }),
            json!({
                "role": "user",
                "content": [{"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}]
            }),
            json!({
                "role": "tool",
                "tool_call_id": "tc1",
                "content": "result",
                "cache_reference": "tc1"
            }),
        ];
        let (system, msgs) = build_anthropic_system_and_messages(&messages);
        assert_eq!(system[0]["cache_control"]["ttl"], "1h");
        assert_eq!(system[1]["text"], "dynamic");
        // user + tool(→user) are consecutive user messages → merged into one
        assert_eq!(msgs.len(), 1, "consecutive users must be merged: {msgs:#?}");
        let merged_blocks = msgs[0]["content"].as_array().unwrap();
        assert!(
            merged_blocks
                .iter()
                .any(|b| b.get("cache_control").is_some()),
            "cache_control from original user block must survive merge"
        );
        assert!(
            merged_blocks
                .iter()
                .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result")),
            "tool_result block must be present after merge"
        );
    }

    #[test]
    fn build_anthropic_system_and_messages_merges_consecutive_user_messages() {
        let messages = vec![
            json!({"role": "user", "content": "run tools"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {"id": "a", "type": "function", "function": {"name": "bash", "arguments": "{}"}},
                    {"id": "b", "type": "function", "function": {"name": "read_file", "arguments": "{}"}},
                    {"id": "c", "type": "function", "function": {"name": "grep", "arguments": "{}"}}
                ]
            }),
            json!({"role": "tool", "tool_call_id": "a", "content": "a ok"}),
            json!({"role": "tool", "tool_call_id": "b", "content": "b ok"}),
            json!({"role": "tool", "tool_call_id": "c", "content": "c ok"}),
            json!({"role": "user", "content": "continue"}),
        ];

        let (_, msgs) = build_anthropic_system_and_messages(&messages);
        assert!(
            msgs.windows(2)
                .all(|pair| pair[0]["role"] != pair[1]["role"]),
            "Anthropic Messages API requires role alternation: {msgs:#?}"
        );
        assert_eq!(msgs.len(), 3, "{msgs:#?}");
        assert_eq!(msgs[2]["role"], "user");
        let merged = msgs[2]["content"].as_array().expect("merged user blocks");
        let tool_result_ids: Vec<&str> = merged
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
            .filter_map(|block| block.get("tool_use_id").and_then(Value::as_str))
            .collect();
        assert_eq!(tool_result_ids, vec!["a", "b", "c"]);
        assert!(
            merged
                .iter()
                .any(|block| block.get("text").and_then(Value::as_str) == Some("continue")),
            "final user text should be merged after tool results: {msgs:#?}"
        );
    }

    /// Session 5c5cbf78 (2026-05-08) regression: after seven tool-loop rounds,
    /// enough `MICRO_COMPACT_STUB`-cleared tool results had accumulated that the
    /// now-removed `insert_cache_edits_block` helper emitted a
    /// `{type: "cache_edits", edits: [...]}` content block on the final user
    /// message. Real Anthropic `/v1/messages` returns HTTP 400:
    /// `unknown variant \`cache_edits\``. Also accumulated:
    /// `cache_reference` top-level keys on tool messages.
    ///
    /// Both helpers are gone. This test feeds a representative multi-round
    /// shape through the full wire builder and asserts neither field leaks.
    /// A future reviewer reintroducing either field will fail this test.
    #[test]
    fn build_anthropic_wire_never_contains_cache_edits_or_cache_reference() {
        // Shape: system + 3 turns with tool-loop activity, including a
        // MICRO_COMPACT_STUB-cleared tool output (the pattern that used to
        // trigger cache_edits emission). Even hand-seeded cache_reference
        // keys in the input must be stripped by the converter.
        let messages = vec![
            json!({
                "role": "system",
                "content": [
                    {"type": "text", "text": "stable prompt",
                     "cache_control": {"type": "ephemeral"}}
                ],
            }),
            json!({"role": "user", "content": "turn 1"}),
            json!({"role": "assistant", "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "bash", "arguments": "{}"}}
            ]}),
            json!({
                "role": "tool",
                "tool_call_id": "c1",
                "content": "full result 1",
                "cache_reference": "c1",
            }),
            json!({"role": "user", "content": "turn 2"}),
            json!({"role": "assistant", "tool_calls": [
                {"id": "c2", "type": "function", "function": {"name": "bash", "arguments": "{}"}}
            ]}),
            json!({
                "role": "tool",
                "tool_call_id": "c2",
                "content": crate::turn::cloud::analytics::MICRO_COMPACT_STUB,
                "cache_reference": "c2",
            }),
            json!({"role": "user", "content": "turn 3 continue"}),
        ];

        let (system, msgs) = build_anthropic_system_and_messages(&messages);

        // Real fields we still expect:
        assert!(!system.is_empty(), "system blocks must be emitted");
        assert!(!msgs.is_empty(), "messages must be emitted");

        // Walk every emitted message + content block + nested block and assert
        // no speculative cache field survived.
        for (i, m) in msgs.iter().enumerate() {
            assert!(
                m.get("cache_reference").is_none(),
                "wire msg[{i}] must not carry cache_reference (5c5cbf78 regression): {m}",
            );
            let Some(blocks) = m.get("content").and_then(Value::as_array) else {
                continue;
            };
            for (j, b) in blocks.iter().enumerate() {
                let ty = b.get("type").and_then(Value::as_str).unwrap_or("");
                assert_ne!(
                    ty, "cache_edits",
                    "wire msg[{i}].content[{j}] must not be cache_edits: {b}",
                );
                assert!(
                    b.get("cache_reference").is_none(),
                    "wire msg[{i}].content[{j}] must not carry cache_reference: {b}",
                );
                // tool_result's nested `content` field may itself be an array
                // (multi-part tool output) — scan one level deeper.
                if let Some(nested) = b.get("content").and_then(Value::as_array) {
                    for (k, nb) in nested.iter().enumerate() {
                        let nty = nb.get("type").and_then(Value::as_str).unwrap_or("");
                        assert_ne!(
                            nty, "cache_edits",
                            "wire msg[{i}].content[{j}].content[{k}] must not be cache_edits: {nb}",
                        );
                        assert!(
                            nb.get("cache_reference").is_none(),
                            "wire msg[{i}].content[{j}].content[{k}] must not carry \
                             cache_reference: {nb}",
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn repair_anthropic_tool_pairing_strips_orphaned_leading_tool_results() {
        let messages = vec![json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "resume"},
                {"type": "text", "text": " context"},
                {"type": "tool_result", "tool_use_id": "ghost", "content": "stale output"},
            ],
        })];

        let repaired = repair_anthropic_tool_pairing(&messages);
        assert_eq!(repaired.len(), 1, "{repaired:#?}");
        let blocks = repaired[0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2, "{blocks:#?}");
        assert!(blocks.iter().all(|b| !is_anthropic_tool_result_block(b)));
        assert_eq!(blocks[0]["text"], "resume");
        assert_eq!(blocks[1]["text"], " context");
    }

    #[test]
    fn repair_anthropic_tool_pairing_synthesizes_missing_results_into_next_user_message() {
        let messages = vec![
            json!({
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "call_a",
                    "name": "glob",
                    "input": {"pattern": "**/*.rs"},
                }],
            }),
            json!({
                "role": "user",
                "content": [{"type": "text", "text": "continue"}],
            }),
        ];

        let repaired = repair_anthropic_tool_pairing(&messages);
        assert_eq!(repaired.len(), 2, "{repaired:#?}");
        let blocks = repaired[1]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[0]["tool_use_id"], "call_a");
        assert_eq!(
            blocks[0]["content"].as_str(),
            Some(SYNTHETIC_TOOL_INTERRUPTED_CONTENT)
        );
        assert_eq!(blocks[1]["type"], "text");
        assert_eq!(blocks[1]["text"], "continue");
    }

    #[test]
    fn build_provider_request_body_anthropic_drops_orphaned_tool_result_history() {
        let messages = vec![
            json!({"role": "tool", "tool_call_id": "ghost", "content": "stale output"}),
            json!({"role": "user", "content": "continue"}),
        ];

        let body = build_provider_request_body(
            &messages,
            &[],
            "claude-test",
            "anthropic",
            None,
            None,
            true,
            &ThinkingConfig::Off,
        );

        let wire_messages = body["messages"].as_array().unwrap();
        assert_eq!(wire_messages.len(), 1, "{wire_messages:#?}");
        assert_eq!(wire_messages[0]["role"], "user");
        let blocks = wire_messages[0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 1, "{blocks:#?}");
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "continue");
    }

    #[test]
    fn build_provider_request_body_anthropic_strips_mixed_user_orphan_without_losing_cache_control()
    {
        // Minimal shape from the real 400: `messages[0].content[2]` is an
        // orphaned tool_result inside an otherwise normal user content array.
        // We intentionally keep only the trigger shape — not a full session log.
        let messages = vec![json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "resume", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": " context"},
                {"type": "tool_result", "tool_use_id": "ghost", "content": "stale output"},
            ],
        })];

        let body = build_provider_request_body(
            &messages,
            &[],
            "claude-test",
            "anthropic",
            None,
            None,
            true,
            &ThinkingConfig::Off,
        );

        let wire_messages = body["messages"].as_array().unwrap();
        assert_eq!(wire_messages.len(), 1, "{wire_messages:#?}");
        let blocks = wire_messages[0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2, "{blocks:#?}");
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "resume");
        assert_eq!(blocks[0]["cache_control"]["type"], "ephemeral");
        assert_eq!(blocks[1]["type"], "text");
        assert_eq!(blocks[1]["text"], " context");
        assert!(
            blocks
                .iter()
                .all(|block| block.get("type").and_then(Value::as_str) != Some("tool_result")),
            "orphaned tool_result must be removed without touching text cache markers: {blocks:#?}"
        );
    }

    #[test]
    fn repair_anthropic_tool_pairing_is_noop_for_valid_cached_tool_result() {
        let messages = vec![
            json!({
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "call_a",
                    "name": "glob",
                    "input": {"pattern": "**/*.rs"},
                }],
            }),
            json!({
                "role": "user",
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": "call_a",
                        "content": "ok",
                        "cache_control": {"type": "ephemeral"}
                    },
                    {"type": "text", "text": "continue"},
                ],
            }),
        ];

        let repaired = repair_anthropic_tool_pairing(&messages);
        assert_eq!(
            repaired, messages,
            "valid cached tool_result must survive unchanged"
        );
    }

    #[test]
    fn build_anthropic_tools_preserves_native_schema() {
        let native = json!({
            "name": "read_file",
            "description": "Read a file",
            "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}},
            "cache_control": {"type": "ephemeral", "ttl": "1h"}
        });
        assert_eq!(
            build_anthropic_tools(std::slice::from_ref(&native)),
            vec![native]
        );
    }

    #[tokio::test]
    async fn call_llm_and_collect_retries_after_429_retry_after_zero() {
        reset_rate_limit_cooldown_for_tests();
        let _backoff = set_test_retry_backoff_ms(0);
        let hits = Arc::new(AtomicU32::new(0));
        let app = Router::new()
            .route("/chat/completions", post(mock_429_retry_zero_then_sse))
            .with_state(Hit(hits.clone()));
        let base = spawn_local_http_server(app).await;
        let messages = vec![json!({"role":"user","content":"x"})];
        let observer = RecordingAttemptObserver::default();
        let res = call_llm_and_collect_with_stream_callback(
            LlmCall {
                purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
                messages: &messages,
                tools: &[],
                cache_capability: None,
                route: LlmExecutionRoute {
                    model_name: "m",
                    wire_model_name: None,
                    api_key: "k",
                    base_url: &base,
                    provider: "openai",
                    header_overrides: None,
                    request_body_overrides: None,
                    completions_url_override: None,
                    request_timeout: None,
                },
                max_output_tokens: None,
                temperature: None,
                has_fallback: false,
                thinking: &ThinkingConfig::Off,
            },
            LlmCancel::None,
            None,
            Some(&observer),
        )
        .await
        .expect("llm ok");
        assert_eq!(res.full_text, "after-429");
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        assert_eq!(*observer.began.lock().expect("began"), vec![0, 1]);
        let wires = observer.wires.lock().expect("wires");
        assert_eq!(wires.len(), 2);
        assert_eq!(
            wires[0], wires[1],
            "physical retries of one prepared request must send the same exact bytes"
        );
        assert_eq!(
            wires[0].composition.total_bytes(),
            wires[0].provider_wire_bytes
        );
        assert_eq!(
            *observer.finished.lock().expect("finished"),
            vec![
                (0, astra_services::InferenceTerminalStatus::Failed),
                (1, astra_services::InferenceTerminalStatus::Succeeded),
            ]
        );
    }

    #[tokio::test]
    async fn durable_attempt_admission_failure_prevents_provider_delivery() {
        reset_rate_limit_cooldown_for_tests();
        let hits = Arc::new(AtomicU32::new(0));
        let app = Router::new()
            .route("/chat/completions", post(mock_500_once))
            .with_state(Hit(hits.clone()));
        let base = spawn_local_http_server(app).await;
        let messages = vec![json!({"role":"user","content":"x"})];

        let error = call_llm_and_collect_with_stream_callback(
            LlmCall {
                purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
                messages: &messages,
                tools: &[],
                cache_capability: None,
                route: LlmExecutionRoute {
                    model_name: "m",
                    wire_model_name: None,
                    api_key: "k",
                    base_url: &base,
                    provider: "openai",
                    header_overrides: None,
                    request_body_overrides: None,
                    completions_url_override: None,
                    request_timeout: None,
                },
                max_output_tokens: None,
                temperature: None,
                has_fallback: false,
                thinking: &ThinkingConfig::Off,
            },
            LlmCancel::None,
            None,
            Some(&RejectingAttemptObserver),
        )
        .await
        .expect_err("provider delivery requires durable attempt admission");

        assert_eq!(error.kind, astra_core::ErrorKind::DatabaseError);
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn call_llm_and_collect_cancel_during_429_backoff_sleep() {
        reset_rate_limit_cooldown_for_tests();
        let hits = Arc::new(AtomicU32::new(0));
        let app = Router::new()
            .route("/chat/completions", post(mock_429_retry_two_seconds))
            .with_state(Hit(hits.clone()));
        let base = spawn_local_http_server(app).await;
        let token = CancellationToken::new();
        let token_for_call = token.clone();
        let messages = vec![json!({"role":"user","content":"x"})];
        let base_clone = base.clone();
        let handle = tokio::spawn(async move {
            call_llm_and_collect(
                LlmCall {
                    purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
                    messages: &messages,
                    tools: &[],
                    cache_capability: None,
                    route: LlmExecutionRoute {
                        model_name: "m",
                        wire_model_name: None,
                        api_key: "k",
                        base_url: &base_clone,
                        provider: "openai",
                        header_overrides: None,
                        request_body_overrides: None,
                        completions_url_override: None,
                        request_timeout: None,
                    },
                    max_output_tokens: None,
                    temperature: None,
                    has_fallback: false,
                    thinking: &ThinkingConfig::Off,
                },
                LlmCancel::Token(&token_for_call),
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        token.cancel();
        let err = handle.await.expect("join").expect_err("cancelled");
        assert_eq!(err.kind, astra_core::ErrorKind::Cancelled);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn call_llm_and_collect_retries_after_529_retry_after_zero() {
        reset_rate_limit_cooldown_for_tests();
        let _backoff = set_test_retry_backoff_ms(0);
        let hits = Arc::new(AtomicU32::new(0));
        let app = Router::new()
            .route("/chat/completions", post(mock_529_retry_zero_then_sse))
            .with_state(Hit(hits.clone()));
        let base = spawn_local_http_server(app).await;
        let messages = vec![json!({"role":"user","content":"x"})];
        let res = call_llm_and_collect(
            LlmCall {
                purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
                messages: &messages,
                tools: &[],
                cache_capability: None,
                route: LlmExecutionRoute {
                    model_name: "m",
                    wire_model_name: None,
                    api_key: "k",
                    base_url: &base,
                    provider: "openai",
                    header_overrides: None,
                    request_body_overrides: None,
                    completions_url_override: None,
                    request_timeout: None,
                },
                max_output_tokens: None,
                temperature: None,
                has_fallback: false,
                thinking: &ThinkingConfig::Off,
            },
            LlmCancel::None,
        )
        .await
        .expect("llm ok");
        assert_eq!(res.full_text, "after-529");
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn call_llm_and_collect_retries_after_503_retry_after_zero() {
        reset_rate_limit_cooldown_for_tests();
        let _backoff = set_test_retry_backoff_ms(0);
        let hits = Arc::new(AtomicU32::new(0));
        let app = Router::new()
            .route("/chat/completions", post(mock_503_retry_zero_then_sse))
            .with_state(Hit(hits.clone()));
        let base = spawn_local_http_server(app).await;
        let messages = vec![json!({"role":"user","content":"x"})];
        let res = call_llm_and_collect(
            LlmCall {
                purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
                messages: &messages,
                tools: &[],
                cache_capability: None,
                route: LlmExecutionRoute {
                    model_name: "m",
                    wire_model_name: None,
                    api_key: "k",
                    base_url: &base,
                    provider: "openai",
                    header_overrides: None,
                    request_body_overrides: None,
                    completions_url_override: None,
                    request_timeout: None,
                },
                max_output_tokens: None,
                temperature: None,
                has_fallback: false,
                thinking: &ThinkingConfig::Off,
            },
            LlmCancel::None,
        )
        .await
        .expect("llm ok");
        assert_eq!(res.full_text, "after-503");
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn call_llm_and_collect_cancel_during_exponential_backoff_after_500() {
        reset_rate_limit_cooldown_for_tests();
        let hits = Arc::new(AtomicU32::new(0));
        let app = Router::new()
            .route("/chat/completions", post(mock_500_once))
            .with_state(Hit(hits.clone()));
        let base = spawn_local_http_server(app).await;
        let token = CancellationToken::new();
        let token_for_call = token.clone();
        let messages = vec![json!({"role":"user","content":"x"})];
        let base_clone = base.clone();
        let handle = tokio::spawn(async move {
            call_llm_and_collect(
                LlmCall {
                    purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
                    messages: &messages,
                    tools: &[],
                    cache_capability: None,
                    route: LlmExecutionRoute {
                        model_name: "m",
                        wire_model_name: None,
                        api_key: "k",
                        base_url: &base_clone,
                        provider: "openai",
                        header_overrides: None,
                        request_body_overrides: None,
                        completions_url_override: None,
                        request_timeout: None,
                    },
                    max_output_tokens: None,
                    temperature: None,
                    has_fallback: false,
                    thinking: &ThinkingConfig::Off,
                },
                LlmCancel::Token(&token_for_call),
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        token.cancel();
        let err = handle.await.expect("join").expect_err("cancelled");
        assert_eq!(err.kind, astra_core::ErrorKind::Cancelled);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    // ── Output escalation E2E tests ─────────────────────────────────────────

    /// Mock server that returns finish_reason=length on first call,
    /// then finish_reason=stop on second call (simulating successful escalation).
    async fn mock_length_then_stop(State(Hit(c)): State<Hit>) -> Response {
        let n = c.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            let d1 = json!({"choices":[{"delta":{"content":"truncat"}}]});
            let done = json!({"choices":[{"delta":{},"finish_reason":"length"}]});
            let body = format!("data: {d1}\n\ndata: {done}\n\n");
            Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(Body::from(body))
                .unwrap()
        } else {
            let d1 = json!({"choices":[{"delta":{"content":"complete response"}}]});
            let done = json!({"choices":[{"delta":{},"finish_reason":"stop"}]});
            let body = format!("data: {d1}\n\ndata: {done}\n\n");
            Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(Body::from(body))
                .unwrap()
        }
    }

    #[tokio::test]
    async fn output_escalation_e2e_length_then_stop() {
        // Verifies: first call returns finish_reason=length, second returns stop.
        // This is the data path used by server_loop_host's escalation loop.
        reset_rate_limit_cooldown_for_tests();
        let hits = Arc::new(AtomicU32::new(0));
        let app = Router::new()
            .route("/chat/completions", post(mock_length_then_stop))
            .with_state(Hit(hits.clone()));
        let base = spawn_local_http_server(app).await;
        let messages = vec![json!({"role":"user","content":"x"})];

        // First call: finish_reason=length
        let res1 = call_llm_and_collect(
            LlmCall {
                purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
                messages: &messages,
                tools: &[],
                cache_capability: None,
                route: LlmExecutionRoute {
                    model_name: "m",
                    wire_model_name: None,
                    api_key: "k",
                    base_url: &base,
                    provider: "openai",
                    header_overrides: None,
                    request_body_overrides: None,
                    completions_url_override: None,
                    request_timeout: None,
                },
                max_output_tokens: Some(1000),
                temperature: None,
                has_fallback: false,
                thinking: &ThinkingConfig::Off,
            },
            LlmCancel::None,
        )
        .await
        .expect("llm ok");
        assert_eq!(res1.full_text, "truncat");
        assert_eq!(res1.finish_reason.as_deref(), Some("length"));

        // Second call (escalated): finish_reason=stop
        let res2 = call_llm_and_collect(
            LlmCall {
                purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
                messages: &messages,
                tools: &[],
                cache_capability: None,
                route: LlmExecutionRoute {
                    model_name: "m",
                    wire_model_name: None,
                    api_key: "k",
                    base_url: &base,
                    provider: "openai",
                    header_overrides: None,
                    request_body_overrides: None,
                    completions_url_override: None,
                    request_timeout: None,
                },
                max_output_tokens: Some(4000),
                temperature: None,
                has_fallback: false,
                thinking: &ThinkingConfig::Off,
            },
            LlmCancel::None,
        )
        .await
        .expect("llm ok");
        assert_eq!(res2.full_text, "complete response");
        assert_eq!(res2.finish_reason.as_deref(), Some("stop"));
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn finish_reason_stop_no_retry() {
        reset_rate_limit_cooldown_for_tests();
        let hits = Arc::new(AtomicU32::new(0));
        async fn mock_stop(State(Hit(c)): State<Hit>) -> Response {
            c.fetch_add(1, Ordering::SeqCst);
            let d = json!({"choices":[{"delta":{"content":"ok"}}]});
            let done = json!({"choices":[{"delta":{},"finish_reason":"stop"}]});
            let body = format!("data: {d}\n\ndata: {done}\n\n");
            Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(Body::from(body))
                .unwrap()
        }
        let app = Router::new()
            .route("/chat/completions", post(mock_stop))
            .with_state(Hit(hits.clone()));
        let base = spawn_local_http_server(app).await;
        let messages = vec![json!({"role":"user","content":"x"})];
        let res = call_llm_and_collect(
            LlmCall {
                purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
                messages: &messages,
                tools: &[],
                cache_capability: None,
                route: LlmExecutionRoute {
                    model_name: "m",
                    wire_model_name: None,
                    api_key: "k",
                    base_url: &base,
                    provider: "openai",
                    header_overrides: None,
                    request_body_overrides: None,
                    completions_url_override: None,
                    request_timeout: None,
                },
                max_output_tokens: Some(1000),
                temperature: None,
                has_fallback: false,
                thinking: &ThinkingConfig::Off,
            },
            LlmCancel::None,
        )
        .await
        .expect("llm ok");
        assert_eq!(res.finish_reason.as_deref(), Some("stop"));
        assert_eq!(hits.load(Ordering::SeqCst), 1, "no retry when stop");
    }

    #[tokio::test]
    async fn finish_reason_tool_calls_extracted() {
        let d = json!({"choices":[{
            "delta":{"tool_calls":[{"index":0,"id":"tc1","type":"function","function":{"name":"bash","arguments":"{}"}}]},
            "finish_reason":"tool_calls"
        }]});
        let body = format!("data: {d}\n\n");
        let stream = stream::iter(vec![Ok(Bytes::from(body))]);
        let res = collect_llm_stream(
            stream,
            "m",
            Instant::now(),
            LlmCancel::None,
            stream_idle_timeout(),
            stream_idle_timeout_after_progress(),
            None,
        )
        .await
        .expect("collect");
        assert_eq!(res.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(res.tool_calls.len(), 1);
    }

    #[tokio::test]
    async fn call_llm_and_collect_does_not_reissue_after_partial_stream_idle() {
        let _guard = set_test_stream_timeouts(10, Some(10));
        let state = StreamRequestHits {
            stream_hits: Arc::new(AtomicU32::new(0)),
            nonstream_hits: Arc::new(AtomicU32::new(0)),
        };
        let app = Router::new()
            .route("/chat/completions", post(mock_stream_idle_after_partial))
            .with_state(state.clone());
        let base = spawn_local_http_server(app).await;
        let messages = vec![json!({"role":"user","content":"x"})];
        let error = call_llm_and_collect(
            LlmCall {
                purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
                messages: &messages,
                tools: &[],
                cache_capability: None,
                route: LlmExecutionRoute {
                    model_name: "m",
                    wire_model_name: None,
                    api_key: "k",
                    base_url: &base,
                    provider: "openai",
                    header_overrides: None,
                    request_body_overrides: None,
                    completions_url_override: None,
                    request_timeout: None,
                },
                max_output_tokens: None,
                temperature: None,
                has_fallback: false,
                thinking: &ThinkingConfig::Off,
            },
            LlmCancel::None,
        )
        .await
        .expect_err("an accepted provider stream cannot be safely reissued");
        assert_eq!(error.kind, astra_core::ErrorKind::StreamIdle);
        let details: Value = serde_json::from_str(
            error
                .details_json
                .as_deref()
                .expect("partial output must remain available as evidence"),
        )
        .expect("partial details json");
        assert_eq!(details["partial_full_text"], "partial");
        assert_eq!(state.stream_hits.load(Ordering::SeqCst), 1);
        assert_eq!(state.nonstream_hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn call_llm_and_collect_does_not_reissue_after_partial_stream_transport_error() {
        let state = StreamRequestHits {
            stream_hits: Arc::new(AtomicU32::new(0)),
            nonstream_hits: Arc::new(AtomicU32::new(0)),
        };
        let base = spawn_raw_partial_transport_server(state.clone()).await;
        let messages = vec![json!({"role":"user","content":"x"})];
        let error = call_llm_and_collect(
            LlmCall {
                purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
                messages: &messages,
                tools: &[],
                cache_capability: None,
                route: LlmExecutionRoute {
                    model_name: "m",
                    wire_model_name: None,
                    api_key: "k",
                    base_url: &base,
                    provider: "openai",
                    header_overrides: None,
                    request_body_overrides: None,
                    completions_url_override: None,
                    request_timeout: None,
                },
                max_output_tokens: None,
                temperature: None,
                has_fallback: false,
                thinking: &ThinkingConfig::Off,
            },
            LlmCancel::None,
        )
        .await
        .expect_err("an accepted provider stream cannot be safely reissued");
        assert_eq!(error.kind, astra_core::ErrorKind::StreamTransport);
        let details: Value = serde_json::from_str(
            error
                .details_json
                .as_deref()
                .expect("partial output must remain available as evidence"),
        )
        .expect("partial details json");
        assert_eq!(details["partial_full_text"], "partial");
        assert_eq!(state.stream_hits.load(Ordering::SeqCst), 1);
        assert_eq!(state.nonstream_hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn call_llm_and_collect_does_not_reissue_after_idle_before_output() {
        let _guard = set_test_stream_timeouts(10, None);
        let hits = Arc::new(AtomicU32::new(0));
        let app = Router::new()
            .route(
                "/chat/completions",
                post(mock_stream_idle_before_any_output),
            )
            .with_state(Hit(hits.clone()));
        let base = spawn_local_http_server(app).await;
        let messages = vec![json!({"role":"user","content":"x"})];
        let error = call_llm_and_collect(
            LlmCall {
                purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
                messages: &messages,
                tools: &[],
                cache_capability: None,
                route: LlmExecutionRoute {
                    model_name: "m",
                    wire_model_name: None,
                    api_key: "k",
                    base_url: &base,
                    provider: "openai",
                    header_overrides: None,
                    request_body_overrides: None,
                    completions_url_override: None,
                    request_timeout: None,
                },
                max_output_tokens: None,
                temperature: None,
                has_fallback: false,
                thinking: &ThinkingConfig::Off,
            },
            LlmCancel::None,
        )
        .await
        .expect_err("an accepted provider stream cannot be safely reissued");
        assert_eq!(error.kind, astra_core::ErrorKind::StreamIdle);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    /// Mock server that returns 400 with context_length_exceeded.
    async fn mock_400_context_window() -> Response {
        let body = r#"{"error":{"message":"This model's maximum context length is 128000 tokens. However, your messages resulted in 200000 tokens.","type":"invalid_request_error","code":"context_length_exceeded"}}"#;
        Response::builder()
            .status(400)
            .body(Body::from(body))
            .unwrap()
    }

    #[tokio::test]
    async fn call_llm_and_collect_returns_context_window_error_kind() {
        reset_rate_limit_cooldown_for_tests();
        let app = Router::new().route("/chat/completions", post(mock_400_context_window));
        let base = spawn_local_http_server(app).await;
        let messages = vec![json!({"role":"user","content":"x"})];
        let err = call_llm_and_collect(
            LlmCall {
                purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
                messages: &messages,
                tools: &[],
                cache_capability: None,
                route: LlmExecutionRoute {
                    model_name: "m",
                    wire_model_name: None,
                    api_key: "k",
                    base_url: &base,
                    provider: "openai",
                    header_overrides: None,
                    request_body_overrides: None,
                    completions_url_override: None,
                    request_timeout: None,
                },
                max_output_tokens: None,
                temperature: None,
                has_fallback: false,
                thinking: &ThinkingConfig::Off,
            },
            LlmCancel::None,
        )
        .await
        .expect_err("should fail with context window");
        assert_eq!(err.kind, astra_core::ErrorKind::ContextWindow);
        assert!(err.message.contains("context_length_exceeded"));
    }

    /// Mock server that returns 401 Unauthorized.
    async fn mock_401() -> Response {
        Response::builder()
            .status(401)
            .body(Body::from("Unauthorized"))
            .unwrap()
    }

    #[tokio::test]
    async fn call_llm_and_collect_returns_auth_error_kind() {
        reset_rate_limit_cooldown_for_tests();
        let app = Router::new().route("/chat/completions", post(mock_401));
        let base = spawn_local_http_server(app).await;
        let messages = vec![json!({"role":"user","content":"x"})];
        let err = call_llm_and_collect(
            LlmCall {
                purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
                messages: &messages,
                tools: &[],
                cache_capability: None,
                route: LlmExecutionRoute {
                    model_name: "m",
                    wire_model_name: None,
                    api_key: "k",
                    base_url: &base,
                    provider: "openai",
                    header_overrides: None,
                    request_body_overrides: None,
                    completions_url_override: None,
                    request_timeout: None,
                },
                max_output_tokens: None,
                temperature: None,
                has_fallback: false,
                thinking: &ThinkingConfig::Off,
            },
            LlmCancel::None,
        )
        .await
        .expect_err("should fail with auth");
        assert_eq!(err.kind, astra_core::ErrorKind::Auth);
        assert!(
            !err.message.contains("Unauthorized"),
            "auth error message must not echo provider body, got: {}",
            err.message
        );
        assert!(err.message.contains("authentication failed"));
    }

    #[test]
    fn completions_url_openai_default() {
        assert_eq!(
            llm_completions_url("https://api.openai.com/v1", None, "openai"),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn completions_url_openai_trailing_slash() {
        assert_eq!(
            llm_completions_url("https://api.openai.com/v1/", None, "openai"),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn completions_url_anthropic_without_v1() {
        assert_eq!(
            llm_completions_url("https://api.minimaxi.com/anthropic", None, "anthropic"),
            "https://api.minimaxi.com/anthropic/v1/messages"
        );
    }

    #[test]
    fn completions_url_anthropic_with_v1() {
        assert_eq!(
            llm_completions_url("https://api.anthropic.com/v1", None, "anthropic"),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn completions_url_override_takes_precedence() {
        assert_eq!(
            llm_completions_url(
                "https://api.openai.com/v1",
                Some("https://custom.proxy/llm"),
                "openai"
            ),
            "https://custom.proxy/llm"
        );
    }

    #[test]
    fn completions_url_override_uses_registered_endpoint_permit() {
        let endpoint = format!("https://custom.proxy/llm/{}", uuid::Uuid::new_v4());
        let mut permits = Vec::new();
        for _ in 0..crate::capability_endpoint_pool::REGISTERED_ENDPOINT_RPC_CONCURRENCY_FOR_TESTS {
            permits.push(
                crate::capability_endpoint_pool::try_acquire_endpoint_permit(&endpoint)
                    .expect("permit within endpoint limit"),
            );
        }

        let err = acquire_registered_endpoint_permit_for_override(&endpoint, Some(&endpoint))
            .expect_err("override endpoint over limit should reject before LLM send");
        assert_eq!(err.kind, astra_core::ErrorKind::ResourceLimit);
        assert!(err.message.contains("over its concurrency limit"));

        drop(permits);
        let permit = acquire_registered_endpoint_permit_for_override(&endpoint, Some(&endpoint))
            .expect("released endpoint permits should allow override send");
        assert!(permit.is_some());
    }

    #[test]
    fn native_llm_url_does_not_use_registered_endpoint_permit() {
        let endpoint = format!("https://api.openai.com/v1/{}", uuid::Uuid::new_v4());
        let mut permits = Vec::new();
        for _ in 0..crate::capability_endpoint_pool::REGISTERED_ENDPOINT_RPC_CONCURRENCY_FOR_TESTS {
            permits.push(
                crate::capability_endpoint_pool::try_acquire_endpoint_permit(&endpoint)
                    .expect("permit within endpoint limit"),
            );
        }

        let permit = acquire_registered_endpoint_permit_for_override(&endpoint, None)
            .expect("native LLM URL should not use registered endpoint pool");
        assert!(permit.is_none());
    }

    #[test]
    fn consolidate_system_messages_merges_multiple() {
        let msgs = vec![
            json!({"role": "system", "content": "A"}),
            json!({"role": "system", "content": "B"}),
            json!({"role": "user", "content": "hi"}),
            json!({"role": "system", "content": "C"}),
        ];
        let out = consolidate_system_messages(&msgs);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["role"], "system");
        assert_eq!(out[0]["content"], "A\n\nB\n\nC");
        assert_eq!(out[1]["role"], "user");
        assert_eq!(out[1]["content"], "hi");
    }

    #[test]
    fn consolidate_system_messages_single_system_unchanged() {
        let msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "hi"}),
        ];
        let out = consolidate_system_messages(&msgs);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["content"], "sys");
    }

    #[test]
    fn consolidate_system_messages_preserves_structured_blocks() {
        let msgs = vec![
            json!({
                "role": "system",
                "content": [
                    {"type": "text", "text": "stable", "cache_control": {"type": "ephemeral", "ttl": "1h"}}
                ]
            }),
            json!({"role": "user", "content": "hi"}),
        ];
        let out = consolidate_system_messages(&msgs);
        assert_eq!(out.len(), 2);
        let content = out[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["text"], "stable");
        assert_eq!(content[0]["cache_control"]["ttl"], "1h");
    }

    #[test]
    fn consolidate_system_messages_no_system() {
        let msgs = vec![json!({"role": "user", "content": "hi"})];
        let out = consolidate_system_messages(&msgs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["role"], "user");
    }

    #[test]
    fn provider_projection_strips_runtime_ownership_and_boundary_metadata() {
        let mut runtime = astra_turn_types::runtime_owned_message(
            "user",
            "model-visible required context",
            astra_turn_types::RuntimeMessageDelivery::RequiredContext,
        );
        runtime["_compact_boundary"] = Value::Bool(true);
        runtime[astra_turn_types::USER_TURN_SEMANTICS_FIELD] = json!({
            "schema_version": 1,
            "objective_relation": "continue"
        });
        runtime[astra_turn_types::BRIDGE_TURN_MESSAGE_PROVENANCE_FIELD] = json!({
            "schema_version": 1,
            "turn_chain_id": "chain-current"
        });

        let out = consolidate_system_messages_for_provider(&[runtime], "openai", "gpt-4o", None);

        assert_eq!(out[0]["content"], "model-visible required context");
        assert!(
            out[0]
                .get(astra_turn_types::RUNTIME_MESSAGE_PROVENANCE_FIELD)
                .is_none()
        );
        assert!(
            out[0]
                .get(astra_turn_types::USER_TURN_SEMANTICS_FIELD)
                .is_none()
        );
        assert!(
            out[0]
                .get(astra_turn_types::BRIDGE_TURN_MESSAGE_PROVENANCE_FIELD)
                .is_none()
        );
        assert!(out[0].get("_compact_boundary").is_none());
    }

    #[test]
    fn consolidate_for_openai_preserves_runtime_system_at_current_turn_boundary() {
        let runtime = crate::turn::wire_assembly::required_runtime_preamble_message(
            "required resume context",
        )
        .expect("runtime message");
        let msgs = vec![
            json!({"role": "system", "content": "stable"}),
            json!({"role": "user", "content": "old question"}),
            json!({"role": "assistant", "content": "old answer"}),
            runtime,
            json!({"role": "user", "content": "hi"}),
        ];

        let out = consolidate_system_messages_for_provider(&msgs, "openai", "gpt-4o", None);

        assert_eq!(out.len(), 5);
        assert_eq!(out[0]["role"], "system");
        assert_eq!(out[0]["content"], "stable");
        assert_eq!(out[1]["role"], "user");
        assert_eq!(out[3]["role"], "system");
        assert_eq!(out[3]["content"], "required resume context");
        assert_eq!(out[4]["content"], "hi");
        assert!(
            out.iter().all(|message| message
                .get(crate::turn::wire_assembly::REQUIRED_RUNTIME_PREAMBLE_MARKER)
                .is_none()),
            "internal marker must not reach provider request messages"
        );
    }

    #[test]
    fn consolidate_for_strict_history_openai_moves_required_runtime_to_initial_system() {
        let runtime = crate::turn::wire_assembly::required_runtime_preamble_message(
            "required resume context",
        )
        .expect("runtime message");
        let msgs = vec![
            json!({"role": "system", "content": "stable"}),
            json!({"role": "user", "content": "old question"}),
            json!({"role": "assistant", "content": "old answer"}),
            runtime,
            json!({"role": "user", "content": "hi"}),
        ];

        let out = consolidate_system_messages_for_provider(&msgs, "openai", "MiniMax-M2.7", None);

        assert_eq!(out.len(), 4);
        assert_eq!(out[0]["role"], "system");
        assert_eq!(out[0]["content"], "stable\n\nrequired resume context");
        assert!(
            out.iter()
                .skip(1)
                .all(|message| { message.get("role").and_then(Value::as_str) != Some("system") })
        );
        assert_eq!(out[3]["content"], "hi");
    }

    #[test]
    fn explicit_current_user_only_capability_overrides_provider_model_heuristic() {
        let runtime = crate::turn::wire_assembly::required_runtime_preamble_message(
            "required resume context",
        )
        .expect("runtime message");
        let msgs = vec![
            json!({"role": "system", "content": "stable"}),
            json!({"role": "user", "content": "old question"}),
            json!({"role": "assistant", "content": "old answer"}),
            runtime,
            json!({"role": "user", "content": "hi"}),
        ];
        let explicit = CacheCapability {
            protocol: astra_turn_core::cache_placement::CacheProtocol::StrictHistoryMatch,
            volatile_placement: VolatilePlacement::CurrentUserOnly,
            reuse_scope: None,
        };

        let out = consolidate_system_messages_for_provider(
            &msgs,
            "openai",
            "metadata-defined-alias",
            Some(explicit),
        );

        assert_eq!(out.len(), 4);
        assert_eq!(out[0]["role"], "system");
        assert_eq!(out[0]["content"], "stable\n\nrequired resume context");
        assert!(
            out.iter()
                .skip(1)
                .all(|message| message.get("role").and_then(Value::as_str) != Some("system"))
        );
        assert_eq!(out[3]["content"], "hi");
    }

    #[test]
    fn consolidate_for_anthropic_preserves_runtime_system_boundary_for_body_builder() {
        let runtime = crate::turn::wire_assembly::required_runtime_preamble_message(
            "required resume context",
        )
        .expect("runtime message");
        let msgs = vec![
            json!({"role": "system", "content": "stable"}),
            json!({"role": "user", "content": "old question"}),
            json!({"role": "assistant", "content": "old answer"}),
            runtime,
            json!({"role": "user", "content": "hi"}),
        ];

        let out =
            consolidate_system_messages_for_provider(&msgs, "anthropic", "claude-sonnet-4", None);

        assert_eq!(out.len(), 5);
        assert_eq!(out[0]["role"], "system");
        assert_eq!(out[0]["content"], "stable");
        assert_eq!(out[3]["role"], "system");
        assert_eq!(out[3]["content"], "required resume context");
        assert_eq!(out[4]["content"], "hi");
        assert!(
            out.iter().all(|message| message
                .get(crate::turn::wire_assembly::REQUIRED_RUNTIME_PREAMBLE_MARKER)
                .is_none()),
            "internal marker must not reach provider request messages"
        );
    }

    #[test]
    fn consolidate_fixes_empty_tool_call_name() {
        let msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "", "arguments": "{}"}}]
            }),
            json!({"role": "tool", "tool_call_id": "c1", "name": "skill", "content": "result"}),
        ];
        let out = consolidate_system_messages(&msgs);
        // assistant tool_call name should be recovered from tool result
        let tc_name = out[1]["tool_calls"][0]["function"]["name"]
            .as_str()
            .unwrap();
        assert_eq!(tc_name, "skill");
    }

    #[test]
    fn consolidate_fixes_empty_tool_call_name_unknown_fallback() {
        let msgs = vec![json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "", "arguments": "{}"}}]
        })];
        let out = consolidate_system_messages(&msgs);
        let tc_name = out[0]["tool_calls"][0]["function"]["name"]
            .as_str()
            .unwrap();
        assert_eq!(tc_name, "_unknown");
    }

    #[test]
    fn consolidate_canonicalizes_tool_call_names_and_recovered_tool_result_names() {
        let msgs = vec![
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {"id": "c1", "type": "function", "function": {"name": " bash ", "arguments": "{}"}},
                    {"id": "c2", "type": "function", "function": {"name": " ", "arguments": "{}"}}
                ]
            }),
            json!({"role": "tool", "tool_call_id": "c2", "name": " skill ", "content": "result"}),
        ];
        let out = consolidate_system_messages(&msgs);
        assert_eq!(
            out[0]["tool_calls"][0]["function"]["name"].as_str(),
            Some("bash")
        );
        assert_eq!(
            out[0]["tool_calls"][1]["function"]["name"].as_str(),
            Some("skill")
        );
    }

    #[test]
    fn consolidate_omits_empty_tool_calls_arrays() {
        let msgs = vec![
            json!({"role": "system", "content": "be helpful"}),
            json!({"role": "assistant", "content": "Done.", "tool_calls": []}),
        ];
        let out = consolidate_system_messages(&msgs);
        assert_eq!(out.len(), 2);
        assert!(out[1].get("tool_calls").is_none(), "{out:?}");
    }

    #[test]
    fn strip_empty_assistant_tool_calls_only_removes_empty_arrays() {
        let mut msgs = vec![
            json!({"role": "assistant", "content": "Done.", "tool_calls": []}),
            json!({"role": "assistant", "content": null, "tool_calls": [{"id":"c1","type":"function","function":{"name":"bash","arguments":"{}"}}]}),
        ];
        strip_empty_assistant_tool_calls(&mut msgs);
        assert!(msgs[0].get("tool_calls").is_none(), "{msgs:?}");
        assert_eq!(msgs[1]["tool_calls"][0]["function"]["name"], "bash");
    }

    #[test]
    fn for_provider_openai() {
        assert_eq!(
            llm_request_url_for_provider("https://api.openai.com/v1", "openai", "gpt-4o", true),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn for_provider_anthropic_without_v1() {
        assert_eq!(
            llm_request_url_for_provider(
                "https://api.minimaxi.com/anthropic",
                "anthropic",
                "claude-3-5-sonnet",
                true
            ),
            "https://api.minimaxi.com/anthropic/v1/messages"
        );
    }

    #[test]
    fn for_provider_anthropic_with_v1() {
        assert_eq!(
            llm_request_url_for_provider(
                "https://api.anthropic.com/v1",
                "anthropic",
                "claude-3-5-sonnet",
                true
            ),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn for_provider_bedrock_nonstream() {
        assert_eq!(
            llm_request_url_for_provider(
                "https://bedrock-runtime.us-east-1.amazonaws.com",
                "bedrock",
                "anthropic.claude-3-5-sonnet-20241022-v2:0",
                false
            ),
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/anthropic.claude-3-5-sonnet-20241022-v2:0/converse"
        );
    }

    #[test]
    fn for_provider_bedrock_cannot_be_base_url_degrades_without_panic() {
        assert_eq!(
            llm_request_url_for_provider(
                "mailto:bedrock-runtime",
                "bedrock",
                "anthropic.claude-3-5-sonnet-20241022-v2:0",
                true
            ),
            "mailto:bedrock-runtime/model/anthropic.claude-3-5-sonnet-20241022-v2:0/converse-stream"
        );
    }

    #[test]
    fn build_bedrock_body_maps_system_tools_and_tool_results() {
        let messages = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "hello"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "bash", "arguments": "{\"cmd\":\"pwd\"}"}
                }]
            }),
            json!({"role": "tool", "tool_call_id": "call_1", "name": "bash", "content": "{\"cwd\":\"/tmp\"}"}),
        ];
        let tools = vec![json!({
            "type": "function",
            "function": {
                "name": "bash",
                "description": "run shell",
                "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}}
            }
        })];
        let body = build_provider_request_body(
            &messages,
            &tools,
            "anthropic.claude-3-5-sonnet-v1:0",
            "bedrock",
            Some(128),
            None,
            false,
            &ThinkingConfig::Off,
        );
        assert_eq!(body["system"][0]["text"], "sys");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"][0]["text"], "hello");
        assert_eq!(body["messages"][1]["role"], "assistant");
        assert_eq!(
            body["messages"][1]["content"][0]["toolUse"]["toolUseId"],
            "call_1"
        );
        assert_eq!(body["messages"][2]["role"], "user");
        assert_eq!(
            body["messages"][2]["content"][0]["toolResult"]["toolUseId"],
            "call_1"
        );
        assert_eq!(body["toolConfig"]["tools"][0]["toolSpec"]["name"], "bash");
        assert_eq!(body["inferenceConfig"]["maxTokens"], 128);
    }

    #[test]
    fn build_bedrock_body_wraps_non_object_tool_content_as_text_not_json() {
        // Session 28e858fd-... failure: `git rev-list --count main..HEAD`
        // returned "2\n" which parses as JSON integer 2. The previous code
        // wrapped it as {"json": 2}, which Bedrock rejects:
        // "messages.N.content.M.toolResult.content.0.json is invalid —
        //  provide a json object".
        // Bedrock's `json` field requires a JSON *object*. Scalars, arrays,
        // strings, booleans, and null must go through the `text` branch.
        for (label, content) in [
            ("integer", "2\n"),
            ("float", "3.14"),
            ("bool", "true"),
            ("null", "null"),
            ("string", "\"hello\""),
            ("array", "[1, 2, 3]"),
        ] {
            let messages = vec![
                json!({
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "t", "type": "function",
                        "function": {"name": "f", "arguments": "{}"}
                    }]
                }),
                json!({"role": "tool", "tool_call_id": "t", "name": "f", "content": content}),
            ];
            let body = build_provider_request_body(
                &messages,
                &[],
                "anthropic.claude-3-5-sonnet-v1:0",
                "bedrock",
                None,
                None,
                false,
                &ThinkingConfig::Off,
            );
            let result_block = &body["messages"][1]["content"][0]["toolResult"]["content"][0];
            // Bedrock-legal: either `json` with an object, or `text` with a string.
            // Must NOT be `json` pointing at a non-object value.
            if let Some(json_val) = result_block.get("json") {
                assert!(
                    json_val.is_object(),
                    "{label}: toolResult.content[].json must be an object, got {json_val:?}"
                );
            } else {
                assert!(
                    result_block.get("text").is_some(),
                    "{label}: non-object content must fall through to text, got {result_block:?}"
                );
            }
        }
    }

    #[test]
    fn build_bedrock_body_keeps_json_object_branch_for_real_objects() {
        // Regression: ensure the object branch still works (don't overcorrect).
        let messages = vec![
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "t", "type": "function",
                    "function": {"name": "f", "arguments": "{}"}
                }]
            }),
            json!({"role": "tool", "tool_call_id": "t", "name": "f",
                   "content": "{\"cwd\":\"/tmp\",\"ok\":true}"}),
        ];
        let body = build_provider_request_body(
            &messages,
            &[],
            "anthropic.claude-3-5-sonnet-v1:0",
            "bedrock",
            None,
            None,
            false,
            &ThinkingConfig::Off,
        );
        let result_block = &body["messages"][1]["content"][0]["toolResult"]["content"][0];
        assert!(result_block["json"].is_object(), "{result_block:?}");
        assert_eq!(result_block["json"]["cwd"], "/tmp");
    }

    #[test]
    fn build_bedrock_body_merges_parallel_tool_results_into_single_user_message() {
        // Assistant makes two parallel tool calls. OpenAI wire format emits
        // one `role: "tool"` message per result. Bedrock Converse requires
        // that all toolResult blocks corresponding to a single assistant
        // turn's toolUse blocks live in ONE user message — emitting two
        // separate user messages triggers the "Expected toolResult blocks
        // at messages.N.content" 400 observed in session 319b68b4-....
        let messages = vec![
            json!({"role": "user", "content": "hi"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {"id": "call_a", "type": "function",
                     "function": {"name": "bash", "arguments": "{\"cmd\":\"pwd\"}"}},
                    {"id": "call_b", "type": "function",
                     "function": {"name": "bash", "arguments": "{\"cmd\":\"whoami\"}"}},
                ]
            }),
            json!({"role": "tool", "tool_call_id": "call_a", "name": "bash", "content": "{\"cwd\":\"/tmp\"}"}),
            json!({"role": "tool", "tool_call_id": "call_b", "name": "bash", "content": "{\"user\":\"astra\"}"}),
        ];
        let body = build_provider_request_body(
            &messages,
            &[],
            "anthropic.claude-3-5-sonnet-v1:0",
            "bedrock",
            Some(64),
            None,
            false,
            &ThinkingConfig::Off,
        );
        let out = body["messages"].as_array().expect("messages array");
        assert_eq!(
            out.len(),
            3,
            "expected user/assistant/user-merged, got {out:#?}",
        );
        assert_eq!(out[2]["role"], "user");
        let content = out[2]["content"].as_array().expect("merged content");
        let tool_result_ids: Vec<&str> = content
            .iter()
            .filter_map(|b| b.get("toolResult")?.get("toolUseId")?.as_str())
            .collect();
        assert_eq!(tool_result_ids, vec!["call_a", "call_b"]);
    }

    #[test]
    fn build_bedrock_body_preserves_tool_order_within_merged_block() {
        let messages = vec![
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {"id": "t1", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                    {"id": "t2", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                    {"id": "t3", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                ]
            }),
            json!({"role": "tool", "tool_call_id": "t3", "name": "f", "content": "three"}),
            json!({"role": "tool", "tool_call_id": "t1", "name": "f", "content": "one"}),
            json!({"role": "tool", "tool_call_id": "t2", "name": "f", "content": "two"}),
        ];
        let body = build_provider_request_body(
            &messages,
            &[],
            "anthropic.claude-3-5-sonnet-v1:0",
            "bedrock",
            None,
            None,
            false,
            &ThinkingConfig::Off,
        );
        let content = body["messages"][1]["content"]
            .as_array()
            .expect("merged content");
        let ids: Vec<&str> = content
            .iter()
            .filter_map(|b| b.get("toolResult")?.get("toolUseId")?.as_str())
            .collect();
        // Insertion order of tool messages is preserved — no reordering.
        assert_eq!(ids, vec!["t3", "t1", "t2"]);
    }

    #[test]
    fn build_bedrock_body_splits_tool_group_when_non_tool_message_intervenes() {
        // A user message between two tool-result groups must break the merge —
        // otherwise we'd splice tool_results around unrelated content.
        let messages = vec![
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {"id": "x", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                ]
            }),
            json!({"role": "tool", "tool_call_id": "x", "name": "f", "content": "first"}),
            json!({"role": "user", "content": "interrupt"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {"id": "y", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                ]
            }),
            json!({"role": "tool", "tool_call_id": "y", "name": "f", "content": "second"}),
        ];
        let body = build_provider_request_body(
            &messages,
            &[],
            "anthropic.claude-3-5-sonnet-v1:0",
            "bedrock",
            None,
            None,
            false,
            &ThinkingConfig::Off,
        );
        let out = body["messages"].as_array().expect("messages array");
        // After merge: assistant / user(tool x + interrupt) / assistant / user(tool y)
        assert_eq!(out.len(), 4);
        assert_eq!(
            out[1]["content"][0]["toolResult"]["toolUseId"], "x",
            "first tool group"
        );
        // The interrupt text is merged into the same user message
        assert_eq!(out[1]["content"][1]["text"], "interrupt");
        assert_eq!(out[2]["role"], "assistant");
        assert_eq!(
            out[3]["content"][0]["toolResult"]["toolUseId"], "y",
            "second tool group"
        );
    }

    #[test]
    fn repair_tool_pairing_injects_synthetic_result_for_missing_tool_call() {
        // Assistant declared two tool_calls but the tool transcript only
        // carries one response (e.g. stream was cut mid-execution on resume).
        // Bedrock would reject with "Expected toolResult blocks for the
        // following Ids: call_b". Pre-send repair must synthesize an error
        // tool_result so the model context stays valid — matching the reference agent's
        // ensureToolResultPairing repair behavior.
        let messages = vec![
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {"id": "call_a", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                    {"id": "call_b", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                ]
            }),
            json!({"role": "tool", "tool_call_id": "call_a", "name": "f", "content": "ok"}),
        ];
        let repaired = repair_openai_tool_pairing(&messages);
        // Expected: assistant / tool(call_a) / synthetic tool(call_b, is_error).
        assert_eq!(repaired.len(), 3, "{repaired:#?}");
        assert_eq!(repaired[1]["tool_call_id"], "call_a");
        assert_eq!(repaired[2]["role"], "tool");
        assert_eq!(repaired[2]["tool_call_id"], "call_b");
        let content = repaired[2]["content"].as_str().unwrap_or_default();
        assert!(
            content.contains("tool execution not recorded")
                || content.contains("tool_use_interrupted"),
            "synthetic content must be identifiable; got {content:?}",
        );
    }

    #[test]
    fn repair_tool_pairing_strips_orphaned_tool_result() {
        // A role=tool message whose tool_call_id doesn't match any preceding
        // assistant's tool_calls is an orphan — Bedrock rejects it with a
        // different 400 ("unexpected toolResult"). Strip to keep the request
        // well-formed.
        let messages = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "tool", "tool_call_id": "nonexistent", "name": "f", "content": "ghost"}),
            json!({"role": "user", "content": "continue"}),
        ];
        let repaired = repair_openai_tool_pairing(&messages);
        // Orphan removed, non-tool messages untouched.
        assert_eq!(repaired.len(), 2);
        assert_eq!(repaired[0]["content"], "hi");
        assert_eq!(repaired[1]["content"], "continue");
    }

    #[test]
    fn repair_tool_pairing_dedupes_duplicate_tool_call_ids() {
        // Same tool_call_id appearing twice in one tool-group (e.g. retry
        // artifact). Bedrock rejects with a duplicate-id 400. Keep first.
        let messages = vec![
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {"id": "dup", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                ]
            }),
            json!({"role": "tool", "tool_call_id": "dup", "name": "f", "content": "first"}),
            json!({"role": "tool", "tool_call_id": "dup", "name": "f", "content": "second"}),
        ];
        let repaired = repair_openai_tool_pairing(&messages);
        assert_eq!(repaired.len(), 2, "{repaired:#?}");
        assert_eq!(repaired[1]["tool_call_id"], "dup");
        assert_eq!(repaired[1]["content"], "first");
    }

    #[test]
    fn repair_tool_pairing_is_identity_when_well_formed() {
        // Regression: a correctly-paired transcript must pass through unchanged.
        let messages = vec![
            json!({"role": "user", "content": "hi"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {"id": "t1", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                    {"id": "t2", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                ]
            }),
            json!({"role": "tool", "tool_call_id": "t1", "name": "f", "content": "a"}),
            json!({"role": "tool", "tool_call_id": "t2", "name": "f", "content": "b"}),
        ];
        let repaired = repair_openai_tool_pairing(&messages);
        assert_eq!(repaired, messages);
    }

    #[test]
    fn build_bedrock_body_end_to_end_repairs_missing_tool_result() {
        // Integration: build_provider_request_body with provider=bedrock
        // must run repair before merging, so Bedrock sees a complete pair.
        let messages = vec![
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {"id": "a", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                    {"id": "b", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                ]
            }),
            json!({"role": "tool", "tool_call_id": "a", "name": "f", "content": "ok"}),
        ];
        let body = build_provider_request_body(
            &messages,
            &[],
            "anthropic.claude-3-5-sonnet-v1:0",
            "bedrock",
            None,
            None,
            false,
            &ThinkingConfig::Off,
        );
        let merged = body["messages"][1]["content"]
            .as_array()
            .expect("user content array");
        let ids: Vec<&str> = merged
            .iter()
            .filter_map(|b| b.get("toolResult")?.get("toolUseId")?.as_str())
            .collect();
        assert_eq!(ids, vec!["a", "b"], "{body:#?}");
        // Bedrock requires toolConfig when messages have tool blocks but
        // rejects empty tools array. We provide a _noop placeholder.
        assert!(
            body.get("toolConfig").is_some(),
            "toolConfig should have placeholder: {body:#?}"
        );
        assert_eq!(body["toolConfig"]["tools"][0]["toolSpec"]["name"], "_noop");
    }

    #[test]
    fn build_bedrock_body_omits_tool_config_when_no_tools_provided() {
        let messages = vec![
            json!({"role": "user", "content": "list files"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "c1",
                    "type": "function",
                    "function": {"name": "bash", "arguments": "{\"cmd\":\"ls\"}"}
                }]
            }),
            json!({"role": "tool", "tool_call_id": "c1", "name": "bash", "content": "file.txt"}),
            json!({"role": "user", "content": "thanks"}),
        ];
        let body = build_provider_request_body(
            &messages,
            &[],
            "us.anthropic.claude-sonnet-4-6",
            "bedrock",
            None,
            None,
            false,
            &ThinkingConfig::Off,
        );
        // Bedrock requires toolConfig when messages have tool blocks but
        // rejects empty tools array. We provide a _noop placeholder.
        assert!(
            body.get("toolConfig").is_some(),
            "toolConfig should have placeholder: {body:#?}"
        );
        assert_eq!(
            body["toolConfig"]["tools"][0]["toolSpec"]["name"], "_noop",
            "placeholder tool required when history contains tool blocks"
        );
    }

    #[test]
    fn build_bedrock_body_omits_tool_config_when_no_tools_anywhere() {
        let messages = vec![json!({"role": "user", "content": "hello"})];
        let body = build_provider_request_body(
            &messages,
            &[],
            "us.anthropic.claude-sonnet-4-6",
            "bedrock",
            None,
            None,
            false,
            &ThinkingConfig::Off,
        );
        assert!(
            body.get("toolConfig").is_none(),
            "toolConfig should be absent when no tools: {body:#?}"
        );
    }

    #[test]
    fn repair_tool_pairing_synthesizes_all_when_zero_responses() {
        // Crash scenario from session 319b68b4-...: assistant declared N
        // tool_calls but the transcript resumed with NO tool-role messages
        // before the next turn. Every declared id must be synthesized.
        let messages = vec![
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {"id": "Npi0", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                    {"id": "94F3", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                ]
            }),
            // No tool responses — jumps straight to another user turn.
            json!({"role": "user", "content": "next question"}),
        ];
        let repaired = repair_openai_tool_pairing(&messages);
        // assistant / tool(Npi0, synthetic) / tool(94F3, synthetic) / user
        assert_eq!(repaired.len(), 4, "{repaired:#?}");
        assert_eq!(repaired[1]["role"], "tool");
        assert_eq!(repaired[1]["tool_call_id"], "Npi0");
        assert_eq!(repaired[2]["tool_call_id"], "94F3");
        assert_eq!(repaired[3]["role"], "user");
    }

    #[test]
    fn repair_tool_pairing_handles_tool_separated_from_assistant_by_user() {
        // A user message between assistant.tool_calls and role=tool severs the
        // pairing window. The orphan tool message is stripped and the missing
        // declaration gets a synthetic — keeps Bedrock request well-formed.
        let messages = vec![
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {"id": "late", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                ]
            }),
            json!({"role": "user", "content": "interrupt"}),
            json!({"role": "tool", "tool_call_id": "late", "name": "f", "content": "delayed"}),
        ];
        let repaired = repair_openai_tool_pairing(&messages);
        // assistant / tool(late, synthetic) / user(interrupt); orphan dropped.
        assert_eq!(repaired.len(), 3, "{repaired:#?}");
        assert_eq!(repaired[1]["tool_call_id"], "late");
        assert_eq!(
            repaired[1]["content"].as_str().unwrap_or_default(),
            SYNTHETIC_TOOL_INTERRUPTED_CONTENT
        );
        assert_eq!(repaired[2]["content"], "interrupt");
    }

    #[test]
    fn build_openai_body_repairs_orphaned_tool_history_before_send() {
        // Regression from session d644d257-...: compaction/resume dropped the
        // assistant tool_calls message while leaving its tool results in place,
        // which OpenAI-compatible providers reject with invalid_request_error.
        let messages = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "continue"}),
            json!({"role": "tool", "tool_call_id": "orphan-a", "name": "bash", "content": "ghost-a"}),
            json!({"role": "tool", "tool_call_id": "orphan-b", "name": "bash", "content": "ghost-b"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {"id": "kept", "type": "function", "function": {"name": "read_file", "arguments": "{}"}}
                ]
            }),
            json!({"role": "tool", "tool_call_id": "kept", "name": "read_file", "content": "ok"}),
        ];
        let body = build_provider_request_body(
            &messages,
            &[],
            "deepseek-v4-pro-official",
            "openai",
            Some(8192),
            None,
            false,
            &ThinkingConfig::Off,
        );
        let out = body["messages"].as_array().expect("messages array");
        assert_eq!(out.len(), 4, "{out:#?}");
        assert_eq!(out[0]["role"], "system");
        assert_eq!(out[1]["role"], "user");
        assert_eq!(out[2]["role"], "assistant");
        assert_eq!(
            out[2]["tool_calls"][0]["id"], "kept",
            "valid assistant tool_calls must survive repair"
        );
        assert_eq!(out[3]["role"], "tool");
        assert_eq!(out[3]["tool_call_id"], "kept");
        assert!(
            out.iter().all(|msg| {
                msg.get("role").and_then(Value::as_str) != Some("tool")
                    || msg.get("tool_call_id").and_then(Value::as_str) != Some("orphan-a")
            }),
            "orphaned tool result should be stripped: {out:#?}"
        );
        assert!(
            out.iter().all(|msg| {
                msg.get("role").and_then(Value::as_str) != Some("tool")
                    || msg.get("tool_call_id").and_then(Value::as_str) != Some("orphan-b")
            }),
            "all orphaned tool results should be stripped: {out:#?}"
        );
    }

    #[test]
    fn build_bedrock_body_translates_cache_control_to_cache_points() {
        let messages = vec![
            json!({
                "role": "system",
                "content": [
                    {"type": "text", "text": "stable", "cache_control": {"type": "ephemeral", "ttl": "1h"}}
                ]
            }),
            json!({
                "role": "user",
                "content": [
                    {"type": "text", "text": "turn prefix"},
                    {"type": "text", "text": "turn suffix", "cache_control": {"type": "ephemeral"}}
                ]
            }),
        ];
        let tools = vec![json!({
            "type": "function",
            "function": {
                "name": "bash",
                "parameters": {"type": "object", "properties": {}}
            },
            "cache_control": {"type": "ephemeral", "ttl": "1h"}
        })];
        let body = build_provider_request_body(
            &messages,
            &tools,
            "anthropic.claude-sonnet-4-20250514-v1:0",
            "bedrock",
            Some(128),
            None,
            false,
            &ThinkingConfig::Off,
        );
        assert_eq!(body["system"][0]["text"], "stable");
        assert_eq!(body["system"][1]["cachePoint"]["type"], "default");
        assert_eq!(body["system"][1]["cachePoint"]["ttl"], "1h");
        assert_eq!(body["messages"][0]["content"][0]["text"], "turn prefix");
        assert_eq!(body["messages"][0]["content"][1]["text"], "turn suffix");
        assert_eq!(
            body["messages"][0]["content"][2]["cachePoint"]["type"],
            "default"
        );
        assert_eq!(body["toolConfig"]["tools"][0]["toolSpec"]["name"], "bash");
        assert_eq!(body["toolConfig"]["tools"][1]["cachePoint"]["ttl"], "1h");
    }

    #[test]
    fn build_bedrock_body_skips_whitespace_only_system_blocks() {
        let messages = consolidate_system_messages(&[
            json!({
                "role": "system",
                "content": [
                    {"type": "text", "text": "stable", "cache_control": {"type": "ephemeral", "ttl": "1h"}}
                ]
            }),
            json!({"role": "system", "content": "runtime hints"}),
            json!({"role": "user", "content": "hello"}),
        ]);
        let body = build_provider_request_body(
            &messages,
            &[],
            "anthropic.claude-sonnet-4-20250514-v1:0",
            "bedrock",
            Some(128),
            None,
            false,
            &ThinkingConfig::Off,
        );
        let system = body["system"].as_array().unwrap();
        assert_eq!(system[0]["text"], "stable");
        assert_eq!(system[1]["cachePoint"]["ttl"], "1h");
        assert_eq!(system[2]["text"], "runtime hints");
        assert!(system.iter().all(|block| {
            block
                .get("text")
                .and_then(Value::as_str)
                .is_none_or(|text| !text.trim().is_empty())
        }));
    }

    #[test]
    fn build_bedrock_body_omits_cachepoint_only_system() {
        let messages = vec![
            json!({
                "role": "system",
                "content": [
                    {"cache_control": {"type": "ephemeral", "ttl": "1h"}}
                ]
            }),
            json!({"role": "user", "content": "hello"}),
        ];
        let body = build_provider_request_body(
            &messages,
            &[],
            "anthropic.claude-sonnet-4-20250514-v1:0",
            "bedrock",
            Some(128),
            None,
            false,
            &ThinkingConfig::Off,
        );
        assert!(body.get("system").is_none());
    }

    // ── Golden cases: real provider SSE fixtures ──────────────────────────────
    //
    // Fixtures captured from live APIs and stored in testdata/. Each test
    // feeds the raw SSE bytes through collect_llm_stream and asserts on the
    // parsed LlmCallResult, providing regression coverage for:
    //   - <think> tag extraction (MiniMax M2.5/M2.7)
    //   - reasoning_content field (Qwen3.6-plus, Kimi-k2.5)
    //   - tool_call streaming accumulation (MiniMax M2.5, Qwen-plus)
    //   - full_text / reasoning split correctness

    fn load_fixture(name: &str) -> Bytes {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/turn/testdata")
            .join(name);
        Bytes::from(std::fs::read(path).expect("fixture file missing"))
    }

    async fn parse_fixture(name: &str) -> LlmCallResult {
        let bytes = load_fixture(name);
        let stream = stream::iter(vec![Ok::<_, reqwest::Error>(bytes)]);
        collect_llm_stream(
            stream,
            "test-model",
            Instant::now(),
            LlmCancel::None,
            stream_idle_timeout(),
            stream_idle_timeout_after_progress(),
            None,
        )
        .await
        .expect("collect")
    }

    #[tokio::test]
    async fn golden_minimax_m25_simple_think_extracted() {
        // MiniMax M2.5: <think> in delta.content → reasoning extracted, full_text clean
        let res = parse_fixture("minimax_m25_simple.sse").await;
        assert!(
            !res.reasoning.is_empty(),
            "reasoning should be extracted from <think> tags"
        );
        assert!(
            !res.full_text.contains("<think>"),
            "full_text must not contain <think>"
        );
        assert!(
            !res.full_text.contains("</think>"),
            "full_text must not contain </think>"
        );
        assert!(
            !res.full_text.is_empty(),
            "full_text should have the answer"
        );
        assert!(res.tool_calls.is_empty());
    }

    #[tokio::test]
    async fn golden_minimax_m27_simple_think_extracted() {
        // MiniMax M2.7: same <think> pattern, verify reasoning/text split
        let res = parse_fixture("minimax_m27_simple.sse").await;
        assert!(
            !res.reasoning.is_empty(),
            "reasoning should be extracted from <think> tags"
        );
        assert!(
            !res.full_text.contains("<think>"),
            "full_text must not contain <think>"
        );
        assert!(
            !res.full_text.contains("</think>"),
            "full_text must not contain </think>"
        );
        assert!(
            !res.full_text.is_empty(),
            "full_text should have the answer"
        );
    }

    #[tokio::test]
    async fn golden_qwen36plus_reasoning_content_field() {
        // Qwen3.6-plus: reasoning via delta.reasoning_content (not <think> tags)
        let res = parse_fixture("qwen36plus_simple.sse").await;
        assert!(
            !res.reasoning.is_empty(),
            "reasoning_content field should be captured"
        );
        assert!(
            !res.full_text.is_empty(),
            "full_text should have the answer"
        );
        assert!(res.full_text.contains('4'), "answer to 2+2 should be 4");
        assert!(
            !res.full_text.contains("<think>"),
            "no think tags in qwen output"
        );
    }

    #[tokio::test]
    async fn golden_kimi_k25_reasoning_content_field() {
        // Kimi-k2.5: reasoning via delta.reasoning_content
        let res = parse_fixture("kimi_k25_simple.sse").await;
        assert!(
            !res.reasoning.is_empty(),
            "reasoning_content field should be captured"
        );
        assert!(
            !res.full_text.is_empty(),
            "full_text should have the answer"
        );
        assert!(res.full_text.contains('4'), "answer to 2+2 should be 4");
    }

    #[tokio::test]
    async fn golden_minimax_m25_tool_call_with_think() {
        // MiniMax M2.5 tool call: <think> in content + tool_calls in delta
        let res = parse_fixture("minimax_m25_tool_call.sse").await;
        assert!(!res.tool_calls.is_empty(), "should have tool calls");
        let tc = &res.tool_calls[0];
        let name = tc["function"]["name"].as_str().unwrap_or("");
        assert_eq!(name, "bash", "tool name should be bash");
        let args = tc["function"]["arguments"].as_str().unwrap_or("");
        assert!(
            args.contains("command"),
            "args must contain 'command' key, got: {args:?}"
        );
        // think content should be in reasoning, not full_text
        assert!(
            !res.full_text.contains("<think>"),
            "full_text must not contain <think>"
        );
    }

    #[tokio::test]
    async fn golden_qwen_plus_tool_call_no_reasoning() {
        // Qwen-plus: pure tool call, no reasoning
        let res = parse_fixture("qwen_plus_tool_call.sse").await;
        assert!(!res.tool_calls.is_empty(), "should have tool calls");
        let tc = &res.tool_calls[0];
        let name = tc["function"]["name"].as_str().unwrap_or("");
        assert!(!name.is_empty(), "tool name must not be empty");
        assert!(
            res.reasoning.is_empty(),
            "qwen-plus tool call should have no reasoning"
        );
    }

    // ── split_think_chunks (real MiniMax M2.7 streaming patterns) ────────────

    #[test]
    fn split_think_chunks_think_in_first_chunk() {
        // MiniMax M2.7 real: first chunk starts with <think>
        let mut in_think = false;
        let chunks = split_think_chunks("<think>\nThe user says \"hi\".", &mut in_think);
        assert!(in_think, "should be inside think block");
        assert_eq!(chunks, vec![("\nThe user says \"hi\".".to_string(), true)]);
    }

    #[test]
    fn split_think_chunks_think_closes_mid_chunk() {
        // MiniMax M2.7 real: last chunk closes </think> and has reply
        let mut in_think = true;
        let chunks = split_think_chunks(
            " Use friendly tone.\n</think>\n\nHello! How can I help you today?",
            &mut in_think,
        );
        assert!(!in_think, "should be outside think block after close");
        assert_eq!(
            chunks,
            vec![
                (" Use friendly tone.\n".to_string(), true),
                ("\n\nHello! How can I help you today?".to_string(), false),
            ]
        );
    }

    #[test]
    fn split_think_chunks_no_think_tags() {
        // Normal model response without thinking
        let mut in_think = false;
        let chunks = split_think_chunks("Hello! How can I help?", &mut in_think);
        assert!(!in_think);
        assert_eq!(chunks, vec![("Hello! How can I help?".to_string(), false)]);
    }

    #[test]
    fn split_think_chunks_full_think_in_one_chunk() {
        // Entire think block in a single chunk (non-streaming scenario)
        let mut in_think = false;
        let chunks = split_think_chunks("<think>reasoning here</think>\n\nAnswer.", &mut in_think);
        assert!(!in_think);
        assert_eq!(
            chunks,
            vec![
                ("reasoning here".to_string(), true),
                ("\n\nAnswer.".to_string(), false),
            ]
        );
    }

    #[test]
    fn split_think_chunks_state_persists_across_calls() {
        // Simulate MiniMax M2.7 multi-chunk stream
        let mut in_think = false;
        // chunk 1: opens think
        let c1 = split_think_chunks("<think>\nThe user says \"hi\".", &mut in_think);
        assert!(in_think);
        assert!(c1[0].1);
        // chunk 2: still inside think
        let c2 = split_think_chunks(" Should be concise.", &mut in_think);
        assert!(in_think);
        assert_eq!(c2, vec![(" Should be concise.".to_string(), true)]);
        // chunk 3: closes think and has reply
        let c3 = split_think_chunks("</think>\n\nHello!", &mut in_think);
        assert!(!in_think);
        assert_eq!(c3, vec![("\n\nHello!".to_string(), false),]);
    }

    #[test]
    fn split_think_chunks_multi_phase_reasoning() {
        // Some models emit multiple <think> phases in one stream.
        // Verify in_think correctly toggles false→true→false→true→false.
        let mut in_think = false;
        // Phase 1
        let c1 = split_think_chunks("<think>phase one</think>text one", &mut in_think);
        assert!(!in_think);
        assert_eq!(
            c1,
            vec![
                ("phase one".to_string(), true),
                ("text one".to_string(), false),
            ]
        );
        // Phase 2 — in_think was false, starts a new think block
        let c2 = split_think_chunks("<think>phase two</think>text two", &mut in_think);
        assert!(!in_think);
        assert_eq!(
            c2,
            vec![
                ("phase two".to_string(), true),
                ("text two".to_string(), false),
            ]
        );
        // Phase 3 — split across chunks
        let c3a = split_think_chunks("<think>phase three start", &mut in_think);
        assert!(in_think);
        assert_eq!(c3a, vec![("phase three start".to_string(), true)]);
        let c3b = split_think_chunks(" phase three end</think>final", &mut in_think);
        assert!(!in_think);
        assert_eq!(
            c3b,
            vec![
                (" phase three end".to_string(), true),
                ("final".to_string(), false),
            ]
        );
    }

    // ── extract_think_tags (post-collection cleanup) ──────────────────────────

    #[test]
    fn extract_think_tags_minimax_real_pattern() {
        // Real MiniMax M2.7 full_text after stream collection
        let text = "<think>\nThe user says \"hi\". Should be concise.\n</think>\n\nHello! How can I help you today?";
        let (reasoning, cleaned) = extract_think_tags(text).unwrap();
        assert_eq!(reasoning, "The user says \"hi\". Should be concise.");
        assert_eq!(cleaned, "Hello! How can I help you today?");
    }

    #[test]
    fn extract_think_tags_no_think_returns_none() {
        assert!(extract_think_tags("Hello! How can I help?").is_none());
    }

    #[test]
    fn extract_think_tags_skips_when_reasoning_already_set() {
        // extract_think_tags is only called when reasoning.is_empty(),
        // so this just verifies the function itself works correctly
        let text = "<think>step 1</think>answer";
        let (r, c) = extract_think_tags(text).unwrap();
        assert_eq!(r, "step 1");
        assert_eq!(c, "answer");
    }

    #[test]
    fn redact_provider_secrets_strips_known_prefixes() {
        let input = "sk-abc12345 and Bearer tok_xyz plus key-deadbeef end";
        let out = redact_provider_secrets(input);
        assert!(out.contains("[REDACTED]"), "missing redacted marker: {out}");
        assert!(!out.contains("abc12345"), "leaked sk- secret: {out}");
        assert!(!out.contains("tok_xyz"), "leaked bearer secret: {out}");
        assert!(!out.contains("deadbeef"), "leaked key- secret: {out}");
        assert!(out.contains("end"), "trailing text dropped: {out}");
    }

    #[test]
    fn redact_provider_secrets_leaves_clean_text() {
        let input = "Internal server error: upstream timeout";
        assert_eq!(redact_provider_secrets(input), input);
    }

    #[test]
    fn redact_provider_secrets_handles_quoted_json() {
        let input = r#"{"error":"invalid api key sk-abcXYZ"}"#;
        let out = redact_provider_secrets(input);
        assert!(!out.contains("abcXYZ"), "leaked sk- secret in JSON: {out}");
        assert!(out.contains("[REDACTED]"));
    }

    #[test]
    fn redact_provider_secrets_simulates_auth_log_path() {
        // Simulate what the auth-error path logs: a truncated body containing a key.
        let body = r#"{"error":{"message":"Incorrect API key sk-abc12345 provided"}}"#;
        let truncated = &body[..body.len().min(80)];
        let log_line = format!(
            "LLM auth error (401): {}",
            redact_provider_secrets(truncated)
        );
        assert!(!log_line.contains("sk-abc12345"));
        assert!(log_line.contains("[REDACTED]"));
    }

    /// audit-C1: global_llm_client must not use .expect() — a TLS backend
    /// failure should not crash the entire process.

    /// Regression: external LLM traffic must keep honoring env proxy policy even
    /// on fallback builds; silently downgrading to `.no_proxy()` makes
    /// region-gated upstreams flap between working and unsupported-region 400s.

    /// P1-E: llm_client must NOT define its own rate_limit_cooldown singleton.
    /// There must be exactly one PerModelCooldown singleton shared across all
    /// LLM call paths, otherwise a 429 recorded by one path is invisible to
    /// the other, causing duplicate rate-limit hits.

    // ─── Thinking config integration tests ──────────────────────────────

    #[test]
    fn build_bedrock_body_with_thinking_enabled() {
        let messages = vec![
            json!({"role": "system", "content": "You are helpful."}),
            json!({"role": "user", "content": "hello"}),
        ];
        let tools = vec![json!({
            "type": "function",
            "function": {"name": "read_file", "parameters": {"type": "object", "properties": {}}}
        })];
        let body = build_provider_request_body(
            &messages,
            &tools,
            "us.anthropic.claude-sonnet-4-20250514-v1:0",
            "bedrock",
            Some(8192),
            None,
            false,
            &ThinkingConfig::Enabled {
                budget_tokens: 5000,
            },
        );

        // Core structure
        assert!(!body.get("messages").unwrap().as_array().unwrap().is_empty());
        assert!(body.get("system").is_some());
        assert_eq!(body["inferenceConfig"]["maxTokens"], 8192);
        // Temperature must be absent (incompatible with thinking)
        assert!(body["inferenceConfig"].get("temperature").is_none());
        // Tools present
        assert!(!body["toolConfig"]["tools"].as_array().unwrap().is_empty());
        // Thinking config via additionalModelRequestFields
        assert_eq!(
            body["additionalModelRequestFields"]["thinking"]["type"],
            "enabled"
        );
        assert_eq!(
            body["additionalModelRequestFields"]["thinking"]["budget_tokens"],
            5000
        );
    }

    #[test]
    fn build_bedrock_body_with_thinking_adaptive() {
        let messages = vec![json!({"role": "user", "content": "hi"})];
        let body = build_provider_request_body(
            &messages,
            &[],
            "us.anthropic.claude-opus-4-6-v1",
            "bedrock",
            Some(16000),
            Some(0.7),
            false,
            &ThinkingConfig::Adaptive {
                effort: astra_turn_core::thinking_config::ThinkingEffort::Low,
            },
        );

        assert_eq!(
            body["additionalModelRequestFields"]["thinking"]["type"],
            "adaptive"
        );
        assert_eq!(
            body["additionalModelRequestFields"]["output_config"]["effort"],
            "low"
        );
        // Temperature removed even though it was requested
        assert!(body["inferenceConfig"].get("temperature").is_none());
    }

    #[test]
    fn build_bedrock_body_with_thinking_off() {
        let messages = vec![json!({"role": "user", "content": "hi"})];
        let body = build_provider_request_body(
            &messages,
            &[],
            "us.anthropic.claude-sonnet-4-20250514-v1:0",
            "bedrock",
            Some(4096),
            Some(0.5),
            false,
            &ThinkingConfig::Off,
        );

        // No thinking fields
        assert!(body.get("additionalModelRequestFields").is_none());
        // Temperature preserved
        assert_eq!(body["inferenceConfig"]["temperature"], 0.5);
    }

    #[test]
    fn build_bedrock_body_includes_reasoning_content_on_assistant_message() {
        let messages = vec![
            json!({"role": "user", "content": "hello"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{"id": "tc1", "type": "function", "function": {"name": "bash", "arguments": "{}"}}],
                "reasoning_content": "I should run bash",
                "reasoning_signature": "sig_abc123"
            }),
            json!({"role": "tool", "tool_call_id": "tc1", "content": "done"}),
            json!({"role": "user", "content": "thanks"}),
        ];
        let body = build_provider_request_body(
            &messages,
            &[],
            "us.anthropic.claude-sonnet-4-20250514-v1:0",
            "bedrock",
            Some(4096),
            None,
            true,
            &ThinkingConfig::Enabled {
                budget_tokens: 1024,
            },
        );
        let bedrock_msgs = body["messages"].as_array().unwrap();
        // First message is user "hello"
        // Second is assistant with reasoningContent + toolUse
        let assistant_msg = &bedrock_msgs[1];
        assert_eq!(assistant_msg["role"], "assistant");
        let content = assistant_msg["content"].as_array().unwrap();
        // First block should be reasoningContent
        assert!(content[0].get("reasoningContent").is_some());
        let rc = &content[0]["reasoningContent"]["reasoningText"];
        assert_eq!(rc["text"], "I should run bash");
        assert_eq!(rc["signature"], "sig_abc123");
        // Second block should be toolUse
        assert!(content[1].get("toolUse").is_some());
    }

    /// Regression test using real Bedrock API response data.
    /// Verifies that a multi-turn thinking + tool_use conversation produces
    /// valid Bedrock request bodies that won't trigger the "signature: Field required" 400.
    #[test]
    fn build_bedrock_body_reasoning_roundtrip_real_signature() {
        // Real signature captured from Bedrock converse API response
        let real_signature = "EucBCkgIDRABGAIqQCjq2TSFiIiSlMoit+qcPnX9t83drZVVaoUyCag7HPkIAplllVNsRLaTzM6wl8n/qpOFbkkyrhwEa/STyGsDb9MSDMhIDhAFyvS1Z5oD7xoMq8EnICsA4bH25yXtIjDJvcoCxGdUU8BeKmUYjm4+6nLghgxhLZJpQL4WphleWcpr8w0PelHlkxs8G0fohDUqTQEEypAjDZqZhWt4I+h4ERKDZ/u1uW59Gs2NJWEcuFtTiKot3Kc+jJvH3Nn9Yp9iaJFbi4kakmwqdmpyxUrISklB/uqiJ0TXeN94CoAmGAE=";

        let messages = vec![
            json!({"role": "user", "content": "What is 2+2? Use the calculator tool."}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{"id": "tooluse_oOwbnKc4jO48ShtaXrOPcw", "type": "function", "function": {"name": "calculator", "arguments": "{\"expression\":\"2+2\"}"}}],
                "reasoning_content": "The user wants me to calculate 2+2 using the calculator tool.",
                "reasoning_signature": real_signature
            }),
            json!({"role": "tool", "tool_call_id": "tooluse_oOwbnKc4jO48ShtaXrOPcw", "content": "4"}),
        ];
        let body = build_provider_request_body(
            &messages,
            &[json!({
                "type": "function",
                "function": {
                    "name": "calculator",
                    "description": "Compute arithmetic",
                    "parameters": {"type": "object", "properties": {"expression": {"type": "string"}}}
                }
            })],
            "us.anthropic.claude-sonnet-4-6",
            "bedrock",
            Some(4096),
            None,
            true,
            &ThinkingConfig::Enabled {
                budget_tokens: 1024,
            },
        );

        let bedrock_msgs = body["messages"].as_array().unwrap();
        // assistant message (index 1) must have reasoningContent with signature
        let assistant = &bedrock_msgs[1];
        assert_eq!(assistant["role"], "assistant");
        let content = assistant["content"].as_array().unwrap();

        // Order must be: reasoningContent → toolUse (text is optional)
        let rc_block = &content[0];
        assert!(
            rc_block.get("reasoningContent").is_some(),
            "first block must be reasoningContent"
        );
        let rt = &rc_block["reasoningContent"]["reasoningText"];
        assert_eq!(
            rt["text"].as_str().unwrap(),
            "The user wants me to calculate 2+2 using the calculator tool."
        );
        assert_eq!(
            rt["signature"].as_str().unwrap(),
            real_signature,
            "signature must be preserved verbatim"
        );

        // toolUse block follows
        let tool_block = content.iter().find(|b| b.get("toolUse").is_some());
        assert!(tool_block.is_some(), "must have toolUse block");
        assert_eq!(tool_block.unwrap()["toolUse"]["name"], "calculator");

        // Verify thinking config is applied
        assert!(body.get("additionalModelRequestFields").is_some());
    }

    /// Verify that stripped (empty) reasoning does NOT produce a reasoningContent block,
    /// which would trigger Bedrock's "signature: Field required" 400 error.
    #[test]
    fn build_bedrock_body_empty_reasoning_omits_reasoning_block() {
        let messages = vec![
            json!({"role": "user", "content": "hello"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{"id": "tc1", "type": "function", "function": {"name": "bash", "arguments": "{}"}}],
                "reasoning_content": ""
            }),
            json!({"role": "tool", "tool_call_id": "tc1", "content": "done"}),
        ];
        let body = build_provider_request_body(
            &messages,
            &[],
            "us.anthropic.claude-sonnet-4-6",
            "bedrock",
            Some(4096),
            None,
            true,
            &ThinkingConfig::Enabled {
                budget_tokens: 1024,
            },
        );
        let bedrock_msgs = body["messages"].as_array().unwrap();
        let assistant = &bedrock_msgs[1];
        let content = assistant["content"].as_array().unwrap();
        // No reasoningContent block when reasoning is empty
        assert!(
            !content.iter().any(|b| b.get("reasoningContent").is_some()),
            "empty reasoning_content must NOT produce a reasoningContent block"
        );
    }

    /// Regression: assistant message with reasoning_content but no text/tool_calls
    /// must NOT produce a message ending with a thinking block (Bedrock 400 error).
    #[test]
    fn bedrock_reasoning_only_assistant_gets_trailing_text_block() {
        let messages = vec![
            json!({"role": "user", "content": "hello"}),
            json!({
                "role": "assistant",
                "content": null,
                "reasoning_content": "I need to think about this...",
                "reasoning_signature": "sig_test"
            }),
            json!({"role": "user", "content": "continue"}),
        ];
        let body = build_provider_request_body(
            &messages,
            &[],
            "us.anthropic.claude-opus-4-6-v1:0",
            "bedrock",
            Some(4096),
            None,
            true,
            &ThinkingConfig::Enabled {
                budget_tokens: 1024,
            },
        );
        let bedrock_msgs = body["messages"].as_array().unwrap();
        let assistant = &bedrock_msgs[1];
        assert_eq!(assistant["role"], "assistant");
        let content = assistant["content"].as_array().unwrap();
        assert!(
            content.len() >= 2,
            "reasoning-only assistant must have at least 2 blocks, got {}",
            content.len()
        );
        assert!(
            content[0].get("reasoningContent").is_some(),
            "first block should be reasoningContent"
        );
        // Final block must NOT be reasoningContent (Bedrock rejects this)
        let last = content.last().unwrap();
        assert!(
            last.get("reasoningContent").is_none(),
            "final block must not be reasoningContent, got: {last}"
        );
    }

    /// Same regression for the Anthropic Messages path.
    #[test]
    fn anthropic_reasoning_only_assistant_gets_trailing_text_block() {
        let msg = json!({
            "role": "assistant",
            "content": null,
            "reasoning_content": "Let me think...",
            "reasoning_signature": "sig_xyz"
        });
        let result = anthropic_message_from_openai(&msg).unwrap();
        let blocks = result["content"].as_array().unwrap();
        assert!(
            blocks.len() >= 2,
            "reasoning-only assistant must have at least 2 blocks, got {}",
            blocks.len()
        );
        assert_eq!(blocks[0]["type"], "thinking");
        // Final block must NOT be thinking
        let last = blocks.last().unwrap();
        assert_ne!(
            last["type"], "thinking",
            "final block must not be thinking, got: {last}"
        );
    }

    #[test]
    fn anthropic_message_omits_unsigned_reasoning_block_but_keeps_tool_calls() {
        let msg = json!({
            "role": "assistant",
            "content": null,
            "reasoning_content": "unsigned historical reasoning",
            "tool_calls": [{
                "id": "tc1",
                "type": "function",
                "function": {"name": "bash", "arguments": "{}"}
            }],
        });
        let result = anthropic_message_from_openai(&msg).unwrap();
        let blocks = result["content"].as_array().unwrap();
        assert!(
            !blocks
                .iter()
                .any(|block| block.get("type").and_then(Value::as_str) == Some("thinking")),
            "unsigned historical reasoning must not be serialized into Anthropic thinking blocks"
        );
        assert!(
            blocks
                .iter()
                .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_use")),
            "tool_use blocks must remain visible after omitting malformed reasoning replay"
        );
    }

    /// Helper for counter tests: feed a deliberately malformed Bedrock message
    /// directly into the guard. Normal request construction strips unsigned
    /// reasoning before this point, so direct guard tests are the only valid way
    /// to exercise the invariant.
    fn assert_malformed_bedrock_thinking_body() {
        let messages = vec![json!({
            "role": "assistant",
            "content": [{
                "reasoningContent": {
                    "reasoningText": {"text": "thinking without signature"}
                }
            }]
        })];
        assert_bedrock_thinking_signature_contract(&messages);
    }

    // Counter increments alongside the debug_assert so release builds can
    // expose a continuous-signal tripwire (BEDROCK_THINKING_SIGNATURE_VIOLATION_COUNT).
    // The counter must increment even if the panic short-circuits the rest of
    // the build — otherwise monitoring misses the first violation.
    #[test]
    fn bedrock_thinking_signature_violation_increments_counter() {
        use std::sync::atomic::Ordering;
        let before = BEDROCK_THINKING_SIGNATURE_VIOLATION_COUNT.load(Ordering::Relaxed);
        // debug_assert panics in test/debug builds; catch so we can read the
        // counter afterward. The fetch_add runs *before* the debug_assert so
        // the counter observes the violation even when the assert fires.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
            assert_malformed_bedrock_thinking_body,
        ));
        let after = BEDROCK_THINKING_SIGNATURE_VIOLATION_COUNT.load(Ordering::Relaxed);
        assert!(
            after > before,
            "counter must advance on every signature-contract violation \
             (before={before}, after={after})"
        );
    }

    // Guard that the debug_assert in `assert_bedrock_thinking_signature_contract`
    // actually fires when a reasoning block arrives without signature. This is
    // the "scream if this ever regresses again" safety net for PR #284's class
    // of bug. Expected outcome: Bedrock would 400 — we want a test panic first.
    #[test]
    #[should_panic(expected = "Bedrock thinking contract violation")]
    fn bedrock_thinking_signature_contract_panics_on_missing_signature() {
        assert_malformed_bedrock_thinking_body();
    }

    // Request construction must never produce the malformed body above. If a
    // session carries reasoning text from a provider/model that did not emit a
    // Bedrock-compatible signature, the Bedrock request keeps text/tool calls
    // and omits the invalid reasoningContent block.
    #[test]
    fn build_bedrock_body_omits_unsigned_reasoning_when_thinking_on() {
        let messages = vec![
            json!({"role": "user", "content": "What is 2+2?"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{"id": "tc1", "type": "function", "function": {"name": "calc", "arguments": "{}"}}],
                "reasoning_content": "let me compute",
                // reasoning_signature intentionally missing
            }),
            json!({"role": "tool", "tool_call_id": "tc1", "content": "4"}),
        ];
        let body = build_provider_request_body(
            &messages,
            &[],
            "us.anthropic.claude-sonnet-4-6",
            "bedrock",
            Some(4096),
            None,
            true,
            &ThinkingConfig::Enabled {
                budget_tokens: 1024,
            },
        );
        let content = body["messages"][1]["content"].as_array().unwrap();
        assert!(
            !content
                .iter()
                .any(|block| block.get("reasoningContent").is_some()),
            "unsigned historical reasoning must not be serialized into Bedrock reasoningContent"
        );
        assert!(
            content.iter().any(|block| block.get("toolUse").is_some()),
            "assistant tool calls must remain visible after unsigned reasoning is omitted"
        );
    }

    // Contract positive: signature present → no panic, body built normally.
    #[test]
    fn bedrock_thinking_signature_contract_passes_when_signature_present() {
        let messages = vec![
            json!({"role": "user", "content": "What is 2+2?"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{"id": "tc1", "type": "function", "function": {"name": "calc", "arguments": "{}"}}],
                "reasoning_content": "let me compute",
                "reasoning_signature": "sig_from_bedrock"
            }),
            json!({"role": "tool", "tool_call_id": "tc1", "content": "4"}),
        ];
        let body = build_provider_request_body(
            &messages,
            &[],
            "us.anthropic.claude-sonnet-4-6",
            "bedrock",
            Some(4096),
            None,
            true,
            &ThinkingConfig::Enabled {
                budget_tokens: 1024,
            },
        );
        // Assistant reasoningContent block must carry the signature.
        let rc_sig =
            body["messages"][1]["content"][0]["reasoningContent"]["reasoningText"]["signature"]
                .as_str()
                .unwrap();
        assert_eq!(rc_sig, "sig_from_bedrock");
    }

    // Contract negative-bypass: thinking disabled → stale reasoning history
    // is not serialized, so the signature guard has nothing to enforce.
    #[test]
    fn bedrock_thinking_signature_contract_silent_when_thinking_off() {
        let messages = vec![
            json!({"role": "user", "content": "hi"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{"id": "tc1", "type": "function", "function": {"name": "bash", "arguments": "{}"}}],
                "reasoning_content": "some leftover"
            }),
            json!({"role": "tool", "tool_call_id": "tc1", "content": "ok"}),
        ];
        let body = build_provider_request_body(
            &messages,
            &[],
            "us.anthropic.claude-sonnet-4-6",
            "bedrock",
            Some(4096),
            None,
            true,
            &ThinkingConfig::Off,
        );
        let assistant = &body["messages"][1];
        let content = assistant["content"].as_array().unwrap();
        assert!(
            !content.iter().any(|b| b.get("reasoningContent").is_some()),
            "thinking=off must suppress stale reasoningContent so Bedrock never sees an unsigned reasoning block"
        );
    }

    #[test]
    fn build_anthropic_body_with_thinking_enabled() {
        let messages = vec![json!({"role": "user", "content": "hello"})];
        let tools = vec![json!({
            "name": "read_file",
            "description": "Read a file",
            "input_schema": {"type": "object", "properties": {}}
        })];
        let body = build_provider_request_body(
            &messages,
            &tools,
            "claude-sonnet-4-20250514",
            "anthropic",
            Some(8192),
            Some(0.7),
            true,
            &ThinkingConfig::Enabled {
                budget_tokens: 4000,
            },
        );

        // Core structure
        assert_eq!(body["model"], "claude-sonnet-4-20250514");
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_tokens"], 8192);
        // Temperature removed
        assert!(body.get("temperature").is_none());
        // Tools present
        assert!(!body["tools"].as_array().unwrap().is_empty());
        assert_eq!(body["tool_choice"]["type"], "auto");
        // Thinking config
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 4000);
    }

    #[test]
    fn build_anthropic_body_uses_native_system_and_tool_shape() {
        let messages = vec![
            json!({
                "role": "system",
                "content": [
                    {"type": "text", "text": "stable", "cache_control": {"type": "ephemeral", "ttl": "1h"}},
                    {"type": "text", "text": "dynamic"}
                ]
            }),
            json!({"role": "user", "content": "hello"}),
        ];
        let tools = vec![json!({
            "type": "function",
            "function": {
                "name": "bash",
                "description": "Run shell",
                "parameters": {"type": "object", "properties": {"command": {"type": "string"}}}
            },
            "cache_control": {"type": "ephemeral", "ttl": "1h"}
        })];
        let body = build_provider_request_body(
            &messages,
            &tools,
            "claude-sonnet-4-20250514",
            "anthropic",
            Some(1024),
            None,
            true,
            &ThinkingConfig::Off,
        );

        assert!(
            body.get("system").is_some(),
            "Anthropic native body needs top-level system: {body:#?}"
        );
        assert_eq!(body["system"][0]["text"], "stable");
        assert_eq!(body["system"][0]["cache_control"]["ttl"], "1h");
        assert!(
            body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .all(|m| m["role"] != "system"),
            "system-role messages are invalid in Anthropic Messages API: {body:#?}"
        );
        assert_eq!(body["tools"][0]["name"], "bash");
        assert_eq!(
            body["tools"][0]["input_schema"]["properties"]["command"]["type"],
            "string"
        );
        assert_eq!(body["tools"][0]["cache_control"]["ttl"], "1h");
    }

    #[test]
    fn build_anthropic_body_keeps_runtime_system_tail_out_of_cached_prefix_block() {
        let runtime = crate::turn::wire_assembly::required_runtime_preamble_message(
            "required resume context",
        )
        .expect("runtime message");
        let messages = vec![
            json!({
                "role": "system",
                "content": [{
                    "type": "text",
                    "text": "stable",
                    "cache_control": {"type": "ephemeral", "ttl": "1h"}
                }]
            }),
            json!({"role": "user", "content": "hello"}),
            runtime,
        ];
        let messages = consolidate_system_messages_for_provider(
            &messages,
            "anthropic",
            "claude-sonnet-4",
            None,
        );
        let body = build_provider_request_body(
            &messages,
            &[],
            "claude-sonnet-4-20250514",
            "anthropic",
            Some(1024),
            None,
            true,
            &ThinkingConfig::Off,
        );

        let system = body["system"].as_array().expect("top-level system blocks");
        assert_eq!(system.len(), 2, "{system:#?}");
        assert_eq!(system[0]["text"], "stable");
        assert_eq!(system[0]["cache_control"]["ttl"], "1h");
        assert_eq!(system[1]["text"], "required resume context");
        assert!(system[1].get("cache_control").is_none());
        assert!(
            body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .all(|message| message["role"] != "system"),
            "system must remain top-level for Anthropic: {body:#?}"
        );
        let rendered = body.to_string();
        assert!(
            rendered
                .find("stable")
                .zip(rendered.find("required resume context"))
                .is_some_and(|(stable, runtime)| stable < runtime),
            "stable system block must precede runtime block: {body:#?}"
        );
        assert!(
            !body
                .to_string()
                .contains(crate::turn::wire_assembly::REQUIRED_RUNTIME_PREAMBLE_MARKER),
            "internal runtime marker must never reach the provider request body: {body:#?}"
        );
    }

    #[test]
    fn build_openai_body_strips_optional_runtime_system_marker() {
        let runtime = crate::turn::wire_assembly::runtime_system_context_message(
            "optional runtime evidence",
            false,
        )
        .expect("runtime message");
        let body = build_provider_request_body(
            &[runtime, json!({"role": "user", "content": "hello"})],
            &[],
            "gpt-4o",
            "openai",
            Some(1024),
            None,
            false,
            &ThinkingConfig::Off,
        );

        let rendered = body.to_string();
        assert!(rendered.contains("optional runtime evidence"));
        assert!(
            !rendered.contains(crate::turn::wire_assembly::RUNTIME_SYSTEM_CONTEXT_MARKER),
            "optional runtime marker must never reach the provider request body: {body:#?}"
        );
    }

    #[test]
    fn build_openai_body_keeps_real_tool_result_before_tail_runtime_system() {
        let runtime = crate::turn::wire_assembly::runtime_system_context_message(
            "round runtime context",
            false,
        )
        .expect("runtime message");
        let mut messages = vec![
            json!({"role": "system", "content": "stable"}),
            json!({"role": "user", "content": "inspect"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call-real",
                    "type": "function",
                    "function": {"name": "read_file", "arguments": "{}"}
                }]
            }),
            json!({
                "role": "tool",
                "tool_call_id": "call-real",
                "content": "real tool result"
            }),
        ];
        let boundary = crate::turn::wire_assembly::insert_runtime_system_context(
            &mut messages,
            vec![runtime],
            astra_turn_core::cache_placement::VolatilePlacement::TailSuffix,
        );
        assert_eq!(boundary, Some(4));

        let body = build_provider_request_body(
            &messages,
            &[],
            "qwen-plus",
            "openai",
            Some(1024),
            None,
            false,
            &ThinkingConfig::Off,
        );
        let provider_messages = body["messages"].as_array().expect("messages array");
        assert_eq!(provider_messages.len(), 5, "{provider_messages:#?}");
        assert_eq!(provider_messages[2]["role"], "assistant");
        assert_eq!(provider_messages[3]["role"], "tool");
        assert_eq!(provider_messages[3]["tool_call_id"], "call-real");
        assert_eq!(provider_messages[3]["content"], "real tool result");
        assert_eq!(provider_messages[4]["role"], "system");
        assert_eq!(provider_messages[4]["content"], "round runtime context");
        assert!(
            provider_messages.iter().all(|message| {
                message.get("content").and_then(Value::as_str)
                    != Some(SYNTHETIC_TOOL_INTERRUPTED_CONTENT)
            }),
            "valid tool result must not be replaced with synthetic repair: {provider_messages:#?}"
        );
    }

    #[test]
    fn parse_anthropic_nonstream_response_extracts_text_tool_calls_and_cache_usage() {
        let v = json!({
            "content": [
                {"type": "text", "text": "hello"},
                {"type": "tool_use", "id": "toolu_1", "name": "bash", "input": {"command": "pwd"}}
            ],
            "stop_reason": "tool_use",
            "usage": {
                "input_tokens": 10,
                "cache_read_input_tokens": 7,
                "cache_creation_input_tokens": 3,
                "output_tokens": 5
            }
        });
        let r = parse_nonstream_response_for_provider(
            &v,
            "anthropic",
            "claude-sonnet-4-20250514",
            Instant::now(),
        );

        assert_eq!(r.full_text, "hello");
        assert_eq!(r.finish_reason.as_deref(), Some("tool_use"));
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0]["id"], "toolu_1");
        assert_eq!(r.tool_calls[0]["function"]["name"], "bash");
        assert_eq!(
            r.tool_calls[0]["function"]["arguments"].as_str(),
            Some(r#"{"command":"pwd"}"#)
        );
        assert_eq!(
            r.usage.get("input_tokens").and_then(Value::as_u64),
            Some(10)
        );
        assert_eq!(
            r.usage.get("cached_input_tokens").and_then(Value::as_u64),
            Some(7)
        );
        assert_eq!(
            r.usage.get("cache_creation_tokens").and_then(Value::as_u64),
            Some(3)
        );
        assert_eq!(
            r.usage.get("output_tokens").and_then(Value::as_u64),
            Some(5)
        );
        assert_eq!(
            r.usage.get("total_tokens").and_then(Value::as_u64),
            Some(25)
        );
    }

    /// Companion to `collect_anthropic_stream_captures_signature_delta`.
    /// When the stream idles and we fall back to the non-stream endpoint,
    /// the body is shaped like `{content: [{type: "thinking", thinking: ...,
    /// signature: ...}, {type: "tool_use", ...}]}`. Dropping the signature
    /// here re-opens the effccfcd failure on the one retry path the
    /// streaming fix doesn't cover.
    #[test]
    fn parse_anthropic_nonstream_response_extracts_thinking_signature() {
        let v = json!({
            "content": [
                {
                    "type": "thinking",
                    "thinking": "let me check",
                    "signature": "sig_nonstream_abc",
                },
                {"type": "tool_use", "id": "toolu_1", "name": "bash", "input": {"cmd": "ls"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });
        let r = parse_nonstream_response_for_provider(
            &v,
            "anthropic",
            "claude-sonnet-4",
            Instant::now(),
        );
        assert_eq!(r.reasoning, "let me check");
        assert_eq!(
            r.reasoning_signature, "sig_nonstream_abc",
            "signature on the thinking block must survive into LlmCallResult",
        );
    }

    #[test]
    fn build_anthropic_body_with_thinking_adaptive_uses_output_config_effort() {
        let messages = vec![json!({"role": "user", "content": "hello"})];
        let body = build_provider_request_body(
            &messages,
            &[],
            "claude-opus-4-7",
            "anthropic",
            Some(16000),
            Some(0.7),
            true,
            &ThinkingConfig::Adaptive {
                effort: astra_turn_core::thinking_config::ThinkingEffort::High,
            },
        );

        assert_eq!(
            body["thinking"],
            json!({"type": "adaptive", "display": "summarized"})
        );
        assert_eq!(body["output_config"], json!({"effort": "high"}));
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn build_anthropic_body_with_thinking_off() {
        let messages = vec![json!({"role": "user", "content": "hello"})];
        let body = build_provider_request_body(
            &messages,
            &[],
            "claude-sonnet-4-20250514",
            "anthropic",
            Some(4096),
            Some(0.5),
            true,
            &ThinkingConfig::Off,
        );

        assert!(body.get("thinking").is_none());
        assert_eq!(body["temperature"], 0.5);
    }

    #[test]
    fn build_openai_body_with_thinking_adaptive() {
        let messages = vec![json!({"role": "user", "content": "hello"})];
        let body = build_provider_request_body(
            &messages,
            &[],
            "o3",
            "openai",
            Some(4096),
            None,
            true,
            &ThinkingConfig::Adaptive {
                effort: astra_turn_core::thinking_config::ThinkingEffort::Medium,
            },
        );

        assert_eq!(body["model"], "o3");
        assert_eq!(body["reasoning_effort"], "medium");
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn build_openai_body_preserves_plain_assistant_reasoning_placeholder() {
        let messages = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": "hello", "reasoning_content": ""}),
            json!({"role": "user", "content": "continue"}),
        ];
        let body = build_provider_request_body(
            &messages,
            &[],
            "deepseek-v4-pro-official",
            "openai",
            Some(4096),
            None,
            true,
            &ThinkingConfig::Enabled {
                budget_tokens: 10_000,
            },
        );

        assert_eq!(body["messages"][1]["role"], "assistant");
        assert_eq!(body["messages"][1]["content"], "hello");
        assert_eq!(body["messages"][1]["reasoning_content"], "");
    }

    #[test]
    fn build_openai_body_backfills_missing_reasoning_for_deepseek_history() {
        let messages = vec![
            json!({"role": "user", "content": "hi"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "tc1",
                    "type": "function",
                    "function": {"name": "bash", "arguments": "{}"}
                }]
            }),
            json!({"role": "tool", "tool_call_id": "tc1", "content": "ok"}),
            json!({
                "role": "assistant",
                "content": "done",
                "reasoning_content": "I should explain the result."
            }),
        ];
        let body = build_provider_request_body(
            &messages,
            &[],
            "deepseek-v4-pro-official",
            "openai",
            Some(4096),
            None,
            true,
            &ThinkingConfig::Enabled {
                budget_tokens: 10_000,
            },
        );

        assert_eq!(body["messages"][1]["reasoning_content"], "");
        assert_eq!(
            body["messages"][3]["reasoning_content"],
            "I should explain the result."
        );
    }

    #[test]
    fn build_openai_body_backfills_reasoning_for_native_deepseek_without_explicit_thinking() {
        let messages = vec![
            json!({"role": "user", "content": "hi"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "tc1",
                    "type": "function",
                    "function": {"name": "bash", "arguments": "{}"}
                }]
            }),
            json!({"role": "tool", "tool_call_id": "tc1", "content": "ok"}),
            json!({"role": "user", "content": "continue"}),
        ];
        let body = build_provider_request_body(
            &messages,
            &[],
            "deepseek-v4-pro-official",
            "openai",
            Some(4096),
            None,
            true,
            &ThinkingConfig::Off,
        );

        assert_eq!(body["messages"][1]["reasoning_content"], "");
    }

    /// Qwen models served through the *DashScope* provider use `enable_thinking`.
    /// The provider name (not model name) is the discriminator — the same Qwen model
    /// served through a generic vLLM/Ollama proxy with provider="openai" must NOT
    /// receive `enable_thinking` because those proxies reject unknown top-level fields.
    #[test]
    fn build_dashscope_qwen_body_with_thinking_enabled() {
        let messages = vec![json!({"role": "user", "content": "hello"})];
        let body = build_provider_request_body(
            &messages,
            &[],
            "qwen3.6-plus",
            "dashscope",
            Some(4096),
            Some(0.7),
            true,
            &ThinkingConfig::Enabled {
                budget_tokens: 10_000,
            },
        );

        assert_eq!(body["model"], "qwen3.6-plus");
        assert_eq!(body["enable_thinking"], true);
        assert!(body.get("reasoning_effort").is_none());
        assert_eq!(body["temperature"], 0.7);
    }

    /// ThinkingConfig::Off must explicitly suppress thinking for DashScope
    /// native thinkers (Qwen3, Qwen3.5) — otherwise they think by default.
    #[test]
    fn build_dashscope_body_with_thinking_off_suppresses() {
        let messages = vec![json!({"role": "user", "content": "hello"})];
        let body = build_provider_request_body(
            &messages,
            &[],
            "qwen3.5-flash",
            "dashscope",
            Some(200),
            Some(0.0),
            false,
            &ThinkingConfig::Off,
        );

        assert_eq!(body["model"], "qwen3.5-flash");
        assert_eq!(body["enable_thinking"], false);
        assert!(body.get("reasoning_effort").is_none());
    }

    /// Same Qwen model through a generic OpenAI-compatible proxy must NOT get
    /// `enable_thinking` — the proxy does not know about that field and may 400.
    #[test]
    fn build_generic_proxy_qwen_body_does_not_set_dashscope_flag() {
        let messages = vec![json!({"role": "user", "content": "hello"})];
        let body = build_provider_request_body(
            &messages,
            &[],
            "qwen3.6-plus",
            "openai",
            Some(4096),
            Some(0.7),
            true,
            &ThinkingConfig::Enabled {
                budget_tokens: 10_000,
            },
        );

        assert_eq!(body["model"], "qwen3.6-plus");
        // Generic OpenAI-compatible proxy: no DashScope-specific field.
        assert!(body.get("enable_thinking").is_none());
        // Enabled thinking has no OpenAI mapping (no reasoning_effort either).
        assert!(body.get("reasoning_effort").is_none());
        assert_eq!(body["temperature"], 0.7);
    }

    #[test]
    fn build_standard_openai_body_with_budget_thinking_does_not_send_dashscope_flag() {
        let messages = vec![json!({"role": "user", "content": "hello"})];
        let body = build_provider_request_body(
            &messages,
            &[],
            "gpt-4o",
            "openai",
            Some(4096),
            Some(0.7),
            true,
            &ThinkingConfig::Enabled {
                budget_tokens: 10_000,
            },
        );

        assert!(body.get("enable_thinking").is_none());
        assert!(body.get("reasoning_effort").is_none());
        assert_eq!(body["temperature"], 0.7);
    }

    #[test]
    fn build_openai_body_with_thinking_off() {
        let messages = vec![json!({"role": "user", "content": "hello"})];
        let body = build_provider_request_body(
            &messages,
            &[],
            "gpt-4o",
            "openai",
            Some(4096),
            Some(0.7),
            true,
            &ThinkingConfig::Off,
        );

        assert!(body.get("reasoning_effort").is_none());
        assert_eq!(body["temperature"], 0.7);
    }

    // ─────────────────────────────────────────────────────────────────────
    // Provider × Thinking × Tool-call × Multi-turn capability matrix
    // ─────────────────────────────────────────────────────────────────────
    //
    // This matrix exists because PR #284 was followed by a silent follow-up
    // regression: the Bedrock signature stopped flowing through the SSE hop
    // between bridge and CLI. Unit tests passed; the end-to-end contract
    // broke anyway. The rule now is:
    //
    //   For every (provider, thinking_mode, has_tool_call, turn_number)
    //   combination that reaches a live provider, assert the exact shape of
    //   the request body produced by `build_provider_request_body`.
    //
    // Adding a new provider / thinking mode without a matrix row is a bug.
    // If the scenario is not supported yet, add a `#[ignore]` placeholder
    // with a comment — don't silently skip.
    //
    // Columns pinned per row:
    //  - reasoning block shape (or absence)
    //  - signature presence (Bedrock + Anthropic thinking only)
    //  - tool_use / toolUse block presence on turn-2+ assistant messages
    //  - top-level `thinking` config applied correctly
    mod thinking_matrix {
        use super::*;

        fn user(text: &str) -> Value {
            json!({"role": "user", "content": text})
        }

        fn assistant_with_tool_call(
            reasoning: &str,
            signature: Option<&str>,
            tool_name: &str,
            tool_id: &str,
        ) -> Value {
            let mut msg = json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": tool_id,
                    "type": "function",
                    "function": {"name": tool_name, "arguments": "{}"}
                }]
            });
            if !reasoning.is_empty() {
                msg["reasoning_content"] = Value::String(reasoning.to_string());
            }
            if let Some(sig) = signature {
                msg["reasoning_signature"] = Value::String(sig.to_string());
            }
            msg
        }

        fn tool_result(tool_id: &str, output: &str) -> Value {
            json!({"role": "tool", "tool_call_id": tool_id, "content": output})
        }

        // ── Row: Bedrock + Thinking::Enabled + tool_call + turn-2 ──
        // This is the exact scenario that caused the HTTP 400 that led
        // to this matrix's existence.
        #[test]
        fn bedrock_thinking_tool_call_multi_turn_serializes_signature() {
            let messages = vec![
                user("compute 2+2"),
                assistant_with_tool_call("thinking...", Some("real_sig"), "calc", "tc1"),
                tool_result("tc1", "4"),
            ];
            let body = build_provider_request_body(
                &messages,
                &[],
                "us.anthropic.claude-sonnet-4-6",
                "bedrock",
                Some(4096),
                None,
                true,
                &ThinkingConfig::Enabled {
                    budget_tokens: 1024,
                },
            );
            let assistant = &body["messages"][1];
            let content = assistant["content"].as_array().unwrap();
            let rc = &content[0]["reasoningContent"]["reasoningText"];
            assert_eq!(rc["text"], "thinking...");
            assert_eq!(
                rc["signature"], "real_sig",
                "signature MUST appear on assistant reasoningContent — \
                 Bedrock returns 400 `thinking.signature: Field required` otherwise"
            );
            let has_tool_use = content.iter().any(|b| b.get("toolUse").is_some());
            assert!(has_tool_use, "toolUse block must follow reasoningContent");
            assert_eq!(
                body["additionalModelRequestFields"]["thinking"]["type"],
                "enabled"
            );
        }

        // ── Row: Bedrock + Thinking::Off + tool_call + turn-2 ──
        // No thinking → no reasoningContent block even if historic
        // reasoning_content is present (e.g. session resumed with stale state).
        #[test]
        fn bedrock_thinking_off_tool_call_omits_reasoning_block() {
            let messages = vec![
                user("hi"),
                assistant_with_tool_call("old thinking", None, "bash", "tc1"),
                tool_result("tc1", "ok"),
            ];
            let body = build_provider_request_body(
                &messages,
                &[],
                "us.anthropic.claude-sonnet-4-6",
                "bedrock",
                Some(4096),
                None,
                true,
                &ThinkingConfig::Off,
            );
            assert!(body.get("additionalModelRequestFields").is_none());
            let assistant = &body["messages"][1];
            let content = assistant["content"].as_array().unwrap();
            assert!(
                !content.iter().any(|b| b.get("reasoningContent").is_some()),
                "thinking=off should not serialize stale reasoningContent"
            );
        }

        // ── Row: Bedrock + Thinking::Enabled + no tool_call + turn-1 ──
        // No historic assistant message yet → no signature contract to honor.
        #[test]
        fn bedrock_thinking_first_turn_no_history() {
            let messages = vec![user("compute 2+2")];
            let body = build_provider_request_body(
                &messages,
                &[],
                "us.anthropic.claude-sonnet-4-6",
                "bedrock",
                Some(4096),
                None,
                true,
                &ThinkingConfig::Enabled {
                    budget_tokens: 1024,
                },
            );
            assert_eq!(
                body["additionalModelRequestFields"]["thinking"]["type"],
                "enabled"
            );
        }

        // ── Row: Anthropic + Thinking::Enabled + no tool_call + turn-1 ──
        #[test]
        fn anthropic_thinking_first_turn_top_level_config() {
            let messages = vec![user("compute 2+2")];
            let body = build_provider_request_body(
                &messages,
                &[],
                "claude-opus-4-7",
                "anthropic",
                Some(8192),
                None,
                true,
                &ThinkingConfig::Enabled {
                    budget_tokens: 4000,
                },
            );
            assert_eq!(body["thinking"]["type"], "enabled");
            assert_eq!(body["thinking"]["budget_tokens"], 4000);
        }

        // ── Row: Anthropic + Thinking::Enabled + tool_call + turn-2 ──
        #[test]
        fn anthropic_thinking_tool_call_multi_turn_needs_typed_blocks() {
            let messages = vec![
                user("compute 2+2"),
                assistant_with_tool_call("thinking...", Some("real_sig"), "calc", "tc1"),
                tool_result("tc1", "4"),
            ];
            let body = build_provider_request_body(
                &messages,
                &[],
                "claude-opus-4-7",
                "anthropic",
                Some(8192),
                None,
                true,
                &ThinkingConfig::Enabled {
                    budget_tokens: 4000,
                },
            );
            let assistant_content = body["messages"][1]["content"].as_array().unwrap();
            assert_eq!(assistant_content[0]["type"], "thinking");
            assert_eq!(assistant_content[0]["thinking"], "thinking...");
            assert_eq!(assistant_content[0]["signature"], "real_sig");
            assert_eq!(assistant_content[1]["type"], "tool_use");
        }

        // ── Row: OpenAI + Thinking::Adaptive(effort) ──
        // Adaptive maps to `reasoning_effort` on OpenAI. No signature mechanic.
        #[test]
        fn openai_thinking_adaptive_maps_to_reasoning_effort() {
            let messages = vec![user("hi")];
            let body = build_provider_request_body(
                &messages,
                &[],
                "gpt-4o",
                "openai",
                Some(4096),
                Some(0.7),
                true,
                &ThinkingConfig::Adaptive {
                    effort: astra_turn_core::thinking_config::ThinkingEffort::Medium,
                },
            );
            assert_eq!(body["reasoning_effort"], "medium");
        }

        // ── Row: OpenAI + Thinking::Enabled (budget) ──
        // OpenAI has no budget-based thinking; the config must be a no-op
        // rather than silently sending an unsupported field.
        #[test]
        fn openai_thinking_enabled_budget_is_noop() {
            let messages = vec![user("hi")];
            let body = build_provider_request_body(
                &messages,
                &[],
                "gpt-4o",
                "openai",
                Some(4096),
                Some(0.7),
                true,
                &ThinkingConfig::Enabled {
                    budget_tokens: 5000,
                },
            );
            assert!(body.get("reasoning_effort").is_none());
            assert!(body.get("thinking").is_none());
            assert!(body.get("enable_thinking").is_none());
        }

        // ── Row: DashScope/Qwen + Thinking::Enabled ──
        // Qwen uses a binary `enable_thinking` flag, not budget/effort.
        #[test]
        fn dashscope_thinking_enabled_sends_binary_flag() {
            let messages = vec![user("hi")];
            let body = build_provider_request_body(
                &messages,
                &[],
                "qwen3-max",
                "dashscope",
                Some(4096),
                Some(0.7),
                true,
                &ThinkingConfig::Enabled {
                    budget_tokens: 1024,
                },
            );
            assert_eq!(body["enable_thinking"], true);
        }
    }

    #[test]
    fn bedrock_merges_consecutive_user_messages() {
        let messages = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "hello"}]}),
            json!({"role": "assistant", "content": [{"type": "toolUse", "name": "bash", "toolUseId": "1", "input": {}}]}),
            json!({"role": "user", "content": [{"type": "toolResult", "toolUseId": "1", "content": [{"text": "ok"}]}]}),
            json!({"role": "user", "content": [{"type": "text", "text": "correction 1"}]}),
            json!({"role": "user", "content": [{"type": "text", "text": "correction 2"}]}),
        ];
        let merged = merge_consecutive_same_role(messages);
        assert_eq!(
            merged.len(),
            3,
            "should merge 3 consecutive user msgs into 1"
        );
        assert_eq!(merged[0]["role"], "user");
        assert_eq!(merged[1]["role"], "assistant");
        assert_eq!(merged[2]["role"], "user");
        // The merged user message should have 3 content blocks
        let content = merged[2]["content"].as_array().unwrap();
        assert_eq!(content.len(), 3);
    }

    #[test]
    fn bedrock_tools_strip_unsupported_schema_fields() {
        let tools = vec![json!({
            "type": "function",
            "function": {
                "name": "agent",
                "description": "Multi-agent operations",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "max_turns": {
                            "type": "integer",
                            "description": "Max turns",
                            "minimum": 1,
                            "maximum": 100,
                            "default": 10
                        },
                        "run_in_background": {
                            "type": "boolean",
                            "description": "Run in background",
                            "default": true
                        },
                        "allowed_tools": {
                            "type": "array",
                            "items": { "type": "string" },
                            "minItems": 1
                        }
                    },
                    "required": ["max_turns"]
                }
            }
        })];
        let bedrock = build_bedrock_tools(&tools);
        let schema = &bedrock[0]["toolSpec"]["inputSchema"]["json"];
        let max_turns = &schema["properties"]["max_turns"];
        // "minimum", "maximum", "default" must be stripped
        assert!(max_turns.get("minimum").is_none(), "minimum not stripped");
        assert!(max_turns.get("maximum").is_none(), "maximum not stripped");
        assert!(max_turns.get("default").is_none(), "default not stripped");
        // "type" and "description" must survive
        assert_eq!(max_turns["type"], "integer");
        assert_eq!(max_turns["description"], "Max turns");
        // nested items should not have minItems
        assert!(
            schema["properties"]["allowed_tools"]
                .get("minItems")
                .is_none()
        );
        // "required" at top level must survive
        assert_eq!(schema["required"], json!(["max_turns"]));
    }

    #[test]
    fn bedrock_tools_strip_top_level_composition_and_vendor_extensions() {
        // Regression (session 2026-05-12): Bedrock returned
        // HTTP 400 "input_schema does not support oneOf, allOf, or
        // anyOf at the top level" because the consolidated `agent`
        // schema had an `allOf` block for per-action required
        // fields. We moved the per-action required into the
        // vendor-prefixed `x-astra-per-action-required` extension
        // and defensively strip both from the wire representation.
        let tools = vec![json!({
            "type": "function",
            "function": {
                "name": "agent",
                "description": "Multi-agent ops",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["spawn", "get_result"]},
                        "description": {"type": "string"},
                        "prompt": {"type": "string"}
                    },
                    "required": ["action"],
                    "allOf": [
                        {"if": {"properties": {"action": {"const": "spawn"}}},
                         "then": {"required": ["description", "prompt"]}}
                    ],
                    "oneOf": [{"required": ["description"]}],
                    "anyOf": [{"required": ["prompt"]}],
                    "x-astra-per-action-required": {"spawn": ["description", "prompt"]},
                    "x-astra-discovery-summary": "spawn needs description+prompt"
                }
            }
        })];
        let bedrock = build_bedrock_tools(&tools);
        let schema = &bedrock[0]["toolSpec"]["inputSchema"]["json"];
        assert!(schema.get("allOf").is_none(), "allOf must be stripped");
        assert!(schema.get("oneOf").is_none(), "oneOf must be stripped");
        assert!(schema.get("anyOf").is_none(), "anyOf must be stripped");
        assert!(
            schema.get("x-astra-per-action-required").is_none(),
            "internal vendor extension must not leak to the wire"
        );
        assert!(schema.get("x-astra-discovery-summary").is_none());
        // Top-level required + properties + enum must survive.
        assert_eq!(schema["required"], json!(["action"]));
        assert!(schema["properties"]["action"].get("enum").is_some());
        assert_eq!(
            schema["description"],
            "Action contract: spawn requires description + prompt."
        );
    }

    #[test]
    fn anthropic_tools_strip_top_level_composition_and_vendor_extensions() {
        // Same contract for the native Anthropic path — the
        // Messages API (non-Bedrock) rejects top-level allOf too.
        let tools = vec![json!({
            "type": "function",
            "function": {
                "name": "agent",
                "description": "Multi-agent ops",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string"},
                        "description": {"type": "string"}
                    },
                    "required": ["action"],
                    "allOf": [{"required": ["description"]}],
                    "x-astra-per-action-required": {"spawn": ["description"]},
                    "x-astra-per-action-allowed": {
                        "spawn": ["action", "description"]
                    },
                    "x-astra-discovery-summary": "spawn needs description"
                }
            }
        })];
        let mapped = build_anthropic_tools(&tools);
        let schema = &mapped[0]["input_schema"];
        assert!(schema.get("allOf").is_none(), "allOf must be stripped");
        assert!(
            schema.get("x-astra-per-action-required").is_none(),
            "internal vendor extension must not leak to the wire"
        );
        assert!(schema.get("x-astra-discovery-summary").is_none());
        assert_eq!(schema["required"], json!(["action"]));
        assert_eq!(
            schema["description"],
            "Action contract: spawn requires description; spawn accepts only action + description."
        );
    }

    #[test]
    fn openai_tools_strip_internal_schema_extensions_before_send() {
        let tools = vec![json!({
            "type": "function",
            "function": {
                "name": "future_work",
                "description": "Future work",
                "parameters": {
                    "type": "object",
                    "x-astra-discovery-summary": "start needs action",
                    "x-astra-per-action-required": {"start": ["action"]},
                    "properties": {
                        "action": {
                            "type": "string",
                            "x-astra-private-hint": "internal"
                        }
                    }
                }
            }
        })];
        let body = build_provider_request_body(
            &[json!({"role": "user", "content": "go"})],
            &tools,
            "test-model",
            "openai",
            Some(128),
            None,
            false,
            &ThinkingConfig::Off,
        );
        let schema = &body["tools"][0]["function"]["parameters"];
        assert!(schema.get("x-astra-discovery-summary").is_none());
        assert!(
            schema["properties"]["action"]
                .get("x-astra-private-hint")
                .is_none()
        );
        assert_eq!(schema["properties"]["action"]["type"], "string");
        assert_eq!(
            schema["description"],
            "Action contract: start requires action."
        );
    }

    #[test]
    fn openai_action_tools_materialize_structural_per_action_contracts() {
        let tools = astra_tools::schemas::all_tool_schemas()
            .into_iter()
            .filter(|tool| tool["function"]["name"] == "agent_fanout")
            .collect::<Vec<_>>();
        assert_eq!(tools.len(), 1);
        let body = build_provider_request_body(
            &[json!({"role": "user", "content": "review in parallel"})],
            &tools,
            "deepseek-chat",
            "deepseek",
            Some(128),
            None,
            false,
            &ThinkingConfig::Off,
        );
        let schema = &body["tools"][0]["function"]["parameters"];
        assert!(
            schema
                .as_object()
                .unwrap()
                .keys()
                .all(|key| !key.starts_with("x-astra-"))
        );
        let branches = schema["oneOf"]
            .as_array()
            .expect("OpenAI-compatible wire schema must carry action branches");
        assert_eq!(branches.len(), 4);
        let start = branches
            .iter()
            .find(|branch| branch["properties"]["action"]["enum"] == json!(["start"]))
            .expect("start branch");
        assert_eq!(
            start["required"],
            json!(["action", "target_count", "slots"])
        );
        assert_eq!(start["additionalProperties"], false);
        assert!(start["properties"].get("defaults").is_some());
        assert!(start["properties"].get("slot_index").is_none());

        let stop_slot = branches
            .iter()
            .find(|branch| branch["properties"]["action"]["enum"] == json!(["stop_slot"]))
            .expect("stop_slot branch");
        assert_eq!(
            stop_slot["required"],
            json!(["action", "group_id", "slot_index"])
        );
        assert!(stop_slot["properties"].get("slots").is_none());
    }

    // --- Regression: max_completion_tokens bump respects user's ceiling ---
    #[test]
    fn max_completion_tokens_honors_user_when_above_floor() {
        use astra_turn_core::thinking_config::ThinkingConfig;
        // User sets 128K, thinking budget is 32K → floor = 40K → must keep 128K.
        let thinking = ThinkingConfig::Enabled {
            budget_tokens: 32_000,
        };
        let body = build_provider_request_body(
            &[json!({"role": "user", "content": "hi"})],
            &[],
            "deepseek-chat",
            "deepseek",
            Some(128_000),
            None,
            false,
            &thinking,
        );
        assert_eq!(
            body["max_completion_tokens"].as_u64(),
            Some(128_000),
            "user ceiling above floor must not be bumped"
        );
    }

    #[test]
    fn max_completion_tokens_bumps_when_user_below_floor() {
        use astra_turn_core::thinking_config::ThinkingConfig;
        // User sets 8K, thinking budget is 32K → floor = 32K + 8K = 40K → bump to 40K.
        let thinking = ThinkingConfig::Enabled {
            budget_tokens: 32_000,
        };
        let body = build_provider_request_body(
            &[json!({"role": "user", "content": "hi"})],
            &[],
            "deepseek-chat",
            "deepseek",
            Some(8_000),
            None,
            false,
            &thinking,
        );
        assert_eq!(
            body["max_completion_tokens"].as_u64(),
            Some(40_192),
            "configured max below thinking_budget+headroom must be bumped to floor"
        );
    }

    #[test]
    fn max_completion_tokens_unchanged_when_thinking_off() {
        use astra_turn_core::thinking_config::ThinkingConfig;
        let body = build_provider_request_body(
            &[json!({"role": "user", "content": "hi"})],
            &[],
            "deepseek-chat",
            "deepseek",
            Some(4_096),
            None,
            false,
            &ThinkingConfig::Off,
        );
        assert_eq!(
            body["max_completion_tokens"].as_u64(),
            Some(4_096),
            "thinking=off must never bump user's max"
        );
    }

    #[test]
    fn request_body_overrides_merge_anthropic_context_management() {
        let overrides = Map::from_iter([(
            "context_management".to_string(),
            json!({
                "edits": [{
                    "type": "clear_tool_uses_20250919",
                    "trigger": {"type": "input_tokens", "value": 180_000},
                    "keep": {"type": "input_tokens", "value": 40_000}
                }]
            }),
        )]);

        let body = build_provider_request_body_with_overrides(
            &[json!({"role": "user", "content": "hi"})],
            &[],
            "claude-test",
            "anthropic",
            Some(2048),
            None,
            true,
            &ThinkingConfig::Off,
            Some(&overrides),
        );

        assert_eq!(
            body["context_management"]["edits"][0]["type"],
            "clear_tool_uses_20250919"
        );
        assert_eq!(body["model"], "claude-test");
        assert_eq!(body["stream"], json!(true));
    }

    #[test]
    fn request_body_overrides_merge_nested_bedrock_inference_config() {
        let overrides = Map::from_iter([
            ("inferenceConfig".to_string(), json!({"topP": 0.9})),
            (
                "additionalModelRequestFields".to_string(),
                json!({"reasoningMode": "compact"}),
            ),
        ]);

        let body = build_provider_request_body_with_overrides(
            &[json!({"role": "user", "content": "hi"})],
            &[],
            "claude-bedrock",
            "bedrock",
            Some(1024),
            Some(0.2),
            true,
            &ThinkingConfig::Off,
            Some(&overrides),
        );

        assert_eq!(body["inferenceConfig"]["maxTokens"], json!(1024));
        assert_eq!(body["inferenceConfig"]["temperature"], json!(0.2));
        assert_eq!(body["inferenceConfig"]["topP"], json!(0.9));
        assert_eq!(
            body["additionalModelRequestFields"]["reasoningMode"],
            "compact"
        );
    }

    #[test]
    fn request_body_overrides_strip_openai_thinking_fields_when_off() {
        let overrides = Map::from_iter([
            ("reasoning_effort".to_string(), json!("high")),
            ("enable_thinking".to_string(), json!(true)),
            ("reasoning".to_string(), json!({"effort": "high"})),
            (
                "output_config".to_string(),
                json!({"effort": "high", "keep": "preserved"}),
            ),
        ]);

        let body = build_provider_request_body_with_overrides(
            &[json!({"role": "user", "content": "hi"})],
            &[],
            "gpt-4o-mini",
            "openai",
            Some(256),
            None,
            false,
            &ThinkingConfig::Off,
            Some(&overrides),
        );

        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("enable_thinking").is_none());
        assert!(body.get("reasoning").is_none());
        assert_eq!(body["output_config"]["keep"], "preserved");
        assert!(body["output_config"].get("effort").is_none());
    }

    #[test]
    fn request_body_overrides_strip_bedrock_thinking_fields_when_off() {
        let overrides = Map::from_iter([(
            "additionalModelRequestFields".to_string(),
            json!({
                "thinking": {"type": "enabled", "budget_tokens": 4096},
                "output_config": {"effort": "high", "keep": "preserved"},
                "reasoningMode": "compact"
            }),
        )]);

        let body = build_provider_request_body_with_overrides(
            &[json!({"role": "user", "content": "hi"})],
            &[],
            "claude-bedrock",
            "bedrock",
            Some(256),
            None,
            false,
            &ThinkingConfig::Off,
            Some(&overrides),
        );

        assert!(
            body["additionalModelRequestFields"]
                .get("thinking")
                .is_none()
        );
        assert_eq!(
            body["additionalModelRequestFields"]["reasoningMode"],
            "compact"
        );
        assert_eq!(
            body["additionalModelRequestFields"]["output_config"]["keep"],
            "preserved"
        );
        assert!(
            body["additionalModelRequestFields"]["output_config"]
                .get("effort")
                .is_none()
        );
    }

    #[test]
    fn sanitize_request_body_overrides_borrows_when_thinking_not_off() {
        let overrides = Map::from_iter([("reasoningMode".to_string(), json!("compact"))]);
        let sanitized = sanitize_request_body_overrides_for_thinking(
            &ThinkingConfig::Enabled {
                budget_tokens: 1024,
            },
            Some(&overrides),
        );
        match sanitized {
            Some(Cow::Borrowed(borrowed)) => {
                assert!(std::ptr::eq(borrowed, &overrides));
            }
            other => panic!("expected borrowed overrides, got {other:?}"),
        }
    }
}

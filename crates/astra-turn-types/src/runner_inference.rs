//! Shared public identity and control protocol for the Runner inference facet.
//!
//! Binding/control metadata contains public model facts and opaque identities.
//! Request/response chunks are private owner-scoped payload, with redacted Debug.
//! Local transport URLs, credentials, headers and file references never travel.
//! Protocol negotiation becomes selectable only after durable executor enrollment.

use std::num::{NonZeroU32, NonZeroU64};

use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

pub const RUNNER_INFERENCE_FRAME_BYTES: usize = 256 * 1024;
pub const RUNNER_INFERENCE_CHUNK_BYTES: usize = 32 * 1024;
pub const RUNNER_INFERENCE_ARTIFACT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceTerminalStatus {
    /// In provider-attempt records this means physical transport/framing
    /// completion only. Logical invocation success requires the canonical
    /// Server collector and an independently supplied logical terminal.
    Succeeded,
    Failed,
    Cancelled,
    DeliveryUnknown,
}

impl InferenceTerminalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::DeliveryUnknown => "delivery_unknown",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceUsage {
    pub input: crate::NormalizedPromptCacheUsage,
    pub output_tokens: u64,
}

/// Normalize usage from the OpenAI-compatible response dialect shared by the
/// Server collector and the local Runner. Field location determines whether
/// prompt cache buckets are inclusive or disjoint; model names are irrelevant.
pub fn normalize_openai_compatible_usage(
    usage: &serde_json::Map<String, serde_json::Value>,
) -> Option<InferenceUsage> {
    let read = |value: Option<&serde_json::Value>| {
        value.and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_i64().map(|value| value.max(0) as u64))
        })
    };
    if !usage.contains_key("prompt_tokens")
        && !usage.contains_key("completion_tokens")
        && (usage.contains_key("input_tokens")
            || usage.contains_key("output_tokens")
            || usage.contains_key("cache_read_input_tokens")
            || usage.contains_key("cache_creation_input_tokens"))
    {
        let fresh = read(usage.get("input_tokens"));
        let output = read(usage.get("output_tokens"));
        let cached = read(usage.get("cache_read_input_tokens")).unwrap_or(0);
        let creation = read(usage.get("cache_creation_input_tokens")).unwrap_or(0);
        if fresh.is_none() && output.is_none() && cached == 0 && creation == 0 {
            return None;
        }
        return Some(InferenceUsage {
            input: crate::NormalizedPromptCacheUsage::new(fresh.unwrap_or(0), cached, creation),
            output_tokens: output.unwrap_or(0),
        });
    }
    let prompt_total = read(usage.get("prompt_tokens"));
    let output = read(usage.get("completion_tokens")).unwrap_or(0);
    let deepseek_cached = read(usage.get("prompt_cache_hit_tokens"));
    let deepseek_fresh = read(usage.get("prompt_cache_miss_tokens"));
    if deepseek_cached.is_some() || deepseek_fresh.is_some() {
        let cached = deepseek_cached.unwrap_or(0);
        return Some(InferenceUsage {
            input: crate::NormalizedPromptCacheUsage::new(
                deepseek_fresh
                    .or_else(|| prompt_total.map(|total| total.saturating_sub(cached)))
                    .unwrap_or(0),
                cached,
                0,
            ),
            output_tokens: output,
        });
    }
    if prompt_total.is_none() && output == 0 {
        return None;
    }
    let prompt_total = prompt_total.unwrap_or(0);
    let details = usage
        .get("prompt_tokens_details")
        .and_then(serde_json::Value::as_object);
    let nested_cached = details.and_then(|details| read(details.get("cached_tokens")));
    let nested_creation =
        details.and_then(|details| read(details.get("cache_creation_input_tokens")));
    let cached = nested_cached
        .or_else(|| read(usage.get("cache_read_input_tokens")))
        .unwrap_or(0);
    let creation = nested_creation
        .or_else(|| read(usage.get("cache_creation_input_tokens")))
        .unwrap_or(0);
    let fresh = if nested_cached.is_some() || nested_creation.is_some() {
        prompt_total.saturating_sub(cached).saturating_sub(creation)
    } else {
        prompt_total
    };
    Some(InferenceUsage {
        input: crate::NormalizedPromptCacheUsage::new(fresh, cached, creation),
        output_tokens: output,
    })
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceUsageStatus {
    ProviderExact,
    ProviderPartial,
    #[default]
    Unavailable,
}

impl InferenceUsageStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProviderExact => "provider_exact",
            Self::ProviderPartial => "provider_partial",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceInvocationTerminal {
    pub status: InferenceTerminalStatus,
    pub usage: InferenceUsage,
    pub usage_status: InferenceUsageStatus,
    pub provider_response_id: Option<String>,
    pub error_kind: Option<String>,
    pub error_message: Option<String>,
}

impl std::fmt::Debug for InferenceInvocationTerminal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InferenceInvocationTerminal")
            .field("status", &self.status)
            .field("usage", &self.usage)
            .field("usage_status", &self.usage_status)
            .field(
                "provider_response_id_present",
                &self.provider_response_id.is_some(),
            )
            .field("error_present", &self.error_kind.is_some())
            .finish()
    }
}

impl InferenceInvocationTerminal {
    pub fn succeeded(usage: InferenceUsage, provider_response_id: Option<String>) -> Self {
        Self {
            status: InferenceTerminalStatus::Succeeded,
            usage,
            usage_status: InferenceUsageStatus::ProviderExact,
            provider_response_id,
            error_kind: None,
            error_message: None,
        }
    }

    pub fn validate_wire_bounds(&self) -> Result<(), &'static str> {
        if self
            .provider_response_id
            .as_ref()
            .is_some_and(|s| s.len() > 255)
            || self.error_kind.as_ref().is_some_and(|s| s.len() > 64)
            || self.error_message.as_ref().is_some_and(|s| s.len() > 4096)
        {
            return Err("inference terminal metadata exceeds bounds");
        }
        Ok(())
    }
}

pub fn inference_terminal_fingerprint(
    terminal: &InferenceInvocationTerminal,
) -> Result<String, serde_json::Error> {
    // Explicit sorted fields preserve the existing fingerprint format without
    // depending on serde_json's per-binary preserve_order feature unification.
    #[derive(Serialize)]
    struct Input {
        cache_creation_tokens: u64,
        cache_read_tokens: u64,
        fresh_input_tokens: u64,
    }
    #[derive(Serialize)]
    struct Usage {
        input: Input,
        output_tokens: u64,
    }
    #[derive(Serialize)]
    struct Fingerprint<'a> {
        error_kind: &'a Option<String>,
        error_message: &'a Option<String>,
        provider_response_id: &'a Option<String>,
        status: InferenceTerminalStatus,
        usage: Usage,
        usage_status: InferenceUsageStatus,
    }
    let payload = serde_json::to_vec(&Fingerprint {
        error_kind: &terminal.error_kind,
        error_message: &terminal.error_message,
        provider_response_id: &terminal.provider_response_id,
        status: terminal.status,
        usage: Usage {
            input: Input {
                cache_creation_tokens: terminal.usage.input.cache_creation_tokens,
                cache_read_tokens: terminal.usage.input.cache_read_tokens,
                fresh_input_tokens: terminal.usage.input.fresh_input_tokens,
            },
            output_tokens: terminal.usage.output_tokens,
        },
        usage_status: terminal.usage_status,
    })?;
    Ok(format!("{:x}", Sha256::digest(payload)))
}

pub fn runner_terminal_digest(
    terminal: &InferenceInvocationTerminal,
    response: &[u8],
) -> Result<RunnerInferenceDigest, serde_json::Error> {
    let fingerprint = inference_terminal_fingerprint(terminal)?;
    let response_digest = format!("{:x}", Sha256::digest(response));
    let encoded = serde_json::to_vec(&(fingerprint, response_digest, response.len()))?;
    Ok(RunnerInferenceDigest(format!(
        "{:x}",
        Sha256::digest(encoded)
    )))
}

/// Strict UTF-8 chunk. JSON escaping can expand it by at most six times, leaving
/// ample identity overhead inside the 256 KiB transport frame limit.
#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct RunnerInferenceChunkData(String);

impl RunnerInferenceChunkData {
    pub fn new(data: String) -> Result<Self, &'static str> {
        if data.is_empty() || data.len() > RUNNER_INFERENCE_CHUNK_BYTES {
            return Err("inference chunk exceeds byte bound or is empty");
        }
        Ok(Self(data))
    }
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl std::fmt::Debug for RunnerInferenceChunkData {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RunnerInferenceChunkData")
            .field("byte_len", &self.0.len())
            .finish()
    }
}

impl<'de> Deserialize<'de> for RunnerInferenceChunkData {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerInferencePayloadChunk {
    pub offset: u32,
    pub data: RunnerInferenceChunkData,
}

/// Header for one response transfer. ACK follows complete verified assembly and
/// durable custody, never this provisional transport header.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerInferenceTerminalTransfer {
    pub attempt: RunnerInferenceAttemptIdentity,
    pub terminal: InferenceInvocationTerminal,
    pub response_sha256: RunnerInferenceDigest,
    pub response_bytes: NonZeroU32,
    pub terminal_sha256: RunnerInferenceDigest,
}

/// Original framing order, with Done distinct from ordinary EOF. These events
/// do not perform agent, DSML, or tool interpretation.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum RunnerInferenceProviderEvent {
    Json(serde_json::Value),
    Done,
    Eof,
}

impl std::fmt::Debug for RunnerInferenceProviderEvent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Json(_) => "Json([REDACTED])",
            Self::Done => "Done",
            Self::Eof => "Eof",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "status_code",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum RunnerInferenceTransportStatus {
    Complete,
    Cancelled,
    Deadline,
    /// Runner could not materialize the configured credential before any
    /// provider I/O. This is action-required local state, not provider 401.
    CredentialUnavailable,
    /// The admitted immutable binding no longer resolves locally.
    BindingUnavailable,
    /// Runner rejected before its durable dispatch fence because its bounded
    /// local execution capacity was full.
    CapacityUnavailable,
    Transport,
    Protocol,
    Limit,
    ConsumerClosed,
    HttpStatus(u16),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerInferenceDeliveryEvidence {
    NotDispatched,
    MayHaveDispatched,
    ResponseHeaders,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerInferenceTransportTerminal {
    pub status: RunnerInferenceTransportStatus,
    pub delivery: RunnerInferenceDeliveryEvidence,
    pub provider_bytes: u64,
    pub events_delivered: u64,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerInferenceResponse {
    pub events: Vec<RunnerInferenceProviderEvent>,
    pub transport: RunnerInferenceTransportTerminal,
}

impl std::fmt::Debug for RunnerInferenceResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RunnerInferenceResponse")
            .field("event_count", &self.events.len())
            .field("transport", &self.transport)
            .finish()
    }
}

pub const RUNNER_INFERENCE_PROTOCOL_VERSION: u16 = 1;
const MAX_INFERENCE_ID_BYTES: usize = 64;
const MAX_MODEL_NAME_BYTES: usize = 255;

/// Opaque public reference, deliberately excluding URL/path syntax. Reconnect
/// generations are transport facts and do not appear in the binding identity.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct RunnerInferenceId(String);

impl RunnerInferenceId {
    pub fn new(value: impl Into<String>) -> Result<Self, &'static str> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_INFERENCE_ID_BYTES
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(
                "inference identity must be 1-64 ASCII letters, digits, hyphens or underscores",
            );
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for RunnerInferenceId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// Provider model identifiers are public request content, not a secret lookup.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct RunnerInferenceModelName(String);

impl RunnerInferenceModelName {
    pub fn new(value: impl Into<String>) -> Result<Self, &'static str> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_MODEL_NAME_BYTES
            || value.trim() != value
            || value.chars().any(char::is_control)
            || value.contains("://")
        {
            return Err("inference model name must be bounded public model text, not a URL");
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for RunnerInferenceModelName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerInferenceBindingIdentity {
    pub runner_id: RunnerInferenceId,
    pub journal_id: RunnerInferenceId,
    pub binding_id: RunnerInferenceId,
    pub binding_revision: NonZeroU64,
    pub profile_revision: NonZeroU64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerInferenceProtocol {
    #[serde(rename = "openai_chat_completions")]
    OpenAiChatCompletions,
}

/// Complete public definition for one revision. Principal/workspace/session
/// ownership is derived by Server, never accepted from this payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerInferenceBindingDefinition {
    pub identity: RunnerInferenceBindingIdentity,
    /// User-facing name chosen in local setup. This is deliberately separate
    /// from `model_name`, which is the exact provider wire identifier.
    pub display_name: RunnerInferenceModelName,
    /// Exact provider model identifier placed in the compiled request body.
    pub model_name: RunnerInferenceModelName,
    pub protocol: RunnerInferenceProtocol,
    pub context_window: NonZeroU32,
    pub max_output_tokens: NonZeroU32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunnerInferenceBindingChange {
    Publish {
        definition: RunnerInferenceBindingDefinition,
    },
    Disable {
        identity: RunnerInferenceBindingIdentity,
    },
}

impl RunnerInferenceBindingChange {
    pub fn identity(&self) -> &RunnerInferenceBindingIdentity {
        match self {
            Self::Publish { definition } => &definition.identity,
            Self::Disable { identity } => identity,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerInferenceBindingPublication {
    pub protocol_version: u16,
    pub operation_id: RunnerInferenceId,
    pub expected_publication_revision: u64,
    pub change: RunnerInferenceBindingChange,
}

/// Durable receipt for an exact publication operation. Replaying a receipt does
/// not make that historical revision current again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerInferenceBindingReceipt {
    pub operation_id: RunnerInferenceId,
    pub publication_revision: NonZeroU64,
    pub identity: RunnerInferenceBindingIdentity,
}

/// Owner-scoped immutable Astra artifact, never an arbitrary fetch URL. Content
/// hashes establish integrity; they do not authorize cross-owner access.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerInferenceArtifactReference {
    pub artifact_id: RunnerInferenceId,
    pub sha256: RunnerInferenceDigest,
    pub byte_len: NonZeroU64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct RunnerInferenceDigest(String);

impl RunnerInferenceDigest {
    pub fn new(value: impl Into<String>) -> Result<Self, &'static str> {
        let value = value.into();
        if value.len() != 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err("inference digest must be 64 lowercase hexadecimal characters");
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for RunnerInferenceDigest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerInferenceAttemptIdentity {
    pub user_id: String,
    pub scope: crate::InferenceInvocationScope,
    pub invocation_id: RunnerInferenceId,
    pub attempt_id: RunnerInferenceId,
    pub binding: RunnerInferenceBindingIdentity,
    pub request: RunnerInferenceArtifactReference,
}

/// One persisted grant, replayed verbatim. Socket delivery generations are not
/// durable start authority and cannot extend either deadline.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerInferenceDispatchGrant {
    pub attempt: RunnerInferenceAttemptIdentity,
    pub grant_id: RunnerInferenceId,
    pub process_boot_nonce: RunnerInferenceId,
    pub start_before_unix_ms: u64,
    pub deadline_unix_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerInferenceTerminalAck {
    pub attempt: RunnerInferenceAttemptIdentity,
    pub terminal_sha256: RunnerInferenceDigest,
}

/// Immutable proof that the Server obtained one exact terminal response from
/// a Runner.  It is intentionally transport-neutral and contains neither a
/// credential nor response bytes: a checkpoint may retain this receipt, while
/// recovery must re-open the owner-scoped artifact and verify its hash.
///
/// This proves custody only.  A receipt becomes consumption evidence only
/// when it is embedded in a canonical Agent Loop checkpoint after the normal
/// post-response path has absorbed the response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerInferenceContinuationReceipt {
    pub attempt: RunnerInferenceAttemptIdentity,
    pub terminal_sha256: RunnerInferenceDigest,
    pub response: RunnerInferenceArtifactReference,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerInferenceStartEvidence {
    FenceCommitted,
    ProviderStarted,
    ExpiredWithoutFence,
    CancelledWithoutFence,
    RejectedWithoutFence,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerInferenceRejection {
    InferenceUnsupported,
    ProtocolVersionUnsupported,
    ConnectionSuperseded,
    BindingIdentityMismatch,
    PublicationConflict,
    StorageUnavailable,
    InvalidEvidence,
    CapacityUnavailable,
}

/// Accepted is emitted only after authenticated durable executor enrollment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunnerInferenceNegotiation {
    Accepted {
        protocol_version: u16,
        delivery_generation: u64,
        max_artifact_bytes: u32,
        server_unix_ms: u64,
    },
    Unavailable {
        reason: RunnerInferenceRejection,
    },
}

impl RunnerInferenceNegotiation {
    pub fn accepted(version: u16, delivery_generation: u64, server_unix_ms: u64) -> Self {
        if version != RUNNER_INFERENCE_PROTOCOL_VERSION {
            Self::Unavailable {
                reason: RunnerInferenceRejection::ProtocolVersionUnsupported,
            }
        } else {
            Self::Accepted {
                protocol_version: version,
                delivery_generation,
                max_artifact_bytes: RUNNER_INFERENCE_ARTIFACT_BYTES as u32,
                server_unix_ms,
            }
        }
    }
}

/// A rejection is a control response, not a durable publication receipt. It
/// does not advance the publication revision or create an effective Offering.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerInferenceBindingRejection {
    pub operation_id: RunnerInferenceId,
    pub reason: RunnerInferenceRejection,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn terminal_digest_binds_native_framing_and_metadata_without_debug_payloads() {
        let mut terminal = InferenceInvocationTerminal::succeeded(
            InferenceUsage::default(),
            Some("private-response-id".into()),
        );
        terminal.error_message = Some("private-error-canary".into());
        let response = |framing| RunnerInferenceResponse {
            events: vec![
                RunnerInferenceProviderEvent::Json(json!({"content":"private-body-canary"})),
                framing,
            ],
            transport: RunnerInferenceTransportTerminal {
                status: RunnerInferenceTransportStatus::Complete,
                delivery: RunnerInferenceDeliveryEvidence::ResponseHeaders,
                provider_bytes: 32,
                events_delivered: 2,
            },
        };
        let done = response(RunnerInferenceProviderEvent::Done);
        let eof = response(RunnerInferenceProviderEvent::Eof);
        let done_bytes = serde_json::to_vec(&done).unwrap();
        assert!(
            String::from_utf8(done_bytes.clone())
                .unwrap()
                .contains("private-body-canary")
        );
        assert_ne!(
            runner_terminal_digest(&terminal, &done_bytes).unwrap(),
            runner_terminal_digest(&terminal, &serde_json::to_vec(&eof).unwrap()).unwrap()
        );
        for canary in [
            "private-response-id",
            "private-error-canary",
            "private-body-canary",
        ] {
            assert!(!format!("{terminal:?} {done:?}").contains(canary));
        }
        let before = runner_terminal_digest(&terminal, &done_bytes).unwrap();
        terminal.usage.output_tokens += 1;
        assert_ne!(
            before,
            runner_terminal_digest(&terminal, &done_bytes).unwrap()
        );
    }

    #[test]
    fn chunk_bound_accounts_for_worst_case_json_escape_expansion() {
        let data =
            RunnerInferenceChunkData::new("\0".repeat(RUNNER_INFERENCE_CHUNK_BYTES)).unwrap();
        let encoded = serde_json::to_vec(&RunnerInferencePayloadChunk {
            offset: u32::MAX,
            data,
        })
        .unwrap();
        assert!(encoded.len() + 4096 < RUNNER_INFERENCE_FRAME_BYTES);
        assert!(
            RunnerInferenceChunkData::new("x".repeat(RUNNER_INFERENCE_CHUNK_BYTES + 1)).is_err()
        );
        assert!(
            serde_json::from_value::<RunnerInferenceChunkData>(json!(
                "x".repeat(RUNNER_INFERENCE_CHUNK_BYTES + 1)
            ))
            .is_err()
        );
        assert!(RunnerInferenceChunkData::new(String::new()).is_err());
    }

    fn publication_json() -> serde_json::Value {
        json!({
            "protocol_version": 1,
            "operation_id": "operation-1",
            "expected_publication_revision": 0,
            "change": {
                "action": "publish",
                "definition": {
                    "identity": {"runner_id": "runner-1", "journal_id": "journal-1", "binding_id": "binding-1", "binding_revision": 1, "profile_revision": 1},
                    "display_name": "Work", "model_name": "public-model", "protocol": "openai_chat_completions",
                    "context_window": 8192, "max_output_tokens": 1024
                }
            }
        })
    }

    #[test]
    fn public_binding_roundtrip_carries_only_public_definition() {
        let value = publication_json();
        let publication: RunnerInferenceBindingPublication =
            serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(publication).unwrap(), value);
    }

    #[test]
    fn private_material_and_caller_claimed_authority_are_rejected_at_every_boundary() {
        for path in ["", "/change/definition", "/change/definition/identity"] {
            for forbidden in [
                "api_key",
                "endpoint_url",
                "headers",
                "credential_ref",
                "user_id",
                "workspace_id",
                "session_id",
                "connection_generation",
            ] {
                let mut value = publication_json();
                value
                    .pointer_mut(path)
                    .unwrap()
                    .as_object_mut()
                    .unwrap()
                    .insert(forbidden.into(), json!("canary-secret"));
                assert!(
                    serde_json::from_value::<RunnerInferenceBindingPublication>(value).is_err(),
                    "accepted {forbidden} at {path}"
                );
            }
        }
    }

    #[test]
    fn binding_identifiers_and_revisions_are_bounded_and_exact() {
        for invalid in [
            "",
            " runner",
            "runner/path",
            "https://private.invalid",
            "runner\n",
        ] {
            assert!(RunnerInferenceId::new(invalid).is_err());
        }
        assert!(RunnerInferenceId::new("r".repeat(65)).is_err());
        assert!(RunnerInferenceId::new("r".repeat(64)).is_ok());
        for field in ["binding_revision", "profile_revision"] {
            let mut value = publication_json();
            value["change"]["definition"]["identity"][field] = json!(0);
            assert!(serde_json::from_value::<RunnerInferenceBindingPublication>(value).is_err());
        }
        assert!(RunnerInferenceModelName::new("m".repeat(256)).is_err());
        assert!(RunnerInferenceModelName::new("https://private.invalid/v1").is_err());
    }

    #[test]
    fn enrollment_receipt_carries_exact_version_generation_limits_and_server_clock() {
        assert_eq!(
            RunnerInferenceNegotiation::accepted(1, 42, 1000),
            RunnerInferenceNegotiation::Accepted {
                protocol_version: 1,
                delivery_generation: 42,
                max_artifact_bytes: RUNNER_INFERENCE_ARTIFACT_BYTES as u32,
                server_unix_ms: 1000
            }
        );
        for version in [0, 2, u16::MAX] {
            assert_eq!(
                RunnerInferenceNegotiation::accepted(version, 42, 1000),
                RunnerInferenceNegotiation::Unavailable {
                    reason: RunnerInferenceRejection::ProtocolVersionUnsupported
                }
            );
        }
    }

    #[test]
    fn openai_usage_normalization_is_shared_and_disjoint() {
        let usage = json!({
            "prompt_tokens": 1000,
            "completion_tokens": 75,
            "prompt_tokens_details": {
                "cached_tokens": 800,
                "cache_creation_input_tokens": 100
            }
        });
        let normalized = normalize_openai_compatible_usage(usage.as_object().unwrap()).unwrap();
        assert_eq!(normalized.input.fresh_input_tokens, 100);
        assert_eq!(normalized.input.cache_read_tokens, 800);
        assert_eq!(normalized.input.cache_creation_tokens, 100);
        assert_eq!(normalized.output_tokens, 75);
    }
}

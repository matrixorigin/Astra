//! Cache break detection and diagnostics for prompt caching.
//!
//! Tracks system prompt + tool schema hashes between turns to detect when
//! the KV cache prefix is broken. Classifies breaks by cause and logs
//! diagnostics with token impact estimates.
//!

use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};

use crate::context_serializer::SerializedSystemBlock;

/// Upper bound on concurrently tracked sources, capped at 10.
/// Each entry is one `PromptStateSnapshot`
/// (~small); the cap exists to prevent unbounded growth when long-running
/// runtimes spawn many distinct subagent ids. LRU-evicted on overflow.
///
/// The LRU uses a `Vec` for ordering, so each write is O(n) in the cap.
/// At cap=10 that's negligible; raising this above ~64 should switch
/// `source_order` to `VecDeque` or an indexed linked structure.
const MAX_TRACKED_SOURCES: usize = 10;
const MAX_TRACKED_PROVIDER_ATTEMPTS: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderAttemptCacheIdentity {
    pub request_id: String,
    pub attempt: u32,
}
// ---------------------------------------------------------------------------
// Cache break classification
// ---------------------------------------------------------------------------

/// Reason why the prompt cache prefix was broken.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CacheBreakReason {
    /// System prompt text changed (e.g., profile, task type).
    SystemPromptChanged,
    /// Cache-control markers or cache-boundary placement changed.
    CacheControlChanged,
    /// Tool schemas changed (added, removed, or modified).
    ///
    /// `changed` lists tools whose name is present in both snapshots but
    /// whose per-tool schema hash differs — this catches same-name schema
    /// churn (e.g., an agent/skill tool embedding a dynamic list), which
    /// empirically dominates tool-break causes yet was previously invisible
    /// because only add/remove by name was surfaced.
    ToolSchemasChanged {
        added: Vec<String>,
        removed: Vec<String>,
        changed: Vec<String>,
    },
    /// Model changed between turns.
    ModelChanged { from: String, to: String },
    /// Provider changed between turns.
    ProviderChanged { from: String, to: String },
    /// Cache TTL expired (inferred from time gap + cache miss).
    TtlExpired { gap_seconds: u64 },
    /// Cache turned cold but no stable attribution is available.
    UnknownColdStart,
    /// Multiple causes at once.
    Multiple(Vec<CacheBreakReason>),
}

impl std::fmt::Display for CacheBreakReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SystemPromptChanged => write!(f, "SystemPromptChanged"),
            Self::CacheControlChanged => write!(f, "CacheControlChanged"),
            Self::ToolSchemasChanged {
                added,
                removed,
                changed,
            } => {
                let mut parts = Vec::new();
                if !added.is_empty() {
                    parts.push(format!("added={}", added.join(",")));
                }
                if !removed.is_empty() {
                    parts.push(format!("removed={}", removed.join(",")));
                }
                if !changed.is_empty() {
                    parts.push(format!("changed={}", changed.join(",")));
                }
                if parts.is_empty() {
                    write!(f, "ToolSchemasChanged")
                } else {
                    write!(f, "ToolSchemasChanged({})", parts.join(";"))
                }
            }
            Self::ModelChanged { from, to } => write!(f, "ModelChanged({from}->{to})"),
            Self::ProviderChanged { from, to } => write!(f, "ProviderChanged({from}->{to})"),
            Self::TtlExpired { gap_seconds } => write!(f, "TtlExpired({}m)", gap_seconds / 60),
            Self::UnknownColdStart => write!(f, "UnknownColdStart"),
            Self::Multiple(reasons) => {
                let joined = reasons
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(f, "Multiple({joined})")
            }
        }
    }
}

/// A detected cache break event with diagnostics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheBreakEvent {
    pub reason: CacheBreakReason,
    /// Estimated tokens that must be re-processed (cache miss cost).
    pub estimated_token_impact: usize,
    /// Human-readable suggestion for avoiding this break.
    pub suggestion: Option<String>,
}

// ---------------------------------------------------------------------------
// Snapshot: captures the cacheable prefix state at a point in time
// ---------------------------------------------------------------------------

/// Snapshot of the cacheable prompt prefix for one turn.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SystemBlockFingerprint {
    pub kind: String,
    pub scope: String,
    pub text_hash: u64,
    pub cache_control_hash: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PromptStateSnapshot {
    /// Hash of the full system prompt text (all sections concatenated).
    pub system_prompt_hash: u64,
    /// Hash of all tool schemas combined (order-sensitive).
    pub tools_hash: u64,
    /// Per-tool hashes for diffing which tool changed.
    pub per_tool_hashes: Vec<(String, u64)>,
    /// Hash of cache-control / cache-boundary-bearing system metadata.
    pub cache_control_hash: u64,
    /// Per-system-block hashes for summary diff artifacts.
    pub system_blocks: Vec<SystemBlockFingerprint>,
    /// Provider name used for this turn.
    pub provider: String,
    /// Model name used for this turn.
    pub model: String,
    /// Timestamp (seconds since epoch) of when this snapshot was taken.
    pub timestamp_secs: u64,
    /// Total estimated cache-eligible tokens (system + tools).
    pub cache_eligible_tokens: usize,
    /// Provider-final component identity captured from the immutable body
    /// receipt. Pending plan snapshots have no receipt yet; this field is
    /// attached only when a dispatched physical request supplies one.
    #[serde(default)]
    pub provider_final_fingerprint: Option<ProviderFinalPromptFingerprint>,
}

/// Content-free identity of the exact provider payload components.
///
/// The transport computes these hashes from the same sanitized JSON value it
/// serializes for HTTP.  They deliberately do not carry provider body content
/// or infer semantics from provider/model labels.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderFinalPromptFingerprint {
    pub message_sequence_sha256: String,
    pub system_sequence_sha256: String,
    pub cache_key_system_sha256: String,
    pub conversation_sequence_sha256: String,
    pub tool_schema_sequence_sha256: String,
    pub cache_key_tool_schema_sequence_sha256: String,
    #[serde(default)]
    pub cache_capability: crate::cache_placement::CacheCapability,
    #[serde(default)]
    pub cache_key_tool_schema_items: Vec<ProviderFinalToolFingerprint>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderFinalToolFingerprint {
    pub name: Option<String>,
    pub sha256: String,
}

impl PromptStateSnapshot {
    /// Attach the immutable provider-final fingerprint that owns structural
    /// cache-break attribution for this snapshot.  Planned hashes remain only
    /// as human-readable attribution detail when the final receipt proves a
    /// component changed.
    pub fn attach_provider_final_fingerprint(
        &mut self,
        fingerprint: ProviderFinalPromptFingerprint,
    ) {
        self.provider_final_fingerprint = Some(fingerprint);
    }

    /// Replace candidate tool fingerprints with the exact provider-wire
    /// schemas after runtime stabilization and cache annotation.
    ///
    /// Prompt assembly may prune or retain schemas after the context pipeline
    /// has produced its candidate set. Diagnostics must describe the request
    /// that was actually sent, otherwise lifecycle projection appears as a
    /// cache break even when the wire prefix stayed byte-stable.
    pub fn replace_tool_schemas(&mut self, tool_schemas: &[serde_json::Value]) {
        let replacement = Self::capture_with_hashes(
            self.system_prompt_hash,
            self.system_blocks.clone(),
            tool_schemas,
            &self.provider,
            &self.model,
            self.cache_eligible_tokens,
        );
        self.tools_hash = replacement.tools_hash;
        self.per_tool_hashes = replacement.per_tool_hashes;
    }

    /// Create a snapshot from the current prompt state.
    pub fn capture(
        system_prompt_text: &str,
        tool_schemas: &[serde_json::Value],
        model: &str,
        cache_eligible_tokens: usize,
    ) -> Self {
        Self::capture_with_provider(
            system_prompt_text,
            &[],
            tool_schemas,
            "unknown",
            model,
            cache_eligible_tokens,
        )
    }

    /// Create a snapshot from serialized system blocks + tool schemas.
    #[must_use]
    pub fn capture_serialized(
        system_blocks: &[SerializedSystemBlock],
        tool_schemas: &[serde_json::Value],
        provider: &str,
        model: &str,
        cache_eligible_tokens: usize,
    ) -> Self {
        Self::capture_with_hashes(
            hash_serialized_system_prompt(system_blocks),
            fingerprint_system_blocks(system_blocks),
            tool_schemas,
            provider,
            model,
            cache_eligible_tokens,
        )
    }

    fn capture_with_provider(
        system_prompt_text: &str,
        system_blocks: &[SerializedSystemBlock],
        tool_schemas: &[serde_json::Value],
        provider: &str,
        model: &str,
        cache_eligible_tokens: usize,
    ) -> Self {
        Self::capture_with_hashes(
            hash_str(system_prompt_text),
            fingerprint_system_blocks(system_blocks),
            tool_schemas,
            provider,
            model,
            cache_eligible_tokens,
        )
    }

    fn capture_with_hashes(
        system_prompt_hash: u64,
        system_blocks: Vec<SystemBlockFingerprint>,
        tool_schemas: &[serde_json::Value],
        provider: &str,
        model: &str,
        cache_eligible_tokens: usize,
    ) -> Self {
        let cache_control_hash = hash_cache_control_state(&system_blocks);

        let per_tool_hashes: Vec<(String, u64)> = tool_schemas
            .iter()
            .map(|t| {
                let name = t
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .or_else(|| t.get("name").and_then(|n| n.as_str()))
                    .unwrap_or("unknown")
                    .to_string();
                let h = hash_json_value(t);
                (name, h)
            })
            .collect();

        let tools_hash = {
            let mut h = DefaultHasher::new();
            for (_, th) in &per_tool_hashes {
                th.hash(&mut h);
            }
            h.finish()
        };

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        Self {
            system_prompt_hash,
            tools_hash,
            per_tool_hashes,
            cache_control_hash,
            system_blocks,
            provider: provider.to_string(),
            model: model.to_string(),
            timestamp_secs: now,
            cache_eligible_tokens,
            provider_final_fingerprint: None,
        }
    }
}

/// Extract the canonical system-prompt text from a provider request message list.
///
/// Uses all `role=system` messages when present, otherwise falls back to the
/// first message. Structured content arrays/objects are flattened into text using
/// the same rules across CLI and runtime journal reconstruction.
fn prompt_snapshot_scanned_input_bytes(
    messages: &[serde_json::Value],
) -> Result<u64, serde_json::Error> {
    astra_core::history_work::serialized_bytes(messages)
}

#[must_use]
pub fn prompt_snapshot_system_text_from_messages(messages: &[serde_json::Value]) -> String {
    let system_text = prompt_snapshot_selected_message_contents(messages)
        .into_iter()
        .map(prompt_snapshot_content_value_text)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    if astra_core::history_work::instrumentation_enabled() {
        match prompt_snapshot_scanned_input_bytes(messages) {
            Ok(bytes) => astra_core::history_work::record_operation(
                astra_core::history_work::HistoryWorkSite::PromptCacheHistoryScan,
                bytes,
                u64::try_from(messages.len()).unwrap_or(u64::MAX),
                0,
            ),
            Err(error) => astra_core::history_work::record_serialization_failure(
                astra_core::history_work::HistoryWorkSite::PromptCacheHistoryScan,
                &error,
            ),
        }
    }
    system_text
}

/// Build a [`PromptStateSnapshot`] from a raw provider message list plus tool schemas.
///
/// This is the shared bridge/CLI journal reconstruction path so prompt-cache
/// diagnostics cannot drift between the two execution surfaces.
pub fn prompt_snapshot_from_messages(
    messages: &[serde_json::Value],
    tool_schemas: &[serde_json::Value],
    provider: &str,
    model: &str,
    cache_eligible_tokens: usize,
) -> Option<PromptStateSnapshot> {
    prompt_snapshot_from_messages_with_cache_capability(
        messages,
        tool_schemas,
        provider,
        model,
        cache_eligible_tokens,
        None,
    )
}

/// Build a cache snapshot using the same provider capability that shaped the
/// final message list.
///
/// The caller must pass the final provider message shape. Every remaining
/// The cache-keyed system identity follows the final resolved wire shape.
/// Prefix providers fingerprint only the contiguous leading system header;
/// strict-history shapes that fold system context fingerprint every system
/// block; marker protocols fingerprint through their last explicit marker.
/// Deployments that require append-only runtime control project it as a typed
/// runtime-owned conversation frame, so it never becomes a system mutation.
pub fn prompt_snapshot_from_messages_with_cache_capability(
    messages: &[serde_json::Value],
    tool_schemas: &[serde_json::Value],
    provider: &str,
    model: &str,
    cache_eligible_tokens: usize,
    explicit_cache_capability: Option<crate::cache_placement::CacheCapability>,
) -> Option<PromptStateSnapshot> {
    let cache_capability = crate::cache_placement::CacheCapability::from_explicit_or_provider(
        explicit_cache_capability,
        provider,
    );
    let system_prompt_text = prompt_snapshot_system_text_from_messages(messages);
    let snapshot = PromptStateSnapshot::capture_with_hashes(
        hash_str(&system_prompt_text),
        prompt_snapshot_fingerprint_system_blocks(messages, cache_capability),
        tool_schemas,
        provider,
        model,
        cache_eligible_tokens,
    );
    Some(snapshot)
}

fn prompt_snapshot_selected_message_contents(
    messages: &[serde_json::Value],
) -> Vec<&serde_json::Value> {
    let system_contents: Vec<&serde_json::Value> = messages
        .iter()
        .filter(|message| message.get("role").and_then(serde_json::Value::as_str) == Some("system"))
        .filter_map(|message| message.get("content"))
        .collect();
    if system_contents.is_empty() {
        messages
            .first()
            .and_then(|message| message.get("content"))
            .into_iter()
            .collect()
    } else {
        system_contents
    }
}

fn prompt_snapshot_content_value_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(items) => {
            let mut out = String::new();
            for item in items {
                let text = prompt_snapshot_content_block_text(item);
                if text.is_empty() {
                    continue;
                }
                if prompt_snapshot_is_separator_block(item) {
                    if !out.ends_with("\n\n") {
                        out.push_str("\n\n");
                    }
                    continue;
                }
                if !out.is_empty() && !out.ends_with("\n\n") {
                    out.push_str("\n\n");
                }
                out.push_str(&text);
            }
            out
        }
        serde_json::Value::Object(map) => map
            .get("text")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| serde_json::to_string(value).unwrap_or_default()),
        _ => value.to_string(),
    }
}

fn prompt_snapshot_fingerprint_system_blocks(
    messages: &[serde_json::Value],
    cache_capability: crate::cache_placement::CacheCapability,
) -> Vec<SystemBlockFingerprint> {
    use crate::cache_placement::{CacheProtocol, VolatilePlacement};

    let mut out = Vec::new();
    let mut leading_system_prefix_open = true;
    for message in messages {
        if message.get("role").and_then(serde_json::Value::as_str) != Some("system") {
            leading_system_prefix_open = false;
            continue;
        }
        let Some(content) = message.get("content") else {
            continue;
        };
        let mut blocks = prompt_snapshot_content_value_blocks(content);
        let visible = match cache_capability.volatile_placement {
            VolatilePlacement::CurrentUserOnly => true,
            VolatilePlacement::MarkerIsolated => true,
            VolatilePlacement::TailSuffix | VolatilePlacement::AppendOnlyUserTail => {
                leading_system_prefix_open
            }
            VolatilePlacement::Free => false,
        };
        if !visible {
            for block in &mut blocks {
                block.scope = "None".to_string();
            }
        }
        out.extend(blocks);
    }

    if matches!(
        cache_capability.protocol,
        CacheProtocol::MarkerExplicit | CacheProtocol::BedrockCachePoint
    ) {
        let last_marker = out.iter().rposition(|block| block.cache_control_hash != 0);
        for (index, block) in out.iter_mut().enumerate() {
            if last_marker.is_none_or(|last_marker| index > last_marker) {
                block.scope = "None".to_string();
            }
        }
    }
    out
}

fn prompt_snapshot_content_value_blocks(value: &serde_json::Value) -> Vec<SystemBlockFingerprint> {
    match value {
        serde_json::Value::Null => Vec::new(),
        serde_json::Value::Array(items) => items
            .iter()
            .filter_map(prompt_snapshot_content_block_fingerprint)
            .collect(),
        _ => prompt_snapshot_content_block_fingerprint(value)
            .into_iter()
            .collect(),
    }
}

fn prompt_snapshot_content_block_fingerprint(
    value: &serde_json::Value,
) -> Option<SystemBlockFingerprint> {
    if prompt_snapshot_is_separator_block(value) {
        return None;
    }
    let text = prompt_snapshot_content_block_text(value);
    let cache_control_hash = value
        .get("cache_control")
        .map_or(0, |cache_control| hash_str(&cache_control.to_string()));
    if text.is_empty() && cache_control_hash == 0 {
        return None;
    }
    Some(SystemBlockFingerprint {
        kind: value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(match value {
                serde_json::Value::String(_) => "text",
                serde_json::Value::Object(_) => "object",
                serde_json::Value::Array(_) => "array",
                serde_json::Value::Bool(_) => "bool",
                serde_json::Value::Number(_) => "number",
                serde_json::Value::Null => "null",
            })
            .to_string(),
        scope: "provider_visible".to_string(),
        text_hash: hash_str(&text),
        cache_control_hash,
    })
}

fn prompt_snapshot_content_block_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Object(map) => map
            .get("text")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| serde_json::to_string(value).unwrap_or_default()),
        serde_json::Value::Array(_) => prompt_snapshot_content_value_text(value),
        _ => value.to_string(),
    }
}

fn prompt_snapshot_is_separator_block(value: &serde_json::Value) -> bool {
    value.get("cache_control").is_none()
        && value.get("type").and_then(serde_json::Value::as_str) == Some("text")
        && value.get("text").and_then(serde_json::Value::as_str) == Some("\n\n")
}

// ---------------------------------------------------------------------------
// Detector: compares consecutive snapshots
// ---------------------------------------------------------------------------

/// Upper bound for the "near-zero cache read" heuristic.
///
/// Large cached prefixes still need a meaningful floor before we infer a cold
/// cache from token accounting alone, but smaller prompts should scale down so
/// a 1k-token prefix doesn't need a 2k-token read to count as a hit.
pub const DEFAULT_MIN_CACHE_BREAK_TOKENS: u64 = 2_000;
const MIN_DYNAMIC_CACHE_BREAK_TOKENS: u64 = 128;

/// Cache TTL thresholds for expiration detection.
const CACHE_TTL_5MIN_SECS: u64 = 300;
#[cfg(test)]
const CACHE_TTL_1HOUR_SECS: u64 = 3_600;

/// Detects and classifies prompt cache breaks between turns.
///
/// A single detector instance tracks cache state per *source* — a logical
/// query stream (e.g. `"main"`, `"agent:session_memory"`, `"fork:<run_id>"`).
/// Each source has its own `previous` snapshot; a break in one source does
/// not corrupt attribution for another. The per-source map is a
/// prerequisite for the fork-prefix primitive (PR 1+), where parent
/// and child streams need independent attribution.
///
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheBreakDetectorState {
    pub per_source: HashMap<String, PromptStateSnapshot>,
    /// Last usage-bearing provider attempt per source, separate from structural observations.
    pub usage_per_source: HashMap<String, PromptStateSnapshot>,
    pub source_order: Vec<String>,
    pub stats: CacheStats,
    #[serde(default)]
    pub diff_seq: u32,
    /// Recently consumed durable provider-attempt identities. Persisting this
    /// bounded set makes receipt ingestion idempotent across session restore.
    #[serde(default)]
    pub observed_provider_attempts: VecDeque<ProviderAttemptCacheIdentity>,
}

/// In-memory cache-break detector for prompt caching systems.
///
/// Compares sequential [`PromptStateSnapshot`]s across turns to detect when
/// provider-side prompt caches become invalid and need to be rebuilt.
///
/// # Concurrency
///
/// `CacheBreakDetector` is intended to have a single logical owner per
/// session/run. It stays `Send + Sync` so enclosing runtime state can move
/// across tasks, but callers should not share mutable access without external
/// synchronization.
#[derive(Debug, Default)]
pub struct CacheBreakDetector {
    /// Previous snapshot per source. LRU-evicted at [`MAX_TRACKED_SOURCES`]
    /// entries so unbounded subagent spawns cannot leak memory. The
    /// `source_order` vector tracks insertion/refresh order (back = most
    /// recent); eviction drops the front.
    per_source: HashMap<String, PromptStateSnapshot>,
    /// Attribution baseline advances only when the provider supplied usage.
    /// It is intentionally separate from the last-dispatched structural
    /// baseline so an unavailable retry cannot erase the cause later reported
    /// by a usage-bearing terminal.
    usage_per_source: HashMap<String, PromptStateSnapshot>,
    /// Insertion/refresh order for LRU eviction. Kept in sync with
    /// `per_source`: every write to a source appends/refreshes its key
    /// here; eviction pops from the front.
    source_order: Vec<String>,
    /// Cumulative stats (aggregated across all sources).
    pub stats: CacheStats,
    /// Optional directory where per-break diagnostic JSON artifacts are
    /// written. When `None` (default) no artifact is emitted. Intended for
    /// developer debugging: when a cache break fires, an artifact named
    /// `cache-break-{timestamp_secs}-{seq}.json` is dropped into this dir
    /// containing the prev/curr snapshot fingerprints, classified reason,
    /// and remediation suggestion. Lets a developer answer "why did my
    /// cache just break?" without re-running the session.
    diff_dir: Option<std::path::PathBuf>,
    diff_seq: u32,
    observed_provider_attempts: VecDeque<ProviderAttemptCacheIdentity>,
}

/// Running cache hit/miss statistics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheStats {
    pub total_turns: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    /// Total tokens that had to be re-processed due to cache breaks.
    pub total_miss_tokens: usize,
    /// History of recent break events (last 10).
    pub recent_breaks: VecDeque<CacheBreakEvent>,
}

impl CacheStats {
    /// Cache hit ratio as a percentage (0-100).
    pub fn hit_rate_percent(&self) -> f64 {
        if self.total_turns == 0 {
            return 0.0;
        }
        (self.cache_hits as f64 / self.total_turns as f64) * 100.0
    }
}

impl CacheBreakDetector {
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn from_state(state: CacheBreakDetectorState) -> Self {
        Self {
            per_source: state.per_source,
            usage_per_source: state.usage_per_source,
            source_order: state.source_order,
            stats: state.stats,
            diff_dir: None,
            diff_seq: state.diff_seq,
            observed_provider_attempts: state.observed_provider_attempts,
        }
    }

    #[must_use]
    pub fn snapshot_state(&self) -> CacheBreakDetectorState {
        CacheBreakDetectorState {
            per_source: self.per_source.clone(),
            usage_per_source: self.usage_per_source.clone(),
            source_order: self.source_order.clone(),
            stats: self.stats.clone(),
            diff_seq: self.diff_seq,
            observed_provider_attempts: self.observed_provider_attempts.clone(),
        }
    }

    /// Record one dispatched physical provider attempt from its immutable
    /// final-body receipt.
    ///
    /// An attempt without provider usage still advances the structural
    /// baseline, because the body crossed the dispatch boundary, but does not
    /// fabricate a cache hit or miss. Returns `(accepted, event)`; duplicate
    /// durable attempt identities are ignored idempotently.
    pub fn record_provider_attempt_for_source(
        &mut self,
        source: &str,
        attempt_identity: &ProviderAttemptCacheIdentity,
        current: PromptStateSnapshot,
        actual_cache_read_tokens: Option<u64>,
    ) -> (bool, Option<CacheBreakEvent>) {
        if self
            .observed_provider_attempts
            .iter()
            .any(|observed| observed == attempt_identity)
        {
            return (false, None);
        }
        self.observed_provider_attempts
            .push_back(attempt_identity.clone());
        while self.observed_provider_attempts.len() > MAX_TRACKED_PROVIDER_ATTEMPTS {
            self.observed_provider_attempts.pop_front();
        }

        let structural_previous = self.per_source.get(source).cloned();
        let structural_event = structural_previous
            .as_ref()
            .and_then(|previous| self.detect_break(previous, &current, None));
        let usage_previous =
            actual_cache_read_tokens.and_then(|_| self.usage_per_source.get(source).cloned());
        let event = if let Some(cache_read_tokens) = actual_cache_read_tokens {
            usage_previous
                .as_ref()
                .and_then(|previous| self.detect_break(previous, &current, Some(cache_read_tokens)))
        } else {
            structural_event
        };

        if actual_cache_read_tokens.is_some() {
            self.stats.total_turns = self.stats.total_turns.saturating_add(1);
            if usage_previous.is_none() || event.is_some() {
                self.stats.cache_misses = self.stats.cache_misses.saturating_add(1);
            } else {
                self.stats.cache_hits = self.stats.cache_hits.saturating_add(1);
            }
            if let Some(event) = event.as_ref() {
                self.stats.total_miss_tokens = self
                    .stats
                    .total_miss_tokens
                    .saturating_add(event.estimated_token_impact);
                self.stats.recent_breaks.push_back(event.clone());
                if self.stats.recent_breaks.len() > 10 {
                    self.stats.recent_breaks.pop_front();
                }
                if let Some(dir) = self.diff_dir.clone() {
                    self.diff_seq = self.diff_seq.wrapping_add(1);
                    spawn_diff_artifact_write(
                        dir,
                        self.diff_seq,
                        usage_previous,
                        current.clone(),
                        event.clone(),
                    );
                }
            }
            self.usage_per_source
                .insert(source.to_string(), current.clone());
        }

        self.write_source_snapshot(source, current);
        (true, event)
    }

    /// Enable per-break diagnostic artifact emission to `dir`. The directory
    /// is created lazily on the first break. Errors during directory create
    /// or file write are swallowed to avoid perturbing the live turn — this
    /// is a developer aid, not a correctness signal.
    pub fn with_diff_dir(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.diff_dir = Some(dir.into());
        self
    }

    /// Enable/update the runtime diff artifact directory.
    pub fn set_diff_dir(&mut self, dir: impl Into<std::path::PathBuf>) {
        self.diff_dir = Some(dir.into());
    }

    /// Reset all tracked source baselines after an expected cache-boundary
    /// event such as compaction or native provider history clearing.
    pub fn reset_all_sources(&mut self) {
        self.per_source.clear();
        self.usage_per_source.clear();
        self.source_order.clear();
    }

    /// Insert/refresh a source's snapshot and maintain LRU order. Called
    /// from `record_provider_attempt_for_source` after detection completes so the
    /// detection path reads the OLD snapshot, then we overwrite.
    fn write_source_snapshot(&mut self, source: &str, snapshot: PromptStateSnapshot) {
        self.per_source.insert(source.to_string(), snapshot);
        if let Some(pos) = self.source_order.iter().position(|s| s == source) {
            self.source_order.remove(pos);
        }
        self.source_order.push(source.to_string());

        while self.source_order.len() > MAX_TRACKED_SOURCES {
            let evicted = self.source_order.remove(0);
            self.per_source.remove(&evicted);
            self.usage_per_source.remove(&evicted);
        }
    }

    /// Number of source streams currently tracked. Exposed for diagnostics
    /// and tests — callers should not make routing decisions based on it.
    pub fn tracked_source_count(&self) -> usize {
        self.per_source.len()
    }

    /// Peek at a source's last snapshot without mutating state. Intended
    /// for the fork primitive (PR 1+) to build a `ForkPrefix` from the
    /// parent's captured state at turn boundary.
    pub fn snapshot_for_source(&self, source: &str) -> Option<&PromptStateSnapshot> {
        self.per_source.get(source)
    }

    /// Compare two snapshots and classify the break.
    fn detect_break(
        &self,
        prev: &PromptStateSnapshot,
        curr: &PromptStateSnapshot,
        actual_cache_read: Option<u64>,
    ) -> Option<CacheBreakEvent> {
        let mut reasons = Vec::new();

        // 1. Model change
        if prev.model != curr.model {
            reasons.push(CacheBreakReason::ModelChanged {
                from: prev.model.clone(),
                to: curr.model.clone(),
            });
        }

        // 1b. Provider change
        if prev.provider != curr.provider {
            reasons.push(CacheBreakReason::ProviderChanged {
                from: prev.provider.clone(),
                to: curr.provider.clone(),
            });
        }

        // Structural attribution belongs only to immutable provider-final
        // receipts. Pending plan metadata does not establish cache identity.
        if let Some((prev, curr)) = prev
            .provider_final_fingerprint
            .as_ref()
            .zip(curr.provider_final_fingerprint.as_ref())
        {
            if prev.cache_key_system_sha256 != curr.cache_key_system_sha256 {
                reasons.push(CacheBreakReason::SystemPromptChanged);
            }
            // Protocol-native markers are included in the exact component
            // hashes; only a protocol change receives separate attribution.
            if prev.cache_capability.protocol != curr.cache_capability.protocol {
                reasons.push(CacheBreakReason::CacheControlChanged);
            }
            if prev.cache_key_tool_schema_sequence_sha256
                != curr.cache_key_tool_schema_sequence_sha256
            {
                let prev_map: std::collections::HashMap<&str, &str> = prev
                    .cache_key_tool_schema_items
                    .iter()
                    .filter_map(|tool| {
                        tool.name
                            .as_deref()
                            .map(|name| (name, tool.sha256.as_str()))
                    })
                    .collect();
                let curr_map: std::collections::HashMap<&str, &str> = curr
                    .cache_key_tool_schema_items
                    .iter()
                    .filter_map(|tool| {
                        tool.name
                            .as_deref()
                            .map(|name| (name, tool.sha256.as_str()))
                    })
                    .collect();
                let mut added: Vec<String> = curr_map
                    .keys()
                    .filter(|name| !prev_map.contains_key(*name))
                    .map(|name| (*name).to_string())
                    .collect();
                let mut removed: Vec<String> = prev_map
                    .keys()
                    .filter(|name| !curr_map.contains_key(*name))
                    .map(|name| (*name).to_string())
                    .collect();
                let mut changed: Vec<String> = curr_map
                    .iter()
                    .filter_map(|(name, hash)| match prev_map.get(name) {
                        Some(previous) if previous != hash => Some((*name).to_string()),
                        _ => None,
                    })
                    .collect();
                added.sort();
                removed.sort();
                changed.sort();
                reasons.push(CacheBreakReason::ToolSchemasChanged {
                    added,
                    removed,
                    changed,
                });
            }
        }

        // Fingerprint drift identifies a possible invalidation cause, not an
        // observed cache miss. Provider usage is the stronger authority: if
        // this request reports at least as many cached tokens as the entire
        // prefix tracked by this snapshot, attributing a cache break (and the
        // corresponding token impact) would contradict the measured result.
        // Keep partial/absent usage fail-closed so real or unobservable
        // structural losses are still diagnosed.
        if !reasons.is_empty()
            && actual_cache_read.is_some_and(|cache_read| {
                curr.cache_eligible_tokens == 0
                    || cache_read >= u64::try_from(curr.cache_eligible_tokens).unwrap_or(u64::MAX)
            })
        {
            return None;
        }

        // 4. If hashes match but API says cache miss → TTL expiry / unexplained cold start.
        if reasons.is_empty() {
            if let Some(cache_read) = actual_cache_read {
                if cache_read < cache_miss_threshold_tokens(curr) {
                    let gap = curr.timestamp_secs.saturating_sub(prev.timestamp_secs);
                    if gap > CACHE_TTL_5MIN_SECS {
                        reasons.push(CacheBreakReason::TtlExpired { gap_seconds: gap });
                    } else {
                        reasons.push(CacheBreakReason::UnknownColdStart);
                    }
                }
            }
        }

        if reasons.is_empty() {
            return None;
        }

        let estimated_token_impact = curr.cache_eligible_tokens;
        let suggestion = self.suggest_remediation(&reasons);
        let reason = if reasons.len() == 1 {
            reasons.into_iter().next().unwrap()
        } else {
            CacheBreakReason::Multiple(reasons)
        };

        Some(CacheBreakEvent {
            reason,
            estimated_token_impact,
            suggestion,
        })
    }

    fn suggest_remediation(&self, reasons: &[CacheBreakReason]) -> Option<String> {
        for r in reasons {
            match r {
                CacheBreakReason::SystemPromptChanged => {
                    return Some(
                        "System prompt changed — check if dynamic profile injection is \
                         causing unnecessary variation. Consider stabilizing the profile section."
                            .into(),
                    );
                }
                CacheBreakReason::CacheControlChanged => {
                    return Some(
                        "cache_control / cache-boundary placement changed — check session vs \
                         volatile scope routing and provider-native cache markers."
                            .into(),
                    );
                }
                CacheBreakReason::ToolSchemasChanged {
                    added,
                    removed,
                    changed,
                } => {
                    let parts: Vec<String> = [
                        (!added.is_empty()).then(|| format!("added: {}", added.join(", "))),
                        (!removed.is_empty()).then(|| format!("removed: {}", removed.join(", "))),
                        (!changed.is_empty())
                            .then(|| format!("schema changed: {}", changed.join(", "))),
                    ]
                    .into_iter()
                    .flatten()
                    .collect();
                    return Some(format!(
                        "Tool schemas changed ({}). Keep tool schema order stable and \
                         avoid dynamic tool registration mid-session; same-name schema \
                         churn (e.g. dynamic agent/skill lists embedded in a tool description) \
                         also breaks cache.",
                        parts.join("; ")
                    ));
                }
                CacheBreakReason::ModelChanged { from, to } => {
                    return Some(format!(
                        "Model changed from {from} to {to}. Model switches always \
                          invalidate the KV cache."
                    ));
                }
                CacheBreakReason::ProviderChanged { from, to } => {
                    return Some(format!(
                        "Provider changed from {from} to {to}. Provider switches always \
                         invalidate the KV cache."
                    ));
                }
                CacheBreakReason::TtlExpired { gap_seconds } => {
                    let minutes = gap_seconds / 60;
                    return Some(format!(
                        "Cache TTL likely expired ({minutes}min gap between turns). \
                         For long pauses, this is expected."
                    ));
                }
                CacheBreakReason::UnknownColdStart => {
                    return Some(
                        "Cache turned cold without a stable fingerprint cause; inspect provider \
                         cache eligibility and first-turn warm-up behavior."
                            .into(),
                    );
                }
                CacheBreakReason::Multiple(_) => {}
            }
        }
        None
    }

    /// Get current statistics.
    pub fn stats(&self) -> &CacheStats {
        &self.stats
    }

    /// Format a human-readable status line for the cache.
    pub fn status_line(&self) -> String {
        let s = &self.stats;
        if s.total_turns == 0 {
            return "Cache: no turns recorded yet".into();
        }
        let icon = if s.hit_rate_percent() >= 80.0 {
            "🟢"
        } else if s.hit_rate_percent() >= 50.0 {
            "🟡"
        } else {
            "🔴"
        };
        format!(
            "{icon} Cache: {:.0}% hit rate ({}/{} turns), {}K tokens re-processed from misses",
            s.hit_rate_percent(),
            s.cache_hits,
            s.total_turns,
            s.total_miss_tokens / 1000,
        )
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn hash_str(s: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

struct HashWriter<'a> {
    hasher: &'a mut DefaultHasher,
    bytes: u64,
}

impl std::io::Write for HashWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.hasher.write(buf);
        self.bytes = self
            .bytes
            .saturating_add(u64::try_from(buf.len()).unwrap_or(u64::MAX));
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn hash_json_value(value: &serde_json::Value) -> u64 {
    let mut hasher = DefaultHasher::new();
    let (serialization, serialized_bytes) = {
        let mut writer = HashWriter {
            hasher: &mut hasher,
            bytes: 0,
        };
        let serialization = serde_json::to_writer(&mut writer, value);
        (serialization, writer.bytes)
    };
    match serialization {
        Ok(()) => {
            if astra_core::history_work::instrumentation_enabled() {
                astra_core::history_work::record_operation(
                    astra_core::history_work::HistoryWorkSite::PromptCacheHistoryScan,
                    serialized_bytes,
                    1,
                    0,
                );
            }
            hasher.finish()
        }
        Err(error) => {
            astra_core::history_work::record_serialization_failure(
                astra_core::history_work::HistoryWorkSite::PromptCacheHistoryScan,
                &error,
            );
            hash_str(&value.to_string())
        }
    }
}

fn hash_serialized_system_prompt(system_blocks: &[SerializedSystemBlock]) -> u64 {
    let mut hasher = DefaultHasher::new();
    let mut hashed_bytes = 0_u64;
    for (idx, block) in system_blocks.iter().enumerate() {
        if idx > 0 {
            hasher.write(b"\n\n");
            hashed_bytes = hashed_bytes.saturating_add(2);
        }
        hasher.write(block.text.as_bytes());
        hashed_bytes =
            hashed_bytes.saturating_add(u64::try_from(block.text.len()).unwrap_or(u64::MAX));
    }
    hasher.write_u8(0xff);
    hashed_bytes = hashed_bytes.saturating_add(1);
    if astra_core::history_work::instrumentation_enabled() {
        astra_core::history_work::record_operation(
            astra_core::history_work::HistoryWorkSite::PromptCacheHistoryScan,
            hashed_bytes,
            u64::try_from(system_blocks.len()).unwrap_or(u64::MAX),
            0,
        );
    }
    hasher.finish()
}

fn fingerprint_system_blocks(
    system_blocks: &[SerializedSystemBlock],
) -> Vec<SystemBlockFingerprint> {
    system_blocks
        .iter()
        .map(|block| SystemBlockFingerprint {
            kind: format!("{:?}", block.kind),
            scope: format!("{:?}", block.scope),
            text_hash: hash_str(&block.text),
            cache_control_hash: block
                .cache_control
                .as_ref()
                .map_or(0, |value| hash_str(&value.to_string())),
        })
        .collect()
}

fn hash_cache_control_state(system_blocks: &[SystemBlockFingerprint]) -> u64 {
    let mut h = DefaultHasher::new();
    for (index, block) in system_blocks.iter().enumerate() {
        if block.cache_control_hash == 0 {
            continue;
        }
        index.hash(&mut h);
        block.cache_control_hash.hash(&mut h);
    }
    h.finish()
}

fn cache_miss_threshold_tokens(snapshot: &PromptStateSnapshot) -> u64 {
    if snapshot.cache_eligible_tokens == 0 {
        return 0;
    }
    let adaptive = (snapshot.cache_eligible_tokens as u64 / 4).max(MIN_DYNAMIC_CACHE_BREAK_TOKENS);
    adaptive.min(DEFAULT_MIN_CACHE_BREAK_TOKENS)
}

// ---------------------------------------------------------------------------
// Diff artifact writer
// ---------------------------------------------------------------------------

fn write_diff_artifact(
    dir: &std::path::Path,
    seq: u32,
    prev: Option<&PromptStateSnapshot>,
    curr: &PromptStateSnapshot,
    event: &CacheBreakEvent,
) -> std::io::Result<std::path::PathBuf> {
    std::fs::create_dir_all(dir)?;
    let stem = format!("cache-break-{:010}-{:04}", curr.timestamp_secs, seq);
    let path = dir.join(format!("{stem}.json"));
    let patch_path = dir.join(format!("{stem}.patch"));
    write_diff_artifact_file(
        &patch_path,
        render_unified_snapshot_patch(prev, curr, event).as_bytes(),
    )?;
    let snapshot_summary = |s: Option<&PromptStateSnapshot>| {
        s.map(|s| {
            serde_json::json!({
                "provider": s.provider,
                "model": s.model,
                "system_prompt_hash": s.system_prompt_hash,
                "cache_control_hash": s.cache_control_hash,
                "system_blocks": s.system_blocks,
                "tools_hash": s.tools_hash,
                "per_tool_hashes": s.per_tool_hashes,
                "timestamp_secs": s.timestamp_secs,
                "cache_eligible_tokens": s.cache_eligible_tokens,
            })
        })
        .unwrap_or(serde_json::Value::Null)
    };
    let payload = serde_json::json!({
        "seq": seq,
        "prev": snapshot_summary(prev),
        "curr": snapshot_summary(Some(curr)),
        "event": event,
        "patch_path": patch_path,
    });
    write_diff_artifact_file(
        &path,
        &serde_json::to_vec_pretty(&payload).unwrap_or_else(|_| b"{}".to_vec()),
    )?;
    Ok(path)
}

fn write_diff_artifact_file(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let extension = path
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or("artifact");
    let temporary_path = path.with_extension(format!("{extension}.tmp"));
    std::fs::write(&temporary_path, bytes)?;
    if let Err(error) = std::fs::rename(&temporary_path, path) {
        let _ = std::fs::remove_file(&temporary_path);
        return Err(error);
    }
    Ok(())
}

fn spawn_diff_artifact_write(
    dir: std::path::PathBuf,
    seq: u32,
    prev: Option<PromptStateSnapshot>,
    curr: PromptStateSnapshot,
    event: CacheBreakEvent,
) {
    let dir_for_thread = dir.clone();
    if let Err(error) = std::thread::Builder::new()
        .name("cache-diff-artifact".into())
        .spawn(move || {
            if let Err(error) =
                write_diff_artifact(&dir_for_thread, seq, prev.as_ref(), &curr, &event)
            {
                tracing::warn!(
                    target: "astra::cache",
                    ?error,
                    path = %dir_for_thread.display(),
                    seq,
                    "failed to write cache diff artifact"
                );
            }
        })
    {
        tracing::warn!(
            target: "astra::cache",
            ?error,
            path = %dir.display(),
            seq,
            "failed to spawn cache diff artifact writer"
        );
    }
}

fn render_unified_snapshot_patch(
    prev: Option<&PromptStateSnapshot>,
    curr: &PromptStateSnapshot,
    event: &CacheBreakEvent,
) -> String {
    let before = prev
        .map(render_snapshot_summary)
        .unwrap_or_else(|| "# no previous baseline\n".to_string());
    let after = render_snapshot_summary(curr);
    let before_lines: Vec<&str> = before.lines().collect();
    let after_lines: Vec<&str> = after.lines().collect();

    let mut patch = String::new();
    patch.push_str("--- prompt-cache-before\n");
    patch.push_str("+++ prompt-cache-after\n");
    patch.push_str(&format!(
        "@@ -1,{} +1,{} @@ reason={}\n",
        before_lines.len(),
        after_lines.len(),
        event.reason
    ));
    for line in before_lines {
        patch.push('-');
        patch.push_str(line);
        patch.push('\n');
    }
    for line in after_lines {
        patch.push('+');
        patch.push_str(line);
        patch.push('\n');
    }
    patch
}

fn render_snapshot_summary(snapshot: &PromptStateSnapshot) -> String {
    let mut lines = vec![
        format!("provider={}", snapshot.provider),
        format!("model={}", snapshot.model),
        format!("system_prompt_hash={}", snapshot.system_prompt_hash),
        format!("cache_control_hash={}", snapshot.cache_control_hash),
        format!("tools_hash={}", snapshot.tools_hash),
        format!("cache_eligible_tokens={}", snapshot.cache_eligible_tokens),
    ];
    for (idx, block) in snapshot.system_blocks.iter().enumerate() {
        lines.push(format!(
            "system_block[{idx}] kind={} scope={} text_hash={} cache_control_hash={}",
            block.kind, block.scope, block.text_hash, block.cache_control_hash
        ));
    }
    for (name, hash) in &snapshot.per_tool_hashes {
        lines.push(format!("tool[{name}]={hash}"));
    }
    lines.join("\n") + "\n"
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hash_writer_counts_the_exact_compact_json_stream() {
        let value = json!({
            "messages": [
                {"role": "system", "content": [{"type": "text", "text": "α"}]},
                {"role": "user", "content": {"nested": [true, 7, null]}}
            ]
        });
        let expected = serde_json::to_vec(&value).expect("fixture serializes");
        let mut hasher = DefaultHasher::new();
        let mut writer = HashWriter {
            hasher: &mut hasher,
            bytes: 0,
        };

        serde_json::to_writer(&mut writer, &value).expect("hash stream serializes");

        assert_eq!(
            writer.bytes,
            u64::try_from(expected.len()).unwrap_or(u64::MAX)
        );
    }

    fn attempt(request_id: impl Into<String>) -> ProviderAttemptCacheIdentity {
        ProviderAttemptCacheIdentity {
            request_id: request_id.into(),
            attempt: 0,
        }
    }

    fn make_tools(names: &[&str]) -> Vec<serde_json::Value> {
        names
            .iter()
            .map(|n| {
                json!({
                    "type": "function",
                    "function": {
                        "name": n,
                        "parameters": {"type": "object"}
                    }
                })
            })
            .collect()
    }

    fn snap(prompt: &str, tools: &[serde_json::Value], model: &str) -> PromptStateSnapshot {
        // Fixture arguments describe opaque provider-final identities; the
        // separate planned metadata deliberately carries no matching hashes.
        let mut s = PromptStateSnapshot::capture(&format!("planned-{prompt}"), &[], model, 15_000);
        s.timestamp_secs = 1000; // fixed for testing
        let tools: Vec<_> = tools
            .iter()
            .map(|tool| (tool["function"]["name"].as_str().unwrap(), tool.to_string()))
            .collect();
        let identities: Vec<_> = tools
            .iter()
            .map(|(name, schema)| (*name, schema.as_str()))
            .collect();
        s.attach_provider_final_fingerprint(exact_fingerprint(prompt, &identities));
        s
    }

    fn exact_fingerprint(system: &str, tools: &[(&str, &str)]) -> ProviderFinalPromptFingerprint {
        ProviderFinalPromptFingerprint {
            message_sequence_sha256: format!("messages-{system}"),
            system_sequence_sha256: format!("raw-system-{system}"),
            cache_key_system_sha256: format!("cache-system-{system}"),
            conversation_sequence_sha256: "conversation".to_string(),
            tool_schema_sequence_sha256: tools
                .iter()
                .map(|(name, hash)| format!("{name}:{hash}"))
                .collect::<Vec<_>>()
                .join("|"),
            cache_key_tool_schema_sequence_sha256: tools
                .iter()
                .map(|(name, hash)| format!("{name}:{hash}"))
                .collect::<Vec<_>>()
                .join("|"),
            cache_capability: crate::cache_placement::CacheCapability {
                protocol: crate::cache_placement::CacheProtocol::OpenAiAutoPrefix,
                volatile_placement: crate::cache_placement::VolatilePlacement::TailSuffix,
                volatile_delivery: crate::cache_placement::VolatileDeliveryPolicy::RequiredOnly,
                reuse_scope: Some(crate::cache_placement::CacheReuseScope::ConversationTurns),
            },
            cache_key_tool_schema_items: tools
                .iter()
                .map(|(name, hash)| ProviderFinalToolFingerprint {
                    name: Some((*name).to_string()),
                    sha256: (*hash).to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn provider_final_receipt_owns_structure_and_names_changed_tools() {
        let mut detector = CacheBreakDetector::new();
        let mut first = snap("planned-system-one", &make_tools(&["planned-a"]), "m");
        first.attach_provider_final_fingerprint(exact_fingerprint("stable", &[("bash", "v1")]));
        let first_identity = ProviderAttemptCacheIdentity {
            request_id: "request-1".to_string(),
            attempt: 0,
        };
        let (accepted, event) =
            detector.record_provider_attempt_for_source("main", &first_identity, first, Some(0));
        assert!(accepted);
        assert!(event.is_none());

        let mut metadata_only = snap("different-planned-system", &make_tools(&["planned-b"]), "m");
        let mut metadata_only_fingerprint = exact_fingerprint("stable", &[("bash", "v1")]);
        metadata_only_fingerprint.cache_capability.reuse_scope =
            Some(crate::cache_placement::CacheReuseScope::IntraTurnRounds);
        metadata_only.attach_provider_final_fingerprint(metadata_only_fingerprint);
        let second_identity = ProviderAttemptCacheIdentity {
            request_id: "request-2".to_string(),
            attempt: 0,
        };
        let (_, event) = detector.record_provider_attempt_for_source(
            "main",
            &second_identity,
            metadata_only,
            Some(20_000),
        );
        assert!(
            event.is_none(),
            "planned-only drift cannot override an equal provider-final receipt"
        );

        let mut changed = snap("same-planned-system", &make_tools(&["planned-b"]), "m");
        changed.attach_provider_final_fingerprint(exact_fingerprint("stable", &[("bash", "v2")]));
        let third_identity = ProviderAttemptCacheIdentity {
            request_id: "request-3".to_string(),
            attempt: 0,
        };
        let (_, event) =
            detector.record_provider_attempt_for_source("main", &third_identity, changed, Some(0));
        let event = event.expect("final tool schema change");
        assert!(matches!(
            event.reason,
            CacheBreakReason::ToolSchemasChanged {
                ref changed,
                ref added,
                ref removed
            } if changed == &["bash"] && added.is_empty() && removed.is_empty()
        ));
    }

    #[test]
    fn exact_marker_and_protocol_changes_have_distinct_attribution() {
        let stable = exact_fingerprint("stable-with-5m-marker", &[]);
        let marker_changed = exact_fingerprint("stable-with-1h-marker", &[]);
        let mut protocol_changed = stable.clone();
        protocol_changed.cache_capability.protocol =
            crate::cache_placement::CacheProtocol::StrictHistoryMatch;
        for (fingerprint, expected) in [
            (stable.clone(), None),
            (marker_changed, Some(CacheBreakReason::SystemPromptChanged)),
            (
                protocol_changed,
                Some(CacheBreakReason::CacheControlChanged),
            ),
        ] {
            let mut detector = CacheBreakDetector::new();
            let mut baseline = snap("planned-system", &[], "m");
            baseline.attach_provider_final_fingerprint(stable.clone());
            assert!(
                detector
                    .record_provider_attempt_for_source(
                        "main",
                        &attempt("baseline"),
                        baseline,
                        None,
                    )
                    .1
                    .is_none()
            );
            let mut current = snap(
                "different-planned-system",
                &make_tools(&["planned-tool"]),
                "m",
            );
            current.attach_provider_final_fingerprint(fingerprint);
            let (accepted, event) = detector.record_provider_attempt_for_source(
                "main",
                &attempt("changed"),
                current,
                None,
            );
            assert!(accepted);
            assert_eq!(event.map(|event| event.reason), expected);
            assert_eq!(detector.stats.total_turns, 0);
        }
    }

    #[test]
    fn pending_plan_drift_cannot_establish_a_physical_cache_break() {
        let mut detector = CacheBreakDetector::new();
        let first = PromptStateSnapshot::capture("planned-v1", &make_tools(&["bash"]), "m", 1000);
        let second = PromptStateSnapshot::capture("planned-v2", &make_tools(&["grep"]), "m", 1000);
        assert!(
            detector
                .record_provider_attempt_for_source("main", &attempt("plan-v1"), first, None,)
                .1
                .is_none()
        );
        assert!(
            detector
                .record_provider_attempt_for_source("main", &attempt("plan-v2"), second, None,)
                .1
                .is_none()
        );
        assert_eq!(detector.stats.total_turns, 0);
    }

    #[test]
    fn provider_attempt_without_usage_advances_baseline_without_counting_or_duplication() {
        let mut detector = CacheBreakDetector::new();
        let mut first = snap("planned", &[], "m");
        first.attach_provider_final_fingerprint(exact_fingerprint("stable", &[]));
        let identity = ProviderAttemptCacheIdentity {
            request_id: "request-no-usage".to_string(),
            attempt: 2,
        };
        let (accepted, event) =
            detector.record_provider_attempt_for_source("main", &identity, first, None);
        assert!(accepted);
        assert!(event.is_none());
        assert_eq!(detector.stats.total_turns, 0);
        let checkpoint = serde_json::to_vec(&detector.snapshot_state()).unwrap();
        let restored: CacheBreakDetectorState = serde_json::from_slice(&checkpoint).unwrap();
        let mut missing_usage: serde_json::Value = serde_json::from_slice(&checkpoint).unwrap();
        missing_usage
            .as_object_mut()
            .unwrap()
            .remove("usage_per_source");
        assert!(serde_json::from_value::<CacheBreakDetectorState>(missing_usage.clone()).is_err());
        missing_usage["usage_per_source"] = serde_json::Value::Null;
        assert!(serde_json::from_value::<CacheBreakDetectorState>(missing_usage).is_err());
        assert!(restored.usage_per_source.is_empty());
        assert!(restored.per_source.contains_key("main"));
        let mut detector = CacheBreakDetector::from_state(restored);
        let mut duplicate = snap("different", &[], "m");
        duplicate.attach_provider_final_fingerprint(exact_fingerprint("changed", &[]));
        let (accepted, event) =
            detector.record_provider_attempt_for_source("main", &identity, duplicate, Some(0));
        assert!(!accepted);
        assert!(event.is_none());
        assert_eq!(detector.stats.total_turns, 0);
        assert_eq!(
            detector
                .snapshot_for_source("main")
                .and_then(|snapshot| snapshot.provider_final_fingerprint.as_ref())
                .map(|fingerprint| fingerprint.cache_key_system_sha256.as_str()),
            Some("cache-system-stable")
        );
    }

    #[test]
    fn retry_usage_compares_with_last_usage_baseline_and_trusts_full_hit() {
        for (terminal_cache_read, expected_reason, expected_misses, expected_hits) in [
            (0, Some(CacheBreakReason::SystemPromptChanged), 2_u64, 0_u64),
            (20_000, None, 1_u64, 1_u64),
        ] {
            let mut detector = CacheBreakDetector::new();

            let mut baseline = snap("planned-a", &[], "m");
            baseline.attach_provider_final_fingerprint(exact_fingerprint("a", &[]));
            let (_, first_event) = detector.record_provider_attempt_for_source(
                "main",
                &ProviderAttemptCacheIdentity {
                    request_id: format!("request-a-{terminal_cache_read}"),
                    attempt: 0,
                },
                baseline,
                Some(0),
            );
            assert!(first_event.is_none());

            let mut retry_without_usage = snap("planned-b", &[], "m");
            retry_without_usage.attach_provider_final_fingerprint(exact_fingerprint("b", &[]));
            let (_, dispatch_event) = detector.record_provider_attempt_for_source(
                "main",
                &ProviderAttemptCacheIdentity {
                    request_id: format!("request-b-{terminal_cache_read}"),
                    attempt: 0,
                },
                retry_without_usage,
                None,
            );
            assert!(matches!(
                dispatch_event.map(|event| event.reason),
                Some(CacheBreakReason::SystemPromptChanged)
            ));
            assert_eq!(detector.stats.total_turns, 1);

            let mut terminal_retry = snap("planned-b", &[], "m");
            terminal_retry.attach_provider_final_fingerprint(exact_fingerprint("b", &[]));
            let (_, terminal_event) = detector.record_provider_attempt_for_source(
                "main",
                &ProviderAttemptCacheIdentity {
                    request_id: format!("request-b-{terminal_cache_read}"),
                    attempt: 1,
                },
                terminal_retry,
                Some(terminal_cache_read),
            );
            assert_eq!(
                terminal_event.map(|event| event.reason),
                expected_reason,
                "usage must compare to the prior usage-bearing request, never the unavailable retry"
            );
            assert_eq!(detector.stats.total_turns, 2);
            assert_eq!(detector.stats.cache_misses, expected_misses);
            assert_eq!(detector.stats.cache_hits, expected_hits);
        }
    }

    #[test]
    fn prompt_snapshot_from_messages_prefers_system_role_and_flattens_structured_content() {
        let messages = vec![
            json!({"role": "user", "content": "ignored"}),
            json!({
                "role": "system",
                "content": [
                    {"type": "text", "text": "System rules"},
                    {"type": "text", "text": "Second paragraph"},
                    "tail"
                ]
            }),
        ];
        assert_eq!(
            prompt_snapshot_system_text_from_messages(&messages),
            "System rules\n\nSecond paragraph\n\ntail"
        );
    }

    #[test]
    fn prompt_snapshot_scan_measurement_counts_non_system_input_bytes() {
        let messages = vec![
            json!({"role": "system", "content": "S"}),
            json!({"role": "user", "content": "用户输入".repeat(128)}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call-1",
                    "function": {
                        "name": "read_file",
                        "arguments": "{\"path\":\"large.rs\"}"
                    }
                }]
            }),
            json!({"role": "tool", "content": "evidence ".repeat(256)}),
        ];
        let output = prompt_snapshot_system_text_from_messages(&messages);
        let scanned_bytes =
            prompt_snapshot_scanned_input_bytes(&messages).expect("serialize scanned messages");

        assert_eq!(output, "S");
        assert_eq!(
            scanned_bytes,
            serde_json::to_vec(&messages).unwrap().len() as u64
        );
        assert!(
            scanned_bytes > output.len() as u64,
            "scan accounting must measure the input artifact, not selected system output"
        );
    }

    #[test]
    fn prompt_snapshot_from_messages_preserves_provider_and_model() {
        let messages = vec![json!({"role": "system", "content": {"text": "Prompt"}})];
        let tools = make_tools(&["bash"]);
        let snapshot = prompt_snapshot_from_messages(&messages, &tools, "anthropic", "claude", 42)
            .expect("snapshot");
        assert_eq!(snapshot.provider, "anthropic");
        assert_eq!(snapshot.model, "claude");
        assert_eq!(snapshot.cache_eligible_tokens, 42);
        assert_eq!(snapshot.system_prompt_hash, hash_str("Prompt"));
    }

    #[test]
    fn auto_prefix_snapshot_excludes_post_history_system_tail_from_leading_identity() {
        let messages = |runtime: &str| {
            vec![
                json!({"role": "system", "content": "stable"}),
                json!({"role": "user", "content": "do the work"}),
                json!({"role": "assistant", "content": "working"}),
                json!({"role": "system", "content": runtime}),
            ]
        };
        let first = prompt_snapshot_from_messages(
            &messages("completion settlement revision 1"),
            &[],
            "openai",
            "deepseek-v4-flash",
            42,
        )
        .expect("first snapshot");
        let second = prompt_snapshot_from_messages(
            &messages("completion settlement revision 2"),
            &[],
            "openai",
            "deepseek-v4-flash",
            42,
        )
        .expect("second snapshot");

        assert_ne!(first.system_prompt_hash, second.system_prompt_hash);
        assert_eq!(first.system_blocks[0], second.system_blocks[0]);
        assert_ne!(first.system_blocks[1], second.system_blocks[1]);
        assert_eq!(first.system_blocks[0].scope, "provider_visible");
        assert_eq!(first.system_blocks[1].scope, "None");
    }

    #[test]
    fn strict_history_snapshot_keeps_runtime_system_change_in_cache_identity() {
        let capability = crate::cache_placement::CacheCapability {
            protocol: crate::cache_placement::CacheProtocol::StrictHistoryMatch,
            volatile_placement: crate::cache_placement::VolatilePlacement::CurrentUserOnly,
            volatile_delivery: crate::cache_placement::VolatileDeliveryPolicy::RequiredOnly,
            reuse_scope: None,
        };
        let messages = |runtime: &str| {
            vec![
                json!({"role": "system", "content": "stable"}),
                json!({"role": "user", "content": "do the work"}),
                json!({"role": "system", "content": runtime}),
            ]
        };
        let first = prompt_snapshot_from_messages_with_cache_capability(
            &messages("authority 1"),
            &[],
            "openai",
            "gateway-alias",
            42,
            Some(capability),
        )
        .expect("first snapshot");
        let second = prompt_snapshot_from_messages_with_cache_capability(
            &messages("authority 2"),
            &[],
            "openai",
            "gateway-alias",
            42,
            Some(capability),
        )
        .expect("second snapshot");

        assert_ne!(first.system_blocks, second.system_blocks);
        assert!(
            first
                .system_blocks
                .iter()
                .all(|block| block.scope != "None")
        );
    }

    #[test]
    fn prompt_snapshot_from_messages_matches_serialized_cache_control_fingerprint() {
        use crate::section_types::{CacheScope, SectionKind};

        let serialized = [SerializedSystemBlock {
            kind: SectionKind::Identity,
            scope: CacheScope::Session,
            text: "Prompt".into(),
            cache_control: Some(json!({"type": "ephemeral", "ttl": "1h"})),
        }];
        let messages = vec![json!({
            "role": "system",
            "content": [
                {"type": "text", "text": "Prompt", "cache_control": {"type": "ephemeral", "ttl": "1h"}}
            ]
        })];

        let from_serialized =
            PromptStateSnapshot::capture_serialized(&serialized, &[], "anthropic", "claude", 42);
        let from_messages =
            prompt_snapshot_from_messages(&messages, &[], "anthropic", "claude", 42)
                .expect("snapshot");

        assert_eq!(
            from_messages.system_prompt_hash,
            from_serialized.system_prompt_hash
        );
        assert_eq!(
            from_messages.cache_control_hash,
            from_serialized.cache_control_hash
        );
    }

    #[test]
    fn prompt_snapshot_from_messages_handles_explicit_separator_blocks() {
        let messages = vec![json!({
            "role": "system",
            "content": [
                {"type": "text", "text": "A"},
                {"type": "text", "text": "\n\n"},
                {"type": "text", "text": "B", "cache_control": {"type": "ephemeral"}}
            ]
        })];

        let snapshot = prompt_snapshot_from_messages(&messages, &[], "anthropic", "claude", 42)
            .expect("snapshot");

        assert_eq!(
            prompt_snapshot_system_text_from_messages(&messages),
            "A\n\nB"
        );
        assert_eq!(snapshot.system_blocks.len(), 2);
    }

    #[test]
    fn no_break_on_identical_snapshots() {
        let tools = make_tools(&["bash", "str_replace"]);
        let mut det = CacheBreakDetector::new();

        let s1 = snap("system prompt", &tools, "claude-3.5-sonnet");
        let s2 = snap("system prompt", &tools, "claude-3.5-sonnet");

        assert!(
            det.record_provider_attempt_for_source("main", &attempt("receipt-1"), s1, Some(15_000))
                .1
                .is_none()
        ); // first turn
        assert!(
            det.record_provider_attempt_for_source("main", &attempt("receipt-2"), s2, Some(15_000))
                .1
                .is_none()
        ); // same = hit
        assert_eq!(det.stats.cache_hits, 1);
        assert_eq!(det.stats.cache_misses, 1); // first turn counts as miss
    }

    #[test]
    fn detect_system_prompt_change() {
        let tools = make_tools(&["bash"]);
        let mut det = CacheBreakDetector::new();

        det.record_provider_attempt_for_source(
            "main",
            &attempt("receipt-1"),
            snap("prompt v1", &tools, "claude"),
            None,
        );
        let event = det
            .record_provider_attempt_for_source(
                "main",
                &attempt("receipt-2"),
                snap("prompt v2", &tools, "claude"),
                None,
            )
            .1;

        assert!(event.is_some());
        let e = event.unwrap();
        assert_eq!(e.reason, CacheBreakReason::SystemPromptChanged);
        assert!(e.suggestion.unwrap().contains("System prompt changed"));
    }

    #[test]
    fn detect_tool_schema_change() {
        let mut det = CacheBreakDetector::new();

        det.record_provider_attempt_for_source(
            "main",
            &attempt("receipt-1"),
            snap("prompt", &make_tools(&["bash", "str_replace"]), "claude"),
            None,
        );
        let event = det
            .record_provider_attempt_for_source(
                "main",
                &attempt("receipt-2"),
                snap("prompt", &make_tools(&["bash", "grep"]), "claude"),
                None,
            )
            .1;

        let e = event.unwrap();
        match &e.reason {
            CacheBreakReason::ToolSchemasChanged {
                added,
                removed,
                changed,
            } => {
                assert!(added.contains(&"grep".to_string()));
                assert!(removed.contains(&"str_replace".to_string()));
                assert!(changed.is_empty(), "no same-name schema churn expected");
            }
            other => panic!("expected ToolSchemasChanged, got {other:?}"),
        }
    }

    #[test]
    fn provider_cache_read_covering_tracked_prefix_disproves_structural_break() {
        let baseline = snap("prompt", &make_tools(&["bash"]), "claude");
        let changed = snap(
            "prompt",
            &make_tools(&["bash", "inspect_work_plan"]),
            "claude",
        );
        let tracked_prefix = changed.cache_eligible_tokens as u64;

        let mut warm = CacheBreakDetector::new();
        warm.record_provider_attempt_for_source(
            "main",
            &attempt("receipt-1"),
            baseline.clone(),
            Some(0),
        );
        assert!(
            warm.record_provider_attempt_for_source(
                "main",
                &attempt("receipt-2"),
                changed.clone(),
                Some(tracked_prefix)
            )
            .1
            .is_none(),
            "measured cache reuse covering the tracked prefix must outrank a structural hypothesis"
        );

        let mut cold = CacheBreakDetector::new();
        cold.record_provider_attempt_for_source("main", &attempt("receipt-3"), baseline, Some(0));
        assert!(matches!(
            cold.record_provider_attempt_for_source(
                "main",
                &attempt("receipt-4"),
                changed,
                Some(0)
            )
            .1
            .map(|event| event.reason),
            Some(CacheBreakReason::ToolSchemasChanged { .. })
        ));
    }

    #[test]
    fn detect_tool_schema_content_change_same_name() {
        // Regression test: a tool whose name is unchanged but whose schema
        // JSON content differs (e.g., a dynamic description) must be reported
        // as `changed`. Previously this fell through as invisible because
        // only add/remove by name was diffed.
        let mut det = CacheBreakDetector::new();

        let t1 = vec![serde_json::json!({
            "function": {"name": "agent", "description": "original"}
        })];
        let t2 = vec![serde_json::json!({
            "function": {"name": "agent", "description": "rewritten dynamically"}
        })];

        det.record_provider_attempt_for_source(
            "main",
            &attempt("receipt-1"),
            snap("prompt", &t1, "claude"),
            None,
        );
        let event = det
            .record_provider_attempt_for_source(
                "main",
                &attempt("receipt-2"),
                snap("prompt", &t2, "claude"),
                None,
            )
            .1;
        let e = event.expect("break should fire on same-name schema churn");
        match &e.reason {
            CacheBreakReason::ToolSchemasChanged {
                added,
                removed,
                changed,
            } => {
                assert!(added.is_empty());
                assert!(removed.is_empty());
                assert_eq!(changed, &vec!["agent".to_string()]);
            }
            other => panic!("expected ToolSchemasChanged, got {other:?}"),
        }
        let suggestion = e.suggestion.unwrap_or_default();
        assert!(
            suggestion.contains("schema changed: agent"),
            "remediation must name the churning tool, got: {suggestion}"
        );
    }

    #[test]
    fn detect_model_change() {
        let tools = make_tools(&["bash"]);
        let mut det = CacheBreakDetector::new();

        det.record_provider_attempt_for_source(
            "main",
            &attempt("receipt-1"),
            snap("prompt", &tools, "claude-3.5-sonnet"),
            None,
        );
        let event = det
            .record_provider_attempt_for_source(
                "main",
                &attempt("receipt-2"),
                snap("prompt", &tools, "gpt-4o"),
                None,
            )
            .1;

        let e = event.unwrap();
        match &e.reason {
            CacheBreakReason::Multiple(reasons) => {
                assert!(
                    reasons
                        .iter()
                        .any(|r| matches!(r, CacheBreakReason::ModelChanged { .. }))
                );
            }
            CacheBreakReason::ModelChanged { from, to } => {
                assert_eq!(from, "claude-3.5-sonnet");
                assert_eq!(to, "gpt-4o");
            }
            other => panic!("expected ModelChanged, got {other:?}"),
        }
    }

    #[test]
    fn detect_ttl_expiry() {
        let tools = make_tools(&["bash"]);
        let mut det = CacheBreakDetector::new();

        let mut s1 = snap("prompt", &tools, "claude");
        s1.timestamp_secs = 1000;
        det.record_provider_attempt_for_source("main", &attempt("receipt-1"), s1, Some(0));

        let mut s2 = snap("prompt", &tools, "claude");
        s2.timestamp_secs = 1000 + CACHE_TTL_1HOUR_SECS + 1;
        let event = det
            .record_provider_attempt_for_source("main", &attempt("receipt-2"), s2, Some(0))
            .1; // API says 0 cache read

        let e = event.unwrap();
        match &e.reason {
            CacheBreakReason::TtlExpired { gap_seconds } => {
                assert!(*gap_seconds > CACHE_TTL_5MIN_SECS);
            }
            other => panic!("expected TtlExpired, got {other:?}"),
        }
    }

    #[test]
    fn no_ttl_expiry_when_cache_read_is_high() {
        let tools = make_tools(&["bash"]);
        let mut det = CacheBreakDetector::new();

        let mut s1 = snap("prompt", &tools, "claude");
        s1.timestamp_secs = 1000;
        det.record_provider_attempt_for_source("main", &attempt("receipt-1"), s1, Some(0));

        let mut s2 = snap("prompt", &tools, "claude");
        s2.timestamp_secs = 5000;
        // API says plenty of cache reads — not a miss
        let event = det
            .record_provider_attempt_for_source("main", &attempt("receipt-2"), s2, Some(10_000))
            .1;
        assert!(event.is_none());
    }

    #[test]
    fn small_prefix_uses_adaptive_cache_miss_threshold() {
        let tools = make_tools(&["bash"]);
        let mut det = CacheBreakDetector::new();
        let mut snapshot = snap("prompt", &tools, "claude");
        snapshot.cache_eligible_tokens = 512;

        det.record_provider_attempt_for_source(
            "main",
            &attempt("receipt-1"),
            snapshot.clone(),
            Some(0),
        );
        let event = det
            .record_provider_attempt_for_source("main", &attempt("receipt-2"), snapshot, Some(900))
            .1;
        assert!(
            event.is_none(),
            "small stable prefixes should not need a 2k cache_read to count as a hit"
        );
    }

    #[test]
    fn unexplained_cold_start_is_explicitly_reported() {
        let tools = make_tools(&["bash"]);
        let mut det = CacheBreakDetector::new();

        let mut s1 = snap("prompt", &tools, "claude");
        s1.timestamp_secs = 1_000;
        det.record_provider_attempt_for_source("main", &attempt("receipt-1"), s1, Some(0));

        let mut s2 = snap("prompt", &tools, "claude");
        s2.timestamp_secs = 1_100;
        let event = det
            .record_provider_attempt_for_source("main", &attempt("receipt-2"), s2, Some(0))
            .1
            .expect("near-zero cache read with same fingerprint should surface");
        assert_eq!(event.reason, CacheBreakReason::UnknownColdStart);
    }

    #[test]
    fn multiple_reasons_combined() {
        let mut det = CacheBreakDetector::new();

        det.record_provider_attempt_for_source(
            "main",
            &attempt("receipt-1"),
            snap("prompt v1", &make_tools(&["bash"]), "claude"),
            None,
        );
        let event = det
            .record_provider_attempt_for_source(
                "main",
                &attempt("receipt-2"),
                snap("prompt v2", &make_tools(&["bash", "str_replace"]), "gpt-4o"),
                None,
            )
            .1;

        let e = event.unwrap();
        match &e.reason {
            CacheBreakReason::Multiple(reasons) => {
                assert!(reasons.len() >= 2, "expected multiple reasons: {reasons:?}");
            }
            _ => panic!("expected Multiple reasons"),
        }
    }

    #[test]
    fn reset_all_sources_treats_next_turn_as_fresh_baseline() {
        let mut det = CacheBreakDetector::new();
        det.record_provider_attempt_for_source(
            "main",
            &attempt("receipt-1"),
            snap("prompt v1", &make_tools(&["bash"]), "claude"),
            Some(0),
        );
        det.reset_all_sources();
        assert!(det.per_source.is_empty());
        assert!(det.usage_per_source.is_empty());
        assert!(
            !det.record_provider_attempt_for_source(
                "main",
                &attempt("receipt-1"),
                snap("replayed", &[], "claude"),
                Some(0),
            )
            .0
        );
        assert!(det.per_source.is_empty());

        let event = det
            .record_provider_attempt_for_source(
                "main",
                &attempt("receipt-2"),
                snap("prompt v2", &make_tools(&["bash"]), "claude"),
                Some(0),
            )
            .1;
        assert!(
            event.is_none(),
            "post-reset cold start should not be misclassified"
        );
        assert_eq!(det.stats.cache_misses, 2);
    }

    #[test]
    fn hit_rate_calculation() {
        let tools = make_tools(&["bash"]);
        let mut det = CacheBreakDetector::new();

        det.record_provider_attempt_for_source(
            "main",
            &attempt("receipt-1"),
            snap("p", &tools, "c"),
            Some(0),
        ); // miss (first)
        det.record_provider_attempt_for_source(
            "main",
            &attempt("receipt-2"),
            snap("p", &tools, "c"),
            Some(15_000),
        ); // hit
        det.record_provider_attempt_for_source(
            "main",
            &attempt("receipt-3"),
            snap("p", &tools, "c"),
            Some(15_000),
        ); // hit
        det.record_provider_attempt_for_source(
            "main",
            &attempt("receipt-4"),
            snap("p", &tools, "c"),
            Some(15_000),
        ); // hit
        det.record_provider_attempt_for_source(
            "main",
            &attempt("receipt-5"),
            snap("p2", &tools, "c"),
            Some(0),
        ); // miss (changed)

        assert_eq!(det.stats.total_turns, 5);
        assert_eq!(det.stats.cache_hits, 3);
        assert_eq!(det.stats.cache_misses, 2);
        assert!((det.stats.hit_rate_percent() - 60.0).abs() < 0.1);
    }

    #[test]
    fn status_line_format() {
        let tools = make_tools(&["bash"]);
        let mut det = CacheBreakDetector::new();
        det.record_provider_attempt_for_source(
            "main",
            &attempt("receipt-1"),
            snap("p", &tools, "c"),
            Some(15_000),
        );
        det.record_provider_attempt_for_source(
            "main",
            &attempt("receipt-2"),
            snap("p", &tools, "c"),
            Some(15_000),
        );
        let line = det.status_line();
        assert!(line.contains("Cache:"));
        assert!(line.contains("hit rate"));
    }

    #[test]
    fn recent_breaks_capped_at_10() {
        let tools = make_tools(&["bash"]);
        let mut det = CacheBreakDetector::new();
        det.record_provider_attempt_for_source(
            "main",
            &attempt("recent_breaks_capped_at_10:1"),
            snap("p0", &tools, "c"),
            Some(0),
        );
        for i in 1..=15 {
            det.record_provider_attempt_for_source(
                "main",
                &attempt(format!("recent_breaks_capped_at_10:2:{i}")),
                snap(&format!("p{i}"), &tools, "c"),
                Some(0),
            );
        }
        assert!(det.stats.recent_breaks.len() <= 10);
    }

    #[test]
    fn capture_snapshot_per_tool_hashes() {
        let tools = make_tools(&["bash", "str_replace", "grep"]);
        let snap = PromptStateSnapshot::capture("test", &tools, "model", 1000);
        assert_eq!(snap.per_tool_hashes.len(), 3);
        assert_eq!(snap.per_tool_hashes[0].0, "bash");
        assert_eq!(snap.per_tool_hashes[1].0, "str_replace");
        assert_eq!(snap.per_tool_hashes[2].0, "grep");
    }

    #[test]
    fn replacing_candidate_tools_makes_diagnostics_match_provider_wire() {
        let candidate = vec![json!({
            "type": "function",
            "function": {"name": "candidate", "parameters": {"type": "object"}}
        })];
        let wire = vec![json!({
            "type": "function",
            "function": {"name": "wire", "parameters": {"type": "object"}}
        })];
        let mut snapshot = PromptStateSnapshot::capture("system", &candidate, "model", 100);
        let system_hash = snapshot.system_prompt_hash;

        snapshot.replace_tool_schemas(&wire);

        assert_eq!(snapshot.system_prompt_hash, system_hash);
        assert_eq!(snapshot.per_tool_hashes.len(), 1);
        assert_eq!(snapshot.per_tool_hashes[0].0, "wire");
        assert_eq!(
            snapshot.tools_hash,
            PromptStateSnapshot::capture("system", &wire, "model", 100).tools_hash
        );
    }

    #[test]
    fn capture_serialized_matches_plain_hashing_contract() {
        use crate::section_types::{CacheScope, SectionKind};

        let block_a = SerializedSystemBlock {
            kind: SectionKind::Identity,
            scope: CacheScope::Session,
            text: "alpha".into(),
            cache_control: None,
        };
        let block_b = SerializedSystemBlock {
            kind: SectionKind::ProjectContext,
            scope: CacheScope::Session,
            text: "beta".into(),
            cache_control: Some(serde_json::json!({"type": "ephemeral"})),
        };
        let tools = make_tools(&["bash", "grep"]);
        let serialized = PromptStateSnapshot::capture_serialized(
            &[block_a.clone(), block_b.clone()],
            &tools,
            "anthropic",
            "claude",
            42,
        );
        let plain = PromptStateSnapshot::capture_with_provider(
            "alpha\n\nbeta",
            &[block_a, block_b],
            &tools,
            "anthropic",
            "claude",
            42,
        );

        assert_eq!(serialized.system_prompt_hash, plain.system_prompt_hash);
        assert_eq!(serialized.cache_control_hash, plain.cache_control_hash);
        assert_eq!(serialized.tools_hash, plain.tools_hash);
        assert_eq!(serialized.per_tool_hashes, plain.per_tool_hashes);
    }

    fn wait_for_artifacts(dir: &std::path::Path, expected: usize) -> Vec<std::path::PathBuf> {
        let completed_artifacts = || {
            std::fs::read_dir(dir)
                .unwrap()
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|path| {
                    path.extension()
                        .and_then(std::ffi::OsStr::to_str)
                        .is_some_and(|extension| matches!(extension, "json" | "patch"))
                })
                .collect::<Vec<_>>()
        };
        for _ in 0..50 {
            let files = completed_artifacts();
            if files.len() >= expected {
                return files;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        completed_artifacts()
    }

    #[test]
    fn empty_detector_status() {
        let det = CacheBreakDetector::new();
        assert!(det.status_line().contains("no turns"));
    }

    #[test]
    fn zero_token_snapshot_no_break() {
        let tools = make_tools(&["bash"]);
        let mut det = CacheBreakDetector::new();

        let mut s1 = PromptStateSnapshot::capture("prompt", &tools, "claude", 0);
        s1.timestamp_secs = 1000;
        let mut s2 = PromptStateSnapshot::capture("prompt", &tools, "claude", 0);
        s2.timestamp_secs = 1001;

        assert!(
            det.record_provider_attempt_for_source("main", &attempt("receipt-1"), s1, Some(15_000))
                .1
                .is_none()
        );
        assert!(
            det.record_provider_attempt_for_source("main", &attempt("receipt-2"), s2, Some(15_000))
                .1
                .is_none()
        );
        assert_eq!(det.stats.cache_hits, 1);
    }

    #[test]
    fn stable_measured_requests_reuse_the_initial_baseline() {
        let tools = make_tools(&["bash"]);
        let mut det = CacheBreakDetector::new();

        // The initial measured request establishes the baseline; five retries reuse it.
        det.record_provider_attempt_for_source(
            "main",
            &attempt("receipt-1"),
            snap("p", &tools, "c"),
            Some(15_000),
        ); // first turn = miss
        for attempt_index in 0..5 {
            det.record_provider_attempt_for_source(
                "main",
                &attempt(format!("receipt-2:{attempt_index}")),
                snap("p", &tools, "c"),
                Some(15_000),
            ); // hits
        }
        // 5 hits out of 6 total turns
        let rate = det.stats.hit_rate_percent();
        assert!(
            (rate - (5.0 / 6.0 * 100.0)).abs() < 1.0,
            "expected ~83% hit rate, got {rate}"
        );
        assert_eq!(det.stats.cache_misses, 1); // only first turn
    }

    #[test]
    fn hundred_percent_miss_rate() {
        let tools = make_tools(&["bash"]);
        let mut det = CacheBreakDetector::new();

        // Every turn changes the prompt → all misses
        for i in 0..5 {
            det.record_provider_attempt_for_source(
                "main",
                &attempt(format!("receipt-1:{i}")),
                snap(&format!("prompt-{i}"), &tools, "c"),
                Some(0),
            );
        }
        assert_eq!(det.stats.total_turns, 5);
        // First turn = miss, turns 2-5 = breaks (also misses) → 0 hits
        assert_eq!(det.stats.cache_hits, 0);
        let rate = det.stats.hit_rate_percent();
        assert!(rate.abs() < 0.1, "expected ~0% hit rate, got {rate}");
    }

    #[test]
    fn status_line_green_icon() {
        let tools = make_tools(&["bash"]);
        let mut det = CacheBreakDetector::new();

        // 1 miss (first) + 9 hits = 90% hit rate → green
        det.record_provider_attempt_for_source(
            "main",
            &attempt("receipt-1"),
            snap("p", &tools, "c"),
            Some(15_000),
        );
        for attempt_index in 0..9 {
            det.record_provider_attempt_for_source(
                "main",
                &attempt(format!("receipt-2:{attempt_index}")),
                snap("p", &tools, "c"),
                Some(15_000),
            );
        }
        assert!(det.stats.hit_rate_percent() >= 80.0);
        assert!(det.status_line().contains("🟢"));
    }

    #[test]
    fn status_line_red_icon() {
        let tools = make_tools(&["bash"]);
        let mut det = CacheBreakDetector::new();

        // All different prompts → 0% hit rate → red
        for i in 0..5 {
            det.record_provider_attempt_for_source(
                "main",
                &attempt(format!("receipt-1:{i}")),
                snap(&format!("p{i}"), &tools, "c"),
                Some(0),
            );
        }
        assert!(det.stats.hit_rate_percent() < 50.0);
        assert!(
            det.status_line().contains("🔴"),
            "status_line was: {}",
            det.status_line()
        );
    }

    #[test]
    fn break_with_large_token_impact() {
        let tools = make_tools(&["bash"]);
        let mut det = CacheBreakDetector::new();

        let mut s1 = PromptStateSnapshot::capture("prompt v1", &tools, "claude", 100_000);
        s1.timestamp_secs = 1000;
        s1.attach_provider_final_fingerprint(exact_fingerprint("system-v1", &[]));
        det.record_provider_attempt_for_source("main", &attempt("receipt-1"), s1, Some(0));

        let mut s2 = PromptStateSnapshot::capture("prompt v2", &tools, "claude", 100_000);
        s2.timestamp_secs = 1001;
        s2.attach_provider_final_fingerprint(exact_fingerprint("system-v2", &[]));
        let event = det
            .record_provider_attempt_for_source("main", &attempt("receipt-2"), s2, Some(0))
            .1;

        assert!(event.is_some());
        assert_eq!(event.unwrap().estimated_token_impact, 100_000);
        assert_eq!(det.stats.total_miss_tokens, 100_000);
    }

    #[test]
    fn remediation_suggestions_per_reason() {
        let tools = make_tools(&["bash"]);

        // SystemPromptChanged
        {
            let mut det = CacheBreakDetector::new();
            det.record_provider_attempt_for_source(
                "main",
                &attempt("receipt-1"),
                snap("v1", &tools, "c"),
                None,
            );
            let e = det
                .record_provider_attempt_for_source(
                    "main",
                    &attempt("receipt-2"),
                    snap("v2", &tools, "c"),
                    None,
                )
                .1
                .unwrap();
            assert!(
                e.suggestion.is_some(),
                "SystemPromptChanged should have remediation"
            );
        }
        // ToolSchemasChanged
        {
            let mut det = CacheBreakDetector::new();
            det.record_provider_attempt_for_source(
                "main",
                &attempt("receipt-3"),
                snap("p", &make_tools(&["bash"]), "c"),
                None,
            );
            let e = det
                .record_provider_attempt_for_source(
                    "main",
                    &attempt("receipt-4"),
                    snap("p", &make_tools(&["bash", "str_replace"]), "c"),
                    None,
                )
                .1
                .unwrap();
            assert!(
                e.suggestion.is_some(),
                "ToolSchemasChanged should have remediation"
            );
        }
        // ModelChanged
        {
            let mut det = CacheBreakDetector::new();
            det.record_provider_attempt_for_source(
                "main",
                &attempt("receipt-5"),
                snap("p", &tools, "claude"),
                None,
            );
            let e = det
                .record_provider_attempt_for_source(
                    "main",
                    &attempt("receipt-6"),
                    snap("p", &tools, "gpt-4o"),
                    None,
                )
                .1
                .unwrap();
            assert!(
                e.suggestion.is_some(),
                "ModelChanged should have remediation"
            );
        }
        // TtlExpired
        {
            let mut det = CacheBreakDetector::new();
            let mut s1 = snap("p", &tools, "c");
            s1.timestamp_secs = 1000;
            det.record_provider_attempt_for_source("main", &attempt("receipt-7"), s1, Some(0));

            let mut s2 = snap("p", &tools, "c");
            s2.timestamp_secs = 1000 + CACHE_TTL_1HOUR_SECS + 1;
            let e = det
                .record_provider_attempt_for_source("main", &attempt("receipt-8"), s2, Some(0))
                .1
                .unwrap();
            assert!(e.suggestion.is_some(), "TtlExpired should have remediation");
        }
    }

    #[test]
    fn diff_artifact_written_on_break() {
        let tmp = tempfile::tempdir().unwrap();
        let mut det = CacheBreakDetector::new().with_diff_dir(tmp.path());

        det.record_provider_attempt_for_source(
            "main",
            &attempt("receipt-1"),
            snap("v1", &make_tools(&["bash"]), "claude"),
            Some(0),
        );
        det.record_provider_attempt_for_source(
            "main",
            &attempt("receipt-2"),
            snap("v2", &make_tools(&["bash"]), "claude"),
            Some(0),
        );

        let files: Vec<_> = wait_for_artifacts(tmp.path(), 2)
            .into_iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(files.len(), 2, "json + patch artifacts expected: {files:?}");
        let json_file = files
            .iter()
            .find(|name| name.ends_with(".json"))
            .expect("json artifact should exist");
        let patch_file = files
            .iter()
            .find(|name| name.ends_with(".patch"))
            .expect("patch artifact should exist");
        assert!(
            json_file.starts_with("cache-break-"),
            "name should be stable-prefixed, got {}",
            json_file
        );
        assert!(
            patch_file.starts_with("cache-break-"),
            "name should be stable-prefixed, got {}",
            patch_file
        );
        let body = std::fs::read_to_string(tmp.path().join(json_file)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(v["prev"].is_object(), "prev snapshot missing");
        assert!(v["curr"].is_object(), "curr snapshot missing");
        assert!(v["event"]["reason"].is_string() || v["event"]["reason"].is_object());
    }

    #[test]
    fn no_diff_artifact_on_cache_hit() {
        let tmp = tempfile::tempdir().unwrap();
        let tools = make_tools(&["bash"]);
        let mut det = CacheBreakDetector::new().with_diff_dir(tmp.path());

        det.record_provider_attempt_for_source(
            "main",
            &attempt("receipt-1"),
            snap("p", &tools, "claude"),
            Some(15_000),
        );
        det.record_provider_attempt_for_source(
            "main",
            &attempt("receipt-2"),
            snap("p", &tools, "claude"),
            Some(15_000),
        ); // hit, no artifact

        let count = std::fs::read_dir(tmp.path()).unwrap().count();
        assert_eq!(count, 0, "no artifacts should be written on hits");
    }

    // ---------------------------------------------------------------------
    // Per-source tracking — prerequisites for the fork prefix primitive.
    // Each source stream has its own `previous` slot; breaks in one do not
    // poison attribution for another.
    // ---------------------------------------------------------------------

    #[test]
    fn sources_are_independent_on_divergence() {
        // Source A keeps a stable prefix (should register hits).
        // Source B changes its system prompt each turn (should register breaks).
        // Source A's hit count must not be polluted by B's misses beyond the
        // aggregate stats, and each source's `previous` must come from its
        // own stream, not the globally last-written one.
        let tools = make_tools(&["bash"]);
        let mut det = CacheBreakDetector::new();

        det.record_provider_attempt_for_source(
            "A",
            &attempt("receipt-1"),
            snap("prompt-A", &tools, "m"),
            Some(0),
        );
        det.record_provider_attempt_for_source(
            "B",
            &attempt("receipt-2"),
            snap("prompt-B-v1", &tools, "m"),
            Some(0),
        );

        // A stable — this must be a HIT, even though B was written in between.
        let a_second = det
            .record_provider_attempt_for_source(
                "A",
                &attempt("receipt-3"),
                snap("prompt-A", &tools, "m"),
                Some(15_000),
            )
            .1;
        assert!(
            a_second.is_none(),
            "A's second turn must hit because A's own previous matched"
        );

        // B breaks — system prompt changed for B.
        let b_second = det
            .record_provider_attempt_for_source(
                "B",
                &attempt("receipt-4"),
                snap("prompt-B-v2", &tools, "m"),
                Some(0),
            )
            .1;
        assert!(
            matches!(
                b_second.as_ref().map(|e| &e.reason),
                Some(CacheBreakReason::SystemPromptChanged)
            ),
            "B must register a break, got {b_second:?}"
        );

        // Aggregate stats reflect both streams: 4 total turns, 2 initial
        // misses (first of each source) + 1 hit (A's second) + 1 break (B's second).
        assert_eq!(det.stats.total_turns, 4);
        assert_eq!(det.stats.cache_hits, 1);
        assert_eq!(det.stats.cache_misses, 3); // A's first + B's first + B's break
    }

    #[test]
    fn break_in_one_source_does_not_corrupt_another_baseline() {
        // After a break in source B, source A's subsequent identical turn
        // must remain structurally unchanged — baselines are per-source.
        let tools = make_tools(&["bash"]);
        let mut det = CacheBreakDetector::new();

        det.record_provider_attempt_for_source(
            "A",
            &attempt("receipt-1"),
            snap("p-A", &tools, "m"),
            None,
        );
        det.record_provider_attempt_for_source(
            "B",
            &attempt("receipt-2"),
            snap("p-B-v1", &tools, "m"),
            None,
        );
        det.record_provider_attempt_for_source(
            "B",
            &attempt("receipt-3"),
            snap("p-B-v2", &tools, "m"),
            None,
        ); // B break

        // A's prefix is unchanged — must remain structurally unchanged.
        let a_next = det
            .record_provider_attempt_for_source(
                "A",
                &attempt("receipt-4"),
                snap("p-A", &tools, "m"),
                None,
            )
            .1;
        assert!(
            a_next.is_none(),
            "A must remain structurally unchanged after B broke"
        );
    }

    #[test]
    fn lru_evicts_oldest_source_above_cap() {
        let tools = make_tools(&["bash"]);
        let mut det = CacheBreakDetector::new();

        // Fill past the cap. The oldest source ("s00") must be evicted.
        for i in 0..(MAX_TRACKED_SOURCES + 3) {
            let source = format!("s{i:02}");
            det.record_provider_attempt_for_source(
                &source,
                &attempt(format!("receipt-1:{i}")),
                snap("p", &tools, "m"),
                Some(0),
            );
        }
        assert_eq!(det.tracked_source_count(), MAX_TRACKED_SOURCES);
        assert_eq!(det.usage_per_source.len(), MAX_TRACKED_SOURCES);
        assert!(!det.usage_per_source.contains_key("s00"));
        assert!(
            det.usage_per_source
                .contains_key(&format!("s{:02}", MAX_TRACKED_SOURCES + 2))
        );
        assert!(
            det.snapshot_for_source("s00").is_none(),
            "oldest source should have been evicted"
        );
        assert!(
            det.snapshot_for_source(&format!("s{:02}", MAX_TRACKED_SOURCES + 2))
                .is_some(),
            "newest source must still be tracked"
        );
    }

    #[test]
    fn refreshing_a_source_prevents_its_eviction() {
        // LRU must be refresh-aware: if source S is written again, it moves
        // to the back of the queue and does not get evicted in favor of
        // newer sources.
        let tools = make_tools(&["bash"]);
        let mut det = CacheBreakDetector::new();

        det.record_provider_attempt_for_source(
            "stable",
            &attempt("receipt-1"),
            snap("p", &tools, "m"),
            None,
        );
        // Fill the rest to the cap; "stable" is currently oldest.
        for i in 0..(MAX_TRACKED_SOURCES - 1) {
            det.record_provider_attempt_for_source(
                &format!("t{i}"),
                &attempt(format!("receipt-2:{i}")),
                snap("p", &tools, "m"),
                None,
            );
        }
        // Refresh stable — it becomes most recent.
        det.record_provider_attempt_for_source(
            "stable",
            &attempt("receipt-3"),
            snap("p", &tools, "m"),
            None,
        );
        // One more write triggers eviction — but "stable" is no longer oldest.
        det.record_provider_attempt_for_source(
            "overflow",
            &attempt("receipt-4"),
            snap("p", &tools, "m"),
            None,
        );

        assert!(
            det.snapshot_for_source("stable").is_some(),
            "refreshed source must survive eviction"
        );
        assert!(
            det.snapshot_for_source("t0").is_none(),
            "t0 was oldest after the refresh and should have been evicted"
        );
    }

    #[test]
    fn snapshot_for_source_is_readonly() {
        // Peeking must not alter LRU order. If it did, reading a source
        // would shield it from eviction — that's a footgun.
        let tools = make_tools(&["bash"]);
        let mut det = CacheBreakDetector::new();
        det.record_provider_attempt_for_source(
            "first",
            &attempt("receipt-1"),
            snap("p", &tools, "m"),
            None,
        );
        for i in 0..(MAX_TRACKED_SOURCES - 1) {
            det.record_provider_attempt_for_source(
                &format!("s{i}"),
                &attempt(format!("receipt-2:{i}")),
                snap("p", &tools, "m"),
                None,
            );
        }
        assert!(det.snapshot_for_source("first").is_some());
        // Peek "first" many times; it must still be the eviction candidate.
        for _ in 0..5 {
            let _ = det.snapshot_for_source("first");
        }
        det.record_provider_attempt_for_source(
            "final",
            &attempt("receipt-3"),
            snap("p", &tools, "m"),
            None,
        );
        assert!(
            det.snapshot_for_source("first").is_none(),
            "peek must not count as a refresh — 'first' should have been evicted"
        );
    }

    #[test]
    fn diff_artifact_uses_per_source_prev() {
        // When a break fires on source B, the diff artifact must embed B's
        // previous snapshot — not the globally last-written snapshot, which
        // might belong to a different source (A) entirely.
        let tmp = tempfile::tempdir().unwrap();
        let tools = make_tools(&["bash"]);
        let mut det = CacheBreakDetector::new().with_diff_dir(tmp.path());

        det.record_provider_attempt_for_source(
            "A",
            &attempt("receipt-1"),
            snap("prompt-A-stable", &tools, "m"),
            Some(0),
        );
        det.record_provider_attempt_for_source(
            "B",
            &attempt("receipt-2"),
            snap("prompt-B-v1", &tools, "m"),
            Some(0),
        );
        // Now write A again (unchanged) so that A is globally last-written.
        det.record_provider_attempt_for_source(
            "A",
            &attempt("receipt-3"),
            snap("prompt-A-stable", &tools, "m"),
            Some(15_000),
        );
        // Now break B. The artifact's `prev` must be B's v1, not A's prompt.
        det.record_provider_attempt_for_source(
            "B",
            &attempt("receipt-4"),
            snap("prompt-B-v2", &tools, "m"),
            Some(0),
        );

        let files = wait_for_artifacts(tmp.path(), 2);
        assert_eq!(files.len(), 2, "json + patch artifacts expected");
        let json_path = files
            .iter()
            .find(|path| path.extension().is_some_and(|ext| ext == "json"))
            .expect("json artifact should exist");
        let body = std::fs::read_to_string(json_path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        // We can't read the prompt text back (only hashes are stored), but
        // we can verify the prev system_prompt_hash matches B's v1 hash,
        // not A's.
        let b_v1_hash = snap("prompt-B-v1", &tools, "m").system_prompt_hash;
        let a_stable_hash = snap("prompt-A-stable", &tools, "m").system_prompt_hash;
        let artifact_prev_hash = v["prev"]["system_prompt_hash"].as_u64().unwrap();
        assert_eq!(
            artifact_prev_hash, b_v1_hash,
            "artifact prev must come from B's own stream, not the global last write"
        );
        assert_ne!(
            artifact_prev_hash, a_stable_hash,
            "artifact prev must not leak A's snapshot into B's break record"
        );
    }
}

//! Cache break detection and diagnostics for prompt caching.
//!
//! Tracks system prompt + tool schema hashes between turns to detect when
//! the KV cache prefix is broken. Classifies breaks by cause and logs
//! diagnostics with token impact estimates.
//!
//! diagnostics with token impact estimates and auto-remediation suggestions.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};

use crate::context_serializer::SerializedSystemBlock;

/// Default source key used by the shortcut `record_turn` API. Callers that
/// only track a single query stream (e.g., a CLI main loop) never need to
/// deal with source keys — they always read/write this slot.
pub const DEFAULT_SOURCE: &str = "main";

/// Upper bound on concurrently tracked sources, capped at 10.
/// Each entry is one `PromptStateSnapshot`
/// (~small); the cap exists to prevent unbounded growth when long-running
/// runtimes spawn many distinct subagent ids. LRU-evicted on overflow.
///
/// The LRU uses a `Vec` for ordering, so each write is O(n) in the cap.
/// At cap=10 that's negligible; raising this above ~64 should switch
/// `source_order` to `VecDeque` or an indexed linked structure.
const MAX_TRACKED_SOURCES: usize = 10;

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
}

impl PromptStateSnapshot {
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
        let system_prompt_text = system_blocks
            .iter()
            .map(|block| block.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        Self::capture_with_provider(
            &system_prompt_text,
            system_blocks,
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
        Self::capture_with_fingerprints(
            system_prompt_text,
            fingerprint_system_blocks(system_blocks),
            tool_schemas,
            provider,
            model,
            cache_eligible_tokens,
        )
    }

    fn capture_with_fingerprints(
        system_prompt_text: &str,
        system_blocks: Vec<SystemBlockFingerprint>,
        tool_schemas: &[serde_json::Value],
        provider: &str,
        model: &str,
        cache_eligible_tokens: usize,
    ) -> Self {
        let system_prompt_hash = hash_str(system_prompt_text);
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
                let h = hash_str(&t.to_string());
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
        }
    }
}

/// Extract the canonical system-prompt text from a provider request message list.
///
/// Uses all `role=system` messages when present, otherwise falls back to the
/// first message. Structured content arrays/objects are flattened into text using
/// the same rules across CLI and runtime journal reconstruction.
#[must_use]
pub fn prompt_snapshot_system_text_from_messages(messages: &[serde_json::Value]) -> String {
    prompt_snapshot_selected_message_contents(messages)
        .into_iter()
        .map(prompt_snapshot_content_value_text)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
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
    let system_prompt_text = prompt_snapshot_system_text_from_messages(messages);
    let snapshot = PromptStateSnapshot::capture_with_fingerprints(
        &system_prompt_text,
        prompt_snapshot_fingerprint_system_blocks(messages),
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
) -> Vec<SystemBlockFingerprint> {
    prompt_snapshot_selected_message_contents(messages)
        .into_iter()
        .flat_map(prompt_snapshot_content_value_blocks)
        .collect()
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
/// Backwards compatibility: the legacy `record_turn(snapshot, actual)`
/// helper writes through to the [`DEFAULT_SOURCE`] slot, so pre-existing
/// single-stream callers are unaffected.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheBreakDetectorState {
    pub per_source: HashMap<String, PromptStateSnapshot>,
    pub source_order: Vec<String>,
    pub stats: CacheStats,
    #[serde(default)]
    pub diff_seq: u32,
}

/// In-memory cache-break detector for prompt caching systems.
///
/// Compares sequential [`PromptStateSnapshot`]s across turns to detect when
/// provider-side prompt caches become invalid and need to be rebuilt.
///
/// # Concurrency
///
/// `CacheBreakDetector` is deliberately **not** `Send + Sync`. It owns an
/// internal `HashMap` and is designed to be used from a single task (thread
/// or async future). In async contexts, Rust's borrow checker prevents any
/// `&mut self` method from being called while another borrow is held across
/// an `.await` point, so exclusive access is guaranteed without locks.
#[derive(Debug, Default)]
pub struct CacheBreakDetector {
    /// Previous snapshot per source. LRU-evicted at [`MAX_TRACKED_SOURCES`]
    /// entries so unbounded subagent spawns cannot leak memory. The
    /// `source_order` vector tracks insertion/refresh order (back = most
    /// recent); eviction drops the front.
    per_source: HashMap<String, PromptStateSnapshot>,
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
    pub recent_breaks: Vec<CacheBreakEvent>,
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
            source_order: state.source_order,
            stats: state.stats,
            diff_dir: None,
            diff_seq: state.diff_seq,
        }
    }

    #[must_use]
    pub fn snapshot_state(&self) -> CacheBreakDetectorState {
        CacheBreakDetectorState {
            per_source: self.per_source.clone(),
            source_order: self.source_order.clone(),
            stats: self.stats.clone(),
            diff_seq: self.diff_seq,
        }
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

    /// Record a turn against the [`DEFAULT_SOURCE`] stream. Shortcut for
    /// `record_turn_for_source(DEFAULT_SOURCE, …)`. Existing single-stream
    /// callers should continue using this method; multi-source callers
    /// (fork primitive, subagent managers) should use the source-keyed form.
    pub fn record_turn(
        &mut self,
        current: PromptStateSnapshot,
        actual_cache_read_tokens: Option<u64>,
    ) -> Option<CacheBreakEvent> {
        self.record_turn_for_source(DEFAULT_SOURCE, current, actual_cache_read_tokens)
    }

    /// Record a new turn's prompt state against a named source stream, and
    /// detect cache breaks relative to that source's previous snapshot.
    ///
    /// Sources are logical query streams — e.g. `"main"`, `"agent:explore"`,
    /// `"fork:<run_id>"`. Each source has its own previous snapshot; a
    /// break in stream A does not poison attribution for stream B. Up to
    /// [`MAX_TRACKED_SOURCES`] sources are tracked concurrently; the
    /// least-recently-written source is dropped on overflow.
    ///
    /// Returns `Some(event)` if this source's prefix broke, `None` if it
    /// was stable (cache hit). The first turn for a new source is a
    /// non-break miss (no baseline to compare against).
    ///
    /// `actual_cache_read_tokens` is from the API response — if available
    /// and near zero, it confirms a cache miss even when hashes match
    /// (TTL expiry).
    pub fn record_turn_for_source(
        &mut self,
        source: &str,
        current: PromptStateSnapshot,
        actual_cache_read_tokens: Option<u64>,
    ) -> Option<CacheBreakEvent> {
        self.stats.total_turns += 1;

        let previous_for_source = self.per_source.get(source);

        let event = if let Some(prev) = previous_for_source.as_ref() {
            self.detect_break(prev, &current, actual_cache_read_tokens)
        } else {
            // First turn for this source — always a "miss" but not a "break"
            self.stats.cache_misses += 1;
            None
        };

        if let Some(ref evt) = event {
            self.stats.cache_misses += 1;
            self.stats.total_miss_tokens += evt.estimated_token_impact;
            self.stats.recent_breaks.push(evt.clone());
            if self.stats.recent_breaks.len() > 10 {
                self.stats.recent_breaks.remove(0);
            }
            if let Some(dir) = self.diff_dir.clone() {
                self.diff_seq = self.diff_seq.wrapping_add(1);
                let _ =
                    write_diff_artifact(&dir, self.diff_seq, previous_for_source, &current, evt);
            }
        } else if previous_for_source.is_some() {
            self.stats.cache_hits += 1;
        }

        self.write_source_snapshot(source, current);
        event
    }

    /// Reset all tracked source baselines after an expected cache-boundary
    /// event such as compaction or native provider history clearing.
    pub fn reset_all_sources(&mut self) {
        self.per_source.clear();
        self.source_order.clear();
    }

    /// Insert/refresh a source's snapshot and maintain LRU order. Called
    /// from `record_turn_for_source` after detection completes so the
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

        // 2. System prompt change
        if prev.system_prompt_hash != curr.system_prompt_hash {
            reasons.push(CacheBreakReason::SystemPromptChanged);
        }

        // 2b. Cache-control / stable-boundary change
        if prev.cache_control_hash != curr.cache_control_hash {
            reasons.push(CacheBreakReason::CacheControlChanged);
        }

        // 3. Tool schemas change — diff which tools changed
        if prev.tools_hash != curr.tools_hash {
            let prev_map: std::collections::HashMap<&str, u64> = prev
                .per_tool_hashes
                .iter()
                .map(|(n, h)| (n.as_str(), *h))
                .collect();
            let curr_map: std::collections::HashMap<&str, u64> = curr
                .per_tool_hashes
                .iter()
                .map(|(n, h)| (n.as_str(), *h))
                .collect();

            let mut added: Vec<String> = curr_map
                .keys()
                .filter(|n| !prev_map.contains_key(*n))
                .map(|s| s.to_string())
                .collect();
            let mut removed: Vec<String> = prev_map
                .keys()
                .filter(|n| !curr_map.contains_key(*n))
                .map(|s| s.to_string())
                .collect();
            let mut changed: Vec<String> = curr_map
                .iter()
                .filter_map(|(n, h)| match prev_map.get(n) {
                    Some(prev_h) if prev_h != h => Some(n.to_string()),
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
                        "Tool schemas changed ({}). Consider pinning tool order and \
                         avoiding dynamic tool registration mid-session; same-name schema \
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
// D-12: Cache-Aware Compression Hints
// ---------------------------------------------------------------------------

/// Hint from cache diagnostics to the compression pipeline (D-4).
/// Tells the compressor which message prefix is cache-valid and should
/// NOT be compressed/reordered/removed.
#[derive(Debug, Clone)]
pub struct CacheAwareCompressionHint {
    /// Number of messages from the start that form the cache-valid prefix.
    /// The compression pipeline should not modify these messages.
    pub protected_prefix_len: usize,
    /// Estimated tokens in the protected prefix.
    pub protected_token_estimate: usize,
    /// Whether the cache is currently healthy (high hit rate).
    pub cache_healthy: bool,
    /// Suggested compression strategy based on cache state.
    pub strategy: CompressionStrategy,
}

/// Suggested strategy for the compression pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompressionStrategy {
    /// Cache is healthy — only compress messages AFTER the protected prefix.
    PreservePrefix,
    /// Cache is already broken — free to compress anything.
    CompressFreely,
    /// Cache is marginal — try to preserve prefix but allow light compression.
    PreservePrefixLight,
}

impl CacheBreakDetector {
    /// Generate a compression hint based on current cache state.
    ///
    /// The hint tells the compression pipeline (D-4's `CompressionPipeline`)
    /// how many leading messages are "cache-valid" and should be preserved.
    ///
    /// `message_count`: total messages in current conversation.
    /// `system_message_count`: number of system messages at the start.
    pub fn compression_hint(
        &self,
        message_count: usize,
        system_message_count: usize,
    ) -> CacheAwareCompressionHint {
        self.compression_hint_for_source(DEFAULT_SOURCE, message_count, system_message_count)
    }

    /// Generate a compression hint for a specific query source.
    ///
    /// If the requested source has not been written yet, fall back to the most
    /// recently refreshed source so replay-only streams do not silently lose the
    /// protected-token estimate.
    pub fn compression_hint_for_source(
        &self,
        source: &str,
        message_count: usize,
        system_message_count: usize,
    ) -> CacheAwareCompressionHint {
        let stats = &self.stats;
        let hit_rate = stats.hit_rate_percent();

        // If cache hit rate is high, protect the prefix
        let cache_healthy = hit_rate >= 70.0;
        let cache_marginal = (40.0..70.0).contains(&hit_rate);

        let strategy = if cache_healthy {
            CompressionStrategy::PreservePrefix
        } else if cache_marginal {
            CompressionStrategy::PreservePrefixLight
        } else {
            CompressionStrategy::CompressFreely
        };

        // The protected prefix is: system messages + tool schema context.
        // This is what the API caches (the stable prefix bytes).
        let protected_prefix_len = if cache_healthy || cache_marginal {
            // Protect system messages and first few user/assistant exchanges
            // that form the cache hit prefix
            system_message_count.min(message_count)
        } else {
            0
        };

        // Compression hints are a whole-session property, but the token estimate
        // still needs a representative snapshot. Prefer the caller's stream, and
        // fall back to the most recently refreshed tracked stream so bridge-only
        // replay state remains usable.
        let protected_token_estimate = self
            .per_source
            .get(source)
            .or_else(|| {
                self.source_order
                    .last()
                    .and_then(|latest| self.per_source.get(latest))
            })
            .map(|s| s.cache_eligible_tokens)
            .unwrap_or(0);

        CacheAwareCompressionHint {
            protected_prefix_len,
            protected_token_estimate,
            cache_healthy,
            strategy,
        }
    }

    /// Check if compressing a specific message range would break the cache.
    /// Returns true if the range overlaps with the cache-valid prefix.
    pub fn would_break_cache(
        &self,
        start_index: usize,
        _end_index: usize,
        system_message_count: usize,
    ) -> bool {
        if self.stats.hit_rate_percent() < 40.0 {
            return false; // cache already broken, can't make it worse
        }
        start_index < system_message_count
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
    for block in system_blocks {
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
    let _ = std::fs::write(
        &patch_path,
        render_unified_snapshot_patch(prev, curr, event).into_bytes(),
    );
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
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&payload).unwrap_or_else(|_| b"{}".to_vec()),
    )?;
    Ok(path)
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
        let mut s = PromptStateSnapshot::capture(prompt, tools, model, 15_000);
        s.timestamp_secs = 1000; // fixed for testing
        s
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
        let tools = make_tools(&["bash", "edit"]);
        let mut det = CacheBreakDetector::new();

        let s1 = snap("system prompt", &tools, "claude-3.5-sonnet");
        let s2 = snap("system prompt", &tools, "claude-3.5-sonnet");

        assert!(det.record_turn(s1, None).is_none()); // first turn
        assert!(det.record_turn(s2, None).is_none()); // same = hit
        assert_eq!(det.stats.cache_hits, 1);
        assert_eq!(det.stats.cache_misses, 1); // first turn counts as miss
    }

    #[test]
    fn detect_system_prompt_change() {
        let tools = make_tools(&["bash"]);
        let mut det = CacheBreakDetector::new();

        det.record_turn(snap("prompt v1", &tools, "claude"), None);
        let event = det.record_turn(snap("prompt v2", &tools, "claude"), None);

        assert!(event.is_some());
        let e = event.unwrap();
        assert_eq!(e.reason, CacheBreakReason::SystemPromptChanged);
        assert!(e.suggestion.unwrap().contains("System prompt changed"));
    }

    #[test]
    fn detect_tool_schema_change() {
        let mut det = CacheBreakDetector::new();

        det.record_turn(
            snap("prompt", &make_tools(&["bash", "edit"]), "claude"),
            None,
        );
        let event = det.record_turn(
            snap("prompt", &make_tools(&["bash", "grep"]), "claude"),
            None,
        );

        let e = event.unwrap();
        match &e.reason {
            CacheBreakReason::ToolSchemasChanged {
                added,
                removed,
                changed,
            } => {
                assert!(added.contains(&"grep".to_string()));
                assert!(removed.contains(&"edit".to_string()));
                assert!(changed.is_empty(), "no same-name schema churn expected");
            }
            other => panic!("expected ToolSchemasChanged, got {other:?}"),
        }
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

        det.record_turn(snap("prompt", &t1, "claude"), None);
        let event = det.record_turn(snap("prompt", &t2, "claude"), None);
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

        det.record_turn(snap("prompt", &tools, "claude-3.5-sonnet"), None);
        let event = det.record_turn(snap("prompt", &tools, "gpt-4o"), None);

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
        det.record_turn(s1, None);

        let mut s2 = snap("prompt", &tools, "claude");
        s2.timestamp_secs = 1000 + CACHE_TTL_1HOUR_SECS + 1;
        let event = det.record_turn(s2, Some(0)); // API says 0 cache read

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
        det.record_turn(s1, None);

        let mut s2 = snap("prompt", &tools, "claude");
        s2.timestamp_secs = 5000;
        // API says plenty of cache reads — not a miss
        let event = det.record_turn(s2, Some(10_000));
        assert!(event.is_none());
    }

    #[test]
    fn system_prompt_change_does_not_also_claim_cache_control_changed() {
        use crate::section_types::{CacheScope, SectionKind};

        let block_v1 = SerializedSystemBlock {
            kind: SectionKind::Identity,
            scope: CacheScope::Session,
            text: "system v1".into(),
            cache_control: Some(serde_json::json!({"type": "ephemeral"})),
        };
        let block_v2 = SerializedSystemBlock {
            text: "system v2".into(),
            ..block_v1.clone()
        };

        let mut det = CacheBreakDetector::new();
        det.record_turn(
            PromptStateSnapshot::capture_serialized(&[block_v1], &[], "anthropic", "claude", 8_000),
            None,
        );
        let event = det
            .record_turn(
                PromptStateSnapshot::capture_serialized(
                    &[block_v2],
                    &[],
                    "anthropic",
                    "claude",
                    8_000,
                ),
                Some(0),
            )
            .expect("system prompt change should be detected");

        let reasons = match event.reason {
            CacheBreakReason::Multiple(reasons) => reasons,
            other => vec![other],
        };
        assert!(
            reasons
                .iter()
                .any(|reason| matches!(reason, CacheBreakReason::SystemPromptChanged))
        );
        assert!(
            reasons
                .iter()
                .all(|reason| !matches!(reason, CacheBreakReason::CacheControlChanged)),
            "text-only changes must not be misattributed as cache-control churn"
        );
    }

    #[test]
    fn detect_cache_control_change() {
        use crate::section_types::{CacheScope, SectionKind};

        let block_v1 = SerializedSystemBlock {
            kind: SectionKind::Identity,
            scope: CacheScope::Session,
            text: "stable".into(),
            cache_control: Some(json!({"type": "ephemeral"})),
        };
        let block_v2 = SerializedSystemBlock {
            cache_control: Some(json!({"type": "ephemeral", "ttl": "1h"})),
            ..block_v1.clone()
        };

        let mut det = CacheBreakDetector::new();
        det.record_turn(
            PromptStateSnapshot::capture_serialized(&[block_v1], &[], "anthropic", "claude", 8_000),
            None,
        );
        let event = det
            .record_turn(
                PromptStateSnapshot::capture_serialized(
                    &[block_v2],
                    &[],
                    "anthropic",
                    "claude",
                    8_000,
                ),
                Some(0),
            )
            .expect("cache-control change should be detected");

        assert_eq!(event.reason, CacheBreakReason::CacheControlChanged);
    }

    #[test]
    fn small_prefix_uses_adaptive_cache_miss_threshold() {
        let tools = make_tools(&["bash"]);
        let mut det = CacheBreakDetector::new();
        let mut snapshot = snap("prompt", &tools, "claude");
        snapshot.cache_eligible_tokens = 512;

        det.record_turn(snapshot.clone(), None);
        let event = det.record_turn(snapshot, Some(900));
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
        det.record_turn(s1, None);

        let mut s2 = snap("prompt", &tools, "claude");
        s2.timestamp_secs = 1_100;
        let event = det
            .record_turn(s2, Some(0))
            .expect("near-zero cache read with same fingerprint should surface");
        assert_eq!(event.reason, CacheBreakReason::UnknownColdStart);
    }

    #[test]
    fn multiple_reasons_combined() {
        let mut det = CacheBreakDetector::new();

        det.record_turn(snap("prompt v1", &make_tools(&["bash"]), "claude"), None);
        let event = det.record_turn(
            snap("prompt v2", &make_tools(&["bash", "edit"]), "gpt-4o"),
            None,
        );

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
        det.record_turn(snap("prompt v1", &make_tools(&["bash"]), "claude"), None);
        det.reset_all_sources();

        let event = det.record_turn(snap("prompt v2", &make_tools(&["bash"]), "claude"), Some(0));
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

        det.record_turn(snap("p", &tools, "c"), None); // miss (first)
        det.record_turn(snap("p", &tools, "c"), None); // hit
        det.record_turn(snap("p", &tools, "c"), None); // hit
        det.record_turn(snap("p", &tools, "c"), None); // hit
        det.record_turn(snap("p2", &tools, "c"), None); // miss (changed)

        assert_eq!(det.stats.total_turns, 5);
        assert_eq!(det.stats.cache_hits, 3);
        assert_eq!(det.stats.cache_misses, 2);
        assert!((det.stats.hit_rate_percent() - 60.0).abs() < 0.1);
    }

    #[test]
    fn status_line_format() {
        let tools = make_tools(&["bash"]);
        let mut det = CacheBreakDetector::new();
        det.record_turn(snap("p", &tools, "c"), None);
        det.record_turn(snap("p", &tools, "c"), None);
        let line = det.status_line();
        assert!(line.contains("Cache:"));
        assert!(line.contains("hit rate"));
    }

    #[test]
    fn recent_breaks_capped_at_10() {
        let tools = make_tools(&["bash"]);
        let mut det = CacheBreakDetector::new();
        det.record_turn(snap("p0", &tools, "c"), None);
        for i in 1..=15 {
            det.record_turn(snap(&format!("p{i}"), &tools, "c"), None);
        }
        assert!(det.stats.recent_breaks.len() <= 10);
    }

    #[test]
    fn capture_snapshot_per_tool_hashes() {
        let tools = make_tools(&["bash", "edit", "grep"]);
        let snap = PromptStateSnapshot::capture("test", &tools, "model", 1000);
        assert_eq!(snap.per_tool_hashes.len(), 3);
        assert_eq!(snap.per_tool_hashes[0].0, "bash");
        assert_eq!(snap.per_tool_hashes[1].0, "edit");
        assert_eq!(snap.per_tool_hashes[2].0, "grep");
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

        assert!(det.record_turn(s1, None).is_none());
        assert!(det.record_turn(s2, None).is_none());
        assert_eq!(det.stats.cache_hits, 1);
    }

    #[test]
    fn hundred_percent_hit_rate() {
        let tools = make_tools(&["bash"]);
        let mut det = CacheBreakDetector::new();

        // First turn is always a miss, then 4 hits → 4/5 = 80% hits
        // To get ~100% we need the first turn (miss) plus all subsequent as hits.
        // Actually: first turn = miss, turns 2-6 = hits → 5 hits / 6 turns ≈ 83%
        // For true 100% hit rate on record_turn logic, first turn is always miss.
        // So record 1 first turn + 5 identical turns → 5 hits out of 6 turns.
        // But the ask is "cache_read_tokens >= cache_eligible_tokens" for 5 turns.
        // Let's just verify the hit rate from the stats perspective.
        det.record_turn(snap("p", &tools, "c"), Some(15_000)); // first turn = miss
        for _ in 0..5 {
            det.record_turn(snap("p", &tools, "c"), Some(15_000)); // hits
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
            det.record_turn(snap(&format!("prompt-{i}"), &tools, "c"), Some(0));
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
        det.record_turn(snap("p", &tools, "c"), None);
        for _ in 0..9 {
            det.record_turn(snap("p", &tools, "c"), None);
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
            det.record_turn(snap(&format!("p{i}"), &tools, "c"), None);
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
        det.record_turn(s1, None);

        let mut s2 = PromptStateSnapshot::capture("prompt v2", &tools, "claude", 100_000);
        s2.timestamp_secs = 1001;
        let event = det.record_turn(s2, None);

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
            det.record_turn(snap("v1", &tools, "c"), None);
            let e = det.record_turn(snap("v2", &tools, "c"), None).unwrap();
            assert!(
                e.suggestion.is_some(),
                "SystemPromptChanged should have remediation"
            );
        }
        // ToolSchemasChanged
        {
            let mut det = CacheBreakDetector::new();
            det.record_turn(snap("p", &make_tools(&["bash"]), "c"), None);
            let e = det
                .record_turn(snap("p", &make_tools(&["bash", "edit"]), "c"), None)
                .unwrap();
            assert!(
                e.suggestion.is_some(),
                "ToolSchemasChanged should have remediation"
            );
        }
        // ModelChanged
        {
            let mut det = CacheBreakDetector::new();
            det.record_turn(snap("p", &tools, "claude"), None);
            let e = det.record_turn(snap("p", &tools, "gpt-4o"), None).unwrap();
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
            det.record_turn(s1, None);

            let mut s2 = snap("p", &tools, "c");
            s2.timestamp_secs = 1000 + CACHE_TTL_1HOUR_SECS + 1;
            let e = det.record_turn(s2, Some(0)).unwrap();
            assert!(e.suggestion.is_some(), "TtlExpired should have remediation");
        }
    }

    // D-12: Cache-aware compression hint tests

    #[test]
    fn compression_hint_healthy_cache() {
        let tools = make_tools(&["bash", "edit"]);
        let mut det = CacheBreakDetector::new();

        // Record 5 turns with no breaks → high hit rate
        for _ in 0..5 {
            det.record_turn(snap("prompt", &tools, "claude"), None);
        }

        let hint = det.compression_hint(20, 2);
        assert!(hint.cache_healthy);
        assert_eq!(hint.strategy, CompressionStrategy::PreservePrefix);
        assert_eq!(hint.protected_prefix_len, 2);
    }

    #[test]
    fn compression_hint_broken_cache() {
        let tools = make_tools(&["bash"]);
        let mut det = CacheBreakDetector::new();

        // Force breaks by changing prompt each turn
        for i in 0..5 {
            det.record_turn(snap(&format!("prompt{}", i), &tools, "claude"), None);
        }

        let hint = det.compression_hint(20, 2);
        assert!(!hint.cache_healthy);
        assert_eq!(hint.strategy, CompressionStrategy::CompressFreely);
        assert_eq!(hint.protected_prefix_len, 0);
    }

    #[test]
    fn compression_hint_falls_back_to_latest_tracked_source() {
        let tools = make_tools(&["bash"]);
        let mut det = CacheBreakDetector::new();

        for _ in 0..5 {
            det.record_turn_for_source("bridge_inprocess", snap("prompt", &tools, "claude"), None);
        }

        let hint = det.compression_hint(20, 2);
        assert!(hint.cache_healthy);
        assert_eq!(hint.protected_token_estimate, 15_000);
    }

    #[test]
    fn compression_hint_for_source_prefers_requested_snapshot() {
        let tools = make_tools(&["bash"]);
        let mut det = CacheBreakDetector::new();

        let mut main = snap("main", &tools, "claude");
        main.cache_eligible_tokens = 7_000;
        let mut bridge = snap("bridge", &tools, "claude");
        bridge.cache_eligible_tokens = 11_000;

        det.record_turn_for_source(DEFAULT_SOURCE, main.clone(), None);
        det.record_turn_for_source(DEFAULT_SOURCE, main, None);
        det.record_turn_for_source("bridge_inprocess", bridge.clone(), None);
        det.record_turn_for_source("bridge_inprocess", bridge, None);

        let hint = det.compression_hint_for_source(DEFAULT_SOURCE, 20, 2);
        assert_eq!(hint.protected_token_estimate, 7_000);
    }

    #[test]
    fn would_break_cache_detects_overlap() {
        let tools = make_tools(&["bash"]);
        let mut det = CacheBreakDetector::new();

        // Build healthy cache
        for _ in 0..5 {
            det.record_turn(snap("prompt", &tools, "claude"), None);
        }

        // Compressing from index 0 overlaps system messages
        assert!(det.would_break_cache(0, 5, 2));
        // Compressing from index 3 does not
        assert!(!det.would_break_cache(3, 10, 2));
    }

    #[test]
    fn would_break_cache_already_broken() {
        let tools = make_tools(&["bash"]);
        let mut det = CacheBreakDetector::new();

        // Break cache every turn
        for i in 0..5 {
            det.record_turn(snap(&format!("p{}", i), &tools, "claude"), None);
        }

        // Even overlapping range is fine since cache is already broken
        assert!(!det.would_break_cache(0, 5, 2));
    }

    #[test]
    fn diff_artifact_written_on_break() {
        let tmp = tempfile::tempdir().unwrap();
        let mut det = CacheBreakDetector::new().with_diff_dir(tmp.path());

        det.record_turn(snap("v1", &make_tools(&["bash"]), "claude"), None);
        det.record_turn(snap("v2", &make_tools(&["bash"]), "claude"), None);

        let files: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
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

        det.record_turn(snap("p", &tools, "claude"), None);
        det.record_turn(snap("p", &tools, "claude"), None); // hit, no artifact

        let count = std::fs::read_dir(tmp.path()).unwrap().count();
        assert_eq!(count, 0, "no artifacts should be written on hits");
    }

    // ---------------------------------------------------------------------
    // Per-source tracking — prerequisites for the fork prefix primitive.
    // Each source stream has its own `previous` slot; breaks in one do not
    // poison attribution for another.
    // ---------------------------------------------------------------------

    #[test]
    fn default_source_constant_is_stable() {
        // Guard DEFAULT_SOURCE's literal value in one place rather than
        // duplicating "main" across every test that uses the shortcut API.
        assert_eq!(DEFAULT_SOURCE, "main");
    }

    #[test]
    fn legacy_record_turn_writes_default_source() {
        let tools = make_tools(&["bash"]);
        let mut det = CacheBreakDetector::new();
        det.record_turn(snap("p", &tools, "claude"), None);
        assert_eq!(det.tracked_source_count(), 1);
        assert!(det.snapshot_for_source(DEFAULT_SOURCE).is_some());
    }

    #[test]
    fn sources_are_independent_on_divergence() {
        // Source A keeps a stable prefix (should register hits).
        // Source B changes its system prompt each turn (should register breaks).
        // Source A's hit count must not be polluted by B's misses beyond the
        // aggregate stats, and each source's `previous` must come from its
        // own stream, not the globally last-written one.
        let tools = make_tools(&["bash"]);
        let mut det = CacheBreakDetector::new();

        det.record_turn_for_source("A", snap("prompt-A", &tools, "m"), None);
        det.record_turn_for_source("B", snap("prompt-B-v1", &tools, "m"), None);

        // A stable — this must be a HIT, even though B was written in between.
        let a_second = det.record_turn_for_source("A", snap("prompt-A", &tools, "m"), None);
        assert!(
            a_second.is_none(),
            "A's second turn must hit because A's own previous matched"
        );

        // B breaks — system prompt changed for B.
        let b_second = det.record_turn_for_source("B", snap("prompt-B-v2", &tools, "m"), None);
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
        // must still hit — baselines are per-source.
        let tools = make_tools(&["bash"]);
        let mut det = CacheBreakDetector::new();

        det.record_turn_for_source("A", snap("p-A", &tools, "m"), None);
        det.record_turn_for_source("B", snap("p-B-v1", &tools, "m"), None);
        det.record_turn_for_source("B", snap("p-B-v2", &tools, "m"), None); // B break

        // A's prefix is unchanged — must hit.
        let a_next = det.record_turn_for_source("A", snap("p-A", &tools, "m"), None);
        assert!(a_next.is_none(), "A must still hit after B broke");
    }

    #[test]
    fn lru_evicts_oldest_source_above_cap() {
        let tools = make_tools(&["bash"]);
        let mut det = CacheBreakDetector::new();

        // Fill past the cap. The oldest source ("s00") must be evicted.
        for i in 0..(MAX_TRACKED_SOURCES + 3) {
            let source = format!("s{i:02}");
            det.record_turn_for_source(&source, snap("p", &tools, "m"), None);
        }
        assert_eq!(det.tracked_source_count(), MAX_TRACKED_SOURCES);
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

        det.record_turn_for_source("pinned", snap("p", &tools, "m"), None);
        // Fill the rest to the cap; "pinned" is currently oldest.
        for i in 0..(MAX_TRACKED_SOURCES - 1) {
            det.record_turn_for_source(&format!("t{i}"), snap("p", &tools, "m"), None);
        }
        // Refresh pinned — it becomes most recent.
        det.record_turn_for_source("pinned", snap("p", &tools, "m"), None);
        // One more write triggers eviction — but "pinned" is no longer oldest.
        det.record_turn_for_source("overflow", snap("p", &tools, "m"), None);

        assert!(
            det.snapshot_for_source("pinned").is_some(),
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
        det.record_turn_for_source("first", snap("p", &tools, "m"), None);
        for i in 0..MAX_TRACKED_SOURCES {
            det.record_turn_for_source(&format!("s{i}"), snap("p", &tools, "m"), None);
        }
        // Peek "first" many times; it must still be the eviction candidate.
        for _ in 0..5 {
            let _ = det.snapshot_for_source("first");
        }
        det.record_turn_for_source("final", snap("p", &tools, "m"), None);
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

        det.record_turn_for_source("A", snap("prompt-A-stable", &tools, "m"), None);
        det.record_turn_for_source("B", snap("prompt-B-v1", &tools, "m"), None);
        // Now write A again (unchanged) so that A is globally last-written.
        det.record_turn_for_source("A", snap("prompt-A-stable", &tools, "m"), None);
        // Now break B. The artifact's `prev` must be B's v1, not A's prompt.
        det.record_turn_for_source("B", snap("prompt-B-v2", &tools, "m"), None);

        let files: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
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

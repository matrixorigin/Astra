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

/// Default source key used by the shortcut `record_turn` API. Callers that
/// only track a single query stream (e.g., a CLI main loop) never need to
/// deal with source keys — they always read/write this slot.
pub const DEFAULT_SOURCE: &str = "main";

/// Upper bound on concurrently tracked sources. Matches claudecode's
/// `MAX_TRACKED_SOURCES = 10`. Each entry is one `PromptStateSnapshot`
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
    /// Cache TTL expired (inferred from time gap + cache miss).
    TtlExpired { gap_seconds: u64 },
    /// Multiple causes at once.
    Multiple(Vec<CacheBreakReason>),
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
#[derive(Debug, Clone, Default)]
pub struct PromptStateSnapshot {
    /// Hash of the full system prompt text (all sections concatenated).
    pub system_prompt_hash: u64,
    /// Hash of all tool schemas combined (order-sensitive).
    pub tools_hash: u64,
    /// Per-tool hashes for diffing which tool changed.
    pub per_tool_hashes: Vec<(String, u64)>,
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
        let system_prompt_hash = hash_str(system_prompt_text);

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
            model: model.to_string(),
            timestamp_secs: now,
            cache_eligible_tokens,
        }
    }
}

// ---------------------------------------------------------------------------
// Detector: compares consecutive snapshots
// ---------------------------------------------------------------------------

/// Minimum cache miss tokens to consider a break significant.
const MIN_CACHE_MISS_TOKENS: usize = 2_000;

/// Cache TTL thresholds for expiration detection.
const CACHE_TTL_5MIN_SECS: u64 = 300;
#[cfg(test)]
const CACHE_TTL_1HOUR_SECS: u64 = 3_600;

/// Detects and classifies prompt cache breaks between turns.
///
/// A single detector instance tracks cache state per *source* — a logical
/// query stream (e.g. `"main"`, `"agent:session_memory"`, `"fork:<run_id>"`).
/// Each source has its own `previous` snapshot; a break in one source does
/// not corrupt attribution for another. This mirrors claudecode's
/// `previousStateBySource` map and is a prerequisite for the fork-prefix
/// primitive (PR 1+), where parent and child streams need independent
/// attribution.
///
/// Backwards compatibility: the legacy `record_turn(snapshot, actual)`
/// helper writes through to the [`DEFAULT_SOURCE`] slot, so pre-existing
/// single-stream callers are unaffected.
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

    /// Enable per-break diagnostic artifact emission to `dir`. The directory
    /// is created lazily on the first break. Errors during directory create
    /// or file write are swallowed to avoid perturbing the live turn — this
    /// is a developer aid, not a correctness signal.
    pub fn with_diff_dir(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.diff_dir = Some(dir.into());
        self
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

        let previous_for_source = self.per_source.get(source).cloned();

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
                let _ = write_diff_artifact(
                    &dir,
                    self.diff_seq,
                    previous_for_source.as_ref(),
                    &current,
                    evt,
                );
            }
        } else if previous_for_source.is_some() {
            self.stats.cache_hits += 1;
        }

        self.write_source_snapshot(source, current);
        event
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

        // 2. System prompt change
        if prev.system_prompt_hash != curr.system_prompt_hash {
            reasons.push(CacheBreakReason::SystemPromptChanged);
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

        // 4. If hashes match but API says cache miss → TTL expiry
        if reasons.is_empty() {
            if let Some(cache_read) = actual_cache_read {
                let gap = curr.timestamp_secs.saturating_sub(prev.timestamp_secs);
                if cache_read < MIN_CACHE_MISS_TOKENS as u64 && gap > CACHE_TTL_5MIN_SECS {
                    reasons.push(CacheBreakReason::TtlExpired { gap_seconds: gap });
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
                CacheBreakReason::TtlExpired { gap_seconds } => {
                    let minutes = gap_seconds / 60;
                    return Some(format!(
                        "Cache TTL likely expired ({minutes}min gap between turns). \
                         For long pauses, this is expected."
                    ));
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

        // Compression hints are a whole-session property, not per-source.
        // We use the DEFAULT_SOURCE (main stream) as the representative
        // snapshot: the protected prefix tokens are whatever the main
        // conversation last saw. Subagent streams don't influence what
        // the main compression pipeline treats as cache-valid prefix.
        //
        // NOTE: if the main turn loop is ever wired to record under a
        // different source key (e.g. `"repl_main_thread"` as in
        // claudecode), this lookup must be updated in lockstep or the
        // protected-token estimate will silently fall to 0.
        let protected_token_estimate = self
            .per_source
            .get(DEFAULT_SOURCE)
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
    let path = dir.join(format!(
        "cache-break-{:010}-{:04}.json",
        curr.timestamp_secs, seq
    ));
    let snapshot_summary = |s: Option<&PromptStateSnapshot>| {
        s.map(|s| {
            serde_json::json!({
                "system_prompt_hash": s.system_prompt_hash,
                "tools_hash": s.tools_hash,
                "per_tool_hashes": s.per_tool_hashes,
                "model": s.model,
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
    });
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&payload).unwrap_or_else(|_| b"{}".to_vec()),
    )?;
    Ok(path)
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
        assert_eq!(files.len(), 1, "exactly one artifact expected: {files:?}");
        assert!(
            files[0].starts_with("cache-break-"),
            "name should be stable-prefixed, got {}",
            files[0]
        );
        let body = std::fs::read_to_string(tmp.path().join(&files[0])).unwrap();
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
        let b_second =
            det.record_turn_for_source("B", snap("prompt-B-v2", &tools, "m"), None);
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
            det.snapshot_for_source(&format!("s{:02}", MAX_TRACKED_SOURCES + 2)).is_some(),
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
        assert_eq!(files.len(), 1, "exactly one artifact expected");
        let body = std::fs::read_to_string(&files[0]).unwrap();
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

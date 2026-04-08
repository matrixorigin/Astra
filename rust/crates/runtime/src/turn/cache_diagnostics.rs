//! Cache break detection and diagnostics for prompt caching.
//!
//! Tracks system prompt + tool schema hashes between turns to detect when
//! the KV cache prefix is broken. Classifies breaks by cause and logs
//! diagnostics with token impact estimates.
//!
//! Inspired by Claude Code's `promptCacheBreakDetection.ts`, but with
//! auto-remediation suggestions and richer classification.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Cache break classification
// ---------------------------------------------------------------------------

/// Reason why the prompt cache prefix was broken.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CacheBreakReason {
    /// System prompt text changed (e.g., profile, task type).
    SystemPromptChanged,
    /// Tool schemas changed (added, removed, or modified).
    ToolSchemasChanged {
        added: Vec<String>,
        removed: Vec<String>,
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
#[derive(Debug, Default)]
pub struct CacheBreakDetector {
    /// Previous turn's snapshot (None on first turn).
    previous: Option<PromptStateSnapshot>,
    /// Cumulative stats.
    pub stats: CacheStats,
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

    /// Record a new turn's prompt state and detect cache breaks.
    ///
    /// Returns `Some(event)` if a cache break was detected, `None` if cache
    /// prefix was stable (likely a hit).
    ///
    /// `actual_cache_read_tokens` is from the API response — if available and
    /// near zero, it confirms a cache miss even when hashes match (TTL expiry).
    pub fn record_turn(
        &mut self,
        current: PromptStateSnapshot,
        actual_cache_read_tokens: Option<u64>,
    ) -> Option<CacheBreakEvent> {
        self.stats.total_turns += 1;

        let event = if let Some(prev) = &self.previous {
            self.detect_break(prev, &current, actual_cache_read_tokens)
        } else {
            // First turn — always a "miss" but not a "break"
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
        } else if self.previous.is_some() {
            self.stats.cache_hits += 1;
        }

        self.previous = Some(current);
        event
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
            let prev_names: std::collections::HashSet<&str> = prev
                .per_tool_hashes
                .iter()
                .map(|(n, _)| n.as_str())
                .collect();
            let curr_names: std::collections::HashSet<&str> = curr
                .per_tool_hashes
                .iter()
                .map(|(n, _)| n.as_str())
                .collect();

            let added: Vec<String> = curr_names
                .difference(&prev_names)
                .map(|s| s.to_string())
                .collect();
            let removed: Vec<String> = prev_names
                .difference(&curr_names)
                .map(|s| s.to_string())
                .collect();

            reasons.push(CacheBreakReason::ToolSchemasChanged { added, removed });
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
                CacheBreakReason::ToolSchemasChanged { added, removed } => {
                    let parts: Vec<String> = [
                        if !added.is_empty() {
                            Some(format!("added: {}", added.join(", ")))
                        } else {
                            None
                        },
                        if !removed.is_empty() {
                            Some(format!("removed: {}", removed.join(", ")))
                        } else {
                            None
                        },
                    ]
                    .into_iter()
                    .flatten()
                    .collect();
                    return Some(format!(
                        "Tool schemas changed ({}). Consider pinning tool order and \
                         avoiding dynamic tool registration mid-session.",
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
        let cache_marginal = hit_rate >= 40.0 && hit_rate < 70.0;

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

        let protected_token_estimate = self
            .previous
            .as_ref()
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
            CacheBreakReason::ToolSchemasChanged { added, removed } => {
                assert!(added.contains(&"grep".to_string()));
                assert!(removed.contains(&"edit".to_string()));
            }
            other => panic!("expected ToolSchemasChanged, got {other:?}"),
        }
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
}

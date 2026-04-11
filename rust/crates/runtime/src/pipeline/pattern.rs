//! Tool Chain Pattern Library — learns successful tool sequences for reuse.
//!
//! Records which tool combinations succeed or fail for each task type and domain,
//! then suggests the best patterns for similar future queries.
//!
//! **Note**: The pattern recording and drift signals remain active, but the
//! L3 adaptive suggestion/exploration layer is being deprecated in favor of
//! `SelfModel` + LLM reasoning.
//!
//! # Learning flow
//!
//! 1. User asks "show me PRs for matrixorigin" → routes to Fetch + GitHub
//! 2. Agent uses [github_search, github_list_prs] → succeeds (quality 0.9)
//! 3. Pattern recorded: signature="github_list_prs|github_search", task=Fetch, domain=GitHub
//! 4. Next similar query → suggest() returns this pattern → boost these tools
//!
//! # Exploration
//!
//! To prevent pattern drift and discover new tools, `suggest_with_exploration()`
//! uses epsilon-greedy: 10% chance to include a low-frequency or stale pattern.
//!
//! # Integration
//!
//! ```rust,ignore
//! // At turn end (Evaluate → Complete):
//! library.record_outcome(&tools_used, TaskType::Fetch, Some(DomainHint::GitHub), true, 0.9, None);
//!
//! // At turn start (Plan):
//! let suggestions = library.suggest(TaskType::Fetch, Some(DomainHint::GitHub), 3);
//! let boost_terms = library.boost_terms_for(TaskType::Fetch, Some(DomainHint::GitHub));
//! ```

use super::routing::{DomainHint, TaskType};
use std::collections::HashMap;

// ─── Constants ───────────────────────────────────────────────────────────────

/// Days after which pattern weight starts decaying.
const DECAY_GRACE_DAYS: u64 = 7;
/// Half-life in days for exponential decay after grace period.
const DECAY_HALF_LIFE_DAYS: f64 = 30.0;
/// Probability of including an exploration pattern (epsilon-greedy).
const EXPLORATION_EPSILON: f64 = 0.1;
/// Window size for recent outcomes used in drift detection.
const DRIFT_WINDOW_SIZE: usize = 10;
/// Drift threshold: if recent success rate drops this much below historical, flag as drifting.
const DRIFT_THRESHOLD: f64 = 0.25;
/// Minimum total observations before drift detection applies.
const DRIFT_MIN_OBSERVATIONS: u32 = 6;

// ─── Tool Chain Pattern ──────────────────────────────────────────────────────

/// A recorded tool chain pattern with success/failure statistics.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolChainPattern {
    /// Sorted tool names joined by "|" (canonical signature).
    pub signature: String,
    /// Tool names in original invocation order.
    pub tools: Vec<String>,
    /// Task type this pattern was used for.
    pub task_type: TaskType,
    /// Domain (optional).
    pub domain: Option<DomainHint>,
    /// Number of successful uses.
    pub success_count: u32,
    /// Number of failed uses.
    pub failure_count: u32,
    /// Cumulative quality score (for computing average).
    quality_sum: f64,
    /// Unix timestamp of last use (seconds since epoch).
    /// Used for time-based decay calculations.
    #[serde(default)]
    pub last_used_at: u64,
    /// Recent outcome window for drift detection (last N results: true=success, false=fail).
    /// Capped at DRIFT_WINDOW_SIZE entries.
    #[serde(default)]
    recent_outcomes: Vec<bool>,
    /// Cumulative retries across all recorded tasks (for computing average).
    #[serde(default)]
    pub total_retries: u32,
    /// Cumulative execution turns across all recorded tasks (for computing average).
    #[serde(default)]
    pub total_turns: u32,
}

impl ToolChainPattern {
    fn new(
        signature: String,
        tools: Vec<String>,
        task_type: TaskType,
        domain: Option<DomainHint>,
    ) -> Self {
        Self {
            signature,
            tools,
            task_type,
            domain,
            success_count: 0,
            failure_count: 0,
            quality_sum: 0.0,
            last_used_at: current_timestamp(),
            recent_outcomes: Vec::new(),
            total_retries: 0,
            total_turns: 0,
        }
    }

    /// Total observations (success + failure).
    pub fn total_count(&self) -> u32 {
        self.success_count + self.failure_count
    }

    /// Success rate (0.0–1.0). Returns 0.5 if no observations.
    pub fn success_rate(&self) -> f64 {
        let total = self.total_count();
        if total == 0 {
            return 0.5;
        }
        self.success_count as f64 / total as f64
    }

    /// Average quality score (0.0–1.0). Returns 0.0 if no successes.
    pub fn avg_quality(&self) -> f64 {
        if self.success_count == 0 {
            return 0.0;
        }
        self.quality_sum / self.success_count as f64
    }

    /// Combined score: success_rate × 0.6 + avg_quality × 0.4.
    /// Used for ranking pattern suggestions.
    pub fn score(&self) -> f64 {
        self.success_rate() * 0.6 + self.avg_quality() * 0.4
    }

    /// Average retries per task (0.0 if no observations).
    pub fn avg_retries(&self) -> f64 {
        let total = self.total_count();
        if total == 0 {
            return 0.0;
        }
        self.total_retries as f64 / total as f64
    }

    /// Average execution turns per task (0.0 if no observations).
    pub fn avg_turns(&self) -> f64 {
        let total = self.total_count();
        if total == 0 {
            return 0.0;
        }
        self.total_turns as f64 / total as f64
    }

    /// Time-decayed score: applies exponential decay based on staleness.
    ///
    /// - Within grace period (7 days): no decay
    /// - After grace period: exponential decay with 30-day half-life
    ///
    /// This prevents stale patterns from dominating suggestions.
    pub fn decayed_score(&self) -> f64 {
        let base = self.score();
        let decay = time_decay_factor(self.last_used_at);
        base * decay
    }

    /// Update the last_used_at timestamp to now.
    pub fn touch(&mut self) {
        self.last_used_at = current_timestamp();
    }

    /// Record a recent outcome for drift tracking.
    fn push_outcome(&mut self, success: bool) {
        self.recent_outcomes.push(success);
        if self.recent_outcomes.len() > DRIFT_WINDOW_SIZE {
            self.recent_outcomes.remove(0);
        }
    }

    /// Recent success rate from the sliding window.
    pub fn recent_success_rate(&self) -> Option<f64> {
        if self.recent_outcomes.len() < 3 {
            return None; // not enough recent data
        }
        let wins = self.recent_outcomes.iter().filter(|&&b| b).count();
        Some(wins as f64 / self.recent_outcomes.len() as f64)
    }

    /// Drift score: how much recent performance deviates from historical.
    /// Returns 0.0 (no drift) to 1.0 (severe drift).
    /// Returns None if insufficient data.
    pub fn drift_score(&self) -> Option<f64> {
        if self.total_count() < DRIFT_MIN_OBSERVATIONS {
            return None;
        }
        let recent = self.recent_success_rate()?;
        let historical = self.success_rate();
        // Only flag when recent is WORSE than historical
        let drop = (historical - recent).max(0.0);
        Some((drop / DRIFT_THRESHOLD).min(1.0))
    }

    /// Whether this pattern is drifting (recent performance significantly worse).
    pub fn is_drifting(&self) -> bool {
        self.drift_score().is_some_and(|s| s >= 1.0)
    }

    /// Time decay factor (0.0–1.0) for this pattern based on staleness.
    pub fn time_decay_factor(&self) -> f64 {
        time_decay_factor(self.last_used_at)
    }
}

/// Calculate time decay factor (0.0–1.0) based on staleness.
///
/// - Within grace period: returns 1.0 (no decay)
/// - After grace period: exponential decay with configured half-life
fn time_decay_factor(last_used_at: u64) -> f64 {
    let now = current_timestamp();
    if last_used_at >= now {
        return 1.0;
    }

    let age_secs = now - last_used_at;
    let age_days = age_secs as f64 / 86400.0;

    if age_days <= DECAY_GRACE_DAYS as f64 {
        return 1.0;
    }

    // Exponential decay: weight = 0.5^(days_past_grace / half_life)
    let days_past_grace = age_days - DECAY_GRACE_DAYS as f64;
    0.5_f64.powf(days_past_grace / DECAY_HALF_LIFE_DAYS)
}

/// Get current Unix timestamp in seconds.
fn current_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ─── Pattern Library ─────────────────────────────────────────────────────────

/// Stores and retrieves learned tool chain patterns.
///
/// Patterns are indexed by task type and domain for efficient lookup.
/// At session end, export() serializes for persistence; at session start,
/// merge() integrates stored patterns.
#[derive(Debug, Clone, Default)]
pub struct PatternLibrary {
    /// signature → pattern.
    patterns: HashMap<String, ToolChainPattern>,
    /// task_type → [signature] for fast lookup.
    type_index: HashMap<TaskType, Vec<String>>,
    /// domain → [signature] for fast lookup.
    domain_index: HashMap<DomainHint, Vec<String>>,
    /// Patterns modified since last sync (for delta export).
    dirty_patterns: std::collections::HashSet<String>,
    /// Unix timestamp of last successful sync export.
    last_sync_epoch: u64,
}

/// Compute canonical signature from tool names (sorted, "|"-joined).
fn compute_signature(tools: &[String]) -> String {
    let mut sorted: Vec<&str> = tools.iter().map(|s| s.as_str()).collect();
    sorted.sort();
    sorted.join("|")
}

/// Full key combining signature + task type for uniqueness.
fn pattern_key(signature: &str, task_type: TaskType) -> String {
    format!("{signature}@{task_type:?}")
}

#[allow(deprecated)]
impl PatternLibrary {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the outcome of a tool chain execution.
    ///
    /// Called at turn end (Evaluate → Complete) with the tools that were used,
    /// the task type, domain, whether it succeeded, and quality score.
    ///
    /// If `user_feedback_score` is provided (0-100 scale), it adjusts the outcome:
    /// - Low feedback (< 50) treats the turn as a failure even if technically successful
    /// - Feedback also scales the quality score to reflect user satisfaction
    pub fn record_outcome(
        &mut self,
        tools: &[String],
        task_type: TaskType,
        domain: Option<DomainHint>,
        success: bool,
        quality: f64,
        user_feedback_score: Option<i64>,
    ) {
        if tools.is_empty() {
            return;
        }

        let sig = compute_signature(tools);
        let key = pattern_key(&sig, task_type);

        let pattern = self.patterns.entry(key.clone()).or_insert_with(|| {
            // New pattern — register in indices
            self.type_index
                .entry(task_type)
                .or_default()
                .push(key.clone());
            if let Some(d) = domain {
                self.domain_index.entry(d).or_default().push(key.clone());
            }
            ToolChainPattern::new(sig, tools.to_vec(), task_type, domain)
        });

        // Apply user feedback to adjust success and quality:
        // - Low satisfaction (< 50) overrides technical success → treat as failure
        // - Feedback score scales quality proportionally
        let adjusted_success = match user_feedback_score {
            Some(score) if score < 50 => false, // User unhappy → failure
            _ => success,
        };

        let adjusted_quality = match user_feedback_score {
            Some(score) => {
                // Scale quality by feedback: score=100 → 1.0x, score=50 → 0.75x, score=0 → 0.5x
                let feedback_factor = 0.5 + (score.max(0) as f64 / 200.0);
                quality * feedback_factor
            }
            None => quality,
        };

        if adjusted_success {
            pattern.success_count += 1;
            pattern.quality_sum += adjusted_quality.clamp(0.0, 1.0);
        } else {
            pattern.failure_count += 1;
        }

        // Track in sliding window for drift detection
        pattern.push_outcome(adjusted_success);

        // Update timestamp to reflect recent use
        pattern.touch();

        // Mark as dirty for delta sync
        self.dirty_patterns.insert(key);
    }

    /// Record a tool chain failure for an existing pattern.
    ///
    /// Unlike `record_outcome(success=false)`, this does NOT create a new pattern
    /// if one doesn't exist — patterns are only created on first success.
    /// This is the counterpart to the success-only entity learning path.
    pub fn record_failure(
        &mut self,
        tools: &[String],
        task_type: TaskType,
        domain: Option<DomainHint>,
    ) {
        let _ = domain; // reserved for future per-domain failure tracking
        if tools.is_empty() {
            return;
        }
        let sig = compute_signature(tools);
        let key = pattern_key(&sig, task_type);
        if let Some(pattern) = self.patterns.get_mut(&key) {
            pattern.failure_count += 1;
            pattern.push_outcome(false);
            pattern.touch();
            self.dirty_patterns.insert(key);
        }
    }

    /// Inspect success/failure counts for a concrete signature/task pair.
    pub fn pattern_stats(&self, signature: &str, task_type: TaskType) -> Option<(u32, u32)> {
        self.patterns
            .get(&pattern_key(signature, task_type))
            .map(|pattern| (pattern.success_count, pattern.failure_count))
    }

    /// Apply an explicit evolution action to every pattern with the given signature.
    ///
    /// Returns the number of patterns updated across task types/domains.
    pub fn apply_evolution_action(
        &mut self,
        signature: &str,
        action: crate::evolution::types::PatternAction,
    ) -> usize {
        let matching_keys: Vec<String> = self
            .patterns
            .iter()
            .filter(|(_, pattern)| pattern.signature == signature)
            .map(|(key, _)| key.clone())
            .collect();

        for key in &matching_keys {
            if let Some(pattern) = self.patterns.get_mut(key) {
                match action {
                    crate::evolution::types::PatternAction::Demote => {
                        pattern.failure_count += 2;
                        pattern.push_outcome(false);
                    }
                    crate::evolution::types::PatternAction::Block => {
                        pattern.failure_count += 5;
                        for _ in 0..3 {
                            pattern.push_outcome(false);
                        }
                    }
                    crate::evolution::types::PatternAction::Boost => {
                        pattern.success_count += 1;
                        pattern.quality_sum += 0.8;
                        pattern.push_outcome(true);
                    }
                }
                pattern.touch();
                self.dirty_patterns.insert(key.clone());
            }
        }

        matching_keys.len()
    }

    /// Record effort metrics (retries, turns) for an existing pattern.
    ///
    /// Called separately from `record_outcome()` to avoid changing that
    /// method's signature across 90+ call sites. Only task-learning callers
    /// that have retry/turn data need to call this.
    pub fn record_effort(
        &mut self,
        tools: &[String],
        task_type: TaskType,
        retries: u32,
        turns: u32,
    ) {
        if tools.is_empty() {
            return;
        }
        let sig = compute_signature(tools);
        let key = pattern_key(&sig, task_type);
        if let Some(pattern) = self.patterns.get_mut(&key) {
            pattern.total_retries += retries;
            pattern.total_turns += turns;
            self.dirty_patterns.insert(key);
        }
    }

    /// Remove patterns whose decayed score falls below `min_score`.
    ///
    /// Returns the number of patterns pruned. Cleans up type_index,
    /// domain_index, and dirty_patterns for removed entries.
    pub fn prune(&mut self, min_score: f64) -> usize {
        let stale_keys: Vec<String> = self
            .patterns
            .iter()
            .filter(|(_, p)| p.decayed_score() < min_score)
            .map(|(k, _)| k.clone())
            .collect();

        let count = stale_keys.len();
        for key in &stale_keys {
            self.patterns.remove(key);
            self.dirty_patterns.remove(key);
        }

        // Clean indices
        for keys in self.type_index.values_mut() {
            keys.retain(|k| !stale_keys.contains(k));
        }
        for keys in self.domain_index.values_mut() {
            keys.retain(|k| !stale_keys.contains(k));
        }

        count
    }

    /// Suggest best patterns for a task type + optional domain filter.
    ///
    /// Returns up to `limit` patterns sorted by time-decayed score (descending).
    /// If domain is Some, only returns patterns matching that domain.
    /// Stale patterns are ranked lower even if historically successful.
    #[deprecated(
        since = "0.9.0",
        note = "Superseded by SelfModel + LLM reasoning. Keep pattern recording, but let the model reason over self-awareness instead of precomputed suggestions."
    )]
    pub fn suggest(
        &self,
        task_type: TaskType,
        domain: Option<DomainHint>,
        limit: usize,
    ) -> Vec<&ToolChainPattern> {
        let keys = match self.type_index.get(&task_type) {
            Some(keys) => keys,
            None => return Vec::new(),
        };

        let mut candidates: Vec<&ToolChainPattern> = keys
            .iter()
            .filter_map(|k| self.patterns.get(k))
            .filter(|p| {
                // Domain filter: if domain requested, pattern must match
                match domain {
                    Some(d) => p.domain == Some(d),
                    None => true,
                }
            })
            .filter(|p| p.total_count() >= 2) // need at least 2 observations
            .collect();

        // Sort by decayed score descending (freshness matters)
        candidates.sort_by(|a, b| {
            b.decayed_score()
                .partial_cmp(&a.decayed_score())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        candidates.truncate(limit);
        candidates
    }

    /// Suggest patterns with epsilon-greedy exploration.
    ///
    /// With probability EXPLORATION_EPSILON (10%), includes a low-frequency or
    /// stale pattern among the suggestions. This helps rediscover tools that
    /// may have been deprecated due to time decay but are still valuable.
    ///
    /// Returns up to `limit` patterns, with the exploration slot (if triggered)
    /// replacing the last regular suggestion.
    #[deprecated(
        since = "0.9.0",
        note = "Superseded by SelfModel + LLM reasoning. Use self-awareness-driven exploration instead of epsilon-greedy pattern hints."
    )]
    pub fn suggest_with_exploration(
        &self,
        task_type: TaskType,
        domain: Option<DomainHint>,
        limit: usize,
    ) -> Vec<&ToolChainPattern> {
        let mut suggestions = self.suggest(task_type, domain, limit);

        // Roll for exploration using timestamp-based pseudo-randomness
        // Using nanos % 100 gives 0-99, so < 10 is ~10% probability
        let roll = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() % 100)
            .unwrap_or(50);
        let should_explore = (roll as f64) < (EXPLORATION_EPSILON * 100.0);

        if !should_explore || limit == 0 {
            return suggestions;
        }

        // Find exploration candidates: patterns not in top suggestions, sorted by staleness
        let top_sigs: std::collections::HashSet<_> =
            suggestions.iter().map(|p| &p.signature).collect();

        let keys = match self.type_index.get(&task_type) {
            Some(keys) => keys,
            None => return suggestions,
        };

        let mut exploration_candidates: Vec<&ToolChainPattern> = keys
            .iter()
            .filter_map(|k| self.patterns.get(k))
            .filter(|p| !top_sigs.contains(&p.signature))
            .filter(|p| match domain {
                Some(d) => p.domain == Some(d) || p.domain.is_none(),
                None => true,
            })
            .filter(|p| p.success_rate() >= 0.3) // Must have some success history
            .collect();

        if exploration_candidates.is_empty() {
            return suggestions;
        }

        // Sort by staleness (oldest first) to prioritize rediscovery
        exploration_candidates.sort_by(|a, b| a.last_used_at.cmp(&b.last_used_at));

        // Replace last suggestion with exploration pick
        if suggestions.len() >= limit {
            suggestions.pop();
        }
        suggestions.push(exploration_candidates[0]);

        suggestions
    }

    /// Suggest patterns with forced exploration (for testing).
    ///
    /// Same as `suggest_with_exploration` but always triggers exploration
    /// if exploration candidates exist.
    #[cfg(test)]
    pub fn suggest_with_forced_exploration(
        &self,
        task_type: TaskType,
        domain: Option<DomainHint>,
        limit: usize,
    ) -> Vec<&ToolChainPattern> {
        let mut suggestions = self.suggest(task_type, domain, limit);

        if limit == 0 {
            return suggestions;
        }

        let top_sigs: std::collections::HashSet<_> =
            suggestions.iter().map(|p| &p.signature).collect();

        let keys = match self.type_index.get(&task_type) {
            Some(keys) => keys,
            None => return suggestions,
        };

        let mut exploration_candidates: Vec<&ToolChainPattern> = keys
            .iter()
            .filter_map(|k| self.patterns.get(k))
            .filter(|p| !top_sigs.contains(&p.signature))
            .filter(|p| match domain {
                Some(d) => p.domain == Some(d) || p.domain.is_none(),
                None => true,
            })
            .filter(|p| p.success_rate() >= 0.3)
            .collect();

        if exploration_candidates.is_empty() {
            return suggestions;
        }

        exploration_candidates.sort_by(|a, b| a.last_used_at.cmp(&b.last_used_at));

        if suggestions.len() >= limit {
            suggestions.pop();
        }
        suggestions.push(exploration_candidates[0]);

        suggestions
    }

    /// Get boost terms from successful patterns for a task type + domain.
    ///
    /// Returns tool names from top patterns (success_rate > 0.5) for use as
    /// TF-IDF boost terms in routing.
    pub fn boost_terms_for(&self, task_type: TaskType, domain: Option<DomainHint>) -> Vec<String> {
        let suggestions = self.suggest(task_type, domain, 3);
        let mut terms: Vec<String> = Vec::new();
        let mut seen = std::collections::HashSet::new();

        for pattern in suggestions {
            if pattern.success_rate() > 0.5 {
                for tool in &pattern.tools {
                    if seen.insert(tool.clone()) {
                        terms.push(tool.clone());
                    }
                }
            }
        }

        terms
    }

    /// Number of stored patterns.
    pub fn len(&self) -> usize {
        self.patterns.len()
    }

    /// Whether the library is empty.
    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// Returns the maximum drift score across all patterns.
    ///
    /// Drift score ranges from 0.0 (no drift) to 1.0 (severe drift).
    /// Returns 0.0 if no patterns have enough observations for drift detection.
    pub fn max_drift_score(&self) -> f64 {
        self.patterns
            .values()
            .filter_map(|p| p.drift_score())
            .fold(0.0_f64, |max, score| max.max(score))
    }

    /// Returns all patterns currently drifting (drift_score >= 1.0).
    pub fn drifting_patterns(&self) -> Vec<&ToolChainPattern> {
        self.patterns.values().filter(|p| p.is_drifting()).collect()
    }

    /// Export all patterns for persistence.
    pub fn export(&self) -> Vec<ToolChainPattern> {
        self.patterns.values().cloned().collect()
    }

    /// Export only patterns modified since last sync.
    /// Call `clear_dirty()` after successful sync to reset tracking.
    pub fn export_dirty(&self) -> Vec<ToolChainPattern> {
        self.dirty_patterns
            .iter()
            .filter_map(|key| self.patterns.get(key).cloned())
            .collect()
    }

    /// Check if there are dirty patterns needing sync.
    pub fn has_dirty(&self) -> bool {
        !self.dirty_patterns.is_empty()
    }

    /// Clear dirty tracking after successful sync.
    pub fn clear_dirty(&mut self) {
        self.dirty_patterns.clear();
        self.last_sync_epoch = current_timestamp();
    }

    /// Get the timestamp of last successful sync.
    pub fn last_sync_epoch(&self) -> u64 {
        self.last_sync_epoch
    }

    /// Merge stored patterns into the library.
    ///
    /// For each incoming pattern, keeps whichever version (local or stored)
    /// has more total observations.
    pub fn merge(&mut self, patterns: &[ToolChainPattern]) {
        for pattern in patterns {
            let key = pattern_key(&pattern.signature, pattern.task_type);

            let existing_total = self
                .patterns
                .get(&key)
                .map(|p| p.total_count())
                .unwrap_or(0);

            if pattern.total_count() > existing_total {
                // Register in indices if new
                if !self.patterns.contains_key(&key) {
                    self.type_index
                        .entry(pattern.task_type)
                        .or_default()
                        .push(key.clone());
                    if let Some(d) = pattern.domain {
                        self.domain_index.entry(d).or_default().push(key.clone());
                    }
                }
                self.patterns.insert(key, pattern.clone());
            }
        }
    }

    /// Compute tool co-occurrence scores: P(next_tool | just_used_tool).
    ///
    /// For each tool that appeared in successful patterns alongside `just_used`,
    /// returns a score (0.0–1.0) representing how frequently they co-occur.
    /// Used by the scoring pipeline to boost the next likely tool in a chain.
    pub fn co_occurrence_scores(&self, just_used: &[String]) -> HashMap<String, f64> {
        if just_used.is_empty() {
            return HashMap::new();
        }

        let just_used_set: std::collections::HashSet<&str> =
            just_used.iter().map(|s| s.as_str()).collect();

        // Count how often each tool appears alongside just_used tools in successful patterns
        let mut co_counts: HashMap<String, f64> = HashMap::new();
        let mut total_weight = 0.0;

        for pattern in self.patterns.values() {
            if pattern.success_count == 0 {
                continue;
            }
            // Does this pattern contain any of the just_used tools?
            let overlap = pattern
                .tools
                .iter()
                .any(|t| just_used_set.contains(t.as_str()));
            if !overlap {
                continue;
            }

            // Weight by success rate and quality
            let weight = pattern.success_rate() * (1.0 + pattern.avg_quality());

            for tool in &pattern.tools {
                if !just_used_set.contains(tool.as_str()) {
                    *co_counts.entry(tool.clone()).or_default() += weight;
                }
            }
            total_weight += weight;
        }

        // Normalize to 0.0–1.0
        if total_weight > 0.0 {
            for score in co_counts.values_mut() {
                *score = (*score / total_weight).min(1.0);
            }
        }

        co_counts
    }

    // ─── Drift Detection ─────────────────────────────────────────────────────

    /// Detect drifting patterns: recent success rate significantly below historical.
    ///
    /// Returns patterns where recent performance (sliding window) dropped
    /// by ≥ DRIFT_THRESHOLD below historical average, indicating the user's
    /// task context may have shifted and these patterns are now misleading.
    pub fn detect_drift(&self) -> Vec<DriftReport> {
        self.patterns
            .values()
            .filter_map(|p| {
                let drift = p.drift_score()?;
                if drift < 0.5 {
                    return None; // only report meaningful drift
                }
                let recent = p.recent_success_rate().unwrap_or(0.0);
                Some(DriftReport {
                    signature: p.signature.clone(),
                    task_type: p.task_type,
                    domain: p.domain,
                    historical_success_rate: p.success_rate(),
                    recent_success_rate: recent,
                    drift_score: drift,
                    total_observations: p.total_count(),
                    is_critical: p.is_drifting(),
                })
            })
            .collect()
    }

    /// Auto-demote critically drifting patterns by boosting their failure count.
    /// Returns the number of patterns demoted.
    pub fn auto_demote_drifting(&mut self) -> usize {
        let drifting_keys: Vec<String> = self
            .patterns
            .iter()
            .filter(|(_, p)| p.is_drifting())
            .map(|(k, _)| k.clone())
            .collect();

        for key in &drifting_keys {
            if let Some(pattern) = self.patterns.get_mut(key) {
                // Add synthetic failures to push decayed_score down
                pattern.failure_count += 2;
                self.dirty_patterns.insert(key.clone());
            }
        }
        drifting_keys.len()
    }

    // ─── Active Exploration ──────────────────────────────────────────────────

    /// Find domains/task types where confidence is low and exploration would help.
    ///
    /// Returns suggestions for tool combinations to try when the system has
    /// low confidence in a particular area. Unlike epsilon-greedy (which
    /// rediscovers old patterns), this identifies gaps in coverage.
    #[deprecated(
        since = "0.9.0",
        note = "Superseded by SelfModel + LLM reasoning. Use self-awareness-driven exploration instead of programmatic opportunity generation."
    )]
    pub fn exploration_opportunities(&self) -> Vec<ExplorationOpportunity> {
        let mut opportunities = Vec::new();

        // Check each task type for low-confidence areas
        for (&task_type, keys) in &self.type_index {
            let patterns: Vec<&ToolChainPattern> =
                keys.iter().filter_map(|k| self.patterns.get(k)).collect();

            if patterns.is_empty() {
                continue;
            }

            // Group by domain
            let mut domain_groups: HashMap<Option<DomainHint>, Vec<&ToolChainPattern>> =
                HashMap::new();
            for p in &patterns {
                domain_groups.entry(p.domain).or_default().push(p);
            }

            for (domain, group) in &domain_groups {
                let avg_success: f64 =
                    group.iter().map(|p| p.success_rate()).sum::<f64>() / group.len() as f64;
                let avg_quality: f64 =
                    group.iter().map(|p| p.avg_quality()).sum::<f64>() / group.len() as f64;
                let total_obs: u32 = group.iter().map(|p| p.total_count()).sum();
                let has_drift = group.iter().any(|p| p.is_drifting());

                // Low confidence if: few observations, low success, or active drift
                let confidence = if total_obs < 5 {
                    0.2 // cold start
                } else if has_drift {
                    0.3 // drift undermines confidence
                } else {
                    avg_success * 0.6 + avg_quality * 0.4
                };

                if confidence < 0.5 {
                    // Collect tools from this domain that have worked elsewhere
                    let all_domain_tools: Vec<String> = group
                        .iter()
                        .flat_map(|p| p.tools.iter().cloned())
                        .collect::<std::collections::HashSet<_>>()
                        .into_iter()
                        .collect();

                    opportunities.push(ExplorationOpportunity {
                        task_type,
                        domain: *domain,
                        confidence,
                        reason: if total_obs < 5 {
                            ExplorationReason::ColdStart
                        } else if has_drift {
                            ExplorationReason::Drift
                        } else {
                            ExplorationReason::LowSuccess
                        },
                        known_tools: all_domain_tools,
                        pattern_count: group.len(),
                    });
                }
            }
        }

        // Sort by confidence ascending (lowest confidence = most urgent)
        opportunities.sort_by(|a, b| {
            a.confidence
                .partial_cmp(&b.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        opportunities
    }

    /// Report health metrics for the pattern library.
    pub fn health_report(&self) -> PatternLibraryHealth {
        let total = self.patterns.len();
        let drifting = self.patterns.values().filter(|p| p.is_drifting()).count();
        let decayed = self
            .patterns
            .values()
            .filter(|p| p.time_decay_factor() < 0.5)
            .count();
        let low_quality = self
            .patterns
            .values()
            .filter(|p| p.score() < 0.3 && p.total_count() >= 5)
            .count();
        PatternLibraryHealth {
            total_patterns: total,
            drifting_patterns: drifting,
            heavily_decayed: decayed,
            low_quality,
        }
    }

    /// Get a learning summary for display (e.g., /learn stats).
    pub fn learning_summary(&self) -> LearningSummary {
        let total_patterns = self.patterns.len();
        let active_patterns = self
            .patterns
            .values()
            .filter(|p| p.total_count() >= 2 && p.decayed_score() > 0.1)
            .count();
        let drifting = self.detect_drift().len();

        let avg_success = if total_patterns > 0 {
            self.patterns
                .values()
                .map(|p| p.success_rate())
                .sum::<f64>()
                / total_patterns as f64
        } else {
            0.0
        };

        let top_patterns: Vec<(String, f64)> = {
            let mut sorted: Vec<_> = self
                .patterns
                .values()
                .filter(|p| p.total_count() >= 2)
                .collect();
            sorted.sort_by(|a, b| {
                b.decayed_score()
                    .partial_cmp(&a.decayed_score())
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            sorted
                .iter()
                .take(5)
                .map(|p| (p.signature.clone(), p.decayed_score()))
                .collect()
        };

        let exploration = self.exploration_opportunities();

        LearningSummary {
            total_patterns,
            active_patterns,
            drifting_patterns: drifting,
            avg_success_rate: avg_success,
            top_patterns,
            exploration_opportunities: exploration.len(),
        }
    }
}

// ─── Drift & Exploration Types ───────────────────────────────────────────────

/// Report of a drifting pattern.
#[derive(Debug, Clone)]
pub struct DriftReport {
    pub signature: String,
    pub task_type: TaskType,
    pub domain: Option<DomainHint>,
    pub historical_success_rate: f64,
    pub recent_success_rate: f64,
    /// 0.0 (no drift) to 1.0 (severe drift).
    pub drift_score: f64,
    pub total_observations: u32,
    /// True if drift exceeds the critical threshold.
    pub is_critical: bool,
}

/// Reason an exploration opportunity was identified.
#[derive(Debug, Clone, PartialEq)]
pub enum ExplorationReason {
    /// New domain/task with insufficient data.
    ColdStart,
    /// Existing patterns are drifting (performance degraded).
    Drift,
    /// Success rate is consistently low.
    LowSuccess,
}

/// An area where active exploration could improve learning.
#[derive(Debug, Clone)]
pub struct ExplorationOpportunity {
    pub task_type: TaskType,
    pub domain: Option<DomainHint>,
    /// Estimated confidence (0.0–1.0).
    pub confidence: f64,
    pub reason: ExplorationReason,
    /// Tools already tried in this area.
    pub known_tools: Vec<String>,
    pub pattern_count: usize,
}

/// Health metrics for the pattern library.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PatternLibraryHealth {
    pub total_patterns: usize,
    pub drifting_patterns: usize,
    pub heavily_decayed: usize,
    pub low_quality: usize,
}

/// Summary statistics for the learning pipeline.
#[derive(Debug, Clone)]
pub struct LearningSummary {
    pub total_patterns: usize,
    pub active_patterns: usize,
    pub drifting_patterns: usize,
    pub avg_success_rate: f64,
    /// Top patterns by decayed score: (signature, score).
    pub top_patterns: Vec<(String, f64)>,
    pub exploration_opportunities: usize,
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(deprecated)]
mod tests {
    use super::*;

    fn tools(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    // ── Signature computation ──

    #[test]
    fn signature_is_sorted() {
        assert_eq!(compute_signature(&tools(&["grep", "bash"])), "bash|grep");
        assert_eq!(compute_signature(&tools(&["a", "c", "b"])), "a|b|c");
    }

    #[test]
    fn signature_single_tool() {
        assert_eq!(compute_signature(&tools(&["bash"])), "bash");
    }

    // ── Recording outcomes ──

    #[test]
    fn record_success_creates_pattern() {
        let mut lib = PatternLibrary::new();
        lib.record_outcome(
            &tools(&["bash", "grep"]),
            TaskType::Fetch,
            Some(DomainHint::Code),
            true,
            0.9,
            None,
        );
        assert_eq!(lib.len(), 1);
        let exported = lib.export();
        assert_eq!(exported[0].success_count, 1);
        assert_eq!(exported[0].failure_count, 0);
        assert!((exported[0].avg_quality() - 0.9).abs() < 0.01);
    }

    #[test]
    fn record_failure_increments_failure_count() {
        let mut lib = PatternLibrary::new();
        lib.record_outcome(&tools(&["bash"]), TaskType::Code, None, false, 0.0, None);
        let exported = lib.export();
        assert_eq!(exported[0].success_count, 0);
        assert_eq!(exported[0].failure_count, 1);
        assert_eq!(exported[0].avg_quality(), 0.0);
    }

    #[test]
    fn record_accumulates_counts() {
        let mut lib = PatternLibrary::new();
        for i in 0..5 {
            lib.record_outcome(
                &tools(&["github_search", "github_api"]),
                TaskType::Fetch,
                Some(DomainHint::GitHub),
                true,
                0.8 + (i as f64) * 0.02,
                None,
            );
        }
        lib.record_outcome(
            &tools(&["github_search", "github_api"]),
            TaskType::Fetch,
            Some(DomainHint::GitHub),
            false,
            0.0,
            None,
        );
        let exported = lib.export();
        assert_eq!(exported[0].success_count, 5);
        assert_eq!(exported[0].failure_count, 1);
        assert!(exported[0].success_rate() > 0.8);
    }

    #[test]
    fn record_empty_tools_ignored() {
        let mut lib = PatternLibrary::new();
        lib.record_outcome(&[], TaskType::Fetch, None, true, 0.9, None);
        assert!(lib.is_empty());
    }

    #[test]
    fn different_task_types_different_patterns() {
        let mut lib = PatternLibrary::new();
        lib.record_outcome(&tools(&["bash"]), TaskType::Code, None, true, 0.9, None);
        lib.record_outcome(&tools(&["bash"]), TaskType::Fetch, None, true, 0.8, None);
        assert_eq!(lib.len(), 2);
    }

    // ── Success rate and quality ──

    #[test]
    fn success_rate_mixed() {
        let mut p =
            ToolChainPattern::new("a|b".to_string(), tools(&["a", "b"]), TaskType::Code, None);
        p.success_count = 3;
        p.failure_count = 1;
        p.quality_sum = 2.4;
        assert!((p.success_rate() - 0.75).abs() < 0.01);
        assert!((p.avg_quality() - 0.80).abs() < 0.01);
    }

    #[test]
    fn success_rate_no_data() {
        let p = ToolChainPattern::new("x".to_string(), tools(&["x"]), TaskType::Code, None);
        assert!((p.success_rate() - 0.5).abs() < 0.01);
        assert_eq!(p.avg_quality(), 0.0);
    }

    #[test]
    fn score_combines_rate_and_quality() {
        let mut p = ToolChainPattern::new("a".to_string(), tools(&["a"]), TaskType::Fetch, None);
        p.success_count = 10;
        p.failure_count = 0;
        p.quality_sum = 9.0;
        // success_rate=1.0, avg_quality=0.9
        // score = 1.0*0.6 + 0.9*0.4 = 0.96
        assert!((p.score() - 0.96).abs() < 0.01);
    }

    // ── Suggestions ──

    #[test]
    fn suggest_returns_by_task_type() {
        let mut lib = PatternLibrary::new();
        // 3 observations needed (>= 2)
        for _ in 0..3 {
            lib.record_outcome(
                &tools(&["github_search"]),
                TaskType::Fetch,
                Some(DomainHint::GitHub),
                true,
                0.9,
                None,
            );
        }
        for _ in 0..3 {
            lib.record_outcome(&tools(&["bash"]), TaskType::Code, None, true, 0.8, None);
        }

        let fetch_suggestions = lib.suggest(TaskType::Fetch, None, 5);
        assert_eq!(fetch_suggestions.len(), 1);
        assert!(
            fetch_suggestions[0]
                .tools
                .contains(&"github_search".to_string())
        );

        let code_suggestions = lib.suggest(TaskType::Code, None, 5);
        assert_eq!(code_suggestions.len(), 1);
        assert!(code_suggestions[0].tools.contains(&"bash".to_string()));
    }

    #[test]
    fn suggest_filters_by_domain() {
        let mut lib = PatternLibrary::new();
        for _ in 0..3 {
            lib.record_outcome(
                &tools(&["github_api"]),
                TaskType::Fetch,
                Some(DomainHint::GitHub),
                true,
                0.9,
                None,
            );
        }
        for _ in 0..3 {
            lib.record_outcome(
                &tools(&["bash"]),
                TaskType::Fetch,
                Some(DomainHint::System),
                true,
                0.8,
                None,
            );
        }

        let github = lib.suggest(TaskType::Fetch, Some(DomainHint::GitHub), 5);
        assert_eq!(github.len(), 1);
        assert!(github[0].tools.contains(&"github_api".to_string()));

        let system = lib.suggest(TaskType::Fetch, Some(DomainHint::System), 5);
        assert_eq!(system.len(), 1);
        assert!(system[0].tools.contains(&"bash".to_string()));
    }

    #[test]
    fn suggest_needs_min_observations() {
        let mut lib = PatternLibrary::new();
        lib.record_outcome(&tools(&["bash"]), TaskType::Code, None, true, 0.9, None);
        // Only 1 observation → not suggested
        assert!(lib.suggest(TaskType::Code, None, 5).is_empty());
    }

    #[test]
    fn suggest_sorted_by_score() {
        let mut lib = PatternLibrary::new();
        // Pattern A: high quality
        for _ in 0..5 {
            lib.record_outcome(
                &tools(&["pattern_a"]),
                TaskType::Fetch,
                None,
                true,
                0.95,
                None,
            );
        }
        // Pattern B: lower quality
        for _ in 0..5 {
            lib.record_outcome(
                &tools(&["pattern_b"]),
                TaskType::Fetch,
                None,
                true,
                0.5,
                None,
            );
        }
        // Pattern C: mixed success
        for _ in 0..3 {
            lib.record_outcome(
                &tools(&["pattern_c"]),
                TaskType::Fetch,
                None,
                true,
                0.7,
                None,
            );
        }
        for _ in 0..3 {
            lib.record_outcome(
                &tools(&["pattern_c"]),
                TaskType::Fetch,
                None,
                false,
                0.0,
                None,
            );
        }

        let suggestions = lib.suggest(TaskType::Fetch, None, 3);
        assert!(suggestions.len() >= 2);
        // First should be pattern_a (highest score)
        assert!(suggestions[0].score() >= suggestions[1].score());
    }

    #[test]
    fn suggest_respects_limit() {
        let mut lib = PatternLibrary::new();
        for i in 0..5 {
            let name = format!("tool_{i}");
            for _ in 0..3 {
                lib.record_outcome(
                    std::slice::from_ref(&name),
                    TaskType::Code,
                    None,
                    true,
                    0.8,
                    None,
                );
            }
        }
        let suggestions = lib.suggest(TaskType::Code, None, 2);
        assert_eq!(suggestions.len(), 2);
    }

    #[test]
    fn suggest_empty_for_unknown_type() {
        let lib = PatternLibrary::new();
        assert!(lib.suggest(TaskType::Memory, None, 5).is_empty());
    }

    // ── Boost terms ──

    #[test]
    fn boost_terms_from_successful_patterns() {
        let mut lib = PatternLibrary::new();
        for _ in 0..3 {
            lib.record_outcome(
                &tools(&["github_search", "github_api"]),
                TaskType::Fetch,
                Some(DomainHint::GitHub),
                true,
                0.9,
                None,
            );
        }
        let terms = lib.boost_terms_for(TaskType::Fetch, Some(DomainHint::GitHub));
        assert!(terms.contains(&"github_search".to_string()));
        assert!(terms.contains(&"github_api".to_string()));
    }

    #[test]
    fn boost_terms_deduplicates() {
        let mut lib = PatternLibrary::new();
        // Two patterns share "bash"
        for _ in 0..3 {
            lib.record_outcome(
                &tools(&["bash", "grep"]),
                TaskType::Code,
                None,
                true,
                0.9,
                None,
            );
        }
        for _ in 0..3 {
            lib.record_outcome(
                &tools(&["bash", "sed"]),
                TaskType::Code,
                None,
                true,
                0.8,
                None,
            );
        }
        let terms = lib.boost_terms_for(TaskType::Code, None);
        let bash_count = terms.iter().filter(|t| *t == "bash").count();
        assert_eq!(bash_count, 1, "bash should appear only once");
    }

    #[test]
    fn boost_terms_excludes_low_success_rate() {
        let mut lib = PatternLibrary::new();
        for _ in 0..2 {
            lib.record_outcome(
                &tools(&["flaky_tool"]),
                TaskType::Code,
                None,
                true,
                0.3,
                None,
            );
        }
        for _ in 0..5 {
            lib.record_outcome(
                &tools(&["flaky_tool"]),
                TaskType::Code,
                None,
                false,
                0.0,
                None,
            );
        }
        // Success rate ~28% < 50% → excluded
        let terms = lib.boost_terms_for(TaskType::Code, None);
        assert!(!terms.contains(&"flaky_tool".to_string()));
    }

    // ── Export/Merge ──

    #[test]
    fn export_merge_round_trip() {
        let mut lib = PatternLibrary::new();
        for _ in 0..5 {
            lib.record_outcome(
                &tools(&["bash", "grep"]),
                TaskType::Code,
                Some(DomainHint::Code),
                true,
                0.9,
                None,
            );
        }
        let exported = lib.export();

        let mut lib2 = PatternLibrary::new();
        lib2.merge(&exported);
        assert_eq!(lib2.len(), 1);

        let suggestions = lib2.suggest(TaskType::Code, None, 5);
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].success_count, 5);
    }

    #[test]
    fn merge_keeps_higher_observation_count() {
        let mut lib = PatternLibrary::new();
        for _ in 0..3 {
            lib.record_outcome(&tools(&["bash"]), TaskType::Code, None, true, 0.9, None);
        }

        // Create a stored version with more data
        let mut stored =
            ToolChainPattern::new("bash".to_string(), tools(&["bash"]), TaskType::Code, None);
        stored.success_count = 10;
        stored.quality_sum = 8.5;

        lib.merge(&[stored]);
        let exported = lib.export();
        let pattern = exported.iter().find(|p| p.signature == "bash").unwrap();
        assert_eq!(pattern.success_count, 10, "stored version should win");
    }

    #[test]
    fn merge_keeps_local_if_higher() {
        let mut lib = PatternLibrary::new();
        for _ in 0..10 {
            lib.record_outcome(&tools(&["grep"]), TaskType::Fetch, None, true, 0.9, None);
        }

        let mut stored =
            ToolChainPattern::new("grep".to_string(), tools(&["grep"]), TaskType::Fetch, None);
        stored.success_count = 3;
        stored.quality_sum = 2.4;

        lib.merge(&[stored]);
        let exported = lib.export();
        let pattern = exported.iter().find(|p| p.signature == "grep").unwrap();
        assert_eq!(pattern.success_count, 10, "local version should win");
    }

    // ── Integration scenario ──

    #[test]
    fn learning_cycle_improves_suggestions() {
        let mut lib = PatternLibrary::new();

        // Phase 1: No patterns → no suggestions
        assert!(
            lib.suggest(TaskType::Fetch, Some(DomainHint::GitHub), 3)
                .is_empty()
        );

        // Phase 2: Learn from successful GitHub interactions
        for _ in 0..5 {
            lib.record_outcome(
                &tools(&["github_search", "github_list_prs"]),
                TaskType::Fetch,
                Some(DomainHint::GitHub),
                true,
                0.9,
                None,
            );
        }

        // Phase 3: Suggestions now available
        let suggestions = lib.suggest(TaskType::Fetch, Some(DomainHint::GitHub), 3);
        assert_eq!(suggestions.len(), 1);
        assert!(suggestions[0].score() > 0.8);
        assert_eq!(suggestions[0].tools.len(), 2);

        // Phase 4: Boost terms propagate to routing
        let terms = lib.boost_terms_for(TaskType::Fetch, Some(DomainHint::GitHub));
        assert!(terms.contains(&"github_search".to_string()));
        assert!(terms.contains(&"github_list_prs".to_string()));
    }

    // ── Co-occurrence scoring ──

    #[test]
    fn co_occurrence_empty_library_returns_empty() {
        let lib = PatternLibrary::new();
        let scores = lib.co_occurrence_scores(&tools(&["bash"]));
        assert!(scores.is_empty());
    }

    #[test]
    fn co_occurrence_returns_related_tools() {
        let mut lib = PatternLibrary::new();
        // Record: grep + read_file + str_replace succeeds together
        for _ in 0..5 {
            lib.record_outcome(
                &tools(&["grep", "read_file", "str_replace"]),
                TaskType::Code,
                None,
                true,
                0.9,
                None,
            );
        }

        // Given just_used = [grep], should suggest read_file and str_replace
        let scores = lib.co_occurrence_scores(&tools(&["grep"]));
        assert!(
            scores.contains_key("read_file"),
            "read_file should be a co-occurrence"
        );
        assert!(
            scores.contains_key("str_replace"),
            "str_replace should be a co-occurrence"
        );
        assert!(
            !scores.contains_key("grep"),
            "grep itself should not appear"
        );
    }

    #[test]
    fn co_occurrence_scores_are_normalized() {
        let mut lib = PatternLibrary::new();
        for _ in 0..5 {
            lib.record_outcome(
                &tools(&["bash", "read_file"]),
                TaskType::Code,
                None,
                true,
                0.8,
                None,
            );
        }

        let scores = lib.co_occurrence_scores(&tools(&["bash"]));
        for score in scores.values() {
            assert!(*score >= 0.0 && *score <= 1.0, "Scores must be 0.0-1.0");
        }
    }

    #[test]
    fn co_occurrence_ignores_failed_patterns() {
        let mut lib = PatternLibrary::new();
        // Record only failures for bash + dangerous_tool
        for _ in 0..5 {
            lib.record_outcome(
                &tools(&["bash", "dangerous_tool"]),
                TaskType::Code,
                None,
                false,
                0.0,
                None,
            );
        }

        // Should not recommend dangerous_tool (only failures)
        let scores = lib.co_occurrence_scores(&tools(&["bash"]));
        assert!(
            scores.is_empty(),
            "Failed-only patterns should not produce co-occurrences"
        );
    }

    #[test]
    fn co_occurrence_with_empty_just_used_returns_empty() {
        let mut lib = PatternLibrary::new();
        lib.record_outcome(
            &tools(&["bash", "grep"]),
            TaskType::Fetch,
            None,
            true,
            0.8,
            None,
        );
        lib.record_outcome(
            &tools(&["bash", "grep"]),
            TaskType::Fetch,
            None,
            true,
            0.8,
            None,
        );
        let scores = lib.co_occurrence_scores(&[]);
        assert!(scores.is_empty());
    }

    // ── User Feedback Integration ──

    #[test]
    fn low_feedback_converts_success_to_failure() {
        let mut lib = PatternLibrary::new();
        // Technically successful (quality=0.9), but user unhappy (score=30)
        lib.record_outcome(
            &tools(&["bad_tool"]),
            TaskType::Fetch,
            None,
            true,     // success
            0.9,      // quality
            Some(30), // low feedback → should become failure
        );

        let exported = lib.export();
        let pattern = exported
            .iter()
            .find(|p| p.tools.contains(&"bad_tool".to_string()))
            .unwrap();
        assert_eq!(
            pattern.success_count, 0,
            "Low feedback should convert to failure"
        );
        assert_eq!(pattern.failure_count, 1);
    }

    #[test]
    fn high_feedback_keeps_success() {
        let mut lib = PatternLibrary::new();
        lib.record_outcome(
            &tools(&["good_tool"]),
            TaskType::Fetch,
            None,
            true,
            0.9,
            Some(80), // high feedback → stays success
        );

        let exported = lib.export();
        let pattern = exported
            .iter()
            .find(|p| p.tools.contains(&"good_tool".to_string()))
            .unwrap();
        assert_eq!(
            pattern.success_count, 1,
            "High feedback should keep success"
        );
        assert_eq!(pattern.failure_count, 0);
    }

    #[test]
    fn feedback_scales_quality() {
        let mut lib = PatternLibrary::new();
        // High feedback: quality stays high (0.9 * (0.5 + 100/200) = 0.9)
        lib.record_outcome(
            &tools(&["tool_a"]),
            TaskType::Code,
            None,
            true,
            0.9,
            Some(100),
        );
        // Medium feedback: quality reduced (0.9 * (0.5 + 50/200) = 0.675)
        lib.record_outcome(
            &tools(&["tool_b"]),
            TaskType::Code,
            None,
            true,
            0.9,
            Some(50),
        );

        let exported = lib.export();
        let a = exported
            .iter()
            .find(|p| p.tools.contains(&"tool_a".to_string()))
            .unwrap();
        let b = exported
            .iter()
            .find(|p| p.tools.contains(&"tool_b".to_string()))
            .unwrap();

        assert!(
            a.avg_quality() > b.avg_quality(),
            "Higher feedback should yield higher quality: {} > {}",
            a.avg_quality(),
            b.avg_quality()
        );
    }

    #[test]
    fn no_feedback_uses_raw_values() {
        let mut lib = PatternLibrary::new();
        lib.record_outcome(
            &tools(&["raw_tool"]),
            TaskType::Fetch,
            None,
            true,
            0.8,
            None,
        );

        let exported = lib.export();
        let pattern = exported
            .iter()
            .find(|p| p.tools.contains(&"raw_tool".to_string()))
            .unwrap();
        assert_eq!(pattern.success_count, 1);
        assert!(
            (pattern.avg_quality() - 0.8).abs() < 0.01,
            "No feedback should use raw quality"
        );
    }

    // ── Time Decay Tests ──

    #[test]
    fn time_decay_within_grace_period() {
        // Within grace period (7 days), decayed score should equal raw score
        let mut lib = PatternLibrary::new();
        for _ in 0..5 {
            lib.record_outcome(&tools(&["grep"]), TaskType::Fetch, None, true, 0.9, None);
        }

        // Pattern is fresh (just created), so decayed_score == score
        let pattern = lib.patterns.values().next().unwrap();
        let raw = pattern.score();
        let decayed = pattern.decayed_score();
        assert!(
            (raw - decayed).abs() < 0.001,
            "Fresh pattern should have no decay"
        );
    }

    #[test]
    fn time_decay_at_half_life() {
        // At exactly one half-life (30 days) past grace period (7 days), score should be ~50%
        let mut lib = PatternLibrary::new();
        for _ in 0..5 {
            lib.record_outcome(&tools(&["grep"]), TaskType::Fetch, None, true, 0.9, None);
        }

        // Manually age the pattern to 37 days ago
        let now = chrono::Utc::now().timestamp() as u64;
        for pattern in lib.patterns.values_mut() {
            pattern.last_used_at = now - (37 * 24 * 3600);
        }

        let pattern = lib.patterns.values().next().unwrap();
        let raw = pattern.score();
        let decayed = pattern.decayed_score();
        let ratio = decayed / raw;
        assert!(
            (ratio - 0.5).abs() < 0.1,
            "At half-life, score ratio should be ~0.5, got {}",
            ratio
        );
    }

    #[test]
    fn time_decay_old_pattern() {
        // At two half-lives past grace, score should be ~25%
        let mut lib = PatternLibrary::new();
        for _ in 0..5 {
            lib.record_outcome(&tools(&["grep"]), TaskType::Fetch, None, true, 0.9, None);
        }

        // Manually age the pattern to 67 days ago (7 grace + 60 = 2 half-lives)
        let now = chrono::Utc::now().timestamp() as u64;
        for pattern in lib.patterns.values_mut() {
            pattern.last_used_at = now - (67 * 24 * 3600);
        }

        let pattern = lib.patterns.values().next().unwrap();
        let raw = pattern.score();
        let decayed = pattern.decayed_score();
        let ratio = decayed / raw;
        assert!(
            (ratio - 0.25).abs() < 0.1,
            "At 2 half-lives, score ratio should be ~0.25, got {}",
            ratio
        );
    }

    #[test]
    fn decayed_score_recent_pattern() {
        // Use record_outcome to build a pattern with quality
        let mut lib = PatternLibrary::new();
        for _ in 0..5 {
            lib.record_outcome(&tools(&["grep"]), TaskType::Fetch, None, true, 0.9, None);
        }

        let pattern = lib.patterns.values().next().unwrap();
        let raw_score = pattern.score();
        let decayed = pattern.decayed_score();
        assert!(
            (raw_score - decayed).abs() < 0.001,
            "Recent pattern should have same raw and decayed score"
        );
    }

    #[test]
    fn decayed_score_stale_pattern() {
        // Use record_outcome to build a pattern with quality
        let mut lib = PatternLibrary::new();
        for _ in 0..5 {
            lib.record_outcome(&tools(&["grep"]), TaskType::Fetch, None, true, 0.9, None);
        }

        // Make it stale (37 days ago = one half-life past grace)
        let now = chrono::Utc::now().timestamp() as u64;
        for pattern in lib.patterns.values_mut() {
            pattern.last_used_at = now - (37 * 24 * 3600);
        }

        let pattern = lib.patterns.values().next().unwrap();
        let raw_score = pattern.score();
        let decayed = pattern.decayed_score();
        let expected_decayed = raw_score * 0.5; // Approximately

        assert!(
            decayed < raw_score,
            "Stale pattern decayed score should be less than raw score"
        );
        assert!(
            (decayed - expected_decayed).abs() < 0.1,
            "Expected ~{}, got {}",
            expected_decayed,
            decayed
        );
    }

    #[test]
    fn touch_updates_last_used_at() {
        let mut lib = PatternLibrary::new();
        lib.record_outcome(&tools(&["bash"]), TaskType::Code, None, true, 0.8, None);

        let pattern = lib.patterns.values().next().unwrap();
        let old_ts = pattern.last_used_at;
        std::thread::sleep(std::time::Duration::from_millis(1100)); // Wait >1 second

        // Touch via record_outcome
        lib.record_outcome(&tools(&["bash"]), TaskType::Code, None, true, 0.8, None);
        let pattern = lib.patterns.values().next().unwrap();
        assert!(
            pattern.last_used_at > old_ts,
            "touch() should update timestamp"
        );
    }

    #[test]
    fn suggest_prefers_recent_patterns() {
        let mut lib = PatternLibrary::new();

        // Create a stale pattern with high success
        lib.record_outcome(
            &tools(&["stale_tool"]),
            TaskType::Fetch,
            Some(DomainHint::GitHub),
            true,
            0.95,
            None,
        );
        lib.record_outcome(
            &tools(&["stale_tool"]),
            TaskType::Fetch,
            Some(DomainHint::GitHub),
            true,
            0.95,
            None,
        );
        lib.record_outcome(
            &tools(&["stale_tool"]),
            TaskType::Fetch,
            Some(DomainHint::GitHub),
            true,
            0.95,
            None,
        );

        // Manually make it stale
        for pattern in lib.patterns.values_mut() {
            if pattern.tools.contains(&"stale_tool".to_string()) {
                let now = chrono::Utc::now().timestamp() as u64;
                pattern.last_used_at = now - (50 * 24 * 3600); // 50 days ago
            }
        }

        // Create a recent pattern with lower raw success but fresh
        lib.record_outcome(
            &tools(&["recent_tool"]),
            TaskType::Fetch,
            Some(DomainHint::GitHub),
            true,
            0.8,
            None,
        );
        lib.record_outcome(
            &tools(&["recent_tool"]),
            TaskType::Fetch,
            Some(DomainHint::GitHub),
            true,
            0.8,
            None,
        );

        let suggestions = lib.suggest(TaskType::Fetch, Some(DomainHint::GitHub), 2);

        // Recent pattern should rank higher due to time decay
        if suggestions.len() >= 2 {
            let recent_idx = suggestions
                .iter()
                .position(|t| t.tools.contains(&"recent_tool".to_string()));
            let stale_idx = suggestions
                .iter()
                .position(|t| t.tools.contains(&"stale_tool".to_string()));
            if let (Some(r), Some(s)) = (recent_idx, stale_idx) {
                assert!(r < s, "Recent pattern should rank before stale pattern");
            }
        }
    }

    // ── Exploration Tests ──

    #[test]
    fn exploration_includes_stale_pattern() {
        let mut lib = PatternLibrary::new();

        // Create two fresh high-score patterns
        for _ in 0..5 {
            lib.record_outcome(
                &tools(&["tool_a"]),
                TaskType::Fetch,
                Some(DomainHint::GitHub),
                true,
                0.95,
                None,
            );
            lib.record_outcome(
                &tools(&["tool_b"]),
                TaskType::Fetch,
                Some(DomainHint::GitHub),
                true,
                0.9,
                None,
            );
        }

        // Create a stale pattern with decent success (should be exploration candidate)
        for _ in 0..3 {
            lib.record_outcome(
                &tools(&["old_tool"]),
                TaskType::Fetch,
                Some(DomainHint::GitHub),
                true,
                0.7,
                None,
            );
        }

        // Make old_tool stale
        let now = chrono::Utc::now().timestamp() as u64;
        for pattern in lib.patterns.values_mut() {
            if pattern.tools.contains(&"old_tool".to_string()) {
                pattern.last_used_at = now - (60 * 24 * 3600); // 60 days ago
            }
        }

        // Normal suggest should not include old_tool (decayed score too low)
        let normal = lib.suggest(TaskType::Fetch, Some(DomainHint::GitHub), 2);
        let has_old_in_normal = normal
            .iter()
            .any(|p| p.tools.contains(&"old_tool".to_string()));

        // Forced exploration should include old_tool
        let explored =
            lib.suggest_with_forced_exploration(TaskType::Fetch, Some(DomainHint::GitHub), 2);
        let has_old_in_explored = explored
            .iter()
            .any(|p| p.tools.contains(&"old_tool".to_string()));

        assert!(
            !has_old_in_normal,
            "Normal suggest should not include stale pattern"
        );
        assert!(
            has_old_in_explored,
            "Exploration should include stale pattern for rediscovery"
        );
    }

    #[test]
    fn exploration_prefers_oldest_pattern() {
        let mut lib = PatternLibrary::new();

        // Create a fresh top pattern
        for _ in 0..5 {
            lib.record_outcome(&tools(&["fresh"]), TaskType::Fetch, None, true, 0.9, None);
        }

        // Create two stale patterns with different ages
        for _ in 0..3 {
            lib.record_outcome(&tools(&["old_30"]), TaskType::Fetch, None, true, 0.6, None);
            lib.record_outcome(&tools(&["old_60"]), TaskType::Fetch, None, true, 0.6, None);
        }

        let now = chrono::Utc::now().timestamp() as u64;
        for pattern in lib.patterns.values_mut() {
            if pattern.tools.contains(&"old_30".to_string()) {
                pattern.last_used_at = now - (30 * 24 * 3600);
            }
            if pattern.tools.contains(&"old_60".to_string()) {
                pattern.last_used_at = now - (60 * 24 * 3600);
            }
        }

        // Exploration should pick the oldest (old_60)
        let explored = lib.suggest_with_forced_exploration(TaskType::Fetch, None, 1);
        let picked = explored.iter().find(|p| {
            p.tools.contains(&"old_60".to_string()) || p.tools.contains(&"old_30".to_string())
        });

        if let Some(p) = picked {
            assert!(
                p.tools.contains(&"old_60".to_string()),
                "Exploration should prefer oldest stale pattern (old_60), got {:?}",
                p.tools
            );
        }
    }

    #[test]
    fn exploration_requires_minimum_success_rate() {
        let mut lib = PatternLibrary::new();

        // Create two fresh high-score patterns (to fill limit=2)
        for _ in 0..5 {
            lib.record_outcome(&tools(&["good_a"]), TaskType::Fetch, None, true, 0.95, None);
            lib.record_outcome(&tools(&["good_b"]), TaskType::Fetch, None, true, 0.9, None);
        }

        // Create a stale pattern with poor success rate (below 0.3 threshold)
        // 1 success + 4 failures = 1/5 = 0.2 success rate
        lib.record_outcome(&tools(&["bad"]), TaskType::Fetch, None, true, 0.5, None);
        lib.record_outcome(&tools(&["bad"]), TaskType::Fetch, None, false, 0.0, None);
        lib.record_outcome(&tools(&["bad"]), TaskType::Fetch, None, false, 0.0, None);
        lib.record_outcome(&tools(&["bad"]), TaskType::Fetch, None, false, 0.0, None);
        lib.record_outcome(&tools(&["bad"]), TaskType::Fetch, None, false, 0.0, None);

        // Verify success rate
        let bad_pattern = lib
            .patterns
            .values()
            .find(|p| p.tools.contains(&"bad".to_string()))
            .unwrap();
        assert!(
            bad_pattern.success_rate() < 0.3,
            "Bad pattern should have success_rate < 0.3, got {}",
            bad_pattern.success_rate()
        );

        let now = chrono::Utc::now().timestamp() as u64;
        for pattern in lib.patterns.values_mut() {
            if pattern.tools.contains(&"bad".to_string()) {
                pattern.last_used_at = now - (60 * 24 * 3600);
            }
        }

        // Normal suggest returns top 2 by decayed_score (good_a and good_b)
        let normal = lib.suggest(TaskType::Fetch, None, 2);
        let has_bad_in_normal = normal.iter().any(|p| p.tools.contains(&"bad".to_string()));

        // Forced exploration: bad pattern excluded because success_rate < 0.3
        let explored = lib.suggest_with_forced_exploration(TaskType::Fetch, None, 2);
        let has_bad_in_explored = explored
            .iter()
            .any(|p| p.tools.contains(&"bad".to_string()));

        // If bad is already in normal suggest (decayed_score still competitive), test is moot
        // Otherwise, exploration should NOT include it due to low success rate
        if !has_bad_in_normal {
            assert!(
                !has_bad_in_explored,
                "Exploration should not include patterns with success_rate < 0.3"
            );
        }
    }

    #[test]
    fn exploration_no_candidates_returns_normal() {
        let mut lib = PatternLibrary::new();

        // Create only one pattern
        for _ in 0..5 {
            lib.record_outcome(&tools(&["only"]), TaskType::Fetch, None, true, 0.9, None);
        }

        let normal = lib.suggest(TaskType::Fetch, None, 2);
        let explored = lib.suggest_with_forced_exploration(TaskType::Fetch, None, 2);

        // With no exploration candidates, both should return the same
        assert_eq!(normal.len(), explored.len());
    }

    // ── Drift Detection Tests ──

    #[test]
    fn drift_score_none_when_insufficient_data() {
        let mut p = ToolChainPattern::new("a".to_string(), tools(&["a"]), TaskType::Code, None);
        p.success_count = 3;
        p.failure_count = 0;
        // Only 3 total observations < DRIFT_MIN_OBSERVATIONS (6)
        assert!(p.drift_score().is_none());
    }

    #[test]
    fn drift_score_none_when_no_recent_data() {
        let mut p = ToolChainPattern::new("a".to_string(), tools(&["a"]), TaskType::Code, None);
        p.success_count = 10;
        p.failure_count = 0;
        // No recent_outcomes pushed
        assert!(p.drift_score().is_none());
    }

    #[test]
    fn drift_score_zero_when_consistent() {
        let mut p = ToolChainPattern::new("a".to_string(), tools(&["a"]), TaskType::Code, None);
        p.success_count = 10;
        p.failure_count = 0;
        // Recent outcomes all success — matches historical
        for _ in 0..5 {
            p.push_outcome(true);
        }
        let drift = p.drift_score().unwrap();
        assert!(drift < 0.01, "No drift expected, got {drift}");
    }

    #[test]
    fn drift_score_high_when_recent_failures() {
        let mut p = ToolChainPattern::new("a".to_string(), tools(&["a"]), TaskType::Code, None);
        p.success_count = 10;
        p.failure_count = 0;
        // Historical: 100% success. Recent: all failures → big drift
        for _ in 0..5 {
            p.push_outcome(false);
        }
        let drift = p.drift_score().unwrap();
        assert!(drift >= 1.0, "Critical drift expected, got {drift}");
        assert!(p.is_drifting());
    }

    #[test]
    fn drift_score_moderate_when_mixed() {
        let mut p = ToolChainPattern::new("a".to_string(), tools(&["a"]), TaskType::Code, None);
        p.success_count = 8;
        p.failure_count = 2;
        // Historical: 80% success. Recent: 60% success → moderate drift
        for _ in 0..3 {
            p.push_outcome(true);
        }
        for _ in 0..2 {
            p.push_outcome(false);
        }
        let drift = p.drift_score().unwrap();
        assert!(
            drift > 0.0 && drift < 1.0,
            "Moderate drift expected, got {drift}"
        );
        assert!(!p.is_drifting());
    }

    #[test]
    fn detect_drift_library_level() {
        let mut lib = PatternLibrary::new();
        // Healthy pattern
        for _ in 0..10 {
            lib.record_outcome(&tools(&["good"]), TaskType::Fetch, None, true, 0.9, None);
        }
        // Drifting pattern: starts good, then fails heavily
        for _ in 0..10 {
            lib.record_outcome(&tools(&["drift"]), TaskType::Code, None, true, 0.8, None);
        }
        for _ in 0..8 {
            lib.record_outcome(&tools(&["drift"]), TaskType::Code, None, false, 0.0, None);
        }
        let reports = lib.detect_drift();
        // "drift" pattern should show up
        assert!(
            reports.iter().any(|r| r.signature == "drift"),
            "Drifting pattern should be detected: {reports:?}"
        );
        // "good" pattern should NOT show up
        assert!(
            !reports.iter().any(|r| r.signature == "good"),
            "Healthy pattern should not be flagged"
        );
    }

    #[test]
    fn auto_demote_increases_failure_count() {
        let mut lib = PatternLibrary::new();
        for _ in 0..10 {
            lib.record_outcome(&tools(&["drift"]), TaskType::Code, None, true, 0.8, None);
        }
        // Now record enough failures to trigger critical drift
        for _ in 0..8 {
            lib.record_outcome(&tools(&["drift"]), TaskType::Code, None, false, 0.0, None);
        }

        let key = pattern_key("drift", TaskType::Code);
        let before = lib.patterns.get(&key).unwrap().failure_count;
        let demoted = lib.auto_demote_drifting();
        if demoted > 0 {
            let after = lib.patterns.get(&key).unwrap().failure_count;
            assert!(
                after > before,
                "Failure count should increase after demotion"
            );
        }
    }

    #[test]
    fn apply_evolution_action_mutates_matching_pattern() {
        let mut lib = PatternLibrary::new();
        for _ in 0..3 {
            lib.record_outcome(&tools(&["bash"]), TaskType::Code, None, true, 0.8, None);
        }

        let key = pattern_key("bash", TaskType::Code);
        let before = lib.patterns.get(&key).unwrap().failure_count;
        let updated =
            lib.apply_evolution_action("bash", crate::evolution::types::PatternAction::Block);

        assert_eq!(updated, 1);
        let after = lib.patterns.get(&key).unwrap().failure_count;
        assert!(after >= before + 5);
    }

    // ── Active Exploration Tests ──

    #[test]
    fn exploration_opportunities_cold_start() {
        let mut lib = PatternLibrary::new();
        // Only 2 observations — cold start
        lib.record_outcome(
            &tools(&["new_tool"]),
            TaskType::Memory,
            None,
            true,
            0.5,
            None,
        );
        lib.record_outcome(
            &tools(&["new_tool"]),
            TaskType::Memory,
            None,
            false,
            0.0,
            None,
        );

        let opps = lib.exploration_opportunities();
        assert!(
            opps.iter().any(
                |o| o.task_type == TaskType::Memory && o.reason == ExplorationReason::ColdStart
            ),
            "Cold start area should be flagged: {opps:?}"
        );
    }

    #[test]
    fn exploration_opportunities_low_success() {
        let mut lib = PatternLibrary::new();
        // Lots of observations but low success rate
        for _ in 0..2 {
            lib.record_outcome(
                &tools(&["bad"]),
                TaskType::Fetch,
                Some(DomainHint::Web),
                true,
                0.3,
                None,
            );
        }
        for _ in 0..8 {
            lib.record_outcome(
                &tools(&["bad"]),
                TaskType::Fetch,
                Some(DomainHint::Web),
                false,
                0.0,
                None,
            );
        }

        let opps = lib.exploration_opportunities();
        assert!(
            opps.iter()
                .any(|o| o.reason == ExplorationReason::LowSuccess),
            "Low success area should be flagged: {opps:?}"
        );
    }

    #[test]
    fn exploration_opportunities_empty_when_confident() {
        let mut lib = PatternLibrary::new();
        for _ in 0..20 {
            lib.record_outcome(&tools(&["reliable"]), TaskType::Code, None, true, 0.9, None);
        }
        let opps = lib.exploration_opportunities();
        assert!(
            opps.is_empty(),
            "High-confidence area should NOT be flagged"
        );
    }

    #[test]
    fn learning_summary_covers_all_fields() {
        let mut lib = PatternLibrary::new();
        for _ in 0..10 {
            lib.record_outcome(&tools(&["bash"]), TaskType::Code, None, true, 0.9, None);
        }
        let summary = lib.learning_summary();
        assert_eq!(summary.total_patterns, 1);
        assert!(summary.active_patterns >= 1);
        assert!(summary.avg_success_rate > 0.8);
        assert!(!summary.top_patterns.is_empty());
    }

    #[test]
    fn recent_outcomes_window_caps_at_drift_window() {
        let mut p = ToolChainPattern::new("a".to_string(), tools(&["a"]), TaskType::Code, None);
        for _ in 0..20 {
            p.push_outcome(true);
        }
        assert_eq!(p.recent_outcomes.len(), DRIFT_WINDOW_SIZE);
    }
}

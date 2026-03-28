//! Tool Chain Pattern Library — learns successful tool sequences for reuse.
//!
//! Records which tool combinations succeed or fail for each task type and domain,
//! then suggests the best patterns for similar future queries.
//!
//! # Learning flow
//!
//! 1. User asks "show me PRs for matrixorigin" → routes to Fetch + GitHub
//! 2. Agent uses [github_search, github_list_prs] → succeeds (quality 0.9)
//! 3. Pattern recorded: signature="github_list_prs|github_search", task=Fetch, domain=GitHub
//! 4. Next similar query → suggest() returns this pattern → boost these tools
//!
//! # Integration
//!
//! ```rust,ignore
//! // At turn end (Evaluate → Complete):
//! library.record_outcome(&tools_used, TaskType::Fetch, Some(DomainHint::GitHub), true, 0.9);
//!
//! // At turn start (Plan):
//! let suggestions = library.suggest(TaskType::Fetch, Some(DomainHint::GitHub), 3);
//! let boost_terms = library.boost_terms_for(TaskType::Fetch, Some(DomainHint::GitHub));
//! ```

use super::routing::{DomainHint, TaskType};
use std::collections::HashMap;

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

impl PatternLibrary {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the outcome of a tool chain execution.
    ///
    /// Called at turn end (Evaluate → Complete) with the tools that were used,
    /// the task type, domain, whether it succeeded, and quality score.
    pub fn record_outcome(
        &mut self,
        tools: &[String],
        task_type: TaskType,
        domain: Option<DomainHint>,
        success: bool,
        quality: f64,
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

        if success {
            pattern.success_count += 1;
            pattern.quality_sum += quality.clamp(0.0, 1.0);
        } else {
            pattern.failure_count += 1;
        }
    }

    /// Suggest best patterns for a task type + optional domain filter.
    ///
    /// Returns up to `limit` patterns sorted by combined score (descending).
    /// If domain is Some, only returns patterns matching that domain.
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

        // Sort by score descending
        candidates.sort_by(|a, b| {
            b.score()
                .partial_cmp(&a.score())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        candidates.truncate(limit);
        candidates
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

    /// Export all patterns for persistence.
    pub fn export(&self) -> Vec<ToolChainPattern> {
        self.patterns.values().cloned().collect()
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
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
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
        lib.record_outcome(&tools(&["bash"]), TaskType::Code, None, false, 0.0);
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
            );
        }
        lib.record_outcome(
            &tools(&["github_search", "github_api"]),
            TaskType::Fetch,
            Some(DomainHint::GitHub),
            false,
            0.0,
        );
        let exported = lib.export();
        assert_eq!(exported[0].success_count, 5);
        assert_eq!(exported[0].failure_count, 1);
        assert!(exported[0].success_rate() > 0.8);
    }

    #[test]
    fn record_empty_tools_ignored() {
        let mut lib = PatternLibrary::new();
        lib.record_outcome(&[], TaskType::Fetch, None, true, 0.9);
        assert!(lib.is_empty());
    }

    #[test]
    fn different_task_types_different_patterns() {
        let mut lib = PatternLibrary::new();
        lib.record_outcome(&tools(&["bash"]), TaskType::Code, None, true, 0.9);
        lib.record_outcome(&tools(&["bash"]), TaskType::Fetch, None, true, 0.8);
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
            );
        }
        for _ in 0..3 {
            lib.record_outcome(&tools(&["bash"]), TaskType::Code, None, true, 0.8);
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
            );
        }
        for _ in 0..3 {
            lib.record_outcome(
                &tools(&["bash"]),
                TaskType::Fetch,
                Some(DomainHint::System),
                true,
                0.8,
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
        lib.record_outcome(&tools(&["bash"]), TaskType::Code, None, true, 0.9);
        // Only 1 observation → not suggested
        assert!(lib.suggest(TaskType::Code, None, 5).is_empty());
    }

    #[test]
    fn suggest_sorted_by_score() {
        let mut lib = PatternLibrary::new();
        // Pattern A: high quality
        for _ in 0..5 {
            lib.record_outcome(&tools(&["pattern_a"]), TaskType::Fetch, None, true, 0.95);
        }
        // Pattern B: lower quality
        for _ in 0..5 {
            lib.record_outcome(&tools(&["pattern_b"]), TaskType::Fetch, None, true, 0.5);
        }
        // Pattern C: mixed success
        for _ in 0..3 {
            lib.record_outcome(&tools(&["pattern_c"]), TaskType::Fetch, None, true, 0.7);
        }
        for _ in 0..3 {
            lib.record_outcome(&tools(&["pattern_c"]), TaskType::Fetch, None, false, 0.0);
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
                lib.record_outcome(std::slice::from_ref(&name), TaskType::Code, None, true, 0.8);
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
            lib.record_outcome(&tools(&["bash", "grep"]), TaskType::Code, None, true, 0.9);
        }
        for _ in 0..3 {
            lib.record_outcome(&tools(&["bash", "sed"]), TaskType::Code, None, true, 0.8);
        }
        let terms = lib.boost_terms_for(TaskType::Code, None);
        let bash_count = terms.iter().filter(|t| *t == "bash").count();
        assert_eq!(bash_count, 1, "bash should appear only once");
    }

    #[test]
    fn boost_terms_excludes_low_success_rate() {
        let mut lib = PatternLibrary::new();
        for _ in 0..2 {
            lib.record_outcome(&tools(&["flaky_tool"]), TaskType::Code, None, true, 0.3);
        }
        for _ in 0..5 {
            lib.record_outcome(&tools(&["flaky_tool"]), TaskType::Code, None, false, 0.0);
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
            lib.record_outcome(&tools(&["bash"]), TaskType::Code, None, true, 0.9);
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
            lib.record_outcome(&tools(&["grep"]), TaskType::Fetch, None, true, 0.9);
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
        lib.record_outcome(&tools(&["bash", "grep"]), TaskType::Fetch, None, true, 0.8);
        lib.record_outcome(&tools(&["bash", "grep"]), TaskType::Fetch, None, true, 0.8);
        let scores = lib.co_occurrence_scores(&[]);
        assert!(scores.is_empty());
    }
}

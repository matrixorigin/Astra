//! Version-vector-based N-way learning merge for multi-agent teams.
//!
//! Each agent produces [`AgentLearning`] outcomes during execution. After a team
//! run completes, [`merge_agent_learnings`] aggregates them using:
//!
//! - **Version vectors** for causal ordering and incremental sync
//! - **Quality-weighted consensus** for pattern aggregation
//! - **Union semantics** for discovered facts
//! - **Caution propagation** — any agent flagging a pattern as failed → cautionary
//! - **Conflict detection** — patterns appearing in both success and failed lists
//!
//! ## Incremental sync
//!
//! [`merge_incremental`] builds on a previous [`MergedLearning`], accumulating
//! results across multiple team runs without reprocessing old data.
//! [`VersionVector::updates_since`] identifies which agents have new data.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

// ─── Version Vector ─────────────────────────────────────────────────────────

/// Logical clock vector for tracking causal ordering across agents.
///
/// Used to determine happened-before / concurrent relationships when merging
/// learning outcomes from 3+ agents.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct VersionVector {
    /// agent_id → logical clock value.
    pub clocks: HashMap<String, u64>,
}

impl VersionVector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Increment the clock for the given agent.
    pub fn increment(&mut self, agent_id: &str) {
        *self.clocks.entry(agent_id.to_string()).or_insert(0) += 1;
    }

    /// Merge another vector into this one (element-wise max).
    pub fn merge(&mut self, other: &VersionVector) {
        for (k, v) in &other.clocks {
            let entry = self.clocks.entry(k.clone()).or_insert(0);
            *entry = (*entry).max(*v);
        }
    }

    /// Returns true if `self` causally happened before `other`.
    ///
    /// `self ≤ other` component-wise AND `self ≠ other` (missing keys treated as 0).
    pub fn happened_before(&self, other: &VersionVector) -> bool {
        // All keys in self must be ≤ corresponding value in other
        let all_le = self.clocks.iter().all(|(k, v)| *v <= other.get(k));
        if !all_le {
            return false;
        }
        // Must be strictly less in at least one dimension (across all keys in either vector)
        self.clocks.iter().any(|(k, v)| *v < other.get(k))
            || other.clocks.iter().any(|(k, v)| *v > self.get(k))
    }

    /// Returns true if the events are concurrent (incomparable).
    pub fn concurrent_with(&self, other: &VersionVector) -> bool {
        !self.happened_before(other) && !other.happened_before(self) && !self.vv_equal(other)
    }

    /// VV-aware equality (treats missing keys as 0).
    fn vv_equal(&self, other: &VersionVector) -> bool {
        self.clocks.iter().all(|(k, v)| *v == other.get(k))
            && other.clocks.iter().all(|(k, v)| *v == self.get(k))
    }

    /// Get the clock value for an agent (0 if not present).
    pub fn get(&self, agent_id: &str) -> u64 {
        self.clocks.get(agent_id).copied().unwrap_or(0)
    }

    /// Return agent IDs that have progressed since `reference`.
    ///
    /// An agent has new data if its clock in `self` is strictly greater than
    /// the corresponding clock in `reference`.
    pub fn updates_since(&self, reference: &VersionVector) -> Vec<String> {
        self.clocks
            .iter()
            .filter(|(k, v)| **v > reference.get(k))
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Return the set of all agent IDs that have contributed.
    pub fn agent_ids(&self) -> Vec<String> {
        self.clocks.keys().cloned().collect()
    }

    /// True if this vector dominates `other` (≥ in every dimension).
    pub fn dominates(&self, other: &VersionVector) -> bool {
        other.clocks.iter().all(|(k, v)| self.get(k) >= *v)
    }
}

// ─── Learning Types ─────────────────────────────────────────────────────────

/// A behavioural pattern discovered by an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearningPattern {
    pub name: String,
    pub frequency: u32,
    pub success_rate: f64,
    pub context: String,
}

/// Learning outcomes from a single agent's execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentLearning {
    pub agent_id: String,
    pub session_id: String,
    pub version: VersionVector,
    pub successful_patterns: Vec<LearningPattern>,
    pub failed_patterns: Vec<LearningPattern>,
    pub discovered_facts: Vec<String>,
    pub quality_score: f64,
}

/// Aggregated learning from N agents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergedLearning {
    /// Merged version vector (element-wise max across all agents).
    pub version: VersionVector,
    /// Patterns endorsed by a weighted majority of agents.
    pub consensus_patterns: Vec<LearningPattern>,
    /// Pattern names flagged as failed by any agent.
    pub cautionary_patterns: Vec<String>,
    /// Patterns that appear in both success and failed lists (needs human review).
    pub conflicted_patterns: Vec<String>,
    /// De-duplicated facts discovered across all agents.
    pub facts: Vec<String>,
    /// Number of agents that contributed.
    pub agent_count: usize,
    /// Sum of quality scores across all contributing agents.
    pub total_quality: f64,
}

// ─── Merge Logic ────────────────────────────────────────────────────────────

/// Merge learning outcomes from N agents into a single aggregated result.
///
/// # Consensus rules
///
/// - A pattern is **consensus** if its quality-weighted vote sum ≥ 50% of total quality.
/// - A pattern is **cautionary** if *any* agent flagged it as failed.
/// - A pattern is **conflicted** if it appears in both success and failed lists.
/// - Facts are de-duplicated by exact string match (weighted by agent quality for ordering).
/// - The merged version vector is the element-wise max across all agents.
pub fn merge_agent_learnings(learnings: &[AgentLearning]) -> MergedLearning {
    if learnings.is_empty() {
        return MergedLearning {
            version: VersionVector::new(),
            consensus_patterns: vec![],
            cautionary_patterns: vec![],
            conflicted_patterns: vec![],
            facts: vec![],
            agent_count: 0,
            total_quality: 0.0,
        };
    }

    let mut merged_version = VersionVector::new();
    // pattern_name → Vec<(quality_score, success_rate, frequency)>
    let mut pattern_votes: HashMap<String, Vec<(f64, f64, u32)>> = HashMap::new();
    let mut failed_set: HashSet<String> = HashSet::new();
    let mut success_set: HashSet<String> = HashSet::new();
    let mut fact_weights: HashMap<String, f64> = HashMap::new();
    let mut total_quality: f64 = 0.0;

    for l in learnings {
        merged_version.merge(&l.version);
        total_quality += l.quality_score;

        for p in &l.successful_patterns {
            pattern_votes.entry(p.name.clone()).or_default().push((
                l.quality_score,
                p.success_rate,
                p.frequency,
            ));
            success_set.insert(p.name.clone());
        }

        for p in &l.failed_patterns {
            failed_set.insert(p.name.clone());
        }

        for fact in &l.discovered_facts {
            let weight = fact_weights.entry(fact.clone()).or_insert(0.0);
            *weight += l.quality_score;
        }
    }

    // Quality-weighted threshold: sum of quality for endorsing agents must be
    // ≥ 50% of total quality. For equal-quality agents, this reduces to majority.
    let quality_threshold = total_quality / 2.0;

    let consensus_patterns: Vec<LearningPattern> = pattern_votes
        .iter()
        .filter(|(_, votes)| {
            let quality_sum: f64 = votes.iter().map(|(q, _, _)| q).sum();
            quality_sum >= quality_threshold
        })
        .map(|(name, votes)| {
            let total_quality_for_pattern: f64 = votes.iter().map(|(q, _, _)| q).sum();
            // Quality-weighted average success rate
            let weighted_rate: f64 =
                votes.iter().map(|(q, r, _)| q * r).sum::<f64>() / total_quality_for_pattern;
            let total_freq: u32 = votes.iter().map(|(_, _, f)| f).sum();
            LearningPattern {
                name: name.clone(),
                frequency: total_freq,
                success_rate: weighted_rate,
                context: format!(
                    "consensus from {} of {} agents (quality weight {:.1}/{:.1})",
                    votes.len(),
                    learnings.len(),
                    total_quality_for_pattern,
                    total_quality,
                ),
            }
        })
        .collect();

    // Detect conflicted patterns (in both success and failed sets)
    let conflicted: Vec<String> = success_set.intersection(&failed_set).cloned().collect();
    let mut conflicted_sorted = conflicted;
    conflicted_sorted.sort();

    // Sort facts by aggregate quality weight (descending)
    let mut facts: Vec<(String, f64)> = fact_weights.into_iter().collect();
    facts.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let facts: Vec<String> = facts.into_iter().map(|(f, _)| f).collect();

    let mut cautionary: Vec<String> = failed_set.into_iter().collect();
    cautionary.sort();

    MergedLearning {
        version: merged_version,
        consensus_patterns,
        cautionary_patterns: cautionary,
        conflicted_patterns: conflicted_sorted,
        facts,
        agent_count: learnings.len(),
        total_quality,
    }
}

/// Incrementally merge new agent learnings into an existing [`MergedLearning`].
///
/// Instead of reprocessing all historical data, this:
/// 1. Filters `new_learnings` to only agents that have updated since `existing.version`
/// 2. Merges their contributions into the existing result
///
/// When `existing` has no prior data (`agent_count == 0`), this is equivalent to
/// calling [`merge_agent_learnings`] directly.
pub fn merge_incremental(
    existing: &MergedLearning,
    new_learnings: &[AgentLearning],
) -> MergedLearning {
    if existing.agent_count == 0 {
        return merge_agent_learnings(new_learnings);
    }

    // Filter to agents with genuinely new data
    let updated: Vec<&AgentLearning> = new_learnings
        .iter()
        .filter(|l| {
            let their_clock = l.version.get(&l.agent_id);
            let our_clock = existing.version.get(&l.agent_id);
            their_clock > our_clock
        })
        .collect();

    if updated.is_empty() {
        return existing.clone();
    }

    let mut merged_version = existing.version.clone();
    let mut total_quality = existing.total_quality;

    // Carry forward existing consensus patterns with their quality weights
    let mut pattern_votes: HashMap<String, Vec<(f64, f64, u32)>> = HashMap::new();
    for p in &existing.consensus_patterns {
        // Re-encode existing consensus as a "virtual vote" with existing total quality
        pattern_votes.entry(p.name.clone()).or_default().push((
            existing.total_quality,
            p.success_rate,
            p.frequency,
        ));
    }

    let mut failed_set: HashSet<String> = existing.cautionary_patterns.iter().cloned().collect();
    let mut success_set: HashSet<String> = existing
        .consensus_patterns
        .iter()
        .map(|p| p.name.clone())
        .collect();
    let mut fact_set: HashSet<String> = existing.facts.iter().cloned().collect();
    let mut new_facts: Vec<String> = Vec::new();

    for l in &updated {
        merged_version.merge(&l.version);
        total_quality += l.quality_score;

        for p in &l.successful_patterns {
            pattern_votes.entry(p.name.clone()).or_default().push((
                l.quality_score,
                p.success_rate,
                p.frequency,
            ));
            success_set.insert(p.name.clone());
        }

        for p in &l.failed_patterns {
            failed_set.insert(p.name.clone());
        }

        for fact in &l.discovered_facts {
            if fact_set.insert(fact.clone()) {
                new_facts.push(fact.clone());
            }
        }
    }

    let quality_threshold = total_quality / 2.0;
    let total_agents = existing.agent_count + updated.len();

    let consensus_patterns: Vec<LearningPattern> = pattern_votes
        .iter()
        .filter(|(_, votes)| {
            let quality_sum: f64 = votes.iter().map(|(q, _, _)| q).sum();
            quality_sum >= quality_threshold
        })
        .map(|(name, votes)| {
            let total_q: f64 = votes.iter().map(|(q, _, _)| q).sum();
            let weighted_rate: f64 = votes.iter().map(|(q, r, _)| q * r).sum::<f64>() / total_q;
            let total_freq: u32 = votes.iter().map(|(_, _, f)| f).sum();
            LearningPattern {
                name: name.clone(),
                frequency: total_freq,
                success_rate: weighted_rate,
                context: format!("incremental merge ({} agents total)", total_agents),
            }
        })
        .collect();

    let conflicted: Vec<String> = success_set.intersection(&failed_set).cloned().collect();
    let mut conflicted_sorted = conflicted;
    conflicted_sorted.sort();

    // Combine existing facts with new ones (existing order preserved)
    let mut all_facts = existing.facts.clone();
    all_facts.extend(new_facts);

    let mut cautionary: Vec<String> = failed_set.into_iter().collect();
    cautionary.sort();

    MergedLearning {
        version: merged_version,
        consensus_patterns,
        cautionary_patterns: cautionary,
        conflicted_patterns: conflicted_sorted,
        facts: all_facts,
        agent_count: total_agents,
        total_quality,
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_vv(entries: &[(&str, u64)]) -> VersionVector {
        let mut vv = VersionVector::new();
        for (k, v) in entries {
            vv.clocks.insert(k.to_string(), *v);
        }
        vv
    }

    // ── VersionVector ──

    #[test]
    fn vv_increment() {
        let mut vv = VersionVector::new();
        vv.increment("a");
        vv.increment("a");
        vv.increment("b");
        assert_eq!(vv.get("a"), 2);
        assert_eq!(vv.get("b"), 1);
        assert_eq!(vv.get("c"), 0);
    }

    #[test]
    fn vv_merge_takes_max() {
        let mut v1 = make_vv(&[("a", 3), ("b", 1)]);
        let v2 = make_vv(&[("a", 1), ("b", 5), ("c", 2)]);
        v1.merge(&v2);
        assert_eq!(v1.get("a"), 3);
        assert_eq!(v1.get("b"), 5);
        assert_eq!(v1.get("c"), 2);
    }

    #[test]
    fn vv_happened_before() {
        let v1 = make_vv(&[("a", 1), ("b", 2)]);
        let v2 = make_vv(&[("a", 2), ("b", 3)]);
        assert!(v1.happened_before(&v2));
        assert!(!v2.happened_before(&v1));
    }

    #[test]
    fn vv_equal_not_happened_before() {
        let v1 = make_vv(&[("a", 1)]);
        let v2 = make_vv(&[("a", 1)]);
        assert!(!v1.happened_before(&v2));
    }

    #[test]
    fn vv_concurrent() {
        let v1 = make_vv(&[("a", 2), ("b", 1)]);
        let v2 = make_vv(&[("a", 1), ("b", 2)]);
        assert!(v1.concurrent_with(&v2));
        assert!(v2.concurrent_with(&v1));
    }

    #[test]
    fn vv_not_concurrent_when_ordered() {
        let v1 = make_vv(&[("a", 1)]);
        let v2 = make_vv(&[("a", 2)]);
        assert!(!v1.concurrent_with(&v2));
    }

    #[test]
    fn vv_missing_key_means_zero() {
        let v1 = make_vv(&[("a", 1)]);
        let v2 = make_vv(&[("a", 1), ("b", 1)]);
        // v1={a:1} with missing "b" treated as 0
        // so v1={a:1,b:0} < v2={a:1,b:1} → happened_before
        assert!(v1.happened_before(&v2));
    }

    // ── Merge Logic ──

    fn make_learning(
        agent_id: &str,
        success: &[(&str, f64)],
        failed: &[&str],
        facts: &[&str],
        quality: f64,
    ) -> AgentLearning {
        AgentLearning {
            agent_id: agent_id.to_string(),
            session_id: "session-1".to_string(),
            version: {
                let mut vv = VersionVector::new();
                vv.increment(agent_id);
                vv
            },
            successful_patterns: success
                .iter()
                .map(|(name, rate)| LearningPattern {
                    name: name.to_string(),
                    frequency: 1,
                    success_rate: *rate,
                    context: "test".to_string(),
                })
                .collect(),
            failed_patterns: failed
                .iter()
                .map(|name| LearningPattern {
                    name: name.to_string(),
                    frequency: 1,
                    success_rate: 0.0,
                    context: "test".to_string(),
                })
                .collect(),
            discovered_facts: facts.iter().map(|f| f.to_string()).collect(),
            quality_score: quality,
        }
    }

    #[test]
    fn merge_empty() {
        let result = merge_agent_learnings(&[]);
        assert_eq!(result.agent_count, 0);
        assert!(result.consensus_patterns.is_empty());
    }

    #[test]
    fn merge_single_agent() {
        let l = make_learning("a1", &[("pattern-x", 0.9)], &[], &["fact-1"], 0.8);
        let result = merge_agent_learnings(&[l]);
        assert_eq!(result.agent_count, 1);
        // With 1 agent, threshold is ceil(0.5) = 1, so the pattern is consensus
        assert_eq!(result.consensus_patterns.len(), 1);
        assert_eq!(result.consensus_patterns[0].name, "pattern-x");
        assert_eq!(result.facts, vec!["fact-1"]);
    }

    #[test]
    fn merge_majority_consensus() {
        // 3 agents: pattern-a in 2/3 → consensus; pattern-b in 1/3 → not
        let l1 = make_learning(
            "a1",
            &[("pattern-a", 0.9), ("pattern-b", 0.5)],
            &[],
            &[],
            0.8,
        );
        let l2 = make_learning("a2", &[("pattern-a", 0.8)], &[], &[], 0.7);
        let l3 = make_learning("a3", &[("pattern-c", 0.7)], &[], &[], 0.6);
        let result = merge_agent_learnings(&[l1, l2, l3]);
        // threshold = ceil(3/2) = 2
        assert_eq!(result.agent_count, 3);
        let names: Vec<_> = result
            .consensus_patterns
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert!(names.contains(&"pattern-a"));
        assert!(!names.contains(&"pattern-b"));
        assert!(!names.contains(&"pattern-c"));
    }

    #[test]
    fn merge_caution_from_any_agent() {
        let l1 = make_learning("a1", &[("good", 0.9)], &["bad-pattern"], &[], 0.8);
        let l2 = make_learning("a2", &[("good", 0.8)], &[], &[], 0.7);
        let result = merge_agent_learnings(&[l1, l2]);
        assert!(
            result
                .cautionary_patterns
                .contains(&"bad-pattern".to_string())
        );
    }

    #[test]
    fn merge_facts_deduplicated_ordered_by_quality() {
        let l1 = make_learning("a1", &[], &[], &["fact-shared", "fact-a-only"], 0.5);
        let l2 = make_learning("a2", &[], &[], &["fact-shared", "fact-b-only"], 0.9);
        let result = merge_agent_learnings(&[l1, l2]);
        assert_eq!(result.facts.len(), 3);
        // fact-shared has weight 0.5 + 0.9 = 1.4 (highest)
        assert_eq!(result.facts[0], "fact-shared");
    }

    #[test]
    fn merge_version_vectors_combined() {
        let l1 = make_learning("a1", &[], &[], &[], 0.5);
        let l2 = make_learning("a2", &[], &[], &[], 0.5);
        let result = merge_agent_learnings(&[l1, l2]);
        assert_eq!(result.version.get("a1"), 1);
        assert_eq!(result.version.get("a2"), 1);
    }

    #[test]
    fn merge_consensus_aggregates_rates() {
        let l1 = make_learning("a1", &[("p", 0.8)], &[], &[], 0.5);
        let l2 = make_learning("a2", &[("p", 0.6)], &[], &[], 0.5);
        let result = merge_agent_learnings(&[l1, l2]);
        let p = &result.consensus_patterns[0];
        assert!((p.success_rate - 0.7).abs() < 0.001); // quality-weighted avg of 0.8 and 0.6 (equal quality)
        assert_eq!(p.frequency, 2); // 1 + 1
    }

    // ── Quality-weighted consensus ──

    #[test]
    fn high_quality_agent_drives_consensus() {
        // 3 agents: pattern-a endorsed by high-quality agent only (quality=2.0)
        // Low-quality agents (0.3 each) don't endorse it
        // Total quality = 2.6, threshold = 1.3, high agent's weight 2.0 ≥ 1.3 → consensus
        let l1 = make_learning("a1", &[("pattern-a", 0.95)], &[], &[], 2.0);
        let l2 = make_learning("a2", &[("pattern-b", 0.5)], &[], &[], 0.3);
        let l3 = make_learning("a3", &[("pattern-b", 0.5)], &[], &[], 0.3);
        let result = merge_agent_learnings(&[l1, l2, l3]);
        let names: Vec<_> = result
            .consensus_patterns
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert!(
            names.contains(&"pattern-a"),
            "high-quality agent should drive consensus"
        );
    }

    #[test]
    fn low_quality_majority_insufficient() {
        // 3 agents: 2 low-quality agree on pattern-a (0.2 each), 1 high disagrees (1.6)
        // Total quality = 2.0, threshold = 1.0
        // pattern-a quality sum = 0.4 < 1.0 → NOT consensus
        let l1 = make_learning("a1", &[("pattern-a", 0.5)], &[], &[], 0.2);
        let l2 = make_learning("a2", &[("pattern-a", 0.5)], &[], &[], 0.2);
        let l3 = make_learning("a3", &[("pattern-b", 0.9)], &[], &[], 1.6);
        let result = merge_agent_learnings(&[l1, l2, l3]);
        let names: Vec<_> = result
            .consensus_patterns
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert!(
            !names.contains(&"pattern-a"),
            "low-quality majority should not reach threshold"
        );
        assert!(
            names.contains(&"pattern-b"),
            "high-quality single agent should reach threshold"
        );
    }

    #[test]
    fn total_quality_tracked() {
        let l1 = make_learning("a1", &[], &[], &[], 0.8);
        let l2 = make_learning("a2", &[], &[], &[], 0.6);
        let result = merge_agent_learnings(&[l1, l2]);
        assert!((result.total_quality - 1.4).abs() < 0.001);
    }

    // ── Conflict detection ──

    #[test]
    fn conflict_detected_when_success_and_fail() {
        let l1 = make_learning("a1", &[("risky-pattern", 0.8)], &[], &[], 0.5);
        let l2 = make_learning("a2", &[], &["risky-pattern"], &[], 0.5);
        let result = merge_agent_learnings(&[l1, l2]);
        assert!(
            result
                .conflicted_patterns
                .contains(&"risky-pattern".to_string())
        );
        assert!(
            result
                .cautionary_patterns
                .contains(&"risky-pattern".to_string())
        );
    }

    #[test]
    fn no_conflict_when_only_success() {
        let l1 = make_learning("a1", &[("good-pattern", 0.9)], &[], &[], 0.5);
        let l2 = make_learning("a2", &[("good-pattern", 0.8)], &[], &[], 0.5);
        let result = merge_agent_learnings(&[l1, l2]);
        assert!(result.conflicted_patterns.is_empty());
    }

    // ── VersionVector new methods ──

    #[test]
    fn vv_updates_since() {
        let v1 = make_vv(&[("a", 3), ("b", 5), ("c", 1)]);
        let ref_v = make_vv(&[("a", 3), ("b", 2)]);
        let mut updates = v1.updates_since(&ref_v);
        updates.sort();
        // b advanced (5 > 2), c is new (1 > 0), a stayed (3 == 3)
        assert_eq!(updates, vec!["b", "c"]);
    }

    #[test]
    fn vv_updates_since_empty_ref() {
        let v1 = make_vv(&[("a", 1), ("b", 2)]);
        let ref_v = VersionVector::new();
        let mut updates = v1.updates_since(&ref_v);
        updates.sort();
        assert_eq!(updates, vec!["a", "b"]);
    }

    #[test]
    fn vv_dominates() {
        let v1 = make_vv(&[("a", 3), ("b", 5)]);
        let v2 = make_vv(&[("a", 2), ("b", 4)]);
        assert!(v1.dominates(&v2));
        assert!(!v2.dominates(&v1));
    }

    #[test]
    fn vv_dominates_equal() {
        let v1 = make_vv(&[("a", 3)]);
        let v2 = make_vv(&[("a", 3)]);
        assert!(v1.dominates(&v2));
        assert!(v2.dominates(&v1));
    }

    #[test]
    fn vv_not_dominates_with_missing_key() {
        let v1 = make_vv(&[("a", 3)]);
        let v2 = make_vv(&[("a", 2), ("b", 1)]);
        // v1 has a=3 >= a=2 but b=0 < b=1
        assert!(!v1.dominates(&v2));
    }

    #[test]
    fn vv_agent_ids() {
        let v = make_vv(&[("a", 1), ("b", 2), ("c", 3)]);
        let mut ids = v.agent_ids();
        ids.sort();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    // ── Incremental merge ──

    fn make_learning_with_version(
        agent_id: &str,
        clock: u64,
        success: &[(&str, f64)],
        failed: &[&str],
        facts: &[&str],
        quality: f64,
    ) -> AgentLearning {
        let mut l = make_learning(agent_id, success, failed, facts, quality);
        l.version = make_vv(&[(agent_id, clock)]);
        l
    }

    #[test]
    fn incremental_from_empty() {
        let empty = MergedLearning {
            version: VersionVector::new(),
            consensus_patterns: vec![],
            cautionary_patterns: vec![],
            conflicted_patterns: vec![],
            facts: vec![],
            agent_count: 0,
            total_quality: 0.0,
        };
        let l = make_learning_with_version("a1", 1, &[("p", 0.9)], &[], &["fact-1"], 0.8);
        let result = merge_incremental(&empty, &[l]);
        assert_eq!(result.agent_count, 1);
        assert_eq!(result.consensus_patterns.len(), 1);
        assert_eq!(result.facts, vec!["fact-1"]);
    }

    #[test]
    fn incremental_skips_stale_agents() {
        // Existing has a1@clock=3
        let existing = MergedLearning {
            version: make_vv(&[("a1", 3)]),
            consensus_patterns: vec![],
            cautionary_patterns: vec![],
            conflicted_patterns: vec![],
            facts: vec!["old-fact".to_string()],
            agent_count: 1,
            total_quality: 0.5,
        };
        // New agent has clock=2 (stale, should be skipped)
        let stale = make_learning_with_version("a1", 2, &[("p", 0.8)], &[], &["new-fact"], 0.3);
        let result = merge_incremental(&existing, &[stale]);
        assert_eq!(result.agent_count, 1); // unchanged
        assert_eq!(result.facts, vec!["old-fact"]); // no new fact added
    }

    #[test]
    fn incremental_merges_new_agent() {
        let existing = MergedLearning {
            version: make_vv(&[("a1", 1)]),
            consensus_patterns: vec![LearningPattern {
                name: "existing-p".to_string(),
                frequency: 1,
                success_rate: 0.9,
                context: "existing".to_string(),
            }],
            cautionary_patterns: vec![],
            conflicted_patterns: vec![],
            facts: vec!["old-fact".to_string()],
            agent_count: 1,
            total_quality: 0.8,
        };
        let new_agent =
            make_learning_with_version("a2", 1, &[("new-p", 0.85)], &[], &["new-fact"], 0.7);
        let result = merge_incremental(&existing, &[new_agent]);
        assert_eq!(result.agent_count, 2);
        assert_eq!(result.version.get("a1"), 1);
        assert_eq!(result.version.get("a2"), 1);
        assert!(result.facts.contains(&"old-fact".to_string()));
        assert!(result.facts.contains(&"new-fact".to_string()));
        assert!((result.total_quality - 1.5).abs() < 0.001);
    }

    #[test]
    fn incremental_detects_new_conflicts() {
        let existing = MergedLearning {
            version: make_vv(&[("a1", 1)]),
            consensus_patterns: vec![LearningPattern {
                name: "controversial".to_string(),
                frequency: 1,
                success_rate: 0.9,
                context: "existing".to_string(),
            }],
            cautionary_patterns: vec![],
            conflicted_patterns: vec![],
            facts: vec![],
            agent_count: 1,
            total_quality: 0.8,
        };
        // New agent reports the same pattern as FAILED
        let new_agent = make_learning_with_version("a2", 1, &[], &["controversial"], &[], 0.7);
        let result = merge_incremental(&existing, &[new_agent]);
        assert!(
            result
                .conflicted_patterns
                .contains(&"controversial".to_string())
        );
    }

    // ── Serialization ──

    #[test]
    fn version_vector_serde_roundtrip() {
        let vv = make_vv(&[("agent-1", 5), ("agent-2", 3)]);
        let json = serde_json::to_string(&vv).unwrap();
        let parsed: VersionVector = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, vv);
    }

    #[test]
    fn merged_learning_serde_roundtrip() {
        let l1 = make_learning("a1", &[("p", 0.9)], &["bad"], &["fact"], 0.8);
        let l2 = make_learning("a2", &[("p", 0.8)], &["bad"], &[], 0.5);
        let merged = merge_agent_learnings(&[l1, l2]);
        let json = serde_json::to_string(&merged).unwrap();
        let parsed: MergedLearning = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.agent_count, 2);
        assert_eq!(
            parsed.consensus_patterns.len(),
            merged.consensus_patterns.len()
        );
        assert_eq!(parsed.conflicted_patterns, merged.conflicted_patterns);
        assert!((parsed.total_quality - merged.total_quality).abs() < 0.001);
    }
}

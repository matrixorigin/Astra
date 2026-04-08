//! Version-vector-based N-way learning merge for multi-agent teams.
//!
//! Each agent produces [`AgentLearning`] outcomes during execution. After a team
//! run completes, [`merge_agent_learnings`] aggregates them using:
//!
//! - **Version vectors** for causal ordering
//! - **Vote-based consensus** for pattern aggregation (majority wins)
//! - **Union semantics** for discovered facts
//! - **Caution propagation** — any agent flagging a pattern as failed → cautionary

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
        !self.happened_before(other)
            && !other.happened_before(self)
            && !self.vv_equal(other)
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
    /// Patterns endorsed by a majority of agents.
    pub consensus_patterns: Vec<LearningPattern>,
    /// Pattern names flagged as failed by any agent.
    pub cautionary_patterns: Vec<String>,
    /// De-duplicated facts discovered across all agents.
    pub facts: Vec<String>,
    /// Number of agents that contributed.
    pub agent_count: usize,
}

// ─── Merge Logic ────────────────────────────────────────────────────────────

/// Merge learning outcomes from N agents into a single aggregated result.
///
/// # Consensus rules
///
/// - A pattern is **consensus** if it appears in ≥ `ceil(N/2)` agents' successful patterns.
/// - A pattern is **cautionary** if *any* agent flagged it as failed.
/// - Facts are de-duplicated by exact string match (weighted by agent quality for ordering).
/// - The merged version vector is the element-wise max across all agents.
pub fn merge_agent_learnings(learnings: &[AgentLearning]) -> MergedLearning {
    if learnings.is_empty() {
        return MergedLearning {
            version: VersionVector::new(),
            consensus_patterns: vec![],
            cautionary_patterns: vec![],
            facts: vec![],
            agent_count: 0,
        };
    }

    let mut merged_version = VersionVector::new();
    let mut pattern_votes: HashMap<String, Vec<(f64, u32)>> = HashMap::new();
    let mut failed_set: HashSet<String> = HashSet::new();
    let mut fact_weights: HashMap<String, f64> = HashMap::new();

    for l in learnings {
        merged_version.merge(&l.version);

        for p in &l.successful_patterns {
            pattern_votes
                .entry(p.name.clone())
                .or_default()
                .push((p.success_rate, p.frequency));
        }

        for p in &l.failed_patterns {
            failed_set.insert(p.name.clone());
        }

        for fact in &l.discovered_facts {
            let weight = fact_weights.entry(fact.clone()).or_insert(0.0);
            *weight += l.quality_score;
        }
    }

    // Consensus threshold: ceil(N/2) — majority of agents must agree
    let threshold = ((learnings.len() as f64) / 2.0).ceil() as usize;

    let consensus_patterns: Vec<LearningPattern> = pattern_votes
        .iter()
        .filter(|(_, votes)| votes.len() >= threshold)
        .map(|(name, votes)| {
            let avg_rate = votes.iter().map(|(r, _)| r).sum::<f64>() / votes.len() as f64;
            let total_freq: u32 = votes.iter().map(|(_, f)| f).sum();
            LearningPattern {
                name: name.clone(),
                frequency: total_freq,
                success_rate: avg_rate,
                context: format!("consensus from {} of {} agents", votes.len(), learnings.len()),
            }
        })
        .collect();

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
        facts,
        agent_count: learnings.len(),
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
        let l1 = make_learning("a1", &[("pattern-a", 0.9), ("pattern-b", 0.5)], &[], &[], 0.8);
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
        assert!(result.cautionary_patterns.contains(&"bad-pattern".to_string()));
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
        assert!((p.success_rate - 0.7).abs() < 0.001); // avg of 0.8 and 0.6
        assert_eq!(p.frequency, 2); // 1 + 1
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
        let l = make_learning("a1", &[("p", 0.9)], &["bad"], &["fact"], 0.8);
        let merged = merge_agent_learnings(&[l]);
        let json = serde_json::to_string(&merged).unwrap();
        let parsed: MergedLearning = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.agent_count, 1);
        assert_eq!(parsed.consensus_patterns.len(), 1);
    }
}

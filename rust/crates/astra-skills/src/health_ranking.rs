//! Health-aware skill ranking.
//!
//! Combines per-skill quality (from [`crate::quality::SkillQualityTracker`]) and
//! per-tool health (reported by the runtime's `ToolHealthTracker`) into a
//! single boost factor consumers can apply when sorting or truncating a list
//! of skills for model consumption.
//!
//! The design is additive and non-invasive: callers feed in the skill
//! manifest's primary tool names (if any) plus the current quality/health
//! signals, and get back a numeric multiplier in `[0.0, 1.5]`. Values below
//! `1.0` indicate *penalize* (push down the list), above `1.0` indicate
//! *boost*. Purely multiplicative so callers can combine with other ranking
//! signals (source priority, manifest ranking_weight, etc.) without
//! branching.
//!
//! ## Scoring
//!
//! For each skill:
//! * Start from the quality boost (fallback `1.0`).
//! * For every tool declared in `primary_tools`, multiply by a per-tool
//!   health factor: recent failure rate `r` maps to factor `1.0 - r` clamped
//!   to `[0.2, 1.0]`. A fully healthy tool is a no-op; a perfectly broken
//!   tool caps the penalty at `0.2` so we never completely hide a skill
//!   (users may still want to try it and receive an error, rather than have
//!   the agent silently route around it).
//! * The total is clamped to `[0.2, 1.5]` to bound the influence of noisy
//!   samples and keep ranking stable across small changes.
//!
//! ## Deprioritization-list short-circuit
//!
//! If a tool appears in the explicit `deprioritized_tools` set (as reported
//! by `ObservabilityHub::low_confidence_tools`), the multiplier for that
//! tool is pinned to `0.3` regardless of the raw failure rate. This encodes
//! the cross-cutting signal already surfaced elsewhere in SelfModel text,
//! so ranking agrees with the self-awareness narrative.

use std::collections::HashSet;

use crate::quality::SkillQualityTracker;

/// Minimum boost a skill can receive even when its primary tools are broken.
///
/// Chosen at `0.2` rather than `0.0` so a broken-tool skill is still
/// *selectable* — the agent's recovery path may depend on attempting it and
/// observing the failure. Setting it to zero would hide error signals from
/// downstream learning.
pub const MIN_HEALTH_BOOST: f64 = 0.2;

/// Maximum boost cap. Keeps well-behaved skills from monopolizing the
/// ranking when their quality score is very high.
pub const MAX_HEALTH_BOOST: f64 = 1.5;

/// Penalty multiplier applied when a tool appears in the explicit
/// deprioritization set (see [`HealthRankingInputs::deprioritized_tools`]).
pub const DEPRIORITIZED_TOOL_FACTOR: f64 = 0.3;

/// Inputs for ranking.
///
/// Note: the failure-rate map is keyed by tool name. Callers reading from
/// `ToolHealthEntry` should pass `entry.failure_rate` directly.
#[derive(Debug, Clone)]
pub struct HealthRankingInputs<'a> {
    /// Per-tool failure rate in `[0.0, 1.0]`.
    pub tool_failure_rates: &'a std::collections::HashMap<String, f64>,
    /// Tools explicitly flagged as low-confidence by the observability hub.
    pub deprioritized_tools: &'a HashSet<String>,
    /// Optional skill-level quality tracker. If `None`, all skills start
    /// from boost `1.0` and are penalized only by tool health.
    pub skill_quality: Option<&'a SkillQualityTracker>,
}

/// Compute the ranking multiplier for a skill given its primary tool list.
///
/// `primary_tools` is the subset of tools the skill is expected to drive
/// (commonly a 1–3 element slice). Pass an empty slice if the skill has no
/// declared tools; in that case the quality boost is returned unchanged.
pub fn rank_multiplier(
    skill_name: &str,
    primary_tools: &[&str],
    inputs: &HealthRankingInputs<'_>,
) -> f64 {
    let mut score = inputs
        .skill_quality
        .map(|q| q.selection_boost(skill_name))
        .unwrap_or(1.0);

    for tool in primary_tools {
        let factor = if inputs.deprioritized_tools.contains(*tool) {
            DEPRIORITIZED_TOOL_FACTOR
        } else {
            let r = inputs
                .tool_failure_rates
                .get(*tool)
                .copied()
                .unwrap_or(0.0)
                .clamp(0.0, 1.0);
            (1.0 - r).max(MIN_HEALTH_BOOST)
        };
        score *= factor;
    }

    score.clamp(MIN_HEALTH_BOOST, MAX_HEALTH_BOOST)
}

/// Sort a slice of `(skill_name, primary_tools)` pairs in-place by descending
/// health-adjusted rank. Stable: skills with equal scores retain their
/// original order (often established by manifest/source priority).
///
/// Returns a `Vec<(skill_name, score)>` parallel to the reordered input,
/// useful for debugging / observability.
pub fn sort_by_health_rank<S: AsRef<str> + Clone>(
    skills: &mut Vec<(String, Vec<S>)>,
    inputs: &HealthRankingInputs<'_>,
) -> Vec<(String, f64)> {
    let scores: Vec<f64> = skills
        .iter()
        .map(|(name, tools)| {
            let refs: Vec<&str> = tools.iter().map(|t| t.as_ref()).collect();
            rank_multiplier(name, &refs, inputs)
        })
        .collect();

    let mut indices: Vec<usize> = (0..skills.len()).collect();
    indices.sort_by(|&a, &b| {
        scores[b]
            .partial_cmp(&scores[a])
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let reordered: Vec<(String, Vec<S>)> =
        indices.iter().map(|&i| skills[i].clone()).collect();
    let scores_reordered: Vec<(String, f64)> = indices
        .iter()
        .map(|&i| (skills[i].0.clone(), scores[i]))
        .collect();
    *skills = reordered;
    scores_reordered
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn rates(pairs: &[(&str, f64)]) -> HashMap<String, f64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn healthy_tool_gives_unchanged_quality_boost() {
        let r = rates(&[("read_file", 0.0)]);
        let dep = HashSet::new();
        let inputs = HealthRankingInputs {
            tool_failure_rates: &r,
            deprioritized_tools: &dep,
            skill_quality: None,
        };
        let m = rank_multiplier("explore", &["read_file"], &inputs);
        assert!((m - 1.0).abs() < 1e-9);
    }

    #[test]
    fn failing_tool_penalizes_multiplier() {
        let r = rates(&[("flaky", 0.8)]);
        let dep = HashSet::new();
        let inputs = HealthRankingInputs {
            tool_failure_rates: &r,
            deprioritized_tools: &dep,
            skill_quality: None,
        };
        let m = rank_multiplier("risky_skill", &["flaky"], &inputs);
        // 1.0 - 0.8 = 0.2, clamped >= MIN_HEALTH_BOOST (0.2)
        assert!((m - 0.2).abs() < 1e-9);
    }

    #[test]
    fn deprioritized_tool_pins_factor_regardless_of_rate() {
        let r = rates(&[("bash", 0.0)]); // looks healthy by rate
        let mut dep = HashSet::new();
        dep.insert("bash".to_string());
        let inputs = HealthRankingInputs {
            tool_failure_rates: &r,
            deprioritized_tools: &dep,
            skill_quality: None,
        };
        let m = rank_multiplier("shell_skill", &["bash"], &inputs);
        assert!((m - DEPRIORITIZED_TOOL_FACTOR).abs() < 1e-9);
    }

    #[test]
    fn no_tools_means_quality_only() {
        let r = rates(&[]);
        let dep = HashSet::new();
        let q = SkillQualityTracker::new();
        let inputs = HealthRankingInputs {
            tool_failure_rates: &r,
            deprioritized_tools: &dep,
            skill_quality: Some(&q),
        };
        let m = rank_multiplier("abstract_skill", &[], &inputs);
        assert!((m - 1.0).abs() < 1e-9);
    }

    #[test]
    fn multiple_tools_compound_penalties() {
        let r = rates(&[("a", 0.5), ("b", 0.4)]);
        let dep = HashSet::new();
        let inputs = HealthRankingInputs {
            tool_failure_rates: &r,
            deprioritized_tools: &dep,
            skill_quality: None,
        };
        let m = rank_multiplier("chain_skill", &["a", "b"], &inputs);
        // 0.5 * 0.6 = 0.3
        assert!((m - 0.3).abs() < 1e-9);
    }

    #[test]
    fn score_clamped_above_min() {
        let r = rates(&[("x", 1.0), ("y", 1.0), ("z", 1.0)]);
        let dep = HashSet::new();
        let inputs = HealthRankingInputs {
            tool_failure_rates: &r,
            deprioritized_tools: &dep,
            skill_quality: None,
        };
        let m = rank_multiplier("tripled_broken", &["x", "y", "z"], &inputs);
        assert!(m >= MIN_HEALTH_BOOST - 1e-9);
    }

    #[test]
    fn sort_stable_ordering_on_ties() {
        let mut skills: Vec<(String, Vec<String>)> = vec![
            ("first".into(), vec!["read_file".into()]),
            ("second".into(), vec!["read_file".into()]),
            ("third".into(), vec!["read_file".into()]),
        ];
        let r = rates(&[("read_file", 0.0)]);
        let dep = HashSet::new();
        let inputs = HealthRankingInputs {
            tool_failure_rates: &r,
            deprioritized_tools: &dep,
            skill_quality: None,
        };
        let scores = sort_by_health_rank(&mut skills, &inputs);
        assert_eq!(skills[0].0, "first");
        assert_eq!(skills[1].0, "second");
        assert_eq!(skills[2].0, "third");
        for (_, s) in &scores {
            assert!((s - 1.0).abs() < 1e-9);
        }
    }

    #[test]
    fn sort_prefers_healthy_over_broken() {
        let mut skills: Vec<(String, Vec<String>)> = vec![
            ("broken".into(), vec!["bash".into()]),
            ("healthy".into(), vec!["read_file".into()]),
        ];
        let r = rates(&[("bash", 0.9), ("read_file", 0.0)]);
        let dep = HashSet::new();
        let inputs = HealthRankingInputs {
            tool_failure_rates: &r,
            deprioritized_tools: &dep,
            skill_quality: None,
        };
        sort_by_health_rank(&mut skills, &inputs);
        assert_eq!(skills[0].0, "healthy");
        assert_eq!(skills[1].0, "broken");
    }
}

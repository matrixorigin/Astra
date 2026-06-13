//! First-class working memory for task continuity.
//!
//! This is the compact, prompt-facing state that must survive compaction and
//! checkpoint restore: decisions, blockers, and next action. Runtime can still
//! provide richer side-channel state, but the pipeline owns this distilled
//! representation so continuity retention is testable.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

/// Caps on how many decisions/blockers the prompt section will carry. The
/// defaults are sized against typical session shape: more than this and the
/// prompt drift starts to outweigh the continuity value. Callers can bump
/// them for long-running specialised agents via [`WorkingMemoryState::with_config`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkingMemoryConfig {
    pub decision_cap: usize,
    pub blocker_cap: usize,
}

impl Default for WorkingMemoryConfig {
    fn default() -> Self {
        Self {
            decision_cap: 8,
            blocker_cap: 6,
        }
    }
}

/// Session-scoped, prompt-facing working memory.
///
/// `decisions` and `blockers` are bounded ring-like queues: new entries
/// append at the back, oldest entries fall off the front when the cap is
/// reached. `VecDeque` keeps that eviction O(1) instead of the O(cap) shift
/// a `Vec`-backed `drain(0..1)` would incur.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkingMemoryState {
    config: WorkingMemoryConfig,
    decisions: VecDeque<String>,
    blockers: VecDeque<String>,
    next_action: Option<String>,
}

impl WorkingMemoryState {
    /// Construct with custom caps. Default construction (`Default::default()`)
    /// uses [`WorkingMemoryConfig::default`].
    #[must_use]
    pub fn with_config(config: WorkingMemoryConfig) -> Self {
        Self {
            config,
            ..Self::default()
        }
    }

    /// Caps currently in effect.
    #[must_use]
    pub fn config(&self) -> WorkingMemoryConfig {
        self.config
    }

    /// Add a stable decision. Duplicate entries are ignored.
    pub fn push_decision(&mut self, decision: impl Into<String>) {
        push_unique_capped(
            &mut self.decisions,
            decision.into(),
            self.config.decision_cap,
        );
    }

    /// Add a current blocker. Duplicate entries are ignored.
    pub fn push_blocker(&mut self, blocker: impl Into<String>) {
        push_unique_capped(&mut self.blockers, blocker.into(), self.config.blocker_cap);
    }

    /// Set the next action the assistant should resume with.
    pub fn set_next_action(&mut self, next_action: impl Into<String>) {
        let next_action = next_action.into();
        self.next_action = (!next_action.trim().is_empty()).then_some(next_action);
    }

    /// Clear the resume action once the work it described has settled.
    pub fn clear_next_action(&mut self) {
        self.next_action = None;
    }

    /// Clear transient blockers once a turn settles cleanly.
    pub fn clear_blockers(&mut self) {
        self.blockers.clear();
    }

    /// Whether the working memory would render an empty prompt section.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.decisions.is_empty() && self.blockers.is_empty() && self.next_action.is_none()
    }

    /// Render a concise, stable prompt section.
    #[must_use]
    pub fn render_prompt_section(&self) -> String {
        if self.is_empty() {
            return String::new();
        }

        let mut out = String::from("## Working Memory\n");
        if !self.decisions.is_empty() {
            out.push_str("Decisions:\n");
            for decision in &self.decisions {
                out.push_str("- ");
                out.push_str(decision);
                out.push('\n');
            }
        }
        if !self.blockers.is_empty() {
            out.push_str("Blockers:\n");
            for blocker in &self.blockers {
                out.push_str("- ");
                out.push_str(blocker);
                out.push('\n');
            }
        }
        if let Some(next) = &self.next_action {
            out.push_str("Next action: ");
            out.push_str(next);
            out.push('\n');
        }
        out.trim_end().to_string()
    }
}

/// Push `value` into `items`, trimming whitespace + deduping against existing
/// entries. When `cap` is reached the oldest entry is dropped via
/// `pop_front` — O(1) on `VecDeque`. Dedup is `iter().any()` which is O(cap);
/// callers keep `cap` small (default 8) so this stays constant-time in
/// practice.
fn push_unique_capped(items: &mut VecDeque<String>, value: String, cap: usize) {
    if cap == 0 {
        return;
    }
    let value = value.trim();
    if value.is_empty() || items.iter().any(|existing| existing == value) {
        return;
    }
    items.push_back(value.to_string());
    while items.len() > cap {
        items.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_empty_is_empty() {
        assert!(
            WorkingMemoryState::default()
                .render_prompt_section()
                .is_empty()
        );
    }

    #[test]
    fn render_decisions_blockers_next_action() {
        let mut wm = WorkingMemoryState::default();
        wm.push_decision("Keep core deterministic.");
        wm.push_blocker("Need restore coverage.");
        wm.set_next_action("Run focused tests.");

        let rendered = wm.render_prompt_section();
        assert!(rendered.contains("## Working Memory"));
        assert!(rendered.contains("- Keep core deterministic."));
        assert!(rendered.contains("- Need restore coverage."));
        assert!(rendered.contains("Next action: Run focused tests."));
    }

    #[test]
    fn default_config_matches_historical_caps() {
        // The 8 / 6 defaults are the sizes the prompt was tuned against.
        // If these change, revisit the prompt token budget tests too.
        let cfg = WorkingMemoryConfig::default();
        assert_eq!(cfg.decision_cap, 8);
        assert_eq!(cfg.blocker_cap, 6);
    }

    #[test]
    fn custom_config_governs_eviction() {
        let mut wm = WorkingMemoryState::with_config(WorkingMemoryConfig {
            decision_cap: 2,
            blocker_cap: 1,
        });
        wm.push_decision("first");
        wm.push_decision("second");
        wm.push_decision("third"); // evicts "first"
        let rendered = wm.render_prompt_section();
        assert!(
            !rendered.contains("first"),
            "oldest decision must be evicted"
        );
        assert!(rendered.contains("second"));
        assert!(rendered.contains("third"));
    }

    #[test]
    fn eviction_preserves_insertion_order_fifo() {
        // VecDeque-backed eviction: pop_front removes the oldest.
        // Test that remaining entries stay in insertion order, not reversed
        // or shuffled — rendered order is what the LLM reads.
        let mut wm = WorkingMemoryState::with_config(WorkingMemoryConfig {
            decision_cap: 3,
            blocker_cap: 6,
        });
        for label in ["A", "B", "C", "D", "E"] {
            wm.push_decision(label);
        }
        let rendered = wm.render_prompt_section();
        // Expect C, D, E in that order (A and B evicted).
        let c_pos = rendered.find("- C").expect("C present");
        let d_pos = rendered.find("- D").expect("D present");
        let e_pos = rendered.find("- E").expect("E present");
        assert!(c_pos < d_pos && d_pos < e_pos, "FIFO order after eviction");
        assert!(!rendered.contains("- A"));
        assert!(!rendered.contains("- B"));
    }

    #[test]
    fn dedup_skips_whitespace_only_and_duplicate_entries() {
        let mut wm = WorkingMemoryState::default();
        wm.push_decision("keep it simple");
        wm.push_decision("keep it simple"); // duplicate
        wm.push_decision("   "); // whitespace-only
        wm.push_decision(""); // empty
        let rendered = wm.render_prompt_section();
        assert_eq!(rendered.matches("keep it simple").count(), 1);
    }

    #[test]
    fn zero_cap_config_disables_accumulation() {
        // Defensive: a cap of 0 should drop silently rather than panic or
        // underflow the pop loop.
        let mut wm = WorkingMemoryState::with_config(WorkingMemoryConfig {
            decision_cap: 0,
            blocker_cap: 0,
        });
        wm.push_decision("ignored");
        wm.push_blocker("ignored");
        assert!(!wm.render_prompt_section().contains("ignored"));
    }

    #[test]
    fn explicit_clearers_remove_resume_and_blocker_pressure() {
        let mut wm = WorkingMemoryState::default();
        wm.push_decision("keep this durable decision");
        wm.push_blocker("temporary tool outage");
        wm.set_next_action("resume validation");

        wm.clear_next_action();
        wm.clear_blockers();

        let rendered = wm.render_prompt_section();
        assert!(rendered.contains("keep this durable decision"));
        assert!(!rendered.contains("temporary tool outage"));
        assert!(!rendered.contains("Next action:"));
    }

    #[test]
    fn legacy_vec_encoded_snapshot_deserializes_into_vecdeque() {
        // Earlier revisions stored `decisions` / `blockers` as `Vec<String>`
        // and emitted JSON arrays for them. `VecDeque<T>` serializes as the
        // same JSON array, so old persisted snapshots must round-trip
        // losslessly. This pins that forward-compat contract.
        let legacy = serde_json::json!({
            "decisions": ["one", "two", "three"],
            "blockers": ["beta"],
            "next_action": "resume"
        });
        let wm: WorkingMemoryState = serde_json::from_value(legacy).expect("legacy JSON");
        let rendered = wm.render_prompt_section();
        assert!(rendered.contains("- one"));
        assert!(rendered.contains("- two"));
        assert!(rendered.contains("- three"));
        assert!(rendered.contains("- beta"));
        assert!(rendered.contains("Next action: resume"));
    }
}

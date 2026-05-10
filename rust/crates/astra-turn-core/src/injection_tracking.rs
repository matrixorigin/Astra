//! Per-channel freshness tracking for runtime-injected prompt signals.
//!
//! Motivation (session f85a02bb diagnostic): runtime-injected meta like
//! `Recent test failures: ...`, tool-outcome bias, memoria lessons, and
//! volatile-lane entries were observed to persist unchanged for dozens
//! of rounds while they had long since become stale. Without a
//! freshness signal, the model keeps reading the same obsolete advice
//! on every round.
//!
//! This module provides pure types + analysis. The runtime observes
//! each channel's current content *once per round* into an
//! [`InjectionHistory`], and introspection can then render a freshness
//! report (see `introspect::render_injection_freshness`) so the agent
//! — and operators — can see which channels have gone stale.
//!
//! Design notes:
//! - Hash is content-derived (stable across rounds) so unchanged
//!   content deterministically appears as the same fingerprint.
//! - Preview is truncated (80 chars, collapsed whitespace) so the
//!   report stays compact even for large channel payloads.
//! - Ring is bounded per channel so long sessions don't grow memory
//!   unbounded; the latest and earliest-matching-current entries are
//!   always retained.
//! - "Stale" threshold is **advisory only** — this module never
//!   suppresses injections, only reports. Suppression / expiry rules
//!   live elsewhere (Tier 1).

use std::collections::VecDeque;
use std::hash::{Hash, Hasher};

use fnv::FnvHasher;
use serde::{Deserialize, Serialize};

/// Stable channel identifier. Add variants when new injection sources
/// are tracked — every new variant MUST also be wired into the runtime
/// observer and into `freshness_report` ordering.
///
/// Every variant corresponds to a `dynamic_sections.push(...)` call in
/// `runtime::turn::bridge_inprocess::build_turn_payload` (or the
/// equivalent CLI edge_profile write). Keep them in one-to-one
/// correspondence — if a new section is added to the prompt without a
/// matching variant here, `introspect subtopic=injection_freshness`
/// goes blind to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum InjectionChannel {
    /// `SelfModel.recent_failing_tests` rendered as "Recent test failures: ..."
    RecentFailingTests,
    /// `SelfModel.outcome_bias` rendered as "Tool outcome bias: bash ↑0.10 ..."
    OutcomeBias,
    /// `SelfModel.lessons` rendered as "📚 Lessons from prior sessions: ..."
    Lessons,
    /// `AgenticLoopState.volatile_pending` content joined into a single fingerprint.
    VolatilePending,
    /// `edge_profile.memoria_insights_text` — per-turn cross-session recall digest.
    MemoriaInsights,
    /// `memoria_prefetch_section` — pre-loaded Memoria memories on session start / compaction.
    MemoriaPrefetch,
    /// `edge_profile.self_awareness_text` — rendered `SelfModel::to_system_prompt_section()`.
    SelfAwareness,
    /// `feedback_store::build_injection_filtered` — learned correction rules.
    FeedbackRules,
    /// Per-turn `detect_implicit_feedback_signal` output — correction / frustration nudges.
    ImplicitFeedback,
    /// `edge_profile.recent_arg_hints_text` — recently-used paths / commands.
    RecentArgHints,
    /// `edge_profile.skill_listing_text` — available skills surface.
    SkillListing,
    /// `tool_round_guidance` — turn-budget batching / retry nudges.
    ToolRoundGuidance,
}

impl InjectionChannel {
    /// Short stable tag for rendering / serialization.
    ///
    /// **Stability contract**: these tags are persisted in
    /// `workspace.yaml::last_context_trace` and surfaced via the
    /// `introspect subtopic=injection_freshness` table. Renaming them
    /// breaks cross-process fingerprint history and any external
    /// dashboards that scrape introspect output.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::RecentFailingTests => "recent_failing_tests",
            Self::OutcomeBias => "outcome_bias",
            Self::Lessons => "lessons",
            Self::VolatilePending => "volatile_pending",
            Self::MemoriaInsights => "memoria_insights",
            Self::MemoriaPrefetch => "memoria_prefetch",
            Self::SelfAwareness => "self_awareness",
            Self::FeedbackRules => "feedback_rules",
            Self::ImplicitFeedback => "implicit_feedback",
            Self::RecentArgHints => "recent_arg_hints",
            Self::SkillListing => "skill_listing",
            Self::ToolRoundGuidance => "tool_round_guidance",
        }
    }

    /// Canonical ordering used when rendering the full report.
    ///
    /// Ordering policy: self-* signals first (failing tests, bias,
    /// lessons), then volatile/content-derived lanes (volatile pending,
    /// memoria insights, memoria prefetch), then behavioural/meta-state
    /// lanes (self-awareness, feedback rules, implicit feedback), then
    /// operational hints (recent-arg, skill listing, tool-round
    /// guidance). Introspect renders in this order for stable output.
    pub fn all() -> [Self; 12] {
        [
            Self::RecentFailingTests,
            Self::OutcomeBias,
            Self::Lessons,
            Self::VolatilePending,
            Self::MemoriaInsights,
            Self::MemoriaPrefetch,
            Self::SelfAwareness,
            Self::FeedbackRules,
            Self::ImplicitFeedback,
            Self::RecentArgHints,
            Self::SkillListing,
            Self::ToolRoundGuidance,
        ]
    }
}

/// Content fingerprint for a channel at a single round.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InjectionFingerprint {
    pub hash: u64,
    pub preview: String,
    /// True when the channel was empty at observation time. Empty
    /// channels still get observed so the history distinguishes
    /// "not rendered this round" from "channel missing from tracking".
    pub is_empty: bool,
}

impl InjectionFingerprint {
    /// Build a fingerprint from raw channel content. Empty or
    /// whitespace-only content is normalized to the "empty" sentinel so
    /// every empty observation has the same hash.
    pub fn from_content(content: &str) -> Self {
        let trimmed = content.trim();
        if trimmed.is_empty() {
            return Self {
                hash: 0,
                preview: String::new(),
                is_empty: true,
            };
        }
        // Use FNV (deterministic, cross-process stable) instead of
        // std::collections::hash_map::DefaultHasher — DefaultHasher's
        // SipHash seed varies per process, so an `InjectionHistory`
        // serialized in one process and deserialized in another would
        // report every channel as Fresh for one round because the
        // newly-computed fingerprint would not match the persisted one.
        let mut hasher = FnvHasher::default();
        trimmed.hash(&mut hasher);
        Self {
            hash: hasher.finish(),
            preview: preview(trimmed, 80),
            is_empty: false,
        }
    }
}

fn preview(text: &str, max_chars: usize) -> String {
    // Collapse runs of whitespace into a single space so multi-line
    // payloads render on one line in the report.
    let collapsed: String = {
        let mut out = String::with_capacity(text.len().min(max_chars * 2));
        let mut prev_ws = false;
        for ch in text.chars() {
            if ch.is_whitespace() {
                if !prev_ws {
                    out.push(' ');
                }
                prev_ws = true;
            } else {
                out.push(ch);
                prev_ws = false;
            }
        }
        out.trim().to_string()
    };
    if collapsed.chars().count() <= max_chars {
        return collapsed;
    }
    let mut out: String = collapsed.chars().take(max_chars).collect();
    out.push('…');
    out
}

/// Single history entry recording when a given fingerprint first
/// appeared on a channel and when it was most recently observed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InjectionHistoryEntry {
    pub channel: InjectionChannel,
    pub fingerprint: InjectionFingerprint,
    pub first_seen_round: u32,
    pub last_seen_round: u32,
}

/// Per-channel bounded ring of fingerprint runs. Each entry captures a
/// contiguous streak of rounds where the fingerprint was identical; a
/// content change starts a new entry.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InjectionHistory {
    /// Entries grouped per channel, most-recent-last. Bounded by
    /// [`Self::MAX_RUNS_PER_CHANNEL`] so long sessions stay O(1) memory.
    entries: VecDeque<InjectionHistoryEntry>,
}

impl InjectionHistory {
    /// Bound on how many fingerprint "runs" are retained per channel.
    /// A run is a contiguous streak of identical fingerprints — the
    /// current run is always preserved; older runs beyond this cap are
    /// dropped. Small by design: we only need enough history to answer
    /// "has this channel changed recently?".
    pub const MAX_RUNS_PER_CHANNEL: usize = 8;

    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `channel` currently has `fingerprint` in round
    /// `round`. Idempotent for the same fingerprint: only the
    /// `last_seen_round` is bumped. A different fingerprint opens a
    /// new run. Panic-free for any input.
    pub fn observe(
        &mut self,
        round: u32,
        channel: InjectionChannel,
        fingerprint: InjectionFingerprint,
    ) {
        // Find the most recent entry for this channel, if any.
        let last = self.entries.iter_mut().rev().find(|e| e.channel == channel);
        match last {
            Some(entry) if entry.fingerprint == fingerprint => {
                entry.last_seen_round = entry.last_seen_round.max(round);
                return;
            }
            _ => {}
        }
        self.entries.push_back(InjectionHistoryEntry {
            channel,
            fingerprint,
            first_seen_round: round,
            last_seen_round: round,
        });
        self.prune(channel);
    }

    /// Prune the oldest runs for a channel down to [`MAX_RUNS_PER_CHANNEL`].
    ///
    /// Invariant: when the newest fingerprint matches a run that would
    /// otherwise be pruned (repeated identical injection beyond the cap),
    /// the earliest-matching run must be retained so `first_seen_round`
    /// survives for the noise subtopic. See the inline comment in the
    /// body and the regression test
    /// `observe_single_fingerprint_beyond_cap_preserves_first_seen_round`
    /// which asserts `rounds_alive` still reflects round 0 after pushing
    /// `MAX_RUNS_PER_CHANNEL + 10` identical fingerprints.
    fn prune(&mut self, channel: InjectionChannel) {
        let count = self.entries.iter().filter(|e| e.channel == channel).count();
        if count <= Self::MAX_RUNS_PER_CHANNEL {
            return;
        }
        // Invariant: preserve both the earliest-matching-current entry
        // (so `first_seen_round` / `rounds_alive` stay honest) AND the
        // latest entry (so the current fingerprint is always recoverable).
        // Drop from the *middle* of the per-channel run, not the head.
        //
        // Without this, a channel that sits on a single fingerprint for
        // `MAX + k` rounds would lose its original `first_seen_round`
        // once pruning dropped the head, and `rounds_alive` would
        // silently reset — defeating the staleness detector.
        let latest_fp = self
            .entries
            .iter()
            .rev()
            .find(|e| e.channel == channel)
            .map(|e| e.fingerprint.clone());
        let earliest_matching_idx = latest_fp.as_ref().and_then(|fp| {
            self.entries
                .iter()
                .position(|e| e.channel == channel && &e.fingerprint == fp)
        });

        let mut to_drop = count - Self::MAX_RUNS_PER_CHANNEL;
        let mut i = 0;
        while i < self.entries.len() && to_drop > 0 {
            if self.entries[i].channel != channel {
                i += 1;
                continue;
            }
            // Never drop the earliest entry that still matches the
            // current fingerprint — it anchors `first_seen_round`.
            if Some(i) == earliest_matching_idx {
                i += 1;
                continue;
            }
            // Never drop the last remaining per-channel entry.
            let remaining = self.entries.iter().filter(|e| e.channel == channel).count();
            if remaining <= 1 {
                break;
            }
            self.entries.remove(i);
            to_drop -= 1;
            // Don't advance i — the next element shifted into this slot.
        }
    }

    /// Latest entry for `channel`, if any.
    pub fn latest(&self, channel: InjectionChannel) -> Option<&InjectionHistoryEntry> {
        self.entries.iter().rev().find(|e| e.channel == channel)
    }

    /// Total number of entries across all channels (for tests/diagnostics).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Rendered status of a channel at report time. `StaleRounds` count
/// measures how long the latest fingerprint has been unchanged,
/// computed as `current_round - first_seen_round`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChannelStatus {
    /// Channel has never been observed.
    Untracked,
    /// Channel was observed but was empty — no injection this round.
    Empty { first_seen_round: u32 },
    /// Fresh: fingerprint changed within the last `stale_threshold` rounds.
    Fresh { rounds_alive: u32 },
    /// Stale: fingerprint unchanged for at least `stale_threshold` rounds.
    Stale { rounds_alive: u32 },
}

/// Per-channel freshness entry for rendering / introspection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelFreshness {
    pub channel: InjectionChannel,
    pub status: ChannelStatus,
    pub preview: String,
    pub first_seen_round: Option<u32>,
}

/// Default threshold: a channel unchanged for ≥ 10 rounds is flagged stale.
/// 10 is ~3× a typical tool-loop length; below that the signal may
/// legitimately still be current.
pub const DEFAULT_STALE_THRESHOLD: u32 = 10;

/// Produce a per-channel freshness report at `current_round`.
/// Channels never observed appear as [`ChannelStatus::Untracked`].
/// The stale threshold is configurable but defaults to
/// [`DEFAULT_STALE_THRESHOLD`].
pub fn freshness_report(history: &InjectionHistory, current_round: u32) -> Vec<ChannelFreshness> {
    freshness_report_with_threshold(history, current_round, DEFAULT_STALE_THRESHOLD)
}

pub fn freshness_report_with_threshold(
    history: &InjectionHistory,
    current_round: u32,
    stale_threshold: u32,
) -> Vec<ChannelFreshness> {
    InjectionChannel::all()
        .iter()
        .map(|&ch| {
            let Some(entry) = history.latest(ch) else {
                return ChannelFreshness {
                    channel: ch,
                    status: ChannelStatus::Untracked,
                    preview: String::new(),
                    first_seen_round: None,
                };
            };
            let first = entry.first_seen_round;
            if entry.fingerprint.is_empty {
                return ChannelFreshness {
                    channel: ch,
                    status: ChannelStatus::Empty {
                        first_seen_round: first,
                    },
                    preview: String::new(),
                    first_seen_round: Some(first),
                };
            }
            let rounds_alive = current_round.saturating_sub(first);
            let status = if rounds_alive >= stale_threshold {
                ChannelStatus::Stale { rounds_alive }
            } else {
                ChannelStatus::Fresh { rounds_alive }
            };
            ChannelFreshness {
                channel: ch,
                status,
                preview: entry.fingerprint.preview.clone(),
                first_seen_round: Some(first),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_all_returns_stable_ordering_of_every_variant() {
        let all = InjectionChannel::all();
        assert_eq!(all.len(), 12);
        // Self-* ordering: failing tests → bias → lessons.
        assert_eq!(all[0], InjectionChannel::RecentFailingTests);
        assert_eq!(all[1], InjectionChannel::OutcomeBias);
        assert_eq!(all[2], InjectionChannel::Lessons);
        // Volatile + content-derived block.
        assert_eq!(all[3], InjectionChannel::VolatilePending);
        assert_eq!(all[4], InjectionChannel::MemoriaInsights);
        assert_eq!(all[5], InjectionChannel::MemoriaPrefetch);
        // Behavioural / meta-state block.
        assert_eq!(all[6], InjectionChannel::SelfAwareness);
        assert_eq!(all[7], InjectionChannel::FeedbackRules);
        assert_eq!(all[8], InjectionChannel::ImplicitFeedback);
        // Operational hints block.
        assert_eq!(all[9], InjectionChannel::RecentArgHints);
        assert_eq!(all[10], InjectionChannel::SkillListing);
        assert_eq!(all[11], InjectionChannel::ToolRoundGuidance);
    }

    #[test]
    fn fingerprint_same_content_yields_same_hash() {
        let a = InjectionFingerprint::from_content("Recent test failures: foo, bar");
        let b = InjectionFingerprint::from_content("Recent test failures: foo, bar");
        assert_eq!(a.hash, b.hash);
        assert_eq!(a.preview, b.preview);
        assert!(!a.is_empty);
    }

    #[test]
    fn fingerprint_different_content_yields_different_hash() {
        let a = InjectionFingerprint::from_content("Recent test failures: foo");
        let b = InjectionFingerprint::from_content("Recent test failures: foo, bar");
        assert_ne!(a.hash, b.hash);
    }

    #[test]
    fn fingerprint_empty_content_flags_is_empty() {
        let fp = InjectionFingerprint::from_content("");
        assert!(fp.is_empty);
        assert_eq!(fp.hash, 0);
        assert!(fp.preview.is_empty());

        let ws = InjectionFingerprint::from_content("   \n\t  ");
        assert!(ws.is_empty, "whitespace-only collapses to empty");
    }

    #[test]
    fn fingerprint_preview_truncates_to_80_chars_with_ellipsis() {
        let long_content = "x".repeat(200);
        let fp = InjectionFingerprint::from_content(&long_content);
        let char_count = fp.preview.chars().count();
        assert!(
            char_count == 81,
            "80 chars + ellipsis; got {char_count}: {:?}",
            fp.preview
        );
        assert!(fp.preview.ends_with('…'));
    }

    #[test]
    fn fingerprint_preview_collapses_whitespace_to_single_line() {
        let multiline = "line1\nline2\n\n  line3";
        let fp = InjectionFingerprint::from_content(multiline);
        assert_eq!(fp.preview, "line1 line2 line3");
        assert!(!fp.preview.contains('\n'));
    }

    #[test]
    fn observe_same_fingerprint_across_rounds_bumps_last_seen_only() {
        let mut h = InjectionHistory::new();
        let fp = InjectionFingerprint::from_content("tests failing: foo");
        for round in 0..15 {
            h.observe(round, InjectionChannel::RecentFailingTests, fp.clone());
        }
        let latest = h.latest(InjectionChannel::RecentFailingTests).unwrap();
        assert_eq!(latest.first_seen_round, 0);
        assert_eq!(latest.last_seen_round, 14);
        assert_eq!(
            h.len(),
            1,
            "identical fingerprint across rounds = 1 run, not 15"
        );
    }

    #[test]
    fn observe_fingerprint_change_opens_new_run() {
        let mut h = InjectionHistory::new();
        let fp_a = InjectionFingerprint::from_content("foo");
        let fp_b = InjectionFingerprint::from_content("bar");
        h.observe(0, InjectionChannel::Lessons, fp_a.clone());
        h.observe(1, InjectionChannel::Lessons, fp_a.clone());
        h.observe(2, InjectionChannel::Lessons, fp_b.clone());
        let latest = h.latest(InjectionChannel::Lessons).unwrap();
        assert_eq!(latest.fingerprint, fp_b);
        assert_eq!(latest.first_seen_round, 2);
        assert_eq!(latest.last_seen_round, 2);
    }

    #[test]
    fn observe_different_channels_are_independent() {
        let mut h = InjectionHistory::new();
        let fp = InjectionFingerprint::from_content("x");
        h.observe(0, InjectionChannel::Lessons, fp.clone());
        h.observe(0, InjectionChannel::OutcomeBias, fp.clone());
        assert_eq!(h.len(), 2);
        assert!(h.latest(InjectionChannel::Lessons).is_some());
        assert!(h.latest(InjectionChannel::OutcomeBias).is_some());
    }

    #[test]
    fn observe_prunes_oldest_runs_beyond_cap_per_channel() {
        let mut h = InjectionHistory::new();
        // Create MAX+3 distinct runs on one channel — each round a
        // different fingerprint so each starts a new run.
        for i in 0..(InjectionHistory::MAX_RUNS_PER_CHANNEL as u32 + 3) {
            let fp = InjectionFingerprint::from_content(&format!("v{i}"));
            h.observe(i, InjectionChannel::Lessons, fp);
        }
        let count = (0..h.len())
            .filter(|&i| h.entries.get(i).map(|e| e.channel) == Some(InjectionChannel::Lessons))
            .count();
        assert_eq!(
            count,
            InjectionHistory::MAX_RUNS_PER_CHANNEL,
            "run count bounded"
        );
        let latest = h.latest(InjectionChannel::Lessons).unwrap();
        assert_eq!(
            latest.fingerprint.preview,
            InjectionFingerprint::from_content(&format!(
                "v{}",
                InjectionHistory::MAX_RUNS_PER_CHANNEL as u32 + 2
            ))
            .preview,
            "newest run retained"
        );
    }

    #[test]
    fn observe_single_fingerprint_beyond_cap_preserves_first_seen_round() {
        // Invariant: when a channel sits on ONE fingerprint for
        // MAX+k rounds, pruning must NOT drop the earliest-matching
        // entry — otherwise `first_seen_round` silently resets and
        // `rounds_alive` lies about staleness. Regression guard for
        // review finding (injection_tracking.rs MAX_RUNS_PER_CHANNEL).
        let mut h = InjectionHistory::new();
        let fp = InjectionFingerprint::from_content("sticky");
        let total = InjectionHistory::MAX_RUNS_PER_CHANNEL as u32 + 10;
        for i in 0..total {
            h.observe(i, InjectionChannel::Lessons, fp.clone());
        }
        // Same fingerprint across rounds = 1 run; first_seen_round
        // must still be 0, last_seen_round = total-1.
        let latest = h.latest(InjectionChannel::Lessons).unwrap();
        assert_eq!(
            latest.first_seen_round, 0,
            "first_seen_round must survive pruning when fingerprint is unchanged"
        );
        assert_eq!(latest.last_seen_round, total - 1);

        // And freshness_report must report the full rounds_alive.
        let report = freshness_report(&h, total);
        let lessons = report
            .iter()
            .find(|e| e.channel == InjectionChannel::Lessons)
            .unwrap();
        match &lessons.status {
            ChannelStatus::Stale { rounds_alive } | ChannelStatus::Fresh { rounds_alive } => {
                assert_eq!(
                    *rounds_alive, total,
                    "rounds_alive must reflect the original first_seen_round (0), not a pruned one"
                );
            }
            other => panic!("expected Fresh/Stale with rounds_alive, got {other:?}"),
        }
    }

    #[test]
    fn freshness_report_channel_never_observed_is_untracked() {
        let h = InjectionHistory::new();
        let report = freshness_report(&h, 10);
        assert_eq!(report.len(), InjectionChannel::all().len());
        for entry in &report {
            assert!(matches!(entry.status, ChannelStatus::Untracked));
            assert!(entry.preview.is_empty());
        }
    }

    #[test]
    fn freshness_report_recent_change_reports_fresh() {
        let mut h = InjectionHistory::new();
        let fp = InjectionFingerprint::from_content("x");
        h.observe(8, InjectionChannel::OutcomeBias, fp);
        let report = freshness_report(&h, 10);
        let bias = report
            .iter()
            .find(|r| r.channel == InjectionChannel::OutcomeBias)
            .unwrap();
        assert!(
            matches!(bias.status, ChannelStatus::Fresh { rounds_alive: 2 }),
            "got: {:?}",
            bias.status
        );
        assert_eq!(bias.first_seen_round, Some(8));
    }

    #[test]
    fn freshness_report_long_unchanged_run_reports_stale() {
        let mut h = InjectionHistory::new();
        let fp = InjectionFingerprint::from_content("could not find Cargo.toml");
        // Observed on round 0, still unchanged at round 58 (the f85a02bb case)
        for round in 0..=58 {
            h.observe(round, InjectionChannel::RecentFailingTests, fp.clone());
        }
        let report = freshness_report(&h, 58);
        let failing = report
            .iter()
            .find(|r| r.channel == InjectionChannel::RecentFailingTests)
            .unwrap();
        match &failing.status {
            ChannelStatus::Stale { rounds_alive } => assert_eq!(*rounds_alive, 58),
            other => panic!("expected Stale(58), got: {other:?}"),
        }
        assert!(failing.preview.contains("Cargo.toml"));
    }

    #[test]
    fn freshness_report_stale_threshold_boundary() {
        let mut h = InjectionHistory::new();
        let fp = InjectionFingerprint::from_content("y");
        h.observe(0, InjectionChannel::Lessons, fp);
        // threshold = 10; at round 9 → Fresh, at round 10 → Stale.
        let at9 = freshness_report(&h, 9);
        let at10 = freshness_report(&h, 10);
        let entry9 = at9
            .iter()
            .find(|r| r.channel == InjectionChannel::Lessons)
            .unwrap();
        let entry10 = at10
            .iter()
            .find(|r| r.channel == InjectionChannel::Lessons)
            .unwrap();
        assert!(matches!(entry9.status, ChannelStatus::Fresh { .. }));
        assert!(matches!(entry10.status, ChannelStatus::Stale { .. }));
    }

    #[test]
    fn freshness_report_empty_observation_reports_empty_status() {
        let mut h = InjectionHistory::new();
        let empty_fp = InjectionFingerprint::from_content("");
        h.observe(5, InjectionChannel::VolatilePending, empty_fp);
        let report = freshness_report(&h, 7);
        let volatile = report
            .iter()
            .find(|r| r.channel == InjectionChannel::VolatilePending)
            .unwrap();
        assert!(matches!(
            volatile.status,
            ChannelStatus::Empty {
                first_seen_round: 5
            }
        ));
        assert!(volatile.preview.is_empty());
    }

    #[test]
    fn freshness_report_picks_up_content_change_clearing_stale() {
        let mut h = InjectionHistory::new();
        let fp_a = InjectionFingerprint::from_content("old failure");
        for round in 0..20 {
            h.observe(round, InjectionChannel::RecentFailingTests, fp_a.clone());
        }
        // Verify stale at round 20
        let before = freshness_report(&h, 20);
        assert!(matches!(
            before
                .iter()
                .find(|r| r.channel == InjectionChannel::RecentFailingTests)
                .unwrap()
                .status,
            ChannelStatus::Stale { .. }
        ));
        // Content changes at round 21 → fresh again
        let fp_b = InjectionFingerprint::from_content("new different failure");
        h.observe(21, InjectionChannel::RecentFailingTests, fp_b);
        let after = freshness_report(&h, 21);
        let entry = after
            .iter()
            .find(|r| r.channel == InjectionChannel::RecentFailingTests)
            .unwrap();
        assert!(
            matches!(entry.status, ChannelStatus::Fresh { rounds_alive: 0 }),
            "content change resets staleness; got: {:?}",
            entry.status
        );
        assert!(entry.preview.contains("new different"));
    }

    #[test]
    fn freshness_report_channel_order_matches_canonical() {
        let mut h = InjectionHistory::new();
        let fp = InjectionFingerprint::from_content("z");
        for ch in InjectionChannel::all() {
            h.observe(1, ch, fp.clone());
        }
        let report = freshness_report(&h, 2);
        let channels: Vec<InjectionChannel> = report.iter().map(|r| r.channel).collect();
        assert_eq!(channels, InjectionChannel::all().to_vec());
    }
}

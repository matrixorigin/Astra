//! wip-4 TDD contract: `InjectionChannel` covers every live injection
//! channel, not just the 4 legacy ones. Motivated by the session
//! 895536bf diagnostic where Memoria insights, self-awareness, feedback
//! rules, recent-arg hints, skill listing, memoria-prefetch, implicit
//! feedback, and tool-round guidance all flow into the system prompt
//! every turn but have no freshness / staleness tracking —
//! `introspect subtopic=injection_freshness` silently omits them.
//!
//! After wip-4, every live injection path must correspond to a variant
//! in `InjectionChannel::all()` so the observer, freshness report, and
//! introspect surface all see it.

use astra_turn_core::injection_tracking::{
    InjectionChannel, InjectionFingerprint, InjectionHistory,
};

/// Every channel added in wip-4. When a new injection source is added
/// to `bridge_inprocess::dynamic_sections`, this list must grow.
const EXPECTED_CHANNELS: &[InjectionChannel] = &[
    // Pre-wip-4 (kept):
    InjectionChannel::RecentFailingTests,
    InjectionChannel::OutcomeBias,
    InjectionChannel::Lessons,
    InjectionChannel::VolatilePending,
    // wip-4 additions — covering all live bridge_inprocess injections:
    InjectionChannel::MemoriaInsights,
    InjectionChannel::MemoriaPrefetch,
    InjectionChannel::SelfAwareness,
    InjectionChannel::FeedbackRules,
    InjectionChannel::ImplicitFeedback,
    InjectionChannel::RecentArgHints,
    InjectionChannel::SkillListing,
    InjectionChannel::ToolRoundGuidance,
];

#[test]
fn all_live_channels_are_enumerated() {
    let actual = InjectionChannel::all();
    assert_eq!(
        actual.len(),
        EXPECTED_CHANNELS.len(),
        "InjectionChannel::all() must enumerate every live channel. \
         Expected {} variants, got {}. \
         Missing coverage creates silent blind spots in introspect subtopic=injection_freshness.",
        EXPECTED_CHANNELS.len(),
        actual.len()
    );
    for expected in EXPECTED_CHANNELS {
        assert!(
            actual.contains(expected),
            "InjectionChannel::all() must include {expected:?}"
        );
    }
}

#[test]
fn each_channel_has_stable_tag() {
    // Tags are written into workspace.yaml and introspect output;
    // renames break cross-process history and external observers.
    let pairs: &[(InjectionChannel, &str)] = &[
        (InjectionChannel::RecentFailingTests, "recent_failing_tests"),
        (InjectionChannel::OutcomeBias, "outcome_bias"),
        (InjectionChannel::Lessons, "lessons"),
        (InjectionChannel::VolatilePending, "volatile_pending"),
        (InjectionChannel::MemoriaInsights, "memoria_insights"),
        (InjectionChannel::MemoriaPrefetch, "memoria_prefetch"),
        (InjectionChannel::SelfAwareness, "self_awareness"),
        (InjectionChannel::FeedbackRules, "feedback_rules"),
        (InjectionChannel::ImplicitFeedback, "implicit_feedback"),
        (InjectionChannel::RecentArgHints, "recent_arg_hints"),
        (InjectionChannel::SkillListing, "skill_listing"),
        (InjectionChannel::ToolRoundGuidance, "tool_round_guidance"),
    ];
    for (channel, expected_tag) in pairs {
        assert_eq!(channel.tag(), *expected_tag, "tag mismatch for {channel:?}");
    }
}

#[test]
fn observer_can_ingest_every_channel_independently() {
    // Each variant must be routable through `observe` without the
    // history silently de-duplicating across variants. This guards
    // against two variants accidentally sharing the same Hash impl.
    let mut history = InjectionHistory::new();
    for (round, channel) in InjectionChannel::all().into_iter().enumerate() {
        let fp = InjectionFingerprint::from_content(&format!("content-{}", channel.tag()));
        history.observe(round as u32, channel, fp);
    }
    // Every channel must have a latest entry of its own.
    for channel in InjectionChannel::all() {
        let latest = history.latest(channel);
        assert!(
            latest.is_some(),
            "channel {channel:?} has no history entry — observer lost it"
        );
        let entry = latest.unwrap();
        assert_eq!(entry.channel, channel);
        assert!(
            entry.fingerprint.preview.contains(channel.tag()),
            "preview lost the tag anchor for {channel:?}: {:?}",
            entry.fingerprint.preview
        );
    }
}

#[test]
fn tags_are_unique() {
    use std::collections::HashSet;
    let tags: HashSet<&str> = InjectionChannel::all().iter().map(|c| c.tag()).collect();
    assert_eq!(
        tags.len(),
        InjectionChannel::all().len(),
        "duplicate tags across variants would make introspect output ambiguous"
    );
}

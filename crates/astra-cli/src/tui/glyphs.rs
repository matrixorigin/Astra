//! Terminal-safe glyph profile for high-frequency workbench state.
//!
//! Unicode is the default because it makes dense run state easier to scan.
//! A user on a constrained terminal can select ASCII without giving up any
//! state distinction: labels and semantic colours remain the source of truth.

use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GlyphProfile {
    Unicode,
    Ascii,
}

impl GlyphProfile {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "unicode" | "auto" => Some(Self::Unicode),
            "ascii" => Some(Self::Ascii),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Glyphs {
    pub agent_fanout: &'static str,
    pub agent_unconfirmed: &'static str,
    pub agent_stale: &'static str,
    pub agent_running: &'static str,
    pub agent_waiting: &'static str,
    pub agent_cancelling: &'static str,
    pub agent_completed: &'static str,
    pub agent_interrupted: &'static str,
    pub agent_failed: &'static str,
    pub agent_cancelled: &'static str,
    pub lineage: &'static str,
    pub detail_branch: &'static str,
    pub detail_last: &'static str,
    pub activity_frames: [&'static str; 4],
}

const UNICODE: Glyphs = Glyphs {
    agent_fanout: "▶",
    agent_unconfirmed: "?",
    agent_stale: "≈",
    agent_running: "◦",
    agent_waiting: "…",
    agent_cancelling: "⊘",
    agent_completed: "✓",
    agent_interrupted: "Ⅱ",
    agent_failed: "✗",
    agent_cancelled: "■",
    lineage: "↳",
    detail_branch: "├─",
    detail_last: "╰─",
    activity_frames: ["·", "•", "●", "•"],
};

const ASCII: Glyphs = Glyphs {
    agent_fanout: ">",
    agent_unconfirmed: "?",
    agent_stale: "~",
    agent_running: "o",
    agent_waiting: ".",
    agent_cancelling: "x",
    agent_completed: "+",
    agent_interrupted: "!",
    agent_failed: "x",
    agent_cancelled: "-",
    lineage: "->",
    detail_branch: "+-",
    detail_last: "`-",
    activity_frames: [".", "o", "O", "o"],
};

pub(crate) fn for_profile(profile: GlyphProfile) -> &'static Glyphs {
    match profile {
        GlyphProfile::Unicode => &UNICODE,
        GlyphProfile::Ascii => &ASCII,
    }
}

static ACTIVE: OnceLock<GlyphProfile> = OnceLock::new();

pub(crate) fn current() -> &'static Glyphs {
    let profile = *ACTIVE.get_or_init(|| {
        std::env::var("ASTRA_TUI_GLYPHS")
            .ok()
            .as_deref()
            .and_then(GlyphProfile::parse)
            .unwrap_or(GlyphProfile::Unicode)
    });
    for_profile(profile)
}

#[cfg(test)]
mod tests {
    use super::{GlyphProfile, for_profile};

    #[test]
    fn ascii_profile_preserves_distinct_terminal_agent_states() {
        let glyphs = for_profile(GlyphProfile::Ascii);
        assert_ne!(glyphs.agent_running, glyphs.agent_completed);
        assert_ne!(glyphs.agent_completed, glyphs.agent_failed);
        assert_ne!(glyphs.agent_waiting, glyphs.agent_cancelled);
        assert!(glyphs.activity_frames.iter().all(|frame| frame.is_ascii()));
    }

    #[test]
    fn unicode_profile_retains_compact_visual_hierarchy() {
        let glyphs = for_profile(GlyphProfile::Unicode);
        assert_eq!(glyphs.agent_fanout, "▶");
        assert_eq!(glyphs.detail_branch, "├─");
    }
}

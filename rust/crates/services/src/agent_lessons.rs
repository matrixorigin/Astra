//! Agent lesson types — cross-session memory of what worked and what didn't.
//!
//! Types used by the lesson extraction, synthesis, and prompt rendering
//! pipeline. The DAO (database persistence) has been replaced by Memoria
//! as the single source of truth (Session Memory Protocol L3).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ── Types ───────────────────────────────────────────────────────────────────

/// Classifier for what a lesson is teaching the agent to do next time.
/// Stable string tags (snake_case) so DB rows and JSON are self-describing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LessonKind {
    /// Avoid this tool for this scope — it failed or was slow last time.
    ToolDeprioritize,
    /// Prefer this tool for this scope — it worked well last time.
    ToolBoost,
    /// The system prompt / context shape that led to success.
    PromptShape,
    /// A postcondition pattern that kept failing — restructure the plan.
    PostconditionPattern,
    /// A recovery recipe for a specific error signature.
    ErrorRecovery,
    /// A positive pattern learned from successful outcomes — the agent
    /// discovered something that works well for this user/project.
    SkillAcquired,
}

impl std::fmt::Display for LessonKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl LessonKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ToolDeprioritize => "tool_deprioritize",
            Self::ToolBoost => "tool_boost",
            Self::PromptShape => "prompt_shape",
            Self::PostconditionPattern => "postcondition_pattern",
            Self::ErrorRecovery => "error_recovery",
            Self::SkillAcquired => "skill_acquired",
        }
    }

    pub fn parse_tag(tag: &str) -> Option<Self> {
        match tag {
            "tool_deprioritize" => Some(Self::ToolDeprioritize),
            "tool_boost" => Some(Self::ToolBoost),
            "prompt_shape" => Some(Self::PromptShape),
            "postcondition_pattern" => Some(Self::PostconditionPattern),
            "error_recovery" => Some(Self::ErrorRecovery),
            "skill_acquired" => Some(Self::SkillAcquired),
            _ => None,
        }
    }
}

/// A persisted lesson.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lesson {
    pub id: String,
    pub user_id: String,
    pub persona: String,
    pub workload_tag: Option<String>,
    pub kind: LessonKind,
    /// Short human-readable description of what triggered this lesson
    /// (e.g. `"3 consecutive ToolMisuse on grep"`). ≤255 chars.
    pub trigger_signal: String,
    /// Short imperative of what to do next time
    /// (e.g. `"deprioritize grep for regex-heavy tasks"`). ≤1024 chars.
    pub action: String,
    pub confidence: f64,
    pub hit_count: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Prompt-bound projection of a persisted [`Lesson`].
///
/// Intentionally drops `id`, `confidence`, `hit_count`, and timestamps —
/// the LLM should read the *advice*, not the metadata. Callers that need
/// to track adoption (for `record_hit`) keep the `id` out-of-band.
///
/// Canonical home: this crate (next to [`Lesson`]). Runtime re-exports it
/// for backwards-compat so existing code that imports from `self_model`
/// continues to compile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LessonHint {
    pub kind: LessonKind,
    pub trigger_signal: String,
    /// Full action text — used when prompt space permits.
    pub action: String,
    /// Short summary (~15 tokens) for compact rendering under prompt
    /// pressure. Inspired by Memoria V2's abstract/overview/detail model.
    /// When `None`, the renderer falls back to `action`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload_tag: Option<String>,
}

impl LessonHint {
    #[must_use]
    pub fn from_lesson(l: &Lesson) -> Self {
        let action = sanitize_for_prompt(&l.action);
        let compact = make_compact(&action);
        Self {
            kind: l.kind,
            trigger_signal: sanitize_for_prompt(&l.trigger_signal),
            action,
            compact,
            workload_tag: l.workload_tag.clone(),
        }
    }
}

/// Generate a compact summary (~60 chars) from a full action string.
/// Returns `None` if the action is already short enough.
fn make_compact(action: &str) -> Option<String> {
    if action.len() <= 80 {
        return None;
    }
    let first_sentence = action
        .split_once(['.', '—', ';', '\n'])
        .map(|(s, _)| s.trim())
        .unwrap_or(action);
    if first_sentence.len() >= action.len() - 5 {
        return None;
    }
    Some(first_sentence.to_string())
}

/// Strip control characters, zero-width Unicode, and bidirectional
/// overrides from content before prompt injection. Covers:
/// - C0/C1 control codes (is_control) except newline
/// - Zero-width spaces/joiners (U+200B–U+200F)
/// - Bidi overrides and isolates (U+2028–U+202F)
/// - Word joiners and invisible separators (U+2060–U+2064)
/// - BOM (U+FEFF)
///
/// Public so `SkillDiagnosis::render_prompt_block` can reuse it for
/// LLM-generated findings/headlines.
pub fn sanitize_for_prompt(s: &str) -> String {
    s.chars()
        .filter(|c| {
            if c.is_control() && *c != '\n' {
                return false;
            }
            !is_invisible_unicode(*c)
        })
        .collect()
}

/// Comprehensive invisible/deceptive Unicode character filter.
fn is_invisible_unicode(c: char) -> bool {
    matches!(
        c,
        // Zero-width spaces and joiners
        '\u{200B}'..='\u{200F}'
        // Line/paragraph separators + bidi overrides/isolates
        | '\u{2028}'..='\u{202F}'
        // Word joiners and invisible operators
        | '\u{2060}'..='\u{2064}'
        // BOM
        | '\u{FEFF}'
        // Soft hyphen (renders as nothing unless line break)
        | '\u{00AD}'
        // Combining grapheme joiner
        | '\u{034F}'
        // Arabic letter mark
        | '\u{061C}'
        // Hangul fillers
        | '\u{115F}' | '\u{1160}' | '\u{3164}' | '\u{FFA0}'
        // Khmer vowel inherent
        | '\u{17B4}' | '\u{17B5}'
        // Mongolian vowel separator
        | '\u{180E}'
        // Tag characters (U+E0001–U+E007F)
        | '\u{E0001}'..='\u{E007F}'
    )
}

/// Payload for `record`. Id / timestamps are assigned by the DAO.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewLesson {
    pub user_id: String,
    pub persona: String,
    pub workload_tag: Option<String>,
    pub kind: LessonKind,
    pub trigger_signal: String,
    pub action: String,
    pub confidence: Option<f64>,
}

pub const MAX_TRIGGER_SIGNAL_LEN: usize = 255;
pub const MAX_ACTION_LEN: usize = 1024;

impl NewLesson {
    /// Reject payloads that would violate the schema or carry nonsensical
    /// values. Validation runs before any SQL is emitted.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.user_id.is_empty() {
            return Err("user_id must not be empty");
        }
        if self.persona.is_empty() {
            return Err("persona must not be empty");
        }
        if self.trigger_signal.is_empty() {
            return Err("trigger_signal must not be empty");
        }
        if self.trigger_signal.len() > MAX_TRIGGER_SIGNAL_LEN {
            return Err("trigger_signal exceeds MAX_TRIGGER_SIGNAL_LEN");
        }
        if self.action.is_empty() {
            return Err("action must not be empty");
        }
        if self.action.len() > MAX_ACTION_LEN {
            return Err("action exceeds MAX_ACTION_LEN");
        }
        if let Some(c) = self.confidence
            && (!(0.0..=1.0).contains(&c) || c.is_nan())
        {
            return Err("confidence must be in [0.0, 1.0]");
        }
        Ok(())
    }
}

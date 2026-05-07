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

    /// True when this lesson carries enough signal to be worth injecting
    /// into the system prompt.
    ///
    /// Filters out low-quality memories that leak through Memoria's
    /// retrieval (e.g. scratchpad entries like `"test"`, single-word
    /// dumps, bare punctuation). These often end up tagged as
    /// `semantic`/`procedural` by the LLM during memory storage but
    /// carry no reusable advice.
    ///
    /// Rules (all must hold):
    /// - `action` has ≥ [`MIN_LESSON_ACTION_CHARS`] non-whitespace chars
    ///   after sanitize.
    /// - `action` contains ≥ [`MIN_LESSON_ACTION_WORDS`] distinct tokens
    ///   (letters, digits, CJK characters). This rejects `"test"`,
    ///   `"ok"`, `"..."` while keeping real single-sentence guidance.
    /// - `action` isn't a well-known scratchpad phrase
    ///   ([`SCRATCHPAD_LOWERCASE_PHRASES`]).
    ///
    /// Callers that load lessons from Memoria or other untrusted stores
    /// should apply this before rendering.
    #[must_use]
    pub fn is_prompt_worthy(&self) -> bool {
        is_action_prompt_worthy(&self.action)
    }
}

/// Minimum non-whitespace character count for a lesson action to be
/// injection-worthy. Chosen to exclude single words like `"test"` /
/// `"ok"` / `"done"` while permitting terse CJK advice.
pub const MIN_LESSON_ACTION_CHARS: usize = 12;

/// Minimum distinct word count (Unicode word-ish tokens). `"test"` → 1,
/// `"run cargo test"` → 3.
pub const MIN_LESSON_ACTION_WORDS: usize = 3;

/// Lowercase phrases that sometimes surface from scratchpad memory
/// storage but never carry reusable advice. Matched case-insensitively
/// after whitespace collapse.
pub const SCRATCHPAD_LOWERCASE_PHRASES: &[&str] = &[
    "test",
    "testing",
    "ok",
    "okay",
    "done",
    "todo",
    "fixme",
    "memoria test",
    "memoria — test",
    "memoria - test",
    "lorem ipsum",
    "hello world",
    "asdf",
    "foo bar",
    "placeholder",
];

/// True when `action` passes the quality gate described on
/// [`LessonHint::is_prompt_worthy`].
pub fn is_action_prompt_worthy(action: &str) -> bool {
    let collapsed = collapse_whitespace_lower(action);
    if collapsed.is_empty() {
        return false;
    }
    if SCRATCHPAD_LOWERCASE_PHRASES
        .iter()
        .any(|p| collapsed == *p)
    {
        return false;
    }
    let non_ws_chars = action.chars().filter(|c| !c.is_whitespace()).count();
    if non_ws_chars < MIN_LESSON_ACTION_CHARS {
        return false;
    }
    let word_count = count_word_tokens(action);
    if word_count < MIN_LESSON_ACTION_WORDS {
        return false;
    }
    true
}

fn collapse_whitespace_lower(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_ws && !out.is_empty() {
                out.push(' ');
                prev_ws = true;
            }
        } else {
            for lower in ch.to_lowercase() {
                out.push(lower);
            }
            prev_ws = false;
        }
    }
    out.trim().to_string()
}

/// Convert a raw Memoria memory JSON object into a [`LessonHint`],
/// applying the type allowlist (`semantic` / `procedural`),
/// [`sanitize_for_prompt`], and the [`is_action_prompt_worthy`] quality
/// gate. Returns `None` when the memory is the wrong type, malformed, or
/// doesn't clear the quality bar.
///
/// Canonical mapper shared by the CLI (`memoria_retrieve_lessons`) and
/// the end-to-end integration tests so both paths are guaranteed to
/// apply the same filters.
pub fn memory_value_to_lesson_hint(m: &serde_json::Value) -> Option<LessonHint> {
    let content = m.get("content")?.as_str()?;
    let memory_type = m.get("memory_type")?.as_str()?;
    if !matches!(memory_type, "semantic" | "procedural") {
        return None;
    }
    let action = sanitize_for_prompt(content);
    if !is_action_prompt_worthy(&action) {
        return None;
    }
    let compact = if action.len() > 80 {
        action
            .split_once(['.', '—', ';'])
            .map(|(s, _)| s.trim().to_string())
    } else {
        None
    };
    Some(LessonHint {
        kind: LessonKind::PromptShape,
        trigger_signal: "memoria".into(),
        action,
        compact,
        workload_tag: None,
    })
}

fn count_word_tokens(s: &str) -> usize {
    let mut count = 0;
    let mut in_word = false;
    for ch in s.chars() {
        let is_wordy = ch.is_alphanumeric();
        if is_wordy {
            if !in_word {
                count += 1;
                in_word = true;
            }
        } else {
            in_word = false;
        }
    }
    count
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

#[cfg(test)]
mod tests {
    use super::*;

    fn hint_with_action(action: &str) -> LessonHint {
        LessonHint {
            kind: LessonKind::PromptShape,
            trigger_signal: "memoria".into(),
            action: action.to_string(),
            compact: None,
            workload_tag: None,
        }
    }

    #[test]
    fn scratchpad_phrase_test_rejected() {
        assert!(!hint_with_action("test").is_prompt_worthy());
    }

    #[test]
    fn scratchpad_phrase_memoria_test_rejected() {
        assert!(!hint_with_action("memoria — test").is_prompt_worthy());
        assert!(!hint_with_action("memoria - test").is_prompt_worthy());
        assert!(!hint_with_action("Memoria Test").is_prompt_worthy());
    }

    #[test]
    fn short_single_word_rejected() {
        assert!(!hint_with_action("done").is_prompt_worthy());
        assert!(!hint_with_action("OK").is_prompt_worthy());
    }

    #[test]
    fn empty_or_whitespace_rejected() {
        assert!(!hint_with_action("").is_prompt_worthy());
        assert!(!hint_with_action("   \n\t  ").is_prompt_worthy());
    }

    #[test]
    fn punctuation_only_rejected() {
        assert!(!hint_with_action("...").is_prompt_worthy());
        assert!(!hint_with_action("???").is_prompt_worthy());
    }

    #[test]
    fn two_short_words_rejected() {
        // Two tokens below word-count threshold (needs 3).
        assert!(!hint_with_action("run it").is_prompt_worthy());
    }

    #[test]
    fn real_advice_accepted() {
        assert!(
            hint_with_action(
                "Always run `cargo test` before committing code changes to main."
            )
            .is_prompt_worthy()
        );
    }

    #[test]
    fn terse_but_meaningful_advice_accepted() {
        // 3+ words, >= min chars, not a scratchpad phrase.
        assert!(hint_with_action("prefer rust for CLI tools").is_prompt_worthy());
    }

    #[test]
    fn cjk_advice_accepted() {
        // CJK ideographs count as alphanumeric; four chars of CJK still
        // passes the char count since char count uses byte-free scan.
        // We pick a longer phrase to pass the char and word threshold.
        let action = "提交前 请先 运行 cargo test";
        assert!(hint_with_action(action).is_prompt_worthy());
    }

    #[test]
    fn whitespace_variants_of_scratchpad_phrase_rejected() {
        // Memoria may store "  memoria  —   test  " or similar —
        // normalized dedup should still reject.
        assert!(!hint_with_action("  memoria   —   test  ").is_prompt_worthy());
    }

    #[test]
    fn boundary_exactly_at_min_chars_and_words() {
        // "run cargo test" = 13 non-ws chars, 3 words → accepted.
        assert!(hint_with_action("run cargo test").is_prompt_worthy());
        // "run cargo" = 8 non-ws chars, 2 words → rejected on char count.
        assert!(!hint_with_action("run cargo").is_prompt_worthy());
    }

    #[test]
    fn count_word_tokens_basic() {
        assert_eq!(count_word_tokens(""), 0);
        assert_eq!(count_word_tokens("hello"), 1);
        assert_eq!(count_word_tokens("hello world"), 2);
        assert_eq!(count_word_tokens("hello, world!"), 2);
        assert_eq!(count_word_tokens("  run  cargo test "), 3);
    }

    #[test]
    fn collapse_whitespace_lower_normalizes() {
        assert_eq!(collapse_whitespace_lower("  Memoria   TEST  "), "memoria test");
        assert_eq!(collapse_whitespace_lower("hello\nworld"), "hello world");
    }

    // ── memory_value_to_lesson_hint ────────────────────────────────────

    #[test]
    fn mapper_rejects_wrong_memory_type() {
        let m = serde_json::json!({
            "content": "Use RS256 for JWT signing, HS512 for internal only",
            "memory_type": "working",
        });
        assert!(memory_value_to_lesson_hint(&m).is_none());
    }

    #[test]
    fn mapper_rejects_scratchpad_content() {
        let m = serde_json::json!({
            "content": "test",
            "memory_type": "semantic",
        });
        assert!(memory_value_to_lesson_hint(&m).is_none());
    }

    #[test]
    fn mapper_rejects_memoria_test_phrase() {
        let m = serde_json::json!({
            "content": "memoria — test",
            "memory_type": "semantic",
        });
        assert!(
            memory_value_to_lesson_hint(&m).is_none(),
            "memoria — test scratchpad must be filtered"
        );
    }

    #[test]
    fn mapper_accepts_real_semantic_lesson() {
        let m = serde_json::json!({
            "content": "Always run `cargo test` before committing to main branch",
            "memory_type": "semantic",
        });
        let hint = memory_value_to_lesson_hint(&m).expect("should accept");
        assert_eq!(hint.kind, LessonKind::PromptShape);
        assert_eq!(hint.trigger_signal, "memoria");
        assert!(hint.action.starts_with("Always run"));
    }

    #[test]
    fn mapper_accepts_procedural_type() {
        let m = serde_json::json!({
            "content": "When encountering rate-limit errors, back off with exponential delay",
            "memory_type": "procedural",
        });
        assert!(memory_value_to_lesson_hint(&m).is_some());
    }

    #[test]
    fn mapper_returns_none_for_missing_content() {
        let m = serde_json::json!({ "memory_type": "semantic" });
        assert!(memory_value_to_lesson_hint(&m).is_none());
    }

    #[test]
    fn mapper_returns_none_for_missing_type() {
        let m = serde_json::json!({ "content": "some valid lesson text here" });
        assert!(memory_value_to_lesson_hint(&m).is_none());
    }
}

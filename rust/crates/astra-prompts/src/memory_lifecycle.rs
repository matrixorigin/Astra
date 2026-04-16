//! Memory lifecycle helpers — heuristics for detecting when to auto-store,
//! promote, or purge memories during the turn pipeline.

// ─── Negation detection ─────────────────────────────────────────────────────
// Prevents "I don't prefer X" from falsely triggering a preference signal.
// Applied to ALL signal categories via `contains_unnegated`.

const NEGATION_PREFIXES: &[&str] = &[
    // English
    "don't ",
    "dont ",
    "do not ",
    "not ",
    "never ",
    "no longer ",
    "won't ",
    "wont ",
    "cannot ",
    "can't ",
    "didn't ",
    "didnt ",
    "wouldn't ",
    "shouldn't ",
    "isn't ",
    "stop ",
    // Chinese
    "不",
    "没",
    "别",
    "不要",
    "不再",
    "不想",
    "没有",
];

/// Check if `text` contains any of `patterns` at a position NOT preceded by negation.
/// For each occurrence of each pattern, inspect the preceding ~20 chars for negation words.
/// If ALL occurrences are negated, returns false.
fn contains_unnegated(text: &str, patterns: &[&str]) -> bool {
    for pattern in patterns {
        for (pos, _) in text.match_indices(pattern) {
            let mut prefix_start = pos.saturating_sub(20);
            // Walk back to a valid char boundary (at most 3 bytes for UTF-8)
            while !text.is_char_boundary(prefix_start) {
                prefix_start = prefix_start.saturating_sub(1);
            }
            let prefix = &text[prefix_start..pos];
            if !NEGATION_PREFIXES.iter().any(|neg| prefix.contains(neg)) {
                return true; // Unnegated match found
            }
        }
    }
    false
}

// ─── Context filtering for ambiguous tracking keywords ──────────────────────
// "follow these steps" is not tracking. "follow matrixorigin" is.
// "watch out" is not tracking. "watch the repo" is.

/// Non-tracking continuations for ambiguous English keywords.
const FOLLOW_NON_TRACKING: &[&str] = &[
    "these",
    "the ",
    "this",
    "up",
    "my ",
    "your ",
    "along",
    "through",
    "ing step",
    "ing instruction",
    "ing the",
    "ing this",
    "ing my",
    "ing your",
    "ing along",
];
const WATCH_NON_TRACKING: &[&str] = &["out", "it ", "ing out", "ing it"];
const TRACK_NON_TRACKING: &[&str] = &["down", "back", " record", "ing down", "ing back"];

/// Check if an ambiguous keyword at `pos` in `text` is in a tracking context.
/// Returns false if followed by a non-tracking continuation (e.g., "follow these").
fn is_tracking_context(text: &str, keyword: &str, pos: usize, bad_suffixes: &[&str]) -> bool {
    // Check negation prefix
    let mut prefix_start = pos.saturating_sub(20);
    while !text.is_char_boundary(prefix_start) {
        prefix_start = prefix_start.saturating_sub(1);
    }
    let prefix = &text[prefix_start..pos];
    if NEGATION_PREFIXES.iter().any(|neg| prefix.contains(neg)) {
        return false;
    }
    // Check non-tracking suffix
    let after = &text[pos + keyword.len()..];
    let trimmed = after.trim_start();
    !bad_suffixes.iter().any(|suf| trimmed.starts_with(suf))
}

// ─── Public API ─────────────────────────────────────────────────────────────

/// Signals in LLM output that suggest a memory-worthy event occurred.
/// Returns Some(category) if the text contains a store-worthy pattern.
///
/// Categories:
/// - "preference" — user preference detected
/// - "tracking"   — user expressing interest in following/watching something
/// - "decision"   — architecture/design decision
/// - "fact" — factual knowledge worth remembering
/// - "convention" — coding convention or project rule
pub fn detect_store_signal(text: &str) -> Option<&'static str> {
    let lower = text.to_lowercase();

    // Tracking/interest intent — checked FIRST (high-value, specific)
    if contains_tracking_signal(&lower) {
        return Some("tracking");
    }

    // Preference patterns
    if contains_preference_signal(&lower) {
        return Some("preference");
    }

    // Decision patterns
    if contains_decision_signal(&lower) {
        return Some("decision");
    }

    // Convention patterns
    if contains_convention_signal(&lower) {
        return Some("convention");
    }

    // Fact patterns (weakest signal — only if explicit)
    if contains_fact_signal(&lower) {
        return Some("fact");
    }

    None
}

/// Detect if user INPUT expresses a tracking/follow/watch interest.
/// Unlike detect_store_signal (which analyzes LLM output), this is for user messages.
pub fn detect_tracking_intent(user_msg: &str) -> bool {
    let lower = user_msg.to_lowercase();
    contains_tracking_signal(&lower)
}

fn contains_tracking_signal(text: &str) -> bool {
    // Unambiguous patterns — Chinese + specific English multi-word phrases
    let clear_patterns = [
        // Chinese (unambiguous in tracking context)
        "关注",
        "跟踪",
        "留意",
        "感兴趣",
        "想了解",
        "想关注",
        "想跟踪",
        "想看",
        "想知道",
        "在意",
        "看好",
        // English (unambiguous multi-word)
        "interested in",
        "care about",
        "keep an eye on",
        "keeping track",
        "i'm following",
        "i follow",
        "i watch",
        "star ",
        "subscribe",
    ];
    if contains_unnegated(text, &clear_patterns) {
        return true;
    }

    // Ambiguous short keywords — context filtering required
    for (keyword, bad_suffixes) in [
        ("follow", FOLLOW_NON_TRACKING as &[&str]),
        ("watch", WATCH_NON_TRACKING),
        ("track", TRACK_NON_TRACKING),
    ] {
        for (pos, _) in text.match_indices(keyword) {
            if is_tracking_context(text, keyword, pos, bad_suffixes) {
                return true;
            }
        }
    }

    false
}

fn contains_preference_signal(text: &str) -> bool {
    let patterns = [
        "i prefer",
        "i always use",
        "i like to",
        "my preferred",
        "我喜欢",
        "我习惯",
        "我偏好",
        "我一般用",
        "i usually",
        "my convention is",
        "i want you to",
    ];
    contains_unnegated(text, &patterns)
}

fn contains_decision_signal(text: &str) -> bool {
    let patterns = [
        "let's go with",
        "we decided",
        "the approach is",
        "决定用",
        "采用",
        "方案是",
        "we'll use",
        "i chose",
        "the plan is",
        "going forward",
    ];
    contains_unnegated(text, &patterns)
}

fn contains_convention_signal(text: &str) -> bool {
    let patterns = [
        "always use",
        "never use",
        "convention is",
        "rule is",
        "standard is",
        "规范是",
        "约定是",
        "必须用",
        "naming convention",
        "code style",
    ];
    contains_unnegated(text, &patterns)
}

fn contains_fact_signal(text: &str) -> bool {
    let patterns = [
        "remember that",
        "keep in mind",
        "important:",
        "note:",
        "记住",
        "注意",
        "重要的是",
        "fyi",
        "for reference",
    ];
    contains_unnegated(text, &patterns)
}

/// Check if a working memory entry should be promoted to semantic.
/// A working memory is promotable when:
/// 1. It has been referenced in 2+ subsequent turns (indicates lasting relevance)
/// 2. It contains a decision, preference, or fact (not just intermediate state)
pub fn should_promote(content: &str, reference_count: u32) -> bool {
    if reference_count >= 2 {
        return true;
    }
    // Single reference but high-value content
    if reference_count >= 1 {
        return detect_store_signal(content).is_some();
    }
    false
}

/// Check if a working memory should be purged.
/// Working memories are purgeable when:
/// 1. They are from a completed task (content contains done/completed markers)
/// 2. They are stale (older than `max_age_turns` turns)
pub fn should_purge_working(content: &str, age_turns: u32, max_age_turns: u32) -> bool {
    if age_turns > max_age_turns {
        return true;
    }
    let lower = content.to_lowercase();
    let done_markers = [
        "@task/done",
        "completed",
        "已完成",
        "done",
        "finished",
        "resolved",
    ];
    done_markers.iter().any(|m| lower.contains(m))
}

/// Suggest a memory namespace tag based on detected signal category.
pub fn suggest_namespace(category: &str) -> &'static str {
    match category {
        "tracking" => "@interest/active",
        "preference" => "@pref/active",
        "decision" => "@decision/semantic",
        "convention" => "@convention/semantic",
        "fact" => "@knowledge/semantic",
        _ => "@fact/semantic",
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── detect_store_signal ─────────────────────────────────────────────

    #[test]
    fn detects_tracking_intent_cn() {
        assert_eq!(detect_store_signal("我关注matrixorigin"), Some("tracking"));
        assert_eq!(detect_store_signal("我跟踪这个项目"), Some("tracking"));
        assert_eq!(detect_store_signal("我感兴趣"), Some("tracking"));
    }

    #[test]
    fn detects_tracking_intent_en() {
        assert_eq!(detect_store_signal("I follow this repo"), Some("tracking"));
        assert_eq!(
            detect_store_signal("interested in matrixone"),
            Some("tracking")
        );
        assert_eq!(detect_store_signal("keep an eye on it"), Some("tracking"));
    }

    #[test]
    fn follow_false_positives_rejected() {
        // "follow" in non-tracking context should NOT trigger
        assert_eq!(detect_store_signal("follow these steps to fix"), None);
        assert_eq!(detect_store_signal("follow the instructions below"), None);
        assert_eq!(detect_store_signal("follow up on the issue"), None);
        assert_eq!(detect_store_signal("following this guide"), None);
        // But tracking uses still work
        assert_eq!(detect_store_signal("follow matrixorigin"), Some("tracking"));
        assert_eq!(
            detect_store_signal("I follow this project"),
            Some("tracking")
        );
    }

    #[test]
    fn watch_false_positives_rejected() {
        assert_eq!(detect_store_signal("watch out for edge cases"), None);
        assert_eq!(detect_store_signal("watch it carefully"), None);
        // Tracking use works
        assert_eq!(detect_store_signal("watch this repo"), Some("tracking"));
        assert_eq!(detect_store_signal("i watch matrixone"), Some("tracking"));
    }

    #[test]
    fn track_false_positives_rejected() {
        assert_eq!(detect_store_signal("track down the bug"), None);
        assert_eq!(detect_store_signal("tracking down the issue"), None);
        // Tracking use works
        assert_eq!(
            detect_store_signal("track matrixorigin releases"),
            Some("tracking")
        );
    }

    #[test]
    fn detects_preference_en() {
        assert_eq!(
            detect_store_signal("I prefer using PostgreSQL"),
            Some("preference")
        );
        assert_eq!(detect_store_signal("I always use tabs"), Some("preference"));
    }

    #[test]
    fn detects_preference_cn() {
        assert_eq!(detect_store_signal("我喜欢用Rust"), Some("preference"));
        assert_eq!(detect_store_signal("我习惯用4空格缩进"), Some("preference"));
    }

    #[test]
    fn detects_decision() {
        assert_eq!(
            detect_store_signal("Let's go with approach B"),
            Some("decision")
        );
        assert_eq!(
            detect_store_signal("We decided to use Redis"),
            Some("decision")
        );
        assert_eq!(detect_store_signal("决定用PostgreSQL"), Some("decision"));
    }

    #[test]
    fn detects_convention() {
        assert_eq!(
            detect_store_signal("Always use snake_case"),
            Some("convention")
        );
        assert_eq!(
            detect_store_signal("The naming convention is camelCase"),
            Some("convention")
        );
        assert_eq!(detect_store_signal("规范是用4空格"), Some("convention"));
    }

    #[test]
    fn detects_fact() {
        assert_eq!(
            detect_store_signal("Remember that the API key is in .env"),
            Some("fact")
        );
        assert_eq!(detect_store_signal("记住这个端口是8080"), Some("fact"));
    }

    #[test]
    fn returns_none_for_normal_text() {
        assert_eq!(detect_store_signal("How do I fix this bug?"), None);
        assert_eq!(detect_store_signal("Please review the code"), None);
        assert_eq!(detect_store_signal("帮我看看这段代码"), None);
    }

    #[test]
    fn negation_suppresses_preference() {
        assert_eq!(detect_store_signal("I don't prefer tabs"), None);
        assert_eq!(detect_store_signal("I don't always use vim"), None);
        assert_eq!(detect_store_signal("不喜欢这个方案"), None);
    }

    #[test]
    fn negation_suppresses_tracking() {
        assert_eq!(detect_store_signal("don't follow this repo"), None);
        assert_eq!(detect_store_signal("I'm not interested in that"), None);
        assert_eq!(detect_store_signal("不关注这个项目"), None);
    }

    #[test]
    fn negation_suppresses_decision() {
        assert_eq!(detect_store_signal("we didn't decide on anything"), None);
        assert_eq!(detect_store_signal("haven't decided yet"), None);
    }

    #[test]
    fn negation_suppresses_convention() {
        assert_eq!(detect_store_signal("don't always use semicolons"), None);
    }

    #[test]
    fn negation_suppresses_fact() {
        assert_eq!(detect_store_signal("don't remember that detail"), None);
    }

    #[test]
    fn priority_tracking_over_preference() {
        // "关注" + "我喜欢" → tracking wins (checked first)
        assert_eq!(
            detect_store_signal("我关注这个，我喜欢这个项目"),
            Some("tracking")
        );
    }

    #[test]
    fn priority_preference_over_fact() {
        // "I prefer" + "remember that" → preference wins over fact
        assert_eq!(
            detect_store_signal("I prefer it this way, remember that"),
            Some("preference")
        );
    }

    // ── should_promote ──────────────────────────────────────────────────

    #[test]
    fn promote_on_multiple_references() {
        assert!(should_promote("random working memory", 2));
        assert!(should_promote("temporary task state", 3));
    }

    #[test]
    fn promote_on_single_reference_with_signal() {
        assert!(should_promote("I prefer using vim", 1));
        assert!(should_promote("决定用Redis", 1));
    }

    #[test]
    fn no_promote_on_single_reference_no_signal() {
        assert!(!should_promote("checking build status", 1));
    }

    #[test]
    fn no_promote_zero_references() {
        assert!(!should_promote("I prefer vim", 0));
    }

    // ── should_purge_working ────────────────────────────────────────────

    #[test]
    fn purge_stale_working_memory() {
        assert!(should_purge_working("task in progress", 11, 10));
    }

    #[test]
    fn purge_completed_task() {
        assert!(should_purge_working("[@task/done] fixed the build", 1, 10));
        assert!(should_purge_working("Task completed successfully", 1, 10));
        assert!(should_purge_working("已完成代码审查", 1, 10));
    }

    #[test]
    fn keep_active_working_memory() {
        assert!(!should_purge_working("task in progress", 3, 10));
    }

    // ── suggest_namespace ───────────────────────────────────────────────

    #[test]
    fn correct_namespace_suggestions() {
        assert_eq!(suggest_namespace("tracking"), "@interest/active");
        assert_eq!(suggest_namespace("preference"), "@pref/active");
        assert_eq!(suggest_namespace("decision"), "@decision/semantic");
        assert_eq!(suggest_namespace("convention"), "@convention/semantic");
        assert_eq!(suggest_namespace("fact"), "@knowledge/semantic");
        assert_eq!(suggest_namespace("unknown"), "@fact/semantic");
    }

    #[test]
    fn chinese_text_utf8_boundary_no_panic() {
        // Multi-byte Chinese chars (3 bytes each) can cause saturating_sub(20) to
        // land mid-character. This must not panic.
        let text = "这是一段很长的中文文本，我偏好使用深色主题";
        let result = detect_store_signal(text);
        assert_eq!(
            result,
            Some("preference"),
            "should detect preference in Chinese text"
        );

        // Negation in Chinese should suppress
        let negated = "这是一段很长的中文文本，不要我偏好使用深色主题";
        let result2 = detect_store_signal(negated);
        assert!(
            result2.is_none() || result2 != Some("preference"),
            "Chinese negation should suppress preference detection"
        );
    }
}

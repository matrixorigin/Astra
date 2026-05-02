//! Memory lifecycle helpers — tracking intent detection and namespace mapping.
//!
//! `detect_store_signal` and keyword-based signal matching were removed in
//! favor of LLM-driven memory decisions via the system prompt type taxonomy
//! (see `memory_types.rs`).

// ─── Negation detection ─────────────────────────────────────────────────────

const NEGATION_PREFIXES: &[&str] = &[
    "don't ", "dont ", "do not ", "not ", "never ", "no longer ",
    "won't ", "wont ", "cannot ", "can't ", "didn't ", "didnt ",
    "wouldn't ", "shouldn't ", "isn't ", "stop ",
    "不", "没", "别", "不要", "不再", "不想", "没有",
];

fn contains_unnegated(text: &str, patterns: &[&str]) -> bool {
    for pattern in patterns {
        for (pos, _) in text.match_indices(pattern) {
            let mut prefix_start = pos.saturating_sub(20);
            while !text.is_char_boundary(prefix_start) {
                prefix_start = prefix_start.saturating_sub(1);
            }
            let prefix = &text[prefix_start..pos];
            if !NEGATION_PREFIXES.iter().any(|neg| prefix.contains(neg)) {
                return true;
            }
        }
    }
    false
}

// ─── Context filtering for ambiguous tracking keywords ──────────────────────

const FOLLOW_NON_TRACKING: &[&str] = &[
    "these", "the ", "this", "up", "my ", "your ", "along", "through",
    "ing step", "ing instruction", "ing the", "ing this", "ing my",
    "ing your", "ing along",
];
const WATCH_NON_TRACKING: &[&str] = &["out", "it ", "ing out", "ing it"];
const TRACK_NON_TRACKING: &[&str] = &["down", "back", " record", "ing down", "ing back"];

fn is_tracking_context(text: &str, keyword: &str, pos: usize, bad_suffixes: &[&str]) -> bool {
    let mut prefix_start = pos.saturating_sub(20);
    while !text.is_char_boundary(prefix_start) {
        prefix_start = prefix_start.saturating_sub(1);
    }
    let prefix = &text[prefix_start..pos];
    if NEGATION_PREFIXES.iter().any(|neg| prefix.contains(neg)) {
        return false;
    }
    let after = &text[pos + keyword.len()..];
    let trimmed = after.trim_start();
    !bad_suffixes.iter().any(|suf| trimmed.starts_with(suf))
}

fn contains_tracking_signal(text: &str) -> bool {
    let clear_patterns = [
        "关注", "跟踪", "留意", "感兴趣", "想了解", "想关注", "想跟踪",
        "想看", "想知道", "在意", "看好",
        "interested in", "care about", "keep an eye on", "keeping track",
        "i'm following", "i follow", "i watch", "star ", "subscribe",
    ];
    if contains_unnegated(text, &clear_patterns) {
        return true;
    }
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

// ─── Public API ─────────────────────────────────────────────────────────────

/// Detect if user input expresses a tracking/follow/watch interest.
pub fn detect_tracking_intent(user_msg: &str) -> bool {
    let lower = user_msg.to_lowercase();
    contains_tracking_signal(&lower)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracking_namespace_is_interest_active() {
        assert_eq!(suggest_namespace("tracking"), "@interest/active");
    }

    #[test]
    fn detect_tracking_intent_chinese() {
        assert!(detect_tracking_intent("我关注matrixorigin"));
        assert!(detect_tracking_intent("我跟踪这个项目"));
        assert!(detect_tracking_intent("我感兴趣"));
    }

    #[test]
    fn detect_tracking_intent_english() {
        assert!(detect_tracking_intent("I'm following this project"));
        assert!(detect_tracking_intent("keep an eye on memoria"));
        assert!(detect_tracking_intent("interested in this library"));
    }

    #[test]
    fn non_tracking_rejected() {
        assert!(!detect_tracking_intent("show me the diff"));
        assert!(!detect_tracking_intent("帮我修复这个bug"));
        assert!(!detect_tracking_intent("follow these steps to fix"));
        assert!(!detect_tracking_intent("watch out for edge cases"));
    }

    #[test]
    fn negation_suppresses_tracking() {
        assert!(!detect_tracking_intent("don't follow that project"));
        assert!(!detect_tracking_intent("不关注这个了"));
    }

    #[test]
    fn suggest_namespace_all_categories() {
        assert_eq!(suggest_namespace("preference"), "@pref/active");
        assert_eq!(suggest_namespace("decision"), "@decision/semantic");
        assert_eq!(suggest_namespace("convention"), "@convention/semantic");
        assert_eq!(suggest_namespace("fact"), "@knowledge/semantic");
        assert_eq!(suggest_namespace("unknown"), "@fact/semantic");
    }
}

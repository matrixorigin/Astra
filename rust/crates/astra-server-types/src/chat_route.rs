use regex::RegexBuilder;
use serde::Serialize;

#[derive(Serialize, PartialEq)]
pub struct ChatRouteResponse {
    query: String,
    intent: Option<String>,
    confidence: f64,
    tier: u8,
    matched_by: String,
    task_type: String,
}

pub fn classify_chat_route(query: String) -> ChatRouteResponse {
    let stripped = query.trim();
    let intent = classify_chat_route_intent(stripped, 0);
    let task_type = classify_chat_route_task_type(stripped);

    ChatRouteResponse {
        query,
        intent: intent.intent.map(str::to_string),
        confidence: intent.confidence,
        tier: intent.tier,
        matched_by: intent.matched_by.to_string(),
        task_type: task_type.to_string(),
    }
}

struct RouteIntentClassification {
    intent: Option<&'static str>,
    confidence: f64,
    tier: u8,
    matched_by: &'static str,
}

struct KeywordMatch {
    label: Option<&'static str>,
}

fn classify_chat_route_intent(query: &str, history_len: usize) -> RouteIntentClassification {
    let regex_intent = regex_classify_chat_route_intent(query);
    let heuristic_intent = heuristic_classify_chat_route_intent(query, history_len);

    if regex_intent.is_some() && regex_intent == heuristic_intent {
        return RouteIntentClassification {
            intent: regex_intent,
            confidence: 0.95,
            tier: 0,
            matched_by: "both",
        };
    }
    if let Some(intent) = regex_intent {
        return RouteIntentClassification {
            intent: Some(intent),
            confidence: 0.80,
            tier: 0,
            matched_by: "regex",
        };
    }
    if let Some(intent) = heuristic_intent {
        return RouteIntentClassification {
            intent: Some(intent),
            confidence: 0.80,
            tier: 0,
            matched_by: "heuristic",
        };
    }

    RouteIntentClassification {
        intent: None,
        confidence: 0.0,
        tier: 0,
        matched_by: "none",
    }
}

fn regex_classify_chat_route_intent(query: &str) -> Option<&'static str> {
    for (intent, pattern) in [
        (
            "preference",
            r"记住|remember|I prefer|I use|需要|always use",
        ),
        ("command", r"^(run|execute|delete|create|list)\b"),
        ("feedback", r"^(不对|wrong|no,|actually)"),
    ] {
        if RegexBuilder::new(pattern)
            .case_insensitive(true)
            .build()
            .map(|regex| regex.is_match(query))
            .unwrap_or(false)
        {
            return Some(intent);
        }
    }
    None
}

fn heuristic_classify_chat_route_intent(query: &str, history_len: usize) -> Option<&'static str> {
    let stripped = query.trim();
    if stripped.is_empty() {
        return None;
    }
    let words = stripped.split_whitespace().count();
    if words <= 3 && stripped.ends_with('?') {
        return None;
    }
    if history_len == 0 && !stripped.ends_with('?') {
        return Some("command");
    }
    None
}

fn classify_chat_route_task_type(query: &str) -> &'static str {
    keyword_registry_match(
        query,
        &[
            (
                "code_review",
                &[
                    "review",
                    "code review",
                    "PR",
                    "pull request",
                    "refactor",
                    "clean up",
                ],
            ),
            (
                "debugging",
                &[
                    "debug",
                    "error",
                    "bug",
                    "fix",
                    "traceback",
                    "exception",
                    "crash",
                    "fail",
                ],
            ),
            (
                "planning",
                &[
                    "plan",
                    "design",
                    "architect",
                    "roadmap",
                    "strategy",
                    "proposal",
                ],
            ),
        ],
        &[],
    )
    .label
    .unwrap_or("general")
}

fn keyword_registry_match(
    query: &str,
    labels: &[(&'static str, &[&str])],
    negative_keywords: &[(&'static str, &[&str])],
) -> KeywordMatch {
    let query = query.trim();
    if query.is_empty() {
        return KeywordMatch { label: None };
    }

    let mut best_label = None;
    let mut best_score = 0.0;

    for (label, keywords) in labels {
        let matched = keywords
            .iter()
            .filter(|keyword| keyword_matches(query, keyword))
            .collect::<Vec<_>>();
        if matched.is_empty() {
            continue;
        }

        let negatives = negative_keywords
            .iter()
            .find(|(negative_label, _)| negative_label == label)
            .map(|(_, keywords)| *keywords)
            .unwrap_or(&[]);
        if negatives
            .iter()
            .any(|keyword| keyword_matches(query, keyword))
        {
            continue;
        }

        let matched_len = matched
            .iter()
            .map(|keyword| keyword.chars().count())
            .sum::<usize>();
        let score = ((matched_len as f64) / (query.chars().count().max(1) as f64)).min(1.0);
        if score > best_score {
            best_score = score;
            best_label = Some(*label);
        }
    }

    KeywordMatch { label: best_label }
}

fn keyword_matches(query: &str, keyword: &str) -> bool {
    if keyword.chars().any(is_cjk) {
        return query.to_lowercase().contains(&keyword.to_lowercase());
    }

    RegexBuilder::new(&format!(r"\b{}\b", regex::escape(keyword)))
        .case_insensitive(true)
        .build()
        .map(|regex| regex.is_match(query))
        .unwrap_or(false)
}

fn is_cjk(ch: char) -> bool {
    ('\u{4E00}'..='\u{9FFF}').contains(&ch)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ──────────────────────────────────────────────────────────
    // is_cjk
    // ──────────────────────────────────────────────────────────

    #[test]
    fn is_cjk_chinese_char() {
        assert!(is_cjk('你'));
        assert!(is_cjk('好'));
    }

    #[test]
    fn is_cjk_ascii_char() {
        assert!(!is_cjk('a'));
        assert!(!is_cjk('Z'));
    }

    #[test]
    fn is_cjk_boundary() {
        assert!(is_cjk('\u{4E00}')); // start
        assert!(is_cjk('\u{9FFF}')); // end
        assert!(!is_cjk('\u{4DFF}')); // just before
    }

    // ──────────────────────────────────────────────────────────
    // keyword_matches
    // ──────────────────────────────────────────────────────────

    #[test]
    fn keyword_matches_exact_en() {
        assert!(keyword_matches("run the tests", "run"));
    }

    #[test]
    fn keyword_matches_boundary_en() {
        // "run" should not match inside "running" via word boundary
        // Actually \brun\b won't match "running", but let's check
        assert!(!keyword_matches("running fast", "run"));
    }

    #[test]
    fn keyword_matches_cjk() {
        assert!(keyword_matches("帮我搜索一下", "搜索"));
    }

    #[test]
    fn keyword_matches_case_insensitive() {
        assert!(keyword_matches("Hello World", "hello"));
    }

    #[test]
    fn keyword_matches_no_match() {
        assert!(!keyword_matches("hello world", "xyzzy"));
    }

    // ──────────────────────────────────────────────────────────
    // keyword_registry_match
    // ──────────────────────────────────────────────────────────

    #[test]
    fn registry_match_empty_query() {
        let m = keyword_registry_match("", &[("test", &["hello"])], &[]);
        assert!(m.label.is_none());
    }

    #[test]
    fn registry_match_single_hit() {
        let m = keyword_registry_match("hello world", &[("greet", &["hello"])], &[]);
        assert_eq!(m.label, Some("greet"));
    }

    #[test]
    fn registry_match_negative_suppresses() {
        let m = keyword_registry_match(
            "search the code",
            &[("search", &["search"])],
            &[("search", &["code"])],
        );
        assert!(m.label.is_none()); // negative keyword "code" suppressed it
    }

    #[test]
    fn registry_match_best_wins() {
        let m = keyword_registry_match(
            "debug this error",
            &[("greet", &["hello"]), ("debug", &["debug", "error"])],
            &[],
        );
        assert_eq!(m.label, Some("debug"));
    }

    // ──────────────────────────────────────────────────────────
    // regex_classify_chat_route_intent
    // ──────────────────────────────────────────────────────────

    #[test]
    fn regex_intent_preference() {
        assert_eq!(
            regex_classify_chat_route_intent("记住我的偏好"),
            Some("preference")
        );
        assert_eq!(
            regex_classify_chat_route_intent("I prefer tabs"),
            Some("preference")
        );
    }

    #[test]
    fn regex_intent_command() {
        assert_eq!(
            regex_classify_chat_route_intent("run cargo test"),
            Some("command")
        );
    }

    #[test]
    fn regex_intent_feedback() {
        assert_eq!(
            regex_classify_chat_route_intent("不对，应该是这样"),
            Some("feedback")
        );
    }

    #[test]
    fn regex_intent_no_match() {
        assert!(regex_classify_chat_route_intent("explain this code").is_none());
    }

    // ──────────────────────────────────────────────────────────
    // heuristic_classify_chat_route_intent
    // ──────────────────────────────────────────────────────────

    #[test]
    fn heuristic_empty_query() {
        assert!(heuristic_classify_chat_route_intent("", 0).is_none());
    }

    #[test]
    fn heuristic_short_question() {
        // ≤3 words ending in ? → None
        assert!(heuristic_classify_chat_route_intent("what is this?", 0).is_none());
    }

    #[test]
    fn heuristic_first_turn_no_question_mark() {
        // history_len=0, no ?, >3 words → command
        assert_eq!(
            heuristic_classify_chat_route_intent("create a new file here", 0),
            Some("command")
        );
    }

    #[test]
    fn heuristic_non_first_turn() {
        // history_len > 0 → None (no heuristic match)
        assert!(heuristic_classify_chat_route_intent("do something", 5).is_none());
    }

    // ──────────────────────────────────────────────────────────
    // classify_chat_route_intent (combined)
    // ──────────────────────────────────────────────────────────

    #[test]
    fn combined_intent_both_match_high_confidence() {
        // "run cargo test" → regex matches "command", heuristic also "command" (first turn)
        let r = classify_chat_route_intent("run cargo test", 0);
        assert_eq!(r.intent, Some("command"));
        assert_eq!(r.confidence, 0.95);
        assert_eq!(r.matched_by, "both");
    }

    #[test]
    fn combined_intent_regex_only() {
        // "remember this" → regex matches "preference", heuristic returns None (history>0)
        let r = classify_chat_route_intent("remember this", 5);
        assert_eq!(r.intent, Some("preference"));
        assert_eq!(r.matched_by, "regex");
    }

    #[test]
    fn combined_intent_none() {
        let r = classify_chat_route_intent("explain this code to me please", 5);
        assert!(r.intent.is_none());
        assert_eq!(r.matched_by, "none");
    }

    // ──────────────────────────────────────────────────────────
    // classify_chat_route_task_type
    // ──────────────────────────────────────────────────────────

    #[test]
    fn task_type_code_review() {
        assert_eq!(
            classify_chat_route_task_type("review this PR"),
            "code_review"
        );
    }

    #[test]
    fn task_type_debugging() {
        assert_eq!(
            classify_chat_route_task_type("debug this error"),
            "debugging"
        );
    }

    #[test]
    fn task_type_planning() {
        assert_eq!(
            classify_chat_route_task_type("design the architecture"),
            "planning"
        );
    }

    #[test]
    fn task_type_general() {
        assert_eq!(
            classify_chat_route_task_type("implement a new feature"),
            "general"
        );
    }

    // ──────────────────────────────────────────────────────────
    // classify_chat_route (full pipeline)
    // ──────────────────────────────────────────────────────────

    #[test]
    fn full_route_conversational() {
        let r = classify_chat_route("hi".to_string());
        assert_eq!(r.query, "hi");
        assert_eq!(r.task_type, "general");
    }

    #[test]
    fn full_route_general_command() {
        let r = classify_chat_route("run all the tests".to_string());
        assert!(r.intent.is_some());
    }
}

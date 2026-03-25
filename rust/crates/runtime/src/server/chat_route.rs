use super::*;

#[derive(Serialize, PartialEq)]
pub(super) struct ChatRouteResponse {
    query: String,
    intent: Option<String>,
    confidence: f64,
    tier: u8,
    matched_by: String,
    tool_filter: String,
    max_tool_rounds: u32,
    task_type: String,
}

pub(super) fn classify_chat_route(query: String) -> ChatRouteResponse {
    let stripped = query.trim();
    let intent = classify_chat_route_intent(stripped, 0);
    let (tool_filter, max_tool_rounds) = classify_chat_route_tool_filter(stripped);
    let task_type = classify_chat_route_task_type(stripped);

    ChatRouteResponse {
        query,
        intent: intent.intent.map(str::to_string),
        confidence: intent.confidence,
        tier: intent.tier,
        matched_by: intent.matched_by.to_string(),
        tool_filter: tool_filter.to_string(),
        max_tool_rounds,
        task_type: task_type.to_string(),
    }
}

pub(super) struct RouteIntentClassification {
    intent: Option<&'static str>,
    confidence: f64,
    tier: u8,
    matched_by: &'static str,
}

pub(super) struct KeywordMatch {
    label: Option<&'static str>,
    score: f64,
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

fn classify_chat_route_tool_filter(query: &str) -> (&'static str, u32) {
    let tool_filter = keyword_registry_match(
        query,
        &[
            (
                "CONVERSATIONAL",
                &[
                    "hello",
                    "hi",
                    "hey",
                    "thanks",
                    "thank you",
                    "bye",
                    "goodbye",
                    "good morning",
                    "good evening",
                    "how are you",
                    "what's up",
                    "who are you",
                    "what can you do",
                    "help me",
                    "yes",
                    "no",
                    "ok",
                    "okay",
                    "sure",
                    "great",
                    "nice",
                    "please",
                    "sorry",
                    "excuse me",
                    "你好",
                    "您好",
                    "谢谢",
                    "感谢",
                    "再见",
                    "拜拜",
                    "早上好",
                    "晚上好",
                    "你是谁",
                    "你能做什么",
                    "好的",
                    "可以",
                    "是的",
                    "不是",
                    "没问题",
                    "请",
                    "抱歉",
                    "对不起",
                ],
            ),
            (
                "EXTERNAL_FETCH",
                &[
                    "search online",
                    "look up",
                    "find online",
                    "web search",
                    "what is the latest",
                    "current price",
                    "today's",
                    "fetch from",
                    "download",
                    "api call",
                    "http",
                    "weather",
                    "news",
                    "stock price",
                    "check the website",
                    "browse",
                    "搜索",
                    "查找",
                    "查一下",
                    "网上找",
                    "最新的",
                    "当前价格",
                    "今天的",
                    "下载",
                    "获取",
                    "抓取",
                    "天气",
                    "新闻",
                    "股价",
                ],
            ),
        ],
        &[(
            "EXTERNAL_FETCH",
            &[
                "file",
                "code",
                "class",
                "function",
                "method",
                "variable",
                "refactor",
                "implement",
                "debug",
                "fix",
                "bug",
                "test",
                "import",
                "module",
                "package",
                "repository",
                "repo",
                "algorithm",
                "sort",
                "tree",
                "array",
                "list",
                "dict",
            ],
        )],
    );

    if tool_filter.label == Some("CONVERSATIONAL") {
        let mut score = tool_filter.score;
        if query.trim().chars().count() < 20 && score > 0.0 {
            score = (score * 2.0).min(1.0);
        }
        if score >= 0.25 {
            return ("all_blocked", 0);
        }
    } else if tool_filter.label == Some("EXTERNAL_FETCH") && tool_filter.score >= 0.25 {
        return ("local_blocked", 3);
    }

    ("none", 10)
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
        return KeywordMatch {
            label: None,
            score: 0.0,
        };
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

    KeywordMatch {
        label: best_label,
        score: best_score,
    }
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

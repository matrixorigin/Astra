use crate::chat_turn_heuristics::{TaskExecutionProfile, is_short_continuation_prompt};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnScenarioHint {
    CodeReview,
    Debugging,
    QuickAnswer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TurnInputSignals {
    pub correction_signal: bool,
    pub low_information_followup: bool,
    pub continue_current_objective: bool,
    pub prohibit_code_review: bool,
    pub scenario_hint: Option<TurnScenarioHint>,
}

impl TurnInputSignals {
    #[must_use]
    pub fn has_signal(self) -> bool {
        self.correction_signal
            || self.low_information_followup
            || self.continue_current_objective
            || self.prohibit_code_review
            || self.scenario_hint.is_some()
    }
}

/// Broader follow-up detector used for prompt anchoring / active-thread
/// attachment. This is intentionally wider than
/// [`is_short_continuation_prompt`], which is reserved for routing-time
/// continuation semantics.
#[must_use]
pub fn is_low_information_followup(line: &str) -> bool {
    if is_short_continuation_prompt(line) {
        return true;
    }

    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.chars().count() > 32 {
        return false;
    }

    let lower = trimmed.to_ascii_lowercase();
    let has_action = contains_any_token(
        &lower,
        &[
            "fix",
            "patch",
            "repair",
            "implement",
            "apply",
            "edit",
            "update",
            "test",
            "verify",
            "run",
            "commit",
            "push",
            "continue",
            "resume",
            "retry",
        ],
    ) || contains_any_token(
        trimmed,
        &[
            "修复",
            "修一下",
            "改一下",
            "改下",
            "处理一下",
            "处理下",
            "优化一下",
            "优化下",
            "测一下",
            "测试一下",
            "验证一下",
            "提交一下",
            "推一下",
            "继续",
            "重试",
        ],
    );
    if !has_action {
        return false;
    }

    let has_deictic_reference =
        contains_any_token(&lower, &["this", "it", "that", "them", "here", "there"])
            || contains_any_token(trimmed, &["这", "这个", "这里", "它", "这些", "那个"]);
    let has_question_shape =
        trimmed.ends_with('?') || trimmed.ends_with('？') || trimmed.ends_with('吗');
    let token_count = trimmed
        .split(|c: char| c.is_whitespace() || c == ',' || c == '，')
        .filter(|part| !part.is_empty())
        .count();
    let short_ascii_action =
        (trimmed.is_ascii() || trimmed.contains(char::is_whitespace)) && token_count <= 3;

    has_deictic_reference || has_question_shape || short_ascii_action
}

#[must_use]
pub fn is_correction_signal(message: &str) -> bool {
    let lower = message.to_lowercase();
    [
        "no,",
        "no i",
        "that's wrong",
        "that's not",
        "i meant",
        "i mean",
        "not that",
        "wrong,",
        "wrong.",
        "incorrect",
        "actually,",
        "actually i",
        "instead,",
        "forget that",
        "ignore that",
        "let me clarify",
        "to clarify",
        "what i want",
        "wait,",
        "hold on",
        "stop,",
        "不对",
        "错了",
        "不是这样",
        "我的意思是",
        "我是说",
        "等等",
        "停一下",
    ]
    .iter()
    .any(|pattern| lower.contains(pattern))
}

#[must_use]
pub fn classify_turn_input(message: &str, task_profile: TaskExecutionProfile) -> TurnInputSignals {
    let trimmed = message.trim();
    if trimmed.is_empty() {
        return TurnInputSignals::default();
    }

    let correction_signal = is_correction_signal(trimmed);
    let low_information_followup = is_low_information_followup(trimmed);
    let continue_current_objective = is_short_continuation_prompt(trimmed);
    let prohibit_code_review = explicitly_denies_review(trimmed);
    let scenario_hint = if continue_current_objective {
        None
    } else if !prohibit_code_review && looks_like_code_review_query(trimmed) {
        Some(TurnScenarioHint::CodeReview)
    } else if looks_like_debug_query(trimmed) {
        Some(TurnScenarioHint::Debugging)
    } else if looks_like_quick_answer_query(trimmed, task_profile) {
        Some(TurnScenarioHint::QuickAnswer)
    } else {
        None
    };

    TurnInputSignals {
        correction_signal,
        low_information_followup,
        continue_current_objective,
        prohibit_code_review,
        scenario_hint,
    }
}

fn looks_like_code_review_query(message: &str) -> bool {
    let lower = message.to_lowercase();
    if [
        "review",
        "code review",
        "diff review",
        "pull request review",
        "pr review",
        "approve",
        "feedback",
        "comment on",
        "评审",
        "审查",
        "代码审查",
        "代码评审",
    ]
    .iter()
    .any(|kw| contains_keyword_or_phrase(&lower, kw))
    {
        return true;
    }

    let mentions_review_action = ["inspect", "check", "look at", "review", "查看", "检查"]
        .iter()
        .any(|kw| contains_keyword_or_phrase(&lower, kw));
    let mentions_review_artifact = [
        "diff",
        "pull request",
        "pr",
        "commit",
        "patch",
        "local changes",
        "current changes",
        "当前修改",
        "本地修改",
        "改动",
        "变更",
    ]
    .iter()
    .any(|kw| contains_keyword_or_phrase(&lower, kw));

    mentions_review_action && mentions_review_artifact
}

fn looks_like_debug_query(message: &str) -> bool {
    let lower = message.to_lowercase();
    [
        "bug",
        "error",
        "crash",
        "debug",
        "issue",
        "problem",
        "wrong",
        "fail",
        "fails",
        "failing",
        "broken",
        "abort",
        "aborts",
        "exception",
        "stack trace",
        "报错",
        "错误",
        "失败",
        "崩溃",
        "异常",
        "出错",
    ]
    .iter()
    .any(|kw| contains_keyword_or_phrase(&lower, kw))
}

fn looks_like_quick_answer_query(message: &str, task_profile: TaskExecutionProfile) -> bool {
    if task_profile.mutates_workspace {
        return false;
    }
    if message.chars().count() > 200 {
        return false;
    }
    if looks_like_debug_query(message) || looks_like_code_review_query(message) {
        return false;
    }

    let lower = message.to_lowercase();
    let has_question_mark = message.ends_with('?') || message.ends_with('？');
    let english_markers = [
        "why", "what", "where", "which", "how", "who", "whose", "whom",
    ];
    let has_english_interrogative = lower
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| english_markers.contains(&word));
    let chinese_markers = [
        "为啥",
        "为什么",
        "怎么",
        "哪里",
        "哪个",
        "什么是",
        "什么情况",
    ];
    let has_chinese_interrogative = chinese_markers.iter().any(|marker| lower.contains(marker));

    has_question_mark || has_english_interrogative || has_chinese_interrogative
}

fn explicitly_denies_review(message: &str) -> bool {
    let lower = message.to_lowercase();
    let compact = lower
        .chars()
        .filter(|c| !c.is_whitespace() && !matches!(c, ',' | '.' | '，' | '。' | '、'))
        .collect::<String>();
    let denial_markers = [
        "do not", "don't", "dont", "not", "no", "avoid", "without", "stop", "不要", "别", "不用",
        "无需", "避免", "禁止",
    ];
    let review_targets = [
        "review",
        "code review",
        "diff review",
        "pull request review",
        "pr review",
        "评审",
        "审查",
        "代码审查",
        "代码评审",
    ];

    denial_markers.iter().any(|deny| {
        review_targets.iter().any(|target| {
            lower.contains(&format!("{deny} {target}"))
                || lower.contains(&format!("{deny}-{target}"))
                || compact.contains(&format!(
                    "{}{}",
                    deny.replace(' ', ""),
                    target.replace(' ', "")
                ))
        })
    })
}

fn contains_keyword_or_phrase(message: &str, keyword: &str) -> bool {
    if keyword.chars().any(char::is_whitespace) {
        return message.contains(keyword);
    }
    if keyword.is_ascii() {
        return message
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|word| word == keyword);
    }
    message.contains(keyword)
}

fn contains_any_token(haystack: &str, tokens: &[&str]) -> bool {
    tokens.iter().any(|token| haystack.contains(token))
}

#[cfg(test)]
mod tests {
    use super::{
        TurnInputSignals, TurnScenarioHint, classify_turn_input, is_correction_signal,
        is_low_information_followup,
    };
    use crate::chat_turn_heuristics::infer_task_execution_profile;

    #[test]
    fn low_information_followup_detects_repair_prompts() {
        assert!(is_low_information_followup("修复?"));
        assert!(is_low_information_followup("fix this"));
        assert!(is_low_information_followup("test it"));
        assert!(is_low_information_followup("还有什么？"));
        assert!(!is_low_information_followup("修一下输入法问题"));
        assert!(!is_low_information_followup(
            "implement request batching in runtime selector"
        ));
    }

    #[test]
    fn correction_signal_handles_english_and_chinese_redirects() {
        assert!(is_correction_signal("No, that's wrong."));
        assert!(is_correction_signal("不对，我的意思是改这里"));
        assert!(!is_correction_signal("please continue with the fix"));
    }

    #[test]
    fn classify_turn_input_detects_short_continuation_and_followup() {
        let message = "继续";
        let signals = classify_turn_input(message, infer_task_execution_profile(message));
        assert_eq!(
            signals,
            TurnInputSignals {
                low_information_followup: true,
                continue_current_objective: true,
                ..TurnInputSignals::default()
            }
        );
    }

    #[test]
    fn classify_turn_input_detects_code_review_hint() {
        let message = "please inspect the current changes";
        let signals = classify_turn_input(message, infer_task_execution_profile(message));
        assert_eq!(signals.scenario_hint, Some(TurnScenarioHint::CodeReview));
    }

    #[test]
    fn classify_turn_input_detects_debugging_hint() {
        let message = "why is this test failing?";
        let signals = classify_turn_input(message, infer_task_execution_profile(message));
        assert_eq!(signals.scenario_hint, Some(TurnScenarioHint::Debugging));
    }

    #[test]
    fn classify_turn_input_detects_quick_answer_hint() {
        let message = "where is the auth flow defined?";
        let signals = classify_turn_input(message, infer_task_execution_profile(message));
        assert_eq!(signals.scenario_hint, Some(TurnScenarioHint::QuickAnswer));
    }

    #[test]
    fn classify_turn_input_keeps_mutating_questions_out_of_quick_answer() {
        let message = "fix it?";
        let signals = classify_turn_input(message, infer_task_execution_profile(message));
        assert_ne!(signals.scenario_hint, Some(TurnScenarioHint::QuickAnswer));
        assert!(signals.continue_current_objective);
    }

    #[test]
    fn classify_turn_input_detects_review_prohibition() {
        let message = "don't review this, just continue the implementation";
        let signals = classify_turn_input(message, infer_task_execution_profile(message));
        assert!(signals.prohibit_code_review);
        assert_ne!(signals.scenario_hint, Some(TurnScenarioHint::CodeReview));
    }
}

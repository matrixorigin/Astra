/// User-input correction signal recognized across runtime routing,
/// prompt nudges, and feedback extraction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserCorrectionSignalKind {
    /// The user says the previous response or assumption was wrong.
    Correction,
    /// The user redirects the task goal or clarifies what should take
    /// precedence. This is correction-like for runtime reanchoring but should
    /// not automatically become a durable learned rule unless a concrete
    /// directive can be extracted.
    Reanchor,
}

#[must_use]
pub fn classify_user_correction_signal(message: &str) -> Option<UserCorrectionSignalKind> {
    let message = message.trim();
    if message.is_empty() {
        return None;
    }

    let lower = message.to_lowercase();
    if is_direct_correction(&lower) {
        return Some(UserCorrectionSignalKind::Correction);
    }
    if is_reanchor_nudge(message, &lower) {
        return Some(UserCorrectionSignalKind::Reanchor);
    }
    None
}

#[must_use]
pub fn is_user_correction_signal(message: &str) -> bool {
    classify_user_correction_signal(message).is_some()
}

/// Whether a correction-like user message contains a concrete directive that
/// is safe to carry into durable memory.
///
/// Correction/reanchor messages often describe a local failure episode ("not a
/// quick patch", "you misunderstood"). Those should reanchor the current turn
/// but should not be indexed as reusable memory. Durable memory needs an
/// actionable rule such as "don't use mocks", "always run tests", or
/// "不要用 case-by-case 修补".
#[must_use]
pub fn has_durable_correction_directive(message: &str) -> bool {
    let message = message.trim();
    if message.is_empty() {
        return false;
    }
    let lower = message.to_lowercase();

    directive_starts_at_boundary(message, &lower)
}

fn is_direct_correction(lower: &str) -> bool {
    [
        "no,",
        "no i",
        "that's wrong",
        "that's not",
        "not that",
        "wrong,",
        "wrong.",
        "wrong answer",
        "wrong approach",
        "incorrect",
        "不对",
        "错了",
        "不是这样",
        "你搞错",
        "不正确",
    ]
    .iter()
    .any(|pattern| lower.contains(pattern))
}

fn is_reanchor_nudge(message: &str, lower: &str) -> bool {
    is_english_reanchor_nudge(lower) || is_chinese_reanchor_nudge(message)
}

fn is_english_reanchor_nudge(lower: &str) -> bool {
    if [
        "you misunderstood",
        "you misread",
        "not what i asked",
        "what i asked for is",
        "what i need is",
        "what i want is",
        "what i want",
        "i asked for",
        "i need you to",
        "i meant",
        "i mean",
        "let me clarify",
        "to clarify",
        "my point is",
        "instead,",
        "forget that",
        "ignore that",
    ]
    .iter()
    .any(|pattern| lower.contains(pattern))
    {
        return true;
    }

    let padded = format!(" {lower} ");
    let has_negation = [
        " not ",
        " don't ",
        " do not ",
        " rather than ",
        " instead of ",
    ]
    .iter()
    .any(|pattern| padded.contains(pattern));
    let has_redirect = [
        "instead",
        "rather",
        "correct",
        "durable",
        "long-term",
        "long term",
        "systemic",
        "first principles",
        "workaround",
    ]
    .iter()
    .any(|pattern| lower.contains(pattern));

    has_negation && has_redirect
}

fn is_chinese_reanchor_nudge(message: &str) -> bool {
    let normalized = message.replace("是不是", "是否");
    if [
        "我的意思是",
        "我是说",
        "我再说一遍",
        "我重新说一次",
        "我要的是",
        "我想要的是",
        "需要的是",
        "说的是",
        "目标是",
        "正确的是",
    ]
    .iter()
    .any(|pattern| normalized.contains(pattern))
    {
        return true;
    }

    let has_negation = ["不是", "不要", "别"].iter().any(|pattern| {
        normalized.contains(pattern) && !normalized.contains(&format!("是否{pattern}"))
    });
    let has_redirect = [
        "而是",
        "要",
        "应该",
        "正确",
        "系统",
        "第一性原则",
        "长期",
        "长久",
        "临时",
        "补丁",
        "修修补补",
        "case by case",
    ]
    .iter()
    .any(|pattern| normalized.contains(pattern));

    has_negation && has_redirect
}

fn directive_starts_at_boundary(original: &str, lower: &str) -> bool {
    if starts_with_directive(lower) {
        return true;
    }

    for sep in &[", ", ". ", "，", "。", "; ", "；", "—", ": ", "："] {
        for (i, _) in lower.match_indices(sep) {
            let after = i + sep.len();
            if starts_with_directive(&lower[after..]) {
                return true;
            }
            // Chinese text often omits spaces after punctuation; use the
            // original byte offset too so CJK directives remain byte-aligned.
            if original
                .get(after..)
                .is_some_and(|rest| starts_with_directive(&rest.to_lowercase()))
            {
                return true;
            }
        }
    }
    false
}

fn starts_with_directive(text: &str) -> bool {
    let trimmed = text.trim_start();
    [
        "don't ", "do not ", "never ", "always ", "stop ", "use ", "prefer ", "avoid ", "不要",
        "别", "禁止", "避免", "应该", "优先", "使用",
    ]
    .iter()
    .any(|prefix| trimmed.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_direct_corrections() {
        assert_eq!(
            classify_user_correction_signal("No, that's wrong."),
            Some(UserCorrectionSignalKind::Correction)
        );
        assert_eq!(
            classify_user_correction_signal("不对，我的意思是改这里"),
            Some(UserCorrectionSignalKind::Correction)
        );
    }

    #[test]
    fn classifies_reanchor_nudges() {
        assert_eq!(
            classify_user_correction_signal("不是修修补补，要系统性解决"),
            Some(UserCorrectionSignalKind::Reanchor)
        );
        assert_eq!(
            classify_user_correction_signal("我想要的是长久健康运行，不是临时补丁"),
            Some(UserCorrectionSignalKind::Reanchor)
        );
        assert_eq!(
            classify_user_correction_signal("What I asked for is a durable fix, not a workaround"),
            Some(UserCorrectionSignalKind::Reanchor)
        );
        assert_eq!(
            classify_user_correction_signal(
                "You misunderstood the goal; keep the session healthy long-term"
            ),
            Some(UserCorrectionSignalKind::Reanchor)
        );
    }

    #[test]
    fn does_not_treat_chinese_question_as_correction() {
        assert_eq!(
            classify_user_correction_signal("是不是可以让 web-agent 支持 taskboard?"),
            None
        );
    }

    #[test]
    fn ambiguous_discourse_markers_are_not_corrections_by_themselves() {
        for message in [
            "wait, can you show the logs first?",
            "hold on while I check the branch name",
            "stop, collaborate and listen",
            "actually, can you show the diff first?",
        ] {
            assert_eq!(
                classify_user_correction_signal(message),
                None,
                "{message:?} should not become durable correction pressure"
            );
        }
    }

    #[test]
    fn normal_collaboration_messages_are_not_correction_pressure() {
        for message in [
            "commit and push",
            "还有看一下/tmp/astra-dev/",
            "方便astra的也可以保留",
            "需要看skill的内容",
        ] {
            assert_eq!(
                classify_user_correction_signal(message),
                None,
                "{message:?} should not become a correction or reanchor signal"
            );
        }
    }

    #[test]
    fn durable_directive_requires_actionable_rule() {
        assert!(has_durable_correction_directive(
            "wrong, don't use mocks in integration tests"
        ));
        assert!(has_durable_correction_directive(
            "我重新说一次，不要用case-by-case修补"
        ));
        assert!(has_durable_correction_directive(
            "always run focused tests before claiming completion"
        ));
        assert!(!has_durable_correction_directive(
            "我要的是长久健康运行，不是临时补丁"
        ));
        assert!(!has_durable_correction_directive(
            "You misunderstood the goal; keep the session healthy long-term"
        ));
    }
}

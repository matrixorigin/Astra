use std::sync::OnceLock;

use regex::{Regex, RegexBuilder};

#[derive(Clone, Debug, PartialEq)]
pub struct ImplicitSignal {
    pub signal_type: String,
    pub confidence: f64,
    pub evidence: String,
}

/// Structured feedback extracted from a correction signal.
///
/// Captures *why* the user corrected and *when* the rule applies, so the system
/// can judge edge cases rather than blindly following statistical patterns.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StructuredFeedback {
    /// The rule itself — what the user wants changed.
    pub rule: String,
    /// Why the user gave this feedback (incident, preference, past failure).
    pub reason: String,
    /// When/where this guidance applies (specific domain, task type, tool).
    pub apply_when: String,
    /// Source signal type that triggered extraction.
    pub source_signal: String,
    /// Confidence from the original signal detection.
    pub confidence: f64,
}

fn compile_patterns(patterns: &[&str]) -> Vec<Regex> {
    patterns
        .iter()
        .map(|pattern| {
            RegexBuilder::new(pattern)
                .case_insensitive(true)
                .build()
                .expect("implicit feedback regex should compile")
        })
        .collect()
}

fn negative_patterns() -> &'static [Regex] {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        compile_patterns(&[
            r"不对|错了|不是这样|你搞错|不正确|wrong|incorrect|that'?s not",
            r"没用|废话|能不能好好|别废话|太啰嗦|太长了|说重点|useless|terrible|awful|wtf|seriously\?",
            r"我(再|重新)说一(遍|次)|let me rephrase|i('?m| am) asking|i meant|what i want",
            r"具体(一点|点)|详细(说|讲)|举个例子|比如呢|能展开|be more specific|give.+example|elaborate",
        ])
    })
}

fn positive_patterns() -> &'static [Regex] {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        compile_patterns(&[
            r"^(谢谢|感谢|太好了|完美|不错|很好|棒|thanks|thank you|perfect|great|awesome|nice|good job|well done)",
        ])
    })
}

pub fn detect_implicit_feedback_signal(
    user_input: &str,
    prev_agent_response: Option<&str>,
) -> ImplicitSignal {
    let text_lower = user_input.trim().to_lowercase();

    if prev_agent_response.is_some() && text_lower.chars().count() < 10 {
        for pattern in negative_patterns().iter().take(2) {
            if pattern.is_match(&text_lower) {
                return ImplicitSignal {
                    signal_type: "correction".to_string(),
                    confidence: 0.9,
                    evidence: pattern.as_str().to_string(),
                };
            }
        }
    }

    for (index, pattern) in negative_patterns().iter().enumerate() {
        if pattern.is_match(user_input) {
            let signal_type = ["correction", "frustration", "rephrasing", "clarification"]
                .get(index)
                .copied()
                .unwrap_or("clarification");
            return ImplicitSignal {
                signal_type: signal_type.to_string(),
                confidence: 0.7,
                evidence: pattern.as_str().to_string(),
            };
        }
    }

    for pattern in positive_patterns() {
        if pattern.is_match(user_input) {
            return ImplicitSignal {
                signal_type: "positive".to_string(),
                confidence: 0.6,
                evidence: pattern.as_str().to_string(),
            };
        }
    }

    ImplicitSignal {
        signal_type: "neutral".to_string(),
        confidence: 0.3,
        evidence: String::new(),
    }
}

pub fn implicit_feedback_rating(signal_type: &str) -> i64 {
    match signal_type {
        "positive" => 5,
        "correction" | "frustration" | "negative" => 1,
        "rephrasing" => 2,
        "clarification" | "neutral" => 3,
        _ => 3,
    }
}

/// Build a context injection message for negative implicit feedback signals.
/// Returns `Some(directive)` for correction/frustration/rephrasing; `None` for positive/neutral.
pub fn implicit_feedback_context_injection(signal: &ImplicitSignal) -> Option<String> {
    match signal.signal_type.as_str() {
        "correction" => Some(format!(
            "[Session Feedback] The user's message suggests the previous response was incorrect or off-target.\n\
             Signal: correction (confidence: {:.1})\n\
             Be more careful, double-check assumptions, and consider asking clarifying questions before acting.",
            signal.confidence
        )),
        "frustration" => Some(format!(
            "[Session Feedback] The user expressed dissatisfaction with the previous response.\n\
             Signal: frustration (confidence: {:.1})\n\
             Slow down, acknowledge the issue, and adjust your approach. Ask what would help.",
            signal.confidence
        )),
        "rephrasing" => Some(format!(
            "[Session Feedback] The user is rephrasing their request, suggesting the previous response missed the point.\n\
             Signal: rephrasing (confidence: {:.1})\n\
             Pay close attention to what they're emphasizing differently this time.",
            signal.confidence
        )),
        "clarification" => Some(format!(
            "[Session Feedback] The user is asking for more detail or specificity.\n\
             Signal: clarification (confidence: {:.1})\n\
             Provide more concrete examples or step-by-step detail.",
            signal.confidence
        )),
        _ => None, // positive, neutral, unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detect(input: &str) -> ImplicitSignal {
        detect_implicit_feedback_signal(input, None)
    }

    fn detect_with_prev(input: &str) -> ImplicitSignal {
        detect_implicit_feedback_signal(input, Some("previous response"))
    }

    // ── Negative: correction ────────────────────────────────────────

    #[test]
    fn correction_chinese_bu_dui() {
        let s = detect("不对，这个答案有问题");
        assert_eq!(s.signal_type, "correction");
        assert_eq!(s.confidence, 0.7);
    }

    #[test]
    fn correction_chinese_cuo_le() {
        let s = detect("错了，应该是另一个");
        assert_eq!(s.signal_type, "correction");
    }

    #[test]
    fn correction_chinese_bu_zheng_que() {
        let s = detect("不正确，请重新回答");
        assert_eq!(s.signal_type, "correction");
    }

    #[test]
    fn correction_english_wrong() {
        let s = detect("wrong answer, try again");
        assert_eq!(s.signal_type, "correction");
        assert_eq!(s.confidence, 0.7);
    }

    #[test]
    fn correction_english_incorrect() {
        let s = detect("that is incorrect");
        assert_eq!(s.signal_type, "correction");
    }

    #[test]
    fn correction_english_thats_not() {
        let s = detect("that's not what I asked");
        assert_eq!(s.signal_type, "correction");
    }

    #[test]
    fn correction_english_thats_not_no_apostrophe() {
        let s = detect("thats not right at all");
        assert_eq!(s.signal_type, "correction");
    }

    // ── Negative: frustration ───────────────────────────────────────

    #[test]
    fn frustration_chinese_fei_hua() {
        let s = detect("废话连篇，一点用都没有");
        assert_eq!(s.signal_type, "frustration");
        assert_eq!(s.confidence, 0.7);
    }

    #[test]
    fn frustration_chinese_mei_yong() {
        let s = detect("没用的回复，浪费时间");
        assert_eq!(s.signal_type, "frustration");
    }

    #[test]
    fn frustration_chinese_tai_luo_suo() {
        let s = detect("太啰嗦了能不能简洁点");
        assert_eq!(s.signal_type, "frustration");
    }

    #[test]
    fn frustration_english_useless() {
        let s = detect("this is useless information");
        assert_eq!(s.signal_type, "frustration");
        assert_eq!(s.confidence, 0.7);
    }

    #[test]
    fn frustration_english_terrible() {
        let s = detect("terrible response honestly");
        assert_eq!(s.signal_type, "frustration");
    }

    #[test]
    fn frustration_english_wtf() {
        let s = detect("wtf is this output");
        assert_eq!(s.signal_type, "frustration");
    }

    // ── Negative: rephrasing ────────────────────────────────────────

    #[test]
    fn rephrasing_chinese_zai_shuo_yi_bian() {
        let s = detect("我再说一遍，请帮我写单元测试");
        assert_eq!(s.signal_type, "rephrasing");
        assert_eq!(s.confidence, 0.7);
    }

    #[test]
    fn rephrasing_chinese_chong_xin_shuo() {
        let s = detect("我重新说一次要求");
        assert_eq!(s.signal_type, "rephrasing");
    }

    #[test]
    fn rephrasing_english_let_me_rephrase() {
        let s = detect("let me rephrase the question");
        assert_eq!(s.signal_type, "rephrasing");
        assert_eq!(s.confidence, 0.7);
    }

    #[test]
    fn rephrasing_english_i_meant() {
        let s = detect("i meant something different");
        assert_eq!(s.signal_type, "rephrasing");
    }

    #[test]
    fn rephrasing_english_what_i_want() {
        let s = detect("what i want is a REST API");
        assert_eq!(s.signal_type, "rephrasing");
    }

    // ── Negative: clarification ─────────────────────────────────────

    #[test]
    fn clarification_chinese_ju_ti_yi_dian() {
        let s = detect("具体一点，给我看代码");
        assert_eq!(s.signal_type, "clarification");
        assert_eq!(s.confidence, 0.7);
    }

    #[test]
    fn clarification_chinese_ju_ge_li_zi() {
        let s = detect("举个例子说明一下");
        assert_eq!(s.signal_type, "clarification");
    }

    #[test]
    fn clarification_chinese_xiang_xi_shuo() {
        let s = detect("详细说说怎么实现的");
        assert_eq!(s.signal_type, "clarification");
    }

    #[test]
    fn clarification_english_be_more_specific() {
        let s = detect("be more specific about the API");
        assert_eq!(s.signal_type, "clarification");
        assert_eq!(s.confidence, 0.7);
    }

    #[test]
    fn clarification_english_give_example() {
        let s = detect("can you give me an example?");
        assert_eq!(s.signal_type, "clarification");
    }

    #[test]
    fn clarification_english_elaborate() {
        let s = detect("please elaborate on that point");
        assert_eq!(s.signal_type, "clarification");
    }

    // ── Short-input high-confidence correction ──────────────────────

    #[test]
    fn short_input_correction_chinese() {
        // "不对" is 2 chars (< 10), prev_response present, matches first pattern
        let s = detect_with_prev("不对");
        assert_eq!(s.signal_type, "correction");
        assert_eq!(s.confidence, 0.9);
    }

    #[test]
    fn short_input_correction_english() {
        // "wrong" is 5 chars (< 10), prev_response present, matches first pattern
        let s = detect_with_prev("wrong");
        assert_eq!(s.signal_type, "correction");
        assert_eq!(s.confidence, 0.9);
    }

    #[test]
    fn short_input_frustration_maps_to_correction() {
        // "废话" is 2 chars (< 10), prev_response present, matches second pattern
        // Short-input path takes first 2 patterns → returns "correction" with 0.9
        let s = detect_with_prev("废话");
        assert_eq!(s.signal_type, "correction");
        assert_eq!(s.confidence, 0.9);
    }

    #[test]
    fn short_input_useless_maps_to_correction() {
        // "useless" is 7 chars (< 10), matches second pattern via short-input path
        let s = detect_with_prev("useless");
        assert_eq!(s.signal_type, "correction");
        assert_eq!(s.confidence, 0.9);
    }

    #[test]
    fn short_input_no_prev_response_uses_normal_path() {
        // Short input but NO previous response → skip short-input path
        let s = detect("不对");
        assert_eq!(s.signal_type, "correction");
        assert_eq!(s.confidence, 0.7); // normal path confidence
    }

    #[test]
    fn long_input_with_prev_response_uses_normal_path() {
        // Long input (>= 10 chars) with prev_response → skip short-input path
        let s = detect_with_prev("wrong, this is completely off");
        assert_eq!(s.signal_type, "correction");
        assert_eq!(s.confidence, 0.7);
    }

    #[test]
    fn short_input_third_pattern_not_in_short_path() {
        // "i meant" is 7 chars, matches third pattern (rephrasing),
        // but short-input path only checks first 2 → falls through to normal path
        let s = detect_with_prev("i meant");
        assert_eq!(s.signal_type, "rephrasing");
        assert_eq!(s.confidence, 0.7);
    }

    // ── Positive patterns ───────────────────────────────────────────

    #[test]
    fn positive_chinese_xie_xie() {
        let s = detect("谢谢你的帮助");
        assert_eq!(s.signal_type, "positive");
        assert_eq!(s.confidence, 0.6);
    }

    #[test]
    fn positive_chinese_tai_hao_le() {
        let s = detect("太好了，就是这个");
        assert_eq!(s.signal_type, "positive");
    }

    #[test]
    fn positive_chinese_wan_mei() {
        let s = detect("完美，非常感谢");
        assert_eq!(s.signal_type, "positive");
    }

    #[test]
    fn positive_english_thanks() {
        let s = detect("thanks for the help");
        assert_eq!(s.signal_type, "positive");
        assert_eq!(s.confidence, 0.6);
    }

    #[test]
    fn positive_english_perfect() {
        let s = detect("perfect, that's exactly right");
        assert_eq!(s.signal_type, "positive");
    }

    #[test]
    fn positive_english_great() {
        let s = detect("great answer!");
        assert_eq!(s.signal_type, "positive");
    }

    #[test]
    fn positive_english_awesome() {
        let s = detect("awesome, works perfectly");
        assert_eq!(s.signal_type, "positive");
    }

    #[test]
    fn positive_english_good_job() {
        let s = detect("good job on that solution");
        assert_eq!(s.signal_type, "positive");
    }

    // ── CJK positive at start of input ──────────────────────────────

    #[test]
    fn cjk_positive_must_be_at_start() {
        // Positive pattern is anchored with ^, so mid-string match should not trigger
        let s = detect("我觉得谢谢也行");
        assert_eq!(s.signal_type, "neutral");
    }

    #[test]
    fn cjk_positive_at_start() {
        let s = detect("感谢你的回复真的很有用");
        assert_eq!(s.signal_type, "positive");
    }

    #[test]
    fn cjk_positive_bang() {
        let s = detect("棒极了");
        assert_eq!(s.signal_type, "positive");
    }

    // ── Neutral fallback ────────────────────────────────────────────

    #[test]
    fn neutral_unmatched_input() {
        let s = detect("tell me about Rust generics");
        assert_eq!(s.signal_type, "neutral");
        assert_eq!(s.confidence, 0.3);
        assert!(s.evidence.is_empty());
    }

    #[test]
    fn neutral_random_chinese() {
        let s = detect("帮我写一个排序算法");
        assert_eq!(s.signal_type, "neutral");
        assert_eq!(s.confidence, 0.3);
    }

    // ── Empty input ─────────────────────────────────────────────────

    #[test]
    fn empty_input_is_neutral() {
        let s = detect("");
        assert_eq!(s.signal_type, "neutral");
        assert_eq!(s.confidence, 0.3);
    }

    #[test]
    fn whitespace_only_is_neutral() {
        let s = detect("   ");
        assert_eq!(s.signal_type, "neutral");
        assert_eq!(s.confidence, 0.3);
    }

    // ── Case insensitivity ──────────────────────────────────────────

    #[test]
    fn case_insensitive_uppercase() {
        let s = detect("WRONG answer completely");
        assert_eq!(s.signal_type, "correction");
    }

    #[test]
    fn case_insensitive_mixed_case() {
        let s = detect("Wrong, that is not correct");
        assert_eq!(s.signal_type, "correction");
    }

    #[test]
    fn case_insensitive_lowercase() {
        let s = detect("wrong approach entirely");
        assert_eq!(s.signal_type, "correction");
    }

    #[test]
    fn case_insensitive_frustration() {
        let s = detect("TERRIBLE answer, do better");
        assert_eq!(s.signal_type, "frustration");
    }

    #[test]
    fn case_insensitive_positive() {
        let s = detect("THANKS a lot!");
        assert_eq!(s.signal_type, "positive");
    }

    #[test]
    fn case_insensitive_short_input_correction() {
        let s = detect_with_prev("WRONG");
        assert_eq!(s.signal_type, "correction");
        assert_eq!(s.confidence, 0.9);
    }

    // ── Rating function ─────────────────────────────────────────────

    #[test]
    fn rating_positive() {
        assert_eq!(implicit_feedback_rating("positive"), 5);
    }

    #[test]
    fn rating_correction() {
        assert_eq!(implicit_feedback_rating("correction"), 1);
    }

    #[test]
    fn rating_frustration() {
        assert_eq!(implicit_feedback_rating("frustration"), 1);
    }

    #[test]
    fn rating_negative() {
        assert_eq!(implicit_feedback_rating("negative"), 1);
    }

    #[test]
    fn rating_rephrasing() {
        assert_eq!(implicit_feedback_rating("rephrasing"), 2);
    }

    #[test]
    fn rating_clarification() {
        assert_eq!(implicit_feedback_rating("clarification"), 3);
    }

    #[test]
    fn rating_neutral() {
        assert_eq!(implicit_feedback_rating("neutral"), 3);
    }

    #[test]
    fn rating_unknown_falls_back_to_3() {
        assert_eq!(implicit_feedback_rating("unknown"), 3);
        assert_eq!(implicit_feedback_rating("something_else"), 3);
    }

    // ── Evidence field ──────────────────────────────────────────────

    #[test]
    fn evidence_populated_on_match() {
        let s = detect("wrong answer");
        assert!(
            !s.evidence.is_empty(),
            "evidence should contain the matched pattern"
        );
    }

    #[test]
    fn evidence_empty_on_neutral() {
        let s = detect("just a normal question");
        assert!(s.evidence.is_empty());
    }

    // ── Context injection ───────────────────────────────────────────

    #[test]
    fn context_injection_correction() {
        let signal = ImplicitSignal {
            signal_type: "correction".to_string(),
            confidence: 0.9,
            evidence: String::new(),
        };
        let ctx = implicit_feedback_context_injection(&signal);
        assert!(ctx.is_some());
        let text = ctx.unwrap();
        assert!(text.contains("[Session Feedback]"));
        assert!(text.contains("correction"));
        assert!(text.contains("0.9"));
    }

    #[test]
    fn context_injection_frustration() {
        let signal = ImplicitSignal {
            signal_type: "frustration".to_string(),
            confidence: 0.7,
            evidence: String::new(),
        };
        let ctx = implicit_feedback_context_injection(&signal);
        assert!(ctx.is_some());
        assert!(ctx.unwrap().contains("dissatisfaction"));
    }

    #[test]
    fn context_injection_neutral_returns_none() {
        let signal = ImplicitSignal {
            signal_type: "neutral".to_string(),
            confidence: 0.3,
            evidence: String::new(),
        };
        assert!(implicit_feedback_context_injection(&signal).is_none());
    }

    #[test]
    fn context_injection_positive_returns_none() {
        let signal = ImplicitSignal {
            signal_type: "positive".to_string(),
            confidence: 0.6,
            evidence: String::new(),
        };
        assert!(implicit_feedback_context_injection(&signal).is_none());
    }
}

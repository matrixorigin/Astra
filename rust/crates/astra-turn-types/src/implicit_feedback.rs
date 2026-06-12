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

    // ── Negative signal type: data-driven ────────────────────────────

    #[test]
    fn negative_signals() {
        let cases: &[(&str, &[&str])] = &[
            // correction
            (
                "correction",
                &[
                    "不对，这个答案有问题",
                    "错了，应该是另一个",
                    "不正确，请重新回答",
                    "wrong answer, try again",
                    "that is incorrect",
                    "that's not what I asked",
                    "thats not right at all",
                ],
            ),
            // frustration
            (
                "frustration",
                &[
                    "废话连篇，一点用都没有",
                    "没用的回复，浪费时间",
                    "太啰嗦了能不能简洁点",
                    "this is useless information",
                    "terrible response honestly",
                    "wtf is this output",
                ],
            ),
            // rephrasing
            (
                "rephrasing",
                &[
                    "我再说一遍，请帮我写单元测试",
                    "我重新说一次要求",
                    "let me rephrase the question",
                    "i meant something different",
                    "what i want is a REST API",
                ],
            ),
            // clarification
            (
                "clarification",
                &[
                    "具体一点，给我看代码",
                    "举个例子说明一下",
                    "详细说说怎么实现的",
                    "be more specific about the API",
                    "can you give me an example?",
                    "please elaborate on that point",
                ],
            ),
        ];
        for (expected_type, inputs) in cases {
            for input in *inputs {
                let s = detect(input);
                assert_eq!(s.signal_type, *expected_type, "input: {input:?}");
                assert_eq!(s.confidence, 0.7, "input: {input:?}");
            }
        }
    }

    // ── Short-input high-confidence correction ───────────────────────

    #[test]
    fn short_input_correction_boost() {
        let cases: &[&str] = &["不对", "wrong", "废话", "useless"];
        for input in cases {
            let s = detect_with_prev(input);
            assert_eq!(s.signal_type, "correction", "input: {input:?}");
            assert_eq!(s.confidence, 0.9, "input: {input:?}");
        }
    }

    #[test]
    fn short_input_no_prev_uses_normal_path() {
        let s = detect("不对");
        assert_eq!(s.signal_type, "correction");
        assert_eq!(s.confidence, 0.7);
    }

    #[test]
    fn long_input_with_prev_uses_normal_path() {
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

    // ── Positive patterns ────────────────────────────────────────────

    #[test]
    fn positive_signals() {
        let inputs = &[
            "谢谢你的帮助",
            "太好了，就是这个",
            "完美，非常感谢",
            "thanks for the help",
            "perfect, that's exactly right",
            "great answer!",
            "awesome, works perfectly",
            "good job on that solution",
            "感谢你的回复真的很有用",
            "棒极了",
        ];
        for input in inputs {
            let s = detect(input);
            assert_eq!(s.signal_type, "positive", "input: {input:?}");
            assert_eq!(s.confidence, 0.6, "input: {input:?}");
        }
    }

    #[test]
    fn cjk_positive_must_be_at_start() {
        let s = detect("我觉得谢谢也行");
        assert_eq!(s.signal_type, "neutral");
    }

    // ── Neutral fallback ─────────────────────────────────────────────

    #[test]
    fn neutral_unmatched() {
        let cases = &[
            "tell me about Rust generics",
            "帮我写一个排序算法",
            "",
            "   ",
        ];
        for input in cases {
            let s = detect(input);
            assert_eq!(s.signal_type, "neutral", "input: {input:?}");
            assert_eq!(s.confidence, 0.3, "input: {input:?}");
            assert!(s.evidence.is_empty(), "input: {input:?}");
        }
    }

    // ── Case insensitivity ───────────────────────────────────────────

    #[test]
    fn case_insensitive_matches() {
        let cases: &[(&str, &str)] = &[
            ("WRONG answer completely", "correction"),
            ("Wrong, that is not correct", "correction"),
            ("wrong approach entirely", "correction"),
            ("TERRIBLE answer, do better", "frustration"),
            ("THANKS a lot!", "positive"),
        ];
        for (input, expected_type) in cases {
            let s = detect(input);
            assert_eq!(s.signal_type, *expected_type, "input: {input:?}");
        }
    }

    #[test]
    fn case_insensitive_short_input_correction() {
        let s = detect_with_prev("WRONG");
        assert_eq!(s.signal_type, "correction");
        assert_eq!(s.confidence, 0.9);
    }

    // ── Rating function ──────────────────────────────────────────────

    #[test]
    fn rating() {
        let cases: &[(&str, i64)] = &[
            ("positive", 5),
            ("correction", 1),
            ("frustration", 1),
            ("negative", 1),
            ("rephrasing", 2),
            ("clarification", 3),
            ("neutral", 3),
            ("unknown", 3),
            ("something_else", 3),
        ];
        for (signal_type, expected) in cases {
            assert_eq!(
                implicit_feedback_rating(signal_type),
                *expected,
                "type: {signal_type}"
            );
        }
    }

    // ── Evidence is populated on match, empty on neutral ─────────────

    #[test]
    fn evidence_present_on_match_empty_on_neutral() {
        let s = detect("wrong answer");
        assert!(
            !s.evidence.is_empty(),
            "evidence should contain matched pattern"
        );

        let s = detect("just a normal question");
        assert!(s.evidence.is_empty());
    }

    // ── Context injection ────────────────────────────────────────────

    #[test]
    fn context_injection_correction_or_frustration() {
        for (signal_type, keyword) in &[
            ("correction", "correction"),
            ("frustration", "dissatisfaction"),
        ] {
            let signal = ImplicitSignal {
                signal_type: signal_type.to_string(),
                confidence: 0.7,
                evidence: String::new(),
            };
            let ctx = implicit_feedback_context_injection(&signal).expect("should produce context");
            assert!(ctx.contains("[Session Feedback]"));
            assert!(ctx.contains(keyword));
        }
    }

    #[test]
    fn context_injection_neutral_and_positive_return_none() {
        for signal_type in &["neutral", "positive"] {
            let signal = ImplicitSignal {
                signal_type: signal_type.to_string(),
                confidence: 0.3,
                evidence: String::new(),
            };
            assert!(
                implicit_feedback_context_injection(&signal).is_none(),
                "type={signal_type}"
            );
        }
    }
}

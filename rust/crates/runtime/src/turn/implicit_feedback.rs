use std::sync::OnceLock;

use regex::{Regex, RegexBuilder};

#[derive(Clone, Debug, PartialEq)]
pub struct ImplicitSignal {
    pub signal_type: String,
    pub confidence: f64,
    pub evidence: String,
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

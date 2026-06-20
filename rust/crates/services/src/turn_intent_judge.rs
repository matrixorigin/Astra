//! LLM-based turn intent judging.
//!
//! The agentic loop must understand what the user's current message is
//! asking for: are they continuing the previous objective, requesting a
//! review, prohibiting one, asking a quick question? Historically this
//! was a string-matching classifier. That works for the cleanest cases but
//! breaks down on paraphrases, mixed-language input, indirect speech, and
//! anything non-trivial — the cases LLMs are actually good at.
//!
//! Architecture
//! ============
//! - [`TurnIntentJudge`] — async trait, sibling of [`crate::LlmJudge`].
//!   Implementations call an LLM (typically via the server's
//!   `/v1/chat/completions` proxy) and produce a structured
//!   [`TurnIntent`].
//! - [`build_turn_intent_prompt`] — pure function that produces the prompt
//!   sent to the judge. Live in services so prompts can be tested
//!   independently of any concrete LLM client.
//! - [`parse_turn_intent_response`] — pure JSON parser that converts the
//!   judge's text into a [`TurnIntent`]. Strict on shape; unknown values
//!   produce `Err` rather than silently degrading.
//!
//! Usage pattern (host side):
//!
//! ```ignore
//! let intent = match judge.judge(&ctx).await {
//!     Ok(intent) => Some(intent),
//!     Err(error) => { /* telemetry, then proceed without explicit intent */ None }
//! };
//! ```
//!
//! The judge is the only component that may classify natural-language turn
//! intent. Runtime fallbacks must use structural facts, not keyword lists.

use astra_config::user_profile::{Scenario, TurnContinuationMode, TurnIntent};
use async_trait::async_trait;

/// Context passed to the turn intent judge.
#[derive(Debug, Clone, Default)]
pub struct TurnIntentJudgeContext {
    /// The user's current message (the one being judged).
    pub message: String,
    /// 1-based turn count so the judge can weight follow-ups vs initial turns.
    pub turn_count: u32,
    /// Tool names used in the most recent assistant turn(s) — useful for the
    /// judge to detect "continue" / "looks good" follow-ups.
    pub recent_tools: Vec<String>,
    /// True when the previous assistant turn produced output (i.e. there is
    /// a current objective the user could be continuing or correcting).
    pub has_prior_assistant_turn: bool,
}

/// Errors a [`TurnIntentJudge`] may return.
#[derive(Debug, thiserror::Error)]
pub enum TurnIntentJudgeError {
    /// LLM call failed (network, rate-limit, auth). The host must not block
    /// the turn on this class; it should proceed without explicit turn intent.
    #[error("LLM transport failure: {0}")]
    Transport(String),

    /// LLM returned a response that could not be parsed into a TurnIntent.
    /// Include the (truncated) raw text so telemetry can attribute the
    /// failure to a specific prompt or model version.
    #[error("LLM returned malformed response: {raw}")]
    Malformed { raw: String },

    /// The judge is configured but the model was rejected (e.g. moderation
    /// flag, unsupported region). Caller should log and continue without
    /// explicit turn intent.
    #[error("LLM rejected: {0}")]
    Rejected(String),
}

/// Trait for LLM-based turn intent judging.
///
/// Lives in `services` so any caller (runtime / cli / harness) can hold an
/// `Arc<dyn TurnIntentJudge>` and inject a concrete implementation without
/// pulling in HTTP-client transitive dependencies.
#[async_trait]
pub trait TurnIntentJudge: Send + Sync {
    /// Judge the user's current turn.
    ///
    /// Implementations MUST honor a reasonable timeout internally — the
    /// agentic loop awaits this call before each turn, so blocking
    /// indefinitely freezes the user's session.
    async fn judge(&self, ctx: &TurnIntentJudgeContext)
    -> Result<TurnIntent, TurnIntentJudgeError>;
}

// ─── Prompt construction ────────────────────────────────────────────────────

/// Build the LLM prompt for turn intent judging.
///
/// Pure function — no IO, no allocations beyond the returned String. The
/// prompt is intentionally short so it caches well and adds minimal
/// per-turn latency.
#[must_use]
pub fn build_turn_intent_prompt(ctx: &TurnIntentJudgeContext) -> String {
    let mut prompt = String::with_capacity(1024);

    prompt.push_str(
        "You are a turn intent classifier for an agentic coding assistant.\n\
         Classify what the user wants from this turn into a structured JSON object.\n\
         \n\
         Output ONLY a JSON object with these fields:\n\
         {\n  \
            \"requested_scenario\": <scenario | null>,\n  \
            \"prohibited_scenarios\": [<scenario>, ...],   // may be empty\n  \
            \"continuation_mode\": \"continue_current_objective\" | \"new_objective\" | \"unknown\"\n\
         }\n\
         \n\
         scenario must be one of:\n\
           code_review, debugging, exploration, planning, implementation,\n\
           refactoring, testing, documentation, dev_ops, learning, quick_answer\n\
         \n\
         Rules:\n\
         - requested_scenario: pick the BEST single scenario the user is asking for, or null when ambiguous.\n\
         - prohibited_scenarios: include any scenario the user explicitly rejected (e.g. \"don't review, just continue\" → [\"code_review\"]).\n\
         - continuation_mode:\n  \
             * \"continue_current_objective\" only when the user explicitly asks to proceed, apply, fix, run, or continue prior work (e.g. \"go on\", \"fix it\", \"继续\").\n  \
             * \"new_objective\" when starting an unrelated task.\n  \
             * \"unknown\" for status/progress/why/what-remains questions unless the user also explicitly asks to execute more work.\n\
         - Return ONLY the JSON. No prose, no markdown fences.\n\
         \n\
         Examples:\n\
         User: \"please inspect the current changes\"\n\
         {\"requested_scenario\":\"code_review\",\"prohibited_scenarios\":[],\"continuation_mode\":\"unknown\"}\n\
         \n\
         User: \"fix it\" (after assistant proposed an implementation)\n\
         {\"requested_scenario\":null,\"prohibited_scenarios\":[],\"continuation_mode\":\"continue_current_objective\"}\n\
         \n\
         User: \"还有什么？\" (after assistant was working on a task)\n\
         {\"requested_scenario\":\"quick_answer\",\"prohibited_scenarios\":[],\"continuation_mode\":\"unknown\"}\n\
         \n\
         User: \"don't review this, just continue the implementation\"\n\
         {\"requested_scenario\":\"implementation\",\"prohibited_scenarios\":[\"code_review\"],\"continuation_mode\":\"continue_current_objective\"}\n\
         \n",
    );

    prompt.push_str(&format!("Turn: {}\n", ctx.turn_count));
    if ctx.has_prior_assistant_turn {
        prompt.push_str("Has prior assistant turn: yes\n");
    } else {
        prompt.push_str("Has prior assistant turn: no\n");
    }
    if !ctx.recent_tools.is_empty() {
        // Cap the recent tools list so a runaway tool chain cannot bloat the prompt.
        let preview: Vec<&str> = ctx
            .recent_tools
            .iter()
            .take(8)
            .map(String::as_str)
            .collect();
        prompt.push_str(&format!("Recent tools: {}\n", preview.join(", ")));
    }

    // Sanitize: strip backticks from the user message so a `User: "..."` block
    // can't be turned into a markdown fence by adversarial input. The judge
    // treats the message as data, not code, so this preserves intent while
    // closing a trivial prompt-injection vector.
    let sanitized = ctx.message.replace('`', "'");
    prompt.push_str(&format!("\nUser: \"{}\"\n", sanitized));

    prompt
}

// ─── Response parser ────────────────────────────────────────────────────────

/// Parse the judge's JSON response into a [`TurnIntent`].
///
/// Strict: an unknown scenario or continuation_mode value produces `Err` so
/// the caller can ignore the explicit intent instead of silently constructing
/// a degraded one.
pub fn parse_turn_intent_response(raw: &str) -> Result<TurnIntent, TurnIntentJudgeError> {
    let trimmed = raw.trim();

    // Strip any markdown fence wrapping. Models occasionally hedge despite
    // the prompt explicitly forbidding it.
    let unfenced = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .map(|s| s.strip_suffix("```").unwrap_or(s))
        .unwrap_or(trimmed)
        .trim();

    // Some models emit prose around the JSON. Try a substring extraction as
    // a fallback before giving up.
    let json_text = if let Ok(value) = serde_json::from_str::<serde_json::Value>(unfenced) {
        Ok(value)
    } else if let (Some(start), Some(end)) = (unfenced.find('{'), unfenced.rfind('}'))
        && start < end
    {
        serde_json::from_str::<serde_json::Value>(&unfenced[start..=end])
    } else {
        Err(serde_json::from_str::<serde_json::Value>(unfenced).unwrap_err())
    };

    let value = json_text.map_err(|_| TurnIntentJudgeError::Malformed {
        raw: truncate(raw, 256),
    })?;

    let mut intent = TurnIntent::default();

    // requested_scenario: optional, must parse if present.
    if let Some(req) = value.get("requested_scenario")
        && !req.is_null()
    {
        let s = req
            .as_str()
            .ok_or_else(|| TurnIntentJudgeError::Malformed {
                raw: format!("requested_scenario not a string: {req}"),
            })?;
        let scenario = parse_scenario(s).ok_or_else(|| TurnIntentJudgeError::Malformed {
            raw: format!("unknown scenario: '{s}'"),
        })?;
        intent.requested_scenario = Some(scenario);
    }

    // prohibited_scenarios: optional array.
    if let Some(prohibited) = value.get("prohibited_scenarios")
        && !prohibited.is_null()
    {
        let arr = prohibited
            .as_array()
            .ok_or_else(|| TurnIntentJudgeError::Malformed {
                raw: format!("prohibited_scenarios not an array: {prohibited}"),
            })?;
        for entry in arr {
            let s = entry
                .as_str()
                .ok_or_else(|| TurnIntentJudgeError::Malformed {
                    raw: format!("prohibited_scenarios entry not a string: {entry}"),
                })?;
            let scenario = parse_scenario(s).ok_or_else(|| TurnIntentJudgeError::Malformed {
                raw: format!("unknown prohibited scenario: '{s}'"),
            })?;
            if !intent.prohibited_scenarios.contains(&scenario) {
                intent.prohibited_scenarios.push(scenario);
            }
        }
    }

    // continuation_mode: optional, defaults to Unknown when absent.
    if let Some(mode) = value.get("continuation_mode")
        && !mode.is_null()
    {
        let s = mode
            .as_str()
            .ok_or_else(|| TurnIntentJudgeError::Malformed {
                raw: format!("continuation_mode not a string: {mode}"),
            })?;
        intent.continuation_mode =
            parse_continuation_mode(s).ok_or_else(|| TurnIntentJudgeError::Malformed {
                raw: format!("unknown continuation_mode: '{s}'"),
            })?;
    }

    Ok(intent)
}

fn parse_scenario(s: &str) -> Option<Scenario> {
    match s.trim().to_lowercase().as_str() {
        "code_review" | "review" => Some(Scenario::CodeReview),
        "debugging" | "debug" => Some(Scenario::Debugging),
        "exploration" | "explore" => Some(Scenario::Exploration),
        "planning" | "plan" => Some(Scenario::Planning),
        "implementation" | "implement" | "impl" => Some(Scenario::Implementation),
        "refactoring" | "refactor" => Some(Scenario::Refactoring),
        "testing" | "test" => Some(Scenario::Testing),
        "documentation" | "docs" | "doc" => Some(Scenario::Documentation),
        "dev_ops" | "devops" => Some(Scenario::DevOps),
        "learning" | "learn" => Some(Scenario::Learning),
        "quick_answer" | "quickanswer" | "quick" => Some(Scenario::QuickAnswer),
        _ => None,
    }
}

fn parse_continuation_mode(s: &str) -> Option<TurnContinuationMode> {
    match s.trim().to_lowercase().as_str() {
        "continue_current_objective" | "continue" => {
            Some(TurnContinuationMode::ContinueCurrentObjective)
        }
        "new_objective" | "new" => Some(TurnContinuationMode::NewObjective),
        "unknown" => Some(TurnContinuationMode::Unknown),
        _ => None,
    }
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max_chars).collect();
        out.push_str("...");
        out
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_includes_message_and_turn() {
        let ctx = TurnIntentJudgeContext {
            message: "please inspect the current changes".into(),
            turn_count: 3,
            recent_tools: vec!["read_file".into(), "bash".into()],
            has_prior_assistant_turn: true,
        };
        let prompt = build_turn_intent_prompt(&ctx);
        assert!(prompt.contains("please inspect the current changes"));
        assert!(prompt.contains("Turn: 3"));
        assert!(prompt.contains("Has prior assistant turn: yes"));
        assert!(prompt.contains("read_file, bash"));
    }

    #[test]
    fn prompt_strips_backticks_in_user_message() {
        let ctx = TurnIntentJudgeContext {
            message: "run `rm -rf /` then ```fix``` it".into(),
            turn_count: 1,
            recent_tools: vec![],
            has_prior_assistant_turn: false,
        };
        let prompt = build_turn_intent_prompt(&ctx);
        assert!(
            !prompt.contains('`'),
            "prompt must not contain backticks from user input: {prompt}"
        );
        assert!(prompt.contains("run 'rm -rf /' then '''fix''' it"));
    }

    #[test]
    fn prompt_caps_recent_tools_to_eight() {
        let ctx = TurnIntentJudgeContext {
            message: "hi".into(),
            turn_count: 1,
            recent_tools: (0..16).map(|i| format!("tool_{i}")).collect(),
            has_prior_assistant_turn: false,
        };
        let prompt = build_turn_intent_prompt(&ctx);
        assert!(prompt.contains("tool_0"));
        assert!(prompt.contains("tool_7"));
        assert!(
            !prompt.contains("tool_8"),
            "recent tools must be capped at 8 entries: {prompt}"
        );
    }

    #[test]
    fn parses_clean_json() {
        let raw = r#"{"requested_scenario":"code_review","prohibited_scenarios":[],"continuation_mode":"unknown"}"#;
        let intent = parse_turn_intent_response(raw).unwrap();
        assert_eq!(intent.requested_scenario, Some(Scenario::CodeReview));
        assert!(intent.prohibited_scenarios.is_empty());
        assert_eq!(intent.continuation_mode, TurnContinuationMode::Unknown);
    }

    #[test]
    fn parses_continuation_with_prohibition() {
        let raw = r#"{
          "requested_scenario": "implementation",
          "prohibited_scenarios": ["code_review"],
          "continuation_mode": "continue_current_objective"
        }"#;
        let intent = parse_turn_intent_response(raw).unwrap();
        assert_eq!(intent.requested_scenario, Some(Scenario::Implementation));
        assert_eq!(intent.prohibited_scenarios, vec![Scenario::CodeReview]);
        assert_eq!(
            intent.continuation_mode,
            TurnContinuationMode::ContinueCurrentObjective
        );
    }

    #[test]
    fn parses_null_requested_scenario_as_none() {
        let raw = r#"{"requested_scenario":null,"prohibited_scenarios":[],"continuation_mode":"continue_current_objective"}"#;
        let intent = parse_turn_intent_response(raw).unwrap();
        assert_eq!(intent.requested_scenario, None);
    }

    #[test]
    fn parses_markdown_fenced_response() {
        let raw = "```json\n{\"requested_scenario\":\"debugging\",\"prohibited_scenarios\":[],\"continuation_mode\":\"unknown\"}\n```";
        let intent = parse_turn_intent_response(raw).unwrap();
        assert_eq!(intent.requested_scenario, Some(Scenario::Debugging));
    }

    #[test]
    fn parses_response_with_surrounding_prose() {
        let raw = "Here is the classification:\n{\"requested_scenario\":\"quick_answer\",\"prohibited_scenarios\":[],\"continuation_mode\":\"unknown\"}\nLet me know if you need more.";
        let intent = parse_turn_intent_response(raw).unwrap();
        assert_eq!(intent.requested_scenario, Some(Scenario::QuickAnswer));
    }

    #[test]
    fn unknown_scenario_returns_malformed() {
        let raw = r#"{"requested_scenario":"mystery","prohibited_scenarios":[],"continuation_mode":"unknown"}"#;
        let err = parse_turn_intent_response(raw).unwrap_err();
        assert!(matches!(err, TurnIntentJudgeError::Malformed { .. }));
    }

    #[test]
    fn unknown_continuation_mode_returns_malformed() {
        let raw = r#"{"requested_scenario":null,"prohibited_scenarios":[],"continuation_mode":"sometimes"}"#;
        let err = parse_turn_intent_response(raw).unwrap_err();
        assert!(matches!(err, TurnIntentJudgeError::Malformed { .. }));
    }

    #[test]
    fn malformed_json_returns_malformed_error() {
        let err = parse_turn_intent_response("not json at all").unwrap_err();
        assert!(matches!(err, TurnIntentJudgeError::Malformed { .. }));
    }

    #[test]
    fn malformed_unicode_response_is_truncated_without_panicking() {
        let raw = "坏".repeat(100);
        let err = parse_turn_intent_response(&raw).unwrap_err();
        match err {
            TurnIntentJudgeError::Malformed { raw } => {
                assert_eq!(raw, "坏".repeat(100));
            }
            other => panic!("expected malformed, got {other:?}"),
        }

        let raw = "坏".repeat(300);
        let err = parse_turn_intent_response(&raw).unwrap_err();
        match err {
            TurnIntentJudgeError::Malformed { raw } => {
                assert!(raw.ends_with("..."));
                assert_eq!(raw.trim_end_matches("...").chars().count(), 256);
            }
            other => panic!("expected malformed, got {other:?}"),
        }
    }

    #[test]
    fn missing_fields_default_to_empty_intent() {
        // All fields are optional — an empty JSON object yields an empty TurnIntent.
        let intent = parse_turn_intent_response("{}").unwrap();
        assert!(intent.requested_scenario.is_none());
        assert!(intent.prohibited_scenarios.is_empty());
        assert_eq!(intent.continuation_mode, TurnContinuationMode::Unknown);
    }

    #[test]
    fn scenario_aliases_are_accepted() {
        // The judge prompt names canonical forms but real LLMs paraphrase.
        // Accept common synonyms to reduce parser failures.
        let cases = [
            ("review", Scenario::CodeReview),
            ("debug", Scenario::Debugging),
            ("explore", Scenario::Exploration),
            ("plan", Scenario::Planning),
            ("implement", Scenario::Implementation),
            ("refactor", Scenario::Refactoring),
            ("test", Scenario::Testing),
            ("docs", Scenario::Documentation),
            ("devops", Scenario::DevOps),
            ("learn", Scenario::Learning),
            ("quick", Scenario::QuickAnswer),
        ];
        for (alias, expected) in cases {
            let raw = format!(
                r#"{{"requested_scenario":"{alias}","prohibited_scenarios":[],"continuation_mode":"unknown"}}"#
            );
            let intent = parse_turn_intent_response(&raw)
                .unwrap_or_else(|e| panic!("alias {alias} failed: {e:?}"));
            assert_eq!(
                intent.requested_scenario,
                Some(expected),
                "alias '{alias}' must map to {expected:?}"
            );
        }
    }
}

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

use astra_config::user_profile::TurnIntent;
use async_trait::async_trait;
use serde_json::{Value, json};

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
            \"prohibited_scenarios\": [<scenario>, ...],\n  \
            \"objective_relation\": \"acknowledge\" | \"continue\" | \"refine\" | \"correct\" | \"replace\" | \"unknown\",\n  \
            \"feedback\": null | {\"kind\": \"approval\" | \"correction\" | \"clarification\" | \"requirement\" | \"preference\", \"target\": \"objective\" | \"scope\" | \"approach\" | \"output\" | \"verification\" | \"general\"},\n  \
            \"workspace_mutation\": \"read_only\" | \"may_mutate\" | \"must_mutate\" | \"unknown\",\n  \
            \"browser_verification_required\": <boolean>\n\
         }\n\
         \n\
         scenario must be one of:\n\
           code_review, debugging, exploration, planning, implementation,\n\
           refactoring, testing, documentation, dev_ops, learning, quick_answer,\n\
           benchmark_comparison\n\
         \n\
         Rules:\n\
         - requested_scenario: pick the BEST single scenario the user is asking for, or null when ambiguous.\n\
         - prohibited_scenarios: include any scenario the user explicitly rejected (e.g. \"don't review, just continue\" → [\"code_review\"]).\n\
         - objective_relation describes how this exact message changes the existing objective:\n  \
             * \"acknowledge\": accepts the prior result without changing work.\n  \
             * \"continue\": asks to proceed without changing objective or requirements.\n  \
             * \"refine\": adds a requirement, constraint, scope item, or verification request while retaining the objective.\n  \
             * \"correct\": says the current understanding or approach is wrong and supplies a correction.\n  \
             * \"replace\": starts an unrelated objective, or explicitly supersedes the old objective. Use this for the first substantive user task when no prior assistant turn exists.\n  \
             * \"unknown\": no reliable relationship, including status/why questions that do not change work.\n\
         - feedback is null when the message contains no evaluation or requirement about prior/current work. Otherwise classify its semantic kind and target. Do not copy or summarize the user text into this object; the canonical message already preserves it.\n\
         - workspace_mutation:\n  \
             * \"must_mutate\" only when the user explicitly requests editing, creating, deleting, applying, refactoring, fixing, or otherwise changing files/workspace state in this turn.\n  \
             * \"read_only\" when the user asks for analysis, explanation, diagnosis, review, lookup, or an answer without asking to change files.\n  \
             * \"may_mutate\" when investigation could lead to edits but the user has not required edits yet.\n  \
             * \"unknown\" when the current message is too ambiguous to decide.\n\
         - browser_verification_required: true only when the user explicitly asks for browser/UI verification, browser testing, screenshots, DOM/canvas/page inspection, or equivalent browser-capable validation.\n\
         - Return ONLY the JSON. No prose, no markdown fences.\n\
         \n\
         Examples:\n\
         User: \"please inspect the current changes\"\n\
         {\"requested_scenario\":\"code_review\",\"prohibited_scenarios\":[],\"objective_relation\":\"replace\",\"feedback\":null,\"workspace_mutation\":\"read_only\",\"browser_verification_required\":false}\n\
         \n\
         User: \"fix it\" (after assistant proposed an implementation)\n\
         {\"requested_scenario\":null,\"prohibited_scenarios\":[],\"objective_relation\":\"continue\",\"feedback\":null,\"workspace_mutation\":\"must_mutate\",\"browser_verification_required\":false}\n\
         \n\
         User: \"还有什么？\" (after assistant was working on a task)\n\
         {\"requested_scenario\":\"quick_answer\",\"prohibited_scenarios\":[],\"objective_relation\":\"unknown\",\"feedback\":null,\"workspace_mutation\":\"read_only\",\"browser_verification_required\":false}\n\
         \n\
         User: \"当前的实现，能够想起来吗？\"\n\
         {\"requested_scenario\":\"quick_answer\",\"prohibited_scenarios\":[],\"objective_relation\":\"unknown\",\"feedback\":null,\"workspace_mutation\":\"read_only\",\"browser_verification_required\":false}\n\
         \n\
         User: \"不对，我要的是系统性修复，不是临时补丁\"\n\
         {\"requested_scenario\":\"refactoring\",\"prohibited_scenarios\":[],\"objective_relation\":\"correct\",\"feedback\":{\"kind\":\"correction\",\"target\":\"approach\"},\"workspace_mutation\":\"must_mutate\",\"browser_verification_required\":false}\n\
         \n\
         User: \"don't review this, just continue the implementation\"\n\
         {\"requested_scenario\":\"implementation\",\"prohibited_scenarios\":[\"code_review\"],\"objective_relation\":\"refine\",\"feedback\":{\"kind\":\"requirement\",\"target\":\"approach\"},\"workspace_mutation\":\"must_mutate\",\"browser_verification_required\":false}\n\
         \n\
         User: \"test the game in a browser and verify the canvas works\"\n\
         {\"requested_scenario\":\"testing\",\"prohibited_scenarios\":[],\"objective_relation\":\"replace\",\"feedback\":null,\"workspace_mutation\":\"read_only\",\"browser_verification_required\":true}\n\
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

    // Serialize instead of interpolating into a hand-written quoted string.
    // This preserves the exact instruction while giving quotes, newlines, and
    // delimiter-like text one unambiguous data representation.
    let encoded_message = serde_json::Value::String(ctx.message.clone()).to_string();
    prompt.push_str("\nUser message JSON: ");
    prompt.push_str(&encoded_message);
    prompt.push('\n');

    prompt
}

/// Build the chat messages sent to the turn-intent judge.
///
/// Keep this centralized so CLI/server judge implementations cannot drift in
/// system wording, prompt shape, or output contract.
#[must_use]
pub fn turn_intent_judge_messages(ctx: &TurnIntentJudgeContext) -> Vec<Value> {
    vec![
        json!({
            "role": "system",
            "content": "You output ONLY a JSON object as described in the user message. No prose. No markdown fences."
        }),
        json!({
            "role": "user",
            "content": build_turn_intent_prompt(ctx),
        }),
    ]
}

// ─── Response parser ────────────────────────────────────────────────────────

/// Parse the judge's JSON response into a [`TurnIntent`].
///
/// Strict: unknown fields or enum values produce `Err` so callers cannot
/// silently construct a degraded intent from an obsolete schema.
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

    serde_json::from_value(value).map_err(|error| TurnIntentJudgeError::Malformed {
        raw: truncate(&format!("{error}: {raw}"), 256),
    })
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
    use astra_config::user_profile::{Scenario, WorkspaceMutationIntent};
    use astra_turn_types::{ObjectiveRelation, UserFeedback, UserFeedbackKind, UserFeedbackTarget};

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
        assert!(prompt.contains("\"objective_relation\""));
        assert!(prompt.contains("\"feedback\""));
        assert!(prompt.contains("\"workspace_mutation\""));
        assert!(prompt.contains("\"browser_verification_required\": <boolean>"));
        assert!(prompt.contains("without changing objective or requirements"));
        assert!(prompt.contains("当前的实现，能够想起来吗？"));
        assert!(prompt.contains("benchmark_comparison"));
    }

    #[test]
    fn prompt_json_encodes_user_message_without_mutating_it() {
        let ctx = TurnIntentJudgeContext {
            message: "quote: \"x\"\nrun `literal`".into(),
            turn_count: 1,
            recent_tools: vec![],
            has_prior_assistant_turn: false,
        };
        let prompt = build_turn_intent_prompt(&ctx);
        assert!(prompt.contains(r#"User message JSON: "quote: \"x\"\nrun `literal`""#));
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
        let raw = r#"{"requested_scenario":"code_review","prohibited_scenarios":[],"objective_relation":"replace"}"#;
        let intent = parse_turn_intent_response(raw).unwrap();
        assert_eq!(intent.requested_scenario, Some(Scenario::CodeReview));
        assert!(intent.prohibited_scenarios.is_empty());
        assert_eq!(intent.objective_relation, ObjectiveRelation::Replace);
    }

    #[test]
    fn parses_refinement_with_prohibition_and_feedback() {
        let raw = r#"{
          "requested_scenario": "implementation",
          "prohibited_scenarios": ["code_review"],
          "objective_relation": "refine",
          "feedback": {"kind": "requirement", "target": "approach"}
        }"#;
        let intent = parse_turn_intent_response(raw).unwrap();
        assert_eq!(intent.requested_scenario, Some(Scenario::Implementation));
        assert_eq!(intent.prohibited_scenarios, vec![Scenario::CodeReview]);
        assert_eq!(intent.objective_relation, ObjectiveRelation::Refine);
        assert!(!intent.reanchors_current_objective());
        assert_eq!(
            intent.feedback,
            Some(UserFeedback {
                kind: UserFeedbackKind::Requirement,
                target: UserFeedbackTarget::Approach,
            })
        );
        assert_eq!(
            intent.workspace_mutation,
            WorkspaceMutationIntent::Unknown,
            "missing workspace_mutation must fail closed"
        );
        assert!(!intent.browser_verification_required);
    }

    #[test]
    fn parses_benchmark_comparison_scenario() {
        let raw = r#"{"requested_scenario":"benchmark_comparison","prohibited_scenarios":[],"objective_relation":"replace"}"#;
        let intent = parse_turn_intent_response(raw).unwrap();
        assert_eq!(
            intent.requested_scenario,
            Some(Scenario::BenchmarkComparison)
        );
    }

    #[test]
    fn parses_structured_correction_as_one_relation() {
        let raw = r#"{
          "requested_scenario": "refactoring",
          "prohibited_scenarios": [],
          "objective_relation": "correct",
          "feedback": {"kind": "correction", "target": "approach"}
        }"#;
        let intent = parse_turn_intent_response(raw).unwrap();
        assert_eq!(intent.requested_scenario, Some(Scenario::Refactoring));
        assert_eq!(intent.objective_relation, ObjectiveRelation::Correct);
        assert!(intent.reanchors_current_objective());
    }

    #[test]
    fn parses_null_requested_scenario_as_none() {
        let raw = r#"{"requested_scenario":null,"prohibited_scenarios":[],"objective_relation":"continue"}"#;
        let intent = parse_turn_intent_response(raw).unwrap();
        assert_eq!(intent.requested_scenario, None);
    }

    #[test]
    fn parses_markdown_fenced_response() {
        let raw = "```json\n{\"requested_scenario\":\"debugging\",\"prohibited_scenarios\":[],\"objective_relation\":\"replace\"}\n```";
        let intent = parse_turn_intent_response(raw).unwrap();
        assert_eq!(intent.requested_scenario, Some(Scenario::Debugging));
    }

    #[test]
    fn parses_response_with_surrounding_prose() {
        let raw = "Here is the classification:\n{\"requested_scenario\":\"quick_answer\",\"prohibited_scenarios\":[],\"objective_relation\":\"unknown\"}\nLet me know if you need more.";
        let intent = parse_turn_intent_response(raw).unwrap();
        assert_eq!(intent.requested_scenario, Some(Scenario::QuickAnswer));
    }

    #[test]
    fn unknown_scenario_returns_malformed() {
        let raw = r#"{"requested_scenario":"mystery","prohibited_scenarios":[],"objective_relation":"unknown"}"#;
        let err = parse_turn_intent_response(raw).unwrap_err();
        assert!(matches!(err, TurnIntentJudgeError::Malformed { .. }));
    }

    #[test]
    fn unknown_objective_relation_returns_malformed() {
        let raw = r#"{"requested_scenario":null,"prohibited_scenarios":[],"objective_relation":"sometimes"}"#;
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
    fn missing_objective_relation_is_malformed() {
        let err = parse_turn_intent_response("{}").unwrap_err();
        assert!(matches!(err, TurnIntentJudgeError::Malformed { .. }));
    }

    #[test]
    fn malformed_feedback_returns_malformed() {
        let raw = r#"{"objective_relation":"correct","feedback":{"kind":"correction","target":"unknown_target"}}"#;
        let err = parse_turn_intent_response(raw).unwrap_err();
        assert!(matches!(err, TurnIntentJudgeError::Malformed { .. }));
    }

    #[test]
    fn parses_workspace_mutation_and_browser_requirement() {
        let raw = r#"{
          "requested_scenario": "testing",
          "prohibited_scenarios": [],
          "objective_relation": "replace",
          "workspace_mutation": "read_only",
          "browser_verification_required": true
        }"#;
        let intent = parse_turn_intent_response(raw).unwrap();
        assert_eq!(intent.requested_scenario, Some(Scenario::Testing));
        assert_eq!(intent.workspace_mutation, WorkspaceMutationIntent::ReadOnly);
        assert!(intent.browser_verification_required);
    }

    #[test]
    fn unknown_workspace_mutation_returns_malformed() {
        let raw = r#"{"objective_relation":"unknown","workspace_mutation":"sometimes"}"#;
        let err = parse_turn_intent_response(raw).unwrap_err();
        assert!(matches!(err, TurnIntentJudgeError::Malformed { .. }));
    }

    #[test]
    fn non_boolean_browser_requirement_returns_malformed() {
        let raw = r#"{"objective_relation":"unknown","browser_verification_required":"yes"}"#;
        let err = parse_turn_intent_response(raw).unwrap_err();
        assert!(matches!(err, TurnIntentJudgeError::Malformed { .. }));
    }

    #[test]
    fn schema_rejects_scenario_aliases() {
        for alias in ["review", "debug", "impl", "quick"] {
            let raw = format!(
                r#"{{"requested_scenario":"{alias}","prohibited_scenarios":[],"objective_relation":"unknown"}}"#
            );
            assert!(
                matches!(
                    parse_turn_intent_response(&raw),
                    Err(TurnIntentJudgeError::Malformed { .. })
                ),
                "non-schema alias {alias:?} must not be normalized"
            );
        }
    }
}

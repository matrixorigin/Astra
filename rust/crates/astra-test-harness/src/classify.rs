//! Failure classification for failed test cases.
//!
//! After a case fails, [`classify`] inspects the outcome and criteria
//! results to bucket the failure into an actionable category. This
//! drives retry logic and report grouping.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::criteria::{Criterion, CriterionResult};
use crate::runner::RunOutcome;

/// Failure category for a failed case run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FailureClass {
    InfraAuth,
    InfraTimeout,
    InfraProviderError { provider: String },
    InfraRateLimit,
    ModelInstructionFollowing,
    ModelCapability,
    Unknown,
}

impl fmt::Display for FailureClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InfraAuth => write!(f, "InfraAuth"),
            Self::InfraTimeout => write!(f, "InfraTimeout"),
            Self::InfraProviderError { provider } => write!(f, "InfraProviderError({provider})"),
            Self::InfraRateLimit => write!(f, "InfraRateLimit"),
            Self::ModelInstructionFollowing => write!(f, "ModelInstructionFollowing"),
            Self::ModelCapability => write!(f, "ModelCapability"),
            Self::Unknown => write!(f, "Unknown"),
        }
    }
}

/// Classify a failure based on outcome signals and criteria results.
pub fn classify(outcome: &RunOutcome, criteria_results: &[CriterionResult]) -> FailureClass {
    // Exit code 124 = timeout (POSIX convention).
    if outcome.exit_code == 124 {
        return FailureClass::InfraTimeout;
    }

    let stderr = &outcome.stderr;

    if stderr.contains("Could not validate credentials") {
        return FailureClass::InfraAuth;
    }

    if stderr.contains("429")
        || stderr.contains("rate limit")
        || stderr.contains("Too many requests")
    {
        return FailureClass::InfraRateLimit;
    }

    if stderr.contains("HTTP 400")
        || stderr.contains("HTTP 500")
        || stderr.contains("HTTP 502")
        || stderr.contains("HTTP 503")
    {
        let provider = extract_provider(&outcome.model, stderr);
        return FailureClass::InfraProviderError { provider };
    }

    // astra exits 3 on auth failure with empty response.
    if outcome.exit_code == 3 && outcome.text.is_empty() {
        return FailureClass::InfraAuth;
    }

    // Criterion-based classification.
    let tools_count_failed = criteria_results
        .iter()
        .any(|r| !r.passed && matches!(r.criterion, Criterion::ToolsCountBetween { .. }));
    let text_contains_passed = criteria_results
        .iter()
        .any(|r| r.passed && matches!(r.criterion, Criterion::TextContains { .. }));

    if tools_count_failed {
        // Check if tool_calls_count exceeds the max bound.
        let exceeded_max = criteria_results.iter().any(|r| {
            if let Criterion::ToolsCountBetween { max, .. } = &r.criterion {
                !r.passed && outcome.tool_calls_count > *max
            } else {
                false
            }
        });
        if exceeded_max {
            return FailureClass::ModelInstructionFollowing;
        }
        if text_contains_passed {
            return FailureClass::ModelInstructionFollowing;
        }
    }

    // All deterministic criteria failed → model can't do the task.
    let deterministic_results: Vec<_> = criteria_results
        .iter()
        .filter(|r| is_deterministic(&r.criterion))
        .collect();
    if !deterministic_results.is_empty() && deterministic_results.iter().all(|r| !r.passed) {
        return FailureClass::ModelCapability;
    }

    FailureClass::Unknown
}

/// Returns a one-line suggested action for the failure class.
pub fn suggested_action(class: &FailureClass) -> &'static str {
    match class {
        FailureClass::InfraAuth => "Check credentials: run `astra-admin login` or verify API keys",
        FailureClass::InfraTimeout => "Increase timeout or check network connectivity to provider",
        FailureClass::InfraProviderError { .. } => {
            "Provider returned an error; check provider status page or retry later"
        }
        FailureClass::InfraRateLimit => "Back off and retry, or request a rate-limit increase",
        FailureClass::ModelInstructionFollowing => {
            "Model ignored constraints; consider a stronger model or refine the prompt"
        }
        FailureClass::ModelCapability => {
            "Model lacks capability for this task; try a more capable model"
        }
        FailureClass::Unknown => "Inspect stderr and session journal for clues",
    }
}

fn extract_provider(model: &str, stderr: &str) -> String {
    // Try model name with slash separator (e.g. "anthropic/claude-3").
    if let Some(slash) = model.find('/') {
        return model[..slash].to_string();
    }
    // Try dot-separated prefix (e.g. "us.anthropic.claude-sonnet-4-6").
    if let Some(dot) = model.find('.') {
        let prefix = &model[..dot];
        // Skip region-like prefixes (us, eu, ap) and take the next segment.
        if matches!(prefix, "us" | "eu" | "ap")
            && let Some(rest) = model.get(dot + 1..)
        {
            if let Some(dot2) = rest.find('.') {
                return rest[..dot2].to_string();
            }
            return rest.to_string();
        }
        return prefix.to_string();
    }
    // Heuristic: look for known provider names in stderr or model name.
    let haystack = format!("{} {}", model.to_lowercase(), stderr.to_lowercase());
    for name in [
        "anthropic",
        "openai",
        "bedrock",
        "google",
        "azure",
        "minimax",
        "moonshot",
        "dashscope",
    ] {
        if haystack.contains(name) {
            return name.to_string();
        }
    }
    "unknown".to_string()
}

fn is_deterministic(criterion: &Criterion) -> bool {
    matches!(
        criterion,
        Criterion::ToolCalled { .. }
            | Criterion::ExitCode { .. }
            | Criterion::ToolsCountBetween { .. }
            | Criterion::StderrMatches { .. }
            | Criterion::TextContains { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::criteria::Criterion;

    fn make_outcome() -> RunOutcome {
        RunOutcome::new("test-model")
    }

    fn cr(criterion: Criterion, passed: bool) -> CriterionResult {
        CriterionResult {
            criterion,
            passed,
            detail: String::new(),
            full_detail: None,
            score: None,
        }
    }

    #[test]
    fn timeout_classification() {
        let outcome = make_outcome().with_exit_code(124);
        assert_eq!(classify(&outcome, &[]), FailureClass::InfraTimeout);
    }

    #[test]
    fn auth_from_stderr() {
        let outcome = make_outcome().with_stderr("Could not validate credentials");
        assert_eq!(classify(&outcome, &[]), FailureClass::InfraAuth);
    }

    #[test]
    fn rate_limit_429() {
        let outcome = make_outcome().with_stderr("error: 429 Too many requests");
        assert_eq!(classify(&outcome, &[]), FailureClass::InfraRateLimit);
    }

    #[test]
    fn provider_error_extracts_provider() {
        let outcome =
            RunOutcome::new("anthropic/claude-3").with_stderr("HTTP 500 internal server error");
        let class = classify(&outcome, &[]);
        assert_eq!(
            class,
            FailureClass::InfraProviderError {
                provider: "anthropic".to_string()
            }
        );
    }

    #[test]
    fn provider_error_extracts_from_dot_separated_model() {
        let outcome =
            RunOutcome::new("us.anthropic.claude-sonnet-4-6").with_stderr("HTTP 400 bad request");
        let class = classify(&outcome, &[]);
        assert_eq!(
            class,
            FailureClass::InfraProviderError {
                provider: "anthropic".to_string()
            }
        );
    }

    #[test]
    fn exit_3_empty_text_is_auth() {
        let outcome = make_outcome().with_exit_code(3);
        assert_eq!(classify(&outcome, &[]), FailureClass::InfraAuth);
    }

    #[test]
    fn instruction_following_when_tools_exceed_max() {
        let outcome = make_outcome().with_tools_used(vec!["a".into(), "b".into(), "c".into()]);
        let results = vec![cr(Criterion::ToolsCountBetween { min: 1, max: 2 }, false)];
        assert_eq!(
            classify(&outcome, &results),
            FailureClass::ModelInstructionFollowing
        );
    }

    #[test]
    fn model_capability_when_all_deterministic_fail() {
        let results = vec![
            cr(Criterion::ToolCalled { name: "x".into() }, false),
            cr(Criterion::TextContains { needle: "y".into() }, false),
        ];
        assert_eq!(
            classify(&make_outcome(), &results),
            FailureClass::ModelCapability
        );
    }

    #[test]
    fn unknown_fallback() {
        let outcome = make_outcome()
            .with_exit_code(1)
            .with_stderr("something weird");
        assert_eq!(classify(&outcome, &[]), FailureClass::Unknown);
    }

    #[test]
    fn suggested_action_not_empty() {
        let classes = [
            FailureClass::InfraAuth,
            FailureClass::InfraTimeout,
            FailureClass::InfraRateLimit,
            FailureClass::Unknown,
        ];
        for class in &classes {
            assert!(!suggested_action(class).is_empty());
        }
    }
}

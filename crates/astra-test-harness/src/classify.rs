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
    /// The agent process reached the runtime, but a durable control-plane or
    /// persistence dependency failed before a canonical run boundary. This is
    /// distinct from credentials: retrying login cannot repair a blocked
    /// database/ledger admission.
    InfraRuntime,
    InfraQuota,
    InfraTimeout,
    InfraModelInactive,
    InfraProviderError {
        provider: String,
    },
    InfraRateLimit,
    InfraVerificationUnavailable,
    PlatformSetupFailed,
    HarnessCleanupFailed,
    ModelInstructionFollowing,
    ModelCapability,
    ModelQualityLow,
    EfficiencyBoundsExceeded,
    /// The process completed, but a hard typed oracle rejected the observed
    /// behavior. This deliberately does not guess whether the model, runtime,
    /// or their interaction owns the fault; the failed criterion and journal
    /// carry that causal evidence.
    BehaviorContractViolation,
    ToolUnavailable,
    Unknown,
}

impl fmt::Display for FailureClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InfraAuth => write!(f, "InfraAuth"),
            Self::InfraRuntime => write!(f, "InfraRuntime"),
            Self::InfraQuota => write!(f, "InfraQuota"),
            Self::InfraTimeout => write!(f, "InfraTimeout"),
            Self::InfraModelInactive => write!(f, "InfraModelInactive"),
            Self::InfraProviderError { provider } => write!(f, "InfraProviderError({provider})"),
            Self::InfraRateLimit => write!(f, "InfraRateLimit"),
            Self::InfraVerificationUnavailable => write!(f, "InfraVerificationUnavailable"),
            Self::PlatformSetupFailed => write!(f, "PlatformSetupFailed"),
            Self::HarnessCleanupFailed => write!(f, "HarnessCleanupFailed"),
            Self::ModelInstructionFollowing => write!(f, "ModelInstructionFollowing"),
            Self::ModelCapability => write!(f, "ModelCapability"),
            Self::ModelQualityLow => write!(f, "ModelQualityLow"),
            Self::EfficiencyBoundsExceeded => write!(f, "EfficiencyBoundsExceeded"),
            Self::BehaviorContractViolation => write!(f, "BehaviorContractViolation"),
            Self::ToolUnavailable => write!(f, "ToolUnavailable"),
            Self::Unknown => write!(f, "Unknown"),
        }
    }
}

/// Classify a failure based on outcome signals and criteria results.
pub fn classify(outcome: &RunOutcome, criteria_results: &[CriterionResult]) -> FailureClass {
    // Exit 124 only proves that the outer deadline fired. If typed execution
    // evidence already proves model/tool progress, calling it an infrastructure
    // outage hides the product's pacing failure and tells reviewers to paper it
    // over by increasing the timeout. Reserve InfraTimeout for runs that never
    // crossed a model/tool boundary.
    if outcome.exit_code == 124 {
        return if timeout_has_execution_progress(outcome, criteria_results) {
            FailureClass::EfficiencyBoundsExceeded
        } else {
            FailureClass::InfraTimeout
        };
    }

    let stderr = &outcome.stderr;

    if stderr.contains("Could not validate credentials") {
        return FailureClass::InfraAuth;
    }

    let stderr_lower = stderr.to_lowercase();
    if stderr_lower.contains("database_error")
        || stderr_lower.contains("durable inference ledger")
        || stderr_lower.contains("logical_invocation_admission")
        || stderr_lower.contains("database connection")
    {
        return FailureClass::InfraRuntime;
    }
    if stderr_lower.contains("quota exceeded")
        || stderr_lower.contains("daily_sessions")
        || stderr_lower.contains("daily session limit reached")
    {
        return FailureClass::InfraQuota;
    }

    if stderr.contains("is inactive (connectivity failed or disabled)") {
        return FailureClass::InfraModelInactive;
    }

    if outcome_is_rate_limited(outcome) {
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

    // Some provider/auth failures exit 3 without a structured envelope, but
    // an empty response alone is not evidence of credentials. Durable ledger
    // failures also exit 3; classify those above and keep remaining
    // no-evidence process failures honest instead of telling users to log in.
    if outcome.exit_code == 3 && outcome.text.is_empty() {
        if stderr_lower.contains("auth")
            || stderr_lower.contains("credential")
            || stderr_lower.contains("api key")
            || stderr_lower.contains("login")
        {
            return FailureClass::InfraAuth;
        }
        return FailureClass::Unknown;
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

    // Tool unavailable: the agent reported a required tool is missing.
    let tool_missing_signals = [
        "tool available in my current toolset",
        "not available in my current tool",
        "tool is not available",
        "no such tool",
    ];
    let text_lower = outcome.text.to_lowercase();
    // Generic pattern: "don't have a <tool_name> tool available/in my..."
    // Requires "available" or "in my" to avoid false positives like
    // "I don't have a good tool for this task".
    let has_generic_dont_have = text_lower.contains("don't have a")
        && text_lower.contains("tool")
        && (text_lower.contains("available") || text_lower.contains("in my"));
    if has_generic_dont_have || tool_missing_signals.iter().any(|s| text_lower.contains(s)) {
        return FailureClass::ToolUnavailable;
    }

    // All deterministic criteria failed → model can't do the task.
    let deterministic_results: Vec<_> = criteria_results
        .iter()
        .filter(|r| is_deterministic(&r.criterion))
        .collect();
    if !deterministic_results.is_empty() && deterministic_results.iter().all(|r| !r.passed) {
        return FailureClass::ModelCapability;
    }

    // Judger-only failure: deterministic checks passed but judger scored low.
    let judger_failed = criteria_results.iter().any(|r| {
        !r.passed
            && matches!(
                r.criterion,
                crate::criteria::Criterion::Judger { .. }
                    | crate::criteria::Criterion::HardJudger { .. }
            )
    });
    let hard_non_judger_pass = criteria_results
        .iter()
        .filter(|r| {
            r.severity == crate::criteria::CriterionSeverity::Hard
                && !matches!(r.criterion, Criterion::HardJudger { .. })
        })
        .all(|r| r.passed);
    let judger_unavailable = criteria_results.iter().any(|r| {
        !r.passed
            && matches!(r.criterion, Criterion::HardJudger { .. })
            && (r.detail.contains("required judger unavailable")
                || r.detail.starts_with("judger call failed:"))
    });
    if judger_unavailable {
        return FailureClass::InfraVerificationUnavailable;
    }
    if judger_failed && hard_non_judger_pass {
        return FailureClass::ModelQualityLow;
    }

    // Soft criteria failure only (efficiency bounds exceeded).
    let soft_failed = criteria_results
        .iter()
        .any(|r| !r.passed && r.severity == crate::criteria::CriterionSeverity::Soft);
    if soft_failed && hard_non_judger_pass {
        return FailureClass::EfficiencyBoundsExceeded;
    }

    // A successful process with failed typed hard evidence is neither an
    // infrastructure outage nor an unknowable failure. Keep the classification
    // ownership-neutral: a durable oracle can expose model behavior, runtime
    // behavior, or a broken interaction contract, and the evidence should be
    // inspected before assigning blame.
    let hard_contract_failed = criteria_results.iter().any(|result| {
        !result.passed
            && result.severity == crate::criteria::CriterionSeverity::Hard
            && !matches!(result.criterion, Criterion::HardJudger { .. })
    });
    if outcome.exit_code == 0 && hard_contract_failed {
        return FailureClass::BehaviorContractViolation;
    }

    FailureClass::Unknown
}

/// A rate-limit classification requires typed failed terminal evidence.
/// Successful answers may legitimately discuss rate limits and must never be
/// relabelled as infrastructure failures by substring matching.
pub(crate) fn outcome_is_rate_limited(outcome: &RunOutcome) -> bool {
    if outcome.exit_code == 0
        || !matches!(
            outcome.final_state.as_deref(),
            Some("interrupted") | Some("failed")
        )
    {
        return false;
    }
    let diagnostic = outcome.stderr.to_ascii_lowercase();
    let text_signal = diagnostic.contains("too many requests")
        || diagnostic.contains("rate_limit")
        || diagnostic.contains("rate limit")
        || diagnostic.contains("[rate_limit]")
        || (diagnostic.contains("429")
            && (diagnostic.contains("error") || diagnostic.contains("http")));
    let typed_signal = outcome.interruption_kind.as_deref().is_some_and(|kind| {
        let kind = kind.to_ascii_lowercase();
        kind.contains("rate") || kind.contains("quota") || kind.contains("429")
    });
    text_signal && typed_signal
}

/// Returns a one-line suggested action for the failure class.
pub fn suggested_action(class: &FailureClass) -> &'static str {
    match class {
        FailureClass::InfraAuth => "Check credentials: run `astra admin login` or verify API keys",
        FailureClass::InfraRuntime => {
            "Check Astra server/database/ledger health, let pending settlements drain, then re-run"
        }
        FailureClass::InfraQuota => {
            "Use a fresh authorized test account, raise its quota, or wait for the quota window to reset"
        }
        FailureClass::InfraTimeout => "Increase timeout or check network connectivity to provider",
        FailureClass::InfraModelInactive => {
            "Model is inactive on the server; run `astra admin model check <model>` or load an active model"
        }
        FailureClass::InfraProviderError { .. } => {
            "Provider returned an error; check provider status page or retry later"
        }
        FailureClass::InfraRateLimit => "Back off and retry, or request a rate-limit increase",
        FailureClass::InfraVerificationUnavailable => {
            "Required verification was unavailable; restore the judger and re-run before trusting this result"
        }
        FailureClass::PlatformSetupFailed => {
            "Case setup_cmd failed; check the command and working directory"
        }
        FailureClass::HarnessCleanupFailed => {
            "Harness cleanup could not prove the environment was restored; inspect the recorded cleanup error"
        }
        FailureClass::ModelInstructionFollowing => {
            "Model ignored constraints; consider a stronger model or refine the prompt"
        }
        FailureClass::ModelCapability => {
            "Model lacks capability for this task; try a more capable model"
        }
        FailureClass::ModelQualityLow => {
            "Model completed the task but judger scored quality below threshold"
        }
        FailureClass::EfficiencyBoundsExceeded => {
            "Execution exceeded its efficiency bound; inspect task scope and round pacing before increasing the limit"
        }
        FailureClass::BehaviorContractViolation => {
            "Inspect the failed typed criterion and durable journal before assigning the fault to model or runtime"
        }
        FailureClass::ToolUnavailable => {
            "A required tool was not exposed at runtime; check tool registry and selector config"
        }
        FailureClass::Unknown => "Inspect stderr and session journal for clues",
    }
}

fn timeout_has_execution_progress(
    outcome: &RunOutcome,
    criteria_results: &[CriterionResult],
) -> bool {
    if outcome.turn_rounds > 0
        || outcome.tool_calls_count > 0
        || stream_has_execution_progress(&outcome.stderr)
    {
        return true;
    }
    criteria_results.iter().any(|result| {
        result.passed
            && match &result.criterion {
                Criterion::JournalToolCallCount { min, .. }
                | Criterion::JournalToolOutcomeCount { min, .. } => *min > 0,
                Criterion::JournalToolJson { .. }
                | Criterion::JournalToolJsonContains { .. }
                | Criterion::JournalToolValueFlowBound { .. }
                | Criterion::JournalArtifactConsumed { .. }
                | Criterion::JournalToolValueFlow { .. }
                | Criterion::JournalToolSequence { .. }
                | Criterion::JournalToolPrecedence { .. }
                | Criterion::JournalWorkItemExecutionFromStart { .. }
                | Criterion::JournalWorkGraphPatch { .. } => true,
                Criterion::TurnRoundsBetween { min, .. } => *min > 0,
                _ => false,
            }
    })
}

/// The CLI's `--stream-events` channel is the last live evidence available
/// when the outer timeout kills the process before the JSON terminal envelope
/// is printed. Count only producer-owned execution events; session/run
/// binding and context-window estimates prove setup, not model progress.
fn stream_has_execution_progress(stderr: &str) -> bool {
    const EXECUTION_EVENTS: &[&str] = &[
        "model_responding",
        "token",
        "thinking_chunk",
        "tool_started",
        "tool_completed",
        "agent_control_started",
        "agent_control_completed",
        "assistant_output_settled",
        "turn_complete",
    ];
    stderr.lines().any(|line| {
        serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .and_then(|event| {
                event
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .map(|event_type| EXECUTION_EVENTS.contains(&event_type))
            })
            .unwrap_or(false)
    })
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
            | Criterion::TextNotContains { .. }
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
            severity: crate::criteria::CriterionSeverity::Hard,
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
    fn deadline_after_model_progress_is_an_efficiency_failure() {
        let mut outcome = make_outcome().with_exit_code(124);
        outcome.turn_rounds = 3;
        assert_eq!(
            classify(&outcome, &[]),
            FailureClass::EfficiencyBoundsExceeded
        );

        // A killed CLI may not flush its terminal summary. Durable journal
        // criteria still prove that the product crossed execution boundaries.
        let outcome = make_outcome().with_exit_code(124);
        let evidence = CriterionResult {
            criterion: Criterion::JournalToolCallCount {
                name: "run_next_work_item".into(),
                min: 1,
                max: 2,
                document: None,
                path: None,
                equals: None,
            },
            severity: crate::criteria::CriterionSeverity::Hard,
            passed: true,
            detail: String::new(),
            full_detail: None,
            score: None,
        };
        assert_eq!(
            classify(&outcome, &[evidence]),
            FailureClass::EfficiencyBoundsExceeded
        );
    }

    #[test]
    fn stream_progress_reclassifies_timeout_without_a_terminal_envelope() {
        let outcome = make_outcome().with_exit_code(124).with_stderr(
            "{\"type\":\"session_bound\",\"session_id\":\"s\"}\n{\"type\":\"model_responding\"}\n",
        );
        assert_eq!(
            classify(&outcome, &[]),
            FailureClass::EfficiencyBoundsExceeded
        );

        let setup_only = make_outcome()
            .with_exit_code(124)
            .with_stderr(
                "{\"type\":\"session_bound\",\"session_id\":\"s\"}\n{\"type\":\"context_window_estimated\"}\n",
            );
        assert_eq!(classify(&setup_only, &[]), FailureClass::InfraTimeout);
    }

    #[test]
    fn vacuous_zero_call_evidence_does_not_reclassify_infra_timeout() {
        let outcome = make_outcome().with_exit_code(124);
        let absence_check = CriterionResult {
            criterion: Criterion::JournalToolCallCount {
                name: "ask_user".into(),
                min: 0,
                max: 0,
                document: None,
                path: None,
                equals: None,
            },
            severity: crate::criteria::CriterionSeverity::Hard,
            passed: true,
            detail: String::new(),
            full_detail: None,
            score: None,
        };
        assert_eq!(
            classify(&outcome, &[absence_check]),
            FailureClass::InfraTimeout
        );
    }

    #[test]
    fn auth_from_stderr() {
        let outcome = make_outcome()
            .with_exit_code(3)
            .with_stderr("Could not validate credentials");
        assert_eq!(classify(&outcome, &[]), FailureClass::InfraAuth);
    }

    #[test]
    fn durable_ledger_timeout_is_runtime_infrastructure_not_auth() {
        let outcome = make_outcome().with_exit_code(3).with_stderr(
            "Error: [database_error] durable inference ledger timed out during logical_invocation_admission",
        );
        assert_eq!(classify(&outcome, &[]), FailureClass::InfraRuntime);
        assert!(suggested_action(&FailureClass::InfraRuntime).contains("database"));
    }

    #[test]
    fn rate_limit_429() {
        let outcome = make_outcome()
            .with_exit_code(1)
            .with_final_state("interrupted")
            .with_interruption_kind("rate_limit")
            .with_stderr("error: 429 Too many requests");
        assert_eq!(classify(&outcome, &[]), FailureClass::InfraRateLimit);

        let successful_discussion = make_outcome()
            .with_exit_code(0)
            .with_final_state("completed")
            .with_stderr("investigation found a rate limit implementation");
        assert_ne!(
            classify(&successful_discussion, &[]),
            FailureClass::InfraRateLimit
        );
    }

    #[test]
    fn daily_session_quota_is_not_misclassified_as_auth() {
        let outcome = make_outcome().with_exit_code(3).with_stderr(
            "Error: Per-user session quota exceeded (daily_sessions): daily session limit reached (50/50)",
        );
        assert_eq!(classify(&outcome, &[]), FailureClass::InfraQuota);
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
    fn exit_3_empty_text_without_auth_evidence_is_unknown() {
        let outcome = make_outcome().with_exit_code(3);
        assert_eq!(classify(&outcome, &[]), FailureClass::Unknown);
    }

    #[test]
    fn unavailable_required_judger_is_infrastructure_not_unknown() {
        let result = CriterionResult {
            criterion: Criterion::HardJudger {
                question: "did the purge succeed?".into(),
                threshold: 0.7,
                model: None,
            },
            severity: crate::criteria::CriterionSeverity::Hard,
            passed: false,
            detail: "required judger unavailable (--no-judger)".into(),
            full_detail: None,
            score: None,
        };
        assert_eq!(
            classify(&make_outcome(), &[result]),
            FailureClass::InfraVerificationUnavailable
        );
    }

    #[test]
    fn model_inactive_is_not_misclassified_as_auth() {
        let outcome = make_outcome()
            .with_exit_code(3)
            .with_stderr("Error: Model 'foo' is inactive (connectivity failed or disabled)");
        assert_eq!(classify(&outcome, &[]), FailureClass::InfraModelInactive);
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
    fn successful_process_with_failed_typed_oracle_is_not_unknown() {
        let outcome = make_outcome()
            .with_exit_code(0)
            .with_final_state("completed");
        let result = cr(
            Criterion::JournalToolOutcomeCount {
                name: "settle_work_item".into(),
                ok: false,
                min: 0,
                max: 0,
            },
            false,
        );

        assert_eq!(
            classify(&outcome, &[result]),
            FailureClass::BehaviorContractViolation
        );
    }

    #[test]
    fn tool_unavailable_generic_tool_name() {
        // Generic pattern: "don't have a <any_tool> tool available/in" should match
        let outcome =
            make_outcome().with_text("I don't have a grep tool available in my current toolset");
        let class = classify(&outcome, &[]);
        assert_eq!(class, FailureClass::ToolUnavailable);
    }

    #[test]
    fn generic_tool_phrase_does_not_false_positive() {
        // "I don't have a good tool for this" is NOT a tool-registry failure
        let outcome = make_outcome()
            .with_text("I don't have a good tool for this task, but I can try manually.");
        let class = classify(&outcome, &[]);
        assert_ne!(
            class,
            FailureClass::ToolUnavailable,
            "casual 'tool' mention should not trigger ToolUnavailable"
        );
    }

    #[test]
    fn tool_unavailable_still_matches_specific_signals() {
        let outcome = make_outcome().with_text("tool is not available");
        assert_eq!(classify(&outcome, &[]), FailureClass::ToolUnavailable);
    }

    #[test]
    fn every_class_has_suggested_action() {
        let classes = [
            FailureClass::InfraAuth,
            FailureClass::InfraRuntime,
            FailureClass::InfraQuota,
            FailureClass::InfraTimeout,
            FailureClass::InfraRateLimit,
            FailureClass::BehaviorContractViolation,
            FailureClass::ToolUnavailable,
            FailureClass::Unknown,
        ];
        for class in &classes {
            assert!(!suggested_action(class).is_empty());
        }
    }
}

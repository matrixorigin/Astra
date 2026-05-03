use crate::HookPoint;
use crate::trace::{SessionTrace, TraceOutcome};

/// A scenario defines expected behavior and runs assertions against a trace.
#[derive(Debug, Clone)]
pub struct Scenario {
    pub name: String,
    pub assertions: Vec<Assertion>,
}

#[derive(Debug, Clone)]
pub enum Assertion {
    /// Trace must contain at least N records.
    MinRecords(usize),
    /// Trace must have a specific outcome.
    Outcome(TraceOutcome),
    /// Total turns must be within [min, max].
    TurnRange { min: u32, max: u32 },
    /// Total tokens must be under this limit.
    MaxTokens(u64),
    /// Hook must fire at least N times.
    HookFiredAtLeast(HookPoint, usize),
    /// No warnings of a specific kind in forensics.
    NoForensicsWarning(crate::forensics::WarningKind),
    /// Session duration must be under N millis.
    MaxDurationMillis(u64),
    /// Custom assertion with closure description.
    Custom {
        description: String,
        check: fn(&SessionTrace) -> bool,
    },
}

/// Result of running a scenario against a trace.
#[derive(Debug)]
pub struct ScenarioResult {
    pub scenario_name: String,
    pub passed: bool,
    pub assertion_results: Vec<AssertionResult>,
}

#[derive(Debug)]
pub struct AssertionResult {
    pub description: String,
    pub passed: bool,
    pub detail: Option<String>,
}

impl Scenario {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            assertions: Vec::new(),
        }
    }

    pub fn assert(mut self, assertion: Assertion) -> Self {
        self.assertions.push(assertion);
        self
    }

    pub fn run(&self, trace: &SessionTrace) -> ScenarioResult {
        let mut results = Vec::new();

        for assertion in &self.assertions {
            let (desc, passed, detail) = check_assertion(assertion, trace);
            results.push(AssertionResult {
                description: desc,
                passed,
                detail,
            });
        }

        let all_passed = results.iter().all(|r| r.passed);

        ScenarioResult {
            scenario_name: self.name.clone(),
            passed: all_passed,
            assertion_results: results,
        }
    }
}

fn check_assertion(assertion: &Assertion, trace: &SessionTrace) -> (String, bool, Option<String>) {
    match assertion {
        Assertion::MinRecords(n) => {
            let actual = trace.record_count();
            (
                format!("min records >= {n}"),
                actual >= *n,
                Some(format!("actual: {actual}")),
            )
        }
        Assertion::Outcome(expected) => {
            let passed = trace.outcome == *expected;
            (
                format!("outcome == {expected:?}"),
                passed,
                Some(format!("actual: {:?}", trace.outcome)),
            )
        }
        Assertion::TurnRange { min, max } => {
            let t = trace.total_turns;
            (
                format!("turns in [{min}, {max}]"),
                t >= *min && t <= *max,
                Some(format!("actual: {t}")),
            )
        }
        Assertion::MaxTokens(limit) => {
            let actual = trace
                .records
                .back()
                .map(|r| r.snapshot.tokens_used_session)
                .unwrap_or(0);
            (
                format!("tokens <= {limit}"),
                actual <= *limit,
                Some(format!("actual: {actual}")),
            )
        }
        Assertion::HookFiredAtLeast(point, count) => {
            let actual = trace.records_at_point(*point).len();
            (
                format!("{point:?} fired >= {count} times"),
                actual >= *count,
                Some(format!("actual: {actual}")),
            )
        }
        Assertion::NoForensicsWarning(kind) => {
            let summary = trace.forensics_summary();
            let found = summary.warnings.iter().any(|w| w.kind == *kind);
            (
                format!("no {kind:?} warnings"),
                !found,
                if found {
                    Some("warning found".into())
                } else {
                    None
                },
            )
        }
        Assertion::MaxDurationMillis(limit) => {
            let actual = trace.duration_millis().unwrap_or(0);
            (
                format!("duration <= {limit}ms"),
                actual <= *limit,
                Some(format!("actual: {actual}ms")),
            )
        }
        Assertion::Custom { description, check } => {
            let passed = check(trace);
            (description.clone(), passed, None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::SessionTrace;
    use crate::{DecisionRecord, HookPoint, RuntimeSnapshot};

    fn make_record(turn: u32, point: HookPoint, tokens: u64) -> DecisionRecord {
        DecisionRecord {
            session_id: "scenario-test".into(),
            turn,
            point,
            wall_time_unix_millis: 1_000_000 + turn as u64 * 1000,
            monotonic_millis_since_session: turn as u64 * 1000,
            snapshot: RuntimeSnapshot {
                turn_number: turn,
                turns_used: turn,
                tokens_used_session: tokens,
                ..RuntimeSnapshot::empty()
            },
        }
    }

    fn sample_trace() -> SessionTrace {
        let mut trace = SessionTrace::new("scenario-test".into());
        trace.started_at_unix_millis = 1_000_000;
        trace.ended_at_unix_millis = Some(1_005_000);
        trace.total_turns = 3;
        trace.outcome = TraceOutcome::Completed;
        trace
            .records
            .push_back(make_record(0, HookPoint::SessionStart, 0));
        trace
            .records
            .push_back(make_record(1, HookPoint::PostLlmResponse, 5_000));
        trace
            .records
            .push_back(make_record(1, HookPoint::PostTurn, 5_000));
        trace
            .records
            .push_back(make_record(2, HookPoint::PostLlmResponse, 10_000));
        trace
            .records
            .push_back(make_record(2, HookPoint::PostTurn, 10_000));
        trace
    }

    #[test]
    fn scenario_all_pass() {
        let scenario = Scenario::new("happy path")
            .assert(Assertion::MinRecords(3))
            .assert(Assertion::Outcome(TraceOutcome::Completed))
            .assert(Assertion::TurnRange { min: 1, max: 5 })
            .assert(Assertion::MaxTokens(100_000))
            .assert(Assertion::HookFiredAtLeast(HookPoint::PostTurn, 2))
            .assert(Assertion::MaxDurationMillis(10_000));

        let result = scenario.run(&sample_trace());
        assert!(result.passed);
        assert_eq!(result.assertion_results.len(), 6);
        assert!(result.assertion_results.iter().all(|r| r.passed));
    }

    #[test]
    fn scenario_fails_on_wrong_outcome() {
        let scenario =
            Scenario::new("expect blocked").assert(Assertion::Outcome(TraceOutcome::Blocked));

        let result = scenario.run(&sample_trace());
        assert!(!result.passed);
    }

    #[test]
    fn scenario_fails_on_token_limit() {
        let scenario = Scenario::new("token budget").assert(Assertion::MaxTokens(3_000));

        let result = scenario.run(&sample_trace());
        assert!(!result.passed);
        let detail = result.assertion_results[0].detail.as_ref().unwrap();
        assert!(detail.contains("10000"));
    }

    #[test]
    fn scenario_fails_on_turn_range() {
        let scenario =
            Scenario::new("too many turns").assert(Assertion::TurnRange { min: 10, max: 20 });

        let result = scenario.run(&sample_trace());
        assert!(!result.passed);
    }

    #[test]
    fn scenario_custom_assertion() {
        let scenario = Scenario::new("custom check").assert(Assertion::Custom {
            description: "has session start".into(),
            check: |t| t.records_at_point(HookPoint::SessionStart).len() == 1,
        });

        let result = scenario.run(&sample_trace());
        assert!(result.passed);
    }

    #[test]
    fn scenario_no_forensics_warning() {
        let scenario = Scenario::new("no stalls").assert(Assertion::NoForensicsWarning(
            crate::forensics::WarningKind::ToolStallDetected,
        ));

        let result = scenario.run(&sample_trace());
        assert!(result.passed);
    }

    #[test]
    fn scenario_result_details() {
        let scenario = Scenario::new("detail check").assert(Assertion::MinRecords(100));

        let result = scenario.run(&sample_trace());
        assert!(!result.passed);
        assert_eq!(
            result.assertion_results[0].detail.as_deref(),
            Some("actual: 5")
        );
    }
}

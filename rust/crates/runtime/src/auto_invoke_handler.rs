//! Production-facing glue between `AutoInvokeGate` and the diagnostic
//! skills it wants to fire.
//!
//! `AutoInvokeGate` is a pure decision machine — it names the skill and
//! cause but does not execute. `SkillDiagnosis::parse_from_skill_output`
//! parses a raw markdown reply into a structured payload. This handler
//! stitches the two together into a single `maybe_fire` call the main
//! loop can invoke once per turn:
//!
//! ```text
//! handler.maybe_fire(&signals, Instant::now()).await
//! ```
//!
//! Returns the list of [`SkillDiagnosis`] payloads successfully extracted
//! this tick. Callers pass each into
//! `SelfModel::with_skill_diagnosis(Some(..))` to inject it on the next
//! turn.
//!
//! Execution is delegated to a caller-supplied `SkillExecutor` trait, so
//! tests don't depend on the real skill-invocation plumbing and the main
//! loop can wire in whatever it already uses.

use std::sync::Arc;
use std::time::Instant;

use astra_skills::auto_invoke::{
    AutoInvokeGate, AutoInvokeRequest, DiagnosisCriterion, DiagnosisMetric, DiagnosisOperator,
    SKILL_DIAGNOSIS_SCHEMA_VERSION, SessionSignals, SkillDiagnosis,
};
use async_trait::async_trait;

/// Abstraction over "run a diagnostic skill and return its markdown
/// reply." The production impl wraps the runtime's real skill invocation;
/// tests can stub it cheaply.
///
/// Returning `None` means the skill failed to produce output (network
/// error, timeout, skill not available). The handler will drop the
/// request silently — auto-diagnosis is advisory, never critical.
#[async_trait]
pub trait SkillExecutor: Send + Sync {
    async fn run(&self, req: &AutoInvokeRequest) -> Option<String>;
}

/// Glue layer around [`AutoInvokeGate`]. Owns the gate's mutable
/// cooldown state; callers keep one handler per session.
pub struct AutoInvokeHandler {
    gate: AutoInvokeGate,
    executor: Arc<dyn SkillExecutor>,
}

impl AutoInvokeHandler {
    #[must_use]
    pub fn new(executor: Arc<dyn SkillExecutor>) -> Self {
        Self {
            gate: AutoInvokeGate::new(),
            executor,
        }
    }

    /// Evaluate signals, fire every triggered skill, parse each reply,
    /// return every successfully-parsed `SkillDiagnosis`.
    ///
    /// Skills whose reply is `None` (executor failure) or fails to parse
    /// a valid diagnosis block are silently dropped — the loop must
    /// never stall on advisory telemetry.
    pub async fn maybe_fire(
        &mut self,
        signals: &SessionSignals,
        now: Instant,
    ) -> Vec<SkillDiagnosis> {
        let requests = self.gate.evaluate(signals, now);
        if requests.is_empty() {
            return Vec::new();
        }

        let mut out = Vec::with_capacity(requests.len());
        for req in &requests {
            let Some(reply) = self.executor.run(req).await else {
                tracing::debug!(
                    target: "auto_invoke_handler",
                    skill = req.skill,
                    "executor returned None; dropping request",
                );
                continue;
            };
            match SkillDiagnosis::parse_from_skill_output(&reply) {
                Some(diag) => out.push(diag),
                None => {
                    tracing::warn!(
                        target: "auto_invoke_handler",
                        skill = req.skill,
                        "skill reply had no parseable diagnosis block",
                    );
                }
            }
        }
        out
    }
}

/// Synthetic [`SkillExecutor`] that returns a canned `skill-diagnosis` block
/// for every request without invoking an LLM. Useful as a default in
/// session bootstrap so the auto-invoke loop exercises end-to-end (gate
/// → executor → diagnosis parse → prompt injection) even before a real
/// skill-invocation runner is wired in.
///
/// The generated headline + findings are deterministic functions of the
/// trigger cause, so operators get a useful "system noticed you stalled
/// N times" hint in the prompt without waiting for a cloud round-trip.
/// Real skill runners should be preferred in production; this executor marks
/// every diagnosis as `synthetic_fallback` so downstream prompts and outcome
/// tracking know it was advisory canned telemetry.
pub struct SyntheticSkillDiagnosisExecutor;

#[async_trait]
impl SkillExecutor for SyntheticSkillDiagnosisExecutor {
    async fn run(&self, req: &AutoInvokeRequest) -> Option<String> {
        use astra_skills::auto_invoke::AutoInvokeCause;
        let (headline, finding, metric, operator, threshold, description) = match &req.cause {
            AutoInvokeCause::SessionStalls { count } => (
                format!("agent stalled {count} times this session"),
                format!("pipeline detected {count} session stall events"),
                "session_stalls_delta",
                "lte",
                0.0,
                "session stalls stop increasing after this hint",
            ),
            AutoInvokeCause::BudgetPressure { level } => (
                format!("budget pressure at {:.0}%", level * 100.0),
                format!("context budget utilisation is {:.2}", level),
                "budget_pressure",
                "lte",
                0.85,
                "budget pressure returns below the auto-invoke threshold",
            ),
            AutoInvokeCause::RepeatedCorrections { count } => (
                format!("user issued {count} corrections this session"),
                format!("{count} distinct corrections recorded"),
                "corrections_delta",
                "lte",
                0.0,
                "new user corrections stop increasing",
            ),
        };
        let payload = serde_json::json!({
            "schema_version": SKILL_DIAGNOSIS_SCHEMA_VERSION,
            "skill": req.skill,
            "cause": req.cause.as_str(),
            "headline": headline,
            "findings": [finding],
            "recommended_action": "review the corresponding skill output for deeper analysis",
            "success_criteria": [{
                "metric": metric,
                "operator": operator,
                "threshold": threshold,
                "window_turns": 3,
                "description": description,
            }],
            "source": "synthetic_fallback",
        });
        Some(format!("```skill-diagnosis\n{}\n```\n", payload))
    }
}

/// Evaluation status for a previously injected diagnosis criterion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosisCriterionStatus {
    Pending,
    Satisfied,
    Failed,
}

/// Outcome of evaluating an active diagnosis after a turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosisOutcome {
    pub skill: String,
    pub cause: String,
    pub statuses: Vec<DiagnosisCriterionStatus>,
    pub complete: bool,
}

#[derive(Debug, Clone)]
struct ActiveDiagnosis {
    diagnosis: SkillDiagnosis,
    baseline: SessionSignals,
    injected_turn: u32,
}

/// Pure per-session tracker for machine-checkable diagnosis postconditions.
#[derive(Debug, Default, Clone)]
pub struct DiagnosisOutcomeTracker {
    active: Vec<ActiveDiagnosis>,
}

impl DiagnosisOutcomeTracker {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn activate(&mut self, diagnosis: SkillDiagnosis, baseline: SessionSignals, turn: u32) {
        self.active.push(ActiveDiagnosis {
            diagnosis,
            baseline,
            injected_turn: turn,
        });
    }

    pub fn evaluate_turn(&mut self, signals: &SessionSignals, turn: u32) -> Vec<DiagnosisOutcome> {
        let mut outcomes = Vec::new();
        self.active.retain(|active| {
            let elapsed = turn.saturating_sub(active.injected_turn);
            let statuses: Vec<_> = active
                .diagnosis
                .success_criteria
                .iter()
                .map(|c| evaluate_criterion(c, &active.baseline, signals, elapsed))
                .collect();
            let complete = statuses
                .iter()
                .all(|s| *s != DiagnosisCriterionStatus::Pending);
            if complete {
                outcomes.push(DiagnosisOutcome {
                    skill: active.diagnosis.skill.clone(),
                    cause: active.diagnosis.cause.clone(),
                    statuses,
                    complete,
                });
                false
            } else {
                true
            }
        });
        outcomes
    }
}

/// Evaluate a single diagnosis criterion against current session signals.
///
/// ## Fail-safe for unwired metrics
///
/// Metrics marked **Pending** in [`DiagnosisMetric`] are not yet surfaced
/// in [`SessionSignals`]. When encountered, this function returns
/// [`DiagnosisCriterionStatus::Pending`] indefinitely — the criterion
/// will never resolve to `Satisfied`, so after `window_turns` elapse the
/// tracker treats it as `Failed`. This is the correct fail-safe: an
/// unwired metric must not silently claim success, and it must not block
/// the tracker forever.
fn evaluate_criterion(
    c: &DiagnosisCriterion,
    baseline: &SessionSignals,
    current: &SessionSignals,
    elapsed_turns: u32,
) -> DiagnosisCriterionStatus {
    let observed = match c.metric {
        // ── Wired metrics: real value from SessionSignals ────────────
        DiagnosisMetric::SessionStallsDelta => f64::from(
            current
                .session_stalls
                .saturating_sub(baseline.session_stalls),
        ),
        DiagnosisMetric::BudgetPressure => current.budget_pressure,
        DiagnosisMetric::CorrectionsDelta => f64::from(
            current
                .recent_corrections
                .saturating_sub(baseline.recent_corrections),
        ),
        // ── Pending metrics: not yet in SessionSignals ──────────────
        // Fail-safe: stay Pending until window expires → Failed.
        // MUST check window here (not early-return Pending) so the
        // tracker can remove completed entries from `active`.
        DiagnosisMetric::ToolCallCount | DiagnosisMetric::UnmetPostconditionsDelta => {
            return if elapsed_turns >= c.window_turns {
                DiagnosisCriterionStatus::Failed
            } else {
                DiagnosisCriterionStatus::Pending
            };
        }
    };

    if compare(observed, c.operator, c.threshold) {
        return DiagnosisCriterionStatus::Satisfied;
    }
    if elapsed_turns >= c.window_turns {
        DiagnosisCriterionStatus::Failed
    } else {
        DiagnosisCriterionStatus::Pending
    }
}

fn compare(lhs: f64, op: DiagnosisOperator, rhs: f64) -> bool {
    match op {
        DiagnosisOperator::Lt => lhs < rhs,
        DiagnosisOperator::Lte => lhs <= rhs,
        DiagnosisOperator::Eq => (lhs - rhs).abs() < f64::EPSILON,
        DiagnosisOperator::Gte => lhs >= rhs,
        DiagnosisOperator::Gt => lhs > rhs,
    }
}

/// Build [`SessionSignals`] from the live observability view the runtime
/// already maintains. Designed as a single reconciliation point for the
/// turn loop:
///
/// ```text
/// let signals = compute_session_signals(&obs);
/// let diagnoses = handler.maybe_fire(&signals, Instant::now()).await;
/// ```
///
/// Mapping:
/// - `session_stalls` → `obs.stall_event_count`. This is cumulative,
///   not literally "consecutive" — but the per-cause cooldown in
///   `AutoInvokeGate` ensures the stall skill fires at most once per
///   60-second window regardless, so treating the cumulative count as
///   an "any stall in this session" gate is sound.
/// - `budget_pressure` → the most recent `context_traces` pressure
///   value, else 0.0 when no traces have been recorded yet.
/// - `recent_corrections` → `obs.user_corrections.len()`. These are
///   already session-scoped, so the list's length doubles as a count.
/// - `corrections_window` → fixed 10 turns (the gate doesn't re-window;
///   this field is surfaced only for logging).
///
/// Returns a zero-valued `SessionSignals` when `obs` is `None`, so the
/// caller can use this unconditionally and let the gate's thresholds
/// decide whether to fire.
#[must_use]
pub fn compute_session_signals(
    obs: Option<&crate::observability_integration::ObservabilitySession>,
) -> SessionSignals {
    let Some(session) = obs else {
        return SessionSignals::default();
    };

    let budget_pressure = session
        .context_traces
        .last()
        .map(|trace| trace.token_budget.budget_pressure)
        .unwrap_or(0.0);

    let recent_corrections = u32::try_from(session.user_corrections.len()).unwrap_or(u32::MAX);

    SessionSignals {
        session_stalls: session.stall_event_count,
        budget_pressure,
        recent_corrections,
        corrections_window: 10,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_skills::auto_invoke::{
        AutoInvokeCause, DiagnosisSource, SKILL_DIAGNOSIS_SCHEMA_VERSION,
    };
    use std::sync::Mutex;
    use std::time::Duration;

    // ── Helpers ─────────────────────────────────────────────────────────────

    fn signals(stalls: u32, pressure: f64, corrections: u32) -> SessionSignals {
        SessionSignals {
            session_stalls: stalls,
            budget_pressure: pressure,
            recent_corrections: corrections,
            corrections_window: 10,
        }
    }

    fn well_formed_reply(skill: &str, cause: &str, headline: &str) -> String {
        let (metric, threshold) = match cause {
            "budget_pressure" => ("budget_pressure", "0.85"),
            "repeated_corrections" => ("corrections_delta", "0.0"),
            _ => ("session_stalls_delta", "0.0"),
        };
        format!(
            "# Analysis\n\nSome prose.\n\n```skill-diagnosis\n{{\n  \
             \"schema_version\": 2,\n  \"skill\": \"{skill}\",\n  \
             \"cause\": \"{cause}\",\n  \"headline\": \"{headline}\",\n  \
             \"findings\": [],\n  \"recommended_action\": \"try rg\",\n  \
             \"success_criteria\": [{{\"metric\":\"{metric}\",\"operator\":\"lte\",\"threshold\":{threshold},\"window_turns\":3,\"description\":\"criterion\"}}],\n  \
             \"source\": \"real_skill\"\n}}\n```\n",
        )
    }

    // ── Stub executor records invocations and returns canned replies ────────

    struct StubExecutor {
        /// Per-skill canned reply; None → executor failure.
        replies: std::collections::HashMap<&'static str, Option<String>>,
        calls: Mutex<Vec<AutoInvokeRequest>>,
    }

    impl StubExecutor {
        fn new() -> Self {
            Self {
                replies: std::collections::HashMap::new(),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn with(mut self, skill: &'static str, reply: Option<String>) -> Self {
            self.replies.insert(skill, reply);
            self
        }

        fn calls(&self) -> Vec<AutoInvokeRequest> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl SkillExecutor for StubExecutor {
        async fn run(&self, req: &AutoInvokeRequest) -> Option<String> {
            self.calls.lock().unwrap().push(req.clone());
            self.replies.get(req.skill).cloned().unwrap_or(None)
        }
    }

    // ── Contract tests ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn quiet_session_fires_nothing() {
        let exec = Arc::new(StubExecutor::new());
        let mut h = AutoInvokeHandler::new(exec.clone());
        let out = h.maybe_fire(&signals(0, 0.1, 0), Instant::now()).await;
        assert!(out.is_empty());
        assert!(
            exec.calls().is_empty(),
            "executor must not be invoked when no trigger"
        );
    }

    #[tokio::test]
    async fn stall_trigger_runs_analyze_session_and_returns_parsed_diagnosis() {
        let exec = Arc::new(StubExecutor::new().with(
            "analyze_session",
            Some(well_formed_reply(
                "analyze_session",
                "session_stalls",
                "agent looped on grep",
            )),
        ));
        let mut h = AutoInvokeHandler::new(exec.clone());

        let out = h.maybe_fire(&signals(5, 0.1, 0), Instant::now()).await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].skill, "analyze_session");
        assert_eq!(out[0].cause, "session_stalls");
        assert_eq!(out[0].schema_version, SKILL_DIAGNOSIS_SCHEMA_VERSION);
        assert_eq!(out[0].headline, "agent looped on grep");

        let calls = exec.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].skill, "analyze_session");
        assert!(matches!(
            calls[0].cause,
            AutoInvokeCause::SessionStalls { count: 5 }
        ));
    }

    #[tokio::test]
    async fn all_three_triggers_fire_executor_thrice_and_return_three_diagnoses() {
        let exec = Arc::new(
            StubExecutor::new()
                .with(
                    "analyze_session",
                    Some(well_formed_reply(
                        "analyze_session",
                        "session_stalls",
                        "stalls found",
                    )),
                )
                .with(
                    "optimize_prompt",
                    Some(well_formed_reply(
                        "optimize_prompt",
                        "budget_pressure",
                        "prompt bloated",
                    )),
                )
                .with(
                    "evaluate_session",
                    Some(well_formed_reply(
                        "evaluate_session",
                        "repeated_corrections",
                        "scope drift",
                    )),
                ),
        );
        let mut h = AutoInvokeHandler::new(exec.clone());

        let out = h.maybe_fire(&signals(5, 0.95, 5), Instant::now()).await;
        assert_eq!(out.len(), 3);
        let skills: Vec<&str> = out.iter().map(|d| d.skill.as_str()).collect();
        assert_eq!(
            skills,
            vec!["analyze_session", "optimize_prompt", "evaluate_session"],
        );
        assert_eq!(exec.calls().len(), 3);
    }

    #[tokio::test]
    async fn executor_none_is_silently_dropped() {
        // Advisory telemetry — a failed skill invocation must not break
        // the loop; the handler just omits it from the output.
        let exec = Arc::new(StubExecutor::new().with("analyze_session", None));
        let mut h = AutoInvokeHandler::new(exec.clone());

        let out = h.maybe_fire(&signals(5, 0.1, 0), Instant::now()).await;
        assert!(out.is_empty(), "None reply must be silently dropped");
        assert_eq!(
            exec.calls().len(),
            1,
            "executor was still invoked even though output was None",
        );
    }

    #[tokio::test]
    async fn unparseable_reply_is_silently_dropped() {
        // Skill returned prose but no diagnosis block → parser fails.
        // The handler drops it; doesn't panic, doesn't error.
        let exec = Arc::new(StubExecutor::new().with(
            "analyze_session",
            Some("sorry, no structured output today".into()),
        ));
        let mut h = AutoInvokeHandler::new(exec);

        let out = h.maybe_fire(&signals(5, 0.1, 0), Instant::now()).await;
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn cooldown_prevents_double_fire_across_calls() {
        // The gate owns cooldowns — the handler must preserve them across
        // successive `maybe_fire` calls on the same instance.
        let exec = Arc::new(StubExecutor::new().with(
            "analyze_session",
            Some(well_formed_reply("analyze_session", "session_stalls", "h")),
        ));
        let mut h = AutoInvokeHandler::new(exec.clone());

        let t0 = Instant::now();
        let first = h.maybe_fire(&signals(5, 0.1, 0), t0).await;
        assert_eq!(first.len(), 1);

        let second = h.maybe_fire(&signals(5, 0.1, 0), t0).await;
        assert!(second.is_empty(), "second fire inside cooldown must drop");
        assert_eq!(
            exec.calls().len(),
            1,
            "executor must not be invoked while the gate is on cooldown",
        );

        // After cooldown elapses (60s for stalls), a new fire should go through.
        let third = h
            .maybe_fire(&signals(5, 0.1, 0), t0 + Duration::from_secs(61))
            .await;
        assert_eq!(third.len(), 1);
        assert_eq!(exec.calls().len(), 2);
    }

    #[tokio::test]
    async fn partial_executor_failure_does_not_block_other_skills() {
        // If analyze_session fails but optimize_prompt succeeds, we still
        // want the latter's diagnosis injected. This is the key property
        // that makes the handler safe to call unconditionally.
        let exec = Arc::new(
            StubExecutor::new()
                .with("analyze_session", None) // fails
                .with(
                    "optimize_prompt",
                    Some(well_formed_reply(
                        "optimize_prompt",
                        "budget_pressure",
                        "bloated",
                    )),
                ),
        );
        let mut h = AutoInvokeHandler::new(exec);

        let out = h.maybe_fire(&signals(5, 0.95, 0), Instant::now()).await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].skill, "optimize_prompt");
    }

    // ── compute_session_signals ────────────────────────────────────────────

    #[test]
    fn compute_signals_none_observability_yields_zero() {
        let s = compute_session_signals(None);
        assert_eq!(s, SessionSignals::default());
    }

    #[test]
    fn compute_signals_reads_cumulative_stall_count() {
        use crate::observability_integration::ObservabilitySession;
        let mut obs = ObservabilitySession::new_simple("s-p7b");
        obs.record_stall_event();
        obs.record_stall_event();
        obs.record_stall_event();

        let s = compute_session_signals(Some(&obs));
        assert_eq!(s.session_stalls, 3);
    }

    #[test]
    fn compute_signals_reads_user_corrections_length() {
        use crate::observability_integration::ObservabilitySession;
        let mut obs = ObservabilitySession::new_simple("s-p7b");
        obs.user_corrections = vec![1, 2, 5, 7];

        let s = compute_session_signals(Some(&obs));
        assert_eq!(s.recent_corrections, 4);
        assert_eq!(s.corrections_window, 10);
    }

    #[test]
    fn compute_signals_budget_pressure_defaults_to_zero_without_traces() {
        use crate::observability_integration::ObservabilitySession;
        let obs = ObservabilitySession::new_simple("s-p7b");
        let s = compute_session_signals(Some(&obs));
        assert_eq!(s.budget_pressure, 0.0);
    }

    // ── SyntheticSkillDiagnosisExecutor ────────────────────────────────────

    #[tokio::test]
    async fn synthetic_executor_emits_parseable_diagnosis_for_each_cause() {
        // Every cause must round-trip: the stub's reply must parse back
        // into a SkillDiagnosis whose schema_version, skill, and cause
        // match the request. Otherwise the loop emits noise the prompt
        // renderer can't use.
        let exec = SyntheticSkillDiagnosisExecutor;
        let cases = [
            (
                "analyze_session",
                AutoInvokeCause::SessionStalls { count: 4 },
                "session_stalls",
            ),
            (
                "optimize_prompt",
                AutoInvokeCause::BudgetPressure { level: 0.92 },
                "budget_pressure",
            ),
            (
                "evaluate_session",
                AutoInvokeCause::RepeatedCorrections { count: 5 },
                "repeated_corrections",
            ),
        ];
        for (skill, cause, tag) in cases {
            let req = AutoInvokeRequest {
                skill,
                focus: "focus",
                cause,
            };
            let reply = exec.run(&req).await.expect("executor must produce reply");
            let diag = SkillDiagnosis::parse_from_skill_output(&reply)
                .expect("reply must parse into diagnosis");
            assert_eq!(diag.schema_version, SKILL_DIAGNOSIS_SCHEMA_VERSION);
            assert_eq!(diag.skill, skill);
            assert_eq!(diag.cause, tag);
            assert_eq!(diag.source, DiagnosisSource::SyntheticFallback);
            assert!(!diag.success_criteria.is_empty());
            assert!(!diag.headline.is_empty());
            assert!(!diag.findings.is_empty());
        }
    }

    #[tokio::test]
    async fn synthetic_executor_headline_mentions_magnitude() {
        // Operators rely on the headline to see the scale of the signal.
        // Pin the contract so a future refactor doesn't silently strip
        // the count / pressure number from the prompt.
        let exec = SyntheticSkillDiagnosisExecutor;
        let stall = exec
            .run(&AutoInvokeRequest {
                skill: "analyze_session",
                focus: "stalls",
                cause: AutoInvokeCause::SessionStalls { count: 7 },
            })
            .await
            .and_then(|r| SkillDiagnosis::parse_from_skill_output(&r))
            .unwrap();
        assert!(stall.headline.contains("7"), "got: {}", stall.headline);

        let pressure = exec
            .run(&AutoInvokeRequest {
                skill: "optimize_prompt",
                focus: "budget",
                cause: AutoInvokeCause::BudgetPressure { level: 0.95 },
            })
            .await
            .and_then(|r| SkillDiagnosis::parse_from_skill_output(&r))
            .unwrap();
        assert!(
            pressure.headline.contains("95"),
            "got: {}",
            pressure.headline
        );
    }

    #[tokio::test]
    async fn synthetic_executor_wired_into_handler_closes_the_loop() {
        // End-to-end: plug SyntheticSkillDiagnosisExecutor into AutoInvokeHandler
        // and confirm the gate → executor → parse → return pipeline
        // actually produces valid diagnoses from real signals.
        let exec: Arc<dyn SkillExecutor> = Arc::new(SyntheticSkillDiagnosisExecutor);
        let mut handler = AutoInvokeHandler::new(exec);

        let mut signals = SessionSignals::default();
        signals.session_stalls = 5;
        signals.recent_corrections = 5;

        let out = handler.maybe_fire(&signals, Instant::now()).await;
        let skills: std::collections::HashSet<&str> =
            out.iter().map(|d| d.skill.as_str()).collect();
        assert!(skills.contains("analyze_session"));
        assert!(skills.contains("evaluate_session"));
        for diag in &out {
            assert_eq!(diag.schema_version, SKILL_DIAGNOSIS_SCHEMA_VERSION);
            assert_eq!(diag.source, DiagnosisSource::SyntheticFallback);
            assert!(!diag.headline.is_empty());
        }
    }

    #[test]
    fn diagnosis_outcome_tracker_succeeds_when_budget_pressure_drops() {
        let cause = AutoInvokeCause::BudgetPressure { level: 0.95 };
        let diag = SkillDiagnosis::new_with_criteria(
            "optimize_prompt",
            &cause,
            "budget high",
            [],
            [DiagnosisCriterion {
                metric: DiagnosisMetric::BudgetPressure,
                operator: DiagnosisOperator::Lte,
                threshold: 0.85,
                window_turns: 3,
                description: "pressure below threshold".into(),
            }],
            Some("trim prompt".into()),
            DiagnosisSource::RealSkill,
        );
        let mut tracker = DiagnosisOutcomeTracker::new();
        tracker.activate(diag, signals(0, 0.95, 0), 10);
        let outcomes = tracker.evaluate_turn(&signals(0, 0.80, 0), 11);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(
            outcomes[0].statuses,
            vec![DiagnosisCriterionStatus::Satisfied]
        );
    }

    #[test]
    fn diagnosis_outcome_tracker_fails_when_window_expires() {
        let cause = AutoInvokeCause::SessionStalls { count: 3 };
        let diag = SkillDiagnosis::new_with_criteria(
            "analyze_session",
            &cause,
            "stalls",
            [],
            [DiagnosisCriterion {
                metric: DiagnosisMetric::SessionStallsDelta,
                operator: DiagnosisOperator::Lte,
                threshold: 0.0,
                window_turns: 2,
                description: "no more stalls".into(),
            }],
            None,
            DiagnosisSource::RealSkill,
        );
        let mut tracker = DiagnosisOutcomeTracker::new();
        // Baseline: 5 stalls. Current: 6 stalls → delta 1 > 0, NOT ≤ 0 → not satisfied.
        tracker.activate(diag, signals(5, 0.1, 0), 1);
        assert!(tracker.evaluate_turn(&signals(6, 0.1, 0), 2).is_empty());
        // After window (window_turns=2, elapsed=2) → Failed.
        let outcomes = tracker.evaluate_turn(&signals(6, 0.1, 0), 3);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].statuses, vec![DiagnosisCriterionStatus::Failed]);
    }

    #[tokio::test]
    async fn compute_signals_end_to_end_with_handler_fires_stall_and_corrections() {
        // Observability populated with stall + corrections must drive the
        // handler to fire analyze_session + evaluate_session (budget
        // pressure requires a ContextAssemblyTrace, which is verified in
        // its own unit test above).
        use crate::observability_integration::ObservabilitySession;
        let mut obs = ObservabilitySession::new_simple("s-p7b-e2e");
        for _ in 0..5 {
            obs.record_stall_event();
        }
        obs.user_corrections = vec![1, 2, 3, 4, 5];

        let signals = compute_session_signals(Some(&obs));
        assert!(signals.session_stalls >= 5);
        assert!(signals.recent_corrections >= 5);

        let exec = Arc::new(
            StubExecutor::new()
                .with(
                    "analyze_session",
                    Some(well_formed_reply(
                        "analyze_session",
                        "session_stalls",
                        "stalls",
                    )),
                )
                .with(
                    "evaluate_session",
                    Some(well_formed_reply(
                        "evaluate_session",
                        "repeated_corrections",
                        "corrections",
                    )),
                ),
        );
        let mut h = AutoInvokeHandler::new(exec);

        let out = h.maybe_fire(&signals, Instant::now()).await;
        let skills: std::collections::HashSet<&str> =
            out.iter().map(|d| d.skill.as_str()).collect();
        assert!(skills.contains("analyze_session"));
        assert!(skills.contains("evaluate_session"));
    }

    // ── R5: unwired metric fail-safe ────────────────────────────────────────

    #[test]
    fn unwired_metrics_stay_pending_then_fail_after_window() {
        // ToolCallCount and UnmetPostconditionsDelta are not yet wired to
        // SessionSignals. The evaluate_criterion function must return
        // Pending for these, and the tracker must resolve them as Failed
        // once window_turns elapse. This is the fail-safe: unwired metrics
        // never claim success.
        use astra_skills::auto_invoke::{DiagnosisCriterion, DiagnosisMetric, DiagnosisOperator};

        let baseline = SessionSignals::default();
        let current = SessionSignals::default();

        for metric in [
            DiagnosisMetric::ToolCallCount,
            DiagnosisMetric::UnmetPostconditionsDelta,
        ] {
            let criterion = DiagnosisCriterion {
                metric,
                operator: DiagnosisOperator::Lte,
                threshold: 0.0,
                window_turns: 3,
                description: "test".into(),
            };

            // Within window → Pending (not Satisfied, not Failed).
            let status = evaluate_criterion(&criterion, &baseline, &current, 1);
            assert_eq!(
                status,
                DiagnosisCriterionStatus::Pending,
                "{metric:?} should be Pending within window"
            );

            // After window expires → must transition to Failed so the
            // tracker can remove the entry from `active`. Without this,
            // the diagnosis would livelock in the active set forever.
            let after_window = evaluate_criterion(&criterion, &baseline, &current, 5);
            assert_eq!(
                after_window,
                DiagnosisCriterionStatus::Failed,
                "{metric:?} must become Failed after window expires"
            );
        }
    }

    #[test]
    fn tracker_removes_entry_with_unwired_metric_after_window() {
        // The load-bearing invariant: a diagnosis containing an unwired
        // metric must NOT stay in the tracker's active set forever.
        // After window_turns elapse, evaluate_turn must produce an
        // outcome (with Failed status) and remove the entry.
        use astra_skills::auto_invoke::{DiagnosisCriterion, DiagnosisMetric, DiagnosisOperator};

        let mut tracker = DiagnosisOutcomeTracker::new();
        let baseline = SessionSignals::default();

        // Build a diagnosis with ONE unwired criterion.
        let mut diag = SkillDiagnosis::new(
            "test_skill",
            &astra_skills::auto_invoke::AutoInvokeCause::SessionStalls { count: 3 },
            "test",
            Vec::<String>::new(),
            None,
        );
        diag.success_criteria = vec![DiagnosisCriterion {
            metric: DiagnosisMetric::ToolCallCount, // unwired
            operator: DiagnosisOperator::Lte,
            threshold: 5.0,
            window_turns: 3,
            description: "tool calls should decrease".into(),
        }];

        tracker.activate(diag, baseline, 0);

        // Turn 1-2: within window → no outcome yet.
        let outcomes = tracker.evaluate_turn(&baseline, 2);
        assert!(outcomes.is_empty(), "within window → no outcome");

        // Turn 4: past window → must produce outcome with Failed.
        let outcomes = tracker.evaluate_turn(&baseline, 4);
        assert_eq!(outcomes.len(), 1, "past window → must complete");
        assert!(outcomes[0].complete);
        assert!(
            outcomes[0]
                .statuses
                .contains(&DiagnosisCriterionStatus::Failed),
            "unwired metric must resolve as Failed"
        );

        // Active set must be empty now.
        let again = tracker.evaluate_turn(&baseline, 5);
        assert!(again.is_empty(), "entry must be removed after completion");
    }

    #[test]
    fn wired_metrics_resolve_normally() {
        // Contrast: wired metrics DO resolve to Satisfied/Failed.
        use astra_skills::auto_invoke::{DiagnosisCriterion, DiagnosisMetric, DiagnosisOperator};

        let baseline = SessionSignals {
            session_stalls: 3,
            budget_pressure: 0.9,
            recent_corrections: 2,
            corrections_window: 10,
        };
        let current = SessionSignals {
            session_stalls: 3,     // no new stalls → delta 0
            budget_pressure: 0.5,  // pressure dropped
            recent_corrections: 2, // no new corrections
            corrections_window: 10,
        };

        // SessionStallsDelta <= 0 → Satisfied (delta is 0).
        let stall_crit = DiagnosisCriterion {
            metric: DiagnosisMetric::SessionStallsDelta,
            operator: DiagnosisOperator::Lte,
            threshold: 0.0,
            window_turns: 3,
            description: "stalls stop".into(),
        };
        assert_eq!(
            evaluate_criterion(&stall_crit, &baseline, &current, 1),
            DiagnosisCriterionStatus::Satisfied,
        );

        // BudgetPressure <= 0.85 → Satisfied (0.5 ≤ 0.85).
        let pressure_crit = DiagnosisCriterion {
            metric: DiagnosisMetric::BudgetPressure,
            operator: DiagnosisOperator::Lte,
            threshold: 0.85,
            window_turns: 3,
            description: "pressure drops".into(),
        };
        assert_eq!(
            evaluate_criterion(&pressure_crit, &baseline, &current, 1),
            DiagnosisCriterionStatus::Satisfied,
        );

        // CorrectionsDelta <= 0 → Satisfied (delta is 0).
        let corr_crit = DiagnosisCriterion {
            metric: DiagnosisMetric::CorrectionsDelta,
            operator: DiagnosisOperator::Lte,
            threshold: 0.0,
            window_turns: 3,
            description: "corrections stop".into(),
        };
        assert_eq!(
            evaluate_criterion(&corr_crit, &baseline, &current, 1),
            DiagnosisCriterionStatus::Satisfied,
        );
    }
}

// ── Unhappy-path coverage audit ─────────────────────────────────────

#[test]
fn evaluate_criterion_negative_delta_from_counter_reset() {
    // If a new session starts and baseline has higher stalls than
    // current (impossible in normal flow but defensive), saturating_sub
    // must produce 0, not underflow.
    use astra_skills::auto_invoke::{DiagnosisCriterion, DiagnosisMetric, DiagnosisOperator};
    let baseline = SessionSignals {
        session_stalls: 10,
        budget_pressure: 0.9,
        recent_corrections: 5,
        corrections_window: 10,
    };
    let current = SessionSignals {
        session_stalls: 3, // less than baseline (hypothetical reset)
        budget_pressure: 0.2,
        recent_corrections: 1,
        corrections_window: 10,
    };
    let criterion = DiagnosisCriterion {
        metric: DiagnosisMetric::SessionStallsDelta,
        operator: DiagnosisOperator::Lte,
        threshold: 0.0,
        window_turns: 5,
        description: "stalls stop".into(),
    };
    // saturating_sub(3, 10) = 0 → delta 0 ≤ 0 → Satisfied.
    let status = evaluate_criterion(&criterion, &baseline, &current, 1);
    assert_eq!(status, DiagnosisCriterionStatus::Satisfied);
}

#[test]
fn tracker_evaluate_on_empty_produces_no_outcomes() {
    let mut tracker = DiagnosisOutcomeTracker::new();
    let signals = SessionSignals::default();
    let outcomes = tracker.evaluate_turn(&signals, 5);
    assert!(outcomes.is_empty());
}

#[test]
fn tracker_handles_diagnosis_with_empty_success_criteria() {
    // A diagnosis with zero criteria should complete immediately
    // (all-non-Pending is vacuously true for an empty set).
    let mut tracker = DiagnosisOutcomeTracker::new();
    let baseline = SessionSignals::default();
    let mut diag = SkillDiagnosis::new(
        "test",
        &astra_skills::auto_invoke::AutoInvokeCause::SessionStalls { count: 1 },
        "h",
        Vec::<String>::new(),
        None,
    );
    diag.success_criteria = vec![]; // explicitly empty
    tracker.activate(diag, baseline, 0);

    let outcomes = tracker.evaluate_turn(&baseline, 1);
    assert_eq!(outcomes.len(), 1, "empty criteria → immediate completion");
    assert!(outcomes[0].complete);
    assert!(outcomes[0].statuses.is_empty());
}

#[test]
fn tracker_mixed_satisfied_and_failed_criteria() {
    use astra_skills::auto_invoke::{DiagnosisCriterion, DiagnosisMetric, DiagnosisOperator};
    let mut tracker = DiagnosisOutcomeTracker::new();
    let baseline = SessionSignals {
        session_stalls: 3,
        budget_pressure: 0.9,
        recent_corrections: 0,
        corrections_window: 10,
    };
    let mut diag = SkillDiagnosis::new(
        "test",
        &astra_skills::auto_invoke::AutoInvokeCause::SessionStalls { count: 3 },
        "h",
        Vec::<String>::new(),
        None,
    );
    diag.success_criteria = vec![
        DiagnosisCriterion {
            metric: DiagnosisMetric::SessionStallsDelta,
            operator: DiagnosisOperator::Lte,
            threshold: 0.0,
            window_turns: 2,
            description: "stalls stop".into(),
        },
        DiagnosisCriterion {
            metric: DiagnosisMetric::BudgetPressure,
            operator: DiagnosisOperator::Lte,
            threshold: 0.5,
            window_turns: 2,
            description: "pressure drops".into(),
        },
    ];
    tracker.activate(diag, baseline, 0);

    // Turn 3: stalls didn't increase (delta 0 ≤ 0 → Satisfied),
    // but pressure is still 0.85 (> 0.5 threshold, window=2, elapsed=3 ≥ 2 → Failed).
    let current = SessionSignals {
        session_stalls: 3, // no new
        budget_pressure: 0.85,
        recent_corrections: 0,
        corrections_window: 10,
    };
    let outcomes = tracker.evaluate_turn(&current, 3);
    assert_eq!(outcomes.len(), 1);
    assert!(outcomes[0].complete);
    assert!(
        outcomes[0]
            .statuses
            .contains(&DiagnosisCriterionStatus::Satisfied)
    );
    assert!(
        outcomes[0]
            .statuses
            .contains(&DiagnosisCriterionStatus::Failed)
    );
}

/// Verify the Arc<Mutex<AutoInvokeHandler>> pattern used at runtime
/// doesn't deadlock or lose results under concurrent access.
#[tokio::test]
async fn concurrent_access_via_arc_mutex() {
    let exec: Arc<dyn SkillExecutor> = Arc::new(SyntheticSkillDiagnosisExecutor);
    let handler = Arc::new(tokio::sync::Mutex::new(AutoInvokeHandler::new(exec)));

    let stall_signals = astra_skills::auto_invoke::SessionSignals {
        session_stalls: 5,
        budget_pressure: 0.0,
        recent_corrections: 0,
        corrections_window: 10,
    };

    // Spawn 4 tasks that all try to fire concurrently.
    let mut handles = Vec::new();
    for _ in 0..4 {
        let h = handler.clone();
        let s = stall_signals;
        handles.push(tokio::spawn(async move {
            let mut guard = h.lock().await;
            guard.maybe_fire(&s, std::time::Instant::now()).await
        }));
    }
    let mut total_diagnoses = 0usize;
    for jh in handles {
        total_diagnoses += jh.await.unwrap().len();
    }
    // Exactly 1 should fire (the first to acquire the lock); the rest
    // hit the cooldown. Tokio Mutex is fair, so ordering is deterministic
    // within the test, but the invariant is: at most 1 fire per cooldown.
    assert_eq!(
        total_diagnoses, 1,
        "cooldown must prevent duplicate fires under concurrent access"
    );
}

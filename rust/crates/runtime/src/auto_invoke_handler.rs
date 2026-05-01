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
    AutoInvokeGate, AutoInvokeRequest, SessionSignals, SkillDiagnosis,
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

/// Stub [`SkillExecutor`] that returns a canned `skill-diagnosis` block
/// for every request without invoking an LLM. Useful as a default in
/// session bootstrap so the auto-invoke loop exercises end-to-end (gate
/// → executor → diagnosis parse → prompt injection) even before a real
/// skill-invocation runner is wired in.
///
/// The generated headline + findings are deterministic functions of the
/// trigger cause, so operators get a useful "system noticed you stalled
/// N times" hint in the prompt without waiting for a cloud round-trip.
/// When a real skill runner lands, callers swap this out for it.
pub struct LoggingSkillExecutor;

#[async_trait]
impl SkillExecutor for LoggingSkillExecutor {
    async fn run(&self, req: &AutoInvokeRequest) -> Option<String> {
        use astra_skills::auto_invoke::AutoInvokeCause;
        let (headline, finding) = match &req.cause {
            AutoInvokeCause::ConsecutiveStalls { count } => (
                format!("agent stalled {count} times this session"),
                format!("pipeline detected {count} consecutive stall events"),
            ),
            AutoInvokeCause::BudgetPressure { level } => (
                format!("budget pressure at {:.0}%", level * 100.0),
                format!("context budget utilisation is {:.2}", level),
            ),
            AutoInvokeCause::RepeatedCorrections { count } => (
                format!("user issued {count} corrections this session"),
                format!("{count} distinct corrections recorded"),
            ),
        };
        let cause_tag = req.cause.as_str();
        // Hand-built JSON so the block stays valid when the cause tag
        // or skill name changes without us forgetting to requote.
        Some(format!(
            "```skill-diagnosis\n\
             {{\n  \
             \"schema_version\": 1,\n  \
             \"skill\": {skill},\n  \
             \"cause\": \"{cause_tag}\",\n  \
             \"headline\": {headline},\n  \
             \"findings\": [{finding}],\n  \
             \"recommended_action\": \"review the corresponding skill output for deeper analysis\"\n\
             }}\n\
             ```\n",
            skill = serde_json::Value::String(req.skill.to_string()),
            headline = serde_json::Value::String(headline),
            finding = serde_json::Value::String(finding),
        ))
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
/// - `consecutive_stalls` → `obs.stall_event_count`. This is cumulative,
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
        consecutive_stalls: session.stall_event_count,
        budget_pressure,
        recent_corrections,
        corrections_window: 10,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_skills::auto_invoke::{AutoInvokeCause, SKILL_DIAGNOSIS_SCHEMA_VERSION};
    use std::sync::Mutex;
    use std::time::Duration;

    // ── Helpers ─────────────────────────────────────────────────────────────

    fn signals(stalls: u32, pressure: f64, corrections: u32) -> SessionSignals {
        SessionSignals {
            consecutive_stalls: stalls,
            budget_pressure: pressure,
            recent_corrections: corrections,
            corrections_window: 10,
        }
    }

    fn well_formed_reply(skill: &str, cause: &str, headline: &str) -> String {
        format!(
            "# Analysis\n\nSome prose.\n\n```skill-diagnosis\n{{\n  \
             \"schema_version\": 1,\n  \"skill\": \"{skill}\",\n  \
             \"cause\": \"{cause}\",\n  \"headline\": \"{headline}\",\n  \
             \"findings\": [],\n  \"recommended_action\": \"try rg\"\n}}\n```\n",
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
                "consecutive_stalls",
                "agent looped on grep",
            )),
        ));
        let mut h = AutoInvokeHandler::new(exec.clone());

        let out = h.maybe_fire(&signals(3, 0.1, 0), Instant::now()).await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].skill, "analyze_session");
        assert_eq!(out[0].cause, "consecutive_stalls");
        assert_eq!(out[0].schema_version, SKILL_DIAGNOSIS_SCHEMA_VERSION);
        assert_eq!(out[0].headline, "agent looped on grep");

        let calls = exec.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].skill, "analyze_session");
        assert!(matches!(
            calls[0].cause,
            AutoInvokeCause::ConsecutiveStalls { count: 3 }
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
                        "consecutive_stalls",
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

        let out = h.maybe_fire(&signals(5, 0.95, 4), Instant::now()).await;
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

        let out = h.maybe_fire(&signals(3, 0.1, 0), Instant::now()).await;
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

        let out = h.maybe_fire(&signals(3, 0.1, 0), Instant::now()).await;
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn cooldown_prevents_double_fire_across_calls() {
        // The gate owns cooldowns — the handler must preserve them across
        // successive `maybe_fire` calls on the same instance.
        let exec = Arc::new(StubExecutor::new().with(
            "analyze_session",
            Some(well_formed_reply(
                "analyze_session",
                "consecutive_stalls",
                "h",
            )),
        ));
        let mut h = AutoInvokeHandler::new(exec.clone());

        let t0 = Instant::now();
        let first = h.maybe_fire(&signals(3, 0.1, 0), t0).await;
        assert_eq!(first.len(), 1);

        let second = h.maybe_fire(&signals(3, 0.1, 0), t0).await;
        assert!(second.is_empty(), "second fire inside cooldown must drop");
        assert_eq!(
            exec.calls().len(),
            1,
            "executor must not be invoked while the gate is on cooldown",
        );

        // After cooldown elapses (60s for stalls), a new fire should go through.
        let third = h
            .maybe_fire(&signals(3, 0.1, 0), t0 + Duration::from_secs(61))
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

        let out = h.maybe_fire(&signals(3, 0.95, 0), Instant::now()).await;
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
        assert_eq!(s.consecutive_stalls, 3);
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

    // ── LoggingSkillExecutor ────────────────────────────────────────────────

    #[tokio::test]
    async fn logging_executor_emits_parseable_diagnosis_for_each_cause() {
        // Every cause must round-trip: the stub's reply must parse back
        // into a SkillDiagnosis whose schema_version, skill, and cause
        // match the request. Otherwise the loop emits noise the prompt
        // renderer can't use.
        let exec = LoggingSkillExecutor;
        let cases = [
            (
                "analyze_session",
                AutoInvokeCause::ConsecutiveStalls { count: 4 },
                "consecutive_stalls",
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
            assert!(!diag.headline.is_empty());
            assert!(!diag.findings.is_empty());
        }
    }

    #[tokio::test]
    async fn logging_executor_headline_mentions_magnitude() {
        // Operators rely on the headline to see the scale of the signal.
        // Pin the contract so a future refactor doesn't silently strip
        // the count / pressure number from the prompt.
        let exec = LoggingSkillExecutor;
        let stall = exec
            .run(&AutoInvokeRequest {
                skill: "analyze_session",
                focus: "stalls",
                cause: AutoInvokeCause::ConsecutiveStalls { count: 7 },
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
    async fn logging_executor_wired_into_handler_closes_the_loop() {
        // End-to-end: plug LoggingSkillExecutor into AutoInvokeHandler
        // and confirm the gate → executor → parse → return pipeline
        // actually produces valid diagnoses from real signals.
        let exec: Arc<dyn SkillExecutor> = Arc::new(LoggingSkillExecutor);
        let mut handler = AutoInvokeHandler::new(exec);

        let mut signals = SessionSignals::default();
        signals.consecutive_stalls = 3;
        signals.recent_corrections = 3;

        let out = handler.maybe_fire(&signals, Instant::now()).await;
        let skills: std::collections::HashSet<&str> =
            out.iter().map(|d| d.skill.as_str()).collect();
        assert!(skills.contains("analyze_session"));
        assert!(skills.contains("evaluate_session"));
        for diag in &out {
            assert_eq!(diag.schema_version, SKILL_DIAGNOSIS_SCHEMA_VERSION);
            assert!(!diag.headline.is_empty());
        }
    }

    #[tokio::test]
    async fn compute_signals_end_to_end_with_handler_fires_stall_and_corrections() {
        // Observability populated with stall + corrections must drive the
        // handler to fire analyze_session + evaluate_session (budget
        // pressure requires a ContextAssemblyTrace, which is verified in
        // its own unit test above).
        use crate::observability_integration::ObservabilitySession;
        let mut obs = ObservabilitySession::new_simple("s-p7b-e2e");
        obs.record_stall_event();
        obs.record_stall_event();
        obs.record_stall_event();
        obs.user_corrections = vec![1, 2, 3];

        let signals = compute_session_signals(Some(&obs));
        assert!(signals.consecutive_stalls >= 3);
        assert!(signals.recent_corrections >= 3);

        let exec = Arc::new(
            StubExecutor::new()
                .with(
                    "analyze_session",
                    Some(well_formed_reply(
                        "analyze_session",
                        "consecutive_stalls",
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
}

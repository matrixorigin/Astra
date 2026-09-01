//! Suite orchestration with parallel execution, circuit breaker,
//! failure classification, and retry on rate-limit.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;

use crate::case::Case;
use crate::classify::{FailureClass, classify};
use crate::criteria::{
    Criterion, CriterionSeverity, evaluate_deterministic_with_session,
    requires_durable_run_binding, requires_session_capture,
};
use crate::digest::DigestCollector;
use crate::exec::CaseExecutor;
use crate::judger::{Judger, evaluate_judger};
use crate::model_profiles::{ModelReuseSupport, load_profiles};
use crate::report::{AttemptRecord, CaseRunReport, CaseRunStatus, StepResult, SuiteReport};
use crate::runner::{RunOutcome, RunnerConfig, resolve_models};
use crate::session_capture::{SessionCapture, load_session, load_session_for_owners};
use crate::session_identity::is_valid_server_session_id;

/// What to do with session journals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionCaptureMode {
    Never,
    OnDebugLog,
    Always,
}

impl SessionCaptureMode {
    fn should_load(self, case: &Case) -> bool {
        match self {
            Self::Never => false,
            Self::OnDebugLog => case.debug_log,
            Self::Always => true,
        }
    }
}

/// Hook for loading session captures.
pub trait SessionLoader: Send + Sync {
    fn load(&self, session_id: &str) -> Option<SessionCapture>;
}

/// Production loader — reads `~/.astra/sessions/<id>.jsonl`.
pub struct DiskSessionLoader;

impl SessionLoader for DiskSessionLoader {
    fn load(&self, session_id: &str) -> Option<SessionCapture> {
        load_session(session_id)
    }
}

/// Disk loader for runs whose local CLI and server artifacts use distinct,
/// explicitly authorized owner namespaces.
pub struct ScopedDiskSessionLoader {
    owner_scopes: Vec<astra_services::OwnerScope>,
}

impl ScopedDiskSessionLoader {
    pub fn new(owner_scopes: Vec<astra_services::OwnerScope>) -> Self {
        Self { owner_scopes }
    }
}

impl SessionLoader for ScopedDiskSessionLoader {
    fn load(&self, session_id: &str) -> Option<SessionCapture> {
        load_session_for_owners(session_id, &self.owner_scopes)
    }
}

/// Configuration for suite-level behavior.
#[derive(Debug, Clone)]
pub struct SuiteConfig {
    /// Max concurrent case executions.
    pub parallel: usize,
    /// Abort after N consecutive infra failures.
    pub circuit_breaker_threshold: usize,
    /// Retry rate-limited cases once after backoff.
    pub retry_on_429: bool,
    /// Run each (case, model) pair N times for flaky detection.
    pub runs: u32,
}

impl Default for SuiteConfig {
    fn default() -> Self {
        Self {
            parallel: 1,
            circuit_breaker_threshold: 3,
            retry_on_429: false,
            runs: 1,
        }
    }
}

/// Orchestrates the full pipeline.
pub struct SuiteRunner<'a> {
    pub executor: &'a dyn CaseExecutor,
    pub judger: &'a dyn Judger,
    pub session_loader: &'a dyn SessionLoader,
    pub digest_collector: Option<&'a dyn DigestCollector>,
    pub runner_cfg: RunnerConfig,
    pub no_judger: bool,
    pub session_mode: SessionCaptureMode,
    pub suite_cfg: SuiteConfig,
    pub dashboard_tx: Option<tokio::sync::broadcast::Sender<crate::dashboard::DashboardEvent>>,
    pub run_id: String,
    pub cancel_flag: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl<'a> SuiteRunner<'a> {
    /// Run every (case × model) pair with concurrency control and circuit breaker.
    pub async fn run_all(&self, cases: &[Case]) -> SuiteReport {
        let wall_start = std::time::Instant::now();
        let started_at = chrono::Utc::now().to_rfc3339();
        let run_id = self.run_id.clone();

        // Build the work items: (case, model, run_index) triples.
        let mut work: Vec<(&Case, String, u32)> = Vec::new();
        let mut unavailable: Vec<CaseRunReport> = Vec::new();
        for case in cases {
            match resolve_models(case, &self.runner_cfg) {
                Ok(models) => {
                    for m in models {
                        for i in 0..self.suite_cfg.runs {
                            work.push((case, m.clone(), i));
                        }
                    }
                }
                Err(e) => {
                    eprintln!(
                        "[astra-test] [UNAVAILABLE] case {:?}: model resolution failed: {e}",
                        case.name
                    );
                    for run_index in 0..self.suite_cfg.runs.max(1) {
                        unavailable.push(self.model_resolution_unavailable(
                            case,
                            &e.to_string(),
                            run_index,
                        ));
                    }
                }
            }
        }

        let unavailable_count = unavailable.len();
        let semaphore = Arc::new(Semaphore::new(self.suite_cfg.parallel));
        let aborted = Arc::new(AtomicBool::new(false));
        let consecutive_infra = Arc::new(AtomicUsize::new(0));
        let total_auth_failures = Arc::new(AtomicUsize::new(0));

        let mut suite = SuiteReport {
            runs: unavailable,
            started_at: Some(started_at.clone()),
            ..Default::default()
        };

        if let Some(ref tx) = self.dashboard_tx {
            use std::collections::BTreeSet;
            let models: Vec<String> = work
                .iter()
                .map(|(_, m, _)| m.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            let _ = tx.send(crate::dashboard::DashboardEvent::SuiteStarted {
                run_id: run_id.clone(),
                total_cases: work.len() + unavailable_count,
                models,
                started_at,
                source: "suite".into(),
                sequence: crate::dashboard::next_dashboard_event_sequence(),
            });
            // Model-resolution failures have no executable work item, but
            // they are still authoritative unavailable rows. Publish them
            // through the same completion stream so live dashboards cannot
            // silently drop them from their counters.
            for report in &suite.runs {
                let _ = tx.send(crate::dashboard::DashboardEvent::CaseCompleted {
                    run_id: run_id.clone(),
                    report: Arc::new(report.clone()),
                    sequence: crate::dashboard::next_dashboard_event_sequence(),
                });
            }
        }

        // Publish the complete admission queue before the first permit is
        // acquired.  A user can now distinguish "waiting for a slot" from
        // "the model call is running" even when suite parallelism is 1.
        if let Some(ref tx) = self.dashboard_tx {
            for (queue_index, (case, model, run_index)) in work.iter().enumerate() {
                let _ = tx.send(crate::dashboard::DashboardEvent::CaseQueued {
                    run_id: run_id.clone(),
                    case_name: case.name.clone(),
                    model: model.clone(),
                    run_index: *run_index,
                    queue_position: queue_index + 1,
                    sequence: crate::dashboard::next_dashboard_event_sequence(),
                });
            }
        }

        if self.suite_cfg.parallel <= 1 {
            // Serial path: simpler, preserves ordering, supports circuit breaker inline.
            let mut stop_at = None;
            for (index, (case, model, run_index)) in work.iter().enumerate() {
                if self
                    .cancel_flag
                    .as_ref()
                    .is_some_and(|f| f.load(Ordering::Relaxed))
                {
                    eprintln!("[astra-test] run cancelled by user");
                    stop_at = Some((index, "cancelled by user"));
                    break;
                }
                if aborted.load(Ordering::Relaxed) {
                    eprintln!("[astra-test] circuit breaker tripped — aborting remaining cases");
                    stop_at = Some((index, "circuit breaker"));
                    break;
                }
                if let Some(ref tx) = self.dashboard_tx {
                    let _ = tx.send(crate::dashboard::DashboardEvent::CaseStarted {
                        run_id: run_id.clone(),
                        case_name: case.name.clone(),
                        model: model.clone(),
                        run_index: *run_index,
                        sequence: crate::dashboard::next_dashboard_event_sequence(),
                    });
                }
                let mut report = self
                    .run_one_with_progress(case, model, &run_id, *run_index)
                    .await;
                report.run_index = *run_index;
                self.update_circuit_breaker(
                    &report,
                    &consecutive_infra,
                    &aborted,
                    &total_auth_failures,
                );
                if let Some(ref tx) = self.dashboard_tx {
                    let _ = tx.send(crate::dashboard::DashboardEvent::CaseCompleted {
                        run_id: run_id.clone(),
                        report: Arc::new(report.clone()),
                        sequence: crate::dashboard::next_dashboard_event_sequence(),
                    });
                }
                suite.runs.push(report);
            }
            if let Some((start, reason)) = stop_at {
                for (case, model, run_index) in work.into_iter().skip(start) {
                    let report = self.cancelled_case_report(case, &model, run_index, reason);
                    if let Some(ref tx) = self.dashboard_tx {
                        let _ = tx.send(crate::dashboard::DashboardEvent::CaseCompleted {
                            run_id: run_id.clone(),
                            report: Arc::new(report.clone()),
                            sequence: crate::dashboard::next_dashboard_event_sequence(),
                        });
                    }
                    suite.runs.push(report);
                }
            }
        } else {
            // Parallel path: use FuturesUnordered with semaphore.
            use futures::stream::{FuturesUnordered, StreamExt};

            let futures: FuturesUnordered<_> = work
                .into_iter()
                .map(|(case, model, run_index)| {
                    let sem = semaphore.clone();
                    let aborted = aborted.clone();
                    let consecutive_infra = consecutive_infra.clone();
                    let total_auth_failures = total_auth_failures.clone();
                    let dashboard_tx = self.dashboard_tx.clone();
                    let run_id = self.run_id.clone();
                    let cancel_flag = self.cancel_flag.clone();
                    async move {
                        if cancel_flag
                            .as_ref()
                            .is_some_and(|f| f.load(Ordering::Relaxed))
                        {
                            let report = self.cancelled_case_report(
                                case,
                                &model,
                                run_index,
                                "cancelled before execution",
                            );
                            if let Some(ref tx) = dashboard_tx {
                                let _ = tx.send(crate::dashboard::DashboardEvent::CaseCompleted {
                                    run_id: run_id.clone(),
                                    report: Arc::new(report.clone()),
                                    sequence: crate::dashboard::next_dashboard_event_sequence(),
                                });
                            }
                            return report;
                        }
                        if aborted.load(Ordering::Relaxed) {
                            let report = self.cancelled_case_report(
                                case,
                                &model,
                                run_index,
                                "circuit breaker before execution",
                            );
                            if let Some(ref tx) = dashboard_tx {
                                let _ = tx.send(crate::dashboard::DashboardEvent::CaseCompleted {
                                    run_id: run_id.clone(),
                                    report: Arc::new(report.clone()),
                                    sequence: crate::dashboard::next_dashboard_event_sequence(),
                                });
                            }
                            return report;
                        }
                        let _permit = match sem.acquire().await {
                            Ok(permit) => permit,
                            Err(_) => {
                                return self.cancelled_case_report(
                                    case,
                                    &model,
                                    run_index,
                                    "execution semaphore closed",
                                );
                            }
                        };
                        // Cancellation can happen while a work item is
                        // queued. Never launch it merely because a permit
                        // became available after cancellation.
                        if cancel_flag
                            .as_ref()
                            .is_some_and(|f| f.load(Ordering::Relaxed))
                        {
                            let report = self.cancelled_case_report(
                                case,
                                &model,
                                run_index,
                                "cancelled while waiting for execution",
                            );
                            if let Some(ref tx) = dashboard_tx {
                                let _ = tx.send(crate::dashboard::DashboardEvent::CaseCompleted {
                                    run_id: run_id.clone(),
                                    report: Arc::new(report.clone()),
                                    sequence: crate::dashboard::next_dashboard_event_sequence(),
                                });
                            }
                            return report;
                        }
                        if aborted.load(Ordering::Relaxed) {
                            let report = self.cancelled_case_report(
                                case,
                                &model,
                                run_index,
                                "circuit breaker while waiting for execution",
                            );
                            if let Some(ref tx) = dashboard_tx {
                                let _ = tx.send(crate::dashboard::DashboardEvent::CaseCompleted {
                                    run_id: run_id.clone(),
                                    report: Arc::new(report.clone()),
                                    sequence: crate::dashboard::next_dashboard_event_sequence(),
                                });
                            }
                            return report;
                        }
                        if let Some(ref tx) = dashboard_tx {
                            let _ = tx.send(crate::dashboard::DashboardEvent::CaseStarted {
                                run_id: run_id.clone(),
                                case_name: case.name.clone(),
                                model: model.clone(),
                                run_index,
                                sequence: crate::dashboard::next_dashboard_event_sequence(),
                            });
                        }
                        let mut report = self
                            .run_one_with_progress(case, &model, &run_id, run_index)
                            .await;
                        report.run_index = run_index;
                        self.update_circuit_breaker(
                            &report,
                            &consecutive_infra,
                            &aborted,
                            &total_auth_failures,
                        );
                        if let Some(ref tx) = dashboard_tx {
                            let _ = tx.send(crate::dashboard::DashboardEvent::CaseCompleted {
                                run_id: run_id.clone(),
                                report: Arc::new(report.clone()),
                                sequence: crate::dashboard::next_dashboard_event_sequence(),
                            });
                        }
                        report
                    }
                })
                .collect();

            let results: Vec<_> = futures.collect().await;
            for r in results {
                suite.runs.push(r);
            }
            // Stabilize ordering: sort by (case_name, model, run_index)
            // so parallel execution doesn't produce non-deterministic diffs.
            suite.runs.sort_by(|a, b| {
                (&a.case_name, &a.model, a.run_index).cmp(&(&b.case_name, &b.model, b.run_index))
            });
        }

        suite.wall_time_ms = wall_start.elapsed().as_millis() as u64;
        suite.ended_at = Some(chrono::Utc::now().to_rfc3339());
        // The dashboard entry point publishes the terminal event after it
        // commits its authoritative snapshot. Emitting it here would let a
        // browser observe completion while REST/reconnect state still points
        // at the previous run.
        suite
    }

    fn update_circuit_breaker(
        &self,
        report: &CaseRunReport,
        consecutive_infra: &AtomicUsize,
        aborted: &AtomicBool,
        total_auth_failures: &AtomicUsize,
    ) {
        if report.is_passed() {
            consecutive_infra.store(0, Ordering::Relaxed);
            return;
        }
        if let Some(ref class) = report.failure_class {
            if matches!(class, FailureClass::InfraAuth) {
                let auth_count = total_auth_failures.fetch_add(1, Ordering::Relaxed) + 1;
                if auth_count == 2 {
                    eprintln!(
                        "[astra-test] WARNING: {auth_count} auth failures — credentials \
                         may have expired mid-run. Remaining cases against this provider \
                         will likely fail. Consider aborting (Ctrl-C) and re-running with \
                         fresh credentials."
                    );
                }
            }
            let is_infra = matches!(
                class,
                FailureClass::InfraAuth
                    | FailureClass::InfraRuntime
                    | FailureClass::InfraQuota
                    | FailureClass::InfraTimeout
                    | FailureClass::InfraModelInactive
                    | FailureClass::InfraProviderError { .. }
                    | FailureClass::InfraRateLimit
            );
            if is_infra {
                let count = consecutive_infra.fetch_add(1, Ordering::Relaxed) + 1;
                if count >= self.suite_cfg.circuit_breaker_threshold {
                    eprintln!(
                        "[astra-test] circuit breaker: {} consecutive infra failures (class: {class})",
                        count
                    );
                    aborted.store(true, Ordering::Relaxed);
                }
            } else {
                consecutive_infra.store(0, Ordering::Relaxed);
            }
        }
    }

    async fn load_session_until_settled(
        &self,
        session_id: &str,
        settled_subsystem: Option<&str>,
    ) -> Option<SessionCapture> {
        let deadline = tokio::time::Instant::now() + self.runner_cfg.session_settle_timeout;
        let mut latest = None;
        loop {
            if let Some(capture) = self.session_loader.load(session_id) {
                let settled = settled_subsystem
                    .is_none_or(|expected| capture.subsystem_settled_for_latest_turn(expected));
                if settled {
                    return Some(capture);
                }
                latest = Some(capture);
            }
            if settled_subsystem.is_none()
                || self.runner_cfg.session_settle_timeout.is_zero()
                || tokio::time::Instant::now() >= deadline
            {
                return latest;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn run_one(&self, case: &Case, model: &str) -> CaseRunReport {
        if let Some(report) = self.skip_for_unsupported_cache_scope(case, model) {
            eprintln!(
                "[astra-test] [UNAVAILABLE] {} × {} (unsupported cache scope)",
                case.name, model
            );
            return report;
        }

        // Setup is part of the case lifecycle, not a branch that may skip
        // teardown. Preserve the setup failure, then flow through the same
        // finalization path so shared state is always cleaned up.
        let setup_error = if let Some(ref cmd) = case.setup_cmd {
            match self.run_shell_hook("setup_cmd", case, cmd).await {
                Ok(()) => None,
                Err(error) => {
                    eprintln!("[astra-test] {error}");
                    Some(error)
                }
            }
        } else {
            None
        };
        let setup_failed = setup_error.is_some();
        let invocation_started_at = chrono::Utc::now();
        let mut outcome = if let Some(error) = &setup_error {
            RunOutcome::new(model)
                .with_text("setup_cmd failed")
                .with_stderr(format!("[astra-test] {error}"))
        } else {
            self.executor.execute(case, model).await
        };
        let mut attempts = vec![AttemptRecord {
            attempt_index: 0,
            outcome: outcome.clone(),
        }];
        let mut step_results: Vec<StepResult> = Vec::new();
        let mut lifecycle_errors = Vec::new();
        if let Some(error) = &setup_error {
            lifecycle_errors.push(format!("setup command failed: {error}"));
        }

        // Multi-turn: execute follow-up steps using the same server-issued
        // session. Identity validity is a lifecycle invariant, not a case
        // criterion: comparing two attacker-controlled strings is insufficient.
        let root_session_is_valid = outcome
            .session_id
            .as_deref()
            .is_some_and(is_valid_server_session_id);
        if !setup_failed && !case.steps.is_empty() && outcome.session_id.is_none() {
            eprintln!(
                "[astra-test] WARNING: case {} has {} steps but turn 1 returned no session_id — \
                 steps will be skipped. This usually means the first turn failed.",
                case.name,
                case.steps.len()
            );
            lifecycle_errors
                .push("follow-up turns require the root turn's server-issued session_id".into());
        } else if !setup_failed && !case.steps.is_empty() && !root_session_is_valid {
            let invalid_id = outcome.session_id.as_deref().unwrap_or("<missing>");
            lifecycle_errors.push(format!(
                "follow-up turns require a valid server-issued UUID session_id (got {invalid_id:?})"
            ));
        }
        if !setup_failed
            && !case.steps.is_empty()
            && root_session_is_valid
            && let Some(ref session_id) = outcome.session_id
        {
            for (idx, step) in case.steps.iter().enumerate() {
                let step_case = Case {
                    name: format!("{}__step{}", case.name, idx),
                    description: None,
                    prompt: step.prompt.clone(),
                    prompt_variants: vec![],
                    models: Some(vec![model.to_string()]),
                    criteria: vec![],
                    debug_log: case.debug_log,
                    extra_cli_args: {
                        let mut args = case.extra_cli_args.clone();
                        args.push("--session-id".into());
                        args.push(session_id.clone());
                        args
                    },
                    timeout_seconds: step.timeout_seconds.unwrap_or(case.timeout_seconds),
                    capability: None,
                    required_cache_scope: None,
                    difficulty: None,
                    weight: 1.0,
                    steps: vec![],
                    cli_env: case.cli_env.clone(),
                    setup_cmd: None,
                    teardown_cmd: None,
                    cleanup_memory_records: false,
                };
                let step_outcome = self.executor.execute(&step_case, model).await;

                // Evaluate step-level criteria against this step's outcome.
                let mut step_criteria_results = if !step.criteria.is_empty() {
                    evaluate_deterministic_with_session(&step.criteria, &step_outcome, None)
                } else {
                    vec![]
                };
                // Step criteria are first-class assertions. In particular, a
                // required judger on the final step must be evaluated rather
                // than left as the deterministic placeholder (which would
                // otherwise make every hard_judger step fail regardless of
                // the model's actual result).
                if !self.no_judger {
                    for (criterion_index, criterion) in step.criteria.iter().enumerate() {
                        if matches!(
                            criterion,
                            Criterion::Judger { .. } | Criterion::HardJudger { .. }
                        ) && let Some(result) =
                            evaluate_judger(self.judger, criterion, &step_outcome).await
                        {
                            step_criteria_results[criterion_index] = result;
                        }
                    }
                } else {
                    for (criterion_index, criterion) in step.criteria.iter().enumerate() {
                        match criterion {
                            Criterion::Judger { .. } => {
                                step_criteria_results[criterion_index].passed = true;
                                step_criteria_results[criterion_index].detail =
                                    "judger skipped (--no-judger)".into();
                            }
                            Criterion::HardJudger { .. } => {
                                step_criteria_results[criterion_index].passed = false;
                                step_criteria_results[criterion_index].detail =
                                    "required judger unavailable (--no-judger)".into();
                            }
                            _ => {}
                        }
                    }
                }
                let mut step_lifecycle_ok = true;
                if step_outcome.exit_code != 0 {
                    step_lifecycle_ok = false;
                    lifecycle_errors.push(format!(
                        "step {idx} did not reach a successful terminal outcome (exit_code={})",
                        step_outcome.exit_code
                    ));
                }
                match step_outcome.session_id.as_deref() {
                    Some(actual) if !is_valid_server_session_id(actual) => {
                        step_lifecycle_ok = false;
                        lifecycle_errors.push(format!(
                            "step {idx} returned an invalid server-issued UUID session_id (got {actual:?})"
                        ));
                    }
                    Some(actual) if actual != session_id => {
                        step_lifecycle_ok = false;
                        lifecycle_errors.push(format!(
                            "step {idx} session identity diverged (expected {}, got {:?})",
                            session_id, step_outcome.session_id
                        ));
                    }
                    None => {
                        step_lifecycle_ok = false;
                        lifecycle_errors.push(format!(
                            "step {idx} session identity diverged (expected {}, got None)",
                            session_id
                        ));
                    }
                    _ => {}
                }
                let step_passed = step_lifecycle_ok
                    && step_criteria_results
                        .iter()
                        .filter(|r| r.severity == CriterionSeverity::Hard)
                        .all(|r| r.passed);

                step_results.push(StepResult {
                    step_index: idx as u32,
                    prompt: step.prompt.clone(),
                    outcome: step_outcome.clone(),
                    duration_ms: step_outcome.duration_ms,
                    criteria: step_criteria_results,
                    passed: step_passed,
                });

                // Merge step outcome into main outcome.
                outcome.completion_tokens += step_outcome.completion_tokens;
                outcome.prompt_tokens += step_outcome.prompt_tokens;
                outcome.cached_input_tokens += step_outcome.cached_input_tokens;
                outcome.cache_creation_tokens += step_outcome.cache_creation_tokens;
                outcome.duration_ms += step_outcome.duration_ms;
                outcome.tool_calls_count += step_outcome.tool_calls_count;
                outcome.tools_used.extend(step_outcome.tools_used);
                outcome.turn_rounds += step_outcome.turn_rounds;
                outcome.total_tool_calls += step_outcome.total_tool_calls;
                outcome.cache_hits += step_outcome.cache_hits;
                // Accumulate text across turns so global criteria
                // (text_contains, judger) see ALL steps, not just the
                // last one. Previous behavior silently dropped earlier
                // turns' output, causing text_contains to miss content
                // from turn 0 or intermediate steps.
                if !step_outcome.text.is_empty() {
                    if !outcome.text.is_empty() {
                        outcome.text.push_str("\n\n");
                    }
                    outcome.text.push_str(&step_outcome.text);
                }
                // First non-zero exit_code wins. A step that fails
                // should not be masked by a later step that succeeds.
                if outcome.exit_code == 0 {
                    outcome.exit_code = step_outcome.exit_code;
                }
                if !step_outcome.stderr.is_empty() {
                    outcome.stderr.push('\n');
                    outcome.stderr.push_str(&step_outcome.stderr);
                }
            }
        }

        // Retry on a typed rate-limit terminal only when the first attempt
        // produced no session, turn, or tool evidence. Once a server-owned
        // session exists, retrying would abandon its durable side effects and
        // health/cleanup obligations; fail closed and retain that attempt.
        if self.suite_cfg.retry_on_429 && case.steps.is_empty() && is_rate_limited(&outcome) {
            let has_first_attempt_evidence = outcome.session_id.is_some()
                || outcome.run_id.is_some()
                || outcome.tool_calls_count > 0
                || outcome.turn_rounds > 0;
            if has_first_attempt_evidence {
                lifecycle_errors.push(
                    "rate-limit retry refused after first attempt produced session or durable evidence"
                        .into(),
                );
            } else {
                eprintln!(
                    "[astra-test] typed rate-limit on case={} model={}, retrying after 5s",
                    case.name, model
                );
                let first_attempt = outcome.clone();
                tokio::time::sleep(Duration::from_secs(5)).await;
                outcome = self.executor.execute(case, model).await;
                attempts.push(AttemptRecord {
                    attempt_index: 1,
                    outcome: outcome.clone(),
                });
                // Preserve total cost/latency across attempts. The first
                // attempt had no server identity, so no durable cleanup is
                // being dropped; its complete terminal receipt remains in
                // `attempts` for audit.
                outcome.prompt_tokens = outcome
                    .prompt_tokens
                    .saturating_add(first_attempt.prompt_tokens);
                outcome.completion_tokens = outcome
                    .completion_tokens
                    .saturating_add(first_attempt.completion_tokens);
                outcome.cached_input_tokens = outcome
                    .cached_input_tokens
                    .saturating_add(first_attempt.cached_input_tokens);
                outcome.cache_creation_tokens = outcome
                    .cache_creation_tokens
                    .saturating_add(first_attempt.cache_creation_tokens);
                outcome.duration_ms = outcome
                    .duration_ms
                    .saturating_add(first_attempt.duration_ms);
                if !first_attempt.stderr.is_empty() {
                    outcome.stderr = format!(
                        "[attempt 0 stderr]\n{}\n[attempt 1 stderr]\n{}",
                        first_attempt.stderr, outcome.stderr
                    );
                }
            }
        }

        if outcome.exit_code != 0 {
            lifecycle_errors.push(format!(
                "root turn did not reach a successful terminal outcome (exit_code={})",
                outcome.exit_code
            ));
        }
        if case.steps.is_empty() {
            match outcome.session_id.as_deref() {
                Some(session_id) if !is_valid_server_session_id(session_id) => {
                    lifecycle_errors.push(format!(
                        "root turn returned an invalid server-issued UUID session_id (got {session_id:?})"
                    ));
                }
                None => lifecycle_errors
                    .push("root turn did not return the server-issued UUID session_id".into()),
                _ => {}
            }
        }
        for error in &lifecycle_errors {
            if !outcome.stderr.is_empty() {
                outcome.stderr.push('\n');
            }
            outcome.stderr.push_str("[astra-test] lifecycle: ");
            outcome.stderr.push_str(error);
        }

        // A visible answer cannot certify product health while asynchronous
        // work is failing out of band. This invariant belongs to the runner,
        // not individual YAML authors or one particular CLI/dashboard entry.
        let mut criteria = case.criteria.clone();
        if let Some(scope) = case.required_cache_scope {
            criteria.push(Criterion::PromptCacheReuseScope { scope });
        }
        if self.runner_cfg.require_session_subsystem_health
            && !criteria
                .iter()
                .any(|criterion| matches!(criterion, Criterion::SessionSubsystemHealthy { .. }))
        {
            criteria.push(Criterion::SessionSubsystemHealthy {
                settled_subsystem: Some("post_loop_memory".into()),
            });
        }
        let settled_subsystem = criteria.iter().find_map(|criterion| match criterion {
            Criterion::SessionSubsystemHealthy { settled_subsystem } => {
                settled_subsystem.as_deref()
            }
            _ => None,
        });

        // Load session whenever a criterion needs durable evidence. Keeping
        // this decision beside the effective criteria prevents entry points
        // from accidentally running the health gate without its evidence.
        let mut session = if self.session_mode.should_load(case)
            || case.cleanup_memory_records
            || requires_session_capture(&criteria)
        {
            if let Some(session_id) = outcome.session_id.as_deref() {
                self.load_session_until_settled(session_id, settled_subsystem)
                    .await
            } else {
                None
            }
        } else {
            None
        };
        // Every executed invocation must identify itself. Never use
        // filter_map here: a missing root/step identity must not disappear
        // merely because another step returned a valid run_id.
        let mut invocation_run_ids = Vec::with_capacity(step_results.len() + 1);
        let mut missing_invocation_id = None;
        match outcome.run_id.as_deref() {
            Some(run_id) if !run_id.trim().is_empty() => {
                invocation_run_ids.push(run_id.to_string())
            }
            _ => missing_invocation_id = Some("root terminal outcome has no run_id".to_string()),
        }
        for (idx, step) in step_results.iter().enumerate() {
            match step.outcome.run_id.as_deref() {
                Some(run_id) if !run_id.trim().is_empty() => {
                    invocation_run_ids.push(run_id.to_string())
                }
                _ => {
                    missing_invocation_id =
                        Some(format!("step {idx} terminal outcome has no run_id"));
                }
            }
        }
        invocation_run_ids.sort_unstable();
        invocation_run_ids.dedup();

        if requires_durable_run_binding(&criteria) {
            let binding_error = if let Some(error) = missing_invocation_id.clone() {
                Some(format!(
                    "hard durable evidence requires every invocation identity: {error}"
                ))
            } else if invocation_run_ids.is_empty() {
                Some("hard durable evidence requires terminal run_id(s)".to_string())
            } else if session.is_none() {
                Some("hard durable evidence unavailable: no session capture".to_string())
            } else {
                invocation_run_ids.iter().find_map(|run_id| {
                    (!session.as_ref().is_some_and(|capture| {
                        capture.has_canonical_run_evidence_since(run_id, invocation_started_at)
                    }))
                    .then(|| {
                        format!(
                            "captured session has no canonical turn for terminal run_id {run_id:?} created during this invocation"
                        )
                    })
                })
            };
            if let Some(error) = binding_error {
                if !outcome.stderr.is_empty() {
                    outcome.stderr.push('\n');
                }
                outcome.stderr.push_str("[astra-test] lifecycle: ");
                outcome.stderr.push_str(&error);
                lifecycle_errors.push(error);
            }
        }

        // Durable criteria must never inspect the whole resumable journal.
        // Bind the evidence surface to every current run id and timestamp;
        // stale turns remain available only through diagnostics outside the
        // certification path.
        if let Some(capture) = session.take() {
            session =
                Some(capture.scoped_to_invocation(&invocation_run_ids, invocation_started_at));
        }
        let mut judger_outcome = outcome.clone();
        if let Some(session) = &session {
            // Preserve complete moderate-sized receipts here; the judger owns
            // the final 8k head+tail prompt bound, which keeps both the start
            // contract and terminal tail of larger fanout results visible.
            let evidence = session.render_tool_evidence(16_000);
            if !evidence.is_empty() {
                judger_outcome
                    .stderr
                    .push_str("\n[durable-tool-evidence jsonl]\n");
                judger_outcome.stderr.push_str(&evidence);
            }
        }

        let mut det = evaluate_deterministic_with_session(&criteria, &outcome, session.as_ref());

        // Always run the judger (unless --no-judger) — the quality
        // score is useful for diagnostics even when Hard criteria fail.
        if !self.no_judger {
            for (i, c) in criteria.iter().enumerate() {
                if matches!(c, Criterion::Judger { .. } | Criterion::HardJudger { .. })
                    && let Some(res) = evaluate_judger(self.judger, c, &judger_outcome).await
                {
                    det[i] = res;
                }
            }
        } else {
            for (i, c) in criteria.iter().enumerate() {
                match c {
                    Criterion::Judger { .. } => {
                        det[i].passed = true;
                        det[i].detail = "judger skipped (--no-judger)".into();
                    }
                    Criterion::HardJudger { .. } => {
                        det[i].passed = false;
                        det[i].detail = "required judger unavailable (--no-judger)".into();
                    }
                    _ => {}
                }
            }
        }

        let steps_passed = step_results.iter().all(|s| s.passed);
        // A case passes if all Hard criteria pass. Soft and Quality
        // failures are warnings, not hard fails — they still contribute
        // to the overall quality score but don't mark the case as FAIL.
        let hard_passed = det
            .iter()
            .filter(|c| c.severity == crate::criteria::CriterionSeverity::Hard)
            .all(|c| c.passed);
        let all_passed = det.iter().all(|c| c.passed) && steps_passed;
        let product_passed = lifecycle_errors.is_empty() && hard_passed && steps_passed;

        // Classify failure.
        let product_failure_class = if setup_failed {
            Some(crate::classify::FailureClass::PlatformSetupFailed)
        } else if !product_passed {
            Some(classify(&outcome, &det))
        } else {
            None
        };

        let reproducer = {
            let r = self.executor.reproducer(case, model);
            if r.is_empty() { None } else { Some(r) }
        };

        // Digest on FAIL.
        let (digest, digest_error) = if !product_passed
            && let Some(collector) = self.digest_collector
            && let Some(sid) = outcome.session_id.as_deref()
        {
            match collector.collect(sid).await {
                Ok(a) => (Some(a), None),
                Err(e) => (None, Some(e)),
            }
        } else {
            (None, None)
        };

        // Run teardown AFTER criteria evaluation so the judger can
        // still verify artifacts (files, DB state) created during the
        // case. Previously teardown ran before the judger, destroying
        // evidence that the judger tried to independently verify.
        let teardown_error = if let Some(ref cmd) = case.teardown_cmd {
            self.run_shell_hook("teardown_cmd", case, cmd).await.err()
        } else {
            None
        };
        let mut cleanup_errors: Vec<String> = teardown_error.into_iter().collect();
        cleanup_errors.extend(
            self.cleanup_session_owned_memories(case, session.as_ref())
                .await,
        );
        for error in &cleanup_errors {
            if !outcome.stderr.is_empty() {
                outcome.stderr.push('\n');
            }
            outcome.stderr.push_str("[astra-test] ");
            outcome.stderr.push_str(error);
        }

        // A run whose cleanup did not complete cannot certify its result: it
        // may contaminate every later case. Keep the product criteria intact
        // in the report, but make the harness failure explicit rather than
        // presenting a green result with an easily missed warning.
        let passed = product_passed && cleanup_errors.is_empty();
        let failure_class = if cleanup_errors.is_empty() {
            product_failure_class
        } else {
            Some(FailureClass::HarnessCleanupFailed)
        };

        // Progress: emit per-case result to stderr so long runs show
        // streaming progress even when stdout is buffered.
        let marker = if passed { "PASS" } else { "FAIL" };
        eprintln!(
            "[astra-test] [{marker}] {case} × {model} ({dur}ms)",
            case = case.name,
            model = model,
            dur = outcome.duration_ms,
        );

        let retry_attempted = attempts.len() > 1;
        CaseRunReport {
            case_name: case.name.clone(),
            model: model.to_string(),
            status: if passed {
                CaseRunStatus::Passed
            } else {
                CaseRunStatus::Failed
            },
            run_index: 0, // overwritten by caller
            capability: case.capability.clone(),
            weight: case.weight,
            difficulty: case.difficulty,
            outcome,
            criteria: det,
            steps: step_results,
            attempts,
            session,
            reproducer,
            digest,
            digest_error,
            failure_class,
            has_warnings: passed && (!all_passed || retry_attempted),
        }
    }

    /// Execute one case while preserving a user-visible liveness boundary.
    /// The heartbeat is deliberately weaker than product progress: it only
    /// proves that the harness task is still awaiting the case.  Tool/turn
    /// counts and terminal criteria remain sourced from the final outcome.
    async fn run_one_with_progress(
        &self,
        case: &Case,
        model: &str,
        run_id: &str,
        run_index: u32,
    ) -> CaseRunReport {
        let started = Instant::now();
        let mut execution = Box::pin(self.run_one(case, model));
        let mut heartbeat = tokio::time::interval(Duration::from_secs(5));
        // Consume interval's immediate first tick so the first heartbeat is
        // a real five-second observation rather than a duplicate start event.
        heartbeat.tick().await;

        loop {
            tokio::select! {
                report = &mut execution => return report,
                _ = heartbeat.tick() => {
                    let elapsed_ms = started.elapsed().as_millis() as u64;
                    eprintln!(
                        "[astra-test] [RUNNING] {} × {} elapsed={}ms",
                        case.name, model, elapsed_ms
                    );
                    if let Some(ref tx) = self.dashboard_tx {
                        let _ = tx.send(crate::dashboard::DashboardEvent::CaseProgress {
                            run_id: run_id.to_string(),
                            case_name: case.name.clone(),
                            model: model.to_string(),
                            run_index,
                            phase: "executing".into(),
                            elapsed_ms,
                            sequence: crate::dashboard::next_dashboard_event_sequence(),
                        });
                    }
                }
            }
        }
    }

    /// Remove records that a case created, using only structured session
    /// evidence and the normal user-authorized CLI path. Never use a topic
    /// purge here: a topic can overlap concurrent cases or user data.
    async fn cleanup_session_owned_memories(
        &self,
        case: &Case,
        session: Option<&SessionCapture>,
    ) -> Vec<String> {
        if !case.cleanup_memory_records {
            return Vec::new();
        }
        let Some(session) = session else {
            return vec![format!(
                "memory cleanup unavailable for case={}: no session capture",
                case.name
            )];
        };
        if session.skipped_lines != 0
            || session.dropped_lines != 0
            || session.has_integrity_errors()
        {
            return vec![format!(
                "memory cleanup unavailable for case={}: session capture is incomplete (skipped_lines={}, dropped_lines={}, integrity_errors={})",
                case.name, session.skipped_lines, session.dropped_lines, session.integrity_errors
            )];
        }

        let mut errors = Vec::new();
        for memory_id in session.created_memory_ids() {
            let mut command = tokio::process::Command::new(&self.runner_cfg.astra_bin);
            if let Some(profile) = &self.runner_cfg.profile {
                command.arg("--profile").arg(profile);
            }
            if let Some(working_dir) = &self.runner_cfg.working_dir {
                command.current_dir(working_dir);
            }
            command
                .arg("memory")
                .arg("forget")
                .arg(&memory_id)
                .arg("--reason")
                .arg(format!("Astra harness cleanup for case {}", case.name))
                .kill_on_drop(true);
            match tokio::time::timeout(Duration::from_secs(30), command.output()).await {
                Ok(Ok(output)) if output.status.success() => {}
                Ok(Ok(output)) => {
                    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                    errors.push(format!(
                        "memory cleanup failed for case={} memory_id={} status={}: {}",
                        case.name,
                        memory_id,
                        output.status,
                        if stderr.is_empty() {
                            "no stderr"
                        } else {
                            &stderr
                        }
                    ));
                }
                Ok(Err(error)) => errors.push(format!(
                    "memory cleanup error for case={} memory_id={}: {error}",
                    case.name, memory_id
                )),
                Err(_) => errors.push(format!(
                    "memory cleanup timed out after 30s for case={} memory_id={}",
                    case.name, memory_id
                )),
            }
        }
        errors
    }

    /// Execute a case setup/teardown hook without losing the command's first
    /// failure or its diagnostic output. Hooks are part of test validity, not
    /// best-effort convenience: `sh -e` prevents a later successful command
    /// from masking an earlier failed cleanup/setup operation.
    async fn run_shell_hook(&self, hook: &str, case: &Case, script: &str) -> Result<(), String> {
        let mut command = tokio::process::Command::new("sh");
        if let Some(working_dir) = &self.runner_cfg.working_dir {
            command.current_dir(working_dir);
        }
        command.arg("-e").arg("-c").arg(script).kill_on_drop(true);
        match tokio::time::timeout(Duration::from_secs(30), command.output()).await {
            Ok(Ok(output)) if output.status.success() => Ok(()),
            Ok(Ok(output)) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let stdout = String::from_utf8_lossy(&output.stdout);
                let detail = hook_output_detail(&stderr, &stdout);
                Err(format!(
                    "{hook} failed for case={} with status={}: {detail}",
                    case.name, output.status
                ))
            }
            Ok(Err(error)) => Err(format!("{hook} error for case={}: {error}", case.name)),
            Err(_) => Err(format!("{hook} timed out after 30s for case={}", case.name)),
        }
    }

    fn model_resolution_unavailable(
        &self,
        case: &Case,
        detail: &str,
        run_index: u32,
    ) -> CaseRunReport {
        let reason = format!("unavailable: could not resolve a model for case: {detail}");
        let criteria = case
            .criteria
            .iter()
            .cloned()
            .map(|criterion| crate::criteria::CriterionResult {
                severity: crate::criteria::criterion_severity(&criterion),
                criterion,
                passed: false,
                detail: reason.clone(),
                full_detail: None,
                score: None,
            })
            .collect();
        CaseRunReport {
            case_name: case.name.clone(),
            model: "<unresolved>".into(),
            status: CaseRunStatus::Unavailable,
            run_index,
            capability: case.capability.clone(),
            weight: case.weight,
            difficulty: case.difficulty,
            outcome: RunOutcome::new("<unresolved>")
                .with_text(reason.clone())
                .with_stderr(reason),
            criteria,
            steps: Vec::new(),
            attempts: Vec::new(),
            session: None,
            reproducer: None,
            digest: None,
            digest_error: None,
            failure_class: Some(crate::classify::FailureClass::InfraVerificationUnavailable),
            has_warnings: false,
        }
    }

    fn cancelled_case_report(
        &self,
        case: &Case,
        model: &str,
        run_index: u32,
        reason: &str,
    ) -> CaseRunReport {
        let detail = format!("cancelled before execution: {reason}");
        let criteria = case
            .criteria
            .iter()
            .cloned()
            .map(|criterion| crate::criteria::CriterionResult {
                severity: crate::criteria::criterion_severity(&criterion),
                criterion,
                passed: false,
                detail: detail.clone(),
                full_detail: None,
                score: None,
            })
            .collect();
        CaseRunReport {
            case_name: case.name.clone(),
            model: model.to_string(),
            status: CaseRunStatus::Cancelled,
            run_index,
            capability: case.capability.clone(),
            weight: case.weight,
            difficulty: case.difficulty,
            outcome: RunOutcome::new(model)
                .with_text(detail.clone())
                .with_stderr(detail),
            criteria,
            steps: Vec::new(),
            attempts: Vec::new(),
            session: None,
            reproducer: None,
            digest: None,
            digest_error: None,
            failure_class: Some(FailureClass::InfraVerificationUnavailable),
            has_warnings: false,
        }
    }

    fn skip_for_unsupported_cache_scope(&self, case: &Case, model: &str) -> Option<CaseRunReport> {
        let required = case.required_cache_scope?;
        let profiles = load_profiles(self.runner_cfg.working_dir.as_deref());
        let reuse_support = profiles
            .get(model)
            .map(|profile| profile.reuse_support)
            .unwrap_or(ModelReuseSupport::Unknown);
        // Unknown metadata is not an exclusion: run the case and let its
        // criteria provide actual evidence. Only an explicit profile claim
        // that cannot satisfy the requested scope is unavailable.
        if !reuse_support.explicitly_unsupported(required) {
            return None;
        }

        let detail = format!(
            "unavailable: model metadata reports prompt-cache reuse_scope={reuse_support:?}, \
             but case requires {required:?}"
        );
        let criteria = case
            .criteria
            .iter()
            .cloned()
            .map(|criterion| crate::criteria::CriterionResult {
                severity: crate::criteria::criterion_severity(&criterion),
                criterion,
                passed: false,
                detail: detail.clone(),
                full_detail: None,
                score: None,
            })
            .collect();
        Some(CaseRunReport {
            case_name: case.name.clone(),
            model: model.to_string(),
            status: CaseRunStatus::Unavailable,
            run_index: 0,
            capability: case.capability.clone(),
            weight: case.weight,
            difficulty: case.difficulty,
            outcome: RunOutcome::new(model)
                .with_text(detail.clone())
                .with_stderr(detail.clone()),
            criteria,
            steps: Vec::new(),
            attempts: Vec::new(),
            session: None,
            reproducer: None,
            digest: None,
            digest_error: None,
            failure_class: Some(crate::classify::FailureClass::InfraVerificationUnavailable),
            has_warnings: false,
        })
    }
}

fn hook_output_detail(stderr: &str, stdout: &str) -> String {
    let text = if stderr.trim().is_empty() {
        stdout
    } else {
        stderr
    }
    .trim();
    if text.is_empty() {
        return "no command output".into();
    }
    const MAX_CHARS: usize = 4_000;
    let mut clipped: String = text.chars().take(MAX_CHARS).collect();
    if text.chars().count() > MAX_CHARS {
        clipped.push_str("… [truncated]");
    }
    clipped
}

fn is_rate_limited(outcome: &crate::runner::RunOutcome) -> bool {
    crate::classify::outcome_is_rate_limited(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::case::PromptCacheReuseScope;
    use crate::criteria::Criterion;
    use crate::exec::test_support::FakeExecutor;
    use crate::judger::JudgerScore;
    use crate::runner::RunOutcome;
    use crate::session_capture::SessionCapture;
    use async_trait::async_trait;
    use std::path::PathBuf;

    fn case_with(name: &str, criteria: Vec<Criterion>) -> Case {
        Case {
            name: name.into(),
            description: None,
            prompt: format!("prompt-for-{name}"),
            prompt_variants: vec![],
            models: None,
            criteria,
            debug_log: false,
            extra_cli_args: vec![],
            timeout_seconds: 60,
            capability: None,
            required_cache_scope: None,
            difficulty: None,
            weight: 1.0,
            steps: vec![],
            cli_env: std::collections::HashMap::new(),
            setup_cmd: None,
            teardown_cmd: None,
            cleanup_memory_records: false,
        }
    }

    #[test]
    fn rate_limit_retry_requires_typed_failed_terminal() {
        let successful_text = RunOutcome::new("m")
            .with_text("the provider mentioned a rate limit")
            .with_stderr("rate limit");
        assert!(!is_rate_limited(&successful_text));

        let untyped_failure = RunOutcome::new("m")
            .with_exit_code(1)
            .with_stderr("rate limit");
        assert!(!is_rate_limited(&untyped_failure));

        let typed_failure = RunOutcome::new("m")
            .with_exit_code(1)
            .with_final_state("interrupted")
            .with_interruption_kind("rate_limit")
            .with_stderr("HTTP 429: Too many requests");
        assert!(is_rate_limited(&typed_failure));
    }

    #[tokio::test]
    async fn rate_limit_retry_refuses_to_abandon_a_session_attempt() {
        let mut first = RunOutcome::new("m")
            .with_exit_code(1)
            .with_session_id("550e8400-e29b-41d4-a716-446655440000")
            .with_final_state("interrupted")
            .with_interruption_kind("rate_limit")
            .with_stderr("HTTP 429: Too many requests");
        first.run_id = Some("run-first".into());
        let mut second = RunOutcome::new("m")
            .with_exit_code(0)
            .with_text("second attempt must not execute")
            .with_session_id("550e8400-e29b-41d4-a716-446655440000");
        second.run_id = Some("run-second".into());
        let exec = SequenceExecutor {
            outcomes: std::sync::Mutex::new(vec![first, second]),
            calls: std::sync::atomic::AtomicUsize::new(0),
        };
        let judger = FixedJudger { score: 1.0 };
        let loader = NoopSessionLoader;
        let suite_cfg = SuiteConfig {
            retry_on_429: true,
            ..SuiteConfig::default()
        };
        let runner = SuiteRunner {
            executor: &exec,
            judger: &judger,
            session_loader: &loader,
            digest_collector: None,
            runner_cfg: RunnerConfig::new(PathBuf::from("astra"))
                .with_fallback_models(vec!["m".into()]),
            no_judger: true,
            session_mode: SessionCaptureMode::Never,
            suite_cfg,
            dashboard_tx: None,
            run_id: String::new(),
            cancel_flag: None,
        };
        let report = runner.run_all(&[case_with("retry-session", vec![])]).await;
        assert_eq!(exec.calls.load(Ordering::Relaxed), 1);
        assert_eq!(report.runs[0].attempts.len(), 1);
        assert!(
            report.runs[0]
                .outcome
                .stderr
                .contains("retry refused after first attempt produced session")
        );
    }

    fn outcome_ok(model: &str, text: &str, tools: &[&str]) -> RunOutcome {
        RunOutcome {
            model: model.into(),
            exit_code: 0,
            text: text.into(),
            stderr: String::new(),
            session_id: Some("550e8400-e29b-41d4-a716-446655440000".into()),
            run_id: Some("run-test".into()),
            tool_calls_count: tools.len() as u32,
            tools_used: tools.iter().map(|s| s.to_string()).collect(),
            completion_tokens: 0,
            prompt_tokens: 0,
            cached_input_tokens: 0,
            cache_creation_tokens: 0,
            duration_ms: 12,
            turn_rounds: 1,
            cache_hits: 0,
            total_tool_calls: tools.len() as u32,
            ttft_ms: 0,
            final_state: None,
            interruption_kind: None,
            tool_result_class_counts: std::collections::BTreeMap::new(),
        }
    }

    struct FixedJudger {
        score: f64,
    }

    struct SequenceExecutor {
        outcomes: std::sync::Mutex<Vec<RunOutcome>>,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl CaseExecutor for SequenceExecutor {
        async fn execute(&self, _case: &Case, _model: &str) -> RunOutcome {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.outcomes.lock().unwrap().remove(0)
        }

        fn reproducer(&self, _case: &Case, _model: &str) -> String {
            "<sequence executor>".into()
        }
    }

    #[async_trait]
    impl Judger for FixedJudger {
        async fn score(
            &self,
            _q: &str,
            _m: Option<&str>,
            _o: &RunOutcome,
        ) -> Result<JudgerScore, String> {
            Ok(JudgerScore {
                score: self.score,
                rationale: "fixed".into(),
                full_rationale: "fixed".into(),
                votes: Vec::new(),
            })
        }
    }

    struct NoopSessionLoader;
    impl SessionLoader for NoopSessionLoader {
        fn load(&self, _id: &str) -> Option<SessionCapture> {
            None
        }
    }

    struct FixedSessionLoader {
        capture: SessionCapture,
    }

    impl SessionLoader for FixedSessionLoader {
        fn load(&self, _id: &str) -> Option<SessionCapture> {
            Some(self.capture.clone())
        }
    }

    #[tokio::test]
    async fn required_subsystem_health_is_runner_wide_and_forces_capture() {
        let exec = FakeExecutor::new();
        exec.seed("healthy", "m", outcome_ok("m", "done", &[]));
        let judger = FixedJudger { score: 1.0 };
        let loader = FixedSessionLoader {
            capture: SessionCapture {
                session_id: "sess-m".into(),
                journal_path: PathBuf::from("/typed-fixture"),
                events: vec![
                    crate::session_capture::JournalEvent {
                        event_type: "turn".into(),
                        raw: serde_json::json!({
                            "type": "turn",
                            "ts": (chrono::Utc::now() + chrono::Duration::minutes(1)).to_rfc3339(),
                            "session_id": "sess-m",
                            "turn": 3,
                            "metadata": {"run_id": "run-test"}
                        }),
                    },
                    crate::session_capture::JournalEvent {
                        event_type: "session_memory_extraction".into(),
                        raw: serde_json::json!({
                        "turn": 3,
                        "ts": (chrono::Utc::now() + chrono::Duration::minutes(1)).to_rfc3339(),
                        "metadata": {"run_id": "run-test", "outcome": "extracted", "source": "rule_fallback"}
                        }),
                    },
                    crate::session_capture::JournalEvent {
                        event_type: "subsystem_settled".into(),
                        raw: serde_json::json!({
                        "turn": 3,
                        "ts": (chrono::Utc::now() + chrono::Duration::minutes(1)).to_rfc3339(),
                        "metadata": {"run_id": "run-test", "subsystem": "post_loop_memory"}
                        }),
                    },
                ],
                skipped_lines: 0,
                dropped_lines: 0,
                integrity_errors: 0,
            },
        };
        let mut cfg = RunnerConfig::new(PathBuf::from("astra"))
            .with_fallback_models(vec!["m".into()])
            .with_required_session_subsystem_health();
        cfg.session_settle_timeout = Duration::ZERO;
        let runner = SuiteRunner {
            executor: &exec,
            judger: &judger,
            session_loader: &loader,
            digest_collector: None,
            runner_cfg: cfg,
            no_judger: true,
            session_mode: SessionCaptureMode::Never,
            suite_cfg: SuiteConfig::default(),
            dashboard_tx: None,
            run_id: String::new(),
            cancel_flag: None,
        };

        let report = runner.run_all(&[case_with("healthy", vec![])]).await;
        assert_eq!(report.passed(), 1);
        assert!(
            report.runs[0].session.is_some(),
            "health gate must force durable capture"
        );
        assert!(report.runs[0].criteria.iter().any(|result| {
            matches!(result.criterion, Criterion::SessionSubsystemHealthy { .. }) && result.passed
        }));
    }

    #[tokio::test]
    async fn hard_session_evidence_rejects_old_run_id_replay() {
        let exec = FakeExecutor::new();
        let mut outcome = outcome_ok("m", "done", &[]);
        outcome.run_id = Some("run-new".into());
        exec.seed("replay", "m", outcome);
        let judger = FixedJudger { score: 1.0 };
        let loader = FixedSessionLoader {
            capture: SessionCapture {
                session_id: "sess-replay".into(),
                journal_path: PathBuf::from("/old-session"),
                events: vec![
                    crate::session_capture::JournalEvent {
                        event_type: "turn".into(),
                        raw: serde_json::json!({
                            "type": "turn",
                            "ts": chrono::Utc::now().to_rfc3339(),
                            "session_id": "sess-replay",
                            "turn": 1,
                            "metadata": {"run_id": "run-old"}
                        }),
                    },
                    crate::session_capture::JournalEvent {
                        event_type: "subsystem_settled".into(),
                        raw: serde_json::json!({
                            "turn": 1,
                            "metadata": {"subsystem": "post_loop_memory"}
                        }),
                    },
                ],
                skipped_lines: 0,
                dropped_lines: 0,
                integrity_errors: 0,
            },
        };
        let mut cfg = RunnerConfig::new(PathBuf::from("astra"))
            .with_fallback_models(vec!["m".into()])
            .with_required_session_subsystem_health();
        cfg.session_settle_timeout = Duration::ZERO;
        let runner = SuiteRunner {
            executor: &exec,
            judger: &judger,
            session_loader: &loader,
            digest_collector: None,
            runner_cfg: cfg,
            no_judger: true,
            session_mode: SessionCaptureMode::Never,
            suite_cfg: SuiteConfig::default(),
            dashboard_tx: None,
            run_id: String::new(),
            cancel_flag: None,
        };
        let report = runner.run_all(&[case_with("replay", vec![])]).await;
        assert!(!report.runs[0].is_passed());
        assert!(report.runs[0].outcome.stderr.contains("canonical turn"));
    }

    #[tokio::test]
    async fn hard_session_evidence_ignores_old_tools_when_fresh_turn_has_none() {
        let exec = FakeExecutor::new();
        let mut outcome = outcome_ok("m", "done", &[]);
        outcome.run_id = Some("run-new".into());
        exec.seed("mixed-replay", "m", outcome);
        let fresh_ts = chrono::Utc::now() + chrono::Duration::minutes(1);
        let loader = FixedSessionLoader {
            capture: SessionCapture {
                session_id: "sess-mixed-replay".into(),
                journal_path: PathBuf::from("/mixed-session"),
                events: vec![
                    crate::session_capture::JournalEvent {
                        event_type: "turn".into(),
                        raw: serde_json::json!({
                            "type": "turn",
                            "ts": chrono::Utc::now().to_rfc3339(),
                            "session_id": "sess-mixed-replay",
                            "turn": 1,
                            "metadata": {"run_id": "run-old"},
                            "tool_calls": [{"name": "Read", "ok": true}]
                        }),
                    },
                    crate::session_capture::JournalEvent {
                        event_type: "turn".into(),
                        raw: serde_json::json!({
                            "type": "turn",
                            "ts": fresh_ts.to_rfc3339(),
                            "session_id": "sess-mixed-replay",
                            "turn": 2,
                            "metadata": {"run_id": "run-new"},
                            "tool_calls": []
                        }),
                    },
                ],
                skipped_lines: 0,
                dropped_lines: 0,
                integrity_errors: 0,
            },
        };
        let mut cfg =
            RunnerConfig::new(PathBuf::from("astra")).with_fallback_models(vec!["m".into()]);
        cfg.session_settle_timeout = Duration::ZERO;
        let runner = SuiteRunner {
            executor: &exec,
            judger: &FixedJudger { score: 1.0 },
            session_loader: &loader,
            digest_collector: None,
            runner_cfg: cfg,
            no_judger: true,
            session_mode: SessionCaptureMode::Never,
            suite_cfg: SuiteConfig::default(),
            dashboard_tx: None,
            run_id: String::new(),
            cancel_flag: None,
        };
        let case = case_with(
            "mixed-replay",
            vec![Criterion::JournalToolCalled {
                name: "Read".into(),
                optional: false,
            }],
        );
        let report = runner.run_all(&[case]).await;
        assert_eq!(report.runs.len(), 1);
        assert!(
            !report.runs[0].is_passed(),
            "old tool evidence must not certify a fresh tool-free turn"
        );
        assert!(report.runs[0].criteria.iter().any(|result| {
            matches!(result.criterion, Criterion::JournalToolCalled { .. }) && !result.passed
        }));
    }

    #[tokio::test]
    async fn required_subsystem_health_fails_without_durable_capture() {
        let exec = FakeExecutor::new();
        exec.seed("missing", "m", outcome_ok("m", "done", &[]));
        let judger = FixedJudger { score: 1.0 };
        let loader = NoopSessionLoader;
        let mut cfg = RunnerConfig::new(PathBuf::from("astra"))
            .with_fallback_models(vec!["m".into()])
            .with_required_session_subsystem_health();
        cfg.session_settle_timeout = Duration::ZERO;
        let runner = SuiteRunner {
            executor: &exec,
            judger: &judger,
            session_loader: &loader,
            digest_collector: None,
            runner_cfg: cfg,
            no_judger: true,
            session_mode: SessionCaptureMode::Never,
            suite_cfg: SuiteConfig::default(),
            dashboard_tx: None,
            run_id: String::new(),
            cancel_flag: None,
        };

        let report = runner.run_all(&[case_with("missing", vec![])]).await;
        assert_eq!(report.passed(), 0);
        assert!(report.runs[0].criteria.iter().any(|result| {
            matches!(result.criterion, Criterion::SessionSubsystemHealthy { .. }) && !result.passed
        }));
    }

    #[tokio::test]
    async fn run_all_serial() {
        let exec = FakeExecutor::new();
        exec.seed("c1", "a", outcome_ok("a", "t", &[]));
        exec.seed("c1", "b", outcome_ok("b", "t", &[]));

        let judger = FixedJudger { score: 1.0 };
        let loader = NoopSessionLoader;
        let cfg = RunnerConfig::new(PathBuf::from("astra"))
            .with_fallback_models(vec!["a".into(), "b".into()]);
        let runner = SuiteRunner {
            executor: &exec,
            judger: &judger,
            session_loader: &loader,
            digest_collector: None,
            runner_cfg: cfg,
            no_judger: true,
            session_mode: SessionCaptureMode::Never,
            suite_cfg: SuiteConfig::default(),
            dashboard_tx: None,
            run_id: String::new(),
            cancel_flag: None,
        };
        let cases = vec![case_with("c1", vec![])];
        let report = runner.run_all(&cases).await;
        assert_eq!(report.total(), 2);
        assert_eq!(report.passed(), 2);
    }

    #[tokio::test]
    async fn dashboard_lifecycle_exposes_queue_before_start_and_terminal() {
        let exec = FakeExecutor::new();
        exec.seed("queued", "m", outcome_ok("m", "done", &[]));
        let judger = FixedJudger { score: 1.0 };
        let loader = NoopSessionLoader;
        let (tx, mut rx) = tokio::sync::broadcast::channel(32);
        let runner = SuiteRunner {
            executor: &exec,
            judger: &judger,
            session_loader: &loader,
            digest_collector: None,
            runner_cfg: RunnerConfig::new(PathBuf::from("astra"))
                .with_fallback_models(vec!["m".into()]),
            no_judger: true,
            session_mode: SessionCaptureMode::Never,
            suite_cfg: SuiteConfig::default(),
            dashboard_tx: Some(tx),
            run_id: "run-lifecycle".into(),
            cancel_flag: None,
        };

        let report = runner.run_all(&[case_with("queued", vec![])]).await;
        assert_eq!(report.passed(), 1);

        let mut types = Vec::new();
        while let Ok(event) = rx.try_recv() {
            let value = serde_json::to_value(event).expect("dashboard event serializes");
            types.push(value["type"].as_str().unwrap().to_string());
        }
        assert_eq!(
            types,
            vec![
                "suite_started",
                "case_queued",
                "case_started",
                "case_completed"
            ],
            "a case must never look started before its queue admission is visible"
        );
    }

    #[tokio::test]
    async fn unsupported_conversation_scope_is_unavailable_from_model_metadata() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join(".models.yaml"),
            r#"
- name: kimi-k2.6
  provider: openai
  prompt_cache_capability:
    protocol: openai_auto_prefix
    volatile_placement: tail_suffix
    reuse_scope: intra_turn_rounds
"#,
        )
        .expect("write models yaml");

        let exec = FakeExecutor::new();
        let judger = FixedJudger { score: 1.0 };
        let loader = NoopSessionLoader;
        let mut cfg = RunnerConfig::new(PathBuf::from("astra"))
            .with_fallback_models(vec!["kimi-k2.6".into()]);
        cfg.working_dir = Some(tmp.path().to_path_buf());
        let runner = SuiteRunner {
            executor: &exec,
            judger: &judger,
            session_loader: &loader,
            digest_collector: None,
            runner_cfg: cfg,
            no_judger: true,
            session_mode: SessionCaptureMode::Never,
            suite_cfg: SuiteConfig::default(),
            dashboard_tx: None,
            run_id: String::new(),
            cancel_flag: None,
        };

        let mut case = case_with("cache-prefix", vec![Criterion::ExitCode { code: 0 }]);
        case.required_cache_scope = Some(PromptCacheReuseScope::ConversationTurns);
        let report = runner.run_all(&[case]).await;
        assert_eq!(report.total(), 1);
        assert_eq!(report.passed(), 0);
        assert_eq!(report.failed(), 0);
        assert_eq!(report.unavailable(), 1);
        assert_eq!(report.runs[0].status, CaseRunStatus::Unavailable);
        assert_eq!(
            exec.calls.lock().unwrap_or_else(|e| e.into_inner()).len(),
            0,
            "executor must not run"
        );
        assert!(
            report.runs[0]
                .outcome
                .text
                .contains("unavailable: model metadata reports"),
            "{:#?}",
            report.runs[0].outcome
        );
        assert!(!report.runs[0].has_warnings);
        assert_eq!(
            report.runs[0].failure_class,
            Some(crate::classify::FailureClass::InfraVerificationUnavailable)
        );
    }

    #[tokio::test]
    async fn unknown_cache_scope_metadata_executes_for_actual_evidence() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join(".models.yaml"),
            "- name: unknown-model\n  provider: openai\n",
        )
        .expect("write models yaml");

        let exec = FakeExecutor::new();
        exec.seed(
            "cache-prefix",
            "unknown-model",
            outcome_ok("unknown-model", "cache evidence", &[]),
        );
        let judger = FixedJudger { score: 1.0 };
        let loader = NoopSessionLoader;
        let mut cfg = RunnerConfig::new(PathBuf::from("astra"))
            .with_fallback_models(vec!["unknown-model".into()]);
        cfg.working_dir = Some(tmp.path().to_path_buf());
        let runner = SuiteRunner {
            executor: &exec,
            judger: &judger,
            session_loader: &loader,
            digest_collector: None,
            runner_cfg: cfg,
            no_judger: true,
            session_mode: SessionCaptureMode::Never,
            suite_cfg: SuiteConfig::default(),
            dashboard_tx: None,
            run_id: String::new(),
            cancel_flag: None,
        };

        let mut case = case_with("cache-prefix", vec![Criterion::ExitCode { code: 0 }]);
        case.required_cache_scope = Some(PromptCacheReuseScope::ConversationTurns);
        let report = runner.run_all(&[case]).await;
        assert_eq!(report.total(), 1);
        assert_eq!(report.passed(), 0);
        assert_eq!(report.failed(), 1);
        assert_eq!(report.unavailable(), 0);
        assert_eq!(report.runs[0].status, CaseRunStatus::Failed);
        assert!(report.runs[0].criteria.iter().any(|criterion| {
            matches!(criterion.criterion, Criterion::PromptCacheReuseScope { .. })
                && !criterion.passed
        }));
        assert_eq!(
            exec.calls.lock().unwrap_or_else(|e| e.into_inner()).len(),
            1,
            "unknown metadata must execute so criteria can prove the case"
        );
    }

    #[tokio::test]
    async fn circuit_breaker_aborts_on_consecutive_infra() {
        let exec = FakeExecutor::new();
        // All cases return auth failure.
        for i in 0..5 {
            let name = format!("c{i}");
            exec.seed(
                &name,
                "m",
                RunOutcome::new("m")
                    .with_exit_code(3)
                    .with_stderr("Could not validate credentials"),
            );
        }

        let judger = FixedJudger { score: 1.0 };
        let loader = NoopSessionLoader;
        let cfg = RunnerConfig::new(PathBuf::from("astra")).with_fallback_models(vec!["m".into()]);
        let runner = SuiteRunner {
            executor: &exec,
            judger: &judger,
            session_loader: &loader,
            digest_collector: None,
            runner_cfg: cfg,
            no_judger: true,
            session_mode: SessionCaptureMode::Never,
            suite_cfg: SuiteConfig {
                parallel: 1,
                circuit_breaker_threshold: 3,
                retry_on_429: false,
                runs: 1,
            },
            dashboard_tx: None,
            run_id: String::new(),
            cancel_flag: None,
        };
        let cases: Vec<Case> = (0..5)
            .map(|i| case_with(&format!("c{i}"), vec![Criterion::ExitCode { code: 0 }]))
            .collect();
        let report = runner.run_all(&cases).await;
        // Every planned item remains a terminal row: three real failures and
        // two explicit circuit-breaker cancellations.
        assert_eq!(report.total(), 5);
        assert_eq!(report.failed(), 3);
        assert_eq!(report.cancelled(), 2);
        assert!(report.runs[3..].iter().all(|r| r.is_cancelled()));
    }

    #[tokio::test]
    async fn failure_class_populated_on_fail() {
        let exec = FakeExecutor::new();
        exec.seed("c1", "m", RunOutcome::new("m").with_exit_code(124));

        let judger = FixedJudger { score: 1.0 };
        let loader = NoopSessionLoader;
        let cfg = RunnerConfig::new(PathBuf::from("astra")).with_fallback_models(vec!["m".into()]);
        let runner = SuiteRunner {
            executor: &exec,
            judger: &judger,
            session_loader: &loader,
            digest_collector: None,
            runner_cfg: cfg,
            no_judger: true,
            session_mode: SessionCaptureMode::Never,
            suite_cfg: SuiteConfig::default(),
            dashboard_tx: None,
            run_id: String::new(),
            cancel_flag: None,
        };
        let cases = vec![case_with("c1", vec![Criterion::ExitCode { code: 0 }])];
        let report = runner.run_all(&cases).await;
        assert_eq!(
            report.runs[0].failure_class,
            Some(FailureClass::InfraTimeout)
        );
    }

    #[tokio::test]
    async fn judger_always_fires_even_when_hard_criteria_fail() {
        let exec = FakeExecutor::new();
        exec.seed("c1", "m", outcome_ok("m", "hello", &["Read"]));
        exec.seed("c2", "m", outcome_ok("m", "hello", &[]));

        struct TrackingJudger {
            hits: std::sync::Mutex<u32>,
        }
        #[async_trait]
        impl Judger for TrackingJudger {
            async fn score(
                &self,
                _q: &str,
                _m: Option<&str>,
                _o: &RunOutcome,
            ) -> Result<JudgerScore, String> {
                *self.hits.lock().unwrap_or_else(|e| e.into_inner()) += 1;
                Ok(JudgerScore {
                    score: 1.0,
                    rationale: "".into(),
                    full_rationale: "".into(),
                    votes: Vec::new(),
                })
            }
        }
        let judger = TrackingJudger {
            hits: std::sync::Mutex::new(0),
        };
        let loader = NoopSessionLoader;
        let cfg = RunnerConfig::new(PathBuf::from("astra")).with_fallback_models(vec!["m".into()]);
        let runner = SuiteRunner {
            executor: &exec,
            judger: &judger,
            session_loader: &loader,
            digest_collector: None,
            runner_cfg: cfg,
            no_judger: false,
            session_mode: SessionCaptureMode::Never,
            suite_cfg: SuiteConfig::default(),
            dashboard_tx: None,
            run_id: String::new(),
            cancel_flag: None,
        };
        let j = Criterion::Judger {
            question: "q?".into(),
            threshold: 0.7,
            model: None,
        };
        let read_req = Criterion::ToolCalled {
            name: "Read".into(),
        };
        let cases = vec![
            case_with("c1", vec![read_req.clone(), j.clone()]),
            case_with("c2", vec![read_req, j]),
        ];
        let report = runner.run_all(&cases).await;
        assert_eq!(report.total(), 2);
        // c2 still fails (Hard criterion ToolCalled fails) even though
        // the judger ran and scored 1.0.
        assert_eq!(report.passed(), 1);
        // Judger fires for BOTH cases — quality score is diagnostic
        // even when Hard criteria fail.
        assert_eq!(*judger.hits.lock().unwrap_or_else(|e| e.into_inner()), 2);
    }

    #[tokio::test]
    async fn no_judger_flag_marks_judger_as_passed_skip() {
        let exec = FakeExecutor::new();
        exec.seed("c1", "m", outcome_ok("m", "hello", &[]));
        let judger = FixedJudger { score: 0.0 };
        let loader = NoopSessionLoader;
        let cfg = RunnerConfig::new(PathBuf::from("astra")).with_fallback_models(vec!["m".into()]);
        let runner = SuiteRunner {
            executor: &exec,
            judger: &judger,
            session_loader: &loader,
            digest_collector: None,
            runner_cfg: cfg,
            no_judger: true,
            session_mode: SessionCaptureMode::Never,
            suite_cfg: SuiteConfig::default(),
            dashboard_tx: None,
            run_id: String::new(),
            cancel_flag: None,
        };
        let case = case_with(
            "c1",
            vec![Criterion::Judger {
                question: "q?".into(),
                threshold: 0.7,
                model: None,
            }],
        );
        let report = runner.run_all(&[case]).await;
        assert_eq!(report.passed(), 1);
        assert!(report.runs[0].criteria[0].detail.contains("skipped"));
    }

    #[tokio::test]
    async fn no_judger_cannot_pass_a_required_judger() {
        let exec = FakeExecutor::new();
        exec.seed("c1", "m", outcome_ok("m", "hello", &[]));
        let judger = FixedJudger { score: 1.0 };
        let loader = NoopSessionLoader;
        let cfg = RunnerConfig::new(PathBuf::from("astra")).with_fallback_models(vec!["m".into()]);
        let runner = SuiteRunner {
            executor: &exec,
            judger: &judger,
            session_loader: &loader,
            digest_collector: None,
            runner_cfg: cfg,
            no_judger: true,
            session_mode: SessionCaptureMode::Never,
            suite_cfg: SuiteConfig::default(),
            dashboard_tx: None,
            run_id: String::new(),
            cancel_flag: None,
        };
        let case = case_with(
            "c1",
            vec![Criterion::HardJudger {
                question: "did the required side effect succeed?".into(),
                threshold: 0.7,
                model: None,
            }],
        );

        let report = runner.run_all(&[case]).await;
        assert_eq!(report.failed(), 1);
        assert!(
            report.runs[0].criteria[0]
                .detail
                .contains("required judger unavailable")
        );
    }

    #[tokio::test]
    async fn reports_case_when_no_model_source_as_unavailable() {
        let exec = FakeExecutor::new();
        let judger = FixedJudger { score: 1.0 };
        let loader = NoopSessionLoader;
        let cfg = RunnerConfig::new(PathBuf::from("astra"));
        let runner = SuiteRunner {
            executor: &exec,
            judger: &judger,
            session_loader: &loader,
            digest_collector: None,
            runner_cfg: cfg,
            no_judger: true,
            session_mode: SessionCaptureMode::Never,
            suite_cfg: SuiteConfig::default(),
            dashboard_tx: None,
            run_id: String::new(),
            cancel_flag: None,
        };
        let cases = vec![case_with("c1", vec![])];
        let report = runner.run_all(&cases).await;
        assert_eq!(report.total(), 1);
        assert_eq!(report.passed(), 0);
        assert_eq!(report.failed(), 0);
        assert_eq!(report.unavailable(), 1);
        assert_eq!(report.runs[0].status, CaseRunStatus::Unavailable);
        assert!(
            report.runs[0]
                .outcome
                .text
                .contains("could not resolve a model")
        );
        assert!(
            exec.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
        );
    }

    #[tokio::test]
    async fn digest_collected_on_fail_only() {
        use crate::digest::test_support::FakeDigestCollector;

        let exec = FakeExecutor::new();
        exec.seed("c_fail", "m", outcome_ok("m", "hello", &[]));
        exec.seed("c_pass", "m", outcome_ok("m", "hello", &["Read"]));
        let judger = FixedJudger { score: 1.0 };
        let loader = NoopSessionLoader;
        let digest = FakeDigestCollector::new();
        digest.seed_ok(
            "550e8400-e29b-41d4-a716-446655440000",
            serde_json::json!({"aggregates": {"turns": 2}}),
        );

        let cfg = RunnerConfig::new(PathBuf::from("astra")).with_fallback_models(vec!["m".into()]);
        let runner = SuiteRunner {
            executor: &exec,
            judger: &judger,
            session_loader: &loader,
            digest_collector: Some(&digest),
            runner_cfg: cfg,
            no_judger: true,
            session_mode: SessionCaptureMode::Never,
            suite_cfg: SuiteConfig::default(),
            dashboard_tx: None,
            run_id: String::new(),
            cancel_flag: None,
        };
        let read_req = Criterion::ToolCalled {
            name: "Read".into(),
        };
        let cases = vec![
            case_with("c_fail", vec![read_req.clone()]),
            case_with("c_pass", vec![read_req]),
        ];
        let report = runner.run_all(&cases).await;
        assert_eq!(report.passed(), 1);
        assert_eq!(report.failed(), 1);
        let calls = digest.calls.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0], "550e8400-e29b-41d4-a716-446655440000");
    }

    #[tokio::test]
    async fn session_capture_on_debug_log() {
        let exec = FakeExecutor::new();
        exec.seed("dbg", "m", outcome_ok("m", "text", &[]));
        let judger = FixedJudger { score: 1.0 };

        struct FixedLoader;
        impl SessionLoader for FixedLoader {
            fn load(&self, session_id: &str) -> Option<SessionCapture> {
                Some(SessionCapture {
                    session_id: session_id.to_string(),
                    journal_path: PathBuf::from("/fake"),
                    events: vec![],
                    skipped_lines: 0,
                    dropped_lines: 0,
                    integrity_errors: 0,
                })
            }
        }
        let loader = FixedLoader;
        let cfg = RunnerConfig::new(PathBuf::from("astra")).with_fallback_models(vec!["m".into()]);
        let runner = SuiteRunner {
            executor: &exec,
            judger: &judger,
            session_loader: &loader,
            digest_collector: None,
            runner_cfg: cfg,
            no_judger: true,
            session_mode: SessionCaptureMode::OnDebugLog,
            suite_cfg: SuiteConfig::default(),
            dashboard_tx: None,
            run_id: String::new(),
            cancel_flag: None,
        };
        let mut case = case_with("dbg", vec![]);
        case.debug_log = true;
        let report = runner.run_all(&[case]).await;
        let s = report.runs[0].session.as_ref().unwrap();
        assert_eq!(s.session_id, "550e8400-e29b-41d4-a716-446655440000");
    }

    #[tokio::test]
    async fn setup_cmd_failure_aborts_case() {
        let exec = FakeExecutor::new();
        exec.seed("c1", "m", outcome_ok("m", "text", &[]));
        let judger = FixedJudger { score: 1.0 };
        let loader = NoopSessionLoader;
        let cfg = RunnerConfig::new(PathBuf::from("astra")).with_fallback_models(vec!["m".into()]);
        let runner = SuiteRunner {
            executor: &exec,
            judger: &judger,
            session_loader: &loader,
            digest_collector: None,
            runner_cfg: cfg,
            no_judger: true,
            session_mode: SessionCaptureMode::Never,
            suite_cfg: SuiteConfig::default(),
            dashboard_tx: None,
            run_id: String::new(),
            cancel_flag: None,
        };
        let mut case = case_with("c1", vec![]);
        case.setup_cmd = Some("false\nprintf 'must-not-run\\n'".into());
        let report = runner.run_all(&[case]).await;
        assert_eq!(report.total(), 1);
        assert!(
            !report.runs[0].is_passed(),
            "setup failure must mark case as FAIL"
        );
        assert_eq!(
            report.runs[0].failure_class,
            Some(crate::classify::FailureClass::PlatformSetupFailed)
        );
        assert!(
            report.runs[0]
                .outcome
                .stderr
                .contains("setup_cmd failed for case=c1"),
            "the artifact must retain setup diagnostics"
        );
        assert!(
            exec.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty(),
            "executor must NOT be called when setup fails"
        );
    }

    #[tokio::test]
    async fn setup_failure_still_runs_teardown() {
        let exec = FakeExecutor::new();
        let judger = FixedJudger { score: 1.0 };
        let loader = NoopSessionLoader;
        let cfg = RunnerConfig::new(PathBuf::from("astra")).with_fallback_models(vec!["m".into()]);
        let marker =
            std::env::temp_dir().join(format!("astra-harness-teardown-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let mut case = case_with("setup-teardown", vec![]);
        case.setup_cmd = Some("false".into());
        case.teardown_cmd = Some(format!("printf done > {}", marker.display()));
        let runner = SuiteRunner {
            executor: &exec,
            judger: &judger,
            session_loader: &loader,
            digest_collector: None,
            runner_cfg: cfg,
            no_judger: true,
            session_mode: SessionCaptureMode::Never,
            suite_cfg: SuiteConfig::default(),
            dashboard_tx: None,
            run_id: String::new(),
            cancel_flag: None,
        };
        let report = runner.run_all(&[case]).await;
        assert!(!report.runs[0].is_passed());
        assert_eq!(
            std::fs::read_to_string(&marker)
                .expect("teardown marker")
                .as_str(),
            "done"
        );
        let _ = std::fs::remove_file(marker);
    }

    #[tokio::test]
    async fn teardown_cmd_failure_fails_the_harness_run() {
        let exec = FakeExecutor::new();
        exec.seed("c1", "m", outcome_ok("m", "text", &[]));
        let judger = FixedJudger { score: 1.0 };
        let loader = NoopSessionLoader;
        let cfg = RunnerConfig::new(PathBuf::from("astra")).with_fallback_models(vec!["m".into()]);
        let runner = SuiteRunner {
            executor: &exec,
            judger: &judger,
            session_loader: &loader,
            digest_collector: None,
            runner_cfg: cfg,
            no_judger: true,
            session_mode: SessionCaptureMode::Never,
            suite_cfg: SuiteConfig::default(),
            dashboard_tx: None,
            run_id: String::new(),
            cancel_flag: None,
        };
        let mut case = case_with("c1", vec![]);
        case.teardown_cmd = Some("false\ntrue".into());

        let report = runner.run_all(&[case]).await;
        assert_eq!(report.total(), 1);
        assert!(
            !report.runs[0].is_passed(),
            "a contaminated environment must not be reported as a passing harness run"
        );
        assert_eq!(
            report.runs[0].failure_class,
            Some(FailureClass::HarnessCleanupFailed)
        );
        assert!(
            report.runs[0]
                .outcome
                .stderr
                .contains("teardown_cmd failed for case=c1"),
            "the persisted artifact must retain the cleanup failure reason"
        );
    }

    #[tokio::test]
    async fn memory_cleanup_uses_only_the_current_sessions_store_record() {
        use crate::test_support::write_executable_shim;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let marker = dir.path().join("cleanup-args.txt");
        let shim = dir.path().join("astra-cleanup-shim");
        write_executable_shim(
            &shim,
            format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\n", marker.display()),
        )
        .unwrap();

        let exec = FakeExecutor::new();
        exec.seed("cleanup", "m", outcome_ok("m", "text", &[]));
        let judger = FixedJudger { score: 1.0 };
        struct StoreLoader;
        impl SessionLoader for StoreLoader {
            fn load(&self, session_id: &str) -> Option<SessionCapture> {
                Some(SessionCapture {
                    session_id: session_id.to_string(),
                    journal_path: PathBuf::from("/fake"),
                    skipped_lines: 0,
                    dropped_lines: 0,
                    integrity_errors: 0,
                    events: vec![crate::session_capture::JournalEvent {
                        event_type: "ToolCallCompleted".into(),
                        raw: serde_json::json!({
                            "ts": chrono::Utc::now().to_rfc3339(),
                            "metadata": {"run_id": "run-test"},
                            "payload": {
                            "tool_name": "memory", "is_error": false,
                            "output": format!(
                                r#"{{"memory_id":"created-by-case","session_id":"{}","created_at":null,"retrieval_score":null}}"#,
                                session_id
                            )
                        }}),
                    }],
                })
            }
        }
        let loader = StoreLoader;
        let runner = SuiteRunner {
            executor: &exec,
            judger: &judger,
            session_loader: &loader,
            digest_collector: None,
            runner_cfg: RunnerConfig::new(shim).with_fallback_models(vec!["m".into()]),
            no_judger: true,
            session_mode: SessionCaptureMode::Never,
            suite_cfg: SuiteConfig::default(),
            dashboard_tx: None,
            run_id: String::new(),
            cancel_flag: None,
        };
        let mut case = case_with("cleanup", vec![]);
        case.cleanup_memory_records = true;

        let report = runner.run_all(&[case]).await;
        assert!(report.runs[0].is_passed(), "{:#?}", report.runs[0]);
        assert_eq!(
            std::fs::read_to_string(marker).unwrap(),
            "memory\nforget\ncreated-by-case\n--reason\nAstra harness cleanup for case cleanup\n"
        );
    }

    #[tokio::test]
    async fn multi_turn_steps_tracked_in_report() {
        let exec = FakeExecutor::new();
        exec.seed("mt", "m", outcome_ok("m", "step0-text", &["Read"]));
        exec.seed("mt__step0", "m", outcome_ok("m", "step1-text", &["Write"]));

        let judger = FixedJudger { score: 1.0 };
        let loader = NoopSessionLoader;
        let cfg = RunnerConfig::new(PathBuf::from("astra")).with_fallback_models(vec!["m".into()]);
        let runner = SuiteRunner {
            executor: &exec,
            judger: &judger,
            session_loader: &loader,
            digest_collector: None,
            runner_cfg: cfg,
            no_judger: true,
            session_mode: SessionCaptureMode::Never,
            suite_cfg: SuiteConfig::default(),
            dashboard_tx: None,
            run_id: String::new(),
            cancel_flag: None,
        };
        let mut case = case_with("mt", vec![]);
        case.steps = vec![crate::case::CaseStep {
            prompt: "follow up".into(),
            criteria: vec![],
            timeout_seconds: None,
        }];
        let report = runner.run_all(&[case]).await;
        assert_eq!(report.total(), 1);
        assert!(report.runs[0].is_passed());
        assert_eq!(report.runs[0].steps.len(), 1);
        assert_eq!(report.runs[0].steps[0].step_index, 0);
        assert_eq!(report.runs[0].steps[0].prompt, "follow up");
        // Text accumulates across turns (not last-step-wins).
        let merged = &report.runs[0].outcome.text;
        assert!(
            merged.contains("step0-text") && merged.contains("step1-text"),
            "accumulated text should contain both turns; got: {merged:?}"
        );
    }

    #[tokio::test]
    async fn lifecycle_gate_rejects_nonzero_root_and_follow_up_outcomes() {
        let exec = FakeExecutor::new();
        let mut root = outcome_ok("m", "root", &["Read"]);
        root.exit_code = 9;
        exec.seed("lifecycle-root", "m", root);
        let mut step = outcome_ok("m", "step", &[]);
        step.exit_code = -1;
        step.session_id = None;
        exec.seed("lifecycle-root__step0", "m", step);

        let judger = FixedJudger { score: 1.0 };
        let loader = NoopSessionLoader;
        let runner = SuiteRunner {
            executor: &exec,
            judger: &judger,
            session_loader: &loader,
            digest_collector: None,
            runner_cfg: RunnerConfig::new(PathBuf::from("astra"))
                .with_fallback_models(vec!["m".into()]),
            no_judger: true,
            session_mode: SessionCaptureMode::Never,
            suite_cfg: SuiteConfig::default(),
            dashboard_tx: None,
            run_id: String::new(),
            cancel_flag: None,
        };
        let mut case = case_with(
            "lifecycle-root",
            vec![Criterion::ToolCalled {
                name: "Read".into(),
            }],
        );
        case.steps = vec![crate::case::CaseStep {
            prompt: "follow up".into(),
            criteria: vec![],
            timeout_seconds: None,
        }];

        let report = runner.run_all(&[case]).await;
        assert!(!report.runs[0].is_passed());
        assert!(
            report.runs[0]
                .outcome
                .stderr
                .contains("lifecycle: root turn did not reach")
        );
        assert!(!report.runs[0].steps[0].passed);
    }

    #[tokio::test]
    async fn lifecycle_gate_rejects_follow_up_session_identity_drift() {
        let exec = FakeExecutor::new();
        exec.seed("lifecycle-session", "m", outcome_ok("m", "root", &[]));
        let mut step = outcome_ok("m", "step", &[]);
        step.session_id = Some("660e8400-e29b-41d4-a716-446655440000".into());
        exec.seed("lifecycle-session__step0", "m", step);

        let judger = FixedJudger { score: 1.0 };
        let loader = NoopSessionLoader;
        let runner = SuiteRunner {
            executor: &exec,
            judger: &judger,
            session_loader: &loader,
            digest_collector: None,
            runner_cfg: RunnerConfig::new(PathBuf::from("astra"))
                .with_fallback_models(vec!["m".into()]),
            no_judger: true,
            session_mode: SessionCaptureMode::Never,
            suite_cfg: SuiteConfig::default(),
            dashboard_tx: None,
            run_id: String::new(),
            cancel_flag: None,
        };
        let mut case = case_with("lifecycle-session", vec![]);
        case.steps = vec![crate::case::CaseStep {
            prompt: "follow up".into(),
            criteria: vec![],
            timeout_seconds: None,
        }];

        let report = runner.run_all(&[case]).await;
        assert!(!report.runs[0].is_passed());
        assert!(
            report.runs[0]
                .outcome
                .stderr
                .contains("session identity diverged")
        );
        assert!(!report.runs[0].steps[0].passed);
    }

    #[tokio::test]
    async fn lifecycle_gate_rejects_invalid_equal_root_identity_before_follow_up() {
        let exec = FakeExecutor::new();
        let mut root = outcome_ok("m", "root", &[]);
        root.session_id = Some("not-a-uuid".into());
        exec.seed("invalid-session", "m", root);
        let mut step = outcome_ok("m", "step", &[]);
        step.session_id = Some("not-a-uuid".into());
        exec.seed("invalid-session__step0", "m", step);

        let judger = FixedJudger { score: 1.0 };
        let loader = NoopSessionLoader;
        let runner = SuiteRunner {
            executor: &exec,
            judger: &judger,
            session_loader: &loader,
            digest_collector: None,
            runner_cfg: RunnerConfig::new(PathBuf::from("astra"))
                .with_fallback_models(vec!["m".into()]),
            no_judger: true,
            session_mode: SessionCaptureMode::Never,
            suite_cfg: SuiteConfig::default(),
            dashboard_tx: None,
            run_id: String::new(),
            cancel_flag: None,
        };
        let mut case = case_with("invalid-session", vec![]);
        case.steps = vec![crate::case::CaseStep {
            prompt: "follow up".into(),
            criteria: vec![],
            timeout_seconds: None,
        }];

        let report = runner.run_all(&[case]).await;
        assert!(!report.runs[0].is_passed());
        assert!(report.runs[0].steps.is_empty());
        assert!(
            report.runs[0]
                .outcome
                .stderr
                .contains("valid server-issued UUID session_id")
        );
        assert_eq!(
            exec.calls.lock().unwrap_or_else(|e| e.into_inner()).len(),
            1,
            "invalid root identity must prevent a follow-up admission"
        );
    }

    #[tokio::test]
    async fn lifecycle_gate_rejects_missing_root_identity_on_success() {
        let exec = FakeExecutor::new();
        let mut root = outcome_ok("m", "root", &[]);
        root.session_id = None;
        exec.seed("missing-session", "m", root);

        let judger = FixedJudger { score: 1.0 };
        let loader = NoopSessionLoader;
        let runner = SuiteRunner {
            executor: &exec,
            judger: &judger,
            session_loader: &loader,
            digest_collector: None,
            runner_cfg: RunnerConfig::new(PathBuf::from("astra"))
                .with_fallback_models(vec!["m".into()]),
            no_judger: true,
            session_mode: SessionCaptureMode::Never,
            suite_cfg: SuiteConfig::default(),
            dashboard_tx: None,
            run_id: String::new(),
            cancel_flag: None,
        };

        let report = runner
            .run_all(&[case_with("missing-session", vec![])])
            .await;
        assert!(!report.runs[0].is_passed());
        assert!(
            report.runs[0]
                .outcome
                .stderr
                .contains("did not return the server-issued UUID session_id")
        );
    }

    #[tokio::test]
    async fn step_criteria_evaluated_and_fail_propagates() {
        let exec = FakeExecutor::new();
        exec.seed("mt", "m", outcome_ok("m", "step0-text", &["bash"]));
        // Step outcome has no tools — step criterion tool_called will fail.
        exec.seed("mt__step0", "m", outcome_ok("m", "step1-no-tools", &[]));

        let judger = FixedJudger { score: 1.0 };
        let loader = NoopSessionLoader;
        let cfg = RunnerConfig::new(PathBuf::from("astra")).with_fallback_models(vec!["m".into()]);
        let runner = SuiteRunner {
            executor: &exec,
            judger: &judger,
            session_loader: &loader,
            digest_collector: None,
            runner_cfg: cfg,
            no_judger: true,
            session_mode: SessionCaptureMode::Never,
            suite_cfg: SuiteConfig::default(),
            dashboard_tx: None,
            run_id: String::new(),
            cancel_flag: None,
        };
        let mut case = case_with("mt", vec![]);
        case.steps = vec![crate::case::CaseStep {
            prompt: "do something with bash".into(),
            criteria: vec![crate::criteria::Criterion::ToolCalled {
                name: "bash".into(),
            }],
            timeout_seconds: None,
        }];
        let report = runner.run_all(&[case]).await;
        assert_eq!(report.total(), 1);
        // Step criterion fails → overall case fails.
        assert!(
            !report.runs[0].is_passed(),
            "step criteria failure must propagate"
        );
        assert!(!report.runs[0].steps[0].passed);
        assert_eq!(report.runs[0].steps[0].criteria.len(), 1);
        assert!(!report.runs[0].steps[0].criteria[0].passed);
    }

    #[tokio::test]
    async fn required_step_judger_is_evaluated_not_left_as_a_placeholder() {
        let exec = FakeExecutor::new();
        exec.seed("mt", "m", outcome_ok("m", "step0-text", &[]));
        exec.seed(
            "mt__step0",
            "m",
            outcome_ok("m", "purge confirmed", &["memory"]),
        );

        let judger = FixedJudger { score: 1.0 };
        let loader = NoopSessionLoader;
        let runner = SuiteRunner {
            executor: &exec,
            judger: &judger,
            session_loader: &loader,
            digest_collector: None,
            runner_cfg: RunnerConfig::new(PathBuf::from("astra"))
                .with_fallback_models(vec!["m".into()]),
            no_judger: false,
            session_mode: SessionCaptureMode::Never,
            suite_cfg: SuiteConfig::default(),
            dashboard_tx: None,
            run_id: String::new(),
            cancel_flag: None,
        };
        let mut case = case_with("mt", vec![]);
        case.steps = vec![crate::case::CaseStep {
            prompt: "purge the record".into(),
            criteria: vec![Criterion::HardJudger {
                question: "did the purge succeed?".into(),
                threshold: 0.7,
                model: None,
            }],
            timeout_seconds: None,
        }];

        let report = runner.run_all(&[case]).await;
        assert!(report.runs[0].is_passed(), "{:#?}", report.runs[0]);
        assert!(report.runs[0].steps[0].criteria[0].passed);
        assert_eq!(report.runs[0].steps[0].criteria[0].score, Some(1.0));
    }

    #[tokio::test]
    async fn capability_and_difficulty_in_report() {
        let exec = FakeExecutor::new();
        exec.seed("c1", "m", outcome_ok("m", "text", &[]));
        let judger = FixedJudger { score: 1.0 };
        let loader = NoopSessionLoader;
        let cfg = RunnerConfig::new(PathBuf::from("astra")).with_fallback_models(vec!["m".into()]);
        let runner = SuiteRunner {
            executor: &exec,
            judger: &judger,
            session_loader: &loader,
            digest_collector: None,
            runner_cfg: cfg,
            no_judger: true,
            session_mode: SessionCaptureMode::Never,
            suite_cfg: SuiteConfig::default(),
            dashboard_tx: None,
            run_id: String::new(),
            cancel_flag: None,
        };
        let mut case = case_with("c1", vec![]);
        case.capability = Some(crate::case::Capability::ToolUse);
        case.difficulty = Some(3);
        let report = runner.run_all(&[case]).await;
        assert_eq!(
            report.runs[0].capability,
            Some(crate::case::Capability::ToolUse)
        );
        assert_eq!(report.runs[0].difficulty, Some(3));
    }

    #[tokio::test]
    async fn wall_time_populated() {
        let exec = FakeExecutor::new();
        exec.seed("c1", "m", outcome_ok("m", "text", &[]));
        let judger = FixedJudger { score: 1.0 };
        let loader = NoopSessionLoader;
        let cfg = RunnerConfig::new(PathBuf::from("astra")).with_fallback_models(vec!["m".into()]);
        let runner = SuiteRunner {
            executor: &exec,
            judger: &judger,
            session_loader: &loader,
            digest_collector: None,
            runner_cfg: cfg,
            no_judger: true,
            session_mode: SessionCaptureMode::Never,
            suite_cfg: SuiteConfig::default(),
            dashboard_tx: None,
            run_id: String::new(),
            cancel_flag: None,
        };
        let report = runner.run_all(&[case_with("c1", vec![])]).await;
        assert!(report.started_at.is_some());
        assert!(report.ended_at.is_some());
        // wall_time_ms should be at least 0 (test completes instantly)
        // but the field must be populated.
    }

    #[tokio::test]
    async fn deterministic_multi_turn_case_does_not_invoke_a_judger() {
        let exec = FakeExecutor::new();
        exec.seed("mt", "m", outcome_ok("m", "step0", &[]));
        exec.seed("mt__step0", "m", outcome_ok("m", "6", &[]));

        struct CaptureJudger {
            questions: std::sync::Mutex<Vec<String>>,
        }
        #[async_trait]
        impl Judger for CaptureJudger {
            async fn score(
                &self,
                q: &str,
                _m: Option<&str>,
                _o: &RunOutcome,
            ) -> Result<crate::judger::JudgerScore, String> {
                self.questions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(q.to_string());
                Ok(crate::judger::JudgerScore {
                    score: 1.0,
                    rationale: "ok".into(),
                    full_rationale: "ok".into(),
                    votes: vec![],
                })
            }
        }
        let judger = CaptureJudger {
            questions: std::sync::Mutex::new(vec![]),
        };
        let loader = NoopSessionLoader;
        let cfg = RunnerConfig::new(PathBuf::from("astra")).with_fallback_models(vec!["m".into()]);
        let runner = SuiteRunner {
            executor: &exec,
            judger: &judger,
            session_loader: &loader,
            digest_collector: None,
            runner_cfg: cfg,
            no_judger: false,
            session_mode: SessionCaptureMode::Never,
            suite_cfg: SuiteConfig::default(),
            dashboard_tx: None,
            run_id: String::new(),
            cancel_flag: None,
        };
        let mut case = case_with("mt", vec![]);
        case.prompt = "What is 2+2? Answer with just the number.".into();
        case.steps = vec![crate::case::CaseStep {
            prompt: "Actually what is 2+2+2?".into(),
            criteria: vec![],
            timeout_seconds: None,
        }];
        let _ = runner.run_all(&[case]).await;
        let questions = judger.questions.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            questions.is_empty(),
            "only an explicit semantic criterion may spend a judge call or product session"
        );
    }

    #[tokio::test]
    async fn auth_failure_warning_on_non_consecutive_failures() {
        let exec = FakeExecutor::new();
        // c0: auth fail, c1: pass, c2: auth fail → total=2, warning expected
        let auth_failure = || {
            RunOutcome::new("m")
                .with_exit_code(3)
                .with_stderr("Could not validate credentials")
        };
        exec.seed("c0", "m", auth_failure());
        exec.seed("c1", "m", outcome_ok("m", "ok", &[]));
        exec.seed("c2", "m", auth_failure());

        let judger = FixedJudger { score: 1.0 };
        let loader = NoopSessionLoader;
        let cfg = RunnerConfig::new(PathBuf::from("astra")).with_fallback_models(vec!["m".into()]);
        let runner = SuiteRunner {
            executor: &exec,
            judger: &judger,
            session_loader: &loader,
            digest_collector: None,
            runner_cfg: cfg,
            no_judger: true,
            session_mode: SessionCaptureMode::Never,
            suite_cfg: SuiteConfig {
                circuit_breaker_threshold: 10,
                ..Default::default()
            },
            dashboard_tx: None,
            run_id: String::new(),
            cancel_flag: None,
        };
        let cases = vec![
            case_with("c0", vec![Criterion::ExitCode { code: 0 }]),
            case_with("c1", vec![]),
            case_with("c2", vec![Criterion::ExitCode { code: 0 }]),
        ];
        let report = runner.run_all(&cases).await;
        // c0 and c2 fail (auth), c1 passes
        assert_eq!(report.failed(), 2);
        assert_eq!(report.passed(), 1);
        // The warning was printed to stderr (we can't capture eprintln
        // easily, but the auth counter logic is exercised).
    }

    #[tokio::test]
    async fn parallel_execution_produces_sorted_results() {
        let exec = FakeExecutor::new();
        exec.seed("b", "m", outcome_ok("m", "b-text", &[]));
        exec.seed("a", "m", outcome_ok("m", "a-text", &[]));
        exec.seed("c", "m", outcome_ok("m", "c-text", &[]));

        let judger = FixedJudger { score: 1.0 };
        let loader = NoopSessionLoader;
        let cfg = RunnerConfig::new(PathBuf::from("astra")).with_fallback_models(vec!["m".into()]);
        let runner = SuiteRunner {
            executor: &exec,
            judger: &judger,
            session_loader: &loader,
            digest_collector: None,
            runner_cfg: cfg,
            no_judger: true,
            session_mode: SessionCaptureMode::Never,
            suite_cfg: SuiteConfig {
                parallel: 3,
                ..Default::default()
            },
            dashboard_tx: None,
            run_id: String::new(),
            cancel_flag: None,
        };
        let cases = vec![
            case_with("b", vec![]),
            case_with("a", vec![]),
            case_with("c", vec![]),
        ];
        let report = runner.run_all(&cases).await;
        assert_eq!(report.total(), 3);
        assert_eq!(report.passed(), 3);
        // Parallel path sorts by (case_name, model, run_index).
        let names: Vec<&str> = report.runs.iter().map(|r| r.case_name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn cancel_flag_stops_run_early() {
        let exec = FakeExecutor::new();
        for i in 0..5 {
            let name = format!("c{i}");
            exec.seed(&name, "m", outcome_ok("m", "text", &[]));
        }

        let cancel = Arc::new(AtomicBool::new(false));
        let judger = FixedJudger { score: 1.0 };
        let loader = NoopSessionLoader;
        let cfg = RunnerConfig::new(PathBuf::from("astra")).with_fallback_models(vec!["m".into()]);

        // Use a custom executor that sets the cancel flag after the first case.
        struct CancelAfterFirst {
            inner: FakeExecutor,
            flag: Arc<AtomicBool>,
            call_count: AtomicUsize,
        }
        #[async_trait]
        impl CaseExecutor for CancelAfterFirst {
            async fn execute(&self, case: &Case, model: &str) -> RunOutcome {
                let n = self.call_count.fetch_add(1, Ordering::Relaxed);
                if n >= 1 {
                    // After the first case completes, set cancel flag.
                    self.flag.store(true, Ordering::Relaxed);
                }
                self.inner.execute(case, model).await
            }
            fn reproducer(&self, _case: &Case, _model: &str) -> String {
                String::new()
            }
        }

        let cancel_exec = CancelAfterFirst {
            inner: {
                let e = FakeExecutor::new();
                for i in 0..5 {
                    let name = format!("c{i}");
                    e.seed(&name, "m", outcome_ok("m", "text", &[]));
                }
                e
            },
            flag: cancel.clone(),
            call_count: AtomicUsize::new(0),
        };

        let runner = SuiteRunner {
            executor: &cancel_exec,
            judger: &judger,
            session_loader: &loader,
            digest_collector: None,
            runner_cfg: cfg,
            no_judger: true,
            session_mode: SessionCaptureMode::Never,
            suite_cfg: SuiteConfig::default(),
            dashboard_tx: None,
            run_id: String::new(),
            cancel_flag: Some(cancel),
        };
        let cases: Vec<Case> = (0..5)
            .map(|i| case_with(&format!("c{i}"), vec![]))
            .collect();
        let report = runner.run_all(&cases).await;
        // Cancellation stops execution, but planned work is represented by
        // explicit terminal rows instead of disappearing from the report.
        assert_eq!(report.total(), 5);
        assert_eq!(report.passed(), 2);
        assert_eq!(report.cancelled(), 3);
        assert!(report.runs[2..].iter().all(|r| r.is_cancelled()));
    }

    #[tokio::test]
    async fn parallel_cancel_does_not_launch_cases_that_are_still_queued() {
        struct SlowExecutor {
            calls: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl CaseExecutor for SlowExecutor {
            async fn execute(&self, _case: &Case, model: &str) -> RunOutcome {
                self.calls.fetch_add(1, Ordering::Relaxed);
                tokio::time::sleep(Duration::from_millis(80)).await;
                outcome_ok(model, "done", &[])
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let exec = SlowExecutor {
            calls: calls.clone(),
        };
        let judger = FixedJudger { score: 1.0 };
        let loader = NoopSessionLoader;
        let runner = SuiteRunner {
            executor: &exec,
            judger: &judger,
            session_loader: &loader,
            digest_collector: None,
            runner_cfg: RunnerConfig::new(PathBuf::from("astra"))
                .with_fallback_models(vec!["m".into()]),
            no_judger: true,
            session_mode: SessionCaptureMode::Never,
            suite_cfg: SuiteConfig {
                parallel: 2,
                ..Default::default()
            },
            dashboard_tx: None,
            run_id: String::new(),
            cancel_flag: Some(cancel.clone()),
        };
        let cases: Vec<Case> = (0..4)
            .map(|index| case_with(&format!("parallel-{index}"), vec![]))
            .collect();

        let mut run = Box::pin(runner.run_all(&cases));
        tokio::select! {
            report = &mut run => panic!("run completed before cancellation: {report:?}"),
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
        cancel.store(true, Ordering::Relaxed);
        let report = run.await;

        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert_eq!(report.passed(), 2);
        assert_eq!(report.cancelled(), 2);
        assert!(
            report
                .runs
                .iter()
                .filter(|r| r.is_cancelled())
                .all(|r| { r.outcome.text.contains("cancelled") })
        );
    }

    #[tokio::test]
    async fn has_warnings_when_soft_criterion_fails() {
        let exec = FakeExecutor::new();
        // Outcome with 0 tokens — the TokensBetween criterion will fail.
        exec.seed("c1", "m", outcome_ok("m", "hello", &["Read"]));

        let judger = FixedJudger { score: 1.0 };
        let loader = NoopSessionLoader;
        let cfg = RunnerConfig::new(PathBuf::from("astra")).with_fallback_models(vec!["m".into()]);
        let runner = SuiteRunner {
            executor: &exec,
            judger: &judger,
            session_loader: &loader,
            digest_collector: None,
            runner_cfg: cfg,
            no_judger: true,
            session_mode: SessionCaptureMode::Never,
            suite_cfg: SuiteConfig::default(),
            dashboard_tx: None,
            run_id: String::new(),
            cancel_flag: None,
        };
        // Hard criterion passes (ToolCalled "Read"), Soft criterion fails
        // (TokensBetween 100..500, but outcome has 0 tokens).
        let case = case_with(
            "c1",
            vec![
                Criterion::ToolCalled {
                    name: "Read".into(),
                },
                Criterion::TokensBetween { min: 100, max: 500 },
            ],
        );
        let report = runner.run_all(&[case]).await;
        assert!(
            report.runs[0].is_passed(),
            "case should PASS because Hard criterion passed"
        );
        assert!(
            report.runs[0].has_warnings,
            "case should have warnings because Soft criterion failed"
        );
    }

    #[tokio::test]
    async fn step_soft_criterion_failure_does_not_fail_case() {
        let exec = FakeExecutor::new();
        exec.seed("mt", "m", outcome_ok("m", "turn0", &["Read"]));
        // Step outcome: 0 tokens, so TokensBetween will fail.
        exec.seed("mt__step0", "m", outcome_ok("m", "turn1", &["Write"]));

        let judger = FixedJudger { score: 1.0 };
        let loader = NoopSessionLoader;
        let cfg = RunnerConfig::new(PathBuf::from("astra")).with_fallback_models(vec!["m".into()]);
        let runner = SuiteRunner {
            executor: &exec,
            judger: &judger,
            session_loader: &loader,
            digest_collector: None,
            runner_cfg: cfg,
            no_judger: true,
            session_mode: SessionCaptureMode::Never,
            suite_cfg: SuiteConfig::default(),
            dashboard_tx: None,
            run_id: String::new(),
            cancel_flag: None,
        };
        let mut case = case_with("mt", vec![]);
        case.steps = vec![crate::case::CaseStep {
            prompt: "follow up".into(),
            // Soft criterion that will fail (0 tokens not in 100..500).
            criteria: vec![Criterion::TokensBetween { min: 100, max: 500 }],
            timeout_seconds: None,
        }];
        let report = runner.run_all(&[case]).await;
        assert_eq!(report.total(), 1);
        // Step's Soft criterion fails, but the case should still PASS
        // because only Hard step criteria cause case failure.
        assert!(
            report.runs[0].is_passed(),
            "step soft criterion failure must NOT fail the case"
        );
        // The step itself should be marked as passed (only Hard matters).
        assert!(
            report.runs[0].steps[0].passed,
            "step should be passed since only Soft criterion failed"
        );
        // Verify the soft criterion actually failed.
        assert!(
            !report.runs[0].steps[0].criteria[0].passed,
            "soft criterion should report as failed"
        );
    }
}

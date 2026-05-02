//! Suite orchestration with parallel execution, circuit breaker,
//! failure classification, and retry on rate-limit.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::Semaphore;

use crate::case::Case;
use crate::classify::{FailureClass, classify};
use crate::criteria::{Criterion, CriterionSeverity, evaluate_deterministic_with_session};
use crate::digest::DigestCollector;
use crate::exec::CaseExecutor;
use crate::judger::{Judger, evaluate_judger};
use crate::report::{CaseRunReport, StepResult, SuiteReport};
use crate::runner::{RunOutcome, RunnerConfig, resolve_models};
use crate::session_capture::{SessionCapture, load_session};

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
                    eprintln!("[astra-test] skip case {:?}: {e}", case.name);
                }
            }
        }

        let semaphore = Arc::new(Semaphore::new(self.suite_cfg.parallel));
        let aborted = Arc::new(AtomicBool::new(false));
        let consecutive_infra = Arc::new(AtomicUsize::new(0));
        let total_auth_failures = Arc::new(AtomicUsize::new(0));

        let mut suite = SuiteReport {
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
                total_cases: work.len(),
                models,
                started_at,
                source: "suite".into(),
            });
        }

        if self.suite_cfg.parallel <= 1 {
            // Serial path: simpler, preserves ordering, supports circuit breaker inline.
            for (case, model, run_index) in work {
                if self
                    .cancel_flag
                    .as_ref()
                    .is_some_and(|f| f.load(Ordering::Relaxed))
                {
                    eprintln!("[astra-test] run cancelled by user");
                    break;
                }
                if aborted.load(Ordering::Relaxed) {
                    eprintln!("[astra-test] circuit breaker tripped — aborting remaining cases");
                    break;
                }
                if let Some(ref tx) = self.dashboard_tx {
                    let _ = tx.send(crate::dashboard::DashboardEvent::CaseStarted {
                        run_id: run_id.clone(),
                        case_name: case.name.clone(),
                        model: model.clone(),
                        run_index,
                    });
                }
                let mut report = self.run_one(case, &model).await;
                report.run_index = run_index;
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
                    });
                }
                suite.runs.push(report);
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
                            return None;
                        }
                        if aborted.load(Ordering::Relaxed) {
                            return None;
                        }
                        let _permit = sem.acquire().await.ok()?;
                        if aborted.load(Ordering::Relaxed) {
                            return None;
                        }
                        if let Some(ref tx) = dashboard_tx {
                            let _ = tx.send(crate::dashboard::DashboardEvent::CaseStarted {
                                run_id: run_id.clone(),
                                case_name: case.name.clone(),
                                model: model.clone(),
                                run_index,
                            });
                        }
                        let mut report = self.run_one(case, &model).await;
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
                            });
                        }
                        Some(report)
                    }
                })
                .collect();

            let results: Vec<_> = futures.collect().await;
            for r in results.into_iter().flatten() {
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
        if let Some(ref tx) = self.dashboard_tx {
            let _ = tx.send(crate::dashboard::DashboardEvent::SuiteCompleted {
                run_id: run_id.clone(),
                report: Arc::new(suite.clone()),
            });
        }
        suite
    }

    fn update_circuit_breaker(
        &self,
        report: &CaseRunReport,
        consecutive_infra: &AtomicUsize,
        aborted: &AtomicBool,
        total_auth_failures: &AtomicUsize,
    ) {
        if report.passed {
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
                    | FailureClass::InfraTimeout
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

    async fn run_one(&self, case: &Case, model: &str) -> CaseRunReport {
        // Run setup command if specified. Non-zero exit aborts the case.
        if let Some(ref cmd) = case.setup_cmd {
            let mut setup_cmd = tokio::process::Command::new("sh");
            if let Some(ref wd) = self.runner_cfg.working_dir {
                setup_cmd.current_dir(wd);
            }
            setup_cmd.arg("-c").arg(cmd).kill_on_drop(true);
            let status = tokio::time::timeout(Duration::from_secs(30), setup_cmd.status()).await;
            let failed = match status {
                Ok(Ok(s)) => !s.success(),
                Ok(Err(_)) => true,
                Err(_) => {
                    eprintln!(
                        "[astra-test] setup_cmd timed out (30s) for case={}",
                        case.name
                    );
                    true
                }
            };
            if failed {
                let exit = status
                    .ok()
                    .and_then(|r| r.ok())
                    .and_then(|s| s.code())
                    .unwrap_or(-1);
                eprintln!(
                    "[astra-test] setup_cmd failed for case={}; aborting (exit {})",
                    case.name, exit
                );
                return CaseRunReport {
                    case_name: case.name.clone(),
                    model: model.to_string(),
                    passed: false,
                    run_index: 0,
                    capability: case.capability.clone(),
                    weight: case.weight,
                    difficulty: case.difficulty,
                    outcome: RunOutcome::new(model)
                        .with_text(format!("setup_cmd failed: exit {exit}")),
                    criteria: vec![],
                    steps: vec![],
                    session: None,
                    reproducer: None,
                    digest: None,
                    digest_error: None,
                    failure_class: Some(crate::classify::FailureClass::PlatformSetupFailed),
                    has_warnings: false,
                };
            }
        }

        let mut outcome = self.executor.execute(case, model).await;
        let mut step_results: Vec<StepResult> = Vec::new();

        // Multi-turn: execute follow-up steps using the same session.
        if !case.steps.is_empty() && outcome.session_id.is_none() {
            eprintln!(
                "[astra-test] WARNING: case {} has {} steps but turn 1 returned no session_id — \
                 steps will be skipped. This usually means the first turn failed.",
                case.name,
                case.steps.len()
            );
        }
        if !case.steps.is_empty()
            && let Some(ref session_id) = outcome.session_id
        {
            for (idx, step) in case.steps.iter().enumerate() {
                let step_case = Case {
                    name: format!("{}__step{}", case.name, idx),
                    description: None,
                    prompt: step.prompt.clone(),
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
                    difficulty: None,
                    weight: 1.0,
                    steps: vec![],
                    setup_cmd: None,
                    teardown_cmd: None,
                };
                let step_outcome = self.executor.execute(&step_case, model).await;

                // Evaluate step-level criteria against this step's outcome.
                let step_criteria_results = if !step.criteria.is_empty() {
                    evaluate_deterministic_with_session(&step.criteria, &step_outcome, None)
                } else {
                    vec![]
                };
                let step_passed = step_criteria_results
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
                outcome.duration_ms += step_outcome.duration_ms;
                outcome.tool_calls_count += step_outcome.tool_calls_count;
                outcome.tools_used.extend(step_outcome.tools_used);
                outcome.turn_rounds += step_outcome.turn_rounds;
                outcome.total_tool_calls += step_outcome.total_tool_calls;
                outcome.cache_hits += step_outcome.cache_hits;
                // Last step's text/exit_code wins.
                outcome.text = step_outcome.text;
                outcome.exit_code = step_outcome.exit_code;
                if !step_outcome.stderr.is_empty() {
                    outcome.stderr.push('\n');
                    outcome.stderr.push_str(&step_outcome.stderr);
                }
            }
        }

        // Retry on 429 if enabled. Skip for multi-turn cases because
        // retrying only re-executes the first turn, not the full step sequence.
        if self.suite_cfg.retry_on_429 && case.steps.is_empty() && is_rate_limited(&outcome) {
            eprintln!(
                "[astra-test] rate-limited on case={} model={}, retrying after 5s",
                case.name, model
            );
            tokio::time::sleep(Duration::from_secs(5)).await;
            outcome = self.executor.execute(case, model).await;
        }

        // Load session.
        let session = if self.session_mode.should_load(case) {
            outcome
                .session_id
                .as_deref()
                .and_then(|id| self.session_loader.load(id))
        } else {
            None
        };

        // Evaluate criteria.
        // Auto-attach a default judger criterion when the case has none
        // and judger is enabled. This ensures every case gets a quality check.
        let mut criteria = case.criteria.clone();
        if !self.no_judger
            && !criteria
                .iter()
                .any(|c| matches!(c, Criterion::Judger { .. }))
        {
            let question = if case.steps.is_empty() {
                let preview: String = case.prompt.trim().chars().take(500).collect();
                format!(
                    "Given the task: \"{preview}\"\nDid the agent complete it correctly and efficiently? \
                     Score 0.0 for wrong/incomplete, 0.5 for partially correct, 1.0 for fully correct."
                )
            } else {
                let initial: String = case.prompt.trim().chars().take(300).collect();
                let last_step = &case.steps.last().unwrap().prompt;
                let followup: String = last_step.trim().chars().take(300).collect();
                format!(
                    "This is a multi-turn conversation.\n\
                     Initial task: \"{initial}\"\n\
                     Follow-up instruction: \"{followup}\"\n\
                     The agent's final response should address the FOLLOW-UP instruction \
                     (which may refine or change the original task).\n\
                     Score 0.0 for wrong/incomplete, 0.5 for partially correct, 1.0 for fully correct."
                )
            };
            criteria.push(Criterion::Judger {
                question,
                threshold: 0.7,
                model: None,
            });
        }

        let mut det = evaluate_deterministic_with_session(&criteria, &outcome, session.as_ref());

        // Always run the judger (unless --no-judger) — the quality
        // score is useful for diagnostics even when Hard criteria fail.
        if !self.no_judger {
            for (i, c) in criteria.iter().enumerate() {
                if let Criterion::Judger { .. } = c
                    && let Some(res) = evaluate_judger(self.judger, c, &outcome).await
                {
                    det[i] = res;
                }
            }
        } else {
            for (i, c) in criteria.iter().enumerate() {
                if let Criterion::Judger { .. } = c {
                    det[i].passed = true;
                    det[i].detail = "judger skipped (--no-judger)".into();
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
        let passed = hard_passed && steps_passed;

        // Classify failure.
        let failure_class = if !passed {
            Some(classify(&outcome, &det))
        } else {
            None
        };

        let reproducer = {
            let r = self.executor.reproducer(case, model);
            if r.is_empty() { None } else { Some(r) }
        };

        // Digest on FAIL.
        let (digest, digest_error) = if !passed
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
        if let Some(ref cmd) = case.teardown_cmd {
            let mut teardown_cmd = tokio::process::Command::new("sh");
            if let Some(ref wd) = self.runner_cfg.working_dir {
                teardown_cmd.current_dir(wd);
            }
            teardown_cmd.arg("-c").arg(cmd).kill_on_drop(true);
            match tokio::time::timeout(Duration::from_secs(30), teardown_cmd.status()).await {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => {
                    eprintln!(
                        "[astra-test] teardown_cmd error for case={}: {e}",
                        case.name
                    );
                }
                Err(_) => {
                    eprintln!(
                        "[astra-test] teardown_cmd timed out (30s) for case={}, continuing",
                        case.name
                    );
                }
            }
        }

        // Progress: emit per-case result to stderr so long runs show
        // streaming progress even when stdout is buffered.
        let marker = if passed { "PASS" } else { "FAIL" };
        eprintln!(
            "[astra-test] [{marker}] {case} × {model} ({dur}ms)",
            case = case.name,
            model = model,
            dur = outcome.duration_ms,
        );

        CaseRunReport {
            case_name: case.name.clone(),
            model: model.to_string(),
            passed,
            run_index: 0, // overwritten by caller
            capability: case.capability.clone(),
            weight: case.weight,
            difficulty: case.difficulty,
            outcome,
            criteria: det,
            steps: step_results,
            session,
            reproducer,
            digest,
            digest_error,
            failure_class,
            has_warnings: passed && !all_passed,
        }
    }
}

fn is_rate_limited(outcome: &crate::runner::RunOutcome) -> bool {
    let s = &outcome.stderr;
    s.contains("Too many requests")
        || s.contains("rate_limit")
        || s.contains("rate limit")
        || s.contains("[rate_limit]")
        || (s.contains("429") && (s.contains("error") || s.contains("Error") || s.contains("HTTP")))
}

#[cfg(test)]
mod tests {
    use super::*;
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
            models: None,
            criteria,
            debug_log: false,
            extra_cli_args: vec![],
            timeout_seconds: 60,
            capability: None,
            difficulty: None,
            weight: 1.0,
            steps: vec![],
            setup_cmd: None,
            teardown_cmd: None,
        }
    }

    fn outcome_ok(model: &str, text: &str, tools: &[&str]) -> RunOutcome {
        RunOutcome {
            model: model.into(),
            exit_code: 0,
            text: text.into(),
            stderr: String::new(),
            session_id: Some(format!("sess-{model}")),
            run_id: None,
            tool_calls_count: tools.len() as u32,
            tools_used: tools.iter().map(|s| s.to_string()).collect(),
            completion_tokens: 0,
            prompt_tokens: 0,
            duration_ms: 12,
            turn_rounds: 1,
            cache_hits: 0,
            total_tool_calls: tools.len() as u32,
            ttft_ms: 0,
        }
    }

    struct FixedJudger {
        score: f64,
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
    async fn circuit_breaker_aborts_on_consecutive_infra() {
        let exec = FakeExecutor::new();
        // All cases return auth failure.
        for i in 0..5 {
            let name = format!("c{i}");
            exec.seed(&name, "m", RunOutcome::new("m").with_exit_code(3));
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
        // Circuit breaker should trip after 3, so we get 3 runs not 5.
        assert_eq!(report.total(), 3);
        assert_eq!(report.failed(), 3);
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
                *self.hits.lock().unwrap() += 1;
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
        assert_eq!(*judger.hits.lock().unwrap(), 2);
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
    async fn skips_case_when_no_model_source() {
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
        assert_eq!(report.total(), 0);
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
        digest.seed_ok("sess-m", serde_json::json!({"aggregates": {"turns": 2}}));

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
        let calls = digest.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0], "sess-m");
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
        assert_eq!(s.session_id, "sess-m");
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
        case.setup_cmd = Some("exit 1".into());
        let report = runner.run_all(&[case]).await;
        assert_eq!(report.total(), 1);
        assert!(
            !report.runs[0].passed,
            "setup failure must mark case as FAIL"
        );
        assert_eq!(
            report.runs[0].failure_class,
            Some(crate::classify::FailureClass::PlatformSetupFailed)
        );
        assert!(
            exec.calls.lock().unwrap().is_empty(),
            "executor must NOT be called when setup fails"
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
        assert!(report.runs[0].passed);
        assert_eq!(report.runs[0].steps.len(), 1);
        assert_eq!(report.runs[0].steps[0].step_index, 0);
        assert_eq!(report.runs[0].steps[0].prompt, "follow up");
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
            !report.runs[0].passed,
            "step criteria failure must propagate"
        );
        assert!(!report.runs[0].steps[0].passed);
        assert_eq!(report.runs[0].steps[0].criteria.len(), 1);
        assert!(!report.runs[0].steps[0].criteria[0].passed);
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
    async fn auto_judger_uses_last_step_prompt_for_multi_turn() {
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
                self.questions.lock().unwrap().push(q.to_string());
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
        let questions = judger.questions.lock().unwrap();
        assert_eq!(questions.len(), 1);
        let q = &questions[0];
        assert!(
            q.contains("2+2+2"),
            "auto-judger question must reference the LAST step prompt, not the initial: {q}"
        );
        assert!(
            q.contains("multi-turn") || q.contains("Follow-up"),
            "auto-judger should indicate this is multi-turn: {q}"
        );
    }

    #[tokio::test]
    async fn auth_failure_warning_on_non_consecutive_failures() {
        let exec = FakeExecutor::new();
        // c0: auth fail, c1: pass, c2: auth fail → total=2, warning expected
        exec.seed("c0", "m", RunOutcome::new("m").with_exit_code(3));
        exec.seed("c1", "m", outcome_ok("m", "ok", &[]));
        exec.seed("c2", "m", RunOutcome::new("m").with_exit_code(3));

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
        // Cancel fires after 1st case; the 2nd case still executes
        // (cancel is checked BEFORE each case, not during), then the
        // loop sees the flag and stops. So we expect fewer than 5.
        assert!(
            report.total() < 5,
            "cancel_flag should stop the run early, got {} results",
            report.total()
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
            report.runs[0].passed,
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
            report.runs[0].passed,
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

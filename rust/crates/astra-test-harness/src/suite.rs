//! Suite orchestration with parallel execution, circuit breaker,
//! failure classification, and retry on rate-limit.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::Semaphore;

use crate::case::Case;
use crate::classify::{FailureClass, classify};
use crate::criteria::{Criterion, evaluate_deterministic_with_session, non_judger_all_pass};
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
}

impl<'a> SuiteRunner<'a> {
    /// Run every (case × model) pair with concurrency control and circuit breaker.
    pub async fn run_all(&self, cases: &[Case]) -> SuiteReport {
        let wall_start = std::time::Instant::now();
        let started_at = chrono::Utc::now().to_rfc3339();

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
            started_at: Some(started_at),
            ..Default::default()
        };

        if self.suite_cfg.parallel <= 1 {
            // Serial path: simpler, preserves ordering, supports circuit breaker inline.
            for (case, model, run_index) in work {
                if aborted.load(Ordering::Relaxed) {
                    eprintln!("[astra-test] circuit breaker tripped — aborting remaining cases");
                    break;
                }
                let mut report = self.run_one(case, &model).await;
                report.run_index = run_index;
                self.update_circuit_breaker(
                    &report,
                    &consecutive_infra,
                    &aborted,
                    &total_auth_failures,
                );
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
                    async move {
                        if aborted.load(Ordering::Relaxed) {
                            return None;
                        }
                        let _permit = sem.acquire().await.ok()?;
                        if aborted.load(Ordering::Relaxed) {
                            return None;
                        }
                        let mut report = self.run_one(case, &model).await;
                        report.run_index = run_index;
                        self.update_circuit_breaker(
                            &report,
                            &consecutive_infra,
                            &aborted,
                            &total_auth_failures,
                        );
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
            setup_cmd.arg("-c").arg(cmd);
            let status = setup_cmd.status().await;
            let failed = match &status {
                Ok(s) => !s.success(),
                Err(_) => true,
            };
            if failed {
                let exit = status.as_ref().ok().and_then(|s| s.code()).unwrap_or(-1);
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
                };
            }
        }

        let mut outcome = self.executor.execute(case, model).await;
        let mut step_results: Vec<StepResult> = Vec::new();

        // Multi-turn: execute follow-up steps using the same session.
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
                let step_passed = step_criteria_results.iter().all(|r| r.passed);

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

        // Run teardown command (always, even on failure).
        if let Some(ref cmd) = case.teardown_cmd {
            let mut teardown_cmd = tokio::process::Command::new("sh");
            if let Some(ref wd) = self.runner_cfg.working_dir {
                teardown_cmd.current_dir(wd);
            }
            teardown_cmd.arg("-c").arg(cmd);
            let _ = teardown_cmd.status().await;
        }

        // Retry on 429 if enabled.
        if self.suite_cfg.retry_on_429 && is_rate_limited(&outcome) {
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

        if !self.no_judger && non_judger_all_pass(&criteria, &det) {
            for (i, c) in criteria.iter().enumerate() {
                if let Criterion::Judger { .. } = c
                    && let Some(res) = evaluate_judger(self.judger, c, &outcome).await
                {
                    det[i] = res;
                }
            }
        } else if self.no_judger {
            for (i, c) in criteria.iter().enumerate() {
                if let Criterion::Judger { .. } = c {
                    det[i].passed = true;
                    det[i].detail = "judger skipped (--no-judger)".into();
                }
            }
        }

        let steps_passed = step_results.iter().all(|s| s.passed);
        let passed = det.iter().all(|c| c.passed) && steps_passed;

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
        };
        let cases = vec![case_with("c1", vec![Criterion::ExitCode { code: 0 }])];
        let report = runner.run_all(&cases).await;
        assert_eq!(
            report.runs[0].failure_class,
            Some(FailureClass::InfraTimeout)
        );
    }

    #[tokio::test]
    async fn judger_only_fires_when_deterministic_checks_pass() {
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
        assert_eq!(report.passed(), 1);
        assert_eq!(*judger.hits.lock().unwrap(), 1);
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
                circuit_breaker_threshold: 10, // high so consecutive breaker doesn't fire
                ..Default::default()
            },
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
}

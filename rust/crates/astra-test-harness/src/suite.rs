//! Suite orchestration.
//!
//! A `SuiteRunner` composes a [`CaseExecutor`] + [`Judger`] and runs
//! every (case, model) pair through the full pipeline:
//!
//! 1. Execute case (subprocess or fake)
//! 2. Evaluate deterministic criteria
//! 3. If every non-Judger criterion passed, run the Judger
//! 4. Optionally load the session journal
//! 5. Collect a `CaseRunReport`
//!
//! The split from `main.rs` is deliberate: this struct is the unit
//! that third-party tools (e.g. a dev-mode harness embedded in the
//! admin dashboard) would want to reuse, and it is what we test
//! against fakes in integration tests.

use crate::case::Case;
use crate::criteria::{Criterion, evaluate_deterministic_with_session, non_judger_all_pass};
use crate::digest::DigestCollector;
use crate::exec::CaseExecutor;
use crate::judger::{Judger, evaluate_judger};
use crate::report::{CaseRunReport, SuiteReport};
use crate::runner::{RunnerConfig, resolve_models};
use crate::session_capture::{SessionCapture, load_session};

/// What to do with session journals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionCaptureMode {
    /// Never load the journal — fastest, smallest report.
    Never,
    /// Load only when the case sets `debug_log: true`.
    OnDebugLog,
    /// Always load. Useful with `--verbose`.
    Always,
}

impl SessionCaptureMode {
    fn should_load(self, case: &Case) -> bool {
        match self {
            SessionCaptureMode::Never => false,
            SessionCaptureMode::OnDebugLog => case.debug_log,
            SessionCaptureMode::Always => true,
        }
    }
}

/// Hook for loading session captures. Default impl reads from disk;
/// tests can inject an in-memory loader.
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

/// Orchestrates the full pipeline. Construct with real impls in
/// `main.rs` or with fakes in integration tests.
pub struct SuiteRunner<'a> {
    pub executor: &'a dyn CaseExecutor,
    pub judger: &'a dyn Judger,
    pub session_loader: &'a dyn SessionLoader,
    /// Optional digest collector. When set, the runner shells out
    /// on FAIL to populate `CaseRunReport.digest`. `None` disables
    /// digest collection entirely — used in unit tests and when the
    /// caller doesn't want to pay the extra subprocess per FAIL.
    pub digest_collector: Option<&'a dyn DigestCollector>,
    pub runner_cfg: RunnerConfig,
    pub no_judger: bool,
    pub session_mode: SessionCaptureMode,
}

impl<'a> SuiteRunner<'a> {
    /// Run every (case × resolved models) combination and return
    /// the aggregated report. Cases whose models can't be resolved
    /// (no `models:` + no fallback) are skipped with a log line —
    /// they don't kill the whole suite.
    pub async fn run_all(&self, cases: &[Case]) -> SuiteReport {
        let mut suite = SuiteReport::default();
        for case in cases {
            let models = match resolve_models(case, &self.runner_cfg) {
                Ok(m) => m,
                Err(e) => {
                    // Unified `[astra-test]` prefix so all harness
                    // self-logging lines are greppable + visually
                    // distinct from astra's `[fork-cache]` /
                    // `[selector]` observability.
                    eprintln!("[astra-test] skip case {:?}: {e}", case.name);
                    continue;
                }
            };
            for model in models {
                suite.runs.push(self.run_one(case, &model).await);
            }
        }
        suite
    }

    async fn run_one(&self, case: &Case, model: &str) -> CaseRunReport {
        let outcome = self.executor.execute(case, model).await;

        // Load session first so session-dependent criteria can see it.
        // If session_mode doesn't request load, criteria that need it
        // SKIP-pass rather than fail — explicit opt-in, no surprises.
        let session = if self.session_mode.should_load(case) {
            outcome
                .session_id
                .as_deref()
                .and_then(|id| self.session_loader.load(id))
        } else {
            None
        };

        let mut det =
            evaluate_deterministic_with_session(&case.criteria, &outcome, session.as_ref());

        if !self.no_judger && non_judger_all_pass(&case.criteria, &det) {
            for (i, c) in case.criteria.iter().enumerate() {
                if let Criterion::Judger { .. } = c
                    && let Some(res) = evaluate_judger(self.judger, c, &outcome).await
                {
                    det[i] = res;
                }
            }
        } else if self.no_judger {
            for (i, c) in case.criteria.iter().enumerate() {
                if let Criterion::Judger { .. } = c {
                    det[i].passed = true;
                    det[i].detail = "judger skipped (--no-judger)".into();
                }
            }
        }

        let passed = det.iter().all(|c| c.passed);

        // Reproducer: a shell command a developer can paste to re-run
        // the case. Empty string for fake executor in tests.
        let reproducer = {
            let r = self.executor.reproducer(case, model);
            if r.is_empty() { None } else { Some(r) }
        };

        // Digest collection: only on FAIL + session_id present +
        // collector configured. Errors go into `digest_error`, not
        // the case's FAIL reason — collector failure shouldn't
        // mask the actual diagnostic.
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
            outcome,
            criteria: det,
            session,
            reproducer,
            digest,
            digest_error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::criteria::Criterion;
    use crate::exec::test_support::FakeExecutor;
    use crate::judger::JudgerScore;
    use crate::runner::RunOutcome;
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
    async fn run_all_runs_each_case_against_each_fallback_model() {
        let exec = FakeExecutor::new();
        exec.seed("c1", "a", outcome_ok("a", "t", &[]));
        exec.seed("c1", "b", outcome_ok("b", "t", &[]));
        exec.seed("c2", "a", outcome_ok("a", "t", &[]));
        exec.seed("c2", "b", outcome_ok("b", "t", &[]));

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
            no_judger: false,
            session_mode: SessionCaptureMode::Never,
        };

        let cases = vec![case_with("c1", vec![]), case_with("c2", vec![])];
        let report = runner.run_all(&cases).await;
        assert_eq!(report.total(), 4);
        assert_eq!(report.passed(), 4);
        assert_eq!(exec.calls.lock().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn judger_only_fires_when_deterministic_checks_pass() {
        let exec = FakeExecutor::new();
        exec.seed("c1", "m", outcome_ok("m", "hello", &["Read"]));
        // c2's deterministic check will FAIL because "Read" wasn't called.
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
        // Judger should fire for c1 (det passed) and NOT for c2 (det failed).
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

        // Empty fallback models and case.models = None → skipped.
        let cfg = RunnerConfig::new(PathBuf::from("astra"));
        let runner = SuiteRunner {
            executor: &exec,
            judger: &judger,
            session_loader: &loader,
            digest_collector: None,
            runner_cfg: cfg,
            no_judger: true,
            session_mode: SessionCaptureMode::Never,
        };
        let cases = vec![case_with("c1", vec![])];
        let report = runner.run_all(&cases).await;
        assert_eq!(report.total(), 0);
    }

    #[tokio::test]
    async fn session_capture_on_debug_log_uses_loader() {
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
        };
        let mut case = case_with("dbg", vec![]);
        case.debug_log = true;
        let report = runner.run_all(&[case]).await;
        let s = report.runs[0].session.as_ref().unwrap();
        assert_eq!(s.session_id, "sess-m");
    }

    #[tokio::test]
    async fn reproducer_threaded_from_executor_to_report() {
        let exec = FakeExecutor::new();
        exec.seed("c1", "m", outcome_ok("m", "hi", &[]));
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
        };
        let cases = vec![case_with("c1", vec![])];
        let report = runner.run_all(&cases).await;
        let repro = report.runs[0].reproducer.as_deref().unwrap();
        assert!(repro.contains("fake executor"));
        assert!(repro.contains("c1"));
    }

    // ── Digest auto-capture (Item 3) ──

    #[tokio::test]
    async fn digest_collected_on_fail_only_with_session_id() {
        use crate::digest::test_support::FakeDigestCollector;

        let exec = FakeExecutor::new();
        // Case c_fail will FAIL (criterion expects Read, outcome has none).
        exec.seed("c_fail", "m", outcome_ok("m", "hello", &[]));
        // Case c_pass will PASS; digest collector must NOT be called.
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

        // Collector was called exactly once, for the failing case.
        let calls = digest.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0], "sess-m");
        drop(calls);

        let fail_run = report.runs.iter().find(|r| !r.passed).unwrap();
        assert!(fail_run.digest.is_some());
        assert!(fail_run.digest_error.is_none());
        let pass_run = report.runs.iter().find(|r| r.passed).unwrap();
        assert!(pass_run.digest.is_none());
    }

    #[tokio::test]
    async fn digest_error_surfaces_without_masking_case_fail() {
        use crate::digest::test_support::FakeDigestCollector;

        let exec = FakeExecutor::new();
        exec.seed("c_fail", "m", outcome_ok("m", "hello", &[]));
        let judger = FixedJudger { score: 1.0 };
        let loader = NoopSessionLoader;

        // Collector fails — digest subprocess unhappy for whatever reason.
        let digest = FakeDigestCollector::new();
        digest.seed_err("sess-m", "session file missing");

        let cfg = RunnerConfig::new(PathBuf::from("astra")).with_fallback_models(vec!["m".into()]);
        let runner = SuiteRunner {
            executor: &exec,
            judger: &judger,
            session_loader: &loader,
            digest_collector: Some(&digest),
            runner_cfg: cfg,
            no_judger: true,
            session_mode: SessionCaptureMode::Never,
        };
        let cases = vec![case_with(
            "c_fail",
            vec![Criterion::ToolCalled {
                name: "Read".into(),
            }],
        )];
        let report = runner.run_all(&cases).await;
        let run = &report.runs[0];
        // Case still FAILs (digest failure doesn't mask the real signal).
        assert!(!run.passed);
        assert!(run.digest.is_none());
        assert_eq!(run.digest_error.as_deref(), Some("session file missing"));
    }

    #[tokio::test]
    async fn digest_skipped_entirely_when_collector_is_none() {
        let exec = FakeExecutor::new();
        exec.seed("c_fail", "m", outcome_ok("m", "hello", &[]));
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
        };
        let cases = vec![case_with(
            "c_fail",
            vec![Criterion::ToolCalled {
                name: "Read".into(),
            }],
        )];
        let report = runner.run_all(&cases).await;
        assert!(!report.runs[0].passed);
        assert!(report.runs[0].digest.is_none());
        assert!(report.runs[0].digest_error.is_none());
    }
}

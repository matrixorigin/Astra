//! `astra-test` — CLI entrypoint. Parse args, run preflight, hand off
//! to [`astra_test_harness::suite::SuiteRunner`].

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use astra_test_harness::case::Case;
use astra_test_harness::digest::AstraCliDigestCollector;
use astra_test_harness::exec::AstraCliExecutor;
use astra_test_harness::judger::{
    AstraCliJudger, ExternalCmdJudger, Judger, JudgerConfig, QuorumAgg, QuorumJudger,
    warn_if_same_family,
};
use astra_test_harness::preflight::run_preflight;
use astra_test_harness::report::{Format, render};
use astra_test_harness::runner::{RunnerConfig, resolve_models};
use astra_test_harness::suite::{DiskSessionLoader, SessionCaptureMode, SuiteConfig, SuiteRunner};

#[derive(Debug, Parser)]
#[command(
    name = "astra-test",
    about = "Declarative CLI test harness for astra: cases × models × agent judger."
)]
struct Args {
    /// Directory containing case YAML files.
    /// Optional when --live-dashboard is used (auto-detected).
    #[arg(long, value_name = "DIR")]
    suite: Option<PathBuf>,

    /// Comma-separated fallback model list.
    #[arg(long, value_name = "CSV", default_value = "")]
    models: String,

    /// Path to the astra binary.
    #[arg(long, value_name = "PATH")]
    astra_bin: Option<PathBuf>,

    /// Working directory for each subprocess.
    #[arg(long, value_name = "DIR")]
    working_dir: Option<PathBuf>,

    /// Filter cases by name (glob pattern, e.g. "fork_*" or "hello*").
    #[arg(long, value_name = "PATTERN")]
    filter: Option<String>,

    /// Override all case-level `models:` fields with this model.
    /// Forces every case to run against exactly this one model.
    #[arg(long, value_name = "MODEL")]
    force_model: Option<String>,

    /// Parallel execution concurrency. Default 1 (serial).
    #[arg(long, default_value_t = 1)]
    parallel: usize,

    /// Run each (case, model) pair N times to detect flaky behavior.
    /// Report shows pass rate per case.
    #[arg(long, default_value_t = 1)]
    runs: u32,

    /// Circuit breaker: abort after N consecutive infra failures.
    #[arg(long, default_value_t = 3)]
    circuit_breaker: usize,

    /// Model for judger scoring.
    #[arg(long, value_name = "MODEL", default_value = "claude-sonnet-4-6")]
    judger_model: String,

    /// Judger timeout in seconds.
    #[arg(long, default_value_t = 120)]
    judger_timeout: u64,

    /// Run the judger N times and aggregate.
    #[arg(long, value_name = "N", default_value_t = 1)]
    judger_n: u32,

    /// Aggregation for --judger-n.
    #[arg(long, value_name = "AGG", default_value = "median")]
    judger_agg: String,

    /// Output format: text or json.
    #[arg(long, default_value = "text")]
    format: String,

    /// Print full stderr/text on every case.
    #[arg(long)]
    verbose: bool,

    /// Skip the LLM judger step entirely.
    #[arg(long)]
    no_judger: bool,

    /// Force-load session journals for every case.
    #[arg(long)]
    capture_session: bool,

    /// Disable on-FAIL journal digest.
    #[arg(long)]
    no_digest_on_fail: bool,

    /// Timeout for digest subprocess.
    #[arg(long, default_value_t = 15)]
    digest_timeout: u64,

    /// Skip pre-flight checks.
    #[arg(long)]
    skip_preflight: bool,

    /// Retry rate-limited (429) cases once after backoff.
    #[arg(long)]
    retry_on_429: bool,

    /// External judger command. Receives {question, outcome} JSON on
    /// stdin, returns {score, rationale} JSON on stdout.
    /// Overrides --judger-model when set.
    #[arg(long, value_name = "CMD")]
    judger_cmd: Option<String>,

    /// External executor command. Receives case JSON on stdin,
    /// returns RunOutcome JSON on stdout. Overrides built-in astra executor.
    #[arg(long, value_name = "CMD")]
    executor_cmd: Option<String>,

    /// Directory to persist per-case artifacts (stdout, stderr, report, digest).
    /// Layout: <dir>/<case>/<model>/<run_index>/
    #[arg(long, value_name = "DIR")]
    artifacts_dir: Option<PathBuf>,

    /// Profile name for astra credentials. When preflight auto-registers
    /// it writes to "harness-auto" by default; this flag lets you pick
    /// a different profile to avoid clobbering active user credentials.
    #[arg(long, value_name = "PROFILE")]
    profile: Option<String>,

    /// Run an LLM-powered post-run summary analyzing cross-model
    /// performance, capability gaps, and efficiency patterns.
    /// Uses the judger model by default; override with --summarize-model.
    #[arg(long)]
    summarize: bool,

    /// Model to use for the post-run summary. Implies --summarize.
    #[arg(long, value_name = "MODEL")]
    summarize_model: Option<String>,

    /// Timeout for the summarizer LLM call in seconds.
    #[arg(long, default_value_t = 180)]
    summarize_timeout: u64,

    /// Save the full JSON report to a file for post-run introspection
    /// by AI agents or dashboards.
    #[arg(long, value_name = "PATH")]
    report_file: Option<PathBuf>,

    /// Start a live dashboard server for real-time test visualization.
    /// Opens http://localhost:PORT (default 9100) in your browser.
    #[arg(long, value_name = "PORT", default_missing_value = "9100", num_args = 0..=1)]
    live_dashboard: Option<u16>,
}

fn resolve_suite_dir(explicit: &std::path::Path, astra_bin: &std::path::Path) -> PathBuf {
    if !explicit.as_os_str().is_empty() && explicit.is_dir() {
        return explicit.to_path_buf();
    }
    // Auto-detect: look for the cases/ directory relative to the astra binary
    // or the current working directory.
    for base in [
        astra_bin
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .map(|p| p.to_path_buf()),
        std::env::current_dir().ok(),
    ]
    .into_iter()
    .flatten()
    {
        let candidate = base.join("rust/crates/astra-test-harness/cases");
        if candidate.is_dir() {
            return candidate;
        }
    }
    PathBuf::from("rust/crates/astra-test-harness/cases")
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn resolve_astra_bin(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        if !p.is_file() {
            anyhow::bail!(
                "--astra-bin {:?} does not exist or is not a file",
                p.display()
            );
        }
        return Ok(p);
    }
    if let Ok(env_path) = std::env::var("ASTRA_BIN")
        && !env_path.trim().is_empty()
    {
        let p = PathBuf::from(env_path);
        if p.is_file() {
            eprintln!(
                "[astra-test] using astra bin from ASTRA_BIN: {}",
                p.display()
            );
            return Ok(p);
        }
    }
    if let Some(found) = find_on_path("astra") {
        eprintln!(
            "[astra-test] using astra bin from PATH: {}",
            found.display()
        );
        return Ok(found);
    }
    let cwd = std::env::current_dir().map_err(|e| anyhow::anyhow!("cwd: {e}"))?;
    for ancestor in cwd.ancestors() {
        let candidate = ancestor.join("rust/target/release/astra");
        if candidate.is_file() {
            eprintln!(
                "[astra-test] using astra bin from workspace: {}",
                candidate.display()
            );
            return Ok(candidate);
        }
    }
    anyhow::bail!(
        "could not locate the astra binary. Tried: --astra-bin flag, \
         ASTRA_BIN env var, `astra` on PATH, and rust/target/release/astra \
         relative to any ancestor of {}",
        cwd.display()
    )
}

fn matches_filter(name: &str, pattern: &str) -> bool {
    // Simple glob: * matches any sequence, ? matches one char.
    let regex_str = format!(
        "^{}$",
        regex::escape(pattern)
            .replace(r"\*", ".*")
            .replace(r"\?", ".")
    );
    regex::Regex::new(&regex_str)
        .map(|re| re.is_match(name))
        .unwrap_or(false)
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    let args = Args::parse();
    let astra_bin = resolve_astra_bin(args.astra_bin.clone())?;

    // ── Dashboard-only mode ─────────────────────────────────────────
    // When --live-dashboard is passed without a full CLI run config,
    // start the dashboard server and let the user configure everything
    // from the web UI.
    if let Some(port) = args.live_dashboard {
        let suite_dir = resolve_suite_dir(args.suite.as_deref().unwrap_or("".as_ref()), &astra_bin);
        let fallback_models: Vec<String> = args
            .models
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let dashboard_config = astra_test_harness::dashboard::DashboardConfig {
            suite_dir,
            astra_bin,
            available_models: fallback_models,
            judger_model: args.judger_model.clone(),
        };
        let server = astra_test_harness::dashboard::DashboardServer::new(dashboard_config);
        eprintln!("[astra-test] starting dashboard at http://localhost:{port}");
        eprintln!("[astra-test] open in your browser — configure and run from there.");
        eprintln!("[astra-test] press Ctrl-C to stop.");
        server.start(port).await?;
        return Ok(());
    }

    // ── Normal CLI mode ─────────────────────────────────────────────
    let suite_path = args.suite.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "--suite is required in CLI mode. Use --live-dashboard for the web console."
        )
    })?;
    let mut cases = Case::load_dir(suite_path)
        .with_context(|| format!("load cases from {}", suite_path.display()))?;
    if cases.is_empty() {
        anyhow::bail!("no cases found in {}", suite_path.display());
    }

    // Apply --filter
    if let Some(ref pattern) = args.filter {
        cases.retain(|c| matches_filter(&c.name, pattern));
        if cases.is_empty() {
            anyhow::bail!("no cases match filter {:?}", pattern);
        }
        eprintln!("[astra-test] filter matched {} case(s)", cases.len());
    }

    // Apply --force-model: override case-level models
    if let Some(ref forced) = args.force_model {
        for case in &mut cases {
            case.models = Some(vec![forced.clone()]);
        }
    }

    let fallback_models: Vec<String> = args
        .models
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    // Pre-flight checks — verify all unique models in the matrix, not just the first.
    if !args.skip_preflight {
        let preflight_models: Vec<String> = if let Some(ref forced) = args.force_model {
            vec![forced.clone()]
        } else {
            let mut all_models: Vec<String> = Vec::new();
            for case in &cases {
                if let Some(ref ms) = case.models {
                    all_models.extend(ms.iter().cloned());
                }
            }
            if all_models.is_empty() {
                all_models = fallback_models.clone();
            }
            all_models.sort();
            all_models.dedup();
            all_models
        };
        match run_preflight(&astra_bin, &preflight_models).await {
            Ok(_) => {}
            Err(e) => {
                anyhow::bail!("pre-flight check failed: {e}\n  (use --skip-preflight to bypass)");
            }
        }
    }

    let mut runner_cfg =
        RunnerConfig::new(astra_bin.clone()).with_fallback_models(fallback_models.clone());
    runner_cfg.working_dir = args.working_dir.clone();
    runner_cfg.profile = args.profile.clone();

    let judger_cfg = JudgerConfig {
        astra_bin: astra_bin.clone(),
        default_model: args.judger_model.clone(),
        timeout_seconds: args.judger_timeout,
    };

    let executor: Box<dyn astra_test_harness::exec::CaseExecutor> =
        if let Some(ref cmd) = args.executor_cmd {
            Box::new(astra_test_harness::exec::ExternalCmdExecutor::new(
                cmd.clone(),
                180,
            ))
        } else {
            Box::new(AstraCliExecutor::new(runner_cfg.clone()))
        };
    let cli_judger = AstraCliJudger::new(judger_cfg);
    let quorum_agg: QuorumAgg = args
        .judger_agg
        .parse()
        .map_err(|e: String| anyhow::anyhow!(e))?;
    let judger: Box<dyn Judger> = if let Some(ref cmd) = args.judger_cmd {
        Box::new(ExternalCmdJudger::new(cmd.clone(), args.judger_timeout))
    } else if args.judger_n > 1 {
        Box::new(QuorumJudger::new(cli_judger, args.judger_n, quorum_agg))
    } else {
        Box::new(cli_judger)
    };

    if !args.no_judger {
        let mut tested: Vec<String> = Vec::new();
        for c in &cases {
            if let Ok(ms) = resolve_models(c, &runner_cfg) {
                tested.extend(ms);
            }
        }
        tested.sort();
        tested.dedup();
        warn_if_same_family(&args.judger_model, &tested);
    }

    let session_loader = DiskSessionLoader;
    let session_mode = if args.capture_session || args.verbose {
        SessionCaptureMode::Always
    } else {
        SessionCaptureMode::OnDebugLog
    };

    let digest = AstraCliDigestCollector::new(astra_bin.clone()).with_timeout(args.digest_timeout);
    let digest_collector: Option<&dyn astra_test_harness::digest::DigestCollector> =
        if args.no_digest_on_fail {
            None
        } else {
            Some(&digest)
        };

    let suite_cfg = SuiteConfig {
        parallel: args.parallel.max(1),
        circuit_breaker_threshold: args.circuit_breaker,
        retry_on_429: args.retry_on_429,
        runs: args.runs.max(1),
    };

    let runner = SuiteRunner {
        executor: executor.as_ref(),
        judger: judger.as_ref(),
        session_loader: &session_loader,
        digest_collector,
        runner_cfg,
        no_judger: args.no_judger,
        session_mode,
        suite_cfg,
        dashboard_tx: None,
    };

    let suite = runner.run_all(&cases).await;

    // Persist artifacts if requested.
    if let Some(ref dir) = args.artifacts_dir {
        for run in &suite.runs {
            if let Err(e) = astra_test_harness::artifacts::persist_artifacts(dir, run) {
                eprintln!(
                    "[astra-test] WARNING: failed to write artifacts for {} × {}: {e}",
                    run.case_name, run.model
                );
            }
        }
    }

    let fmt: Format = args
        .format
        .parse()
        .map_err(|e: String| anyhow::anyhow!(e))?;
    println!("{}", render(&suite, fmt, args.verbose));

    // Save JSON report for post-run introspection.
    if let Some(ref path) = args.report_file {
        let json = astra_test_harness::report::render(
            &suite,
            astra_test_harness::report::Format::Json,
            false,
        );
        if let Err(e) = std::fs::write(path, &json) {
            eprintln!(
                "[astra-test] WARNING: failed to write report to {}: {e}",
                path.display()
            );
        } else {
            eprintln!(
                "[astra-test] report saved to {} ({} bytes)",
                path.display(),
                json.len()
            );
        }
    }

    // Optional LLM summary.
    let want_summary = args.summarize || args.summarize_model.is_some();
    if want_summary {
        let summary_model = args
            .summarize_model
            .as_deref()
            .unwrap_or(&args.judger_model);
        eprintln!("[astra-test] generating LLM summary (model={summary_model})...");
        match astra_test_harness::summarizer::summarize(
            &astra_bin,
            summary_model,
            &suite,
            args.summarize_timeout,
        )
        .await
        {
            Ok(text) => {
                println!("\n=== LLM summary ===\n{text}\n");
            }
            Err(e) => {
                eprintln!("[astra-test] WARNING: summarizer failed: {e}");
            }
        }
    }

    if suite.failed() > 0 {
        std::process::exit(1);
    }
    Ok(())
}

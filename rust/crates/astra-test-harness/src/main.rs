//! `astra-test` — CLI entrypoint. Parse args, build deps, hand off
//! to [`astra_test_harness::suite::SuiteRunner`]. Keep this file
//! small — all orchestration logic lives in the library so it can
//! be unit-tested with fakes.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use astra_test_harness::case::Case;
use astra_test_harness::exec::AstraCliExecutor;
use astra_test_harness::judger::{
    warn_if_same_family, AstraCliJudger, Judger, JudgerConfig, QuorumAgg, QuorumJudger,
};
use astra_test_harness::report::{render, Format};
use astra_test_harness::runner::{resolve_models, RunnerConfig};
use astra_test_harness::suite::{DiskSessionLoader, SessionCaptureMode, SuiteRunner};

#[derive(Debug, Parser)]
#[command(
    name = "astra-test",
    about = "Declarative CLI test harness for astra: cases × models × agent judger."
)]
struct Args {
    /// Directory containing case YAML files. Non-YAML files are skipped.
    #[arg(long, value_name = "DIR")]
    suite: PathBuf,

    /// Comma-separated fallback model list. Used when a case has no
    /// `models:` field. If both are missing the case is skipped.
    #[arg(long, value_name = "CSV", default_value = "")]
    models: String,

    /// Path to the astra release binary. Subprocess invocation target.
    #[arg(
        long,
        value_name = "PATH",
        default_value = "./rust/target/release/astra"
    )]
    astra_bin: PathBuf,

    /// Working directory for each subprocess. Defaults to CWD.
    #[arg(long, value_name = "DIR")]
    working_dir: Option<PathBuf>,

    /// Model to use when running a judger criterion. Per-case
    /// override still wins.
    #[arg(long, value_name = "MODEL", default_value = "claude-sonnet-4-6")]
    judger_model: String,

    /// Judger timeout in seconds. Keeps the suite from hanging on a
    /// pathological judger call.
    #[arg(long, default_value_t = 60)]
    judger_timeout: u64,

    /// Run the judger N times per criterion and aggregate the scores.
    /// N=1 (default) matches pre-quorum behavior. N>=3 is recommended
    /// when you've observed flaky pass/fail from a single call.
    #[arg(long, value_name = "N", default_value_t = 1)]
    judger_n: u32,

    /// Aggregation for `--judger-n`: `median` (default, robust to one
    /// outlier), `mean`, `min` (paranoid — one LOW vote kills), `max`.
    #[arg(long, value_name = "AGG", default_value = "median")]
    judger_agg: String,

    /// Output format: `text` (default, human) or `json` (CI).
    #[arg(long, default_value = "text")]
    format: String,

    /// Print full stderr/text on every case and always load session
    /// journals.
    #[arg(long)]
    verbose: bool,

    /// Skip the LLM judger step entirely. Useful for offline CI —
    /// deterministic checks still run.
    #[arg(long)]
    no_judger: bool,

    /// Force-load session journals for every case (normally only
    /// loaded when `debug_log: true`). Enables session-dependent
    /// criteria across the whole suite.
    #[arg(long)]
    capture_session: bool,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    let args = Args::parse();

    let cases = Case::load_dir(&args.suite)
        .with_context(|| format!("load cases from {}", args.suite.display()))?;
    if cases.is_empty() {
        anyhow::bail!("no cases found in {}", args.suite.display());
    }

    let fallback_models: Vec<String> = args
        .models
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let mut runner_cfg =
        RunnerConfig::new(args.astra_bin.clone()).with_fallback_models(fallback_models);
    runner_cfg.working_dir = args.working_dir.clone();

    let judger_cfg = JudgerConfig {
        astra_bin: args.astra_bin.clone(),
        default_model: args.judger_model.clone(),
        timeout_seconds: args.judger_timeout,
    };

    let executor = AstraCliExecutor::new(runner_cfg.clone());
    let cli_judger = AstraCliJudger::new(judger_cfg);
    let quorum_agg: QuorumAgg = args
        .judger_agg
        .parse()
        .map_err(|e: String| anyhow::anyhow!(e))?;
    // Build the active judger — either the raw CLI judger (N=1) or a
    // quorum decorator. Doing this with a boxed trait object keeps the
    // SuiteRunner signature unchanged.
    let judger: Box<dyn Judger> = if args.judger_n > 1 {
        Box::new(QuorumJudger::new(cli_judger, args.judger_n, quorum_agg))
    } else {
        Box::new(cli_judger)
    };

    // Same-family warning: judger scoring its own family is a known
    // source of inflated scores. Resolve the full set of tested models
    // from the cases + fallback so the advisor sees every collision.
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

    let runner = SuiteRunner {
        executor: &executor,
        judger: judger.as_ref(),
        session_loader: &session_loader,
        runner_cfg,
        no_judger: args.no_judger,
        session_mode,
    };

    let suite = runner.run_all(&cases).await;

    let fmt: Format = args
        .format
        .parse()
        .map_err(|e: String| anyhow::anyhow!(e))?;
    println!("{}", render(&suite, fmt, args.verbose));

    // Non-zero exit when any case failed — so CI can gate on this.
    if suite.failed() > 0 {
        std::process::exit(1);
    }
    Ok(())
}

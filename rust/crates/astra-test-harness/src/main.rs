//! `astra-test` — CLI entrypoint. Parse args, build deps, hand off
//! to [`astra_test_harness::suite::SuiteRunner`]. Keep this file
//! small — all orchestration logic lives in the library so it can
//! be unit-tested with fakes.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use astra_test_harness::case::Case;
use astra_test_harness::digest::AstraCliDigestCollector;
use astra_test_harness::exec::AstraCliExecutor;
use astra_test_harness::judger::{
    AstraCliJudger, Judger, JudgerConfig, QuorumAgg, QuorumJudger, warn_if_same_family,
};
use astra_test_harness::report::{Format, render};
use astra_test_harness::runner::{RunnerConfig, resolve_models};
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

    /// Path to the astra binary. Subprocess invocation target. When
    /// unset, the harness searches (in order):
    /// 1. `ASTRA_BIN` env var
    /// 2. `astra` on `$PATH`
    /// 3. `rust/target/release/astra` relative to the nearest
    ///    workspace root above CWD.
    ///
    /// Resolution is performed only once at startup; the chosen path
    /// is logged to stderr for transparency.
    #[arg(long, value_name = "PATH")]
    astra_bin: Option<PathBuf>,

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

    /// Disable the on-FAIL journal digest auto-capture. By default the
    /// harness shells out to `astra journal digest` for each failed
    /// case and embeds the aggregate JSON in the report. Useful when
    /// the digest subprocess is slow or the digest binary is missing.
    #[arg(long)]
    no_digest_on_fail: bool,

    /// Timeout (seconds) for the `astra journal digest` subprocess
    /// the harness spawns on FAIL. Default 15s is fine on warmed-up
    /// dev machines; cold CI containers running debug builds may
    /// need 30-60.
    #[arg(long, default_value_t = 15)]
    digest_timeout: u64,
}

/// Unix: scan `$PATH` for an executable file with the given name.
/// Windows callers would need to also try common extensions (`.exe`,
/// `.bat`) — astra is Unix-only today so we don't.
///
/// Inlined to avoid pulling the `which` crate in for one callsite;
/// this is the entirety of what we'd use there.
fn find_on_path(name: &str) -> Option<PathBuf> {
    // `OS_PATH_SEPARATOR` would be `;` on Windows — mirror that if
    // anyone ports the harness; on Unix it's `:`.
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

/// Resolve the astra binary path. See the `--astra-bin` doc comment
/// for the resolution order. Returns an error only when no candidate
/// is usable, with a message that explains what was tried.
fn resolve_astra_bin(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        // Explicit flag wins — but still verify it's an executable
        // file so the first subprocess spawn isn't the one to surface
        // the typo.
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
    // Last resort: walk up from CWD looking for a Cargo workspace root
    // with `rust/target/release/astra`. Covers the common dev case of
    // running the harness from any subdirectory inside the repo.
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

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    let args = Args::parse();

    let astra_bin = resolve_astra_bin(args.astra_bin.clone())?;

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

    let mut runner_cfg = RunnerConfig::new(astra_bin.clone()).with_fallback_models(fallback_models);
    runner_cfg.working_dir = args.working_dir.clone();

    let judger_cfg = JudgerConfig {
        astra_bin: astra_bin.clone(),
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

    // Digest collector: always construct, conditionally wire. Keeping
    // the value out of scope when disabled lets the SuiteRunner see
    // `None` and skip the subprocess per-FAIL.
    let digest = AstraCliDigestCollector::new(astra_bin.clone()).with_timeout(args.digest_timeout);
    let digest_collector: Option<&dyn astra_test_harness::digest::DigestCollector> =
        if args.no_digest_on_fail {
            None
        } else {
            Some(&digest)
        };

    let runner = SuiteRunner {
        executor: &executor,
        judger: judger.as_ref(),
        session_loader: &session_loader,
        digest_collector,
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

//! `astra-test` — CLI entrypoint. Parse args, run preflight, hand off
//! to [`astra_test_harness::suite::SuiteRunner`].

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use astra_test_harness::case::{Case, matches_filter};
use astra_test_harness::criteria::requires_session_capture;
use astra_test_harness::digest::AstraCliDigestCollector;
use astra_test_harness::exec::AstraCliExecutor;
use astra_test_harness::judger::{
    AstraCliJudger, ExternalCmdJudger, Judger, JudgerConfig, QuorumAgg, QuorumJudger,
    warn_if_same_family,
};
use astra_test_harness::preflight::run_preflight;
use astra_test_harness::report::{Format, render};
use astra_test_harness::runner::{RunnerConfig, resolve_models};
use astra_test_harness::suite::{
    ScopedDiskSessionLoader, SessionCaptureMode, SuiteConfig, SuiteRunner,
};

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

    /// Save the structured evaluation report (capability scores, runtime
    /// health, efficiency metrics) to a JSON file.
    #[arg(long, value_name = "PATH")]
    eval_file: Option<PathBuf>,

    /// Start a live dashboard server for real-time test visualization.
    /// Opens http://localhost:PORT (default 9100) in your browser.
    #[arg(long, value_name = "PORT", default_missing_value = "9100", num_args = 0..=1)]
    live_dashboard: Option<u16>,
}

fn resolve_suite_dir(explicit: &std::path::Path, astra_bin: &std::path::Path) -> PathBuf {
    if !explicit.as_os_str().is_empty() && explicit.is_dir() {
        return std::fs::canonicalize(explicit).unwrap_or_else(|_| explicit.to_path_buf());
    }
    let astra_abs = std::fs::canonicalize(astra_bin).unwrap_or_else(|_| astra_bin.to_path_buf());
    // Walk up from the astra binary to find the repo root containing the cases dir.
    // Binary is at <repo>/target/{debug,release}/astra, so check each ancestor.
    let mut dir = astra_abs.as_path();
    while let Some(parent) = dir.parent() {
        let candidate = parent.join("crates/astra-test-harness/cases");
        if candidate.is_dir() {
            return candidate;
        }
        // Also check if parent IS the rust/ dir (cwd might be there).
        let candidate2 = parent.join("crates/astra-test-harness/cases");
        if candidate2.is_dir() {
            return candidate2;
        }
        dir = parent;
    }
    // Fallback: try relative to cwd.
    if let Ok(cwd) = std::env::current_dir() {
        for suffix in [
            "crates/astra-test-harness/cases",
            "crates/astra-test-harness/cases",
        ] {
            let candidate = cwd.join(suffix);
            if candidate.is_dir() {
                return candidate;
            }
        }
    }
    PathBuf::from("crates/astra-test-harness/cases")
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

fn newer_workspace_binary(a: PathBuf, b: PathBuf) -> PathBuf {
    let a_mtime = std::fs::metadata(&a).and_then(|m| m.modified()).ok();
    let b_mtime = std::fs::metadata(&b).and_then(|m| m.modified()).ok();
    match (a_mtime, b_mtime) {
        (Some(a_time), Some(b_time)) if b_time > a_time => b,
        _ => a,
    }
}

fn resolve_workspace_astra_bin(ancestor: &std::path::Path) -> Option<PathBuf> {
    let debug = ancestor.join("target/debug/astra");
    let release = ancestor.join("target/release/astra");
    match (debug.is_file(), release.is_file()) {
        (true, true) => Some(newer_workspace_binary(debug, release)),
        (true, false) => Some(debug),
        (false, true) => Some(release),
        (false, false) => None,
    }
}

struct RunnerProfileIdentity {
    profile_name: String,
    local_owner_scope: astra_services::OwnerScope,
    artifact_owner_scopes: Vec<astra_services::OwnerScope>,
}

fn resolve_runner_profile_owner(requested_profile: Option<&str>) -> Result<RunnerProfileIdentity> {
    let credentials = astra_credentials::CredentialStore::new()
        .load()
        .context("load CLI credentials for harness session capture")?;
    let profile_name = astra_credentials::CredentialStore::resolve_profile_name(
        requested_profile,
        credentials.current_profile.as_deref(),
    );
    let profile = credentials.profiles.get(&profile_name).ok_or_else(|| {
        anyhow::anyhow!(
            "credential profile `{profile_name}` is unavailable after preflight; \
             authenticate that profile before running the harness"
        )
    })?;
    let account_id = profile.account_id.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "credential profile `{profile_name}` has no server-issued account_id; \
             log in again before running owner-scoped harness verification"
        )
    })?;
    let local_owner_id = astra_credentials::local_profile_owner_id(&profile_name, Some(account_id))
        .map_err(anyhow::Error::msg)?;
    let local_owner_scope =
        astra_services::OwnerScope::user(local_owner_id).map_err(anyhow::Error::msg)?;
    let server_owner_scope =
        astra_services::OwnerScope::user(account_id).map_err(anyhow::Error::msg)?;
    let mut artifact_owner_scopes = vec![local_owner_scope.clone()];
    if server_owner_scope != local_owner_scope {
        artifact_owner_scopes.push(server_owner_scope);
    }
    Ok(RunnerProfileIdentity {
        profile_name,
        local_owner_scope,
        artifact_owner_scopes,
    })
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
        if let Some(candidate) = resolve_workspace_astra_bin(ancestor) {
            eprintln!(
                "[astra-test] using astra bin from workspace: {}",
                candidate.display()
            );
            return Ok(candidate);
        }
    }
    anyhow::bail!(
        "could not locate the astra binary. Tried: --astra-bin flag, \
         ASTRA_BIN env var, `astra` on PATH, and target/{{debug,release}}/astra \
         relative to any ancestor of {}",
        cwd.display()
    )
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
    let mut runner_profile = args.profile.clone();

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
        match run_preflight(&astra_bin, &preflight_models, args.profile.as_deref()).await {
            Ok(effective_profile) => {
                if effective_profile.is_some() {
                    runner_profile = effective_profile;
                }
            }
            Err(e) => {
                anyhow::bail!("pre-flight check failed: {e}\n  (use --skip-preflight to bypass)");
            }
        }
    }

    let runner_identity = resolve_runner_profile_owner(runner_profile.as_deref())?;
    astra_services::configure_local_owner_scope(runner_identity.local_owner_scope.clone());
    runner_profile = Some(runner_identity.profile_name);

    let mut runner_cfg =
        RunnerConfig::new(astra_bin.clone()).with_fallback_models(fallback_models.clone());
    runner_cfg.working_dir = args.working_dir.clone();
    runner_cfg.profile = runner_profile.clone();
    runner_cfg.artifact_owner_scopes = runner_identity.artifact_owner_scopes.clone();

    let judger_cfg = JudgerConfig {
        astra_bin: astra_bin.clone(),
        profile: runner_profile.clone(),
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

    let session_loader =
        ScopedDiskSessionLoader::new(runner_identity.artifact_owner_scopes.clone());
    let needs_session_capture = cases
        .iter()
        .any(|case| requires_session_capture(&case.criteria));
    let session_mode = if args.capture_session || args.verbose || needs_session_capture {
        SessionCaptureMode::Always
    } else {
        SessionCaptureMode::OnDebugLog
    };

    let digest = AstraCliDigestCollector::new(astra_bin.clone())
        .with_timeout(args.digest_timeout)
        .with_profile(runner_profile.clone());
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
        run_id: String::from("cli"),
        cancel_flag: None,
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

    // Save structured evaluation report.
    if let Some(ref path) = args.eval_file {
        let eval = astra_test_harness::eval::evaluate(&suite);
        let json = serde_json::to_string_pretty(&eval).unwrap_or_default();
        if let Err(e) = std::fs::write(path, &json) {
            eprintln!(
                "[astra-test] WARNING: failed to write eval to {}: {e}",
                path.display()
            );
        } else {
            eprintln!(
                "[astra-test] eval saved to {} (overall={:.0})",
                path.display(),
                eval.overall_score
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
            runner_profile.as_deref(),
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

#[cfg(test)]
mod tests {
    use super::{resolve_runner_profile_owner, resolve_workspace_astra_bin};
    use std::fs;
    use std::time::Duration;

    #[test]
    fn workspace_bin_prefers_only_available_debug_binary() {
        let dir = tempfile::tempdir().unwrap();
        let debug = dir.path().join("target/debug");
        fs::create_dir_all(&debug).unwrap();
        let debug_bin = debug.join("astra");
        fs::write(&debug_bin, b"debug").unwrap();

        assert_eq!(resolve_workspace_astra_bin(dir.path()), Some(debug_bin));
    }

    #[test]
    fn workspace_bin_prefers_newer_profile_binary() {
        let dir = tempfile::tempdir().unwrap();
        let release = dir.path().join("target/release");
        let debug = dir.path().join("target/debug");
        fs::create_dir_all(&release).unwrap();
        fs::create_dir_all(&debug).unwrap();
        let release_bin = release.join("astra");
        let debug_bin = debug.join("astra");

        fs::write(&release_bin, b"release").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        fs::write(&debug_bin, b"debug").unwrap();
        assert_eq!(
            resolve_workspace_astra_bin(dir.path()),
            Some(debug_bin.clone())
        );

        std::thread::sleep(Duration::from_millis(20));
        fs::write(&release_bin, b"release-newer").unwrap();
        assert_eq!(resolve_workspace_astra_bin(dir.path()), Some(release_bin));
    }

    #[test]
    fn runner_profile_owner_uses_the_bound_account_namespace() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = astra_credentials::set_test_credentials_dir(dir.path().to_path_buf());
        astra_credentials::CredentialStore::new()
            .mutate(|credentials| {
                credentials.current_profile = Some("profile-a".to_string());
                credentials.profiles.insert(
                    "profile-a".to_string(),
                    astra_credentials::Profile {
                        account_id: Some("account-a".to_string()),
                        ..Default::default()
                    },
                );
            })
            .unwrap();

        let identity = resolve_runner_profile_owner(None).unwrap();
        assert_eq!(identity.profile_name, "profile-a");
        assert_eq!(
            identity.local_owner_scope.id(),
            astra_credentials::local_profile_owner_id("profile-a", Some("account-a")).unwrap()
        );
        assert_eq!(identity.artifact_owner_scopes.len(), 2);
        assert_eq!(identity.artifact_owner_scopes[1].id(), "account-a");
    }
}

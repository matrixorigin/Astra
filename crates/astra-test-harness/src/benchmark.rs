//! Comparable benchmark identities, aggregates, and baseline comparisons.
//!
//! The harness already records one authoritative row per `(case, model,
//! run_index)`.  This module turns those rows into a small, deterministic
//! comparison surface.  It deliberately keeps product correctness, evidence
//! completeness, and efficiency separate: an early failure must never look
//! like an efficiency win, and an incomplete journal must never look like a
//! smaller execution trace.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::case::Case;
use crate::report::{CaseRunReport, CaseRunStatus, SuiteReport};
use crate::runner::{RunOutcome, RunnerConfig, resolve_models};

pub const BENCHMARK_MANIFEST_SCHEMA: &str = "astra.benchmark.manifest.v1";
pub const BENCHMARK_AGGREGATE_SCHEMA: &str = "astra.benchmark.aggregate.v1";
pub const BENCHMARK_COMPARISON_SCHEMA: &str = "astra.benchmark.comparison.v1";

/// Identity of the executable that actually ran the benchmark.
///
/// A harness package version alone is not enough: a stale or independently
/// built `astra` binary can be selected with `--astra-bin`.  The CLI exposes a
/// side-effect-free `--build-info-json` probe, whose typed response is kept
/// here.  Probe failures remain explicit so a report cannot silently claim a
/// reproducible binary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BinaryIdentity {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_info: Option<BinaryBuildInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BinaryBuildInfo {
    pub schema: String,
    pub git_sha: String,
    pub git_dirty: bool,
    pub target: String,
    pub profile: String,
}

/// Effective harness settings that affect whether two runs are comparable.
/// Paths and command contents are represented as supplied or hashed values;
/// credentials and private environment values are never copied into a
/// manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchmarkConfig {
    pub profile: Option<String>,
    pub working_dir: Option<String>,
    pub runs: u32,
    pub parallel: usize,
    pub circuit_breaker_threshold: usize,
    pub retry_on_429: bool,
    pub session_capture_mode: String,
    pub no_judger: bool,
    pub judger_kind: JudgerKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judger_command_digest: Option<String>,
    pub judger_model: String,
    pub judger_n: u32,
    pub judger_agg: String,
    pub judger_timeout_seconds: u64,
    pub capability_probes: bool,
    pub prompt_variants: bool,
    pub executor_kind: ExecutorKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor_command_digest: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutorKind {
    Builtin,
    External,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JudgerKind {
    Disabled,
    Builtin,
    External,
}

/// Run identity persisted with every CLI-generated suite report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchmarkManifest {
    pub schema: String,
    pub harness_version: String,
    pub platform: String,
    pub generated_at: String,
    pub suite_digest: String,
    pub case_names: Vec<String>,
    pub effective_models: Vec<String>,
    pub config: BenchmarkConfig,
    pub tested_binary: BinaryIdentity,
    /// Identity of the process that executed cases.  Built-in runs use the
    /// tested Astra binary; external executors intentionally leave this
    /// absent because a command string is not a trustworthy build identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor_identity: Option<BinaryIdentity>,
}

impl BenchmarkManifest {
    pub fn new(
        cases: &[Case],
        effective_models: &[String],
        config: BenchmarkConfig,
        tested_binary: BinaryIdentity,
        executor_identity: Option<BinaryIdentity>,
        generated_at: impl Into<String>,
    ) -> Self {
        let mut case_names: Vec<String> = cases.iter().map(|case| case.name.clone()).collect();
        case_names.sort();
        case_names.dedup();

        let mut effective_models: Vec<String> = effective_models.to_vec();
        effective_models.sort();
        effective_models.dedup();

        Self {
            schema: BENCHMARK_MANIFEST_SCHEMA.to_string(),
            harness_version: env!("CARGO_PKG_VERSION").to_string(),
            platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
            generated_at: generated_at.into(),
            suite_digest: digest_cases(cases),
            case_names,
            effective_models,
            config,
            tested_binary,
            executor_identity,
        }
    }
}

/// Probe a selected Astra executable without starting a session or touching
/// credentials.  This is intentionally a separate process from the tested
/// run, and has a short timeout so a broken binary cannot hold the harness.
pub async fn probe_binary_identity(path: &Path) -> BinaryIdentity {
    let display_path = std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string();
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new(path)
            .arg("--build-info-json")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .output(),
    )
    .await;

    let output = match result {
        Ok(Ok(output)) => output,
        Ok(Err(_)) => {
            return BinaryIdentity {
                path: display_path,
                build_info: None,
                probe_error: Some("spawn_failed".to_string()),
            };
        }
        Err(_) => {
            return BinaryIdentity {
                path: display_path,
                build_info: None,
                probe_error: Some("timeout".to_string()),
            };
        }
    };

    if !output.status.success() {
        return BinaryIdentity {
            path: display_path,
            build_info: None,
            probe_error: Some(format!(
                "probe_exit_nonzero:{}",
                output
                    .status
                    .code()
                    .map_or_else(|| "signal".to_string(), |code| code.to_string())
            )),
        };
    }

    match parse_binary_build_info(&output.stdout) {
        Ok(build_info) => BinaryIdentity {
            path: display_path,
            build_info: Some(build_info),
            probe_error: None,
        },
        Err(error) => BinaryIdentity {
            path: display_path,
            build_info: None,
            probe_error: Some(error),
        },
    }
}

fn parse_binary_build_info(bytes: &[u8]) -> Result<BinaryBuildInfo, String> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("build-info output is not JSON: {error}"))?;
    let object = value
        .as_object()
        .ok_or_else(|| "build-info output must be a JSON object".to_string())?;
    let string_field = |name: &str| {
        object
            .get(name)
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(ToOwned::to_owned)
            .ok_or_else(|| format!("build-info field {name:?} must be a non-empty string"))
    };
    let schema = string_field("schema")?;
    if schema != "astra.build_info.v1" {
        return Err("unsupported_build_info_schema".to_string());
    }
    let git_dirty = object
        .get("git_dirty")
        .and_then(Value::as_bool)
        .ok_or_else(|| "build-info field \"git_dirty\" must be boolean".to_string())?;
    Ok(BinaryBuildInfo {
        schema,
        git_sha: string_field("git_sha")?,
        git_dirty,
        target: string_field("target")?,
        profile: string_field("profile")?,
    })
}

/// Resolve the directory actually inherited by subprocesses.  Recording the
/// effective absolute path avoids treating `None` and an implicit current
/// directory as different configurations, while relative `--working-dir`
/// values are compared after the same resolution the executor receives.
pub fn effective_working_dir(path: Option<&Path>) -> Option<String> {
    let current = std::env::current_dir().ok()?;
    let resolved = path.map_or_else(
        || current.clone(),
        |path| {
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                current.join(path)
            }
        },
    );
    Some(
        std::fs::canonicalize(&resolved)
            .unwrap_or(resolved)
            .display()
            .to_string(),
    )
}

/// Resolve the same effective model matrix used by `SuiteRunner`.  A model
/// resolution error still leaves the requested case-level/fallback IDs in the
/// manifest, so an unavailable run remains auditable rather than looking like
/// an empty matrix.
pub fn effective_model_ids(cases: &[Case], runner_cfg: &RunnerConfig) -> Vec<String> {
    let mut ids = BTreeSet::new();
    for case in cases {
        match resolve_models(case, runner_cfg) {
            Ok(models) => ids.extend(models),
            Err(_) => {
                if let Some(models) = &case.models {
                    ids.extend(models.iter().cloned());
                } else {
                    ids.extend(runner_cfg.fallback_models.iter().cloned());
                }
            }
        }
    }
    ids.into_iter().collect()
}

pub fn digest_command(command: Option<&str>) -> Option<String> {
    command.map(|command| digest_bytes(command.as_bytes()))
}

/// Aggregated counts and cost distributions for one complete suite.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkAggregate {
    pub schema: String,
    pub totals: BenchmarkCounts,
    pub all_cost: SampleStats,
    /// Passed rows with complete execution evidence, the only cost bucket
    /// eligible for efficiency scoring and historical efficiency claims.
    pub successful_cost: SampleStats,
    /// Passed rows whose execution attribution was explicitly incomplete.
    /// Kept separate for diagnostics; never used to claim efficiency.
    #[serde(default)]
    pub successful_incomplete_cost: SampleStats,
    pub wall_time_ms: u64,
    pub groups: Vec<BenchmarkGroupAggregate>,
}

impl Default for BenchmarkAggregate {
    fn default() -> Self {
        Self {
            schema: BENCHMARK_AGGREGATE_SCHEMA.to_string(),
            totals: BenchmarkCounts::default(),
            all_cost: SampleStats::default(),
            successful_cost: SampleStats::default(),
            successful_incomplete_cost: SampleStats::default(),
            wall_time_ms: 0,
            groups: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchmarkCounts {
    pub planned: usize,
    /// Rows with an actual terminal executor outcome. Cancelled and
    /// unavailable rows remain planned but are deliberately excluded here.
    pub executed: usize,
    pub passed: usize,
    pub failed: usize,
    pub cancelled: usize,
    pub unavailable: usize,
    /// Terminal rows whose capture, when present, was not marked incomplete.
    pub evidence_complete: usize,
    pub evidence_incomplete: usize,
}

impl BenchmarkCounts {
    pub fn pass_rate(&self) -> Option<f64> {
        // A cancelled row was planned but never produced evidence. It must
        // remain in the denominator so a circuit breaker cannot make a
        // partially run suite look fully green. Deliberately unavailable
        // rows are excluded because no executable model was selected.
        let scoreable = self.planned.saturating_sub(self.unavailable);
        (scoreable > 0).then(|| self.passed as f64 / scoreable as f64)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkGroupAggregate {
    pub case_name: String,
    pub model: String,
    pub counts: BenchmarkCounts,
    pub all_cost: SampleStats,
    pub successful_cost: SampleStats,
    #[serde(default)]
    pub successful_incomplete_cost: SampleStats,
    pub execution: ExecutionAggregate,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SampleStats {
    pub sample_count: usize,
    pub total_tokens: u64,
    pub p50_tokens: Option<u64>,
    pub p95_tokens: Option<u64>,
    pub total_duration_ms: u64,
    pub p50_duration_ms: Option<u64>,
    pub p95_duration_ms: Option<u64>,
    pub total_turns: u64,
    pub p50_turns: Option<u64>,
    pub p95_turns: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionAggregate {
    pub attribution_complete_runs: usize,
    pub attribution_incomplete_runs: usize,
    pub attribution_missing_runs: usize,
    pub total_tool_calls: u64,
    pub executed_tool_calls: u64,
    pub successful_tool_calls: u64,
    pub failed_tool_calls: u64,
    pub rejected_tool_calls: u64,
    pub reused_tool_calls: u64,
    pub suppressed_tool_calls: u64,
    pub deferred_tool_calls: u64,
    pub unknown_outcome_tool_calls: u64,
    pub unknown_disposition_tool_calls: u64,
    pub settlement_attempts: u64,
    pub successful_settlements: u64,
    pub rejected_settlements: u64,
    pub runtime_rejection_reasons: BTreeMap<String, u64>,
}

impl BenchmarkAggregate {
    pub fn from_report(report: &SuiteReport) -> Self {
        let mut grouped: BTreeMap<(String, String), Vec<&CaseRunReport>> = BTreeMap::new();
        for run in &report.runs {
            grouped
                .entry((run.case_name.clone(), run.model.clone()))
                .or_default()
                .push(run);
        }

        let groups: Vec<BenchmarkGroupAggregate> = grouped
            .into_iter()
            .map(|((case_name, model), runs)| {
                let counts = aggregate_counts(&runs);
                let all_runs: Vec<&CaseRunReport> = runs
                    .iter()
                    .copied()
                    .filter(|run| run.is_evidence())
                    .collect();
                let successful_runs: Vec<&CaseRunReport> = all_runs
                    .iter()
                    .copied()
                    .filter(|run| run.is_passed() && is_complete_success(run))
                    .collect();
                let successful_incomplete_runs: Vec<&CaseRunReport> = all_runs
                    .iter()
                    .copied()
                    .filter(|run| run.is_passed() && !is_complete_success(run))
                    .collect();
                BenchmarkGroupAggregate {
                    case_name,
                    model,
                    counts,
                    all_cost: sample_stats(&all_runs),
                    successful_cost: sample_stats(&successful_runs),
                    successful_incomplete_cost: sample_stats(&successful_incomplete_runs),
                    execution: aggregate_execution(&runs),
                }
            })
            .collect();

        let totals = groups
            .iter()
            .fold(BenchmarkCounts::default(), |mut total, group| {
                add_counts(&mut total, &group.counts);
                total
            });
        let all_runs: Vec<&CaseRunReport> =
            report.runs.iter().filter(|run| run.is_evidence()).collect();
        let successful_runs: Vec<&CaseRunReport> = all_runs
            .iter()
            .copied()
            .filter(|run| run.is_passed() && is_complete_success(run))
            .collect();
        let successful_incomplete_runs: Vec<&CaseRunReport> = all_runs
            .iter()
            .copied()
            .filter(|run| run.is_passed() && !is_complete_success(run))
            .collect();
        let all_cost = sample_stats(&all_runs);
        let successful_cost = sample_stats(&successful_runs);
        let successful_incomplete_cost = sample_stats(&successful_incomplete_runs);

        Self {
            schema: BENCHMARK_AGGREGATE_SCHEMA.to_string(),
            totals,
            all_cost,
            successful_cost,
            successful_incomplete_cost,
            wall_time_ms: report.wall_time_ms,
            groups,
        }
    }
}

fn aggregate_counts(runs: &[&CaseRunReport]) -> BenchmarkCounts {
    let mut counts = BenchmarkCounts {
        planned: runs.len(),
        ..BenchmarkCounts::default()
    };
    for run in runs {
        match run.status {
            CaseRunStatus::Passed => {
                counts.executed += 1;
                counts.passed += 1;
            }
            CaseRunStatus::Failed => {
                counts.executed += 1;
                counts.failed += 1;
            }
            CaseRunStatus::Cancelled => counts.cancelled += 1,
            CaseRunStatus::Unavailable => counts.unavailable += 1,
        }
        if run.is_evidence() {
            if run
                .execution
                .as_ref()
                .is_none_or(|execution| execution.evidence_complete)
            {
                counts.evidence_complete += 1;
            } else {
                counts.evidence_incomplete += 1;
            }
        }
    }
    counts
}

fn add_counts(total: &mut BenchmarkCounts, next: &BenchmarkCounts) {
    total.planned += next.planned;
    total.executed += next.executed;
    total.passed += next.passed;
    total.failed += next.failed;
    total.cancelled += next.cancelled;
    total.unavailable += next.unavailable;
    total.evidence_complete += next.evidence_complete;
    total.evidence_incomplete += next.evidence_incomplete;
}

fn sample_stats(runs: &[&CaseRunReport]) -> SampleStats {
    let mut tokens = Vec::with_capacity(runs.len());
    let mut durations = Vec::with_capacity(runs.len());
    let mut turns = Vec::with_capacity(runs.len());
    for run in runs {
        tokens.push(total_tokens(&run.outcome));
        durations.push(run.outcome.duration_ms);
        turns.push(u64::from(run.outcome.turn_rounds));
    }
    stats_from_values(&tokens, &durations, &turns)
}

fn stats_from_values(tokens: &[u64], durations: &[u64], turns: &[u64]) -> SampleStats {
    debug_assert_eq!(tokens.len(), durations.len());
    debug_assert_eq!(tokens.len(), turns.len());
    SampleStats {
        sample_count: tokens.len(),
        total_tokens: tokens.iter().copied().sum(),
        p50_tokens: percentile(tokens, 0.50),
        p95_tokens: percentile(tokens, 0.95),
        total_duration_ms: durations.iter().copied().sum(),
        p50_duration_ms: percentile(durations, 0.50),
        p95_duration_ms: percentile(durations, 0.95),
        total_turns: turns.iter().copied().sum(),
        p50_turns: percentile(turns, 0.50),
        p95_turns: percentile(turns, 0.95),
    }
}

fn aggregate_execution(runs: &[&CaseRunReport]) -> ExecutionAggregate {
    let mut aggregate = ExecutionAggregate::default();
    for run in runs {
        // Cancellation and unavailable rows are accounting rows.  Ignore any
        // stale or malformed execution payload attached to them rather than
        // attributing work that the runner says never executed.
        if !run.is_evidence() {
            continue;
        }
        let Some(execution) = &run.execution else {
            aggregate.attribution_missing_runs += 1;
            continue;
        };
        if execution.evidence_complete {
            aggregate.attribution_complete_runs += 1;
        } else {
            aggregate.attribution_incomplete_runs += 1;
        }
        aggregate.total_tool_calls += u64::from(execution.total_tool_calls);
        aggregate.executed_tool_calls += u64::from(execution.executed_tool_calls);
        aggregate.successful_tool_calls += u64::from(execution.successful_tool_calls);
        aggregate.failed_tool_calls += u64::from(execution.failed_tool_calls);
        aggregate.rejected_tool_calls += u64::from(execution.rejected_tool_calls);
        aggregate.reused_tool_calls += u64::from(execution.reused_tool_calls);
        aggregate.suppressed_tool_calls += u64::from(execution.suppressed_tool_calls);
        aggregate.deferred_tool_calls += u64::from(execution.deferred_tool_calls);
        aggregate.unknown_outcome_tool_calls += u64::from(execution.unknown_outcome_tool_calls);
        aggregate.unknown_disposition_tool_calls +=
            u64::from(execution.unknown_disposition_tool_calls);
        aggregate.settlement_attempts += u64::from(execution.settlement_attempts);
        aggregate.successful_settlements += u64::from(execution.successful_settlements);
        aggregate.rejected_settlements += u64::from(execution.rejected_settlements);
        for (reason, count) in &execution.runtime_rejection_reasons {
            *aggregate
                .runtime_rejection_reasons
                .entry(reason.clone())
                .or_default() += u64::from(*count);
        }
    }
    aggregate
}

fn is_complete_success(run: &CaseRunReport) -> bool {
    run.is_passed()
        && run
            .execution
            .as_ref()
            .is_none_or(|execution| execution.evidence_complete)
}

/// Billable token total used by aggregate, summary, and comparison consumers.
/// It includes fresh, cached, and cache-creation input buckets plus output.
pub fn total_tokens(outcome: &RunOutcome) -> u64 {
    astra_turn_types::NormalizedPromptCacheUsage::new(
        outcome.prompt_tokens,
        outcome.cached_input_tokens,
        outcome.cache_creation_tokens,
    )
    .total_input_tokens()
    .saturating_add(outcome.completion_tokens)
}

fn percentile(values: &[u64], quantile: f64) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let rank = (quantile * sorted.len() as f64).ceil() as usize;
    Some(sorted[rank.saturating_sub(1).min(sorted.len() - 1)])
}

/// Status of one quality or efficiency comparison.  `InsufficientEvidence`
/// is intentionally distinct from `Unchanged`: a one-sample run cannot prove
/// stability, and a missing/incomplete journal cannot prove a tool-count win.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComparisonStatus {
    Improved,
    Regressed,
    Unchanged,
    Mixed,
    InsufficientEvidence,
    Incomparable,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricDelta {
    pub baseline: Option<u64>,
    pub current: Option<u64>,
    pub delta: Option<i64>,
}

impl MetricDelta {
    fn new(baseline: Option<u64>, current: Option<u64>) -> Self {
        let delta = baseline.zip(current).map(|(baseline, current)| {
            let difference = i128::from(current) - i128::from(baseline);
            difference.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
        });
        Self {
            baseline,
            current,
            delta,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkGroupComparison {
    pub case_name: String,
    pub model: String,
    pub baseline_rows: usize,
    pub current_rows: usize,
    pub baseline_pass_rate: Option<f64>,
    pub current_pass_rate: Option<f64>,
    pub pass_rate_delta: Option<f64>,
    pub quality_status: ComparisonStatus,
    pub successful_tokens_p50: MetricDelta,
    pub successful_tokens_p95: MetricDelta,
    pub successful_duration_p50: MetricDelta,
    pub successful_duration_p95: MetricDelta,
    pub efficiency_status: ComparisonStatus,
    pub note: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComparisonSummary {
    pub quality_improved: usize,
    pub quality_regressed: usize,
    pub efficiency_improved: usize,
    pub efficiency_regressed: usize,
    pub efficiency_mixed: usize,
    pub insufficient_evidence: usize,
    pub incomparable: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkComparison {
    pub schema: String,
    pub comparable: bool,
    pub performance_comparable: bool,
    pub reasons: Vec<String>,
    pub performance_reasons: Vec<String>,
    pub binary_changed: bool,
    pub summary: ComparisonSummary,
    pub groups: Vec<BenchmarkGroupComparison>,
}

impl BenchmarkComparison {
    pub fn compare(current: &SuiteReport, baseline: &SuiteReport) -> Self {
        let current_aggregate = BenchmarkAggregate::from_report(current);
        let baseline_aggregate = BenchmarkAggregate::from_report(baseline);
        let (comparable, reasons, performance_comparable, performance_reasons, binary_changed) =
            compare_manifests(current.manifest.as_ref(), baseline.manifest.as_ref());

        let mut baseline_groups = BTreeMap::new();
        for group in baseline_aggregate.groups {
            baseline_groups.insert((group.case_name.clone(), group.model.clone()), group);
        }
        let mut current_groups = BTreeMap::new();
        for group in current_aggregate.groups {
            current_groups.insert((group.case_name.clone(), group.model.clone()), group);
        }
        let keys: BTreeSet<(String, String)> = baseline_groups
            .keys()
            .chain(current_groups.keys())
            .cloned()
            .collect();

        let mut groups = Vec::with_capacity(keys.len());
        let mut summary = ComparisonSummary::default();
        for key in keys {
            let baseline_group = baseline_groups.get(&key);
            let current_group = current_groups.get(&key);
            let comparison = compare_group(
                &key,
                baseline_group,
                current_group,
                comparable,
                performance_comparable,
            );
            if comparison.quality_status == ComparisonStatus::Improved {
                summary.quality_improved += 1;
            } else if comparison.quality_status == ComparisonStatus::Regressed {
                summary.quality_regressed += 1;
            }
            match comparison.efficiency_status {
                ComparisonStatus::Improved => summary.efficiency_improved += 1,
                ComparisonStatus::Regressed => summary.efficiency_regressed += 1,
                ComparisonStatus::Mixed => summary.efficiency_mixed += 1,
                ComparisonStatus::InsufficientEvidence => summary.insufficient_evidence += 1,
                ComparisonStatus::Incomparable => summary.incomparable += 1,
                ComparisonStatus::Unchanged => {}
            }
            groups.push(comparison);
        }

        Self {
            schema: BENCHMARK_COMPARISON_SCHEMA.to_string(),
            comparable,
            performance_comparable,
            reasons,
            performance_reasons,
            binary_changed,
            summary,
            groups,
        }
    }
}

fn compare_manifests(
    current: Option<&BenchmarkManifest>,
    baseline: Option<&BenchmarkManifest>,
) -> (bool, Vec<String>, bool, Vec<String>, bool) {
    let mut reasons = Vec::new();
    let mut performance_reasons = Vec::new();
    let (Some(current), Some(baseline)) = (current, baseline) else {
        reasons.push("both reports must contain a benchmark manifest".to_string());
        performance_reasons.push("binary/platform identity is missing".to_string());
        return (false, reasons, false, performance_reasons, false);
    };
    if current.schema != baseline.schema {
        reasons.push(format!(
            "manifest schema differs (baseline={}, current={})",
            baseline.schema, current.schema
        ));
    }
    if current.suite_digest != baseline.suite_digest {
        reasons.push("suite digest differs".to_string());
    }
    if current.case_names != baseline.case_names {
        reasons.push("case set differs".to_string());
    }
    if current.effective_models != baseline.effective_models {
        reasons.push("effective model matrix differs".to_string());
    }
    append_config_differences(&mut reasons, &baseline.config, &current.config);

    if current.platform != baseline.platform {
        performance_reasons.push(format!(
            "platform differs (baseline={}, current={})",
            baseline.platform, current.platform
        ));
    }
    if baseline.tested_binary.build_info.is_none() || current.tested_binary.build_info.is_none() {
        performance_reasons.push("one or both tested binary build identities are unknown".into());
    }
    if baseline.executor_identity.is_none() || current.executor_identity.is_none() {
        performance_reasons.push(
            "one or both case executor identities are unknown; external executors must provide a trusted identity"
                .into(),
        );
    }
    if baseline
        .executor_identity
        .as_ref()
        .is_some_and(|identity| identity.build_info.is_none())
        || current
            .executor_identity
            .as_ref()
            .is_some_and(|identity| identity.build_info.is_none())
    {
        performance_reasons.push("one or both case executor build identities are unknown".into());
    }
    if baseline
        .executor_identity
        .as_ref()
        .is_some_and(|identity| identity.probe_error.is_some())
        || current
            .executor_identity
            .as_ref()
            .is_some_and(|identity| identity.probe_error.is_some())
    {
        performance_reasons.push("one or both case executor identity probes failed".into());
    }
    // Paths are machine-local provenance and may differ between CI workers;
    // only the typed identity of the process that executed cases denotes a
    // binary change.  Auxiliary Astra CLI provenance is never used here.
    let binary_changed = current
        .executor_identity
        .as_ref()
        .and_then(|identity| identity.build_info.as_ref())
        != baseline
            .executor_identity
            .as_ref()
            .and_then(|identity| identity.build_info.as_ref());
    let comparable = reasons.is_empty();
    let performance_comparable = comparable && performance_reasons.is_empty();
    (
        comparable,
        reasons,
        performance_comparable,
        performance_reasons,
        binary_changed,
    )
}

fn append_config_differences(
    reasons: &mut Vec<String>,
    baseline: &BenchmarkConfig,
    current: &BenchmarkConfig,
) {
    macro_rules! compare_field {
        ($field:ident) => {
            if baseline.$field != current.$field {
                reasons.push(format!("config.{} differs", stringify!($field)));
            }
        };
    }
    compare_field!(profile);
    compare_field!(working_dir);
    compare_field!(runs);
    compare_field!(parallel);
    compare_field!(circuit_breaker_threshold);
    compare_field!(retry_on_429);
    compare_field!(session_capture_mode);
    compare_field!(no_judger);
    compare_field!(judger_kind);
    compare_field!(judger_command_digest);
    compare_field!(judger_model);
    compare_field!(judger_n);
    compare_field!(judger_agg);
    compare_field!(judger_timeout_seconds);
    compare_field!(capability_probes);
    compare_field!(prompt_variants);
    compare_field!(executor_kind);
    compare_field!(executor_command_digest);
}

// Two observations can only show a difference; three are the minimum for a
// reproducible median/p95 signal and keep one outlier from deciding a verdict.
const MIN_COMPARISON_SAMPLES: usize = 3;

fn compare_group(
    key: &(String, String),
    baseline: Option<&BenchmarkGroupAggregate>,
    current: Option<&BenchmarkGroupAggregate>,
    comparable: bool,
    performance_comparable: bool,
) -> BenchmarkGroupComparison {
    let (Some(baseline), Some(current)) = (baseline, current) else {
        return BenchmarkGroupComparison {
            case_name: key.0.clone(),
            model: key.1.clone(),
            baseline_rows: baseline.map_or(0, |group| group.counts.planned),
            current_rows: current.map_or(0, |group| group.counts.planned),
            baseline_pass_rate: baseline.and_then(|group| group.counts.pass_rate()),
            current_pass_rate: current.and_then(|group| group.counts.pass_rate()),
            pass_rate_delta: None,
            quality_status: ComparisonStatus::Incomparable,
            successful_tokens_p50: MetricDelta::default(),
            successful_tokens_p95: MetricDelta::default(),
            successful_duration_p50: MetricDelta::default(),
            successful_duration_p95: MetricDelta::default(),
            efficiency_status: ComparisonStatus::Incomparable,
            note: "case/model group exists in only one report".to_string(),
        };
    };

    let baseline_pass_rate = baseline.counts.pass_rate();
    let current_pass_rate = current.counts.pass_rate();
    let pass_rate_delta = baseline_pass_rate
        .zip(current_pass_rate)
        .map(|(b, c)| c - b);
    let coverage_gap = current.counts.executed < baseline.counts.executed
        || current.counts.cancelled > baseline.counts.cancelled
        || current.counts.unavailable > baseline.counts.unavailable;
    let quality_status = if !comparable {
        ComparisonStatus::Incomparable
    } else if coverage_gap
        || baseline.counts.executed < MIN_COMPARISON_SAMPLES
        || current.counts.executed < MIN_COMPARISON_SAMPLES
    {
        ComparisonStatus::InsufficientEvidence
    } else {
        compare_quality(baseline_pass_rate, current_pass_rate)
    };

    let successful_tokens_p50 = MetricDelta::new(
        baseline.successful_cost.p50_tokens,
        current.successful_cost.p50_tokens,
    );
    let successful_tokens_p95 = MetricDelta::new(
        baseline.successful_cost.p95_tokens,
        current.successful_cost.p95_tokens,
    );
    let successful_duration_p50 = MetricDelta::new(
        baseline.successful_cost.p50_duration_ms,
        current.successful_cost.p50_duration_ms,
    );
    let successful_duration_p95 = MetricDelta::new(
        baseline.successful_cost.p95_duration_ms,
        current.successful_cost.p95_duration_ms,
    );

    let (efficiency_status, note) = if !performance_comparable {
        (
            ComparisonStatus::Incomparable,
            "performance comparison requires matching platform and known binary identities"
                .to_string(),
        )
    } else if quality_status == ComparisonStatus::Regressed {
        (
            ComparisonStatus::InsufficientEvidence,
            "quality regressed; lower cost is not credited as an efficiency improvement".into(),
        )
    } else if coverage_gap {
        (
            ComparisonStatus::InsufficientEvidence,
            "current run has less execution coverage; missing or cancelled rows cannot lower the cost baseline"
                .into(),
        )
    } else if baseline.counts.evidence_incomplete > 0
        || current.counts.evidence_incomplete > 0
        || baseline.successful_cost.sample_count < MIN_COMPARISON_SAMPLES
        || current.successful_cost.sample_count < MIN_COMPARISON_SAMPLES
    {
        (
            ComparisonStatus::InsufficientEvidence,
            "need at least three successful, complete observations in both reports".into(),
        )
    } else {
        (
            compare_efficiency(
                &successful_tokens_p50,
                &successful_tokens_p95,
                &successful_duration_p50,
                &successful_duration_p95,
            ),
            "successful samples only; failed or cancelled rows do not lower the cost baseline"
                .into(),
        )
    };

    BenchmarkGroupComparison {
        case_name: key.0.clone(),
        model: key.1.clone(),
        baseline_rows: baseline.counts.planned,
        current_rows: current.counts.planned,
        baseline_pass_rate,
        current_pass_rate,
        pass_rate_delta,
        quality_status,
        successful_tokens_p50,
        successful_tokens_p95,
        successful_duration_p50,
        successful_duration_p95,
        efficiency_status,
        note,
    }
}

fn compare_quality(baseline: Option<f64>, current: Option<f64>) -> ComparisonStatus {
    let (Some(baseline), Some(current)) = (baseline, current) else {
        return ComparisonStatus::InsufficientEvidence;
    };
    const EPSILON: f64 = 1e-12;
    if current > baseline + EPSILON {
        ComparisonStatus::Improved
    } else if current + EPSILON < baseline {
        ComparisonStatus::Regressed
    } else {
        ComparisonStatus::Unchanged
    }
}

fn compare_efficiency(
    tokens_p50: &MetricDelta,
    tokens_p95: &MetricDelta,
    duration_p50: &MetricDelta,
    duration_p95: &MetricDelta,
) -> ComparisonStatus {
    let deltas: Vec<i64> = [
        tokens_p50.delta,
        tokens_p95.delta,
        duration_p50.delta,
        duration_p95.delta,
    ]
    .into_iter()
    .flatten()
    .collect();
    if deltas.is_empty() {
        return ComparisonStatus::InsufficientEvidence;
    }
    let has_better = deltas.iter().any(|delta| *delta < 0);
    let has_worse = deltas.iter().any(|delta| *delta > 0);
    match (has_better, has_worse) {
        (true, false) => ComparisonStatus::Improved,
        (false, true) => ComparisonStatus::Regressed,
        (true, true) => ComparisonStatus::Mixed,
        (false, false) => ComparisonStatus::Unchanged,
    }
}

/// SHA-256 over a canonical representation of the loaded case documents.
/// `Case::prompt_variants` is intentionally skipped by its public serde
/// representation, so it is inserted explicitly here; dormant variants are
/// part of the benchmark definition even before expansion.
pub fn digest_cases(cases: &[Case]) -> String {
    let mut fingerprints: Vec<Value> = cases
        .iter()
        .map(|case| {
            let mut value = serde_json::to_value(case).unwrap_or(Value::Null);
            if let Value::Object(object) = &mut value {
                object.insert(
                    "prompt_variants".to_string(),
                    serde_json::to_value(&case.prompt_variants).unwrap_or(Value::Null),
                );
            }
            canonicalize(value)
        })
        .collect();
    fingerprints.sort_by(|left, right| {
        let left_name = left.get("name").and_then(Value::as_str).unwrap_or_default();
        let right_name = right
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        left_name.cmp(right_name)
    });
    let value = canonicalize(Value::Array(fingerprints));
    let bytes = serde_json::to_vec(&value).unwrap_or_default();
    digest_bytes(&bytes)
}

fn canonicalize(value: Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut sorted = serde_json::Map::new();
            let mut entries: Vec<_> = object.into_iter().collect();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            for (key, value) in entries {
                sorted.insert(key, canonicalize(value));
            }
            Value::Object(sorted)
        }
        Value::Array(values) => Value::Array(values.into_iter().map(canonicalize).collect()),
        other => other,
    }
}

fn digest_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut rendered = String::with_capacity(7 + digest.len() * 2);
    rendered.push_str("sha256:");
    for byte in digest {
        rendered.push_str(&format!("{byte:02x}"));
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::case::Capability;
    use crate::report::CaseRunStatus;

    fn outcome(model: &str, tokens: u64, duration_ms: u64) -> RunOutcome {
        let mut outcome = RunOutcome::new(model);
        outcome.prompt_tokens = tokens;
        outcome.duration_ms = duration_ms;
        outcome.turn_rounds = 2;
        outcome.exit_code = 0;
        outcome
    }

    fn run(
        case: &str,
        model: &str,
        index: u32,
        status: CaseRunStatus,
        tokens: u64,
    ) -> CaseRunReport {
        CaseRunReport {
            case_name: case.into(),
            model: model.into(),
            status,
            run_index: index,
            capability: Some(Capability::Reasoning),
            weight: 1.0,
            difficulty: Some(1),
            outcome: outcome(model, tokens, tokens),
            criteria: vec![],
            steps: vec![],
            attempts: vec![],
            session: None,
            execution: None,
            reproducer: None,
            digest: None,
            digest_error: None,
            failure_class: None,
            has_warnings: false,
        }
    }

    fn manifest(cases: &[Case], binary_sha: &str) -> BenchmarkManifest {
        BenchmarkManifest::new(
            cases,
            &["model".into()],
            BenchmarkConfig {
                profile: Some("p".into()),
                working_dir: None,
                runs: 2,
                parallel: 1,
                circuit_breaker_threshold: 3,
                retry_on_429: false,
                session_capture_mode: "always".into(),
                no_judger: true,
                judger_kind: JudgerKind::Disabled,
                judger_command_digest: None,
                judger_model: "judge".into(),
                judger_n: 1,
                judger_agg: "median".into(),
                judger_timeout_seconds: 120,
                capability_probes: false,
                prompt_variants: false,
                executor_kind: ExecutorKind::Builtin,
                executor_command_digest: None,
            },
            BinaryIdentity {
                path: "/bin/astra".into(),
                build_info: Some(BinaryBuildInfo {
                    schema: "astra.build_info.v1".into(),
                    git_sha: binary_sha.into(),
                    git_dirty: false,
                    target: "x86_64".into(),
                    profile: "release".into(),
                }),
                probe_error: None,
            },
            Some(BinaryIdentity {
                path: "/bin/astra".into(),
                build_info: Some(BinaryBuildInfo {
                    schema: "astra.build_info.v1".into(),
                    git_sha: binary_sha.into(),
                    git_dirty: false,
                    target: "x86_64".into(),
                    profile: "release".into(),
                }),
                probe_error: None,
            }),
            "2026-09-16T00:00:00Z",
        )
    }

    fn case(name: &str, prompt: &str) -> Case {
        serde_yaml_ng::from_str(&format!("name: {name}\nprompt: {prompt}\n")).unwrap()
    }

    #[test]
    fn case_digest_is_order_and_map_order_independent() {
        let first: Case =
            serde_yaml_ng::from_str("name: a\nprompt: one\ncli_env:\n  B: two\n  A: one\n")
                .unwrap();
        let second: Case =
            serde_yaml_ng::from_str("name: b\nprompt: two\ncli_env:\n  A: one\n  B: two\n")
                .unwrap();
        assert_eq!(
            digest_cases(&[first.clone(), second.clone()]),
            digest_cases(&[second, first])
        );
    }

    #[test]
    fn aggregate_keeps_cancelled_unavailable_and_uses_success_costs() {
        let mut passed_a = run("c", "model", 0, CaseRunStatus::Passed, 10);
        passed_a.outcome.cache_creation_tokens = 5;
        let passed_b = run("c", "model", 1, CaseRunStatus::Passed, 20);
        let failed = run("c", "model", 2, CaseRunStatus::Failed, 1);
        let cancelled = run("c", "model", 3, CaseRunStatus::Cancelled, 0);
        let unavailable = run("c", "model", 4, CaseRunStatus::Unavailable, 0);
        let report = SuiteReport {
            runs: vec![passed_a, passed_b, failed, cancelled, unavailable],
            wall_time_ms: 99,
            ..Default::default()
        };
        let aggregate = BenchmarkAggregate::from_report(&report);
        assert_eq!(aggregate.totals.planned, 5);
        assert_eq!(aggregate.totals.executed, 3);
        assert_eq!(aggregate.totals.passed, 2);
        assert_eq!(aggregate.totals.failed, 1);
        assert_eq!(aggregate.totals.cancelled, 1);
        assert_eq!(aggregate.totals.unavailable, 1);
        assert_eq!(aggregate.all_cost.sample_count, 3);
        assert_eq!(aggregate.successful_cost.sample_count, 2);
        assert_eq!(aggregate.successful_cost.total_tokens, 35);
        assert_eq!(aggregate.successful_incomplete_cost.sample_count, 0);
        assert_eq!(aggregate.wall_time_ms, 99);
    }

    #[test]
    fn incomplete_attribution_never_looks_like_zero_tool_execution() {
        let mut first = run("c", "model", 0, CaseRunStatus::Passed, 10);
        first.execution = Some(crate::pipeline_analysis::ExecutionTraceReport {
            total_tool_calls: 2,
            executed_tool_calls: 1,
            successful_tool_calls: 1,
            evidence_complete: false,
            ..Default::default()
        });
        let report = SuiteReport {
            runs: vec![first],
            ..Default::default()
        };
        let group = &BenchmarkAggregate::from_report(&report).groups[0];
        assert_eq!(group.execution.attribution_incomplete_runs, 1);
        assert_eq!(group.execution.total_tool_calls, 2);
        assert_eq!(group.counts.evidence_incomplete, 1);
        assert_eq!(group.successful_cost.sample_count, 0);
        assert_eq!(group.successful_incomplete_cost.sample_count, 1);
    }

    #[test]
    fn comparison_requires_manifest_and_three_successes_before_efficiency_claim() {
        let cases = vec![case("c", "prompt")];
        let baseline = SuiteReport {
            runs: vec![run("c", "model", 0, CaseRunStatus::Passed, 10)],
            manifest: Some(manifest(&cases, "old")),
            ..Default::default()
        };
        let current = SuiteReport {
            runs: vec![run("c", "model", 0, CaseRunStatus::Passed, 1)],
            manifest: Some(manifest(&cases, "new")),
            ..Default::default()
        };
        let comparison = BenchmarkComparison::compare(&current, &baseline);
        assert!(comparison.comparable);
        assert!(comparison.performance_comparable);
        assert_eq!(
            comparison.groups[0].quality_status,
            ComparisonStatus::InsufficientEvidence
        );
        assert_eq!(
            comparison.groups[0].efficiency_status,
            ComparisonStatus::InsufficientEvidence
        );
        assert!(comparison.binary_changed);

        let without_manifest = SuiteReport {
            runs: current.runs.clone(),
            ..Default::default()
        };
        let incomparable = BenchmarkComparison::compare(&without_manifest, &baseline);
        assert!(!incomparable.comparable);
        assert_eq!(
            incomparable.groups[0].quality_status,
            ComparisonStatus::Incomparable
        );
    }

    #[test]
    fn quality_regression_blocks_cost_improvement_credit() {
        let cases = vec![case("c", "prompt")];
        let mut baseline_runs = vec![run("c", "model", 0, CaseRunStatus::Passed, 100)];
        baseline_runs.push(run("c", "model", 1, CaseRunStatus::Passed, 100));
        baseline_runs.push(run("c", "model", 2, CaseRunStatus::Passed, 100));
        let mut current_runs = vec![run("c", "model", 0, CaseRunStatus::Failed, 1)];
        current_runs.push(run("c", "model", 1, CaseRunStatus::Passed, 1));
        current_runs.push(run("c", "model", 2, CaseRunStatus::Passed, 1));
        let baseline = SuiteReport {
            runs: baseline_runs,
            manifest: Some(manifest(&cases, "old")),
            ..Default::default()
        };
        let current = SuiteReport {
            runs: current_runs,
            manifest: Some(manifest(&cases, "new")),
            ..Default::default()
        };
        let group = &BenchmarkComparison::compare(&current, &baseline).groups[0];
        assert_eq!(group.quality_status, ComparisonStatus::Regressed);
        assert_eq!(
            group.efficiency_status,
            ComparisonStatus::InsufficientEvidence
        );
        assert!(group.note.contains("quality regressed"));
    }

    #[test]
    fn missing_execution_coverage_cannot_look_like_quality_or_efficiency_gain() {
        let cases = vec![case("c", "prompt")];
        let baseline = SuiteReport {
            runs: (0..6)
                .map(|index| {
                    run(
                        "c",
                        "model",
                        index,
                        if index < 3 {
                            CaseRunStatus::Passed
                        } else {
                            CaseRunStatus::Failed
                        },
                        100,
                    )
                })
                .collect(),
            manifest: Some(manifest(&cases, "same")),
            ..Default::default()
        };
        let current = SuiteReport {
            runs: (0..6)
                .map(|index| {
                    run(
                        "c",
                        "model",
                        index,
                        if index < 3 {
                            CaseRunStatus::Passed
                        } else {
                            CaseRunStatus::Unavailable
                        },
                        1,
                    )
                })
                .collect(),
            manifest: Some(manifest(&cases, "same")),
            ..Default::default()
        };
        let group = &BenchmarkComparison::compare(&current, &baseline).groups[0];
        assert_eq!(group.baseline_pass_rate, Some(0.5));
        assert_eq!(group.current_pass_rate, Some(1.0));
        assert_eq!(group.quality_status, ComparisonStatus::InsufficientEvidence);
        assert_eq!(
            group.efficiency_status,
            ComparisonStatus::InsufficientEvidence
        );
        assert!(group.note.contains("coverage"));
    }

    #[test]
    fn external_executor_without_identity_disables_performance_comparison() {
        let cases = vec![case("c", "prompt")];
        let mut baseline_manifest = manifest(&cases, "same");
        baseline_manifest.config.executor_kind = ExecutorKind::External;
        baseline_manifest.config.executor_command_digest = Some("sha256:executor".into());
        baseline_manifest.executor_identity = None;
        let current_manifest = baseline_manifest.clone();
        let baseline = SuiteReport {
            runs: vec![run("c", "model", 0, CaseRunStatus::Passed, 10)],
            manifest: Some(baseline_manifest),
            ..Default::default()
        };
        let current = SuiteReport {
            runs: vec![run("c", "model", 0, CaseRunStatus::Passed, 1)],
            manifest: Some(current_manifest),
            ..Default::default()
        };
        let comparison = BenchmarkComparison::compare(&current, &baseline);
        assert!(comparison.comparable);
        assert!(!comparison.performance_comparable);
        assert!(
            comparison
                .performance_reasons
                .iter()
                .any(|reason| reason.contains("executor identities"))
        );
        assert_eq!(
            comparison.groups[0].efficiency_status,
            ComparisonStatus::Incomparable
        );
    }

    #[test]
    fn judger_configuration_is_part_of_comparison_identity() {
        let cases = vec![case("c", "prompt")];
        let baseline = SuiteReport {
            runs: vec![run("c", "model", 0, CaseRunStatus::Passed, 10)],
            manifest: Some(manifest(&cases, "same")),
            ..Default::default()
        };
        let mut current_manifest = manifest(&cases, "same");
        current_manifest.config.judger_timeout_seconds = 60;
        let current = SuiteReport {
            runs: vec![run("c", "model", 0, CaseRunStatus::Passed, 1)],
            manifest: Some(current_manifest),
            ..Default::default()
        };
        let comparison = BenchmarkComparison::compare(&current, &baseline);
        assert!(!comparison.comparable);
        assert!(
            comparison
                .reasons
                .iter()
                .any(|reason| reason.contains("judger_timeout_seconds"))
        );
    }

    #[test]
    fn binary_probe_errors_do_not_echo_external_output() {
        let error = parse_binary_build_info(br#"{"schema":"secret-provider-output"}"#)
            .expect_err("unexpected schema must fail");
        assert_eq!(error, "unsupported_build_info_schema");
    }
}

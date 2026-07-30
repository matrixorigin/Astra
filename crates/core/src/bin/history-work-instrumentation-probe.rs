//! Reproducible Phase-0 instrumentation probe for local O(history) counters.
//!
//! This is a deliberately synthetic, dedicated-process runner. It produces a
//! machine-readable plumbing artifact. It does not execute production history,
//! provider, database, cache, admission, or queue paths and is not a baseline.

use std::fs;
use std::hint::black_box;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use astra_core::history_work::{
    HISTORY_WORK_COVERAGE_COMPLETE, HISTORY_WORK_KNOWN_OMISSIONS, HistoryWorkDelta,
    HistoryWorkMeasurement, HistoryWorkScenario, HistoryWorkSite, QueueBytesReservation,
    record_bytes, serialized_bytes,
};
use serde::Serialize;
use sha2::{Digest, Sha256};

const SCHEMA: &str = "astra.history_work_instrumentation_probe.v1";
const DEFAULT_SEED: u64 = 0x0730_2026_5eed;
const DEFAULT_SAMPLES: usize = 30;
const INPUT_BUDGET_PERCENT: u64 = 80;
const TARGET_PRESSURE_PERCENT: u64 = 90;
const SYNTHETIC_BYTES_PER_TOKEN: u64 = 4;

const ANCHORS: [AnchorSpec; 3] = [
    AnchorSpec {
        name: "synthetic_shape_128k_10t_1r",
        window_tokens: 131_072,
        turns: 10,
        rounds_per_turn: 1,
        declared_cache_scenario: "warm",
    },
    AnchorSpec {
        name: "synthetic_shape_200k_50t_3r",
        window_tokens: 204_800,
        turns: 50,
        rounds_per_turn: 3,
        declared_cache_scenario: "warm",
    },
    AnchorSpec {
        name: "synthetic_shape_1m_100t_10r",
        window_tokens: 1_000_000,
        turns: 100,
        rounds_per_turn: 10,
        declared_cache_scenario: "cold",
    },
];

#[derive(Debug, Clone, Copy)]
struct AnchorSpec {
    name: &'static str,
    window_tokens: u64,
    turns: usize,
    rounds_per_turn: usize,
    declared_cache_scenario: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct FixtureMessage {
    role: &'static str,
    kind: &'static str,
    turn: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    round: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    correlation_id: Option<String>,
    content: String,
}

#[derive(Debug)]
struct Fixture {
    messages: Vec<FixtureMessage>,
    serialized: Vec<u8>,
    sha256: String,
    target_input_tokens: u64,
    target_payload_bytes: u64,
}

#[derive(Debug, Serialize)]
struct InstrumentationProbeArtifact {
    schema: &'static str,
    generated_at_unix_seconds: u64,
    run: RunMetadata,
    machine: MachineMetadata,
    coverage: Coverage,
    fixture_suite_hash_sha256: String,
    anchors: Vec<AnchorObservation>,
    unmeasured_phase0_metrics: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
struct RunMetadata {
    seed: u64,
    samples_per_anchor: usize,
    git_sha: String,
    git_dirty: Option<bool>,
    git_diff_sha256: Option<String>,
    git_untracked_files: Vec<SourceFileHash>,
    executable_sha256: Option<String>,
    build_profile: &'static str,
    rustc: String,
    fixture_contract: FixtureContract,
}

#[derive(Debug, Serialize)]
struct SourceFileHash {
    path: String,
    sha256: String,
}

#[derive(Debug, Serialize)]
struct FixtureContract {
    input_budget_percent: u64,
    target_pressure_percent: u64,
    synthetic_bytes_per_token: u64,
    token_semantics: &'static str,
}

#[derive(Debug, Serialize)]
struct MachineMetadata {
    os: &'static str,
    os_release: Option<String>,
    arch: &'static str,
    kernel: Option<String>,
    cpu_model: Option<String>,
    logical_cpu_count: Option<usize>,
    memory_bytes: Option<u64>,
}

#[derive(Debug, Serialize)]
struct Coverage {
    coverage_complete: bool,
    instrumented_site_count: usize,
    instrumented_sites: Vec<SiteInventory>,
    omissions_are_exhaustive: bool,
    known_omissions: Vec<Omission>,
    scope_statement: &'static str,
}

#[derive(Debug, Serialize)]
struct SiteInventory {
    site: &'static str,
    owner: &'static str,
    primary_target_phase: u8,
}

#[derive(Debug, Serialize)]
struct Omission {
    key: &'static str,
    owner: &'static str,
    target_phase: u8,
    reason: &'static str,
}

#[derive(Debug, Serialize)]
struct AnchorObservation {
    name: &'static str,
    window_tokens: u64,
    usable_input_tokens: u64,
    target_input_tokens: u64,
    fixture_turns: usize,
    fixture_rounds_per_turn: usize,
    declared_cache_scenario: &'static str,
    cache_behavior_exercised: bool,
    fixture_message_count: usize,
    fixture_payload_bytes: u64,
    fixture_hash_sha256: String,
    synthetic_fixture_serialization: SyntheticFixtureSerialization,
    clone_integrity_check: CloneIntegrityCheck,
    clone_timing_ns: TimingSummary,
    history_hash_work_ns: u64,
    instrumentation_overhead: InstrumentationOverhead,
    synthetic_counter_probe: CounterProbeObservation,
}

#[derive(Debug, Serialize)]
struct SyntheticFixtureSerialization {
    compact_json_bytes: u64,
    semantics: &'static str,
}

#[derive(Debug, Serialize)]
struct CloneIntegrityCheck {
    kind: &'static str,
    before_sha256: String,
    after_sha256: String,
    exact_structured_equality: bool,
    passed: bool,
}

#[derive(Debug, Serialize)]
struct TimingSummary {
    samples: usize,
    min: u64,
    median: u64,
    p95_nearest_rank: u64,
    max: u64,
}

#[derive(Debug, Default, Serialize)]
struct InstrumentationOverhead {
    excluded_from_clone_timing: bool,
    counter_recording_ns: u64,
    serialized_byte_measurement_ns: u64,
    clone_integrity_check_ns: u64,
    queue_accounting_ns: u64,
    scenario_begin_ns: u64,
    scenario_finish_ns: u64,
    note: &'static str,
}

#[derive(Debug, Serialize)]
struct CounterProbeObservation {
    production_path_exercised: bool,
    measurement_scope: &'static str,
    process_delta_consistency: &'static str,
    id: u64,
    label: String,
    elapsed_ns: u64,
    queue_leak_free: bool,
    scoped_sites: Vec<SiteMeasurement>,
    process_delta_sites: Vec<SiteDeltaMeasurement>,
}

#[derive(Debug, Serialize)]
struct SiteMeasurement {
    site: &'static str,
    owner: &'static str,
    primary_target_phase: u8,
    evidence_kind: &'static str,
    #[serde(flatten)]
    measurement: HistoryWorkMeasurement,
}

#[derive(Debug, Serialize)]
struct SiteDeltaMeasurement {
    site: &'static str,
    evidence_kind: &'static str,
    events: u64,
    bytes: u64,
    rows: u64,
    admission_units: u64,
    queue_current_bytes_change: i128,
    queue_peak_bytes_increase: u64,
    accounting_errors: u64,
}

#[derive(Debug)]
struct Options {
    seed: u64,
    samples: usize,
    output: Option<PathBuf>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("history-work-instrumentation-probe: {error}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let Some(options) = parse_options(std::env::args().skip(1))? else {
        print_help();
        return Ok(());
    };
    let artifact = build_artifact(&options)?;
    let mut json = serde_json::to_vec_pretty(&artifact)?;
    json.push(b'\n');
    write_artifact(options.output.as_deref(), &json)?;
    Ok(())
}

fn parse_options(
    arguments: impl IntoIterator<Item = String>,
) -> Result<Option<Options>, Box<dyn std::error::Error>> {
    let mut seed = DEFAULT_SEED;
    let mut samples = DEFAULT_SAMPLES;
    let mut output = None;
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "-h" | "--help" => return Ok(None),
            "--seed" => {
                let value = arguments
                    .next()
                    .ok_or("--seed requires an unsigned integer")?;
                seed = value.parse()?;
            }
            "--samples" => {
                let value = arguments
                    .next()
                    .ok_or("--samples requires an integer from 3 through 100")?;
                samples = value.parse()?;
                if !(3..=100).contains(&samples) {
                    return Err("--samples must be in 3..=100".into());
                }
            }
            "--output" => {
                let value = arguments.next().ok_or("--output requires a path or '-'")?;
                output = (value != "-").then(|| PathBuf::from(value));
            }
            _ => return Err(format!("unknown argument: {argument}").into()),
        }
    }
    Ok(Some(Options {
        seed,
        samples,
        output,
    }))
}

fn print_help() {
    println!(
        "Usage: cargo run -p astra-core --bin history-work-instrumentation-probe -- \\\n         [--seed U64] [--samples 3..100] [--output PATH|-]"
    );
}

fn build_artifact(
    options: &Options,
) -> Result<InstrumentationProbeArtifact, Box<dyn std::error::Error>> {
    let mut anchors = Vec::with_capacity(ANCHORS.len());
    for (index, spec) in ANCHORS.iter().enumerate() {
        anchors.push(measure_anchor(
            *spec,
            mix_seed(options.seed, index as u64),
            options.samples,
        )?);
    }

    let mut suite_hasher = Sha256::new();
    suite_hasher.update(options.seed.to_le_bytes());
    for anchor in &anchors {
        suite_hasher.update(anchor.name.as_bytes());
        suite_hasher.update(anchor.fixture_hash_sha256.as_bytes());
    }

    Ok(InstrumentationProbeArtifact {
        schema: SCHEMA,
        generated_at_unix_seconds: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        run: RunMetadata {
            seed: options.seed,
            samples_per_anchor: options.samples,
            git_sha: command_output(repo_root(), "git", &["rev-parse", "HEAD"])
                .unwrap_or_else(|| "unknown".to_owned()),
            git_dirty: git_dirty(),
            git_diff_sha256: command_bytes(repo_root(), "git", &["diff", "--binary", "HEAD"])
                .map(|bytes| sha256_hex(&bytes)),
            git_untracked_files: git_untracked_file_hashes()?,
            executable_sha256: std::env::current_exe()
                .ok()
                .and_then(|path| fs::read(path).ok())
                .map(|bytes| sha256_hex(&bytes)),
            build_profile: if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            },
            rustc: command_output(repo_root(), "rustc", &["-Vv"])
                .unwrap_or_else(|| "unknown".to_owned()),
            fixture_contract: FixtureContract {
                input_budget_percent: INPUT_BUDGET_PERCENT,
                target_pressure_percent: TARGET_PRESSURE_PERCENT,
                synthetic_bytes_per_token: SYNTHETIC_BYTES_PER_TOKEN,
                token_semantics: "synthetic ASCII payload calibration; not provider tokenizer truth",
            },
        },
        machine: machine_metadata(),
        coverage: coverage(),
        fixture_suite_hash_sha256: hex_digest(suite_hasher.finalize().as_slice()),
        anchors,
        unmeasured_phase0_metrics: vec![
            "production-path per-turn and per-round O(history) amplification",
            "provider cache share",
            "projection lag",
            "compaction rate and tokens freed",
            "estimator error against canonical provider usage",
            "multi-tenant fairness and queue wait",
            "database/object-store operations outside instrumented sites",
            "RSS and allocator amplification",
        ],
    })
}

fn measure_anchor(
    spec: AnchorSpec,
    seed: u64,
    samples: usize,
) -> Result<AnchorObservation, Box<dyn std::error::Error>> {
    let fixture = build_fixture(spec, seed)?;

    // Warm allocator code paths before timing. This says nothing about the
    // declared provider-cache scenario, which this synthetic probe does not
    // exercise.
    black_box(fixture.messages.clone());

    let begin = Instant::now();
    let scenario = HistoryWorkScenario::begin(spec.name)?;
    let scenario_begin_ns = elapsed_ns(begin);
    let mut clone_samples = Vec::with_capacity(samples);
    let mut last_clone = None;
    let mut counter_recording_ns = 0_u64;
    for _ in 0..samples {
        let started = Instant::now();
        let cloned = black_box(fixture.messages.clone());
        clone_samples.push(elapsed_ns(started));

        let recording_started = Instant::now();
        record_bytes(
            HistoryWorkSite::AgenticRequestSnapshot,
            fixture.serialized.len() as u64,
        );
        counter_recording_ns = counter_recording_ns.saturating_add(elapsed_ns(recording_started));
        last_clone = Some(cloned);
    }

    let byte_measurement_started = Instant::now();
    let independently_measured_bytes = serialized_bytes(&fixture.messages)?;
    let serialized_byte_measurement_ns = elapsed_ns(byte_measurement_started);
    if independently_measured_bytes != fixture.serialized.len() as u64 {
        return Err("serialized byte counter disagrees with synthetic fixture JSON bytes".into());
    }
    let recording_started = Instant::now();
    record_bytes(
        HistoryWorkSite::CslFileAppendSerialization,
        independently_measured_bytes,
    );
    counter_recording_ns = counter_recording_ns.saturating_add(elapsed_ns(recording_started));

    let hash_started = Instant::now();
    let measured_hash = sha256_hex(&fixture.serialized);
    let history_hash_ns = elapsed_ns(hash_started);
    let recording_started = Instant::now();
    record_bytes(
        HistoryWorkSite::CslMessageHash,
        fixture.serialized.len() as u64,
    );
    counter_recording_ns = counter_recording_ns.saturating_add(elapsed_ns(recording_started));

    let queue_started = Instant::now();
    {
        let full_queue = QueueBytesReservation::for_site(
            HistoryWorkSite::CliPostCommitQueue,
            fixture.serialized.len() as u64,
        );
        let suffix_queue = QueueBytesReservation::for_site(
            HistoryWorkSite::CliPostCommitQueue,
            (fixture.serialized.len() as u64 / 10).max(1),
        );
        drop(suffix_queue);
        drop(full_queue);
    }
    let queue_accounting_ns = elapsed_ns(queue_started);

    let integrity_check_started = Instant::now();
    let last_clone = last_clone.expect("samples are validated as nonzero");
    let after_serialized = serde_json::to_vec(&last_clone)?;
    let after_hash = sha256_hex(&after_serialized);
    let exact_structured_equality = fixture.messages == last_clone;
    let integrity_check_passed = exact_structured_equality
        && fixture.sha256 == after_hash
        && fixture.sha256 == measured_hash;
    let clone_integrity_check_ns = elapsed_ns(integrity_check_started);
    if !integrity_check_passed {
        return Err(format!("clone integrity check failed for {}", spec.name).into());
    }

    let finish_started = Instant::now();
    let report = scenario.finish()?;
    let scenario_finish_ns = elapsed_ns(finish_started);
    validate_counter_probe_accounting(&report.scoped, &report.global_delta)
        .map_err(|reason| format!("synthetic counter probe {} rejected: {reason}", spec.name))?;
    let queue_leak_free = true;

    Ok(AnchorObservation {
        name: spec.name,
        window_tokens: spec.window_tokens,
        usable_input_tokens: spec.window_tokens.saturating_mul(INPUT_BUDGET_PERCENT) / 100,
        target_input_tokens: fixture.target_input_tokens,
        fixture_turns: spec.turns,
        fixture_rounds_per_turn: spec.rounds_per_turn,
        declared_cache_scenario: spec.declared_cache_scenario,
        cache_behavior_exercised: false,
        fixture_message_count: fixture.messages.len(),
        fixture_payload_bytes: fixture.target_payload_bytes,
        fixture_hash_sha256: fixture.sha256.clone(),
        synthetic_fixture_serialization: SyntheticFixtureSerialization {
            compact_json_bytes: fixture.serialized.len() as u64,
            semantics: "compact JSON bytes of FixtureMessage values; not a provider request body or provider-cache measurement",
        },
        clone_integrity_check: CloneIntegrityCheck {
            kind: "same-source Vec clone equality plus SHA-256 of compact fixture JSON; not an independent production semantic oracle",
            before_sha256: fixture.sha256,
            after_sha256: after_hash,
            exact_structured_equality,
            passed: integrity_check_passed,
        },
        clone_timing_ns: timing_summary(clone_samples),
        history_hash_work_ns: history_hash_ns,
        instrumentation_overhead: InstrumentationOverhead {
            excluded_from_clone_timing: true,
            counter_recording_ns,
            serialized_byte_measurement_ns,
            clone_integrity_check_ns,
            queue_accounting_ns,
            scenario_begin_ns,
            scenario_finish_ns,
            note: "counter, byte-measurement, clone-integrity, and scenario lifecycle time are reported separately from clone samples",
        },
        synthetic_counter_probe: CounterProbeObservation {
            production_path_exercised: false,
            measurement_scope: "synthetic single workload in a dedicated process; concurrent production recorders would invalidate exact attribution",
            process_delta_consistency: "dedicated-process consistency check only; global before/after snapshots remain per-counter and are not one transaction",
            id: report.id,
            label: report.label,
            elapsed_ns: duration_ns(report.elapsed),
            queue_leak_free,
            scoped_sites: site_measurements(&report.scoped),
            process_delta_sites: delta_measurements(&report.global_delta),
        },
    })
}

fn build_fixture(spec: AnchorSpec, seed: u64) -> Result<Fixture, serde_json::Error> {
    let usable_input_tokens = spec.window_tokens.saturating_mul(INPUT_BUDGET_PERCENT) / 100;
    let target_input_tokens = usable_input_tokens.saturating_mul(TARGET_PRESSURE_PERCENT) / 100;
    let target_payload_bytes = target_input_tokens.saturating_mul(SYNTHETIC_BYTES_PER_TOKEN);
    let content_messages_per_turn = spec.rounds_per_turn.saturating_add(2);
    let content_message_count = spec.turns.saturating_mul(content_messages_per_turn).max(1);
    let base_payload = target_payload_bytes / content_message_count as u64;
    let remainder = target_payload_bytes % content_message_count as u64;
    let mut content_index = 0_u64;
    let mut random = DeterministicBytes::new(seed);
    let mut messages = Vec::with_capacity(
        spec.turns
            .saturating_mul(spec.rounds_per_turn.saturating_mul(2).saturating_add(2)),
    );

    for turn in 0..spec.turns {
        messages.push(FixtureMessage {
            role: "user",
            kind: "message",
            turn,
            round: None,
            correlation_id: None,
            content: random.payload(payload_len(base_payload, remainder, &mut content_index)),
        });
        for round in 0..spec.rounds_per_turn {
            let correlation_id = format!("call-{turn}-{round}");
            messages.push(FixtureMessage {
                role: "assistant",
                kind: "tool_call",
                turn,
                round: Some(round),
                correlation_id: Some(correlation_id.clone()),
                content: format!("{{\"turn\":{turn},\"round\":{round}}}"),
            });
            messages.push(FixtureMessage {
                role: "tool",
                kind: "tool_result",
                turn,
                round: Some(round),
                correlation_id: Some(correlation_id),
                content: random.payload(payload_len(base_payload, remainder, &mut content_index)),
            });
        }
        messages.push(FixtureMessage {
            role: "assistant",
            kind: "message",
            turn,
            round: None,
            correlation_id: None,
            content: random.payload(payload_len(base_payload, remainder, &mut content_index)),
        });
    }
    debug_assert_eq!(content_index, content_message_count as u64);
    let serialized = serde_json::to_vec(&messages)?;
    let sha256 = sha256_hex(&serialized);
    Ok(Fixture {
        messages,
        serialized,
        sha256,
        target_input_tokens,
        target_payload_bytes,
    })
}

fn payload_len(base: u64, remainder: u64, index: &mut u64) -> usize {
    let bytes = base.saturating_add(u64::from(*index < remainder));
    *index = index.saturating_add(1);
    usize::try_from(bytes).unwrap_or(usize::MAX)
}

struct DeterministicBytes {
    state: u64,
}

impl DeterministicBytes {
    fn new(seed: u64) -> Self {
        Self { state: seed.max(1) }
    }

    fn payload(&mut self, bytes: usize) -> String {
        const ALPHABET: &[u8] =
            b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-,:;. ";
        let mut output = String::with_capacity(bytes);
        for _ in 0..bytes {
            self.state ^= self.state << 13;
            self.state ^= self.state >> 7;
            self.state ^= self.state << 17;
            output.push(ALPHABET[self.state as usize % ALPHABET.len()] as char);
        }
        output
    }
}

fn timing_summary(mut samples: Vec<u64>) -> TimingSummary {
    samples.sort_unstable();
    let count = samples.len();
    let p95_index = count.saturating_mul(95).div_ceil(100).saturating_sub(1);
    TimingSummary {
        samples: count,
        min: samples[0],
        median: samples[count / 2],
        p95_nearest_rank: samples[p95_index],
        max: samples[count - 1],
    }
}

fn site_measurements(
    snapshot: &astra_core::history_work::HistoryWorkSnapshot,
) -> Vec<SiteMeasurement> {
    HistoryWorkSite::ALL
        .into_iter()
        .map(|site| {
            let measurement = snapshot.measurement(site);
            SiteMeasurement {
                site: site.as_str(),
                owner: site.owner(),
                primary_target_phase: site.primary_target_phase(),
                evidence_kind: measurement_evidence_kind(&measurement),
                measurement,
            }
        })
        .collect()
}

fn delta_measurements(delta: &HistoryWorkDelta) -> Vec<SiteDeltaMeasurement> {
    HistoryWorkSite::ALL
        .into_iter()
        .map(|site| {
            let measurement = delta.measurement(site);
            SiteDeltaMeasurement {
                site: site.as_str(),
                evidence_kind: delta_evidence_kind(&measurement),
                events: measurement.events,
                bytes: measurement.bytes,
                rows: measurement.rows,
                admission_units: measurement.admission_units,
                queue_current_bytes_change: measurement.queue_current_bytes_change,
                queue_peak_bytes_increase: measurement.queue_peak_bytes_increase,
                accounting_errors: measurement.accounting_errors,
            }
        })
        .collect()
}

fn measurement_evidence_kind(measurement: &HistoryWorkMeasurement) -> &'static str {
    if measurement.events != 0
        || measurement.bytes != 0
        || measurement.rows != 0
        || measurement.admission_units != 0
        || measurement.queue_current_bytes != 0
        || measurement.queue_peak_bytes != 0
        || measurement.accounting_errors != 0
    {
        "synthetic_manual_counter_call"
    } else {
        "not_exercised"
    }
}

fn delta_evidence_kind(
    measurement: &astra_core::history_work::HistoryWorkMeasurementDelta,
) -> &'static str {
    if measurement.events != 0
        || measurement.bytes != 0
        || measurement.rows != 0
        || measurement.admission_units != 0
        || measurement.queue_current_bytes_change != 0
        || measurement.queue_peak_bytes_increase != 0
        || measurement.accounting_errors != 0
    {
        "synthetic_manual_counter_call"
    } else {
        "not_exercised"
    }
}

fn validate_counter_probe_accounting(
    scoped: &astra_core::history_work::HistoryWorkSnapshot,
    process_delta: &HistoryWorkDelta,
) -> Result<(), &'static str> {
    let queue_leak = scoped
        .sites
        .iter()
        .any(|(_, measurement)| measurement.queue_current_bytes != 0)
        || process_delta
            .sites
            .iter()
            .any(|(_, measurement)| measurement.queue_current_bytes_change != 0);
    if queue_leak {
        return Err("queue accounting did not return to its starting state");
    }

    let accounting_error = scoped
        .sites
        .iter()
        .any(|(_, measurement)| measurement.accounting_errors != 0)
        || process_delta
            .sites
            .iter()
            .any(|(_, measurement)| measurement.accounting_errors != 0);
    if accounting_error {
        return Err("scoped or process-delta accounting_errors is nonzero");
    }
    Ok(())
}

fn coverage() -> Coverage {
    Coverage {
        coverage_complete: HISTORY_WORK_COVERAGE_COMPLETE,
        instrumented_site_count: HistoryWorkSite::ALL.len(),
        instrumented_sites: HistoryWorkSite::ALL
            .into_iter()
            .map(|site| SiteInventory {
                site: site.as_str(),
                owner: site.owner(),
                primary_target_phase: site.primary_target_phase(),
            })
            .collect(),
        omissions_are_exhaustive: HISTORY_WORK_COVERAGE_COMPLETE,
        known_omissions: HISTORY_WORK_KNOWN_OMISSIONS
            .iter()
            .map(|omission| Omission {
                key: omission.key,
                owner: omission.owner,
                target_phase: omission.target_phase,
                reason: omission.reason,
            })
            .collect(),
        scope_statement: "synthetic counter-plumbing probe in a dedicated process only; not a Phase-0 baseline and not a production-path, provider, database, cache, topology, admission, queue-backlog, or fairness benchmark; production concurrency is not exactly attributable",
    }
}

fn machine_metadata() -> MachineMetadata {
    MachineMetadata {
        os: std::env::consts::OS,
        os_release: os_release(),
        arch: std::env::consts::ARCH,
        kernel: command_output(repo_root(), "uname", &["-sr"]),
        cpu_model: read_first_prefixed_line("/proc/cpuinfo", "model name"),
        logical_cpu_count: std::thread::available_parallelism().ok().map(usize::from),
        memory_bytes: read_mem_total_bytes(),
    }
}

fn os_release() -> Option<String> {
    fs::read_to_string("/etc/os-release")
        .ok()?
        .lines()
        .find_map(|line| {
            let value = line.strip_prefix("PRETTY_NAME=")?;
            Some(value.trim_matches('"').to_owned())
        })
}

fn git_dirty() -> Option<bool> {
    command_output(repo_root(), "git", &["status", "--porcelain=v1"])
        .map(|output| !output.is_empty())
}

fn git_untracked_file_hashes() -> Result<Vec<SourceFileHash>, Box<dyn std::error::Error>> {
    let root = repo_root();
    let output = command_bytes(
        root.clone(),
        "git",
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )
    .ok_or("failed to enumerate git-untracked files")?;
    let mut files = Vec::new();
    for encoded_path in output
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let path = std::str::from_utf8(encoded_path)?;
        let relative = Path::new(path);
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err(format!("git returned an unsafe untracked path: {path:?}").into());
        }
        let absolute = root.join(relative);
        let metadata = fs::symlink_metadata(&absolute)?;
        let contents = if metadata.file_type().is_symlink() {
            fs::read_link(&absolute)?
                .as_os_str()
                .as_encoded_bytes()
                .to_vec()
        } else {
            fs::read(&absolute)?
        };
        files.push(SourceFileHash {
            path: path.to_owned(),
            sha256: sha256_hex(&contents),
        });
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

fn command_output(directory: PathBuf, program: &str, arguments: &[&str]) -> Option<String> {
    let output = command_bytes(directory, program, arguments)?;
    Some(String::from_utf8_lossy(&output).trim().to_owned())
}

fn command_bytes(directory: PathBuf, program: &str, arguments: &[&str]) -> Option<Vec<u8>> {
    let output = Command::new(program)
        .current_dir(directory)
        .args(arguments)
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read_first_prefixed_line(path: &str, prefix: &str) -> Option<String> {
    fs::read_to_string(path).ok()?.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        (key.trim() == prefix).then(|| value.trim().to_owned())
    })
}

fn read_mem_total_bytes() -> Option<u64> {
    let value = read_first_prefixed_line("/proc/meminfo", "MemTotal")?;
    let kib = value.split_whitespace().next()?.parse::<u64>().ok()?;
    kib.checked_mul(1024)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex_digest(digest.as_slice())
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

fn mix_seed(seed: u64, index: u64) -> u64 {
    seed ^ index.wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

fn elapsed_ns(started: Instant) -> u64 {
    duration_ns(started.elapsed())
}

fn duration_ns(duration: std::time::Duration) -> u64 {
    duration.as_nanos().try_into().unwrap_or(u64::MAX)
}

fn write_artifact(path: Option<&Path>, bytes: &[u8]) -> io::Result<()> {
    let Some(path) = path else {
        return io::stdout().lock().write_all(bytes);
    };
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    if let Some(parent) = parent {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!(
        "{}.tmp-{}",
        path.extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("json"),
        std::process::id()
    ));
    {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SMALL: AnchorSpec = AnchorSpec {
        name: "test",
        window_tokens: 1_000,
        turns: 2,
        rounds_per_turn: 2,
        declared_cache_scenario: "cold",
    };

    #[test]
    fn fixture_is_reproducible_but_seed_sensitive() {
        let first = build_fixture(SMALL, 17).unwrap();
        let second = build_fixture(SMALL, 17).unwrap();
        let different = build_fixture(SMALL, 18).unwrap();

        assert_eq!(first.sha256, second.sha256);
        assert_eq!(first.messages, second.messages);
        assert_ne!(first.sha256, different.sha256);
        assert_eq!(
            first
                .messages
                .iter()
                .filter(|message| message.kind != "tool_call")
                .map(|message| message.content.len())
                .sum::<usize>() as u64,
            first.target_payload_bytes,
        );
    }

    #[test]
    fn pinned_anchor_windows_cover_required_scales() {
        assert_eq!(
            ANCHORS.map(|anchor| anchor.window_tokens),
            [131_072, 204_800, 1_000_000]
        );
        assert_eq!(ANCHORS[2].turns, 100);
        assert_eq!(ANCHORS[2].rounds_per_turn, 10);
        assert_eq!(ANCHORS[0].rounds_per_turn, 1);
        assert!(coverage().coverage_complete);
        assert!(coverage().omissions_are_exhaustive);
        assert!(coverage().known_omissions.is_empty());
        assert!(
            coverage()
                .scope_statement
                .contains("not a Phase-0 baseline")
        );
    }

    #[test]
    fn timing_summary_uses_nearest_rank_p95() {
        let summary = timing_summary((1..=20).collect());
        assert_eq!(summary.min, 1);
        assert_eq!(summary.median, 11);
        assert_eq!(summary.p95_nearest_rank, 19);
        assert_eq!(summary.max, 20);
    }

    #[test]
    fn cli_rejects_too_few_samples() {
        let error = parse_options(["--samples".to_owned(), "2".to_owned()])
            .unwrap_err()
            .to_string();
        assert!(error.contains("3..=100"));
    }

    #[test]
    fn zero_measurements_are_not_claimed_as_exercised() {
        assert_eq!(
            measurement_evidence_kind(&HistoryWorkMeasurement::default()),
            "not_exercised"
        );
        assert_eq!(
            delta_evidence_kind(&astra_core::history_work::HistoryWorkMeasurementDelta::default()),
            "not_exercised"
        );

        let measured = HistoryWorkMeasurement {
            events: 1,
            ..HistoryWorkMeasurement::default()
        };
        assert_eq!(
            measurement_evidence_kind(&measured),
            "synthetic_manual_counter_call"
        );
        let measured_delta = astra_core::history_work::HistoryWorkMeasurementDelta {
            queue_current_bytes_change: -1,
            ..astra_core::history_work::HistoryWorkMeasurementDelta::default()
        };
        assert_eq!(
            delta_evidence_kind(&measured_delta),
            "synthetic_manual_counter_call"
        );
    }

    #[test]
    fn probe_rejects_queue_leaks_and_accounting_errors() {
        let blank_snapshot = || astra_core::history_work::HistoryWorkSnapshot {
            sites: HistoryWorkSite::ALL
                .into_iter()
                .map(|site| (site, HistoryWorkMeasurement::default()))
                .collect(),
        };
        let blank_delta = || HistoryWorkDelta {
            sites: HistoryWorkSite::ALL
                .into_iter()
                .map(|site| {
                    (
                        site,
                        astra_core::history_work::HistoryWorkMeasurementDelta::default(),
                    )
                })
                .collect(),
        };

        assert!(validate_counter_probe_accounting(&blank_snapshot(), &blank_delta()).is_ok());

        let mut leaked_scope = blank_snapshot();
        leaked_scope.sites[0].1.queue_current_bytes = 1;
        assert_eq!(
            validate_counter_probe_accounting(&leaked_scope, &blank_delta()).unwrap_err(),
            "queue accounting did not return to its starting state"
        );

        let mut leaked_process = blank_delta();
        leaked_process.sites[0].1.queue_current_bytes_change = -1;
        assert_eq!(
            validate_counter_probe_accounting(&blank_snapshot(), &leaked_process).unwrap_err(),
            "queue accounting did not return to its starting state"
        );

        let mut scoped_error = blank_snapshot();
        scoped_error.sites[0].1.accounting_errors = 1;
        assert_eq!(
            validate_counter_probe_accounting(&scoped_error, &blank_delta()).unwrap_err(),
            "scoped or process-delta accounting_errors is nonzero"
        );

        let mut process_error = blank_delta();
        process_error.sites[0].1.accounting_errors = 1;
        assert_eq!(
            validate_counter_probe_accounting(&blank_snapshot(), &process_error).unwrap_err(),
            "scoped or process-delta accounting_errors is nonzero"
        );
    }
}

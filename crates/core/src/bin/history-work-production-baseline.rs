//! Assemble and verify production-path Phase-0 baseline fragments.
//!
//! Workload companions are separate real CLI, Server, and Edge processes.
//! This binary cannot record history work. It only joins their process
//! counter deltas with correlated scenario facts and refuses to write an
//! artifact unless the complete typed contract verifies.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use astra_core::history_work::{HISTORY_WORK_COVERAGE_COMPLETE, HISTORY_WORK_KNOWN_OMISSIONS};
use astra_core::history_work_baseline::{
    BaselineProvenance, CoverageInventory, PRODUCTION_BASELINE_SCHEMA, ProductionBaselineArtifact,
    ProductionExecutableEvidence, ProductionProcessCapture, ProductionProcessRole,
    ProductionScenario, aggregate_process_site_totals, verify_current_build_attestation,
    write_json_atomic,
};
use sha2::{Digest, Sha256};

#[derive(Debug, PartialEq, Eq)]
struct Options {
    scenarios: Vec<PathBuf>,
    captures: Vec<PathBuf>,
    production_executables: BTreeMap<ProductionProcessRole, PathBuf>,
    output: PathBuf,
}

fn parse_options<I>(args: I) -> Result<Options, String>
where
    I: IntoIterator<Item = String>,
{
    let mut args = args.into_iter();
    let _program = args.next();
    let mut scenarios = Vec::new();
    let mut captures = Vec::new();
    let mut production_executables = BTreeMap::new();
    let mut output = None;
    while let Some(argument) = args.next() {
        let executable_role = match argument.as_str() {
            "--cli-executable" => Some(ProductionProcessRole::Cli),
            "--server-executable" => Some(ProductionProcessRole::Server),
            "--edge-executable" => Some(ProductionProcessRole::Edge),
            _ => None,
        };
        if let Some(role) = executable_role {
            let value = args
                .next()
                .ok_or_else(|| format!("{argument} requires a path"))?;
            if production_executables
                .insert(role, PathBuf::from(value))
                .is_some()
            {
                return Err(format!("{argument} may be supplied only once"));
            }
            continue;
        }
        let destination = match argument.as_str() {
            "--scenario" => &mut scenarios,
            "--capture" => &mut captures,
            "--output" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--output requires a path".to_string())?;
                if output.replace(PathBuf::from(value)).is_some() {
                    return Err("--output may be supplied only once".to_string());
                }
                continue;
            }
            "--help" | "-h" => {
                return Err("usage: history-work-production-baseline \
                     --scenario <facts.json>... --capture <process.json>... \
                     --cli-executable <path> --server-executable <path> \
                     --edge-executable <path> \
                     --output <artifact.json>"
                    .to_string());
            }
            _ => return Err(format!("unknown argument: {argument}")),
        };
        destination.push(PathBuf::from(
            args.next()
                .ok_or_else(|| format!("{argument} requires a path"))?,
        ));
    }
    if scenarios.is_empty() {
        return Err("at least one --scenario is required".to_string());
    }
    if captures.is_empty() {
        return Err("at least one --capture is required".to_string());
    }
    for role in ProductionProcessRole::ALL {
        if !production_executables.contains_key(&role) {
            return Err(format!(
                "a path for the {} executable is required",
                role.expected_executable_name()
            ));
        }
    }
    Ok(Options {
        scenarios,
        captures,
        production_executables,
        output: output.ok_or_else(|| "--output is required".to_string())?,
    })
}

fn run(options: Options) -> Result<(), Box<dyn std::error::Error>> {
    let checked_out_git_sha = command_text("git", &["rev-parse", "--verify", "HEAD^{commit}"])?;
    let scenarios = read_json_files::<ProductionScenario>(&options.scenarios)?;
    let captures = read_json_files::<ProductionProcessCapture>(&options.captures)?;
    let baseline_run_ids = scenarios
        .iter()
        .map(|scenario| scenario.baseline_run_id.as_str())
        .chain(
            captures
                .iter()
                .map(|capture| capture.baseline_run_id.as_str()),
        )
        .collect::<BTreeSet<_>>();
    if baseline_run_ids.len() != 1 {
        return Err("scenario and process fragments must share exactly one baseline run id".into());
    }
    let baseline_run_id = baseline_run_ids
        .first()
        .copied()
        .ok_or("production baseline has no run id")?;
    verify_current_build_attestation(&checked_out_git_sha, baseline_run_id)?;
    let site_totals = aggregate_process_site_totals(&captures)?;
    let artifact = ProductionBaselineArtifact {
        schema: PRODUCTION_BASELINE_SCHEMA.to_string(),
        provenance: collect_provenance(&options.production_executables)?,
        inventory: CoverageInventory {
            coverage_complete: HISTORY_WORK_COVERAGE_COMPLETE,
            omissions_are_exhaustive: HISTORY_WORK_COVERAGE_COMPLETE,
            known_omissions: HISTORY_WORK_KNOWN_OMISSIONS
                .iter()
                .map(|omission| omission.key.to_string())
                .collect(),
        },
        process_captures: captures,
        scenarios,
        site_totals,
    };
    artifact.verify()?;
    write_json_atomic(&options.output, &artifact)?;
    Ok(())
}

fn read_json_files<T>(paths: &[PathBuf]) -> Result<Vec<T>, Box<dyn std::error::Error>>
where
    T: serde::de::DeserializeOwned,
{
    paths
        .iter()
        .map(|path| -> Result<T, Box<dyn std::error::Error>> {
            let bytes = fs::read(path)?;
            Ok(serde_json::from_slice(&bytes)?)
        })
        .collect()
}

fn collect_provenance(
    production_executable_paths: &BTreeMap<ProductionProcessRole, PathBuf>,
) -> Result<BaselineProvenance, Box<dyn std::error::Error>> {
    let executable = std::env::current_exe()?;
    let git_sha = command_text("git", &["rev-parse", "HEAD"])?;
    let git_status = command_bytes("git", &["status", "--porcelain=v1", "-z"])?;
    let git_diff = command_bytes("git", &["diff", "--binary", "HEAD"])?;
    let untracked = command_bytes("git", &["ls-files", "--others", "--exclude-standard", "-z"])?;
    let mut untracked_file_sha256 = BTreeMap::new();
    for raw_path in untracked
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let path = PathBuf::from(String::from_utf8(raw_path.to_vec())?);
        if path.is_file() {
            untracked_file_sha256.insert(
                path.to_string_lossy().into_owned(),
                sha256_hex(&fs::read(path)?),
            );
        }
    }
    Ok(BaselineProvenance {
        git_sha,
        git_dirty: !git_status.is_empty(),
        git_diff_sha256: sha256_hex(&git_diff),
        untracked_file_sha256,
        executable_sha256: sha256_hex(&fs::read(executable)?),
        production_executables: production_executable_paths
            .iter()
            .map(|(&role, path)| production_executable_evidence(role, path))
            .collect::<Result<_, _>>()?,
        generated_at_unix_seconds: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        machine_id: machine_id(),
        rustc: command_text("rustc", &["-Vv"])?,
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        cpu_model: cpu_model(),
        logical_cpu_count: std::thread::available_parallelism()?.get(),
        memory_bytes: memory_bytes()?,
    })
}

fn production_executable_evidence(
    role: ProductionProcessRole,
    path: &Path,
) -> Result<ProductionExecutableEvidence, Box<dyn std::error::Error>> {
    let canonical = fs::canonicalize(path)?;
    if !canonical.is_file() {
        return Err(format!("production executable is not a file: {}", path.display()).into());
    }
    let file_name = canonical
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            format!(
                "production executable filename is not valid UTF-8: {}",
                canonical.display()
            )
        })?;
    let executable_name = file_name
        .strip_suffix(".exe")
        .unwrap_or(file_name)
        .to_string();
    if executable_name != role.expected_executable_name() {
        return Err(format!(
            "{role:?} executable path names {executable_name}, expected {}",
            role.expected_executable_name()
        )
        .into());
    }
    Ok(ProductionExecutableEvidence {
        role,
        executable_name,
        executable_sha256: sha256_hex(&fs::read(canonical)?),
    })
}

fn command_text(program: &str, args: &[&str]) -> Result<String, Box<dyn std::error::Error>> {
    let bytes = command_bytes(program, args)?;
    Ok(String::from_utf8(bytes)?.trim().to_string())
}

fn command_bytes(program: &str, args: &[&str]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let output = Command::new(program).args(args).output()?;
    if !output.status.success() {
        return Err(format!("{program} {args:?} failed with {}", output.status).into());
    }
    Ok(output.stdout)
}

fn machine_id() -> String {
    fs::read("/etc/machine-id")
        .map(|bytes| sha256_hex(&bytes))
        .unwrap_or_else(|_| {
            sha256_hex(format!("{}:{}", std::env::consts::OS, std::env::consts::ARCH).as_bytes())
        })
}

fn cpu_model() -> String {
    fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                let (key, value) = line.split_once(':')?;
                (key.trim() == "model name").then(|| value.trim().to_string())
            })
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn memory_bytes() -> Result<u64, Box<dyn std::error::Error>> {
    let meminfo = fs::read_to_string("/proc/meminfo")?;
    let kib = meminfo
        .lines()
        .find_map(|line| {
            let suffix = line.strip_prefix("MemTotal:")?;
            suffix.split_whitespace().next()?.parse::<u64>().ok()
        })
        .ok_or("MemTotal is absent from /proc/meminfo")?;
    kib.checked_mul(1024)
        .ok_or_else(|| "MemTotal byte conversion overflowed".into())
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn main() {
    let result: Result<(), Box<dyn std::error::Error>> = match parse_options(std::env::args()) {
        Ok(options) => run(options),
        Err(error) => Err(error.into()),
    };
    if let Err(error) = result {
        eprintln!("production baseline assembly failed: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_core::history_work::HistoryWorkSite;
    use astra_core::history_work_baseline::{
        PROCESS_CAPTURE_SCHEMA, ProcessSiteDelta, ProductionCaptureScope, ProductionProcessCapture,
        ProductionTopology, WindowClass, production_process_capture_id,
    };

    fn capture(role: ProductionProcessRole) -> ProductionProcessCapture {
        let scope = match role {
            ProductionProcessRole::Cli => {
                ProductionCaptureScope::cold(ProductionTopology::CliServer, WindowClass::K128)
            }
            ProductionProcessRole::Server => ProductionCaptureScope::Setup,
            ProductionProcessRole::Edge => {
                ProductionCaptureScope::service(ProductionTopology::EdgeServer, WindowClass::K128)
            }
        };
        let baseline_run_id = "b".repeat(64);
        ProductionProcessCapture {
            schema: PROCESS_CAPTURE_SCHEMA.to_string(),
            capture_id: production_process_capture_id(&baseline_run_id, role, scope),
            baseline_run_id,
            scope,
            git_sha: "c".repeat(40),
            build_git_dirty: false,
            role,
            executable_name: role.expected_executable_name().to_string(),
            executable_sha256: "a".repeat(64),
            pid: role as u32 + 1,
            started_at_unix_seconds: 1,
            finished_at_unix_seconds: 2,
            sites: HistoryWorkSite::ALL
                .into_iter()
                .enumerate()
                .map(|(index, site)| ProcessSiteDelta {
                    site: site.as_str().to_string(),
                    owner: site.owner().to_string(),
                    target_phase: site.primary_target_phase(),
                    events: u64::from(index == 0),
                    bytes: u64::from(index == 0) * 2,
                    rows: u64::from(index == 0) * 3,
                    admission_units: u64::from(index == 0) * 4,
                    queue_current_bytes_change: 0,
                    queue_peak_bytes_increase: u64::from(index == 0) * 5,
                    accounting_errors: 0,
                })
                .collect(),
        }
    }

    #[test]
    fn parser_requires_scenarios_captures_and_output() {
        let options = parse_options([
            "baseline".to_string(),
            "--scenario".to_string(),
            "scenario.json".to_string(),
            "--capture".to_string(),
            "capture.json".to_string(),
            "--cli-executable".to_string(),
            "/tmp/astra".to_string(),
            "--server-executable".to_string(),
            "/tmp/astra-server".to_string(),
            "--edge-executable".to_string(),
            "/tmp/astra-edge".to_string(),
            "--output".to_string(),
            "artifact.json".to_string(),
        ])
        .unwrap();
        assert_eq!(options.scenarios, vec![PathBuf::from("scenario.json")]);
        assert_eq!(options.captures, vec![PathBuf::from("capture.json")]);
        assert_eq!(
            options
                .production_executables
                .get(&ProductionProcessRole::Server),
            Some(&PathBuf::from("/tmp/astra-server"))
        );
        assert_eq!(options.output, PathBuf::from("artifact.json"));
    }

    #[test]
    fn parser_requires_every_typed_production_executable() {
        assert!(
            parse_options([
                "baseline".to_string(),
                "--scenario".to_string(),
                "scenario.json".to_string(),
                "--capture".to_string(),
                "capture.json".to_string(),
                "--cli-executable".to_string(),
                "/tmp/astra".to_string(),
                "--server-executable".to_string(),
                "/tmp/astra-server".to_string(),
                "--output".to_string(),
                "artifact.json".to_string(),
            ])
            .is_err()
        );
    }

    #[test]
    fn aggregation_preserves_honest_zero_sites_and_uses_max_process_peak() {
        let captures = vec![
            capture(ProductionProcessRole::Cli),
            capture(ProductionProcessRole::Server),
            capture(ProductionProcessRole::Edge),
        ];
        let totals = aggregate_process_site_totals(&captures).unwrap();
        assert_eq!(totals.len(), HistoryWorkSite::ALL.len());
        assert_eq!(totals.iter().filter(|total| total.events > 0).count(), 1);
        let exercised = totals.iter().find(|total| total.events > 0).unwrap();
        assert_eq!(exercised.events, 3);
        assert_eq!(exercised.bytes, 6);
        assert_eq!(exercised.queue_peak_bytes, 5);
        assert!(totals.iter().any(|total| total.events == 0));
    }

    #[test]
    fn aggregation_rejects_counter_overflow() {
        let mut first = capture(ProductionProcessRole::Cli);
        let mut second = capture(ProductionProcessRole::Server);
        first.sites[0].events = u64::MAX;
        second.sites[0].events = 1;
        let error = aggregate_process_site_totals(&[first, second]).unwrap_err();
        assert!(
            error
                .violations
                .iter()
                .any(|violation| violation.contains("aggregate overflow"))
        );
    }
}

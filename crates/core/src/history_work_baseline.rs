//! Fail-closed contract for the Phase-0 production history-work baseline.
//!
//! This module deliberately does not execute workloads and has no API for
//! incrementing history-work counters. Production companions must collect
//! facts from real CLI, Server, and Edge entrypoints and then ask this module
//! to validate the resulting artifact.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::history_work::{HistoryWorkSite, HistoryWorkSnapshot, instrumentation_enabled};

pub const PRODUCTION_BASELINE_SCHEMA: &str = "astra.history_work.production_baseline.v4";
pub const PROCESS_CAPTURE_SCHEMA: &str = "astra.history_work.production_process_capture.v3";
const PROCESS_CAPTURE_PATH_ENV: &str = "ASTRA_HISTORY_WORK_BASELINE_FRAGMENT";
pub const BASELINE_RUN_ID_ENV: &str = "ASTRA_HISTORY_WORK_BASELINE_RUN_ID";
pub const BASELINE_GIT_SHA_ENV: &str = "ASTRA_HISTORY_WORK_BASELINE_GIT_SHA";
pub const BASELINE_CAPTURE_SCOPE_ENV: &str = "ASTRA_HISTORY_WORK_BASELINE_CAPTURE_SCOPE";
pub const BUILD_GIT_SHA: &str = env!("ASTRA_BUILD_GIT_SHA");
pub const BUILD_GIT_DIRTY: &str = env!("ASTRA_BUILD_GIT_DIRTY");
pub const BUILD_ATTESTATION_NONCE: &str = env!("ASTRA_BUILD_ATTESTATION_NONCE");
const MAX_BASELINE_RUN_SECONDS: u64 = 24 * 60 * 60;
const ATOMIC_WRITE_CREATE_ATTEMPTS: usize = 64;
static ATOMIC_WRITE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const MAX_CAPTURE_ASSEMBLY_DELAY_SECONDS: u64 = 60 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProductionTopology {
    CliServer,
    ServerOnly,
    EdgeServer,
}

impl ProductionTopology {
    pub const ALL: [Self; 3] = [Self::CliServer, Self::ServerOnly, Self::EdgeServer];

    const fn required_entrypoints(self) -> &'static [ProductionEntrypoint] {
        match self {
            Self::CliServer => &[
                ProductionEntrypoint::CliStreamTurn,
                ProductionEntrypoint::ServerChatStream,
            ],
            Self::ServerOnly => &[ProductionEntrypoint::ServerChatStream],
            Self::EdgeServer => &[
                ProductionEntrypoint::EdgeWebSocket,
                ProductionEntrypoint::ServerChatStream,
            ],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProductionEntrypoint {
    CliStreamTurn,
    ServerChatStream,
    EdgeWebSocket,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProductionProcessRole {
    Cli,
    Server,
    Edge,
}

impl ProductionProcessRole {
    pub const ALL: [Self; 3] = [Self::Cli, Self::Server, Self::Edge];

    pub const fn expected_executable_name(self) -> &'static str {
        match self {
            Self::Cli => "astra",
            Self::Server => "astra-server",
            Self::Edge => "astra-edge",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProductionCapturePhase {
    Service,
    Cold,
    WarmEligible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProductionCaptureScope {
    Setup,
    Scenario {
        topology: ProductionTopology,
        window_class: WindowClass,
        phase: ProductionCapturePhase,
    },
}

impl ProductionCaptureScope {
    pub const fn service(topology: ProductionTopology, window_class: WindowClass) -> Self {
        Self::Scenario {
            topology,
            window_class,
            phase: ProductionCapturePhase::Service,
        }
    }

    pub const fn cold(topology: ProductionTopology, window_class: WindowClass) -> Self {
        Self::Scenario {
            topology,
            window_class,
            phase: ProductionCapturePhase::Cold,
        }
    }

    pub const fn warm_eligible(topology: ProductionTopology, window_class: WindowClass) -> Self {
        Self::Scenario {
            topology,
            window_class,
            phase: ProductionCapturePhase::WarmEligible,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProductionProcessCapture {
    pub schema: String,
    pub baseline_run_id: String,
    pub capture_id: String,
    pub scope: ProductionCaptureScope,
    pub git_sha: String,
    pub build_git_dirty: bool,
    pub role: ProductionProcessRole,
    pub executable_name: String,
    pub executable_sha256: String,
    pub pid: u32,
    pub started_at_unix_seconds: u64,
    pub finished_at_unix_seconds: u64,
    pub sites: Vec<ProcessSiteDelta>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessSiteDelta {
    pub site: String,
    pub owner: String,
    pub target_phase: u8,
    pub events: u64,
    pub bytes: u64,
    pub rows: u64,
    pub admission_units: u64,
    pub queue_current_bytes_change: i128,
    pub queue_peak_bytes_increase: u64,
    pub accounting_errors: u64,
}

/// Optional process-lifetime capture used only by the production baseline
/// orchestrator.
///
/// The guard reads one explicit output path from the environment, snapshots
/// the real process counters, and writes their delta on [`Self::finish`]. It
/// cannot increment or synthesize counters.
#[derive(Debug)]
pub struct ProductionProcessCaptureGuard {
    baseline_run_id: String,
    git_sha: String,
    role: ProductionProcessRole,
    scope: ProductionCaptureScope,
    path: PathBuf,
    executable_name: String,
    executable_sha256: String,
    started_at_unix_seconds: u64,
    before: HistoryWorkSnapshot,
}

impl ProductionProcessCaptureGuard {
    pub fn from_env(role: ProductionProcessRole) -> io::Result<Option<Self>> {
        let Some(path) = std::env::var_os(PROCESS_CAPTURE_PATH_ENV) else {
            return Ok(None);
        };
        if !instrumentation_enabled() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{PROCESS_CAPTURE_PATH_ENV} requires ASTRA_HISTORY_WORK_TRACE=1 before process startup"
                ),
            ));
        }
        let baseline_run_id = std::env::var(BASELINE_RUN_ID_ENV).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{PROCESS_CAPTURE_PATH_ENV} requires {BASELINE_RUN_ID_ENV}"),
            )
        })?;
        validate_baseline_run_id(&baseline_run_id)?;
        let configured_git_sha = std::env::var(BASELINE_GIT_SHA_ENV).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{PROCESS_CAPTURE_PATH_ENV} requires {BASELINE_GIT_SHA_ENV}"),
            )
        })?;
        let git_sha = verify_current_build_attestation(&configured_git_sha, &baseline_run_id)?;
        let scope = std::env::var(BASELINE_CAPTURE_SCOPE_ENV)
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{PROCESS_CAPTURE_PATH_ENV} requires {BASELINE_CAPTURE_SCOPE_ENV}"),
                )
            })
            .and_then(|raw| {
                serde_json::from_str::<ProductionCaptureScope>(&raw).map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("invalid {BASELINE_CAPTURE_SCOPE_ENV}: {error}"),
                    )
                })
            })?;
        if !expected_capture_slots().contains(&(scope, role)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{BASELINE_CAPTURE_SCOPE_ENV} does not permit process role {role:?}"),
            ));
        }
        let path = PathBuf::from(path);
        if path.as_os_str().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{PROCESS_CAPTURE_PATH_ENV} cannot be empty"),
            ));
        }
        let Some(parent) = path.parent() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{PROCESS_CAPTURE_PATH_ENV} must include a parent directory"),
            ));
        };
        if !parent.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "baseline fragment parent does not exist: {}",
                    parent.display()
                ),
            ));
        }
        let (executable_name, executable_sha256) = current_executable_identity()?;
        Ok(Some(Self {
            baseline_run_id,
            git_sha,
            role,
            scope,
            path,
            executable_name,
            executable_sha256,
            started_at_unix_seconds: unix_seconds()?,
            before: HistoryWorkSnapshot::capture(),
        }))
    }

    pub fn finish(self) -> io::Result<ProductionProcessCapture> {
        let after = HistoryWorkSnapshot::capture();
        let delta = after.delta_since(&self.before);
        let capture = ProductionProcessCapture {
            schema: PROCESS_CAPTURE_SCHEMA.to_string(),
            capture_id: production_process_capture_id(&self.baseline_run_id, self.role, self.scope),
            baseline_run_id: self.baseline_run_id,
            scope: self.scope,
            git_sha: self.git_sha,
            build_git_dirty: false,
            role: self.role,
            executable_name: self.executable_name,
            executable_sha256: self.executable_sha256,
            pid: std::process::id(),
            started_at_unix_seconds: self.started_at_unix_seconds,
            finished_at_unix_seconds: unix_seconds()?,
            sites: HistoryWorkSite::ALL
                .into_iter()
                .map(|site| {
                    let measurement = delta.measurement(site);
                    ProcessSiteDelta {
                        site: site.as_str().to_string(),
                        owner: site.owner().to_string(),
                        target_phase: site.primary_target_phase(),
                        events: measurement.events,
                        bytes: measurement.bytes,
                        rows: measurement.rows,
                        admission_units: measurement.admission_units,
                        queue_current_bytes_change: measurement.queue_current_bytes_change,
                        queue_peak_bytes_increase: measurement.queue_peak_bytes_increase,
                        accounting_errors: measurement.accounting_errors,
                    }
                })
                .collect(),
        };
        write_json_atomic(&self.path, &capture)?;
        Ok(capture)
    }
}

struct AtomicWriteStage {
    path: PathBuf,
    file: Option<fs::File>,
    published: bool,
}

impl AtomicWriteStage {
    fn create(target: &Path, parent: &Path) -> io::Result<Self> {
        let file_name = target.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("atomic output has no filename: {}", target.display()),
            )
        })?;
        for _ in 0..ATOMIC_WRITE_CREATE_ATTEMPTS {
            let sequence = ATOMIC_WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let mut temporary_name = OsString::from(".");
            temporary_name.push(file_name);
            temporary_name.push(format!(".tmp-{}-{sequence}", std::process::id()));
            let path = parent.join(temporary_name);
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(file) => {
                    return Ok(Self {
                        path,
                        file: Some(file),
                        published: false,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "could not allocate a unique atomic output beside {}",
                target.display()
            ),
        ))
    }

    fn write_and_publish(mut self, target: &Path, parent: &Path, bytes: &[u8]) -> io::Result<()> {
        let file = self
            .file
            .as_mut()
            .expect("atomic stage owns its file until publish");
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(self.file.take());
        fs::rename(&self.path, target)?;
        self.published = true;
        sync_parent_directory(parent)
    }
}

impl Drop for AtomicWriteStage {
    fn drop(&mut self) {
        drop(self.file.take());
        if !self.published {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(unix)]
fn sync_parent_directory(parent: &Path) -> io::Result<()> {
    fs::File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &Path) -> io::Result<()> {
    Ok(())
}

/// Serialize and durably publish JSON without exposing a partial target.
///
/// The staged file lives beside the target so the final rename stays on one
/// filesystem. Every failure before publication closes and removes the stage.
pub fn write_json_atomic<T: Serialize + ?Sized>(path: &Path, value: &T) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("atomic output has no parent: {}", path.display()),
        )
    })?;
    if !parent.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("atomic output parent does not exist: {}", parent.display()),
        ));
    }
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    AtomicWriteStage::create(path, parent)?.write_and_publish(path, parent, &bytes)
}

fn unix_seconds() -> io::Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn validate_baseline_run_id(value: &str) -> io::Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{BASELINE_RUN_ID_ENV} must be a 64-character hexadecimal identifier"),
        ));
    }
    Ok(())
}

fn validate_git_sha(value: &str) -> io::Result<()> {
    if !is_full_git_sha(value) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{BASELINE_GIT_SHA_ENV} must be a full hexadecimal Git object id"),
        ));
    }
    Ok(())
}

fn verified_build_git_sha(configured: &str, built: &str) -> io::Result<String> {
    validate_git_sha(configured)?;
    if !is_full_git_sha(built) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "production baseline capture requires a binary with an embedded full Git commit SHA",
        ));
    }
    if !configured.eq_ignore_ascii_case(built) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{BASELINE_GIT_SHA_ENV} does not match the Git commit embedded in this binary"),
        ));
    }
    Ok(built.to_ascii_lowercase())
}

fn verify_clean_build_attestation(built_dirty: &str) -> io::Result<()> {
    if built_dirty != "false" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "production baseline capture requires a binary built from a clean worktree",
        ));
    }
    Ok(())
}

/// Bind the running binary to one expected clean source revision.
///
/// All production baseline entrypoints call this before accepting or emitting
/// evidence. A stale, dirty, or unversioned build therefore fails closed even
/// when the caller supplies a plausible Git SHA.
pub fn verify_current_build_attestation(
    expected_sha: &str,
    expected_run_id: &str,
) -> io::Result<String> {
    verify_build_attestation(
        expected_sha,
        expected_run_id,
        BUILD_GIT_SHA,
        BUILD_GIT_DIRTY,
        BUILD_ATTESTATION_NONCE,
    )
}

fn verify_build_attestation(
    expected_sha: &str,
    expected_run_id: &str,
    built_sha: &str,
    built_dirty: &str,
    built_nonce: &str,
) -> io::Result<String> {
    let git_sha = verified_build_git_sha(expected_sha, built_sha)?;
    verify_clean_build_attestation(built_dirty)?;
    validate_baseline_run_id(expected_run_id)?;
    validate_baseline_run_id(built_nonce).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "binary was not built with a valid production baseline attestation nonce",
        )
    })?;
    if built_nonce != expected_run_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "production baseline run id does not match the nonce embedded in this binary",
        ));
    }
    Ok(git_sha)
}

fn is_full_git_sha(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn current_executable_identity() -> io::Result<(String, String)> {
    let executable = std::env::current_exe()?;
    let file_name = executable
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "current executable name is not valid UTF-8",
            )
        })?;
    let executable_name = file_name
        .strip_suffix(".exe")
        .unwrap_or(file_name)
        .to_string();
    let mut file = fs::File::open(executable)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok((executable_name, format!("{:x}", hasher.finalize())))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowClass {
    K128,
    K200,
    M1,
}

impl WindowClass {
    pub const ALL: [Self; 3] = [Self::K128, Self::K200, Self::M1];

    const fn accepts(self, tokens: u64) -> bool {
        match self {
            Self::K128 => tokens >= 120_000 && tokens <= 140_000,
            Self::K200 => tokens >= 190_000 && tokens <= 220_000,
            Self::M1 => tokens >= 900_000 && tokens <= 1_100_000,
        }
    }
}

fn expected_capture_slots() -> BTreeSet<(ProductionCaptureScope, ProductionProcessRole)> {
    let mut slots =
        BTreeSet::from([(ProductionCaptureScope::Setup, ProductionProcessRole::Server)]);
    for topology in ProductionTopology::ALL {
        for window_class in WindowClass::ALL {
            slots.insert((
                ProductionCaptureScope::service(topology, window_class),
                ProductionProcessRole::Server,
            ));
            match topology {
                ProductionTopology::CliServer => {
                    slots.insert((
                        ProductionCaptureScope::cold(topology, window_class),
                        ProductionProcessRole::Cli,
                    ));
                    slots.insert((
                        ProductionCaptureScope::warm_eligible(topology, window_class),
                        ProductionProcessRole::Cli,
                    ));
                }
                ProductionTopology::ServerOnly => {}
                ProductionTopology::EdgeServer => {
                    slots.insert((
                        ProductionCaptureScope::service(topology, window_class),
                        ProductionProcessRole::Edge,
                    ));
                }
            }
        }
    }
    slots
}

fn expected_scenario_capture_slots(
    topology: ProductionTopology,
    window_class: WindowClass,
) -> BTreeSet<(ProductionCaptureScope, ProductionProcessRole)> {
    let mut slots = BTreeSet::from([(
        ProductionCaptureScope::service(topology, window_class),
        ProductionProcessRole::Server,
    )]);
    match topology {
        ProductionTopology::CliServer => {
            slots.insert((
                ProductionCaptureScope::cold(topology, window_class),
                ProductionProcessRole::Cli,
            ));
            slots.insert((
                ProductionCaptureScope::warm_eligible(topology, window_class),
                ProductionProcessRole::Cli,
            ));
        }
        ProductionTopology::ServerOnly => {}
        ProductionTopology::EdgeServer => {
            slots.insert((
                ProductionCaptureScope::service(topology, window_class),
                ProductionProcessRole::Edge,
            ));
        }
    }
    slots
}

/// Stable, domain-separated identity for one process capture slot.
///
/// The ID is derived inside the production process rather than supplied by the
/// orchestrator, so a capture cannot silently change roles or workload scope
/// while retaining its identity.
pub fn production_process_capture_id(
    baseline_run_id: &str,
    role: ProductionProcessRole,
    scope: ProductionCaptureScope,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"astra.history_work.production_process_capture.id.v1\0");
    hasher.update(baseline_run_id.as_bytes());
    hasher.update([0]);
    hasher.update(match role {
        ProductionProcessRole::Cli => b"cli".as_slice(),
        ProductionProcessRole::Server => b"server".as_slice(),
        ProductionProcessRole::Edge => b"edge".as_slice(),
    });
    hasher.update([0]);
    match scope {
        ProductionCaptureScope::Setup => hasher.update(b"setup"),
        ProductionCaptureScope::Scenario {
            topology,
            window_class,
            phase,
        } => {
            hasher.update(b"scenario\0");
            hasher.update(match topology {
                ProductionTopology::CliServer => b"cli_server".as_slice(),
                ProductionTopology::ServerOnly => b"server_only".as_slice(),
                ProductionTopology::EdgeServer => b"edge_server".as_slice(),
            });
            hasher.update([0]);
            hasher.update(match window_class {
                WindowClass::K128 => b"k128".as_slice(),
                WindowClass::K200 => b"k200".as_slice(),
                WindowClass::M1 => b"m1".as_slice(),
            });
            hasher.update([0]);
            hasher.update(match phase {
                ProductionCapturePhase::Service => b"service".as_slice(),
                ProductionCapturePhase::Cold => b"cold".as_slice(),
                ProductionCapturePhase::WarmEligible => b"warm_eligible".as_slice(),
            });
        }
    }
    format!("{:x}", hasher.finalize())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProductionBaselineArtifact {
    pub schema: String,
    pub provenance: BaselineProvenance,
    pub inventory: CoverageInventory,
    pub process_captures: Vec<ProductionProcessCapture>,
    pub scenarios: Vec<ProductionScenario>,
    pub site_totals: Vec<SiteTotal>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaselineProvenance {
    pub git_sha: String,
    pub git_dirty: bool,
    pub git_diff_sha256: String,
    pub untracked_file_sha256: BTreeMap<String, String>,
    pub executable_sha256: String,
    pub production_executables: Vec<ProductionExecutableEvidence>,
    pub generated_at_unix_seconds: u64,
    pub machine_id: String,
    pub rustc: String,
    pub os: String,
    pub arch: String,
    pub cpu_model: String,
    pub logical_cpu_count: usize,
    pub memory_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProductionExecutableEvidence {
    pub role: ProductionProcessRole,
    pub executable_name: String,
    pub executable_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverageInventory {
    pub coverage_complete: bool,
    pub omissions_are_exhaustive: bool,
    pub known_omissions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteTotal {
    pub site: String,
    pub owner: String,
    pub target_phase: u8,
    pub events: u64,
    pub bytes: u64,
    pub rows: u64,
    pub admission_units: u64,
    pub queue_peak_bytes: u64,
    pub accounting_errors: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProductionScenario {
    pub baseline_run_id: String,
    pub topology: ProductionTopology,
    pub window_class: WindowClass,
    pub capture_refs: Vec<ProcessCaptureRef>,
    pub model: ModelOfferingEvidence,
    pub entrypoints: BTreeSet<ProductionEntrypoint>,
    pub correlation: CorrelationEvidence,
    pub work: ScenarioWorkEvidence,
    pub provider_usage: ProviderUsageEvidence,
    pub cache: CacheEvidence,
    pub projection: ProjectionEvidence,
    pub compaction: CompactionEvidence,
    pub estimator: EstimatorEvidence,
    pub fairness: FairnessEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ProcessCaptureRef {
    pub capture_id: String,
    pub capture_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelOfferingEvidence {
    pub offering_id: String,
    pub resolved_model_name: String,
    pub context_window_tokens: u64,
    pub metadata_source: ModelMetadataSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelMetadataSource {
    DatabaseOffering,
}

/// Closed authority for joining one measured turn to its physical provider
/// attempts.
///
/// `/chat/turn` bridge executions have stable transport correlation while the
/// inference ledger remains session-scoped. They are deliberately not durable
/// agent runs. Keeping that authority distinct prevents baseline collectors
/// from fabricating `agent_runs` ownership or treating durable run replay as a
/// bridge capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "authority", rename_all = "snake_case", deny_unknown_fields)]
pub enum CorrelationEvidence {
    DurableRun {
        owner_id: String,
        session_id: String,
        run_id: String,
        turn: u32,
        provider_attempts: Vec<ProviderAttemptEvidence>,
    },
    CliSessionBridge {
        owner_id: String,
        session_id: String,
        cli_execution_id: String,
        session_turn: u32,
        turn_chain_id: String,
        user_query_event_id: String,
        exchange_count: u32,
        provider_attempts: Vec<ProviderAttemptEvidence>,
    },
}

impl CorrelationEvidence {
    fn owner_id(&self) -> &str {
        match self {
            Self::DurableRun { owner_id, .. } | Self::CliSessionBridge { owner_id, .. } => owner_id,
        }
    }

    fn run_identity(&self) -> &str {
        match self {
            Self::DurableRun { run_id, .. } => run_id,
            Self::CliSessionBridge { turn_chain_id, .. } => turn_chain_id,
        }
    }

    fn provider_attempts(&self) -> &[ProviderAttemptEvidence] {
        match self {
            Self::DurableRun {
                provider_attempts, ..
            }
            | Self::CliSessionBridge {
                provider_attempts, ..
            } => provider_attempts,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderAttemptEvidence {
    pub request_id: String,
    pub round: u32,
    pub logical_attempt: u32,
    pub attempt: u32,
    pub operation_id: String,
    pub wire_request_sha256: String,
    pub wire_request_bytes: u64,
    pub terminal_status: AttemptTerminalStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptTerminalStatus {
    Succeeded,
    Failed,
    DeliveryUnknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScenarioWorkEvidence {
    pub history_events: u64,
    pub clone_hash_serialization_bytes: u64,
    pub db_rows: u64,
    pub admission_units: u64,
    pub queue_peak_bytes: u64,
    pub queue_current_bytes_change: i128,
    pub accounting_errors: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderUsageEvidence {
    pub source: ProviderUsageSource,
    pub requests: u64,
    pub fresh_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub normalized_input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderUsageSource {
    ProviderResponse,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheEvidence {
    /// Successful provider responses from the first, explicitly cold
    /// production run.
    pub cold_path_requests: u64,
    /// Successful provider responses from the repeated stable-prefix run.
    ///
    /// This names the exercised path, not an inferred provider outcome. Some
    /// providers legitimately report zero cache reads even for a repeated
    /// prefix.
    pub warm_eligible_path_requests: u64,
    /// Successful responses whose typed provider usage reported cache reads.
    pub observed_cache_read_requests: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub total_input_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "authority", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProjectionEvidence {
    DurableRun {
        durable_event_index: u64,
        projected_event_index: u64,
        lag_events: u64,
    },
    /// A session-scoped CLI bridge has no `agent_runs` projection or replay.
    CliSessionBridgeNotApplicable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionEvidence {
    pub attempts: u64,
    pub effective_attempts: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub tokens_freed: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EstimatorEvidence {
    pub estimated_input_tokens: u64,
    pub canonical_provider_input_tokens: u64,
    pub absolute_error_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FairnessEvidence {
    pub tenants: Vec<TenantAdmissionEvidence>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenantAdmissionEvidence {
    pub owner_id: String,
    pub admission_units: u64,
    pub wait_micros: u64,
    pub completed_requests: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaselineVerificationError {
    pub violations: Vec<String>,
}

impl std::fmt::Display for BaselineVerificationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "production history-work baseline is incomplete: {}",
            self.violations.join("; ")
        )
    }
}

impl std::error::Error for BaselineVerificationError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScenarioCaptureEvidence {
    pub capture_refs: Vec<ProcessCaptureRef>,
    pub work: ScenarioWorkEvidence,
}

/// Hash the typed capture value using its deterministic serde field order.
///
/// The digest intentionally covers the parsed value rather than file
/// whitespace, so assemblers and standalone verifiers agree on one identity.
pub fn production_process_capture_sha256(
    capture: &ProductionProcessCapture,
) -> Result<String, BaselineVerificationError> {
    serde_json::to_vec(capture)
        .map(|bytes| format!("{:x}", Sha256::digest(bytes)))
        .map_err(|error| BaselineVerificationError {
            violations: vec![format!(
                "process capture {} cannot be canonically serialized: {error}",
                capture.capture_id
            )],
        })
}

pub fn production_process_capture_ref(
    capture: &ProductionProcessCapture,
) -> Result<ProcessCaptureRef, BaselineVerificationError> {
    Ok(ProcessCaptureRef {
        capture_id: capture.capture_id.clone(),
        capture_sha256: production_process_capture_sha256(capture)?,
    })
}

/// Build the only accepted capture references and work aggregate for one
/// external production scenario.
///
/// This is shared by the scenario companion and the artifact verifier. It
/// rejects setup captures, captures from another topology/window, incomplete
/// role/phase sets, duplicate slots, and counter overflow.
pub fn scenario_capture_evidence(
    baseline_run_id: &str,
    topology: ProductionTopology,
    window_class: WindowClass,
    captures: &[ProductionProcessCapture],
) -> Result<ScenarioCaptureEvidence, BaselineVerificationError> {
    let expected_slots = expected_scenario_capture_slots(topology, window_class);
    let mut actual_slots = BTreeSet::new();
    let mut capture_ids = BTreeSet::new();
    let mut capture_refs = Vec::with_capacity(captures.len());
    let mut violations = Vec::new();
    for capture in captures {
        require(
            capture.baseline_run_id == baseline_run_id,
            &format!(
                "scenario {topology:?}/{window_class:?} references capture {} from another baseline run",
                capture.capture_id
            ),
            &mut violations,
        );
        require(
            capture.schema == PROCESS_CAPTURE_SCHEMA,
            &format!(
                "scenario {topology:?}/{window_class:?} references capture {} with the wrong schema",
                capture.capture_id
            ),
            &mut violations,
        );
        require(
            capture.capture_id
                == production_process_capture_id(
                    &capture.baseline_run_id,
                    capture.role,
                    capture.scope,
                ),
            &format!(
                "scenario {topology:?}/{window_class:?} references capture with an invalid derived ID"
            ),
            &mut violations,
        );
        require(
            capture_ids.insert(capture.capture_id.as_str()),
            &format!(
                "scenario {topology:?}/{window_class:?} contains duplicate capture ID {}",
                capture.capture_id
            ),
            &mut violations,
        );
        let slot = (capture.scope, capture.role);
        require(
            actual_slots.insert(slot),
            &format!(
                "scenario {topology:?}/{window_class:?} contains duplicate capture slot {:?}/{:?}",
                capture.scope, capture.role
            ),
            &mut violations,
        );
        match production_process_capture_ref(capture) {
            Ok(capture_ref) => capture_refs.push(capture_ref),
            Err(error) => violations.extend(error.violations),
        }
    }
    for slot in expected_slots.difference(&actual_slots) {
        violations.push(format!(
            "scenario {topology:?}/{window_class:?} is missing capture slot {:?}/{:?}",
            slot.0, slot.1
        ));
    }
    for slot in actual_slots.difference(&expected_slots) {
        violations.push(format!(
            "scenario {topology:?}/{window_class:?} contains unexpected capture slot {:?}/{:?}",
            slot.0, slot.1
        ));
    }
    capture_refs.sort();
    let work = match aggregate_scenario_work(captures) {
        Ok(work) => Some(work),
        Err(error) => {
            violations.extend(error.violations);
            None
        }
    };
    match (violations.is_empty(), work) {
        (true, Some(work)) => Ok(ScenarioCaptureEvidence { capture_refs, work }),
        _ => Err(BaselineVerificationError { violations }),
    }
}

pub fn aggregate_scenario_work(
    captures: &[ProductionProcessCapture],
) -> Result<ScenarioWorkEvidence, BaselineVerificationError> {
    let mut work = ScenarioWorkEvidence {
        history_events: 0,
        clone_hash_serialization_bytes: 0,
        db_rows: 0,
        admission_units: 0,
        queue_peak_bytes: 0,
        queue_current_bytes_change: 0,
        accounting_errors: 0,
    };
    let mut violations = Vec::new();
    for capture in captures {
        for site in &capture.sites {
            checked_add_measurement(
                &mut work.history_events,
                site.events,
                &site.site,
                "events",
                &mut violations,
            );
            checked_add_measurement(
                &mut work.clone_hash_serialization_bytes,
                site.bytes,
                &site.site,
                "bytes",
                &mut violations,
            );
            checked_add_measurement(
                &mut work.db_rows,
                site.rows,
                &site.site,
                "rows",
                &mut violations,
            );
            checked_add_measurement(
                &mut work.admission_units,
                site.admission_units,
                &site.site,
                "admission_units",
                &mut violations,
            );
            match work
                .queue_current_bytes_change
                .checked_add(site.queue_current_bytes_change)
            {
                Some(value) => work.queue_current_bytes_change = value,
                None => violations.push(format!(
                    "aggregate overflow at {}.queue_current_bytes_change",
                    site.site
                )),
            }
            work.queue_peak_bytes = work.queue_peak_bytes.max(site.queue_peak_bytes_increase);
            checked_add_measurement(
                &mut work.accounting_errors,
                site.accounting_errors,
                &site.site,
                "accounting_errors",
                &mut violations,
            );
        }
    }
    if violations.is_empty() {
        Ok(work)
    } else {
        Err(BaselineVerificationError { violations })
    }
}

/// Recompute the canonical aggregate from process-owned counter deltas.
///
/// Keeping this in the contract module lets both the assembler and the
/// standalone verifier use the same overflow-checked implementation.
pub fn aggregate_process_site_totals(
    captures: &[ProductionProcessCapture],
) -> Result<Vec<SiteTotal>, BaselineVerificationError> {
    let mut violations = Vec::new();
    let mut totals = HistoryWorkSite::ALL
        .into_iter()
        .map(|site| {
            (
                site.as_str().to_string(),
                SiteTotal {
                    site: site.as_str().to_string(),
                    owner: site.owner().to_string(),
                    target_phase: site.primary_target_phase(),
                    events: 0,
                    bytes: 0,
                    rows: 0,
                    admission_units: 0,
                    queue_peak_bytes: 0,
                    accounting_errors: 0,
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    for capture in captures {
        let mut seen = BTreeSet::new();
        for delta in &capture.sites {
            if !seen.insert(delta.site.as_str()) {
                violations.push(format!(
                    "duplicate site {} in process capture {}",
                    delta.site, capture.pid
                ));
                continue;
            }
            let Some(total) = totals.get_mut(&delta.site) else {
                violations.push(format!(
                    "unknown site {} in process capture {}",
                    delta.site, capture.pid
                ));
                continue;
            };
            if delta.owner != total.owner || delta.target_phase != total.target_phase {
                violations.push(format!("site inventory drift in capture: {}", delta.site));
                continue;
            }
            checked_add_measurement(
                &mut total.events,
                delta.events,
                &delta.site,
                "events",
                &mut violations,
            );
            checked_add_measurement(
                &mut total.bytes,
                delta.bytes,
                &delta.site,
                "bytes",
                &mut violations,
            );
            checked_add_measurement(
                &mut total.rows,
                delta.rows,
                &delta.site,
                "rows",
                &mut violations,
            );
            checked_add_measurement(
                &mut total.admission_units,
                delta.admission_units,
                &delta.site,
                "admission_units",
                &mut violations,
            );
            total.queue_peak_bytes = total.queue_peak_bytes.max(delta.queue_peak_bytes_increase);
            checked_add_measurement(
                &mut total.accounting_errors,
                delta.accounting_errors,
                &delta.site,
                "accounting_errors",
                &mut violations,
            );
        }
        require(
            seen.len() == HistoryWorkSite::ALL.len(),
            &format!(
                "process capture {} has {} sites, expected {}",
                capture.pid,
                seen.len(),
                HistoryWorkSite::ALL.len()
            ),
            &mut violations,
        );
    }
    if violations.is_empty() {
        Ok(totals.into_values().collect())
    } else {
        Err(BaselineVerificationError { violations })
    }
}

fn checked_add_measurement(
    total: &mut u64,
    delta: u64,
    site: &str,
    field: &str,
    violations: &mut Vec<String>,
) {
    match total.checked_add(delta) {
        Some(value) => *total = value,
        None => violations.push(format!("aggregate overflow at {site}.{field}")),
    }
}

impl ProductionBaselineArtifact {
    pub fn verify(&self) -> Result<(), BaselineVerificationError> {
        let mut violations = Vec::new();
        require(
            self.schema == PRODUCTION_BASELINE_SCHEMA,
            "schema mismatch",
            &mut violations,
        );
        verify_provenance(&self.provenance, &mut violations);
        require(
            self.inventory.coverage_complete,
            "coverage_complete is false",
            &mut violations,
        );
        require(
            self.inventory.omissions_are_exhaustive,
            "omissions_are_exhaustive is false",
            &mut violations,
        );
        require(
            self.inventory.known_omissions.is_empty(),
            "known omissions remain",
            &mut violations,
        );
        verify_process_captures(&self.process_captures, &self.provenance, &mut violations);
        for capture in &self.process_captures {
            require(
                capture.finished_at_unix_seconds <= self.provenance.generated_at_unix_seconds,
                &format!(
                    "process capture {} finishes after artifact generation",
                    capture.pid
                ),
                &mut violations,
            );
        }
        verify_capture_time_window(
            &self.process_captures,
            self.provenance.generated_at_unix_seconds,
            &mut violations,
        );

        let mut combinations = BTreeSet::new();
        let mut scenario_run_ids = BTreeSet::new();
        let mut scenario_request_ids = BTreeSet::new();
        let mut offerings_by_window = BTreeMap::new();
        let mut windows_by_offering = BTreeMap::new();
        let mut captures_by_id = BTreeMap::new();
        for capture in &self.process_captures {
            captures_by_id
                .entry(capture.capture_id.as_str())
                .or_insert(capture);
        }
        let mut scenario_referenced_capture_ids = BTreeSet::new();
        let mut baseline_run_ids = self
            .process_captures
            .iter()
            .map(|capture| capture.baseline_run_id.as_str())
            .collect::<BTreeSet<_>>();
        for scenario in &self.scenarios {
            baseline_run_ids.insert(scenario.baseline_run_id.as_str());
            combinations.insert((scenario.topology, scenario.window_class));
            require(
                scenario_run_ids.insert(scenario.correlation.run_identity()),
                "duplicate scenario correlation run identity",
                &mut violations,
            );
            for attempt in scenario.correlation.provider_attempts() {
                require(
                    scenario_request_ids.insert(attempt.request_id.as_str()),
                    "duplicate scenario provider attempt request_id",
                    &mut violations,
                );
            }
            if let Some(expected) = offerings_by_window.get(&scenario.window_class) {
                require(
                    *expected == &scenario.model,
                    &format!(
                        "window {:?} does not use one identical database offering across topologies",
                        scenario.window_class
                    ),
                    &mut violations,
                );
            } else {
                offerings_by_window.insert(scenario.window_class, &scenario.model);
            }
            if let Some(expected_window) = windows_by_offering
                .insert(scenario.model.offering_id.as_str(), scenario.window_class)
            {
                require(
                    expected_window == scenario.window_class,
                    "one offering_id is assigned to multiple window classes",
                    &mut violations,
                );
            }
            verify_scenario(scenario, &mut violations);
            verify_scenario_capture_binding(
                scenario,
                &captures_by_id,
                &mut scenario_referenced_capture_ids,
                &mut violations,
            );
        }
        require(
            baseline_run_ids.len() == 1,
            "scenario and process artifacts do not share one baseline run id",
            &mut violations,
        );
        for topology in ProductionTopology::ALL {
            for window in WindowClass::ALL {
                require(
                    combinations.contains(&(topology, window)),
                    &format!("missing scenario {topology:?}/{window:?}"),
                    &mut violations,
                );
            }
        }
        require(
            combinations.len() == self.scenarios.len(),
            "duplicate topology/window scenario",
            &mut violations,
        );
        require(
            self.scenarios.len() == ProductionTopology::ALL.len() * WindowClass::ALL.len(),
            "production baseline must contain exactly nine scenarios",
            &mut violations,
        );
        for capture in &self.process_captures {
            match capture.scope {
                ProductionCaptureScope::Setup => require(
                    !scenario_referenced_capture_ids.contains(capture.capture_id.as_str()),
                    "setup capture must not be referenced by a production scenario",
                    &mut violations,
                ),
                ProductionCaptureScope::Scenario { .. } => require(
                    scenario_referenced_capture_ids.contains(capture.capture_id.as_str()),
                    &format!(
                        "production scenario capture {} is not referenced exactly once",
                        capture.capture_id
                    ),
                    &mut violations,
                ),
            }
        }

        let totals = self
            .site_totals
            .iter()
            .map(|total| (total.site.as_str(), total))
            .collect::<BTreeMap<_, _>>();
        require(
            totals.len() == self.site_totals.len(),
            "duplicate site total",
            &mut violations,
        );
        require(
            totals.len() == HistoryWorkSite::ALL.len(),
            "site totals do not exactly match the typed inventory",
            &mut violations,
        );
        for total in &self.site_totals {
            require(
                HistoryWorkSite::ALL
                    .into_iter()
                    .any(|site| site.as_str() == total.site),
                &format!("unknown site total {}", total.site),
                &mut violations,
            );
        }
        match aggregate_process_site_totals(&self.process_captures) {
            Ok(recomputed) => {
                let recomputed = recomputed
                    .iter()
                    .map(|total| (total.site.as_str(), total))
                    .collect::<BTreeMap<_, _>>();
                for (site, declared) in &totals {
                    if let Some(expected) = recomputed.get(site) {
                        require(
                            *declared == *expected,
                            &format!("site total {site} does not match process captures"),
                            &mut violations,
                        );
                    }
                }
            }
            Err(error) => violations.extend(error.violations),
        }
        let mut exercised_sites = 0_usize;
        for site in HistoryWorkSite::ALL {
            let Some(total) = totals.get(site.as_str()) else {
                violations.push(format!("missing site total {}", site.as_str()));
                continue;
            };
            require(
                total.owner == site.owner(),
                &format!("owner mismatch for {}", site.as_str()),
                &mut violations,
            );
            require(
                total.target_phase == site.primary_target_phase(),
                &format!("target phase mismatch for {}", site.as_str()),
                &mut violations,
            );
            if total.events > 0 {
                exercised_sites = exercised_sites.saturating_add(1);
            } else {
                require(
                    total.bytes == 0
                        && total.rows == 0
                        && total.admission_units == 0
                        && total.queue_peak_bytes == 0,
                    &format!(
                        "unexercised site {} contains non-zero measurements",
                        site.as_str()
                    ),
                    &mut violations,
                );
            }
            require(
                total.accounting_errors == 0,
                &format!("accounting error at {}", site.as_str()),
                &mut violations,
            );
        }
        require(
            exercised_sites > 0,
            "no production history-work site was exercised",
            &mut violations,
        );

        if violations.is_empty() {
            Ok(())
        } else {
            Err(BaselineVerificationError { violations })
        }
    }
}

fn verify_process_captures(
    captures: &[ProductionProcessCapture],
    provenance: &BaselineProvenance,
    violations: &mut Vec<String>,
) {
    let inventory = HistoryWorkSite::ALL
        .into_iter()
        .map(|site| (site.as_str(), site))
        .collect::<BTreeMap<_, _>>();
    let expected_executables = provenance
        .production_executables
        .iter()
        .map(|executable| (executable.role, executable))
        .collect::<BTreeMap<_, _>>();
    let mut roles = BTreeSet::new();
    let mut capture_identities = BTreeSet::new();
    let mut capture_contents = BTreeSet::new();
    let mut capture_ids = BTreeSet::new();
    let mut capture_slots = BTreeSet::new();
    for capture in captures {
        match production_process_capture_sha256(capture) {
            Ok(digest) => {
                require(
                    capture_contents.insert(digest),
                    &format!("duplicate process capture content for pid {}", capture.pid),
                    violations,
                );
            }
            Err(error) => violations.extend(error.violations),
        }
        require(
            capture.schema == PROCESS_CAPTURE_SCHEMA,
            &format!("process capture schema mismatch for pid {}", capture.pid),
            violations,
        );
        require_hexadecimal_64(
            "process baseline_run_id",
            &capture.baseline_run_id,
            violations,
        );
        require_sha256("process capture_id", &capture.capture_id, violations);
        require(
            capture.capture_id
                == production_process_capture_id(
                    &capture.baseline_run_id,
                    capture.role,
                    capture.scope,
                ),
            &format!(
                "process capture {} has an invalid derived capture_id",
                capture.pid
            ),
            violations,
        );
        require(
            capture_ids.insert(capture.capture_id.as_str()),
            &format!("duplicate process capture_id {}", capture.capture_id),
            violations,
        );
        require(
            capture_slots.insert((capture.scope, capture.role)),
            &format!(
                "duplicate production capture slot {:?}/{:?}",
                capture.scope, capture.role
            ),
            violations,
        );
        require(
            capture.git_sha == provenance.git_sha,
            &format!(
                "process capture {} git_sha does not match artifact provenance",
                capture.pid
            ),
            violations,
        );
        require(
            !capture.build_git_dirty,
            &format!(
                "process capture {} was produced by a dirty build",
                capture.pid
            ),
            violations,
        );
        roles.insert(capture.role);
        require(
            capture_identities.insert((
                capture.role,
                capture.pid,
                capture.started_at_unix_seconds,
                capture.finished_at_unix_seconds,
            )),
            &format!("duplicate process capture identity for pid {}", capture.pid),
            violations,
        );
        require(
            capture.executable_name == capture.role.expected_executable_name(),
            &format!(
                "process role {:?} was captured by executable {}",
                capture.role, capture.executable_name
            ),
            violations,
        );
        require_sha256(
            "process executable_sha256",
            &capture.executable_sha256,
            violations,
        );
        if let Some(expected) = expected_executables.get(&capture.role) {
            require(
                capture.executable_name == expected.executable_name
                    && capture.executable_sha256 == expected.executable_sha256,
                &format!(
                    "process capture {} does not match the verified {:?} executable",
                    capture.pid, capture.role
                ),
                violations,
            );
        }
        require(capture.pid > 0, "process capture pid is zero", violations);
        require(
            capture.started_at_unix_seconds > 0
                && capture.finished_at_unix_seconds >= capture.started_at_unix_seconds,
            &format!(
                "process capture timestamps are invalid for pid {}",
                capture.pid
            ),
            violations,
        );
        let mut seen = BTreeSet::new();
        for delta in &capture.sites {
            require(
                seen.insert(delta.site.as_str()),
                &format!(
                    "duplicate site {} in process capture {}",
                    delta.site, capture.pid
                ),
                violations,
            );
            let Some(site) = inventory.get(delta.site.as_str()) else {
                violations.push(format!(
                    "unknown site {} in process capture {}",
                    delta.site, capture.pid
                ));
                continue;
            };
            require(
                delta.owner == site.owner(),
                &format!("process capture owner mismatch for {}", delta.site),
                violations,
            );
            require(
                delta.target_phase == site.primary_target_phase(),
                &format!("process capture target phase mismatch for {}", delta.site),
                violations,
            );
            require(
                delta.queue_current_bytes_change == 0,
                &format!("process capture leaked queue bytes at {}", delta.site),
                violations,
            );
            require(
                delta.accounting_errors == 0,
                &format!("process capture accounting error at {}", delta.site),
                violations,
            );
            if delta.events == 0 {
                require(
                    delta.bytes == 0
                        && delta.rows == 0
                        && delta.admission_units == 0
                        && delta.queue_peak_bytes_increase == 0,
                    &format!(
                        "process capture has measurements without events at {}",
                        delta.site
                    ),
                    violations,
                );
            }
        }
        require(
            seen.len() == inventory.len(),
            &format!(
                "process capture {} has {} sites, expected {}",
                capture.pid,
                seen.len(),
                inventory.len()
            ),
            violations,
        );
    }
    for role in ProductionProcessRole::ALL {
        require(
            roles.contains(&role),
            &format!("missing production process role: {role:?}"),
            violations,
        );
    }
    let expected_slots = expected_capture_slots();
    for slot in expected_slots.difference(&capture_slots) {
        violations.push(format!(
            "missing production capture slot {:?}/{:?}",
            slot.0, slot.1
        ));
    }
    for slot in capture_slots.difference(&expected_slots) {
        violations.push(format!(
            "unexpected production capture slot {:?}/{:?}",
            slot.0, slot.1
        ));
    }
    require(
        captures.len() == expected_slots.len(),
        &format!(
            "production baseline must contain exactly {} process captures",
            expected_slots.len()
        ),
        violations,
    );
}

fn verify_capture_time_window(
    captures: &[ProductionProcessCapture],
    generated_at_unix_seconds: u64,
    violations: &mut Vec<String>,
) {
    let Some(started_at) = captures
        .iter()
        .map(|capture| capture.started_at_unix_seconds)
        .min()
    else {
        return;
    };
    let finished_at = captures
        .iter()
        .map(|capture| capture.finished_at_unix_seconds)
        .max()
        .unwrap_or(started_at);
    require(
        finished_at
            .checked_sub(started_at)
            .is_some_and(|elapsed| elapsed <= MAX_BASELINE_RUN_SECONDS),
        "process captures span more than one production baseline run window",
        violations,
    );
    require(
        generated_at_unix_seconds
            .checked_sub(finished_at)
            .is_some_and(|delay| delay <= MAX_CAPTURE_ASSEMBLY_DELAY_SECONDS),
        "process captures are stale relative to artifact generation",
        violations,
    );
}

fn verify_provenance(provenance: &BaselineProvenance, violations: &mut Vec<String>) {
    require(
        is_full_git_sha(&provenance.git_sha),
        "git_sha is not a full hexadecimal object id",
        violations,
    );
    require(!provenance.git_dirty, "git worktree is dirty", violations);
    require_sha256("git_diff_sha256", &provenance.git_diff_sha256, violations);
    require(
        provenance.git_diff_sha256 == format!("{:x}", Sha256::digest([])),
        "clean worktree provenance has a non-empty diff digest",
        violations,
    );
    require(
        provenance.untracked_file_sha256.is_empty(),
        "git worktree contains untracked files",
        violations,
    );
    require_sha256(
        "executable_sha256",
        &provenance.executable_sha256,
        violations,
    );
    let mut executable_roles = BTreeSet::new();
    for executable in &provenance.production_executables {
        require(
            executable_roles.insert(executable.role),
            &format!(
                "duplicate production executable evidence for {:?}",
                executable.role
            ),
            violations,
        );
        require(
            executable.executable_name == executable.role.expected_executable_name(),
            &format!(
                "production executable name mismatch for {:?}",
                executable.role
            ),
            violations,
        );
        require_sha256(
            "production executable_sha256",
            &executable.executable_sha256,
            violations,
        );
    }
    for role in ProductionProcessRole::ALL {
        require(
            executable_roles.contains(&role),
            &format!("missing production executable evidence for {role:?}"),
            violations,
        );
    }
    require(
        provenance.production_executables.len() == ProductionProcessRole::ALL.len(),
        "production executable evidence does not exactly match the typed role inventory",
        violations,
    );
    require_nonempty("machine_id", &provenance.machine_id, violations);
    require_nonempty("rustc", &provenance.rustc, violations);
    require_nonempty("os", &provenance.os, violations);
    require_nonempty("arch", &provenance.arch, violations);
    require_nonempty("cpu_model", &provenance.cpu_model, violations);
    require(
        provenance.logical_cpu_count > 0,
        "logical_cpu_count is zero",
        violations,
    );
    require(
        provenance.memory_bytes > 0,
        "memory_bytes is zero",
        violations,
    );
    require(
        provenance.generated_at_unix_seconds > 0,
        "generated_at_unix_seconds is zero",
        violations,
    );
}

fn verify_scenario(scenario: &ProductionScenario, violations: &mut Vec<String>) {
    require_hexadecimal_64(
        "scenario baseline_run_id",
        &scenario.baseline_run_id,
        violations,
    );
    require(
        scenario
            .window_class
            .accepts(scenario.model.context_window_tokens),
        "offering context window does not match its window class",
        violations,
    );
    require_nonempty("offering_id", &scenario.model.offering_id, violations);
    require_nonempty(
        "resolved_model_name",
        &scenario.model.resolved_model_name,
        violations,
    );
    let expected_entrypoints = scenario
        .topology
        .required_entrypoints()
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    require(
        scenario.entrypoints == expected_entrypoints,
        &format!(
            "scenario {:?}/{:?} entrypoints do not exactly match its topology",
            scenario.topology, scenario.window_class
        ),
        violations,
    );
    verify_correlation(&scenario.correlation, violations);
    match scenario.topology {
        ProductionTopology::CliServer => {
            require(
                matches!(
                    &scenario.correlation,
                    CorrelationEvidence::CliSessionBridge { .. }
                ),
                "CLI topology must use session-scoped bridge correlation authority",
                violations,
            );
            require(
                matches!(
                    &scenario.projection,
                    ProjectionEvidence::CliSessionBridgeNotApplicable
                ),
                "CLI topology must mark durable replay/projection not applicable",
                violations,
            );
        }
        ProductionTopology::ServerOnly | ProductionTopology::EdgeServer => {
            require(
                matches!(
                    &scenario.correlation,
                    CorrelationEvidence::DurableRun { .. }
                ),
                "Server/Edge topology must use durable-run correlation authority",
                violations,
            );
            require(
                matches!(&scenario.projection, ProjectionEvidence::DurableRun { .. }),
                "Server/Edge topology must provide durable projection evidence",
                violations,
            );
        }
    }
    require(
        scenario.work.history_events > 0,
        "scenario has no production history-work events",
        violations,
    );
    require(
        scenario.work.clone_hash_serialization_bytes > 0,
        "scenario has no measured clone/hash/serialization bytes",
        violations,
    );
    require(
        scenario.work.db_rows > 0,
        "scenario has no measured database rows",
        violations,
    );
    require(
        scenario.work.admission_units > 0,
        "scenario has no measured admission units",
        violations,
    );
    require(
        scenario.work.queue_peak_bytes > 0,
        "scenario has no measured queue peak",
        violations,
    );
    require(
        scenario.work.queue_current_bytes_change == 0,
        "scenario leaked queue-held bytes",
        violations,
    );
    require(
        scenario.work.accounting_errors == 0,
        "scenario contains accounting errors",
        violations,
    );
    verify_provider_usage(&scenario.provider_usage, violations);
    verify_cache(&scenario.cache, &scenario.provider_usage, violations);
    verify_projection(&scenario.projection, violations);
    verify_compaction(scenario.topology, &scenario.compaction, violations);
    verify_estimator(&scenario.estimator, violations);
    verify_fairness(
        &scenario.fairness,
        &scenario.correlation,
        &scenario.work,
        violations,
    );
}

fn verify_scenario_capture_binding(
    scenario: &ProductionScenario,
    captures_by_id: &BTreeMap<&str, &ProductionProcessCapture>,
    globally_referenced_capture_ids: &mut BTreeSet<String>,
    violations: &mut Vec<String>,
) {
    let mut declared_refs = BTreeSet::new();
    let mut resolved = Vec::with_capacity(scenario.capture_refs.len());
    let mut sorted_refs = scenario.capture_refs.clone();
    sorted_refs.sort();
    require(
        scenario.capture_refs == sorted_refs,
        &format!(
            "scenario {:?}/{:?} capture references are not in canonical order",
            scenario.topology, scenario.window_class
        ),
        violations,
    );
    for capture_ref in &scenario.capture_refs {
        require_sha256("scenario capture_id", &capture_ref.capture_id, violations);
        require_sha256(
            "scenario capture_sha256",
            &capture_ref.capture_sha256,
            violations,
        );
        require(
            declared_refs.insert(capture_ref),
            &format!(
                "scenario {:?}/{:?} contains a duplicate capture reference",
                scenario.topology, scenario.window_class
            ),
            violations,
        );
        require(
            globally_referenced_capture_ids.insert(capture_ref.capture_id.clone()),
            &format!(
                "process capture {} is referenced by more than one scenario",
                capture_ref.capture_id
            ),
            violations,
        );
        let Some(capture) = captures_by_id.get(capture_ref.capture_id.as_str()) else {
            violations.push(format!(
                "scenario {:?}/{:?} references unknown process capture {}",
                scenario.topology, scenario.window_class, capture_ref.capture_id
            ));
            continue;
        };
        match production_process_capture_sha256(capture) {
            Ok(actual_digest) => require(
                actual_digest == capture_ref.capture_sha256,
                &format!(
                    "scenario {:?}/{:?} process capture {} digest mismatch",
                    scenario.topology, scenario.window_class, capture_ref.capture_id
                ),
                violations,
            ),
            Err(error) => violations.extend(error.violations),
        }
        resolved.push((*capture).clone());
    }

    match scenario_capture_evidence(
        &scenario.baseline_run_id,
        scenario.topology,
        scenario.window_class,
        &resolved,
    ) {
        Ok(recomputed) => {
            require(
                scenario.capture_refs == recomputed.capture_refs,
                &format!(
                    "scenario {:?}/{:?} capture references do not match its exact process matrix",
                    scenario.topology, scenario.window_class
                ),
                violations,
            );
            require(
                scenario.work == recomputed.work,
                &format!(
                    "scenario {:?}/{:?} work does not equal its referenced process captures",
                    scenario.topology, scenario.window_class
                ),
                violations,
            );
        }
        Err(error) => violations.extend(error.violations),
    }
}

fn verify_correlation(correlation: &CorrelationEvidence, violations: &mut Vec<String>) {
    let (owner_id, session_id, turn, attempts) = match correlation {
        CorrelationEvidence::DurableRun {
            owner_id,
            session_id,
            run_id,
            turn,
            provider_attempts,
        } => {
            require_nonempty("durable run_id", run_id, violations);
            (owner_id, session_id, *turn, provider_attempts)
        }
        CorrelationEvidence::CliSessionBridge {
            owner_id,
            session_id,
            cli_execution_id,
            session_turn,
            turn_chain_id,
            user_query_event_id,
            exchange_count,
            provider_attempts,
        } => {
            require_nonempty("CLI execution_id", cli_execution_id, violations);
            require_nonempty("bridge turn_chain_id", turn_chain_id, violations);
            require(
                cli_execution_id == turn_chain_id,
                "CLI execution_id differs from turn_chain_id",
                violations,
            );
            require_nonempty(
                "bridge user_query_event_id",
                user_query_event_id,
                violations,
            );
            require(
                *exchange_count > 0,
                "CLI bridge exchange_count is zero",
                violations,
            );
            (owner_id, session_id, *session_turn, provider_attempts)
        }
    };
    require_nonempty("owner_id", owner_id, violations);
    require_nonempty("session_id", session_id, violations);
    require(
        !attempts.is_empty(),
        "scenario has no physical provider attempts",
        violations,
    );
    require(turn > 0, "scenario turn is zero", violations);
    let mut identities = BTreeSet::new();
    let mut request_ids = BTreeSet::new();
    let mut succeeded = 0_u64;
    for attempt in attempts {
        require_nonempty(
            "provider attempt request_id",
            &attempt.request_id,
            violations,
        );
        require(
            request_ids.insert(attempt.request_id.as_str()),
            "duplicate provider attempt request_id",
            violations,
        );
        require_nonempty(
            "provider attempt operation_id",
            &attempt.operation_id,
            violations,
        );
        require(
            identities.insert((
                attempt.round,
                attempt.logical_attempt,
                attempt.attempt,
                attempt.operation_id.as_str(),
            )),
            "duplicate round/logical-attempt/physical-attempt identity",
            violations,
        );
        require_sha256(
            "wire_request_sha256",
            &attempt.wire_request_sha256,
            violations,
        );
        require(
            attempt.wire_request_bytes > 0,
            "wire request bytes are zero",
            violations,
        );
        if attempt.terminal_status == AttemptTerminalStatus::Succeeded {
            succeeded = succeeded.saturating_add(1);
        }
    }
    require(
        succeeded > 0,
        "scenario has no succeeded physical provider attempt",
        violations,
    );
}

fn verify_provider_usage(usage: &ProviderUsageEvidence, violations: &mut Vec<String>) {
    let Some(expected) = usage
        .fresh_input_tokens
        .checked_add(usage.cache_read_input_tokens)
        .and_then(|value| value.checked_add(usage.cache_creation_input_tokens))
    else {
        violations.push("provider usage input sum overflowed".to_string());
        return;
    };
    require(
        usage.requests > 0,
        "provider request count is zero",
        violations,
    );
    require(
        usage.normalized_input_tokens > 0,
        "normalized provider input is zero",
        violations,
    );
    require(
        usage.normalized_input_tokens == expected,
        "normalized provider input is not fresh + read + create",
        violations,
    );
    require(
        usage.output_tokens > 0,
        "provider output tokens are unmeasured",
        violations,
    );
}

fn verify_cache(
    cache: &CacheEvidence,
    usage: &ProviderUsageEvidence,
    violations: &mut Vec<String>,
) {
    require(
        cache.cold_path_requests > 0 && cache.warm_eligible_path_requests > 0,
        "cold and warm-eligible cache paths were not both exercised",
        violations,
    );
    require(
        cache
            .cold_path_requests
            .checked_add(cache.warm_eligible_path_requests)
            .is_some_and(|requests| requests == usage.requests),
        "cold + warm-eligible request count does not match provider usage",
        violations,
    );
    require(
        cache.observed_cache_read_requests <= usage.requests,
        "observed cache-read request count exceeds provider usage",
        violations,
    );
    require(
        (cache.observed_cache_read_requests == 0) == (cache.cache_read_input_tokens == 0),
        "observed cache-read request count disagrees with cache-read tokens",
        violations,
    );
    require(
        cache.cache_read_input_tokens == usage.cache_read_input_tokens
            && cache.cache_creation_input_tokens == usage.cache_creation_input_tokens,
        "cache token evidence does not match provider response usage",
        violations,
    );
    require(
        cache.total_input_tokens == usage.normalized_input_tokens,
        "cache-share denominator does not match normalized provider input",
        violations,
    );
}

fn verify_projection(projection: &ProjectionEvidence, violations: &mut Vec<String>) {
    let ProjectionEvidence::DurableRun {
        durable_event_index,
        projected_event_index,
        lag_events,
    } = projection
    else {
        return;
    };
    require(
        *durable_event_index > 0,
        "durable projection cursor is zero",
        violations,
    );
    require(
        *projected_event_index > 0,
        "projected cursor is zero",
        violations,
    );
    require(
        projected_event_index <= durable_event_index,
        "projection index exceeds durable event index",
        violations,
    );
    require(
        *lag_events == durable_event_index.saturating_sub(*projected_event_index),
        "projection lag is inconsistent with durable/projected indices",
        violations,
    );
}

fn verify_compaction(
    topology: ProductionTopology,
    compaction: &CompactionEvidence,
    violations: &mut Vec<String>,
) {
    if compaction.attempts == 0 {
        // Phase 0 preserves the current CLI-owned migration adapter, including
        // its measured lossy continuation behavior. Durable Server loops must
        // still prove quiet-path compaction under the seeded history pressure.
        require(
            topology == ProductionTopology::CliServer,
            "durable topology did not exercise compaction under baseline history pressure",
            violations,
        );
        require(
            compaction.effective_attempts == 0
                && compaction.input_tokens == 0
                && compaction.output_tokens == 0
                && compaction.tokens_freed == 0,
            "zero-attempt compaction evidence contains invented work",
            violations,
        );
        return;
    }
    require(
        compaction.effective_attempts > 0 && compaction.effective_attempts <= compaction.attempts,
        "effective compaction count is invalid",
        violations,
    );
    require(
        compaction.output_tokens <= compaction.input_tokens,
        "compaction output exceeds input",
        violations,
    );
    require(
        compaction.tokens_freed
            == compaction
                .input_tokens
                .saturating_sub(compaction.output_tokens),
        "compaction tokens_freed is inconsistent",
        violations,
    );
    require(
        compaction.tokens_freed > 0,
        "effective compaction freed no tokens",
        violations,
    );
}

fn verify_estimator(estimator: &EstimatorEvidence, violations: &mut Vec<String>) {
    require(
        estimator.canonical_provider_input_tokens > 0,
        "canonical provider input is unmeasured",
        violations,
    );
    require(
        estimator.absolute_error_tokens
            == estimator
                .estimated_input_tokens
                .abs_diff(estimator.canonical_provider_input_tokens),
        "estimator absolute error is inconsistent",
        violations,
    );
}

fn verify_fairness(
    fairness: &FairnessEvidence,
    correlation: &CorrelationEvidence,
    work: &ScenarioWorkEvidence,
    violations: &mut Vec<String>,
) {
    require(
        fairness.tenants.len() >= 2,
        "fairness evidence has fewer than two tenants",
        violations,
    );
    let mut owners = BTreeSet::new();
    let mut total_admission_units = Some(0_u64);
    for tenant in &fairness.tenants {
        require_nonempty("fairness owner_id", &tenant.owner_id, violations);
        require(
            owners.insert(tenant.owner_id.as_str()),
            "duplicate fairness tenant",
            violations,
        );
        require(
            tenant.admission_units > 0,
            "tenant admission units are zero",
            violations,
        );
        require(
            tenant.completed_requests > 0,
            "tenant completed no requests",
            violations,
        );
        total_admission_units =
            total_admission_units.and_then(|total| total.checked_add(tenant.admission_units));
    }
    require(
        total_admission_units.is_some(),
        "fairness admission unit sum overflowed",
        violations,
    );
    if let Some(total_admission_units) = total_admission_units {
        require(
            work.admission_units >= total_admission_units,
            "scenario work admission units are below per-tenant fairness evidence",
            violations,
        );
    }
    require(
        owners.contains(correlation.owner_id()),
        "scenario owner is absent from fairness evidence",
        violations,
    );
}

fn require(condition: bool, message: &str, violations: &mut Vec<String>) {
    if !condition {
        violations.push(message.to_string());
    }
}

fn require_nonempty(field: &str, value: &str, violations: &mut Vec<String>) {
    require(
        !value.trim().is_empty(),
        &format!("{field} is empty"),
        violations,
    );
}

fn require_sha256(field: &str, value: &str, violations: &mut Vec<String>) {
    require_hexadecimal_64(field, value, violations);
}

fn require_hexadecimal_64(field: &str, value: &str, violations: &mut Vec<String>) {
    require(
        value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
        &format!("{field} is not a 64-character hexadecimal value"),
        violations,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history_work::{HistoryWorkSite, record_operation};

    struct RejectSerialization;

    impl Serialize for RejectSerialization {
        fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            Err(serde::ser::Error::custom("rejected by fixture"))
        }
    }

    fn incomplete_artifact() -> ProductionBaselineArtifact {
        ProductionBaselineArtifact {
            schema: PRODUCTION_BASELINE_SCHEMA.to_string(),
            provenance: BaselineProvenance {
                git_sha: "abc".into(),
                git_dirty: false,
                git_diff_sha256: "def".into(),
                untracked_file_sha256: BTreeMap::new(),
                executable_sha256: "ghi".into(),
                production_executables: Vec::new(),
                generated_at_unix_seconds: 1,
                machine_id: "machine".into(),
                rustc: "rustc".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                cpu_model: "cpu".into(),
                logical_cpu_count: 1,
                memory_bytes: 1,
            },
            inventory: CoverageInventory {
                coverage_complete: false,
                omissions_are_exhaustive: false,
                known_omissions: vec!["production gap".into()],
            },
            process_captures: Vec::new(),
            scenarios: Vec::new(),
            site_totals: Vec::new(),
        }
    }

    fn role_hash(role: ProductionProcessRole) -> String {
        match role {
            ProductionProcessRole::Cli => "a".repeat(64),
            ProductionProcessRole::Server => "b".repeat(64),
            ProductionProcessRole::Edge => "c".repeat(64),
        }
    }

    fn clean_provenance(generated_at_unix_seconds: u64) -> BaselineProvenance {
        BaselineProvenance {
            git_sha: "d".repeat(40),
            git_dirty: false,
            git_diff_sha256: format!("{:x}", Sha256::digest([])),
            untracked_file_sha256: BTreeMap::new(),
            executable_sha256: "e".repeat(64),
            production_executables: ProductionProcessRole::ALL
                .into_iter()
                .map(|role| ProductionExecutableEvidence {
                    role,
                    executable_name: role.expected_executable_name().to_string(),
                    executable_sha256: role_hash(role),
                })
                .collect(),
            generated_at_unix_seconds,
            machine_id: "machine".into(),
            rustc: "rustc".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            cpu_model: "cpu".into(),
            logical_cpu_count: 1,
            memory_bytes: 1,
        }
    }

    fn zero_capture(
        role: ProductionProcessRole,
        pid: u32,
        started_at_unix_seconds: u64,
        finished_at_unix_seconds: u64,
    ) -> ProductionProcessCapture {
        let scope = match role {
            ProductionProcessRole::Cli => {
                ProductionCaptureScope::cold(ProductionTopology::CliServer, WindowClass::K128)
            }
            ProductionProcessRole::Server => ProductionCaptureScope::Setup,
            ProductionProcessRole::Edge => {
                ProductionCaptureScope::service(ProductionTopology::EdgeServer, WindowClass::K128)
            }
        };
        zero_capture_in_scope(
            role,
            scope,
            pid,
            started_at_unix_seconds,
            finished_at_unix_seconds,
        )
    }

    fn zero_capture_in_scope(
        role: ProductionProcessRole,
        scope: ProductionCaptureScope,
        pid: u32,
        started_at_unix_seconds: u64,
        finished_at_unix_seconds: u64,
    ) -> ProductionProcessCapture {
        let baseline_run_id = "f".repeat(64);
        ProductionProcessCapture {
            schema: PROCESS_CAPTURE_SCHEMA.to_string(),
            capture_id: production_process_capture_id(&baseline_run_id, role, scope),
            baseline_run_id,
            scope,
            git_sha: "d".repeat(40),
            build_git_dirty: false,
            role,
            executable_name: role.expected_executable_name().to_string(),
            executable_sha256: role_hash(role),
            pid,
            started_at_unix_seconds,
            finished_at_unix_seconds,
            sites: HistoryWorkSite::ALL
                .into_iter()
                .map(|site| ProcessSiteDelta {
                    site: site.as_str().to_string(),
                    owner: site.owner().to_string(),
                    target_phase: site.primary_target_phase(),
                    events: 0,
                    bytes: 0,
                    rows: 0,
                    admission_units: 0,
                    queue_current_bytes_change: 0,
                    queue_peak_bytes_increase: 0,
                    accounting_errors: 0,
                })
                .collect(),
        }
    }

    #[test]
    fn verifier_rejects_synthetic_or_partial_artifacts() {
        let error = incomplete_artifact().verify().unwrap_err();
        assert!(
            error
                .violations
                .iter()
                .any(|violation| violation == "coverage_complete is false")
        );
        assert!(
            error
                .violations
                .iter()
                .any(|violation| violation.starts_with("missing scenario"))
        );
        assert!(
            error
                .violations
                .iter()
                .any(|violation| violation.starts_with("missing site total"))
        );
    }

    #[test]
    fn verifier_rejects_inconsistent_normalized_usage_without_text_inference() {
        let usage = ProviderUsageEvidence {
            source: ProviderUsageSource::ProviderResponse,
            requests: 1,
            fresh_input_tokens: 10,
            cache_read_input_tokens: 20,
            cache_creation_input_tokens: 30,
            normalized_input_tokens: 59,
            output_tokens: 1,
        };
        let mut violations = Vec::new();
        verify_provider_usage(&usage, &mut violations);
        assert_eq!(
            violations,
            vec!["normalized provider input is not fresh + read + create"]
        );
    }

    #[test]
    fn cache_evidence_must_reconcile_with_provider_response_usage() {
        let usage = ProviderUsageEvidence {
            source: ProviderUsageSource::ProviderResponse,
            requests: 2,
            fresh_input_tokens: 80,
            cache_read_input_tokens: 15,
            cache_creation_input_tokens: 5,
            normalized_input_tokens: 100,
            output_tokens: 7,
        };
        let mut cache = CacheEvidence {
            cold_path_requests: 1,
            warm_eligible_path_requests: 1,
            observed_cache_read_requests: 1,
            cache_read_input_tokens: 15,
            cache_creation_input_tokens: 5,
            total_input_tokens: 100,
        };
        let mut violations = Vec::new();
        verify_cache(&cache, &usage, &mut violations);
        assert!(violations.is_empty());

        cache.total_input_tokens = 99;
        verify_cache(&cache, &usage, &mut violations);
        assert_eq!(
            violations,
            vec!["cache-share denominator does not match normalized provider input"]
        );
    }

    #[test]
    fn warm_eligible_path_does_not_infer_a_cache_hit_from_provider_identity() {
        let usage = ProviderUsageEvidence {
            source: ProviderUsageSource::ProviderResponse,
            requests: 2,
            fresh_input_tokens: 100,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            normalized_input_tokens: 100,
            output_tokens: 7,
        };
        let cache = CacheEvidence {
            cold_path_requests: 1,
            warm_eligible_path_requests: 1,
            observed_cache_read_requests: 0,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            total_input_tokens: 100,
        };
        let mut violations = Vec::new();
        verify_cache(&cache, &usage, &mut violations);
        assert!(
            violations.is_empty(),
            "a provider may legitimately report no cache read for a repeated prefix: {violations:?}"
        );
    }

    #[test]
    fn provenance_hashes_require_full_hexadecimal_sha256_values() {
        let mut violations = Vec::new();
        require_sha256("wire", &"a".repeat(64), &mut violations);
        assert!(violations.is_empty());

        require_sha256("wire", &"a".repeat(63), &mut violations);
        require_sha256("wire", &format!("{}g", "a".repeat(63)), &mut violations);
        assert_eq!(violations.len(), 2);
    }

    #[test]
    fn process_capture_git_sha_must_match_the_binary_build_revision() {
        let built = "a".repeat(40);
        assert_eq!(
            verified_build_git_sha(&built.to_ascii_uppercase(), &built).unwrap(),
            built
        );
        assert!(verified_build_git_sha(&"b".repeat(40), &built).is_err());
        assert!(verified_build_git_sha(&built, "unknown").is_err());
        assert!(verify_clean_build_attestation("false").is_ok());
        assert!(verify_clean_build_attestation("true").is_err());
        assert!(verify_clean_build_attestation("unknown").is_err());

        let run_id = "1".repeat(64);
        assert!(verify_build_attestation(&built, &run_id, &built, "false", &run_id).is_ok());
        assert!(
            verify_build_attestation(&built, &run_id, &built, "false", &"2".repeat(64)).is_err()
        );
        assert!(verify_build_attestation(&built, &run_id, &built, "false", "absent").is_err());
    }

    #[test]
    fn topology_contract_is_a_typed_cross_product() {
        let combinations = ProductionTopology::ALL
            .into_iter()
            .flat_map(|topology| {
                WindowClass::ALL
                    .into_iter()
                    .map(move |window| (topology, window))
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(combinations.len(), 9);
    }

    fn exact_capture_matrix() -> Vec<ProductionProcessCapture> {
        expected_capture_slots()
            .into_iter()
            .enumerate()
            .map(|(index, (scope, role))| {
                zero_capture_in_scope(role, scope, index as u32 + 1, 100, 110)
            })
            .collect()
    }

    fn scenario_captures(
        topology: ProductionTopology,
        window_class: WindowClass,
    ) -> Vec<ProductionProcessCapture> {
        let mut captures = expected_scenario_capture_slots(topology, window_class)
            .into_iter()
            .enumerate()
            .map(|(index, (scope, role))| {
                zero_capture_in_scope(role, scope, index as u32 + 100, 100, 110)
            })
            .collect::<Vec<_>>();
        captures[0].sites[0].events = 1;
        captures[0].sites[0].bytes = 2;
        captures[0].sites[0].rows = 3;
        captures[0].sites[0].admission_units = 4;
        captures[0].sites[0].queue_peak_bytes_increase = 5;
        captures
    }

    fn bound_scenario(
        topology: ProductionTopology,
        window_class: WindowClass,
        evidence: ScenarioCaptureEvidence,
    ) -> ProductionScenario {
        let context_window_tokens = match window_class {
            WindowClass::K128 => 128_000,
            WindowClass::K200 => 200_000,
            WindowClass::M1 => 1_000_000,
        };
        ProductionScenario {
            baseline_run_id: "f".repeat(64),
            topology,
            window_class,
            capture_refs: evidence.capture_refs,
            model: ModelOfferingEvidence {
                offering_id: format!("offering-{window_class:?}"),
                resolved_model_name: "production-model".into(),
                context_window_tokens,
                metadata_source: ModelMetadataSource::DatabaseOffering,
            },
            entrypoints: topology.required_entrypoints().iter().copied().collect(),
            correlation: if topology == ProductionTopology::CliServer {
                CorrelationEvidence::CliSessionBridge {
                    owner_id: "owner-a".into(),
                    session_id: "session".into(),
                    cli_execution_id: "bridge-run".into(),
                    session_turn: 1,
                    turn_chain_id: "bridge-run".into(),
                    user_query_event_id: "query-event".into(),
                    exchange_count: 1,
                    provider_attempts: vec![ProviderAttemptEvidence {
                        request_id: "request".into(),
                        round: 1,
                        logical_attempt: 0,
                        attempt: 1,
                        operation_id: "bridge-operation".into(),
                        wire_request_sha256: "a".repeat(64),
                        wire_request_bytes: 1,
                        terminal_status: AttemptTerminalStatus::Succeeded,
                    }],
                }
            } else {
                CorrelationEvidence::DurableRun {
                    owner_id: "owner-a".into(),
                    session_id: "session".into(),
                    run_id: "run".into(),
                    turn: 1,
                    provider_attempts: vec![ProviderAttemptEvidence {
                        request_id: "request".into(),
                        round: 1,
                        logical_attempt: 0,
                        attempt: 1,
                        operation_id: "run-operation".into(),
                        wire_request_sha256: "a".repeat(64),
                        wire_request_bytes: 1,
                        terminal_status: AttemptTerminalStatus::Succeeded,
                    }],
                }
            },
            work: evidence.work,
            provider_usage: ProviderUsageEvidence {
                source: ProviderUsageSource::ProviderResponse,
                requests: 2,
                fresh_input_tokens: 1,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
                normalized_input_tokens: 1,
                output_tokens: 1,
            },
            cache: CacheEvidence {
                cold_path_requests: 1,
                warm_eligible_path_requests: 1,
                observed_cache_read_requests: 0,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
                total_input_tokens: 1,
            },
            projection: if topology == ProductionTopology::CliServer {
                ProjectionEvidence::CliSessionBridgeNotApplicable
            } else {
                ProjectionEvidence::DurableRun {
                    durable_event_index: 1,
                    projected_event_index: 1,
                    lag_events: 0,
                }
            },
            compaction: CompactionEvidence {
                attempts: 1,
                effective_attempts: 1,
                input_tokens: 2,
                output_tokens: 1,
                tokens_freed: 1,
            },
            estimator: EstimatorEvidence {
                estimated_input_tokens: 1,
                canonical_provider_input_tokens: 1,
                absolute_error_tokens: 0,
            },
            fairness: FairnessEvidence {
                tenants: vec![
                    TenantAdmissionEvidence {
                        owner_id: "owner-a".into(),
                        admission_units: 1,
                        wait_micros: 0,
                        completed_requests: 1,
                    },
                    TenantAdmissionEvidence {
                        owner_id: "owner-b".into(),
                        admission_units: 1,
                        wait_micros: 0,
                        completed_requests: 1,
                    },
                ],
            },
        }
    }

    #[test]
    fn verifier_requires_the_exact_external_process_matrix() {
        let mut captures = exact_capture_matrix();
        assert_eq!(captures.len(), 19);
        let omitted = captures.pop().unwrap();
        let mut violations = Vec::new();
        verify_process_captures(&captures, &clean_provenance(140), &mut violations);
        assert!(violations.iter().any(|violation| {
            violation
                == &format!(
                    "missing production capture slot {:?}/{:?}",
                    omitted.scope, omitted.role
                )
        }));
        assert!(violations.iter().any(|violation| {
            violation == "production baseline must contain exactly 19 process captures"
        }));
    }

    #[test]
    fn scenario_capture_evidence_rejects_in_process_and_mixed_fragments() {
        let error = scenario_capture_evidence(
            &"f".repeat(64),
            ProductionTopology::ServerOnly,
            WindowClass::K128,
            &[],
        )
        .unwrap_err();
        assert!(
            error
                .violations
                .iter()
                .any(|violation| violation.contains("is missing capture slot"))
        );

        let foreign = zero_capture_in_scope(
            ProductionProcessRole::Server,
            ProductionCaptureScope::service(ProductionTopology::CliServer, WindowClass::K128),
            99,
            100,
            110,
        );
        let error = scenario_capture_evidence(
            &"f".repeat(64),
            ProductionTopology::ServerOnly,
            WindowClass::K128,
            &[foreign],
        )
        .unwrap_err();
        assert!(
            error
                .violations
                .iter()
                .any(|violation| violation.contains("unexpected capture slot"))
        );
    }

    #[test]
    fn verifier_binds_scenario_refs_digests_and_recomputed_work() {
        let captures = scenario_captures(ProductionTopology::CliServer, WindowClass::K128);
        let evidence = scenario_capture_evidence(
            &"f".repeat(64),
            ProductionTopology::CliServer,
            WindowClass::K128,
            &captures,
        )
        .unwrap();
        let scenario = bound_scenario(ProductionTopology::CliServer, WindowClass::K128, evidence);
        let captures_by_id = captures
            .iter()
            .map(|capture| (capture.capture_id.as_str(), capture))
            .collect::<BTreeMap<_, _>>();
        let mut referenced = BTreeSet::new();
        let mut violations = Vec::new();
        verify_scenario_capture_binding(
            &scenario,
            &captures_by_id,
            &mut referenced,
            &mut violations,
        );
        assert!(violations.is_empty(), "{violations:?}");

        let mut wrong_digest = scenario.clone();
        wrong_digest.capture_refs[0].capture_sha256 = "0".repeat(64);
        referenced.clear();
        verify_scenario_capture_binding(
            &wrong_digest,
            &captures_by_id,
            &mut referenced,
            &mut violations,
        );
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("digest mismatch"))
        );

        violations.clear();
        referenced.clear();
        let mut wrong_work = scenario;
        wrong_work.work.history_events += 1;
        verify_scenario_capture_binding(
            &wrong_work,
            &captures_by_id,
            &mut referenced,
            &mut violations,
        );
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("work does not equal"))
        );
    }

    #[test]
    fn scenario_work_aggregation_fails_closed_on_overflow() {
        let mut captures = scenario_captures(ProductionTopology::CliServer, WindowClass::K128);
        captures[0].sites[0].events = u64::MAX;
        captures[1].sites[0].events = 1;
        let error = aggregate_scenario_work(&captures).unwrap_err();
        assert!(
            error
                .violations
                .iter()
                .any(|violation| violation.contains("aggregate overflow"))
        );
    }

    #[test]
    fn verifier_recomputes_site_totals_instead_of_trusting_artifact_json() {
        let captures = vec![
            zero_capture(ProductionProcessRole::Cli, 1, 100, 110),
            zero_capture(ProductionProcessRole::Server, 2, 100, 120),
            zero_capture(ProductionProcessRole::Edge, 3, 100, 130),
        ];
        let mut artifact = incomplete_artifact();
        artifact.provenance = clean_provenance(140);
        artifact.site_totals = aggregate_process_site_totals(&captures).unwrap();
        artifact.site_totals[0].events = 1;
        artifact.process_captures = captures;

        let error = artifact.verify().unwrap_err();
        assert!(error.violations.iter().any(|violation| {
            violation.starts_with("site total ") && violation.ends_with("process captures")
        }));
    }

    #[test]
    fn verifier_rejects_duplicate_capture_identity_and_content() {
        let cli = zero_capture(ProductionProcessRole::Cli, 1, 100, 110);
        let captures = vec![
            cli.clone(),
            cli,
            zero_capture(ProductionProcessRole::Server, 2, 100, 120),
            zero_capture(ProductionProcessRole::Edge, 3, 100, 130),
        ];
        let mut violations = Vec::new();
        verify_process_captures(&captures, &clean_provenance(140), &mut violations);
        assert!(
            violations
                .iter()
                .any(|violation| violation.starts_with("duplicate process capture identity"))
        );
        assert!(
            violations
                .iter()
                .any(|violation| violation.starts_with("duplicate process capture content"))
        );
    }

    #[test]
    fn verifier_binds_capture_revision_binary_and_freshness() {
        let mut capture = zero_capture(ProductionProcessRole::Cli, 1, 100, 110);
        capture.git_sha = "9".repeat(40);
        capture.executable_sha256 = "8".repeat(64);
        let provenance = clean_provenance(110 + MAX_CAPTURE_ASSEMBLY_DELAY_SECONDS + 1);
        let mut violations = Vec::new();
        verify_process_captures(&[capture.clone()], &provenance, &mut violations);
        verify_capture_time_window(
            &[capture],
            provenance.generated_at_unix_seconds,
            &mut violations,
        );
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("git_sha does not match"))
        );
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("verified Cli executable"))
        );
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("stale relative"))
        );
    }

    #[test]
    fn fairness_units_must_fit_scenario_work_without_overflow() {
        let correlation = CorrelationEvidence::DurableRun {
            owner_id: "owner-a".into(),
            session_id: "session".into(),
            run_id: "run".into(),
            turn: 1,
            provider_attempts: Vec::new(),
        };
        let work = ScenarioWorkEvidence {
            history_events: 1,
            clone_hash_serialization_bytes: 1,
            db_rows: 1,
            admission_units: 5,
            queue_peak_bytes: 1,
            queue_current_bytes_change: 0,
            accounting_errors: 0,
        };
        let fairness = FairnessEvidence {
            tenants: vec![
                TenantAdmissionEvidence {
                    owner_id: "owner-a".into(),
                    admission_units: u64::MAX,
                    wait_micros: 0,
                    completed_requests: 1,
                },
                TenantAdmissionEvidence {
                    owner_id: "owner-b".into(),
                    admission_units: 1,
                    wait_micros: 0,
                    completed_requests: 1,
                },
            ],
        };
        let mut violations = Vec::new();
        verify_fairness(&fairness, &correlation, &work, &mut violations);
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("sum overflowed"))
        );

        let bounded = FairnessEvidence {
            tenants: vec![
                TenantAdmissionEvidence {
                    owner_id: "owner-a".into(),
                    admission_units: 3,
                    wait_micros: 0,
                    completed_requests: 1,
                },
                TenantAdmissionEvidence {
                    owner_id: "owner-b".into(),
                    admission_units: 3,
                    wait_micros: 0,
                    completed_requests: 1,
                },
            ],
        };
        violations.clear();
        verify_fairness(&bounded, &correlation, &work, &mut violations);
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("below per-tenant"))
        );
    }

    #[test]
    fn zero_compaction_rate_is_valid_only_for_the_phase0_cli_adapter() {
        let zero = CompactionEvidence {
            attempts: 0,
            effective_attempts: 0,
            input_tokens: 0,
            output_tokens: 0,
            tokens_freed: 0,
        };
        let mut violations = Vec::new();
        verify_compaction(ProductionTopology::CliServer, &zero, &mut violations);
        assert!(violations.is_empty());

        verify_compaction(ProductionTopology::ServerOnly, &zero, &mut violations);
        assert_eq!(
            violations,
            vec!["durable topology did not exercise compaction under baseline history pressure"]
        );

        violations.clear();
        let invented = CompactionEvidence {
            tokens_freed: 1,
            ..zero
        };
        verify_compaction(ProductionTopology::CliServer, &invented, &mut violations);
        assert_eq!(
            violations,
            vec!["zero-attempt compaction evidence contains invented work"]
        );
    }

    #[test]
    fn process_capture_reads_real_counter_delta_without_a_recording_api() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("capture.json");
        let (executable_name, executable_sha256) = current_executable_identity().unwrap();
        let guard = ProductionProcessCaptureGuard {
            baseline_run_id: "b".repeat(64),
            git_sha: "c".repeat(40),
            role: ProductionProcessRole::Server,
            scope: ProductionCaptureScope::Setup,
            path: path.clone(),
            executable_name,
            executable_sha256,
            started_at_unix_seconds: 1,
            before: HistoryWorkSnapshot::capture(),
        };
        record_operation(HistoryWorkSite::ProviderBodySerialization, 71, 2, 3);

        let capture = guard.finish().unwrap();
        let site = capture
            .sites
            .iter()
            .find(|site| site.site == HistoryWorkSite::ProviderBodySerialization.as_str())
            .unwrap();
        assert!(site.events >= 1);
        assert!(site.bytes >= 71);
        assert!(site.rows >= 2);
        assert!(site.admission_units >= 3);
        assert_eq!(
            serde_json::from_slice::<ProductionProcessCapture>(&fs::read(path).unwrap()).unwrap(),
            capture
        );
    }

    #[test]
    fn atomic_json_publish_leaves_only_the_complete_target() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("artifact.json");

        write_json_atomic(&path, &serde_json::json!({"complete": true})).unwrap();

        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&fs::read(&path).unwrap()).unwrap(),
            serde_json::json!({"complete": true})
        );
        let entries = fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(entries, vec![path]);
    }

    #[test]
    fn atomic_json_stage_is_cleaned_on_serialization_and_publish_failure() {
        let serialization_directory = tempfile::tempdir().unwrap();
        let serialization_path = serialization_directory.path().join("artifact.json");
        assert!(write_json_atomic(&serialization_path, &RejectSerialization).is_err());
        assert_eq!(
            fs::read_dir(serialization_directory.path())
                .unwrap()
                .count(),
            0
        );

        let publish_directory = tempfile::tempdir().unwrap();
        let publish_path = publish_directory.path().join("artifact.json");
        fs::create_dir(&publish_path).unwrap();
        assert!(write_json_atomic(&publish_path, &serde_json::json!({"complete": true})).is_err());
        let entries = fs::read_dir(publish_directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(
            entries,
            vec![publish_path],
            "failed publication must not leak a staged file"
        );
    }
}

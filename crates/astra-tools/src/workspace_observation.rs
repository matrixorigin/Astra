//! Bounded, executor-owned observation of workspace changes.
//!
//! Shell syntax is not a reliable mutation oracle: an inline interpreter,
//! generated script, or a project-specific build tool can change files without
//! containing a known redirect/verb.  This module provides a small post-
//! execution receipt based on the bound workspace state instead of extending a
//! command-name allowlist.  It is deliberately best-effort: an unavailable or
//! over-large fingerprint yields `None`, never a positive mutation claim.

use std::collections::{HashMap, hash_map::DefaultHasher};
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, OnceLock, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use sha2::{Digest, Sha256};

const MAX_MANIFEST_ENTRIES: usize = 16_384;
const MAX_STATUS_CONTENT_BYTES: usize = 32 * 1024 * 1024;
const MAX_STATUS_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
const MAX_STATUS_ENTRIES: usize = 8_192;
// Ignored build/output directories are useful deliverables, but expanding a
// whole cache would make every Bash call expensive.  Scan a small bounded
// tree; larger caches intentionally become Unknown rather than being treated
// as unchanged.
const MAX_IGNORED_ENTRIES: usize = 2_048;
const MAX_IGNORED_CONTENT_BYTES: usize = 8 * 1024 * 1024;
const FINGERPRINT_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_LEASE_WAIT: Duration = Duration::from_secs(120);
#[cfg(target_os = "linux")]
const MAX_ACTIVE_GENERATION_WATCH_PATHS: usize = 16_384;
#[cfg(target_os = "linux")]
const GENERATION_WATCH_CONTROL_TIMEOUT: Duration = Duration::from_secs(2);

#[cfg(unix)]
const TRUSTED_GIT_PATH: &str = "/usr/bin:/bin";
#[cfg(windows)]
const TRUSTED_GIT_PATH: &str = r"C:\Windows\System32";

/// Metadata keys carried in a `ToolResult` and projected into the lossless
/// `ToolCallRecord`.  The scope value is intentionally explicit so a future
/// executor cannot accidentally turn an external/scratch observation into a
/// bound-workspace completion receipt.
pub const OBSERVED_FIELD: &str = "workspace_mutation_observed";
pub const SCOPE_FIELD: &str = "workspace_mutation_scope";
pub const RECEIPT_FIELD: &str = "workspace_mutation_receipt";
pub const OWNERSHIP_FIELD: &str = "workspace_mutation_ownership";
pub const BOUND_WORKSPACE_SCOPE: &str = "bound_workspace";
pub const INVOCATION_CGROUP_OWNERSHIP: &str = "invocation_cgroup";
pub const INVOCATION_SUPERVISOR_OWNERSHIP: &str = "invocation_supervisor";
pub const FOREGROUND_PROCESS_GROUP_OWNERSHIP: &str = "foreground_process_group";
/// Ownership marker for structured workspace tools (for example write_file
/// and str_replace).  Unlike a shell fingerprint this fact is produced by
/// the executor that owns the workspace binding, so a server must not
/// re-resolve the target path against its own filesystem.
pub const TYPED_WORKSPACE_TOOL_OWNERSHIP: &str = "typed_workspace_tool";
pub const TYPED_MULTI_PATH_WRITER_OWNERSHIP: &str = "typed_multi_path_writer";
pub const TYPED_WORKSPACE_OBSERVER_OWNERSHIP: &str = "typed_workspace_observer";
/// Owner-internal marker set only when a complete-state typed writer proved
/// that its requested target bytes already existed.  The marker is not a
/// portable receipt by itself; the workspace-owning execution boundary must
/// bind it to the invocation target before transport.
pub const DESIRED_STATE_CONVERGED_FIELD: &str = "workspace_desired_state_converged";
const DESIRED_STATE_CONVERGENCE_MARKER_SCHEMA: &str =
    "workspace_desired_state_convergence_marker.v1";
static DESIRED_STATE_MARKER_REGISTRY: OnceLock<
    std::sync::Mutex<HashMap<String, (WorkspaceFileStateIdentity, WorkspaceFileStateIdentity)>>,
> = OnceLock::new();
const MAX_LIVE_DESIRED_STATE_MARKERS: usize = 4096;
pub const OBSERVATION_RECEIPT_FIELD: &str = "workspace_observation_receipt";
pub const OBSERVATION_SCOPE_FIELD: &str = "workspace_observation_scope";
/// Structured Bash argument naming the bounded external state roots whose
/// exact pre/post state the executor must observe.
pub const EXTERNAL_STATE_PATHS_FIELD: &str = "external_state_paths";
pub const EXTERNAL_EFFECT_OBSERVED_FIELD: &str = "external_effect_observed";
pub const EXTERNAL_EFFECT_SCOPE_FIELD: &str = "external_effect_scope";
pub const EXTERNAL_EFFECT_RECEIPT_FIELD: &str = "external_effect_receipt";
pub const DECLARED_EXTERNAL_STATE_SCOPE: &str = "declared_external_state";
const MAX_EXTERNAL_STATE_PATHS: usize = 16;
/// Minimal path for the only external helper allowed in a detached command
/// (`sleep`). Builtins do not consult PATH, and clearing the inherited
/// environment removes BASH_ENV, exported functions, LD_PRELOAD, and user
/// PATH shadowing from the detached process.
pub const DETACHABLE_PATH: &str = "/usr/bin:/bin";

/// Detached execution is intentionally narrower than ordinary execution.
/// Without a terminal-owned post receipt, only shell builtins (plus the
/// harmless `sleep` helper used by interactive flows) are eligible; all
/// filesystem/network/tool commands stay foreground where the observer can
/// attribute their final state.
pub fn bash_command_is_detachable_safe(command: &str) -> bool {
    let command = command.trim();
    if command.is_empty()
        || ["&&", "||", ";", "|", ">", "<", "$(", "`", "\n", "&"]
            .iter()
            .any(|marker| command.contains(marker))
    {
        return false;
    }
    let mut words = command.split_whitespace();
    let Some(first) = words.next() else {
        return false;
    };
    if is_env_assignment(first) || first == "env" {
        return false;
    }
    matches!(
        first,
        "echo" | "printf" | "pwd" | "true" | "false" | ":" | "test" | "[" | "sleep"
    )
}

fn is_env_assignment(word: &str) -> bool {
    let mut chars = word.chars();
    match chars.next() {
        Some(ch) if ch.is_ascii_alphabetic() || ch == '_' => {}
        _ => return false,
    }
    let mut saw_equal = false;
    for ch in chars {
        if ch == '=' {
            saw_equal = true;
            break;
        }
        if !(ch.is_ascii_alphanumeric() || ch == '_') {
            return false;
        }
    }
    saw_equal
}

static OBSERVATION_GATES: OnceLock<
    std::sync::Mutex<
        std::collections::HashMap<
            std::path::PathBuf,
            std::sync::Weak<std::sync::atomic::AtomicBool>,
        >,
    >,
> = OnceLock::new();

static WRITER_EPOCHS: OnceLock<
    std::sync::Mutex<
        std::collections::HashMap<std::path::PathBuf, std::sync::Weak<WriterEpochState>>,
    >,
> = OnceLock::new();

/// Quarantined roots deliberately keep their state alive after the last
/// ordinary writer/fingerprint handle is dropped.  A weak-only registry would
/// silently recreate a clean state on the next call and undo the fail-closed
/// guarantee.  The map is limited to roots that actually became uncertain;
/// normal workspaces retain the existing weak lifecycle.
static QUARANTINED_WRITER_STATES: OnceLock<
    std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, Arc<WriterEpochState>>>,
> = OnceLock::new();

// Fingerprinting can walk a bounded but still non-trivial tree.  Keep it off
// runtime workers and bound concurrent postimages under multi-session load.
static EXTERNAL_POSTIMAGE_PERMITS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();

fn recover_mutex<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[derive(Debug, Default)]
struct WriterEpochState {
    epoch: std::sync::atomic::AtomicU64,
    active: std::sync::atomic::AtomicUsize,
    /// Set when an invocation ended without a provable ownership boundary.
    /// A later fingerprint must not turn an un-attributed descendant write
    /// into a positive receipt.  This is deliberately sticky for the bound
    /// workspace: clearing it implicitly would recreate the same race.
    quarantined: std::sync::atomic::AtomicBool,
    /// Stronger terminal fact: the executor positively failed to settle the
    /// invocation's descendants. Attribution quarantine alone only prevents
    /// future receipts; it must not turn every foreground-process-group call
    /// into an incomplete user task.
    ownership_unsettled: std::sync::atomic::AtomicBool,
}

/// A writer that cannot take the ordinary non-reentrant observation mutex
/// (notably `run_script`, whose RPC can recursively invoke file tools) still
/// participates in attribution.  Fingerprints include this epoch, so any
/// Bash pre/post window that overlaps such a writer fails closed instead of
/// assigning its delta to Bash.
pub struct WorkspaceWriterGuard {
    state: Arc<WriterEpochState>,
    lease: WorkspaceObservationLease,
}

impl WorkspaceWriterGuard {
    /// A top-level opaque writer may only publish owner facts while the
    /// stable namespace and the bound workspace still name the generation
    /// admitted before execution.
    pub fn integrity_valid(&self) -> bool {
        self.lease.integrity_valid()
    }
}

impl Drop for WorkspaceWriterGuard {
    fn drop(&mut self) {
        self.state
            .active
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        self.state
            .epoch
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }
}

fn writer_epoch_state(workspace_root: &Path) -> Option<Arc<WriterEpochState>> {
    let canonical_key = workspace_root.canonicalize().ok();
    let lexical_key = workspace_lexical_key(workspace_root)?;
    let mut lookup_keys = Vec::with_capacity(2);
    if let Some(key) = canonical_key.clone() {
        lookup_keys.push(key);
    }
    if !lookup_keys.iter().any(|key| key == &lexical_key) {
        lookup_keys.push(lexical_key.clone());
    }
    let quarantine_map =
        QUARANTINED_WRITER_STATES.get_or_init(|| std::sync::Mutex::new(Default::default()));
    let map = recover_mutex(quarantine_map);
    if let Some(state) = lookup_keys.iter().find_map(|key| map.get(key).cloned()) {
        return Some(state);
    }
    let epochs = WRITER_EPOCHS.get_or_init(|| std::sync::Mutex::new(Default::default()));
    let mut map = recover_mutex(epochs);
    map.retain(|_, weak| weak.strong_count() > 0);
    if let Some(state) = lookup_keys
        .iter()
        .find_map(|key| map.get(key).and_then(std::sync::Weak::upgrade))
    {
        return Some(state);
    }
    let state = Arc::new(WriterEpochState::default());
    let weak = Arc::downgrade(&state);
    for key in lookup_keys {
        map.insert(key, weak.clone());
    }
    Some(state)
}

/// Resolve a stable binding key even when an invocation has removed or
/// temporarily renamed the workspace root. Canonicalization is preferred so
/// symlink aliases converge while the path exists; the lexical absolute form
/// is the fail-closed fallback once the binding has disappeared.
fn workspace_binding_key(workspace_root: &Path) -> Option<PathBuf> {
    if let Ok(canonical) = workspace_root.canonicalize() {
        return Some(canonical);
    }
    workspace_lexical_key(workspace_root)
}

fn workspace_lexical_key(workspace_root: &Path) -> Option<PathBuf> {
    let absolute = if workspace_root.is_absolute() {
        workspace_root.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(workspace_root)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(value) => normalized.push(value),
        }
    }
    (!normalized.as_os_str().is_empty()).then_some(normalized)
}

/// Return the current epoch for the bound workspace. If registration cannot
/// be completed, return `None`; callers then fail closed and emit no receipt.
pub fn workspace_writer_epoch(workspace_root: &Path) -> Option<u64> {
    let state = writer_epoch_state(workspace_root)?;
    Some(state.epoch.load(std::sync::atomic::Ordering::Acquire))
}

/// Put a workspace into a fail-closed observation quarantine after an
/// invocation whose descendants could not be proven dead.  The quarantine is
/// scoped to the canonical workspace root and is intentionally sticky for the
/// lifetime of this process.  A new session/workspace binding gets a fresh
/// state; no caller may silently re-baseline a potentially still-running
/// descendant.
pub fn mark_workspace_observation_unsettled(workspace_root: &Path) -> bool {
    mark_workspace_observation_quarantine(workspace_root, true)
}

fn mark_workspace_observation_quarantine(workspace_root: &Path, ownership_unsettled: bool) -> bool {
    let Some(key) = workspace_binding_key(workspace_root) else {
        return false;
    };
    let lexical_key = workspace_lexical_key(workspace_root);
    let Some(state) = writer_epoch_state(workspace_root) else {
        return false;
    };
    state
        .quarantined
        .store(true, std::sync::atomic::Ordering::Release);
    if ownership_unsettled {
        state
            .ownership_unsettled
            .store(true, std::sync::atomic::Ordering::Release);
    }
    state
        .epoch
        .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    let mut aliases = vec![key];
    if let Some(key) = lexical_key
        && !aliases.iter().any(|alias| alias == &key)
    {
        aliases.push(key);
    }
    // Preserve every alias that was registered while the invocation still
    // held the state. This closes the symlink/rebind race: a terminal path
    // lookup must quarantine the pre-existing state, not create a new clean
    // state for the symlink's new target.
    let epochs = WRITER_EPOCHS.get_or_init(|| std::sync::Mutex::new(Default::default()));
    {
        let map = recover_mutex(epochs);
        for (alias, weak) in map.iter() {
            if weak
                .upgrade()
                .is_some_and(|candidate| Arc::ptr_eq(&candidate, &state))
                && !aliases.iter().any(|existing| existing == alias)
            {
                aliases.push(alias.clone());
            }
        }
    }
    let quarantine_map =
        QUARANTINED_WRITER_STATES.get_or_init(|| std::sync::Mutex::new(Default::default()));
    let mut quarantined = recover_mutex(quarantine_map);
    for alias in aliases {
        quarantined.insert(alias, state.clone());
    }
    true
}

/// A settled foreground process group is sufficient to close the current
/// synchronous call, but not to prove that a descendant never escaped into a
/// new session. Prevent subsequent fingerprint attribution without promoting
/// that weaker provenance into a terminal ownership failure.
pub fn quarantine_after_weak_receipt(workspace_root: &Path, ownership: Option<&str>) -> bool {
    if ownership == Some(FOREGROUND_PROCESS_GROUP_OWNERSHIP) {
        mark_workspace_observation_quarantine(workspace_root, false)
    } else {
        false
    }
}

/// Whether observations for this workspace are quarantined.  This is exposed
/// for host telemetry and deterministic tests; the actual safety decision is
/// made inside [`WorkspaceFingerprint::capture`].
pub fn workspace_observation_is_quarantined(workspace_root: &Path) -> Option<bool> {
    let state = writer_epoch_state(workspace_root)?;
    Some(state.quarantined.load(std::sync::atomic::Ordering::Acquire))
}

/// Whether the executor positively failed to settle invocation ownership.
/// Unlike attribution quarantine, this is a terminal completion barrier.
pub fn workspace_ownership_is_unsettled(workspace_root: &Path) -> Option<bool> {
    let state = writer_epoch_state(workspace_root)?;
    Some(
        state
            .ownership_unsettled
            .load(std::sync::atomic::Ordering::Acquire),
    )
}

/// Mark an executor-owned writer that may recursively invoke other tools.
/// The epoch changes at both admission and completion, making overlap visible
/// even when the writer itself returns to a clean workspace state.
pub fn begin_workspace_writer(workspace_root: &Path) -> Option<WorkspaceWriterGuard> {
    begin_workspace_writer_sync_with_options(workspace_root, None, DEFAULT_LEASE_WAIT)
}

/// Admit a recursively-dispatched writer without blocking an async runtime.
///
/// `run_script` is an opaque workspace writer, so its top-level invocation
/// owns the same exclusive lease as Bash and typed writers. Authenticated RPC
/// callbacks re-use that parent authority through a task-local marker and do
/// not acquire another lease; no unauthenticated caller may request that
/// re-entrant route.
pub async fn begin_workspace_writer_with_options(
    workspace_root: &Path,
    cancel_token: Option<&CancellationToken>,
    max_wait: Duration,
) -> Option<WorkspaceWriterGuard> {
    let lease =
        acquire_workspace_mutation_lease_with_options(workspace_root, cancel_token, max_wait)
            .await?;
    begin_workspace_writer_after_lease(workspace_root, lease)
}

fn begin_workspace_writer_sync_with_options(
    workspace_root: &Path,
    cancel_token: Option<&CancellationToken>,
    max_wait: Duration,
) -> Option<WorkspaceWriterGuard> {
    let lease =
        acquire_workspace_mutation_lease_sync_with_options(workspace_root, cancel_token, max_wait)?;
    begin_workspace_writer_after_lease(workspace_root, lease)
}

fn begin_workspace_writer_after_lease(
    workspace_root: &Path,
    lease: WorkspaceObservationLease,
) -> Option<WorkspaceWriterGuard> {
    let state = writer_epoch_state(workspace_root)?;
    state
        .active
        .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    state
        .epoch
        .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    Some(WorkspaceWriterGuard { state, lease })
}

pub struct WorkspaceObservationLease {
    gate: Arc<std::sync::atomic::AtomicBool>,
    locks: Vec<CrossProcessFileLock>,
    binding_identity: WorkspaceBindingIdentity,
    writer_state: Arc<WriterEpochState>,
    tamper_watch: GenerationTamperWatch,
}

impl WorkspaceObservationLease {
    /// Verify that the external coordination paths, kernel event history, and
    /// bound workspace components still name the generation admitted before
    /// execution.
    pub fn integrity_valid(&self) -> bool {
        self.binding_identity.is_unchanged()
            && self.tamper_watch.is_untampered()
            && !self
                .writer_state
                .quarantined
                .load(std::sync::atomic::Ordering::Acquire)
            && self
                .locks
                .iter()
                .all(CrossProcessFileLock::path_identity_is_unchanged)
    }
}

/// Kernel-backed sticky evidence that a coordination inode or any lexical
/// binding component was modified, renamed, or unlinked while a writer held
/// the generation. End-state inode checks alone miss unlink→recreate→restore
/// attacks, which can briefly admit a second lock generation.
struct GenerationTamperWatch {
    #[cfg(target_os = "linux")]
    subscription: GenerationWatchSubscription,
}

impl GenerationTamperWatch {
    fn arm(
        lock_paths: impl IntoIterator<Item = PathBuf>,
        binding_paths: impl IntoIterator<Item = PathBuf>,
        cancel_token: Option<&CancellationToken>,
    ) -> std::io::Result<Self> {
        #[cfg(target_os = "linux")]
        {
            let lock_mask = libc::IN_ATTRIB
                | libc::IN_DELETE_SELF
                | libc::IN_MODIFY
                | libc::IN_MOVE_SELF
                | libc::IN_UNMOUNT;
            let binding_mask = libc::IN_DELETE_SELF | libc::IN_MOVE_SELF | libc::IN_UNMOUNT;
            let mut specifications = HashMap::<PathBuf, u32>::new();
            for path in lock_paths {
                specifications
                    .entry(path)
                    .and_modify(|mask| *mask |= lock_mask)
                    .or_insert(lock_mask);
            }
            for path in binding_paths {
                // Only self-removal/rebinding is generation tamper. Ordinary
                // writes below a watched workspace directory legitimately
                // change that directory's metadata and must remain receiptable.
                specifications
                    .entry(path)
                    .and_modify(|mask| *mask |= binding_mask)
                    .or_insert(binding_mask);
            }
            let subscription = generation_watch_registry()?.register(
                specifications
                    .into_iter()
                    .map(|(path, mask)| GenerationWatchSpec { path, mask })
                    .collect(),
                cancel_token,
            )?;
            Ok(Self { subscription })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (lock_paths, binding_paths);
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "workspace generation tamper watch is unavailable",
            ))
        }
    }

    fn is_untampered(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.subscription.is_untampered()
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct GenerationWatchSpec {
    path: PathBuf,
    mask: u32,
}

/// One process-level inotify instance multiplexes every live workspace
/// generation.  A per-lease inotify fd exhausts Linux's relatively small
/// `max_user_instances` limit at ordinary multi-user concurrency (128 on many
/// hosts). Registrations remain independently fail-closed while descriptors
/// and the event reader stay bounded at the process boundary.
#[cfg(target_os = "linux")]
struct GenerationWatchRegistry {
    commands: mpsc::Sender<GenerationWatchCommand>,
    alive: Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(target_os = "linux")]
struct GenerationWatchSubscription {
    id: u64,
    tampered: Arc<std::sync::atomic::AtomicBool>,
    registry: Arc<GenerationWatchRegistry>,
}

#[cfg(target_os = "linux")]
struct GenerationWatchAdmission {
    id: Option<u64>,
    commands: mpsc::Sender<GenerationWatchCommand>,
    alive: Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(target_os = "linux")]
impl GenerationWatchAdmission {
    fn commit(mut self) -> u64 {
        self.id.take().expect("uncommitted watch admission")
    }

    fn retire_and_wait(mut self) -> bool {
        let Some(id) = self.id.take() else {
            return true;
        };
        let (reply, response) = mpsc::sync_channel(1);
        if self
            .commands
            .send(GenerationWatchCommand::Retire {
                id,
                reply: Some(reply),
            })
            .is_err()
            || response
                .recv_timeout(GENERATION_WATCH_CONTROL_TIMEOUT)
                .is_err()
        {
            self.alive
                .store(false, std::sync::atomic::Ordering::Release);
            return false;
        }
        true
    }
}

#[cfg(target_os = "linux")]
impl Drop for GenerationWatchAdmission {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            let _ = self
                .commands
                .send(GenerationWatchCommand::Retire { id, reply: None });
        }
    }
}

#[cfg(target_os = "linux")]
impl GenerationWatchSubscription {
    fn is_untampered(&self) -> bool {
        if !self
            .registry
            .alive
            .load(std::sync::atomic::Ordering::Acquire)
            || self.tampered.load(std::sync::atomic::Ordering::Acquire)
        {
            return false;
        }
        let (reply, response) = mpsc::sync_channel(1);
        if self
            .registry
            .commands
            .send(GenerationWatchCommand::Check { id: self.id, reply })
            .is_err()
        {
            return false;
        }
        response
            .recv_timeout(GENERATION_WATCH_CONTROL_TIMEOUT)
            .unwrap_or(false)
    }
}

#[cfg(target_os = "linux")]
impl Drop for GenerationWatchSubscription {
    fn drop(&mut self) {
        let _ = self
            .registry
            .commands
            .send(GenerationWatchCommand::Unregister { id: self.id });
    }
}

#[cfg(target_os = "linux")]
enum GenerationWatchCommand {
    Register {
        specifications: Vec<GenerationWatchSpec>,
        tampered: Arc<std::sync::atomic::AtomicBool>,
        cancelled: Arc<std::sync::atomic::AtomicBool>,
        retire_commands: mpsc::Sender<GenerationWatchCommand>,
        alive: Arc<std::sync::atomic::AtomicBool>,
        reply: mpsc::SyncSender<Result<GenerationWatchAdmission, String>>,
    },
    Check {
        id: u64,
        reply: mpsc::SyncSender<bool>,
    },
    Unregister {
        id: u64,
    },
    Retire {
        id: u64,
        reply: Option<mpsc::SyncSender<()>>,
    },
    #[cfg(test)]
    Stats {
        reply: mpsc::SyncSender<(usize, usize)>,
    },
    #[cfg(test)]
    ContainsPath {
        path: PathBuf,
        reply: mpsc::SyncSender<bool>,
    },
}

#[cfg(target_os = "linux")]
struct ActiveGenerationSubscription {
    tampered: Arc<std::sync::atomic::AtomicBool>,
    watches: Vec<(i32, u32)>,
}

#[cfg(target_os = "linux")]
struct MultiplexedWatchEntry {
    path: PathBuf,
    subscribers: HashMap<u64, u32>,
}

#[cfg(target_os = "linux")]
struct GenerationWatchWorker {
    fd: std::os::fd::OwnedFd,
    next_subscription_id: u64,
    subscriptions: HashMap<u64, ActiveGenerationSubscription>,
    watches_by_descriptor: HashMap<i32, MultiplexedWatchEntry>,
    descriptor_by_path: HashMap<PathBuf, i32>,
}

#[cfg(target_os = "linux")]
impl GenerationWatchWorker {
    fn new(fd: std::os::fd::OwnedFd) -> Self {
        Self {
            fd,
            next_subscription_id: 1,
            subscriptions: HashMap::new(),
            watches_by_descriptor: HashMap::new(),
            descriptor_by_path: HashMap::new(),
        }
    }

    fn run(
        mut self,
        commands: mpsc::Receiver<GenerationWatchCommand>,
        alive: Arc<std::sync::atomic::AtomicBool>,
    ) {
        struct LivenessGuard(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for LivenessGuard {
            fn drop(&mut self) {
                self.0.store(false, std::sync::atomic::Ordering::Release);
            }
        }
        let _liveness = LivenessGuard(alive);
        loop {
            self.drain_events();
            match commands.recv_timeout(Duration::from_millis(5)) {
                Ok(GenerationWatchCommand::Register {
                    specifications,
                    tampered,
                    cancelled,
                    retire_commands,
                    alive,
                    reply,
                }) => {
                    self.drain_events();
                    let outcome = if cancelled.load(std::sync::atomic::Ordering::Acquire) {
                        Err("workspace generation watcher registration was cancelled; no receipt authority was granted".to_string())
                    } else {
                        self.register(specifications, tampered)
                    };
                    if cancelled.load(std::sync::atomic::Ordering::Acquire) {
                        if let Ok(id) = outcome {
                            self.unregister(id);
                        }
                        let _ = reply.send(Err(
                            "workspace generation watcher registration was cancelled; no receipt authority was granted".to_string(),
                        ));
                    } else {
                        let outcome = outcome.map(|id| GenerationWatchAdmission {
                            id: Some(id),
                            commands: retire_commands,
                            alive,
                        });
                        if let Err(error) = reply.send(outcome)
                            && let Ok(mut admission) = error.0
                            && let Some(id) = admission.id.take()
                        {
                            // The acquiring caller cancelled or timed out after
                            // admission. Retire the orphaned subscription before
                            // another generation can reuse its descriptors.
                            self.unregister(id);
                            self.drain_events();
                        }
                    }
                }
                Ok(GenerationWatchCommand::Check { id, reply }) => {
                    // This drain is the synchronization fence between a
                    // filesystem event and a receipt authority check.
                    self.drain_events();
                    let valid = self.subscriptions.get(&id).is_some_and(|subscription| {
                        !subscription
                            .tampered
                            .load(std::sync::atomic::Ordering::Acquire)
                    });
                    let _ = reply.send(valid);
                }
                Ok(GenerationWatchCommand::Unregister { id }) => {
                    self.drain_events();
                    self.unregister(id);
                    // `inotify_rm_watch` queues IN_IGNORED. Drain it before a
                    // later registration can receive a recycled descriptor.
                    self.drain_events();
                }
                Ok(GenerationWatchCommand::Retire { id, reply }) => {
                    self.drain_events();
                    self.unregister(id);
                    self.drain_events();
                    if let Some(reply) = reply {
                        let _ = reply.send(());
                    }
                }
                #[cfg(test)]
                Ok(GenerationWatchCommand::Stats { reply }) => {
                    self.drain_events();
                    let _ =
                        reply.send((self.subscriptions.len(), self.watches_by_descriptor.len()));
                }
                #[cfg(test)]
                Ok(GenerationWatchCommand::ContainsPath { path, reply }) => {
                    self.drain_events();
                    let _ = reply.send(self.descriptor_by_path.contains_key(&path));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        for subscription in self.subscriptions.values() {
            subscription
                .tampered
                .store(true, std::sync::atomic::Ordering::Release);
        }
    }

    fn register(
        &mut self,
        specifications: Vec<GenerationWatchSpec>,
        tampered: Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<u64, String> {
        let additional_paths = specifications
            .iter()
            .filter(|specification| !self.descriptor_by_path.contains_key(&specification.path))
            .count();
        if self
            .descriptor_by_path
            .len()
            .saturating_add(additional_paths)
            > MAX_ACTIVE_GENERATION_WATCH_PATHS
        {
            return Err(format!(
                "workspace generation watcher capacity exceeded (active_paths={}, requested_new_paths={}, limit={MAX_ACTIVE_GENERATION_WATCH_PATHS}); no receipt authority was granted",
                self.descriptor_by_path.len(),
                additional_paths,
            ));
        }

        let id = self.next_subscription_id;
        self.next_subscription_id = self.next_subscription_id.wrapping_add(1).max(1);
        self.subscriptions.insert(
            id,
            ActiveGenerationSubscription {
                tampered,
                watches: Vec::with_capacity(specifications.len()),
            },
        );
        for specification in specifications {
            let descriptor = match self.add_watch(&specification.path, specification.mask) {
                Ok(descriptor) => descriptor,
                Err(error) => {
                    self.unregister(id);
                    self.drain_events();
                    return Err(format!(
                        "failed to arm workspace generation watcher for {}: {error}; no receipt authority was granted",
                        specification.path.display()
                    ));
                }
            };
            let entry = self
                .watches_by_descriptor
                .entry(descriptor)
                .or_insert_with(|| MultiplexedWatchEntry {
                    path: specification.path.clone(),
                    subscribers: HashMap::new(),
                });
            if entry.path != specification.path {
                self.fail_all();
                self.unregister(id);
                return Err(
                    "inotify descriptor was reused before its retired generation settled; all workspace receipt authority was revoked"
                        .to_string(),
                );
            }
            entry.subscribers.insert(id, specification.mask);
            self.descriptor_by_path
                .insert(specification.path, descriptor);
            self.subscriptions
                .get_mut(&id)
                .expect("new generation subscription")
                .watches
                .push((descriptor, specification.mask));
        }
        Ok(id)
    }

    fn add_watch(&self, path: &Path, mask: u32) -> std::io::Result<i32> {
        use std::ffi::CString;
        use std::os::fd::AsRawFd;
        use std::os::unix::ffi::OsStrExt;

        let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "workspace coordination path contains an interior NUL",
            )
        })?;
        let descriptor = unsafe {
            libc::inotify_add_watch(
                self.fd.as_raw_fd(),
                path.as_ptr(),
                mask | libc::IN_DONT_FOLLOW | libc::IN_MASK_ADD,
            )
        };
        if descriptor < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(descriptor)
        }
    }

    fn unregister(&mut self, id: u64) {
        use std::os::fd::AsRawFd;

        let Some(subscription) = self.subscriptions.remove(&id) else {
            return;
        };
        let mut retired = Vec::new();
        for (descriptor, _) in subscription.watches {
            let Some(entry) = self.watches_by_descriptor.get_mut(&descriptor) else {
                continue;
            };
            entry.subscribers.remove(&id);
            if entry.subscribers.is_empty() {
                retired.push((descriptor, entry.path.clone()));
            }
        }
        for (descriptor, path) in retired {
            self.watches_by_descriptor.remove(&descriptor);
            self.descriptor_by_path.remove(&path);
            let _ = unsafe { libc::inotify_rm_watch(self.fd.as_raw_fd(), descriptor) };
        }
    }

    fn fail_all(&self) {
        for subscription in self.subscriptions.values() {
            subscription
                .tampered
                .store(true, std::sync::atomic::Ordering::Release);
        }
    }

    fn drain_events(&mut self) {
        use std::mem::size_of;
        use std::os::fd::AsRawFd;

        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = unsafe {
                libc::read(
                    self.fd.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                )
            };
            if read > 0 {
                let mut offset = 0usize;
                let read = read as usize;
                while offset.saturating_add(size_of::<libc::inotify_event>()) <= read {
                    let event = unsafe {
                        buffer
                            .as_ptr()
                            .add(offset)
                            .cast::<libc::inotify_event>()
                            .read_unaligned()
                    };
                    let record_len =
                        size_of::<libc::inotify_event>().saturating_add(event.len as usize);
                    if record_len == 0 || offset.saturating_add(record_len) > read {
                        self.fail_all();
                        return;
                    }
                    self.apply_event(event.wd, event.mask);
                    offset = offset.saturating_add(record_len);
                }
                continue;
            }
            if read == 0 {
                self.fail_all();
                return;
            }
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return;
            }
            if error.kind() != std::io::ErrorKind::Interrupted {
                self.fail_all();
                return;
            }
        }
    }

    fn apply_event(&mut self, descriptor: i32, mask: u32) {
        if mask & libc::IN_Q_OVERFLOW != 0 {
            self.fail_all();
            return;
        }
        let terminal = libc::IN_IGNORED | libc::IN_UNMOUNT;
        let affected = self
            .watches_by_descriptor
            .get(&descriptor)
            .map(|entry| {
                entry
                    .subscribers
                    .iter()
                    .filter_map(|(id, requested)| {
                        (mask & (*requested | terminal) != 0).then_some(*id)
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for id in affected {
            if let Some(subscription) = self.subscriptions.get(&id) {
                subscription
                    .tampered
                    .store(true, std::sync::atomic::Ordering::Release);
            }
        }
        if mask & libc::IN_IGNORED != 0
            && let Some(entry) = self.watches_by_descriptor.remove(&descriptor)
        {
            self.descriptor_by_path.remove(&entry.path);
        }
    }
}

#[cfg(target_os = "linux")]
impl GenerationWatchRegistry {
    fn start() -> std::io::Result<Arc<Self>> {
        use std::os::fd::FromRawFd;

        let raw_fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
        if raw_fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw_fd) };
        let (commands, receiver) = mpsc::channel();
        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let worker_alive = alive.clone();
        thread::Builder::new()
            .name("astra-generation-watch".to_string())
            .spawn(move || GenerationWatchWorker::new(fd).run(receiver, worker_alive))?;
        Ok(Arc::new(Self { commands, alive }))
    }

    fn register(
        self: &Arc<Self>,
        specifications: Vec<GenerationWatchSpec>,
        cancel_token: Option<&CancellationToken>,
    ) -> std::io::Result<GenerationWatchSubscription> {
        if specifications.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "workspace generation watcher received no paths",
            ));
        }
        let tampered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (reply, response) = mpsc::sync_channel(1);
        self.commands
            .send(GenerationWatchCommand::Register {
                specifications,
                tampered: tampered.clone(),
                cancelled: cancelled.clone(),
                retire_commands: self.commands.clone(),
                alive: self.alive.clone(),
                reply,
            })
            .map_err(|_| {
                std::io::Error::other(
                    "workspace generation watcher exited; no receipt authority was granted",
                )
            })?;
        let deadline = Instant::now() + GENERATION_WATCH_CONTROL_TIMEOUT;
        let admission = loop {
            if cancel_token.is_some_and(CancellationToken::is_cancelled) {
                cancelled.store(true, std::sync::atomic::Ordering::Release);
                if let Ok(Ok(admission)) = response.recv_timeout(GENERATION_WATCH_CONTROL_TIMEOUT) {
                    admission.retire_and_wait();
                }
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "workspace generation watcher registration was cancelled; no receipt authority was granted",
                ));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                cancelled.store(true, std::sync::atomic::Ordering::Release);
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "workspace generation watcher did not acknowledge registration; no receipt authority was granted",
                ));
            }
            match response.recv_timeout(remaining.min(Duration::from_millis(5))) {
                Ok(outcome) => break outcome.map_err(std::io::Error::other)?,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(std::io::Error::other(
                        "workspace generation watcher exited during registration; no receipt authority was granted",
                    ));
                }
            }
        };
        if cancel_token.is_some_and(CancellationToken::is_cancelled) {
            cancelled.store(true, std::sync::atomic::Ordering::Release);
            admission.retire_and_wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "workspace generation watcher registration was cancelled after admission; no receipt authority was granted",
            ));
        }
        let id = admission.commit();
        Ok(GenerationWatchSubscription {
            id,
            tampered,
            registry: self.clone(),
        })
    }

    #[cfg(test)]
    fn stats(&self) -> Option<(usize, usize)> {
        let (reply, response) = mpsc::sync_channel(1);
        self.commands
            .send(GenerationWatchCommand::Stats { reply })
            .ok()?;
        response.recv_timeout(GENERATION_WATCH_CONTROL_TIMEOUT).ok()
    }

    #[cfg(test)]
    fn contains_path(&self, path: PathBuf) -> Option<bool> {
        let (reply, response) = mpsc::sync_channel(1);
        self.commands
            .send(GenerationWatchCommand::ContainsPath { path, reply })
            .ok()?;
        response.recv_timeout(GENERATION_WATCH_CONTROL_TIMEOUT).ok()
    }
}

#[cfg(target_os = "linux")]
static GENERATION_WATCH_REGISTRY: OnceLock<Result<Arc<GenerationWatchRegistry>, String>> =
    OnceLock::new();

#[cfg(target_os = "linux")]
fn generation_watch_registry() -> std::io::Result<Arc<GenerationWatchRegistry>> {
    match GENERATION_WATCH_REGISTRY.get_or_init(|| {
        GenerationWatchRegistry::start().map_err(|error| {
            format!("failed to start process-level workspace generation watcher: {error}")
        })
    }) {
        Ok(registry) => Ok(registry.clone()),
        Err(error) => Err(std::io::Error::other(error.clone())),
    }
}

impl Drop for WorkspaceObservationLease {
    fn drop(&mut self) {
        self.gate.store(false, std::sync::atomic::Ordering::Release);
    }
}

fn observation_gate(workspace_root: &Path) -> Option<Arc<std::sync::atomic::AtomicBool>> {
    let key = workspace_root.canonicalize().ok()?;
    let gates = OBSERVATION_GATES.get_or_init(|| std::sync::Mutex::new(Default::default()));
    let mut map = gates.lock().ok()?;
    map.retain(|_, weak| weak.strong_count() > 0);
    if let Some(gate) = map.get(&key).and_then(std::sync::Weak::upgrade) {
        return Some(gate);
    }
    let gate = Arc::new(std::sync::atomic::AtomicBool::new(false));
    map.insert(key, Arc::downgrade(&gate));
    Some(gate)
}

#[derive(Clone, Copy)]
enum CoordinationLockKind {
    Observation,
}

impl CoordinationLockKind {
    fn suffix(self) -> &'static str {
        match self {
            Self::Observation => "observation",
        }
    }
}

#[derive(Clone, Copy)]
enum CrossProcessLockMode {
    Exclusive,
}

struct CrossProcessFileLock {
    // Linux abstract-UDS names are kernel-owned and cannot be unlinked or
    // renamed by a same-UID tool. The file remains a separate receipt-
    // integrity witness; neither layer trusts peer-controlled contents.
    _kernel_namespace: CrossProcessKernelLock,
    file: std::fs::File,
    path: PathBuf,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    owner_uid: u32,
    #[cfg(unix)]
    mode: u32,
}

struct CrossProcessKernelLock {
    #[cfg(target_os = "linux")]
    _fd: std::os::fd::OwnedFd,
}

#[derive(Debug)]
struct WorkspacePathIdentity {
    path: PathBuf,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    file_type: u32,
}

impl WorkspacePathIdentity {
    fn capture(path: PathBuf) -> Option<Self> {
        let metadata = fs::symlink_metadata(&path).ok()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Some(Self {
                path,
                device: metadata.dev(),
                inode: metadata.ino(),
                file_type: metadata.mode() & libc::S_IFMT,
            })
        }
        #[cfg(not(unix))]
        {
            let _ = metadata;
            Some(Self { path })
        }
    }

    fn is_unchanged(&self) -> bool {
        let Ok(metadata) = fs::symlink_metadata(&self.path) else {
            return false;
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            metadata.dev() == self.device
                && metadata.ino() == self.inode
                && metadata.mode() & libc::S_IFMT == self.file_type
        }
        #[cfg(not(unix))]
        {
            let _ = metadata;
            true
        }
    }
}

/// Admission identity for every lexical path component plus the resolved
/// target. Capturing the component chain closes parent-directory and symlink
/// replacement races which a final `canonicalize()` check alone misses.
#[derive(Debug)]
struct WorkspaceBindingIdentity {
    canonical: PathBuf,
    path_components: Vec<WorkspacePathIdentity>,
}

impl WorkspaceBindingIdentity {
    fn capture(workspace_root: &Path) -> Option<Self> {
        let lexical = workspace_lexical_key(workspace_root)?;
        let canonical = workspace_root.canonicalize().ok()?;
        let mut path_components = Vec::new();
        for binding_path in [&lexical, &canonical] {
            let mut current = PathBuf::new();
            for component in binding_path.components() {
                current.push(component.as_os_str());
                if !path_components
                    .iter()
                    .any(|identity: &WorkspacePathIdentity| identity.path == current)
                {
                    path_components.push(WorkspacePathIdentity::capture(current.clone())?);
                }
            }
        }
        Some(Self {
            canonical,
            path_components,
        })
    }

    fn is_unchanged(&self) -> bool {
        self.path_components
            .iter()
            .all(WorkspacePathIdentity::is_unchanged)
            && self
                .path_components
                .last()
                .and_then(|identity| identity.path.canonicalize().ok())
                .as_ref()
                == Some(&self.canonical)
    }
}

impl Drop for CrossProcessFileLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

impl CrossProcessFileLock {
    fn path_identity_is_unchanged(&self) -> bool {
        let Ok(metadata) = fs::symlink_metadata(&self.path) else {
            return false;
        };
        if !metadata.file_type().is_file() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            metadata.dev() == self.device
                && metadata.ino() == self.inode
                && metadata.nlink() == 1
                && metadata.len() == 0
                && metadata.uid() == self.owner_uid
                && metadata.mode() & 0o777 == self.mode
        }
        #[cfg(not(unix))]
        {
            // Windows shell results never receive authoritative process
            // ownership today. Existence/type is still checked so a removed
            // coordination file cannot silently look healthy.
            true
        }
    }
}

fn stable_coordination_root() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        let root = PathBuf::from("/tmp");
        let metadata = fs::symlink_metadata(&root).ok()?;
        // A root-owned sticky directory is the only unprivileged namespace in
        // which a different OS user cannot unlink this user's lock file. If a
        // host does not provide that contract, mutation execution is rejected
        // rather than silently falling back to a workspace-local generation.
        (metadata.is_dir()
            && metadata.uid() == 0
            && metadata.mode() & libc::S_ISVTX != 0
            && metadata.mode() & 0o002 != 0)
            .then_some(root)
    }
    #[cfg(not(target_os = "linux"))]
    {
        // No cross-user stable namespace has been established for this
        // platform. Callers fail closed and do not claim receipt authority.
        None
    }
}

fn coordination_key_digest(path: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"astra-workspace-coordination-v4\0");
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        hasher.update(path.as_os_str().as_bytes());
    }
    #[cfg(not(unix))]
    hasher.update(path.as_os_str().to_string_lossy().as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CoordinationLockSpec {
    /// Kernel-owned, cross-UID mutual exclusion key. No filesystem owner can
    /// replace, unlink, or lend authority through this name.
    kernel_namespace_key: String,
    /// Persistent integrity witness owned and trusted only by this effective
    /// UID. A later UID uses a distinct inode after acquiring the same global
    /// kernel key.
    witness_path: PathBuf,
}

fn workspace_coordination_lock_specs_for_uid(
    workspace_root: &Path,
    kind: CoordinationLockKind,
    effective_uid: u32,
) -> Option<(Vec<CoordinationLockSpec>, WorkspaceBindingIdentity)> {
    let coordination_root = stable_coordination_root()?;
    let binding_identity = WorkspaceBindingIdentity::capture(workspace_root)?;
    let lexical = workspace_lexical_key(workspace_root)?;
    let mut keys = vec![lexical];
    if !keys.iter().any(|key| key == &binding_identity.canonical) {
        keys.push(binding_identity.canonical.clone());
    }
    let mut specifications = keys
        .iter()
        .map(|key| {
            let digest = coordination_key_digest(key);
            CoordinationLockSpec {
                kernel_namespace_key: format!("astra-ws-v2-{digest}-{}", kind.suffix()),
                witness_path: coordination_root.join(format!(
                    ".astra-workspace-{digest}-{}-uid-{effective_uid}.lock",
                    kind.suffix()
                )),
            }
        })
        .collect::<Vec<_>>();
    specifications
        .sort_by(|left, right| left.kernel_namespace_key.cmp(&right.kernel_namespace_key));
    specifications.dedup_by(|left, right| left.kernel_namespace_key == right.kernel_namespace_key);
    Some((specifications, binding_identity))
}

fn workspace_coordination_lock_specs(
    workspace_root: &Path,
    kind: CoordinationLockKind,
) -> Option<(Vec<CoordinationLockSpec>, WorkspaceBindingIdentity)> {
    #[cfg(unix)]
    let effective_uid = unsafe { libc::geteuid() };
    #[cfg(not(unix))]
    let effective_uid = 0;
    workspace_coordination_lock_specs_for_uid(workspace_root, kind, effective_uid)
}

fn workspace_coordination_lock_paths(
    workspace_root: &Path,
    kind: CoordinationLockKind,
) -> Option<(Vec<PathBuf>, WorkspaceBindingIdentity)> {
    workspace_coordination_lock_specs(workspace_root, kind).map(
        |(specifications, binding_identity)| {
            (
                specifications
                    .into_iter()
                    .map(|specification| specification.witness_path)
                    .collect(),
                binding_identity,
            )
        },
    )
}

/// Return the deterministic external coordination files for diagnostics and
/// tamper tests. The paths contain no secret and their contents are never
/// trusted; callers must not create or replace them during normal execution.
pub fn workspace_coordination_paths_for_diagnostics(workspace_root: &Path) -> Option<Vec<PathBuf>> {
    workspace_coordination_lock_paths(workspace_root, CoordinationLockKind::Observation)
        .map(|(paths, _)| paths)
}

fn open_coordination_lock(path: &Path) -> Option<std::fs::File> {
    let configure_safe = |options: &mut fs::OpenOptions| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        }
    };
    let mut create_options = fs::OpenOptions::new();
    create_options
        .read(true)
        .write(true)
        .truncate(false)
        .create_new(true);
    configure_safe(&mut create_options);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        create_options.mode(0o600);
    }
    let created_file = match create_options.open(path) {
        Ok(file) => Some(file),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => None,
        Err(_) => return None,
    };
    #[cfg(unix)]
    if let Some(file) = created_file.as_ref() {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .ok()?;
    }
    // Advisory flock does not need write access. Reopen read-only so a
    // legitimate contending Astra process cannot generate a write-close
    // tamper event merely by waiting on the same stable inode.
    drop(created_file);
    let mut existing_options = fs::OpenOptions::new();
    existing_options.read(true);
    configure_safe(&mut existing_options);
    let file = existing_options.open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() != 0 {
        return None;
    }
    #[cfg(unix)]
    {
        // Predictable names in /tmp are safe only when this process created
        // (or already owns) the inode. A foreign precreation is a denial of
        // service, never a second lock generation or trusted lock contents.
        if !unix_coordination_lock_metadata_is_trusted(&metadata, unsafe { libc::geteuid() }) {
            return None;
        }
    }
    Some(file)
}

#[cfg(unix)]
fn unix_coordination_lock_metadata_is_trusted(metadata: &fs::Metadata, effective_uid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.is_file()
        && metadata.uid() == effective_uid
        && metadata.nlink() == 1
        && metadata.len() == 0
        && metadata.mode() & 0o777 == 0o600
}

fn locked_coordination_file(
    file: std::fs::File,
    path: PathBuf,
    kernel_namespace: CrossProcessKernelLock,
) -> Option<CrossProcessFileLock> {
    let metadata = file.metadata().ok()?;
    Some(CrossProcessFileLock {
        _kernel_namespace: kernel_namespace,
        file,
        path,
        #[cfg(unix)]
        device: {
            use std::os::unix::fs::MetadataExt;
            metadata.dev()
        },
        #[cfg(unix)]
        inode: {
            use std::os::unix::fs::MetadataExt;
            metadata.ino()
        },
        #[cfg(unix)]
        owner_uid: {
            use std::os::unix::fs::MetadataExt;
            metadata.uid()
        },
        #[cfg(unix)]
        mode: {
            use std::os::unix::fs::MetadataExt;
            metadata.mode() & 0o777
        },
    })
}

/// Reserve a process-private, kernel-owned global name for one deterministic
/// coordination key. Abstract Unix sockets have no filesystem entry for a
/// tool to unlink/rename, are unique across OS users, and disappear when the
/// last owning descriptor closes (including process crash). An existing bind
/// is only a contention fact; no bytes or peer identity are trusted.
fn try_acquire_kernel_coordination_namespace(
    namespace_key: &str,
) -> std::io::Result<Option<CrossProcessKernelLock>> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::FromRawFd;

        let raw_fd = unsafe {
            libc::socket(
                libc::AF_UNIX,
                libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                0,
            )
        };
        if raw_fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw_fd) };
        let name = namespace_key.as_bytes();
        let mut address = unsafe { std::mem::zeroed::<libc::sockaddr_un>() };
        address.sun_family = libc::AF_UNIX as libc::sa_family_t;
        if name.len().saturating_add(1) > address.sun_path.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "workspace coordination namespace name is too long",
            ));
        }
        // sun_path[0] == NUL selects Linux's abstract namespace. The name is
        // a fixed-domain SHA-256 digest and contains no caller-controlled NUL.
        for (slot, byte) in address.sun_path[1..].iter_mut().zip(name.iter().copied()) {
            *slot = byte as libc::c_char;
        }
        let address_len = std::mem::offset_of!(libc::sockaddr_un, sun_path)
            .saturating_add(1)
            .saturating_add(name.len());
        let result = unsafe {
            libc::bind(
                raw_fd,
                (&raw const address).cast::<libc::sockaddr>(),
                address_len as libc::socklen_t,
            )
        };
        if result == 0 {
            return Ok(Some(CrossProcessKernelLock { _fd: fd }));
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EADDRINUSE) {
            return Ok(None);
        }
        Err(error)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = namespace_key;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "kernel workspace coordination namespace is unavailable",
        ))
    }
}

fn try_lock_coordination_file(
    file: &std::fs::File,
    mode: CrossProcessLockMode,
) -> std::io::Result<bool> {
    let result = match mode {
        CrossProcessLockMode::Exclusive => fs2::FileExt::try_lock_exclusive(file),
    };
    match result {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
        Err(error) => Err(error),
    }
}

async fn acquire_cross_process_lock_async(
    specification: CoordinationLockSpec,
    mode: CrossProcessLockMode,
    cancel_token: Option<&CancellationToken>,
    max_wait: Duration,
) -> Option<CrossProcessFileLock> {
    let deadline = tokio::time::Instant::now() + max_wait;
    let kernel_namespace = loop {
        if cancel_token.is_some_and(CancellationToken::is_cancelled)
            || tokio::time::Instant::now() >= deadline
        {
            return None;
        }
        match try_acquire_kernel_coordination_namespace(&specification.kernel_namespace_key) {
            Ok(Some(namespace)) => break namespace,
            Ok(None) => {}
            Err(_) => return None,
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let delay = tokio::time::sleep(remaining.min(Duration::from_millis(5)));
        tokio::pin!(delay);
        if let Some(cancel_token) = cancel_token {
            tokio::select! {
                biased;
                _ = cancel_token.cancelled() => return None,
                _ = &mut delay => {},
            }
        } else {
            delay.await;
        }
    };
    let file = open_coordination_lock(&specification.witness_path)?;
    loop {
        if cancel_token.is_some_and(CancellationToken::is_cancelled)
            || tokio::time::Instant::now() >= deadline
        {
            return None;
        }
        match try_lock_coordination_file(&file, mode) {
            Ok(true) => {
                if cancel_token.is_some_and(CancellationToken::is_cancelled)
                    || tokio::time::Instant::now() >= deadline
                {
                    let _ = fs2::FileExt::unlock(&file);
                    return None;
                }
                return locked_coordination_file(
                    file,
                    specification.witness_path,
                    kernel_namespace,
                );
            }
            Ok(false) => {}
            Err(_) => return None,
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let delay = tokio::time::sleep(remaining.min(Duration::from_millis(5)));
        tokio::pin!(delay);
        if let Some(cancel_token) = cancel_token {
            tokio::select! {
                biased;
                _ = cancel_token.cancelled() => return None,
                _ = &mut delay => {},
            }
        } else {
            delay.await;
        }
    }
}

fn acquire_cross_process_lock_sync(
    specification: CoordinationLockSpec,
    mode: CrossProcessLockMode,
    cancel_token: Option<&CancellationToken>,
    max_wait: Duration,
) -> Option<CrossProcessFileLock> {
    let deadline = Instant::now() + max_wait;
    let kernel_namespace = loop {
        if cancel_token.is_some_and(CancellationToken::is_cancelled) || Instant::now() >= deadline {
            return None;
        }
        match try_acquire_kernel_coordination_namespace(&specification.kernel_namespace_key) {
            Ok(Some(namespace)) => break namespace,
            Ok(None) => {}
            Err(_) => return None,
        }
        thread::sleep(Duration::from_millis(5));
    };
    let file = open_coordination_lock(&specification.witness_path)?;
    loop {
        if cancel_token.is_some_and(CancellationToken::is_cancelled) || Instant::now() >= deadline {
            return None;
        }
        match try_lock_coordination_file(&file, mode) {
            Ok(true) => {
                if cancel_token.is_some_and(CancellationToken::is_cancelled)
                    || Instant::now() >= deadline
                {
                    let _ = fs2::FileExt::unlock(&file);
                    return None;
                }
                return locked_coordination_file(
                    file,
                    specification.witness_path,
                    kernel_namespace,
                );
            }
            Ok(false) => {}
            Err(_) => return None,
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// Serialize one pre→execute→post observation window per bound workspace.
/// Different workspaces remain concurrent; overlapping calls on the same
/// workspace cannot attribute one caller's delta to another caller.
pub async fn acquire_workspace_observation_lease(
    workspace_root: &Path,
) -> Option<WorkspaceObservationLease> {
    acquire_workspace_observation_lease_with_options(workspace_root, None, DEFAULT_LEASE_WAIT).await
}

/// Acquire the per-workspace lease without allowing queueing to outlive the
/// caller's cancellation/deadline. `None` means the workspace could not be
/// canonicalized, the wait expired, or cancellation won; callers must return
/// an error rather than executing unobserved.
pub async fn acquire_workspace_observation_lease_with_options(
    workspace_root: &Path,
    cancel_token: Option<&CancellationToken>,
    max_wait: Duration,
) -> Option<WorkspaceObservationLease> {
    acquire_workspace_lease_async(workspace_root, cancel_token, max_wait).await
}

/// Serialize a typed workspace mutation against Bash observation windows.
///
/// Top-level typed, Bash, and `run_script` writers all take this same
/// exclusive generation. Authenticated `run_script` RPC callbacks are the
/// sole re-entrant route and deliberately skip acquisition at their executor
/// boundary, relying on the still-live parent lease.
pub async fn acquire_workspace_mutation_lease_with_options(
    workspace_root: &Path,
    cancel_token: Option<&CancellationToken>,
    max_wait: Duration,
) -> Option<WorkspaceObservationLease> {
    acquire_workspace_lease_async(workspace_root, cancel_token, max_wait).await
}

/// Synchronous counterpart used by blocking shell adapters.
pub fn acquire_workspace_mutation_lease_sync_with_options(
    workspace_root: &Path,
    cancel_token: Option<&CancellationToken>,
    max_wait: Duration,
) -> Option<WorkspaceObservationLease> {
    acquire_workspace_lease_sync(workspace_root, cancel_token, max_wait)
}

async fn acquire_workspace_lease_async(
    workspace_root: &Path,
    cancel_token: Option<&CancellationToken>,
    max_wait: Duration,
) -> Option<WorkspaceObservationLease> {
    let deadline = tokio::time::Instant::now() + max_wait;
    let writer_state = writer_epoch_state(workspace_root)?;
    if writer_state
        .quarantined
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return None;
    }
    let (lock_specifications, binding_identity) =
        workspace_coordination_lock_specs(workspace_root, CoordinationLockKind::Observation)?;
    let trusted_coordination_root = stable_coordination_root()?;
    let gate = observation_gate(workspace_root)?;
    loop {
        if cancel_token.is_some_and(CancellationToken::is_cancelled)
            || tokio::time::Instant::now() >= deadline
        {
            return None;
        }
        if gate
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
        {
            // Cancellation/deadline may race the CAS.  Never hand out a
            // lease after the caller has stopped waiting; release it before
            // returning so the next caller is not stranded behind a ghost
            // owner.
            if cancel_token.is_some_and(CancellationToken::is_cancelled)
                || tokio::time::Instant::now() >= deadline
            {
                gate.store(false, std::sync::atomic::Ordering::Release);
                return None;
            }
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let delay = tokio::time::sleep(remaining.min(Duration::from_millis(5)));
        tokio::pin!(delay);
        if let Some(cancel_token) = cancel_token {
            tokio::select! {
                biased;
                _ = cancel_token.cancelled() => return None,
                _ = &mut delay => {},
            }
        } else {
            delay.await;
        }
    }
    let mut locks = Vec::with_capacity(lock_specifications.len());
    for specification in lock_specifications {
        let lock = acquire_cross_process_lock_async(
            specification,
            CrossProcessLockMode::Exclusive,
            cancel_token,
            deadline.saturating_duration_since(tokio::time::Instant::now()),
        )
        .await;
        let Some(lock) = lock else {
            gate.store(false, std::sync::atomic::Ordering::Release);
            return None;
        };
        locks.push(lock);
    }
    let watch_lock_paths = locks
        .iter()
        .map(|lock| lock.path.clone())
        .collect::<Vec<_>>();
    let watch_binding_paths = binding_identity
        .path_components
        .iter()
        // The stable root and its ancestors are privileged host namespace.
        // Watching `/tmp` itself would let unrelated users' traffic revoke
        // every active lease.
        .filter(|identity| !trusted_coordination_root.starts_with(&identity.path))
        .map(|identity| identity.path.clone())
        .collect::<Vec<_>>();
    let watcher_cancel = cancel_token.cloned();
    let tamper_watch = tokio::task::spawn_blocking(move || {
        let tamper_watch = GenerationTamperWatch::arm(
            watch_lock_paths,
            watch_binding_paths,
            watcher_cancel.as_ref(),
        )?;
        if !tamper_watch.is_untampered() {
            return Err(std::io::Error::other(
                "workspace generation watcher was revoked before admission completed",
            ));
        }
        Ok(tamper_watch)
    })
    .await;
    let tamper_watch = match tamper_watch {
        Ok(Ok(tamper_watch)) => tamper_watch,
        Ok(Err(error)) => {
            tracing::warn!(
                workspace_root = %workspace_root.display(),
                error = %error,
                "workspace generation watcher refused receipt authority"
            );
            gate.store(false, std::sync::atomic::Ordering::Release);
            return None;
        }
        Err(error) => {
            tracing::warn!(
                workspace_root = %workspace_root.display(),
                error = %error,
                "workspace generation watcher worker join failed; no receipt authority granted"
            );
            gate.store(false, std::sync::atomic::Ordering::Release);
            return None;
        }
    };
    let binding_unchanged = binding_identity.is_unchanged();
    // The control-plane fence ran on the blocking worker above; do not park
    // a Tokio runtime worker on an inotify acknowledgement.
    let tamper_untampered = true;
    let quarantined = writer_state
        .quarantined
        .load(std::sync::atomic::Ordering::Acquire);
    if !binding_unchanged || !tamper_untampered || quarantined {
        gate.store(false, std::sync::atomic::Ordering::Release);
        return None;
    }
    Some(WorkspaceObservationLease {
        gate,
        locks,
        binding_identity,
        writer_state,
        tamper_watch,
    })
}

/// Synchronous counterpart for legacy edge paths that run on a blocking
/// worker. It uses the same atomic gate as async callers, so the two routes
/// cannot overlap one workspace observation window.
pub fn acquire_workspace_observation_lease_sync(
    workspace_root: &Path,
    max_wait: Duration,
) -> Option<WorkspaceObservationLease> {
    acquire_workspace_observation_lease_sync_with_options(workspace_root, None, max_wait)
}

/// Blocking counterpart with the same cancellation contract as the async
/// acquisition path.  The caller is already on a blocking worker, so a
/// short polling interval keeps cancellation responsive without parking a
/// Tokio task or holding a runtime mutex.
pub fn acquire_workspace_observation_lease_sync_with_options(
    workspace_root: &Path,
    cancel_token: Option<&CancellationToken>,
    max_wait: Duration,
) -> Option<WorkspaceObservationLease> {
    acquire_workspace_lease_sync(workspace_root, cancel_token, max_wait)
}

fn acquire_workspace_lease_sync(
    workspace_root: &Path,
    cancel_token: Option<&CancellationToken>,
    max_wait: Duration,
) -> Option<WorkspaceObservationLease> {
    let deadline = Instant::now() + max_wait;
    let writer_state = writer_epoch_state(workspace_root)?;
    if writer_state
        .quarantined
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return None;
    }
    let (lock_specifications, binding_identity) =
        workspace_coordination_lock_specs(workspace_root, CoordinationLockKind::Observation)?;
    let trusted_coordination_root = stable_coordination_root()?;
    let gate = observation_gate(workspace_root)?;
    loop {
        if cancel_token.is_some_and(CancellationToken::is_cancelled) || Instant::now() >= deadline {
            return None;
        }
        if gate
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
        {
            if cancel_token.is_some_and(CancellationToken::is_cancelled)
                || Instant::now() >= deadline
            {
                gate.store(false, std::sync::atomic::Ordering::Release);
                return None;
            }
            break;
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(5));
    }
    let mut locks = Vec::with_capacity(lock_specifications.len());
    for specification in lock_specifications {
        let Some(lock) = acquire_cross_process_lock_sync(
            specification,
            CrossProcessLockMode::Exclusive,
            cancel_token,
            deadline.saturating_duration_since(Instant::now()),
        ) else {
            gate.store(false, std::sync::atomic::Ordering::Release);
            return None;
        };
        locks.push(lock);
    }
    let tamper_watch = GenerationTamperWatch::arm(
        locks.iter().map(|lock| lock.path.clone()),
        binding_identity
            .path_components
            .iter()
            .filter(|identity| !trusted_coordination_root.starts_with(&identity.path))
            .map(|identity| identity.path.clone()),
        cancel_token,
    );
    let tamper_watch = match tamper_watch {
        Ok(tamper_watch) => tamper_watch,
        Err(error) => {
            tracing::warn!(
                workspace_root = %workspace_root.display(),
                error = %error,
                "workspace generation watcher refused receipt authority"
            );
            gate.store(false, std::sync::atomic::Ordering::Release);
            return None;
        }
    };
    let binding_unchanged = binding_identity.is_unchanged();
    let tamper_untampered = tamper_watch.is_untampered();
    let quarantined = writer_state
        .quarantined
        .load(std::sync::atomic::Ordering::Acquire);
    if !binding_unchanged || !tamper_untampered || quarantined {
        gate.store(false, std::sync::atomic::Ordering::Release);
        return None;
    }
    Some(WorkspaceObservationLease {
        gate,
        locks,
        binding_identity,
        writer_state,
        tamper_watch,
    })
}

/// Validate the executor-owned receipt before projecting it into a completion
/// obligation.  The boolean/scope fields are intentionally not sufficient:
/// callers must have the schema and provenance marker as well, otherwise a
/// tool or model could self-report a mutation through ordinary metadata.
pub fn is_changed_receipt(receipt: &serde_json::Value) -> bool {
    let ownership_valid = matches!(
        receipt.get("ownership").and_then(serde_json::Value::as_str),
        Some(
            INVOCATION_CGROUP_OWNERSHIP
                | INVOCATION_SUPERVISOR_OWNERSHIP
                | FOREGROUND_PROCESS_GROUP_OWNERSHIP
        )
    );
    ownership_valid
        && receipt.get("schema").and_then(serde_json::Value::as_str)
            == Some("workspace_mutation_receipt.v1")
        && receipt.get("source").and_then(serde_json::Value::as_str)
            == Some("post_execution_fingerprint")
        && receipt.get("scope").and_then(serde_json::Value::as_str) == Some(BOUND_WORKSPACE_SCOPE)
        && receipt.get("changed").and_then(serde_json::Value::as_bool) == Some(true)
}

/// Whether a changed receipt came from an authoritative invocation owner.
/// Both a delegated cgroup and an invocation-private subreaper prove their
/// complete descendant set empty; a foreground process group does not.
pub fn is_authoritative_changed_receipt(receipt: &serde_json::Value) -> bool {
    is_changed_receipt(receipt)
        && matches!(
            receipt.get("ownership").and_then(serde_json::Value::as_str),
            Some(INVOCATION_CGROUP_OWNERSHIP | INVOCATION_SUPERVISOR_OWNERSHIP)
        )
}

/// A foreground process-group receipt is useful evidence about the state seen
/// at the end of one call, but it is not a durable ownership proof: a child
/// can create a new session and write after the leader's process group exits.
/// Consumers on another process (notably the server receiving an Edge result)
/// must derive the same sticky quarantine from the receipt itself rather than
/// relying on the producer's process-local registry.
pub fn is_weak_changed_receipt(receipt: &serde_json::Value) -> bool {
    is_changed_receipt(receipt)
        && receipt.get("ownership").and_then(serde_json::Value::as_str)
            == Some(FOREGROUND_PROCESS_GROUP_OWNERSHIP)
}

#[derive(Debug, Clone)]
pub struct WorkspaceFingerprint {
    digest: u64,
    writer_state: Arc<WriterEpochState>,
    writer_epoch: u64,
    writer_active: usize,
}

/// Executor-owned preimage for an explicit, bounded set of external state
/// roots.  This is intentionally opt-in: scanning arbitrary host state is
/// neither attributable nor affordable, while inferring paths from shell text
/// would make model wording a completion authority.
#[derive(Debug, Clone)]
pub struct ExternalEffectFingerprint {
    targets: Vec<ExternalObservedTarget>,
    target_set_digest: String,
}

#[derive(Debug, Clone)]
struct ExternalObservedTarget {
    declared_path: PathBuf,
    observation_root: PathBuf,
    root_before: WorkspaceFingerprint,
    /// Files and not-yet-created paths use an exact-path conjunct so a
    /// sibling change in their observation parent cannot manufacture proof.
    /// Directories are already their own exact observation root.
    leaf_before: Option<u64>,
}

/// Cross-process leases covering every canonical external observation root.
/// They are held from preimage through postimage so another session cannot
/// donate its mutation to this invocation's receipt.
pub struct ExternalEffectObservationLease {
    leases: Vec<WorkspaceObservationLease>,
}

impl ExternalEffectObservationLease {
    pub fn integrity_valid(&self) -> bool {
        self.leases
            .iter()
            .all(WorkspaceObservationLease::integrity_valid)
    }
}

pub async fn acquire_external_effect_observation_lease_with_options(
    args: &serde_json::Value,
    workspace_root: &Path,
    cancel_token: Option<&CancellationToken>,
    max_wait: Duration,
) -> Result<Option<ExternalEffectObservationLease>, String> {
    let Some(roots) = external_effect_observation_roots(args, workspace_root)? else {
        return Ok(None);
    };
    let deadline = tokio::time::Instant::now() + max_wait;
    let mut leases = Vec::with_capacity(roots.len());
    for root in roots {
        let Some(lease) = acquire_workspace_observation_lease_with_options(
            &root,
            cancel_token,
            deadline.saturating_duration_since(tokio::time::Instant::now()),
        )
        .await
        else {
            return Ok(None);
        };
        leases.push(lease);
    }
    Ok(Some(ExternalEffectObservationLease { leases }))
}

pub fn acquire_external_effect_observation_lease_sync_with_options(
    args: &serde_json::Value,
    workspace_root: &Path,
    cancel_token: Option<&CancellationToken>,
    max_wait: Duration,
) -> Result<Option<ExternalEffectObservationLease>, String> {
    let Some(roots) = external_effect_observation_roots(args, workspace_root)? else {
        return Ok(None);
    };
    let deadline = Instant::now() + max_wait;
    let mut leases = Vec::with_capacity(roots.len());
    for root in roots {
        let Some(lease) = acquire_workspace_observation_lease_sync_with_options(
            &root,
            cancel_token,
            deadline.saturating_duration_since(Instant::now()),
        ) else {
            return Ok(None);
        };
        leases.push(lease);
    }
    Ok(Some(ExternalEffectObservationLease { leases }))
}

fn external_effect_observation_roots(
    args: &serde_json::Value,
    workspace_root: &Path,
) -> Result<Option<Vec<PathBuf>>, String> {
    let Some(raw_paths) = args.get(EXTERNAL_STATE_PATHS_FIELD) else {
        return Ok(None);
    };
    let paths = raw_paths.as_array().ok_or_else(|| {
        format!("{EXTERNAL_STATE_PATHS_FIELD} must be an array of absolute paths")
    })?;
    if paths.is_empty() || paths.len() > MAX_EXTERNAL_STATE_PATHS {
        return Err(format!(
            "{EXTERNAL_STATE_PATHS_FIELD} must contain 1..={MAX_EXTERNAL_STATE_PATHS} paths"
        ));
    }
    let workspace = workspace_root
        .canonicalize()
        .map_err(|e| format!("bound workspace cannot be resolved: {e}"))?;
    let mut roots = Vec::with_capacity(paths.len());
    for raw in paths {
        let raw = raw
            .as_str()
            .ok_or_else(|| format!("{EXTERNAL_STATE_PATHS_FIELD} entries must be strings"))?;
        let target = PathBuf::from(raw);
        if !target.is_absolute()
            || target
                .components()
                .any(|c| matches!(c, Component::ParentDir))
        {
            return Err(format!(
                "external state path must be absolute and traversal-free: {raw}"
            ));
        }
        let root = external_observation_root(&target)
            .ok_or_else(|| format!("external state path has no observable existing root: {raw}"))?;
        let root = root.canonicalize().map_err(|e| {
            format!("external state observation root cannot be resolved for {raw}: {e}")
        })?;
        if root.starts_with(&workspace) || workspace.starts_with(&root) {
            return Err(format!(
                "external state path overlaps the bound workspace and cannot issue an external receipt: {raw}"
            ));
        }
        roots.push(root);
    }
    roots.sort();
    roots.dedup();
    Ok(Some(roots))
}

impl ExternalEffectFingerprint {
    /// Stable identity for the declared external target set. This contains no
    /// host observation and is safe to retain as the correlation key between
    /// a started cancellation and a later executor-owned receipt. The receipt
    /// remains the authority for the observed mutation.
    pub fn declared_target_set_digest_from_args(args: &serde_json::Value) -> Option<String> {
        let raw_paths = args.get(EXTERNAL_STATE_PATHS_FIELD)?.as_array()?;
        if raw_paths.is_empty() || raw_paths.len() > MAX_EXTERNAL_STATE_PATHS {
            return None;
        }
        let mut paths = raw_paths
            .iter()
            .map(|raw| raw.as_str().map(PathBuf::from))
            .collect::<Option<Vec<_>>>()?;
        if paths.iter().any(|path| {
            !path.is_absolute()
                || path
                    .components()
                    .any(|component| matches!(component, Component::ParentDir))
        }) {
            return None;
        }
        paths.sort();
        paths.dedup();
        let mut hasher = Sha256::new();
        for path in paths {
            hasher.update(path.as_os_str().as_encoded_bytes());
            hasher.update([0]);
        }
        Some(hex::encode(hasher.finalize()))
    }

    /// Validate and capture an explicit external-state observation contract.
    ///
    /// Every path must be absolute, traversal-free, outside the bound
    /// workspace, and narrow enough for the existing bounded fingerprint. An
    /// explicit but unobservable target is rejected before execution rather
    /// than silently degrading into exit-status inference.
    pub fn capture_from_args(
        args: &serde_json::Value,
        workspace_root: &Path,
    ) -> Result<Option<Self>, String> {
        let Some(raw_paths) = args.get(EXTERNAL_STATE_PATHS_FIELD) else {
            return Ok(None);
        };
        let paths = raw_paths.as_array().ok_or_else(|| {
            format!("{EXTERNAL_STATE_PATHS_FIELD} must be an array of absolute paths")
        })?;
        if paths.is_empty() || paths.len() > MAX_EXTERNAL_STATE_PATHS {
            return Err(format!(
                "{EXTERNAL_STATE_PATHS_FIELD} must contain 1..={MAX_EXTERNAL_STATE_PATHS} paths"
            ));
        }
        let workspace = workspace_root
            .canonicalize()
            .map_err(|error| format!("bound workspace cannot be resolved: {error}"))?;
        let mut targets = Vec::with_capacity(paths.len());
        for raw in paths {
            let raw = raw
                .as_str()
                .ok_or_else(|| format!("{EXTERNAL_STATE_PATHS_FIELD} entries must be strings"))?;
            let target = PathBuf::from(raw);
            if !target.is_absolute()
                || target
                    .components()
                    .any(|component| matches!(component, Component::ParentDir))
            {
                return Err(format!(
                    "external state path must be absolute and traversal-free: {raw}"
                ));
            }
            let observation_root = external_observation_root(&target).ok_or_else(|| {
                format!("external state path has no observable existing root: {raw}")
            })?;
            let canonical = observation_root.canonicalize().map_err(|error| {
                format!("external state observation root cannot be resolved for {raw}: {error}")
            })?;
            if canonical.starts_with(&workspace) || workspace.starts_with(&canonical) {
                return Err(format!(
                    "external state path overlaps the bound workspace and cannot issue an external receipt: {raw}"
                ));
            }
            targets.push((target, canonical));
        }
        targets.sort_by(|left, right| left.0.cmp(&right.0));
        targets.dedup_by(|left, right| left.0 == right.0);
        let mut target_hasher = Sha256::new();
        let targets = targets
            .iter()
            .map(|(declared_path, observation_root)| {
                target_hasher.update(declared_path.as_os_str().as_encoded_bytes());
                target_hasher.update([0]);
                let root_before = WorkspaceFingerprint::capture(observation_root).ok_or_else(|| {
                    format!(
                        "external state root is unavailable, ambiguous, or exceeds observation bounds: {}",
                        observation_root.display()
                    )
                })?;
                let leaf_before = (!declared_path.is_dir())
                    .then(|| external_leaf_fingerprint(declared_path))
                    .transpose()?;
                Ok::<_, String>(ExternalObservedTarget {
                    declared_path: declared_path.clone(),
                    observation_root: observation_root.clone(),
                    root_before,
                    leaf_before,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Some(Self {
            targets,
            target_set_digest: hex::encode(target_hasher.finalize()),
        }))
    }

    /// Produce a receipt only for an observed delta owned by an authoritative
    /// invocation boundary. Failure to capture any postimage is unknown, not
    /// unchanged and never success evidence.
    pub fn changed_receipt(
        &self,
        ownership: Option<&str>,
    ) -> Option<serde_json::Map<String, serde_json::Value>> {
        if !matches!(
            ownership,
            Some(INVOCATION_CGROUP_OWNERSHIP | INVOCATION_SUPERVISOR_OWNERSHIP)
        ) {
            return None;
        }
        let mut changed = false;
        for target in &self.targets {
            let root_after = WorkspaceFingerprint::capture(&target.observation_root)?;
            let root_changed = target.root_before.changed_from(Some(root_after));
            let leaf_changed = match target.leaf_before {
                Some(before) => external_leaf_fingerprint(&target.declared_path).ok()? != before,
                None => true,
            };
            changed |= root_changed && leaf_changed;
        }
        changed.then(|| {
            external_effect_changed_receipt(
                ownership.expect("authoritative ownership matched"),
                &self.target_set_digest,
                self.targets.len(),
            )
        })
    }

    /// Async callers must use this path: no recursive fingerprint work runs
    /// on a Tokio worker, and a shared cap prevents a burst of sessions from
    /// saturating the blocking pool.
    pub async fn changed_receipt_async(
        &self,
        ownership: Option<&str>,
    ) -> Option<serde_json::Map<String, serde_json::Value>> {
        let permit = EXTERNAL_POSTIMAGE_PERMITS
            .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(8)))
            .clone()
            .acquire_owned()
            .await
            .ok()?;
        let fingerprint = self.clone();
        let ownership = ownership.map(str::to_owned);
        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            fingerprint.changed_receipt(ownership.as_deref())
        })
        .await
        .ok()
        .flatten();
        result
    }
}

fn external_leaf_fingerprint(path: &Path) -> Result<u64, String> {
    let mut hasher = DefaultHasher::new();
    let mut content_bytes = 0usize;
    hash_path_state(path, &mut hasher, &mut content_bytes).ok_or_else(|| {
        format!(
            "external state path cannot be fingerprinted: {}",
            path.display()
        )
    })?;
    Ok(hasher.finish())
}

fn external_observation_root(target: &Path) -> Option<PathBuf> {
    if target.is_dir() {
        return Some(target.to_path_buf());
    }
    let mut candidate = target.parent()?.to_path_buf();
    while !candidate.exists() {
        candidate = candidate.parent()?.to_path_buf();
    }
    Some(candidate)
}

fn external_effect_changed_receipt(
    ownership: &str,
    target_set_digest: &str,
    observed_roots: usize,
) -> serde_json::Map<String, serde_json::Value> {
    serde_json::Map::from_iter([
        (
            EXTERNAL_EFFECT_OBSERVED_FIELD.to_string(),
            serde_json::Value::Bool(true),
        ),
        (
            EXTERNAL_EFFECT_SCOPE_FIELD.to_string(),
            serde_json::Value::String(DECLARED_EXTERNAL_STATE_SCOPE.to_string()),
        ),
        (
            EXTERNAL_EFFECT_RECEIPT_FIELD.to_string(),
            serde_json::json!({
                "schema": "external_effect_receipt.v1",
                "source": "post_execution_fingerprint",
                "scope": DECLARED_EXTERNAL_STATE_SCOPE,
                "changed": true,
                "ownership": ownership,
                "target_set_digest": target_set_digest,
                "observed_roots": observed_roots,
            }),
        ),
    ])
}

/// Validate the durable projection of an executor-owned external delta.
pub fn is_authoritative_external_effect_receipt(receipt: &serde_json::Value) -> bool {
    receipt.get("schema").and_then(serde_json::Value::as_str) == Some("external_effect_receipt.v1")
        && receipt.get("source").and_then(serde_json::Value::as_str)
            == Some("post_execution_fingerprint")
        && receipt.get("scope").and_then(serde_json::Value::as_str)
            == Some(DECLARED_EXTERNAL_STATE_SCOPE)
        && receipt.get("changed").and_then(serde_json::Value::as_bool) == Some(true)
        && matches!(
            receipt.get("ownership").and_then(serde_json::Value::as_str),
            Some(INVOCATION_CGROUP_OWNERSHIP | INVOCATION_SUPERVISOR_OWNERSHIP)
        )
        && receipt
            .get("target_set_digest")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|digest| {
                digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        && receipt
            .get("observed_roots")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|count| (1..=MAX_EXTERNAL_STATE_PATHS as u64).contains(&count))
}

impl WorkspaceFingerprint {
    /// Capture a bounded fingerprint for `root`.
    ///
    /// Git status is the fast path and reports tracked/untracked workspace
    /// changes without walking file contents.  Non-git workspaces use a
    /// bounded metadata manifest.  If the manifest exceeds its bound or any
    /// required metadata cannot be read, return `None` so callers fail closed.
    pub fn capture(root: &Path) -> Option<Self> {
        let root = root.canonicalize().ok()?;
        let writer_state = writer_epoch_state(&root)?;
        let epoch_before = writer_state
            .epoch
            .load(std::sync::atomic::Ordering::Acquire);
        let active_before = writer_state
            .active
            .load(std::sync::atomic::Ordering::Acquire);
        let quarantined_before = writer_state
            .quarantined
            .load(std::sync::atomic::Ordering::Acquire);
        // A recursive/typed writer already in flight makes the snapshot
        // ambiguous.  Do not sample a half-written tree and later attribute
        // it to the surrounding Bash call.
        if active_before != 0 || quarantined_before {
            return None;
        }
        let digest = match git_status_fingerprint(&root) {
            GitFingerprint::Captured(digest) => Some(digest),
            // Do not retry an over-limit/ambiguous Git workspace with a
            // second full manifest: that only blocks the executor again and
            // still cannot produce trustworthy evidence.
            GitFingerprint::Unknown => None,
            GitFingerprint::NotGit => manifest_fingerprint(&root),
        }?;
        let epoch_after = writer_state
            .epoch
            .load(std::sync::atomic::Ordering::Acquire);
        let active_after = writer_state
            .active
            .load(std::sync::atomic::Ordering::Acquire);
        let quarantined_after = writer_state
            .quarantined
            .load(std::sync::atomic::Ordering::Acquire);
        // The writer may have started and finished while the bounded probe
        // was running.  An epoch or active-count change makes this sample
        // un-attributable; fail closed instead of producing a false receipt.
        if epoch_before != epoch_after
            || active_before != active_after
            || active_after != 0
            || quarantined_after
        {
            return None;
        }
        Some(Self {
            digest,
            writer_state,
            writer_epoch: epoch_after,
            writer_active: active_after,
        })
    }

    pub fn changed_from(&self, after: Option<Self>) -> bool {
        let Some(after) = after else {
            return false;
        };
        // A recursive writer may change bytes and then restore them, so a
        // generation mismatch is not positive evidence for Bash.  The
        // entire pre/post interval is simply un-attributable whenever a
        // writer was active or its generation changed.
        if !Arc::ptr_eq(&self.writer_state, &after.writer_state)
            || self.writer_active != 0
            || after.writer_active != 0
            || self.writer_epoch != after.writer_epoch
        {
            return false;
        }
        self.digest != after.digest
    }
}

enum GitFingerprint {
    Captured(u64),
    NotGit,
    Unknown,
}

struct BoundedCommandOutput {
    success: bool,
    stdout: Vec<u8>,
}

/// Run a metadata probe with both a byte cap and a wall-clock deadline.
/// `Command::output()` is intentionally avoided: it buffers an unbounded
/// child stdout before the caller can inspect the size, which would let a
/// large repository hold the workspace lease indefinitely.
fn run_bounded_probe(
    mut command: Command,
    max_stdout_bytes: usize,
    timeout: Duration,
) -> Option<BoundedCommandOutput> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = match command.spawn() {
        Ok(child) => child,
        // A minimal agent image may not contain Git at all.  That is a
        // normal non-Git workspace, so let the caller use the bounded
        // manifest fallback.  Timeouts, output overflow, and other spawn
        // failures remain `None` (ambiguous) and fail closed.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Some(BoundedCommandOutput {
                success: false,
                stdout: Vec::new(),
            });
        }
        Err(_) => return None,
    };
    let Some(stdout) = child.stdout.take() else {
        terminate_probe_child(&mut child);
        return None;
    };
    let (sender, receiver) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut bytes = Vec::with_capacity(max_stdout_bytes.min(64 * 1024));
        let mut limited = stdout.take(max_stdout_bytes.saturating_add(1) as u64);
        let result = limited
            .read_to_end(&mut bytes)
            .ok()
            .and_then(|_| (bytes.len() <= max_stdout_bytes).then_some(bytes));
        let _ = sender.send(result);
    });

    let started = Instant::now();
    loop {
        if let Ok(bytes) = receiver.try_recv() {
            if bytes.is_none() {
                terminate_probe_child(&mut child);
                let _ = reader.join();
                return None;
            }
            let status = loop {
                if let Ok(Some(status)) = child.try_wait() {
                    break status;
                }
                if started.elapsed() >= timeout {
                    terminate_probe_child(&mut child);
                    let _ = reader.join();
                    return None;
                }
                thread::sleep(Duration::from_millis(5));
            };
            let _ = reader.join();
            return Some(BoundedCommandOutput {
                success: status.success(),
                stdout: bytes?,
            });
        }
        if started.elapsed() >= timeout {
            terminate_probe_child(&mut child);
            let _ = reader.join();
            return None;
        }
        if let Ok(Some(status)) = child.try_wait() {
            let bytes = match receiver.recv_timeout(Duration::from_millis(50)) {
                Ok(Some(bytes)) => bytes,
                _ => {
                    // A child can exit while a descendant keeps stdout open.
                    // Do not let the reader thread (and therefore the lease)
                    // wait forever on that inherited pipe.
                    terminate_probe_child(&mut child);
                    let _ = reader.join();
                    return None;
                }
            };
            let _ = reader.join();
            return Some(BoundedCommandOutput {
                success: status.success(),
                stdout: bytes,
            });
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn terminate_probe_child(child: &mut Child) {
    #[cfg(unix)]
    {
        let pid = child.id();
        if pid <= i32::MAX as u32 {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn git_status_fingerprint(root: &Path) -> GitFingerprint {
    let Some(mut git_root_command) = hardened_git_command(root) else {
        // Git is optional in minimal agent images.  Falling back to the
        // bounded manifest is safe; it is materially different from
        // executing a program selected through the caller's PATH.
        return GitFingerprint::NotGit;
    };
    git_root_command.args(["rev-parse", "--show-toplevel"]);
    let Some(git_root_output) =
        run_bounded_probe(git_root_command, 128 * 1024, FINGERPRINT_PROBE_TIMEOUT)
    else {
        return GitFingerprint::Unknown;
    };
    if !git_root_output.success {
        return GitFingerprint::NotGit;
    }
    let Some(git_root) = String::from_utf8(git_root_output.stdout)
        .ok()
        .map(|path| Path::new(path.trim()).to_path_buf())
        .and_then(|path| path.canonicalize().ok())
    else {
        return GitFingerprint::Unknown;
    };
    if !root.starts_with(&git_root) {
        return GitFingerprint::NotGit;
    }
    let Ok(relative_root) = root.strip_prefix(&git_root) else {
        return GitFingerprint::Unknown;
    };
    let tree_spec = if relative_root.as_os_str().is_empty() {
        "HEAD^{tree}".to_string()
    } else {
        format!("HEAD:{}", relative_root.to_string_lossy())
    };
    let Some(mut tree_command) = hardened_git_command(root) else {
        return GitFingerprint::NotGit;
    };
    tree_command.args(["rev-parse", &tree_spec]);
    let Some(tree_output) = run_bounded_probe(tree_command, 128 * 1024, FINGERPRINT_PROBE_TIMEOUT)
    else {
        return GitFingerprint::Unknown;
    };
    let tree_bytes = if tree_output.success {
        tree_output.stdout
    } else {
        // A repository can legitimately have no commit yet, or the bound
        // directory can exist only in the worktree and not in HEAD. Both are
        // deterministic baseline states, not probe failure.
        let Some(mut head_command) = hardened_git_command(root) else {
            return GitFingerprint::NotGit;
        };
        head_command.args(["rev-parse", "--verify", "HEAD"]);
        let Some(head_output) =
            run_bounded_probe(head_command, 128 * 1024, FINGERPRINT_PROBE_TIMEOUT)
        else {
            return GitFingerprint::Unknown;
        };
        if head_output.success {
            b"astra-bound-subtree-missing-v1".to_vec()
        } else {
            b"astra-unborn-head-v1".to_vec()
        }
    };
    let Some(mut status_command) = hardened_git_command(root) else {
        return GitFingerprint::NotGit;
    };
    status_command.args([
        "status",
        "--porcelain=v1",
        "-z",
        "--untracked-files=all",
        // `traditional` recursively expands every ignored build/cache
        // file.  `matching` keeps direct ignored files observable while
        // representing ignored directories as bounded entries; an
        // over-large/ambiguous result fails closed below.
        "--ignored=matching",
        "--",
        ".",
    ]);
    let Some(output) = run_bounded_probe(
        status_command,
        MAX_STATUS_OUTPUT_BYTES,
        FINGERPRINT_PROBE_TIMEOUT,
    ) else {
        return GitFingerprint::Unknown;
    };
    if !output.success {
        return GitFingerprint::Unknown;
    }
    let mut hasher = DefaultHasher::new();
    // A clean commit has no status entry. Include the bound subtree identity
    // so an opaque command that edits, commits, and leaves a clean worktree
    // still produces a delta; hashing the full repository tree would make a
    // sibling-only commit look like a change in a nested workspace.
    tree_bytes.hash(&mut hasher);
    // Status alone is insufficient for a pre-dirty workspace: changing a
    // file that was already marked `M` leaves the status bytes unchanged.
    // Hash the bounded content of every path reported by status so a generic
    // opaque writer still yields a delta without parsing its command text.
    let mut content_bytes = 0usize;
    let mut rename_target = false;
    let mut status_entries = 0usize;
    let mut ignored_entries = 0usize;
    for raw_path in output.stdout.split(|byte| *byte == 0) {
        if raw_path.is_empty() {
            continue;
        }
        status_entries = status_entries.saturating_add(1);
        if status_entries > MAX_STATUS_ENTRIES {
            return GitFingerprint::Unknown;
        }
        let (status, path_bytes) = if rename_target {
            rename_target = false;
            (b"rename_target".as_slice(), raw_path)
        } else if raw_path.len() >= 3 && raw_path[2] == b' ' {
            let status = &raw_path[..2];
            rename_target = matches!(status, b"R " | b" R" | b"C " | b" C");
            (status, &raw_path[3..])
        } else {
            (b"path".as_slice(), raw_path)
        };
        // Git's `-z` format preserves raw path bytes.  Do not lossy-decode a
        // non-UTF-8 name into a different path and then claim a trustworthy
        // receipt; an ambiguous path makes this whole fingerprint unknown.
        let Ok(path_text) = std::str::from_utf8(path_bytes) else {
            return GitFingerprint::Unknown;
        };
        let path = Path::new(path_text);
        if path.is_absolute()
            || path
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return GitFingerprint::Unknown;
        }
        let path_from_git_root = git_root.join(path);
        let Ok(path_from_workspace) = path_from_git_root.strip_prefix(root) else {
            // `-- .` should already enforce this boundary. Keep the check
            // explicit so a future Git/config change cannot turn a sibling
            // path into bound-workspace evidence.
            continue;
        };
        // `.astra` is executor/session coordination state, not a user
        // deliverable. The manifest fallback already excludes this exact
        // top-level directory; Git-backed observation must use the same
        // scope so creating or locking workspace coordination files cannot
        // manufacture a mutation delta.
        if path_from_workspace
            .components()
            .next()
            .is_some_and(|component| component.as_os_str() == ".astra")
        {
            continue;
        }
        status.hash(&mut hasher);
        path_from_workspace.to_string_lossy().hash(&mut hasher);
        // `--ignored=matching` reports ignored directories compactly. Expand
        // each one only within a deliberately smaller bound so a small
        // generated deliverable is observable while a large cache fails
        // closed instead of being silently treated as unchanged.
        let is_real_directory = fs::symlink_metadata(&path_from_git_root)
            .map(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
            .unwrap_or(false);
        if status == b"!!" && is_real_directory {
            if !hash_ignored_directory(
                &git_root,
                &path_from_git_root,
                &mut hasher,
                &mut ignored_entries,
                &mut content_bytes,
            ) {
                return GitFingerprint::Unknown;
            }
            continue;
        }
        if hash_path_state(&path_from_git_root, &mut hasher, &mut content_bytes).is_none() {
            return GitFingerprint::Unknown;
        }
    }
    GitFingerprint::Captured(hasher.finish())
}

fn hash_ignored_directory(
    git_root: &Path,
    directory: &Path,
    hasher: &mut DefaultHasher,
    entries: &mut usize,
    content_bytes: &mut usize,
) -> bool {
    let mut children = Vec::new();
    let Ok(read_dir) = fs::read_dir(directory) else {
        return false;
    };
    for entry in read_dir {
        let Ok(entry) = entry else {
            return false;
        };
        *entries = entries.saturating_add(1);
        if *entries > MAX_IGNORED_ENTRIES {
            return false;
        }
        children.push(entry);
    }
    children.sort_by_key(|entry| entry.file_name());

    for entry in children {
        let path = entry.path();
        let Ok(relative) = path.strip_prefix(git_root) else {
            return false;
        };
        relative.to_string_lossy().hash(hasher);
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            return false;
        };
        metadata.file_type().is_symlink().hash(hasher);
        metadata.is_dir().hash(hasher);
        metadata.len().hash(hasher);
        hash_permissions(&metadata, hasher);
        if metadata.file_type().is_symlink() {
            let Ok(target) = fs::read_link(&path) else {
                return false;
            };
            target.to_string_lossy().hash(hasher);
        } else if metadata.is_file() {
            let remaining = MAX_IGNORED_CONTENT_BYTES.saturating_sub(*content_bytes);
            if metadata.len() > remaining as u64 {
                return false;
            }
            let Ok(mut file) = fs::File::open(&path) else {
                return false;
            };
            let mut buffer = [0u8; 8192];
            loop {
                let Ok(read) = file.read(&mut buffer) else {
                    return false;
                };
                if read == 0 {
                    break;
                }
                buffer[..read].hash(hasher);
                *content_bytes = content_bytes.saturating_add(read);
                if *content_bytes > MAX_IGNORED_CONTENT_BYTES {
                    return false;
                }
            }
            let Ok(after) = fs::symlink_metadata(&path) else {
                return false;
            };
            if after.len() != metadata.len() {
                return false;
            }
        }
        if metadata.is_dir()
            && !hash_ignored_directory(git_root, &path, hasher, entries, content_bytes)
        {
            return false;
        }
    }
    true
}

/// Construct the read-only Git probe used by the observer.
///
/// Repository-local `core.fsmonitor` may point at an arbitrary executable.
/// The observer runs outside the tool sandbox, so allowing that hook here
/// would turn a metadata probe into host code execution. Disable hook-backed
/// acceleration and optional locks explicitly; the ordinary status result is
/// still bounded and any ambiguity fails closed.
fn hardened_git_command(root: &Path) -> Option<Command> {
    let program = trusted_git_program()?;
    let mut command = Command::new(program);
    command
        // Do not inherit PATH, HOME, GIT_* or loader/configuration variables
        // from the model-controlled tool environment.  In particular, a
        // workspace-local executable named `git` must never become the
        // observer's host process.
        .env_clear()
        .env("PATH", TRUSTED_GIT_PATH)
        .env("HOME", "/nonexistent")
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .arg("-C")
        .arg(root)
        .arg("-c")
        .arg("core.fsmonitor=false")
        .arg("-c")
        .arg("core.untrackedCache=false")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0");
    #[cfg(unix)]
    {
        // Do not inherit a user/global config that can add include files or
        // other process hooks to a probe running in a server host process.
        command
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null");
    }
    Some(command)
}

/// Resolve Git only from administrator-owned system locations.  Looking it
/// up through PATH would let a task write `./git` (or a virtualenv shim) and
/// execute that file from the observer, which runs outside the tool sandbox.
fn trusted_git_program() -> Option<&'static Path> {
    #[cfg(unix)]
    const CANDIDATES: &[&str] = &["/usr/bin/git", "/bin/git", "/usr/local/bin/git"];
    #[cfg(windows)]
    const CANDIDATES: &[&str] = &[
        r"C:\Program Files\Git\cmd\git.exe",
        r"C:\Program Files\Git\bin\git.exe",
    ];
    #[cfg(not(any(unix, windows)))]
    const CANDIDATES: &[&str] = &[];

    CANDIDATES
        .iter()
        .map(Path::new)
        .find(|candidate| trusted_git_path(candidate))
}

/// The observer runs outside the tool sandbox, so a regular file at a fixed
/// path is not sufficient provenance: a group-writable `/usr/local/bin` (or a
/// writable parent) could still replace it between probes.  On Unix require a
/// root-owned, non-symlink binary and root-owned, non-group/other-writable
/// parent directories.  If the platform cannot prove this, the caller falls
/// back to the bounded manifest instead of executing an ambiguous helper.
fn trusted_git_path(candidate: &Path) -> bool {
    let Ok(metadata) = fs::symlink_metadata(candidate) else {
        return false;
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != 0 || metadata.permissions().mode() & 0o022 != 0 {
            return false;
        }
        let mut parent = candidate.parent();
        while let Some(path) = parent {
            let Ok(parent_meta) = fs::symlink_metadata(path) else {
                return false;
            };
            if !parent_meta.is_dir()
                || parent_meta.file_type().is_symlink()
                || parent_meta.uid() != 0
                || parent_meta.permissions().mode() & 0o022 != 0
            {
                return false;
            }
            if path == Path::new("/") {
                break;
            }
            parent = path.parent();
        }
    }

    // Windows candidates are already restricted to administrator-managed
    // installation roots.  ACL inspection is platform-specific and is not
    // available through std::fs; a symlink-free regular file is the strongest
    // portable check, while failure still falls back safely above.
    true
}

fn hash_path_state(
    path: &Path,
    hasher: &mut DefaultHasher,
    content_bytes: &mut usize,
) -> Option<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            "missing".hash(hasher);
            return Some(());
        }
        Err(_) => return None,
    };
    metadata.file_type().is_symlink().hash(hasher);
    metadata.is_dir().hash(hasher);
    metadata.len().hash(hasher);
    hash_permissions(&metadata, hasher);
    if metadata.file_type().is_symlink() {
        fs::read_link(path).ok()?.to_string_lossy().hash(hasher);
    }
    if metadata.is_file() {
        let remaining = MAX_STATUS_CONTENT_BYTES.saturating_sub(*content_bytes);
        if metadata.len() > remaining as u64 {
            return None;
        }
        let mut file = fs::File::open(path).ok()?;
        let mut buffer = [0u8; 8192];
        loop {
            let read = file.read(&mut buffer).ok()?;
            if read == 0 {
                break;
            }
            buffer[..read].hash(hasher);
            *content_bytes = (*content_bytes).saturating_add(read);
            if *content_bytes > MAX_STATUS_CONTENT_BYTES {
                return None;
            }
        }
        let after = fs::symlink_metadata(path).ok()?;
        if after.len() != metadata.len() {
            return None;
        }
    }
    Some(())
}

fn manifest_fingerprint(root: &Path) -> Option<u64> {
    let mut hasher = DefaultHasher::new();
    let mut entries = 0usize;
    let mut content_bytes = 0usize;
    walk_manifest(root, root, &mut hasher, &mut entries, &mut content_bytes)
        .then_some(hasher.finish())
}

fn walk_manifest(
    root: &Path,
    directory: &Path,
    hasher: &mut DefaultHasher,
    entries: &mut usize,
    content_bytes: &mut usize,
) -> bool {
    let Ok(entries_in_directory) = fs::read_dir(directory) else {
        return false;
    };
    let mut children = Vec::new();
    for entry in entries_in_directory {
        let Ok(entry) = entry else {
            return false;
        };
        *entries = entries.saturating_add(1);
        if *entries > MAX_MANIFEST_ENTRIES {
            return false;
        }
        children.push(entry);
    }
    children.sort_by_key(|entry| entry.file_name());

    for entry in children {
        let name = entry.file_name();
        if name == ".git" || name == ".astra" {
            continue;
        }
        let path = entry.path();
        let Some(relative) = path.strip_prefix(root).ok() else {
            return false;
        };
        relative.to_string_lossy().hash(hasher);
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            return false;
        };
        metadata.file_type().is_symlink().hash(hasher);
        metadata.is_dir().hash(hasher);
        metadata.len().hash(hasher);
        hash_permissions(&metadata, hasher);
        if metadata.file_type().is_symlink() {
            let Ok(target) = fs::read_link(&path) else {
                return false;
            };
            target.to_string_lossy().hash(hasher);
        } else if metadata.is_file() {
            let remaining = MAX_STATUS_CONTENT_BYTES.saturating_sub(*content_bytes);
            if metadata.len() > remaining as u64 {
                return false;
            }
            let Ok(mut file) = fs::File::open(&path) else {
                return false;
            };
            let mut buffer = [0u8; 8192];
            loop {
                let Ok(read) = file.read(&mut buffer) else {
                    return false;
                };
                if read == 0 {
                    break;
                }
                buffer[..read].hash(hasher);
                *content_bytes = (*content_bytes).saturating_add(read);
                if *content_bytes > MAX_STATUS_CONTENT_BYTES {
                    return false;
                }
            }
            let Ok(after) = fs::symlink_metadata(&path) else {
                return false;
            };
            if after.len() != metadata.len() {
                return false;
            }
        }

        if metadata.is_dir() && !walk_manifest(root, &path, hasher, entries, content_bytes) {
            return false;
        }
    }
    true
}

fn hash_permissions(metadata: &fs::Metadata, hasher: &mut DefaultHasher) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode().hash(hasher);
    }
    #[cfg(not(unix))]
    {
        metadata.permissions().readonly().hash(hasher);
    }
}

/// Build the compact metadata projection consumed by the runtime ledger.
pub fn changed_receipt() -> serde_json::Map<String, serde_json::Value> {
    changed_receipt_with_ownership(INVOCATION_CGROUP_OWNERSHIP)
}

/// Build a receipt with explicit process-boundary provenance.  Spatial scope
/// and process ownership are intentionally separate: the former says which
/// workspace changed, while the latter says how confidently the executor
/// attributed the change.
pub fn changed_receipt_with_ownership(
    ownership: &str,
) -> serde_json::Map<String, serde_json::Value> {
    serde_json::Map::from_iter([
        (OBSERVED_FIELD.to_string(), serde_json::Value::Bool(true)),
        (
            SCOPE_FIELD.to_string(),
            serde_json::Value::String(BOUND_WORKSPACE_SCOPE.to_string()),
        ),
        (
            OWNERSHIP_FIELD.to_string(),
            serde_json::Value::String(ownership.to_string()),
        ),
        (
            RECEIPT_FIELD.to_string(),
            serde_json::json!({
                "schema": "workspace_mutation_receipt.v1",
                "source": "post_execution_fingerprint",
                "scope": BOUND_WORKSPACE_SCOPE,
                "changed": true,
                "ownership": ownership,
            }),
        ),
    ])
}

/// Build a compact receipt for a successful, structured workspace writer.
///
/// The tool contract and the owner executor already performed path and
/// permission checks before returning success.  The receipt therefore binds
/// the *execution fact* to the owner route without copying host paths or
/// asking a remote server to stat the file.  Post-mutation observation is
/// still required by the runtime; this receipt only proves that a typed
/// workspace-writing boundary occurred.
pub fn typed_workspace_tool_receipt() -> serde_json::Map<String, serde_json::Value> {
    serde_json::Map::from_iter([
        (OBSERVED_FIELD.to_string(), serde_json::Value::Bool(true)),
        (
            SCOPE_FIELD.to_string(),
            serde_json::Value::String(BOUND_WORKSPACE_SCOPE.to_string()),
        ),
        (
            OWNERSHIP_FIELD.to_string(),
            serde_json::Value::String(TYPED_WORKSPACE_TOOL_OWNERSHIP.to_string()),
        ),
        (
            RECEIPT_FIELD.to_string(),
            serde_json::json!({
                "schema": "workspace_mutation_receipt.v1",
                "source": "typed_workspace_tool",
                "scope": BOUND_WORKSPACE_SCOPE,
                "changed": true,
                "ownership": TYPED_WORKSPACE_TOOL_OWNERSHIP,
            }),
        ),
    ])
}

/// Produce a structured-writer receipt only after the owner has established
/// both sides of the contract: this was an applied (not dry-run) operation,
/// and every explicit target resolves inside the bound workspace.  The
/// generic `may mutate` predicate is intentionally not used here; it is an
/// admission/lease decision, not evidence that bytes changed.
pub fn typed_workspace_tool_receipt_for_applied(
    name: &str,
    args: &serde_json::Value,
    workspace_root: &Path,
    is_error: bool,
    applied: bool,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    // `rename_symbol` and the LSP contract are preview-first: omitted
    // `dry_run` means preview, not an applied mutation.  Do not infer a
    // changed receipt merely because the tool is in the mutation family.
    let dry_run = args
        .get("dry_run")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(matches!(name, "rename_symbol" | "lsp"));
    if is_error
        || !applied
        || dry_run
        || !crate::executor::is_workspace_mutation_tool(name, args)
        || !structured_targets_are_bound(name, args, workspace_root)
    {
        return None;
    }
    Some(typed_workspace_tool_receipt())
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceFileStateIdentity {
    pub kind: String,
    pub sha256: String,
    pub bytes: u64,
}

const MAX_CONVERGENCE_SNAPSHOT_BYTES: u64 = 10 * 1024 * 1024;
const MAX_PENDING_CONVERGENCE_SNAPSHOTS: usize = 4096;

#[derive(Debug, Clone, Default)]
pub struct DesiredStateConvergenceTracker {
    pending: Arc<std::sync::Mutex<PendingConvergenceSnapshots>>,
}

#[derive(Debug, Default)]
struct PendingConvergenceSnapshots {
    entries: HashMap<(String, String), u64>,
    next_sequence: u64,
}

impl DesiredStateConvergenceTracker {
    pub fn register(&self, authority: &str, receipt: &serde_json::Value) -> bool {
        if authority.trim().is_empty() {
            return false;
        }
        let Some(target) = typed_workspace_desired_state_convergence_target(receipt) else {
            return false;
        };
        let mut pending = recover_mutex(&self.pending);
        let key = (authority.to_string(), target.to_string());
        if !pending.entries.contains_key(&key)
            && pending.entries.len() >= MAX_PENDING_CONVERGENCE_SNAPSHOTS
            && let Some(oldest) = pending
                .entries
                .iter()
                .min_by_key(|(_, sequence)| **sequence)
                .map(|(key, _)| key.clone())
        {
            // Capacity eviction only revokes the old strong-snapshot lane; it
            // cannot create positive evidence. The abandoned runtime receipt
            // therefore remains pending/fails closed, while a long-lived
            // executor can continue serving a new live turn.
            pending.entries.remove(&oldest);
        }
        // One later full observation settles every identical no-op writer for
        // this live authority/target. Counting retries would make an
        // idempotent model retry require an arbitrary number of reads and
        // retain stale authority after the first strong snapshot.
        let sequence = pending.next_sequence;
        pending.next_sequence = pending.next_sequence.wrapping_add(1);
        pending.entries.insert(key, sequence);
        true
    }

    pub fn requires_snapshot_lease(
        &self,
        authority: &str,
        name: &str,
        args: &serde_json::Value,
        workspace_root: &Path,
    ) -> bool {
        let Some(target) = full_read_file_normalized_target(name, args, workspace_root) else {
            return false;
        };
        recover_mutex(&self.pending)
            .entries
            .contains_key(&(authority.to_string(), target))
    }

    pub fn consume_snapshot(
        &self,
        authority: &str,
        name: &str,
        args: &serde_json::Value,
        workspace_root: &Path,
    ) {
        let Some(target) = full_read_file_normalized_target(name, args, workspace_root) else {
            return;
        };
        let key = (authority.to_string(), target);
        recover_mutex(&self.pending).entries.remove(&key);
    }

    pub fn clear_authority(&self, authority: &str) {
        recover_mutex(&self.pending)
            .entries
            .retain(|(owner, _), _| owner != authority);
    }

    #[cfg(test)]
    fn pending_count(&self) -> usize {
        recover_mutex(&self.pending).entries.len()
    }
}

#[derive(Debug, Default)]
pub struct TypedWorkspaceConvergenceProjection {
    pub convergence_receipt: Option<serde_json::Map<String, serde_json::Value>>,
    pub observation_receipt: Option<serde_json::Map<String, serde_json::Value>>,
}

/// Project the live desired-state lane at the workspace-owner boundary.
///
/// All executors use this single transition so receipt publication cannot
/// drift from tracker registration or strong-observer consumption. Mutation
/// receipts remain a separate contract and are intentionally not handled
/// here.
#[allow(clippy::too_many_arguments)]
pub fn project_typed_workspace_convergence(
    tracker: &DesiredStateConvergenceTracker,
    authority: Option<&str>,
    name: &str,
    args: &serde_json::Value,
    workspace_root: &Path,
    is_error: bool,
    desired_state: Option<&WorkspaceFileStateIdentity>,
    convergence_allowed: bool,
    targeted_observer: bool,
    strong_snapshot_authority: bool,
) -> Result<TypedWorkspaceConvergenceProjection, &'static str> {
    let convergence_receipt = typed_workspace_desired_state_convergence_receipt_for(
        name,
        args,
        workspace_root,
        is_error,
        convergence_allowed.then_some(desired_state).flatten(),
    );
    if let Some(receipt) = convergence_receipt.as_ref() {
        let authority = authority
            .filter(|authority| !authority.trim().is_empty())
            .ok_or("desired-state convergence requires non-empty live run/turn authority")?;
        let registered = receipt
            .get(RECEIPT_FIELD)
            .is_some_and(|value| tracker.register(authority, value));
        if !registered {
            return Err("desired-state convergence tracker rejected owner receipt");
        }
    }

    let observation_receipt = if targeted_observer {
        let authority = authority
            .filter(|authority| !authority.trim().is_empty())
            .ok_or("desired-state observation requires non-empty live run/turn authority")?;
        let receipt = typed_workspace_observation_snapshot_receipt_for(
            name,
            args,
            workspace_root,
            is_error,
            strong_snapshot_authority,
        );
        if receipt.is_some() {
            tracker.consume_snapshot(authority, name, args, workspace_root);
        }
        receipt
    } else {
        typed_workspace_observation_receipt_for(name, args, workspace_root, is_error)
    };

    Ok(TypedWorkspaceConvergenceProjection {
        convergence_receipt,
        observation_receipt,
    })
}

pub fn workspace_file_state_identity(bytes: &[u8]) -> WorkspaceFileStateIdentity {
    WorkspaceFileStateIdentity {
        kind: "file_bytes".to_string(),
        sha256: hex::encode(Sha256::digest(bytes)),
        bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
    }
}

fn write_file_invocation_desired_state_identity(
    args: &serde_json::Value,
) -> Option<WorkspaceFileStateIdentity> {
    let raw_path = args.get("path")?.as_str()?;
    let content = args.get("content")?.as_str()?;
    let normalized = crate::fs_ops::normalize_content_before_write(Path::new(raw_path), content);
    Some(workspace_file_state_identity(normalized.as_bytes()))
}

fn validated_workspace_file_state_identity(
    value: &serde_json::Value,
) -> Option<WorkspaceFileStateIdentity> {
    let object = value.as_object()?;
    if object.len() != 3
        || object.get("kind").and_then(serde_json::Value::as_str) != Some("file_bytes")
    {
        return None;
    }
    let sha256 = object.get("sha256")?.as_str()?;
    if sha256.len() != 64
        || !sha256
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return None;
    }
    Some(WorkspaceFileStateIdentity {
        kind: "file_bytes".to_string(),
        sha256: sha256.to_string(),
        bytes: object.get("bytes")?.as_u64()?,
    })
}

pub fn workspace_desired_state_convergence_marker(
    requested_state: &WorkspaceFileStateIdentity,
    desired_state: &WorkspaceFileStateIdentity,
) -> serde_json::Value {
    let marker_id = uuid::Uuid::new_v4().to_string();
    let mut registry = recover_mutex(DESIRED_STATE_MARKER_REGISTRY.get_or_init(Default::default));
    if registry.len() < MAX_LIVE_DESIRED_STATE_MARKERS {
        registry.insert(
            marker_id.clone(),
            (requested_state.clone(), desired_state.clone()),
        );
    }
    drop(registry);
    serde_json::json!({
        "schema": DESIRED_STATE_CONVERGENCE_MARKER_SCHEMA,
        "marker_id": marker_id,
        "state": "already_desired",
        "request": requested_state,
        "desired_state": desired_state,
    })
}

/// Consume the owner-internal convergence marker exactly once.  A bare bool,
/// malformed state binding, invocation mismatch, or simultaneous mutation
/// claim is an executor integrity error rather than completion authority.
pub fn consume_workspace_desired_state_convergence_marker(
    fields: &mut Option<serde_json::Map<String, serde_json::Value>>,
    args: &serde_json::Value,
    workspace_root: &Path,
) -> Result<Option<WorkspaceFileStateIdentity>, &'static str> {
    let Some(fields) = fields.as_mut() else {
        return Ok(None);
    };
    let Some(marker) = fields.remove(DESIRED_STATE_CONVERGED_FIELD) else {
        return Ok(None);
    };
    let registered = marker
        .get("marker_id")
        .and_then(serde_json::Value::as_str)
        .and_then(|marker_id| {
            recover_mutex(DESIRED_STATE_MARKER_REGISTRY.get_or_init(Default::default))
                .remove(marker_id)
        });
    if fields
        .get("workspace_mutation_applied")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
        || fields.contains_key(RECEIPT_FIELD)
        || fields.contains_key(OBSERVATION_RECEIPT_FIELD)
    {
        return Err("desired-state convergence marker conflicts with existing workspace authority");
    }
    let object = marker
        .as_object()
        .ok_or("desired-state convergence marker must be an owner-produced structured value")?;
    if object.len() != 5
        || object.get("schema").and_then(serde_json::Value::as_str)
            != Some(DESIRED_STATE_CONVERGENCE_MARKER_SCHEMA)
        || object.get("state").and_then(serde_json::Value::as_str) != Some("already_desired")
    {
        return Err("desired-state convergence marker schema is invalid");
    }
    let registered = registered
        .ok_or("desired-state convergence marker is forged, expired, or already consumed")?;
    let request = validated_workspace_file_state_identity(
        object
            .get("request")
            .ok_or("desired-state convergence marker is missing its request binding")?,
    )
    .ok_or("desired-state convergence request binding is invalid")?;
    let requested_content = args
        .get("content")
        .and_then(serde_json::Value::as_str)
        .ok_or("desired-state convergence invocation has no string content")?;
    if request != workspace_file_state_identity(requested_content.as_bytes()) {
        return Err("desired-state convergence marker does not match the live invocation content");
    }
    let desired_state = validated_workspace_file_state_identity(
        object
            .get("desired_state")
            .ok_or("desired-state convergence marker is missing desired state")?,
    )
    .ok_or("desired-state convergence desired-state binding is invalid")?;
    if registered != (request, desired_state.clone())
        || crate::fs_ops::write_file_desired_state_identity(workspace_root, args)
            != Some(desired_state.clone())
    {
        return Err("desired-state convergence marker does not match the typed writer outcome");
    }
    Ok(Some(desired_state))
}

pub fn discard_workspace_desired_state_convergence_marker(
    fields: &mut serde_json::Map<String, serde_json::Value>,
) {
    let Some(marker) = fields.remove(DESIRED_STATE_CONVERGED_FIELD) else {
        return;
    };
    let Some(marker_id) = marker.get("marker_id").and_then(serde_json::Value::as_str) else {
        return;
    };
    recover_mutex(DESIRED_STATE_MARKER_REGISTRY.get_or_init(Default::default)).remove(marker_id);
}

/// Produce live, owner-bound evidence that a complete-state typed writer did
/// not mutate because its exact requested target state already existed.
///
/// This is deliberately not a mutation receipt: `changed` is false and the
/// runtime may use it only together with a later, same-target typed
/// observation from the same live invocation.  Patch-style writers are not
/// eligible because a no-op patch does not prove a complete desired state.
pub fn typed_workspace_desired_state_convergence_receipt_for(
    name: &str,
    args: &serde_json::Value,
    workspace_root: &Path,
    is_error: bool,
    desired_state: Option<&WorkspaceFileStateIdentity>,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    if is_error
        || desired_state.is_none()
        || name != "write_file"
        || args
            .get("delete")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    {
        return None;
    }
    let target = normalized_bound_target_identity(name, args, workspace_root)?;
    let invocation_target = raw_invocation_target_identity(name, args)?;
    let desired_state = desired_state?.clone();
    let request = workspace_file_state_identity(args.get("content")?.as_str()?.as_bytes());
    if write_file_invocation_desired_state_identity(args) != Some(desired_state.clone()) {
        return None;
    }
    let normalized_path = target.get("path")?.as_str()?;
    if stable_bounded_file_state_identity(&workspace_root.join(normalized_path))? != desired_state {
        return None;
    }
    Some(serde_json::Map::from_iter([
        (
            SCOPE_FIELD.to_string(),
            serde_json::Value::String(BOUND_WORKSPACE_SCOPE.to_string()),
        ),
        (
            RECEIPT_FIELD.to_string(),
            serde_json::json!({
                "schema": "workspace_desired_state_convergence_receipt.v1",
                "source": "typed_workspace_writer",
                "scope": BOUND_WORKSPACE_SCOPE,
                "ownership": TYPED_WORKSPACE_TOOL_OWNERSHIP,
                "authority": "live_invocation",
                "receipt_id": uuid::Uuid::new_v4().to_string(),
                "target": target,
                "invocation_target": invocation_target,
                "request": request,
                "desired_state": desired_state,
                "state": "already_desired",
                "changed": false,
            }),
        ),
    ]))
}

fn structured_targets_are_bound(
    name: &str,
    args: &serde_json::Value,
    workspace_root: &Path,
) -> bool {
    // Patch/rollback commands can contain paths in an opaque patch or a
    // journal handle; guessing those paths would turn scope into a lexical
    // claim. Their executor may add a richer receipt later.
    if matches!(name, "apply_patch" | "rollback_git_worktrees") {
        return false;
    }
    // The git executor is already bound to the project/workspace root.  Its
    // mutating actions (commit/revert/stash apply, etc.) have no per-file
    // operand, so the root binding is the owner-side target contract.
    if name == "git" {
        return true;
    }
    let object = match args.as_object() {
        Some(object) => object,
        None => return false,
    };
    // Multi-file edit entries may each carry their own target. Every such
    // target must be bound; accepting only the top-level path would let a
    // mixed external batch inherit a false workspace scope.
    if matches!(name, "str_replace" | "multi_edit")
        && let Some(edits) = object.get("edits").and_then(serde_json::Value::as_array)
    {
        let top_path = object.get("path").and_then(serde_json::Value::as_str);
        if edits.is_empty() {
            return false;
        }
        for edit in edits {
            let Some(path) = edit
                .get("path")
                .and_then(serde_json::Value::as_str)
                .or(top_path)
            else {
                return false;
            };
            if crate::fs_ops::resolve_path_sandboxed(workspace_root, path, &[]).is_err() {
                return false;
            }
        }
        return true;
    }
    // `rename_symbol` searches the whole bound project when `path` is
    // omitted, so the omission itself is a bound target.  Other structured
    // writers must expose an explicit path for an owner receipt.
    if name == "rename_symbol" && !object.contains_key("path") {
        return true;
    }
    let keys: &[&str] = if name == "lsp" {
        &["file"]
    } else {
        &[
            "path",
            "file_path",
            "notebook_path",
            "target",
            "destination",
            "dest",
        ]
    };
    let mut found = false;
    for key in keys {
        let Some(value) = object.get(*key) else {
            continue;
        };
        found = true;
        let Some(path) = value.as_str() else {
            return false;
        };
        if crate::fs_ops::resolve_path_sandboxed(workspace_root, path, &[]).is_err() {
            return false;
        }
    }
    found
}

fn normalized_bound_target_identity(
    name: &str,
    args: &serde_json::Value,
    workspace_root: &Path,
) -> Option<serde_json::Value> {
    let object = args.as_object()?;
    let keys: &[&str] = if name == "lsp" {
        &["file"]
    } else {
        &[
            "path",
            "file_path",
            "notebook_path",
            "target",
            "destination",
            "dest",
            "root",
            "directory",
        ]
    };
    let mut normalized_target: Option<String> = None;
    for key in keys {
        let Some(raw) = object.get(*key) else {
            continue;
        };
        let raw = raw.as_str()?;
        let resolved = crate::fs_ops::resolve_path_sandboxed(workspace_root, raw, &[]).ok()?;
        let relative = crate::fs_ops::relative_to_workspace_root(workspace_root, &resolved)?;
        let normalized = normalized_relative_target(&relative)?;
        if normalized_target
            .as_ref()
            .is_some_and(|existing| existing != &normalized)
        {
            return None;
        }
        normalized_target = Some(normalized);
    }
    let normalized = normalized_target?;
    let digest = desired_state_target_digest(&normalized);
    Some(serde_json::json!({
        "kind": "workspace_relative_path",
        "path": normalized,
        "sha256": digest,
    }))
}

fn normalized_relative_target(relative: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?.to_string()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn desired_state_target_digest(normalized: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"astra.workspace_target.v1\0");
    hasher.update(normalized.as_bytes());
    hex::encode(hasher.finalize())
}

fn raw_invocation_target_identity(
    name: &str,
    args: &serde_json::Value,
) -> Option<serde_json::Value> {
    let key = if name == "lsp" { "file" } else { "path" };
    let raw = args.get(key)?.as_str()?;
    let mut hasher = Sha256::new();
    hasher.update(b"astra.workspace_invocation_target.v1\0");
    hasher.update(raw.as_bytes());
    Some(serde_json::json!({
        "kind": "tool_path_argument",
        "sha256": hex::encode(hasher.finalize()),
    }))
}

fn validated_raw_invocation_target_identity(value: &serde_json::Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    object.len() == 2
        && object.get("kind").and_then(serde_json::Value::as_str) == Some("tool_path_argument")
        && object
            .get("sha256")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|digest| {
                digest.len() == 64
                    && digest
                        .as_bytes()
                        .iter()
                        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            })
}

fn validated_target_identity(value: &serde_json::Value) -> Option<&str> {
    let target = value.get("target")?.as_object()?;
    if target.get("kind").and_then(serde_json::Value::as_str) != Some("workspace_relative_path") {
        return None;
    }
    let path = target.get("path")?.as_str()?;
    let normalized = normalized_relative_target(Path::new(path))?;
    let expected_digest = desired_state_target_digest(path);
    if normalized != path
        || target.get("sha256").and_then(serde_json::Value::as_str)
            != Some(expected_digest.as_str())
    {
        return None;
    }
    Some(path)
}

/// Validate the owner-produced structured-writer receipt.  This is separate
/// from the shell receipt validator because the two routes have different
/// provenance contracts; neither accepts model text or an arbitrary metadata
/// shape as evidence.
pub fn is_typed_workspace_tool_receipt(value: &serde_json::Value) -> bool {
    value.get("schema").and_then(serde_json::Value::as_str) == Some("workspace_mutation_receipt.v1")
        && value.get("source").and_then(serde_json::Value::as_str) == Some("typed_workspace_tool")
        && value.get("scope").and_then(serde_json::Value::as_str) == Some(BOUND_WORKSPACE_SCOPE)
        && value.get("changed").and_then(serde_json::Value::as_bool) == Some(true)
        && value.get("ownership").and_then(serde_json::Value::as_str)
            == Some(TYPED_WORKSPACE_TOOL_OWNERSHIP)
}

pub fn is_typed_workspace_desired_state_convergence_receipt(value: &serde_json::Value) -> bool {
    value.get("schema").and_then(serde_json::Value::as_str)
        == Some("workspace_desired_state_convergence_receipt.v1")
        && value.get("source").and_then(serde_json::Value::as_str) == Some("typed_workspace_writer")
        && value.get("scope").and_then(serde_json::Value::as_str) == Some(BOUND_WORKSPACE_SCOPE)
        && value.get("ownership").and_then(serde_json::Value::as_str)
            == Some(TYPED_WORKSPACE_TOOL_OWNERSHIP)
        && value.get("authority").and_then(serde_json::Value::as_str) == Some("live_invocation")
        && value.get("state").and_then(serde_json::Value::as_str) == Some("already_desired")
        && value.get("changed").and_then(serde_json::Value::as_bool) == Some(false)
        && value
            .get("receipt_id")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
        && validated_target_identity(value).is_some()
        && value
            .get("invocation_target")
            .is_some_and(validated_raw_invocation_target_identity)
        && value
            .get("request")
            .and_then(validated_workspace_file_state_identity)
            .is_some()
        && value
            .get("desired_state")
            .and_then(validated_workspace_file_state_identity)
            .is_some()
}

pub fn typed_workspace_desired_state_convergence_target(value: &serde_json::Value) -> Option<&str> {
    is_typed_workspace_desired_state_convergence_receipt(value)
        .then(|| validated_target_identity(value))
        .flatten()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedWorkspaceDesiredStateConvergenceEvidence {
    pub target: String,
    pub invocation_target: serde_json::Value,
    pub request: WorkspaceFileStateIdentity,
    pub desired_state: WorkspaceFileStateIdentity,
    pub receipt_id: String,
}

pub fn typed_workspace_desired_state_convergence_evidence(
    value: &serde_json::Value,
) -> Option<TypedWorkspaceDesiredStateConvergenceEvidence> {
    Some(TypedWorkspaceDesiredStateConvergenceEvidence {
        target: typed_workspace_desired_state_convergence_target(value)?.to_string(),
        invocation_target: value.get("invocation_target")?.clone(),
        request: validated_workspace_file_state_identity(value.get("request")?)?,
        desired_state: validated_workspace_file_state_identity(value.get("desired_state")?)?,
        receipt_id: value.get("receipt_id")?.as_str()?.to_string(),
    })
}

pub fn typed_workspace_desired_state_convergence_evidence_for_invocation(
    value: &serde_json::Value,
    args: &serde_json::Value,
) -> Option<TypedWorkspaceDesiredStateConvergenceEvidence> {
    let evidence = typed_workspace_desired_state_convergence_evidence(value)?;
    if raw_invocation_target_identity("write_file", args)
        != Some(evidence.invocation_target.clone())
        || args
            .get("content")
            .and_then(serde_json::Value::as_str)
            .map(|content| workspace_file_state_identity(content.as_bytes()))
            != Some(evidence.request.clone())
        || write_file_invocation_desired_state_identity(args)
            != Some(evidence.desired_state.clone())
    {
        return None;
    }
    Some(evidence)
}

/// Structured tools whose successful result is an observation of the bound
/// workspace.  The list is a tool-contract boundary, not a command/file-name
/// heuristic; opaque shell and MCP results never enter this lane.
pub fn is_typed_workspace_observer(name: &str) -> bool {
    matches!(
        name,
        "read_file"
            | "read_metadata"
            | "list_dir"
            | "glob"
            | "grep"
            | "find_definition"
            | "find_references"
            | "lsp"
            | "git_diff"
            | "git_status"
            | "inspect_file"
    )
}

/// `bash` is never inferred to be an observer from its command text.  A
/// caller must opt into this narrow contract and the owner executor must
/// subsequently prove a settled, unchanged workspace before it emits the
/// corresponding receipt.
pub fn is_explicit_workspace_verification_request(name: &str, args: &serde_json::Value) -> bool {
    name == "bash"
        && args.get("mode").and_then(serde_json::Value::as_str) == Some("verify")
        && args
            .get("command")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|command| !command.trim().is_empty())
}

/// Source-authored recovery evidence for an explicit Bash verification whose
/// bounded workspace fingerprint is unavailable.  A successful shell exit is
/// deliberately not enough to replace this evidence: only an Edge-owned
/// receipt can authorize post-mutation completion.
pub fn explicit_workspace_verification_unavailable_evidence() -> astra_core::ToolFailureEvidence {
    astra_core::ToolFailureEvidence::new(
        astra_core::ErrorKind::ToolUnavailable,
        astra_core::ToolFailureCause::ScopeTooBroad,
        false,
        vec![astra_core::ToolRecoveryAction::SelectAvailableCapability],
    )
}

pub const EXPLICIT_WORKSPACE_VERIFICATION_UNAVAILABLE_MESSAGE: &str = "Error: verify-mode could not capture a bounded workspace observation. This verification cannot produce a completion receipt for the current workspace generation; use a typed observer such as read_file, list_dir, or git_diff for the changed artifact instead.";

pub fn typed_workspace_observation_receipt() -> serde_json::Map<String, serde_json::Value> {
    serde_json::Map::from_iter([
        (
            OBSERVATION_SCOPE_FIELD.to_string(),
            serde_json::Value::String(BOUND_WORKSPACE_SCOPE.to_string()),
        ),
        (
            OBSERVATION_RECEIPT_FIELD.to_string(),
            serde_json::json!({
                "schema": "workspace_observation_receipt.v1",
                "source": "typed_workspace_observer",
                "scope": BOUND_WORKSPACE_SCOPE,
                "ownership": TYPED_WORKSPACE_OBSERVER_OWNERSHIP,
            }),
        ),
    ])
}

/// The observer receipt is portable only when the owner can bind its explicit
/// path arguments. Tools with no path default to the already-bound workspace;
/// an explicit external path is never upgraded by tool name alone.
pub fn typed_workspace_observation_receipt_for(
    name: &str,
    args: &serde_json::Value,
    workspace_root: &Path,
    is_error: bool,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    if is_error
        || !is_typed_workspace_observer(name)
        || (name == "lsp"
            && args.get("dry_run").and_then(serde_json::Value::as_bool) == Some(false))
    {
        return None;
    }
    let object = args.as_object()?;
    let keys: &[&str] = if name == "glob" {
        // `pattern` is a selector, not a filesystem operand.  Treating it as
        // a path would reject ordinary bound glob observations and would
        // couple evidence to the spelling of a pattern.
        &["path"]
    } else if name == "lsp" {
        // LSP's workspace is not a safe inference from the tool name: its
        // file operand is the actual scope-bearing contract.
        &["file"]
    } else {
        &[
            "path",
            "file_path",
            "notebook_path",
            "target",
            "destination",
            "dest",
            "root",
            "directory",
        ]
    };
    if name == "lsp" && !object.contains_key("file") {
        return None;
    }
    for key in keys {
        let Some(value) = object.get(*key) else {
            continue;
        };
        let path = value.as_str()?;
        if crate::fs_ops::resolve_path_sandboxed(workspace_root, path, &[]).is_err() {
            return None;
        }
    }
    let mut receipt = typed_workspace_observation_receipt();
    if let Some(target) = normalized_bound_target_identity(name, args, workspace_root)
        && let Some(value) = receipt.get_mut(OBSERVATION_RECEIPT_FIELD)
        && let Some(object) = value.as_object_mut()
    {
        object.insert("target".to_string(), target);
        object.insert(
            "invocation_target".to_string(),
            raw_invocation_target_identity(name, args)?,
        );
    }
    Some(receipt)
}

/// Produce a fresh owner snapshot for a targeted typed observer. Callers must
/// hold the bound-workspace observation lease across tool execution and this
/// receipt mint so the state digest cannot be borrowed from a concurrent
/// writer. Generic observer receipts remain valid for ordinary settlement but
/// cannot pair with desired-state convergence.
pub fn typed_workspace_observation_snapshot_receipt_for(
    name: &str,
    args: &serde_json::Value,
    workspace_root: &Path,
    is_error: bool,
    snapshot_authority: bool,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    if !snapshot_authority {
        return None;
    }
    let mut receipt =
        typed_workspace_observation_receipt_for(name, args, workspace_root, is_error)?;
    let value = receipt
        .get_mut(OBSERVATION_RECEIPT_FIELD)?
        .as_object_mut()?;
    let normalized_path =
        validated_target_identity(&serde_json::Value::Object(value.clone()))?.to_string();
    let observed_state =
        stable_bounded_file_state_identity(&workspace_root.join(&normalized_path))?;
    value.insert(
        "observed_state".to_string(),
        serde_json::to_value(observed_state).ok()?,
    );
    value.insert(
        "observation_id".to_string(),
        serde_json::Value::String(uuid::Uuid::new_v4().to_string()),
    );
    Some(receipt)
}

fn stable_bounded_file_state_identity(path: &Path) -> Option<WorkspaceFileStateIdentity> {
    fn capture(path: &Path) -> Option<WorkspaceFileStateIdentity> {
        let mut file = fs::File::open(path).ok()?;
        let metadata = file.metadata().ok()?;
        if !metadata.is_file() || metadata.len() > MAX_CONVERGENCE_SNAPSHOT_BYTES {
            return None;
        }
        let mut hasher = Sha256::new();
        let mut total = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer).ok()?;
            if read == 0 {
                break;
            }
            total = total.checked_add(u64::try_from(read).ok()?)?;
            if total > MAX_CONVERGENCE_SNAPSHOT_BYTES {
                return None;
            }
            hasher.update(&buffer[..read]);
        }
        (total == metadata.len()).then(|| WorkspaceFileStateIdentity {
            kind: "file_bytes".to_string(),
            sha256: hex::encode(hasher.finalize()),
            bytes: total,
        })
    }

    let first = capture(path)?;
    let second = capture(path)?;
    (first == second).then_some(second)
}

pub fn is_typed_workspace_observation_receipt(value: &serde_json::Value) -> bool {
    value.get("schema").and_then(serde_json::Value::as_str)
        == Some("workspace_observation_receipt.v1")
        && value.get("source").and_then(serde_json::Value::as_str)
            == Some("typed_workspace_observer")
        && value.get("scope").and_then(serde_json::Value::as_str) == Some(BOUND_WORKSPACE_SCOPE)
        && value.get("ownership").and_then(serde_json::Value::as_str)
            == Some(TYPED_WORKSPACE_OBSERVER_OWNERSHIP)
}

pub fn typed_workspace_observation_target(value: &serde_json::Value) -> Option<&str> {
    is_typed_workspace_observation_receipt(value)
        .then(|| validated_target_identity(value))
        .flatten()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedWorkspaceObservationEvidence {
    pub target: String,
    pub invocation_target: serde_json::Value,
    pub observed_state: WorkspaceFileStateIdentity,
    pub observation_id: String,
}

pub fn typed_workspace_observation_evidence(
    value: &serde_json::Value,
) -> Option<TypedWorkspaceObservationEvidence> {
    if !is_typed_workspace_observation_receipt(value) {
        return None;
    }
    if !value
        .get("invocation_target")
        .is_some_and(validated_raw_invocation_target_identity)
    {
        return None;
    }
    let observation_id = value.get("observation_id")?.as_str()?;
    uuid::Uuid::parse_str(observation_id).ok()?;
    Some(TypedWorkspaceObservationEvidence {
        target: validated_target_identity(value)?.to_string(),
        invocation_target: value.get("invocation_target")?.clone(),
        observed_state: validated_workspace_file_state_identity(value.get("observed_state")?)?,
        observation_id: observation_id.to_string(),
    })
}

pub fn typed_workspace_observation_evidence_for_invocation(
    value: &serde_json::Value,
    name: &str,
    args: &serde_json::Value,
) -> Option<TypedWorkspaceObservationEvidence> {
    let evidence = typed_workspace_observation_evidence(value)?;
    (raw_invocation_target_identity(name, args) == Some(evidence.invocation_target.clone()))
        .then_some(evidence)
}

fn full_read_file_normalized_target(
    name: &str,
    args: &serde_json::Value,
    workspace_root: &Path,
) -> Option<String> {
    if name != "read_file"
        || args.get("start_line").is_some()
        || args.get("end_line").is_some()
        || args.get("outline").and_then(serde_json::Value::as_bool) == Some(true)
    {
        return None;
    }
    normalized_bound_target_identity(name, args, workspace_root)
        .and_then(|target| target.get("path")?.as_str().map(ToString::to_string))
}

/// A workspace observation produced by the executor's explicit verification
/// path.  This is intentionally a separate schema from typed read tools: it
/// is valid only with `bash { mode: "verify" }` and is never inferred from
/// shell text or stdout.
pub fn explicit_workspace_verification_receipt() -> serde_json::Map<String, serde_json::Value> {
    serde_json::Map::from_iter([
        (
            OBSERVATION_SCOPE_FIELD.to_string(),
            serde_json::Value::String(BOUND_WORKSPACE_SCOPE.to_string()),
        ),
        (
            OBSERVATION_RECEIPT_FIELD.to_string(),
            serde_json::json!({
                "schema": "workspace_observation_receipt.v2",
                "source": "executor_bash_verify",
                "scope": BOUND_WORKSPACE_SCOPE,
                "ownership": TYPED_WORKSPACE_OBSERVER_OWNERSHIP,
            }),
        ),
    ])
}

pub fn is_explicit_workspace_verification_receipt(value: &serde_json::Value) -> bool {
    value.get("schema").and_then(serde_json::Value::as_str)
        == Some("workspace_observation_receipt.v2")
        && value.get("source").and_then(serde_json::Value::as_str) == Some("executor_bash_verify")
        && value.get("scope").and_then(serde_json::Value::as_str) == Some(BOUND_WORKSPACE_SCOPE)
        && value.get("ownership").and_then(serde_json::Value::as_str)
            == Some(TYPED_WORKSPACE_OBSERVER_OWNERSHIP)
}

/// Validate the owner-authored fact that a typed multi-path writer committed
/// a strict prefix before failing.  This is deliberately a separate schema
/// from a successful mutation receipt: it can quarantine attribution, but it
/// must never satisfy completion or budget authority.
pub fn is_typed_partial_workspace_mutation_receipt(value: &serde_json::Value) -> bool {
    value.get("schema").and_then(serde_json::Value::as_str)
        == Some("workspace_mutation_partial_receipt.v1")
        && value.get("source").and_then(serde_json::Value::as_str)
            == Some("typed_multi_path_writer")
        && value.get("scope").and_then(serde_json::Value::as_str) == Some(BOUND_WORKSPACE_SCOPE)
        && value.get("ownership").and_then(serde_json::Value::as_str)
            == Some(TYPED_MULTI_PATH_WRITER_OWNERSHIP)
        && value.get("changed").and_then(serde_json::Value::as_bool) == Some(true)
        && value
            .get("paths")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|paths| {
                !paths.is_empty()
                    && paths
                        .iter()
                        .all(|path| path.as_str().is_some_and(|path| !path.trim().is_empty()))
            })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const CROSS_PROCESS_LEASE_HELPER_ENV: &str =
        "ASTRA_TEST_CROSS_PROCESS_WORKSPACE_OBSERVATION_ROOT";
    const CROSS_PROCESS_LEASE_MARKER_ENV: &str =
        "ASTRA_TEST_CROSS_PROCESS_WORKSPACE_OBSERVATION_MARKER";
    const CROSS_PROCESS_LEASE_MODE_ENV: &str =
        "ASTRA_TEST_CROSS_PROCESS_WORKSPACE_OBSERVATION_MODE";

    /// Re-exec fixture for proving that the workspace observation lease is a
    /// process boundary, not merely a mutex inside one Astra process.
    #[test]
    fn cross_process_workspace_observation_lease_helper() {
        let Some(root) = std::env::var_os(CROSS_PROCESS_LEASE_HELPER_ENV) else {
            return;
        };
        let marker =
            std::env::var_os(CROSS_PROCESS_LEASE_MARKER_ENV).expect("cross-process helper marker");
        let mode = std::env::var(CROSS_PROCESS_LEASE_MODE_ENV)
            .unwrap_or_else(|_| "observation".to_string());
        match mode.as_str() {
            "observation" => {
                let _lease = acquire_workspace_observation_lease_sync(
                    Path::new(&root),
                    Duration::from_secs(5),
                )
                .expect("child acquires observation lease");
                fs::write(marker, "owned by observation").expect("child write");
            }
            "mutation" => {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_time()
                    .build()
                    .expect("child runtime");
                let _lease = runtime
                    .block_on(acquire_workspace_mutation_lease_with_options(
                        Path::new(&root),
                        None,
                        Duration::from_secs(5),
                    ))
                    .expect("child acquires mutation lease");
                fs::write(marker, "owned by mutation").expect("child write");
            }
            "recursive-writer" => {
                let _writer = begin_workspace_writer(Path::new(&root))
                    .expect("child acquires recursive writer barrier");
                fs::write(marker, "owned by recursive writer").expect("child write");
            }
            #[cfg(target_os = "linux")]
            "kernel-namespace-holder" => {
                let (specifications, _) = workspace_coordination_lock_specs(
                    Path::new(&root),
                    CoordinationLockKind::Observation,
                )
                .expect("coordination specifications");
                let _locks = specifications
                    .iter()
                    .map(|specification| {
                        try_acquire_kernel_coordination_namespace(
                            &specification.kernel_namespace_key,
                        )
                        .expect("kernel namespace bind")
                        .expect("uncontended kernel namespace")
                    })
                    .collect::<Vec<_>>();
                fs::write(marker, "kernel namespace held").expect("child marker");
                std::thread::sleep(Duration::from_secs(30));
            }
            other => panic!("unknown helper mode: {other}"),
        }
    }

    #[test]
    fn external_effect_receipt_requires_observed_delta_and_authoritative_ownership() {
        let workspace = tempfile::tempdir().expect("workspace");
        let external = tempfile::tempdir().expect("external state");
        let target = external.path().join("managed.conf");
        fs::write(&target, "before").expect("external fixture");
        let args = serde_json::json!({
            EXTERNAL_STATE_PATHS_FIELD: [target],
        });

        let unchanged = ExternalEffectFingerprint::capture_from_args(&args, workspace.path())
            .expect("valid observation contract")
            .expect("external preimage");
        fs::write(external.path().join("unrelated.conf"), "sibling change")
            .expect("external sibling fixture");
        assert!(
            unchanged
                .changed_receipt(Some(INVOCATION_CGROUP_OWNERSHIP))
                .is_none(),
            "a sibling delta cannot satisfy the exact declared file target"
        );

        let before = ExternalEffectFingerprint::capture_from_args(&args, workspace.path())
            .expect("valid observation contract")
            .expect("external preimage");
        fs::write(&target, "after").expect("external mutation");
        assert!(before.changed_receipt(Some("model_claim")).is_none());
        let fields = before
            .changed_receipt(Some(INVOCATION_SUPERVISOR_OWNERSHIP))
            .expect("authoritative observed delta");
        assert_eq!(
            fields.get(EXTERNAL_EFFECT_OBSERVED_FIELD),
            Some(&serde_json::Value::Bool(true))
        );
        assert!(
            fields
                .get(EXTERNAL_EFFECT_RECEIPT_FIELD)
                .is_some_and(is_authoritative_external_effect_receipt)
        );
    }

    #[test]
    fn external_effect_observation_rejects_workspace_overlap_and_bad_paths() {
        let workspace = tempfile::tempdir().expect("workspace");
        for args in [
            serde_json::json!({EXTERNAL_STATE_PATHS_FIELD: [workspace.path()]}),
            serde_json::json!({EXTERNAL_STATE_PATHS_FIELD: ["relative/path"]}),
            serde_json::json!({EXTERNAL_STATE_PATHS_FIELD: []}),
        ] {
            assert!(ExternalEffectFingerprint::capture_from_args(&args, workspace.path()).is_err());
        }
    }

    #[tokio::test]
    async fn external_effect_lease_excludes_competing_observation_window() {
        let workspace = tempfile::tempdir().expect("workspace");
        let external = tempfile::tempdir().expect("external state");
        let target = external.path().join("managed.conf");
        fs::write(&target, "before").expect("external fixture");
        let args = serde_json::json!({ EXTERNAL_STATE_PATHS_FIELD: [target] });

        let first = acquire_external_effect_observation_lease_with_options(
            &args,
            workspace.path(),
            None,
            Duration::from_secs(2),
        )
        .await
        .expect("valid external contract")
        .expect("first external window admitted");
        let second = acquire_external_effect_observation_lease_with_options(
            &args,
            workspace.path(),
            None,
            Duration::ZERO,
        )
        .await
        .expect("same valid external contract");
        assert!(
            second.is_none(),
            "a competing session cannot observe the same external root"
        );
        assert!(first.integrity_valid());
    }

    fn typed_workspace_tool_receipt_for(
        name: &str,
        args: &serde_json::Value,
        workspace_root: &Path,
        is_error: bool,
    ) -> Option<serde_json::Map<String, serde_json::Value>> {
        typed_workspace_tool_receipt_for_applied(name, args, workspace_root, is_error, true)
    }

    fn manifest_snapshot(root: &Path) -> WorkspaceFingerprint {
        WorkspaceFingerprint::capture(root).expect("small workspace manifest")
    }

    #[test]
    fn manifest_fingerprint_detects_generic_interpreter_write() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::write(temp.path().join("input.txt"), "before").unwrap();
        let before = manifest_snapshot(temp.path());
        fs::write(temp.path().join("output.txt"), "created by python").unwrap();
        let after = manifest_snapshot(temp.path());
        assert!(before.changed_from(Some(after)));
    }

    #[test]
    fn overlarge_manifest_is_unknown_not_positive() {
        let temp = tempfile::tempdir().expect("tempdir");
        for index in 0..=MAX_MANIFEST_ENTRIES {
            fs::write(temp.path().join(format!("file-{index}")), "x").unwrap();
        }
        assert!(manifest_fingerprint(temp.path()).is_none());
    }

    #[test]
    fn unavailable_explicit_verification_has_non_retryable_typed_evidence() {
        let evidence = explicit_workspace_verification_unavailable_evidence();
        assert_eq!(evidence.kind, astra_core::ErrorKind::ToolUnavailable);
        assert_eq!(evidence.cause, astra_core::ToolFailureCause::ScopeTooBroad);
        assert!(!evidence.retryable);
        assert_eq!(
            evidence.recovery_actions,
            vec![astra_core::ToolRecoveryAction::SelectAvailableCapability]
        );
    }

    #[test]
    fn changed_receipt_is_scoped_and_typed() {
        let receipt = changed_receipt();
        assert_eq!(receipt[OBSERVED_FIELD], serde_json::Value::Bool(true));
        assert_eq!(receipt[SCOPE_FIELD], BOUND_WORKSPACE_SCOPE);
        assert_eq!(receipt[OWNERSHIP_FIELD], INVOCATION_CGROUP_OWNERSHIP);
        assert_eq!(
            receipt[RECEIPT_FIELD]["schema"],
            "workspace_mutation_receipt.v1"
        );
        assert!(is_changed_receipt(&receipt[RECEIPT_FIELD]));
        assert!(is_authoritative_changed_receipt(&receipt[RECEIPT_FIELD]));
        let fallback = changed_receipt_with_ownership(FOREGROUND_PROCESS_GROUP_OWNERSHIP);
        assert!(is_changed_receipt(&fallback[RECEIPT_FIELD]));
        assert!(!is_authoritative_changed_receipt(&fallback[RECEIPT_FIELD]));
        let supervisor = changed_receipt_with_ownership(INVOCATION_SUPERVISOR_OWNERSHIP);
        assert!(is_changed_receipt(&supervisor[RECEIPT_FIELD]));
        assert!(is_authoritative_changed_receipt(&supervisor[RECEIPT_FIELD]));
        assert!(!is_changed_receipt(&serde_json::json!({
            "schema": "workspace_mutation_receipt.v1",
            "source": "post_execution_fingerprint",
            "scope": BOUND_WORKSPACE_SCOPE,
            "changed": true,
            "ownership": "model"
        })));
        assert!(!is_changed_receipt(&serde_json::json!({
            "schema": "workspace_mutation_receipt.v1",
            "source": "post_execution_fingerprint",
            "scope": BOUND_WORKSPACE_SCOPE,
            "changed": true,
        })));
        assert!(!is_changed_receipt(&serde_json::json!({
            "schema": "workspace_mutation_receipt.v1",
            "source": "model",
            "scope": BOUND_WORKSPACE_SCOPE,
            "changed": true,
        })));
        let typed = typed_workspace_tool_receipt();
        assert!(is_typed_workspace_tool_receipt(&typed[RECEIPT_FIELD]));
        assert!(!is_authoritative_changed_receipt(&typed[RECEIPT_FIELD]));
        assert!(!is_typed_workspace_tool_receipt(&serde_json::json!({
            "schema": "workspace_mutation_receipt.v1",
            "source": "model",
            "scope": BOUND_WORKSPACE_SCOPE,
            "changed": true,
            "ownership": TYPED_WORKSPACE_TOOL_OWNERSHIP,
        })));
        let observation = typed_workspace_observation_receipt();
        assert!(is_typed_workspace_observation_receipt(
            &observation[OBSERVATION_RECEIPT_FIELD]
        ));
        assert!(is_typed_workspace_observer("read_file"));
        assert!(!is_typed_workspace_observer("bash"));
    }

    #[test]
    fn desired_state_convergence_is_distinct_target_bound_live_evidence() {
        let workspace = tempfile::tempdir().expect("workspace");
        let target = workspace.path().join("answer.txt");
        fs::write(&target, "done\n").expect("target");
        let args = serde_json::json!({"path": "./answer.txt", "content": "done\n"});
        let desired_state = workspace_file_state_identity(b"done\n");

        let fields = typed_workspace_desired_state_convergence_receipt_for(
            "write_file",
            &args,
            workspace.path(),
            false,
            Some(&desired_state),
        )
        .expect("owner-bound convergence receipt");
        let receipt = &fields[RECEIPT_FIELD];
        assert!(is_typed_workspace_desired_state_convergence_receipt(
            receipt
        ));
        assert!(!is_typed_workspace_tool_receipt(receipt));
        assert!(!is_changed_receipt(receipt));
        assert_eq!(receipt["changed"], false);
        assert_eq!(receipt["state"], "already_desired");
        assert_eq!(
            typed_workspace_desired_state_convergence_target(receipt),
            Some("answer.txt")
        );

        let observation = typed_workspace_observation_snapshot_receipt_for(
            "read_file",
            &serde_json::json!({"path": target}),
            workspace.path(),
            false,
            true,
        )
        .expect("same-target observation");
        assert_eq!(
            typed_workspace_observation_target(&observation[OBSERVATION_RECEIPT_FIELD]),
            Some("answer.txt")
        );

        let mut malformed = receipt.clone();
        malformed["target"]["sha256"] = serde_json::json!("forged");
        assert!(!is_typed_workspace_desired_state_convergence_receipt(
            &malformed
        ));
    }

    #[test]
    fn desired_state_convergence_rejects_non_owner_non_full_state_and_external_targets() {
        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::NamedTempFile::new().expect("outside");
        let desired_state = workspace_file_state_identity(b"done");
        for (name, args, is_error, already_desired) in [
            (
                "str_replace",
                serde_json::json!({"path": "answer.txt", "old_str": "a", "new_str": "a"}),
                false,
                true,
            ),
            (
                "write_file",
                serde_json::json!({"path": outside.path(), "content": "done"}),
                false,
                true,
            ),
            (
                "write_file",
                serde_json::json!({"path": "answer.txt", "content": "done"}),
                true,
                true,
            ),
            (
                "write_file",
                serde_json::json!({"path": "answer.txt", "content": "done"}),
                false,
                false,
            ),
        ] {
            assert!(
                typed_workspace_desired_state_convergence_receipt_for(
                    name,
                    &args,
                    workspace.path(),
                    is_error,
                    already_desired.then_some(&desired_state),
                )
                .is_none()
            );
        }
    }

    #[test]
    fn desired_state_marker_is_owner_minted_single_use_and_conflict_closed() {
        let workspace = tempfile::tempdir().expect("workspace");
        fs::write(workspace.path().join("answer.txt"), "done\n").expect("target");
        let args = serde_json::json!({"path": "answer.txt", "content": "done"});
        let requested = workspace_file_state_identity(b"done");
        let desired = workspace_file_state_identity(b"done\n");
        let marker = workspace_desired_state_convergence_marker(&requested, &desired);
        let mut fields = Some(serde_json::Map::from_iter([(
            DESIRED_STATE_CONVERGED_FIELD.to_string(),
            marker.clone(),
        )]));

        assert_eq!(
            consume_workspace_desired_state_convergence_marker(
                &mut fields,
                &args,
                workspace.path(),
            )
            .expect("owner marker"),
            Some(desired.clone())
        );
        assert!(
            !fields
                .as_ref()
                .expect("fields")
                .contains_key(DESIRED_STATE_CONVERGED_FIELD)
        );

        let mut reused = Some(serde_json::Map::from_iter([(
            DESIRED_STATE_CONVERGED_FIELD.to_string(),
            marker,
        )]));
        assert!(
            consume_workspace_desired_state_convergence_marker(
                &mut reused,
                &args,
                workspace.path(),
            )
            .is_err()
        );

        let mut forged = Some(serde_json::Map::from_iter([(
            DESIRED_STATE_CONVERGED_FIELD.to_string(),
            serde_json::json!({
                "schema": DESIRED_STATE_CONVERGENCE_MARKER_SCHEMA,
                "marker_id": uuid::Uuid::new_v4().to_string(),
                "state": "already_desired",
                "request": requested,
                "desired_state": desired,
            }),
        )]));
        assert!(
            consume_workspace_desired_state_convergence_marker(
                &mut forged,
                &args,
                workspace.path(),
            )
            .is_err()
        );

        let conflict_marker = workspace_desired_state_convergence_marker(
            &workspace_file_state_identity(b"done"),
            &workspace_file_state_identity(b"done\n"),
        );
        let mut conflict = Some(serde_json::Map::from_iter([
            (DESIRED_STATE_CONVERGED_FIELD.to_string(), conflict_marker),
            (
                "workspace_mutation_applied".to_string(),
                serde_json::Value::Bool(true),
            ),
        ]));
        assert!(
            consume_workspace_desired_state_convergence_marker(
                &mut conflict,
                &args,
                workspace.path(),
            )
            .is_err()
        );
    }

    #[test]
    fn convergence_snapshot_tracker_is_authority_isolated_unexpired_and_bounded() {
        let workspace = tempfile::tempdir().expect("workspace");
        fs::write(workspace.path().join("answer.txt"), "done\n").expect("target");
        let write_args = serde_json::json!({"path": "answer.txt", "content": "done\n"});
        let desired = workspace_file_state_identity(b"done\n");
        let receipt = typed_workspace_desired_state_convergence_receipt_for(
            "write_file",
            &write_args,
            workspace.path(),
            false,
            Some(&desired),
        )
        .expect("receipt");
        let receipt = &receipt[RECEIPT_FIELD];
        let read_args = serde_json::json!({"path": "answer.txt"});
        let tracker = DesiredStateConvergenceTracker::default();
        assert!(tracker.register("session-a/turn-1", receipt));
        assert!(tracker.register("session-a/turn-1", receipt));
        assert_eq!(
            tracker.pending_count(),
            1,
            "idempotent writer retries must not accumulate read obligations"
        );
        assert!(tracker.requires_snapshot_lease(
            "session-a/turn-1",
            "read_file",
            &read_args,
            workspace.path(),
        ));
        assert!(!tracker.requires_snapshot_lease(
            "session-b/turn-1",
            "read_file",
            &read_args,
            workspace.path(),
        ));
        let other_executor_tracker = DesiredStateConvergenceTracker::default();
        assert!(!other_executor_tracker.requires_snapshot_lease(
            "session-a/turn-1",
            "read_file",
            &read_args,
            workspace.path(),
        ));
        let workspace_path = workspace.path().to_path_buf();
        std::thread::scope(|scope| {
            for index in 0..200 {
                let tracker = tracker.clone();
                let read_args = read_args.clone();
                let workspace_path = workspace_path.clone();
                scope.spawn(move || {
                    assert!(!tracker.requires_snapshot_lease(
                        &format!("other-session-{index}"),
                        "read_file",
                        &read_args,
                        &workspace_path,
                    ));
                });
            }
        });
        assert!(
            tracker.requires_snapshot_lease(
                "session-a/turn-1",
                "read_file",
                &read_args,
                workspace.path(),
            ),
            "authority is lifecycle-bound, not expired by elapsed/provider delay"
        );
        tracker.clear_authority("session-a/turn-1");
        assert!(!tracker.requires_snapshot_lease(
            "session-a/turn-1",
            "read_file",
            &read_args,
            workspace.path(),
        ));

        for index in 0..MAX_PENDING_CONVERGENCE_SNAPSHOTS {
            assert!(tracker.register(&format!("capacity-run-{index}"), receipt));
        }
        assert_eq!(tracker.pending_count(), MAX_PENDING_CONVERGENCE_SNAPSHOTS);
        assert!(tracker.register("overflow-run", receipt));
        assert_eq!(tracker.pending_count(), MAX_PENDING_CONVERGENCE_SNAPSHOTS);
        assert!(
            !tracker.requires_snapshot_lease(
                "capacity-run-0",
                "read_file",
                &read_args,
                workspace.path(),
            ),
            "bounded eviction must revoke the oldest abandoned authority rather than leak"
        );
        assert!(
            tracker.requires_snapshot_lease(
                "overflow-run",
                "read_file",
                &read_args,
                workspace.path(),
            ),
            "capacity must remain recoverable for a new live turn"
        );

        let large = workspace.path().join("large.txt");
        let file = fs::File::create(&large).expect("large file");
        file.set_len(MAX_CONVERGENCE_SNAPSHOT_BYTES + 1)
            .expect("sparse large file");
        assert!(
            typed_workspace_observation_snapshot_receipt_for(
                "read_file",
                &serde_json::json!({"path": "large.txt"}),
                workspace.path(),
                false,
                true,
            )
            .is_none()
        );
    }

    #[test]
    fn structured_receipts_respect_preview_defaults_and_lsp_scope() {
        let workspace = tempfile::tempdir().expect("workspace");
        let inside = workspace.path().join("src.rs");
        std::fs::write(&inside, "fn main() {}\n").expect("source");

        assert!(
            typed_workspace_tool_receipt_for(
                "rename_symbol",
                &serde_json::json!({"symbol": "old", "new_name": "new"}),
                workspace.path(),
                false
            )
            .is_none()
        );
        assert!(
            typed_workspace_tool_receipt_for(
                "rename_symbol",
                &serde_json::json!({"symbol": "old", "new_name": "new", "dry_run": false}),
                workspace.path(),
                false
            )
            .is_some()
        );

        assert!(
            typed_workspace_observation_receipt_for(
                "lsp",
                &serde_json::json!({"operation": "hover", "file": inside}),
                workspace.path(),
                false
            )
            .is_some()
        );
        assert!(
            typed_workspace_observation_receipt_for(
                "lsp",
                &serde_json::json!({"operation": "hover", "file": "/tmp/outside.rs"}),
                workspace.path(),
                false
            )
            .is_none()
        );
        assert!(
            typed_workspace_observation_receipt_for(
                "lsp",
                &serde_json::json!({"operation": "rename", "file": inside, "dry_run": false}),
                workspace.path(),
                false
            )
            .is_none()
        );
        assert!(
            typed_workspace_tool_receipt_for(
                "lsp",
                &serde_json::json!({"operation": "rename", "file": inside, "dry_run": false}),
                workspace.path(),
                false
            )
            .is_some()
        );
    }

    #[test]
    fn typed_structured_receipts_require_applied_bound_targets() {
        let workspace = tempfile::tempdir().expect("workspace");
        let inside = workspace.path().join("out.txt");
        let outside = tempfile::NamedTempFile::new().expect("outside");
        let inside_args = serde_json::json!({"path": inside});
        let outside_args = serde_json::json!({"path": outside.path()});

        assert!(
            typed_workspace_tool_receipt_for("write_file", &inside_args, workspace.path(), false)
                .is_some()
        );
        assert!(
            typed_workspace_tool_receipt_for_applied(
                "write_file",
                &inside_args,
                workspace.path(),
                false,
                false,
            )
            .is_none()
        );
        assert!(
            typed_workspace_tool_receipt_for(
                "str_replace",
                &serde_json::json!({"path": inside, "dry_run": true}),
                workspace.path(),
                false
            )
            .is_none()
        );
        assert!(
            typed_workspace_tool_receipt_for("write_file", &outside_args, workspace.path(), false)
                .is_none()
        );

        assert!(
            typed_workspace_observation_receipt_for(
                "read_file",
                &inside_args,
                workspace.path(),
                false
            )
            .is_some()
        );
        assert!(
            typed_workspace_observation_receipt_for(
                "read_file",
                &outside_args,
                workspace.path(),
                false
            )
            .is_none()
        );
        assert!(
            typed_workspace_observation_receipt_for(
                "read_file",
                &serde_json::json!({"path": "/tmp"}),
                Path::new("/tmp/workspace"),
                false
            )
            .is_none()
        );
        assert!(
            typed_workspace_tool_receipt_for(
                "write_file",
                &serde_json::json!({"path": "/tmp/workspace/out.txt"}),
                Path::new("/tmp/workspace"),
                false
            )
            .is_some()
        );
        assert!(
            typed_workspace_tool_receipt_for(
                "str_replace",
                &serde_json::json!({
                    "edits": [
                        {"path": "inside.txt", "old_str": "a", "new_str": "b"},
                        {"path": outside.path(), "old_str": "a", "new_str": "b"}
                    ],
                    "path": "inside.txt"
                }),
                workspace.path(),
                false
            )
            .is_none()
        );
        assert!(
            typed_workspace_tool_receipt_for_applied(
                "git",
                &serde_json::json!({"action": "commit", "message": "update"}),
                workspace.path(),
                false,
                true,
            )
            .is_some(),
            "mutating git actions are bound to the owner project root"
        );
        assert!(
            typed_workspace_tool_receipt_for_applied(
                "git",
                &serde_json::json!({"action": "status"}),
                workspace.path(),
                false,
                true,
            )
            .is_none(),
            "read-only git actions must not mint mutation evidence"
        );

        #[cfg(unix)]
        {
            let external_dir = tempfile::tempdir().expect("external");
            let external_file = external_dir.path().join("outside.txt");
            fs::write(&external_file, "outside").expect("external file");
            let link = workspace.path().join("link.txt");
            std::os::unix::fs::symlink(&external_file, &link).expect("symlink");
            assert!(
                typed_workspace_tool_receipt_for(
                    "write_file",
                    &serde_json::json!({"path": link}),
                    workspace.path(),
                    false
                )
                .is_none()
            );
            assert!(
                typed_workspace_observation_receipt_for(
                    "read_file",
                    &serde_json::json!({"path": link}),
                    workspace.path(),
                    false
                )
                .is_none()
            );
        }
    }

    #[test]
    fn detached_gate_allows_only_non_mutating_builtin_shape() {
        for command in ["echo done", "printf done", "pwd", "sleep 1"] {
            assert!(bash_command_is_detachable_safe(command), "{command}");
        }
        for command in [
            "env echo done",
            "python3 -c 'open(\"out\", \"w\").write(\"x\")'",
            "git status",
            "find . -name '*.rs'",
        ] {
            assert!(!bash_command_is_detachable_safe(command), "{command}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn bounded_probe_timeout_kills_the_probe_without_blocking_the_lease() {
        let started = Instant::now();
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 2"]);
        assert!(run_bounded_probe(command, 1024, Duration::from_millis(20)).is_none());
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "probe timeout must not wait for the child command"
        );
    }

    #[test]
    fn missing_probe_binary_is_classified_as_non_git_for_manifest_fallback() {
        let command = Command::new("/definitely/missing/astra-git-probe");
        let output = run_bounded_probe(command, 1024, Duration::from_millis(50))
            .expect("missing executable is a normal non-git signal");
        assert!(!output.success);
        assert!(output.stdout.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn git_probe_is_pinned_outside_model_controlled_path() {
        let command = hardened_git_command(Path::new("/tmp")).expect("trusted git installed");
        let program = command.get_program().to_string_lossy();
        assert!(
            program == "/usr/bin/git" || program == "/bin/git" || program == "/usr/local/bin/git",
            "observer must execute only a fixed system Git: {program}"
        );
        let path = command
            .get_envs()
            .find_map(|(key, value)| (key == std::ffi::OsStr::new("PATH")).then_some(value))
            .flatten()
            .expect("probe has an explicit minimal PATH");
        assert_eq!(path, std::ffi::OsStr::new(TRUSTED_GIT_PATH));
    }

    #[test]
    fn git_fingerprint_detects_change_inside_pre_dirty_path() {
        let temp = tempfile::tempdir().expect("tempdir");
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .args(["-C", temp.path().to_str().unwrap()])
                .args(args)
                .status()
                .expect("git available");
            assert!(status.success(), "git command failed: {args:?}");
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "astra@example.invalid"]);
        run(&["config", "user.name", "Astra Test"]);
        fs::write(temp.path().join("tracked.txt"), "base").unwrap();
        run(&["add", "tracked.txt"]);
        run(&["commit", "-qm", "initial"]);

        // The path is already dirty before the observation window starts.
        fs::write(temp.path().join("tracked.txt"), "one!").unwrap();
        let before = WorkspaceFingerprint::capture(temp.path()).expect("git fingerprint");
        fs::write(temp.path().join("tracked.txt"), "two!").unwrap();
        let after = WorkspaceFingerprint::capture(temp.path()).expect("git fingerprint");
        assert!(before.changed_from(Some(after)));
    }

    #[test]
    fn git_fingerprint_detects_clean_commit_inside_bound_workspace() {
        let temp = tempfile::tempdir().expect("tempdir");
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .args(["-C", temp.path().to_str().unwrap()])
                .args(args)
                .status()
                .expect("git available");
            assert!(status.success(), "git command failed: {args:?}");
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "astra@example.invalid"]);
        run(&["config", "user.name", "Astra Test"]);
        fs::write(temp.path().join("tracked.txt"), "base").unwrap();
        run(&["add", "tracked.txt"]);
        run(&["commit", "-qm", "initial"]);

        let before = WorkspaceFingerprint::capture(temp.path()).expect("git fingerprint");
        fs::write(temp.path().join("tracked.txt"), "committed").unwrap();
        run(&["add", "tracked.txt"]);
        run(&["commit", "-qm", "change"]);
        let after = WorkspaceFingerprint::capture(temp.path()).expect("git fingerprint");
        assert!(before.changed_from(Some(after)));
    }

    #[test]
    fn git_fingerprint_handles_unborn_head_and_detects_worktree_write() {
        let temp = tempfile::tempdir().expect("tempdir");
        let status = Command::new("git")
            .args(["-C", temp.path().to_str().unwrap(), "init", "-q"])
            .status()
            .expect("git available");
        assert!(status.success());

        let before = WorkspaceFingerprint::capture(temp.path()).expect("unborn git fingerprint");
        fs::write(temp.path().join("new.txt"), "created before first commit").unwrap();
        let after = WorkspaceFingerprint::capture(temp.path()).expect("unborn git fingerprint");
        assert!(before.changed_from(Some(after)));
    }

    #[test]
    fn git_fingerprint_handles_bound_subtree_missing_from_head() {
        let temp = tempfile::tempdir().expect("tempdir");
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .args(["-C", temp.path().to_str().unwrap()])
                .args(args)
                .status()
                .expect("git available");
            assert!(status.success(), "git command failed: {args:?}");
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "astra@example.invalid"]);
        run(&["config", "user.name", "Astra Test"]);
        fs::create_dir(temp.path().join("tracked")).unwrap();
        fs::write(temp.path().join("tracked/base"), "base").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "initial"]);

        let bound = temp.path().join("generated");
        fs::create_dir(&bound).unwrap();
        let before = WorkspaceFingerprint::capture(&bound).expect("missing subtree fingerprint");
        fs::write(bound.join("artifact"), "generated").unwrap();
        let after = WorkspaceFingerprint::capture(&bound).expect("missing subtree fingerprint");
        assert!(before.changed_from(Some(after)));
    }

    #[test]
    fn writer_epoch_marks_a_recursive_writer_even_when_bytes_are_restored() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::write(temp.path().join("stable"), "same").unwrap();
        let before = WorkspaceFingerprint::capture(temp.path()).expect("fingerprint");
        let writer = begin_workspace_writer(temp.path()).expect("writer registration");
        let during = WorkspaceFingerprint::capture(temp.path());
        drop(writer);
        let after = WorkspaceFingerprint::capture(temp.path()).expect("fingerprint");
        // A recursive writer overlaps both boundaries.  Its state transition
        // is intentionally not attributed to the surrounding Bash window,
        // even if bytes happen to be restored before the post snapshot.
        assert!(during.is_none());
        assert!(!before.changed_from(Some(after)));
    }

    #[test]
    fn unsettled_writer_quarantines_future_fingerprints() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::write(temp.path().join("stable"), "same").unwrap();
        let before = WorkspaceFingerprint::capture(temp.path()).expect("fingerprint");

        assert!(mark_workspace_observation_unsettled(temp.path()));
        drop(before);
        assert_eq!(
            workspace_observation_is_quarantined(temp.path()),
            Some(true)
        );
        assert_eq!(workspace_ownership_is_unsettled(temp.path()), Some(true));
        assert!(
            WorkspaceFingerprint::capture(temp.path()).is_none(),
            "an unowned descendant must make later observations fail closed"
        );
    }

    #[test]
    fn foreground_receipt_quarantines_attribution_without_blocking_completion() {
        let temp = tempfile::tempdir().unwrap();
        assert!(quarantine_after_weak_receipt(
            temp.path(),
            Some(FOREGROUND_PROCESS_GROUP_OWNERSHIP)
        ));
        assert_eq!(
            workspace_observation_is_quarantined(temp.path()),
            Some(true)
        );
        assert_eq!(workspace_ownership_is_unsettled(temp.path()), Some(false));
        assert!(
            WorkspaceFingerprint::capture(temp.path()).is_none(),
            "later calls must not receive a potentially misattributed fingerprint"
        );
        assert!(!quarantine_after_weak_receipt(
            temp.path(),
            Some(INVOCATION_CGROUP_OWNERSHIP)
        ));
    }

    #[test]
    fn unsettled_writer_keeps_quarantine_when_bound_root_disappears() {
        let parent = tempfile::tempdir().expect("parent tempdir");
        let root = parent.path().join("workspace");
        fs::create_dir(&root).expect("workspace");
        let other = tempfile::tempdir().expect("other tempdir");
        let writer = begin_workspace_writer(&root).expect("writer registration");

        fs::remove_dir_all(&root).expect("remove workspace during invocation");
        assert!(mark_workspace_observation_unsettled(&root));
        drop(writer);

        assert_eq!(workspace_observation_is_quarantined(&root), Some(true));
        assert!(WorkspaceFingerprint::capture(&root).is_none());
        assert_eq!(
            workspace_observation_is_quarantined(other.path()),
            Some(false)
        );
    }

    #[cfg(unix)]
    #[test]
    fn unsettled_writer_keeps_identity_when_symlink_binding_is_repointed() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().expect("parent tempdir");
        let first = parent.path().join("first");
        let second = parent.path().join("second");
        let link = parent.path().join("bound");
        fs::create_dir(&first).expect("first workspace");
        fs::create_dir(&second).expect("second workspace");
        symlink(&first, &link).expect("initial binding");
        let writer = begin_workspace_writer(&link).expect("writer registration");

        fs::remove_file(&link).expect("remove old binding");
        symlink(&second, &link).expect("repoint binding");
        assert!(mark_workspace_observation_unsettled(&link));
        drop(writer);

        assert_eq!(workspace_observation_is_quarantined(&link), Some(true));
        assert_eq!(workspace_observation_is_quarantined(&first), Some(true));
        assert_eq!(workspace_observation_is_quarantined(&second), Some(true));
    }

    #[tokio::test]
    async fn lease_wait_honors_cancellation_and_can_be_reacquired() {
        let temp = tempfile::tempdir().expect("tempdir");
        let first = acquire_workspace_observation_lease(temp.path())
            .await
            .expect("first lease");
        let cancel = CancellationToken::new();
        let root = temp.path().to_path_buf();
        let waiter_cancel = cancel.clone();
        let waiter = tokio::spawn(async move {
            acquire_workspace_observation_lease_with_options(
                &root,
                Some(&waiter_cancel),
                Duration::from_secs(30),
            )
            .await
            .is_some()
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.cancel();
        let acquired = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("cancelled lease waiter must return")
            .expect("waiter task");
        assert!(!acquired);
        drop(first);
        assert!(
            tokio::time::timeout(
                Duration::from_secs(1),
                acquire_workspace_observation_lease_with_options(
                    temp.path(),
                    None,
                    Duration::from_millis(100),
                ),
            )
            .await
            .expect("lease reacquisition timeout")
            .is_some()
        );
    }

    #[test]
    fn blocking_lease_wait_honors_cancellation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let first = acquire_workspace_observation_lease_sync(temp.path(), Duration::from_secs(1))
            .expect("first lease");
        let cancel = CancellationToken::new();
        let waiter_cancel = cancel.clone();
        let root = temp.path().to_path_buf();
        let waiter = std::thread::spawn(move || {
            acquire_workspace_observation_lease_sync_with_options(
                &root,
                Some(&waiter_cancel),
                Duration::from_secs(30),
            )
            .is_some()
        });
        std::thread::sleep(Duration::from_millis(20));
        cancel.cancel();
        assert!(!waiter.join().expect("blocking waiter"));
        drop(first);
    }

    #[test]
    fn workspace_observation_lease_serializes_other_astra_process_writers() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mutation_marker = temp.path().join("typed-writer");
        let recursive_marker = temp.path().join("recursive-writer");
        let lease = acquire_workspace_observation_lease_sync(temp.path(), Duration::from_secs(1))
            .expect("parent lease");

        let spawn_helper = |mode: &str, marker: &Path| {
            Command::new(std::env::current_exe().expect("current test executable"))
            .arg("workspace_observation::tests::cross_process_workspace_observation_lease_helper")
            .arg("--exact")
            .arg("--nocapture")
            .env(CROSS_PROCESS_LEASE_HELPER_ENV, temp.path())
                .env(CROSS_PROCESS_LEASE_MARKER_ENV, marker)
                .env(CROSS_PROCESS_LEASE_MODE_ENV, mode)
            .spawn()
                .expect("spawn second Astra process")
        };
        let mut mutation_child = spawn_helper("mutation", &mutation_marker);
        let mut recursive_child = spawn_helper("recursive-writer", &recursive_marker);

        std::thread::sleep(Duration::from_millis(150));
        assert!(
            !mutation_marker.exists(),
            "a typed writer must wait outside the parent's observation window"
        );
        assert!(
            !recursive_marker.exists(),
            "run_script's recursive writer must wait outside the parent's observation window"
        );
        drop(lease);

        let deadline = Instant::now() + Duration::from_secs(5);
        while (!mutation_marker.exists() || !recursive_marker.exists()) && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        for (child, label) in [
            (&mut mutation_child, "mutation"),
            (&mut recursive_child, "recursive writer"),
        ] {
            let status = child.wait().expect("wait for lease helper");
            assert!(status.success(), "{label} helper failed: {status}");
        }
        assert_eq!(
            fs::read_to_string(mutation_marker).unwrap(),
            "owned by mutation"
        );
        assert_eq!(
            fs::read_to_string(recursive_marker).unwrap(),
            "owned by recursive writer"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn same_uid_lock_replacement_cannot_admit_a_second_process_generation() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("tempdir");
        let marker = temp.path().join("second-generation-writer");
        let lease = acquire_workspace_observation_lease_sync(temp.path(), Duration::from_secs(1))
            .expect("first generation");
        let lock_path = lease.locks[0].path.clone();
        fs::remove_file(&lock_path).unwrap();
        let replacement = fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        replacement
            .set_permissions(fs::Permissions::from_mode(0o600))
            .unwrap();
        drop(replacement);

        let mut child = Command::new(std::env::current_exe().unwrap())
            .arg("workspace_observation::tests::cross_process_workspace_observation_lease_helper")
            .arg("--exact")
            .arg("--nocapture")
            .env(CROSS_PROCESS_LEASE_HELPER_ENV, temp.path())
            .env(CROSS_PROCESS_LEASE_MARKER_ENV, &marker)
            .env(CROSS_PROCESS_LEASE_MODE_ENV, "mutation")
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            !marker.exists(),
            "a replacement file inode must not create a simultaneous kernel lock generation"
        );
        assert!(!lease.integrity_valid());
        drop(lease);

        assert!(child.wait().unwrap().success());
        assert_eq!(fs::read_to_string(marker).unwrap(), "owned by mutation");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn untrusted_kernel_prebind_wait_is_cancelable_and_never_authoritative() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (specifications, _) =
            workspace_coordination_lock_specs(temp.path(), CoordinationLockKind::Observation)
                .unwrap();
        let blocker =
            try_acquire_kernel_coordination_namespace(&specifications[0].kernel_namespace_key)
                .unwrap()
                .expect("simulated foreign prebind");
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            trigger.cancel();
        });

        assert!(
            acquire_workspace_observation_lease_with_options(
                temp.path(),
                Some(&cancel),
                Duration::from_secs(30),
            )
            .await
            .is_none(),
            "an existing abstract name is only contention and never trusted authority"
        );
        drop(blocker);
        assert!(
            acquire_workspace_observation_lease_with_options(
                temp.path(),
                None,
                Duration::from_secs(1),
            )
            .await
            .is_some(),
            "cancellation must release the in-process gate for a later clean acquisition"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn kernel_namespace_is_released_by_holder_crash_and_restart() {
        let temp = tempfile::tempdir().expect("tempdir");
        let marker = temp.path().join("kernel-holder-ready");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .arg("workspace_observation::tests::cross_process_workspace_observation_lease_helper")
            .arg("--exact")
            .arg("--nocapture")
            .env(CROSS_PROCESS_LEASE_HELPER_ENV, temp.path())
            .env(CROSS_PROCESS_LEASE_MARKER_ENV, &marker)
            .env(CROSS_PROCESS_LEASE_MODE_ENV, "kernel-namespace-holder")
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !marker.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(marker.exists(), "holder did not bind its kernel namespace");
        assert!(
            acquire_workspace_observation_lease_sync(temp.path(), Duration::from_millis(50))
                .is_none()
        );

        child.kill().unwrap();
        let _ = child.wait().unwrap();
        assert!(
            acquire_workspace_observation_lease_sync(temp.path(), Duration::from_secs(1)).is_some(),
            "kernel-owned abstract names must disappear when a holder crashes"
        );
    }

    #[tokio::test]
    async fn opaque_writer_is_exclusive_and_wait_is_cancelable() {
        let temp = tempfile::tempdir().expect("tempdir");
        let writer = begin_workspace_writer_with_options(temp.path(), None, Duration::from_secs(1))
            .await
            .expect("opaque writer");
        let nested = acquire_workspace_mutation_lease_with_options(
            temp.path(),
            None,
            Duration::from_millis(100),
        )
        .await;
        assert!(
            nested.is_none(),
            "only the authenticated task-local RPC route may reuse an opaque writer lease"
        );
        assert!(
            begin_workspace_writer_with_options(temp.path(), None, Duration::from_millis(50),)
                .await
                .is_none(),
            "two top-level run_script writers must mutually exclude"
        );

        let cancel = CancellationToken::new();
        let waiter_cancel = cancel.clone();
        let root = temp.path().to_path_buf();
        let waiter = tokio::spawn(async move {
            begin_workspace_writer_with_options(
                &root,
                Some(&waiter_cancel),
                Duration::from_secs(30),
            )
            .await
            .is_some()
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.cancel();
        assert!(
            !tokio::time::timeout(Duration::from_secs(1), waiter)
                .await
                .expect("cancelled barrier waiter timeout")
                .expect("barrier waiter task")
        );
        drop(writer);
    }

    #[test]
    fn coordination_files_are_stable_external_and_do_not_create_a_manifest_delta() {
        let temp = tempfile::tempdir().expect("tempdir");
        let before = WorkspaceFingerprint::capture(temp.path()).expect("before fingerprint");
        let lease = acquire_workspace_observation_lease_sync(temp.path(), Duration::from_secs(1))
            .expect("stable external lease");

        assert!(!temp.path().join(".astra").exists());
        assert!(!lease.locks.is_empty());
        assert!(
            lease.locks.iter().all(|lock| lock.path.starts_with("/tmp")),
            "coordination files must be outside the tool-writable workspace"
        );
        assert!(lease.integrity_valid());
        let after = WorkspaceFingerprint::capture(temp.path()).expect("after fingerprint");
        assert!(
            !before.changed_from(Some(after)),
            "internal lock creation must not look like a user workspace mutation"
        );

        #[cfg(unix)]
        assert!(stable_coordination_root().is_some());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unrelated_sticky_root_activity_does_not_revoke_generation() {
        let workspace = tempfile::tempdir().expect("workspace");
        let lease =
            acquire_workspace_observation_lease_sync(workspace.path(), Duration::from_secs(1))
                .expect("lease");

        let unrelated = tempfile::tempdir_in("/tmp").expect("unrelated tempdir");
        fs::write(unrelated.path().join("traffic"), "unrelated").unwrap();
        drop(unrelated);

        assert!(
            lease.integrity_valid(),
            "activity in the privileged shared ancestor must not revoke an unrelated workspace"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ordinary_workspace_write_preserves_generation_integrity() {
        let workspace = tempfile::tempdir().expect("workspace");
        let lease =
            acquire_workspace_observation_lease_sync(workspace.path(), Duration::from_secs(1))
                .expect("lease");

        fs::write(workspace.path().join("result.txt"), "committed").unwrap();

        assert!(
            lease.integrity_valid(),
            "a tool-owned child-path mutation is the receipt payload, not a binding replacement"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn three_hundred_distinct_workspace_leases_share_one_inotify_instance() {
        fn inotify_fd_count() -> usize {
            fs::read_dir("/proc/self/fd")
                .expect("Linux proc fd directory")
                .filter_map(Result::ok)
                .filter_map(|entry| fs::read_link(entry.path()).ok())
                .filter(|target| target.as_os_str() == "anon_inode:inotify")
                .count()
        }

        let parent = tempfile::tempdir().expect("workspace parent");
        let roots = (0..300)
            .map(|index| {
                let root = parent.path().join(format!("workspace-{index:03}"));
                fs::create_dir(&root).expect("create distinct workspace");
                root
            })
            .collect::<Vec<_>>();
        let inotify_before = inotify_fd_count();
        let starting_line = Arc::new(tokio::sync::Barrier::new(roots.len() + 1));
        let mut acquisitions = tokio::task::JoinSet::new();
        for root in roots.iter().cloned() {
            let starting_line = starting_line.clone();
            acquisitions.spawn(async move {
                starting_line.wait().await;
                acquire_workspace_observation_lease_with_options(
                    &root,
                    None,
                    Duration::from_secs(10),
                )
                .await
            });
        }
        starting_line.wait().await;
        let started = Instant::now();
        let mut leases = Vec::with_capacity(roots.len());
        while let Some(result) = acquisitions.join_next().await {
            if let Some(lease) = result.expect("concurrent lease task must not panic") {
                leases.push(lease);
            }
        }

        assert_eq!(
            leases.len(),
            roots.len(),
            "300 independent users/workspaces must not exhaust the host's per-user inotify-instance budget"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "300 simultaneous workspace admissions must complete within the shared latency budget"
        );
        assert!(
            leases
                .iter()
                .all(WorkspaceObservationLease::integrity_valid),
            "every concurrently-held workspace generation must retain receipt authority"
        );
        assert!(
            inotify_fd_count() <= inotify_before.saturating_add(1),
            "workspace leases must multiplex one process-level inotify instance"
        );
        let (active_subscriptions, active_watch_paths) = generation_watch_registry()
            .expect("process-level watcher")
            .stats()
            .expect("watcher stats");
        assert!(active_subscriptions >= 300, "all leases must be registered");
        assert!(
            active_watch_paths <= MAX_ACTIVE_GENERATION_WATCH_PATHS,
            "the process watcher must enforce its explicit active-path bound"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn multiplexed_watch_registration_cancellation_is_prompt_and_leaves_no_authority() {
        let workspace = tempfile::tempdir().expect("workspace");
        let path = workspace.path().to_path_buf();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let registry = GenerationWatchRegistry::start().expect("isolated process watcher");
        let started = Instant::now();
        let error = registry
            .register(
                vec![GenerationWatchSpec {
                    path: path.clone(),
                    mask: libc::IN_DELETE_SELF | libc::IN_MOVE_SELF,
                }],
                Some(&cancel),
            )
            .err()
            .expect("cancelled registration must grant no authority");

        assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "watch registration cancellation must not wait for the control timeout"
        );
        assert_eq!(
            registry.contains_path(path),
            Some(false),
            "a cancelled registration must be retired before the next control-plane fence"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cancel_after_watch_admission_before_consumption_retires_to_baseline() {
        let workspace = tempfile::tempdir().expect("workspace");
        let registry = GenerationWatchRegistry::start().expect("isolated process watcher");
        let baseline = registry.stats().expect("baseline stats");
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (reply, response) = mpsc::sync_channel(1);
        registry
            .commands
            .send(GenerationWatchCommand::Register {
                specifications: vec![GenerationWatchSpec {
                    path: workspace.path().to_path_buf(),
                    mask: libc::IN_DELETE_SELF | libc::IN_MOVE_SELF,
                }],
                tampered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                cancelled: cancelled.clone(),
                retire_commands: registry.commands.clone(),
                alive: registry.alive.clone(),
                reply,
            })
            .expect("queue provisional admission");
        let admitted = registry.stats().expect("admission fence");
        assert_eq!(admitted.0, baseline.0 + 1, "one provisional subscription");
        cancelled.store(true, std::sync::atomic::Ordering::Release);
        drop(response);

        assert_eq!(
            registry.stats(),
            Some(baseline),
            "dropping a cancelled, admitted-but-unconsumed response must synchronously retire its ID before later control-plane observations"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn multiplexed_watch_capacity_failure_is_bounded_and_diagnostic() {
        use std::os::fd::FromRawFd;

        let raw_fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
        assert!(
            raw_fd >= 0,
            "test inotify instance: {}",
            std::io::Error::last_os_error()
        );
        let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw_fd) };
        let mut worker = GenerationWatchWorker::new(fd);
        for index in 0..MAX_ACTIVE_GENERATION_WATCH_PATHS {
            worker
                .descriptor_by_path
                .insert(PathBuf::from(format!("/capacity/{index}")), index as i32);
        }
        let error = worker
            .register(
                vec![GenerationWatchSpec {
                    path: PathBuf::from("/capacity/overflow"),
                    mask: libc::IN_DELETE_SELF,
                }],
                Arc::new(std::sync::atomic::AtomicBool::new(false)),
            )
            .expect_err("capacity overflow must fail closed");

        assert!(error.contains("capacity exceeded"), "{error}");
        assert!(
            error.contains(&format!("active_paths={MAX_ACTIVE_GENERATION_WATCH_PATHS}")),
            "{error}"
        );
        assert!(error.contains("requested_new_paths=1"), "{error}");
        assert!(
            error.contains(&format!("limit={MAX_ACTIVE_GENERATION_WATCH_PATHS}")),
            "{error}"
        );
        assert!(worker.subscriptions.is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn multiplexed_watch_overflow_and_worker_exit_revoke_all_authority() {
        use std::os::fd::FromRawFd;

        let raw_fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
        assert!(raw_fd >= 0, "test inotify instance");
        let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw_fd) };
        let mut worker = GenerationWatchWorker::new(fd);
        let overflow_authority = Arc::new(std::sync::atomic::AtomicBool::new(false));
        worker.subscriptions.insert(
            1,
            ActiveGenerationSubscription {
                tampered: overflow_authority.clone(),
                watches: Vec::new(),
            },
        );
        worker.apply_event(-1, libc::IN_Q_OVERFLOW);
        assert!(
            overflow_authority.load(std::sync::atomic::Ordering::Acquire),
            "an overflow must revoke every active subscription"
        );

        let (commands, receiver) = mpsc::channel();
        drop(receiver);
        let exited_registry = Arc::new(GenerationWatchRegistry {
            commands,
            alive: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        });
        let exited_subscription = GenerationWatchSubscription {
            id: 1,
            tampered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            registry: exited_registry,
        };
        assert!(
            !exited_subscription.is_untampered(),
            "a dead watcher cannot retain receipt authority"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn retired_watch_events_do_not_contaminate_the_next_generation() {
        let directory = tempfile::tempdir().expect("watch directory");
        let retired_path = directory.path().join("retired");
        let current_path = directory.path().join("current");
        fs::write(&retired_path, "old").expect("retired file");
        fs::write(&current_path, "new").expect("current file");
        let registry = GenerationWatchRegistry::start().expect("private watcher");
        let retired = registry
            .register(
                vec![GenerationWatchSpec {
                    path: retired_path.clone(),
                    mask: libc::IN_ATTRIB | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF,
                }],
                None,
            )
            .expect("retired generation");
        drop(retired);
        registry.stats().expect("retirement fence");

        let current = registry
            .register(
                vec![GenerationWatchSpec {
                    path: current_path.clone(),
                    mask: libc::IN_ATTRIB | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF,
                }],
                None,
            )
            .expect("current generation");
        fs::remove_file(retired_path).expect("mutate retired generation");
        assert!(
            current.is_untampered(),
            "retired events must be drained before descriptor reuse can authorize a new generation"
        );
        fs::remove_file(current_path).expect("mutate current generation");
        assert!(
            !current.is_untampered(),
            "the current generation must still receive its own terminal event"
        );
    }

    #[test]
    fn git_fingerprint_excludes_workspace_coordination_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        let run = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .args(["-C", temp.path().to_str().unwrap()])
                    .args(args)
                    .status()
                    .expect("git available")
                    .success()
            );
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "astra@example.invalid"]);
        run(&["config", "user.name", "Astra Test"]);
        fs::write(temp.path().join("tracked"), "base").unwrap();
        run(&["add", "tracked"]);
        run(&["commit", "-qm", "initial"]);

        let before = WorkspaceFingerprint::capture(temp.path()).expect("before fingerprint");
        let _lease = acquire_workspace_observation_lease_sync(temp.path(), Duration::from_secs(1))
            .expect("workspace-local lease");
        let after = WorkspaceFingerprint::capture(temp.path()).expect("after fingerprint");
        assert!(!before.changed_from(Some(after)));
    }

    #[test]
    fn replaced_coordination_file_revokes_lease_integrity() {
        let temp = tempfile::tempdir().expect("tempdir");
        let lease = acquire_workspace_observation_lease_sync(temp.path(), Duration::from_secs(1))
            .expect("lease");
        let observation = lease.locks[0].path.clone();
        fs::remove_file(&observation).expect("remove locked path");
        let replacement = fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&observation)
            .expect("replace locked path");
        assert!(
            !lease.integrity_valid(),
            "an unlinked/recreated lock path cannot retain receipt authority"
        );
        drop(replacement);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn transient_lock_generation_split_is_sticky_even_after_inode_restore() {
        let temp = tempfile::tempdir().expect("tempdir");
        let lease = acquire_workspace_observation_lease_sync(temp.path(), Duration::from_secs(1))
            .expect("lease");
        let lock_path = lease.locks[0].path.clone();
        let saved = lock_path.with_extension("saved-by-integrity-test");
        if saved.exists() {
            fs::remove_file(&saved).unwrap();
        }

        fs::rename(&lock_path, &saved).unwrap();
        let replacement = fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        drop(replacement);
        fs::remove_file(&lock_path).unwrap();
        fs::rename(&saved, &lock_path).unwrap();

        assert!(
            !lease.integrity_valid(),
            "kernel event history must catch rename/recreate/restore, not only final inode equality"
        );
        assert!(
            !lease.integrity_valid(),
            "tamper evidence must remain sticky after the event queue is drained"
        );
    }

    #[test]
    fn typed_commit_after_generation_change_has_zero_durable_receipt() {
        let temp = tempfile::tempdir().expect("tempdir");
        let lease = acquire_workspace_mutation_lease_sync_with_options(
            temp.path(),
            None,
            Duration::from_secs(1),
        )
        .expect("typed writer lease");
        let target = temp.path().join("answer.txt");
        fs::write(&target, "committed").expect("typed commit");

        let lock_path = lease.locks[0].path.clone();
        fs::remove_file(&lock_path).expect("invalidate coordination generation");
        let replacement = fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&lock_path)
            .expect("replacement generation");

        assert_eq!(fs::read_to_string(target).unwrap(), "committed");
        assert!(!lease.integrity_valid());
        let receipt = typed_workspace_tool_receipt_for_applied(
            "write_file",
            &serde_json::json!({"path": "answer.txt", "content": "committed"}),
            temp.path(),
            false,
            lease.integrity_valid(),
        );
        assert!(
            receipt.is_none(),
            "a committed write in a revoked generation must issue zero durable receipt"
        );
        drop(replacement);
    }

    #[cfg(unix)]
    #[test]
    fn lexical_symlink_and_target_share_lock_and_repoint_revokes_generation() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("tempdir");
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        let binding = temp.path().join("binding");
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        symlink(&first, &binding).unwrap();

        let lease = acquire_workspace_observation_lease_sync(&binding, Duration::from_secs(1))
            .expect("symlink binding lease");
        assert!(
            acquire_workspace_observation_lease_sync(&first, Duration::from_millis(50)).is_none(),
            "canonical target must share a lock with its lexical symlink binding"
        );
        fs::remove_file(&binding).unwrap();
        symlink(&second, &binding).unwrap();
        assert!(
            !lease.integrity_valid(),
            "retargeting the lexical binding must revoke the admitted generation"
        );
    }

    #[test]
    fn root_and_parent_replacement_revoke_generation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let parent = temp.path().join("parent");
        let root = parent.join("workspace");
        fs::create_dir_all(&root).unwrap();
        let lease = acquire_workspace_observation_lease_sync(&root, Duration::from_secs(1))
            .expect("binding lease");

        fs::rename(&parent, temp.path().join("old-parent")).unwrap();
        fs::create_dir_all(&root).unwrap();
        assert!(
            !lease.integrity_valid(),
            "replacing a parent/root path must revoke the admitted generation"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn transient_parent_replacement_is_detected_after_original_is_restored() {
        let temp = tempfile::tempdir().expect("tempdir");
        let parent = temp.path().join("parent");
        let root = parent.join("workspace");
        let saved = temp.path().join("saved-parent");
        fs::create_dir_all(&root).unwrap();
        let lease = acquire_workspace_observation_lease_sync(&root, Duration::from_secs(1))
            .expect("binding lease");

        fs::rename(&parent, &saved).unwrap();
        fs::create_dir_all(&root).unwrap();
        fs::remove_dir_all(&parent).unwrap();
        fs::rename(&saved, &parent).unwrap();

        assert!(
            !lease.integrity_valid(),
            "transient parent replacement cannot restore receipt authority"
        );
    }

    #[cfg(unix)]
    #[test]
    fn foreign_or_insecure_precreation_is_rejected() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("tempdir");
        let (paths, _) =
            workspace_coordination_lock_paths(temp.path(), CoordinationLockKind::Observation)
                .expect("coordination namespace");
        for path in &paths {
            if path.exists() {
                fs::remove_file(path).unwrap();
            }
            let file = fs::OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .open(path)
                .unwrap();
            file.set_permissions(fs::Permissions::from_mode(0o666))
                .unwrap();
        }
        assert!(
            acquire_workspace_observation_lease_sync(temp.path(), Duration::from_millis(50))
                .is_none(),
            "a permissive/foreign-shaped predictable inode must be rejected, never trusted"
        );
    }

    #[cfg(unix)]
    #[test]
    fn foreign_owner_precreation_is_rejected_by_lock_shape_contract() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::NamedTempFile::new_in("/tmp").unwrap();
        temp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .unwrap();
        let metadata = temp.as_file().metadata().unwrap();
        let foreign_effective_uid = unsafe { libc::geteuid() }.wrapping_add(1);

        assert!(unix_coordination_lock_metadata_is_trusted(
            &metadata,
            unsafe { libc::geteuid() }
        ));
        assert!(
            !unix_coordination_lock_metadata_is_trusted(&metadata, foreign_effective_uid),
            "the same safe-shaped inode must be rejected when it belongs to another OS user"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sequential_cross_uid_generations_share_global_mutex_not_integrity_witness() {
        let workspace = tempfile::tempdir().expect("workspace");
        let uid_a = unsafe { libc::geteuid() };
        let uid_b = uid_a.wrapping_add(1);
        let (specifications_a, _) = workspace_coordination_lock_specs_for_uid(
            workspace.path(),
            CoordinationLockKind::Observation,
            uid_a,
        )
        .expect("UID A coordination specification");
        let (specifications_b, _) = workspace_coordination_lock_specs_for_uid(
            workspace.path(),
            CoordinationLockKind::Observation,
            uid_b,
        )
        .expect("UID B coordination specification");
        assert_eq!(specifications_a.len(), specifications_b.len());

        for (specification_a, specification_b) in specifications_a.iter().zip(&specifications_b) {
            assert_eq!(
                specification_a.kernel_namespace_key, specification_b.kernel_namespace_key,
                "cross-UID mutual exclusion must remain global and kernel-owned"
            );
            assert_ne!(
                specification_a.witness_path, specification_b.witness_path,
                "one UID must never depend on another UID's persistent 0600 witness"
            );
            assert!(
                specification_a
                    .witness_path
                    .file_name()
                    .and_then(std::ffi::OsStr::to_str)
                    .is_some_and(|name| name.contains(&format!("-uid-{uid_a}.")))
            );
            assert!(
                specification_b
                    .witness_path
                    .file_name()
                    .and_then(std::ffi::OsStr::to_str)
                    .is_some_and(|name| name.contains(&format!("-uid-{uid_b}.")))
            );
        }

        let first = acquire_cross_process_lock_sync(
            specifications_a[0].clone(),
            CrossProcessLockMode::Exclusive,
            None,
            Duration::from_secs(1),
        )
        .expect("UID A modeled generation");
        assert!(
            acquire_cross_process_lock_sync(
                specifications_b[0].clone(),
                CrossProcessLockMode::Exclusive,
                None,
                Duration::from_millis(25),
            )
            .is_none(),
            "a different UID witness must not bypass the global abstract-UDS mutex"
        );
        drop(first);
        let second = acquire_cross_process_lock_sync(
            specifications_b[0].clone(),
            CrossProcessLockMode::Exclusive,
            None,
            Duration::from_secs(1),
        )
        .expect("UID B must proceed after UID A releases the global generation");
        assert!(second.path_identity_is_unchanged());
    }

    #[test]
    fn missing_workspace_cannot_create_a_coordination_authority() {
        let temp = tempfile::tempdir().expect("tempdir");
        let missing = temp.path().join("missing");
        assert!(
            acquire_workspace_observation_lease_sync(&missing, Duration::from_millis(20)).is_none()
        );
        assert!(!missing.join(".astra").exists());
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn platform_without_stable_tamper_watch_refuses_receipt_authority() {
        let temp = tempfile::tempdir().expect("tempdir");
        assert!(stable_coordination_root().is_none());
        assert!(
            acquire_workspace_observation_lease_sync(temp.path(), Duration::from_millis(20))
                .is_none(),
            "unsupported platforms must reject execution instead of claiming cross-user authority"
        );
    }

    #[test]
    fn fallback_ignores_mtime_but_detects_same_size_rewrite_and_mode_change() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("value");
        fs::write(&path, b"abcd").unwrap();
        let before = manifest_snapshot(temp.path());
        fs::write(&path, b"wxyz").unwrap();
        let after_content = manifest_snapshot(temp.path());
        assert!(before.changed_from(Some(after_content)));

        fs::write(&path, b"abcd").unwrap();
        let restored = manifest_snapshot(temp.path());
        assert!(!before.changed_from(Some(restored)));

        // A metadata-only timestamp change is not a deliverable mutation.
        let touch = Command::new("touch").arg(&path).status().unwrap();
        assert!(touch.success());
        let touched = manifest_snapshot(temp.path());
        assert!(!before.changed_from(Some(touched)));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(0o600);
            fs::set_permissions(&path, permissions).unwrap();
            let mode_changed = manifest_snapshot(temp.path());
            assert!(before.changed_from(Some(mode_changed)));
        }
    }

    #[test]
    fn git_fingerprint_is_bound_to_subdirectory_and_observes_direct_ignored_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .args(["-C", temp.path().to_str().unwrap()])
                .args(args)
                .status()
                .expect("git available");
            assert!(status.success(), "git command failed: {args:?}");
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "astra@example.invalid"]);
        run(&["config", "user.name", "Astra Test"]);
        fs::create_dir_all(temp.path().join("app")).unwrap();
        fs::create_dir_all(temp.path().join("sibling")).unwrap();
        fs::write(temp.path().join("app/tracked"), "base").unwrap();
        fs::write(temp.path().join("sibling/outside"), "base").unwrap();
        fs::write(temp.path().join(".gitignore"), "app/ignored.txt\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "initial"]);
        fs::write(temp.path().join("app/ignored.txt"), "one").unwrap();

        let app = temp.path().join("app");
        let before = WorkspaceFingerprint::capture(&app).expect("git fingerprint");
        fs::write(temp.path().join("sibling/outside"), "changed").unwrap();
        let sibling_only = WorkspaceFingerprint::capture(&app).expect("git fingerprint");
        assert!(!before.changed_from(Some(sibling_only)));

        fs::write(temp.path().join("app/ignored.txt"), "two").unwrap();
        let ignored_changed = WorkspaceFingerprint::capture(&app).expect("git fingerprint");
        assert!(before.changed_from(Some(ignored_changed)));
    }

    #[test]
    fn ignored_directory_is_bounded_without_walking_an_unbounded_tree() {
        let temp = tempfile::tempdir().expect("tempdir");
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .args(["-C", temp.path().to_str().unwrap()])
                .args(args)
                .status()
                .expect("git available");
            assert!(status.success());
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "astra@example.invalid"]);
        run(&["config", "user.name", "Astra Test"]);
        fs::write(temp.path().join(".gitignore"), "build/\n").unwrap();
        fs::write(temp.path().join("tracked"), "base").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "initial"]);
        fs::create_dir(temp.path().join("build")).unwrap();
        for index in 0..100 {
            fs::write(temp.path().join(format!("build/file-{index}")), "x").unwrap();
        }
        let before = WorkspaceFingerprint::capture(temp.path()).expect("bounded git fingerprint");
        fs::write(temp.path().join("build/file-1"), "changed").unwrap();
        let after = WorkspaceFingerprint::capture(temp.path()).expect("bounded git fingerprint");
        assert!(before.changed_from(Some(after)));

        // A cache-sized ignored tree is not silently treated as unchanged;
        // the bounded observer fails closed once its evidence budget is
        // exceeded.
        fs::create_dir(temp.path().join("build/large")).unwrap();
        for index in 0..(MAX_IGNORED_ENTRIES + 1) {
            fs::write(temp.path().join(format!("build/large/file-{index}")), "x").unwrap();
        }
        assert!(WorkspaceFingerprint::capture(temp.path()).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn ignored_directory_symlink_does_not_escape_bound_workspace() {
        let temp = tempfile::tempdir().expect("tempdir");
        let external = tempfile::tempdir().expect("external tempdir");
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .args(["-C", temp.path().to_str().unwrap()])
                .args(args)
                .status()
                .expect("git available");
            assert!(status.success(), "git command failed: {args:?}");
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "astra@example.invalid"]);
        run(&["config", "user.name", "Astra Test"]);
        fs::write(temp.path().join(".gitignore"), "ignored/\n").unwrap();
        fs::write(temp.path().join("tracked"), "base").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "initial"]);
        fs::write(external.path().join("payload"), "one").unwrap();
        std::os::unix::fs::symlink(external.path(), temp.path().join("ignored")).unwrap();

        let before = WorkspaceFingerprint::capture(temp.path()).expect("fingerprint");
        fs::write(external.path().join("payload"), "two").unwrap();
        let after = WorkspaceFingerprint::capture(temp.path()).expect("fingerprint");
        assert!(
            !before.changed_from(Some(after)),
            "external symlink target must not affect bound workspace receipt"
        );
    }
}

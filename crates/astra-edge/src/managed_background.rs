//! Background lifecycle for an MOI user-runner command.
//!
//! The connect command is not given a new flag. Detach happens only when that
//! existing argv is started from a terminal. Sandbox and image-managed runners
//! pass credentials through the environment and stay in the foreground.

use fs2::FileExt;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::future::Future;
use std::io::{self, ErrorKind, Read, Seek, SeekFrom, Write};
use std::os::fd::FromRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, Once, OnceLock};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

pub const FOREGROUND_ENV: &str = "ASTRA_EDGE_FOREGROUND";
pub const MANAGED_ENV: &str = "ASTRA_EDGE_MANAGED";
const STATE_DIR_ENV: &str = "ASTRA_EDGE_STATE_DIR";
const READY_FD_ENV: &str = "ASTRA_EDGE_READY_FD";
const READY_TIMEOUT: Duration = Duration::from_secs(60);
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
const LOG_ROTATE_BYTES: u64 = 8 * 1024 * 1024;
const LOG_ROTATE_INTERVAL: Duration = Duration::from_secs(30);
const LOG_TAIL_LINES: usize = 200;
const LOG_TAIL_BYTES: u64 = 256 * 1024;

static INSTANCE_LOCK: Mutex<Option<File>> = Mutex::new(None);
static SHUTDOWN: OnceLock<CancellationToken> = OnceLock::new();
static STARTUP_CANCELLED: AtomicBool = AtomicBool::new(false);
static PARENT_RELEASED: AtomicBool = AtomicBool::new(false);
/// Epoch of the attempt whose ready byte was written, if any.
static SENT_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// A ready byte for `SENT_EPOCH` is in the parent socket.
static READY_SENT: AtomicBool = AtomicBool::new(false);
static PHASE_STATE: Mutex<PhaseState> = Mutex::new(PhaseState {
    epoch: 0,
    connected: None,
});

#[derive(Debug)]
enum StartupAcceptError {
    /// The starting command closed its end of the readiness socket.
    ParentGone,
    /// This attempt left `connected` before the parent accepted it.
    ConnectionEnded,
}

impl std::fmt::Display for StartupAcceptError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ParentGone => write!(
                formatter,
                "the starting command is gone; runner startup was not accepted"
            ),
            Self::ConnectionEnded => write!(
                formatter,
                "the connection ended before startup was accepted"
            ),
        }
    }
}

#[cfg(test)]
fn startup_io_error(error: StartupAcceptError) -> io::Error {
    let kind = match error {
        StartupAcceptError::ParentGone => ErrorKind::BrokenPipe,
        StartupAcceptError::ConnectionEnded => ErrorKind::ConnectionAborted,
    };
    io::Error::new(kind, error.to_string())
}

struct PhaseState {
    epoch: u64,
    /// `connected.lock`, held exactly while the runner is connected.
    connected: Option<File>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Identity {
    edge_id: String,
    server_url: String,
    workspace: PathBuf,
}

pub fn is_managed_child() -> bool {
    std::env::var(MANAGED_ENV).ok().as_deref() == Some("1")
}

pub fn should_detach(args: &[String], stdin_is_terminal: bool) -> bool {
    let workspace_env = std::env::var("ASTRA_WORKSPACE_DIR").ok();
    should_detach_with(
        args,
        stdin_is_terminal,
        std::env::var(FOREGROUND_ENV).ok().as_deref(),
        is_managed_child(),
        workspace_env.as_deref(),
    )
}

fn should_detach_with(
    args: &[String],
    stdin_is_terminal: bool,
    foreground: Option<&str>,
    managed_child: bool,
    workspace_env: Option<&str>,
) -> bool {
    stdin_is_terminal
        && !managed_child
        && !foreground_requested(foreground)
        && matches!(user_runner_identity_with(args, workspace_env), Ok(Some(_)))
}

pub async fn until_cancelled<T>(
    shutdown: &CancellationToken,
    work: impl Future<Output = T>,
) -> Result<T, ()> {
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => Err(()),
        value = work => Ok(value),
    }
}

fn foreground_requested(value: Option<&str>) -> bool {
    matches!(
        value.map(str::trim).map(str::to_ascii_lowercase).as_deref(),
        Some("1" | "true" | "yes")
    )
}

pub fn shutdown_token() -> CancellationToken {
    SHUTDOWN.get_or_init(CancellationToken::new).clone()
}

pub fn shutdown_requested() -> bool {
    is_managed_child() && shutdown_token().is_cancelled()
}

pub fn connection_shutdown() -> CancellationToken {
    if is_managed_child() {
        shutdown_token()
    } else {
        CancellationToken::new()
    }
}

pub fn install_shutdown_listener() {
    let token = shutdown_token();
    tokio::spawn(async move {
        let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        else {
            return;
        };
        terminate.recv().await;
        note_phase("stopping");
        token.cancel();
    });
}

pub fn note_phase(phase: &str) {
    let Ok(dir) = std::env::var(STATE_DIR_ENV) else {
        return;
    };
    if dir.is_empty() {
        return;
    }
    note_phase_with(&PHASE_STATE, Path::new(&dir), phase);
}

pub fn current_phase_epoch() -> u64 {
    phase_epoch(&PHASE_STATE)
}

/// Publish readiness only when no newer phase was recorded after `epoch`.
/// A connection that has already moved to reconnecting or stopping keeps
/// that later phase, and the parent is not told that this attempt is ready.
pub fn signal_ready_for_epoch(epoch: u64) {
    let dir = std::env::var(STATE_DIR_ENV)
        .ok()
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from);
    let ready_fd = ready_fd_from_env();
    if let Err(error) = signal_ready_for_epoch_with(&PHASE_STATE, epoch, dir.as_deref(), ready_fd) {
        eprintln!("Error: {error}.");
        std::process::exit(1);
    }
}

fn ready_fd_from_env() -> Option<i32> {
    std::env::var(READY_FD_ENV)
        .ok()
        .and_then(|raw| raw.parse::<i32>().ok())
        .filter(|fd| *fd >= 0)
}

/// The ready byte was published and this process has since left that attempt
/// without the starting command accepting it.
pub fn readiness_abandoned() -> bool {
    READY_SENT.load(Ordering::SeqCst)
        && !PARENT_RELEASED.load(Ordering::SeqCst)
        && phase_epoch(&PHASE_STATE) != SENT_EPOCH.load(Ordering::SeqCst)
}

/// Completes when the starting command's socket is closed before acceptance.
/// After acceptance the future stays pending: the parent is expected to exit.
pub async fn until_starting_command_lost() {
    until_parent_fd_lost(ready_fd_from_env()).await;
}

async fn until_parent_fd_lost(fd: Option<i32>) {
    let Some(fd) = fd else {
        std::future::pending::<()>().await;
        return;
    };
    loop {
        if PARENT_RELEASED.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        match parent_socket_is_closed(fd) {
            Ok(true) if !PARENT_RELEASED.load(Ordering::SeqCst) => return,
            Ok(_) => {}
            Err(_) => {
                if !PARENT_RELEASED.load(Ordering::SeqCst) {
                    return;
                }
                std::future::pending::<()>().await;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn parent_socket_is_closed(fd: i32) -> io::Result<bool> {
    let mut buf = [0u8; 1];
    // Peek so the acceptance read still observes the parent's byte.
    let read = unsafe {
        nix::libc::recv(
            fd,
            buf.as_mut_ptr().cast::<nix::libc::c_void>(),
            buf.len(),
            nix::libc::MSG_PEEK | nix::libc::MSG_DONTWAIT,
        )
    };
    if read == 0 {
        return Ok(true);
    }
    if read > 0 {
        return Ok(false);
    }
    let error = io::Error::last_os_error();
    if error.kind() == ErrorKind::WouldBlock || error.kind() == ErrorKind::Interrupted {
        Ok(false)
    } else {
        Err(error)
    }
}

fn phase_epoch(state: &Mutex<PhaseState>) -> u64 {
    lock_phase(state).epoch
}

/// `connected` is published only by holding `connected.lock`; the phase file
/// describes a runner that is not connected. Leaving `connected` releases
/// the lock, which needs no filesystem write, so a failed phase write cannot
/// leave the runner reported as connected.
fn note_phase_with(state: &Mutex<PhaseState>, dir: &Path, phase: &str) {
    let mut gate = lock_phase(state);
    if let Err(error) = write_private(&dir.join("phase"), phase.as_bytes()) {
        tracing::warn!(target: "astra.edge", phase, error = %error, "cannot record the runner phase");
    }
    if phase != "connected" {
        gate.epoch = gate.epoch.wrapping_add(1);
        if let Some(connected) = gate.connected.as_ref() {
            let _ = connected.unlock();
        }
    }
}

fn signal_ready_for_epoch_with(
    state: &Mutex<PhaseState>,
    epoch: u64,
    dir: Option<&Path>,
    ready_fd: Option<i32>,
) -> Result<(), StartupAcceptError> {
    // Phase publication and the ready byte stay in this critical section.
    // The parent's acknowledgement is waited for only after the guard drops,
    // so a later disconnect can still be recorded while the parent answers.
    // Acceptance is committed under the lock again, after that answer.
    let pending_ack = {
        let gate = lock_phase(state);
        if gate.epoch != epoch {
            return Ok(());
        }
        if let Some(connected) = gate.connected.as_ref()
            && let Err(error) = connected.lock_exclusive()
        {
            tracing::warn!(target: "astra.edge", error = %error, "cannot publish the connected state");
            return Ok(());
        }
        publish_connected_ready(dir, ready_fd, epoch)?
    };
    let Some(mut channel) = pending_ack else {
        return Ok(());
    };
    finish_parent_acceptance(state, epoch, &mut channel)
}

fn finish_parent_acceptance(
    state: &Mutex<PhaseState>,
    epoch: u64,
    channel: &mut File,
) -> Result<(), StartupAcceptError> {
    read_parent_ack(channel)?;
    // Commit only if this attempt is still the connected one. The confirm
    // byte is written before the lock is released, so a disconnect either
    // happens first and refuses acceptance, or happens after the parent has
    // a confirm that was true at that moment.
    let held = lock_phase(state);
    if held.epoch != epoch {
        return Err(StartupAcceptError::ConnectionEnded);
    }
    PARENT_RELEASED.store(true, Ordering::SeqCst);
    if let Err(error) = write_confirm(channel) {
        PARENT_RELEASED.store(false, Ordering::SeqCst);
        tracing::warn!(
            target: "astra.edge",
            error = %error,
            "the starting command did not accept readiness"
        );
        return Err(StartupAcceptError::ParentGone);
    }
    drop(held);
    Ok(())
}

fn lock_phase(state: &Mutex<PhaseState>) -> std::sync::MutexGuard<'_, PhaseState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn publish_connected_ready(
    dir: Option<&Path>,
    ready_fd: Option<i32>,
    epoch: u64,
) -> Result<Option<File>, StartupAcceptError> {
    if let Some(dir) = dir {
        let _ = write_private(&dir.join("phase"), b"connected");
    }
    static NOTIFIED: Once = Once::new();
    let mut pending_ack = Ok(None);
    NOTIFIED.call_once(|| {
        if let Some(fd) = ready_fd {
            SENT_EPOCH.store(epoch, Ordering::SeqCst);
            READY_SENT.store(true, Ordering::SeqCst);
            match write_ready_byte(fd) {
                Ok(channel) => pending_ack = Ok(Some(channel)),
                Err(error) => {
                    READY_SENT.store(false, Ordering::SeqCst);
                    tracing::warn!(
                        target: "astra.edge",
                        error = %error,
                        "the starting command did not accept readiness"
                    );
                    pending_ack = Err(StartupAcceptError::ParentGone);
                }
            }
        }
    });
    pending_ack
}

#[cfg(test)]
fn notify_parent_ready_fd(fd: i32) -> io::Result<()> {
    let mut channel = write_ready_byte(fd)?;
    read_parent_ack(&mut channel).map_err(startup_io_error)?;
    write_confirm(&mut channel)
}

fn write_ready_byte(fd: i32) -> io::Result<File> {
    // The parent owns the other end of this socket. Writing the ready byte
    // only fills the buffer; acceptance is the byte the parent writes back,
    // followed by this process confirming it is still that connection.
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(b"1")?;
    file.flush()?;
    Ok(file)
}

fn write_confirm(channel: &mut File) -> io::Result<()> {
    let mut written = false;
    for _ in 0..50 {
        match channel.write_all(b"1") {
            Ok(()) => {
                written = true;
                break;
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                let _ = poll_fd(as_fd_raw(channel), nix::libc::POLLOUT, 20);
            }
            Err(error) => return Err(error),
        }
    }
    if !written {
        return Err(io::Error::new(
            ErrorKind::WouldBlock,
            "the starting command did not accept readiness",
        ));
    }
    channel.flush()
}

fn read_parent_ack(channel: &mut File) -> Result<(), StartupAcceptError> {
    set_nonblocking(channel);
    loop {
        if shutdown_requested() || readiness_abandoned() {
            return Err(StartupAcceptError::ConnectionEnded);
        }
        let mut ack = [0u8; 1];
        match channel.read(&mut ack) {
            Ok(1) if ack == *b"1" => return Ok(()),
            Ok(1) => {
                return Err(StartupAcceptError::ParentGone);
            }
            Ok(0) | Ok(_) => return Err(StartupAcceptError::ParentGone),
            Err(error)
                if error.kind() == ErrorKind::WouldBlock
                    || error.kind() == ErrorKind::Interrupted =>
            {
                let _ = poll_read(as_fd_raw(channel), 200);
            }
            Err(error) if error.kind() == ErrorKind::UnexpectedEof => {
                return Err(StartupAcceptError::ParentGone);
            }
            Err(_) => return Err(StartupAcceptError::ParentGone),
        }
    }
}

fn set_nonblocking(file: &File) {
    let _ = nix::fcntl::fcntl(
        file,
        nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK),
    );
}

fn poll_read(fd: i32, timeout_ms: i32) -> io::Result<bool> {
    poll_fd(fd, nix::libc::POLLIN, timeout_ms)
}

fn poll_fd(fd: i32, events: i16, timeout_ms: i32) -> io::Result<bool> {
    let mut fds = [nix::libc::pollfd {
        fd,
        events,
        revents: 0,
    }];
    let ready = unsafe { nix::libc::poll(fds.as_mut_ptr(), 1, timeout_ms) };
    if ready < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == ErrorKind::Interrupted {
            return Ok(false);
        }
        return Err(error);
    }
    Ok(ready > 0)
}

fn readiness_channel() -> io::Result<(File, File)> {
    use std::os::fd::OwnedFd;
    // A connected pair, not a pipe: the child writes readiness and then reads
    // the parent's acceptance. Both ends are close-on-exec until the child
    // end is passed across `exec`.
    let (parent, child) = std::os::unix::net::UnixStream::pair()?;
    Ok((
        File::from(OwnedFd::from(parent)),
        File::from(OwnedFd::from(child)),
    ))
}

pub fn prepare_child() -> io::Result<()> {
    let dir = PathBuf::from(std::env::var(STATE_DIR_ENV).map_err(|_| {
        io::Error::new(
            ErrorKind::InvalidInput,
            "managed edge is missing its state directory",
        )
    })?);
    detach_session()?;
    let lock = acquire_instance_lock(&dir)?;
    lock_phase(&PHASE_STATE).connected = Some(open_lock_file(&dir.join("connected.lock"))?);
    note_phase("starting");
    *INSTANCE_LOCK
        .lock()
        .map_err(|_| io::Error::other("runner instance lock poisoned"))? = Some(lock);
    let log = dir.join("edge.log");
    rotate_log(&log)?;
    redirect_stdio(&log)
}

pub fn management_exit_code(args: &[String], root: &Path) -> Option<i32> {
    let request = parse_management(args)?;
    Some(run_management(root, request))
}

pub fn supervise(executable: &Path, args: &[String], root: &Path) -> i32 {
    // The canonical parser is the only command check. An unknown option must
    // not queue a credential or start a runner.
    if let Err(code) = crate::validate_managed_command(args) {
        return code;
    }
    let identity = match user_runner_identity(args) {
        Ok(Some(identity)) => identity,
        Ok(None) => {
            eprintln!("Error: the runner command is incomplete");
            return 1;
        }
        Err(error) => {
            eprintln!("Error: cannot use the workspace directory: {error}");
            return 1;
        }
    };
    let dir = state_dir(root, &identity);
    // The registry lock covers directory creation and an uncontended
    // `start.lock` only. A start already in progress holds that per-runner
    // lock; waiting for it happens after the registry lock is released, so
    // other runners can still be listed and stopped.
    let start = match lock_published_start(root, &dir) {
        Ok(file) => file,
        Err(error) => {
            eprintln!("Error: cannot lock runner startup: {error}");
            return 1;
        }
    };
    // Released once the child owns `instance.lock`, so `stop` can signal that
    // process without waiting for authentication. Until then this guard still
    // covers directory publication.
    let mut start_guard = Some(Unlock(&start));
    match lock_names_file(&dir, &start) {
        Ok(true) => {}
        Ok(false) => {
            eprintln!("Error: runner startup was cancelled before the runner started.");
            return 1;
        }
        Err(error) => {
            eprintln!("Error: cannot confirm runner startup: {error}");
            return 1;
        }
    }
    let token = option_value(args, "--token").unwrap_or("");
    match instance_is_locked(&dir) {
        Ok(true) => return running_instance(&dir, &identity, token, executable),
        Ok(false) => {}
        Err(error) => {
            eprintln!("Error: cannot inspect the existing runner: {error}");
            return 1;
        }
    }
    // A credential handed to a runner that has exited must not override the
    // credential this command starts with.
    if let Err(error) = discard_handoff(&dir) {
        eprintln!("Error: cannot clear the previous runner's credential: {error}");
        return 1;
    }
    if let Err(error) = clear_stop_request(&dir) {
        eprintln!("Error: cannot clear the previous stop: {error}");
        return 1;
    }
    if let Err(error) = write_metadata(&dir, &identity) {
        eprintln!("Error: cannot record runner state: {error}");
        return 1;
    }
    match spawn_and_wait(executable, args, &dir, &identity.workspace, &mut || {
        start_guard.take();
    }) {
        Ok(()) => {
            print_connected(executable, &identity.edge_id, &identity.server_url);
            0
        }
        Err(error) => {
            eprintln!("Error: {error}");
            eprintln!("Logs: {}", dir.join("edge.log").display());
            1
        }
    }
}

/// Called with `start.lock` held, so a handoff is ordered against the
/// runner's final authentication decision (`lock_credential_handoff`).
/// Every connect command hands its credential over; the runner's
/// `TokenManager` alone decides whether it differs from the one in use.
fn running_instance(dir: &Path, identity: &Identity, token: &str, executable: &Path) -> i32 {
    let record = match read_metadata(dir) {
        Ok(record) => record,
        Err(error) => {
            eprintln!("Error: cannot read runner state: {error}");
            return 1;
        }
    };
    if record.workspace != identity.workspace.display().to_string() {
        eprintln!(
            "Error: runner {} is already connected to another workspace ({})",
            record.edge_id, record.workspace
        );
        eprintln!(
            "Stop it with: {}",
            management_command(executable, "stop", &record.edge_id, &record.server_url)
        );
        return 1;
    }
    let phase = match phase_label(dir) {
        Ok(phase) => phase,
        Err(error) => {
            eprintln!("Error: cannot inspect the runner lock: {error}");
            return 1;
        }
    };
    if phase == "stopping" {
        eprintln!("Error: the runner is stopping. Run the command again after it stops.");
        return 1;
    }
    if let Err(error) = queue_pending_credential(dir, token) {
        eprintln!("Error: cannot hand the credential to the running runner: {error}");
        return 1;
    }
    let code = if phase == "connected" {
        eprintln!("Runner is already connected in the background.");
        0
    } else {
        eprintln!("Runner is already running in phase {phase}.");
        1
    };
    eprintln!(
        "It uses this command's credential from its next connection attempt; running work is not interrupted."
    );
    print_actions(executable, &record.edge_id, &record.server_url);
    code
}

fn pending_credential_path(dir: &Path) -> PathBuf {
    dir.join("credential.next")
}

fn claimed_credential_path(dir: &Path) -> PathBuf {
    dir.join("credential.claimed")
}

pub fn queue_pending_credential(dir: &Path, token: &str) -> io::Result<()> {
    write_private(&pending_credential_path(dir), token.as_bytes())
}

fn discard_handoff(dir: &Path) -> io::Result<()> {
    for path in [pending_credential_path(dir), claimed_credential_path(dir)] {
        remove_if_present(&path)?;
    }
    Ok(())
}

fn stop_requested_path(dir: &Path) -> PathBuf {
    dir.join("stop.requested")
}

fn stop_is_requested(dir: &Path) -> bool {
    stop_requested_path(dir).is_file()
}

fn clear_stop_request(dir: &Path) -> io::Result<()> {
    remove_if_present(&stop_requested_path(dir))
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// State directory through which connect commands hand credentials to this
/// managed runner.
pub fn credential_handoff_dir() -> Option<PathBuf> {
    if !is_managed_child() {
        return None;
    }
    std::env::var(STATE_DIR_ENV)
        .ok()
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
}

/// Read the credential handed to this runner. A newer handoff replaces an
/// unfinished claim by rename, so acknowledging one value never removes its
/// successor. The claimed file stays until `finish_claim`, so a failed read
/// or an interrupted consumer retries it on the next call.
pub fn claim_pending_credential(dir: &Path) -> io::Result<Option<String>> {
    match std::fs::rename(pending_credential_path(dir), claimed_credential_path(dir)) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    match std::fs::read_to_string(claimed_credential_path(dir)) {
        Ok(token) if token.trim().is_empty() => {
            finish_claim(dir);
            Ok(None)
        }
        Ok(token) => Ok(Some(token.trim().to_string())),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Call after the claimed credential is staged in the `TokenManager`.
pub fn finish_claim(dir: &Path) {
    let _ = std::fs::remove_file(claimed_credential_path(dir));
}

pub struct HandoffLock {
    _lock: File,
}

impl HandoffLock {
    /// Hold the lock until the process exits, so a connect command waiting on
    /// it finds this runner gone and starts a new one.
    pub fn keep_until_exit(self) {
        std::mem::forget(self);
    }
}

/// Serialize this runner's final authentication decision with connect
/// commands handing it a credential. Before readiness the starting command
/// itself holds the lock and no handoff can be in progress.
pub fn lock_credential_handoff(dir: Option<&Path>) -> Option<HandoffLock> {
    lock_credential_handoff_with(dir, PARENT_RELEASED.load(Ordering::SeqCst))
}

fn lock_credential_handoff_with(dir: Option<&Path>, parent_released: bool) -> Option<HandoffLock> {
    if !parent_released {
        return None;
    }
    match lock_start(dir?) {
        Ok(file) => Some(HandoffLock { _lock: file }),
        Err(error) => {
            tracing::warn!(
                target: "astra.edge",
                error = %error,
                "cannot order the final authentication decision with a credential handoff"
            );
            None
        }
    }
}

/// Serializes starting, stopping and handing a credential to one runner.
fn lock_start(dir: &Path) -> io::Result<File> {
    let file = open_lock_file(&dir.join("start.lock"))?;
    file.lock_exclusive()?;
    Ok(file)
}

fn try_lock_start(dir: &Path) -> io::Result<Option<File>> {
    let file = open_lock_file(&dir.join("start.lock"))?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(file)),
        Err(error) if error.kind() == ErrorKind::WouldBlock => Ok(None),
        Err(error) => Err(error),
    }
}

/// Publish `dir` only while `start.lock` is held, without waiting for another
/// start while the registry lock is held.
fn lock_published_start(root: &Path, dir: &Path) -> io::Result<File> {
    let registry = lock_registry(root)?;
    ensure_private_dir(dir)?;
    if let Some(file) = try_lock_start(dir)? {
        drop(registry);
        return Ok(file);
    }
    drop(registry);
    lock_start(dir)
}

/// Orders publication of a runner directory with `stop`. A start holds this
/// lock from before the directory exists until `start.lock` is held.
fn lock_registry(root: &Path) -> io::Result<File> {
    let dir = root.join("edge-runners");
    ensure_private_dir(&dir)?;
    let file = open_lock_file(&dir.join("registry.lock"))?;
    file.lock_exclusive()?;
    Ok(file)
}

fn open_lock_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    set_mode(path, 0o600);
    Ok(file)
}

fn spawn_and_wait(
    executable: &Path,
    args: &[String],
    dir: &Path,
    workspace: &Path,
    release_start: &mut dyn FnMut(),
) -> Result<(), String> {
    let startup = StartupCancel::arm().map_err(|error| error.to_string())?;
    let args = command_with_workspace(args, workspace);
    let (parent_fd, child_fd) = readiness_channel().map_err(|error| error.to_string())?;
    let child_raw = as_fd_raw(&child_fd);
    let mut command = Command::new(executable);
    command
        .args(&args)
        .env(MANAGED_ENV, "1")
        .env(STATE_DIR_ENV, dir)
        .env(READY_FD_ENV, child_raw.to_string())
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    unsafe {
        command.pre_exec(move || clear_cloexec(child_raw));
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot start the background runner: {error}"))?;
    drop(child_fd);
    let mut channel = parent_fd;
    if let Err(error) = wait_for_ready_in(
        &mut child,
        &mut channel,
        READY_TIMEOUT,
        startup.flag(),
        Some(dir),
        release_start,
    ) {
        kill_child(&mut child);
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
fn wait_for_ready(
    child: &mut Child,
    channel: &mut File,
    timeout: Duration,
    cancelled: &AtomicBool,
) -> Result<(), String> {
    wait_for_ready_in(child, channel, timeout, cancelled, None, &mut || {})
}

fn wait_for_ready_in(
    child: &mut Child,
    channel: &mut File,
    timeout: Duration,
    cancelled: &AtomicBool,
    dir: Option<&Path>,
    release_start: &mut dyn FnMut(),
) -> Result<(), String> {
    set_nonblocking(channel);
    let started = Instant::now();
    let deadline = started + timeout;
    loop {
        release_start_if_running(dir, release_start);
        match read_ready_byte(channel)? {
            ReadyByte::Ready => {
                return accept_parent_ready(child, channel, cancelled, dir, deadline);
            }
            ReadyByte::Pending => {}
        }
        if cancelled.load(Ordering::Relaxed) {
            return Err("startup cancelled before the runner was ready".into());
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("cannot observe the runner: {error}"))?
        {
            // A byte left in the pipe by a child that has already exited is
            // not a running runner.
            if read_ready_byte(channel)? == ReadyByte::Ready {
                return accept_parent_ready(child, channel, cancelled, dir, deadline);
            }
            return Err(format!("runner exited before it was ready ({status})"));
        }
        if Instant::now() >= deadline {
            return Err("timed out waiting for the runner to authenticate".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn release_start_if_running(dir: Option<&Path>, release_start: &mut dyn FnMut()) {
    if dir.is_some_and(|dir| matches!(instance_is_locked(dir), Ok(true))) {
        release_start();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReadyByte {
    Ready,
    Pending,
}

fn accept_parent_ready(
    child: &mut Child,
    channel: &mut File,
    cancelled: &AtomicBool,
    dir: Option<&Path>,
    deadline: Instant,
) -> Result<(), String> {
    accept_ready(child, cancelled)?;
    if let Some(dir) = dir {
        match phase_label(dir) {
            Ok(phase) if phase == "connected" => {}
            Ok(phase) => {
                return Err(format!(
                    "runner is {phase}; the starting command did not accept readiness"
                ));
            }
            Err(error) => return Err(format!("cannot inspect the runner: {error}")),
        }
    }
    channel
        .write_all(b"1")
        .and_then(|()| channel.flush())
        .map_err(|error| format!("cannot accept runner readiness: {error}"))?;
    read_confirm(child, channel, cancelled, deadline)
}

fn read_confirm(
    child: &mut Child,
    channel: &mut File,
    cancelled: &AtomicBool,
    deadline: Instant,
) -> Result<(), String> {
    loop {
        // Cancellation and child exit stay authoritative after the ack.
        // A confirm byte that arrives later must not become success.
        accept_ready(child, cancelled)?;
        match read_ready_byte(channel)? {
            ReadyByte::Ready => {
                accept_ready(child, cancelled)?;
                return Ok(());
            }
            ReadyByte::Pending => {
                if Instant::now() >= deadline {
                    return Err("timed out waiting for the runner to authenticate".into());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

fn accept_ready(child: &mut Child, cancelled: &AtomicBool) -> Result<(), String> {
    if cancelled.load(Ordering::Relaxed) {
        return Err("startup cancelled before the runner was ready".into());
    }
    match child
        .try_wait()
        .map_err(|error| format!("cannot observe the runner: {error}"))?
    {
        Some(status) => Err(format!("runner exited before it was ready ({status})")),
        None => Ok(()),
    }
}

fn read_ready_byte(read: &mut File) -> Result<ReadyByte, String> {
    let mut buf = [0u8; 1];
    match read.read(&mut buf) {
        Ok(1) if buf[0] == b'1' => Ok(ReadyByte::Ready),
        Ok(0) => Err("runner closed its readiness signal before authentication".into()),
        Ok(_) => Err("runner sent an unexpected readiness signal".into()),
        Err(error)
            if error.kind() == ErrorKind::WouldBlock || error.kind() == ErrorKind::Interrupted =>
        {
            Ok(ReadyByte::Pending)
        }
        Err(error) => Err(error.to_string()),
    }
}

extern "C" fn handle_startup_cancel(_signal: std::os::raw::c_int) {
    STARTUP_CANCELLED.store(true, Ordering::Relaxed);
}

struct StartupCancel {
    previous_int: nix::sys::signal::SigAction,
    previous_term: nix::sys::signal::SigAction,
    previous_hup: nix::sys::signal::SigAction,
}

impl StartupCancel {
    fn arm() -> io::Result<Self> {
        use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
        STARTUP_CANCELLED.store(false, Ordering::SeqCst);
        let action = SigAction::new(
            SigHandler::Handler(handle_startup_cancel),
            SaFlags::empty(),
            SigSet::empty(),
        );
        let previous_int = unsafe { sigaction(Signal::SIGINT, &action).map_err(io_error)? };
        let previous_term = match unsafe { sigaction(Signal::SIGTERM, &action) } {
            Ok(previous) => previous,
            Err(error) => {
                let _ = unsafe { sigaction(Signal::SIGINT, &previous_int) };
                return Err(io_error(error));
            }
        };
        // Closing the terminal delivers SIGHUP to the waiting supervisor.
        // The child has left the session, so the supervisor has to cancel it.
        let previous_hup = match unsafe { sigaction(Signal::SIGHUP, &action) } {
            Ok(previous) => previous,
            Err(error) => {
                let _ = unsafe { sigaction(Signal::SIGINT, &previous_int) };
                let _ = unsafe { sigaction(Signal::SIGTERM, &previous_term) };
                return Err(io_error(error));
            }
        };
        Ok(Self {
            previous_int,
            previous_term,
            previous_hup,
        })
    }

    fn flag(&self) -> &'static AtomicBool {
        &STARTUP_CANCELLED
    }
}

impl Drop for StartupCancel {
    fn drop(&mut self) {
        use nix::sys::signal::{Signal, sigaction};
        unsafe {
            let _ = sigaction(Signal::SIGINT, &self.previous_int);
            let _ = sigaction(Signal::SIGTERM, &self.previous_term);
            let _ = sigaction(Signal::SIGHUP, &self.previous_hup);
        }
        STARTUP_CANCELLED.store(false, Ordering::SeqCst);
    }
}

fn kill_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[derive(Clone, Copy)]
enum Action {
    Status,
    Stop,
    Logs,
}

struct Management {
    action: Action,
    edge_id: Option<String>,
    server_url: Option<String>,
}

const MANAGEMENT_USAGE: &str =
    "Usage: astra-edge status|stop|logs [--edge-id ID] [--server-url URL]";

fn usage_request(action: Action) -> Management {
    Management {
        action,
        edge_id: Some(String::new()),
        server_url: None,
    }
}

fn parse_management(args: &[String]) -> Option<Management> {
    let action = match args.first().map(String::as_str) {
        Some("status") => Action::Status,
        Some("stop") => Action::Stop,
        Some("logs") => Action::Logs,
        _ => return None,
    };
    let mut edge_id = None;
    let mut server_url = None;
    let mut rest = args[1..].iter();
    while let Some(arg) = rest.next() {
        let (name, value) = if let Some(value) = arg.strip_prefix("--edge-id=") {
            ("--edge-id", Some(value.to_string()))
        } else if arg == "--edge-id" {
            ("--edge-id", rest.next().cloned())
        } else if let Some(value) = arg.strip_prefix("--server-url=") {
            ("--server-url", Some(value.to_string()))
        } else if arg == "--server-url" {
            ("--server-url", rest.next().cloned())
        } else {
            eprintln!("Error: unsupported argument {arg}");
            eprintln!("{MANAGEMENT_USAGE}");
            return Some(usage_request(action));
        };
        let Some(value) = value.filter(|value| !value.is_empty()) else {
            eprintln!("Error: {name} requires a value");
            eprintln!("{MANAGEMENT_USAGE}");
            return Some(usage_request(action));
        };
        match name {
            "--edge-id" => edge_id = Some(value),
            "--server-url" => server_url = Some(value),
            _ => return Some(usage_request(action)),
        }
    }
    Some(Management {
        action,
        edge_id,
        server_url,
    })
}

fn run_management(root: &Path, request: Management) -> i32 {
    if request.edge_id.as_deref() == Some("") || request.server_url.as_deref() == Some("") {
        return 2;
    }
    let registry = match lock_registry(root) {
        Ok(file) => file,
        Err(error) => {
            eprintln!("Error: cannot lock runner startup: {error}");
            return 1;
        }
    };
    let instances = match list_instances(root) {
        Ok(instances) => instances,
        Err(error) => {
            eprintln!("Error: cannot read runner state: {error}");
            return 1;
        }
    };
    if matches!(request.action, Action::Stop)
        && let Some(pending) = stop_pending_starts(&request, &instances)
    {
        drop(registry);
        return match pending {
            PendingStart::Dir(dir) => stop_unpublished(&dir),
            PendingStart::Error(code) => code,
        };
    }
    let uninspectable = report_uninspectable(
        &instances.unreadable,
        request.edge_id.as_deref(),
        request.server_url.as_deref(),
    );
    // An unreadable live runner may be the one this request means, so it is
    // never stopped or reported absent by elimination.
    if uninspectable && !matches!(request.action, Action::Status) {
        return 1;
    }
    let selected = match select_instances(
        &instances.records,
        request.edge_id.as_deref(),
        request.server_url.as_deref(),
        request.action,
        uninspectable,
    ) {
        Ok(selected) => selected,
        Err(code) => return code,
    };
    drop(registry);
    match request.action {
        Action::Status => {
            let mut code = i32::from(uninspectable);
            for record in selected {
                if print_status(record) != 0 {
                    code = 1;
                }
            }
            code
        }
        Action::Stop => stop_instance(selected[0]),
        Action::Logs => print_logs(selected[0]),
    }
}

struct Record {
    dir: PathBuf,
    edge_id: String,
    server_url: String,
    workspace: String,
}

fn select_instances<'a>(
    records: &'a [Record],
    edge_id: Option<&str>,
    server_url: Option<&str>,
    action: Action,
    uninspectable: bool,
) -> Result<Vec<&'a Record>, i32> {
    let matched: Vec<&Record> = records
        .iter()
        .filter(|record| edge_id.is_none_or(|id| record.edge_id == id))
        .filter(|record| server_url.is_none_or(|url| same_destination(&record.server_url, url)))
        .collect();
    if matched.is_empty() {
        if uninspectable {
            return Err(1);
        }
        match action {
            Action::Status | Action::Stop => {
                eprintln!("No managed runner.");
                return Err(0);
            }
            Action::Logs => {
                eprintln!("Error: no managed runner log");
                return Err(1);
            }
        }
    }
    let allow_many = matches!(action, Action::Status);
    if matched.len() > 1 && !allow_many {
        let hint = match (edge_id, server_url) {
            (None, _) => "pass --edge-id and --server-url",
            (_, None) => "pass --server-url",
            _ => "narrow the runner with the printed server URL",
        };
        eprintln!("Error: more than one managed runner; {hint}");
        for record in matched {
            eprintln!("  {} {}", record.edge_id, record.server_url);
        }
        return Err(2);
    }
    Ok(matched)
}

enum PendingStart {
    Dir(PathBuf),
    Error(i32),
}

/// A start whose directory is visible and whose `start.lock` is held, but
/// which has not published metadata. The caller drops the registry lock
/// before waiting, so the long stop does not block other runners.
fn stop_pending_starts(request: &Management, instances: &Instances) -> Option<PendingStart> {
    let mut pending = Vec::new();
    for (dir, _) in &instances.unreadable {
        if !dir_selected(
            dir,
            request.edge_id.as_deref(),
            request.server_url.as_deref(),
        ) {
            continue;
        }
        // Only a held `start.lock` is an in-progress start. An unlocked
        // directory with unreadable metadata is a stopped runner's state;
        // deleting it would throw away its logs.
        match (instance_is_locked(dir), start_lock_is_held(dir)) {
            (Ok(false), Ok(true)) => pending.push(dir.clone()),
            (Ok(_), Ok(_)) => {}
            (Err(error), _) | (_, Err(error)) => {
                eprintln!("Error: cannot inspect the runner lock: {error}");
                return Some(PendingStart::Error(1));
            }
        }
    }
    if pending.is_empty() {
        return None;
    }
    let readable = instances
        .records
        .iter()
        .filter(|record| {
            request
                .edge_id
                .as_ref()
                .is_none_or(|id| record.edge_id == *id)
        })
        .filter(|record| {
            request
                .server_url
                .as_deref()
                .is_none_or(|url| same_destination(&record.server_url, url))
        })
        .count();
    if readable > 0 {
        return None;
    }
    if pending.len() > 1 {
        eprintln!("Error: more than one runner startup; pass --edge-id and --server-url");
        return Some(PendingStart::Error(2));
    }
    Some(PendingStart::Dir(pending.remove(0)))
}

fn dir_selected(dir: &Path, edge_id: Option<&str>, server_url: Option<&str>) -> bool {
    let Some((edge_id, server_url)) = edge_id.zip(server_url) else {
        return true;
    };
    let Ok(server_url) = astra_edge::edge_ws_url(server_url) else {
        return false;
    };
    let key = identity_key(&Identity {
        edge_id: edge_id.to_string(),
        server_url,
        workspace: PathBuf::new(),
    });
    dir.file_name() == Some(std::ffi::OsStr::new(&key))
}

/// Called with no readable metadata and no instance owner visible yet.
/// Waits for the start's `start.lock`, then either stops the runner that
/// start published or removes the unpublished directory so the start cannot
/// launch after this stop returns.
fn stop_unpublished(dir: &Path) -> i32 {
    let start = match lock_start(dir) {
        Ok(start) => start,
        Err(error) => {
            eprintln!("Error: cannot lock runner startup: {error}");
            return 1;
        }
    };
    match lock_names_file(dir, &start) {
        Ok(true) => {}
        Ok(false) => {
            eprintln!("Runner is already stopped.");
            return 0;
        }
        Err(error) => {
            eprintln!("Error: cannot confirm runner startup: {error}");
            return 1;
        }
    }
    match instance_owner(dir) {
        Ok(Some(pid)) => stop_owner(dir, pid, start, true),
        Ok(None) if dir.join("metadata.json").is_file() => {
            eprintln!("Runner is already stopped.");
            0
        }
        Ok(None) => {
            let removed = std::fs::remove_dir_all(dir);
            drop(start);
            match removed {
                Ok(()) => {
                    eprintln!("Runner is already stopped.");
                    0
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    eprintln!("Runner is already stopped.");
                    0
                }
                Err(error) => {
                    eprintln!("Error: cannot discard the unpublished runner: {error}");
                    1
                }
            }
        }
        Err(error) => {
            eprintln!("Error: cannot inspect the runner lock: {error}");
            1
        }
    }
}

fn phase_label(dir: &Path) -> io::Result<String> {
    let recorded = match std::fs::read_to_string(dir.join("phase")) {
        Ok(value) => Some(value.trim().to_string()).filter(|value| !value.is_empty()),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(_) => Some("unknown".to_string()),
    };
    if !instance_is_locked(dir)? {
        return Ok(if recorded.as_deref() == Some("authentication-failure") {
            "authentication-failure".to_string()
        } else {
            "stopped".to_string()
        });
    }
    // Written by `stop` under `start.lock`, before the runner's signal handler
    // can release `connected.lock`. A connect command must observe it even
    // while the runner still looks connected.
    if stop_is_requested(dir) {
        return Ok("stopping".to_string());
    }
    if connected_lock_is_held(dir)? {
        return Ok("connected".to_string());
    }
    Ok(match recorded.as_deref() {
        None => "starting",
        // The runner left `connected` but could not record its next phase.
        Some("connected") => "unknown",
        Some(phase) => phase,
    }
    .to_string())
}

fn print_status(record: &Record) -> i32 {
    eprintln!("edge-id: {}", record.edge_id);
    let code = match phase_label(&record.dir) {
        Ok(phase) => {
            eprintln!("phase: {phase}");
            i32::from(phase == "unknown")
        }
        Err(error) => {
            eprintln!("phase: unknown");
            eprintln!("Error: cannot inspect the runner lock: {error}");
            1
        }
    };
    eprintln!("server: {}", record.server_url);
    eprintln!("workspace: {}", record.workspace);
    eprintln!("log: {}", record.dir.join("edge.log").display());
    code
}

fn stop_instance(record: &Record) -> i32 {
    stop_instance_with(record, true)
}

fn stop_instance_with(record: &Record, announce: bool) -> i32 {
    // A start in progress holds `start.lock` until its runner owns the
    // instance lock, so it either finishes first and is stopped here, or
    // begins after this stop.
    let start = match lock_start(&record.dir) {
        Ok(start) => start,
        Err(error) => {
            eprintln!("Error: cannot lock runner startup: {error}");
            return 1;
        }
    };
    let pid = match instance_owner(&record.dir) {
        Ok(None) => {
            if announce {
                eprintln!("Runner is already stopped.");
            }
            return 0;
        }
        Ok(Some(pid)) => pid,
        Err(error) => {
            eprintln!("Error: cannot inspect the runner lock: {error}");
            return 1;
        }
    };
    stop_owner(&record.dir, pid, start, announce)
}

/// `start` is the held `start.lock`. The stop marker is published before the
/// signal and before the lock is released, so a connect command cannot observe
/// the runner as connected after the stop has been decided.
fn stop_owner(dir: &Path, pid: u32, start: File, announce: bool) -> i32 {
    if let Err(error) = write_private(&stop_requested_path(dir), b"") {
        eprintln!("Error: cannot record the stop: {error}");
        return 1;
    }
    match nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGTERM,
    ) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => {}
        Err(error) => {
            eprintln!("Error: cannot stop the runner: {error}");
            return 1;
        }
    }
    // The runner may need the lock to finish its final decision. The stop
    // marker stays until the process is gone, so a connect command that takes
    // the lock next still refuses the handoff.
    drop(start);
    let started = Instant::now();
    while started.elapsed() < STOP_TIMEOUT {
        match instance_is_locked(dir) {
            Ok(false) => {
                let _ = clear_stop_request(dir);
                if announce {
                    eprintln!("Runner stopped.");
                }
                return 0;
            }
            Ok(true) => std::thread::sleep(Duration::from_millis(100)),
            Err(error) => {
                eprintln!("Error: cannot inspect the runner lock: {error}");
                return 1;
            }
        }
    }
    eprintln!("Error: runner is still stopping; admitted work has not finished");
    1
}

fn print_logs(record: &Record) -> i32 {
    let current = record.dir.join("edge.log");
    let previous = record.dir.join("edge.log.1");
    let mut lines = Vec::new();
    match read_log_tail(&previous, LOG_TAIL_LINES, LOG_TAIL_BYTES) {
        Ok(mut older) => lines.append(&mut older),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            eprintln!("Error: cannot read {}: {error}", previous.display());
            return 1;
        }
    }
    match read_log_tail(&current, LOG_TAIL_LINES, LOG_TAIL_BYTES) {
        Ok(mut newer) => lines.append(&mut newer),
        Err(error) if error.kind() == ErrorKind::NotFound && lines.is_empty() => {
            eprintln!("No logs yet.");
            eprintln!("log: {}", current.display());
            return 0;
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            eprintln!("Error: cannot read {}: {error}", current.display());
            return 1;
        }
    }
    if lines.is_empty() {
        eprintln!("No logs yet.");
        eprintln!("log: {}", current.display());
        return 0;
    }
    let start = lines.len().saturating_sub(LOG_TAIL_LINES);
    println!("{}", lines[start..].join("\n"));
    0
}

fn read_log_tail(path: &Path, max_lines: usize, max_bytes: u64) -> io::Result<Vec<String>> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    let start = length.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if start > 0
        && let Some(index) = text.find('\n')
    {
        text.replace_range(..=index, "");
    }
    let lines: Vec<String> = text.lines().map(str::to_string).collect();
    let start_line = lines.len().saturating_sub(max_lines);
    Ok(lines[start_line..].to_vec())
}

struct Instances {
    records: Vec<Record>,
    /// State directories whose metadata could not be read.
    unreadable: Vec<(PathBuf, io::Error)>,
}

fn list_instances(root: &Path) -> io::Result<Instances> {
    let dir = root.join("edge-runners");
    let mut instances = Instances {
        records: Vec::new(),
        unreadable: Vec::new(),
    };
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(instances),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        match read_metadata(&entry.path()) {
            Ok(record) => instances.records.push(record),
            Err(error) => instances.unreadable.push((entry.path(), error)),
        }
    }
    instances
        .records
        .sort_by(|left, right| left.edge_id.cmp(&right.edge_id));
    Ok(instances)
}

/// Report unreadable state directories that may belong to a live runner the
/// request selects. Only a positively unheld instance lock, or a directory
/// that cannot match the requested identity, proves it irrelevant.
fn report_uninspectable(
    unreadable: &[(PathBuf, io::Error)],
    edge_id: Option<&str>,
    server_url: Option<&str>,
) -> bool {
    let requested = edge_id.zip(server_url).and_then(|(edge_id, server_url)| {
        let server_url = astra_edge::edge_ws_url(server_url).ok()?;
        Some(identity_key(&Identity {
            edge_id: edge_id.to_string(),
            server_url,
            workspace: PathBuf::new(),
        }))
    });
    let mut found = false;
    for (dir, error) in unreadable {
        if let Some(requested) = &requested
            && dir.file_name() != Some(std::ffi::OsStr::new(requested))
        {
            continue;
        }
        match instance_is_locked(dir) {
            Ok(false) => match start_lock_is_held(dir) {
                Ok(false) => continue,
                Ok(true) => eprintln!(
                    "Error: a runner start is in progress in {}: its state is not published yet",
                    dir.display()
                ),
                Err(lock_error) => eprintln!(
                    "Error: cannot inspect runner startup in {}: {error}; lock: {lock_error}",
                    dir.display()
                ),
            },
            Ok(true) => eprintln!(
                "Error: cannot read the state of a running runner in {}: {error}",
                dir.display()
            ),
            Err(lock_error) => eprintln!(
                "Error: cannot inspect runner state in {}: {error}; lock: {lock_error}",
                dir.display()
            ),
        }
        found = true;
    }
    if found && requested.is_none() {
        eprintln!("Pass --edge-id and --server-url to select a runner whose state is readable.");
    }
    found
}

fn read_metadata(dir: &Path) -> io::Result<Record> {
    let bytes = std::fs::read(dir.join("metadata.json"))?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(ErrorKind::InvalidData, error))?;
    let edge_id = value
        .get("edge_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let server_url = value
        .get("server_url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let workspace = value
        .get("workspace")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if edge_id.is_empty() || server_url.is_empty() || workspace.is_empty() {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "runner metadata is incomplete",
        ));
    }
    Ok(Record {
        dir: dir.to_path_buf(),
        edge_id,
        server_url,
        workspace,
    })
}

fn user_runner_identity(args: &[String]) -> io::Result<Option<Identity>> {
    let workspace_env = std::env::var("ASTRA_WORKSPACE_DIR").ok();
    user_runner_identity_with(args, workspace_env.as_deref())
}

fn user_runner_identity_with(
    args: &[String],
    workspace_env: Option<&str>,
) -> io::Result<Option<Identity>> {
    let Some(edge_id) = option_value(args, "--edge-id") else {
        return Ok(None);
    };
    let Some(server_url) = option_value(args, "--server-url") else {
        return Ok(None);
    };
    let Some(token) = option_value(args, "--token") else {
        return Ok(None);
    };
    let Some(reconnect) = option_value(args, "--reconnect") else {
        return Ok(None);
    };
    if !valid_edge_id(edge_id)
        || !valid_server_url(server_url)
        || token.is_empty()
        || reconnect != "true"
    {
        return Ok(None);
    }
    let server_url = match astra_edge::edge_ws_url(server_url) {
        Ok(server_url) => server_url,
        Err(_) => return Ok(None),
    };
    let workspace = workspace_from_args(args, workspace_env)?;
    Ok(Some(Identity {
        edge_id: edge_id.to_string(),
        server_url: server_url.to_string(),
        workspace,
    }))
}

fn workspace_from_args(args: &[String], workspace_env: Option<&str>) -> io::Result<PathBuf> {
    let raw = option_value(args, "--workspace-dir")
        .or(workspace_env)
        .unwrap_or(".");
    if raw.is_empty() {
        return Err(io::Error::new(
            ErrorKind::NotFound,
            "workspace directory is empty",
        ));
    }
    std::fs::canonicalize(raw)
}

fn command_with_workspace(args: &[String], workspace: &Path) -> Vec<String> {
    let mut rewritten = Vec::with_capacity(args.len() + 2);
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        if arg == "--workspace-dir" {
            let _ = rest.next();
            continue;
        }
        if arg.starts_with("--workspace-dir=") {
            continue;
        }
        rewritten.push(arg.clone());
    }
    rewritten.push("--workspace-dir".to_string());
    rewritten.push(workspace.display().to_string());
    rewritten
}

fn valid_edge_id(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("runner-") else {
        return false;
    };
    !rest.is_empty()
        && rest
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn valid_server_url(value: &str) -> bool {
    !value.is_empty()
        && !value
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte < 0x20)
}

fn option_value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if let Some(value) = arg.strip_prefix(&format!("{name}=")) {
            return Some(value);
        }
        if arg == name {
            return args.next().map(String::as_str);
        }
    }
    None
}

fn state_dir(root: &Path, identity: &Identity) -> PathBuf {
    root.join("edge-runners").join(identity_key(identity))
}

fn identity_key(identity: &Identity) -> String {
    let mut hasher = Sha256::new();
    hasher.update(identity.edge_id.as_bytes());
    hasher.update([0]);
    hasher.update(identity.server_url.as_bytes());
    hasher
        .finalize()
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn print_connected(executable: &Path, edge_id: &str, server_url: &str) {
    eprintln!("Runner connected in the background.");
    print_actions(executable, edge_id, server_url);
}

fn print_actions(executable: &Path, edge_id: &str, server_url: &str) {
    eprintln!("  edge-id: {edge_id}");
    eprintln!("  server: {server_url}");
    eprintln!(
        "  status: {}",
        management_command(executable, "status", edge_id, server_url)
    );
    eprintln!(
        "  logs: {}",
        management_command(executable, "logs", edge_id, server_url)
    );
    eprintln!(
        "  stop: {}",
        management_command(executable, "stop", edge_id, server_url)
    );
}

fn management_command(executable: &Path, action: &str, edge_id: &str, server_url: &str) -> String {
    format!(
        "{} {action} --edge-id {} --server-url {}",
        shell_quote(&executable.display().to_string()),
        shell_quote(edge_id),
        shell_quote(server_url)
    )
}

fn shell_quote(value: &str) -> String {
    let mut quoted = String::from("'");
    for character in value.chars() {
        if character == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(character);
        }
    }
    quoted.push('\'');
    quoted
}

fn same_destination(left: &str, right: &str) -> bool {
    match (
        astra_edge::edge_ws_url(left),
        astra_edge::edge_ws_url(right),
    ) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

/// The instance lock is a POSIX record lock, so the kernel reports the
/// process that owns it. The runner must not open `instance.lock` again:
/// closing any descriptor for the file releases a record lock.
fn acquire_instance_lock(dir: &Path) -> io::Result<File> {
    let file = open_lock_file(&dir.join("instance.lock"))?;
    let lock = write_record_lock();
    match nix::fcntl::fcntl(&file, nix::fcntl::FcntlArg::F_SETLK(&lock)) {
        Ok(_) => Ok(file),
        Err(nix::errno::Errno::EAGAIN | nix::errno::Errno::EACCES) => Err(io::Error::new(
            ErrorKind::WouldBlock,
            "the runner is already running",
        )),
        Err(error) => Err(io_error(error)),
    }
}

fn write_record_lock() -> nix::libc::flock {
    // SAFETY: `flock` is a plain C struct; all-zero is a valid whole-file range.
    let mut lock: nix::libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = nix::libc::F_WRLCK as _;
    lock.l_whence = nix::libc::SEEK_SET as _;
    lock
}

/// The process that currently owns this runner's instance lock.
fn instance_owner(dir: &Path) -> io::Result<Option<u32>> {
    let Some(file) = open_existing_lock(&dir.join("instance.lock"))? else {
        return Ok(None);
    };
    let mut lock = write_record_lock();
    nix::fcntl::fcntl(&file, nix::fcntl::FcntlArg::F_GETLK(&mut lock)).map_err(io_error)?;
    let mut unlocked = write_record_lock();
    unlocked.l_type = nix::libc::F_UNLCK as _;
    if lock.l_type == unlocked.l_type {
        return Ok(None);
    }
    u32::try_from(lock.l_pid)
        .ok()
        .filter(|pid| *pid > 0)
        .map(Some)
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "runner lock has no owner"))
}

fn instance_is_locked(dir: &Path) -> io::Result<bool> {
    Ok(instance_owner(dir)?.is_some())
}

fn lock_names_file(dir: &Path, file: &File) -> io::Result<bool> {
    let Some(current) = open_existing_lock(&dir.join("start.lock"))? else {
        return Ok(false);
    };
    let held = file.metadata()?;
    let named = current.metadata()?;
    Ok(held.dev() == named.dev() && held.ino() == named.ino())
}

fn start_lock_is_held(dir: &Path) -> io::Result<bool> {
    let Some(file) = open_existing_lock(&dir.join("start.lock"))? else {
        return Ok(false);
    };
    // Same flock as `lock_start`. std's `File::try_lock` is a different API
    // and must not be used to observe this lock.
    match file.try_lock_exclusive() {
        Ok(()) => {
            let _ = file.unlock();
            Ok(false)
        }
        Err(error) if error.kind() == ErrorKind::WouldBlock => Ok(true),
        Err(error) => Err(error),
    }
}

fn connected_lock_is_held(dir: &Path) -> io::Result<bool> {
    let Some(file) = open_existing_lock(&dir.join("connected.lock"))? else {
        return Ok(false);
    };
    match file.try_lock_shared() {
        Ok(()) => {
            let _ = file.unlock();
            Ok(false)
        }
        Err(std::fs::TryLockError::WouldBlock) => Ok(true),
        Err(std::fs::TryLockError::Error(error)) => Err(error),
    }
}

fn open_existing_lock(path: &Path) -> io::Result<Option<File>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "runner lock is not a regular file",
            ));
        }
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map(Some)
}

fn write_metadata(dir: &Path, identity: &Identity) -> io::Result<()> {
    let body = serde_json::json!({
        "edge_id": identity.edge_id,
        "server_url": identity.server_url,
        "workspace": identity.workspace.display().to_string(),
    });
    write_private(&dir.join("metadata.json"), body.to_string().as_bytes())
}

fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        set_mode(&tmp, 0o600);
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    set_mode(path, 0o600);
    Ok(())
}

fn ensure_private_dir(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        set_mode(parent, 0o700);
    }
    std::fs::create_dir_all(path)?;
    set_mode(path, 0o700);
    Ok(())
}

fn rotate_log(path: &Path) -> io::Result<()> {
    rotate_log_file(path, LOG_ROTATE_BYTES).map(|_| ())
}

fn rotate_log_file(path: &Path, limit: u64) -> io::Result<Option<File>> {
    rotate_log_file_opening(path, limit, open_log_append)
}

fn open_log_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn rotate_log_file_opening(
    path: &Path,
    limit: u64,
    mut open_replacement: impl FnMut(&Path) -> io::Result<File>,
) -> io::Result<Option<File>> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        // A previous rotation can rename the live file and then fail to create
        // its replacement. The archived inode is still the current log.
        Err(error) if error.kind() == ErrorKind::NotFound => return restore_current_log(path),
        Err(error) => return Err(error),
    };
    if metadata.len() <= limit {
        return Ok(None);
    }
    let rotated = path.with_extension("log.1");
    let _ = std::fs::remove_file(&rotated);
    std::fs::rename(path, &rotated)?;
    match open_replacement(path) {
        Ok(file) => {
            set_mode(path, 0o600);
            Ok(Some(file))
        }
        Err(error) => {
            // Put the archived inode back so the next attempt can rotate it.
            // Stdout still refers to that inode until redirection succeeds.
            let _ = std::fs::rename(&rotated, path);
            Err(error)
        }
    }
}

fn restore_current_log(path: &Path) -> io::Result<Option<File>> {
    let rotated = path.with_extension("log.1");
    if !rotated.exists() {
        return Ok(None);
    }
    std::fs::rename(&rotated, path)?;
    Ok(None)
}

/// Runs on the runner's tokio runtime; no dedicated thread.
pub fn spawn_log_rotator() {
    let Some(dir) = credential_handoff_dir() else {
        return;
    };
    let log = dir.join("edge.log");
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(LOG_ROTATE_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            match rotate_log_file(&log, LOG_ROTATE_BYTES) {
                Ok(Some(file)) => {
                    if let Err(error) = redirect_log_fd(&file) {
                        tracing::warn!(
                            target: "astra.edge",
                            error = %error,
                            "cannot switch the runner log; rotation will be retried"
                        );
                    }
                }
                Ok(None) => {
                    if let Err(error) = redirect_stdio_if_different(&log) {
                        tracing::warn!(
                            target: "astra.edge",
                            error = %error,
                            "cannot point the runner log at the current file"
                        );
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        target: "astra.edge",
                        error = %error,
                        "cannot rotate the runner log; it will be retried"
                    );
                }
            }
        }
    });
}

fn redirect_stdio_if_different(path: &Path) -> io::Result<()> {
    let _ = redirect_fd_if_different(1, path)?;
    let _ = redirect_fd_if_different(2, path)?;
    Ok(())
}

fn redirect_fd_if_different(fd: i32, path: &Path) -> io::Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let file = OpenOptions::new().append(true).read(true).open(path)?;
    let desired = file.metadata()?;
    let (device, inode) = fd_identity(fd)?;
    if device == desired.dev() && inode == desired.ino() {
        return Ok(false);
    }
    let result = unsafe { nix::libc::dup2(as_fd_raw(&file), fd) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(true)
}

fn fd_identity(fd: i32) -> io::Result<(u64, u64)> {
    let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
    let stat = nix::sys::stat::fstat(borrowed).map_err(io_error)?;
    Ok((stat.st_dev as u64, stat.st_ino as u64))
}

fn redirect_log_fd(file: &File) -> io::Result<()> {
    nix::unistd::dup2_stdout(file).map_err(io_error)?;
    nix::unistd::dup2_stderr(file).map_err(io_error)?;
    Ok(())
}

fn redirect_stdio(log: &Path) -> io::Result<()> {
    let null = File::open("/dev/null")?;
    let log_file = OpenOptions::new().create(true).append(true).open(log)?;
    set_mode(log, 0o600);
    nix::unistd::dup2_stdin(&null).map_err(io_error)?;
    nix::unistd::dup2_stdout(&log_file).map_err(io_error)?;
    nix::unistd::dup2_stderr(&log_file).map_err(io_error)?;
    Ok(())
}

fn detach_session() -> io::Result<()> {
    nix::unistd::setsid().map_err(io_error)?;
    unsafe {
        nix::sys::signal::signal(
            nix::sys::signal::Signal::SIGHUP,
            nix::sys::signal::SigHandler::SigIgn,
        )
        .map_err(io_error)?;
    }
    Ok(())
}

fn clear_cloexec(fd: i32) -> io::Result<()> {
    nix::fcntl::fcntl(
        unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) },
        nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::empty()),
    )
    .map_err(io_error)?;
    Ok(())
}

fn as_fd_raw(fd: &impl std::os::fd::AsFd) -> i32 {
    use std::os::fd::AsRawFd;
    fd.as_fd().as_raw_fd()
}

fn io_error(error: nix::errno::Errno) -> io::Error {
    io::Error::from_raw_os_error(error as i32)
}

fn set_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
}

struct Unlock<'a>(&'a File);

impl Drop for Unlock<'_> {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::FromRawFd;
    use std::os::unix::fs::PermissionsExt;

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_string()).collect()
    }

    fn moi_args() -> Vec<String> {
        args(&[
            "--edge-id",
            "runner-abc",
            "--server-url",
            "wss://astra.example",
            "--token",
            "secret-token",
            "--reconnect",
            "true",
        ])
    }

    #[test]
    fn interactive_moi_command_detaches_without_a_new_flag() {
        assert!(should_detach_with(&moi_args(), true, None, false, None));
    }

    #[test]
    fn sandbox_environment_and_other_invocations_stay_in_the_foreground() {
        assert!(!should_detach_with(&args(&[]), true, None, false, None));
        assert!(!should_detach_with(
            &args(&[
                "--edge-id",
                "edge-sandbox",
                "--server-url",
                "wss://astra.example",
                "--token",
                "t",
                "--reconnect",
                "true"
            ]),
            true,
            None,
            false,
            None,
        ));
        assert!(!should_detach_with(&moi_args(), false, None, false, None));
        assert!(!should_detach_with(
            &moi_args(),
            true,
            Some("1"),
            false,
            None
        ));
        assert!(!should_detach_with(
            &moi_args(),
            true,
            Some("true"),
            false,
            None
        ));
        assert!(!should_detach_with(
            &moi_args(),
            true,
            Some("yes"),
            false,
            None
        ));
        assert!(!should_detach_with(&moi_args(), true, None, true, None));
        assert!(!should_detach_with(
            &args(&[
                "--edge-id",
                "runner-abc",
                "--server-url",
                "wss://astra.example",
                "--token",
                "t"
            ]),
            true,
            None,
            false,
            None,
        ));
    }

    #[test]
    fn equals_form_matches_and_token_is_not_stored() {
        let command = args(&[
            "--edge-id=runner-abc",
            "--server-url=wss://astra.example",
            "--token=secret-token",
            "--reconnect=true",
        ]);
        let identity = user_runner_identity_with(&command, None).unwrap().unwrap();
        let dir = state_dir(std::env::temp_dir().as_path(), &identity);
        assert!(!dir.display().to_string().contains("secret-token"));
        assert_eq!(identity_key(&identity).len(), 32);
        assert_eq!(identity.edge_id, "runner-abc");
        let root = tempfile::tempdir().unwrap();
        let stored = root.path().join("instance");
        std::fs::create_dir_all(&stored).unwrap();
        write_metadata(&stored, &identity).unwrap();
        let metadata = std::fs::read_to_string(stored.join("metadata.json")).unwrap();
        assert!(!metadata.contains("secret-token"));
        assert!(metadata.contains("runner-abc"));
    }

    #[test]
    fn status_stop_and_logs_are_separate_from_the_connect_command() {
        assert!(parse_management(&moi_args()).is_none());
        let status = parse_management(&args(&["status", "--edge-id", "runner-abc"])).unwrap();
        assert!(matches!(status.action, Action::Status));
        assert_eq!(status.edge_id.as_deref(), Some("runner-abc"));
        assert!(status.server_url.is_none());
        let logs = parse_management(&args(&[
            "logs",
            "--edge-id",
            "runner-abc",
            "--server-url",
            "wss://astra.example",
        ]))
        .unwrap();
        assert_eq!(logs.server_url.as_deref(), Some("wss://astra.example"));
        let missing = parse_management(&args(&["status", "--edge-id"])).unwrap();
        assert_eq!(missing.edge_id.as_deref(), Some(""));
    }

    #[test]
    fn workspace_env_matches_the_runtime_and_is_passed_to_the_child() {
        let root = tempfile::tempdir().unwrap();
        let identity = user_runner_identity_with(&moi_args(), Some(root.path().to_str().unwrap()))
            .unwrap()
            .unwrap();
        assert_eq!(identity.workspace, root.path().canonicalize().unwrap());
        let flagged = args(&[
            "--workspace-dir=/tmp",
            "--edge-id",
            "runner-abc",
            "--server-url",
            "wss://astra.example",
            "--token",
            "secret-token",
            "--reconnect",
            "true",
        ]);
        let from_flag = user_runner_identity_with(&flagged, Some(root.path().to_str().unwrap()))
            .unwrap()
            .unwrap();
        assert_eq!(from_flag.workspace, std::fs::canonicalize("/tmp").unwrap());
        let child = command_with_workspace(&flagged, &identity.workspace);
        assert_eq!(
            child
                .iter()
                .filter(|arg| *arg == "--workspace-dir" || arg.starts_with("--workspace-dir="))
                .count(),
            1
        );
        assert!(child.windows(2).any(|pair| {
            pair[0] == "--workspace-dir" && pair[1] == identity.workspace.display().to_string()
        }));
    }

    #[test]
    fn same_edge_id_on_two_servers_is_selected_by_server_url() {
        let root = tempfile::tempdir().unwrap();
        let records = [
            Record {
                dir: root.path().to_path_buf(),
                edge_id: "runner-abc".to_string(),
                server_url: "wss://a.example".to_string(),
                workspace: "/work".to_string(),
            },
            Record {
                dir: root.path().to_path_buf(),
                edge_id: "runner-abc".to_string(),
                server_url: "wss://b.example".to_string(),
                workspace: "/work".to_string(),
            },
        ];
        assert!(matches!(
            select_instances(&records, Some("runner-abc"), None, Action::Stop, false),
            Err(2)
        ));
        let selected = select_instances(
            &records,
            Some("runner-abc"),
            Some("https://b.example/"),
            Action::Logs,
            false,
        )
        .unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].server_url, "wss://b.example");
    }

    #[test]
    fn log_rotation_and_tail_keep_only_the_recent_end() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("edge.log");
        std::fs::write(&path, "old-log").unwrap();
        let rotated = rotate_log_file(&path, 3).unwrap().unwrap();
        drop(rotated);
        let archived = std::fs::read_to_string(root.path().join("edge.log.1")).unwrap();
        assert_eq!(archived, "old-log");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
        let mut body = String::new();
        for index in 0..1000 {
            body.push_str(&format!("line-{index:04}\n"));
        }
        std::fs::write(&path, body).unwrap();
        let lines = read_log_tail(&path, 5, 64).unwrap();
        assert!(lines.iter().all(|line| !line.contains("line-0000")));
        assert!(lines.last().unwrap().contains("line-0999"));
    }

    #[test]
    fn a_failed_log_replacement_keeps_the_current_file_retryable() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("edge.log");
        std::fs::write(&path, b"0123456789").unwrap();
        let error = rotate_log_file_opening(&path, 3, |_| Err(io::Error::from_raw_os_error(24)))
            .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(24));
        assert_eq!(std::fs::read(&path).unwrap(), b"0123456789");
        assert!(!path.with_extension("log.1").exists());
        let rotated = rotate_log_file(&path, 3).unwrap().unwrap();
        drop(rotated);
        assert_eq!(
            std::fs::read(path.with_extension("log.1")).unwrap(),
            b"0123456789"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"");
    }

    #[test]
    fn a_missing_current_log_is_restored_from_the_rotated_file() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("edge.log");
        std::fs::write(path.with_extension("log.1"), b"still-growing").unwrap();
        assert!(rotate_log_file(&path, 3).unwrap().is_none());
        assert_eq!(std::fs::read(&path).unwrap(), b"still-growing");
        assert!(!path.with_extension("log.1").exists());
    }

    #[test]
    fn stdio_follows_the_current_log_after_a_failed_redirection() {
        let root = tempfile::tempdir().unwrap();
        let current = root.path().join("edge.log");
        let archived = root.path().join("edge.log.1");
        std::fs::write(&archived, b"old").unwrap();
        std::fs::write(&current, b"new").unwrap();
        let old = File::open(&archived).unwrap();
        let fd = as_fd_raw(&old);
        assert!(redirect_fd_if_different(fd, &current).unwrap());
        let (device, inode) = fd_identity(fd).unwrap();
        let desired = std::fs::metadata(&current).unwrap();
        assert_eq!(device, desired.dev());
        assert_eq!(inode, desired.ino());
        assert!(!redirect_fd_if_different(fd, &current).unwrap());
    }

    #[tokio::test]
    async fn cancellation_interrupts_pending_work() {
        let token = CancellationToken::new();
        token.cancel();
        let started = Instant::now();
        assert!(
            until_cancelled(&token, std::future::pending::<()>())
                .await
                .is_err()
        );
        assert!(
            until_cancelled(&token, tokio::time::sleep(Duration::from_secs(30)))
                .await
                .is_err()
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn current_readiness_updates_phase_and_stale_readiness_does_not() {
        let state = Mutex::new(PhaseState {
            epoch: 0,
            connected: None,
        });
        let root = tempfile::tempdir().unwrap();
        let (parent_fd, child_fd) = readiness_channel().unwrap();
        let raw = as_fd_raw(&child_fd);
        std::mem::forget(child_fd);
        let acker = std::thread::spawn(move || {
            let mut parent = parent_fd;
            let raw = as_fd_raw(&parent);
            let _ = nix::fcntl::fcntl(
                unsafe { std::os::fd::BorrowedFd::borrow_raw(raw) },
                nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK),
            );
            let mut buf = [0u8; 1];
            let started = Instant::now();
            loop {
                match parent.read(&mut buf) {
                    Ok(1) => {
                        parent.write_all(b"1").unwrap();
                        parent.flush().unwrap();
                        let mut confirm = [0u8; 1];
                        let confirm_started = Instant::now();
                        loop {
                            match parent.read(&mut confirm) {
                                Ok(1) => return,
                                _ if confirm_started.elapsed() > Duration::from_secs(2) => return,
                                _ => std::thread::sleep(Duration::from_millis(10)),
                            }
                        }
                    }
                    _ if started.elapsed() > Duration::from_secs(2) => return,
                    _ => std::thread::sleep(Duration::from_millis(10)),
                }
            }
        });
        let epoch = phase_epoch(&state);
        signal_ready_for_epoch_with(&state, epoch, Some(root.path()), Some(raw))
            .expect("current readiness is accepted");
        acker.join().unwrap();
        note_phase_with(&state, root.path(), "reconnecting");
        signal_ready_for_epoch_with(&state, epoch, Some(root.path()), None).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.path().join("phase")).unwrap(),
            "reconnecting"
        );
        let epoch = phase_epoch(&state);
        signal_ready_for_epoch_with(&state, epoch, Some(root.path()), None).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.path().join("phase")).unwrap(),
            "connected"
        );
    }

    #[test]
    fn a_reconnect_that_wins_the_phase_lock_is_not_published_as_connected() {
        let state = std::sync::Arc::new(Mutex::new(PhaseState {
            epoch: 0,
            connected: None,
        }));
        let root = tempfile::tempdir().unwrap();
        let epoch = phase_epoch(&state);
        let mut held = lock_phase(&state);
        let callback_state = std::sync::Arc::clone(&state);
        let dir = root.path().to_path_buf();
        let callback = std::thread::spawn(move || {
            signal_ready_for_epoch_with(&callback_state, epoch, Some(&dir), None).unwrap();
        });
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !callback.is_finished(),
            "readiness was published without the phase lock"
        );
        std::fs::write(root.path().join("phase"), b"reconnecting").unwrap();
        held.epoch = held.epoch.wrapping_add(1);
        drop(held);
        callback.join().unwrap();
        assert_eq!(
            std::fs::read(root.path().join("phase")).unwrap(),
            b"reconnecting"
        );
    }

    #[tokio::test]
    async fn delayed_readiness_cannot_overwrite_a_newer_phase() {
        let state = std::sync::Arc::new(Mutex::new(PhaseState {
            epoch: 0,
            connected: None,
        }));
        let root = tempfile::tempdir().unwrap();
        note_phase_with(&state, root.path(), "starting");
        let epoch = phase_epoch(&state);
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let (done_sender, done_receiver) = tokio::sync::oneshot::channel();
        let task_state = std::sync::Arc::clone(&state);
        let dir = root.path().to_path_buf();
        tokio::spawn(async move {
            if receiver.await.is_ok() {
                signal_ready_for_epoch_with(&task_state, epoch, Some(&dir), None).unwrap();
            }
            let _ = done_sender.send(());
        });
        note_phase_with(&state, root.path(), "reconnecting");
        sender.send(()).unwrap();
        done_receiver.await.unwrap();
        assert_eq!(
            std::fs::read_to_string(root.path().join("phase")).unwrap(),
            "reconnecting"
        );
    }

    #[cfg(unix)]
    #[test]
    fn readiness_byte_unblocks_the_parent_and_failure_reaps_the_child() {
        // POSIX sh only redirects single-digit descriptors, so the pipe is
        // mapped to stdout rather than passed by its allocated number.
        let (mut parent_fd, child_fd) = readiness_channel().unwrap();
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("printf 1 >&1; dd bs=1 count=1 of=/dev/null <&1 >/dev/null 2>/dev/null; printf 1 >&1; sleep 30")
            .env(READY_FD_ENV, "1")
            .stdout(Stdio::from(child_fd));
        let mut child = command.spawn().unwrap();
        drop(command);
        let cancelled = AtomicBool::new(false);
        wait_for_ready(
            &mut child,
            &mut parent_fd,
            Duration::from_secs(2),
            &cancelled,
        )
        .unwrap();
        assert!(
            child.try_wait().unwrap().is_none(),
            "readiness was accepted after the child exited"
        );
        let _ = child.kill();
        let _ = child.wait();

        let (read_fd, write_fd) = nix::unistd::pipe().unwrap();
        let raw = as_fd_raw(&write_fd);
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("exit 1")
            .env(READY_FD_ENV, raw.to_string());
        unsafe {
            command.pre_exec(move || clear_cloexec(raw));
        }
        let mut child = command.spawn().unwrap();
        drop(write_fd);
        let cancelled = AtomicBool::new(false);
        let error = wait_for_ready(
            &mut child,
            &mut File::from(read_fd),
            Duration::from_secs(2),
            &cancelled,
        );
        assert!(error.is_err());
        kill_child(&mut child);
    }

    #[cfg(unix)]
    #[test]
    fn cancelling_startup_reaps_the_child_before_it_is_ready() {
        let (read_fd, write_fd) = nix::unistd::pipe().unwrap();
        let raw = as_fd_raw(&write_fd);
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("sleep 30")
            .env(READY_FD_ENV, raw.to_string());
        unsafe {
            command.pre_exec(move || clear_cloexec(raw));
        }
        let mut child = command.spawn().unwrap();
        let cancelled = AtomicBool::new(true);
        let started = Instant::now();
        let error = wait_for_ready(
            &mut child,
            &mut File::from(read_fd),
            Duration::from_secs(30),
            &cancelled,
        );
        assert!(error.unwrap_err().contains("startup cancelled"));
        assert!(started.elapsed() < Duration::from_secs(2));
        kill_child(&mut child);
        drop(write_fd);
    }

    #[cfg(unix)]
    #[test]
    fn sigint_marks_startup_cancelled_without_killing_the_parent() {
        let startup = StartupCancel::arm().unwrap();
        nix::sys::signal::raise(nix::sys::signal::Signal::SIGINT).unwrap();
        assert!(startup.flag().load(Ordering::Relaxed));
        drop(startup);
        assert!(!STARTUP_CANCELLED.load(Ordering::Relaxed));
    }

    #[test]
    fn equivalent_server_urls_share_one_runner_and_commands_use_the_installed_path() {
        let workspace = std::fs::canonicalize(".").unwrap();
        let with_slash = connect_args("https://example.test/", "secret-token", &workspace);
        let without_slash = connect_args("https://example.test", "secret-token", &workspace);
        let left = user_runner_identity_with(&with_slash, None)
            .unwrap()
            .unwrap();
        let right = user_runner_identity_with(&without_slash, None)
            .unwrap()
            .unwrap();
        assert_eq!(left.server_url, right.server_url);
        assert_eq!(identity_key(&left), identity_key(&right));
        assert!(same_destination(
            "https://example.test",
            "https://example.test/"
        ));
        assert!(!same_destination("https://a.example", "https://b.example"));
        let command = management_command(
            Path::new("/opt/My Tools/astra-edge"),
            "stop",
            "runner-abc",
            &left.server_url,
        );
        assert_eq!(
            command,
            format!(
                "'/opt/My Tools/astra-edge' stop --edge-id 'runner-abc' --server-url {}",
                shell_quote(&left.server_url)
            )
        );
        assert_eq!(shell_quote("/tmp/a'b"), "'/tmp/a'\\''b'");
    }

    #[test]
    fn final_authentication_decision_excludes_a_concurrent_handoff() {
        let dir = tempfile::tempdir().unwrap();
        assert!(lock_credential_handoff_with(Some(dir.path()), false).is_none());
        let held = lock_credential_handoff_with(Some(dir.path()), true).unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let path = dir.path().to_owned();
        let command = std::thread::spawn(move || {
            let start = lock_start(&path).unwrap();
            sender.send(()).unwrap();
            drop(start);
        });
        assert!(
            receiver.recv_timeout(Duration::from_millis(200)).is_err(),
            "a connect command entered while the runner was deciding"
        );
        drop(held);
        receiver.recv_timeout(Duration::from_secs(5)).unwrap();
        command.join().unwrap();
    }

    fn connect_args(server_url: &str, token: &str, workspace: &Path) -> Vec<String> {
        args(&[
            "--edge-id",
            "runner-abc",
            "--server-url",
            server_url,
            "--token",
            token,
            "--reconnect",
            "true",
            "--workspace-dir",
            workspace.to_str().unwrap(),
        ])
    }

    struct RestoreMode(PathBuf, u32);

    impl Drop for RestoreMode {
        fn drop(&mut self) {
            set_mode(&self.0, self.1);
        }
    }

    const HOLD_DIR_ENV: &str = "ASTRA_EDGE_TEST_HOLD_DIR";
    const HOLD_CONNECTED_ENV: &str = "ASTRA_EDGE_TEST_HOLD_CONNECTED";
    const HOLD_DELAY_ENV: &str = "ASTRA_EDGE_TEST_HOLD_DELAY_MS";
    const HOLD_READY_DELAY_ENV: &str = "ASTRA_EDGE_TEST_HOLD_READY_DELAY_MS";
    const HOLDER_TEST: &str = "managed_background::tests::runner_lock_holder";
    const HOLDER_READY: &str = "astra-edge-test-holder-ready";

    /// Does nothing in a normal run. Other tests start this binary with only
    /// this test selected so that a separate process owns a runner's locks,
    /// as a real runner does; a process never sees its own record lock.
    #[test]
    fn runner_lock_holder() {
        let Some(dir) = std::env::var_os(HOLD_DIR_ENV).map(PathBuf::from) else {
            return;
        };
        if let Some(delay) = std::env::var(HOLD_DELAY_ENV)
            .ok()
            .and_then(|value| value.parse().ok())
        {
            std::thread::sleep(Duration::from_millis(delay));
        }
        if std::env::var("ASTRA_EDGE_TEST_HOLD_IGNORE_TERM").as_deref() == Ok("1") {
            // SAFETY: SIG_IGN is a valid handler and SIGTERM is a valid signal.
            unsafe {
                let _ = nix::sys::signal::signal(
                    nix::sys::signal::Signal::SIGTERM,
                    nix::sys::signal::SigHandler::SigIgn,
                );
            }
        }
        let _instance = acquire_instance_lock(&dir).unwrap();
        let connected = open_lock_file(&dir.join("connected.lock")).unwrap();
        if std::env::var(HOLD_CONNECTED_ENV).as_deref() != Ok("0") {
            connected.lock_exclusive().unwrap();
        }
        if let Some(delay) = std::env::var(HOLD_READY_DELAY_ENV)
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
        {
            std::thread::sleep(Duration::from_millis(delay));
        }
        if let Some(fd) = std::env::var(READY_FD_ENV)
            .ok()
            .and_then(|value| value.parse().ok())
        {
            let _ = notify_parent_ready_fd(fd);
        }
        println!("{HOLDER_READY}");
        std::io::stdout().flush().unwrap();
        std::thread::sleep(Duration::from_secs(60));
    }

    struct Holder(Child);

    impl Holder {
        fn spawn(dir: &Path, connected: bool) -> Self {
            Self::spawn_inner(dir, connected, false)
        }

        fn spawn_ignoring_term(dir: &Path, connected: bool) -> Self {
            Self::spawn_inner(dir, connected, true)
        }

        fn spawn_inner(dir: &Path, connected: bool, ignore_term: bool) -> Self {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([HOLDER_TEST, "--exact", "--nocapture", "--test-threads=1"])
                .env(HOLD_DIR_ENV, dir)
                .env(HOLD_CONNECTED_ENV, if connected { "1" } else { "0" })
                .env_remove(READY_FD_ENV)
                .stdin(Stdio::null())
                .stdout(Stdio::piped());
            if ignore_term {
                command.env("ASTRA_EDGE_TEST_HOLD_IGNORE_TERM", "1");
            }
            let mut child = command.spawn().unwrap();
            let stdout = child.stdout.take().unwrap();
            let mut lines = std::io::BufRead::lines(std::io::BufReader::new(stdout));
            loop {
                let line = lines
                    .next()
                    .expect("holder exited before it held the lock")
                    .unwrap();
                if line.trim_end().ends_with(HOLDER_READY) {
                    break;
                }
            }
            Self(child)
        }

        fn pid(&self) -> u32 {
            self.0.id()
        }

        fn is_alive(&mut self) -> bool {
            self.0.try_wait().unwrap().is_none()
        }
    }

    impl Drop for Holder {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// Kills whatever process owns the runner's instance lock when dropped.
    struct KillOwner(PathBuf);

    impl Drop for KillOwner {
        fn drop(&mut self) {
            if let Ok(Some(pid)) = instance_owner(&self.0) {
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(pid as i32),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
        }
    }

    /// A stand-in runner executable for `supervise`: it records its argv and
    /// then owns the runner's locks, connected, and reports readiness.
    fn holder_executable(root: &Path, name: &str, delay_ms: u64) -> (PathBuf, PathBuf) {
        holder_executable_with(root, name, delay_ms, 0)
    }

    fn holder_executable_with(
        root: &Path,
        name: &str,
        delay_ms: u64,
        ready_delay_ms: u64,
    ) -> (PathBuf, PathBuf) {
        let argv_log = root.join(format!("{name}.argv"));
        let script = root.join(name);
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> {argv}\n{HOLD_DIR_ENV}=\"$ASTRA_EDGE_STATE_DIR\" {HOLD_DELAY_ENV}={delay_ms} {HOLD_READY_DELAY_ENV}={ready_delay_ms} exec {exe} {HOLDER_TEST} --exact --nocapture --test-threads=1\n",
                argv = shell_quote(&argv_log.display().to_string()),
                exe = shell_quote(&std::env::current_exe().unwrap().display().to_string()),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        (script, argv_log)
    }

    fn wait_until_unlocked(dir: &Path) {
        for _ in 0..200 {
            if !instance_is_locked(dir).unwrap() {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("instance lock was not released");
    }

    fn management(root: &Path, parts: &[&str]) -> i32 {
        management_exit_code(&args(parts), root).unwrap()
    }

    fn is_root() -> bool {
        // SAFETY: geteuid has no preconditions and cannot fail.
        unsafe { nix::libc::geteuid() == 0 }
    }

    #[test]
    fn instance_lock_reports_its_owner_and_is_released_with_the_process() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(instance_owner(root.path()).unwrap(), None);
        let holder = Holder::spawn(root.path(), false);
        assert_eq!(instance_owner(root.path()).unwrap(), Some(holder.pid()));
        assert!(
            acquire_instance_lock(root.path()).is_err(),
            "a second runner took the instance lock"
        );
        drop(holder);
        wait_until_unlocked(root.path());
    }

    #[test]
    fn stopped_runner_keeps_an_authentication_failure() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(phase_label(root.path()).unwrap(), "stopped");
        write_private(&root.path().join("phase"), b"authentication-failure").unwrap();
        assert_eq!(phase_label(root.path()).unwrap(), "authentication-failure");
        let holder = Holder::spawn(root.path(), false);
        assert_eq!(phase_label(root.path()).unwrap(), "authentication-failure");
        drop(holder);
        wait_until_unlocked(root.path());
        assert_eq!(phase_label(root.path()).unwrap(), "authentication-failure");
    }

    #[test]
    fn connected_is_reported_only_while_the_runner_holds_its_connected_lock() {
        let root = tempfile::tempdir().unwrap();
        let identity = Identity {
            edge_id: "runner-abc".to_string(),
            server_url: "wss://astra.example".to_string(),
            workspace: std::fs::canonicalize(".").unwrap(),
        };
        let exe = Path::new("astra-edge");
        assert_eq!(running_instance(root.path(), &identity, "token", exe), 1);
        write_metadata(root.path(), &identity).unwrap();

        let disconnected = Holder::spawn(root.path(), false);
        write_private(&root.path().join("phase"), b"reconnecting").unwrap();
        assert_eq!(phase_label(root.path()).unwrap(), "reconnecting");
        assert_eq!(running_instance(root.path(), &identity, "token", exe), 1);
        // A phase file that still says connected is not proof of a connection.
        write_private(&root.path().join("phase"), b"connected").unwrap();
        assert_eq!(phase_label(root.path()).unwrap(), "unknown");
        assert_eq!(running_instance(root.path(), &identity, "token", exe), 1);
        drop(disconnected);
        wait_until_unlocked(root.path());

        let _connected = Holder::spawn(root.path(), true);
        assert_eq!(phase_label(root.path()).unwrap(), "connected");
        assert_eq!(running_instance(root.path(), &identity, "token", exe), 0);
    }

    #[test]
    fn a_failed_phase_write_does_not_leave_the_runner_reported_connected() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let _instance = Holder::spawn(dir, false);
        let state = Mutex::new(PhaseState {
            epoch: 0,
            connected: Some(open_lock_file(&dir.join("connected.lock")).unwrap()),
        });
        let epoch = phase_epoch(&state);
        signal_ready_for_epoch_with(&state, epoch, Some(dir), None).unwrap();
        assert_eq!(phase_label(dir).unwrap(), "connected");
        set_mode(dir, 0o500);
        let restore = RestoreMode(dir.to_path_buf(), 0o700);
        note_phase_with(&state, dir, "reconnecting");
        let phase = phase_label(dir).unwrap();
        drop(restore);
        assert_ne!(phase, "connected");
        if !is_root() {
            assert_eq!(phase, "unknown");
        }
    }

    #[test]
    fn every_connect_command_hands_its_credential_to_the_running_runner() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let workspace = workspace.path().canonicalize().unwrap();
        let (holder, argv_log) = holder_executable(root.path(), "holder.sh", 0);
        let old = connect_args("https://example.test", "old-token", &workspace);
        let identity = user_runner_identity_with(&old, None).unwrap().unwrap();
        let dir = state_dir(root.path(), &identity);
        ensure_private_dir(&dir).unwrap();
        // Left behind by a runner that exited: it must not reach a new runner.
        queue_pending_credential(&dir, "stale-token").unwrap();
        let _cleanup = KillOwner(dir.clone());
        assert_eq!(supervise(&holder, &old, root.path()), 0);
        assert_eq!(claim_pending_credential(&dir).unwrap(), None);
        let owner = instance_owner(&dir).unwrap().unwrap();

        let fresh = connect_args("https://example.test/", "fresh-token", &workspace);
        assert_eq!(supervise(&holder, &fresh, root.path()), 0);
        assert_eq!(
            claim_pending_credential(&dir).unwrap().as_deref(),
            Some("fresh-token")
        );
        // The runner tried the credential and it was rejected.
        finish_claim(&dir);
        assert_eq!(supervise(&holder, &fresh, root.path()), 0);
        assert_eq!(
            claim_pending_credential(&dir).unwrap().as_deref(),
            Some("fresh-token"),
            "a rejected credential could not be submitted again"
        );
        finish_claim(&dir);

        assert_eq!(instance_owner(&dir).unwrap(), Some(owner));
        let argv = std::fs::read_to_string(&argv_log).unwrap();
        assert!(argv.contains("old-token"), "first runner did not start");
        assert!(
            !argv.contains("fresh-token"),
            "a handoff started a second process"
        );
        let metadata = std::fs::read_to_string(dir.join("metadata.json")).unwrap();
        assert!(!metadata.contains("old-token"));
        assert!(!metadata.contains("fresh-token"));

        write_private(&dir.join("phase"), b"stopping").unwrap();
        // A stopping runner holds no connected lock and takes no credential.
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(owner as i32),
            nix::sys::signal::Signal::SIGKILL,
        )
        .unwrap();
        wait_until_unlocked(&dir);
        let _stopping = Holder::spawn(&dir, false);
        let late = connect_args("https://example.test", "late-token", &workspace);
        assert_eq!(supervise(&holder, &late, root.path()), 1);
        assert_eq!(claim_pending_credential(&dir).unwrap(), None);
    }

    #[test]
    fn claims_survive_read_failures_and_interrupted_consumers() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        queue_pending_credential(dir, "generation-one").unwrap();
        assert_eq!(
            claim_pending_credential(dir).unwrap().as_deref(),
            Some("generation-one")
        );
        // The consumer stopped before finishing: the claim is retried.
        assert_eq!(
            claim_pending_credential(dir).unwrap().as_deref(),
            Some("generation-one")
        );
        // A newer handoff supersedes the unfinished claim; finishing one
        // claim never removes a later handoff.
        queue_pending_credential(dir, "generation-two").unwrap();
        assert_eq!(
            claim_pending_credential(dir).unwrap().as_deref(),
            Some("generation-two")
        );
        queue_pending_credential(dir, "generation-three").unwrap();
        finish_claim(dir);
        assert_eq!(
            claim_pending_credential(dir).unwrap().as_deref(),
            Some("generation-three")
        );
        finish_claim(dir);
        assert_eq!(claim_pending_credential(dir).unwrap(), None);

        if is_root() {
            return;
        }
        queue_pending_credential(dir, "generation-four").unwrap();
        std::fs::rename(pending_credential_path(dir), claimed_credential_path(dir)).unwrap();
        set_mode(&claimed_credential_path(dir), 0o000);
        let restore = RestoreMode(claimed_credential_path(dir), 0o600);
        assert!(claim_pending_credential(dir).is_err());
        assert!(
            claimed_credential_path(dir).exists(),
            "a failed read lost the credential"
        );
        drop(restore);
        assert_eq!(
            claim_pending_credential(dir).unwrap().as_deref(),
            Some("generation-four")
        );
    }

    #[test]
    fn stop_waits_for_a_start_in_progress_and_stops_its_runner() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let workspace = workspace.path().canonicalize().unwrap();
        let (slow, _) = holder_executable(root.path(), "slow-holder.sh", 700);
        let command = connect_args("https://example.test", "secret-token", &workspace);
        let identity = user_runner_identity_with(&command, None).unwrap().unwrap();
        let dir = state_dir(root.path(), &identity);
        let _cleanup = KillOwner(dir.clone());
        let start_root = root.path().to_path_buf();
        let start = std::thread::spawn(move || supervise(&slow, &command, &start_root));
        while !dir.join("metadata.json").exists() {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(instance_owner(&dir).unwrap(), None);
        let stopped = management(
            root.path(),
            &[
                "stop",
                "--edge-id",
                "runner-abc",
                "--server-url",
                "https://example.test",
            ],
        );
        let start_code = start.join().unwrap();
        assert_eq!(stopped, 0);
        assert_eq!(
            instance_owner(&dir).unwrap(),
            None,
            "a runner started during stop remained running"
        );
        assert!(
            start_code == 0 || start_code == 1,
            "start exited {start_code}"
        );
    }

    #[test]
    fn stop_does_not_wait_for_authentication_once_the_runner_exists() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let workspace = workspace.path().canonicalize().unwrap();
        let (slow, _) = holder_executable_with(root.path(), "slow-ready.sh", 0, 5_000);
        let command = connect_args("https://example.test", "secret-token", &workspace);
        let identity = user_runner_identity_with(&command, None).unwrap().unwrap();
        let dir = state_dir(root.path(), &identity);
        let _cleanup = KillOwner(dir.clone());
        let start_root = root.path().to_path_buf();
        let start = std::thread::spawn(move || supervise(&slow, &command, &start_root));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !instance_is_locked(&dir).unwrap_or(false) || start_lock_is_held(&dir).unwrap_or(true)
        {
            assert!(
                Instant::now() < deadline,
                "startup did not release its lock after the runner existed"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let started = Instant::now();
        let stopped = management(
            root.path(),
            &[
                "stop",
                "--edge-id",
                "runner-abc",
                "--server-url",
                "https://example.test",
            ],
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "stop waited for authentication"
        );
        assert_eq!(stopped, 0);
        assert_ne!(start.join().unwrap(), 0);
        assert_eq!(instance_owner(&dir).unwrap(), None);
    }

    #[test]
    fn stop_claims_a_start_that_has_not_published_metadata() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let workspace = workspace.path().canonicalize().unwrap();
        let command = connect_args("https://example.test", "secret-token", &workspace);
        let identity = user_runner_identity_with(&command, None).unwrap().unwrap();
        let dir = state_dir(root.path(), &identity);
        ensure_private_dir(&dir).unwrap();
        let held = lock_start(&dir).unwrap();
        let stop_root = root.path().to_path_buf();
        let stop = std::thread::spawn(move || {
            management(
                &stop_root,
                &[
                    "stop",
                    "--edge-id",
                    "runner-abc",
                    "--server-url",
                    "https://example.test",
                ],
            )
        });
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !stop.is_finished(),
            "stop returned while startup still held start.lock"
        );
        write_metadata(&dir, &identity).unwrap();
        let mut runner = Holder::spawn(&dir, true);
        drop(held);
        assert_eq!(stop.join().unwrap(), 0);
        assert!(
            !runner.is_alive(),
            "the runner published by the in-progress start survived stop"
        );
    }

    #[test]
    fn stop_discards_an_unpublished_start_that_has_no_runner() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let workspace = workspace.path().canonicalize().unwrap();
        let command = connect_args("https://example.test", "secret-token", &workspace);
        let identity = user_runner_identity_with(&command, None).unwrap().unwrap();
        let dir = state_dir(root.path(), &identity);
        ensure_private_dir(&dir).unwrap();
        let held = lock_start(&dir).unwrap();
        let stop_root = root.path().to_path_buf();
        let stop = std::thread::spawn(move || {
            management(
                &stop_root,
                &[
                    "stop",
                    "--edge-id",
                    "runner-abc",
                    "--server-url",
                    "https://example.test",
                ],
            )
        });
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !stop.is_finished(),
            "stop returned while startup still held start.lock"
        );
        drop(held);
        assert_eq!(stop.join().unwrap(), 0);
        assert!(
            !dir.exists(),
            "the unpublished start can still launch after stop"
        );
    }

    #[test]
    fn stop_waits_while_a_start_holds_the_registry_before_its_lock_exists() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let workspace = workspace.path().canonicalize().unwrap();
        let command = connect_args("https://example.test", "secret-token", &workspace);
        let identity = user_runner_identity_with(&command, None).unwrap().unwrap();
        let dir = state_dir(root.path(), &identity);
        let registry = lock_registry(root.path()).unwrap();
        assert!(!dir.exists());
        let stop_root = root.path().to_path_buf();
        let stop = std::thread::spawn(move || {
            management(
                &stop_root,
                &[
                    "stop",
                    "--edge-id",
                    "runner-abc",
                    "--server-url",
                    "https://example.test",
                ],
            )
        });
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !stop.is_finished(),
            "stop listed runners while a start held the registry"
        );
        ensure_private_dir(&dir).unwrap();
        let start = lock_start(&dir).unwrap();
        drop(registry);
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !stop.is_finished(),
            "stop returned before the start lock was released"
        );
        drop(start);
        assert_eq!(stop.join().unwrap(), 0);
        assert!(
            !dir.exists(),
            "the unpublished start can still launch after stop"
        );
    }

    #[test]
    fn a_duplicate_start_does_not_block_another_runners_status() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let workspace = workspace.path().canonicalize().unwrap();
        let (slow, _) = holder_executable(root.path(), "slow-holder.sh", 1_500);
        let command = connect_args("https://example.test", "secret-token", &workspace);
        let identity = user_runner_identity_with(&command, None).unwrap().unwrap();
        let dir = state_dir(root.path(), &identity);
        let _cleanup = KillOwner(dir.clone());
        let start_root = root.path().to_path_buf();
        let start_command = command.clone();
        let first_slow = slow.clone();
        let first = std::thread::spawn(move || supervise(&first_slow, &start_command, &start_root));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !start_lock_is_held(&dir).unwrap_or(false) {
            assert!(
                Instant::now() < deadline,
                "the first start did not take its lock"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let duplicate_root = root.path().to_path_buf();
        let duplicate_command = command.clone();
        let duplicate_slow = slow.clone();
        let duplicate = std::thread::spawn(move || {
            supervise(&duplicate_slow, &duplicate_command, &duplicate_root)
        });
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !duplicate.is_finished(),
            "the duplicate start did not wait for the runner already starting"
        );
        let status_root = root.path().to_path_buf();
        let started = Instant::now();
        let status = std::thread::spawn(move || {
            management(
                &status_root,
                &[
                    "status",
                    "--edge-id",
                    "runner-other",
                    "--server-url",
                    "https://other.example",
                ],
            )
        });
        while started.elapsed() < Duration::from_millis(400) && !status.is_finished() {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            status.is_finished(),
            "status waited for a different runner's start"
        );
        assert_eq!(status.join().unwrap(), 0);
        assert_eq!(first.join().unwrap(), 0);
        // The duplicate may observe the runner as soon as it owns its process,
        // which can be before the first command has accepted readiness.
        let duplicate_code = duplicate.join().unwrap();
        assert!(
            duplicate_code == 0 || duplicate_code == 1,
            "duplicate start exited {duplicate_code}"
        );
    }

    #[test]
    fn logs_report_an_unreadable_file_instead_of_no_logs() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let workspace = workspace.path().canonicalize().unwrap();
        let command = connect_args("https://example.test", "secret-token", &workspace);
        let identity = user_runner_identity_with(&command, None).unwrap().unwrap();
        let dir = state_dir(root.path(), &identity);
        ensure_private_dir(&dir).unwrap();
        write_metadata(&dir, &identity).unwrap();
        let logs = [
            "logs",
            "--edge-id",
            "runner-abc",
            "--server-url",
            "https://example.test",
        ];
        assert_eq!(management(root.path(), &logs), 0);
        let current = dir.join("edge.log");
        std::fs::write(&current, b"kept").unwrap();
        set_mode(&current, 0);
        assert_eq!(management(root.path(), &logs), 1);
        set_mode(&current, 0o600);
        let previous = dir.join("edge.log.1");
        std::fs::write(&previous, b"older").unwrap();
        set_mode(&previous, 0);
        assert_eq!(management(root.path(), &logs), 1);
    }

    #[test]
    fn stop_does_not_delete_unreadable_state_that_is_not_starting() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let workspace = workspace.path().canonicalize().unwrap();
        let command = connect_args("https://example.test", "secret-token", &workspace);
        let identity = user_runner_identity_with(&command, None).unwrap().unwrap();
        let dir = state_dir(root.path(), &identity);
        ensure_private_dir(&dir).unwrap();
        std::fs::write(dir.join("metadata.json"), b"not json").unwrap();
        std::fs::write(dir.join("edge.log"), b"kept").unwrap();
        assert_eq!(
            management(
                root.path(),
                &[
                    "stop",
                    "--edge-id",
                    "runner-abc",
                    "--server-url",
                    "https://example.test",
                ],
            ),
            0
        );
        assert_eq!(std::fs::read(dir.join("edge.log")).unwrap(), b"kept");
    }

    #[test]
    fn a_stop_rejects_a_credential_while_the_runner_is_still_connected() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let workspace = workspace.path().canonicalize().unwrap();
        let command = connect_args("https://example.test", "secret-token", &workspace);
        let identity = user_runner_identity_with(&command, None).unwrap().unwrap();
        let dir = state_dir(root.path(), &identity);
        ensure_private_dir(&dir).unwrap();
        write_metadata(&dir, &identity).unwrap();
        let runner = Holder::spawn_ignoring_term(&dir, true);
        let stop_root = root.path().to_path_buf();
        let stop = std::thread::spawn(move || {
            management(
                &stop_root,
                &[
                    "stop",
                    "--edge-id",
                    "runner-abc",
                    "--server-url",
                    "https://example.test",
                ],
            )
        });
        for _ in 0..100 {
            if stop_is_requested(&dir) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(stop_is_requested(&dir), "stop did not record its decision");
        assert_eq!(phase_label(&dir).unwrap(), "stopping");
        assert_eq!(
            running_instance(&dir, &identity, "fresh-token", Path::new("astra-edge")),
            1
        );
        assert!(!pending_credential_path(&dir).exists());
        drop(runner);
        assert_eq!(stop.join().unwrap(), 0);
    }

    #[test]
    fn an_invalid_connect_option_does_not_hand_off_a_credential() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let workspace = workspace.path().canonicalize().unwrap();
        let command = connect_args("https://example.test", "secret-token", &workspace);
        let identity = user_runner_identity_with(&command, None).unwrap().unwrap();
        let dir = state_dir(root.path(), &identity);
        ensure_private_dir(&dir).unwrap();
        write_metadata(&dir, &identity).unwrap();
        let _runner = Holder::spawn(&dir, true);
        let mut invalid = command;
        invalid.push("--typoed-option".into());
        let code = supervise(Path::new("astra-edge"), &invalid, root.path());
        assert_ne!(code, 0);
        assert!(!pending_credential_path(&dir).exists());
    }

    #[test]
    fn a_buffered_ready_byte_is_not_success_after_the_child_exits() {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let (read_fd, write_fd) = nix::unistd::pipe().unwrap();
        let mut read = File::from(read_fd);
        let mut write = File::from(write_fd);
        assert!(child.wait().unwrap().success());
        write.write_all(b"1").unwrap();
        drop(write);
        let error = wait_for_ready(
            &mut child,
            &mut read,
            Duration::from_secs(2),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(error.contains("exited"), "{error}");
    }

    #[test]
    fn a_buffered_ready_byte_does_not_override_startup_cancellation() {
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let (read_fd, write_fd) = nix::unistd::pipe().unwrap();
        let mut read = File::from(read_fd);
        let mut write = File::from(write_fd);
        write.write_all(b"1").unwrap();
        drop(write);
        let error = wait_for_ready(
            &mut child,
            &mut read,
            Duration::from_secs(2),
            &AtomicBool::new(true),
        )
        .unwrap_err();
        assert!(error.contains("cancelled"), "{error}");
        assert!(child.try_wait().unwrap().is_none());
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn cancellation_during_confirmation_is_not_reported_as_success() {
        let (mut parent, mut child_end) = readiness_channel().unwrap();
        let mut sleeper = Command::new("sleep").arg("30").spawn().unwrap();
        let cancelled = AtomicBool::new(false);
        let started = Instant::now();
        let error = std::thread::scope(|scope| {
            scope.spawn(|| {
                child_end.write_all(b"1").unwrap();
                child_end.flush().unwrap();
                let mut ack = [0u8; 1];
                child_end.read_exact(&mut ack).unwrap();
                cancelled.store(true, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(300));
                let _ = child_end.write_all(b"1");
                let _ = child_end.flush();
            });
            wait_for_ready(
                &mut sleeper,
                &mut parent,
                Duration::from_secs(2),
                &cancelled,
            )
        })
        .unwrap_err();
        assert!(
            error.contains("cancelled"),
            "confirmation after cancellation was accepted: {error}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "cancellation waited for the startup deadline"
        );
        let _ = sleeper.kill();
        let _ = sleeper.wait();
    }

    #[test]
    fn hangup_cancels_startup() {
        let startup = StartupCancel::arm().unwrap();
        nix::sys::signal::raise(nix::sys::signal::Signal::SIGHUP).unwrap();
        let mut seen = false;
        for _ in 0..50 {
            if startup.flag().load(Ordering::SeqCst) {
                seen = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(seen, "SIGHUP did not cancel startup");
    }

    #[test]
    fn a_closed_readiness_pipe_is_not_accepted() {
        let (read_fd, write_fd) = nix::unistd::pipe().unwrap();
        drop(read_fd);
        use std::os::fd::IntoRawFd;
        let error = notify_parent_ready_fd(write_fd.into_raw_fd()).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::BrokenPipe);
    }

    #[test]
    fn a_ready_byte_is_not_acceptance_until_the_parent_acks() {
        use std::os::fd::IntoRawFd;
        let (mut parent, child) = readiness_channel().unwrap();
        let raw = child.into_raw_fd();
        let worker = std::thread::spawn(move || notify_parent_ready_fd(raw));
        let mut buf = [0u8; 1];
        parent.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"1");
        assert!(
            !worker.is_finished(),
            "the child treated the write as acceptance"
        );
        drop(parent);
        let error = worker.join().unwrap().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::BrokenPipe);
    }

    #[test]
    fn a_buffered_ready_byte_is_refused_when_the_connection_is_gone() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let _runner = Holder::spawn(dir, false);
        write_private(&dir.join("phase"), b"reconnecting").unwrap();
        let (mut parent, mut child_end) = readiness_channel().unwrap();
        child_end.write_all(b"1").unwrap();
        child_end.flush().unwrap();
        let mut sleeper = Command::new("sleep").arg("30").spawn().unwrap();
        let error = wait_for_ready_in(
            &mut sleeper,
            &mut parent,
            Duration::from_secs(2),
            &AtomicBool::new(false),
            Some(dir),
            &mut || {},
        )
        .unwrap_err();
        assert!(
            error.contains("reconnecting"),
            "a stale ready byte was accepted: {error}"
        );
        let _ = sleeper.kill();
        let _ = sleeper.wait();
    }

    #[test]
    fn an_acknowledgement_after_disconnect_does_not_accept_startup() {
        let state = Mutex::new(PhaseState {
            epoch: 0,
            connected: None,
        });
        let root = tempfile::tempdir().unwrap();
        let (mut parent, child) = readiness_channel().unwrap();
        let raw = as_fd_raw(&child);
        std::mem::forget(child);
        let epoch = phase_epoch(&state);
        let worker = std::thread::scope(|scope| {
            let handle = scope.spawn(|| {
                let mut channel = unsafe { File::from_raw_fd(raw) };
                finish_parent_acceptance(&state, epoch, &mut channel)
            });
            std::thread::sleep(Duration::from_millis(50));
            note_phase_with(&state, root.path(), "reconnecting");
            parent.write_all(b"1").unwrap();
            parent.flush().unwrap();
            handle.join().expect("acceptance task panicked")
        });
        assert!(
            matches!(worker, Err(StartupAcceptError::ConnectionEnded)),
            "startup was accepted after the connection ended: {worker:?}"
        );
    }

    #[tokio::test]
    async fn losing_the_starting_command_is_observed_before_acceptance() {
        let (parent, child) = readiness_channel().unwrap();
        let raw = as_fd_raw(&child);
        std::mem::forget(child);
        let waiting = tokio::spawn(async move {
            loop {
                if parent_socket_is_closed(raw).unwrap_or(true) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiting.is_finished());
        drop(parent);
        tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .expect("parent loss was not observed")
            .unwrap();
    }

    #[test]
    fn stop_signals_only_the_process_that_owns_the_instance_lock() {
        let root = tempfile::tempdir().unwrap();
        let workspace = std::fs::canonicalize(".").unwrap();
        let command = connect_args("https://example.test", "secret-token", &workspace);
        let identity = user_runner_identity_with(&command, None).unwrap().unwrap();
        let dir = state_dir(root.path(), &identity);
        ensure_private_dir(&dir).unwrap();
        write_metadata(&dir, &identity).unwrap();
        let mut decoy = Command::new("sleep").arg("30").spawn().unwrap();
        // A stale process id from an earlier runner is never trusted.
        write_private(&dir.join("pid"), decoy.id().to_string().as_bytes()).unwrap();
        let mut runner = Holder::spawn(&dir, true);
        let stop = [
            "stop",
            "--edge-id",
            "runner-abc",
            "--server-url",
            "https://example.test",
        ];
        assert_eq!(management(root.path(), &stop), 0);
        assert!(!runner.is_alive());
        assert!(
            decoy.try_wait().unwrap().is_none(),
            "stop signalled another process"
        );
        let _ = decoy.kill();
        let _ = decoy.wait();
        assert_eq!(management(root.path(), &stop), 0);
    }

    #[test]
    fn live_runner_with_unreadable_metadata_is_never_reported_absent() {
        let root = tempfile::tempdir().unwrap();
        let workspace = std::fs::canonicalize(".").unwrap();
        let command = connect_args("https://example.test", "secret-token", &workspace);
        let identity = user_runner_identity_with(&command, None).unwrap().unwrap();
        let dir = state_dir(root.path(), &identity);
        ensure_private_dir(&dir).unwrap();
        std::fs::write(dir.join("metadata.json"), b"not json").unwrap();
        let full = [
            "--edge-id",
            "runner-abc",
            "--server-url",
            "https://example.test",
        ];
        let mut runner = Holder::spawn(&dir, true);
        for action in ["status", "stop", "logs"] {
            let mut request = vec![action];
            request.extend(full);
            assert_eq!(management(root.path(), &request), 1, "{action}");
        }
        assert_eq!(
            management(root.path(), &["status", "--edge-id", "runner-abc"]),
            1
        );
        assert_eq!(management(root.path(), &["stop"]), 1);
        assert!(runner.is_alive(), "stop acted on an uninspected runner");
        // Another runner's directory cannot match a fully specified request.
        let other = [
            "status",
            "--edge-id",
            "runner-other",
            "--server-url",
            "https://example.test",
        ];
        assert_eq!(management(root.path(), &other), 0);
        drop(runner);
        wait_until_unlocked(&dir);
        // Without a live owner, unreadable state is not a runner.
        assert_eq!(management(root.path(), &["status"]), 0);
    }

    #[test]
    fn unreadable_lock_is_not_reported_as_stopped() {
        if is_root() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let workspace = workspace.path().canonicalize().unwrap();
        let command = connect_args("https://example.test", "secret-token", &workspace);
        let identity = user_runner_identity_with(&command, None).unwrap().unwrap();
        let dir = state_dir(root.path(), &identity);
        ensure_private_dir(&dir).unwrap();
        write_metadata(&dir, &identity).unwrap();
        let _runner = Holder::spawn(&dir, true);
        let lock_path = dir.join("instance.lock");
        set_mode(&lock_path, 0o000);
        let _restore = RestoreMode(lock_path, 0o600);
        assert_eq!(
            instance_is_locked(&dir).unwrap_err().kind(),
            ErrorKind::PermissionDenied
        );
        for action in ["status", "stop"] {
            assert_eq!(
                management(
                    root.path(),
                    &[
                        action,
                        "--edge-id",
                        "runner-abc",
                        "--server-url",
                        "https://example.test",
                    ],
                ),
                1,
                "{action}"
            );
        }
    }
}

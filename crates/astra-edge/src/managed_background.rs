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
const LOG_TAIL_LINES: usize = 200;
const LOG_TAIL_BYTES: u64 = 256 * 1024;

static INSTANCE_LOCK: Mutex<Option<File>> = Mutex::new(None);
static SHUTDOWN: OnceLock<CancellationToken> = OnceLock::new();
static STARTUP_CANCELLED: AtomicBool = AtomicBool::new(false);
static PHASE_STATE: Mutex<PhaseState> = Mutex::new(PhaseState { epoch: 0 });

struct PhaseState {
    epoch: u64,
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
    let ready_fd = std::env::var(READY_FD_ENV)
        .ok()
        .and_then(|raw| raw.parse::<i32>().ok())
        .filter(|fd| *fd >= 0);
    signal_ready_for_epoch_with(&PHASE_STATE, epoch, dir.as_deref(), ready_fd);
}

fn phase_epoch(state: &Mutex<PhaseState>) -> u64 {
    lock_phase(state).epoch
}

fn note_phase_with(state: &Mutex<PhaseState>, dir: &Path, phase: &str) {
    let mut gate = lock_phase(state);
    if phase != "connected" {
        gate.epoch = gate.epoch.wrapping_add(1);
    }
    let _ = write_private(&dir.join("phase"), phase.as_bytes());
}

fn signal_ready_for_epoch_with(
    state: &Mutex<PhaseState>,
    epoch: u64,
    dir: Option<&Path>,
    ready_fd: Option<i32>,
) {
    let _gate = lock_phase(state);
    if _gate.epoch != epoch {
        return;
    }
    signal_ready_in(dir, ready_fd);
}

fn lock_phase(state: &Mutex<PhaseState>) -> std::sync::MutexGuard<'_, PhaseState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn signal_ready_in(dir: Option<&Path>, ready_fd: Option<i32>) {
    if let Some(dir) = dir {
        let _ = write_private(&dir.join("phase"), b"connected");
    }
    static NOTIFIED: Once = Once::new();
    NOTIFIED.call_once(|| {
        if let Some(fd) = ready_fd {
            notify_parent_ready_fd(fd);
        }
    });
}

fn notify_parent_ready_fd(fd: i32) {
    // The parent created this pipe end and passed its number. This process is
    // the only remaining owner after the parent drops its copy.
    let mut file = unsafe { File::from_raw_fd(fd) };
    let _ = file.write_all(b"1");
    let _ = file.flush();
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
    write_private(&dir.join("pid"), std::process::id().to_string().as_bytes())?;
    note_phase("starting");
    *INSTANCE_LOCK
        .lock()
        .map_err(|_| io::Error::other("runner instance lock poisoned"))? = Some(lock);
    let log = dir.join("edge.log");
    rotate_log(&log)?;
    redirect_stdio(&log)?;
    spawn_log_rotator();
    Ok(())
}

pub fn management_exit_code(args: &[String], root: &Path) -> Option<i32> {
    let request = parse_management(args)?;
    Some(run_management(root, request))
}

pub fn supervise(executable: &Path, args: &[String], root: &Path) -> i32 {
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
    if let Err(error) = ensure_private_dir(&dir) {
        eprintln!("Error: cannot create runner state: {error}");
        return 1;
    }
    let start = match OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join("start.lock"))
    {
        Ok(file) => file,
        Err(error) => {
            eprintln!("Error: cannot lock runner startup: {error}");
            return 1;
        }
    };
    set_mode(&dir.join("start.lock"), 0o600);
    if let Err(error) = start.lock_exclusive() {
        eprintln!("Error: cannot lock runner startup: {error}");
        return 1;
    }
    let _start_guard = Unlock(&start);
    let token = option_value(args, "--token").unwrap_or("");
    let fingerprint = credential_fingerprint(token);
    match instance_is_locked(&dir) {
        Ok(true) => match reconcile_running(&dir, &identity, &fingerprint) {
            Running::Same => return existing_instance(&dir, &identity, executable),
            Running::Conflict | Running::Unreadable => {
                return existing_instance(&dir, &identity, executable);
            }
            Running::Replace => {
                let record = match read_metadata(&dir) {
                    Ok(record) => record,
                    Err(error) => {
                        eprintln!("Error: cannot read runner state: {error}");
                        return 1;
                    }
                };
                eprintln!("Refreshing the background runner credential.");
                if stop_instance_with(&record, false) != 0 {
                    return 1;
                }
                if instance_is_locked(&dir).unwrap_or(true) {
                    eprintln!("Error: runner is still stopping");
                    return 1;
                }
            }
        },
        Ok(false) => {}
        Err(error) => {
            eprintln!("Error: cannot inspect the existing runner: {error}");
            return 1;
        }
    }
    if let Err(error) = write_metadata(&dir, &identity) {
        eprintln!("Error: cannot record runner state: {error}");
        return 1;
    }
    match spawn_and_wait(executable, args, &dir, &identity.workspace) {
        Ok(()) => {
            if let Err(error) = write_private(&fingerprint_path(&dir), fingerprint.as_bytes()) {
                eprintln!("Error: cannot record the runner credential generation: {error}");
                return 1;
            }
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

enum Running {
    Same,
    Replace,
    Conflict,
    Unreadable,
}

fn reconcile_running(dir: &Path, identity: &Identity, fingerprint: &str) -> Running {
    let record = match read_metadata(dir) {
        Ok(record) => record,
        Err(_) => return Running::Unreadable,
    };
    if record.workspace != identity.workspace.display().to_string() {
        return Running::Conflict;
    }
    match read_fingerprint(dir) {
        Some(stored) if stored == fingerprint => Running::Same,
        _ => Running::Replace,
    }
}

fn credential_fingerprint(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn fingerprint_path(dir: &Path) -> PathBuf {
    dir.join("credential.fingerprint")
}

fn read_fingerprint(dir: &Path) -> Option<String> {
    std::fs::read_to_string(fingerprint_path(dir))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn existing_instance(dir: &Path, identity: &Identity, executable: &Path) -> i32 {
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
            "Stop it with: {} stop --edge-id {} --server-url {}",
            shell_quote(&executable.display().to_string()),
            shell_quote(&record.edge_id),
            shell_quote(&record.server_url)
        );
        return 1;
    }
    let phase = phase_label(dir);
    if phase == "connected" {
        eprintln!("Runner is already connected in the background.");
        print_actions(executable, &record.edge_id, &record.server_url);
        return 0;
    }
    eprintln!("Runner is already running in phase {phase}.");
    print_actions(executable, &record.edge_id, &record.server_url);
    1
}

fn spawn_and_wait(
    executable: &Path,
    args: &[String],
    dir: &Path,
    workspace: &Path,
) -> Result<(), String> {
    let startup = StartupCancel::arm().map_err(|error| error.to_string())?;
    let args = command_with_workspace(args, workspace);
    let (read_fd, write_fd) = nix::unistd::pipe().map_err(|error| error.to_string())?;
    let write_raw = as_fd_raw(&write_fd);
    let mut command = Command::new(executable);
    command
        .args(&args)
        .env(MANAGED_ENV, "1")
        .env(STATE_DIR_ENV, dir)
        .env(READY_FD_ENV, write_raw.to_string())
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    unsafe {
        command.pre_exec(move || clear_cloexec(write_raw));
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot start the background runner: {error}"))?;
    drop(write_fd);
    let mut read = File::from(read_fd);
    if let Err(error) = wait_for_ready(&mut child, &mut read, READY_TIMEOUT, startup.flag()) {
        kill_child(&mut child);
        return Err(error);
    }
    Ok(())
}

fn wait_for_ready(
    child: &mut Child,
    read: &mut File,
    timeout: Duration,
    cancelled: &AtomicBool,
) -> Result<(), String> {
    let raw = as_fd_raw(read);
    let _ = nix::fcntl::fcntl(
        unsafe { std::os::fd::BorrowedFd::borrow_raw(raw) },
        nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK),
    );
    let started = Instant::now();
    loop {
        match read_ready_byte(read)? {
            ReadyByte::Ready => return Ok(()),
            ReadyByte::Pending => {}
        }
        if cancelled.load(Ordering::Relaxed) {
            return Err("startup cancelled before the runner was ready".into());
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("cannot observe the runner: {error}"))?
        {
            if read_ready_byte(read)? == ReadyByte::Ready {
                return Ok(());
            }
            return Err(format!("runner exited before it was ready ({status})"));
        }
        if started.elapsed() > timeout {
            return Err("timed out waiting for the runner to authenticate".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReadyByte {
    Ready,
    Pending,
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
        Ok(Self {
            previous_int,
            previous_term,
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
    let records = match list_instances(root) {
        Ok(records) => records,
        Err(error) => {
            eprintln!("Error: cannot read runner state: {error}");
            return 1;
        }
    };
    let selected = match select_instances(
        &records,
        request.edge_id.as_deref(),
        request.server_url.as_deref(),
        request.action,
    ) {
        Ok(selected) => selected,
        Err(code) => return code,
    };
    match request.action {
        Action::Status => {
            for record in selected {
                print_status(record);
            }
            0
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
) -> Result<Vec<&'a Record>, i32> {
    let matched: Vec<&Record> = records
        .iter()
        .filter(|record| edge_id.is_none_or(|id| record.edge_id == id))
        .filter(|record| server_url.is_none_or(|url| same_destination(&record.server_url, url)))
        .collect();
    if matched.is_empty() {
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

fn phase_label(dir: &Path) -> String {
    let recorded = std::fs::read_to_string(dir.join("phase"))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    if instance_is_locked(dir).unwrap_or(false) {
        recorded.unwrap_or_else(|| "starting".to_string())
    } else if recorded.as_deref() == Some("authentication-failure") {
        "authentication-failure".to_string()
    } else {
        "stopped".to_string()
    }
}

fn print_status(record: &Record) {
    let phase = phase_label(&record.dir);
    eprintln!("edge-id: {}", record.edge_id);
    eprintln!("phase: {phase}");
    eprintln!("server: {}", record.server_url);
    eprintln!("workspace: {}", record.workspace);
    eprintln!("log: {}", record.dir.join("edge.log").display());
}

fn stop_instance(record: &Record) -> i32 {
    stop_instance_with(record, true)
}

fn stop_instance_with(record: &Record, announce: bool) -> i32 {
    if !instance_is_locked(&record.dir).unwrap_or(false) {
        if announce {
            eprintln!("Runner is already stopped.");
        }
        return 0;
    }
    let Some(pid) = read_pid(&record.dir) else {
        eprintln!("Error: runner state has no process id");
        return 1;
    };
    if !is_stoppable_pid(record, pid) {
        eprintln!("Error: refusing to signal process {pid}; it is not astra-edge");
        return 1;
    }
    if let Err(error) = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGTERM,
    ) {
        eprintln!("Error: cannot stop the runner: {error}");
        return 1;
    }
    let started = Instant::now();
    while started.elapsed() < STOP_TIMEOUT {
        if !instance_is_locked(&record.dir).unwrap_or(true) {
            if announce {
                eprintln!("Runner stopped.");
            }
            return 0;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    eprintln!("Error: runner is still stopping; admitted work has not finished");
    1
}

fn print_logs(record: &Record) -> i32 {
    let current = record.dir.join("edge.log");
    let previous = record.dir.join("edge.log.1");
    let mut lines = Vec::new();
    if let Ok(mut older) = read_log_tail(&previous, LOG_TAIL_LINES, LOG_TAIL_BYTES) {
        lines.append(&mut older);
    }
    match read_log_tail(&current, LOG_TAIL_LINES, LOG_TAIL_BYTES) {
        Ok(mut newer) => lines.append(&mut newer),
        Err(error) if error.kind() == ErrorKind::NotFound && lines.is_empty() => {
            eprintln!("No logs yet.");
            eprintln!("log: {}", current.display());
            return 0;
        }
        Err(_) => {}
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

fn list_instances(root: &Path) -> io::Result<Vec<Record>> {
    let dir = root.join("edge-runners");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut records = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if let Ok(record) = read_metadata(&entry.path()) {
            records.push(record);
        }
    }
    records.sort_by(|left, right| left.edge_id.cmp(&right.edge_id));
    Ok(records)
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

fn read_pid(dir: &Path) -> Option<u32> {
    std::fs::read_to_string(dir.join("pid"))
        .ok()
        .and_then(|value| value.trim().parse().ok())
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

fn acquire_instance_lock(dir: &Path) -> io::Result<File> {
    let path = dir.join("instance.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)?;
    set_mode(&path, 0o600);
    file.try_lock_exclusive()?;
    Ok(file)
}

fn instance_is_locked(dir: &Path) -> io::Result<bool> {
    let path = dir.join("instance.lock");
    if !path.exists() {
        return Ok(false);
    }
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    match file.try_lock_exclusive() {
        Ok(()) => {
            let _ = file.unlock();
            Ok(false)
        }
        Err(error) if error.kind() == ErrorKind::WouldBlock => Ok(true),
        Err(error) => Err(error),
    }
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
    let tmp = path.with_extension("tmp");
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
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if metadata.len() <= limit {
        return Ok(None);
    }
    let rotated = path.with_extension("log.1");
    let _ = std::fs::remove_file(&rotated);
    std::fs::rename(path, rotated)?;
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    set_mode(path, 0o600);
    Ok(Some(file))
}

fn spawn_log_rotator() {
    let Ok(dir) = std::env::var(STATE_DIR_ENV) else {
        return;
    };
    if dir.is_empty() {
        return;
    }
    let token = shutdown_token();
    std::thread::spawn(move || {
        let log = PathBuf::from(dir).join("edge.log");
        while !token.is_cancelled() {
            std::thread::sleep(Duration::from_secs(1));
            if let Ok(Some(file)) = rotate_log_file(&log, LOG_ROTATE_BYTES) {
                let _ = redirect_log_fd(&file);
            }
        }
    });
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

fn is_stoppable_pid(record: &Record, pid: u32) -> bool {
    if is_astra_edge_pid(pid) {
        return true;
    }
    // Unit tests hold the instance lock with a short-lived helper process.
    // Release builds do not contain this branch.
    #[cfg(test)]
    {
        record.dir.join("test-allow-stop").is_file()
    }
    #[cfg(not(test))]
    {
        let _ = record;
        false
    }
}

fn is_astra_edge_pid(pid: u32) -> bool {
    process_exe(pid)
        .and_then(|path| path.file_name().map(|name| name == "astra-edge"))
        .unwrap_or(false)
}

fn process_exe(pid: u32) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/exe")).ok()
    }
    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            fn proc_pidpath(pid: i32, buffer: *mut i8, buffersize: u32) -> i32;
        }
        let mut buffer = vec![0i8; 4096];
        let length = unsafe { proc_pidpath(pid as i32, buffer.as_mut_ptr(), buffer.len() as u32) };
        if length <= 0 {
            return None;
        }
        let bytes = &buffer[..length as usize];
        let bytes = bytes.iter().map(|byte| *byte as u8).collect::<Vec<_>>();
        Some(PathBuf::from(String::from_utf8_lossy(&bytes).into_owned()))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
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
    fn stopped_runner_keeps_an_authentication_failure() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(phase_label(root.path()), "stopped");
        write_private(&root.path().join("phase"), b"authentication-failure").unwrap();
        assert_eq!(phase_label(root.path()), "authentication-failure");
        {
            let _lock = acquire_instance_lock(root.path()).unwrap();
            assert_eq!(phase_label(root.path()), "authentication-failure");
        }
        assert_eq!(phase_label(root.path()), "authentication-failure");
    }

    #[test]
    fn held_instance_lock_is_visible_and_released_with_the_file() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        assert!(!instance_is_locked(dir).unwrap());
        let lock = acquire_instance_lock(dir).unwrap();
        assert!(instance_is_locked(dir).unwrap());
        drop(lock);
        let mut released = false;
        for _ in 0..50 {
            if !instance_is_locked(dir).unwrap() {
                released = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(released, "instance lock was not released with the file");
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
    fn running_phase_is_not_reported_as_connected() {
        let root = tempfile::tempdir().unwrap();
        let identity = Identity {
            edge_id: "runner-abc".to_string(),
            server_url: "wss://astra.example".to_string(),
            workspace: std::fs::canonicalize(".").unwrap(),
        };
        assert_eq!(
            existing_instance(root.path(), &identity, Path::new("astra-edge")),
            1
        );
        write_metadata(root.path(), &identity).unwrap();
        write_private(&root.path().join("phase"), b"reconnecting").unwrap();
        let _lock = acquire_instance_lock(root.path()).unwrap();
        assert_eq!(
            existing_instance(root.path(), &identity, Path::new("astra-edge")),
            1
        );
        write_private(&root.path().join("phase"), b"connected").unwrap();
        assert_eq!(
            existing_instance(root.path(), &identity, Path::new("astra-edge")),
            0
        );
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
            select_instances(&records, Some("runner-abc"), None, Action::Stop),
            Err(2)
        ));
        let selected = select_instances(
            &records,
            Some("runner-abc"),
            Some("https://b.example/"),
            Action::Logs,
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
        let state = Mutex::new(PhaseState { epoch: 0 });
        let root = tempfile::tempdir().unwrap();
        let (read_fd, write_fd) = nix::unistd::pipe().unwrap();
        let raw = as_fd_raw(&write_fd);
        std::mem::forget(write_fd);
        let epoch = phase_epoch(&state);
        signal_ready_for_epoch_with(&state, epoch, Some(root.path()), Some(raw));
        note_phase_with(&state, root.path(), "reconnecting");
        signal_ready_for_epoch_with(&state, epoch, Some(root.path()), None);
        assert_eq!(
            std::fs::read_to_string(root.path().join("phase")).unwrap(),
            "reconnecting"
        );
        let epoch = phase_epoch(&state);
        signal_ready_for_epoch_with(&state, epoch, Some(root.path()), None);
        assert_eq!(
            std::fs::read_to_string(root.path().join("phase")).unwrap(),
            "connected"
        );
        let mut read = File::from(read_fd);
        let mut buf = [0u8; 8];
        let count = read.read(&mut buf).unwrap();
        assert_eq!(&buf[..count], b"1");
    }

    #[tokio::test]
    async fn delayed_readiness_cannot_overwrite_a_newer_phase() {
        let state = std::sync::Arc::new(Mutex::new(PhaseState { epoch: 0 }));
        let root = tempfile::tempdir().unwrap();
        note_phase_with(&state, root.path(), "starting");
        let epoch = phase_epoch(&state);
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let (done_sender, done_receiver) = tokio::sync::oneshot::channel();
        let task_state = std::sync::Arc::clone(&state);
        let dir = root.path().to_path_buf();
        tokio::spawn(async move {
            if receiver.await.is_ok() {
                signal_ready_for_epoch_with(&task_state, epoch, Some(&dir), None);
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
        let (read_fd, write_fd) = nix::unistd::pipe().unwrap();
        let raw = as_fd_raw(&write_fd);
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("printf 1 >&\"$ASTRA_EDGE_READY_FD\"")
            .env(READY_FD_ENV, raw.to_string());
        unsafe {
            command.pre_exec(move || clear_cloexec(raw));
        }
        let mut child = command.spawn().unwrap();
        drop(write_fd);
        let cancelled = AtomicBool::new(false);
        wait_for_ready(
            &mut child,
            &mut File::from(read_fd),
            Duration::from_secs(2),
            &cancelled,
        )
        .unwrap();
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
    fn replacement_token_restarts_the_connected_runner_without_storing_it() {
        let python = Command::new("python3")
            .arg("--version")
            .output()
            .expect("python3 is required to hold the test runner lock");
        assert!(python.status.success(), "python3 --version failed");
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let workspace = workspace.path().canonicalize().unwrap();
        let argv_log = root.path().join("argv.txt");
        let holder_py = root.path().join("holder.py");
        let holder = root.path().join("holder.sh");
        std::fs::write(
            &holder_py,
            r#"import fcntl, os, time
state = os.environ["ASTRA_EDGE_STATE_DIR"]
lock = open(os.path.join(state, "instance.lock"), "a")
fcntl.flock(lock.fileno(), fcntl.LOCK_EX)
with open(os.path.join(state, "pid"), "w") as pid:
    pid.write(str(os.getpid()))
os.write(int(os.environ["ASTRA_EDGE_READY_FD"]), b"1")
time.sleep(30)
"#,
        )
        .unwrap();
        std::fs::write(
            &holder,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\nexec python3 '{}'\n",
                argv_log.display(),
                holder_py.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&holder, std::fs::Permissions::from_mode(0o755)).unwrap();
        let old = connect_args("https://example.test", "old-token", &workspace);
        let identity = user_runner_identity_with(&old, None).unwrap().unwrap();
        let dir = state_dir(root.path(), &identity);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("test-allow-stop"), b"1").unwrap();
        let _stop = StopHolder(dir.clone());
        assert_eq!(supervise(&holder, &old, root.path()), 0);
        let fresh = connect_args("https://example.test/", "fresh-token", &workspace);
        assert_eq!(supervise(&holder, &fresh, root.path()), 0);
        let argv = std::fs::read_to_string(&argv_log).unwrap();
        assert!(argv.contains("old-token"), "first runner did not start");
        assert!(
            argv.contains("fresh-token"),
            "replacement token was not given to the new runner"
        );
        let metadata = std::fs::read_to_string(dir.join("metadata.json")).unwrap();
        assert!(!metadata.contains("old-token"));
        assert!(!metadata.contains("fresh-token"));
        let fingerprint = std::fs::read_to_string(dir.join("credential.fingerprint")).unwrap();
        assert!(!fingerprint.contains("fresh-token"));
        assert_eq!(fingerprint.trim(), credential_fingerprint("fresh-token"));
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

    struct StopHolder(PathBuf);

    impl Drop for StopHolder {
        fn drop(&mut self) {
            let Ok(pid) = std::fs::read_to_string(self.0.join("pid")) else {
                return;
            };
            let Ok(pid) = pid.trim().parse::<i32>() else {
                return;
            };
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
}

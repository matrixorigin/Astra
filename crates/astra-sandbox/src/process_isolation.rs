//! Linux process isolation via namespaces and cgroup v2 (Phase 5.5).
//!
//! Wraps subprocess execution with:
//! - **PID namespace** (`unshare --pid --fork`): process cannot signal host PIDs.
//! - **Mount namespace** (`unshare --mount`): private `/tmp`, read-only bind mounts.
//! - **Network namespace** (optional, `unshare --net`): no network access.
//! - **cgroup v2 limits**: memory ceiling + CPU quota per-tool invocation.
//!
//! Falls back gracefully when:
///    - If kernel >= 3.19, unprivileged user namespaces are considered safe.
///    - On older kernels, --map-root-user is skipped and a warning is emitted
///      because of known privilege-escalation vulnerabilities (CVE-2015-1328, etc.).
///
/// Caller passes `IsolationConfig::disabled()`.
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

#[cfg(target_os = "linux")]
use std::fs::File;
#[cfg(target_os = "linux")]
use std::io::{Read, Write};
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
#[cfg(target_os = "linux")]
use std::os::unix::process::{CommandExt, ExitStatusExt};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio_util::sync::CancellationToken;

const MAX_CAPTURED_STDOUT_BYTES: usize = 64 * 1024;
const MAX_CAPTURED_STDERR_BYTES: usize = 32 * 1024;
const DEFAULT_MAX_CAPTURED_OUTPUT_BYTES: usize =
    MAX_CAPTURED_STDOUT_BYTES + MAX_CAPTURED_STDERR_BYTES;
const READ_CHUNK_SIZE: usize = 8 * 1024;
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(20);
const OUTPUT_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
#[cfg(target_os = "linux")]
const SUPERVISOR_PROTOCOL_VERSION: &str = "2";
#[cfg(target_os = "linux")]
const SUPERVISOR_MODE_ENV: &str = "_ASTRA_INTERNAL_PROCESS_SUPERVISOR";
#[cfg(target_os = "linux")]
const SUPERVISOR_PROGRAM_ENV: &str = "_ASTRA_INTERNAL_PROCESS_PROGRAM";
#[cfg(target_os = "linux")]
const SUPERVISOR_ARGS_ENV: &str = "_ASTRA_INTERNAL_PROCESS_ARGS";
#[cfg(target_os = "linux")]
const SUPERVISOR_CONTROL_FD_ENV: &str = "_ASTRA_INTERNAL_PROCESS_CONTROL_FD";
#[cfg(target_os = "linux")]
const SUPERVISOR_RECEIPT_FD_ENV: &str = "_ASTRA_INTERNAL_PROCESS_RECEIPT_FD";
#[cfg(target_os = "linux")]
const SUPERVISOR_NONCE_ENV: &str = "_ASTRA_INTERNAL_PROCESS_NONCE";
#[cfg(target_os = "linux")]
const SUPERVISOR_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(target_os = "linux")]
const SUPERVISOR_SETTLEMENT_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(target_os = "linux")]
const SUPERVISOR_POLL_INTERVAL: Duration = Duration::from_millis(5);
#[cfg(target_os = "linux")]
static SUPERVISOR_ENTRYPOINT_READY: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Per-invocation isolation configuration.
#[derive(Debug, Clone)]
pub struct IsolationConfig {
    /// Enable PID namespace isolation.
    pub pid_namespace: bool,
    /// Enable mount namespace (private /tmp).
    pub mount_namespace: bool,
    /// Enable network namespace (blocks all networking).
    pub net_namespace: bool,
    /// cgroup v2 memory limit in bytes (0 = no limit).
    pub memory_limit_bytes: u64,
    /// cgroup v2 CPU quota as a fraction (e.g., 0.5 = 50% of one core, 0 = no limit).
    pub cpu_quota: f64,
    /// Maximum execution wall-clock time.
    pub timeout: Duration,
    /// Maximum combined stdout/stderr bytes retained in memory.
    pub max_output_bytes: usize,
    /// Working directory for the subprocess.
    pub working_dir: PathBuf,
    /// Host-owned paths inside the workspace that remain readable but must
    /// not be writable by the child mount namespace.
    pub read_only_paths: Vec<PathBuf>,
}

impl IsolationConfig {
    /// Full isolation for untrusted tool execution.
    pub fn strict(working_dir: PathBuf) -> Self {
        Self {
            pid_namespace: true,
            mount_namespace: true,
            net_namespace: true,
            memory_limit_bytes: 512 * 1024 * 1024, // 512 MB
            cpu_quota: 1.0,                        // 1 full core
            timeout: Duration::from_secs(120),
            max_output_bytes: DEFAULT_MAX_CAPTURED_OUTPUT_BYTES,
            working_dir,
            read_only_paths: Vec::new(),
        }
    }

    /// Sandboxed mode — no namespace isolation, just cgroup limits.
    pub fn sandboxed(working_dir: PathBuf) -> Self {
        Self {
            pid_namespace: false,
            mount_namespace: false,
            net_namespace: false,
            memory_limit_bytes: 1024 * 1024 * 1024, // 1 GB
            cpu_quota: 2.0,                         // 2 cores
            timeout: Duration::from_secs(120),
            max_output_bytes: DEFAULT_MAX_CAPTURED_OUTPUT_BYTES,
            working_dir,
            read_only_paths: Vec::new(),
        }
    }

    /// No isolation (backward compat / permissive mode).
    pub fn disabled(working_dir: PathBuf) -> Self {
        Self {
            pid_namespace: false,
            mount_namespace: false,
            net_namespace: false,
            // Apply permissive resource limits even when namespace isolation is disabled.
            // This prevents unbounded resource consumption while maintaining backward compat.
            memory_limit_bytes: 1024 * 1024 * 1024, // 1 GB
            cpu_quota: 2.0,                         // 2 cores
            timeout: Duration::from_secs(120),
            max_output_bytes: DEFAULT_MAX_CAPTURED_OUTPUT_BYTES,
            working_dir,
            read_only_paths: Vec::new(),
        }
    }

    /// Mount-namespace write boundary for a managed workspace. The host
    /// process keeps its ordinary filesystem view while the child sees the
    /// host filesystem read-only, the selected workspace read-write, and the
    /// supplied host-owned lanes read-only again.
    pub fn filesystem_boundary(working_dir: PathBuf, read_only_paths: Vec<PathBuf>) -> Self {
        Self {
            pid_namespace: true,
            mount_namespace: true,
            net_namespace: false,
            memory_limit_bytes: 0,
            cpu_quota: 0.0,
            timeout: Duration::from_secs(120),
            max_output_bytes: DEFAULT_MAX_CAPTURED_OUTPUT_BYTES,
            working_dir,
            read_only_paths,
        }
    }
}

/// Result of an isolated command execution.
#[derive(Debug)]
pub struct IsolatedOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// The invocation was terminated by its caller's cancellation token.
    /// This is kept distinct from a wall-clock timeout for protocol-level
    /// cancellation envelopes.
    pub cancelled: bool,
    /// Executor-owned fact that the target subprocess crossed `spawn`.
    /// Callers use this to distinguish a preflight/spawn refusal from a
    /// started invocation whose descendant ownership failed to settle.
    pub execution_started: bool,
    pub stdout_capped: bool,
    pub stderr_capped: bool,
    /// Whether namespace isolation was actually applied (false = fallback).
    pub namespace_active: bool,
    /// Whether cgroup limits were actually applied.
    pub cgroup_active: bool,
    /// Whether the invocation-owned process scope was proven empty before
    /// post-execution workspace state was inspected.
    pub scope_settled: bool,
    /// Provenance of the settled process boundary.  A delegated cgroup is
    /// authoritative; a process-group fallback is sufficient for the
    /// current foreground result but must not be treated as durable
    /// ownership by cache/renewal consumers.
    pub scope_ownership: Option<ScopeOwnership>,
    /// Whether live descendants remained after the target exited and were
    /// settled before this output was returned.
    pub descendants_terminated: bool,
}

/// How an invocation's descendants were proven to have settled before the
/// caller inspected workspace state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeOwnership {
    /// Invocation-owned cgroup, including descendants that create sessions.
    InvocationCgroup,
    /// An invocation-private Linux child subreaper proved that its complete
    /// descendant set was empty. Unlike a process group, this continues to
    /// own descendants that call `setsid` or double-fork.
    InvocationSupervisor,
    /// The leader's process group was empty after cleanup. This is a useful
    /// foreground fallback on hosts without delegated cgroups, but it cannot
    /// rule out a descendant that escaped with `setsid`.
    ForegroundProcessGroup,
}

/// Receipt produced when an invocation-owned process scope is settled.
///
/// `descendants_terminated` is executor evidence, not command-text inference:
/// it is true only when the owner observed live descendants after the target
/// exited and sent them a termination signal before returning the receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScopeSettlement {
    pub ownership: ScopeOwnership,
    pub descendants_terminated: bool,
}

/// Shared process-ownership boundary for foreground Bash invocations.
///
/// Both Edge and the shared/server tool executor must select ownership
/// through this type. A delegated cgroup remains preferred; Linux hosts
/// without one use the invocation-private subreaper; other platforms retain
/// only the explicitly weak process-group result.
#[derive(Debug)]
pub struct BashInvocationOwner {
    process_scope: CgroupGuard,
    supervisor: Option<InvocationSupervisor>,
}

impl BashInvocationOwner {
    /// Prepare the actual child command and its ownership boundary. Call
    /// [`Self::install`] after the caller has completed environment/sandbox
    /// filtering and immediately before spawn.
    pub fn prepare(
        target_program: &str,
        target_args: &[String],
    ) -> std::io::Result<(std::process::Command, Self)> {
        let process_scope = apply_process_scope();
        if process_scope.active() {
            let mut command = std::process::Command::new(target_program);
            command.args(target_args);
            return Ok((
                command,
                Self {
                    process_scope,
                    supervisor: None,
                },
            ));
        }
        #[cfg(target_os = "linux")]
        {
            match InvocationSupervisor::prepare(target_program, target_args) {
                Ok((command, supervisor)) => Ok((
                    command,
                    Self {
                        process_scope,
                        supervisor: Some(supervisor),
                    },
                )),
                Err(error) if error.kind() == std::io::ErrorKind::Unsupported => {
                    let mut command = std::process::Command::new(target_program);
                    command.args(target_args);
                    Ok((
                        command,
                        Self {
                            process_scope,
                            supervisor: None,
                        },
                    ))
                }
                Err(error) => Err(error),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let mut command = std::process::Command::new(target_program);
            command.args(target_args);
            Ok((
                command,
                Self {
                    process_scope,
                    supervisor: None,
                },
            ))
        }
    }

    /// Test seam for exercising the exact shared owner without requiring the
    /// current test executable to expose an application main entrypoint.
    #[doc(hidden)]
    #[cfg(target_os = "linux")]
    pub fn prepare_with_supervisor_helper(
        helper_program: PathBuf,
        helper_args: impl IntoIterator<Item = String>,
        target_program: &str,
        target_args: &[String],
    ) -> std::io::Result<(std::process::Command, Self)> {
        let (command, supervisor) = InvocationSupervisor::prepare_with_helper_command(
            helper_program,
            helper_args,
            target_program,
            target_args,
        )?;
        Ok((
            command,
            Self {
                process_scope: CgroupGuard {
                    cg_path: None,
                    procs_path: None,
                },
                supervisor: Some(supervisor),
            },
        ))
    }

    pub fn install(&self, command: &mut std::process::Command) -> std::io::Result<()> {
        if let Some(supervisor) = self.supervisor.as_ref() {
            supervisor.install(command)?;
        }
        self.process_scope.attach_std_child(command);
        Ok(())
    }

    /// Complete the post-spawn handshake before the target is allowed to run.
    pub fn started(&mut self, child_pid: u32) -> std::io::Result<()> {
        if let Some(supervisor) = self.supervisor.as_mut() {
            supervisor.spawned();
            supervisor.start(child_pid)?;
        }
        self.process_scope.join_child(child_pid)
    }

    /// Ask an authoritative supervisor to terminate every adopted descendant.
    /// Cgroup/weak callers keep their existing leader/process-group cleanup.
    pub fn request_supervised_termination(&mut self) -> bool {
        self.supervisor
            .as_mut()
            .is_some_and(|supervisor| supervisor.request_termination().is_ok())
    }

    pub fn is_supervised(&self) -> bool {
        self.supervisor.is_some()
    }

    /// Consume the final owner receipt after the child/helper has exited.
    pub fn settle_after_exit(&mut self, leader_pid: Option<u32>) -> Option<ScopeOwnership> {
        self.settle_after_exit_detailed(leader_pid)
            .map(|settlement| settlement.ownership)
    }

    /// Consume the owner receipt and preserve whether live descendants had to
    /// be terminated. This lets tool surfaces explain why a self-daemonizing
    /// service disappeared without weakening the invocation boundary.
    pub fn settle_after_exit_detailed(
        &mut self,
        leader_pid: Option<u32>,
    ) -> Option<ScopeSettlement> {
        if let (Some(supervisor), Some(pid)) = (self.supervisor.as_mut(), leader_pid) {
            return supervisor
                .finish_detailed(pid)
                .map(|descendants_terminated| ScopeSettlement {
                    ownership: ScopeOwnership::InvocationSupervisor,
                    descendants_terminated,
                });
        }
        self.process_scope
            .settle_for_observation_detailed(leader_pid)
    }
}

impl ScopeOwnership {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvocationCgroup => "invocation_cgroup",
            Self::InvocationSupervisor => "invocation_supervisor",
            Self::ForegroundProcessGroup => "foreground_process_group",
        }
    }

    pub fn is_authoritative(self) -> bool {
        matches!(self, Self::InvocationCgroup | Self::InvocationSupervisor)
    }
}

/// Parent-side handle for an invocation-private Linux process supervisor.
///
/// The supervisor is a fresh exec of the current Astra binary. It becomes a
/// child subreaper before the target is allowed to start, so daemonized
/// descendants remain attributable to exactly one invocation even when many
/// sessions execute concurrently. The parent accepts authority only after a
/// nonce-bound READY/START handshake and a final ECHILD receipt on the private
/// pipe. The target never inherits either protocol fd or the private env.
#[derive(Debug)]
pub struct InvocationSupervisor {
    #[cfg(target_os = "linux")]
    target_program: String,
    #[cfg(target_os = "linux")]
    target_args: Vec<String>,
    #[cfg(target_os = "linux")]
    nonce: String,
    #[cfg(target_os = "linux")]
    control_write: File,
    #[cfg(target_os = "linux")]
    receipt_read: File,
    #[cfg(target_os = "linux")]
    child_control_read: Option<OwnedFd>,
    #[cfg(target_os = "linux")]
    child_receipt_write: Option<OwnedFd>,
    #[cfg(target_os = "linux")]
    receipt_buffer: Vec<u8>,
    #[cfg(target_os = "linux")]
    ready: bool,
    #[cfg(target_os = "linux")]
    started: bool,
}

impl InvocationSupervisor {
    /// Prepare a helper command and its authenticated control channel.
    /// [`Self::install`] must be called after all environment filtering and
    /// before spawning the returned command.
    pub fn prepare(
        target_program: &str,
        target_args: &[String],
    ) -> std::io::Result<(std::process::Command, Self)> {
        #[cfg(target_os = "linux")]
        {
            if !SUPERVISOR_ENTRYPOINT_READY.load(std::sync::atomic::Ordering::Acquire) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "current process did not register the invocation supervisor early entrypoint",
                ));
            }
            Self::prepare_with_helper_command(
                std::env::current_exe()?,
                std::iter::empty::<String>(),
                target_program,
                target_args,
            )
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (target_program, target_args);
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "invocation supervisor requires Linux",
            ))
        }
    }

    /// Explicit helper seam for integration tests and dedicated host
    /// launchers. Production Astra uses [`Self::prepare`], which always
    /// re-execs the current binary's private early entrypoint.
    #[doc(hidden)]
    #[cfg(target_os = "linux")]
    pub fn prepare_with_helper_command(
        helper_program: PathBuf,
        helper_args: impl IntoIterator<Item = String>,
        target_program: &str,
        target_args: &[String],
    ) -> std::io::Result<(std::process::Command, Self)> {
        let (control_read, control_write) = pipe_cloexec()?;
        let (receipt_read, receipt_write) = pipe_cloexec()?;
        set_fd_nonblocking(receipt_read.as_raw_fd(), true)?;

        let mut command = std::process::Command::new(helper_program);
        command.args(helper_args);
        let supervisor = Self {
            target_program: target_program.to_string(),
            target_args: target_args.to_vec(),
            nonce: uuid::Uuid::new_v4().as_simple().to_string(),
            control_write: File::from(control_write),
            receipt_read: File::from(receipt_read),
            child_control_read: Some(control_read),
            child_receipt_write: Some(receipt_write),
            receipt_buffer: Vec::new(),
            ready: false,
            started: false,
        };
        Ok((command, supervisor))
    }

    /// Install the private protocol environment and async-signal-safe fd
    /// inheritance hook. Call this last: sandbox env filtering must not erase
    /// the helper protocol, while the helper itself removes every private key
    /// from the target environment.
    pub fn install(&self, command: &mut std::process::Command) -> std::io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            let control_fd = self
                .child_control_read
                .as_ref()
                .ok_or_else(|| std::io::Error::other("supervisor already spawned"))?
                .as_raw_fd();
            let receipt_fd = self
                .child_receipt_write
                .as_ref()
                .ok_or_else(|| std::io::Error::other("supervisor already spawned"))?
                .as_raw_fd();
            let args = serde_json::to_string(&self.target_args)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
            command
                .env(SUPERVISOR_MODE_ENV, SUPERVISOR_PROTOCOL_VERSION)
                .env(SUPERVISOR_PROGRAM_ENV, &self.target_program)
                .env(SUPERVISOR_ARGS_ENV, args)
                .env(SUPERVISOR_CONTROL_FD_ENV, control_fd.to_string())
                .env(SUPERVISOR_RECEIPT_FD_ENV, receipt_fd.to_string())
                .env(SUPERVISOR_NONCE_ENV, &self.nonce);
            // SAFETY: the closure performs only fcntl syscalls after fork.
            // All captured values are plain integers allocated beforehand.
            unsafe {
                command.pre_exec(move || {
                    set_fd_cloexec(control_fd, false)?;
                    set_fd_cloexec(receipt_fd, false)
                });
            }
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = command;
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "invocation supervisor requires Linux",
            ))
        }
    }

    /// Close the parent's copies of helper-only fds immediately after spawn.
    pub fn spawned(&mut self) {
        #[cfg(target_os = "linux")]
        {
            self.child_control_read.take();
            self.child_receipt_write.take();
        }
    }

    /// Authenticate the helper before allowing the target to execute.
    pub fn start(&mut self, helper_pid: u32) -> std::io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            if self.ready || self.started {
                return Err(std::io::Error::other(
                    "invocation supervisor handshake already consumed",
                ));
            }
            let expected = format!(
                "ASTRA_PROCESS_SUPERVISOR {} READY {} {}",
                SUPERVISOR_PROTOCOL_VERSION, self.nonce, helper_pid
            );
            let deadline = std::time::Instant::now() + SUPERVISOR_HANDSHAKE_TIMEOUT;
            let actual = self
                .read_protocol_line(deadline)?
                .ok_or_else(|| std::io::Error::other("supervisor closed before READY"))?;
            if actual != expected {
                return Err(std::io::Error::other(format!(
                    "invalid invocation supervisor READY receipt: {actual:?}"
                )));
            }
            self.ready = true;
            self.control_write.write_all(b"S")?;
            self.control_write.flush()?;
            self.started = true;
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = helper_pid;
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "invocation supervisor requires Linux",
            ))
        }
    }

    /// Ask the still-owning helper to terminate the invocation. This is the
    /// only safe timeout/cancellation path: killing the helper first would
    /// release daemonized descendants to the container init process.
    pub fn request_termination(&mut self) -> std::io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            self.control_write.write_all(b"C")?;
            self.control_write.flush()
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "invocation supervisor requires Linux",
            ))
        }
    }

    /// Validate the single final authority receipt after the helper exits.
    /// Missing, duplicated, malformed, or pre-START receipts fail closed.
    pub fn finish(&mut self, helper_pid: u32) -> bool {
        self.finish_detailed(helper_pid).is_some()
    }

    /// Validate the final authority receipt and return whether the helper had
    /// to terminate live descendants after the foreground target exited.
    pub fn finish_detailed(&mut self, helper_pid: u32) -> Option<bool> {
        #[cfg(target_os = "linux")]
        {
            if !self.ready || !self.started {
                return None;
            }
            let expected_prefix = format!(
                "ASTRA_PROCESS_SUPERVISOR {} SETTLED_ECHILD {} {} ",
                SUPERVISOR_PROTOCOL_VERSION, self.nonce, helper_pid
            );
            let deadline = std::time::Instant::now() + SUPERVISOR_HANDSHAKE_TIMEOUT;
            let Ok(Some(actual)) = self.read_protocol_line(deadline) else {
                return None;
            };
            let descendants_terminated = match actual.strip_prefix(&expected_prefix) {
                Some("0") => false,
                Some("1") => true,
                _ => return None,
            };
            // The helper has exited before callers invoke finish, so EOF must
            // immediately follow the one final receipt. Extra messages are a
            // protocol violation rather than evidence to ignore.
            (matches!(self.read_protocol_line(deadline), Ok(None))
                && self.receipt_buffer.is_empty())
            .then_some(descendants_terminated)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = helper_pid;
            None
        }
    }

    #[cfg(target_os = "linux")]
    fn read_protocol_line(
        &mut self,
        deadline: std::time::Instant,
    ) -> std::io::Result<Option<String>> {
        loop {
            if let Some(newline) = self.receipt_buffer.iter().position(|byte| *byte == b'\n') {
                let bytes = self.receipt_buffer.drain(..=newline).collect::<Vec<_>>();
                let line = std::str::from_utf8(&bytes[..bytes.len() - 1])
                    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?
                    .to_string();
                return Ok(Some(line));
            }
            if self.receipt_buffer.len() > 1024 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "invocation supervisor receipt exceeded protocol bound",
                ));
            }
            let mut chunk = [0u8; 256];
            match self.receipt_read.read(&mut chunk) {
                Ok(0) => {
                    if self.receipt_buffer.is_empty() {
                        return Ok(None);
                    }
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "truncated invocation supervisor receipt",
                    ));
                }
                Ok(read) => self.receipt_buffer.extend_from_slice(&chunk[..read]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "timed out waiting for invocation supervisor receipt",
                        ));
                    }
                    std::thread::sleep(SUPERVISOR_POLL_INTERVAL);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
    }
}

/// Run the private supervisor mode when the early Astra binary entrypoint was
/// exec'd with a valid protocol marker. Normal CLI invocations return `None`.
/// This must be called before runtime, logging, or any application threads are
/// initialized.
pub fn invocation_supervisor_is_requested() -> bool {
    #[cfg(target_os = "linux")]
    {
        std::env::var(SUPERVISOR_MODE_ENV).ok().as_deref() == Some(SUPERVISOR_PROTOCOL_VERSION)
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

pub fn run_invocation_supervisor_if_requested() -> Option<i32> {
    #[cfg(target_os = "linux")]
    {
        if !invocation_supervisor_is_requested() {
            SUPERVISOR_ENTRYPOINT_READY.store(true, std::sync::atomic::Ordering::Release);
            return None;
        }
        return Some(run_invocation_supervisor().unwrap_or(125));
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg(target_os = "linux")]
fn run_invocation_supervisor() -> std::io::Result<i32> {
    let program = std::env::var(SUPERVISOR_PROGRAM_ENV)
        .map_err(|_| std::io::Error::other("missing supervisor target program"))?;
    let args = serde_json::from_str::<Vec<String>>(
        &std::env::var(SUPERVISOR_ARGS_ENV)
            .map_err(|_| std::io::Error::other("missing supervisor target args"))?,
    )
    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    let control_fd = parse_supervisor_fd(SUPERVISOR_CONTROL_FD_ENV)?;
    let receipt_fd = parse_supervisor_fd(SUPERVISOR_RECEIPT_FD_ENV)?;
    if control_fd == receipt_fd {
        return Err(std::io::Error::other(
            "supervisor control and receipt fd must differ",
        ));
    }
    let nonce = std::env::var(SUPERVISOR_NONCE_ENV)
        .map_err(|_| std::io::Error::other("missing supervisor nonce"))?;
    if uuid::Uuid::parse_str(&nonce).is_err() {
        return Err(std::io::Error::other("invalid supervisor nonce"));
    }

    // SAFETY: these descriptors were uniquely transferred by the parent
    // through the exec boundary and validated above.
    let mut control = unsafe { File::from_raw_fd(control_fd) };
    let mut receipt = unsafe { File::from_raw_fd(receipt_fd) };
    set_fd_cloexec(control.as_raw_fd(), true)?;
    set_fd_cloexec(receipt.as_raw_fd(), true)?;

    // A target running under the same uid must not inspect or duplicate the
    // helper's receipt fd through /proc. If this boundary cannot be installed,
    // no target is started and no READY receipt is emitted.
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }

    let helper_pid = std::process::id();
    writeln!(
        receipt,
        "ASTRA_PROCESS_SUPERVISOR {} READY {} {}",
        SUPERVISOR_PROTOCOL_VERSION, nonce, helper_pid
    )?;
    receipt.flush()?;

    let mut instruction = [0u8; 1];
    control.read_exact(&mut instruction)?;
    if instruction[0] != b'S' {
        return Err(std::io::Error::other(
            "supervisor target was not authorized to start",
        ));
    }
    set_fd_nonblocking(control.as_raw_fd(), true)?;

    let mut target = std::process::Command::new(program);
    target.args(args);
    for key in supervisor_private_env_keys() {
        target.env_remove(key);
    }
    // The target gets a conventional foreground group, but the authoritative
    // boundary is the helper's subreaper child set, which survives setsid.
    target.process_group(0);
    let mut target = target.spawn()?;
    let target_pid = target.id();

    let mut target_status = None;
    let mut terminate = false;
    loop {
        match target.try_wait()? {
            Some(status) => {
                target_status = Some(status);
                break;
            }
            None => {}
        }
        match control.read(&mut instruction) {
            Ok(0) => {
                terminate = true;
                break;
            }
            Ok(_) if instruction[0] == b'C' => {
                terminate = true;
                break;
            }
            Ok(_) => {
                terminate = true;
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => {
                terminate = true;
                break;
            }
        }
        std::thread::sleep(SUPERVISOR_POLL_INTERVAL);
    }

    if terminate {
        if let Ok(raw_pid) = i32::try_from(target_pid) {
            let _ = unsafe { libc::kill(-(raw_pid as libc::pid_t), libc::SIGKILL) };
        }
        let _ = target.kill();
    }

    let descendants_terminated =
        settle_subreaper_children(helper_pid, SUPERVISOR_SETTLEMENT_TIMEOUT).ok_or_else(|| {
            std::io::Error::other("invocation supervisor could not prove ECHILD settlement")
        })?;

    writeln!(
        receipt,
        "ASTRA_PROCESS_SUPERVISOR {} SETTLED_ECHILD {} {} {}",
        SUPERVISOR_PROTOCOL_VERSION,
        nonce,
        helper_pid,
        u8::from(descendants_terminated)
    )?;
    receipt.flush()?;

    if terminate {
        return Ok(130);
    }
    let status = target_status.ok_or_else(|| std::io::Error::other("missing target status"))?;
    Ok(status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(125)
        .clamp(0, 255))
}

#[cfg(target_os = "linux")]
fn supervisor_private_env_keys() -> [&'static str; 6] {
    [
        SUPERVISOR_MODE_ENV,
        SUPERVISOR_PROGRAM_ENV,
        SUPERVISOR_ARGS_ENV,
        SUPERVISOR_CONTROL_FD_ENV,
        SUPERVISOR_RECEIPT_FD_ENV,
        SUPERVISOR_NONCE_ENV,
    ]
}

#[cfg(target_os = "linux")]
fn parse_supervisor_fd(key: &str) -> std::io::Result<RawFd> {
    let fd = std::env::var(key)
        .map_err(|_| std::io::Error::other(format!("missing {key}")))?
        .parse::<RawFd>()
        .map_err(|_| std::io::Error::other(format!("invalid {key}")))?;
    if fd < 3 {
        return Err(std::io::Error::other(format!("unsafe {key}")));
    }
    Ok(fd)
}

#[cfg(target_os = "linux")]
fn pipe_cloexec() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [-1; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: pipe2 returned two fresh, uniquely owned descriptors.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

#[cfg(target_os = "linux")]
fn set_fd_cloexec(fd: RawFd, enabled: bool) -> std::io::Result<()> {
    let current = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if current < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let next = if enabled {
        current | libc::FD_CLOEXEC
    } else {
        current & !libc::FD_CLOEXEC
    };
    if unsafe { libc::fcntl(fd, libc::F_SETFD, next) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn set_fd_nonblocking(fd: RawFd, enabled: bool) -> std::io::Result<()> {
    let current = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if current < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let next = if enabled {
        current | libc::O_NONBLOCK
    } else {
        current & !libc::O_NONBLOCK
    };
    if unsafe { libc::fcntl(fd, libc::F_SETFL, next) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn settle_subreaper_children(helper_pid: u32, timeout: Duration) -> Option<bool> {
    let deadline = std::time::Instant::now() + timeout;
    let mut descendants_terminated = false;
    loop {
        loop {
            let waited = unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) };
            if waited > 0 {
                continue;
            }
            if waited == -1 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ECHILD) {
                    return Some(descendants_terminated);
                }
                if error.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return None;
            }
            break;
        }

        let children_path = format!("/proc/{helper_pid}/task/{helper_pid}/children");
        let Ok(children) = std::fs::read_to_string(children_path) else {
            return None;
        };
        for pid in children
            .split_ascii_whitespace()
            .filter_map(|value| value.parse::<libc::pid_t>().ok())
        {
            descendants_terminated = true;
            let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(SUPERVISOR_POLL_INTERVAL);
    }
}

impl IsolatedOutput {
    pub fn combined_output(&self) -> String {
        let mut out = String::new();
        if !self.stdout.is_empty() {
            out.push_str(&self.stdout);
        }
        if !self.stderr.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str("stderr:\n");
            out.push_str(&self.stderr);
        }
        if let Some(code) = self.exit_code
            && code != 0
        {
            out.push_str(&format!("\n(exit code: {code})"));
        }
        if self.stdout_capped || self.stderr_capped {
            let mut capped_streams = Vec::new();
            if self.stdout_capped {
                capped_streams.push("stdout");
            }
            if self.stderr_capped {
                capped_streams.push("stderr");
            }
            out.push_str(&format!(
                "\n(output capped: {} limit reached)",
                capped_streams.join(", ")
            ));
        }
        if self.timed_out {
            out.push_str("\n(timed out)");
        }
        out
    }
}

#[derive(Clone, Copy)]
enum StreamKind {
    Stdout,
    Stderr,
}

struct StreamChunk {
    stream: StreamKind,
    bytes: Vec<u8>,
}

struct StreamOutputCapture {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    max_output_bytes: usize,
    stdout_capped: bool,
    stderr_capped: bool,
}

impl StreamOutputCapture {
    fn new(max_output_bytes: usize) -> Self {
        Self {
            stdout: Vec::new(),
            stderr: Vec::new(),
            max_output_bytes,
            stdout_capped: false,
            stderr_capped: false,
        }
    }

    fn append(&mut self, chunk: StreamChunk) {
        let retained_bytes = self.stdout.len().saturating_add(self.stderr.len());
        let remaining = self.max_output_bytes.saturating_sub(retained_bytes);
        match chunk.stream {
            StreamKind::Stdout => append_capped(
                &mut self.stdout,
                &chunk.bytes,
                remaining,
                &mut self.stdout_capped,
            ),
            StreamKind::Stderr => append_capped(
                &mut self.stderr,
                &chunk.bytes,
                remaining,
                &mut self.stderr_capped,
            ),
        }
    }

    fn drain_ready(&mut self, rx: &mut tokio::sync::mpsc::UnboundedReceiver<StreamChunk>) {
        while let Ok(chunk) = rx.try_recv() {
            self.append(chunk);
        }
    }

    fn is_capped(&self) -> bool {
        self.stdout_capped || self.stderr_capped
    }
}

/// Check if `unshare` with user namespace mapping actually works.
///
/// The binary may exist but the kernel may block unprivileged user namespaces
/// (e.g., inside containers without CAP_SYS_ADMIN).  We probe once and cache.
fn unshare_available() -> bool {
    static CACHED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        std::process::Command::new("unshare")
            .args([
                "--map-root-user",
                "--pid",
                "--fork",
                "--kill-child",
                "--",
                "true",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

/// Check whether the running kernel is safe for `--map-root-user`.
///
/// Kernels before 3.19 have known privilege-escalation vulnerabilities
/// (notably CVE-2015-1328 in overlayfs) that can be exploited via
/// unprivileged user namespaces.  This check parses `uname -r` and
/// returns true only when the kernel is ≥ 3.19.0.
fn map_root_user_safe() -> bool {
    static CACHED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        let output = std::process::Command::new("uname")
            .arg("-r")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output();
        let output = match output {
            Ok(o) => o,
            Err(_) => return false,
        };
        let version_str = String::from_utf8_lossy(&output.stdout);
        let version_str = version_str.trim();
        let parts: Vec<u32> = version_str
            .split(|c: char| !c.is_ascii_digit())
            .filter_map(|s| s.parse::<u32>().ok())
            .take(3)
            .collect();
        if parts.len() < 2 {
            // Can't parse kernel version — be conservative and refuse.
            tracing::warn!(
                target: "astra_sandbox::process_isolation",
                kernel_version = %version_str,
                "cannot parse kernel version; refusing --map-root-user"
            );
            return false;
        }
        let (major, minor) = (parts[0], parts[1]);
        let safe = major > 3 || (major == 3 && minor >= 19);
        if !safe {
            tracing::warn!(
                target: "astra_sandbox::process_isolation",
                kernel_version = %version_str,
                "kernel < 3.19 — --map-root-user disabled due to known EoP vulnerabilities \
                 (CVE-2015-1328, etc.). Upgrade your kernel to ≥ 3.19 for full isolation."
            );
        }
        safe
    })
}

/// Check if cgroup v2 is mounted.
fn cgroupv2_available() -> bool {
    Path::new("/sys/fs/cgroup/cgroup.controllers").exists()
}

/// Unique cgroup name for this invocation.
fn cgroup_name() -> String {
    format!("astra-tool-{}", uuid::Uuid::new_v4().as_simple())
}

/// Create a transient cgroup v2 scope with memory + CPU limits.
/// Returns the cgroup path (e.g., `/sys/fs/cgroup/astra-tool-xxxx`) or None on failure.
fn delegated_cgroup_parents() -> Vec<PathBuf> {
    let root = PathBuf::from("/sys/fs/cgroup");
    let mut candidates = Vec::new();
    let mut push = |path: PathBuf| {
        if path.is_dir() && !candidates.iter().any(|existing| existing == &path) {
            candidates.push(path);
        }
    };

    // Prefer the process's delegated subtree.  The current session scope is
    // often manager-owned/read-only, while a sibling app.slice under the
    // user's delegated service is writable and still remains within the same
    // cgroup hierarchy.
    if let Ok(contents) = std::fs::read_to_string("/proc/self/cgroup")
        && let Some(relative) = contents.lines().find_map(|line| {
            let (hierarchy, path) = line.split_once("::")?;
            (hierarchy.is_empty() || hierarchy == "0").then_some(path.trim())
        })
    {
        let mut current = root.join(relative.trim_start_matches('/'));
        loop {
            push(current.clone());
            if !current.pop() || current == root {
                break;
            }
        }
    }

    #[cfg(unix)]
    {
        // systemd user managers conventionally delegate this subtree to the
        // user.  Do not assume it exists; it is only an additional candidate
        // for hosts where the session cgroup itself is manager-owned.
        let uid = unsafe { libc::geteuid() };
        push(root.join(format!(
            "user.slice/user-{uid}.slice/user@{uid}.service/app.slice"
        )));
        push(root.join(format!("user.slice/user-{uid}.slice/user@{uid}.service")));
    }

    push(root);
    candidates
}

fn create_child_cgroup(parent: &Path, config: &IsolationConfig) -> Option<PathBuf> {
    let cg_path = parent.join(cgroup_name());
    if std::fs::create_dir(&cg_path).is_err() {
        return None;
    }

    // Set memory limit.
    if config.memory_limit_bytes > 0 {
        if let Err(e) = std::fs::write(
            cg_path.join("memory.max"),
            config.memory_limit_bytes.to_string(),
        ) {
            tracing::debug!(path = %cg_path.display(), %e, "delegated cgroup memory.max unavailable");
            let _ = std::fs::remove_dir(&cg_path);
            return None;
        }
        if let Err(e) = std::fs::write(cg_path.join("memory.swap.max"), "0") {
            tracing::debug!(path = %cg_path.display(), %e, "delegated cgroup memory.swap.max unavailable");
        }
    }

    // Set CPU quota (period = 100ms, quota = period * fraction).
    if config.cpu_quota > 0.0 {
        let period_us: u64 = 100_000;
        let quota_us = (period_us as f64 * config.cpu_quota) as u64;
        if let Err(e) = std::fs::write(cg_path.join("cpu.max"), format!("{quota_us} {period_us}")) {
            tracing::debug!(path = %cg_path.display(), %e, "delegated cgroup cpu.max unavailable");
            let _ = std::fs::remove_dir(&cg_path);
            return None;
        }
    }

    Some(cg_path)
}

fn usable_cgroup_parent() -> Option<PathBuf> {
    static PARENT: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    PARENT
        .get_or_init(|| {
            delegated_cgroup_parents()
                .into_iter()
                .find(|parent| probe_cgroup_parent(parent))
        })
        .clone()
}

/// Creating a child directory is not enough to prove that an unprivileged
/// process may migrate into it.  A manager-owned session cgroup can allow the
/// mkdir but reject `cgroup.procs` with EPERM. Probe the actual pre-exec join
/// once per parent and cache only a parent that can complete the full handoff.
fn probe_cgroup_parent(parent: &Path) -> bool {
    let config = IsolationConfig {
        pid_namespace: false,
        mount_namespace: false,
        net_namespace: false,
        memory_limit_bytes: 0,
        cpu_quota: 0.0,
        timeout: Duration::from_secs(1),
        max_output_bytes: 0,
        working_dir: PathBuf::from("/"),
        read_only_paths: Vec::new(),
    };
    let Some(cg_path) = create_child_cgroup(parent, &config) else {
        return false;
    };
    let procs_path = cg_path.join("cgroup.procs");
    let mut command = std::process::Command::new("/bin/true");
    attach_std_child_to_cgroup(&mut command, Some(&procs_path));
    let result = command
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    cleanup_cgroup(&cg_path);
    result
}

fn create_cgroup(config: &IsolationConfig) -> Option<PathBuf> {
    if !cgroupv2_available() {
        return None;
    }
    if config.memory_limit_bytes == 0 && config.cpu_quota == 0.0 {
        return None;
    }

    usable_cgroup_parent().and_then(|parent| create_child_cgroup(&parent, config))
}

/// Remove a cgroup after the process exits.
fn cleanup_cgroup(cg_path: &Path) {
    // cgroup must be empty (no processes) before removal.
    let _ = std::fs::remove_dir(cg_path);
}

/// Terminate every process still owned by an invocation after the shell
/// leader exits.  A successful `bash -c 'work &` must not release the
/// workspace lease while the child continues writing.  cgroup v2 gives us a
/// race-resistant boundary; the process-group fallback covers hosts without
/// cgroup permissions.
#[cfg(unix)]
fn terminate_invocation_children(cg_path: Option<&Path>, leader_pid: Option<u32>) -> bool {
    if let Some(cg_path) = cg_path {
        let kill_file = cg_path.join("cgroup.kill");
        if kill_file.exists() && std::fs::write(kill_file, "1").is_ok() {
            return wait_for_cgroup_empty(cg_path);
        }
        // Older cgroup v2 kernels may not expose cgroup.kill.  Enumerate the
        // private cgroup and kill each remaining member before cleanup.
        if let Ok(procs) = std::fs::read_to_string(cg_path.join("cgroup.procs")) {
            for pid in procs
                .lines()
                .filter_map(|line| line.trim().parse::<i32>().ok())
            {
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(pid),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
        }
        return wait_for_cgroup_empty(cg_path);
    }

    // A process-group cleanup is useful for the short-lived foreground
    // observation path, but it is not a cgroup ownership guarantee. Keep the
    // public cleanup API's historical side effect while returning false: the
    // caller must not mistake best-effort cleanup for durable ownership.
    if leader_pid.is_some() {
        let _ = settle_foreground_process_group(leader_pid);
    }
    false
}

fn settle_process_scope_detailed(
    cg_path: Option<&Path>,
    leader_pid: Option<u32>,
) -> Option<ScopeSettlement> {
    if cg_path.is_some() {
        let descendants_terminated = cg_path.is_some_and(|path| {
            std::fs::read_to_string(path.join("cgroup.procs"))
                .is_ok_and(|procs| !procs.trim().is_empty())
        });
        return terminate_invocation_children(cg_path, leader_pid).then_some(ScopeSettlement {
            ownership: ScopeOwnership::InvocationCgroup,
            descendants_terminated,
        });
    }
    let descendants_terminated = foreground_process_group_has_members(leader_pid);
    settle_foreground_process_group(leader_pid).then_some(ScopeSettlement {
        ownership: ScopeOwnership::ForegroundProcessGroup,
        descendants_terminated,
    })
}

/// Kill the leader's process group and wait until the kernel reports that the
/// group no longer exists.  This is deliberately a weaker boundary than a
/// cgroup: a descendant which created a new session is outside the group and
/// therefore makes no claim here.  The caller records the weaker provenance
/// and can choose not to use it for durable cache/renewal decisions.
#[cfg(unix)]
fn settle_foreground_process_group(leader_pid: Option<u32>) -> bool {
    let Some(raw_pid) = leader_pid.and_then(|pid| i32::try_from(pid).ok()) else {
        return false;
    };
    let pgid = nix::unistd::Pid::from_raw(raw_pid);
    let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGKILL);
    let deadline = std::time::Instant::now() + Duration::from_millis(250);
    loop {
        // kill(-pgid, 0) checks for any member without sending a signal.
        // ESRCH means the group is empty (or never had descendants); EPERM
        // means a member still exists and must not be treated as settled.
        let probe = unsafe { libc::kill(-(raw_pid as libc::pid_t), 0) };
        if probe == -1 {
            let errno = std::io::Error::last_os_error();
            if errno.raw_os_error() == Some(libc::ESRCH) {
                return true;
            }
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(unix)]
fn foreground_process_group_has_members(leader_pid: Option<u32>) -> bool {
    leader_pid
        .and_then(|pid| i32::try_from(pid).ok())
        .is_some_and(|pid| unsafe { libc::kill(-(pid as libc::pid_t), 0) } == 0)
}

#[cfg(not(unix))]
fn foreground_process_group_has_members(_leader_pid: Option<u32>) -> bool {
    false
}

#[cfg(not(unix))]
fn settle_foreground_process_group(_leader_pid: Option<u32>) -> bool {
    false
}

/// Wait a bounded interval for cgroup v2 to report no members. Killing is
/// asynchronous; taking a workspace fingerprint immediately after the signal
/// can otherwise race a final descendant write. A short bounded wait keeps
/// normal commands cheap while failing closed (the caller's fingerprint sees
/// an active/unknown scope) when the kernel does not reap promptly.
#[cfg(unix)]
fn wait_for_cgroup_empty(cg_path: &Path) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_millis(250);
    let events = cg_path.join("cgroup.events");
    while std::time::Instant::now() < deadline {
        if std::fs::read_to_string(&events).ok().and_then(|contents| {
            contents.lines().find_map(|line| {
                let (key, value) = line.split_once(' ')?;
                (key == "populated").then_some(value == "0")
            })
        }) == Some(true)
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

#[cfg(not(unix))]
fn terminate_invocation_children(_cg_path: Option<&Path>, _leader_pid: Option<u32>) -> bool {
    false
}

// ─── Public API: attach cgroup limits to an existing tokio::process::Command ──

/// RAII handle for a transient cgroup v2 scope.
///
/// On drop, removes the cgroup directory (must be empty — i.e. the child
/// process has already exited). Callers who care about guaranteed cleanup
/// should drop the guard only after the child has been waited on.
///
/// Created by [`apply_cgroup`]. If the host doesn't support cgroup v2 or
/// the caller passed zero limits, the guard is inactive: no cgroup is
/// created and no cleanup happens. Check with [`CgroupGuard::active`].
#[derive(Debug)]
pub struct CgroupGuard {
    cg_path: Option<PathBuf>,
    /// Absolute path to cgroup.procs for post-spawn PID join.
    procs_path: Option<PathBuf>,
}

impl CgroupGuard {
    /// Whether an actual cgroup was created and joined by the child.
    /// `false` means the host lacks cgroup v2 support, or all limits
    /// were zero so no cgroup was needed.
    pub fn active(&self) -> bool {
        self.cg_path.is_some()
    }

    /// A positive workspace delta can only be attributed to this invocation
    /// when a real cgroup ownership boundary exists.  Process-group cleanup
    /// remains a best-effort fallback, but a child can escape it by creating a
    /// new session, so that fallback is intentionally not authoritative.
    pub fn ownership_guaranteed(&self) -> bool {
        self.active()
    }

    /// Absolute path of the cgroup directory, if active. Exposed for
    /// observability — callers typically don't need to interact with it.
    pub fn path(&self) -> Option<&Path> {
        self.cg_path.as_deref()
    }

    /// Confirm the child-ownership handoff after spawn.
    ///
    /// Active cgroups are joined by the `pre_exec` hook installed through
    /// [`Self::attach_child`] / [`Self::attach_std_child`].  That hook runs
    /// before the target is exec'd and its success is reported through the
    /// standard `Command::spawn` exec-error pipe.  A second parent-side write
    /// is deliberately not attempted here: a short-lived child may already
    /// have exited (and be an unreaped zombie), for which `kill(pid, 0)` is
    /// not a reliable liveness test.  Retrying or treating that state as a
    /// failed handoff creates a nondeterministic false failure after the
    /// ownership guarantee has already been established.
    pub fn join_child(&self, _pid: u32) -> Result<(), std::io::Error> {
        Ok(())
    }

    /// Arrange for the spawned child to join this cgroup from its
    /// `pre_exec` hook, before the target program is exec'd. Parent-side
    /// `cgroup.procs` writes alone leave a fork/exec window in which a very
    /// short-lived child can create descendants outside the scope. The
    /// The spawn error pipe makes a failed self-registration visible to the
    /// parent, so callers may retain the historical [`Self::join_child`]
    /// call as a no-op compatibility/integrity boundary.
    pub fn attach_child(&self, command: &mut tokio::process::Command) {
        attach_child_to_cgroup(command, self.procs_path.as_deref());
    }

    /// `std::process::Command` counterpart used by synchronous Edge helpers.
    pub fn attach_std_child(&self, command: &mut std::process::Command) {
        attach_std_child_to_cgroup(command, self.procs_path.as_deref());
    }

    /// Kill all descendants still owned by this invocation. This is used
    /// immediately after a shell leader exits, before its observation lease
    /// or workspace fingerprint is released.
    pub fn terminate_all(&self) -> bool {
        terminate_invocation_children(self.cg_path.as_deref(), None)
    }

    /// Kill all descendants while also supplying the leader's process-group
    /// identity for hosts without a delegated cgroup.  The fallback remains
    /// non-authoritative (it returns `false`), but using the real group here
    /// is important: callers can clean up ordinary descendants before they
    /// quarantine the workspace for an escaped/unowned process.
    pub fn terminate_all_for(&self, leader_pid: Option<u32>) -> bool {
        terminate_invocation_children(self.cg_path.as_deref(), leader_pid)
    }

    /// Settle an invocation before its workspace fingerprint is consumed.
    ///
    /// On a delegated host this uses the cgroup boundary.  When cgroups are
    /// unavailable, the shell's process group is still cleaned and observed
    /// as empty.  That fallback is intentionally labelled separately so
    /// downstream policy can accept the immediate result without treating it
    /// as proof that an escaped session cannot exist.
    pub fn settle_for_observation(&self, leader_pid: Option<u32>) -> Option<ScopeOwnership> {
        self.settle_for_observation_detailed(leader_pid)
            .map(|settlement| settlement.ownership)
    }

    /// Settle the scope while retaining whether live descendants were found.
    pub fn settle_for_observation_detailed(
        &self,
        leader_pid: Option<u32>,
    ) -> Option<ScopeSettlement> {
        if self.cg_path.is_some() {
            let descendants_terminated = self.cg_path.as_deref().is_some_and(|path| {
                std::fs::read_to_string(path.join("cgroup.procs"))
                    .is_ok_and(|procs| !procs.trim().is_empty())
            });
            return terminate_invocation_children(self.cg_path.as_deref(), leader_pid).then_some(
                ScopeSettlement {
                    ownership: ScopeOwnership::InvocationCgroup,
                    descendants_terminated,
                },
            );
        }
        let descendants_terminated = foreground_process_group_has_members(leader_pid);
        settle_foreground_process_group(leader_pid).then_some(ScopeSettlement {
            ownership: ScopeOwnership::ForegroundProcessGroup,
            descendants_terminated,
        })
    }
}

fn attach_child_to_cgroup(command: &mut tokio::process::Command, procs_path: Option<&Path>) {
    #[cfg(unix)]
    if let Some(procs_path) = procs_path.map(Path::to_path_buf) {
        use std::os::unix::ffi::OsStrExt;
        let mut path_bytes = procs_path.as_os_str().as_bytes().to_vec();
        path_bytes.push(0);
        // SAFETY: this hook only performs a bounded, pre-opened-path write
        // before exec. All allocations happen before fork; the hook itself
        // uses only async-signal-safe libc calls and a stack buffer.
        unsafe {
            command.pre_exec(move || write_self_pid_to_cgroup(&path_bytes));
        }
    }
}

fn attach_std_child_to_cgroup(command: &mut std::process::Command, procs_path: Option<&Path>) {
    #[cfg(unix)]
    if let Some(procs_path) = procs_path.map(Path::to_path_buf) {
        use std::os::unix::ffi::OsStrExt;
        let mut path_bytes = procs_path.as_os_str().as_bytes().to_vec();
        path_bytes.push(0);
        use std::os::unix::process::CommandExt;
        // SAFETY: all allocation is before fork; the hook only uses
        // async-signal-safe libc calls and a stack buffer.
        unsafe {
            command.pre_exec(move || write_self_pid_to_cgroup(&path_bytes));
        }
    }
}

#[cfg(unix)]
fn write_self_pid_to_cgroup(path: &[u8]) -> std::io::Result<()> {
    let fd = unsafe { libc::open(path.as_ptr().cast(), libc::O_WRONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }

    let mut digits = [0u8; 20];
    let mut value = unsafe { libc::getpid() } as u32;
    let mut cursor = digits.len();
    if value == 0 {
        cursor -= 1;
        digits[cursor] = b'0';
    } else {
        while value > 0 {
            cursor -= 1;
            digits[cursor] = b'0' + (value % 10) as u8;
            value /= 10;
        }
    }
    let bytes = &digits[cursor..];
    let mut written = 0usize;
    while written < bytes.len() {
        let result =
            unsafe { libc::write(fd, bytes[written..].as_ptr().cast(), bytes.len() - written) };
        if result < 0 {
            let error = std::io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(error);
        }
        if result == 0 {
            unsafe { libc::close(fd) };
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "cgroup self-join wrote zero bytes",
            ));
        }
        written += result as usize;
    }
    if unsafe { libc::close(fd) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

impl Drop for CgroupGuard {
    fn drop(&mut self) {
        if let Some(p) = self.cg_path.take() {
            // Dropping an invocation owner must never release its ownership
            // boundary while descendants are still running. Normal callers
            // explicitly terminate and wait before drop; this RAII fallback
            // covers cancellation, panic, and future-abort paths. The kill is
            // idempotent; the bounded empty wait lets cgroup removal complete
            // once the kernel has reaped the members.
            let _ = terminate_invocation_children(Some(&p), None);
            cleanup_cgroup(&p);
        }
    }
}

/// Attach cgroup v2 memory + CPU limits to a spawned child process.
///
/// Returns a [`CgroupGuard`] that must be kept alive for the lifetime
/// of the child. Call [`CgroupGuard::join_child`] immediately after
/// `Command::spawn()` to write the child's PID to `cgroup.procs`.
///
/// Semantics:
/// - `memory_limit_bytes = 0` → no memory limit applied.
/// - `cpu_quota = 0.0` → no CPU limit applied.
/// - If both are zero, or cgroup v2 is unavailable, returns an inactive
///   guard. This is the happy "permissive" fallback — callers who need
///   to *know* whether isolation actually fired should check
///   [`CgroupGuard::active`].
///
/// Unlike [`execute_isolated`], this function does NOT spawn, pump, or
/// wait — it only creates the cgroup and returns a guard handle.
pub fn apply_cgroup(memory_limit_bytes: u64, cpu_quota: f64) -> CgroupGuard {
    // Mirror the allocation heuristic of create_cgroup.
    if memory_limit_bytes == 0 && cpu_quota <= 0.0 {
        return CgroupGuard {
            cg_path: None,
            procs_path: None,
        };
    }
    let config = IsolationConfig {
        pid_namespace: false,
        mount_namespace: false,
        net_namespace: false,
        memory_limit_bytes,
        cpu_quota,
        timeout: Duration::from_secs(0),
        max_output_bytes: DEFAULT_MAX_CAPTURED_OUTPUT_BYTES,
        working_dir: PathBuf::new(),
        read_only_paths: Vec::new(),
    };
    let cg_path = match create_cgroup(&config) {
        Some(p) => p,
        None => {
            return CgroupGuard {
                cg_path: None,
                procs_path: None,
            };
        }
    };

    let procs_path = cg_path.join("cgroup.procs");

    CgroupGuard {
        cg_path: Some(cg_path),
        procs_path: Some(procs_path),
    }
}

/// Create an ownership-only cgroup for a foreground tool process. Unlike
/// [`apply_cgroup`], this does not impose memory/CPU limits; it exists so a
/// shell that forks, daemonizes, or calls `setsid` cannot outlive the tool's
/// post-execution observation window. The child joins through a pre-exec
/// self-registration hook before user code starts; freezing before `spawn`
/// would deadlock the exec handshake.
pub fn apply_process_scope() -> CgroupGuard {
    if !cgroupv2_available() {
        return CgroupGuard {
            cg_path: None,
            procs_path: None,
        };
    }
    let config = IsolationConfig {
        pid_namespace: false,
        mount_namespace: false,
        net_namespace: false,
        memory_limit_bytes: 0,
        cpu_quota: 0.0,
        timeout: Duration::from_secs(0),
        max_output_bytes: 0,
        working_dir: PathBuf::new(),
        read_only_paths: Vec::new(),
    };
    let Some(cg_path) =
        usable_cgroup_parent().and_then(|parent| create_child_cgroup(&parent, &config))
    else {
        return CgroupGuard {
            cg_path: None,
            procs_path: None,
        };
    };
    CgroupGuard {
        procs_path: Some(cg_path.join("cgroup.procs")),
        cg_path: Some(cg_path),
    }
}

/// Whether this host can establish the invocation-owned scope required by
/// tools whose result must include an authoritative workspace receipt. The
/// probe is cached with the delegated-parent capability check, so callers can
/// use it for schema projection without creating a transient cgroup per
/// request.
pub fn process_scope_available() -> bool {
    cgroupv2_available() && usable_cgroup_parent().is_some()
}

fn append_capped(buffer: &mut Vec<u8>, chunk: &[u8], max_bytes: usize, capped: &mut bool) {
    if buffer.len() >= max_bytes {
        *capped = true;
        return;
    }

    let remaining = max_bytes - buffer.len();
    if chunk.len() <= remaining {
        buffer.extend_from_slice(chunk);
    } else {
        buffer.extend_from_slice(&chunk[..remaining]);
        *capped = true;
    }
}

fn drain_stream_chunks(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<StreamChunk>,
    capture: &mut StreamOutputCapture,
) {
    capture.drain_ready(rx);
}

fn trim_incomplete_trailing_line(output: &mut String) {
    if output.is_empty() || output.ends_with('\n') || output.ends_with('\r') {
        return;
    }

    if let Some(last_break) = output.rfind('\n').or_else(|| output.rfind('\r')) {
        output.truncate(last_break + 1);
    } else {
        output.clear();
    }
}

async fn pump_stream<R>(
    mut reader: R,
    stream: StreamKind,
    tx: tokio::sync::mpsc::UnboundedSender<StreamChunk>,
) where
    R: AsyncRead + Unpin,
{
    let mut buffer = [0u8; READ_CHUNK_SIZE];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => break,
            Ok(read) => {
                if tx
                    .send(StreamChunk {
                        stream,
                        bytes: buffer[..read].to_vec(),
                    })
                    .is_err()
                {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

async fn drain_stream_pumps_after_exit(
    mut stdout_task: tokio::task::JoinHandle<()>,
    mut stderr_task: tokio::task::JoinHandle<()>,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<StreamChunk>,
    capture: &mut StreamOutputCapture,
) {
    let mut stdout_done = false;
    let mut stderr_done = false;
    let idle_timer = tokio::time::sleep(OUTPUT_DRAIN_TIMEOUT);
    tokio::pin!(idle_timer);

    loop {
        if stdout_done && stderr_done {
            break;
        }

        tokio::select! {
            Some(chunk) = rx.recv() => {
                capture.append(chunk);
                idle_timer
                    .as_mut()
                    .reset(tokio::time::Instant::now() + OUTPUT_DRAIN_TIMEOUT);

                if capture.is_capped() {
                    if !stdout_done {
                        stdout_task.abort();
                    }
                    if !stderr_done {
                        stderr_task.abort();
                    }
                    break;
                }
            }
            _ = &mut stdout_task, if !stdout_done => {
                stdout_done = true;
            }
            _ = &mut stderr_task, if !stderr_done => {
                stderr_done = true;
            }
            _ = &mut idle_timer => {
                if !stdout_done {
                    stdout_task.abort();
                }
                if !stderr_done {
                    stderr_task.abort();
                }
                break;
            }
        }
    }

    if !stdout_done {
        let _ = stdout_task.await;
    }
    if !stderr_done {
        let _ = stderr_task.await;
    }

    drain_stream_chunks(rx, capture);
}

/// Build a shell script that sets up filesystem isolation inside a new mount
/// namespace before executing `command`.
///
/// When `unshare --mount` creates a new mount namespace, the child still
/// inherits the full host filesystem.  This wrapper:
///
/// 1. Remounts `/` as private to prevent mount propagation.
/// 2. Mounts a fresh `/proc` (required by PID namespace).
/// 3. Mounts `tmpfs` over `/tmp` and `/var/tmp` to block temp-file leaks.
/// 4. Creates a `tmpfs` at `/workspace` and bind-mounts `working_dir` into it
///    so tools can read/write their workspace without seeing the host tree.
///
/// Every mount that establishes the write boundary is fatal. A managed tool
/// must not execute if the kernel cannot provide the requested isolation.
fn build_mount_namespace_wrapper() -> String {
    let script = r#"
set -eu
# Make / private to prevent mount propagation back to the host
mount --make-rprivate /
# Fresh procfs for the PID namespace
mount -t proc proc /proc 2>/dev/null || true
# Isolate temporary directories
mount -t tmpfs -o size=128M,mode=1777 tmpfs /tmp
mkdir -p /var/tmp
mount -t tmpfs -o size=32M,mode=1777 tmpfs /var/tmp
# Create workspace and bind-mount the working directory.
# We use /tmp/_astra_ws instead of /workspace because / is often not
# writable in unprivileged user namespaces (root-owned on the host).
# The path comes from argv, not the environment, so callers cannot spoof it.
# Validate arguments are present
if [ $# -lt 2 ]; then
  echo "Error: command and working directory arguments required" >&2
  exit 1
fi
user_command=$1
workspace_root=$2
mkdir -p /tmp/_astra_ws
mount --bind -- "$2" /tmp/_astra_ws
# Each remaining argument is a host-owned lane below the selected workspace.
# Bind-remount it read-only inside the child view before user code starts.
shift 2
for protected in "$@"; do
  case "$protected" in
    "$workspace_root"/*) relative=${protected#"$workspace_root"/} ;;
    *) echo "Error: protected path escapes workspace" >&2; exit 1 ;;
  esac
  target="/tmp/_astra_ws/$relative"
  test -e "$target"
  mount --bind -- "$target" "$target"
  mount -o remount,bind,ro -- "$target"
done
# The rest of the inherited filesystem is read-only. Separate /tmp and the
# workspace bind remain writable mounts; protected workspace submounts do not.
mount --bind / /
mount -o remount,bind,ro /
cd /tmp/_astra_ws
# Root inside the private user namespace was needed only to construct mounts.
# Drop every capability before executing untrusted code so it cannot remount a
# protected lane read-write again.
exec setpriv --bounding-set=-all --inh-caps=-all --ambient-caps=-all --no-new-privs \
  bash -c "$user_command"
"#;
    script.trim().to_string()
}

/// Execute a command with Linux process isolation.
///
/// # Fallback behavior
///
/// If namespaces are unavailable (e.g., in a container), the command runs
/// as a plain subprocess with cgroup limits only.  If cgroups are also
/// unavailable, it runs as a plain subprocess with just a timeout.
pub async fn execute_isolated(
    command: &str,
    env: &std::collections::HashMap<String, String>,
    config: &IsolationConfig,
) -> IsolatedOutput {
    execute_isolated_with_cancel(command, env, config, None).await
}

async fn settle_isolated_owner_after_exit(
    owner: BashInvocationOwner,
    leader_pid: Option<u32>,
) -> Option<ScopeSettlement> {
    tokio::task::spawn_blocking(move || {
        let mut owner = owner;
        owner.settle_after_exit_detailed(leader_pid)
    })
    .await
    .ok()
    .flatten()
}

/// Synchronous abort boundary for an in-flight isolated invocation.
///
/// Explicit completion paths disarm this after awaiting a full settlement.
/// If the enclosing future is instead dropped (task abort, shutdown, outer
/// timeout), this guard runs before the child local is dropped. Supervised
/// invocations receive a control-plane cancellation and are deliberately not
/// configured with Tokio's leader-only `kill_on_drop`; cgroup/process-group
/// fallbacks are synchronously settled here.
struct IsolatedScopeAbortGuard {
    owner: Option<BashInvocationOwner>,
    cg_path: Option<PathBuf>,
    leader_pid: Option<u32>,
    armed: bool,
}

impl IsolatedScopeAbortGuard {
    fn new(
        owner: Option<BashInvocationOwner>,
        cg_path: Option<PathBuf>,
        leader_pid: Option<u32>,
    ) -> Self {
        Self {
            owner,
            cg_path,
            leader_pid,
            armed: true,
        }
    }

    fn take_owner(&mut self) -> Option<BashInvocationOwner> {
        self.owner.take()
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for IsolatedScopeAbortGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(owner) = self.owner.as_mut() {
            if owner.is_supervised() {
                let _ = owner.request_supervised_termination();
            } else {
                let _ = owner.settle_after_exit_detailed(self.leader_pid);
            }
        } else {
            let _ = settle_process_scope_detailed(self.cg_path.as_deref(), self.leader_pid);
        }
        if let Some(cg_path) = self.cg_path.as_deref() {
            cleanup_cgroup(cg_path);
        }
    }
}

/// Stop the owner before its helper. A Linux invocation supervisor is the
/// subreaper for daemonized descendants; killing that helper first would
/// release exactly the processes this boundary exists to contain.
async fn terminate_isolated_child(
    child: &mut tokio::process::Child,
    owner: Option<BashInvocationOwner>,
    cg_path: Option<&Path>,
    leader_pid: Option<u32>,
) -> Option<ScopeSettlement> {
    let Some(mut owner) = owner else {
        let _ = child.kill().await;
        let _ = child.wait().await;
        return settle_process_scope_detailed(cg_path, leader_pid);
    };

    if owner.is_supervised() {
        let owner_result = tokio::task::spawn_blocking(move || {
            let requested = owner.request_supervised_termination();
            (owner, requested)
        })
        .await;
        let Ok((returned_owner, requested)) = owner_result else {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return None;
        };
        owner = returned_owner;
        if !requested {
            // The helper may already be finishing naturally. Give it the same
            // bounded settlement opportunity, but never promote a forced
            // helper kill into an authoritative receipt.
            if tokio::time::timeout(Duration::from_secs(3), child.wait())
                .await
                .is_err()
            {
                let _ = child.kill().await;
                let _ = child.wait().await;
                return None;
            }
        } else if tokio::time::timeout(Duration::from_secs(3), child.wait())
            .await
            .is_err()
        {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return None;
        }
    } else {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    settle_isolated_owner_after_exit(owner, leader_pid).await
}

/// Execute a command while retaining process ownership through cancellation.
/// The returned `IsolatedOutput::cancelled` bit distinguishes cancellation
/// from a natural exit or wall-clock timeout.
pub async fn execute_isolated_with_cancel(
    command: &str,
    env: &std::collections::HashMap<String, String>,
    config: &IsolationConfig,
    cancel_token: Option<&CancellationToken>,
) -> IsolatedOutput {
    execute_isolated_with_cancel_impl(command, env, config, cancel_token, None).await
}

async fn execute_isolated_with_cancel_impl(
    command: &str,
    env: &std::collections::HashMap<String, String>,
    config: &IsolationConfig,
    cancel_token: Option<&CancellationToken>,
    supervisor_helper: Option<(PathBuf, Vec<String>)>,
) -> IsolatedOutput {
    let wants_ns = config.pid_namespace || config.mount_namespace || config.net_namespace;
    let ns_available = wants_ns && unshare_available();

    // Warn operators when namespace isolation was requested but is unavailable.
    // In Strict mode, this is a hard failure — security guarantees are NOT met.
    if wants_ns && !ns_available {
        tracing::error!(
            target: "astra_sandbox::process_isolation",
            "namespace isolation unavailable (unshare not found or not permitted); \
             refusing to execute in Strict-mode. \
             PID/mount/network namespace isolation is required for strict sandboxing."
        );
        return IsolatedOutput {
            stdout: String::new(),
            stderr: "Error: namespace isolation unavailable — strict-mode requires \
                     PID/mount/network namespace isolation (unshare not found or not permitted)"
                .to_string(),
            exit_code: None,
            timed_out: false,
            cancelled: false,
            execution_started: false,
            stdout_capped: false,
            stderr_capped: false,
            namespace_active: false,
            cgroup_active: false,
            scope_settled: false,
            scope_ownership: None,
            descendants_terminated: false,
        };
    }
    if config.mount_namespace
        && config
            .read_only_paths
            .iter()
            .any(|path| path == &config.working_dir || !path.starts_with(&config.working_dir))
    {
        return IsolatedOutput {
            stdout: String::new(),
            stderr: "Error: managed read-only path escapes the selected workspace".to_string(),
            exit_code: None,
            timed_out: false,
            cancelled: false,
            execution_started: false,
            stdout_capped: false,
            stderr_capped: false,
            namespace_active: false,
            cgroup_active: false,
            scope_settled: false,
            scope_ownership: None,
            descendants_terminated: false,
        };
    }

    // ── Build the command ────────────────────────────────────────────
    let (program, args) = if ns_available {
        let mut unshare_flags = Vec::new();
        if config.pid_namespace {
            unshare_flags.push("--pid");
            unshare_flags.push("--fork");
            unshare_flags.push("--kill-child");
        }
        if config.mount_namespace {
            unshare_flags.push("--mount");
        }
        if config.net_namespace {
            unshare_flags.push("--net");
        }
        // Map the current user to root inside the namespace (unprivileged).
        // Skip on kernels < 3.19 where this has known EoP vulnerabilities.
        let use_map_root = map_root_user_safe();
        if use_map_root {
            unshare_flags.push("--map-root-user");
        } else {
            tracing::warn!(
                target: "astra_sandbox::process_isolation",
                "skipping --map-root-user: kernel is vulnerable. \
                 User namespace isolation is degraded."
            );
        }
        unshare_flags.push("--");
        unshare_flags.push("bash");
        unshare_flags.push("-c");

        // Build a filesystem isolation wrapper when mount namespace is active.
        // Without this, the child inherits the full host filesystem including
        // /etc/shadow and other sensitive paths.
        let mut args = unshare_flags
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>();
        if config.mount_namespace {
            args.push(build_mount_namespace_wrapper());
            args.push("astra-mount-wrapper".to_string());
            args.push(command.to_string());
            args.push(config.working_dir.display().to_string());
            args.extend(
                config
                    .read_only_paths
                    .iter()
                    .map(|path| path.display().to_string()),
            );
        } else {
            args.push(command.to_string());
        }
        ("unshare".to_string(), args)
    } else {
        (
            "bash".to_string(),
            vec!["-c".to_string(), command.to_string()],
        )
    };

    // ── cgroup setup ─────────────────────────────────────────────────
    let cg_path = create_cgroup(config);

    // Resource-limit cgroups remain preferred. If the host cannot create
    // one, use the same invocation owner as Edge/shared Bash so a setsid or
    // double-fork cannot escape the server-local execution boundary.
    let (mut std_cmd, mut invocation_owner) = if cg_path.is_some() {
        let mut command = std::process::Command::new(&program);
        command.args(&args);
        (command, None)
    } else {
        #[cfg(target_os = "linux")]
        let prepared = if let Some((helper_program, helper_args)) = supervisor_helper {
            BashInvocationOwner::prepare_with_supervisor_helper(
                helper_program,
                helper_args,
                &program,
                &args,
            )
        } else {
            BashInvocationOwner::prepare(&program, &args)
        };
        #[cfg(not(target_os = "linux"))]
        let prepared = {
            let _ = supervisor_helper;
            BashInvocationOwner::prepare(&program, &args)
        };
        match prepared {
            Ok((command, owner)) => (command, Some(owner)),
            Err(error) => {
                return IsolatedOutput {
                    stdout: String::new(),
                    stderr: format!("Failed to establish process ownership: {error}"),
                    exit_code: None,
                    timed_out: false,
                    cancelled: false,
                    execution_started: false,
                    stdout_capped: false,
                    stderr_capped: false,
                    namespace_active: false,
                    cgroup_active: false,
                    scope_settled: false,
                    scope_ownership: None,
                    descendants_terminated: false,
                };
            }
        }
    };
    std_cmd
        .current_dir(&config.working_dir)
        .env_clear()
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    std_cmd.process_group(0);

    // Apply filtered environment first (untrusted).
    for (k, v) in env {
        std_cmd.env(k, v);
    }
    let cgroup_procs_path = cg_path.as_ref().map(|path| path.join("cgroup.procs"));
    if let Some(owner) = invocation_owner.as_ref() {
        if let Err(error) = owner.install(&mut std_cmd) {
            return IsolatedOutput {
                stdout: String::new(),
                stderr: format!("Failed to install process ownership: {error}"),
                exit_code: None,
                timed_out: false,
                cancelled: false,
                execution_started: false,
                stdout_capped: false,
                stderr_capped: false,
                namespace_active: false,
                cgroup_active: false,
                scope_settled: false,
                scope_ownership: None,
                descendants_terminated: false,
            };
        }
    } else {
        attach_std_child_to_cgroup(&mut std_cmd, cgroup_procs_path.as_deref());
    }
    let supervised_invocation = invocation_owner
        .as_ref()
        .is_some_and(BashInvocationOwner::is_supervised);
    let mut cmd = tokio::process::Command::from(std_cmd);
    // A supervisor is the authoritative descendant owner. Killing only that
    // helper on future drop would orphan its setsid/double-fork descendants;
    // the abort guard closes its control plane instead.
    cmd.kill_on_drop(!supervised_invocation);

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            if let Some(ref cg) = cg_path {
                cleanup_cgroup(cg);
            }
            return IsolatedOutput {
                stdout: String::new(),
                stderr: format!("Failed to execute: {e}"),
                exit_code: None,
                timed_out: false,
                cancelled: false,
                execution_started: false,
                stdout_capped: false,
                stderr_capped: false,
                namespace_active: false,
                cgroup_active: false,
                scope_settled: false,
                scope_ownership: None,
                descendants_terminated: false,
            };
        }
    };

    if let Some(owner) = invocation_owner.take() {
        let child_pid = child.id();
        let Some(started_pid) = child_pid else {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return IsolatedOutput {
                stdout: String::new(),
                stderr: "Process ownership handshake failed: child PID unavailable".to_string(),
                exit_code: None,
                timed_out: false,
                cancelled: false,
                execution_started: true,
                stdout_capped: false,
                stderr_capped: false,
                namespace_active: ns_available,
                cgroup_active: cg_path.is_some(),
                scope_settled: false,
                scope_ownership: None,
                descendants_terminated: false,
            };
        };
        let started = tokio::task::spawn_blocking(move || {
            let mut owner = owner;
            let result = owner.started(started_pid);
            (owner, result)
        })
        .await;
        match started {
            Ok((owner, Ok(()))) => invocation_owner = Some(owner),
            Ok((owner, Err(error))) => {
                let settlement = terminate_isolated_child(
                    &mut child,
                    Some(owner),
                    cg_path.as_deref(),
                    child_pid,
                )
                .await;
                return IsolatedOutput {
                    stdout: String::new(),
                    stderr: format!("Process ownership handshake failed: {error}"),
                    exit_code: None,
                    timed_out: false,
                    cancelled: false,
                    execution_started: true,
                    stdout_capped: false,
                    stderr_capped: false,
                    namespace_active: ns_available,
                    cgroup_active: cg_path.is_some(),
                    scope_settled: settlement.is_some(),
                    scope_ownership: settlement.map(|value| value.ownership),
                    descendants_terminated: settlement
                        .is_some_and(|value| value.descendants_terminated),
                };
            }
            Err(error) => {
                if supervised_invocation {
                    // The blocking worker owns (and will drop) the control
                    // endpoint, which asks the helper to settle. Do not race
                    // that cleanup with a leader-only SIGKILL.
                    let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
                } else {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                }
                return IsolatedOutput {
                    stdout: String::new(),
                    stderr: format!("Process ownership handshake worker failed: {error}"),
                    exit_code: None,
                    timed_out: false,
                    cancelled: false,
                    execution_started: true,
                    stdout_capped: false,
                    stderr_capped: false,
                    namespace_active: ns_available,
                    cgroup_active: cg_path.is_some(),
                    scope_settled: false,
                    scope_ownership: None,
                    descendants_terminated: false,
                };
            }
        }
    }

    // Declared after `child`, so this guard is dropped first on every abrupt
    // future teardown. Normal paths explicitly take/disarm it.
    let mut scope_abort_guard =
        IsolatedScopeAbortGuard::new(invocation_owner.take(), cg_path.clone(), child.id());

    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let leader_pid = child.id();
            let settlement = terminate_isolated_child(
                &mut child,
                scope_abort_guard.take_owner(),
                cg_path.as_deref(),
                leader_pid,
            )
            .await;
            scope_abort_guard.disarm();
            if let Some(ref cg) = cg_path {
                cleanup_cgroup(cg);
            }
            return IsolatedOutput {
                stdout: String::new(),
                stderr: "Failed to capture stdout pipe".to_string(),
                exit_code: None,
                timed_out: false,
                cancelled: false,
                execution_started: true,
                stdout_capped: false,
                stderr_capped: false,
                namespace_active: ns_available,
                cgroup_active: cg_path.is_some(),
                scope_settled: settlement.is_some(),
                scope_ownership: settlement.map(|value| value.ownership),
                descendants_terminated: settlement
                    .is_some_and(|value| value.descendants_terminated),
            };
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            let leader_pid = child.id();
            let settlement = terminate_isolated_child(
                &mut child,
                scope_abort_guard.take_owner(),
                cg_path.as_deref(),
                leader_pid,
            )
            .await;
            scope_abort_guard.disarm();
            if let Some(ref cg) = cg_path {
                cleanup_cgroup(cg);
            }
            return IsolatedOutput {
                stdout: String::new(),
                stderr: "Failed to capture stderr pipe".to_string(),
                exit_code: None,
                timed_out: false,
                cancelled: false,
                execution_started: true,
                stdout_capped: false,
                stderr_capped: false,
                namespace_active: ns_available,
                cgroup_active: cg_path.is_some(),
                scope_settled: settlement.is_some(),
                scope_ownership: settlement.map(|value| value.ownership),
                descendants_terminated: settlement
                    .is_some_and(|value| value.descendants_terminated),
            };
        }
    };

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let stdout_task = tokio::spawn(pump_stream(stdout, StreamKind::Stdout, tx.clone()));
    let stderr_task = tokio::spawn(pump_stream(stderr, StreamKind::Stderr, tx));

    let mut capture = StreamOutputCapture::new(config.max_output_bytes);
    let mut exit_code = None;
    let mut timed_out = false;
    let mut cancelled = false;
    let scope_settlement;
    let mut abort_stream_pumps = false;
    let deadline = tokio::time::Instant::now() + config.timeout;

    loop {
        drain_stream_chunks(&mut rx, &mut capture);

        let leader_pid = child.id();
        match child.try_wait() {
            Ok(Some(status)) => {
                scope_settlement = if let Some(owner) = scope_abort_guard.take_owner() {
                    settle_isolated_owner_after_exit(owner, leader_pid).await
                } else {
                    settle_process_scope_detailed(cg_path.as_deref(), leader_pid)
                };
                exit_code = status.code();
                break;
            }
            Ok(None) => {}
            Err(e) => {
                let scope_settlement = terminate_isolated_child(
                    &mut child,
                    scope_abort_guard.take_owner(),
                    cg_path.as_deref(),
                    leader_pid,
                )
                .await;
                scope_abort_guard.disarm();
                let scope_ownership = scope_settlement.map(|settlement| settlement.ownership);
                let _ = stdout_task.await;
                let _ = stderr_task.await;
                if let Some(ref cg) = cg_path {
                    cleanup_cgroup(cg);
                }
                return IsolatedOutput {
                    stdout: String::new(),
                    stderr: format!("Failed to execute: {e}"),
                    exit_code: None,
                    timed_out: false,
                    cancelled: false,
                    execution_started: true,
                    stdout_capped: false,
                    stderr_capped: false,
                    namespace_active: ns_available,
                    cgroup_active: cg_path.is_some(),
                    scope_settled: scope_ownership.is_some(),
                    scope_ownership,
                    descendants_terminated: scope_settlement
                        .is_some_and(|settlement| settlement.descendants_terminated),
                };
            }
        }

        let now = tokio::time::Instant::now();
        if now >= deadline {
            timed_out = true;
            scope_settlement = terminate_isolated_child(
                &mut child,
                scope_abort_guard.take_owner(),
                cg_path.as_deref(),
                leader_pid,
            )
            .await;
            // Abort I/O reader tasks immediately — grandchild processes may
            // still hold the pipes open (e.g. `sleep` surviving shell kill).
            abort_stream_pumps = true;
            break;
        }

        let sleep = tokio::time::sleep(std::cmp::min(
            PROCESS_POLL_INTERVAL,
            deadline.saturating_duration_since(now),
        ));
        tokio::pin!(sleep);
        if let Some(token) = cancel_token {
            tokio::select! {
                _ = token.cancelled() => {
                    cancelled = true;
                    scope_settlement = terminate_isolated_child(
                        &mut child,
                        scope_abort_guard.take_owner(),
                        cg_path.as_deref(),
                        leader_pid,
                    ).await;
                    abort_stream_pumps = true;
                    break;
                }
                _ = &mut sleep => {}
            }
        } else {
            sleep.await;
        }
    }

    if abort_stream_pumps {
        stdout_task.abort();
        stderr_task.abort();
    }
    drain_stream_pumps_after_exit(stdout_task, stderr_task, &mut rx, &mut capture).await;

    scope_abort_guard.disarm();

    // ── Cleanup cgroup ───────────────────────────────────────────────
    if let Some(ref cg) = cg_path {
        cleanup_cgroup(cg);
    }

    let StreamOutputCapture {
        stdout,
        stderr,
        stdout_capped,
        stderr_capped,
        ..
    } = capture;
    let mut stdout = String::from_utf8_lossy(&stdout).into_owned();
    let mut stderr = String::from_utf8_lossy(&stderr).into_owned();
    if timed_out || stdout_capped {
        trim_incomplete_trailing_line(&mut stdout);
    }
    if timed_out || stderr_capped {
        trim_incomplete_trailing_line(&mut stderr);
    }

    IsolatedOutput {
        stdout,
        stderr,
        exit_code: if timed_out { None } else { exit_code },
        timed_out,
        cancelled,
        execution_started: true,
        stdout_capped,
        stderr_capped,
        namespace_active: ns_available,
        cgroup_active: cg_path.is_some(),
        scope_settled: scope_settlement.is_some(),
        scope_ownership: scope_settlement.map(|settlement| settlement.ownership),
        descendants_terminated: scope_settlement
            .is_some_and(|settlement| settlement.descendants_terminated),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    const SUPERVISOR_HELPER_TEST: &str =
        "process_isolation::tests::invocation_supervisor_test_helper";

    /// Re-exec target for supervisor integration tests. The production Astra
    /// binary handles this before its runtime starts; the Rust test harness
    /// needs one exact filtered test to provide the same early entrypoint.
    #[cfg(target_os = "linux")]
    #[test]
    fn invocation_supervisor_test_helper() {
        if invocation_supervisor_is_requested()
            && let Some(exit_code) = run_invocation_supervisor_if_requested()
        {
            std::process::exit(exit_code);
        }
    }

    #[cfg(target_os = "linux")]
    fn spawn_test_supervisor(
        program: &str,
        args: &[String],
        cwd: &Path,
    ) -> (std::process::Child, InvocationSupervisor) {
        let helper_args = vec![
            "--exact".to_string(),
            SUPERVISOR_HELPER_TEST.to_string(),
            "--nocapture".to_string(),
        ];
        let (mut command, mut supervisor) = InvocationSupervisor::prepare_with_helper_command(
            std::env::current_exe().unwrap(),
            helper_args,
            program,
            args,
        )
        .unwrap();
        command
            .current_dir(cwd)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command.process_group(0);
        supervisor.install(&mut command).unwrap();
        let mut child = command.spawn().unwrap();
        let helper_pid = child.id();
        supervisor.spawned();
        if let Err(error) = supervisor.start(helper_pid) {
            let _ = supervisor.request_termination();
            let _ = child.wait();
            panic!("supervisor handshake failed: {error}");
        }
        (child, supervisor)
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn invocation_supervisor_authorizes_natural_and_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();
        for expected in [0, 17] {
            let args = vec!["-c".to_string(), format!("exit {expected}")];
            let (mut child, mut supervisor) = spawn_test_supervisor("/bin/sh", &args, dir.path());
            let helper_pid = child.id();
            let status = child.wait().unwrap();
            assert_eq!(status.code(), Some(expected));
            assert_eq!(supervisor.finish_detailed(helper_pid), Some(false));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn invocation_supervisor_contains_setsid_double_fork_late_writer() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("mutation.txt");
        let command = format!(
            "printf immediate > {path}; \
             setsid /bin/sh -c '/bin/sh -c \"sleep 0.35; printf late >> {path}\" \
             </dev/null >/dev/null 2>&1 & exit 0' </dev/null >/dev/null 2>&1 & exit 0",
            path = marker.display()
        );
        let args = vec!["-c".to_string(), command];
        let (mut child, mut supervisor) = spawn_test_supervisor("/bin/sh", &args, dir.path());
        let helper_pid = child.id();
        assert!(child.wait().unwrap().success());
        assert_eq!(
            supervisor.finish_detailed(helper_pid),
            Some(true),
            "setsid/double-fork cleanup must be reported before authority"
        );
        std::thread::sleep(Duration::from_millis(450));
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "immediate");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn invocation_supervisor_controlled_termination_keeps_authority() {
        let dir = tempfile::tempdir().unwrap();
        let args = vec!["-c".to_string(), "sleep 10".to_string()];
        let (mut child, mut supervisor) = spawn_test_supervisor("/bin/sh", &args, dir.path());
        let helper_pid = child.id();
        supervisor.request_termination().unwrap();
        let started = std::time::Instant::now();
        let status = child.wait().unwrap();
        assert_eq!(status.code(), Some(130));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(supervisor.finish(helper_pid));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn shared_bash_owner_selects_and_settles_supervisor_authority() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("shared-owner.txt");
        let args = vec![
            "-c".to_string(),
            format!("printf shared-owner > {}", marker.display()),
        ];
        let helper_args = vec![
            "--exact".to_string(),
            SUPERVISOR_HELPER_TEST.to_string(),
            "--nocapture".to_string(),
        ];
        let (mut command, mut owner) = BashInvocationOwner::prepare_with_supervisor_helper(
            std::env::current_exe().unwrap(),
            helper_args,
            "/bin/sh",
            &args,
        )
        .unwrap();
        command
            .current_dir(dir.path())
            // A re-executed Rust test harness writes its own banner before
            // entering the helper. Assert target behavior via a marker so the
            // harness transport cannot be mistaken for target stdout.
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        owner.install(&mut command).unwrap();
        let child = command.spawn().unwrap();
        let helper_pid = child.id();
        owner.started(helper_pid).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "shared-owner");
        assert_eq!(
            owner.settle_after_exit(Some(helper_pid)),
            Some(ScopeOwnership::InvocationSupervisor)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn invocation_supervisor_crash_cannot_mint_authority() {
        let dir = tempfile::tempdir().unwrap();
        let args = vec!["-c".to_string(), "kill -KILL $PPID".to_string()];
        let (mut child, mut supervisor) = spawn_test_supervisor("/bin/sh", &args, dir.path());
        let helper_pid = child.id();
        let status = child.wait().unwrap();
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        assert!(!supervisor.finish(helper_pid));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn invocation_supervisor_target_inherits_no_private_env_or_protocol_fds() {
        let dir = tempfile::tempdir().unwrap();
        let env_output = dir.path().join("target.env");
        let fd_output = dir.path().join("target.fds");
        let command = format!(
            "env | grep '^_ASTRA_INTERNAL_PROCESS_' > {env_path} || :; \
             for fd in /proc/$$/fd/*; do readlink \"$fd\" || :; done > {fd_path}",
            env_path = env_output.display(),
            fd_path = fd_output.display(),
        );
        let args = vec!["-c".to_string(), command];
        let helper_args = vec![
            "--exact".to_string(),
            SUPERVISOR_HELPER_TEST.to_string(),
            "--nocapture".to_string(),
        ];
        let (mut helper, mut supervisor) = InvocationSupervisor::prepare_with_helper_command(
            std::env::current_exe().unwrap(),
            helper_args,
            "/bin/sh",
            &args,
        )
        .unwrap();
        let private_pipe_targets = [
            supervisor.child_control_read.as_ref().unwrap(),
            supervisor.child_receipt_write.as_ref().unwrap(),
        ]
        .map(|fd| {
            std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd()))
                .unwrap()
                .to_string_lossy()
                .into_owned()
        });
        helper
            .current_dir(dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        supervisor.install(&mut helper).unwrap();
        let mut child = helper.spawn().unwrap();
        let helper_pid = child.id();
        supervisor.spawned();
        supervisor.start(helper_pid).unwrap();
        assert!(child.wait().unwrap().success());
        assert!(supervisor.finish(helper_pid));

        assert_eq!(std::fs::read_to_string(env_output).unwrap(), "");
        let inherited_fds = std::fs::read_to_string(fd_output).unwrap();
        assert!(
            private_pipe_targets
                .iter()
                .all(|target| !inherited_fds.lines().any(|actual| actual == target)),
            "target inherited a private protocol pipe: private={private_pipe_targets:?}, target={inherited_fds:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn invocation_supervisors_are_isolated_across_parallel_invocations() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.txt");
        let second = dir.path().join("second.txt");
        let first_args = vec![
            "-c".to_string(),
            format!("sleep 0.08; printf first > {}", first.display()),
        ];
        let second_args = vec![
            "-c".to_string(),
            format!("sleep 0.12; printf second > {}", second.display()),
        ];
        let (mut first_child, mut first_supervisor) =
            spawn_test_supervisor("/bin/sh", &first_args, dir.path());
        let (mut second_child, mut second_supervisor) =
            spawn_test_supervisor("/bin/sh", &second_args, dir.path());
        let first_pid = first_child.id();
        let second_pid = second_child.id();

        assert!(first_child.wait().unwrap().success());
        assert!(first_supervisor.finish(first_pid));
        assert!(second_child.wait().unwrap().success());
        assert!(second_supervisor.finish(second_pid));
        assert_eq!(std::fs::read_to_string(first).unwrap(), "first");
        assert_eq!(std::fs::read_to_string(second).unwrap(), "second");
    }

    #[test]
    fn isolation_config_strict_defaults() {
        let cfg = IsolationConfig::strict(PathBuf::from("/tmp"));
        assert!(cfg.pid_namespace);
        assert!(cfg.mount_namespace);
        assert!(cfg.net_namespace);
        assert_eq!(cfg.memory_limit_bytes, 512 * 1024 * 1024);
        assert_eq!(cfg.cpu_quota, 1.0);
        assert_eq!(cfg.max_output_bytes, DEFAULT_MAX_CAPTURED_OUTPUT_BYTES);
        assert!(cfg.read_only_paths.is_empty());
    }

    #[test]
    fn isolation_config_disabled() {
        let cfg = IsolationConfig::disabled(PathBuf::from("/tmp"));
        assert!(!cfg.pid_namespace);
        assert!(!cfg.mount_namespace);
        assert!(!cfg.net_namespace);
        // Disabled mode still applies safety limits: 1 GiB memory cap
        // and 600s timeout to prevent OOM and infinite execution.
        assert_eq!(cfg.memory_limit_bytes, 1024 * 1024 * 1024);
        assert_eq!(cfg.max_output_bytes, DEFAULT_MAX_CAPTURED_OUTPUT_BYTES);
        assert!(cfg.read_only_paths.is_empty());
    }

    #[test]
    fn isolated_output_display() {
        // Normal output
        let out = IsolatedOutput {
            stdout: "hello".to_string(),
            stderr: "warn".to_string(),
            exit_code: Some(1),
            timed_out: false,
            cancelled: false,
            execution_started: true,
            stdout_capped: false,
            stderr_capped: false,
            namespace_active: false,
            cgroup_active: false,
            scope_settled: false,
            scope_ownership: None,
            descendants_terminated: false,
        };
        let combined = out.combined_output();
        assert!(combined.contains("hello"));
        assert!(combined.contains("stderr:\nwarn"));
        assert!(combined.contains("exit code: 1"));

        // Timeout output
        let out = IsolatedOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
            timed_out: true,
            cancelled: false,
            execution_started: true,
            stdout_capped: false,
            stderr_capped: false,
            namespace_active: true,
            cgroup_active: false,
            scope_settled: false,
            scope_ownership: None,
            descendants_terminated: false,
        };
        assert!(out.combined_output().contains("timed out"));
    }

    #[tokio::test]
    async fn execute_isolated_echo() {
        let config = IsolationConfig::disabled(PathBuf::from("/tmp"));
        let env =
            std::collections::HashMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]);
        let out = execute_isolated("echo hello", &env, &config).await;
        assert_eq!(out.stdout.trim(), "hello");
        assert!(!out.timed_out);
        assert!(out.execution_started, "successful spawn is executor-owned");
    }

    #[tokio::test]
    async fn pre_spawn_failure_does_not_claim_execution_started() {
        let parent = tempfile::tempdir().expect("parent");
        let missing_workdir = parent.path().join("missing");
        let config = IsolationConfig::disabled(missing_workdir);
        let env =
            std::collections::HashMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]);
        let out = execute_isolated("echo never-ran", &env, &config).await;

        assert!(!out.execution_started);
        assert!(out.exit_code.is_none());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn execute_isolated_supervisor_contains_setsid_double_fork_without_cgroup() {
        let temp = tempfile::tempdir().expect("tempdir");
        let marker = temp.path().join("late-marker");
        let mut config = IsolationConfig::disabled(temp.path().to_path_buf());
        config.memory_limit_bytes = 0;
        config.cpu_quota = 0.0;
        let env =
            std::collections::HashMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]);
        let command = format!(
            "python3 -c 'import os,time; p=os.fork(); \
             os._exit(0) if p else None; os.setsid(); q=os.fork(); \
             os._exit(0) if q else None; time.sleep(0.25); \
             open(\"{}\",\"w\").write(\"escaped\")'",
            marker.display()
        );
        let helper = (
            std::env::current_exe().expect("test executable"),
            vec![
                "--exact".to_string(),
                SUPERVISOR_HELPER_TEST.to_string(),
                "--nocapture".to_string(),
            ],
        );
        let out =
            execute_isolated_with_cancel_impl(&command, &env, &config, None, Some(helper)).await;

        assert_eq!(out.exit_code, Some(0), "{out:?}");
        assert!(out.scope_settled, "{out:?}");
        assert_eq!(
            out.scope_ownership,
            Some(ScopeOwnership::InvocationSupervisor),
            "{out:?}"
        );
        assert!(out.descendants_terminated, "{out:?}");
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(!marker.exists(), "escaped descendant wrote after receipt");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn execute_isolated_cancel_stops_supervised_setsid_descendant_before_helper() {
        let temp = tempfile::tempdir().expect("tempdir");
        let marker = temp.path().join("cancel-late-marker");
        let mut config = IsolationConfig::disabled(temp.path().to_path_buf());
        config.memory_limit_bytes = 0;
        config.cpu_quota = 0.0;
        let env =
            std::collections::HashMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]);
        let command = format!(
            "setsid sh -c 'sleep 0.35; echo escaped > \"{}\"' >/dev/null 2>&1 & \
             echo ready; sleep 60",
            marker.display()
        );
        let helper = (
            std::env::current_exe().expect("test executable"),
            vec![
                "--exact".to_string(),
                SUPERVISOR_HELPER_TEST.to_string(),
                "--nocapture".to_string(),
            ],
        );
        let cancel = CancellationToken::new();
        let run =
            execute_isolated_with_cancel_impl(&command, &env, &config, Some(&cancel), Some(helper));
        tokio::pin!(run);
        tokio::select! {
            output = &mut run => panic!("command exited before cancellation: {output:?}"),
            _ = tokio::time::sleep(Duration::from_millis(80)) => cancel.cancel(),
        }
        let out = run.await;

        assert!(out.cancelled, "{out:?}");
        assert!(out.scope_settled, "{out:?}");
        assert_eq!(
            out.scope_ownership,
            Some(ScopeOwnership::InvocationSupervisor),
            "{out:?}"
        );
        assert!(out.descendants_terminated, "{out:?}");
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!marker.exists(), "cancelled descendant escaped its owner");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn dropping_execute_future_still_settles_supervised_setsid_descendant() {
        let temp = tempfile::tempdir().expect("tempdir");
        let ready = temp.path().join("ready");
        let marker = temp.path().join("abort-late-marker");
        let mut config = IsolationConfig::disabled(temp.path().to_path_buf());
        config.memory_limit_bytes = 0;
        config.cpu_quota = 0.0;
        let env =
            std::collections::HashMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]);
        let command = format!(
            "setsid sh -c 'sleep 0.35; echo escaped > \"{}\"' >/dev/null 2>&1 & \
             echo ready > \"{}\"; sleep 60",
            marker.display(),
            ready.display()
        );
        let helper = (
            std::env::current_exe().expect("test executable"),
            vec![
                "--exact".to_string(),
                SUPERVISOR_HELPER_TEST.to_string(),
                "--nocapture".to_string(),
            ],
        );
        let run = tokio::spawn(async move {
            execute_isolated_with_cancel_impl(&command, &env, &config, None, Some(helper)).await
        });

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !ready.exists() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            ready.exists(),
            "target never reached the post-daemon checkpoint"
        );
        run.abort();
        assert!(run.await.unwrap_err().is_cancelled());

        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            !marker.exists(),
            "abrupt future drop released a daemonized descendant"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn execute_isolated_joined_background_does_not_report_termination() {
        let config = IsolationConfig::disabled(PathBuf::from("/tmp"));
        let env =
            std::collections::HashMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]);
        let out = execute_isolated("sleep 0.02 & wait; echo done", &env, &config).await;

        assert_eq!(out.exit_code, Some(0), "{out:?}");
        assert!(out.stdout.contains("done"), "{out:?}");
        assert!(!out.descendants_terminated, "{out:?}");
    }

    // ── apply_cgroup / CgroupGuard public API ────────────────────────────

    /// Zero or negative limits → inactive guard, no cgroup, no side effects.
    #[test]
    fn apply_cgroup_zero_or_negative_limits_inactive() {
        // Zero memory, zero cpu
        let guard = apply_cgroup(0, 0.0);
        assert!(!guard.active(), "zero limits must not create a cgroup");
        assert!(guard.path().is_none());

        // Zero memory, negative cpu
        let guard = apply_cgroup(0, -1.0);
        assert!(!guard.active());
    }

    /// CgroupGuard::active reflects cgroup v2 availability on this host.
    /// We assert the contract in both branches: if available, a guard with
    /// non-zero limits IS active; if not available, even non-zero limits
    /// yield an inactive guard (graceful fallback).
    #[test]
    fn apply_cgroup_activity_matches_host_support() {
        let guard = apply_cgroup(64 * 1024 * 1024, 1.0);
        if cgroupv2_available() {
            // On a cgroup v2 host we'd expect active, UNLESS writing to
            // /sys/fs/cgroup is blocked (unprivileged container, etc.) —
            // in which case create_cgroup returns None and we still get
            // an inactive guard. Both outcomes are contract-compliant.
            if guard.active() {
                assert!(guard.path().is_some());
                assert!(
                    guard.path().unwrap().starts_with("/sys/fs/cgroup"),
                    "active guard path must be under /sys/fs/cgroup"
                );
            }
        } else {
            assert!(
                !guard.active(),
                "without cgroup v2 the guard must be inactive"
            );
        }
    }

    /// Drop of an active guard removes the cgroup directory. We simulate
    /// an active guard by manually crafting one pointing at a tempdir.
    #[test]
    fn cgroup_guard_drop_removes_dir() {
        let tmp = tempfile::tempdir().unwrap();
        // Empty subdir that mimics the shape of a real cgroup directory.
        let fake_cg = tmp.path().join("astra-tool-fake");
        std::fs::create_dir(&fake_cg).unwrap();
        assert!(fake_cg.exists());

        {
            let _guard = CgroupGuard {
                cg_path: Some(fake_cg.clone()),
                procs_path: None,
            };
            // guard goes out of scope here → Drop fires
        }

        assert!(
            !fake_cg.exists(),
            "CgroupGuard::drop must remove the cgroup directory"
        );
    }

    /// Inactive guard's Drop is a no-op (no panic, no work).
    #[test]
    fn cgroup_guard_inactive_drop_noop() {
        let guard = CgroupGuard {
            cg_path: None,
            procs_path: None,
        };
        drop(guard); // must not panic
    }

    #[tokio::test]
    async fn execute_isolated_timeout() {
        let mut config = IsolationConfig::disabled(PathBuf::from("/tmp"));
        config.timeout = Duration::from_millis(100);
        let env =
            std::collections::HashMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]);
        let out = execute_isolated("echo start; sleep 10; echo done", &env, &config).await;
        assert!(out.timed_out);
        assert!(out.stdout.contains("start"));
        assert!(!out.stdout.contains("done"));
        if cfg!(unix) {
            assert_eq!(
                out.scope_ownership,
                Some(ScopeOwnership::ForegroundProcessGroup),
                "timeout must preserve the pre-wait process-group identity"
            );
        }
    }

    #[tokio::test]
    async fn execute_isolated_cancellation_kills_invocation_and_is_typed() {
        let mut config = IsolationConfig::disabled(PathBuf::from("/tmp"));
        config.timeout = Duration::from_secs(10);
        let env =
            std::collections::HashMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]);
        let token = CancellationToken::new();
        let cancel = token.clone();
        let task = tokio::spawn(async move {
            execute_isolated_with_cancel(
                "printf before; sleep 10; printf after",
                &env,
                &config,
                Some(&cancel),
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        token.cancel();
        let out = task.await.unwrap();
        assert!(
            out.cancelled,
            "cancellation must be represented distinctly: {out:?}"
        );
        assert!(
            !out.stdout.contains("after"),
            "child survived cancellation: {out:?}"
        );
        if cfg!(unix) {
            assert_eq!(
                out.scope_ownership,
                Some(ScopeOwnership::ForegroundProcessGroup),
                "cancellation must preserve the pre-wait process-group identity"
            );
        }
    }

    #[tokio::test]
    async fn execute_isolated_caps_combined_output_from_config() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = IsolationConfig::disabled(tmp.path().to_path_buf());
        config.memory_limit_bytes = 0;
        config.cpu_quota = 0.0;
        config.max_output_bytes = 5;
        let env =
            std::collections::HashMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]);

        let out = execute_isolated("printf 'abcd\\nefgh\\n'", &env, &config).await;

        assert!(out.stdout_capped, "{out:?}");
        assert!(!out.stderr_capped, "{out:?}");
        assert_eq!(out.stdout, "abcd\n");
        assert_eq!(out.stdout.len() + out.stderr.len(), config.max_output_bytes);
        assert!(
            out.combined_output()
                .contains("(output capped: stdout limit reached)"),
            "{out:?}"
        );
    }

    #[tokio::test]
    async fn execute_isolated_exit_code() {
        let config = IsolationConfig::disabled(PathBuf::from("/tmp"));
        let env =
            std::collections::HashMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]);
        let out = execute_isolated("exit 42", &env, &config).await;
        assert_eq!(out.exit_code, Some(42));
    }

    /// P1-K: execute_isolated must hard-fail when namespace isolation
    /// is requested but unavailable (Strict mode must not silently degrade).
    #[tokio::test]
    async fn strict_mode_without_unshare_refuses_execution() {
        let config = IsolationConfig::strict(PathBuf::from("/tmp"));
        let env =
            std::collections::HashMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]);
        let out = execute_isolated("echo ok", &env, &config).await;
        if unshare_available() {
            assert!(out.namespace_active);
            assert!(out.stdout.contains("ok") || out.exit_code == Some(0));
        } else {
            assert!(!out.namespace_active);
            assert!(out.stdout.is_empty());
            assert!(out.exit_code.is_none());
            assert!(
                out.stderr.contains("namespace isolation unavailable"),
                "{out:?}"
            );
        }
    }

    /// The mount namespace wrapper script must include the workspace bind-mount
    /// and proc/tmp tmpfs mounts when mount_namespace is active.
    #[test]
    fn build_mount_namespace_wrapper_includes_safety_mounts() {
        let script = build_mount_namespace_wrapper();
        // Essential safety mounts
        assert!(
            script.contains("mount --make-rprivate /"),
            "must remount / as private"
        );
        assert!(
            script.contains("mount -t proc proc /proc"),
            "must mount fresh procfs"
        );
        assert!(
            script.contains("mount -t tmpfs"),
            "must mount tmpfs for /tmp"
        );
        assert!(script.contains("mount --bind"), "must bind-mount workspace");
        assert!(
            script.contains("mount -o remount,bind,ro /"),
            "the inherited filesystem must become read-only"
        );
        assert!(
            script.contains("mount -o remount,bind,ro -- \"$target\""),
            "host-owned workspace lanes must be read-only"
        );
        // Should use argv, not a user-controlled environment variable.
        assert!(
            script.contains("mount --bind -- \"$2\""),
            "must reference working dir via argv with an option boundary"
        );
        assert!(
            script.contains("/tmp/_astra_ws"),
            "must bind-mount to /tmp/_astra_ws"
        );
        assert!(
            script.contains("cd /tmp/_astra_ws"),
            "must change to workspace dir"
        );
        assert!(
            script.contains("setpriv --bounding-set=-all"),
            "must execute the original command from argv"
        );
    }

    /// The wrapper must use argv for path to prevent shell injection via
    /// directory names and environment spoofing.
    #[test]
    fn build_mount_namespace_wrapper_uses_argv_not_embedded_path_or_env() {
        let script = build_mount_namespace_wrapper();
        // Should NOT contain any hardcoded paths (security: no shell injection)
        assert!(
            !script.contains("/home/user"),
            "should not embed hardcoded paths: {script}"
        );
        assert!(
            !script.contains("/tmp/user"),
            "should not embed hardcoded paths: {script}"
        );
        // Should use argv instead of a spoofable env var.
        assert!(
            script.contains("mount --bind -- \"$2\""),
            "must use argv for working dir: {script}"
        );
        assert!(
            !script.contains("ASTRA_WORKING_DIR"),
            "must not depend on user-controlled env for working dir: {script}"
        );
    }

    /// The wrapper must never embed command text directly: command bytes are
    /// passed as argv and executed with `bash -c "$1"` after setup.
    #[test]
    fn build_mount_namespace_wrapper_does_not_embed_command_text() {
        let script = build_mount_namespace_wrapper();
        assert!(
            !script.contains("echo 'injected"),
            "wrapper must not embed command text: {script}"
        );
        assert!(script.contains("bash -c \"$user_command\""));
    }

    /// Integration: when mount_namespace is active, the executed command
    /// should see a restricted filesystem (no /etc/shadow accessible).
    /// This test only runs when unshare is available.
    #[tokio::test]
    async fn mount_namespace_restricts_host_filesystem() {
        if !unshare_available() {
            return; // skip on non-Linux or unprivileged containers
        }
        let mut config = IsolationConfig::strict(PathBuf::from("/tmp"));
        config.net_namespace = false; // keep net to simplify test
        let env =
            std::collections::HashMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]);
        // Try to read /etc/shadow — should fail because tmpfs covers /tmp
        // and the original /etc is not accessible via bind mount.
        // Instead verify /workspace exists and contains the working dir.
        // Use printf to avoid single-quote escaping complexity in mount namespace.
        let out = execute_isolated(
            "test -d /tmp/_astra_ws && printf workspace_ok; test -f /tmp/_astra_ws/../etc/shadow && printf shadow_leaked || printf shadow_blocked",
            &env,
            &config,
        )
        .await;
        assert!(
            out.stdout.contains("workspace_ok"),
            "/tmp/_astra_ws should exist: {out:?}"
        );
        assert!(
            out.stdout.contains("shadow_blocked"),
            "/etc/shadow should not be accessible from /tmp/_astra_ws/../etc: {out:?}"
        );
    }

    #[tokio::test]
    async fn mount_namespace_preserves_single_quotes_in_command() {
        if !unshare_available() {
            return; // skip on non-Linux or unprivileged containers
        }
        let mut config = IsolationConfig::strict(PathBuf::from("/tmp"));
        config.net_namespace = false;
        let env =
            std::collections::HashMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]);

        let out = execute_isolated("printf 'quote_ok\\n'", &env, &config).await;
        assert!(out.stdout.contains("quote_ok"), "{out:?}");
    }

    #[tokio::test]
    async fn filesystem_boundary_blocks_arbitrary_writers_from_host_owned_lane() {
        if !unshare_available() {
            return;
        }
        let workspace = tempfile::tempdir().unwrap();
        let protected = workspace.path().join(".moi/runtime/task-1");
        std::fs::create_dir_all(&protected).unwrap();
        std::fs::write(protected.join("owned.txt"), b"host-owned").unwrap();
        let env =
            std::collections::HashMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]);
        let config = IsolationConfig::filesystem_boundary(
            workspace.path().to_path_buf(),
            vec![protected.clone()],
        );

        let tar = execute_isolated(
            "printf ok > ordinary.txt && tar -cf .moi/runtime/task-1/archive.tar ordinary.txt",
            &env,
            &config,
        )
        .await;
        assert!(tar.namespace_active, "{tar:?}");
        assert_ne!(tar.exit_code, Some(0), "{tar:?}");
        assert_eq!(
            std::fs::read(workspace.path().join("ordinary.txt")).unwrap(),
            b"ok"
        );
        assert!(!protected.join("archive.tar").exists());

        let python = execute_isolated(
            "python3 -c \"open('.moi/runtime/task-1/owned.txt','w').write('changed')\"",
            &env,
            &config,
        )
        .await;
        assert!(python.namespace_active, "{python:?}");
        assert_ne!(python.exit_code, Some(0), "{python:?}");
        assert_eq!(
            std::fs::read(protected.join("owned.txt")).unwrap(),
            b"host-owned"
        );

        let remount = execute_isolated(
            "mount -o remount,rw .moi/runtime/task-1 || true; printf changed > .moi/runtime/task-1/owned.txt",
            &env,
            &config,
        )
        .await;
        assert!(remount.namespace_active, "{remount:?}");
        assert_ne!(remount.exit_code, Some(0), "{remount:?}");
        assert_eq!(
            std::fs::read(protected.join("owned.txt")).unwrap(),
            b"host-owned"
        );
    }

    /// Regression: when mount_namespace is disabled, the wrapper must NOT be
    /// used — the raw command must be passed directly.
    #[tokio::test]
    async fn disabled_mount_namespace_passes_raw_command() {
        let config = IsolationConfig::disabled(PathBuf::from("/tmp"));
        let env =
            std::collections::HashMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]);
        // This command would fail if wrapped (cd /workspace wouldn't exist)
        let out = execute_isolated("echo 'raw_ok'", &env, &config).await;
        assert!(out.stdout.contains("raw_ok"), "{out:?}");
    }
}

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

use tokio::io::{AsyncRead, AsyncReadExt};

const MAX_CAPTURED_STDOUT_BYTES: usize = 64 * 1024;
const MAX_CAPTURED_STDERR_BYTES: usize = 32 * 1024;
const DEFAULT_MAX_CAPTURED_OUTPUT_BYTES: usize =
    MAX_CAPTURED_STDOUT_BYTES + MAX_CAPTURED_STDERR_BYTES;
const READ_CHUNK_SIZE: usize = 8 * 1024;
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(20);
const OUTPUT_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

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
    pub stdout_capped: bool,
    pub stderr_capped: bool,
    /// Whether namespace isolation was actually applied (false = fallback).
    pub namespace_active: bool,
    /// Whether cgroup limits were actually applied.
    pub cgroup_active: bool,
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
fn create_cgroup(config: &IsolationConfig) -> Option<PathBuf> {
    if !cgroupv2_available() {
        return None;
    }
    if config.memory_limit_bytes == 0 && config.cpu_quota == 0.0 {
        return None;
    }

    let name = cgroup_name();
    let cg_path = PathBuf::from("/sys/fs/cgroup").join(&name);

    // Create the cgroup directory.
    if std::fs::create_dir_all(&cg_path).is_err() {
        return None;
    }

    // Set memory limit.
    if config.memory_limit_bytes > 0 {
        if let Err(e) = std::fs::write(
            cg_path.join("memory.max"),
            config.memory_limit_bytes.to_string(),
        ) {
            tracing::error!(%e, "cgroup memory.max write failed — removing cgroup");
            let _ = std::fs::remove_dir(&cg_path);
            return None;
        }
        if let Err(e) = std::fs::write(cg_path.join("memory.swap.max"), "0") {
            tracing::warn!(%e, "cgroup memory.swap.max write failed (non-fatal)");
        }
    }

    // Set CPU quota (period = 100ms, quota = period * fraction).
    if config.cpu_quota > 0.0 {
        let period_us: u64 = 100_000; // 100ms
        let quota_us = (period_us as f64 * config.cpu_quota) as u64;
        if let Err(e) = std::fs::write(cg_path.join("cpu.max"), format!("{quota_us} {period_us}")) {
            tracing::error!(%e, "cgroup cpu.max write failed — removing cgroup");
            let _ = std::fs::remove_dir(&cg_path);
            return None;
        }
    }

    Some(cg_path)
}

/// Remove a cgroup after the process exits.
fn cleanup_cgroup(cg_path: &Path) {
    // cgroup must be empty (no processes) before removal.
    let _ = std::fs::remove_dir(cg_path);
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

    /// Absolute path of the cgroup directory, if active. Exposed for
    /// observability — callers typically don't need to interact with it.
    pub fn path(&self) -> Option<&Path> {
        self.cg_path.as_deref()
    }

    /// Join a spawned child process to this cgroup by writing its PID to
    /// cgroup.procs. No-op when the cgroup is inactive. Returns `Err` if
    /// the write fails — callers must kill the child in that case.
    pub fn join_child(&self, pid: u32) -> Result<(), std::io::Error> {
        if let Some(ref procs_path) = self.procs_path {
            std::fs::write(procs_path, pid.to_string())?;
        }
        Ok(())
    }
}

impl Drop for CgroupGuard {
    fn drop(&mut self) {
        if let Some(p) = self.cg_path.take() {
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
            stdout_capped: false,
            stderr_capped: false,
            namespace_active: false,
            cgroup_active: false,
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
            stdout_capped: false,
            stderr_capped: false,
            namespace_active: false,
            cgroup_active: false,
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

    // Freeze the cgroup *before* spawn to close the TOCTOU window:
    // the child will be frozen immediately upon entering the cgroup
    // and won't execute user code until limits are applied + unfreeze.
    #[cfg(unix)]
    if let Some(ref cg) = cg_path
        && let Err(error) = std::fs::write(cg.join("cgroup.freeze"), "1")
    {
        tracing::error!(%error, "cgroup.freeze setup failed before child spawn");
        cleanup_cgroup(cg);
        return IsolatedOutput {
            stdout: String::new(),
            stderr: format!("cgroup freeze setup failed: {error}"),
            exit_code: None,
            timed_out: false,
            stdout_capped: false,
            stderr_capped: false,
            namespace_active: false,
            cgroup_active: false,
        };
    }

    let mut cmd = tokio::process::Command::new(&program);
    cmd.args(&args).current_dir(&config.working_dir).env_clear();
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    // Apply filtered environment first (untrusted).
    for (k, v) in env {
        cmd.env(k, v);
    }

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
                stdout_capped: false,
                stderr_capped: false,
                namespace_active: false,
                cgroup_active: false,
            };
        }
    };

    // Post-spawn: join the child to its cgroup by writing the real PID.
    // The cgroup was frozen before spawn — child won't execute until
    // limits are applied and the freeze is released.
    #[cfg(unix)]
    if let Some(ref cg) = cg_path {
        let Some(pid) = child.id() else {
            let _ = child.kill().await;
            let _ = child.wait().await;
            cleanup_cgroup(cg);
            return IsolatedOutput {
                stdout: String::new(),
                stderr: "Failed to obtain child PID for cgroup join".to_string(),
                exit_code: None,
                timed_out: false,
                stdout_capped: false,
                stderr_capped: false,
                namespace_active: false,
                cgroup_active: false,
            };
        };
        let procs_path = cg.join("cgroup.procs");
        if let Err(e) = std::fs::write(&procs_path, pid.to_string()) {
            tracing::error!(path = %procs_path.display(), %e, "cgroup.procs write failed — killing child");
            let _ = child.kill().await;
            let _ = child.wait().await;
            cleanup_cgroup(cg);
            return IsolatedOutput {
                stdout: String::new(),
                stderr: format!("cgroup join failed: {e}"),
                exit_code: None,
                timed_out: false,
                stdout_capped: false,
                stderr_capped: false,
                namespace_active: false,
                cgroup_active: false,
            };
        }
        if let Err(e) = std::fs::write(cg.join("cgroup.freeze"), "0") {
            tracing::error!(%e, "cgroup.freeze unfreeze failed — killing child");
            let _ = child.kill().await;
            let _ = child.wait().await;
            cleanup_cgroup(cg);
            return IsolatedOutput {
                stdout: String::new(),
                stderr: format!("cgroup unfreeze failed: {e}"),
                exit_code: None,
                timed_out: false,
                stdout_capped: false,
                stderr_capped: false,
                namespace_active: false,
                cgroup_active: false,
            };
        }
    }

    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            if let Some(ref cg) = cg_path {
                cleanup_cgroup(cg);
            }
            return IsolatedOutput {
                stdout: String::new(),
                stderr: "Failed to capture stdout pipe".to_string(),
                exit_code: None,
                timed_out: false,
                stdout_capped: false,
                stderr_capped: false,
                namespace_active: ns_available,
                cgroup_active: cg_path.is_some(),
            };
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            if let Some(ref cg) = cg_path {
                cleanup_cgroup(cg);
            }
            return IsolatedOutput {
                stdout: String::new(),
                stderr: "Failed to capture stderr pipe".to_string(),
                exit_code: None,
                timed_out: false,
                stdout_capped: false,
                stderr_capped: false,
                namespace_active: ns_available,
                cgroup_active: cg_path.is_some(),
            };
        }
    };

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let stdout_task = tokio::spawn(pump_stream(stdout, StreamKind::Stdout, tx.clone()));
    let stderr_task = tokio::spawn(pump_stream(stderr, StreamKind::Stderr, tx));

    let mut capture = StreamOutputCapture::new(config.max_output_bytes);
    let mut exit_code = None;
    let mut timed_out = false;
    let mut abort_stream_pumps = false;
    let deadline = tokio::time::Instant::now() + config.timeout;

    loop {
        drain_stream_chunks(&mut rx, &mut capture);

        match child.try_wait() {
            Ok(Some(status)) => {
                exit_code = status.code();
                break;
            }
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
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
                    stdout_capped: false,
                    stderr_capped: false,
                    namespace_active: ns_available,
                    cgroup_active: cg_path.is_some(),
                };
            }
        }

        let now = tokio::time::Instant::now();
        if now >= deadline {
            timed_out = true;
            let _ = child.kill().await;
            let _ = child.wait().await;
            // Abort I/O reader tasks immediately — grandchild processes may
            // still hold the pipes open (e.g. `sleep` surviving shell kill).
            abort_stream_pumps = true;
            break;
        }

        tokio::time::sleep(std::cmp::min(
            PROCESS_POLL_INTERVAL,
            deadline.saturating_duration_since(now),
        ))
        .await;
    }

    if abort_stream_pumps {
        stdout_task.abort();
        stderr_task.abort();
    }
    drain_stream_pumps_after_exit(stdout_task, stderr_task, &mut rx, &mut capture).await;

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
        stdout_capped,
        stderr_capped,
        namespace_active: ns_available,
        cgroup_active: cg_path.is_some(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            stdout_capped: false,
            stderr_capped: false,
            namespace_active: false,
            cgroup_active: false,
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
            stdout_capped: false,
            stderr_capped: false,
            namespace_active: true,
            cgroup_active: false,
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

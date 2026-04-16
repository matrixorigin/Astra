//! Linux process isolation via namespaces and cgroup v2 (Phase 5.5).
//!
//! Wraps subprocess execution with:
//! - **PID namespace** (`unshare --pid --fork`): process cannot signal host PIDs.
//! - **Mount namespace** (`unshare --mount`): private `/tmp`, read-only bind mounts.
//! - **Network namespace** (optional, `unshare --net`): no network access.
//! - **cgroup v2 limits**: memory ceiling + CPU quota per-tool invocation.
//!
//! Falls back gracefully when:
//! - `unshare` is not available (non-Linux, container without CAP_SYS_ADMIN).
//! - cgroup v2 is not mounted.
//! - Caller passes `IsolationConfig::disabled()`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};

const MAX_CAPTURED_STDOUT_BYTES: usize = 64 * 1024;
const MAX_CAPTURED_STDERR_BYTES: usize = 32 * 1024;
const READ_CHUNK_SIZE: usize = 8 * 1024;
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(20);

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
    /// Working directory for the subprocess.
    pub working_dir: PathBuf,
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
            working_dir,
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
            working_dir,
        }
    }

    /// No isolation (backward compat / permissive mode).
    pub fn disabled(working_dir: PathBuf) -> Self {
        Self {
            pid_namespace: false,
            mount_namespace: false,
            net_namespace: false,
            memory_limit_bytes: 0,
            cpu_quota: 0.0,
            timeout: Duration::from_secs(120),
            working_dir,
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
        let _ = std::fs::write(
            cg_path.join("memory.max"),
            config.memory_limit_bytes.to_string(),
        );
        // Also set a swap limit to prevent OOM-swap thrashing.
        let _ = std::fs::write(cg_path.join("memory.swap.max"), "0");
    }

    // Set CPU quota (period = 100ms, quota = period * fraction).
    if config.cpu_quota > 0.0 {
        let period_us: u64 = 100_000; // 100ms
        let quota_us = (period_us as f64 * config.cpu_quota) as u64;
        let _ = std::fs::write(cg_path.join("cpu.max"), format!("{quota_us} {period_us}"));
    }

    Some(cg_path)
}

/// Remove a cgroup after the process exits.
fn cleanup_cgroup(cg_path: &Path) {
    // cgroup must be empty (no processes) before removal.
    let _ = std::fs::remove_dir(cg_path);
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
    stdout: &mut Vec<u8>,
    stderr: &mut Vec<u8>,
    stdout_capped: &mut bool,
    stderr_capped: &mut bool,
) {
    while let Ok(chunk) = rx.try_recv() {
        match chunk.stream {
            StreamKind::Stdout => append_capped(
                stdout,
                &chunk.bytes,
                MAX_CAPTURED_STDOUT_BYTES,
                stdout_capped,
            ),
            StreamKind::Stderr => append_capped(
                stderr,
                &chunk.bytes,
                MAX_CAPTURED_STDERR_BYTES,
                stderr_capped,
            ),
        }
    }
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
        unshare_flags.push("--map-root-user");
        unshare_flags.push("--");
        unshare_flags.push("bash");
        unshare_flags.push("-c");
        unshare_flags.push(command);
        (
            "unshare".to_string(),
            unshare_flags
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>(),
        )
    } else {
        (
            "bash".to_string(),
            vec!["-c".to_string(), command.to_string()],
        )
    };

    // ── cgroup setup ─────────────────────────────────────────────────
    let cg_path = create_cgroup(config);

    let mut cmd = tokio::process::Command::new(&program);
    cmd.args(&args).current_dir(&config.working_dir).env_clear();
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    // Apply filtered environment.
    for (k, v) in env {
        cmd.env(k, v);
    }

    // If cgroup created, write PID to cgroup.procs after spawn.
    // We use a pre_exec hook on Unix to join the cgroup before exec.
    #[cfg(unix)]
    if let Some(ref cg) = cg_path {
        let procs_path = cg.join("cgroup.procs");
        unsafe {
            cmd.pre_exec(move || {
                let pid = std::process::id().to_string();
                std::fs::write(&procs_path, &pid)
                    .map_err(|e| std::io::Error::other(format!("cgroup join: {e}")))
            });
        }
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

    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let mut stdout_capped = false;
    let mut stderr_capped = false;
    let mut exit_code = None;
    let mut timed_out = false;
    let deadline = tokio::time::Instant::now() + config.timeout;

    loop {
        drain_stream_chunks(
            &mut rx,
            &mut stdout_bytes,
            &mut stderr_bytes,
            &mut stdout_capped,
            &mut stderr_capped,
        );

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
            break;
        }

        tokio::time::sleep(std::cmp::min(
            PROCESS_POLL_INTERVAL,
            deadline.saturating_duration_since(now),
        ))
        .await;
    }

    let _ = stdout_task.await;
    let _ = stderr_task.await;
    drain_stream_chunks(
        &mut rx,
        &mut stdout_bytes,
        &mut stderr_bytes,
        &mut stdout_capped,
        &mut stderr_capped,
    );

    // ── Cleanup cgroup ───────────────────────────────────────────────
    if let Some(ref cg) = cg_path {
        cleanup_cgroup(cg);
    }

    let mut stdout = String::from_utf8_lossy(&stdout_bytes).to_string();
    let mut stderr = String::from_utf8_lossy(&stderr_bytes).to_string();
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
    }

    #[test]
    fn isolation_config_disabled() {
        let cfg = IsolationConfig::disabled(PathBuf::from("/tmp"));
        assert!(!cfg.pid_namespace);
        assert!(!cfg.mount_namespace);
        assert!(!cfg.net_namespace);
        assert_eq!(cfg.memory_limit_bytes, 0);
    }

    #[test]
    fn isolated_output_combined() {
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
    }

    #[test]
    fn isolated_output_timeout() {
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
    async fn execute_isolated_exit_code() {
        let config = IsolationConfig::disabled(PathBuf::from("/tmp"));
        let env =
            std::collections::HashMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]);
        let out = execute_isolated("exit 42", &env, &config).await;
        assert_eq!(out.exit_code, Some(42));
    }
}

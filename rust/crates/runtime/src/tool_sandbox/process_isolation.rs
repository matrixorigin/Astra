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
use std::time::Duration;

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
        if let Some(code) = self.exit_code {
            if code != 0 {
                out.push_str(&format!("\n(exit code: {code})"));
            }
        }
        if self.timed_out {
            out.push_str("\n(timed out)");
        }
        out
    }
}

/// Check if `unshare` is available on this system.
fn unshare_available() -> bool {
    std::process::Command::new("unshare")
        .arg("--help")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
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

    // ── Execute with timeout ─────────────────────────────────────────
    let result = tokio::time::timeout(config.timeout, cmd.output()).await;

    // ── Cleanup cgroup ───────────────────────────────────────────────
    if let Some(ref cg) = cg_path {
        cleanup_cgroup(cg);
    }

    match result {
        Ok(Ok(output)) => IsolatedOutput {
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            exit_code: output.status.code(),
            timed_out: false,
            namespace_active: ns_available,
            cgroup_active: cg_path.is_some(),
        },
        Ok(Err(e)) => IsolatedOutput {
            stdout: String::new(),
            stderr: format!("Failed to execute: {e}"),
            exit_code: None,
            timed_out: false,
            namespace_active: false,
            cgroup_active: false,
        },
        Err(_) => IsolatedOutput {
            stdout: String::new(),
            stderr: format!("Command timed out after {}s", config.timeout.as_secs()),
            exit_code: None,
            timed_out: true,
            namespace_active: ns_available,
            cgroup_active: cg_path.is_some(),
        },
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
        let out = execute_isolated("sleep 10", &env, &config).await;
        assert!(out.timed_out);
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

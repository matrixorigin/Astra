//! Sandbox policy configuration.

use std::path::PathBuf;

/// Security enforcement level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    /// No restrictions — backward compatible with current behavior.
    Permissive,
    /// Path boundary enforcement + env filtering. No OS-level isolation.
    Standard,
    /// Full isolation: Standard + resource limits + restricted shell.
    Strict,
}

/// Configurable security policy for tool execution.
#[derive(Debug, Clone)]
pub struct SandboxPolicy {
    /// Security enforcement level.
    pub mode: SandboxMode,

    /// Primary project directory — all relative paths resolve here.
    pub project_root: PathBuf,

    /// Additional allowed path prefixes (e.g., /tmp, ~/.config).
    /// Paths outside project_root AND these prefixes are rejected.
    pub allowed_paths: Vec<PathBuf>,

    /// Environment variable allowlist. When set, only these vars (plus
    /// a safe baseline) are passed to child processes.
    pub env_allowlist: Option<Vec<String>>,

    /// Maximum command execution time in seconds.
    pub max_execution_secs: f64,

    /// Maximum output size in bytes before truncation.
    pub max_output_bytes: usize,

    /// Whether to allow network access from bash commands.
    /// When false, adds `--network=none` to unshare (Strict mode only).
    pub network_allowed: bool,

    /// Maximum number of processes a sandboxed command can spawn.
    /// 0 = unlimited (Permissive/Standard). Strict mode uses 512.
    ///
    /// NOTE: `ulimit -u` sets RLIMIT_NPROC which counts ALL processes
    /// for the user (UID-wide), not per-process. If the user already
    /// has N processes, the child shell can only fork (limit - N) more.
    /// Values below the user's current process count cause immediate
    /// fork failures ("Resource temporarily unavailable").
    pub max_processes: u32,

    /// Maximum memory in bytes a sandboxed command can use.
    /// 0 = unlimited. Typical: 512 MB.
    pub max_memory_bytes: u64,
}

/// Baseline environment variables always allowed in Standard+ modes.
pub(crate) const ENV_BASELINE: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LANG",
    "LC_ALL",
    "TERM",
    "SHELL",
    "TMPDIR",
    "TMP",
    "TEMP",
    // Git/dev tooling
    "GIT_AUTHOR_NAME",
    "GIT_AUTHOR_EMAIL",
    "GIT_COMMITTER_NAME",
    "GIT_COMMITTER_EMAIL",
    "EDITOR",
    "VISUAL",
    // Build tools
    "CC",
    "CXX",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "GOPATH",
    "GOROOT",
    "NODE_PATH",
    "NPM_CONFIG_PREFIX",
    "VIRTUAL_ENV",
    "CONDA_PREFIX",
    // MatrixOne specific
    "MATRIXONE_HOST",
    "MATRIXONE_PORT",
    "MATRIXONE_USER",
    "MATRIXONE_PASSWORD",
    "MATRIXONE_DATABASE",
];

impl SandboxPolicy {
    /// Create a policy for a project directory with Standard mode defaults.
    pub fn for_project(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            mode: SandboxMode::Standard,
            project_root: root,
            allowed_paths: vec![PathBuf::from("/tmp"), PathBuf::from("/var/tmp")],
            env_allowlist: None,
            max_execution_secs: 30.0,
            max_output_bytes: 20_000,
            network_allowed: true,
            max_processes: 0, // Standard mode: no ulimit -u (too fragile)
            max_memory_bytes: 512 * 1024 * 1024, // 512 MB
        }
    }

    /// Create a permissive policy (backward compatible, no restrictions).
    pub fn permissive(root: impl Into<PathBuf>) -> Self {
        Self {
            mode: SandboxMode::Permissive,
            project_root: root.into(),
            allowed_paths: vec![],
            env_allowlist: None,
            max_execution_secs: 30.0,
            max_output_bytes: 20_000,
            network_allowed: true,
            max_processes: 0,
            max_memory_bytes: 0,
        }
    }

    /// Create a strict policy (full isolation).
    pub fn strict(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            mode: SandboxMode::Strict,
            project_root: root,
            allowed_paths: vec![PathBuf::from("/tmp")],
            env_allowlist: Some(Vec::new()), // Only baseline vars
            max_execution_secs: 15.0,
            max_output_bytes: 10_000,
            network_allowed: false,
            max_processes: 512, // Strict: high enough to avoid fork failures
            max_memory_bytes: 256 * 1024 * 1024, // 256 MB
        }
    }

    /// Check if a path prefix is allowed (project root or allowed_paths).
    pub fn is_path_allowed(&self, path: &std::path::Path) -> bool {
        if self.mode == SandboxMode::Permissive {
            return true;
        }
        if path.starts_with(&self.project_root) {
            return true;
        }
        self.allowed_paths.iter().any(|ap| path.starts_with(ap))
    }

    /// Check if an environment variable should be passed to child process.
    pub fn is_env_allowed(&self, key: &str) -> bool {
        if self.mode == SandboxMode::Permissive {
            return true;
        }
        if ENV_BASELINE.contains(&key) {
            return true;
        }
        if let Some(ref allowlist) = self.env_allowlist {
            return allowlist.iter().any(|a| a == key);
        }
        // No explicit allowlist in Standard mode → allow all
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_policy_defaults() {
        let p = SandboxPolicy::for_project("/home/user/project");
        assert_eq!(p.mode, SandboxMode::Standard);
        assert_eq!(p.max_execution_secs, 30.0);
        assert_eq!(p.max_processes, 0); // Standard: no ulimit -u
        assert!(p.network_allowed);
        assert!(p.is_path_allowed(std::path::Path::new("/home/user/project/src")));
        assert!(p.is_path_allowed(std::path::Path::new("/tmp/build")));
        assert!(!p.is_path_allowed(std::path::Path::new("/etc/passwd")));
    }

    #[test]
    fn permissive_allows_everything() {
        let p = SandboxPolicy::permissive("/home/user/project");
        assert!(p.is_path_allowed(std::path::Path::new("/etc/passwd")));
        assert!(p.is_env_allowed("SECRET_KEY"));
    }

    #[test]
    fn strict_policy_restrictive() {
        let p = SandboxPolicy::strict("/home/user/project");
        assert_eq!(p.mode, SandboxMode::Strict);
        assert!(!p.network_allowed);
        assert_eq!(p.max_processes, 512);
        assert!(p.is_path_allowed(std::path::Path::new("/tmp/x")));
        assert!(!p.is_path_allowed(std::path::Path::new("/var/tmp/x")));
    }

    #[test]
    fn env_baseline_always_allowed() {
        let p = SandboxPolicy::strict("/");
        assert!(p.is_env_allowed("PATH"));
        assert!(p.is_env_allowed("HOME"));
        assert!(p.is_env_allowed("CARGO_HOME"));
        assert!(p.is_env_allowed("MATRIXONE_HOST"));
    }

    #[test]
    fn env_allowlist_in_strict() {
        let mut p = SandboxPolicy::strict("/");
        p.env_allowlist = Some(vec!["MY_CUSTOM_VAR".to_string()]);
        assert!(p.is_env_allowed("PATH")); // baseline
        assert!(p.is_env_allowed("MY_CUSTOM_VAR")); // explicit
        assert!(!p.is_env_allowed("SECRET_API_KEY")); // not in list
    }

    #[test]
    fn standard_no_allowlist_allows_all_env() {
        let p = SandboxPolicy::for_project("/");
        assert!(p.is_env_allowed("ANYTHING"));
    }

    #[test]
    fn project_root_always_allowed() {
        let p = SandboxPolicy::strict("/home/user/proj");
        assert!(p.is_path_allowed(std::path::Path::new("/home/user/proj")));
        assert!(p.is_path_allowed(std::path::Path::new("/home/user/proj/deep/dir")));
        assert!(!p.is_path_allowed(std::path::Path::new("/home/user")));
    }
}

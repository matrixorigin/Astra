//! Sandbox policy configuration.

use std::path::PathBuf;

/// Security enforcement level (ordered from least to most restrictive).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SandboxMode {
    /// No restrictions — backward compatible with current behavior.
    Permissive,
    /// Path boundary enforcement + env filtering. No OS-level isolation.
    Standard,
    /// Full isolation: Standard + restricted shell.
    Strict,
}

/// Configurable security policy for tool execution.
///
/// Note: ulimit-based resource limits (max_processes, max_memory_bytes) were
/// removed to match Claude Code's approach. Resource control now relies on:
/// - Concurrent tool execution limit (MAX_CONCURRENT_READ_ONLY_TOOLS = 10)
/// - Per-command timeouts (max_execution_secs)
///
/// ulimit -u is UID-wide and caused false-positive fork failures when the
/// user already had many processes running.
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
    "MATRIXONE_DATABASE_PREFIX",
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
        }
    }

    /// Create a policy based on the skill's trust tier.
    ///
    /// Maps trust tiers to sandbox modes:
    /// - Bundled → Permissive (platform-tested, full trust)
    /// - Verified → Standard (reviewed publisher, path enforcement)
    /// - Community → Standard + env allowlist (automated scan only)
    /// - Unverified → Strict (no verification, full isolation)
    pub fn for_trust_tier(
        tier: &astra_skills::manifest::TrustTier,
        project_root: impl Into<PathBuf>,
    ) -> Self {
        use astra_skills::manifest::TrustTier;
        let root = project_root.into();
        match tier {
            TrustTier::Bundled => Self::permissive(root),
            TrustTier::Verified => Self::for_project(root),
            TrustTier::Community => {
                let mut p = Self::for_project(root);
                // Community skills get env filtering (only baseline + build vars)
                p.env_allowlist = Some(Vec::new());
                p
            }
            TrustTier::Unverified => Self::strict(root),
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

    #[test]
    fn trust_tier_bundled_is_permissive() {
        use astra_skills::manifest::TrustTier;
        let p = SandboxPolicy::for_trust_tier(&TrustTier::Bundled, "/proj");
        assert_eq!(p.mode, SandboxMode::Permissive);
        assert!(p.network_allowed);
        assert!(p.is_path_allowed(std::path::Path::new("/etc/passwd")));
    }

    #[test]
    fn trust_tier_verified_is_standard() {
        use astra_skills::manifest::TrustTier;
        let p = SandboxPolicy::for_trust_tier(&TrustTier::Verified, "/proj");
        assert_eq!(p.mode, SandboxMode::Standard);
        assert!(p.network_allowed);
        assert!(p.is_path_allowed(std::path::Path::new("/proj/src/main.rs")));
        assert!(!p.is_path_allowed(std::path::Path::new("/etc/passwd")));
    }

    #[test]
    fn trust_tier_community_has_env_filter() {
        use astra_skills::manifest::TrustTier;
        let p = SandboxPolicy::for_trust_tier(&TrustTier::Community, "/proj");
        assert_eq!(p.mode, SandboxMode::Standard);
        // Community gets env allowlist (only baseline vars)
        assert!(p.env_allowlist.is_some());
        assert!(p.is_env_allowed("PATH")); // baseline always allowed
        assert!(!p.is_env_allowed("SECRET_API_KEY")); // non-baseline blocked
    }

    #[test]
    fn trust_tier_unverified_is_strict() {
        use astra_skills::manifest::TrustTier;
        let p = SandboxPolicy::for_trust_tier(&TrustTier::Unverified, "/proj");
        assert_eq!(p.mode, SandboxMode::Strict);
        assert!(!p.network_allowed);
        assert_eq!(p.max_execution_secs, 15.0);
        assert!(!p.is_path_allowed(std::path::Path::new("/etc/passwd")));
    }

    // --- edge cases ---

    #[test]
    fn sandbox_mode_ordering() {
        // Modes are ordered: Permissive < Standard < Strict
        assert!(SandboxMode::Permissive < SandboxMode::Standard);
        assert!(SandboxMode::Standard < SandboxMode::Strict);
    }

    #[test]
    fn strict_max_output_smaller_than_standard() {
        let standard = SandboxPolicy::for_project("/proj");
        let strict = SandboxPolicy::strict("/proj");
        assert!(strict.max_output_bytes < standard.max_output_bytes);
    }

    #[test]
    fn strict_timeout_shorter_than_standard() {
        let standard = SandboxPolicy::for_project("/proj");
        let strict = SandboxPolicy::strict("/proj");
        assert!(strict.max_execution_secs < standard.max_execution_secs);
    }

    #[test]
    fn standard_allows_var_tmp_but_strict_does_not() {
        let standard = SandboxPolicy::for_project("/proj");
        let strict = SandboxPolicy::strict("/proj");
        let var_tmp = std::path::Path::new("/var/tmp/some_file");
        assert!(standard.is_path_allowed(var_tmp));
        assert!(!strict.is_path_allowed(var_tmp));
    }

    #[test]
    fn empty_env_allowlist_blocks_non_baseline() {
        let mut p = SandboxPolicy::for_project("/proj");
        p.env_allowlist = Some(Vec::new());
        assert!(p.is_env_allowed("PATH")); // baseline
        assert!(!p.is_env_allowed("CUSTOM_VAR")); // not in allowlist
    }

    #[test]
    fn project_root_exact_match_allowed() {
        let p = SandboxPolicy::strict("/home/user/proj");
        // Exact project root itself
        assert!(p.is_path_allowed(std::path::Path::new("/home/user/proj")));
    }

    #[test]
    fn permissive_no_allowed_paths_but_allows_all() {
        let p = SandboxPolicy::permissive("/proj");
        assert!(p.allowed_paths.is_empty());
        // Yet allows any path due to Permissive mode
        assert!(p.is_path_allowed(std::path::Path::new("/anywhere")));
    }

    #[test]
    fn env_baseline_covers_matrixone_vars() {
        let p = SandboxPolicy::strict("/proj");
        for var in &[
            "MATRIXONE_HOST",
            "MATRIXONE_PORT",
            "MATRIXONE_USER",
            "MATRIXONE_PASSWORD",
            "MATRIXONE_DATABASE",
            "MATRIXONE_DATABASE_PREFIX",
        ] {
            assert!(p.is_env_allowed(var), "Baseline missing: {}", var);
        }
    }

    #[test]
    fn community_tier_allows_network() {
        use astra_skills::manifest::TrustTier;
        let p = SandboxPolicy::for_trust_tier(&TrustTier::Community, "/proj");
        // Community uses Standard mode which allows network
        assert!(p.network_allowed);
    }
}

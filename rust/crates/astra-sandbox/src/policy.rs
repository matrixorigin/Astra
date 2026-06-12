//! Sandbox policy configuration.

use crate::path::normalize_path;
use std::path::{Path, PathBuf};

fn unique_path_variants(path: &Path) -> Vec<PathBuf> {
    let mut variants = vec![normalize_path(path)];
    if let Ok(canonical) = path.canonicalize()
        && !variants.iter().any(|existing| existing == &canonical)
    {
        variants.push(canonical);
    }
    variants
}

fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

fn default_temp_allowed_paths() -> Vec<PathBuf> {
    let mut paths = vec![PathBuf::from("/tmp")];
    push_unique_path(&mut paths, normalize_path(&std::env::temp_dir()));
    paths
}

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
/// removed. Resource control now relies on:
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
    "ASTRA_DATABASE",
    "ASTRA_DATABASE_PREFIX",
];

impl SandboxPolicy {
    /// Create a policy for a project directory with Standard mode defaults.
    pub fn for_project(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let mut allowed_paths = default_temp_allowed_paths();
        push_unique_path(&mut allowed_paths, PathBuf::from("/var/tmp"));
        push_unique_path(&mut allowed_paths, PathBuf::from("/dev/null"));
        Self {
            mode: SandboxMode::Standard,
            project_root: root,
            allowed_paths,
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
        let mut allowed_paths = default_temp_allowed_paths();
        push_unique_path(&mut allowed_paths, PathBuf::from("/dev/null"));
        Self {
            mode: SandboxMode::Strict,
            project_root: root,
            allowed_paths,
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
        let path_variants = unique_path_variants(path);
        let prefix_matches = |prefix: &Path| {
            let prefix_variants = unique_path_variants(prefix);
            path_variants.iter().any(|candidate| {
                prefix_variants
                    .iter()
                    .any(|allowed| candidate.starts_with(allowed))
            })
        };
        prefix_matches(&self.project_root) || self.allowed_paths.iter().any(|ap| prefix_matches(ap))
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
        assert!(p.allowed_paths.contains(&PathBuf::from("/tmp")));
        assert!(
            p.allowed_paths
                .contains(&normalize_path(&std::env::temp_dir()))
        );
        assert!(p.is_path_allowed(std::path::Path::new("/home/user/project/src")));
        assert!(p.is_path_allowed(std::path::Path::new("/tmp/build")));
        assert!(!p.is_path_allowed(std::path::Path::new("/etc/passwd")));

        // Standard policy without project-scoped allowlist allows any env
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

    #[cfg(unix)]
    #[test]
    fn symlink_project_root_alias_is_allowed_for_existing_paths() {
        let real_root = tempfile::tempdir().unwrap();
        let alias_parent = tempfile::tempdir().unwrap();
        let alias_root = alias_parent.path().join("project-alias");
        std::os::unix::fs::symlink(real_root.path(), &alias_root).unwrap();

        let file = real_root.path().join("nested.txt");
        std::fs::write(&file, "hello").unwrap();

        let p = SandboxPolicy::for_project(&alias_root);
        assert!(p.is_path_allowed(&file.canonicalize().unwrap()));
    }

    #[test]
    fn trust_tier_policy_mapping() {
        use astra_skills::manifest::TrustTier;
        let cases: Vec<(TrustTier, SandboxMode, bool, f64)> = vec![
            (TrustTier::Bundled, SandboxMode::Permissive, true, 30.0),
            (TrustTier::Verified, SandboxMode::Standard, true, 30.0),
            (TrustTier::Community, SandboxMode::Standard, true, 30.0),
            (TrustTier::Unverified, SandboxMode::Strict, false, 15.0),
        ];
        for (tier, mode, network, max_secs) in cases {
            let p = SandboxPolicy::for_trust_tier(&tier, "/proj");
            assert_eq!(p.mode, mode, "{tier:?}");
            assert_eq!(p.network_allowed, network, "{tier:?}");
            assert_eq!(p.max_execution_secs, max_secs, "{tier:?}");
            // Permissive allows any path; others block outside
            let passwd_ok = p.is_path_allowed(std::path::Path::new("/etc/passwd"));
            assert_eq!(passwd_ok, tier == TrustTier::Bundled, "passwd for {tier:?}");
            // Community has env allowlist
            if tier == TrustTier::Community {
                assert!(p.env_allowlist.is_some());
                assert!(!p.is_env_allowed("SECRET_API_KEY"));
            }
        }
    }

    // --- mode comparisons ---

    #[test]
    fn mode_ordering_and_constraints() {
        assert!(SandboxMode::Permissive < SandboxMode::Standard);
        assert!(SandboxMode::Standard < SandboxMode::Strict);
        let standard = SandboxPolicy::for_project("/proj");
        let strict = SandboxPolicy::strict("/proj");
        assert!(strict.max_output_bytes < standard.max_output_bytes);
        assert!(strict.max_execution_secs < standard.max_execution_secs);
        // /var/tmp allowed in Standard, blocked in Strict
        let var_tmp = std::path::Path::new("/var/tmp/some_file");
        assert!(standard.is_path_allowed(var_tmp));
        assert!(!strict.is_path_allowed(var_tmp));
    }

    #[test]
    fn env_baseline_and_allowlist_rules() {
        let p = SandboxPolicy::strict("/proj");
        let baseline: &[&str] = &[
            "PATH", "HOME", "CARGO_HOME", "MATRIXONE_HOST",
            "MATRIXONE_PORT", "MATRIXONE_USER", "MATRIXONE_PASSWORD",
            "ASTRA_DATABASE", "ASTRA_DATABASE_PREFIX",
        ];
        for var in baseline {
            assert!(p.is_env_allowed(var), "Baseline missing: {var}");
        }
        // Custom allowlist
        let mut p2 = SandboxPolicy::strict("/");
        p2.env_allowlist = Some(vec!["MY_CUSTOM_VAR".to_string()]);
        assert!(p2.is_env_allowed("PATH"));
        assert!(p2.is_env_allowed("MY_CUSTOM_VAR"));
        assert!(!p2.is_env_allowed("SECRET_API_KEY"));
        // Empty allowlist blocks non-baseline
        let mut p3 = SandboxPolicy::for_project("/proj");
        p3.env_allowlist = Some(Vec::new());
        assert!(p3.is_env_allowed("PATH"));
        assert!(!p3.is_env_allowed("CUSTOM_VAR"));
    }

    #[test]
    fn permissive_path_and_env_rules() {
        let p = SandboxPolicy::permissive("/proj");
        assert!(p.allowed_paths.is_empty());
        assert!(p.is_path_allowed(std::path::Path::new("/anywhere")));
        assert!(p.is_path_allowed(std::path::Path::new("/etc/passwd")));
        assert!(p.is_env_allowed("SECRET_KEY"));
    }

    // ── Regression: /dev/null blocked by sandbox (session 5f21382b) ──

    #[test]
    fn dev_null_allowed_in_all_modes() {
        for p in [
            SandboxPolicy::permissive("/proj"),
            SandboxPolicy::for_project("/proj"),
            SandboxPolicy::strict("/proj"),
        ] {
            assert!(
                p.is_path_allowed(std::path::Path::new("/dev/null")),
                "{p:?}"
            );
        }
    }

}

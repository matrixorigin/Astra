//! Sandbox policy configuration.

use crate::path::{canonicalize_parent_and_append, normalize_path};
use std::path::{Path, PathBuf};

fn unique_path_variants(path: &Path) -> Vec<PathBuf> {
    let mut variants = vec![normalize_path(path)];
    if let Ok(canonical) = path.canonicalize()
        && !variants.iter().any(|existing| existing == &canonical)
    {
        variants.push(canonical);
    } else if let Ok(canonical_parent) = canonicalize_parent_and_append(path)
        && !variants
            .iter()
            .any(|existing| existing == &canonical_parent)
    {
        variants.push(canonical_parent);
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
pub enum IsolationLevel {
    /// No restrictions.
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
    pub isolation: IsolationLevel,

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
    /// When false, adds `--network=none` to unshare (Strict isolation only).
    pub network_allowed: bool,
}

/// Baseline environment variables always allowed in Standard+ isolation.
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

/// Environment variable substrings that are NEVER passed to child processes,
/// even in Permissive isolation. These are credential-bearing names that
/// no tool, even fully trusted bundled tools, should have access to.
const ALWAYS_DENIED_ENV_KEY_PATTERNS: &[&str] = &[
    "SECRET",
    "TOKEN",
    "PASSWORD",
    "PASSWD",
    "CREDENTIAL",
    "API_KEY",
    "ENCRYPTION_KEY",
    "FERNET_KEY",
    "MASTER_KEY",
    "PRIVATE_KEY",
];

/// Credential path substrings — data-at-rest threats: secret keys, password
/// databases, credential stores, environment files, cloud SDK caches. These
/// are matched as substrings against the full path string, so `.env` matches
/// both `/home/user/.env` and `/app/.env.local`-style variants intentionally
/// (never readable, even in Permissive isolation).
const SENSITIVE_PATH_SUBSTRINGS: &[&str] = &[
    // System account/password files — leak account metadata and password hashes.
    "/etc/passwd",
    "/etc/shadow",
    "/etc/gshadow",
    "/etc/sudoers",
    // SSH host private keys and config.
    "/etc/ssh/",
    ".ssh/id_",
    ".ssh/authorized_keys",
    ".ssh/identity",
    ".aws/credentials",
    ".gnupg/secring.gpg",
    ".gnupg/private-keys-v1.d",
    ".docker/config.json",
    ".netrc",
    "/var/run/secrets",
    // Environment files carry app secrets (DB URLs, API tokens).
    ".env",
    // Cloud SDK credential caches.
    ".kube/config",
    ".npmrc",
    ".pypirc",
    ".git-credentials",
    ".config/gcloud/credentials",
    // Generic credential file names.
    "credentials.json",
    "credentials.db",
];

/// System directories whose entire subtree is sensitive — expanding any of
/// these to the sandbox exposes credentials, account metadata, sudo policy,
/// or kernel/device state. A directory is sensitive if it equals one of these
/// prefixes or is a direct child of one.
const SENSITIVE_SYSTEM_DIR_PREFIXES: &[&str] = &[
    "/etc",
    "/var/run/secrets",
    "/var/lib/secrets",
    "/boot",
    "/proc",
    "/sys",
    "/dev",
];

/// Home-directory credential markers — match the directory itself or any
/// descendant path. These cover credential stores that live under a user's
/// home directory.
const SENSITIVE_CRED_DIR_MARKERS: &[&str] = &[
    "/.ssh",
    "/.aws",
    "/.kube",
    "/.gnupg",
    "/.azure",
    "/.config/gh",
    "/.config/gcloud",
    "/.docker",
    "/.git-credentials",
    "/.netrc",
];

/// Credential file names — matched by the final path component. These are
/// private keys and secret-bearing files that must never be exposed regardless
/// of their parent directory.
const SENSITIVE_CRED_FILE_NAMES: &[&str] = &[
    "id_rsa",
    "id_ed25519",
    "id_ecdsa",
    "id_ed25519_sk",
    "id_dsa",
    ".env",
    ".pgpass",
    ".my.cnf",
    "htpasswd",
    "token.json",
    "access_token",
    ".terraformrc",
];

/// Device files under `/dev` that are universally safe — they are sinks or
/// zero sources, never secret-bearing. All other `/dev/*` entries (block
/// devices, character devices, USB endpoints) remain sensitive.
const SAFE_DEVICE_FILES: &[&str] = &["/dev/null", "/dev/zero", "/dev/full"];

/// True if `path` is a system-sensitive **directory** subtree (`/etc`, `/boot`,
/// `/proc`, `/sys`, `/dev`, credential dirs under `$HOME`).
///
/// This is the directory-level gate used to reject sandbox expansion: opening
/// any of these to the sandbox would expose entire subtrees of secrets or
/// kernel state. Use [`is_sensitive_path`] for the file/credential-level check.
pub fn is_sensitive_system_dir(path: &std::path::Path) -> bool {
    let lower = path.to_string_lossy().to_ascii_lowercase();
    // Carve out universally-safe device sinks before the /dev prefix check.
    // /dev/null, /dev/zero, /dev/full are deterministic sinks/sources with no
    // secret surface; all other /dev nodes (block devices, tty, usb) stay blocked.
    if SAFE_DEVICE_FILES.contains(&lower.as_str()) {
        return false;
    }
    for prefix in SENSITIVE_SYSTEM_DIR_PREFIXES {
        if lower == *prefix || lower.starts_with(&format!("{prefix}/")) {
            return true;
        }
    }
    for marker in SENSITIVE_CRED_DIR_MARKERS {
        if lower == marker.trim_start_matches('/')
            || lower.ends_with(marker)
            || lower.contains(&format!("{marker}/"))
        {
            return true;
        }
    }
    false
}

/// True if `path` is sensitive — a system file, credential, system directory,
/// or credential-bearing path. This is the **single source of truth** for all
/// path-sensitivity checks across the sandbox, shell-hardening, and permission
/// layers.
///
/// Combines:
/// - [`is_sensitive_system_dir`] (directory-subtree match)
/// - [`SENSITIVE_PATH_SUBSTRINGS`] (broad credential-file substring match)
/// - [`SENSITIVE_CRED_FILE_NAMES`] (exact file-name match)
pub fn is_sensitive_path(path: &std::path::Path) -> bool {
    if is_sensitive_system_dir(path) {
        return true;
    }
    let path_str = path.to_string_lossy();
    if SENSITIVE_PATH_SUBSTRINGS
        .iter()
        .any(|p| path_str.contains(p))
    {
        return true;
    }
    if let Some(file_name) = path.file_name().and_then(|n| n.to_str())
        && SENSITIVE_CRED_FILE_NAMES.contains(&file_name)
    {
        return true;
    }
    false
}

/// Check if a path matches any never-readable pattern.
///
/// Thin alias of [`is_sensitive_path`] retained as the canonical read-gating
/// predicate name. A path that is sensitive is never readable, even in
/// Permissive isolation.
pub fn is_never_readable_path(path: &std::path::Path) -> bool {
    is_sensitive_path(path)
}

/// Check if an env key matches any always-denied pattern.
fn is_always_denied_env_key(key: &str) -> bool {
    let upper = key.to_uppercase();
    ALWAYS_DENIED_ENV_KEY_PATTERNS
        .iter()
        .any(|pattern| upper.contains(pattern))
}

impl SandboxPolicy {
    /// Create a policy for a project directory with Standard isolation defaults.
    pub fn for_project(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let mut allowed_paths = default_temp_allowed_paths();
        push_unique_path(&mut allowed_paths, PathBuf::from("/var/tmp"));
        push_unique_path(&mut allowed_paths, PathBuf::from("/dev/null"));
        Self {
            isolation: IsolationLevel::Standard,
            project_root: root,
            allowed_paths,
            env_allowlist: None,
            max_execution_secs: 30.0,
            max_output_bytes: 20_000,
            network_allowed: true,
        }
    }

    /// Create a permissive policy with no restrictions.
    pub fn permissive(root: impl Into<PathBuf>) -> Self {
        Self {
            isolation: IsolationLevel::Permissive,
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
            isolation: IsolationLevel::Strict,
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
    /// Maps trust tiers to isolation levels:
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
    /// Never-readable credential paths are always denied, even in Permissive mode.
    pub fn is_path_allowed(&self, path: &std::path::Path) -> bool {
        // Never-readable paths are blocked at every isolation level.
        if is_never_readable_path(path) {
            return false;
        }
        if self.isolation == IsolationLevel::Permissive {
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
    /// Credential-bearing env vars are always denied, even in Permissive mode.
    pub fn is_env_allowed(&self, key: &str) -> bool {
        // Baseline vars are explicitly permitted at every isolation level.
        if ENV_BASELINE.contains(&key) {
            return true;
        }
        // Credential env vars are blocked at every isolation level.
        if is_always_denied_env_key(key) {
            return false;
        }
        if self.isolation == IsolationLevel::Permissive {
            return true;
        }
        if let Some(ref allowlist) = self.env_allowlist {
            return allowlist.iter().any(|a| a == key);
        }
        // No explicit allowlist in Standard isolation: allow all.
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_policy_defaults() {
        let p = SandboxPolicy::for_project("/home/user/project");
        assert_eq!(p.isolation, IsolationLevel::Standard);
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

        // Standard isolation without project-scoped allowlist allows any env.
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
        let cases: Vec<(TrustTier, IsolationLevel, bool, f64)> = vec![
            (TrustTier::Bundled, IsolationLevel::Permissive, true, 30.0),
            (TrustTier::Verified, IsolationLevel::Standard, true, 30.0),
            (TrustTier::Community, IsolationLevel::Standard, true, 30.0),
            (TrustTier::Unverified, IsolationLevel::Strict, false, 15.0),
        ];
        for (tier, isolation, network, max_secs) in cases {
            let p = SandboxPolicy::for_trust_tier(&tier, "/proj");
            assert_eq!(p.isolation, isolation, "{tier:?}");
            assert_eq!(p.network_allowed, network, "{tier:?}");
            assert_eq!(p.max_execution_secs, max_secs, "{tier:?}");
            // Permissive allows arbitrary non-sensitive paths; others block
            // outside the project root.
            let arbitrary_ok = p.is_path_allowed(std::path::Path::new("/var/data"));
            assert_eq!(
                arbitrary_ok,
                tier == TrustTier::Bundled,
                "arbitrary path for {tier:?}"
            );
            // System account files are never-readable at every tier — the
            // prior assertion that /etc/passwd was allowed in Permissive
            // mode was the very bug fixed by review CRITICAL #1.
            assert!(
                !p.is_path_allowed(std::path::Path::new("/etc/passwd")),
                "/etc/passwd must be blocked at every tier ({tier:?})"
            );
            // Community has env allowlist
            if tier == TrustTier::Community {
                assert!(p.env_allowlist.is_some());
                assert!(!p.is_env_allowed("SECRET_API_KEY"));
            }
        }
    }

    // --- isolation comparisons ---

    #[test]
    fn isolation_ordering_and_constraints() {
        assert!(IsolationLevel::Permissive < IsolationLevel::Standard);
        assert!(IsolationLevel::Standard < IsolationLevel::Strict);
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
            "PATH",
            "HOME",
            "CARGO_HOME",
            "MATRIXONE_HOST",
            "MATRIXONE_PORT",
            "MATRIXONE_USER",
            "MATRIXONE_PASSWORD",
            "ASTRA_DATABASE",
            "ASTRA_DATABASE_PREFIX",
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
        // Permissive allows arbitrary non-sensitive paths.
        assert!(p.is_path_allowed(std::path::Path::new("/anywhere")));
        assert!(p.is_path_allowed(std::path::Path::new("/var/data")));
        // System account files and credential paths are always blocked,
        // even in Permissive mode — review CRITICAL #1 closed the
        // ReadShortCircuit bypass for /etc/passwd, /etc/shadow, /etc/sudoers.
        assert!(!p.is_path_allowed(std::path::Path::new("/etc/passwd")));
        assert!(!p.is_path_allowed(std::path::Path::new("/etc/shadow")));
        assert!(!p.is_path_allowed(std::path::Path::new("/etc/sudoers")));
        assert!(!p.is_path_allowed(std::path::Path::new("/home/user/.ssh/id_rsa")));
        // Credential env vars are always blocked, even in Permissive mode.
        assert!(!p.is_env_allowed("SECRET_KEY"));
        assert!(!p.is_env_allowed("GITHUB_TOKEN"));
        // Non-credential vars are allowed in Permissive mode.
        assert!(p.is_env_allowed("HOME"));
        assert!(p.is_env_allowed("EDITOR"));
    }

    // ── Regression: /dev/null blocked by sandbox (session 5f21382b) ──

    #[test]
    fn dev_null_allowed_in_all_isolation_levels() {
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

    // ── Credential paths must be never-readable (review C2/H1) ──
    // These are data-at-rest secrets. They must be blocked even in
    // Permissive isolation, regardless of project root.

    #[test]
    fn never_readable_paths_block_dotenv() {
        assert!(is_never_readable_path(std::path::Path::new(
            "/home/user/.env"
        )));
        assert!(is_never_readable_path(std::path::Path::new("/proj/.env")));
        assert!(is_never_readable_path(std::path::Path::new(".env.local")));
    }

    #[test]
    fn never_readable_paths_block_cloud_and_pkg_credentials() {
        assert!(is_never_readable_path(std::path::Path::new(
            "/home/user/.kube/config"
        )));
        assert!(is_never_readable_path(std::path::Path::new(
            "/home/user/.npmrc"
        )));
        assert!(is_never_readable_path(std::path::Path::new(
            "/home/user/.pypirc"
        )));
        assert!(is_never_readable_path(std::path::Path::new(
            "/home/user/.config/gcloud/credentials.db"
        )));
        assert!(is_never_readable_path(std::path::Path::new(
            "/home/user/.git-credentials"
        )));
        assert!(is_never_readable_path(std::path::Path::new(
            "/proj/credentials.json"
        )));
    }

    // ── System credential/account files must be never-readable (review CRITICAL #1) ──
    // /etc/passwd, /etc/shadow, /etc/sudoers, and /etc/ssh/ contain account
    // metadata, password hashes, and host keys. In Auto mode, read_file on
    // these would otherwise be Allow via ReadShortCircuit unless listed here.

    #[test]
    fn never_readable_paths_block_system_account_files() {
        assert!(is_never_readable_path(std::path::Path::new("/etc/passwd")));
        assert!(is_never_readable_path(std::path::Path::new("/etc/shadow")));
        assert!(is_never_readable_path(std::path::Path::new("/etc/gshadow")));
        assert!(is_never_readable_path(std::path::Path::new("/etc/sudoers")));
        assert!(is_never_readable_path(std::path::Path::new(
            "/etc/sudoers.d/extra"
        )));
    }

    #[test]
    fn never_readable_paths_block_ssh_host_keys_directory() {
        assert!(is_never_readable_path(std::path::Path::new(
            "/etc/ssh/ssh_host_rsa_key"
        )));
        assert!(is_never_readable_path(std::path::Path::new(
            "/etc/ssh/ssh_host_ed25519_key"
        )));
    }

    #[test]
    fn permissive_mode_blocks_never_readable_system_account_files() {
        let p = SandboxPolicy::permissive("/proj");
        assert!(!p.is_path_allowed(std::path::Path::new("/etc/passwd")));
        assert!(!p.is_path_allowed(std::path::Path::new("/etc/shadow")));
        assert!(!p.is_path_allowed(std::path::Path::new("/etc/sudoers")));
    }

    #[test]
    fn permissive_mode_blocks_never_readable_credential_paths() {
        let p = SandboxPolicy::permissive("/proj");
        assert!(p.is_path_allowed(std::path::Path::new("/anywhere")));
        assert!(!p.is_path_allowed(std::path::Path::new("/home/u/.env")));
        assert!(!p.is_path_allowed(std::path::Path::new("/home/u/.kube/config")));
        assert!(!p.is_path_allowed(std::path::Path::new("/home/u/.npmrc")));
    }
}

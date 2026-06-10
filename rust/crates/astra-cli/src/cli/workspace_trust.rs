//! Issue #326 P5b / R1 Critical 3 / scenarios #36/37/40:
//! workspace-level trust ledger.
//!
//! ## Why
//!
//! `.astra/permissions.json` is a project-level file that ends up
//! committed to git. If a user clones a hostile repo (or just an
//! unfamiliar one) and starts astra, the project file can grant
//! the agent capabilities the user didn't intend — the
//! infrastructure to read it is the same as for a friendly file.
//!
//! VSCode solves this with "workspace trust": the first time you
//! open a folder, you're asked whether to trust it; trusted
//! folders get full features, untrusted ones get a restricted
//! mode. We adopt the same model here, scoped to permission
//! rules:
//!
//! - Trusted workspace → [`PermissionLoadPolicy::InteractiveTrusted`]
//!   (full apply of allow + deny rules).
//! - Untrusted workspace → [`PermissionLoadPolicy::InteractiveUntrusted`]
//!   (apply deny rules only; ignore allow rules + opt-in flags).
//!
//! The ledger is per-user (`~/.astra/trusted_workspaces.json`), not
//! per-project, so trust survives across machine reboots but doesn't
//! escape into git history.
//!
//! ## File format
//!
//! ```json
//! {
//!   "version": 1,
//!   "workspaces": {
//!     "/Users/alice/work/app-a": {
//!       "trust": "trusted",
//!       "trusted_at": "2026-05-13T11:25:00Z",
//!       "rules_hash": "sha256:abc..."
//!     },
//!     "/Users/alice/work/random-clone": {
//!       "trust": "untrusted",
//!       "trusted_at": null,
//!       "rules_hash": null
//!     }
//!   }
//! }
//! ```
//!
//! `rules_hash` is the SHA-256 of `.astra/permissions.json` at the
//! moment trust was granted. If the project file later changes
//! (team adds rules), the next session sees a hash mismatch and
//! can prompt the user "N rules changed — re-review?".
//!
//! Corruption is surfaced loudly per the P5b contract: if the
//! ledger file fails to parse, we treat ALL workspaces as
//! [`TrustState::Ask`] and emit a `tracing::warn`. Silent
//! fall-back to "trust everything" would defeat the whole point.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Current ledger schema version. Stamped into every saved file
/// so future migrations have an anchor.
pub const TRUSTED_WORKSPACES_VERSION: u32 = 1;

/// Trust posture for a workspace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustState {
    /// Workspace is in the ledger and explicitly trusted.
    Trusted,
    /// Workspace is in the ledger and explicitly untrusted (the
    /// user picked "Run untrusted, never ask again"). Stays in
    /// untrusted mode without prompting.
    Untrusted,
    /// Workspace is not in the ledger (or was added with this
    /// state). UI should prompt the user on session start.
    Ask,
}

impl Default for TrustState {
    fn default() -> Self {
        Self::Ask
    }
}

/// Why a workspace is, or is not, allowed to apply project-level
/// allow rules.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceTrustReason {
    Trusted,
    UnknownWorkspace,
    ExplicitlyUntrusted,
    RulesHashChanged,
    LedgerError(String),
    RulesHashError(String),
}

impl WorkspaceTrustReason {
    #[must_use]
    pub fn display(&self) -> String {
        match self {
            Self::Trusted => "trusted workspace".to_string(),
            Self::UnknownWorkspace => "workspace has not been trusted".to_string(),
            Self::ExplicitlyUntrusted => "workspace is marked untrusted".to_string(),
            Self::RulesHashChanged => {
                "project permission rules changed since trust was granted".to_string()
            }
            Self::LedgerError(err) => {
                format!("workspace trust ledger could not be loaded: {err}")
            }
            Self::RulesHashError(err) => {
                format!("project permission rules could not be hashed: {err}")
            }
        }
    }
}

/// Effective trust decision for a workspace at manager construction time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceTrustEvaluation {
    pub state: TrustState,
    pub recorded_rules_hash: Option<String>,
    pub current_rules_hash: Option<String>,
    pub reason: WorkspaceTrustReason,
}

impl WorkspaceTrustEvaluation {
    #[must_use]
    pub fn applies_project_allow(&self) -> bool {
        matches!(self.reason, WorkspaceTrustReason::Trusted)
    }

    #[must_use]
    pub fn summary_line(&self) -> String {
        if self.applies_project_allow() {
            return "Workspace trust: trusted; saved workspace rules are active".to_string();
        }
        format!(
            "Workspace trust: {}; saved workspace rules are ignored",
            self.reason.display()
        )
    }
}

/// Per-workspace ledger entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceTrustEntry {
    pub trust: TrustState,
    /// RFC 3339 timestamp; populated on `Trusted` and `Untrusted`,
    /// `None` for `Ask`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trusted_at: Option<String>,
    /// SHA-256 of `.astra/permissions.json` at the moment the trust
    /// decision was made. `None` when there was no project file
    /// yet (a brand-new workspace) or when the trust state is
    /// `Ask`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules_hash: Option<String>,
}

impl Default for WorkspaceTrustEntry {
    fn default() -> Self {
        Self {
            trust: TrustState::Ask,
            trusted_at: None,
            rules_hash: None,
        }
    }
}

/// On-disk shape of `~/.astra/trusted_workspaces.json`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TrustedWorkspacesFile {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub workspaces: BTreeMap<String, WorkspaceTrustEntry>,
}

fn default_version() -> u32 {
    TRUSTED_WORKSPACES_VERSION
}

/// In-memory ledger.
///
/// Loaded lazily on first query; mutations go through
/// [`Self::update`] which atomically rewrites the JSON file.
#[derive(Clone, Debug, Default)]
pub struct WorkspaceTrustLedger {
    file: TrustedWorkspacesFile,
    /// Path the ledger was loaded from / will be saved to.
    path: PathBuf,
    /// `true` if the in-memory representation has unsaved changes.
    /// Reset to `false` after every successful save.
    dirty: bool,
}

/// Errors that can occur loading / saving the ledger.
#[derive(Debug)]
pub enum WorkspaceTrustError {
    /// Couldn't determine a home directory at construction time.
    NoHomeDir,
    /// I/O error during read/write.
    Io {
        stage: &'static str,
        source: std::io::Error,
    },
    /// JSON parse error.
    Corrupt { path: PathBuf, message: String },
}

impl std::fmt::Display for WorkspaceTrustError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoHomeDir => write!(f, "could not determine home directory"),
            Self::Io { stage, source } => write!(f, "{stage} failed: {source}"),
            Self::Corrupt { path, message } => {
                write!(f, "{} is not valid JSON: {message}", path.display())
            }
        }
    }
}

impl std::error::Error for WorkspaceTrustError {}

impl WorkspaceTrustLedger {
    /// Default ledger location: `~/.astra/trusted_workspaces.json`.
    #[must_use]
    pub fn default_path() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(".astra").join("trusted_workspaces.json"))
    }

    /// Construct an empty ledger at a specific path. Use when you
    /// want to override the default location (e.g. tests).
    #[must_use]
    pub fn empty_at(path: PathBuf) -> Self {
        Self {
            file: TrustedWorkspacesFile::default(),
            path,
            dirty: false,
        }
    }

    /// Load (or create empty) the ledger at the default path.
    ///
    /// Returns the ledger plus an optional "this is what went
    /// wrong loading the file" error. The ledger object is always
    /// usable — corrupt JSON yields an empty ledger and the error,
    /// not a panic. The caller decides whether to surface the
    /// error to the user (TUI banner, `tracing::warn`, exit-1).
    #[must_use]
    pub fn load() -> (Self, Option<WorkspaceTrustError>) {
        let Some(path) = Self::default_path() else {
            return (
                Self::empty_at(PathBuf::from("trusted_workspaces.json")),
                Some(WorkspaceTrustError::NoHomeDir),
            );
        };
        Self::load_from(path)
    }

    /// Like [`Self::load`] but with an explicit path. Tests use this.
    #[must_use]
    pub fn load_from(path: PathBuf) -> (Self, Option<WorkspaceTrustError>) {
        let mut ledger = Self {
            file: TrustedWorkspacesFile::default(),
            path: path.clone(),
            dirty: false,
        };
        match std::fs::read_to_string(&path) {
            Ok(content) => match serde_json::from_str::<TrustedWorkspacesFile>(&content) {
                Ok(file) => {
                    ledger.file = file;
                    (ledger, None)
                }
                Err(e) => (
                    ledger,
                    Some(WorkspaceTrustError::Corrupt {
                        path,
                        message: e.to_string(),
                    }),
                ),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (ledger, None),
            Err(e) => (
                ledger,
                Some(WorkspaceTrustError::Io {
                    stage: "read trusted_workspaces.json",
                    source: e,
                }),
            ),
        }
    }

    /// Trust state for a workspace path. `Ask` if the path isn't
    /// in the ledger.
    #[must_use]
    pub fn state_for(&self, workspace: &Path) -> TrustState {
        let key = canonicalize_key(workspace);
        self.file
            .workspaces
            .get(&key)
            .map(|e| e.trust)
            .unwrap_or(TrustState::Ask)
    }

    /// Recorded `rules_hash` for a workspace, if any. Used by the
    /// TUI to detect "team added new rules since you trusted this".
    #[must_use]
    pub fn rules_hash_for(&self, workspace: &Path) -> Option<&str> {
        let key = canonicalize_key(workspace);
        self.file
            .workspaces
            .get(&key)
            .and_then(|e| e.rules_hash.as_deref())
    }

    /// Full entry for a workspace, if present.
    #[must_use]
    pub fn entry_for(&self, workspace: &Path) -> Option<&WorkspaceTrustEntry> {
        let key = canonicalize_key(workspace);
        self.file.workspaces.get(&key)
    }

    /// Set the trust state for a workspace.
    ///
    /// Marks the in-memory ledger dirty; call [`Self::save`] to
    /// persist. `rules_hash` should be the SHA-256 of the project
    /// file at decision time when state is `Trusted`, or `None`
    /// otherwise.
    pub fn set(
        &mut self,
        workspace: &Path,
        state: TrustState,
        rules_hash: Option<String>,
        timestamp_iso: Option<String>,
    ) {
        let key = canonicalize_key(workspace);
        let entry = WorkspaceTrustEntry {
            trust: state,
            trusted_at: timestamp_iso,
            rules_hash,
        };
        self.file.workspaces.insert(key, entry);
        self.dirty = true;
    }

    /// Remove a workspace from the ledger. The next session will
    /// see `TrustState::Ask` and prompt again.
    pub fn revoke(&mut self, workspace: &Path) {
        let key = canonicalize_key(workspace);
        if self.file.workspaces.remove(&key).is_some() {
            self.dirty = true;
        }
    }

    /// Whether the in-memory ledger has changes that haven't hit
    /// disk yet.
    #[must_use]
    pub fn dirty(&self) -> bool {
        self.dirty
    }

    /// Atomic save (tempfile + fsync + rename + parent fsync).
    ///
    /// Mirrors the [`PermissionSettings::save`] pattern from P0:
    /// half-written files would corrupt all subsequent loads, so
    /// we never accept that risk.
    pub fn save(&mut self) -> Result<(), WorkspaceTrustError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| WorkspaceTrustError::Io {
                stage: "create ~/.astra/",
                source: e,
            })?;
        }

        let json =
            serde_json::to_string_pretty(&self.file).map_err(|e| WorkspaceTrustError::Io {
                stage: "serialize",
                source: std::io::Error::other(e),
            })?;

        let dir = self
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        let mut tmp =
            tempfile::NamedTempFile::new_in(&dir).map_err(|e| WorkspaceTrustError::Io {
                stage: "create temp file",
                source: e,
            })?;
        std::io::Write::write_all(&mut tmp, json.as_bytes()).map_err(|e| {
            WorkspaceTrustError::Io {
                stage: "write temp",
                source: e,
            }
        })?;
        tmp.as_file()
            .sync_all()
            .map_err(|e| WorkspaceTrustError::Io {
                stage: "fsync temp",
                source: e,
            })?;
        tmp.persist(&self.path)
            .map_err(|e| WorkspaceTrustError::Io {
                stage: "rename",
                source: e.error,
            })?;

        if let Ok(dir_handle) = std::fs::File::open(&dir) {
            let _ = dir_handle.sync_all();
        }
        self.dirty = false;
        Ok(())
    }
}

/// Evaluate workspace trust using the default per-user ledger.
#[must_use]
pub fn evaluate_workspace_trust(workspace: &Path) -> WorkspaceTrustEvaluation {
    let (ledger, load_error) = WorkspaceTrustLedger::load();
    evaluate_workspace_trust_with_ledger(workspace, &ledger, load_error)
}

/// Evaluate workspace trust from a specific ledger path. Tests use this
/// to avoid touching the caller's real `~/.astra` state.
#[must_use]
pub fn evaluate_workspace_trust_from_path(
    workspace: &Path,
    ledger_path: PathBuf,
) -> WorkspaceTrustEvaluation {
    let (ledger, load_error) = WorkspaceTrustLedger::load_from(ledger_path);
    evaluate_workspace_trust_with_ledger(workspace, &ledger, load_error)
}

fn evaluate_workspace_trust_with_ledger(
    workspace: &Path,
    ledger: &WorkspaceTrustLedger,
    load_error: Option<WorkspaceTrustError>,
) -> WorkspaceTrustEvaluation {
    let current_hash = match project_permissions_hash(workspace) {
        Ok(hash) => hash,
        Err(err) => {
            return WorkspaceTrustEvaluation {
                state: TrustState::Ask,
                recorded_rules_hash: None,
                current_rules_hash: None,
                reason: WorkspaceTrustReason::RulesHashError(err.to_string()),
            };
        }
    };

    if let Some(err) = load_error {
        return WorkspaceTrustEvaluation {
            state: TrustState::Ask,
            recorded_rules_hash: None,
            current_rules_hash: current_hash,
            reason: WorkspaceTrustReason::LedgerError(err.to_string()),
        };
    }

    let entry = ledger.entry_for(workspace);
    let state = entry.map(|e| e.trust).unwrap_or(TrustState::Ask);
    let recorded_hash = entry.and_then(|e| e.rules_hash.clone());
    let reason = match state {
        TrustState::Trusted if recorded_hash == current_hash => WorkspaceTrustReason::Trusted,
        TrustState::Trusted => WorkspaceTrustReason::RulesHashChanged,
        TrustState::Untrusted => WorkspaceTrustReason::ExplicitlyUntrusted,
        TrustState::Ask => WorkspaceTrustReason::UnknownWorkspace,
    };

    WorkspaceTrustEvaluation {
        state,
        recorded_rules_hash: recorded_hash,
        current_rules_hash: current_hash,
        reason,
    }
}

/// Use the canonicalized absolute path as the ledger key when
/// possible; fall back to the lossy display string when
/// canonicalization fails (the workspace may not exist yet).
fn canonicalize_key(workspace: &Path) -> String {
    std::fs::canonicalize(workspace)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| workspace.to_string_lossy().into_owned())
}

/// SHA-256 of `<workspace>/.astra/permissions.json`, if it exists.
pub fn project_permissions_hash(workspace: &Path) -> Result<Option<String>, WorkspaceTrustError> {
    let path = workspace.join(".astra").join("permissions.json");
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(WorkspaceTrustError::Io {
                stage: "read .astra/permissions.json",
                source: e,
            });
        }
    };
    let digest = Sha256::digest(&bytes);
    let hex = digest
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    Ok(Some(format!("sha256:{hex}")))
}

#[cfg(test)]
mod tests {
    use super::{
        evaluate_workspace_trust_from_path, project_permissions_hash, TrustState,
        WorkspaceTrustError, WorkspaceTrustLedger, WorkspaceTrustReason,
    };
    use std::path::Path;

    fn fresh_ledger() -> (tempfile::TempDir, WorkspaceTrustLedger) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trusted_workspaces.json");
        let ledger = WorkspaceTrustLedger::empty_at(path);
        (dir, ledger)
    }

    #[test]
    fn unknown_workspace_defaults_to_ask() {
        let (_dir, ledger) = fresh_ledger();
        assert_eq!(ledger.state_for(Path::new("/some/path")), TrustState::Ask);
    }

    #[test]
    fn set_then_query_returns_state() {
        let (_dir, mut ledger) = fresh_ledger();
        let path = std::env::temp_dir();
        ledger.set(
            &path,
            TrustState::Trusted,
            Some("sha256:abc".into()),
            Some("2026-05-13T11:25:00Z".into()),
        );
        assert_eq!(ledger.state_for(&path), TrustState::Trusted);
        assert_eq!(ledger.rules_hash_for(&path), Some("sha256:abc"));
    }

    #[test]
    fn revoke_removes_from_ledger() {
        let (_dir, mut ledger) = fresh_ledger();
        let path = std::env::temp_dir();
        ledger.set(&path, TrustState::Trusted, None, None);
        assert_eq!(ledger.state_for(&path), TrustState::Trusted);
        ledger.revoke(&path);
        assert_eq!(ledger.state_for(&path), TrustState::Ask);
    }

    #[test]
    fn save_then_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trusted_workspaces.json");
        let ws = std::env::temp_dir();

        let mut ledger = WorkspaceTrustLedger::empty_at(path.clone());
        ledger.set(
            &ws,
            TrustState::Trusted,
            Some("sha256:abc".into()),
            Some("2026-05-13T11:25:00Z".into()),
        );
        ledger.save().unwrap();

        let (reloaded, err) = WorkspaceTrustLedger::load_from(path);
        assert!(err.is_none(), "unexpected load error: {err:?}");
        assert_eq!(reloaded.state_for(&ws), TrustState::Trusted);
        assert_eq!(reloaded.rules_hash_for(&ws), Some("sha256:abc"));
    }

    #[test]
    fn corrupt_json_returns_empty_ledger_with_loud_error() {
        // Corruption MUST surface — silently treating "trust
        // everything" as the fallback would defeat the entire
        // P5b model.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trusted_workspaces.json");
        std::fs::write(&path, "{ not json").unwrap();

        let (ledger, err) = WorkspaceTrustLedger::load_from(path);
        match err {
            Some(WorkspaceTrustError::Corrupt { .. }) => {}
            other => panic!("expected Corrupt, got {other:?}"),
        }
        // Effective ledger is empty → all workspaces fall back to Ask.
        assert_eq!(ledger.state_for(Path::new("/anywhere")), TrustState::Ask);
    }

    #[test]
    fn missing_file_loads_empty_ledger_without_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.json");
        let (ledger, err) = WorkspaceTrustLedger::load_from(path);
        assert!(err.is_none());
        assert_eq!(ledger.state_for(Path::new("/")), TrustState::Ask);
    }

    #[test]
    fn dirty_flag_tracks_mutations() {
        let (_dir, mut ledger) = fresh_ledger();
        assert!(!ledger.dirty());

        ledger.set(Path::new("/tmp"), TrustState::Trusted, None, None);
        assert!(ledger.dirty());

        ledger.save().unwrap();
        assert!(!ledger.dirty());
    }

    #[test]
    fn project_permissions_hash_returns_sha256_when_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        let kiro = dir.path().join(".astra");
        std::fs::create_dir_all(&kiro).unwrap();
        std::fs::write(kiro.join("permissions.json"), b"{}").unwrap();
        let hash = project_permissions_hash(dir.path()).unwrap().unwrap();
        assert!(hash.starts_with("sha256:"));
        assert_eq!(hash.len(), "sha256:".len() + 64);
    }

    #[test]
    fn project_permissions_hash_returns_none_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(project_permissions_hash(dir.path()).unwrap(), None);
    }

    #[test]
    fn project_permissions_hash_changes_when_rules_change() {
        let dir = tempfile::tempdir().unwrap();
        let kiro = dir.path().join(".astra");
        std::fs::create_dir_all(&kiro).unwrap();
        let path = kiro.join("permissions.json");
        std::fs::write(&path, br#"{"allow":["Bash(ls:*)"]}"#).unwrap();
        let first = project_permissions_hash(dir.path()).unwrap();
        std::fs::write(&path, br#"{"allow":["Bash(cargo test:*)"]}"#).unwrap();
        let second = project_permissions_hash(dir.path()).unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn evaluate_workspace_trust_requires_matching_rules_hash() {
        let dir = tempfile::tempdir().unwrap();
        let kiro = dir.path().join(".astra");
        std::fs::create_dir_all(&kiro).unwrap();
        let path = kiro.join("permissions.json");
        std::fs::write(&path, br#"{"allow":["Bash(ls:*)"]}"#).unwrap();
        let trusted_hash = project_permissions_hash(dir.path()).unwrap();

        let ledger_path = dir.path().join("trusted_workspaces.json");
        let mut ledger = WorkspaceTrustLedger::empty_at(ledger_path.clone());
        ledger.set(
            dir.path(),
            TrustState::Trusted,
            trusted_hash,
            Some("2026-05-13T11:25:00Z".into()),
        );
        ledger.save().unwrap();

        let trusted = evaluate_workspace_trust_from_path(dir.path(), ledger_path.clone());
        assert!(trusted.applies_project_allow());

        std::fs::write(&path, br#"{"allow":["Bash(cargo test:*)"]}"#).unwrap();
        let changed = evaluate_workspace_trust_from_path(dir.path(), ledger_path);
        assert!(!changed.applies_project_allow());
        assert_eq!(changed.reason, WorkspaceTrustReason::RulesHashChanged);
    }
}

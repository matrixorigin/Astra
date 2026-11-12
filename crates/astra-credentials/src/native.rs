//! Versioned MOI sessions. This is separate from legacy password/Memoria
//! profiles because rotation intent, account CAS and logout tombstones are
//! part of this protocol, not optional legacy profile attributes.
use fs2::FileExt;
mod device_models;
mod diagnostics;
#[cfg(all(test, unix))]
mod publication_tests;
use diagnostics::{RotationDiagnostic, response_request_id, transport_cause};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub const VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Environment {
    pub issuer: String,
    pub astra_url: String,
    pub moi_url: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub revocation_endpoint: String,
    pub jwks_uri: String,
}

impl Environment {
    pub fn key(&self) -> String {
        let mut hash = Sha256::new();
        for value in [&self.issuer, &self.astra_url, &self.moi_url] {
            hash.update(value.as_bytes());
            hash.update([0]);
        }
        format!("{:x}", hash.finalize())
    }

    pub fn validate(&self) -> Result<(), String> {
        let issuer = secure_url(&self.issuer)?;
        for value in [&self.astra_url, &self.moi_url] {
            secure_url(value)?;
        }
        for value in [
            &self.authorization_endpoint,
            &self.token_endpoint,
            &self.revocation_endpoint,
            &self.jwks_uri,
        ] {
            let endpoint = secure_url(value)?;
            if endpoint.origin() != issuer.origin() {
                return Err("UC endpoint origin mismatch".into());
            }
        }
        if self.token_endpoint != format!("{}/protocol/openid-connect/token", self.issuer)
            || self.revocation_endpoint != format!("{}/protocol/openid-connect/revoke", self.issuer)
            || self.jwks_uri != format!("{}/protocol/openid-connect/certs", self.issuer)
        {
            return Err("UC protocol endpoint mismatch".into());
        }
        Ok(())
    }
}

pub fn secure_url(value: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(value).map_err(|_| "invalid authentication URL")?;
    let local = matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "localhost"));
    if !(url.scheme() == "https" || url.scheme() == "http" && local)
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(
            "authentication requires HTTPS; loopback HTTP is only for local development".into(),
        );
    }
    Ok(url)
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeSession {
    pub environment: Environment,
    pub generation: String,
    pub subject: String,
    pub session_id: String,
    pub astra_user_id: String,
    pub moi_principal_id: String,
    pub catalog_user_id: String,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
    pub workspace_id: Option<String>,
    pub role_id: Option<String>,
    /// Durable intent: a crashed/ambiguous rotating request is not replayable.
    #[serde(default)]
    pub refresh_pending: bool,
}

impl std::fmt::Debug for NativeSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeSession")
            .field("environment", &self.environment)
            .field("generation", &self.generation)
            .field("subject", &self.subject)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    version: u32,
    active: Option<String>,
    #[serde(default)]
    environments: BTreeMap<String, Environment>,
    /// None is a selected, logged-out environment, never a legacy fallback.
    sessions: BTreeMap<String, Option<NativeSession>>,
}

#[derive(Clone, Debug)]
pub struct NativeStore {
    root: PathBuf,
}

/// Short local publication/fence lease, never a workspace authorization grant.
/// Drop it before provider I/O. Logout and account/workspace changes serialize
/// against this lease through the existing native credential lock.
pub struct NativeSessionLease {
    _lock: File,
}

/// Cancellation wins only before the atomic rename is admitted. Callers must
/// join publication after requesting cancellation; never detach a writer.
#[derive(Clone)]
pub struct PublicationControl {
    phase: std::sync::Arc<std::sync::atomic::AtomicU8>,
    deadline: std::time::Instant,
}

impl PublicationControl {
    pub fn new(wait: std::time::Duration) -> Self {
        Self {
            phase: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
            deadline: std::time::Instant::now() + wait,
        }
    }

    pub fn cancel(&self) -> bool {
        self.phase
            .compare_exchange(
                0,
                1,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_ok()
    }

    fn check(&self) -> Result<(), String> {
        if self.phase.load(std::sync::atomic::Ordering::SeqCst) == 1 {
            return Err(
                "Operation stopped before credential publication; saved credentials are unchanged."
                    .into(),
            );
        }
        if std::time::Instant::now() >= self.deadline {
            return Err("Timed out waiting to publish credentials; the saved sign-in is unchanged. Close other sign-in operations and try again.".into());
        }
        Ok(())
    }

    fn begin_commit(&self) -> Result<(), String> {
        self.check()?;
        self.phase
            .compare_exchange(
                0,
                2,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .map_err(|_| {
                "Operation stopped before credential publication; saved credentials are unchanged."
                    .to_string()
            })?;
        self.check()
    }
}

/// Rename made the new session visible even when directory fsync failed.
pub struct PublishedLogin {
    pub session: NativeSession,
    pub previous: Option<NativeSession>,
    pub directory_synced: bool,
}

#[derive(Debug)]
pub struct Committed<T> {
    pub value: T,
    pub directory_synced: bool,
}

/// Read-only local status; observing it never sends a refresh request.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SignInStatus {
    SignedIn,
    RefreshInProgress,
    ReauthenticationRequired,
}

impl NativeStore {
    pub fn new() -> Result<Self, String> {
        let root = match std::env::var_os("MOI_AUTH_DIR") {
            Some(path) if !path.is_empty() => PathBuf::from(path),
            Some(_) => return Err("MOI_AUTH_DIR cannot be empty".into()),
            None => dirs::home_dir()
                .ok_or("home directory unavailable")?
                .join(".moi"),
        };
        if !root.is_absolute() {
            return Err("MOI_AUTH_DIR must be absolute".into());
        }
        Ok(Self { root })
    }

    pub fn with_directory(root: PathBuf) -> Self {
        Self { root }
    }

    /// Private local IPC may refer to this owner, never copy its credentials.
    pub fn directory(&self) -> &Path {
        &self.root
    }

    pub fn configured(&self) -> Result<bool, String> {
        match fs::symlink_metadata(&self.root) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(_) => return Err("cannot inspect MOI auth directory".into()),
            // A missing credential file is meaningful only inside a real
            // directory, never through a symlink (including a dangling one).
            Ok(metadata) if !metadata.is_dir() => self.check_dir()?,
            Ok(_) => (),
        }
        // An ordinary directory with no auth.json does not select MOI. Keep
        // legacy login usable without changing that directory's permissions.
        // Once a credential file exists, read() enforces all security checks.
        match fs::symlink_metadata(self.root.join("auth.json")) {
            Ok(_) => self.read().map(|s| s.active.is_some()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(_) => Err("cannot inspect MOI credential state".into()),
        }
    }

    pub fn current(&self) -> Result<NativeSession, String> {
        let state = self.read()?;
        let key = state.active.ok_or(MOI_NOT_CONFIGURED)?;
        state
            .sessions
            .get(&key)
            .and_then(Clone::clone)
            .ok_or_else(|| MOI_LOGGED_OUT.into())
    }

    pub fn device_model_lease(
        &self,
        expected: &NativeSession,
    ) -> Result<NativeSessionLease, String> {
        self.check_dir()?;
        let lock = private_open(&self.root.join("auth.lock"), true)?;
        FileExt::lock_shared(&lock).map_err(|_| "cannot lock MOI credentials")?;
        validate_device_session(&self.read_locked()?, expected)?;
        Ok(NativeSessionLease { _lock: lock })
    }

    pub fn selected_environment(&self) -> Result<Option<Environment>, String> {
        let state = self.read()?;
        match state.active {
            None => Ok(None),
            Some(key) => {
                let environment = state
                    .environments
                    .get(&key)
                    .ok_or("selected MOI environment is missing")?;
                environment.validate()?;
                if environment.key() != key {
                    return Err("selected MOI environment binding is invalid".into());
                }
                Ok(Some(environment.clone()))
            }
        }
    }

    pub fn publish(
        &self,
        session: NativeSession,
    ) -> Result<(NativeSession, Option<NativeSession>), String> {
        let result = self.publish_controlled(
            session,
            &PublicationControl::new(std::time::Duration::from_secs(10)),
        )?;
        Ok((result.session, result.previous))
    }

    pub fn publish_controlled(
        &self,
        session: NativeSession,
        control: &PublicationControl,
    ) -> Result<PublishedLogin, String> {
        self.publish_with_sync(session, control, sync_auth_directory)
    }

    fn publish_with_sync(
        &self,
        mut session: NativeSession,
        control: &PublicationControl,
        sync_directory: impl FnOnce(&Path) -> std::io::Result<()>,
    ) -> Result<PublishedLogin, String> {
        session.environment.validate()?;
        if [
            &session.subject,
            &session.session_id,
            &session.astra_user_id,
            &session.moi_principal_id,
            &session.catalog_user_id,
            &session.access_token,
            &session.refresh_token,
        ]
        .iter()
        .any(|s| s.is_empty())
        {
            return Err("cannot publish an incomplete MOI session".into());
        }
        session.generation = uuid::Uuid::new_v4().to_string();
        session.refresh_pending = false;
        session.workspace_id = None;
        session.role_id = None;
        let committed = self.transaction_with_sync(Some(control), sync_directory, |state| {
            let previous = state
                .active
                .as_ref()
                .and_then(|key| state.sessions.get(key))
                .and_then(Clone::clone);
            if let Some(old_key) = &state.active {
                state.sessions.insert(old_key.clone(), None);
            }
            let key = session.environment.key();
            state.active = Some(key.clone());
            state
                .environments
                .insert(key.clone(), session.environment.clone());
            state.sessions.insert(key, Some(session.clone()));
            Ok((session, previous))
        })?;
        Ok(PublishedLogin {
            session: committed.value.0,
            previous: committed.value.1,
            directory_synced: committed.directory_synced,
        })
    }

    pub fn logout(&self) -> Result<Committed<Option<NativeSession>>, String> {
        self.transaction(|state| {
            let key = state
                .active
                .as_ref()
                .ok_or("MOI authentication is not configured")?;
            Ok(state.sessions.insert(key.clone(), None).flatten())
        })
    }

    /// Context is bound to the login generation; never a workspace grant.
    /// The caller must select from the product owner's membership projection.
    pub fn select_workspace(
        &self,
        expected: &NativeSession,
        workspace: Option<&str>,
    ) -> Result<Committed<()>, String> {
        self.select_workspace_with_control(expected, workspace, None)
    }

    pub fn select_workspace_controlled(
        &self,
        expected: &NativeSession,
        workspace: Option<&str>,
        control: &PublicationControl,
    ) -> Result<Committed<()>, String> {
        self.select_workspace_with_control(expected, workspace, Some(control))
    }

    fn select_workspace_with_control(
        &self,
        expected: &NativeSession,
        workspace: Option<&str>,
        control: Option<&PublicationControl>,
    ) -> Result<Committed<()>, String> {
        if workspace
            .is_some_and(|v| v.is_empty() || v.len() > 128 || v.chars().any(char::is_whitespace))
        {
            return Err("invalid workspace identity".into());
        }
        self.update_with_control(expected, control, |session| {
            if session.workspace_id.as_deref() != workspace || session.role_id.is_some() {
                // The auth lock is held. Invalidate consent before publishing
                // the new selection, so A -> B -> A cannot revive old consent.
                self.clear_device_consent_locked()?;
            }
            session.workspace_id = workspace.map(str::to_owned);
            session.role_id = None;
            Ok(())
        })
    }

    fn update_with_control(
        &self,
        expected: &NativeSession,
        control: Option<&PublicationControl>,
        f: impl FnOnce(&mut NativeSession) -> Result<(), String>,
    ) -> Result<Committed<()>, String> {
        self.update_with_sync(expected, control, sync_auth_directory, f)
    }

    fn update_with_sync(
        &self,
        expected: &NativeSession,
        control: Option<&PublicationControl>,
        sync_directory: impl FnOnce(&Path) -> std::io::Result<()>,
        f: impl FnOnce(&mut NativeSession) -> Result<(), String>,
    ) -> Result<Committed<()>, String> {
        self.transaction_with_sync(control, sync_directory, |state| {
            let key = expected.environment.key();
            if state.active.as_ref() != Some(&key) {
                return Err("MOI environment changed during operation".into());
            }
            let session = state
                .sessions
                .get_mut(&key)
                .and_then(Option::as_mut)
                .ok_or("MOI session logged out during operation")?;
            if session.generation != expected.generation
                || session.subject != expected.subject
                || session.environment != expected.environment
            {
                return Err("MOI account changed during operation".into());
            }
            f(session)
        })
    }

    fn check_dir(&self) -> Result<(), String> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let meta = fs::symlink_metadata(&self.root)
                .map_err(|_| "cannot inspect MOI auth directory")?;
            if !meta.is_dir()
                || meta.file_type().is_symlink()
                || meta.mode() & 0o077 != 0
                || meta.uid() != unsafe { libc::geteuid() }
            {
                return Err("MOI auth directory must be owned by the current user, mode 0700, and not a symlink".into());
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            Err("MOI native authentication currently supports macOS and Linux".into())
        }
    }

    /// Validate the credential destination before opening a browser. This does
    /// not relax permissions or modify an existing credential file.
    pub fn prepare_for_login(&self) -> Result<(), String> {
        self.prepare_dir()
    }

    fn prepare_dir(&self) -> Result<(), String> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            match fs::DirBuilder::new().mode(0o700).create(&self.root) {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
                Err(_) => return Err("cannot create MOI auth directory".into()),
            }
        }
        self.check_dir()
    }

    fn read(&self) -> Result<State, String> {
        self.check_dir()?;
        let lock = private_open(&self.root.join("auth.lock"), true)?;
        FileExt::lock_shared(&lock).map_err(|_| "cannot lock MOI credentials")?;
        self.read_locked()
    }

    fn read_locked(&self) -> Result<State, String> {
        let path = self.root.join("auth.json");
        if fs::symlink_metadata(&path).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) {
            return Ok(State {
                version: VERSION,
                ..State::default()
            });
        }
        let mut body = Vec::new();
        private_open(&path, false)?
            .take(1_048_577)
            .read_to_end(&mut body)
            .map_err(|_| "cannot read MOI credentials")?;
        if body.len() > 1_048_576 {
            return Err("MOI credential file exceeds size limit".into());
        }
        let state: State =
            serde_json::from_slice(&body).map_err(|_| "invalid MOI credential file")?;
        if state.version != VERSION {
            return Err("unsupported MOI credential version".into());
        }
        Ok(state)
    }

    fn transaction<R>(
        &self,
        f: impl FnOnce(&mut State) -> Result<R, String>,
    ) -> Result<Committed<R>, String> {
        self.transaction_with_sync(None, sync_auth_directory, f)
    }

    fn transaction_with_sync<R>(
        &self,
        control: Option<&PublicationControl>,
        sync_directory: impl FnOnce(&Path) -> std::io::Result<()>,
        f: impl FnOnce(&mut State) -> Result<R, String>,
    ) -> Result<Committed<R>, String> {
        self.prepare_dir()?;
        let lock = private_open(&self.root.join("auth.lock"), true)?;
        if let Some(control) = control {
            loop {
                control.check()?;
                match FileExt::try_lock_exclusive(&lock) {
                    Ok(()) => break,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(25));
                    }
                    Err(_) => return Err("cannot lock MOI credentials".into()),
                }
            }
        } else {
            FileExt::lock_exclusive(&lock).map_err(|_| "cannot lock MOI credentials")?;
        }
        let mut state = self.read_locked()?;
        let result = f(&mut state)?;
        let body = serde_json::to_vec(&state).map_err(|_| "cannot encode MOI credentials")?;
        let tmp_path = self.root.join(format!("auth-{}.tmp", uuid::Uuid::new_v4()));
        let write = || -> Result<bool, String> {
            let mut tmp = private_create(&tmp_path)?;
            tmp.write_all(&body)
                .map_err(|_| "cannot write MOI credentials")?;
            tmp.sync_all().map_err(|_| "cannot sync MOI credentials")?;
            if let Some(control) = control {
                control.begin_commit()?;
            }
            fs::rename(&tmp_path, self.root.join("auth.json"))
                .map_err(|_| "cannot publish MOI credentials")?;
            // The commit is visible. Never return a pre-commit error here or
            // revoke the newly published session as if publication had failed.
            let directory_synced = sync_directory(&self.root).is_ok();
            if !directory_synced {
                tracing::warn!(
                    "MOI credentials are visible but directory durability is unconfirmed"
                );
            }
            Ok(directory_synced)
        };
        let directory_synced = write().inspect_err(|_| {
            let _ = fs::remove_file(&tmp_path);
        })?;
        Ok(Committed {
            value: result,
            directory_synced,
        })
    }
}

fn validate_device_identity<'a>(
    state: &'a State,
    expected: &NativeSession,
) -> Result<&'a NativeSession, String> {
    let key = expected.environment.key();
    let session = state
        .sessions
        .get(&key)
        .and_then(Option::as_ref)
        .ok_or("MOI session logged out during device model operation")?;
    if state.active.as_deref() != Some(key.as_str())
        || session.environment != expected.environment
        || session.generation != expected.generation
        || session.subject != expected.subject
        || session.session_id != expected.session_id
        || session.astra_user_id != expected.astra_user_id
        || session.moi_principal_id != expected.moi_principal_id
        || session.catalog_user_id != expected.catalog_user_id
        || session.workspace_id != expected.workspace_id
        || session.role_id != expected.role_id
    {
        return Err("MOI sign-in or workspace changed during device model operation".into());
    }
    Ok(session)
}

fn validate_device_session(state: &State, expected: &NativeSession) -> Result<(), String> {
    let session = validate_device_identity(state, expected)?;
    if session.refresh_pending
        || session.access_token.is_empty()
        || session.expires_at <= unix_now()?
    {
        return Err("MOI sign-in must be renewed before executing a device model".into());
    }
    Ok(())
}

fn sync_auth_directory(root: &Path) -> std::io::Result<()> {
    File::open(root).and_then(|file| file.sync_all())
}

fn private_create(path: &Path) -> Result<File, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .map_err(|_| "cannot securely create MOI credential file".into())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err("MOI authentication requires macOS or Linux".into())
    }
}

fn private_open(path: &Path, create: bool) -> Result<File, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let file = OpenOptions::new()
            .read(true)
            .write(create)
            .create(create)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .map_err(|_| "cannot securely open MOI credential file")?;
        let meta = file
            .metadata()
            .map_err(|_| "cannot inspect MOI credential file")?;
        if !meta.is_file()
            || meta.mode() & 0o077 != 0
            || meta.uid() != unsafe { libc::geteuid() }
            || meta.nlink() != 1
        {
            return Err(
                "MOI credential files must be private, current-user owned regular files".into(),
            );
        }
        Ok(file)
    }
    #[cfg(not(unix))]
    {
        let _ = (path, create);
        Err("MOI native authentication requires macOS or Linux".into())
    }
}

/// A frozen command identity. The helper returns only the access token and
/// binding; refresh/model/service secrets never leave the credential owner.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credential {
    pub version: u32,
    pub environment: String,
    pub issuer: String,
    pub subject: String,
    pub generation: String,
    pub endpoint: String,
    pub expires_at: i64,
    pub access_token: String,
    pub workspace_id: Option<String>,
    pub role_id: Option<String>,
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("environment", &self.environment)
            .field("subject", &self.subject)
            .field("generation", &self.generation)
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

pub const TOKEN_SERVICE_UNREACHABLE: &str = "cannot connect to token service; retry when available";
pub const TOKEN_ROTATION_UNCONFIRMED: &str = "token rotation was not confirmed; run astra login";
pub const TOKEN_ROTATION_REJECTED: &str = "token rotation rejected; run astra login";
pub const TOKEN_ROTATION_INTERRUPTED: &str =
    "previous token rotation was interrupted; run astra login";
pub const MOI_LOGGED_OUT: &str = "MOI session is logged out; run astra login";
pub const MOI_NOT_CONFIGURED: &str = "MOI authentication is not configured; run astra login";

/// Why a native credential read cannot return an access token. The variant is
/// the user-action decision; display text is only for logs and String boundaries.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CredentialFailure {
    #[error("{MOI_NOT_CONFIGURED}")]
    NotConfigured,
    #[error("{MOI_LOGGED_OUT}")]
    LoggedOut,
    #[error("{TOKEN_SERVICE_UNREACHABLE}")]
    NetworkUnavailable,
    #[error("{TOKEN_ROTATION_UNCONFIRMED}")]
    RotationUnconfirmed,
    #[error("{TOKEN_ROTATION_REJECTED}")]
    RotationRejected,
    #[error("{TOKEN_ROTATION_INTERRUPTED}")]
    RotationInterrupted,
    /// Another rotation holds the per-environment lock. The saved session is unchanged.
    #[error("MOI credential refresh is busy")]
    RefreshInProgress,
    #[error("MOI account or environment changed; restart Astra")]
    AccountChanged,
    /// Storage or protocol failure that does not prove the session is invalid.
    #[error("{0}")]
    Unavailable(String),
}

impl CredentialFailure {
    pub fn access_miss(&self) -> AccessMiss {
        match self {
            Self::NotConfigured | Self::LoggedOut => AccessMiss::NotLoggedIn,
            Self::NetworkUnavailable => AccessMiss::NetworkUnavailable,
            Self::RotationUnconfirmed | Self::RotationRejected | Self::RotationInterrupted => {
                AccessMiss::ReauthenticationRequired
            }
            Self::RefreshInProgress => AccessMiss::RefreshInProgress,
            Self::AccountChanged => AccessMiss::AccountChanged,
            Self::Unavailable(_) => AccessMiss::Unavailable,
        }
    }

    fn from_session_read(error: String) -> Self {
        match error.as_str() {
            MOI_LOGGED_OUT => Self::LoggedOut,
            MOI_NOT_CONFIGURED => Self::NotConfigured,
            _ => Self::Unavailable(error),
        }
    }
}

impl From<CredentialFailure> for String {
    fn from(value: CredentialFailure) -> Self {
        value.to_string()
    }
}

/// Why a native credential read cannot return an access token. Callers match
/// these variants; the display text is only for logs and existing String errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessMiss {
    NotLoggedIn,
    NetworkUnavailable,
    ReauthenticationRequired,
    RefreshInProgress,
    AccountChanged,
    Unavailable,
}

impl AccessMiss {
    /// Before the workbench exists, recovery commands must be shell commands.
    pub fn startup_warning(self) -> &'static str {
        match self {
            Self::NotLoggedIn => "You are not signed in. Run astra login.",
            Self::NetworkUnavailable => {
                "Cannot connect to the sign-in service. Your saved sign-in is unchanged; retry when the network is back."
            }
            Self::ReauthenticationRequired => {
                "Your sign-in could not be renewed. Run astra login to continue."
            }
            Self::RefreshInProgress => {
                "Your sign-in is still being refreshed. Try starting Astra again shortly; you do not need to log in again."
            }
            Self::AccountChanged => "Your sign-in changed. Restart Astra.",
            Self::Unavailable => "Cannot use your saved sign-in. Try starting Astra again.",
        }
    }

    pub fn user_warning(self) -> &'static str {
        match self {
            Self::NotLoggedIn => "  Not logged in. Use /login to authenticate.",
            Self::NetworkUnavailable => {
                "  Network unavailable. Your sign-in is unchanged; retry when the network is back."
            }
            Self::ReauthenticationRequired => "  Sign-in needs to be renewed. Run /login.",
            Self::RefreshInProgress => {
                "  Sign-in refresh is still running. Retry this message; do not log in again."
            }
            Self::AccountChanged => "  Sign-in changed. Restart Astra.",
            Self::Unavailable => {
                "  Could not use the saved sign-in. Retry this message; do not log in again."
            }
        }
    }
}

pub fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "cannot create native authentication client".into())
}

#[derive(Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: i64,
    pub token_type: String,
    pub id_token: Option<String>,
}

pub async fn bounded_json<T: serde::de::DeserializeOwned>(
    mut response: reqwest::Response,
) -> Result<T, String> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "authentication response interrupted")?
    {
        if body.len() + chunk.len() > 1_048_576 {
            return Err("authentication response exceeds size limit".into());
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| "invalid authentication response".into())
}

impl NativeStore {
    // Credential reads and transactions take filesystem locks. Keep those off
    // the async runtime, including the ordinary per-request fresh-token path.
    async fn blocking<R: Send + 'static>(
        &self,
        operation: impl FnOnce(Self) -> Result<R, String> + Send + 'static,
    ) -> Result<R, String> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || operation(store))
            .await
            .map_err(|_| "native credential operation interrupted".to_string())?
    }

    pub async fn current_off_runtime(&self) -> Result<NativeSession, String> {
        self.blocking(|store| store.current()).await
    }

    fn rotation_lock(
        &self,
        key: &str,
        wait: std::time::Duration,
    ) -> Result<std::sync::Arc<File>, CredentialFailure> {
        let rotation = private_open(&self.root.join(format!("refresh-{key}.lock")), true)
            .map_err(CredentialFailure::Unavailable)?;
        let deadline = std::time::Instant::now() + wait;
        loop {
            match FileExt::try_lock_exclusive(&rotation) {
                Ok(()) => return Ok(std::sync::Arc::new(rotation)),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        return Err(CredentialFailure::RefreshInProgress);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                Err(_) => {
                    return Err(CredentialFailure::Unavailable(
                        "cannot lock native credential refresh".into(),
                    ));
                }
            }
        }
    }

    pub async fn status(&self) -> Result<(NativeSession, SignInStatus), String> {
        self.blocking(|store| {
            let previous = store.current()?;
            if !previous.refresh_pending {
                return Ok((previous, SignInStatus::SignedIn));
            }
            // Hold the same lock used by credential() while re-reading pending.
            // A free lock is not evidence that an old refresh token is reusable.
            let rotation =
                match store.rotation_lock(&previous.environment.key(), std::time::Duration::ZERO) {
                    Ok(lock) => Some(lock),
                    Err(CredentialFailure::RefreshInProgress) => None,
                    Err(error) => return Err(error.into()),
                };
            let current = store.current()?;
            if current.generation != previous.generation
                || current.environment != previous.environment
            {
                return Err(CredentialFailure::AccountChanged.into());
            }
            let status = if !current.refresh_pending {
                SignInStatus::SignedIn
            } else if rotation.is_some() {
                SignInStatus::ReauthenticationRequired
            } else {
                SignInStatus::RefreshInProgress
            };
            Ok((current, status))
        })
        .await
    }

    pub async fn credential(
        &self,
        target: &str,
        expected_generation: Option<&str>,
    ) -> Result<Credential, CredentialFailure> {
        self.credential_with_lock_wait(
            target,
            expected_generation,
            std::time::Duration::from_secs(20),
        )
        .await
    }

    pub(crate) async fn credential_with_lock_wait(
        &self,
        target: &str,
        expected_generation: Option<&str>,
        lock_wait: std::time::Duration,
    ) -> Result<Credential, CredentialFailure> {
        let frozen = self
            .blocking(|store| store.current())
            .await
            .map_err(CredentialFailure::from_session_read)?;
        frozen
            .environment
            .validate()
            .map_err(CredentialFailure::Unavailable)?;
        if expected_generation.is_some_and(|v| v != frozen.generation) {
            return Err(CredentialFailure::AccountChanged);
        }
        if target != "moi" && target != "astra" {
            return Err(CredentialFailure::Unavailable(
                "unknown native credential target".into(),
            ));
        }
        // A separate per-environment lock serializes rotation without blocking
        // logout/account changes on the short global state transaction.
        // Fresh credentials need only a shared state read, not the rotation
        // lock. Pending intent still has to wait for its in-flight owner.
        let diagnostic = if frozen.expires_at
            <= unix_now().map_err(CredentialFailure::Unavailable)? + 60
            || frozen.refresh_pending
        {
            Some(RotationDiagnostic::new(self, &frozen))
        } else {
            None
        };
        let rotation = if let Some(diagnostic) = &diagnostic {
            let key = frozen.environment.key();
            diagnostic.record("lock", "waiting", None, None, None).await;
            let result = self
                .blocking(move |store| Ok(store.rotation_lock(&key, lock_wait)))
                .await
                .map_err(CredentialFailure::Unavailable)?;
            match result {
                Ok(rotation) => {
                    diagnostic
                        .record("lock", "acquired", None, None, None)
                        .await;
                    Some(rotation)
                }
                Err(error) => {
                    let reason = if error == CredentialFailure::RefreshInProgress {
                        "lock_wait_timeout"
                    } else {
                        "lock_failed"
                    };
                    diagnostic.record("lock", reason, None, None, None).await;
                    return Err(error);
                }
            }
        } else {
            None
        };
        let mut current = if rotation.is_some() {
            self.blocking(|store| store.current())
                .await
                .map_err(CredentialFailure::from_session_read)?
        } else {
            frozen.clone()
        };
        if current.generation != frozen.generation {
            return Err(CredentialFailure::AccountChanged);
        }
        if current.refresh_pending {
            if let Some(diagnostic) = &diagnostic {
                diagnostic
                    .record("pending", "previous_rotation_unsettled", None, None, None)
                    .await;
            }
            return Err(CredentialFailure::RotationInterrupted);
        }
        let now = unix_now().map_err(CredentialFailure::Unavailable)?;
        if let Some((rotation, diagnostic)) = rotation
            .zip(diagnostic)
            .filter(|_| current.expires_at <= now + 60)
        {
            let client = http_client().map_err(CredentialFailure::Unavailable)?;
            let request = client
                .post(&current.environment.token_endpoint)
                .form(&[
                    ("client_id", "astra-cli"),
                    ("grant_type", "refresh_token"),
                    ("refresh_token", &current.refresh_token),
                ])
                .build()
                .map_err(|_| {
                    CredentialFailure::Unavailable("cannot build token rotation request".into())
                })?;
            let expected = current.clone();
            let store = self.clone();
            // Settlement owns the refresh after the intent is durable. Dropping
            // this caller (a 5s accounting deadline, or an aborted turn) must
            // not abandon an in-flight rotation or clear a token that may
            // already have been consumed.
            let settlement = tokio::spawn(async move {
                settle_token_rotation(
                    store,
                    expected,
                    client,
                    request,
                    rotation,
                    diagnostic,
                    sync_auth_directory,
                )
                .await
            });
            current = settlement.await.map_err(|_| {
                tracing::warn!("native token rotation task panicked before settlement");
                CredentialFailure::Unavailable("native token rotation task failed".into())
            })??;
        }
        if current.generation != frozen.generation {
            return Err(CredentialFailure::AccountChanged);
        }
        Ok(Credential {
            version: VERSION,
            environment: current.environment.key(),
            issuer: current.environment.issuer,
            subject: current.subject,
            generation: current.generation,
            endpoint: if target == "moi" {
                current.environment.moi_url
            } else {
                current.environment.astra_url
            },
            expires_at: current.expires_at,
            access_token: current.access_token,
            workspace_id: current.workspace_id,
            role_id: current.role_id,
        })
    }
}

async fn settle_token_rotation<S>(
    store: NativeStore,
    expected: NativeSession,
    client: reqwest::Client,
    request: reqwest::Request,
    rotation: std::sync::Arc<std::fs::File>,
    diagnostic: RotationDiagnostic,
    sync_directory: S,
) -> Result<NativeSession, CredentialFailure>
where
    S: Fn(&Path) -> std::io::Result<()> + Clone + Send + 'static,
{
    diagnostic
        .record("pending_write", "started", None, None, None)
        .await;
    let write_guard = rotation.clone();
    let pending_expected = expected.clone();
    let pending_sync = sync_directory.clone();
    let pending = store
        .blocking(move |store| {
            // spawn_blocking outlives cancellation of its awaiting task.
            // Retain rotation ownership until the durable write finishes.
            let _write_guard = write_guard;
            store.update_with_sync(&pending_expected, None, pending_sync, |session| {
                session.refresh_pending = true;
                Ok(())
            })
        })
        .await;
    match pending {
        Err(error) => {
            diagnostic
                .record("pending_write", "storage_failed", None, None, Some(&error))
                .await;
            return Err(CredentialFailure::Unavailable(error));
        }
        Ok(committed) if !committed.directory_synced => {
            let error = "refresh intent is visible but its durability is unconfirmed";
            diagnostic
                .record("pending_write", "storage_failed", None, None, Some(error))
                .await;
            return Err(CredentialFailure::RotationInterrupted);
        }
        Ok(_) => {}
    }
    diagnostic
        .record("http", "request_started", None, None, None)
        .await;
    // After sending, any failure may hide a successful rotation. Keep the
    // intent; never automatically replay the previous refresh token. Only a
    // connect error proves the request never reached the issuer.
    let response = match client.execute(request).await {
        Ok(response) => response,
        Err(error) if error.is_connect() => {
            diagnostic
                .record(
                    "http",
                    "connect_failed",
                    None,
                    None,
                    Some(&transport_cause(error)),
                )
                .await;
            let expected = expected.clone();
            let write_guard = rotation.clone();
            let clear_sync = sync_directory.clone();
            let cleared = store
                .blocking(move |store| {
                    let _write_guard = write_guard;
                    store.update_with_sync(&expected, None, clear_sync, |session| {
                        session.refresh_pending = false;
                        Ok(())
                    })
                })
                .await;
            if let Err(error) = cleared {
                diagnostic
                    .record("pending_clear", "storage_failed", None, None, Some(&error))
                    .await;
                return Err(CredentialFailure::Unavailable(error));
            }
            diagnostic
                .record(
                    "settled",
                    "connection_failure_preserved_session",
                    None,
                    None,
                    None,
                )
                .await;
            return Err(log_rotation_failure(CredentialFailure::NetworkUnavailable));
        }
        Err(error) => {
            let reason = if error.is_timeout() {
                "request_timeout"
            } else {
                "transport_failed"
            };
            diagnostic
                .record("http", reason, None, None, Some(&transport_cause(error)))
                .await;
            return Err(log_rotation_failure(CredentialFailure::RotationUnconfirmed));
        }
    };
    let status = response.status().as_u16();
    let request_id = response_request_id(&response);
    if !response.status().is_success() {
        // Even a 5xx can be generated by a proxy or after the issuer
        // committed rotation. HTTP status is not proof of non-consumption.
        diagnostic
            .record(
                "http",
                "http_rejected",
                Some(status),
                request_id.as_deref(),
                None,
            )
            .await;
        return Err(log_rotation_failure(CredentialFailure::RotationRejected));
    }
    diagnostic
        .record(
            "response",
            "received",
            Some(status),
            request_id.as_deref(),
            None,
        )
        .await;
    let token: TokenResponse = match bounded_json(response).await {
        Ok(token) => token,
        Err(error) => {
            // The request was sent and the pending flag is durable. An unreadable
            // body is not proof the issuer rejected the refresh, and the old
            // refresh token must not be replayed.
            tracing::warn!(error = %error, "native token rotation response was unreadable");
            diagnostic
                .record(
                    "response",
                    "invalid_response",
                    Some(status),
                    request_id.as_deref(),
                    Some(&error),
                )
                .await;
            return Err(log_rotation_failure(CredentialFailure::RotationUnconfirmed));
        }
    };
    if token.access_token.is_empty()
        || token.refresh_token.is_empty()
        || token.expires_in <= 0
        || token.expires_in > 86400
        || !token.token_type.eq_ignore_ascii_case("bearer")
    {
        diagnostic
            .record(
                "response",
                "invalid_token_fields",
                Some(status),
                request_id.as_deref(),
                None,
            )
            .await;
        return Err(log_rotation_failure(CredentialFailure::RotationUnconfirmed));
    }
    diagnostic
        .record(
            "identity",
            "verification_started",
            Some(status),
            request_id.as_deref(),
            None,
        )
        .await;
    if let Err(error) = verify_rotated_identity(&expected, &token.access_token, &diagnostic).await {
        diagnostic
            .record(
                "identity",
                "verification_failed",
                Some(status),
                request_id.as_deref(),
                Some(&error),
            )
            .await;
        let _ = revoke(&expected.environment, &token.refresh_token).await;
        tracing::warn!(error = %error, "rotated token identity check failed");
        return Err(log_rotation_failure(CredentialFailure::RotationUnconfirmed));
    }
    let access_token = token.access_token.clone();
    let refresh_token = token.refresh_token.clone();
    let expires_in = token.expires_in;
    let write_guard = rotation;
    let saved = expected.clone();
    diagnostic
        .record("save", "started", Some(status), request_id.as_deref(), None)
        .await;
    let writer_store = store.clone();
    let write_result = tokio::task::spawn_blocking(move || {
        let _write_guard = write_guard;
        writer_store.update_with_sync(&saved, None, sync_directory, |session| {
            session.access_token = access_token;
            session.refresh_token = refresh_token;
            session.expires_at = unix_now()? + expires_in;
            session.refresh_pending = false;
            Ok(())
        })
    })
    .await;
    let durable = match write_result {
        Ok(Err(error)) => {
            diagnostic
                .record(
                    "save",
                    "storage_failed",
                    Some(status),
                    request_id.as_deref(),
                    Some(&error),
                )
                .await;
            let _ = revoke(&expected.environment, &token.refresh_token).await;
            tracing::warn!(cause = %error, "rotated token could not be saved");
            return Err(log_rotation_failure(classify_post_rotation_save(error)));
        }
        Err(_) => {
            // A worker panic does not establish whether rename committed.
            // Revoking the replacement could invalidate visible credentials.
            return Err(log_rotation_failure(CredentialFailure::RotationUnconfirmed));
        }
        Ok(Ok(committed)) if !committed.directory_synced => {
            tracing::warn!("rotated credentials are visible but crash durability is unconfirmed");
            false
        }
        Ok(Ok(_)) => true,
    };
    diagnostic
        .record(
            "settled",
            if durable {
                "success"
            } else {
                "visible_durability_unconfirmed"
            },
            Some(status),
            request_id.as_deref(),
            None,
        )
        .await;
    store
        .blocking(|store| store.current())
        .await
        .map_err(CredentialFailure::from_session_read)
}

fn classify_post_rotation_save(error: String) -> CredentialFailure {
    match error.as_str() {
        "MOI session logged out during operation" => CredentialFailure::LoggedOut,
        "MOI environment changed during operation" | "MOI account changed during operation" => {
            CredentialFailure::AccountChanged
        }
        _ => CredentialFailure::RotationUnconfirmed,
    }
}

fn log_rotation_failure(error: CredentialFailure) -> CredentialFailure {
    tracing::warn!(error = %error, "native token rotation failed to settle");
    error
}

async fn verify_rotated_identity(
    current: &NativeSession,
    token: &str,
    diagnostic: &RotationDiagnostic,
) -> Result<(), String> {
    use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
    if token.len() > 16 * 1024 {
        return Err("rotated UC token exceeds size limit".into());
    }
    let header = decode_header(token).map_err(|_| "invalid rotated UC token")?;
    if header.alg != Algorithm::RS256 {
        return Err("invalid rotated UC token algorithm".into());
    }
    let kid = header.kid.ok_or("missing rotated UC token signing key")?;
    let response = http_client()?
        .get(&current.environment.jwks_uri)
        .send()
        .await;
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            let reason = if error.is_timeout() {
                "request_timeout"
            } else {
                "transport_failed"
            };
            diagnostic
                .record(
                    "signing_keys",
                    reason,
                    None,
                    None,
                    Some(&transport_cause(error)),
                )
                .await;
            return Err("UC signing keys unavailable".into());
        }
    };
    diagnostic
        .record(
            "signing_keys",
            "received",
            Some(response.status().as_u16()),
            response_request_id(&response).as_deref(),
            None,
        )
        .await;
    if !response.status().is_success() {
        return Err("UC signing keys unavailable".into());
    }
    let keys: JwkSet = bounded_json(response).await?;
    let key = DecodingKey::from_jwk(keys.find(&kid).ok_or("unknown UC signing key")?)
        .map_err(|_| "invalid UC signing key")?;
    #[derive(Clone, Deserialize)]
    struct Claims {
        sub: String,
        sid: String,
        azp: String,
        aud: Vec<String>,
        iat: i64,
    }
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_issuer(&[&current.environment.issuer]);
    validation.set_audience(&["astra-api", "aistudio-api"]);
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub", "iat"]);
    validation.validate_nbf = true;
    validation.leeway = 10;
    let claims = decode::<Claims>(token, &key, &validation)
        .map_err(|_| "rotated UC token verification failed")?
        .claims;
    if claims.sub != current.subject
        || claims.sid != current.session_id
        || claims.azp != "astra-cli"
        || claims.iat > unix_now()? + 10
        || claims.aud.len() != 2
        || !claims.aud.iter().any(|v| v == "astra-api")
        || !claims.aud.iter().any(|v| v == "aistudio-api")
    {
        return Err("rotated UC token identity changed; run astra login".into());
    }
    Ok(())
}

pub fn unix_now() -> Result<i64, String> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .map_err(|_| "system clock is invalid".into())
}

pub async fn revoke(environment: &Environment, refresh_token: &str) -> Result<(), String> {
    environment.validate()?;
    let response = http_client()?
        .post(&environment.revocation_endpoint)
        .form(&[
            ("client_id", "astra-cli"),
            ("token_type_hint", "refresh_token"),
            ("token", refresh_token),
        ])
        .send()
        .await
        .map_err(|_| "remote logout not confirmed; local session has been removed")?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err("remote logout not confirmed; local session has been removed".into())
    }
}

#[cfg(all(test, unix))]
mod tests {
    #[cfg(unix)]
    #[test]
    fn login_preflight_rejects_unsafe_directory_without_changing_it() {
        use std::os::unix::fs::PermissionsExt;
        let parent = tempfile::tempdir().unwrap();
        let path = parent.path().join("auth");
        let store = super::NativeStore::with_directory(path.clone());
        store.prepare_for_login().unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(store.prepare_for_login().is_err());
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert!(!path.join("auth.json").exists());
    }

    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    async fn wait_for_diagnostics(store: &NativeStore, required: &[(&str, &str)]) -> String {
        let path = store.root.join("auth-refresh.jsonl");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let log = fs::read_to_string(&path).unwrap_or_default();
                if let Ok(events) = log
                    .lines()
                    .map(serde_json::from_str::<serde_json::Value>)
                    .collect::<Result<Vec<_>, _>>()
                    && required.iter().all(|(stage, reason)| {
                        events
                            .iter()
                            .any(|event| event["stage"] == *stage && event["reason"] == *reason)
                    })
                {
                    return log;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("refresh diagnostic was not written")
    }

    struct RotationFixture {
        environment: Environment,
        rotations: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        revocations: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        entered: std::sync::Arc<tokio::sync::Notify>,
        release: std::sync::Arc<tokio::sync::Notify>,
        server: tokio::task::JoinHandle<()>,
    }
    impl Drop for RotationFixture {
        fn drop(&mut self) {
            self.server.abort();
        }
    }

    async fn rotation_fixture(subject: &'static str) -> RotationFixture {
        use axum::{
            Form, Json, Router,
            routing::{get, post},
        };
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
        use rsa::{pkcs1::EncodeRsaPrivateKey, traits::PublicKeyParts};
        use std::sync::{
            Arc, LazyLock,
            atomic::{AtomicUsize, Ordering},
        };
        static KEY: LazyLock<rsa::RsaPrivateKey> =
            LazyLock::new(|| rsa::RsaPrivateKey::new(&mut rand::rngs::OsRng, 2048).unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let issuer = format!("{origin}/realms/moi");
        let environment = Environment {
            issuer: issuer.clone(),
            authorization_endpoint: format!("{origin}/api/v1/uc/oauth2/authorize"),
            token_endpoint: format!("{issuer}/protocol/openid-connect/token"),
            revocation_endpoint: format!("{issuer}/protocol/openid-connect/revoke"),
            jwks_uri: format!("{issuer}/protocol/openid-connect/certs"),
            ..session("A").environment
        };
        let der = KEY.to_pkcs1_der().unwrap();
        let encoding = jsonwebtoken::EncodingKey::from_rsa_der(der.as_bytes());
        let jwks = serde_json::json!({"keys":[{"kty":"RSA","alg":"RS256","use":"sig","kid":"synthetic-test", "n":URL_SAFE_NO_PAD.encode(KEY.n().to_bytes_be()),"e":URL_SAFE_NO_PAD.encode(KEY.e().to_bytes_be())}]});
        let rotations = Arc::new(AtomicUsize::new(0));
        let revocations = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let (calls, revoked, started, proceed) = (
            rotations.clone(),
            revocations.clone(),
            entered.clone(),
            release.clone(),
        );
        let app = Router::new()
            .route("/realms/moi/protocol/openid-connect/certs", get(move || async move { Json(jwks) }))
            .route("/realms/moi/protocol/openid-connect/token", post(move |Form(form): Form<BTreeMap<String,String>>| async move {
                assert_eq!(form["client_id"],"astra-cli");
                assert_eq!(form["grant_type"],"refresh_token");
                assert_eq!(form["refresh_token"],"synthetic-refresh");
                calls.fetch_add(1,Ordering::SeqCst);
                started.notify_one();
                proceed.notified().await;
                let now = unix_now().unwrap();
                let claims = serde_json::json!({"iss":issuer,"sub":subject,"sid":"sid-A","azp":"astra-cli","aud":["astra-api","aistudio-api"],"iat":now,"exp":now+900});
                let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
                header.kid = Some("synthetic-test".into());
                Json(serde_json::json!({"access_token":jsonwebtoken::encode(&header,&claims,&encoding).unwrap(),"refresh_token":"synthetic-rotated","expires_in":900,"token_type":"Bearer"}))
            }))
            .route("/realms/moi/protocol/openid-connect/revoke", post(move |Form(form): Form<BTreeMap<String,String>>| async move {
                assert_eq!(form["token"],"synthetic-rotated");
                revoked.fetch_add(1,Ordering::SeqCst);
                axum::http::StatusCode::OK
            }));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        RotationFixture {
            environment,
            rotations,
            revocations,
            entered,
            release,
            server,
        }
    }

    #[tokio::test]
    async fn account_switch_while_waiting_for_refresh_does_not_return_new_accounts_token() {
        let (_directory, store) = store();
        let (published, _) = store.publish(session("A")).unwrap();
        let lock = store
            .rotation_lock(&published.environment.key(), std::time::Duration::ZERO)
            .unwrap();
        store
            .update_with_control(&published, None, |current| {
                current.refresh_pending = true;
                Ok(())
            })
            .unwrap();
        let pending = store.credential("astra", Some(&published.generation));
        tokio::pin!(pending);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(80), &mut pending)
                .await
                .is_err()
        );
        let (replacement, _) = store.publish(session("B")).unwrap();
        drop(lock);
        assert_eq!(
            pending.await.unwrap_err(),
            CredentialFailure::AccountChanged
        );
        assert_eq!(store.current().unwrap().generation, replacement.generation);
        assert!(!store.current().unwrap().refresh_pending);
    }

    #[tokio::test]
    async fn status_observes_live_rotation_without_refreshing_or_requiring_login() {
        use std::sync::atomic::Ordering;
        let fixture = rotation_fixture("A").await;
        let (_directory, store) = store();
        let mut expiring = session("A");
        expiring.environment = fixture.environment.clone();
        expiring.expires_at = unix_now().unwrap();
        store.publish(expiring).unwrap();
        assert_eq!(store.status().await.unwrap().1, SignInStatus::SignedIn);
        assert_eq!(fixture.rotations.load(Ordering::SeqCst), 0);
        let clone = store.clone();
        let helper = tokio::spawn(async move { clone.credential("astra", None).await });
        fixture.entered.notified().await;
        assert_eq!(
            store.status().await.unwrap().1,
            SignInStatus::RefreshInProgress
        );
        assert_eq!(fixture.rotations.load(Ordering::SeqCst), 1);
        fixture.release.notify_one();
        helper.await.unwrap().unwrap();
        assert_eq!(store.status().await.unwrap().1, SignInStatus::SignedIn);
        assert_eq!(fixture.rotations.load(Ordering::SeqCst), 1);
        let log = wait_for_diagnostics(
            &store,
            &[("signing_keys", "received"), ("settled", "success")],
        )
        .await;
        let events: Vec<serde_json::Value> = log
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(events.iter().any(|event| event["reason"] == "success"));
        assert!(events.iter().any(|event| event["stage"] == "signing_keys"));
        assert!(!log.contains("synthetic-refresh"));
        assert!(!log.contains("synthetic-rotated"));
        assert!(!log.contains(&store.current().unwrap().access_token));
    }

    #[test]
    fn killed_refresh_owner_releases_lock_but_does_not_authorize_replay() {
        const CHILD: &str = "ASTRA_REFRESH_CRASH_FIXTURE_DIR";
        if let Some(root) = std::env::var_os(CHILD) {
            let store = NativeStore::with_directory(root.into());
            let session = store.current().unwrap();
            let _rotation = store
                .rotation_lock(&session.environment.key(), std::time::Duration::ZERO)
                .unwrap();
            store
                .update_with_control(&session, None, |current| {
                    current.refresh_pending = true;
                    Ok(())
                })
                .unwrap();
            std::fs::write(store.root.join("child-ready"), b"ready").unwrap();
            loop {
                std::thread::park();
            }
        }
        let (_directory, store) = store();
        store.publish(session("A")).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "native::tests::killed_refresh_owner_releases_lock_but_does_not_authorize_replay",
            ])
            .env(CHILD, &store.root)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !store.root.join("child-ready").exists() {
            if std::time::Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("child did not acquire refresh lock");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        child.kill().unwrap();
        child.wait().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            assert_eq!(
                store.status().await.unwrap().1,
                SignInStatus::ReauthenticationRequired
            );
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                store.credential("astra", None),
            )
            .await
            .unwrap();
            assert_eq!(result.unwrap_err(), CredentialFailure::RotationInterrupted);
        });
        assert!(store.current().unwrap().refresh_pending);
        assert_eq!(store.current().unwrap().refresh_token, "synthetic-refresh");
    }

    #[tokio::test]
    async fn concurrent_helpers_rotate_exactly_once_and_keep_the_command_generation() {
        use std::sync::atomic::Ordering;
        let fixture = rotation_fixture("A").await;
        let (_directory, store) = store();
        let mut expiring = session("A");
        expiring.environment = fixture.environment.clone();
        expiring.expires_at = unix_now().unwrap();
        let (published, _) = store.publish(expiring).unwrap();
        fixture.release.notify_one();
        let (a, b) = tokio::join!(
            store.credential("astra", Some(&published.generation)),
            store.credential("moi", Some(&published.generation))
        );
        assert_eq!(a.unwrap().generation, published.generation);
        assert_eq!(b.unwrap().generation, published.generation);
        assert_eq!(fixture.rotations.load(Ordering::SeqCst), 1);
        assert_eq!(store.current().unwrap().refresh_token, "synthetic-rotated");
        assert!(!store.current().unwrap().refresh_pending);
    }

    #[tokio::test]
    async fn logout_during_rotation_keeps_tombstone_and_revokes_the_orphan() {
        use std::sync::atomic::Ordering;
        let fixture = rotation_fixture("A").await;
        let (_directory, store) = store();
        let mut expiring = session("A");
        expiring.environment = fixture.environment.clone();
        expiring.expires_at = unix_now().unwrap();
        store.publish(expiring).unwrap();
        let clone = store.clone();
        let helper = tokio::spawn(async move { clone.credential("moi", None).await });
        fixture.entered.notified().await;
        store.logout().unwrap();
        fixture.release.notify_one();
        assert!(helper.await.unwrap().is_err());
        assert!(store.configured().unwrap() && store.current().is_err());
        assert_eq!(fixture.revocations.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn rotated_token_cannot_change_the_authenticated_subject() {
        use std::sync::atomic::Ordering;
        let fixture = rotation_fixture("wrong-account").await;
        let (_directory, store) = store();
        let mut expiring = session("A");
        expiring.environment = fixture.environment.clone();
        expiring.expires_at = unix_now().unwrap();
        store.publish(expiring).unwrap();
        fixture.release.notify_one();
        assert!(
            store
                .credential("moi", None)
                .await
                .unwrap_err()
                .to_string()
                .contains("not confirmed")
        );
        assert_eq!(fixture.revocations.load(Ordering::SeqCst), 1);
        assert!(store.current().unwrap().refresh_pending);
        assert_eq!(store.current().unwrap().subject, "A");
    }

    pub(super) fn session(subject: &str) -> NativeSession {
        let issuer = "https://uc.example.test/realms/moi";
        NativeSession {
            environment: Environment {
                issuer: issuer.into(),
                astra_url: "https://astra.example.test".into(),
                moi_url: "https://moi.example.test/newmoi".into(),
                authorization_endpoint: "https://uc.example.test/api/v1/uc/oauth2/authorize".into(),
                token_endpoint: format!("{issuer}/protocol/openid-connect/token"),
                revocation_endpoint: format!("{issuer}/protocol/openid-connect/revoke"),
                jwks_uri: format!("{issuer}/protocol/openid-connect/certs"),
            },
            generation: "caller-cannot-choose-generation".into(),
            subject: subject.into(),
            session_id: "sid-A".into(),
            astra_user_id: "astra-A".into(),
            moi_principal_id: "moi-A".into(),
            catalog_user_id: "catalog-A".into(),
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            expires_at: unix_now().unwrap() + 3600,
            workspace_id: Some("stale-workspace".into()),
            role_id: Some("stale-role".into()),
            refresh_pending: false,
        }
    }

    pub(super) fn store() -> (tempfile::TempDir, NativeStore) {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let store = NativeStore::with_directory(directory.path().to_path_buf());
        (directory, store)
    }

    #[test]
    fn device_fence_lease_serializes_logout_and_rejects_a_changed_login_or_workspace() {
        let (directory, store) = store();
        let current = store.publish(session("A")).unwrap().0;
        let lease = store.device_model_lease(&current).unwrap();
        let writer = private_open(&directory.path().join("auth.lock"), true).unwrap();
        assert_eq!(
            FileExt::try_lock_exclusive(&writer).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        drop(lease);
        FileExt::try_lock_exclusive(&writer).unwrap();
        drop(writer);
        store
            .select_workspace(&current, Some("workspace-next"))
            .unwrap();
        assert!(store.device_model_lease(&current).is_err());
        let changed = store.current().unwrap();
        assert!(store.device_model_lease(&changed).is_ok());
        store.publish(session("B")).unwrap();
        assert!(store.device_model_lease(&changed).is_err());
        let latest = store.current().unwrap();
        store.logout().unwrap();
        assert!(store.device_model_lease(&latest).is_err());
    }

    #[test]
    fn unused_directory_does_not_select_moi_or_relax_credential_security() {
        let (directory, store) = store();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!store.configured().unwrap());
        assert!(!directory.path().join("auth.lock").exists());
        assert_eq!(
            fs::metadata(directory.path()).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert!(store.publish(session("A")).is_err());
        assert!(!directory.path().join("auth.json").exists());

        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        store.publish(session("A")).unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(store.configured().is_err());
    }

    #[test]
    fn configured_rejects_linked_roots_even_without_credentials() {
        let directory = tempfile::tempdir().unwrap();
        let link = directory.path().join("link");
        let target = directory.path().join("target");
        symlink(&target, &link).unwrap();
        let store = NativeStore::with_directory(link);
        assert!(store.configured().is_err());
        fs::create_dir(&target).unwrap();
        assert!(store.configured().is_err());
        assert!(
            !NativeStore::with_directory(directory.path().join("absent"))
                .configured()
                .unwrap()
        );
    }

    #[test]
    fn atomic_publication_resets_context_and_logout_is_sticky() {
        let (directory, store) = store();
        assert!(!store.configured().unwrap());
        assert!(store.publish(session("A")).unwrap().1.is_none());
        let first = store.current().unwrap();
        assert_ne!(first.generation, "caller-cannot-choose-generation");
        assert!(first.workspace_id.is_none() && first.role_id.is_none());
        assert!(store.configured().unwrap());
        assert_eq!(
            fs::metadata(directory.path().join("auth.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let old = store.publish(session("B")).unwrap().1.unwrap();
        assert_eq!(old.generation, first.generation);
        assert_ne!(store.current().unwrap().generation, first.generation);
        assert!(
            store
                .update_with_control(&first, None, |s| {
                    s.access_token = "stale".into();
                    Ok(())
                })
                .is_err()
        );
        assert_eq!(store.logout().unwrap().value.unwrap().subject, "B");
        assert!(store.configured().unwrap());
        assert!(store.current().is_err());
        assert_eq!(
            store.selected_environment().unwrap(),
            Some(first.environment)
        );
        assert!(store.logout().unwrap().value.is_none());
        let disk = fs::read_to_string(directory.path().join("auth.json")).unwrap();
        assert!(!disk.contains("synthetic-access") && !disk.contains("synthetic-refresh"));
    }

    #[test]
    fn environment_switch_removes_old_credentials() {
        let (_directory, store) = store();
        store.publish(session("A")).unwrap();
        let mut second = session("B");
        second.environment.astra_url = "https://second.example.test".into();
        store.publish(second).unwrap();
        let state = store.read().unwrap();
        assert!(
            state
                .sessions
                .get(&session("A").environment.key())
                .unwrap()
                .is_none()
        );
        assert_eq!(store.current().unwrap().subject, "B");
    }

    #[test]
    fn corrupt_insecure_and_linked_stores_fail_closed() {
        let (directory, store) = store();
        store.publish(session("A")).unwrap();
        let path = directory.path().join("auth.json");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(store.current().is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&path, "corrupt").unwrap();
        assert!(store.configured().is_err());
        fs::remove_file(&path).unwrap();
        let target = directory.path().join("target");
        fs::write(&target, "do-not-overwrite").unwrap();
        symlink(&target, &path).unwrap();
        assert!(store.publish(session("B")).is_err());
        assert_eq!(fs::read_to_string(target).unwrap(), "do-not-overwrite");
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(store.current().is_err());
    }

    #[test]
    fn protocol_endpoints_are_pinned() {
        let original = session("A").environment;
        assert!(original.validate().is_ok());
        for address in [
            "http://outside.example.test/token",
            "https://user:pass@uc.example.test/token",
            "https://other.example.test/token",
            "https://uc.example.test/other",
        ] {
            let mut changed = original.clone();
            changed.token_endpoint = address.into();
            assert!(changed.validate().is_err());
        }
        assert!(secure_url("http://127.0.0.1:18100").is_ok());
        assert!(secure_url("https://uc.example.test?redirect=evil").is_err());
        assert_ne!(original.key(), {
            let mut changed = original;
            changed.issuer.push_str("-test");
            changed.key()
        });
    }

    #[tokio::test]
    async fn credential_freezes_generation_and_never_exports_refresh_token() {
        let (_directory, store) = store();
        store.publish(session("A")).unwrap();
        let first = store.current().unwrap();
        let credential = store
            .credential("moi", Some(&first.generation))
            .await
            .unwrap();
        let output = serde_json::to_string(&credential).unwrap();
        assert!(!output.contains("refresh"));
        assert_eq!(credential.endpoint, first.environment.moi_url);
        assert!(store.credential("genesis", None).await.is_err());
        store.publish(session("B")).unwrap();
        assert!(
            store
                .credential("moi", Some(&first.generation))
                .await
                .is_err()
        );
        let current = store.current().unwrap();
        store
            .update_with_control(&current, None, |s| {
                s.refresh_pending = true;
                Ok(())
            })
            .unwrap();
        assert!(store.credential("astra", None).await.is_err());
    }

    #[tokio::test]
    async fn fresh_credentials_do_not_wait_for_rotation_lock() {
        let (_directory, store) = store();
        let (published, _) = store.publish(session("A")).unwrap();
        let rotation = private_open(
            &store
                .root
                .join(format!("refresh-{}.lock", published.environment.key())),
            true,
        )
        .unwrap();
        FileExt::lock_exclusive(&rotation).unwrap();
        let credentials = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::join!(
                store.credential("astra", None),
                store.credential("moi", None)
            )
        })
        .await
        .expect("fresh credentials must not acquire the rotation lock");
        assert_eq!(credentials.0.unwrap().access_token, published.access_token);
        assert_eq!(credentials.1.unwrap().access_token, published.access_token);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn credential_state_lock_does_not_block_async_runtime() {
        let (_directory, store) = store();
        store.publish(session("A")).unwrap();
        let lock = private_open(&store.root.join("auth.lock"), true).unwrap();
        FileExt::lock_exclusive(&lock).unwrap();
        let (released, observed) = std::sync::mpsc::channel();
        // Bound even a regressed blocking implementation so this test cannot
        // hang the suite. A responsive runtime releases this lock immediately.
        let watchdog = std::thread::spawn(move || {
            let prompted = observed
                .recv_timeout(std::time::Duration::from_secs(2))
                .is_ok();
            drop(lock);
            prompted
        });
        let clone = store.clone();
        let request = tokio::spawn(async move { clone.credential("astra", None).await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let _ = released.send(());
        assert!(request.await.unwrap().is_ok());
        assert!(
            watchdog.join().unwrap(),
            "filesystem locking stalled the async runtime"
        );
    }

    #[tokio::test]
    async fn refresh_lock_wait_is_refresh_in_progress_not_reauthentication() {
        let (_directory, store) = store();
        let (published, _) = store.publish(session("A")).unwrap();
        store
            .update_with_control(&published, None, |session| {
                session.refresh_pending = true;
                Ok(())
            })
            .unwrap();
        let lock = private_open(
            &store.root.join(format!(
                "refresh-{}.lock",
                store.current().unwrap().environment.key()
            )),
            true,
        )
        .unwrap();
        FileExt::lock_exclusive(&lock).unwrap();
        let error = store
            .credential_with_lock_wait("astra", None, std::time::Duration::from_millis(80))
            .await
            .unwrap_err();
        assert_eq!(error, CredentialFailure::RefreshInProgress);
        assert_eq!(
            error.access_miss(),
            AccessMiss::RefreshInProgress,
            "a busy rotation lock must not ask the user to log in again"
        );
        assert!(store.current().unwrap().refresh_pending);
        drop(lock);
    }

    async fn rotate_with_sync<S>(
        store: NativeStore,
        expected: NativeSession,
        sync: S,
    ) -> Result<NativeSession, CredentialFailure>
    where
        S: Fn(&Path) -> std::io::Result<()> + Clone + Send + 'static,
    {
        let rotation = store
            .rotation_lock(&expected.environment.key(), std::time::Duration::ZERO)
            .unwrap();
        let client = http_client().unwrap();
        let request = client
            .post(&expected.environment.token_endpoint)
            .form(&[
                ("client_id", "astra-cli"),
                ("grant_type", "refresh_token"),
                ("refresh_token", expected.refresh_token.as_str()),
            ])
            .build()
            .unwrap();
        let diagnostic = RotationDiagnostic::new(&store, &expected);
        settle_token_rotation(store, expected, client, request, rotation, diagnostic, sync).await
    }

    async fn prepared_rotation() -> (
        RotationFixture,
        tempfile::TempDir,
        NativeStore,
        NativeSession,
    ) {
        let fixture = rotation_fixture("A").await;
        let (directory, store) = store();
        let mut expired = session("A");
        expired.environment = fixture.environment.clone();
        expired.expires_at = unix_now().unwrap();
        let (expected, _) = store.publish(expired).unwrap();
        (fixture, directory, store, expected)
    }

    #[tokio::test]
    async fn unconfirmed_pending_intent_never_sends_refresh() {
        use std::sync::atomic::Ordering;
        let (fixture, _directory, store, expected) = prepared_rotation().await;
        let result = rotate_with_sync(store.clone(), expected, |_| {
            Err(std::io::Error::other("injected sync failure"))
        })
        .await;
        assert!(matches!(
            result,
            Err(CredentialFailure::RotationInterrupted)
        ));
        assert_eq!(fixture.rotations.load(Ordering::SeqCst), 0);
        assert!(store.current().unwrap().refresh_pending);
    }

    #[tokio::test]
    async fn visible_rotated_token_is_not_revoked_when_directory_sync_fails() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let (fixture, _directory, store, expected) = prepared_rotation().await;
        let calls = Arc::new(AtomicUsize::new(0));
        fixture.release.notify_one();
        let result = rotate_with_sync(store.clone(), expected, move |root| {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                sync_auth_directory(root)
            } else {
                Err(std::io::Error::other("injected sync failure"))
            }
        })
        .await
        .unwrap();
        assert_eq!(result.refresh_token, "synthetic-rotated");
        assert_eq!(store.current().unwrap().refresh_token, "synthetic-rotated");
        assert_eq!(fixture.rotations.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.revocations.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn unknown_worker_outcome_does_not_revoke_visible_rotated_token() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let (fixture, _directory, store, expected) = prepared_rotation().await;
        let calls = Arc::new(AtomicUsize::new(0));
        fixture.release.notify_one();
        let result = rotate_with_sync(store.clone(), expected, move |root| {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                sync_auth_directory(root)
            } else {
                panic!("injected failure after rename")
            }
        })
        .await;
        assert!(matches!(
            result,
            Err(CredentialFailure::RotationUnconfirmed)
        ));
        assert_eq!(store.current().unwrap().refresh_token, "synthetic-rotated");
        assert_eq!(fixture.revocations.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn unsaved_rotation_requires_reauthentication_and_is_not_replayed() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::Ordering;
        let fixture = rotation_fixture("A").await;
        let (directory, store) = store();
        let mut expiring = session("A");
        expiring.environment = fixture.environment.clone();
        expiring.expires_at = unix_now().unwrap();
        store.publish(expiring).unwrap();
        let clone = store.clone();
        let caller = tokio::spawn(async move { clone.credential("astra", None).await });
        fixture.entered.notified().await;
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o500)).unwrap();
        fixture.release.notify_one();
        let error = caller.await.unwrap().unwrap_err();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(error, CredentialFailure::RotationUnconfirmed);
        assert_eq!(error.access_miss(), AccessMiss::ReauthenticationRequired);
        assert_eq!(fixture.revocations.load(Ordering::SeqCst), 1);
        let current = store.current().unwrap();
        assert!(current.refresh_pending);
        assert_eq!(current.refresh_token, "synthetic-refresh");
        assert_eq!(
            store.credential("astra", None).await.unwrap_err(),
            CredentialFailure::RotationInterrupted
        );
        assert_eq!(fixture.rotations.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn connection_failure_does_not_poison_refresh() {
        let mut fixture = rotation_fixture("A").await;
        let (_directory, store) = store();
        let mut expiring = session("A");
        expiring.environment = fixture.environment.clone();
        expiring.expires_at = unix_now().unwrap();
        store.publish(expiring).unwrap();
        fixture.server.abort();
        // Wait until the listener has been dropped, so connect is refused.
        let _ = (&mut fixture.server).await;
        for _ in 0..2 {
            assert!(
                store
                    .credential("moi", None)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("cannot connect")
            );
            assert!(!store.current().unwrap().refresh_pending);
            assert_eq!(store.current().unwrap().refresh_token, "synthetic-refresh");
        }
    }

    #[tokio::test]
    async fn cancelled_caller_still_settles_rotation_after_pending_is_durable() {
        let fixture = rotation_fixture("A").await;
        let (_directory, store) = store();
        let mut expiring = session("A");
        expiring.environment = fixture.environment.clone();
        expiring.expires_at = unix_now().unwrap();
        store.publish(expiring).unwrap();
        let clone = store.clone();
        let caller = tokio::spawn(async move { clone.credential("moi", None).await });
        fixture.entered.notified().await;
        assert!(store.current().unwrap().refresh_pending);
        caller.abort();
        fixture.release.notify_one();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let current = store.current().unwrap();
            if !current.refresh_pending && current.refresh_token == "synthetic-rotated" {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "detached rotation did not settle after the caller was cancelled"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn unreadable_rotation_response_requires_reauthentication() {
        for body in ["{}", ""] {
            assert_unreadable_rotation_requires_reauthentication(body).await;
        }
    }

    async fn assert_unreadable_rotation_requires_reauthentication(body: &str) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let payload = body.to_string();
        let request = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut input = [0; 8192];
            let len = stream.read(&mut input).await.unwrap();
            assert!(
                String::from_utf8_lossy(&input[..len])
                    .starts_with("POST /realms/moi/protocol/openid-connect/token ")
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let (_directory, store) = store();
        let mut expiring = session("A");
        expiring.environment = Environment {
            issuer: format!("{origin}/realms/moi"),
            authorization_endpoint: format!("{origin}/api/v1/uc/oauth2/authorize"),
            token_endpoint: format!("{origin}/realms/moi/protocol/openid-connect/token"),
            revocation_endpoint: format!("{origin}/realms/moi/protocol/openid-connect/revoke"),
            jwks_uri: format!("{origin}/realms/moi/protocol/openid-connect/certs"),
            ..expiring.environment
        };
        expiring.expires_at = unix_now().unwrap();
        store.publish(expiring).unwrap();
        let error = store.credential("moi", None).await.unwrap_err();
        assert_eq!(error, CredentialFailure::RotationUnconfirmed);
        assert_eq!(error.access_miss(), AccessMiss::ReauthenticationRequired);
        request.await.unwrap();
        assert!(store.current().unwrap().refresh_pending);
        let followed = store.credential("moi", None).await.unwrap_err();
        assert_eq!(followed, CredentialFailure::RotationInterrupted);
    }

    #[tokio::test]
    async fn dropped_rotation_response_requires_reauthentication() {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let request = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut input = [0; 8192];
            let _ = stream.read(&mut input).await.unwrap();
            drop(stream);
        });
        let (_directory, store) = store();
        let mut expiring = session("A");
        expiring.environment = Environment {
            issuer: format!("{origin}/realms/moi"),
            authorization_endpoint: format!("{origin}/api/v1/uc/oauth2/authorize"),
            token_endpoint: format!("{origin}/realms/moi/protocol/openid-connect/token"),
            revocation_endpoint: format!("{origin}/realms/moi/protocol/openid-connect/revoke"),
            jwks_uri: format!("{origin}/realms/moi/protocol/openid-connect/certs"),
            ..expiring.environment
        };
        expiring.expires_at = unix_now().unwrap();
        store.publish(expiring).unwrap();
        let error = store.credential("astra", None).await.unwrap_err();
        assert_eq!(error, CredentialFailure::RotationUnconfirmed);
        assert_eq!(error.access_miss(), AccessMiss::ReauthenticationRequired);
        request.await.unwrap();
        assert!(store.current().unwrap().refresh_pending);
    }

    #[tokio::test]
    async fn rejected_rotation_preserves_intent_and_is_not_replayed() {
        for status in [
            "400 Bad Request",
            "502 Bad Gateway",
            "503 Service Unavailable",
        ] {
            assert_rejected_rotation_is_not_replayed(status).await;
        }
    }

    async fn assert_rejected_rotation_is_not_replayed(status: &'static str) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let request = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut input = [0; 8192];
            let len = stream.read(&mut input).await.unwrap();
            assert!(
                String::from_utf8_lossy(&input[..len])
                    .starts_with("POST /realms/moi/protocol/openid-connect/token ")
            );
            stream.write_all(format!("HTTP/1.1 {status}\r\nX-Request-ID: test-request-1\r\nContent-Length: 25\r\nConnection: close\r\n\r\n{{\"error\":\"invalid_grant\"}}").as_bytes()).await.unwrap();
        });
        let (_directory, store) = store();
        let mut expiring = session("A");
        expiring.environment = Environment {
            issuer: format!("{origin}/realms/moi"),
            authorization_endpoint: format!("{origin}/api/v1/uc/oauth2/authorize"),
            token_endpoint: format!("{origin}/realms/moi/protocol/openid-connect/token"),
            revocation_endpoint: format!("{origin}/realms/moi/protocol/openid-connect/revoke"),
            jwks_uri: format!("{origin}/realms/moi/protocol/openid-connect/certs"),
            ..expiring.environment
        };
        expiring.expires_at = unix_now().unwrap();
        store.publish(expiring).unwrap();
        assert_eq!(
            store.credential("moi", None).await.unwrap_err(),
            CredentialFailure::RotationRejected
        );
        request.await.unwrap();
        let log = wait_for_diagnostics(&store, &[("http", "http_rejected")]).await;
        let rejected: serde_json::Value = log
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .find(|event: &serde_json::Value| event["reason"] == "http_rejected")
            .unwrap();
        assert_eq!(rejected["http_status"], status[..3].parse::<u16>().unwrap());
        assert_eq!(rejected["request_id"], "test-request-1");
        assert!(
            !log.contains("invalid_grant"),
            "response bodies are not diagnostics"
        );
        assert!(!log.contains("synthetic-refresh"));
        assert!(!log.contains("synthetic-access"));
        assert!(store.current().unwrap().refresh_pending);
        // No server remains; this must fail from persisted intent, not retry.
        assert!(
            store
                .credential("moi", None)
                .await
                .unwrap_err()
                .to_string()
                .contains("interrupted")
        );
    }
}

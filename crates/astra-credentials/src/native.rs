//! Versioned MOI sessions. This is separate from legacy password/Memoria
//! profiles because rotation intent, account CAS and logout tombstones are
//! part of this protocol, not optional legacy profile attributes.
use fs2::FileExt;
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
        let key = state
            .active
            .ok_or("MOI authentication is not configured; run astra login")?;
        state
            .sessions
            .get(&key)
            .and_then(Clone::clone)
            .ok_or_else(|| "MOI session is logged out; run astra login".into())
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
        mut session: NativeSession,
    ) -> Result<(NativeSession, Option<NativeSession>), String> {
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
        self.transaction(|state| {
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
        })
    }

    pub fn logout(&self) -> Result<Option<NativeSession>, String> {
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
    ) -> Result<(), String> {
        if workspace
            .is_some_and(|v| v.is_empty() || v.len() > 128 || v.chars().any(char::is_whitespace))
        {
            return Err("invalid workspace identity".into());
        }
        self.update(expected, |session| {
            session.workspace_id = workspace.map(str::to_owned);
            session.role_id = None;
            Ok(())
        })
    }

    fn update(
        &self,
        expected: &NativeSession,
        f: impl FnOnce(&mut NativeSession) -> Result<(), String>,
    ) -> Result<(), String> {
        self.transaction(|state| {
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

    fn transaction<R>(&self, f: impl FnOnce(&mut State) -> Result<R, String>) -> Result<R, String> {
        self.prepare_dir()?;
        let lock = private_open(&self.root.join("auth.lock"), true)?;
        FileExt::lock_exclusive(&lock).map_err(|_| "cannot lock MOI credentials")?;
        let mut state = self.read_locked()?;
        let result = f(&mut state)?;
        let body = serde_json::to_vec(&state).map_err(|_| "cannot encode MOI credentials")?;
        let tmp_path = self.root.join(format!("auth-{}.tmp", uuid::Uuid::new_v4()));
        let write = || -> Result<(), String> {
            let mut tmp = private_create(&tmp_path)?;
            tmp.write_all(&body)
                .map_err(|_| "cannot write MOI credentials")?;
            tmp.sync_all().map_err(|_| "cannot sync MOI credentials")?;
            fs::rename(&tmp_path, self.root.join("auth.json"))
                .map_err(|_| "cannot publish MOI credentials")?;
            File::open(&self.root)
                .and_then(|f| f.sync_all())
                .map_err(|_| "cannot sync MOI auth directory")?;
            Ok(())
        };
        if let Err(error) = write() {
            let _ = fs::remove_file(&tmp_path);
            return Err(error);
        }
        Ok(result)
    }
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

    pub async fn credential(
        &self,
        target: &str,
        expected_generation: Option<&str>,
    ) -> Result<Credential, String> {
        let frozen = self.blocking(|store| store.current()).await?;
        frozen.environment.validate()?;
        if expected_generation.is_some_and(|v| v != frozen.generation) {
            return Err("MOI account changed during operation".into());
        }
        if target != "moi" && target != "astra" {
            return Err("unknown native credential target".into());
        }
        // A separate per-environment lock serializes rotation without blocking
        // logout/account changes on the short global state transaction.
        // Fresh credentials need only a shared state read, not the rotation
        // lock. Pending intent still has to wait for its in-flight owner.
        let rotation = if frozen.expires_at <= unix_now()? + 60 || frozen.refresh_pending {
            let key = frozen.environment.key();
            Some(
                self.blocking(move |store| {
                    let rotation =
                        private_open(&store.root.join(format!("refresh-{key}.lock")), true)?;
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
                    loop {
                        match FileExt::try_lock_exclusive(&rotation) {
                            Ok(()) => return Ok(std::sync::Arc::new(rotation)),
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                if std::time::Instant::now() >= deadline {
                                    return Err("MOI credential refresh is busy".into());
                                }
                                std::thread::sleep(std::time::Duration::from_millis(25));
                            }
                            Err(_) => return Err("cannot lock native credential refresh".into()),
                        }
                    }
                })
                .await?,
            )
        } else {
            None
        };
        let mut current = if rotation.is_some() {
            self.blocking(|store| store.current()).await?
        } else {
            frozen.clone()
        };
        if current.generation != frozen.generation {
            return Err("MOI account changed during operation".into());
        }
        if current.refresh_pending {
            return Err("previous token rotation was interrupted; run astra login".into());
        }
        let now = unix_now()?;
        if let Some(rotation) = rotation.filter(|_| current.expires_at <= now + 60) {
            let client = http_client()?;
            let request = client
                .post(&current.environment.token_endpoint)
                .form(&[
                    ("client_id", "astra-cli"),
                    ("grant_type", "refresh_token"),
                    ("refresh_token", &current.refresh_token),
                ])
                .build()
                .map_err(|_| "cannot build token rotation request")?;
            let expected = current.clone();
            let write_guard = rotation.clone();
            self.blocking(move |store| {
                // spawn_blocking outlives cancellation of its awaiting task.
                // Retain rotation ownership until the durable write finishes.
                let _write_guard = write_guard;
                store.update(&expected, |s| {
                    s.refresh_pending = true;
                    Ok(())
                })
            })
            .await?;
            // After sending, any failure may hide a successful rotation. Keep
            // the intent; never automatically replay the previous refresh token.
            let response = match client.execute(request).await {
                Ok(response) => response,
                Err(error) if error.is_connect() => {
                    // No HTTP request reached the issuer. The rotation lock is
                    // still held and update checks the login generation, so a
                    // concurrent logout/account switch cannot be resurrected.
                    let expected = current.clone();
                    let write_guard = rotation.clone();
                    self.blocking(move |store| {
                        let _write_guard = write_guard;
                        store.update(&expected, |s| {
                            s.refresh_pending = false;
                            Ok(())
                        })
                    })
                    .await?;
                    return Err("cannot connect to token service; retry when available".into());
                }
                Err(_) => {
                    return Err("token rotation was not confirmed; run astra login".into());
                }
            };
            if !response.status().is_success() {
                // Even a 5xx can be generated by a proxy or after the issuer
                // committed rotation. HTTP status is not proof of non-consumption.
                return Err("token rotation rejected; run astra login".into());
            }
            let token: TokenResponse = bounded_json(response).await?;
            if token.access_token.is_empty()
                || token.refresh_token.is_empty()
                || token.expires_in <= 0
                || token.expires_in > 86400
                || !token.token_type.eq_ignore_ascii_case("bearer")
            {
                return Err("invalid token rotation response; run astra login".into());
            }
            if let Err(error) = verify_rotated_identity(&current, &token.access_token).await {
                let _ = revoke(&current.environment, &token.refresh_token).await;
                return Err(error);
            }
            let expected = current.clone();
            let access_token = token.access_token.clone();
            let refresh_token = token.refresh_token.clone();
            let expires_in = token.expires_in;
            let write_guard = rotation.clone();
            if let Err(error) = self
                .blocking(move |store| {
                    let _write_guard = write_guard;
                    store.update(&expected, |s| {
                        s.access_token = access_token;
                        s.refresh_token = refresh_token;
                        s.expires_at = unix_now()? + expires_in;
                        s.refresh_pending = false;
                        Ok(())
                    })
                })
                .await
            {
                let _ = revoke(&current.environment, &token.refresh_token).await;
                return Err(error);
            }
            current = self.blocking(|store| store.current()).await?;
        }
        if current.generation != frozen.generation {
            return Err("MOI account changed during operation".into());
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

async fn verify_rotated_identity(current: &NativeSession, token: &str) -> Result<(), String> {
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
        .await
        .map_err(|_| "UC signing keys unavailable")?;
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
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

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
                .contains("identity changed")
        );
        assert_eq!(fixture.revocations.load(Ordering::SeqCst), 1);
        assert!(store.current().unwrap().refresh_pending);
        assert_eq!(store.current().unwrap().subject, "A");
    }

    fn session(subject: &str) -> NativeSession {
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

    fn store() -> (tempfile::TempDir, NativeStore) {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let store = NativeStore::with_directory(directory.path().to_path_buf());
        (directory, store)
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
                .update(&first, |s| {
                    s.access_token = "stale".into();
                    Ok(())
                })
                .is_err()
        );
        assert_eq!(store.logout().unwrap().unwrap().subject, "B");
        assert!(store.configured().unwrap());
        assert!(store.current().is_err());
        assert_eq!(
            store.selected_environment().unwrap(),
            Some(first.environment)
        );
        assert!(store.logout().unwrap().is_none());
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
            .update(&current, |s| {
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
                    .contains("cannot connect")
            );
            assert!(!store.current().unwrap().refresh_pending);
            assert_eq!(store.current().unwrap().refresh_token, "synthetic-refresh");
        }
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
            stream.write_all(format!("HTTP/1.1 {status}\r\nContent-Length: 25\r\nConnection: close\r\n\r\n{{\"error\":\"invalid_grant\"}}").as_bytes()).await.unwrap();
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
        assert!(
            store
                .credential("moi", None)
                .await
                .unwrap_err()
                .contains("rotation rejected")
        );
        request.await.unwrap();
        assert!(store.current().unwrap().refresh_pending);
        // No server remains; this must fail from persisted intent, not retry.
        assert!(
            store
                .credential("moi", None)
                .await
                .unwrap_err()
                .contains("interrupted")
        );
    }
}

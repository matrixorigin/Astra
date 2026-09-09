//! Private local attachment transport. This owns process/credential lifetime,
//! never work, admission, retries, or provider requests. No TCP listener and no
//! secret-bearing type is exposed through Server DTOs or diagnostic formatting.

use std::fs::File;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use astra_credentials::{LocalCredentialRef, LocalModelScope, ResolvedLocalCredential};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, OnceCell};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::inference_host::{InferenceHost, InferenceHostError};
use crate::inference_journal::{
    atomic_write, ensure_private_directory, open_private, read_private,
};

// The private IPC protocol is introduced with the managed host itself. Keep a
// single current version; pre-release protocol revisions are not a supported
// migration boundary.
const PROTOCOL: u32 = 1;
const FRAME_BYTES: usize = 64 * 1024;
const LEASE_TIMEOUT: Duration = Duration::from_secs(30);
const REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Stable installation coordinates, scoped by verified deployment/account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installation {
    pub runner_id: String,
    socket: PathBuf,
    root: PathBuf,
    scope: String,
}

impl Installation {
    pub fn socket_path(&self) -> &std::path::Path {
        &self.socket
    }

    pub fn open(scope: &LocalModelScope) -> Result<Self, InferenceHostError> {
        let root = scope.root().join("managed-host");
        let runtime =
            PathBuf::from("/tmp").join(format!("astra-inference-{}", unsafe { libc::geteuid() }));
        ensure_private_directory(&runtime)?;
        Self::open_at(
            scope.identity(),
            root,
            runtime.join(&scope.identity()[..24]),
        )
    }

    fn open_at(scope: &str, root: PathBuf, runtime: PathBuf) -> Result<Self, InferenceHostError> {
        ensure_private_directory(&root)?;
        let lock = open_private(&root.join("identity.lock"), true)?;
        fs2::FileExt::lock_exclusive(&lock).map_err(|_| InferenceHostError::JournalIo)?;
        let path = root.join("runner-id");
        let runner_id = if path
            .try_exists()
            .map_err(|_| InferenceHostError::JournalIo)?
        {
            let bytes = read_private(&path, 128)?;
            let value = String::from_utf8(bytes).map_err(|_| InferenceHostError::Corrupt)?;
            let suffix = value
                .strip_prefix("edge-local-")
                .ok_or(InferenceHostError::Corrupt)?;
            uuid::Uuid::parse_str(suffix).map_err(|_| InferenceHostError::Corrupt)?;
            value
        } else {
            let value = format!("edge-local-{}", uuid::Uuid::new_v4());
            atomic_write(&path, value.as_bytes())?;
            value
        };
        // macOS sockaddr_un is short. Do not put a socket below the potentially
        // long credentials path. Every ancestor after /tmp is owner protected,
        // and the full account/deployment identity is checked in the handshake.
        ensure_private_directory(&runtime)?;
        Ok(Self {
            runner_id,
            socket: runtime.join("host.sock"),
            root,
            scope: scope.to_owned(),
        })
    }

    fn verify_peer(&self, stream: &UnixStream) -> Result<(), InferenceHostError> {
        if stream
            .peer_cred()
            .map_err(|_| InferenceHostError::UnsafeStorage)?
            .uid()
            != unsafe { libc::geteuid() }
        {
            return Err(InferenceHostError::OwnerMismatch);
        }
        Ok(())
    }

    pub fn status_hint(&self) -> Option<String> {
        let bytes = read_private(&self.root.join("status.json"), 1024).ok()?;
        let status: HostStatus = serde_json::from_slice(&bytes).ok()?;
        Some(status.message().to_owned())
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostStatus {
    Connecting,
    Ready,
    AuthenticationFailed,
    LocalStateUnavailable,
    ConnectionUnavailable,
    Idle,
}

impl HostStatus {
    fn message(self) -> &'static str {
        match self {
            Self::Connecting => "Connecting to Astra Server; check Server connectivity",
            Self::Ready => {
                "Local host authenticated; inspect model configuration and attachment availability"
            }
            Self::AuthenticationFailed => {
                "Astra authentication failed; sign in to the selected profile again"
            }
            Self::LocalStateUnavailable => {
                "Local model state or network configuration needs repair; existing journals were preserved"
            }
            Self::ConnectionUnavailable => {
                "Astra Server connection is unavailable; check connectivity and proxy settings"
            }
            Self::Idle => "Local model host is idle; reopen Astra to attach",
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Hello {
    version: u32,
    scope: String,
}

fn network_environment() -> Vec<(String, String)> {
    astra_core::net::RUNNER_NETWORK_ENV_VARS
        .iter()
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .map(|value| ((*name).to_owned(), value))
        })
        .collect()
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
enum Handshake {
    Ready { attachment: Attachment },
    NetworkPolicyMismatch,
    ProtocolMismatch,
}

// Only the private IPC decoder may deserialize this input. It intentionally
// does not implement Debug or Serialize; diagnostics never print a frame.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialInput {
    name: String,
    revision: u64,
    value: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Refresh {
    credentials: Option<Vec<CredentialInput>>,
    revision: Option<u64>,
    #[serde(default)]
    more: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attachment {
    pub runner_id: String,
    pub journal_id: String,
    pub lease_id: String,
    scope: String,
    version: u32,
}

impl Attachment {
    pub fn belongs_to(&self, scope: &LocalModelScope) -> bool {
        self.scope == scope.identity()
    }
}

struct Lifecycle {
    clients: usize,
    idle_since: Instant,
    drain_since: Option<Instant>,
    closing: bool,
}

pub struct ManagedHost {
    installation: Installation,
    host: OnceCell<Arc<InferenceHost>>,
    lifecycle: Mutex<Lifecycle>,
    shutdown: CancellationToken,
    _lock: File,
}

impl std::fmt::Debug for ManagedHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedHost").finish_non_exhaustive()
    }
}

impl ManagedHost {
    pub fn bind(installation: Installation) -> Result<Arc<Self>, InferenceHostError> {
        let lock = open_private(&installation.root.join("process.lock"), true)?;
        fs2::FileExt::try_lock_exclusive(&lock).map_err(|_| InferenceHostError::AlreadyRunning)?;
        match std::fs::symlink_metadata(&installation.socket) {
            Ok(metadata) => {
                if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() }
                {
                    return Err(InferenceHostError::UnsafeStorage);
                }
                // Only the exclusive process-lock owner can remove a stale
                // socket. Never delete a lock or journal to force takeover.
                std::fs::remove_file(&installation.socket)
                    .map_err(|_| InferenceHostError::JournalIo)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(InferenceHostError::JournalIo),
        }
        let listener =
            UnixListener::bind(&installation.socket).map_err(|_| InferenceHostError::JournalIo)?;
        std::fs::set_permissions(&installation.socket, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| InferenceHostError::UnsafeStorage)?;
        let control = Arc::new(Self {
            installation,
            host: OnceCell::new(),
            lifecycle: Mutex::new(Lifecycle {
                clients: 0,
                idle_since: Instant::now(),
                drain_since: None,
                closing: false,
            }),
            shutdown: CancellationToken::new(),
            _lock: lock,
        });
        control.report_status(HostStatus::Connecting);
        let owner = control.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = owner.shutdown.cancelled() => break,
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        let mut state = owner.lifecycle.lock().await;
                        if state.closing || state.clients >= 32 || owner.installation.verify_peer(&stream).is_err() { continue; }
                        state.clients += 1;
                        drop(state);
                        let owner = owner.clone();
                        tokio::spawn(async move {
                            let _ = owner.serve(stream).await;
                            let mut state = owner.lifecycle.lock().await;
                            state.clients -= 1;
                            if state.clients == 0 { state.idle_since = Instant::now(); state.drain_since = None; }
                        });
                    }
                }
            }
        });
        let owner = control.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let mut state = owner.lifecycle.lock().await;
                if state.clients != 0 || state.idle_since.elapsed() < IDLE_TIMEOUT {
                    continue;
                }
                if let Some(host) = owner.host.get() {
                    if host.active_count().await != 0 {
                        state.drain_since = None;
                        continue;
                    }
                    let drained = *state.drain_since.get_or_insert_with(Instant::now);
                    if drained.elapsed() < Duration::from_secs(5)
                        && !host.pending(1).await.unwrap_or_default().is_empty()
                    {
                        continue;
                    }
                }
                // Existing WS worker has had the idle grace to flush terminals;
                // anything not ACKed remains in the same journal for restart.
                state.closing = true;
                owner.report_status(HostStatus::Idle);
                owner.shutdown.cancel();
                break;
            }
        });
        Ok(control)
    }

    pub async fn install(&self, host: Arc<InferenceHost>) -> Result<(), InferenceHostError> {
        host.enable_managed().await;
        if let Some(existing) = self.host.get() {
            return if Arc::ptr_eq(existing, &host) {
                Ok(())
            } else {
                Err(InferenceHostError::OwnerMismatch)
            };
        }
        self.host
            .set(host)
            .map_err(|_| InferenceHostError::OwnerMismatch)?;
        self.report_status(HostStatus::Ready);
        Ok(())
    }

    pub fn report_status(&self, status: HostStatus) {
        // One bounded owner-protected diagnostic projection, not a raw log or
        // execution state machine. Errors/URLs/tokens never enter this file.
        if let Ok(bytes) = serde_json::to_vec(&status) {
            let _ = atomic_write(&self.installation.root.join("status.json"), &bytes);
        }
    }

    pub async fn shutdown_requested(&self) {
        self.shutdown.cancelled().await;
    }

    async fn serve(&self, mut stream: UnixStream) -> Result<(), InferenceHostError> {
        let hello: Hello = read_frame(&mut stream).await?;
        if hello.scope != self.installation.scope {
            return Err(InferenceHostError::OwnerMismatch);
        }
        if hello.version != PROTOCOL {
            write_frame(&mut stream, &Handshake::ProtocolMismatch).await?;
            return Err(InferenceHostError::LocalProtocolMismatch);
        }
        let host = tokio::time::timeout(LEASE_TIMEOUT, async {
            loop {
                if let Some(host) = self.host.get() {
                    break host.clone();
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .map_err(|_| InferenceHostError::BindingUnavailable)?;
        let lease_id = uuid::Uuid::new_v4().to_string();
        let result = async {
            write_frame(
                &mut stream,
                &Handshake::Ready {
                    attachment: Attachment {
                        runner_id: self.installation.runner_id.clone(),
                        journal_id: host.journal_id().as_str().to_owned(),
                        lease_id: lease_id.clone(),
                        scope: self.installation.scope.clone(),
                        version: PROTOCOL,
                    },
                },
            )
            .await?;
            // Network policy can contain proxy credentials. Receive it only
            // after the client verified the full owner/installation response.
            let network: Vec<(String, String)> = read_frame(&mut stream).await?;
            if network != network_environment() {
                write_frame(&mut stream, &false).await?;
                return Err(InferenceHostError::NetworkPolicyMismatch);
            }
            host.attach_client(lease_id.clone()).await?;
            write_frame(&mut stream, &true).await?;
            let mut pending_credentials = Vec::new();
            let mut pending_revision = None;
            loop {
                let refresh: Refresh = read_frame(&mut stream).await?;
                let Some(inputs) = refresh.credentials else {
                    if refresh.more || pending_revision.is_some() || refresh.revision.is_some() {
                        return Err(InferenceHostError::InvalidRequest);
                    }
                    write_frame(&mut stream, &PROTOCOL).await?;
                    continue;
                };
                let revision = refresh.revision.ok_or(InferenceHostError::InvalidRequest)?;
                if pending_revision.is_some_and(|previous| previous != revision) {
                    return Err(InferenceHostError::InvalidRequest);
                }
                pending_revision = Some(revision);
                if inputs.len() + pending_credentials.len() > 256 {
                    return Err(InferenceHostError::Capacity);
                }
                for input in inputs {
                    if input.name.len() > 256 || input.value.len() > 8192 {
                        return Err(InferenceHostError::TooLarge);
                    }
                    let reference = LocalCredentialRef::Environment {
                        name: "LOCAL_IPC_INPUT".into(),
                    };
                    let credential = ResolvedLocalCredential::from_environment(&reference, |_| {
                        Some(input.value.clone())
                    })
                    .map_err(|_| InferenceHostError::CredentialUnavailable)?
                    .ok_or(InferenceHostError::CredentialUnavailable)?;
                    pending_credentials.push((input.name, input.revision, credential));
                }
                if refresh.more {
                    continue;
                }
                let applied = host
                    .refresh_client(
                        &lease_id,
                        revision,
                        std::mem::take(&mut pending_credentials),
                    )
                    .await;
                pending_revision = None;
                let ack = match applied {
                    Ok(()) => PROTOCOL,
                    // The candidate was not applied. Keep the connection and
                    // ask the terminal for its new complete snapshot.
                    Err(InferenceHostError::BindingUnavailable) => 0,
                    Err(error) => return Err(error),
                };
                write_frame(&mut stream, &ack).await?;
            }
        }
        .await;
        host.detach_client(&lease_id).await;
        result
    }
}

/// Observation of one local connection, not authority and not serializable.
/// Cloning it cannot keep the client or its credential lease alive.
#[derive(Clone, Debug, Default)]
pub struct ConnectionLiveness(std::sync::Weak<()>);

impl ConnectionLiveness {
    pub fn is_alive(&self) -> bool {
        self.0.strong_count() != 0
    }
}

impl PartialEq for ConnectionLiveness {
    fn eq(&self, other: &Self) -> bool {
        self.0.ptr_eq(&other.0)
    }
}

impl Eq for ConnectionLiveness {}

pub struct ManagedClient {
    pub attachment: Attachment,
    liveness: ConnectionLiveness,
    task: tokio::task::JoinHandle<()>,
}

impl ManagedClient {
    pub async fn connect(
        installation: &Installation,
        scope: LocalModelScope,
        deployment: &str,
        profile: Option<&str>,
    ) -> Result<Self, InferenceHostError> {
        // Validate caller coordinates before opening a socket or resolving any
        // environment credential. An Installation from another account must
        // never be usable with this account's local configuration.
        if installation.scope != scope.identity() {
            return Err(InferenceHostError::OwnerMismatch);
        }
        let mut stream = UnixStream::connect(&installation.socket)
            .await
            .map_err(|_| InferenceHostError::BindingUnavailable)?;
        installation.verify_peer(&stream)?;
        write_frame(
            &mut stream,
            &Hello {
                version: PROTOCOL,
                scope: installation.scope.clone(),
            },
        )
        .await?;
        let handshake = read_frame(&mut stream).await?;
        let attachment = match handshake {
            Handshake::Ready { attachment } => attachment,
            Handshake::NetworkPolicyMismatch => {
                return Err(InferenceHostError::NetworkPolicyMismatch);
            }
            Handshake::ProtocolMismatch => {
                return Err(InferenceHostError::LocalProtocolMismatch);
            }
        };
        if attachment.version != PROTOCOL {
            return Err(InferenceHostError::LocalProtocolMismatch);
        }
        if attachment.scope != installation.scope
            || attachment.runner_id != installation.runner_id
            || uuid::Uuid::parse_str(&attachment.lease_id).is_err()
            || astra_turn_types::runner_inference::RunnerInferenceId::new(
                attachment.journal_id.clone(),
            )
            .is_err()
        {
            return Err(InferenceHostError::OwnerMismatch);
        }
        if !LocalModelScope::for_profile(deployment, profile)
            .is_ok_and(|current| current.identity() == scope.identity())
        {
            return Err(InferenceHostError::OwnerMismatch);
        }
        write_frame(&mut stream, &network_environment()).await?;
        if !read_frame::<bool>(&mut stream).await? {
            return Err(InferenceHostError::NetworkPolicyMismatch);
        }
        let mut revision = None;
        refresh(&mut stream, &scope, &mut revision).await?;
        let deployment = deployment.to_owned();
        let profile = profile.map(str::to_owned);
        let alive = Arc::new(());
        let liveness = ConnectionLiveness(Arc::downgrade(&alive));
        let task = tokio::spawn(async move {
            // Drop also covers task cancellation, including cancellation
            // before its first poll. Observers never retain this strong owner.
            let _alive = alive;
            loop {
                tokio::time::sleep(REFRESH_INTERVAL).await;
                // Logout or account/profile switching removes this view's
                // credential lease. A retained account_id alone is not login.
                let current = LocalModelScope::for_profile(&deployment, profile.as_deref());
                if !current.is_ok_and(|current| current.identity() == scope.identity()) {
                    break;
                }
                if refresh(&mut stream, &scope, &mut revision).await.is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            attachment,
            liveness,
            task,
        })
    }

    pub fn is_alive(&self) -> bool {
        self.liveness.is_alive() && !self.task.is_finished()
    }

    pub fn liveness(&self) -> ConnectionLiveness {
        self.liveness.clone()
    }
}

impl Drop for ManagedClient {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn refresh(
    stream: &mut UnixStream,
    scope: &LocalModelScope,
    previous_revision: &mut Option<u64>,
) -> Result<(), InferenceHostError> {
    let store = scope.models();
    let config = tokio::task::spawn_blocking(move || store.load())
        .await
        .map_err(|_| InferenceHostError::JournalIo)?
        .map_err(|_| InferenceHostError::BindingUnavailable)?;
    let mut credentials = Vec::new();
    let revision = config.revision;
    if *previous_revision == Some(revision) {
        write_frame(stream, &serde_json::json!({ "credentials": null })).await?;
        let ack: u32 = read_frame(stream).await?;
        return if ack == PROTOCOL {
            Ok(())
        } else {
            Err(InferenceHostError::WrongIncarnation)
        };
    }
    for (name, model) in config.models {
        if !matches!(model.credential, LocalCredentialRef::Environment { .. }) {
            continue;
        }
        if let Ok(Some(value)) =
            ResolvedLocalCredential::from_environment(&model.credential, |name| {
                std::env::var(name).ok()
            })
        {
            credentials.push(serde_json::json!({ "name": name, "revision": model.binding_revision, "value": value.expose_to_local_transport() }));
        }
    }
    // This is the only secret-bearing encoder, used exclusively on the verified
    // same-UID private socket. Never return its value or serialization errors.
    // One credential fits even at JSON's maximum escaping expansion. Stage
    // bounded chunks in the host and activate only the complete snapshot.
    if credentials.len() > 256 {
        return Err(InferenceHostError::Capacity);
    }
    for credential in credentials {
        write_frame(
            stream,
            &serde_json::json!({ "credentials": [credential], "revision": revision, "more": true }),
        )
        .await?;
    }
    write_frame(
        stream,
        &serde_json::json!({ "credentials": [], "revision": revision, "more": false }),
    )
    .await?;
    let ack: u32 = read_frame(stream).await?;
    if ack == 0 {
        *previous_revision = None;
        return Ok(());
    }
    if ack != PROTOCOL {
        return Err(InferenceHostError::WrongIncarnation);
    }
    *previous_revision = Some(revision);
    Ok(())
}

async fn read_frame<T: serde::de::DeserializeOwned>(
    stream: &mut UnixStream,
) -> Result<T, InferenceHostError> {
    tokio::time::timeout(LEASE_TIMEOUT, async {
        let size = stream
            .read_u32()
            .await
            .map_err(|_| InferenceHostError::JournalIo)? as usize;
        if size == 0 || size > FRAME_BYTES {
            return Err(InferenceHostError::TooLarge);
        }
        let mut bytes = vec![0; size];
        stream
            .read_exact(&mut bytes)
            .await
            .map_err(|_| InferenceHostError::JournalIo)?;
        serde_json::from_slice(&bytes).map_err(|_| InferenceHostError::InvalidRequest)
    })
    .await
    .map_err(|_| InferenceHostError::BindingUnavailable)?
}

async fn write_frame<T: Serialize>(
    stream: &mut UnixStream,
    value: &T,
) -> Result<(), InferenceHostError> {
    let bytes = serde_json::to_vec(value).map_err(|_| InferenceHostError::InvalidRequest)?;
    if bytes.len() > FRAME_BYTES {
        return Err(InferenceHostError::TooLarge);
    }
    tokio::time::timeout(LEASE_TIMEOUT, async {
        stream
            .write_u32(bytes.len() as u32)
            .await
            .map_err(|_| InferenceHostError::JournalIo)?;
        stream
            .write_all(&bytes)
            .await
            .map_err(|_| InferenceHostError::JournalIo)
    })
    .await
    .map_err(|_| InferenceHostError::BindingUnavailable)?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inference_host::InferenceOwner;
    use astra_inference_adapter::transport::ProviderTransport;
    use astra_turn_types::runner_inference::RunnerInferenceId;

    #[tokio::test]
    async fn connection_observers_do_not_retain_an_aborted_client_lease() {
        let alive = Arc::new(());
        let observer = ConnectionLiveness(Arc::downgrade(&alive));
        let another_window_snapshot = observer.clone();
        let task = tokio::spawn(async move {
            let _alive = alive;
            std::future::pending::<()>().await;
        });
        assert!(observer.is_alive());
        task.abort();
        let _ = task.await;
        assert!(!observer.is_alive());
        assert!(!another_window_snapshot.is_alive());
    }

    fn installation(directory: &std::path::Path) -> Installation {
        Installation::open_at(
            "test-owner-scope",
            directory.join("identity"),
            directory.join("runtime"),
        )
        .unwrap()
    }

    async fn host(directory: &std::path::Path, installation: &Installation) -> Arc<InferenceHost> {
        InferenceHost::open(
            directory.join("journal"),
            InferenceOwner {
                deployment_identity: "https://fixture.invalid".into(),
                user_id: "fixture-owner".into(),
                runner_id: RunnerInferenceId::new(installation.runner_id.clone()).unwrap(),
            },
            directory.join("models.json"),
            directory.join("secrets"),
            ProviderTransport::build(reqwest::Client::builder().no_proxy()).unwrap(),
        )
        .await
        .unwrap()
    }

    async fn attach(installation: &Installation) -> (UnixStream, Attachment) {
        let mut stream = UnixStream::connect(&installation.socket).await.unwrap();
        write_frame(
            &mut stream,
            &Hello {
                version: PROTOCOL,
                scope: installation.scope.clone(),
            },
        )
        .await
        .unwrap();
        let Handshake::Ready { attachment } = read_frame(&mut stream).await.unwrap() else {
            panic!("expected ready host")
        };
        write_frame(&mut stream, &network_environment())
            .await
            .unwrap();
        assert!(read_frame::<bool>(&mut stream).await.unwrap());
        write_frame(&mut stream, &serde_json::json!({ "credentials": null }))
            .await
            .unwrap();
        assert_eq!(read_frame::<u32>(&mut stream).await.unwrap(), PROTOCOL);
        (stream, attachment)
    }

    #[tokio::test]
    async fn managed_ipc_rejects_mixed_account_coordinates_before_connecting() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let installation = installation(directory.path());
        let foreign = LocalModelScope::for_owner("https://fixture.invalid", "foreign").unwrap();
        let result =
            ManagedClient::connect(&installation, foreign, "https://fixture.invalid", None).await;
        assert!(matches!(result, Err(InferenceHostError::OwnerMismatch)));
        assert!(!installation.socket.exists());
    }

    #[tokio::test]
    async fn stale_chunked_snapshot_cannot_restore_a_recreated_models_old_credential() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let installation = installation(directory.path());
        let control = ManagedHost::bind(installation.clone()).unwrap();
        let host = host(directory.path(), &installation).await;
        let store = astra_credentials::LocalModelConfigStore::with_path(
            directory.path().join("models.json"),
        );
        let mut config = astra_credentials::LocalModelConfig::default();
        config.models.insert(
            "work".into(),
            astra_credentials::LocalModelDefinition {
                protocol: astra_credentials::LocalInferenceProtocol::OpenaiCompatible,
                base_url: "http://127.0.0.1:9".into(),
                model: "fixture".into(),
                binding_revision: 1,
                context_window: 1024,
                max_output_tokens: 64,
                credential: LocalCredentialRef::Environment {
                    name: "OLD_KEY".into(),
                },
                probe: astra_credentials::LocalModelProbeState::default(),
            },
        );
        let original = store.replace(0, config).unwrap();
        control.install(host.clone()).await.unwrap();
        let (mut stream, _) = attach(&installation).await;
        let old_chunk = serde_json::json!({"revision": original.revision, "credentials": [{"name":"work", "revision":1, "value":"old-key"}], "more": true});
        write_frame(&mut stream, &old_chunk).await.unwrap();
        write_frame(
            &mut stream,
            &serde_json::json!({"revision": original.revision, "credentials": [], "more": false}),
        )
        .await
        .unwrap();
        assert_eq!(read_frame::<u32>(&mut stream).await.unwrap(), PROTOCOL);
        let old_identity = host.bindings().await.unwrap().remove(0).identity;
        let mut unrelated = original.clone();
        let mut other = original.models["work"].clone();
        other.credential = LocalCredentialRef::None;
        unrelated.models.insert("other".into(), other);
        let unrelated = store.replace(original.revision, unrelated).unwrap();
        assert!(
            host.bindings()
                .await
                .unwrap()
                .iter()
                .any(|binding| binding.identity == old_identity)
        );
        write_frame(&mut stream, &old_chunk).await.unwrap();
        let empty = store
            .replace(
                unrelated.revision,
                astra_credentials::LocalModelConfig::default(),
            )
            .unwrap();
        let mut recreated = original.clone();
        recreated.models.get_mut("work").unwrap().credential = LocalCredentialRef::Environment {
            name: "NEW_KEY".into(),
        };
        let recreated = store.replace(empty.revision, recreated).unwrap();
        assert!(
            recreated.models["work"].binding_revision > original.models["work"].binding_revision
        );
        assert!(host.bindings().await.unwrap().is_empty());
        write_frame(
            &mut stream,
            &serde_json::json!({"revision": original.revision, "credentials": [], "more": false}),
        )
        .await
        .unwrap();
        assert_eq!(read_frame::<u32>(&mut stream).await.unwrap(), 0);
        assert!(host.bindings().await.unwrap().is_empty());
        write_frame(&mut stream, &serde_json::json!({"revision": recreated.revision, "credentials": [{"name":"work", "revision":recreated.models["work"].binding_revision, "value":"new-key"}], "more":false})).await.unwrap();
        assert_eq!(read_frame::<u32>(&mut stream).await.unwrap(), PROTOCOL);
        let bindings = host.bindings().await.unwrap();
        assert_eq!(bindings.len(), 1);
        assert_eq!(
            bindings[0].identity.profile_revision.get(),
            recreated.models["work"].binding_revision
        );
    }

    #[tokio::test]
    async fn credential_snapshot_larger_than_a_frame_applies_only_when_complete() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let installation = installation(directory.path());
        let control = ManagedHost::bind(installation.clone()).unwrap();
        let host = host(directory.path(), &installation).await;
        let mut config = astra_credentials::LocalModelConfig::default();
        for index in 0..8 {
            config.models.insert(
                format!("model-{index}"),
                astra_credentials::LocalModelDefinition {
                    protocol: astra_credentials::LocalInferenceProtocol::OpenaiCompatible,
                    base_url: "http://127.0.0.1:9".into(),
                    model: "fixture".into(),
                    binding_revision: 1,
                    context_window: 1024,
                    max_output_tokens: 64,
                    credential: LocalCredentialRef::Environment {
                        name: "TEST_KEY".into(),
                    },
                    probe: astra_credentials::LocalModelProbeState::default(),
                },
            );
        }
        astra_credentials::LocalModelConfigStore::with_path(directory.path().join("models.json"))
            .replace(0, config)
            .unwrap();
        control.install(host.clone()).await.unwrap();
        let (mut stream, _) = attach(&installation).await;
        for index in 0..8 {
            write_frame(&mut stream, &serde_json::json!({
                "credentials": [{ "name": format!("model-{index}"), "revision": 1, "value": "x".repeat(8192) }],
                "revision": 1, "more": true,
            })).await.unwrap();
        }
        assert!(host.bindings().await.unwrap().is_empty());
        write_frame(
            &mut stream,
            &serde_json::json!({ "credentials": [], "revision": 1, "more": false }),
        )
        .await
        .unwrap();
        assert_eq!(read_frame::<u32>(&mut stream).await.unwrap(), PROTOCOL);
        assert_eq!(host.bindings().await.unwrap().len(), 8);
    }

    #[tokio::test]
    async fn managed_ipc_cold_start_has_one_owner_and_independent_live_leases() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let installation = installation(directory.path());
        assert_eq!(
            installation.runner_id,
            super::Installation::open_at(
                "test-owner-scope",
                directory.path().join("identity"),
                directory.path().join("runtime")
            )
            .unwrap()
            .runner_id
        );
        let control = ManagedHost::bind(installation.clone()).unwrap();
        assert_eq!(
            ManagedHost::bind(installation.clone()).unwrap_err(),
            InferenceHostError::AlreadyRunning
        );
        control
            .install(host(directory.path(), &installation).await)
            .await
            .unwrap();
        let (a, attachment_a) = attach(&installation).await;
        let (mut b, attachment_b) = attach(&installation).await;
        assert_eq!(attachment_a.runner_id, attachment_b.runner_id);
        assert_eq!(attachment_a.journal_id, attachment_b.journal_id);
        assert_ne!(attachment_a.lease_id, attachment_b.lease_id);
        drop(a);
        tokio::time::timeout(Duration::from_secs(2), async {
            while control.lifecycle.lock().await.clients != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        write_frame(
            &mut b,
            &serde_json::json!({ "credentials": [], "revision": 0 }),
        )
        .await
        .unwrap();
        assert_eq!(read_frame::<u32>(&mut b).await.unwrap(), PROTOCOL);
        assert!(!control.shutdown.is_cancelled());
        control.shutdown.cancel();
    }

    #[tokio::test]
    async fn managed_ipc_wrong_scope_and_oversized_frames_do_not_attach() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let installation = installation(directory.path());
        let control = ManagedHost::bind(installation.clone()).unwrap();
        control
            .install(host(directory.path(), &installation).await)
            .await
            .unwrap();
        let mut stream = UnixStream::connect(&installation.socket).await.unwrap();
        write_frame(
            &mut stream,
            &Hello {
                version: PROTOCOL,
                scope: "foreign-owner".into(),
            },
        )
        .await
        .unwrap();
        assert!(read_frame::<Attachment>(&mut stream).await.is_err());
        let mut stream = UnixStream::connect(&installation.socket).await.unwrap();
        stream.write_u32(FRAME_BYTES as u32 + 1).await.unwrap();
        assert!(read_frame::<Attachment>(&mut stream).await.is_err());
        assert!(
            control
                .host
                .get()
                .unwrap()
                .bindings()
                .await
                .unwrap()
                .is_empty()
        );
        control.shutdown.cancel();
    }

    #[tokio::test]
    async fn managed_ipc_network_policy_mismatch_is_explicit_without_private_value_echo() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let installation = installation(directory.path());
        let control = ManagedHost::bind(installation.clone()).unwrap();
        control
            .install(host(directory.path(), &installation).await)
            .await
            .unwrap();
        let mut stream = UnixStream::connect(&installation.socket).await.unwrap();
        let hello = Hello {
            version: PROTOCOL,
            scope: installation.scope.clone(),
        };
        write_frame(&mut stream, &hello).await.unwrap();
        let _: Handshake = read_frame(&mut stream).await.unwrap();
        write_frame(
            &mut stream,
            &vec![("HTTPS_PROXY", "private-network-canary")],
        )
        .await
        .unwrap();
        let reply: serde_json::Value = read_frame(&mut stream).await.unwrap();
        assert_eq!(reply, serde_json::json!(false));
        assert!(!reply.to_string().contains("private-network-canary"));
        control.shutdown.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn managed_ipc_last_client_expiry_shuts_down_without_waiting_for_server() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let control = ManagedHost::bind(installation(directory.path())).unwrap();
        // Even an unreachable Server cannot leave an orphaned managed daemon.
        tokio::time::timeout(
            IDLE_TIMEOUT + Duration::from_secs(2),
            control.shutdown_requested(),
        )
        .await
        .unwrap();
        assert!(control.shutdown.is_cancelled());
    }

    #[test]
    fn managed_identity_never_follows_a_replaced_symlink() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let first = installation(directory.path());
        let target = directory.path().join("outside");
        atomic_write(&target, b"unchanged").unwrap();
        let id_path = first.root.join("runner-id");
        std::fs::remove_file(&id_path).unwrap();
        std::os::unix::fs::symlink(&target, &id_path).unwrap();
        assert!(
            Installation::open_at(
                "test-owner-scope",
                first.root,
                directory.path().join("runtime")
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"unchanged");
    }
}

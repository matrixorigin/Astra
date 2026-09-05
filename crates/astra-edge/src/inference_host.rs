//! Local provider execution under one durable installation owner. Configuration
//! and credentials stay local; only public bindings and bounded response custody
//! leave this boundary.

use std::collections::HashMap;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use astra_credentials::{
    LocalCredentialRef, LocalModelConfig, LocalModelConfigLease, LocalModelConfigStore,
    LocalSecretStore, ResolvedLocalCredential,
};
use astra_inference_adapter::transport::{
    DeliveryEvidence, ExecutionLimits, ExecutionStatus, ExecutionTerminal, ProviderEvent,
    ProviderTransport, ResponseMode, provider_headers,
};
use astra_inference_adapter::{ExactProviderRequest, ProviderProtocol, RequestIdentity};
use astra_turn_types::runner_inference::*;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, Notify, mpsc};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub use crate::inference_journal::{InferenceHostError, RetainedTerminal};
use crate::inference_journal::{InferenceJournal, JournalRecord, RecordState};

const MAX_ACTIVE: usize = 4;
const RESPONSE_OVERHEAD_RESERVE: usize = 64 * 1024;

#[cfg(all(test, unix))]
#[path = "inference_tests.rs"]
mod tests;

pub struct InferenceOwner {
    pub deployment_identity: String,
    pub user_id: String,
    pub runner_id: RunnerInferenceId,
}

impl std::fmt::Debug for InferenceOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("InferenceOwner(<authenticated local scope>)")
    }
}

/// A conservative authenticated clock observation. The round-trip bound is
/// subtracted from grant lifetime; reconnect cannot extend an absolute grant.
#[derive(Clone, Copy, Debug)]
pub struct GrantClock {
    server_unix_ms: u64,
    observed: Instant,
    uncertainty: Duration,
}

impl GrantClock {
    pub fn observed(
        server_unix_ms: u64,
        hello_sent: Instant,
        received: Instant,
    ) -> Result<Self, InferenceHostError> {
        let uncertainty = received.saturating_duration_since(hello_sent);
        if uncertainty > Duration::from_secs(5) {
            return Err(InferenceHostError::InvalidRequest);
        }
        Ok(Self {
            server_unix_ms,
            observed: received,
            uncertainty,
        })
    }

    pub(crate) fn latest_server_time(&self) -> u64 {
        self.server_unix_ms
            .saturating_add(self.observed.elapsed().as_millis().min(u64::MAX as u128) as u64)
            .saturating_add(self.uncertainty.as_millis() as u64)
    }

    fn deadline(
        &self,
        grant: &RunnerInferenceDispatchGrant,
    ) -> Result<Instant, InferenceHostError> {
        let now = self.latest_server_time();
        if grant.start_before_unix_ms <= now
            || grant.deadline_unix_ms <= now
            || grant.start_before_unix_ms > grant.deadline_unix_ms
        {
            return Err(InferenceHostError::InvalidRequest);
        }
        let remaining = Duration::from_millis(grant.deadline_unix_ms - now);
        if remaining > Duration::from_secs(3600) {
            return Err(InferenceHostError::InvalidRequest);
        }
        Ok(Instant::now() + remaining)
    }
}

#[derive(Debug)]
pub enum DispatchOutcome {
    Started,
    Active,
    NotStarted(RunnerInferenceStartEvidence),
    Terminal(RetainedTerminal),
    Acknowledged,
    Unknown,
}

#[derive(Default)]
struct HostState {
    active: HashMap<String, CancellationToken>,
    attached_environment: HashMap<String, (u64, Arc<ResolvedLocalCredential>)>,
}

pub struct InferenceHost {
    owner: InferenceOwner,
    journal_id: RunnerInferenceId,
    process_boot_nonce: RunnerInferenceId,
    journal: Arc<std::sync::Mutex<InferenceJournal>>,
    models_path: PathBuf,
    secrets_root: PathBuf,
    transport: ProviderTransport,
    state: Mutex<HostState>,
    terminal_ready: Notify,
}

impl std::fmt::Debug for InferenceHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InferenceHost")
            .field("journal_id", &self.journal_id)
            .field("process_boot_nonce", &self.process_boot_nonce)
            .finish()
    }
}

impl InferenceHost {
    pub async fn open(
        root: PathBuf,
        owner: InferenceOwner,
        models_path: PathBuf,
        secrets_root: PathBuf,
        transport: ProviderTransport,
    ) -> Result<Arc<Self>, InferenceHostError> {
        if owner.user_id.is_empty()
            || owner.user_id.len() > 255
            || owner.deployment_identity.is_empty()
            || owner.deployment_identity.len() > 4096
        {
            return Err(InferenceHostError::OwnerMismatch);
        }
        let owner_sha256 = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&(&owner.deployment_identity, &owner.user_id, &owner.runner_id))
                    .map_err(|_| InferenceHostError::OwnerMismatch)?
            )
        );
        let journal =
            tokio::task::spawn_blocking(move || InferenceJournal::open(root, owner_sha256))
                .await
                .map_err(|_| InferenceHostError::JournalIo)??;
        let journal_id = journal.journal_id().clone();
        let host = Arc::new(Self {
            owner,
            journal_id,
            process_boot_nonce: RunnerInferenceId::new(uuid::Uuid::new_v4().to_string())
                .map_err(|_| InferenceHostError::Corrupt)?,
            journal: Arc::new(std::sync::Mutex::new(journal)),
            models_path,
            secrets_root,
            transport,
            state: Mutex::new(HostState::default()),
            terminal_ready: Notify::new(),
        });
        // Recovered fences are evidence of possible delivery, never requests to
        // replay. Persist their unknown terminal before advertising capacity.
        host.with_journal(|journal| {
            for grant in journal.fenced() {
                let response = RunnerInferenceResponse {
                    events: Vec::new(),
                    transport: RunnerInferenceTransportTerminal {
                        status: RunnerInferenceTransportStatus::Transport,
                        delivery: RunnerInferenceDeliveryEvidence::MayHaveDispatched,
                        provider_bytes: 0,
                        events_delivered: 0,
                    },
                };
                let terminal = physical_terminal(&response);
                let payload = RetainedTerminal::new(
                    terminal,
                    serde_json::to_string(&response).map_err(|_| InferenceHostError::Corrupt)?,
                )?;
                journal.complete(&grant, payload)?;
            }
            Ok(())
        })
        .await?;
        Ok(host)
    }

    pub fn journal_id(&self) -> &RunnerInferenceId {
        &self.journal_id
    }
    pub fn process_boot_nonce(&self) -> &RunnerInferenceId {
        &self.process_boot_nonce
    }

    async fn config(&self) -> Result<LocalModelConfig, InferenceHostError> {
        let path = self.models_path.clone();
        tokio::task::spawn_blocking(move || {
            LocalModelConfigStore::with_path(path)
                .load()
                .map_err(|_| InferenceHostError::BindingUnavailable)
        })
        .await
        .map_err(|_| InferenceHostError::JournalIo)?
    }

    async fn config_lease(&self) -> Result<LocalModelConfigLease, InferenceHostError> {
        let path = self.models_path.clone();
        tokio::task::spawn_blocking(move || {
            LocalModelConfigStore::with_path(path)
                .lease()
                .map_err(|_| InferenceHostError::BindingUnavailable)
        })
        .await
        .map_err(|_| InferenceHostError::JournalIo)?
    }

    pub async fn bindings(
        &self,
    ) -> Result<Vec<RunnerInferenceBindingDefinition>, InferenceHostError> {
        let config = self.config().await?;
        self.project_bindings(&config).await
    }

    async fn project_bindings(
        &self,
        config: &LocalModelConfig,
    ) -> Result<Vec<RunnerInferenceBindingDefinition>, InferenceHostError> {
        if config.models.len() > 256 {
            return Err(InferenceHostError::Capacity);
        }
        if config.models.is_empty() {
            return Ok(Vec::new());
        }
        let published = self.with_journal(|journal| Ok(journal.published())).await?;
        config
            .models
            .iter()
            .map(|(name, model)| {
                let profile_revision = NonZeroU64::new(model.binding_revision)
                    .ok_or(InferenceHostError::BindingUnavailable)?;
                let binding_id =
                    RunnerInferenceId::new(format!("{:x}", Sha256::digest(name.as_bytes())))
                        .map_err(|_| InferenceHostError::BindingUnavailable)?;
                let binding_revision = match published.get(binding_id.as_str()) {
                    Some(previous)
                        if previous.enabled
                            && previous.identity.profile_revision == profile_revision =>
                    {
                        previous.identity.binding_revision
                    }
                    previous => NonZeroU64::new(
                        previous
                            .map_or(0, |previous| previous.identity.binding_revision.get())
                            .checked_add(1)
                            .ok_or(InferenceHostError::Capacity)?,
                    )
                    .ok_or(InferenceHostError::Capacity)?,
                };
                Ok(RunnerInferenceBindingDefinition {
                    identity: RunnerInferenceBindingIdentity {
                        runner_id: self.owner.runner_id.clone(),
                        journal_id: self.journal_id.clone(),
                        binding_id,
                        binding_revision,
                        profile_revision,
                    },
                    display_name: RunnerInferenceModelName::new(name.clone())
                        .map_err(|_| InferenceHostError::BindingUnavailable)?,
                    model_name: RunnerInferenceModelName::new(model.model.clone())
                        .map_err(|_| InferenceHostError::BindingUnavailable)?,
                    protocol: RunnerInferenceProtocol::OpenAiChatCompletions,
                    context_window: std::num::NonZeroU32::new(model.context_window)
                        .ok_or(InferenceHostError::BindingUnavailable)?,
                    max_output_tokens: std::num::NonZeroU32::new(model.max_output_tokens)
                        .ok_or(InferenceHostError::BindingUnavailable)?,
                })
            })
            .collect()
    }

    /// Environment material must come from the attaching process, never from
    /// whichever terminal happened to start a shared host. It is not persisted.
    pub async fn attach_environment(
        &self,
        name: String,
        binding_revision: u64,
        credential: ResolvedLocalCredential,
    ) -> Result<(), InferenceHostError> {
        let mut state = self.state.lock().await;
        let config = self.config().await?;
        if !config.models.get(&name).is_some_and(|model| {
            model.binding_revision == binding_revision
                && matches!(model.credential, LocalCredentialRef::Environment { .. })
        }) {
            return Err(InferenceHostError::BindingUnavailable);
        }
        state
            .attached_environment
            .insert(name, (binding_revision, Arc::new(credential)));
        Ok(())
    }

    fn validate_owner(
        &self,
        grant: &RunnerInferenceDispatchGrant,
    ) -> Result<(), InferenceHostError> {
        if grant.attempt.user_id != self.owner.user_id
            || grant.attempt.binding.runner_id != self.owner.runner_id
            || grant.attempt.binding.journal_id != self.journal_id
        {
            return Err(InferenceHostError::OwnerMismatch);
        }
        Ok(())
    }

    async fn retain_no_start(
        &self,
        grant: &RunnerInferenceDispatchGrant,
        evidence: RunnerInferenceStartEvidence,
        status: RunnerInferenceTransportStatus,
    ) -> Result<(), InferenceHostError> {
        let valid = matches!(
            (evidence, status),
            (
                RunnerInferenceStartEvidence::CancelledWithoutFence,
                RunnerInferenceTransportStatus::Cancelled
            ) | (
                RunnerInferenceStartEvidence::ExpiredWithoutFence,
                RunnerInferenceTransportStatus::Deadline
            ) | (
                RunnerInferenceStartEvidence::RejectedWithoutFence,
                RunnerInferenceTransportStatus::CredentialUnavailable
                    | RunnerInferenceTransportStatus::BindingUnavailable
                    | RunnerInferenceTransportStatus::CapacityUnavailable
                    | RunnerInferenceTransportStatus::Protocol
            )
        );
        if !valid {
            return Err(InferenceHostError::IdentityConflict);
        }
        let response = RunnerInferenceResponse {
            events: vec![RunnerInferenceProviderEvent::Eof],
            transport: RunnerInferenceTransportTerminal {
                status,
                delivery: RunnerInferenceDeliveryEvidence::NotDispatched,
                provider_bytes: 0,
                events_delivered: 1,
            },
        };
        let payload = RetainedTerminal::new(
            physical_terminal(&response),
            serde_json::to_string(&response).map_err(|_| InferenceHostError::Corrupt)?,
        )?;
        let stored = grant.clone();
        self.with_journal(move |journal| {
            journal.complete_without_start(&stored, evidence, payload)
        })
        .await?;
        self.terminal_ready.notify_one();
        Ok(())
    }

    pub async fn dispatch(
        self: &Arc<Self>,
        grant: RunnerInferenceDispatchGrant,
        request_json: String,
        clock: GrantClock,
    ) -> Result<DispatchOutcome, InferenceHostError> {
        self.validate_owner(&grant)?;
        let mut state = self.state.lock().await;
        let existing = self.lookup(&grant).await?;
        if let Some(existing) = existing {
            return Ok(outcome(existing));
        }
        if grant.process_boot_nonce != self.process_boot_nonce {
            return Err(InferenceHostError::WrongIncarnation);
        }
        if grant.start_before_unix_ms <= clock.latest_server_time() {
            self.retain_no_start(
                &grant,
                RunnerInferenceStartEvidence::ExpiredWithoutFence,
                RunnerInferenceTransportStatus::Deadline,
            )
            .await?;
            return Ok(DispatchOutcome::NotStarted(
                RunnerInferenceStartEvidence::ExpiredWithoutFence,
            ));
        }
        let preparation = async {
            let deadline = clock.deadline(&grant)?;
            if state.active.len() >= MAX_ACTIVE {
                return Err(InferenceHostError::Capacity);
            }
            let config_lease = self.config_lease().await?;
            let config = config_lease.config();
            let definitions = self.project_bindings(config).await?;
            let definition = definitions
                .iter()
                .find(|definition| definition.identity == grant.attempt.binding)
                .ok_or(InferenceHostError::BindingUnavailable)?;
            let (name, model) = config
                .models
                .iter()
                .find(|(name, _)| {
                    format!("{:x}", Sha256::digest(name.as_bytes()))
                        == definition.identity.binding_id.as_str()
                })
                .ok_or(InferenceHostError::BindingUnavailable)?;
            let artifact = ExactProviderRequest::verify_received(
                bytes::Bytes::from(request_json),
                &RequestIdentity {
                    protocol: ProviderProtocol::OpenAiCompatible,
                    sha256: grant.attempt.request.sha256.as_str().to_owned(),
                    bytes: grant.attempt.request.byte_len.get(),
                },
                RUNNER_INFERENCE_ARTIFACT_BYTES,
            )
            .map_err(|_| InferenceHostError::InvalidRequest)?;
            let value: Value = serde_json::from_slice(&artifact.body())
                .map_err(|_| InferenceHostError::InvalidRequest)?;
            if value.get("model").and_then(Value::as_str) != Some(model.model.as_str())
                || value.get("n").is_some_and(|n| n.as_u64() != Some(1))
                || value
                    .get("stream")
                    .is_some_and(|stream| !stream.is_boolean())
            {
                return Err(InferenceHostError::InvalidRequest);
            }
            let output_limit = value
                .get("max_completion_tokens")
                .or_else(|| value.get("max_tokens"))
                .and_then(Value::as_u64)
                .ok_or(InferenceHostError::InvalidRequest)?;
            if output_limit == 0 || output_limit > u64::from(model.max_output_tokens) {
                return Err(InferenceHostError::InvalidRequest);
            }
            let mode = if value
                .get("stream")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                ResponseMode::Sse
            } else {
                ResponseMode::Json
            };
            let protected;
            let key = match &model.credential {
                LocalCredentialRef::Environment { .. } => state
                    .attached_environment
                    .get(name)
                    .filter(|(revision, _)| *revision == model.binding_revision)
                    .map(|(_, key)| key.expose_to_local_transport())
                    .ok_or(InferenceHostError::CredentialUnavailable)?,
                LocalCredentialRef::None => "",
                _ => {
                    let root = self.secrets_root.clone();
                    let reference = model.credential.clone();
                    protected = tokio::task::spawn_blocking(move || {
                        LocalSecretStore::with_root(root)
                            .resolve(&reference)
                            .map_err(|_| InferenceHostError::CredentialUnavailable)
                    })
                    .await
                    .map_err(|_| InferenceHostError::JournalIo)??;
                    protected
                        .as_ref()
                        .map(ResolvedLocalCredential::expose_to_local_transport)
                        .unwrap_or_default()
                }
            };
            let headers = provider_headers(ProviderProtocol::OpenAiCompatible, key, [])
                .map_err(|_| InferenceHostError::InvalidRequest)?;
            let mut endpoint = reqwest::Url::parse(&model.base_url)
                .map_err(|_| InferenceHostError::InvalidRequest)?;
            endpoint.set_path(&format!(
                "{}/chat/completions",
                endpoint.path().trim_end_matches('/')
            ));
            let request = self
                .transport
                .prepare(endpoint.as_str(), headers, &artifact, None)
                .map_err(|_| InferenceHostError::InvalidRequest)?;
            Ok::<_, InferenceHostError>((request, mode, deadline, config_lease))
        }
        .await;
        let (request, mode, deadline, config_lease) = match preparation {
            Ok(prepared) => prepared,
            Err(error) => {
                // Only an authenticated, exact current-incarnation grant may
                // receive negative evidence. Persistence failure is not proof.
                let status = match error {
                    InferenceHostError::CredentialUnavailable => {
                        RunnerInferenceTransportStatus::CredentialUnavailable
                    }
                    InferenceHostError::BindingUnavailable => {
                        RunnerInferenceTransportStatus::BindingUnavailable
                    }
                    InferenceHostError::Capacity => {
                        RunnerInferenceTransportStatus::CapacityUnavailable
                    }
                    InferenceHostError::JournalIo
                    | InferenceHostError::Corrupt
                    | InferenceHostError::UnsafeStorage
                    | InferenceHostError::UnsupportedPlatform
                    | InferenceHostError::AlreadyRunning => return Err(error),
                    _ => RunnerInferenceTransportStatus::Protocol,
                };
                self.retain_no_start(
                    &grant,
                    RunnerInferenceStartEvidence::RejectedWithoutFence,
                    status,
                )
                .await?;
                return Ok(DispatchOutcome::NotStarted(
                    RunnerInferenceStartEvidence::RejectedWithoutFence,
                ));
            }
        };
        if grant.start_before_unix_ms <= clock.latest_server_time() {
            self.retain_no_start(
                &grant,
                RunnerInferenceStartEvidence::ExpiredWithoutFence,
                RunnerInferenceTransportStatus::Deadline,
            )
            .await?;
            return Ok(DispatchOutcome::NotStarted(
                RunnerInferenceStartEvidence::ExpiredWithoutFence,
            ));
        }
        // The OS-persisted fence is committed while the same lock excludes
        // cancellation and binding material attachment changes.
        let stored = grant.clone();
        if !self
            .with_journal(move |journal| journal.fence(&stored))
            .await?
        {
            return Err(InferenceHostError::IdentityConflict);
        }
        // The durable fence now owns the exact revision/material snapshot.
        // A concurrent CLI mutation may proceed only after this point.
        drop(config_lease);
        let cancellation = CancellationToken::new();
        state.active.insert(
            grant.attempt.attempt_id.as_str().to_owned(),
            cancellation.clone(),
        );
        let host = self.clone();
        tokio::spawn(async move {
            // A slow durable commit can cross the start cutoff. The fence is
            // already conservative, but no HTTP request may begin afterwards.
            let deadline = if grant.start_before_unix_ms <= clock.latest_server_time() {
                Instant::now()
            } else {
                deadline
            };
            let payload =
                execute_and_retain(&host.transport, request, mode, deadline, &cancellation).await;
            let stored = grant.clone();
            let result = match payload {
                Ok(payload) => {
                    host.with_journal(move |journal| journal.complete(&stored, payload))
                        .await
                }
                Err(error) => Err(error),
            };
            host.state
                .lock()
                .await
                .active
                .remove(grant.attempt.attempt_id.as_str());
            if result.is_err() {
                tracing::error!(
                    "local inference terminal persistence failed; attempt remains fenced"
                );
            }
            host.terminal_ready.notify_one();
        });
        Ok(DispatchOutcome::Started)
    }

    pub async fn cancel(
        &self,
        grant: &RunnerInferenceDispatchGrant,
    ) -> Result<DispatchOutcome, InferenceHostError> {
        self.validate_owner(grant)?;
        let state = self.state.lock().await;
        if let Some(record) = self.lookup(grant).await? {
            if let Some(token) = state.active.get(grant.attempt.attempt_id.as_str()) {
                token.cancel();
            }
            return Ok(outcome(record));
        }
        if grant.process_boot_nonce != self.process_boot_nonce {
            return Err(InferenceHostError::WrongIncarnation);
        }
        self.retain_no_start(
            grant,
            RunnerInferenceStartEvidence::CancelledWithoutFence,
            RunnerInferenceTransportStatus::Cancelled,
        )
        .await?;
        Ok(DispatchOutcome::NotStarted(
            RunnerInferenceStartEvidence::CancelledWithoutFence,
        ))
    }

    pub async fn reconcile(
        &self,
        grant: &RunnerInferenceDispatchGrant,
    ) -> Result<DispatchOutcome, InferenceHostError> {
        self.validate_owner(grant)?;
        Ok(self
            .lookup(grant)
            .await?
            .map(outcome)
            .unwrap_or(DispatchOutcome::Unknown))
    }

    pub async fn acknowledge(
        &self,
        ack: RunnerInferenceTerminalAck,
    ) -> Result<(), InferenceHostError> {
        if ack.attempt.user_id != self.owner.user_id
            || ack.attempt.binding.journal_id != self.journal_id
        {
            return Err(InferenceHostError::OwnerMismatch);
        }
        self.with_journal(move |journal| journal.acknowledge(&ack))
            .await
    }

    pub async fn next_publication(
        &self,
    ) -> Result<Option<RunnerInferenceBindingPublication>, InferenceHostError> {
        let desired = self.bindings().await?;
        self.with_journal(move |journal| journal.next_publication(desired))
            .await
    }

    pub async fn publication_ack(
        &self,
        receipt: RunnerInferenceBindingReceipt,
    ) -> Result<(), InferenceHostError> {
        self.with_journal(move |journal| journal.publication_ack(&receipt))
            .await
    }

    pub async fn pending(
        &self,
        limit: usize,
    ) -> Result<Vec<(RunnerInferenceDispatchGrant, RetainedTerminal)>, InferenceHostError> {
        self.with_journal(move |journal| {
            Ok(journal
                .pending(limit)
                .into_iter()
                .filter_map(|record| match record.state {
                    RecordState::TerminalAwaitingAck { payload }
                    | RecordState::NotStartedAwaitingAck { payload, .. } => {
                        Some((record.grant, payload))
                    }
                    _ => None,
                })
                .collect())
        })
        .await
    }

    pub async fn terminal_ready(&self) {
        self.terminal_ready.notified().await;
    }

    async fn lookup(
        &self,
        grant: &RunnerInferenceDispatchGrant,
    ) -> Result<Option<JournalRecord>, InferenceHostError> {
        let grant = grant.clone();
        self.with_journal(move |journal| journal.record(&grant).map(|record| record.cloned()))
            .await
    }

    async fn with_journal<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut InferenceJournal) -> Result<T, InferenceHostError> + Send + 'static,
    ) -> Result<T, InferenceHostError> {
        let journal = self.journal.clone();
        tokio::task::spawn_blocking(move || {
            let mut journal = journal.lock().map_err(|_| InferenceHostError::JournalIo)?;
            operation(&mut journal)
        })
        .await
        .map_err(|_| InferenceHostError::JournalIo)?
    }
}

fn outcome(record: JournalRecord) -> DispatchOutcome {
    match record.state {
        RecordState::ExecutionFenced => DispatchOutcome::Active,
        RecordState::NotStartedAwaitingAck { evidence, .. } => {
            DispatchOutcome::NotStarted(evidence)
        }
        RecordState::TerminalAwaitingAck { payload } => DispatchOutcome::Terminal(payload),
        RecordState::Acknowledged { .. } => DispatchOutcome::Acknowledged,
    }
}

async fn execute_and_retain(
    transport: &ProviderTransport,
    request: astra_inference_adapter::transport::PreparedHttpAttempt,
    mode: ResponseMode,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<RetainedTerminal, InferenceHostError> {
    let (sender, mut receiver) = mpsc::channel(4);
    let limits = ExecutionLimits {
        event_bytes: 1024 * 1024,
        total_bytes: RUNNER_INFERENCE_ARTIFACT_BYTES,
        events: 16_384,
    };
    let execution = transport.execute(request, mode, limits, deadline, cancellation, &sender);
    tokio::pin!(execution);
    let mut events = Vec::new();
    let mut encoded_bytes = RESPONSE_OVERHEAD_RESERVE;
    let mut overflow = false;
    let mut terminal;
    loop {
        tokio::select! {
            biased;
            Some(event) = receiver.recv() => {
                    let event = match event { ProviderEvent::Json(value) => RunnerInferenceProviderEvent::Json(value), ProviderEvent::Done => RunnerInferenceProviderEvent::Done, ProviderEvent::Eof => RunnerInferenceProviderEvent::Eof };
                    let bytes = serde_json::to_vec(&event).map_err(|_| InferenceHostError::TooLarge)?.len() + 1;
                    if bytes > RUNNER_INFERENCE_ARTIFACT_BYTES.saturating_sub(encoded_bytes) { overflow = true; cancellation.cancel(); receiver.close(); }
                    else if !overflow { encoded_bytes += bytes; events.push(event); }
            }
            result = &mut execution => {
                terminal = result;
                // Provider execution resolves only after its final event send,
                // but that event may still be queued when both select branches
                // become ready. Drain the bounded queue before sealing custody.
                while let Ok(event) = receiver.try_recv() {
                    let event = match event { ProviderEvent::Json(value) => RunnerInferenceProviderEvent::Json(value), ProviderEvent::Done => RunnerInferenceProviderEvent::Done, ProviderEvent::Eof => RunnerInferenceProviderEvent::Eof };
                    let bytes = serde_json::to_vec(&event).map_err(|_| InferenceHostError::TooLarge)?.len() + 1;
                    if bytes > RUNNER_INFERENCE_ARTIFACT_BYTES.saturating_sub(encoded_bytes) { overflow = true; }
                    else if !overflow { encoded_bytes += bytes; events.push(event); }
                }
                break;
            }
        }
    }
    // The event branch is biased above, so a ready event is consumed before a
    // ready terminal and no successful final queued event disappears.
    if overflow {
        terminal.status = ExecutionStatus::Limit;
    }
    let response = RunnerInferenceResponse {
        events,
        transport: map_transport(terminal),
    };
    RetainedTerminal::new(
        physical_terminal(&response),
        serde_json::to_string(&response).map_err(|_| InferenceHostError::TooLarge)?,
    )
}

fn map_transport(terminal: ExecutionTerminal) -> RunnerInferenceTransportTerminal {
    RunnerInferenceTransportTerminal {
        status: match terminal.status {
            ExecutionStatus::Complete => RunnerInferenceTransportStatus::Complete,
            ExecutionStatus::Cancelled => RunnerInferenceTransportStatus::Cancelled,
            ExecutionStatus::Deadline => RunnerInferenceTransportStatus::Deadline,
            ExecutionStatus::Transport => RunnerInferenceTransportStatus::Transport,
            ExecutionStatus::Protocol => RunnerInferenceTransportStatus::Protocol,
            ExecutionStatus::Limit => RunnerInferenceTransportStatus::Limit,
            ExecutionStatus::ConsumerClosed => RunnerInferenceTransportStatus::ConsumerClosed,
            ExecutionStatus::HttpStatus(code) => RunnerInferenceTransportStatus::HttpStatus(code),
        },
        delivery: match terminal.delivery {
            DeliveryEvidence::NotDispatched => RunnerInferenceDeliveryEvidence::NotDispatched,
            DeliveryEvidence::MayHaveDispatched => {
                RunnerInferenceDeliveryEvidence::MayHaveDispatched
            }
            DeliveryEvidence::ResponseHeaders => RunnerInferenceDeliveryEvidence::ResponseHeaders,
        },
        provider_bytes: terminal.provider_bytes,
        events_delivered: terminal.events_delivered,
    }
}

fn physical_terminal(response: &RunnerInferenceResponse) -> InferenceInvocationTerminal {
    let status = match (response.transport.status, response.transport.delivery) {
        (RunnerInferenceTransportStatus::Complete, _) => InferenceTerminalStatus::Succeeded,
        (RunnerInferenceTransportStatus::HttpStatus(_), _) => InferenceTerminalStatus::Failed,
        (
            RunnerInferenceTransportStatus::Cancelled,
            RunnerInferenceDeliveryEvidence::NotDispatched,
        ) => InferenceTerminalStatus::Cancelled,
        (
            RunnerInferenceTransportStatus::Deadline
            | RunnerInferenceTransportStatus::Transport
            | RunnerInferenceTransportStatus::CredentialUnavailable
            | RunnerInferenceTransportStatus::BindingUnavailable
            | RunnerInferenceTransportStatus::CapacityUnavailable
            | RunnerInferenceTransportStatus::Protocol
            | RunnerInferenceTransportStatus::Limit,
            RunnerInferenceDeliveryEvidence::NotDispatched,
        ) => InferenceTerminalStatus::Failed,
        _ => InferenceTerminalStatus::DeliveryUnknown,
    };
    let provider_response_id = response.events.iter().find_map(|event| match event {
        RunnerInferenceProviderEvent::Json(value) => value
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| id.len() <= 255)
            .map(str::to_owned),
        _ => None,
    });
    let usage = response.events.iter().rev().find_map(|event| match event {
        RunnerInferenceProviderEvent::Json(value) => value
            .get("usage")
            .and_then(Value::as_object)
            .and_then(astra_turn_types::runner_inference::normalize_openai_compatible_usage),
        _ => None,
    });
    InferenceInvocationTerminal {
        status,
        usage: usage.clone().unwrap_or_default(),
        usage_status: if usage.is_some() {
            InferenceUsageStatus::ProviderExact
        } else {
            InferenceUsageStatus::Unavailable
        },
        provider_response_id,
        error_kind: (status != InferenceTerminalStatus::Succeeded).then(|| {
            match response.transport.status {
                RunnerInferenceTransportStatus::CredentialUnavailable => {
                    "runner_credential_unavailable"
                }
                RunnerInferenceTransportStatus::BindingUnavailable => "runner_binding_unavailable",
                RunnerInferenceTransportStatus::CapacityUnavailable => {
                    "runner_capacity_unavailable"
                }
                _ => "runner_provider_transport",
            }
            .to_string()
        }),
        error_message: None,
    }
}

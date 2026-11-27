//! One local execution inbox/outbox. A durable fence is deliberately
//! conservative: after an ambiguous write or restart it never authorizes resend.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

use astra_turn_types::runner_inference::{
    InferenceInvocationTerminal, RUNNER_INFERENCE_ARTIFACT_BYTES, RunnerInferenceAttemptIdentity,
    RunnerInferenceDigest, RunnerInferenceDispatchGrant, RunnerInferenceId,
    RunnerInferenceRejection, RunnerInferenceStartEvidence, RunnerInferenceTerminalAck,
    RunnerInferenceTerminalRetirement, runner_terminal_digest,
};
use astra_turn_types::runner_inference::{
    RUNNER_INFERENCE_PROTOCOL_VERSION, RunnerInferenceBindingChange,
    RunnerInferenceBindingDefinition, RunnerInferenceBindingIdentity,
    RunnerInferenceBindingPublication, RunnerInferenceBindingReceipt,
};
use serde::{Deserialize, Serialize};

const MAX_IDENTITY_BYTES: usize = 1024 * 1024;
const MAX_RECORDS: usize = 4096;
const MAX_RECORD_BYTES: usize = RUNNER_INFERENCE_ARTIFACT_BYTES * 2 + 64 * 1024;
const MAX_RESERVED_BYTES: usize = 256 * 1024 * 1024;
const ACK_TOMBSTONE_RETENTION_MS: u64 = 24 * 60 * 60 * 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InferenceHostError {
    #[error("local inference journal I/O failed; reconcile before retrying")]
    JournalIo,
    #[error("local inference journal is already owned by another process")]
    AlreadyRunning,
    #[error("local inference journal failed integrity validation")]
    Corrupt,
    #[error("local inference journal owner does not match")]
    OwnerMismatch,
    #[error("local inference journal is not protected by the OS user boundary")]
    UnsafeStorage,
    #[error("protected inference journal is not supported on this platform")]
    UnsupportedPlatform,
    #[error("local inference capacity is exhausted")]
    Capacity,
    #[error("inference identity conflicts with durable evidence")]
    IdentityConflict,
    #[error("inference terminal exceeds the admitted custody limit")]
    TooLarge,
    #[error("inference binding is unavailable or changed")]
    BindingUnavailable,
    #[error("local provider credential is unavailable")]
    CredentialUnavailable,
    #[error("inference request failed local validation")]
    InvalidRequest,
    #[error("inference grant is for a different process incarnation")]
    WrongIncarnation,
    #[error(
        "local model host uses different proxy or CA settings; close other local Astra sessions and reopen, or use an explicitly managed Runner"
    )]
    NetworkPolicyMismatch,
    #[error(
        "local model host protocol version differs; upgrade Astra and astra-edge together, close other local Astra sessions, let active work drain and the shared host exit, then reopen Astra"
    )]
    LocalProtocolMismatch,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedTerminal {
    pub terminal: InferenceInvocationTerminal,
    pub response_json: String,
    pub terminal_sha256: RunnerInferenceDigest,
}

impl std::fmt::Debug for RetainedTerminal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetainedTerminal")
            .field("terminal", &self.terminal)
            .field("response_bytes", &self.response_json.len())
            .finish()
    }
}

impl RetainedTerminal {
    pub fn new(
        terminal: InferenceInvocationTerminal,
        response_json: String,
    ) -> Result<Self, InferenceHostError> {
        terminal
            .validate_wire_bounds()
            .map_err(|_| InferenceHostError::TooLarge)?;
        if response_json.len() > RUNNER_INFERENCE_ARTIFACT_BYTES {
            return Err(InferenceHostError::TooLarge);
        }
        let terminal_sha256 = runner_terminal_digest(&terminal, response_json.as_bytes())
            .map_err(|_| InferenceHostError::Corrupt)?;
        Ok(Self {
            terminal,
            response_json,
            terminal_sha256,
        })
    }

    fn validate(&self) -> Result<(), InferenceHostError> {
        self.terminal
            .validate_wire_bounds()
            .map_err(|_| InferenceHostError::TooLarge)?;
        if self.response_json.len() > RUNNER_INFERENCE_ARTIFACT_BYTES {
            return Err(InferenceHostError::TooLarge);
        }
        let expected = runner_terminal_digest(&self.terminal, self.response_json.as_bytes())
            .map_err(|_| InferenceHostError::Corrupt)?;
        if expected != self.terminal_sha256 {
            return Err(InferenceHostError::Corrupt);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum RecordState {
    ExecutionFenced,
    NotStartedAwaitingAck {
        evidence: RunnerInferenceStartEvidence,
        payload: RetainedTerminal,
    },
    TerminalAwaitingAck {
        payload: RetainedTerminal,
    },
    TerminalQuarantined {
        payload: RetainedTerminal,
        reason: RunnerInferenceRejection,
        no_start_evidence: Option<RunnerInferenceStartEvidence>,
    },
    Retired {
        terminal_sha256: RunnerInferenceDigest,
        retired_unix_ms: u64,
    },
    Acknowledged {
        terminal_sha256: RunnerInferenceDigest,
        acknowledged_unix_ms: u64,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct JournalRecord {
    version: u32,
    pub grant: RunnerInferenceDispatchGrant,
    pub state: RecordState,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalIdentity {
    version: u32,
    owner_sha256: String,
    journal_id: RunnerInferenceId,
    publication_revision: u64,
    pending_publication: Option<RunnerInferenceBindingPublication>,
    published: BTreeMap<String, PublishedBinding>,
}

impl JournalIdentity {
    fn discard_obsolete_bindings(&mut self) {
        self.published.retain(|id, binding| {
            binding.enabled || (!id.starts_with("env-") && !id.starts_with("native-"))
        });
    }

    fn after_publication(
        &self,
        revision: u64,
        publication: &RunnerInferenceBindingPublication,
    ) -> Self {
        let mut candidate = self.clone();
        let identity = publication.change.identity();
        candidate.published.insert(
            identity.binding_id.as_str().to_owned(),
            PublishedBinding {
                identity: identity.clone(),
                enabled: matches!(
                    publication.change,
                    RunnerInferenceBindingChange::Publish { .. }
                ),
            },
        );
        candidate.discard_obsolete_bindings();
        candidate.publication_revision = revision;
        candidate.pending_publication = None;
        candidate
    }

    fn encode(&self) -> Result<Vec<u8>, InferenceHostError> {
        let encoded = serde_json::to_vec(self).map_err(|_| InferenceHostError::Corrupt)?;
        if encoded.len() > MAX_IDENTITY_BYTES {
            return Err(InferenceHostError::TooLarge);
        }
        Ok(encoded)
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublishedBinding {
    pub identity: RunnerInferenceBindingIdentity,
    pub enabled: bool,
}

pub(crate) struct InferenceJournal {
    root: PathBuf,
    identity: JournalIdentity,
    records: BTreeMap<String, JournalRecord>,
    healthy: bool,
    _lock: File,
    #[cfg(test)]
    fail_next_commit: bool,
}

impl InferenceJournal {
    pub fn open(root: PathBuf, owner_sha256: String) -> Result<Self, InferenceHostError> {
        ensure_private_directory(&root)?;
        let lock = open_private(&root.join("host.lock"), true)?;
        fs2::FileExt::try_lock_exclusive(&lock).map_err(|_| InferenceHostError::AlreadyRunning)?;
        let identity_path = root.join("identity.json");
        let identity = match read_private(&identity_path, MAX_IDENTITY_BYTES) {
            Ok(bytes) => serde_json::from_slice::<JournalIdentity>(&bytes)
                .map_err(|_| InferenceHostError::Corrupt)?,
            Err(InferenceHostError::JournalIo)
                if !identity_path
                    .try_exists()
                    .map_err(|_| InferenceHostError::JournalIo)? =>
            {
                if fs::read_dir(&root)
                    .map_err(|_| InferenceHostError::JournalIo)?
                    .filter_map(Result::ok)
                    .any(|entry| entry.file_name() != "host.lock")
                {
                    return Err(InferenceHostError::Corrupt);
                }
                let identity = JournalIdentity {
                    version: 1,
                    owner_sha256: owner_sha256.clone(),
                    journal_id: RunnerInferenceId::new(uuid::Uuid::new_v4().to_string())
                        .map_err(|_| InferenceHostError::Corrupt)?,
                    publication_revision: 0,
                    pending_publication: None,
                    published: BTreeMap::new(),
                };
                atomic_write(
                    &identity_path,
                    &serde_json::to_vec(&identity).map_err(|_| InferenceHostError::Corrupt)?,
                )?;
                identity
            }
            Err(error) => return Err(error),
        };
        if identity.version != 1 {
            return Err(InferenceHostError::Corrupt);
        }
        if identity.owner_sha256 != owner_sha256 {
            return Err(InferenceHostError::OwnerMismatch);
        }
        let mut records = BTreeMap::new();
        for entry in fs::read_dir(&root).map_err(|_| InferenceHostError::JournalIo)? {
            let entry = entry.map_err(|_| InferenceHostError::JournalIo)?;
            let filename = entry.file_name();
            let Some(filename) = filename.to_str() else {
                return Err(InferenceHostError::Corrupt);
            };
            if filename == "identity.json"
                || filename == "host.lock"
                || filename.starts_with(".inference-")
            {
                continue;
            }
            if !filename.starts_with("attempt-") || !filename.ends_with(".json") {
                return Err(InferenceHostError::Corrupt);
            }
            if records.len() >= MAX_RECORDS {
                return Err(InferenceHostError::Capacity);
            }
            let record: JournalRecord =
                serde_json::from_slice(&read_private(&entry.path(), MAX_RECORD_BYTES)?)
                    .map_err(|_| InferenceHostError::Corrupt)?;
            if record.version != 1
                || record.grant.attempt.binding.journal_id != identity.journal_id
                || filename != format!("attempt-{}.json", record.grant.attempt.attempt_id.as_str())
            {
                return Err(InferenceHostError::Corrupt);
            }
            if let RecordState::TerminalAwaitingAck { payload }
            | RecordState::TerminalQuarantined { payload, .. }
            | RecordState::NotStartedAwaitingAck { payload, .. } = &record.state
            {
                payload.validate()?;
            }
            if matches!(
                record.state,
                RecordState::NotStartedAwaitingAck {
                    evidence: RunnerInferenceStartEvidence::FenceCommitted
                        | RunnerInferenceStartEvidence::ProviderStarted
                        | RunnerInferenceStartEvidence::ExecutionActive
                        | RunnerInferenceStartEvidence::ExecutionEndedWithoutTerminal,
                    ..
                }
            ) {
                return Err(InferenceHostError::Corrupt);
            }
            records.insert(record.grant.attempt.attempt_id.as_str().to_owned(), record);
        }
        let journal = Self {
            root,
            identity,
            records,
            healthy: true,
            _lock: lock,
            #[cfg(test)]
            fail_next_commit: false,
        };
        if journal.reserved_bytes() > MAX_RESERVED_BYTES {
            return Err(InferenceHostError::Capacity);
        }
        Ok(journal)
    }

    pub fn journal_id(&self) -> &RunnerInferenceId {
        &self.identity.journal_id
    }

    pub fn published(&self) -> BTreeMap<String, PublishedBinding> {
        self.identity.published.clone()
    }

    pub fn pending_binding_id(&self) -> Option<String> {
        match &self.identity.pending_publication.as_ref()?.change {
            RunnerInferenceBindingChange::Publish { definition } => {
                Some(definition.identity.binding_id.as_str().into())
            }
            RunnerInferenceBindingChange::Disable { .. } => None,
        }
    }

    pub fn next_publication(
        &mut self,
        desired: Vec<RunnerInferenceBindingDefinition>,
    ) -> Result<Option<RunnerInferenceBindingPublication>, InferenceHostError> {
        if !self.healthy {
            return Err(InferenceHostError::JournalIo);
        }
        if let Some(pending) = &self.identity.pending_publication {
            return Ok(Some(pending.clone()));
        }
        let change = self
            .identity
            .published
            .values()
            .find(|published| {
                published.enabled
                    && !desired.iter().any(|definition| {
                        definition.identity.binding_id == published.identity.binding_id
                    })
            })
            .map(|published| RunnerInferenceBindingChange::Disable {
                identity: published.identity.clone(),
            })
            .or_else(|| {
                desired
                    .into_iter()
                    .find(|definition| {
                        !self
                            .identity
                            .published
                            .get(definition.identity.binding_id.as_str())
                            .is_some_and(|published| {
                                published.enabled && published.identity == definition.identity
                            })
                    })
                    .map(|definition| RunnerInferenceBindingChange::Publish { definition })
            });
        let Some(mut change) = change else {
            return Ok(None);
        };
        let next_revision = NonZeroU64::new(
            self.identity
                .publication_revision
                .checked_add(1)
                .ok_or(InferenceHostError::Capacity)?,
        )
        .ok_or(InferenceHostError::Capacity)?;
        if let RunnerInferenceBindingChange::Publish { definition } = &mut change
            && definition
                .identity
                .binding_id
                .as_str()
                .starts_with("native-")
        {
            // The journal-wide sequence never repeats, even after a disabled
            // native binding's local watermark is collected.
            definition.identity.binding_revision = next_revision;
        }
        let publication = RunnerInferenceBindingPublication {
            protocol_version: RUNNER_INFERENCE_PROTOCOL_VERSION,
            operation_id: RunnerInferenceId::new(uuid::Uuid::new_v4().to_string())
                .map_err(|_| InferenceHostError::Corrupt)?,
            expected_publication_revision: self.identity.publication_revision,
            change,
        };
        let mut candidate = self.identity.clone();
        candidate.discard_obsolete_bindings();
        candidate.pending_publication = Some(publication.clone());
        // Prove that both halves fit before any operation can reach Server.
        let acknowledged = candidate.after_publication(next_revision.get(), &publication);
        acknowledged.encode()?;
        self.persist_identity(candidate)?;
        Ok(Some(publication))
    }

    pub fn publication_ack(
        &mut self,
        receipt: &RunnerInferenceBindingReceipt,
    ) -> Result<(), InferenceHostError> {
        if receipt.publication_revision.get() <= self.identity.publication_revision {
            return Ok(());
        }
        let pending = self
            .identity
            .pending_publication
            .as_ref()
            .ok_or(InferenceHostError::IdentityConflict)?;
        if pending.operation_id != receipt.operation_id
            || *pending.change.identity() != receipt.identity
            || receipt.publication_revision.get()
                != self
                    .identity
                    .publication_revision
                    .checked_add(1)
                    .ok_or(InferenceHostError::Capacity)?
        {
            return Err(InferenceHostError::IdentityConflict);
        }
        let candidate = self
            .identity
            .after_publication(receipt.publication_revision.get(), pending);
        self.persist_identity(candidate)
    }

    fn persist_identity(&mut self, candidate: JournalIdentity) -> Result<(), InferenceHostError> {
        if !self.healthy {
            return Err(InferenceHostError::JournalIo);
        }
        let encoded = candidate.encode()?;
        if let Err(error) = atomic_write(&self.root.join("identity.json"), &encoded) {
            self.healthy = false;
            return Err(error);
        }
        self.identity = candidate;
        Ok(())
    }

    pub fn record(
        &self,
        grant: &RunnerInferenceDispatchGrant,
    ) -> Result<Option<&JournalRecord>, InferenceHostError> {
        let record = self.records.get(grant.attempt.attempt_id.as_str());
        if record.is_some_and(|record| record.grant != *grant) {
            return Err(InferenceHostError::IdentityConflict);
        }
        Ok(record)
    }

    pub fn fence(
        &mut self,
        grant: &RunnerInferenceDispatchGrant,
    ) -> Result<bool, InferenceHostError> {
        if !self.healthy {
            return Err(InferenceHostError::JournalIo);
        }
        if self.record(grant)?.is_some() {
            return Ok(false);
        }
        self.prune_expired_tombstones()?;
        if grant.attempt.binding.journal_id != self.identity.journal_id {
            return Err(InferenceHostError::OwnerMismatch);
        }
        if self.records.len() >= MAX_RECORDS
            || self.reserved_bytes().saturating_add(MAX_RECORD_BYTES) > MAX_RESERVED_BYTES
        {
            return Err(InferenceHostError::Capacity);
        }
        self.commit(JournalRecord {
            version: 1,
            grant: grant.clone(),
            state: RecordState::ExecutionFenced,
        })?;
        Ok(true)
    }

    pub fn complete(
        &mut self,
        grant: &RunnerInferenceDispatchGrant,
        payload: RetainedTerminal,
    ) -> Result<(), InferenceHostError> {
        payload.validate()?;
        match self.record(grant)?.map(|record| &record.state) {
            Some(RecordState::ExecutionFenced) => self.commit(JournalRecord {
                version: 1,
                grant: grant.clone(),
                state: RecordState::TerminalAwaitingAck { payload },
            }),
            Some(RecordState::TerminalAwaitingAck { payload: previous })
                if previous.terminal_sha256 == payload.terminal_sha256 =>
            {
                Ok(())
            }
            _ => Err(InferenceHostError::IdentityConflict),
        }
    }

    pub fn complete_without_start(
        &mut self,
        grant: &RunnerInferenceDispatchGrant,
        evidence: RunnerInferenceStartEvidence,
        payload: RetainedTerminal,
    ) -> Result<(), InferenceHostError> {
        payload.validate()?;
        self.prune_expired_tombstones()?;
        match self.record(grant)?.map(|record| &record.state) {
            None => {
                if grant.attempt.binding.journal_id != self.identity.journal_id {
                    return Err(InferenceHostError::OwnerMismatch);
                }
                if self.records.len() >= MAX_RECORDS
                    || self.reserved_bytes().saturating_add(MAX_RECORD_BYTES) > MAX_RESERVED_BYTES
                {
                    return Err(InferenceHostError::Capacity);
                }
                self.commit(JournalRecord {
                    version: 1,
                    grant: grant.clone(),
                    state: RecordState::NotStartedAwaitingAck { evidence, payload },
                })
            }
            Some(RecordState::NotStartedAwaitingAck {
                evidence: recorded,
                payload: previous,
            }) if *recorded == evidence && previous.terminal_sha256 == payload.terminal_sha256 => {
                Ok(())
            }
            _ => Err(InferenceHostError::IdentityConflict),
        }
    }

    pub fn acknowledge(
        &mut self,
        ack: &RunnerInferenceTerminalAck,
    ) -> Result<(), InferenceHostError> {
        let record = self.terminal_record(&ack.attempt, &ack.terminal_sha256)?;
        if matches!(
            record.state,
            RecordState::Acknowledged { .. } | RecordState::Retired { .. }
        ) {
            return Ok(());
        }
        self.commit(JournalRecord {
            version: 1,
            grant: record.grant.clone(),
            state: RecordState::Acknowledged {
                terminal_sha256: ack.terminal_sha256.clone(),
                acknowledged_unix_ms: unix_time_ms(),
            },
        })
    }

    fn terminal_record(
        &self,
        attempt: &RunnerInferenceAttemptIdentity,
        terminal_sha256: &RunnerInferenceDigest,
    ) -> Result<&JournalRecord, InferenceHostError> {
        let record = self
            .records
            .get(attempt.attempt_id.as_str())
            .ok_or(InferenceHostError::IdentityConflict)?;
        if record.grant.attempt != *attempt {
            return Err(InferenceHostError::IdentityConflict);
        }
        let expected = match &record.state {
            RecordState::TerminalAwaitingAck { payload }
            | RecordState::TerminalQuarantined { payload, .. }
            | RecordState::NotStartedAwaitingAck { payload, .. } => &payload.terminal_sha256,
            RecordState::Acknowledged {
                terminal_sha256, ..
            }
            | RecordState::Retired {
                terminal_sha256, ..
            } => terminal_sha256,
            _ => return Err(InferenceHostError::IdentityConflict),
        };
        if expected != terminal_sha256 {
            return Err(InferenceHostError::IdentityConflict);
        }
        Ok(record)
    }

    pub fn retire(
        &mut self,
        receipt: &RunnerInferenceTerminalRetirement,
    ) -> Result<(), InferenceHostError> {
        let record = self.terminal_record(&receipt.attempt, &receipt.terminal_sha256)?;
        if matches!(record.state, RecordState::Retired { .. }) {
            return Ok(());
        }
        self.commit(JournalRecord {
            version: 1,
            grant: record.grant.clone(),
            state: RecordState::Retired {
                terminal_sha256: receipt.terminal_sha256.clone(),
                retired_unix_ms: unix_time_ms(),
            },
        })
    }

    pub fn quarantine(
        &mut self,
        attempt: &RunnerInferenceAttemptIdentity,
        terminal_sha256: &RunnerInferenceDigest,
        reason: RunnerInferenceRejection,
    ) -> Result<(), InferenceHostError> {
        if !matches!(
            reason,
            RunnerInferenceRejection::InvalidEvidence
                | RunnerInferenceRejection::BindingIdentityMismatch
        ) {
            return Err(InferenceHostError::InvalidRequest);
        }
        let record = self.terminal_record(attempt, terminal_sha256)?;
        let (payload, no_start_evidence) = match &record.state {
            RecordState::TerminalAwaitingAck { payload } => (payload.clone(), None),
            RecordState::NotStartedAwaitingAck { payload, evidence } => {
                (payload.clone(), Some(*evidence))
            }
            RecordState::TerminalQuarantined { .. }
            | RecordState::Retired { .. }
            | RecordState::Acknowledged { .. } => return Ok(()),
            _ => return Err(InferenceHostError::IdentityConflict),
        };
        self.commit(JournalRecord {
            version: 1,
            grant: record.grant.clone(),
            state: RecordState::TerminalQuarantined {
                payload,
                reason,
                no_start_evidence,
            },
        })
    }

    pub fn pending(&self, limit: usize) -> Vec<JournalRecord> {
        self.records
            .values()
            .filter(|record| {
                matches!(
                    record.state,
                    RecordState::TerminalAwaitingAck { .. }
                        | RecordState::NotStartedAwaitingAck { .. }
                )
            })
            .take(limit.min(8))
            .cloned()
            .collect()
    }

    pub fn fenced(&self) -> Vec<RunnerInferenceDispatchGrant> {
        self.records
            .values()
            .filter(|record| matches!(record.state, RecordState::ExecutionFenced))
            .map(|record| record.grant.clone())
            .collect()
    }

    #[cfg(test)]
    pub fn fail_next_commit(&mut self) {
        self.fail_next_commit = true;
    }

    fn reserved_bytes(&self) -> usize {
        self.records
            .values()
            .map(|record| match &record.state {
                RecordState::ExecutionFenced => MAX_RECORD_BYTES,
                RecordState::TerminalAwaitingAck { payload }
                | RecordState::TerminalQuarantined { payload, .. }
                | RecordState::NotStartedAwaitingAck { payload, .. } => payload
                    .response_json
                    .len()
                    .saturating_mul(2)
                    .saturating_add(64 * 1024),
                _ => 64 * 1024,
            })
            .sum()
    }

    fn commit(&mut self, record: JournalRecord) -> Result<(), InferenceHostError> {
        if !self.healthy {
            return Err(InferenceHostError::JournalIo);
        }
        let encoded = serde_json::to_vec(&record).map_err(|_| InferenceHostError::Corrupt)?;
        if encoded.len() > MAX_RECORD_BYTES {
            return Err(InferenceHostError::TooLarge);
        }
        #[cfg(test)]
        if std::mem::take(&mut self.fail_next_commit) {
            self.healthy = false;
            return Err(InferenceHostError::JournalIo);
        }
        let path = self.root.join(format!(
            "attempt-{}.json",
            record.grant.attempt.attempt_id.as_str()
        ));
        if let Err(error) = atomic_write(&path, &encoded) {
            self.healthy = false;
            return Err(error);
        }
        self.records
            .insert(record.grant.attempt.attempt_id.as_str().to_owned(), record);
        Ok(())
    }

    fn prune_expired_tombstones(&mut self) -> Result<(), InferenceHostError> {
        let now = unix_time_ms();
        let expired = self
            .records
            .iter()
            .filter_map(|(attempt_id, record)| match record.state {
                RecordState::Acknowledged {
                    acknowledged_unix_ms,
                    ..
                }
                | RecordState::Retired {
                    retired_unix_ms: acknowledged_unix_ms,
                    ..
                } if tombstone_expired(
                    now,
                    acknowledged_unix_ms,
                    record.grant.deadline_unix_ms,
                ) =>
                {
                    Some(attempt_id.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        if expired.is_empty() {
            return Ok(());
        }
        for attempt_id in &expired {
            if let Err(error) =
                fs::remove_file(self.root.join(format!("attempt-{attempt_id}.json")))
            {
                self.healthy = false;
                tracing::warn!(
                    attempt_id,
                    error = %error,
                    "failed to prune expired Runner inference tombstone"
                );
                return Err(InferenceHostError::JournalIo);
            }
        }
        File::open(&self.root)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| {
                self.healthy = false;
                InferenceHostError::JournalIo
            })?;
        for attempt_id in expired {
            self.records.remove(&attempt_id);
        }
        Ok(())
    }
}

fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn tombstone_expired(now: u64, acknowledged_at: u64, grant_deadline: u64) -> bool {
    acknowledged_at
        .max(grant_deadline)
        .checked_add(ACK_TOMBSTONE_RETENTION_MS)
        .is_some_and(|expires_at| now >= expires_at)
}

pub(crate) fn ensure_private_directory(path: &Path) -> Result<(), InferenceHostError> {
    #[cfg(not(unix))]
    {
        let _ = path;
        return Err(InferenceHostError::UnsupportedPlatform);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .map_err(|_| InferenceHostError::JournalIo)?;
        let metadata = fs::symlink_metadata(path).map_err(|_| InferenceHostError::JournalIo)?;
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(InferenceHostError::UnsafeStorage);
        }
        if let Some(parent) = path.parent() {
            File::open(parent)
                .and_then(|parent| parent.sync_all())
                .map_err(|_| InferenceHostError::JournalIo)?;
        }
        Ok(())
    }
}

pub(crate) fn open_private(path: &Path, create: bool) -> Result<File, InferenceHostError> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(create)
        .create(create)
        .truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .map_err(|_| InferenceHostError::JournalIo)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let metadata = file.metadata().map_err(|_| InferenceHostError::JournalIo)?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o077 != 0
            || metadata.nlink() != 1
        {
            return Err(InferenceHostError::UnsafeStorage);
        }
    }
    Ok(file)
}

pub(crate) fn read_private(path: &Path, limit: usize) -> Result<Vec<u8>, InferenceHostError> {
    let file = open_private(path, false)?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| InferenceHostError::JournalIo)?;
    if bytes.len() > limit {
        return Err(InferenceHostError::TooLarge);
    }
    Ok(bytes)
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), InferenceHostError> {
    let parent = path.parent().ok_or(InferenceHostError::UnsafeStorage)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".inference-")
        .tempfile_in(parent)
        .map_err(|_| InferenceHostError::JournalIo)?;
    temporary
        .write_all(bytes)
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|_| InferenceHostError::JournalIo)?;
    temporary
        .persist(path)
        .map_err(|_| InferenceHostError::JournalIo)?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| InferenceHostError::JournalIo)
}

#[cfg(test)]
mod retention_tests {
    use super::*;

    #[test]
    fn tombstones_outlive_both_ack_and_grant_replay_horizons() {
        let day = ACK_TOMBSTONE_RETENTION_MS;
        assert!(!tombstone_expired(day - 1, 0, 0));
        assert!(tombstone_expired(day, 0, 0));
        assert!(!tombstone_expired(day + 99, 100, 0));
        assert!(tombstone_expired(day + 100, 100, 0));
        assert!(!tombstone_expired(day + 199, 100, 200));
        assert!(tombstone_expired(day + 200, 100, 200));
        assert!(!tombstone_expired(u64::MAX, u64::MAX, 0));
    }
}

#[cfg(all(test, unix))]
mod publication_tests {
    use super::*;
    use astra_turn_types::runner_inference::{
        RunnerInferenceModelName, RunnerInferenceProtocol, RunnerInferenceSource,
    };
    use std::num::NonZeroU32;

    fn definition(journal: &InferenceJournal, id: &str) -> RunnerInferenceBindingDefinition {
        RunnerInferenceBindingDefinition {
            identity: RunnerInferenceBindingIdentity {
                runner_id: RunnerInferenceId::new("test-runner").unwrap(),
                journal_id: journal.journal_id().clone(),
                binding_id: RunnerInferenceId::new(id).unwrap(),
                binding_revision: NonZeroU64::MIN,
                profile_revision: NonZeroU64::MIN,
            },
            display_name: RunnerInferenceModelName::new("Model").unwrap(),
            model_name: RunnerInferenceModelName::new("model").unwrap(),
            protocol: RunnerInferenceProtocol::OpenAiChatCompletions,
            source: RunnerInferenceSource::StaticCredential,
            supported_reasoning_efforts: vec![],
            context_window: NonZeroU32::new(1024).unwrap(),
            max_output_tokens: NonZeroU32::new(64).unwrap(),
        }
    }

    #[test]
    fn oversized_identity_never_exposes_an_unpersisted_operation() {
        let root = tempfile::tempdir().unwrap();
        let mut journal =
            InferenceJournal::open(root.path().join("journal"), "owner".into()).unwrap();
        let target = definition(&journal, "test-model");
        let base_size = journal.identity.encode().unwrap().len();
        let slot = |n| {
            let key = format!("stored-{n:06}-{}", "x".repeat(48));
            let binding = PublishedBinding {
                identity: definition(&journal, &key).identity,
                enabled: false,
            };
            (key, binding)
        };
        let (key, binding) = slot(0);
        let mut measured = journal.identity.clone();
        measured.published.insert(key, binding);
        let slot_size = measured.encode().unwrap().len() - base_size + 1;
        let count = (MAX_IDENTITY_BYTES - base_size) / slot_size;
        journal.identity.published = (0..count).map(slot).collect();
        journal.persist_identity(journal.identity.clone()).unwrap();
        for _ in 0..2 {
            assert!(matches!(
                journal.next_publication(vec![target.clone()]),
                Err(InferenceHostError::TooLarge)
            ));
            assert!(journal.identity.pending_publication.is_none());
        }
        drop(journal);
        let reopened = InferenceJournal::open(root.path().join("journal"), "owner".into()).unwrap();
        assert!(reopened.identity.pending_publication.is_none());
    }

    #[test]
    fn server_acceptance_before_restart_replays_the_exact_durable_operation() {
        let root = tempfile::tempdir().unwrap();
        let mut journal =
            InferenceJournal::open(root.path().join("journal"), "owner".into()).unwrap();
        let old = definition(&journal, "native-old");
        journal.identity.published.insert(
            "native-old".into(),
            PublishedBinding {
                identity: old.identity,
                enabled: false,
            },
        );
        let target = definition(&journal, "native-current");
        let operation = journal
            .next_publication(vec![target.clone()])
            .unwrap()
            .unwrap();
        let receipt = RunnerInferenceBindingReceipt {
            operation_id: operation.operation_id.clone(),
            publication_revision: NonZeroU64::MIN,
            identity: operation.change.identity().clone(),
        };
        drop(journal); // Server accepted the operation, but the ACK was lost.
        let mut reopened =
            InferenceJournal::open(root.path().join("journal"), "owner".into()).unwrap();
        assert_eq!(
            reopened.next_publication(vec![target]).unwrap(),
            Some(operation)
        );
        reopened.publication_ack(&receipt).unwrap();
        assert!(!reopened.identity.published.contains_key("native-old"));
        assert!(reopened.identity.pending_publication.is_none());
    }
}

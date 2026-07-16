use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use astra_server_types::edge_ws_protocol::{EdgeClientMessage, ToolInvocationIdentity};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;
use tokio::io::AsyncWriteExt;

const JOURNAL_VERSION: &str = "edge-invocation-journal-v3";
const MAX_RECORDS: usize = 1_024;
const MAX_JOURNAL_STATE_BYTES: usize = 192 * 1024 * 1024;
const MAX_WAL_BYTES: usize = 256 * 1024 * 1024;
const WAL_COMPACTION_ENTRY_THRESHOLD: usize = 4_096;
const WAL_COMPACTION_BYTE_THRESHOLD: usize = 8 * 1024 * 1024;
const MAX_RESULT_BYTES: usize = 256 * 1024;
const _: () = {
    assert!(MAX_RECORDS >= 512);
    assert!(MAX_JOURNAL_STATE_BYTES <= 256 * 1024 * 1024);
    assert!(MAX_WAL_BYTES <= 256 * 1024 * 1024);
    assert!(WAL_COMPACTION_BYTE_THRESHOLD < MAX_WAL_BYTES);
    assert!(WAL_COMPACTION_ENTRY_THRESHOLD > MAX_RECORDS);
};

#[derive(Debug, Error)]
pub(crate) enum JournalError {
    #[error("edge invocation journal I/O at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("edge invocation journal is corrupt at {path}: {detail}")]
    Corrupt { path: PathBuf, detail: String },
    #[error("edge invocation journal is full ({MAX_RECORDS} unacknowledged invocations)")]
    Full,
    #[error("edge invocation request conflicts with durable identity {request_id}")]
    IdentityConflict { request_id: String },
    #[error("edge invocation journal state would exceed {MAX_JOURNAL_STATE_BYTES} bytes")]
    TooLarge,
    #[error("edge invocation journal WAL would exceed {MAX_WAL_BYTES} bytes")]
    WalFull,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum DurableState {
    Running,
    CompletedAwaitingAck,
    OutcomeUnknownAwaitingAck,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct DurableEdgeResult {
    pub(crate) output: String,
    pub(crate) is_error: bool,
    pub(crate) duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) tool_result_fields: Option<Map<String, Value>>,
}

impl DurableEdgeResult {
    pub(crate) fn from_tool_result(result: astra_tools::ToolResult, duration_ms: u64) -> Self {
        Self {
            output: result.output,
            is_error: result.is_error,
            duration_ms,
            tool_result_fields: result.metadata,
        }
    }

    fn outcome_unknown(reason: impl Into<String>) -> Self {
        Self {
            output: reason.into(),
            is_error: true,
            duration_ms: 0,
            tool_result_fields: Some(Map::from_iter([(
                "outcome_certainty".to_string(),
                Value::String("unknown".to_string()),
            )])),
        }
    }

    pub(crate) fn not_dispatched_rejection(reason: impl Into<String>) -> Self {
        Self {
            output: reason.into(),
            is_error: true,
            duration_ms: 0,
            tool_result_fields: Some(Map::from_iter([
                (
                    "outcome_certainty".to_string(),
                    Value::String("not_dispatched".to_string()),
                ),
                ("retryable".to_string(), Value::Bool(true)),
                (
                    "error_kind".to_string(),
                    Value::String("edge_admission_capacity".to_string()),
                ),
            ])),
        }
    }

    pub(crate) fn with_journal_status(mut self, status: &EdgeInvocationJournalStatus) -> Self {
        self.tool_result_fields
            .get_or_insert_with(Map::new)
            .insert("edgeJournal".to_string(), status.as_json());
        self
    }

    pub(crate) fn client_message(
        &self,
        request_id: String,
        identity: ToolInvocationIdentity,
        delivery_generation: u64,
    ) -> EdgeClientMessage {
        EdgeClientMessage::ToolResult {
            request_id,
            identity,
            delivery_generation,
            output: self.output.clone(),
            is_error: self.is_error,
            duration_ms: Some(self.duration_ms),
            tool_result_fields: self.tool_result_fields.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DurableInvocationRecord {
    identity: ToolInvocationIdentity,
    /// The latest server delivery incarnation. Results and ACKs use this
    /// generation so reconnect can supersede an older socket safely.
    delivery_generation: u64,
    /// The incarnation that crossed the side-effect boundary. It is stable
    /// across redelivery and is absent only for a durable not-dispatched
    /// admission rejection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    execution_generation: Option<u64>,
    tool: String,
    canonical_arguments_hash: String,
    state: DurableState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    result: Option<DurableEdgeResult>,
}

impl DurableInvocationRecord {
    fn matches(&self, identity: &ToolInvocationIdentity, tool: &str, args: &Value) -> bool {
        self.identity == *identity
            && self.tool == tool
            && self.canonical_arguments_hash
                == astra_turn_types::canonical_public_arguments_hash(args)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct JournalFile {
    contract_version: String,
    last_sequence: u64,
    records: BTreeMap<String, DurableInvocationRecord>,
}

impl Default for JournalFile {
    fn default() -> Self {
        Self {
            contract_version: JOURNAL_VERSION.to_string(),
            last_sequence: 0,
            records: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct JournalMutation {
    contract_version: String,
    sequence: u64,
    request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    record: Option<DurableInvocationRecord>,
}

pub(crate) enum PrepareOutcome {
    Execute,
    Active,
    Replay(DurableEdgeResult),
}

pub(crate) struct PendingResult {
    pub(crate) request_id: String,
    pub(crate) identity: ToolInvocationIdentity,
    pub(crate) delivery_generation: u64,
    pub(crate) result: DurableEdgeResult,
}

pub(crate) struct EdgeInvocationJournal {
    path: PathBuf,
    wal_path: PathBuf,
    state: JournalFile,
    state_bytes: usize,
    wal: tokio::fs::File,
    wal_entries: usize,
    wal_bytes: usize,
    _lock: std::fs::File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EdgeInvocationJournalStatus {
    pub(crate) records: usize,
    pub(crate) running: usize,
    pub(crate) awaiting_ack: usize,
    pub(crate) state_bytes: usize,
    pub(crate) wal_entries: usize,
    pub(crate) wal_bytes: usize,
}

impl EdgeInvocationJournalStatus {
    fn as_json(self) -> Value {
        serde_json::json!({
            "contractVersion": JOURNAL_VERSION,
            "records": self.records,
            "running": self.running,
            "awaitingAck": self.awaiting_ack,
            "recordCapacity": MAX_RECORDS,
            "stateBytes": self.state_bytes,
            "stateByteCapacity": MAX_JOURNAL_STATE_BYTES,
            "walEntries": self.wal_entries,
            "walBytes": self.wal_bytes,
            "walByteCapacity": MAX_WAL_BYTES,
        })
    }
}

impl EdgeInvocationJournal {
    pub(crate) async fn open(path: PathBuf) -> Result<Self, JournalError> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|source| JournalError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        let lock_path = path.with_extension("json.lock");
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|source| JournalError::Io {
                path: lock_path.clone(),
                source,
            })?;
        fs2::FileExt::try_lock_exclusive(&lock).map_err(|source| JournalError::Io {
            path: lock_path,
            source,
        })?;
        let mut state = match tokio::fs::read(&path).await {
            Ok(bytes) => serde_json::from_slice::<JournalFile>(&bytes).map_err(|error| {
                JournalError::Corrupt {
                    path: path.clone(),
                    detail: error.to_string(),
                }
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => JournalFile::default(),
            Err(source) => {
                return Err(JournalError::Io {
                    path: path.clone(),
                    source,
                });
            }
        };
        validate_file(&path, &state)?;
        let wal_path = path.with_extension("json.wal");
        let wal_bytes = match tokio::fs::read(&wal_path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(source) => {
                return Err(JournalError::Io {
                    path: wal_path.clone(),
                    source,
                });
            }
        };
        if wal_bytes.len() > MAX_WAL_BYTES {
            return Err(JournalError::WalFull);
        }
        let (wal_entries, valid_wal_bytes) = replay_wal(&wal_path, &wal_bytes, &mut state)?;
        validate_file(&path, &state)?;
        let wal_existed =
            tokio::fs::try_exists(&wal_path)
                .await
                .map_err(|source| JournalError::Io {
                    path: wal_path.clone(),
                    source,
                })?;
        let wal = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&wal_path)
            .await
            .map_err(|source| JournalError::Io {
                path: wal_path.clone(),
                source,
            })?;
        if valid_wal_bytes != wal_bytes.len() {
            wal.set_len(valid_wal_bytes as u64)
                .await
                .map_err(|source| JournalError::Io {
                    path: wal_path.clone(),
                    source,
                })?;
            wal.sync_data().await.map_err(|source| JournalError::Io {
                path: wal_path.clone(),
                source,
            })?;
        }
        if !wal_existed {
            sync_directory(parent).await?;
        }
        let state_bytes = encoded_state_len(&path, &state)?;
        let mut journal = Self {
            path,
            wal_path,
            state,
            state_bytes,
            wal,
            wal_entries,
            wal_bytes: valid_wal_bytes,
            _lock: lock,
        };

        let running = journal
            .state
            .records
            .iter()
            .filter(|(_, record)| record.state == DurableState::Running)
            .map(|(request_id, record)| {
                let mut record = record.clone();
                record.state = DurableState::OutcomeUnknownAwaitingAck;
                record.result = Some(DurableEdgeResult::outcome_unknown(
                    "Edge process restarted after dispatch but before durable completion; the tool outcome is unknown and was not re-executed",
                ));
                (request_id.clone(), record)
            })
            .collect::<Vec<_>>();
        for (request_id, record) in running {
            journal.commit_record(request_id, Some(record)).await?;
        }
        if journal.should_compact() {
            journal.compact().await?;
        }
        Ok(journal)
    }

    pub(crate) fn status(&self) -> EdgeInvocationJournalStatus {
        let mut running = 0;
        let mut awaiting_ack = 0;
        for record in self.state.records.values() {
            match record.state {
                DurableState::Running => running += 1,
                DurableState::CompletedAwaitingAck | DurableState::OutcomeUnknownAwaitingAck => {
                    awaiting_ack += 1;
                }
            }
        }
        EdgeInvocationJournalStatus {
            records: self.state.records.len(),
            running,
            awaiting_ack,
            state_bytes: self.state_bytes,
            wal_entries: self.wal_entries,
            wal_bytes: self.wal_bytes,
        }
    }

    pub(crate) async fn prepare(
        &mut self,
        request_id: &str,
        identity: &ToolInvocationIdentity,
        delivery_generation: u64,
        tool: &str,
        args: &Value,
        execution_capacity_available: bool,
    ) -> Result<PrepareOutcome, JournalError> {
        if request_id != identity.storage_key() {
            return Err(JournalError::IdentityConflict {
                request_id: request_id.to_string(),
            });
        }
        if let Some(record) = self.state.records.get(request_id).cloned() {
            if !record.matches(identity, tool, args) {
                return Err(JournalError::IdentityConflict {
                    request_id: request_id.to_string(),
                });
            }
            let mut updated = record.clone();
            updated.delivery_generation = delivery_generation;
            self.commit_record(request_id.to_string(), Some(updated))
                .await?;
            let outcome = match record.state {
                DurableState::Running => PrepareOutcome::Active,
                DurableState::CompletedAwaitingAck | DurableState::OutcomeUnknownAwaitingAck => {
                    PrepareOutcome::Replay(record.result.clone().ok_or_else(|| {
                        JournalError::Corrupt {
                            path: self.path.clone(),
                            detail: format!("terminal record {request_id} has no result"),
                        }
                    })?)
                }
            };
            return Ok(outcome);
        }
        if self.state.records.len() >= MAX_RECORDS {
            return Err(JournalError::Full);
        }
        let record = DurableInvocationRecord {
            identity: identity.clone(),
            delivery_generation,
            execution_generation: execution_capacity_available.then_some(delivery_generation),
            tool: tool.to_string(),
            canonical_arguments_hash: astra_turn_types::canonical_public_arguments_hash(args),
            state: if execution_capacity_available {
                DurableState::Running
            } else {
                DurableState::CompletedAwaitingAck
            },
            result: (!execution_capacity_available).then(|| {
                DurableEdgeResult::not_dispatched_rejection(
                    "Edge execution capacity is temporarily saturated before dispatch",
                )
            }),
        };
        let rejection = record.result.clone();
        self.commit_record(request_id.to_string(), Some(record))
            .await?;
        if let Some(rejection) = rejection {
            Ok(PrepareOutcome::Replay(rejection))
        } else {
            Ok(PrepareOutcome::Execute)
        }
    }

    pub(crate) async fn complete(
        &mut self,
        request_id: &str,
        delivery_generation: u64,
        result: DurableEdgeResult,
    ) -> Result<DurableEdgeResult, JournalError> {
        let serialized_result =
            serde_json::to_vec(&result).map_err(|error| JournalError::Corrupt {
                path: self.path.clone(),
                detail: format!("result serialization failed: {error}"),
            })?;
        let (state, durable_result) = if serialized_result.len() > MAX_RESULT_BYTES {
            (
                DurableState::OutcomeUnknownAwaitingAck,
                DurableEdgeResult::outcome_unknown(format!(
                    "Edge tool completed but its {} byte result exceeded the durable {} byte boundary; outcome evidence is unavailable",
                    serialized_result.len(),
                    MAX_RESULT_BYTES
                )),
            )
        } else {
            (DurableState::CompletedAwaitingAck, result)
        };
        let mut record = self
            .execution_record(request_id, delivery_generation)?
            .clone();
        if record.state != DurableState::Running {
            return Err(JournalError::Corrupt {
                path: self.path.clone(),
                detail: format!("record {request_id} is not running"),
            });
        }
        record.state = state;
        record.result = Some(durable_result.clone());
        self.commit_record(request_id.to_string(), Some(record))
            .await?;
        Ok(durable_result)
    }

    pub(crate) async fn acknowledge(
        &mut self,
        request_id: &str,
        delivery_generation: u64,
    ) -> Result<bool, JournalError> {
        let Some(record) = self.state.records.get(request_id) else {
            return Ok(false);
        };
        if record.delivery_generation != delivery_generation
            || !matches!(
                record.state,
                DurableState::CompletedAwaitingAck | DurableState::OutcomeUnknownAwaitingAck
            )
        {
            return Ok(false);
        }
        self.commit_record(request_id.to_string(), None).await?;
        if self.should_compact() {
            self.compact().await?;
        }
        Ok(true)
    }

    pub(crate) fn running_execution_generation(
        &self,
        request_id: &str,
        delivery_generation: u64,
    ) -> Option<u64> {
        let record = self.state.records.get(request_id)?;
        (record.state == DurableState::Running && record.delivery_generation == delivery_generation)
            .then_some(record.execution_generation)
            .flatten()
    }

    pub(crate) fn pending_results(&self) -> Result<Vec<PendingResult>, JournalError> {
        self.state
            .records
            .iter()
            .filter(|(_, record)| {
                matches!(
                    record.state,
                    DurableState::CompletedAwaitingAck | DurableState::OutcomeUnknownAwaitingAck
                )
            })
            .map(|(request_id, record)| {
                Ok(PendingResult {
                    request_id: request_id.clone(),
                    identity: record.identity.clone(),
                    delivery_generation: record.delivery_generation,
                    result: record.result.clone().ok_or_else(|| JournalError::Corrupt {
                        path: self.path.clone(),
                        detail: format!("terminal record {request_id} has no result"),
                    })?,
                })
            })
            .collect()
    }

    fn execution_record(
        &self,
        request_id: &str,
        execution_generation: u64,
    ) -> Result<&DurableInvocationRecord, JournalError> {
        let record = self
            .state
            .records
            .get(request_id)
            .ok_or_else(|| JournalError::Corrupt {
                path: self.path.clone(),
                detail: format!("record {request_id} does not exist"),
            })?;
        if record.execution_generation != Some(execution_generation) {
            return Err(JournalError::IdentityConflict {
                request_id: request_id.to_string(),
            });
        }
        Ok(record)
    }

    async fn commit_record(
        &mut self,
        request_id: String,
        record: Option<DurableInvocationRecord>,
    ) -> Result<(), JournalError> {
        let old_bytes = self
            .state
            .records
            .get(&request_id)
            .map(|record| encoded_record_entry_len(&request_id, record))
            .transpose()?
            .unwrap_or_default();
        let new_bytes = record
            .as_ref()
            .map(|record| encoded_record_entry_len(&request_id, record))
            .transpose()?
            .unwrap_or_default();
        let projected_bytes = self
            .state_bytes
            .checked_sub(old_bytes)
            .and_then(|value| value.checked_add(new_bytes))
            .ok_or(JournalError::TooLarge)?;
        if projected_bytes > MAX_JOURNAL_STATE_BYTES {
            return Err(JournalError::TooLarge);
        }
        let sequence =
            self.state
                .last_sequence
                .checked_add(1)
                .ok_or_else(|| JournalError::Corrupt {
                    path: self.path.clone(),
                    detail: "journal sequence overflow".to_string(),
                })?;
        let mutation = JournalMutation {
            contract_version: JOURNAL_VERSION.to_string(),
            sequence,
            request_id: request_id.clone(),
            record: record.clone(),
        };
        let mut line = serde_json::to_vec(&mutation).map_err(|error| JournalError::Corrupt {
            path: self.wal_path.clone(),
            detail: format!("WAL serialization failed: {error}"),
        })?;
        line.push(b'\n');
        if self.wal_bytes.saturating_add(line.len()) > MAX_WAL_BYTES {
            self.compact().await?;
        }
        if self.wal_bytes.saturating_add(line.len()) > MAX_WAL_BYTES {
            return Err(JournalError::WalFull);
        }
        self.wal
            .write_all(&line)
            .await
            .map_err(|source| JournalError::Io {
                path: self.wal_path.clone(),
                source,
            })?;
        self.wal
            .sync_data()
            .await
            .map_err(|source| JournalError::Io {
                path: self.wal_path.clone(),
                source,
            })?;
        match record {
            Some(record) => {
                self.state.records.insert(request_id, record);
            }
            None => {
                self.state.records.remove(&request_id);
            }
        }
        self.state.last_sequence = sequence;
        self.state_bytes = projected_bytes;
        self.wal_entries = self.wal_entries.saturating_add(1);
        self.wal_bytes = self.wal_bytes.saturating_add(line.len());
        Ok(())
    }

    fn should_compact(&self) -> bool {
        self.wal_entries >= WAL_COMPACTION_ENTRY_THRESHOLD
            || self.wal_bytes >= WAL_COMPACTION_BYTE_THRESHOLD
    }

    async fn compact(&mut self) -> Result<(), JournalError> {
        let bytes = serde_json::to_vec(&self.state).map_err(|error| JournalError::Corrupt {
            path: self.path.clone(),
            detail: format!("snapshot serialization failed: {error}"),
        })?;
        if bytes.len() > MAX_JOURNAL_STATE_BYTES {
            return Err(JournalError::TooLarge);
        }
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        let temp = self.path.with_extension("json.tmp");
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temp)
            .await
            .map_err(|source| JournalError::Io {
                path: temp.clone(),
                source,
            })?;
        file.write_all(&bytes)
            .await
            .map_err(|source| JournalError::Io {
                path: temp.clone(),
                source,
            })?;
        file.sync_all().await.map_err(|source| JournalError::Io {
            path: temp.clone(),
            source,
        })?;
        drop(file);
        tokio::fs::rename(&temp, &self.path)
            .await
            .map_err(|source| JournalError::Io {
                path: self.path.clone(),
                source,
            })?;
        sync_directory(parent).await?;
        self.wal
            .set_len(0)
            .await
            .map_err(|source| JournalError::Io {
                path: self.wal_path.clone(),
                source,
            })?;
        self.wal
            .sync_data()
            .await
            .map_err(|source| JournalError::Io {
                path: self.wal_path.clone(),
                source,
            })?;
        self.state_bytes = bytes.len();
        self.wal_entries = 0;
        self.wal_bytes = 0;
        Ok(())
    }
}

fn encoded_state_len(path: &Path, state: &JournalFile) -> Result<usize, JournalError> {
    serde_json::to_vec(state)
        .map(|bytes| bytes.len())
        .map_err(|error| JournalError::Corrupt {
            path: path.to_path_buf(),
            detail: format!("snapshot serialization failed: {error}"),
        })
}

fn encoded_record_entry_len(
    request_id: &str,
    record: &DurableInvocationRecord,
) -> Result<usize, JournalError> {
    let key_bytes = serde_json::to_vec(request_id).map_err(|error| JournalError::Corrupt {
        path: PathBuf::from("<memory>"),
        detail: format!("record key serialization failed: {error}"),
    })?;
    let record_bytes = serde_json::to_vec(record).map_err(|error| JournalError::Corrupt {
        path: PathBuf::from("<memory>"),
        detail: format!("record serialization failed: {error}"),
    })?;
    Ok(key_bytes.len() + 1 + record_bytes.len() + 1)
}

fn replay_wal(
    path: &Path,
    bytes: &[u8],
    state: &mut JournalFile,
) -> Result<(usize, usize), JournalError> {
    let mut entries = 0;
    let mut offset = 0;
    while let Some(relative_end) = bytes[offset..].iter().position(|byte| *byte == b'\n') {
        let end = offset + relative_end;
        if end == offset {
            return Err(JournalError::Corrupt {
                path: path.to_path_buf(),
                detail: format!("empty WAL entry at byte {offset}"),
            });
        }
        let mutation: JournalMutation =
            serde_json::from_slice(&bytes[offset..end]).map_err(|error| JournalError::Corrupt {
                path: path.to_path_buf(),
                detail: format!("invalid WAL entry at byte {offset}: {error}"),
            })?;
        if mutation.contract_version != JOURNAL_VERSION {
            return Err(JournalError::Corrupt {
                path: path.to_path_buf(),
                detail: format!(
                    "unsupported WAL contract version {}",
                    mutation.contract_version
                ),
            });
        }
        if mutation.sequence > state.last_sequence {
            let expected =
                state
                    .last_sequence
                    .checked_add(1)
                    .ok_or_else(|| JournalError::Corrupt {
                        path: path.to_path_buf(),
                        detail: "journal sequence overflow".to_string(),
                    })?;
            if mutation.sequence != expected {
                return Err(JournalError::Corrupt {
                    path: path.to_path_buf(),
                    detail: format!(
                        "non-contiguous WAL sequence: expected {expected}, got {}",
                        mutation.sequence
                    ),
                });
            }
            if let Some(record) = mutation.record {
                validate_record(path, &mutation.request_id, &record)?;
                state.records.insert(mutation.request_id, record);
            } else {
                state.records.remove(&mutation.request_id);
            }
            state.last_sequence = mutation.sequence;
        }
        entries += 1;
        offset = end + 1;
    }
    Ok((entries, offset))
}

async fn sync_directory(path: &Path) -> Result<(), JournalError> {
    let directory = tokio::fs::File::open(path)
        .await
        .map_err(|source| JournalError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    directory
        .sync_all()
        .await
        .map_err(|source| JournalError::Io {
            path: path.to_path_buf(),
            source,
        })
}

fn validate_file(path: &Path, state: &JournalFile) -> Result<(), JournalError> {
    if state.contract_version != JOURNAL_VERSION {
        return Err(JournalError::Corrupt {
            path: path.to_path_buf(),
            detail: format!("unsupported contract version {}", state.contract_version),
        });
    }
    if state.records.len() > MAX_RECORDS {
        return Err(JournalError::Corrupt {
            path: path.to_path_buf(),
            detail: format!("record count {} exceeds {MAX_RECORDS}", state.records.len()),
        });
    }
    if encoded_state_len(path, state)? > MAX_JOURNAL_STATE_BYTES {
        return Err(JournalError::TooLarge);
    }
    for (request_id, record) in &state.records {
        validate_record(path, request_id, record)?;
    }
    Ok(())
}

fn validate_record(
    path: &Path,
    request_id: &str,
    record: &DurableInvocationRecord,
) -> Result<(), JournalError> {
    if request_id != record.identity.storage_key() {
        return Err(JournalError::Corrupt {
            path: path.to_path_buf(),
            detail: format!("record {request_id} does not match its logical identity"),
        });
    }
    let terminal = matches!(
        record.state,
        DurableState::CompletedAwaitingAck | DurableState::OutcomeUnknownAwaitingAck
    );
    if terminal != record.result.is_some() {
        return Err(JournalError::Corrupt {
            path: path.to_path_buf(),
            detail: format!("record {request_id} has inconsistent terminal evidence"),
        });
    }
    if matches!(
        record.state,
        DurableState::Running | DurableState::OutcomeUnknownAwaitingAck
    ) && record.execution_generation.is_none()
    {
        return Err(JournalError::Corrupt {
            path: path.to_path_buf(),
            detail: format!("record {request_id} crossed dispatch without an execution generation"),
        });
    }
    if let Some(result) = &record.result {
        let result_bytes = serde_json::to_vec(result).map_err(|error| JournalError::Corrupt {
            path: path.to_path_buf(),
            detail: format!("record {request_id} result cannot be serialized: {error}"),
        })?;
        if result_bytes.len() > MAX_RESULT_BYTES {
            return Err(JournalError::Corrupt {
                path: path.to_path_buf(),
                detail: format!(
                    "record {request_id} result is {} bytes; maximum is {MAX_RESULT_BYTES}",
                    result_bytes.len()
                ),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn identity(id: &str) -> ToolInvocationIdentity {
        ToolInvocationIdentity::new("user", "session", "run", "turn", id).unwrap()
    }

    #[tokio::test]
    async fn completed_result_survives_restart_until_exact_ack() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.json");
        let identity = identity("call-1");
        let request_id = identity.storage_key();
        let mut journal = EdgeInvocationJournal::open(path.clone()).await.unwrap();
        assert!(matches!(
            journal
                .prepare(
                    &request_id,
                    &identity,
                    7,
                    "read_file",
                    &json!({"path":"a"}),
                    true,
                )
                .await
                .unwrap(),
            PrepareOutcome::Execute
        ));
        journal
            .complete(
                &request_id,
                7,
                DurableEdgeResult {
                    output: "body".into(),
                    is_error: false,
                    duration_ms: 3,
                    tool_result_fields: None,
                },
            )
            .await
            .unwrap();
        drop(journal);

        let mut restored = EdgeInvocationJournal::open(path).await.unwrap();
        assert_eq!(restored.pending_results().unwrap().len(), 1);
        assert!(!restored.acknowledge(&request_id, 6).await.unwrap());
        assert!(restored.acknowledge(&request_id, 7).await.unwrap());
        assert!(restored.pending_results().unwrap().is_empty());
    }

    #[tokio::test]
    async fn redelivery_updates_ack_generation_without_stealing_execution_generation() {
        let dir = tempfile::tempdir().unwrap();
        let identity = identity("call-redelivered");
        let request_id = identity.storage_key();
        let args = json!({"command":"effect"});
        let mut journal = EdgeInvocationJournal::open(dir.path().join("journal.json"))
            .await
            .unwrap();
        assert!(matches!(
            journal
                .prepare(&request_id, &identity, 4, "bash", &args, true)
                .await
                .unwrap(),
            PrepareOutcome::Execute
        ));
        assert!(matches!(
            journal
                .prepare(&request_id, &identity, 5, "bash", &args, true)
                .await
                .unwrap(),
            PrepareOutcome::Active
        ));

        journal
            .complete(
                &request_id,
                4,
                DurableEdgeResult {
                    output: "done".into(),
                    is_error: false,
                    duration_ms: 1,
                    tool_result_fields: None,
                },
            )
            .await
            .unwrap();
        let pending = journal.pending_results().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].delivery_generation, 5);
        assert!(!journal.acknowledge(&request_id, 4).await.unwrap());
        assert!(journal.acknowledge(&request_id, 5).await.unwrap());
    }

    #[tokio::test]
    async fn restart_converts_running_to_outcome_unknown_without_redispatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.json");
        let identity = identity("call-2");
        let request_id = identity.storage_key();
        let args = json!({"command":"effect"});
        let mut journal = EdgeInvocationJournal::open(path.clone()).await.unwrap();
        journal
            .prepare(&request_id, &identity, 4, "bash", &args, true)
            .await
            .unwrap();
        drop(journal);

        let mut restored = EdgeInvocationJournal::open(path).await.unwrap();
        let pending = restored.pending_results().unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].result.is_error);
        assert_eq!(
            pending[0].result.tool_result_fields.as_ref().unwrap()["outcome_certainty"],
            "unknown"
        );
        assert!(matches!(
            restored
                .prepare(&request_id, &identity, 5, "bash", &args, true)
                .await
                .unwrap(),
            PrepareOutcome::Replay(_)
        ));
    }

    #[tokio::test]
    async fn saturated_admission_is_durable_not_dispatched_and_replays() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.json");
        let identity = identity("call-saturated");
        let request_id = identity.storage_key();
        let args = json!({"command":"effect"});
        let mut journal = EdgeInvocationJournal::open(path.clone()).await.unwrap();
        assert!(matches!(
            journal
                .prepare(&request_id, &identity, 4, "bash", &args, false)
                .await
                .unwrap(),
            PrepareOutcome::Replay(_)
        ));
        drop(journal);

        let mut restored = EdgeInvocationJournal::open(path).await.unwrap();
        let pending = restored.pending_results().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].result.tool_result_fields.as_ref().unwrap()["outcome_certainty"],
            "not_dispatched"
        );
        assert!(matches!(
            restored
                .prepare(&request_id, &identity, 5, "bash", &args, true)
                .await
                .unwrap(),
            PrepareOutcome::Replay(_)
        ));
    }

    #[tokio::test]
    async fn incomplete_wal_tail_is_discarded_without_losing_durable_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.json");
        let wal_path = path.with_extension("json.wal");
        let identity = identity("call-partial-wal");
        let request_id = identity.storage_key();
        let args = json!({"path":"a"});
        let mut journal = EdgeInvocationJournal::open(path.clone()).await.unwrap();
        journal
            .prepare(&request_id, &identity, 1, "read_file", &args, false)
            .await
            .unwrap();
        drop(journal);
        let valid_len = tokio::fs::metadata(&wal_path).await.unwrap().len();
        let mut wal = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&wal_path)
            .await
            .unwrap();
        wal.write_all(b"{\"contract_version\":").await.unwrap();
        wal.sync_data().await.unwrap();
        drop(wal);

        let mut restored = EdgeInvocationJournal::open(path).await.unwrap();
        assert_eq!(
            tokio::fs::metadata(&wal_path).await.unwrap().len(),
            valid_len
        );
        assert!(matches!(
            restored
                .prepare(&request_id, &identity, 2, "read_file", &args, true)
                .await
                .unwrap(),
            PrepareOutcome::Replay(_)
        ));
    }

    #[tokio::test]
    async fn snapshot_compaction_preserves_state_and_bounds_the_wal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.json");
        let wal_path = path.with_extension("json.wal");
        let identity = identity("call-compact");
        let request_id = identity.storage_key();
        let mut journal = EdgeInvocationJournal::open(path.clone()).await.unwrap();
        journal
            .prepare(
                &request_id,
                &identity,
                1,
                "read_file",
                &json!({"path":"a"}),
                true,
            )
            .await
            .unwrap();
        journal
            .complete(
                &request_id,
                1,
                DurableEdgeResult {
                    output: "body".into(),
                    is_error: false,
                    duration_ms: 3,
                    tool_result_fields: None,
                },
            )
            .await
            .unwrap();
        assert!(tokio::fs::metadata(&wal_path).await.unwrap().len() > 0);
        journal.compact().await.unwrap();
        assert_eq!(tokio::fs::metadata(&wal_path).await.unwrap().len(), 0);
        drop(journal);

        let restored = EdgeInvocationJournal::open(path).await.unwrap();
        assert_eq!(restored.pending_results().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn identity_reuse_with_changed_arguments_fails_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let identity = identity("call-3");
        let request_id = identity.storage_key();
        let mut journal = EdgeInvocationJournal::open(dir.path().join("journal.json"))
            .await
            .unwrap();
        journal
            .prepare(
                &request_id,
                &identity,
                1,
                "bash",
                &json!({"command":"a"}),
                true,
            )
            .await
            .unwrap();
        let error = journal
            .prepare(
                &request_id,
                &identity,
                2,
                "bash",
                &json!({"command":"b"}),
                true,
            )
            .await
            .err()
            .unwrap();
        assert!(matches!(error, JournalError::IdentityConflict { .. }));
    }

    #[tokio::test]
    async fn corrupt_journal_never_silently_resets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.json");
        tokio::fs::write(&path, b"not-json").await.unwrap();
        assert!(matches!(
            EdgeInvocationJournal::open(path).await.err().unwrap(),
            JournalError::Corrupt { .. }
        ));
    }

    #[tokio::test]
    async fn noncanonical_journal_version_is_rejected_explicitly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.json");
        tokio::fs::write(
            &path,
            br#"{"contract_version":"edge-invocation-journal-v2","last_sequence":0,"records":{}}"#,
        )
        .await
        .unwrap();
        let error = EdgeInvocationJournal::open(path).await.err().unwrap();
        assert!(matches!(error, JournalError::Corrupt { .. }));
        assert!(error.to_string().contains("unsupported contract version"));
    }

    #[tokio::test]
    async fn second_process_cannot_open_the_same_journal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.json");
        let first = EdgeInvocationJournal::open(path.clone()).await.unwrap();
        assert!(matches!(
            EdgeInvocationJournal::open(path.clone())
                .await
                .err()
                .unwrap(),
            JournalError::Io { .. }
        ));
        drop(first);
        EdgeInvocationJournal::open(path).await.unwrap();
    }

    #[test]
    fn admission_rejection_carries_bounded_journal_capacity_evidence() {
        let status = EdgeInvocationJournalStatus {
            records: 1_024,
            running: 7,
            awaiting_ack: 1_014,
            state_bytes: 42,
            wal_entries: 88,
            wal_bytes: 99,
        };
        let result =
            DurableEdgeResult::not_dispatched_rejection("saturated").with_journal_status(&status);
        let journal = &result.tool_result_fields.unwrap()["edgeJournal"];
        assert_eq!(journal["records"], 1_024);
        assert_eq!(journal["running"], 7);
        assert_eq!(journal["awaitingAck"], 1_014);
        assert_eq!(journal["recordCapacity"], MAX_RECORDS);
        assert_eq!(journal["walByteCapacity"], MAX_WAL_BYTES);
    }
}

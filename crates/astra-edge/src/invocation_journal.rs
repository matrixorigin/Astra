use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use astra_server_types::edge_ws_protocol::{EdgeClientMessage, ToolInvocationIdentity};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

const JOURNAL_VERSION: &str = "edge-invocation-journal-v1";
const MAX_RECORDS: usize = 64;
const MAX_JOURNAL_BYTES: usize = 20 * 1024 * 1024;
const MAX_RESULT_BYTES: usize = 256 * 1024;

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
    #[error("edge invocation journal would exceed {MAX_JOURNAL_BYTES} bytes")]
    TooLarge,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum DurableState {
    Prepared,
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
    delivery_generation: u64,
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
    records: BTreeMap<String, DurableInvocationRecord>,
}

impl Default for JournalFile {
    fn default() -> Self {
        Self {
            contract_version: JOURNAL_VERSION.to_string(),
            records: BTreeMap::new(),
        }
    }
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
    state: JournalFile,
    _lock: std::fs::File,
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

        let mut recovered = false;
        for record in state.records.values_mut() {
            if matches!(record.state, DurableState::Prepared | DurableState::Running) {
                record.state = DurableState::OutcomeUnknownAwaitingAck;
                record.result = Some(DurableEdgeResult::outcome_unknown(
                    "Edge process restarted after dispatch but before durable completion; the tool outcome is unknown and was not re-executed",
                ));
                recovered = true;
            }
        }
        let journal = Self {
            path,
            state,
            _lock: lock,
        };
        if recovered {
            journal.persist().await?;
        }
        Ok(journal)
    }

    pub(crate) async fn prepare(
        &mut self,
        request_id: &str,
        identity: &ToolInvocationIdentity,
        delivery_generation: u64,
        tool: &str,
        args: &Value,
    ) -> Result<PrepareOutcome, JournalError> {
        if request_id != identity.storage_key() {
            return Err(JournalError::IdentityConflict {
                request_id: request_id.to_string(),
            });
        }
        if let Some(record) = self.state.records.get_mut(request_id) {
            if !record.matches(identity, tool, args) {
                return Err(JournalError::IdentityConflict {
                    request_id: request_id.to_string(),
                });
            }
            record.delivery_generation = delivery_generation;
            let outcome = match record.state {
                DurableState::Prepared | DurableState::Running => PrepareOutcome::Active,
                DurableState::CompletedAwaitingAck | DurableState::OutcomeUnknownAwaitingAck => {
                    PrepareOutcome::Replay(record.result.clone().ok_or_else(|| {
                        JournalError::Corrupt {
                            path: self.path.clone(),
                            detail: format!("terminal record {request_id} has no result"),
                        }
                    })?)
                }
            };
            self.persist().await?;
            return Ok(outcome);
        }
        if self.state.records.len() >= MAX_RECORDS {
            return Err(JournalError::Full);
        }
        self.state.records.insert(
            request_id.to_string(),
            DurableInvocationRecord {
                identity: identity.clone(),
                delivery_generation,
                tool: tool.to_string(),
                canonical_arguments_hash: astra_turn_types::canonical_public_arguments_hash(args),
                state: DurableState::Prepared,
                result: None,
            },
        );
        self.persist().await?;
        Ok(PrepareOutcome::Execute)
    }

    pub(crate) async fn mark_running(
        &mut self,
        request_id: &str,
        delivery_generation: u64,
    ) -> Result<(), JournalError> {
        let record = self.record_mut(request_id, delivery_generation)?;
        if record.state != DurableState::Prepared {
            return Err(JournalError::Corrupt {
                path: self.path.clone(),
                detail: format!("record {request_id} is not prepared"),
            });
        }
        record.state = DurableState::Running;
        self.persist().await
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
        let record = self.record_mut(request_id, delivery_generation)?;
        if record.state != DurableState::Running {
            return Err(JournalError::Corrupt {
                path: self.path.clone(),
                detail: format!("record {request_id} is not running"),
            });
        }
        record.state = state;
        record.result = Some(durable_result.clone());
        self.persist().await?;
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
        self.state.records.remove(request_id);
        self.persist().await?;
        Ok(true)
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

    fn record_mut(
        &mut self,
        request_id: &str,
        delivery_generation: u64,
    ) -> Result<&mut DurableInvocationRecord, JournalError> {
        let record =
            self.state
                .records
                .get_mut(request_id)
                .ok_or_else(|| JournalError::Corrupt {
                    path: self.path.clone(),
                    detail: format!("record {request_id} does not exist"),
                })?;
        if record.delivery_generation != delivery_generation {
            return Err(JournalError::IdentityConflict {
                request_id: request_id.to_string(),
            });
        }
        Ok(record)
    }

    async fn persist(&self) -> Result<(), JournalError> {
        let bytes = serde_json::to_vec(&self.state).map_err(|error| JournalError::Corrupt {
            path: self.path.clone(),
            detail: format!("serialization failed: {error}"),
        })?;
        if bytes.len() > MAX_JOURNAL_BYTES {
            return Err(JournalError::TooLarge);
        }
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|source| JournalError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        let temp = self.path.with_extension("json.tmp");
        let mut options = tokio::fs::OpenOptions::new();
        options.create(true).truncate(true).write(true);
        let mut file = options
            .open(&temp)
            .await
            .map_err(|source| JournalError::Io {
                path: temp.clone(),
                source,
            })?;
        use tokio::io::AsyncWriteExt;
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
        let directory = tokio::fs::File::open(parent)
            .await
            .map_err(|source| JournalError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        directory
            .sync_all()
            .await
            .map_err(|source| JournalError::Io {
                path: parent.to_path_buf(),
                source,
            })
    }
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
    for (request_id, record) in &state.records {
        if *request_id != record.identity.storage_key() {
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
                .prepare(&request_id, &identity, 7, "read_file", &json!({"path":"a"}))
                .await
                .unwrap(),
            PrepareOutcome::Execute
        ));
        journal.mark_running(&request_id, 7).await.unwrap();
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
    async fn restart_converts_running_to_outcome_unknown_without_redispatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.json");
        let identity = identity("call-2");
        let request_id = identity.storage_key();
        let args = json!({"command":"effect"});
        let mut journal = EdgeInvocationJournal::open(path.clone()).await.unwrap();
        journal
            .prepare(&request_id, &identity, 4, "bash", &args)
            .await
            .unwrap();
        journal.mark_running(&request_id, 4).await.unwrap();
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
                .prepare(&request_id, &identity, 5, "bash", &args)
                .await
                .unwrap(),
            PrepareOutcome::Replay(_)
        ));
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
            .prepare(&request_id, &identity, 1, "bash", &json!({"command":"a"}))
            .await
            .unwrap();
        let error = journal
            .prepare(&request_id, &identity, 2, "bash", &json!({"command":"b"}))
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
}

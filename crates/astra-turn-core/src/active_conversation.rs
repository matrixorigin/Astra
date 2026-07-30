use std::sync::Arc;

use astra_turn_types::{
    CONVERSATION_COMMIT_SCHEMA_VERSION, CONVERSATION_PROJECTION_SCHEMA_VERSION,
    ConversationCommitV1, ConversationDeltaV1, ConversationReplaceReason,
    DEFAULT_CONVERSATION_BRANCH_ID, SESSION_CURSOR_SCHEMA_VERSION, SessionCursorV1,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

const ROOT_HASH_DOMAIN: &[u8] = b"astra.canonical-conversation.v1\0";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveConversationSource {
    Live,
    Journal,
    CslProjection,
    Checkpoint,
    LegacyDisplayProjection,
}

impl ActiveConversationSource {
    fn is_durable_canonical(self) -> bool {
        matches!(self, Self::Live | Self::Journal)
    }
}

#[derive(Debug, Clone)]
pub struct ActiveConversation {
    cursor: SessionCursorV1,
    messages: Arc<[Value]>,
    source: ActiveConversationSource,
}

#[derive(Debug, Clone)]
pub struct PreparedConversationCommit {
    pub commit: ConversationCommitV1,
    pub next: ActiveConversation,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ActiveConversationError {
    #[error("conversation owner is empty")]
    EmptyOwner,
    #[error("conversation session id is empty")]
    EmptySession,
    #[error("conversation commit schema {0} is unsupported")]
    UnsupportedCommitSchema(u32),
    #[error("session cursor schema {0} is unsupported")]
    UnsupportedCursorSchema(u32),
    #[error("conversation commit belongs to owner `{actual}`, expected `{expected}`")]
    OwnerMismatch { expected: String, actual: String },
    #[error("conversation commit belongs to session `{actual}`, expected `{expected}`")]
    SessionMismatch { expected: String, actual: String },
    #[error("conversation branch `{0}` is unsupported")]
    UnsupportedBranch(String),
    #[error("conversation sequence {actual} does not follow {expected}")]
    ConversationSequence { expected: u64, actual: u64 },
    #[error("journal conversation sequence {actual} does not follow {expected}")]
    JournalSequence { expected: u64, actual: u64 },
    #[error("conversation commit base root does not match the active root")]
    BaseRootMismatch,
    #[error("conversation commit root does not match its materialized messages")]
    RootMismatch,
    #[error("completed turn regressed from {previous} to {actual}")]
    TurnRegression { previous: u32, actual: u32 },
    #[error("canonical conversation cursor sequence is exhausted")]
    CursorSequenceExhausted,
    #[error("canonical conversation compaction generation is exhausted")]
    CompactionGenerationExhausted,
}

impl ActiveConversation {
    pub fn empty(owner_id: &str, session_id: &str) -> Result<Self, ActiveConversationError> {
        Self::from_projection(
            owner_id,
            session_id,
            Vec::new(),
            0,
            ActiveConversationSource::Live,
        )
    }

    pub fn from_projection(
        owner_id: &str,
        session_id: &str,
        messages: Vec<Value>,
        completed_turn: u32,
        source: ActiveConversationSource,
    ) -> Result<Self, ActiveConversationError> {
        let owner_id = owner_id.trim();
        if owner_id.is_empty() {
            return Err(ActiveConversationError::EmptyOwner);
        }
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return Err(ActiveConversationError::EmptySession);
        }
        let canonical_root_hash = canonical_conversation_root(&messages);
        Ok(Self {
            cursor: SessionCursorV1 {
                schema_version: SESSION_CURSOR_SCHEMA_VERSION,
                owner_id: owner_id.to_string(),
                session_id: session_id.to_string(),
                branch_id: DEFAULT_CONVERSATION_BRANCH_ID.to_string(),
                completed_turn,
                journal_event_seq: 0,
                conversation_seq: 0,
                canonical_root_hash,
                projection_schema: CONVERSATION_PROJECTION_SCHEMA_VERSION,
                compaction_generation: 0,
                config_version_id: None,
            },
            messages: messages.into(),
            source,
        })
    }

    pub fn replay(
        owner_id: &str,
        session_id: &str,
        commits: impl IntoIterator<Item = ConversationCommitV1>,
    ) -> Result<Option<Self>, ActiveConversationError> {
        let mut active = Self::empty(owner_id, session_id)?;
        let mut found = false;
        for commit in commits {
            active = active.apply_commit(commit, ActiveConversationSource::Journal)?;
            found = true;
        }
        Ok(found.then_some(active))
    }

    pub fn cursor(&self) -> &SessionCursorV1 {
        &self.cursor
    }

    pub fn source(&self) -> ActiveConversationSource {
        self.source
    }

    pub fn messages(&self) -> &[Value] {
        &self.messages
    }

    pub fn materialize(&self) -> Vec<Value> {
        self.messages.as_ref().to_vec()
    }

    pub fn prepare_commit(
        &self,
        completed_turn: u32,
        config_version_id: Option<String>,
        messages: Vec<Value>,
    ) -> Result<PreparedConversationCommit, ActiveConversationError> {
        if completed_turn < self.cursor.completed_turn {
            return Err(ActiveConversationError::TurnRegression {
                previous: self.cursor.completed_turn,
                actual: completed_turn,
            });
        }

        let durable_base = self.source.is_durable_canonical();
        let common_prefix = if durable_base {
            common_prefix_len(self.messages(), &messages)
        } else {
            0
        };
        let (base_root_hash, delta, compaction_generation) =
            if durable_base && common_prefix == self.messages.len() {
                (
                    self.cursor.canonical_root_hash.clone(),
                    ConversationDeltaV1::Append {
                        messages: messages[common_prefix..].to_vec(),
                    },
                    self.cursor.compaction_generation,
                )
            } else {
                let reason = if durable_base {
                    ConversationReplaceReason::Compaction
                } else {
                    ConversationReplaceReason::ProjectionMigration
                };
                (
                    if durable_base {
                        self.cursor.canonical_root_hash.clone()
                    } else {
                        canonical_conversation_root(&[])
                    },
                    ConversationDeltaV1::Replace {
                        messages: messages.clone(),
                        reason,
                    },
                    self.cursor
                        .compaction_generation
                        .checked_add(1)
                        .ok_or(ActiveConversationError::CompactionGenerationExhausted)?,
                )
            };

        let next_conversation_seq = self
            .cursor
            .conversation_seq
            .checked_add(1)
            .ok_or(ActiveConversationError::CursorSequenceExhausted)?;
        let next_journal_seq = self
            .cursor
            .journal_event_seq
            .checked_add(1)
            .ok_or(ActiveConversationError::CursorSequenceExhausted)?;
        let cursor = SessionCursorV1 {
            schema_version: SESSION_CURSOR_SCHEMA_VERSION,
            owner_id: self.cursor.owner_id.clone(),
            session_id: self.cursor.session_id.clone(),
            branch_id: self.cursor.branch_id.clone(),
            completed_turn,
            journal_event_seq: next_journal_seq,
            conversation_seq: next_conversation_seq,
            canonical_root_hash: canonical_conversation_root(&messages),
            projection_schema: CONVERSATION_PROJECTION_SCHEMA_VERSION,
            compaction_generation,
            config_version_id,
        };
        let commit = ConversationCommitV1 {
            schema_version: CONVERSATION_COMMIT_SCHEMA_VERSION,
            base_root_hash,
            cursor: cursor.clone(),
            delta,
        };
        Ok(PreparedConversationCommit {
            commit,
            next: Self {
                cursor,
                messages: messages.into(),
                source: ActiveConversationSource::Live,
            },
        })
    }

    fn apply_commit(
        &self,
        commit: ConversationCommitV1,
        source: ActiveConversationSource,
    ) -> Result<Self, ActiveConversationError> {
        validate_commit_identity(&self.cursor, &commit)?;
        if commit.base_root_hash != self.cursor.canonical_root_hash {
            return Err(ActiveConversationError::BaseRootMismatch);
        }
        let messages = match commit.delta {
            ConversationDeltaV1::Append { messages } => {
                let mut materialized = self.materialize();
                materialized.extend(messages);
                materialized
            }
            ConversationDeltaV1::Replace { messages, .. } => messages,
        };
        if canonical_conversation_root(&messages) != commit.cursor.canonical_root_hash {
            return Err(ActiveConversationError::RootMismatch);
        }
        Ok(Self {
            cursor: commit.cursor,
            messages: messages.into(),
            source,
        })
    }
}

fn validate_commit_identity(
    current: &SessionCursorV1,
    commit: &ConversationCommitV1,
) -> Result<(), ActiveConversationError> {
    if commit.schema_version != CONVERSATION_COMMIT_SCHEMA_VERSION {
        return Err(ActiveConversationError::UnsupportedCommitSchema(
            commit.schema_version,
        ));
    }
    if commit.cursor.schema_version != SESSION_CURSOR_SCHEMA_VERSION {
        return Err(ActiveConversationError::UnsupportedCursorSchema(
            commit.cursor.schema_version,
        ));
    }
    if commit.cursor.owner_id != current.owner_id {
        return Err(ActiveConversationError::OwnerMismatch {
            expected: current.owner_id.clone(),
            actual: commit.cursor.owner_id.clone(),
        });
    }
    if commit.cursor.session_id != current.session_id {
        return Err(ActiveConversationError::SessionMismatch {
            expected: current.session_id.clone(),
            actual: commit.cursor.session_id.clone(),
        });
    }
    if commit.cursor.branch_id != DEFAULT_CONVERSATION_BRANCH_ID {
        return Err(ActiveConversationError::UnsupportedBranch(
            commit.cursor.branch_id.clone(),
        ));
    }
    let expected = current
        .conversation_seq
        .checked_add(1)
        .ok_or(ActiveConversationError::CursorSequenceExhausted)?;
    if commit.cursor.conversation_seq != expected {
        return Err(ActiveConversationError::ConversationSequence {
            expected,
            actual: commit.cursor.conversation_seq,
        });
    }
    let expected = current
        .journal_event_seq
        .checked_add(1)
        .ok_or(ActiveConversationError::CursorSequenceExhausted)?;
    if commit.cursor.journal_event_seq != expected {
        return Err(ActiveConversationError::JournalSequence {
            expected,
            actual: commit.cursor.journal_event_seq,
        });
    }
    if commit.cursor.completed_turn < current.completed_turn {
        return Err(ActiveConversationError::TurnRegression {
            previous: current.completed_turn,
            actual: commit.cursor.completed_turn,
        });
    }
    Ok(())
}

fn common_prefix_len(left: &[Value], right: &[Value]) -> usize {
    left.iter()
        .zip(right)
        .take_while(|(left, right)| left == right)
        .count()
}

pub fn canonical_conversation_root(messages: &[Value]) -> String {
    let mut bytes = Vec::new();
    bytes.push(b'[');
    for (index, message) in messages.iter().enumerate() {
        if index > 0 {
            bytes.push(b',');
        }
        write_canonical_json(message, &mut bytes);
    }
    bytes.push(b']');
    let mut digest = Sha256::new();
    digest.update(ROOT_HASH_DOMAIN);
    digest.update((bytes.len() as u64).to_be_bytes());
    digest.update(bytes);
    format!("{:x}", digest.finalize())
}

fn write_canonical_json(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(value) => out.extend_from_slice(if *value { b"true" } else { b"false" }),
        Value::Number(value) => out.extend_from_slice(value.to_string().as_bytes()),
        Value::String(value) => {
            out.extend_from_slice(
                serde_json::to_string(value)
                    .expect("serializing a JSON string cannot fail")
                    .as_bytes(),
            );
        }
        Value::Array(values) => {
            out.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_canonical_json(value, out);
            }
            out.push(b']');
        }
        Value::Object(values) => {
            out.push(b'{');
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_canonical_json(&Value::String(key.clone()), out);
                out.push(b':');
                write_canonical_json(&values[key], out);
            }
            out.push(b'}');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ActiveConversation, ActiveConversationError, ActiveConversationSource,
        canonical_conversation_root,
    };
    use astra_turn_types::{ConversationDeltaV1, ConversationReplaceReason};
    use serde_json::json;

    fn tool_turn(label: &str) -> Vec<serde_json::Value> {
        vec![
            json!({"role": "user", "content": format!("user-{label}")}),
            json!({
                "role": "assistant",
                "tool_calls": [{
                    "id": format!("call-{label}"),
                    "type": "function",
                    "function": {"name": "lookup", "arguments": "{\"key\":\"v\"}"}
                }]
            }),
            json!({"role": "tool", "tool_call_id": format!("call-{label}"), "content": format!("result-{label}")}),
            json!({"role": "assistant", "content": format!("answer-{label}")}),
        ]
    }

    #[test]
    fn append_commits_preserve_typed_tool_rounds_across_four_turns() {
        let mut active = ActiveConversation::empty("owner", "session").unwrap();
        let mut commits = Vec::new();
        let mut messages = Vec::new();
        for turn in 1..=4 {
            messages.extend(tool_turn(&turn.to_string()));
            let prepared = active.prepare_commit(turn, None, messages.clone()).unwrap();
            assert!(matches!(
                prepared.commit.delta,
                ConversationDeltaV1::Append { .. }
            ));
            commits.push(prepared.commit);
            active = prepared.next;
        }

        let replayed = ActiveConversation::replay("owner", "session", commits)
            .unwrap()
            .unwrap();
        assert_eq!(replayed.messages(), messages);
        assert_eq!(replayed.cursor().completed_turn, 4);
        assert_eq!(replayed.cursor().conversation_seq, 4);
        assert_eq!(replayed.source(), ActiveConversationSource::Journal);
    }

    #[test]
    fn compaction_is_an_explicit_replace_and_advances_generation() {
        let active = ActiveConversation::empty("owner", "session")
            .unwrap()
            .prepare_commit(1, None, tool_turn("one"))
            .unwrap()
            .next;
        let compacted = vec![
            json!({"role": "user", "content": "summary"}),
            json!({"role": "assistant", "content": "compacted"}),
        ];
        let prepared = active.prepare_commit(2, None, compacted.clone()).unwrap();
        assert!(matches!(
            prepared.commit.delta,
            ConversationDeltaV1::Replace {
                reason: ConversationReplaceReason::Compaction,
                ..
            }
        ));
        assert_eq!(prepared.next.messages(), compacted);
        assert_eq!(prepared.next.cursor().compaction_generation, 1);
    }

    #[test]
    fn projection_migration_replaces_instead_of_referencing_an_undurable_base() {
        let projected = ActiveConversation::from_projection(
            "owner",
            "session",
            tool_turn("legacy"),
            1,
            ActiveConversationSource::CslProjection,
        )
        .unwrap();
        let prepared = projected
            .prepare_commit(2, None, {
                let mut messages = tool_turn("legacy");
                messages.extend(tool_turn("new"));
                messages
            })
            .unwrap();
        assert_eq!(
            prepared.commit.base_root_hash,
            canonical_conversation_root(&[])
        );
        assert!(matches!(
            prepared.commit.delta,
            ConversationDeltaV1::Replace {
                reason: ConversationReplaceReason::ProjectionMigration,
                ..
            }
        ));
    }

    #[test]
    fn canonical_root_ignores_object_key_insertion_order() {
        assert_eq!(
            canonical_conversation_root(&[json!({"role": "user", "content": "hi"})]),
            canonical_conversation_root(&[json!({"content": "hi", "role": "user"})])
        );
    }

    #[test]
    fn replay_rejects_a_commit_spliced_onto_the_wrong_base() {
        let active = ActiveConversation::empty("owner", "session").unwrap();
        let mut commit = active
            .prepare_commit(1, None, tool_turn("one"))
            .unwrap()
            .commit;
        commit.base_root_hash = "wrong".to_string();
        assert_eq!(
            ActiveConversation::replay("owner", "session", [commit]).unwrap_err(),
            ActiveConversationError::BaseRootMismatch
        );
    }
}

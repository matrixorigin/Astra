use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SESSION_CURSOR_SCHEMA_VERSION: u32 = 1;
pub const CONVERSATION_COMMIT_SCHEMA_VERSION: u32 = 1;
pub const CONVERSATION_PROJECTION_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_CONVERSATION_BRANCH_ID: &str = "main";

/// Durable identity of one committed canonical-conversation boundary.
///
/// `journal_event_seq` is the monotonic sequence of the canonical
/// conversation lane. It deliberately does not depend on wall-clock
/// timestamps or the number of unrelated observability events in the same
/// journal.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionCursorV1 {
    pub schema_version: u32,
    pub owner_id: String,
    pub session_id: String,
    pub branch_id: String,
    pub completed_turn: u32,
    pub journal_event_seq: u64,
    pub conversation_seq: u64,
    pub canonical_root_hash: String,
    pub projection_schema: u32,
    pub compaction_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_version_id: Option<String>,
}

/// Canonical conversation change committed with the primary turn event.
///
/// Ordinary turns append only their changed suffix. A compaction or a
/// migration from a legacy projection installs an explicit replacement
/// snapshot so replay never has to infer a rewrite from display text.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConversationDeltaV1 {
    Append {
        messages: Vec<Value>,
    },
    Replace {
        messages: Vec<Value>,
        reason: ConversationReplaceReason,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversationReplaceReason {
    Compaction,
    ProjectionMigration,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConversationCommitV1 {
    pub schema_version: u32,
    pub base_root_hash: String,
    pub cursor: SessionCursorV1,
    pub delta: ConversationDeltaV1,
}

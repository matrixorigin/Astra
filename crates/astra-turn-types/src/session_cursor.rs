use std::io::{self, Write};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const SESSION_CURSOR_SCHEMA_VERSION: u32 = 1;
pub const CONVERSATION_COMMIT_SCHEMA_VERSION: u32 = 1;
pub const CONVERSATION_PROJECTION_SCHEMA_VERSION: u32 = 1;
pub const SEGMENTED_CONVERSATION_PROJECTION_SCHEMA_VERSION: u32 = 2;
pub const DEFAULT_CONVERSATION_BRANCH_ID: &str = "main";
const ROOT_HASH_DOMAIN: &[u8] = b"astra.canonical-conversation.v1\0";

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

/// Content root for canonical ordered typed conversation messages.
///
/// JSON object keys are sorted recursively so equivalent wire objects have
/// one placement-independent root.
pub fn canonical_conversation_root(messages: &[Value]) -> String {
    let mut counter = CountingWriter::default();
    write_canonical_messages(messages, &mut counter)
        .expect("counting canonical JSON bytes cannot fail");

    let mut digest = Sha256::new();
    digest.update(ROOT_HASH_DOMAIN);
    digest.update(counter.bytes.to_be_bytes());
    write_canonical_messages(messages, &mut DigestWriter(&mut digest))
        .expect("hashing canonical JSON bytes cannot fail");
    format!("{:x}", digest.finalize())
}

/// Number of bytes in the canonical JSON representation used by
/// [`canonical_conversation_root`].
///
/// This deliberately counts through a writer instead of allocating a second
/// full-history buffer. Segment stores use it for byte-weighted admission and
/// cache accounting.
pub fn canonical_conversation_serialized_len(messages: &[Value]) -> u64 {
    let mut counter = CountingWriter::default();
    write_canonical_messages(messages, &mut counter)
        .expect("counting canonical JSON bytes cannot fail");
    counter.bytes
}

/// Count a JSON payload without allocating its serialized representation.
///
/// Admission paths use this for fresh request components that are not yet
/// canonical conversation messages (attachments, typed parts, and runtime
/// context). JSON object ordering does not affect the byte count.
pub fn json_serialized_len<T: Serialize + ?Sized>(value: &T) -> Result<u64, serde_json::Error> {
    let mut counter = CountingWriter::default();
    serde_json::to_writer(&mut counter, value)?;
    Ok(counter.bytes)
}

#[derive(Default)]
struct CountingWriter {
    bytes: u64,
}

impl Write for CountingWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(buffer.len() as u64)
            .ok_or_else(|| io::Error::other("canonical JSON length overflow"))?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct DigestWriter<'a>(&'a mut Sha256);

impl Write for DigestWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.update(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn write_canonical_messages(messages: &[Value], out: &mut impl Write) -> io::Result<()> {
    out.write_all(b"[")?;
    for (index, message) in messages.iter().enumerate() {
        if index > 0 {
            out.write_all(b",")?;
        }
        write_canonical_json(message, out)?;
    }
    out.write_all(b"]")
}

fn write_canonical_json(value: &Value, out: &mut impl Write) -> io::Result<()> {
    match value {
        Value::Null => out.write_all(b"null")?,
        Value::Bool(value) => out.write_all(if *value { b"true" } else { b"false" })?,
        Value::Number(value) => write_json(value, out)?,
        Value::String(value) => write_json(value, out)?,
        Value::Array(values) => {
            out.write_all(b"[")?;
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    out.write_all(b",")?;
                }
                write_canonical_json(value, out)?;
            }
            out.write_all(b"]")?;
        }
        Value::Object(values) => {
            out.write_all(b"{")?;
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.write_all(b",")?;
                }
                write_json(key, out)?;
                out.write_all(b":")?;
                write_canonical_json(&values[key], out)?;
            }
            out.write_all(b"}")?;
        }
    }
    Ok(())
}

fn write_json(value: &impl Serialize, out: &mut impl Write) -> io::Result<()> {
    serde_json::to_writer(out, value).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{canonical_conversation_root, json_serialized_len};

    #[test]
    fn streaming_root_preserves_the_v1_wire_hash() {
        let messages = vec![
            json!({"role": "user", "content": "hello 世界"}),
            json!({
                "role": "assistant",
                "content": [{"type": "text", "text": "ok\nnext"}],
                "meta": {"b": 2, "a": true}
            }),
        ];

        assert_eq!(
            canonical_conversation_root(&messages),
            "18fd5901a9aa39a4802a649a8f13a2f79a5266ff76f514645d33d5cd1bd6891b"
        );
    }

    #[test]
    fn streaming_json_length_matches_wire_serialization() {
        let payload = json!({
            "parts": [{"type": "text", "text": "hello 世界"}],
            "attachments": [{"name": "a.txt", "bytes": 1234}],
        });
        assert_eq!(
            json_serialized_len(&payload).unwrap(),
            serde_json::to_vec(&payload).unwrap().len() as u64
        );
    }
}

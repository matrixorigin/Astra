//! Immutable, session-owned history compressed out of the live model context.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const CONTEXT_HISTORY_ARTIFACT_KIND: &str = "context_history_v1";
pub const CONTEXT_HISTORY_ARTIFACT_URI_PREFIX: &str = "artifact://session/context-history/";
pub const CONTEXT_HISTORY_MANIFEST_ID_PREFIX: &str = "manifest-";
pub const MAX_CONTEXT_HISTORY_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextHistoryArtifactV1 {
    pub schema_version: u16,
    pub session_id: String,
    pub source_run_id: String,
    pub message_count: usize,
    pub transcript_json: String,
    pub byte_len: usize,
    pub content_sha256: String,
}

impl ContextHistoryArtifactV1 {
    pub fn new(
        session_id: &str,
        run_id: &str,
        message_count: usize,
        transcript_json: String,
    ) -> Result<Self, &'static str> {
        let artifact = Self {
            schema_version: 1,
            session_id: session_id.into(),
            source_run_id: run_id.into(),
            message_count,
            byte_len: transcript_json.len(),
            content_sha256: format!("{:x}", Sha256::digest(transcript_json.as_bytes())),
            transcript_json,
        };
        artifact.validate(session_id)?;
        Ok(artifact)
    }

    pub fn validate(&self, session_id: &str) -> Result<(), &'static str> {
        if self.schema_version != 1
            || self.session_id != session_id
            || session_id.is_empty()
            || session_id.len() > 64
            || self.source_run_id.is_empty()
            || self.source_run_id.len() > 256
        {
            return Err("context history identity is invalid");
        }
        if self.byte_len > MAX_CONTEXT_HISTORY_BYTES
            || self.byte_len != self.transcript_json.len()
            || self.content_sha256
                != format!("{:x}", Sha256::digest(self.transcript_json.as_bytes()))
        {
            return Err("context history content integrity is invalid");
        }
        let messages: serde_json::Value = serde_json::from_str(&self.transcript_json)
            .map_err(|_| "context history transcript is invalid JSON")?;
        if !messages
            .as_array()
            .is_some_and(|messages| messages.len() == self.message_count && !messages.is_empty())
        {
            return Err("context history message count is invalid");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn history_integrity_rejects_session_count_and_content_changes() {
        let artifact = ContextHistoryArtifactV1::new("session", "run", 1, "[{}]".into()).unwrap();
        assert!(artifact.validate("other-session").is_err());
        let mut changed = artifact.clone();
        changed.message_count = 2;
        assert!(changed.validate("session").is_err());
        changed = artifact;
        changed.transcript_json = "[null]".into();
        assert!(changed.validate("session").is_err());
    }
}

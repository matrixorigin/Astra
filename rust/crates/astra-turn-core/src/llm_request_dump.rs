//! Dump the full LLM request payload on error for post-mortem debugging.
//!
//! Two outputs:
//! 1. **Local file**: `~/.astra/sessions/<session_id>/llm_error_<ts>.json`
//!    — immediate access for the user / CLI developer.
//! 2. **Cloud event**: `event_type = "llm_request_dump"` in `agent_events`
//!    — queryable via `/events/session/{session_id}` for support staff.

use std::sync::Arc;

use astra_services::{SessionArtifactJsonRecord, SessionArtifactJsonStore, SessionArtifactStore};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::contracts::{TurnAuxiliaryEventRecord, TurnAuxiliaryEventWriter};

const ERROR_PREVIEW_MAX_CHARS: usize = 200;

/// First `max_chars` Unicode scalars of `s` (no panic on UTF-8 boundaries).
fn truncate_chars(s: &str, max_chars: usize) -> &str {
    s.char_indices()
        .nth(max_chars)
        .map(|(i, _)| &s[..i])
        .unwrap_or(s)
}

/// Capture the LLM request state at the moment of failure.
#[derive(Debug, Clone)]
pub struct LlmRequestDump {
    pub session_id: String,
    pub agent_id: Option<String>,
    pub model: String,
    pub provider: String,
    pub error: String,
    pub messages: Vec<Value>,
    pub tools: Vec<Value>,
    pub round: i64,
    pub max_output_tokens: Option<usize>,
}

impl LlmRequestDump {
    /// Serialize to JSON for persistence.
    pub fn to_json(&self) -> Value {
        json!({
            "session_id": self.session_id,
            "model": self.model,
            "provider": self.provider,
            "error": self.error,
            "round": self.round,
            "max_output_tokens": self.max_output_tokens,
            "message_count": self.messages.len(),
            "tool_count": self.tools.len(),
            "messages": self.messages,
            "tools": self.tools,
        })
    }

    pub fn to_remote_artifact_record(&self, user_id: &str) -> SessionArtifactJsonRecord {
        SessionArtifactJsonRecord {
            artifact_id: String::new(),
            session_id: self.session_id.clone(),
            user_id: user_id.to_string(),
            artifact_kind: "llm_request_dump".to_string(),
            source: Some("llm_request_dump".to_string()),
            turn: None,
            round: u32::try_from(self.round).ok(),
            content: self.to_json(),
            metadata: Some(json!({
                "agent_id": self.agent_id,
                "model": self.model,
                "provider": self.provider,
                "error_preview": truncate_chars(&self.error, ERROR_PREVIEW_MAX_CHARS),
            })),
        }
    }

    /// Write to local file under the session directory.
    /// Returns the path on success.
    pub fn write_local(&self) -> Option<String> {
        let dir = astra_services::local_session_artifact_store()
            .session_dir(&self.session_id)
            .ok()?;
        std::fs::create_dir_all(&dir).ok()?;
        let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
        let path = dir.join(format!("llm_error_{ts}.json"));
        let content = serde_json::to_string_pretty(&self.to_json()).ok()?;
        std::fs::write(&path, content).ok()?;
        Some(path.display().to_string())
    }

    /// Persist as an auxiliary event to the cloud DB (fire-and-forget).
    pub fn persist_cloud(
        &self,
        user_id: &str,
        causal_chain_id: &str,
        writer: Arc<dyn TurnAuxiliaryEventWriter>,
    ) {
        let event = TurnAuxiliaryEventRecord {
            event_id: Uuid::now_v7().to_string(),
            user_id: user_id.to_string(),
            session_id: self.session_id.clone(),
            agent_id: self.agent_id.clone(),
            event_type: "llm_request_dump".to_string(),
            content: serde_json::to_string(&self.to_json()).unwrap_or_default(),
            parent_event_id: None,
            parent_event_ids: Vec::new(),
            causal_chain_id: causal_chain_id.to_string(),
            metadata: Some(json!({
                "model": self.model,
                "provider": self.provider,
                "round": self.round,
                "message_count": self.messages.len(),
                "tool_count": self.tools.len(),
                "error_preview": truncate_chars(&self.error, ERROR_PREVIEW_MAX_CHARS),
            })),
            reasoning_content: None,
        };
        tokio::spawn(async move {
            if let Err(e) = writer.persist_events(vec![event]).await {
                eprintln!("[llm_error_dump] cloud persist failed: {e}");
            }
        });
    }

    pub async fn persist_remote(
        &self,
        user_id: &str,
        store: &dyn SessionArtifactJsonStore,
    ) -> Result<(), String> {
        store
            .persist_json_artifact(self.to_remote_artifact_record(user_id))
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

/// Build a dump from the current bridge state.
#[allow(clippy::too_many_arguments)]
pub fn build_llm_request_dump(
    session_id: &str,
    agent_id: Option<&str>,
    model: &str,
    provider: &str,
    error: &str,
    messages: &[Value],
    tools: &[Value],
    round: i64,
    max_output_tokens: Option<usize>,
) -> LlmRequestDump {
    LlmRequestDump {
        session_id: session_id.to_string(),
        agent_id: agent_id.map(ToString::to_string),
        model: model.to_string(),
        provider: provider.to_string(),
        error: error.to_string(),
        messages: messages.to_vec(),
        tools: tools.to_vec(),
        round,
        max_output_tokens,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::session_journal::JournalDirGuard;

    #[test]
    fn truncate_chars_keeps_short_strings() {
        assert_eq!(truncate_chars("hello", ERROR_PREVIEW_MAX_CHARS), "hello");
    }

    #[test]
    fn truncate_chars_limits_to_max_unicode_scalars() {
        let s: String = (0..250)
            .map(|i| char::from(b'a' + (i % 26) as u8))
            .collect();
        let t = truncate_chars(&s, ERROR_PREVIEW_MAX_CHARS);
        assert_eq!(t.chars().count(), ERROR_PREVIEW_MAX_CHARS);
    }

    #[test]
    fn truncate_chars_does_not_split_utf8() {
        let wide = "😀".repeat(300);
        let t = truncate_chars(&wide, ERROR_PREVIEW_MAX_CHARS);
        assert_eq!(t.chars().count(), ERROR_PREVIEW_MAX_CHARS);
        assert!(std::str::from_utf8(t.as_bytes()).is_ok());
    }

    #[test]
    fn dump_to_json_includes_all_fields() {
        let dump = build_llm_request_dump(
            "sess-1",
            Some("test-agent"),
            "kimi-k2.5",
            "moonshot",
            "LLM error 400: thinking is enabled but reasoning_content is missing",
            &[json!({"role": "user", "content": "hi"})],
            &[json!({"type": "function", "function": {"name": "bash"}})],
            2,
            Some(8192),
        );
        let j = dump.to_json();
        assert_eq!(j["session_id"], "sess-1");
        assert_eq!(j["model"], "kimi-k2.5");
        assert_eq!(j["message_count"], 1);
        assert_eq!(j["tool_count"], 1);
        assert_eq!(j["round"], 2);
        assert!(j["error"].as_str().unwrap().contains("reasoning_content"));
        assert!(j["messages"].as_array().unwrap().len() == 1);
    }

    #[test]
    fn remote_artifact_record_uses_dump_kind() {
        let dump = build_llm_request_dump(
            "sess-1",
            Some("test-agent"),
            "kimi-k2.5",
            "moonshot",
            "LLM error 400",
            &[json!({"role": "user", "content": "hi"})],
            &[],
            2,
            Some(8192),
        );
        let record = dump.to_remote_artifact_record("user-1");
        assert_eq!(record.artifact_kind, "llm_request_dump");
        assert_eq!(record.round, Some(2));
        assert_eq!(record.metadata.as_ref().unwrap()["model"], "kimi-k2.5");
    }

    #[test]
    fn write_local_uses_session_artifact_root_override() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let dump = build_llm_request_dump(
            "sess-1",
            Some("test-agent"),
            "kimi-k2.5",
            "moonshot",
            "LLM error 400",
            &[json!({"role": "user", "content": "hi"})],
            &[],
            0,
            Some(1024),
        );
        let path = dump.write_local().expect("dump path");
        assert!(
            path.starts_with(&*temp.path().join("sess-1").to_string_lossy()),
            "{path}"
        );
    }
}

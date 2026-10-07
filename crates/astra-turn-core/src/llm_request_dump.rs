//! Dump the full LLM request payload on error for post-mortem debugging.
//!
//! Server persists an owner-scoped remote JSON artifact for request failures.
//! Request content and Unicode-safe error previews share the existing artifact
//! store; this module does not write to host-local session directories.

use astra_services::{SessionArtifactJsonRecord, SessionArtifactJsonStore};
use serde_json::{Value, json};

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
        astra_core::history_work::record_serialized_value(
            astra_core::history_work::HistoryWorkSite::RequestDumpClone,
            &self.messages,
        );
        astra_core::history_work::record_serialized_value(
            astra_core::history_work::HistoryWorkSite::RequestDumpClone,
            &self.tools,
        );
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
            references: Vec::new(),
        }
    }

    pub async fn persist_remote(
        &self,
        user_id: &str,
        store: &dyn SessionArtifactJsonStore,
    ) -> Result<(), String> {
        let record = self.to_remote_artifact_record(user_id);
        astra_core::history_work::record_serialized_value(
            astra_core::history_work::HistoryWorkSite::RequestDumpSerialization,
            &record.content,
        );
        store
            .persist_json_artifact(record)
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
    astra_core::history_work::record_serialized_value(
        astra_core::history_work::HistoryWorkSite::RequestDumpClone,
        messages,
    );
    astra_core::history_work::record_serialized_value(
        astra_core::history_work::HistoryWorkSite::RequestDumpClone,
        tools,
    );
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
}

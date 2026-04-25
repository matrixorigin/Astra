use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use astra_services::{SessionArtifactJsonRecord, SessionArtifactJsonStore, SessionArtifactStore};
use serde_json::{Value, json};

const LLM_CAPTURE_ENV: &str = "MO_CAPTURE_LLM_EXCHANGES";

fn env_truthy(name: &str) -> bool {
    std::env::var(name).ok().is_some_and(|value| {
        !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no" | "off"
        )
    })
}

fn sanitize_component(raw: &str) -> String {
    let sanitized: String = raw
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => ch,
            _ => '-',
        })
        .collect();
    let trimmed = sanitized.trim_matches('-');
    if trimmed.is_empty() {
        "unknown".to_string()
    } else {
        trimmed.to_string()
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_remote_capture_record(
    session_id: &str,
    user_id: &str,
    turn: u32,
    round: u32,
    agent_id: Option<&str>,
    source: &str,
    model: &str,
    provider: &str,
    messages: &[Value],
    tools: &[Value],
    max_output_tokens: Option<usize>,
    outcome: &str,
    response: Value,
) -> SessionArtifactJsonRecord {
    SessionArtifactJsonRecord {
        artifact_id: String::new(),
        session_id: session_id.to_string(),
        user_id: user_id.to_string(),
        artifact_kind: "llm_capture".to_string(),
        source: Some(source.to_string()),
        turn: Some(turn),
        round: Some(round),
        content: json!({
            "request": {
                "message_count": messages.len(),
                "tool_count": tools.len(),
                "max_output_tokens": max_output_tokens,
                "messages": messages,
                "tools": tools,
            },
            "response": response,
        }),
        metadata: Some(json!({
            "agent_id": agent_id,
            "model": model,
            "provider": provider,
            "outcome": outcome,
        })),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn persist_remote_capture(
    store: &dyn SessionArtifactJsonStore,
    session_id: &str,
    user_id: &str,
    turn: u32,
    round: u32,
    agent_id: Option<&str>,
    source: &str,
    model: &str,
    provider: &str,
    messages: &[Value],
    tools: &[Value],
    max_output_tokens: Option<usize>,
    outcome: &str,
    response: Value,
) -> Result<(), String> {
    store
        .persist_json_artifact(build_remote_capture_record(
            session_id,
            user_id,
            turn,
            round,
            agent_id,
            source,
            model,
            provider,
            messages,
            tools,
            max_output_tokens,
            outcome,
            response,
        ))
        .await
        .map(|_| ())
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn persist_configured_capture(
    remote_store: Option<&dyn SessionArtifactJsonStore>,
    session_id: &str,
    user_id: &str,
    turn: u32,
    round: u32,
    agent_id: Option<&str>,
    source: &str,
    model: &str,
    provider: &str,
    messages: &[Value],
    tools: &[Value],
    max_output_tokens: Option<usize>,
    outcome: &str,
    response: Value,
) -> Result<(), String> {
    let _ = persist_capture(
        session_id,
        turn,
        round,
        agent_id,
        source,
        model,
        provider,
        messages,
        tools,
        max_output_tokens,
        outcome,
        response.clone(),
    );
    if let Some(store) = remote_store {
        persist_remote_capture(
            store,
            session_id,
            user_id,
            turn,
            round,
            agent_id,
            source,
            model,
            provider,
            messages,
            tools,
            max_output_tokens,
            outcome,
            response,
        )
        .await?;
    }
    Ok(())
}

fn capture_file_path(
    session_id: &str,
    turn: u32,
    round: u32,
    source: &str,
    outcome: &str,
) -> PathBuf {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    astra_services::local_session_artifact_store()
        .session_dir(session_id)
        .expect("validated session_id must resolve llm capture dir")
        .join(format!(
            "llm_capture_t{turn}_r{round}_{source}_{outcome}_{millis}.json"
        ))
}

pub(crate) fn capture_enabled() -> bool {
    env_truthy(LLM_CAPTURE_ENV)
}

#[allow(clippy::too_many_arguments)]
fn persist_capture_inner(
    session_id: &str,
    turn: u32,
    round: u32,
    agent_id: Option<&str>,
    source: &str,
    model: &str,
    provider: &str,
    messages: &[Value],
    tools: &[Value],
    max_output_tokens: Option<usize>,
    outcome: &str,
    response: Value,
) -> Option<String> {
    if session_id.trim().is_empty() {
        return None;
    }

    let source = sanitize_component(source);
    let outcome = sanitize_component(outcome);
    let path = capture_file_path(session_id, turn, round, &source, &outcome);
    let parent = path.parent()?;
    std::fs::create_dir_all(parent).ok()?;

    let payload = json!({
        "captured_at_ms": SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis(),
        "session_id": session_id,
        "turn": turn,
        "round": round,
        "agent_id": agent_id,
        "source": source,
        "model": model,
        "provider": provider,
        "request": {
            "message_count": messages.len(),
            "tool_count": tools.len(),
            "max_output_tokens": max_output_tokens,
            "messages": messages,
            "tools": tools,
        },
        "outcome": outcome,
        "response": response,
    });

    let content = serde_json::to_string_pretty(&payload).ok()?;
    std::fs::write(&path, content).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Some(path.display().to_string())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn persist_capture(
    session_id: &str,
    turn: u32,
    round: u32,
    agent_id: Option<&str>,
    source: &str,
    model: &str,
    provider: &str,
    messages: &[Value],
    tools: &[Value],
    max_output_tokens: Option<usize>,
    outcome: &str,
    response: Value,
) -> Option<String> {
    if !capture_enabled() {
        return None;
    }
    persist_capture_inner(
        session_id,
        turn,
        round,
        agent_id,
        source,
        model,
        provider,
        messages,
        tools,
        max_output_tokens,
        outcome,
        response,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::{StoredSessionArtifact, session_journal::JournalDirGuard};
    use async_trait::async_trait;
    use tempfile::tempdir;

    #[derive(Default)]
    struct RecordingArtifactStore {
        records: std::sync::Mutex<Vec<SessionArtifactJsonRecord>>,
    }

    #[async_trait]
    impl SessionArtifactJsonStore for RecordingArtifactStore {
        async fn persist_json_artifact(
            &self,
            record: SessionArtifactJsonRecord,
        ) -> Result<StoredSessionArtifact, String> {
            self.records
                .lock()
                .expect("recording store lock")
                .push(record.clone());
            Ok(StoredSessionArtifact {
                artifact_id: if record.artifact_id.is_empty() {
                    "artifact-1".to_string()
                } else {
                    record.artifact_id.clone()
                },
                session_id: record.session_id,
                user_id: record.user_id,
                artifact_kind: record.artifact_kind,
                source: record.source,
                turn: record.turn,
                round: record.round,
                content: record.content,
                metadata: record.metadata,
                created_at: None,
            })
        }

        async fn load_json_artifact(
            &self,
            _artifact_id: &str,
        ) -> Result<Option<StoredSessionArtifact>, String> {
            Ok(None)
        }

        async fn load_latest_json_artifact(
            &self,
            _session_id: &str,
            _artifact_kind: &str,
        ) -> Result<Option<StoredSessionArtifact>, String> {
            Ok(None)
        }

        async fn list_json_artifacts(
            &self,
            _session_id: &str,
            _artifact_kind: Option<&str>,
            _limit: usize,
        ) -> Result<Vec<StoredSessionArtifact>, String> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn persist_capture_writes_request_and_response_under_session_dir() {
        let temp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let path = persist_capture_inner(
            "sess-123",
            4,
            2,
            Some("agent-1"),
            "server_loop_host",
            "gpt-5.4",
            "openai",
            &[json!({"role": "user", "content": "hello"})],
            &[json!({"type":"function","function":{"name":"bash"}})],
            Some(2048),
            "success",
            json!({"finish_reason":"stop","full_text":"done"}),
        )
        .expect("capture path");

        let written = std::fs::read_to_string(&path).unwrap();
        let parsed: Value = serde_json::from_str(&written).unwrap();
        assert_eq!(parsed["session_id"], "sess-123");
        assert_eq!(parsed["turn"], 4);
        assert_eq!(parsed["round"], 2);
        assert_eq!(parsed["request"]["messages"][0]["content"], "hello");
        assert_eq!(parsed["response"]["full_text"], "done");
    }

    #[test]
    fn persist_capture_noops_when_disabled() {
        let temp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        unsafe { std::env::remove_var(LLM_CAPTURE_ENV) };
        let path = persist_capture(
            "sess-123",
            1,
            0,
            None,
            "server_loop_host",
            "gpt-5.4",
            "openai",
            &[],
            &[],
            None,
            "success",
            json!({}),
        );
        assert!(path.is_none());
    }

    #[test]
    fn build_remote_capture_record_sets_request_and_metadata() {
        let record = build_remote_capture_record(
            "sess-123",
            "user-1",
            4,
            2,
            Some("agent-1"),
            "server_loop_host",
            "gpt-5.4",
            "openai",
            &[json!({"role": "user", "content": "hello"})],
            &[json!({"type":"function","function":{"name":"bash"}})],
            Some(2048),
            "success",
            json!({"finish_reason":"stop"}),
        );
        assert_eq!(record.artifact_kind, "llm_capture");
        assert_eq!(record.turn, Some(4));
        assert_eq!(record.round, Some(2));
        assert_eq!(record.content["request"]["messages"][0]["content"], "hello");
        assert_eq!(record.metadata.as_ref().unwrap()["outcome"], "success");
    }

    #[tokio::test]
    async fn persist_configured_capture_writes_local_file_when_enabled() {
        let temp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        unsafe { std::env::set_var(LLM_CAPTURE_ENV, "1") };

        persist_configured_capture(
            None,
            "sess-123",
            "user-1",
            2,
            1,
            Some("agent-1"),
            "server_loop_host",
            "gpt-5.4",
            "openai",
            &[json!({"role": "user", "content": "hello"})],
            &[],
            Some(2048),
            "success",
            json!({"full_text":"done"}),
        )
        .await
        .expect("configured capture");

        let session_dir = astra_services::local_session_artifact_store()
            .session_dir("sess-123")
            .expect("session dir");
        let files: Vec<_> = std::fs::read_dir(session_dir)
            .expect("capture dir")
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            files.iter().any(|name| name.contains("llm_capture_t2_r1_server_loop_host_success")),
            "expected local capture file, got {files:?}"
        );

        unsafe { std::env::remove_var(LLM_CAPTURE_ENV) };
    }

    #[tokio::test]
    async fn persist_configured_capture_persists_remote_record_when_store_provided() {
        let temp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        unsafe { std::env::remove_var(LLM_CAPTURE_ENV) };
        let store = RecordingArtifactStore::default();

        persist_configured_capture(
            Some(&store),
            "sess-123",
            "user-1",
            2,
            1,
            Some("agent-1"),
            "server_loop_host",
            "gpt-5.4",
            "openai",
            &[json!({"role": "user", "content": "hello"})],
            &[],
            Some(2048),
            "success",
            json!({"full_text":"done"}),
        )
        .await
        .expect("configured capture");

        let records = store.records.lock().expect("recording store lock");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].artifact_kind, "llm_capture");
        assert_eq!(records[0].content["response"]["full_text"], "done");
    }
}

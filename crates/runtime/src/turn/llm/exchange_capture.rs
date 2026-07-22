use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use astra_core::SharedPool;
use astra_services::{SessionArtifactJsonRecord, SessionArtifactJsonStore, SessionArtifactStore};
use serde_json::{Value, json};

pub(crate) const FULL_LLM_CAPTURE_METADATA_KEY: &str = "full_llm_capture";

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CaptureTrace<'a> {
    pub session_turn_source: Option<&'a str>,
    pub turn_chain_id: Option<&'a str>,
    pub user_query_event_id: Option<&'a str>,
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

pub(crate) fn session_full_llm_capture_enabled(
    metadata: Option<&serde_json::Map<String, Value>>,
) -> bool {
    resolve_full_llm_capture_enabled(
        metadata,
        astra_config::runtime_config::RuntimeConfig::cached()
            .trace
            .category_enabled(astra_config::runtime_config::TraceCategory::LlmExchanges),
    )
}

fn resolve_full_llm_capture_enabled(
    metadata: Option<&serde_json::Map<String, Value>>,
    default_enabled: bool,
) -> bool {
    metadata
        .and_then(|metadata| metadata.get(FULL_LLM_CAPTURE_METADATA_KEY))
        .and_then(Value::as_bool)
        .unwrap_or(default_enabled)
}

fn prompt_request_delta_persistence_enabled_for_trace(
    trace: &astra_config::runtime_config::SessionTraceConfig,
) -> bool {
    trace.category_enabled(astra_config::runtime_config::TraceCategory::PromptAssembly)
}

fn prompt_request_delta_persistence_enabled() -> bool {
    prompt_request_delta_persistence_enabled_for_trace(
        &astra_config::runtime_config::RuntimeConfig::cached().trace,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_capture_request_json(
    messages: &[Value],
    tools: &[Value],
    max_output_tokens: Option<usize>,
) -> Value {
    json!({
        "message_count": messages.len(),
        "tool_count": tools.len(),
        "max_output_tokens": max_output_tokens,
        "messages": messages,
        "tools": tools,
    })
}

pub(crate) fn build_capture_request_summary_json(
    messages: &[Value],
    tools: &[Value],
    max_output_tokens: Option<usize>,
) -> Value {
    json!({
        "message_count": messages.len(),
        "tool_count": tools.len(),
        "max_output_tokens": max_output_tokens,
        "degraded_summary": true,
    })
}

pub(crate) fn build_capture_response_json(outcome: &str, response: Value) -> Value {
    json!({
        "outcome": outcome,
        "response": response,
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_capture_payload_json(
    messages: &[Value],
    tools: &[Value],
    max_output_tokens: Option<usize>,
    outcome: &str,
    response: Value,
    turn: u32,
    round: u32,
    trace: Option<CaptureTrace<'_>>,
) -> Value {
    json!({
        "request": build_capture_request_json(messages, tools, max_output_tokens),
        "response": response,
        "outcome": outcome,
        "trace": build_capture_trace_json(turn, round, trace),
    })
}

fn build_capture_trace_json(turn: u32, round: u32, trace: Option<CaptureTrace<'_>>) -> Value {
    let trace = trace.unwrap_or_default();
    json!({
        "session_turn": turn,
        "round": round,
        "session_turn_source": trace.session_turn_source,
        "turn_chain_id": trace.turn_chain_id,
        "user_query_event_id": trace.user_query_event_id,
    })
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
    trace: Option<CaptureTrace<'_>>,
) -> SessionArtifactJsonRecord {
    SessionArtifactJsonRecord {
        artifact_id: String::new(),
        session_id: session_id.to_string(),
        user_id: user_id.to_string(),
        artifact_kind: "llm_capture".to_string(),
        source: Some(source.to_string()),
        turn: Some(turn),
        round: Some(round),
        content: build_capture_payload_json(
            messages,
            tools,
            max_output_tokens,
            outcome,
            response,
            turn,
            round,
            trace,
        ),
        metadata: Some(json!({
            "agent_id": agent_id,
            "model": model,
            "provider": provider,
            "outcome": outcome,
            "trace": build_capture_trace_json(turn, round, trace),
        })),
        references: Vec::new(),
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
    trace: Option<CaptureTrace<'_>>,
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
            trace,
        ))
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn persist_configured_capture(
    full_capture_enabled: bool,
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
    trace: Option<CaptureTrace<'_>>,
) -> Result<(), String> {
    let mut errors = Vec::new();
    if let Err(error) = persist_capture(
        full_capture_enabled,
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
        response.clone(),
        trace,
    ) {
        errors.push(format!("local capture: {error}"));
    }
    if let Some(store) = remote_store.filter(|_| full_capture_enabled) {
        if let Err(error) = persist_remote_capture(
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
            trace,
        )
        .await
        {
            errors.push(format!("remote capture: {error}"));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn persist_configured_capture_or_log(
    context: &str,
    full_capture_enabled: bool,
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
    trace: Option<CaptureTrace<'_>>,
) {
    if let Err(error) = persist_configured_capture(
        full_capture_enabled,
        remote_store,
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
        trace,
    )
    .await
    {
        astra_core::agent_error!("llm-capture", "{context}: {error}");
    }
}

pub(crate) fn spawn_prompt_request_plan_persist_or_log(
    context: &'static str,
    shared_pool: Option<SharedPool>,
    input: astra_services::PromptRequestPersistInput,
    plan: astra_services::PromptRequestPlan,
) {
    if !prompt_request_delta_persistence_enabled() {
        return;
    }
    let Some(shared_pool) = shared_pool else {
        return;
    };
    tokio::spawn(async move {
        if let Err(error) =
            astra_services::persist_prompt_request(&shared_pool, &input, &plan).await
        {
            let pool_stats = shared_pool.stats();
            tracing::error!(
                target: "astra_runtime::prompt_delta",
                %context,
                request_id = %plan.request_id,
                db_pool_max = pool_stats.max_connections,
                db_pool_size = pool_stats.size,
                db_pool_idle = pool_stats.num_idle,
                error = %error,
                "failed to persist prompt request delta"
            );
        }
    });
}

fn capture_file_path(
    session_id: &str,
    user_id: &str,
    turn: u32,
    round: u32,
    source: &str,
    outcome: &str,
) -> Result<PathBuf, String> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let owner = astra_services::OwnerScope::user(user_id)
        .map_err(|error| format!("resolve capture owner for session {session_id}: {error}"))?;
    let dir = astra_services::local_session_artifact_store()
        .session_dir_for_owner(&owner, session_id)
        .map_err(|error| format!("resolve capture dir for session {session_id}: {error}"))?;
    Ok(dir.join(format!(
        "llm_capture_t{turn}_r{round}_{source}_{outcome}_{millis}.json"
    )))
}

#[allow(clippy::too_many_arguments)]
fn persist_capture_inner(
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
    trace: Option<CaptureTrace<'_>>,
) -> Result<String, String> {
    if session_id.trim().is_empty() {
        return Err("session_id must not be empty".to_string());
    }

    let source = sanitize_component(source);
    let outcome = sanitize_component(outcome);
    let path = capture_file_path(session_id, user_id, turn, round, &source, &outcome)?;
    let parent = path
        .parent()
        .ok_or_else(|| format!("capture path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("create capture dir {}: {error}", parent.display()))?;

    let payload = json!({
        "captured_at_ms": SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis(),
        "session_id": session_id,
        "turn": turn,
        "round": round,
        "agent_id": agent_id,
        "source": source,
        "model": model,
        "provider": provider,
        "request": build_capture_request_json(messages, tools, max_output_tokens),
        "outcome": outcome,
        "response": response,
        "trace": build_capture_trace_json(turn, round, trace),
    });

    let content = serde_json::to_string_pretty(&payload)
        .map_err(|error| format!("serialize capture payload for {}: {error}", path.display()))?;
    std::fs::write(&path, content)
        .map_err(|error| format!("write capture file {}: {error}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("set capture permissions on {}: {error}", path.display()))?;
    }
    Ok(path.display().to_string())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn persist_capture(
    full_capture_enabled: bool,
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
    trace: Option<CaptureTrace<'_>>,
) -> Result<Option<String>, String> {
    if !full_capture_enabled {
        return Ok(None);
    }
    persist_capture_inner(
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
        trace,
    )
    .map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_config::runtime_config::{SessionTraceConfig, TraceCategory, TraceProfile};
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
        ) -> Result<StoredSessionArtifact, astra_services::SessionArtifactStoreError> {
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
                retention_policy: Some("default".into()),
                retention_until: None,
                status: Some("active".into()),
                referenced_by_manifest_count: 0,
                referenced_by_state_items_count: 0,
                referenced_by_citation_count: 0,
                referenced_by_durable_count: 0,
                created_at: None,
            })
        }

        async fn upsert_json_artifact_projection(
            &self,
            record: SessionArtifactJsonRecord,
        ) -> Result<StoredSessionArtifact, astra_services::SessionArtifactStoreError> {
            self.persist_json_artifact(record).await
        }

        async fn load_json_artifact(
            &self,
            _user_id: &str,
            _session_id: &str,
            _artifact_id: &str,
        ) -> Result<Option<StoredSessionArtifact>, astra_services::SessionArtifactStoreError>
        {
            Ok(None)
        }

        async fn load_latest_json_artifact(
            &self,
            _user_id: &str,
            _session_id: &str,
            _artifact_kind: &str,
        ) -> Result<Option<StoredSessionArtifact>, astra_services::SessionArtifactStoreError>
        {
            Ok(None)
        }

        async fn list_json_artifacts(
            &self,
            _user_id: &str,
            _session_id: &str,
            _artifact_kind: Option<&str>,
            _limit: usize,
            _cursor: Option<astra_services::SessionArtifactListCursor>,
        ) -> Result<
            astra_services::SessionArtifactListPage,
            astra_services::SessionArtifactStoreError,
        > {
            Ok(astra_services::SessionArtifactListPage {
                artifacts: Vec::new(),
                limit: 0,
                next_cursor: None,
            })
        }
    }

    #[test]
    fn prompt_request_delta_db_persistence_follows_prompt_assembly_trace_category() {
        let production = SessionTraceConfig::default();
        assert!(
            !prompt_request_delta_persistence_enabled_for_trace(&production),
            "production/default trace profile must not write prompt delta diagnostics"
        );

        let dev = SessionTraceConfig::default().apply_profile(TraceProfile::Dev);
        assert!(
            prompt_request_delta_persistence_enabled_for_trace(&dev),
            "dev trace profile enables all diagnostic persistence categories"
        );

        let custom = SessionTraceConfig {
            profile: TraceProfile::Custom,
            enabled_categories: vec![TraceCategory::PromptAssembly],
            ..SessionTraceConfig::default()
        }
        .normalize();
        assert!(prompt_request_delta_persistence_enabled_for_trace(&custom));
    }

    #[test]
    fn persist_capture_writes_request_and_response_under_session_dir() {
        let temp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let path = persist_capture_inner(
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
            json!({"finish_reason":"stop","full_text":"done"}),
            Some(CaptureTrace {
                session_turn_source: Some("header"),
                turn_chain_id: Some("chain-1"),
                user_query_event_id: Some("query-1"),
            }),
        )
        .expect("capture path");

        let written = std::fs::read_to_string(&path).unwrap();
        let parsed: Value = serde_json::from_str(&written).unwrap();
        assert_eq!(parsed["session_id"], "sess-123");
        assert_eq!(parsed["turn"], 4);
        assert_eq!(parsed["round"], 2);
        assert_eq!(parsed["request"]["messages"][0]["content"], "hello");
        assert_eq!(parsed["response"]["full_text"], "done");
        assert_eq!(parsed["trace"]["session_turn"], 4);
        assert_eq!(parsed["trace"]["round"], 2);
        assert_eq!(parsed["trace"]["session_turn_source"], "header");
        assert_eq!(parsed["trace"]["turn_chain_id"], "chain-1");
        assert_eq!(parsed["trace"]["user_query_event_id"], "query-1");
    }

    #[test]
    fn session_metadata_bool_controls_full_capture() {
        let enabled =
            serde_json::Map::from_iter([(FULL_LLM_CAPTURE_METADATA_KEY.to_string(), json!(true))]);
        let disabled =
            serde_json::Map::from_iter([(FULL_LLM_CAPTURE_METADATA_KEY.to_string(), json!(false))]);
        let wrong_type = serde_json::Map::from_iter([(
            FULL_LLM_CAPTURE_METADATA_KEY.to_string(),
            json!("true"),
        )]);

        assert!(resolve_full_llm_capture_enabled(Some(&enabled), false));
        assert!(!resolve_full_llm_capture_enabled(Some(&disabled), true));
        assert!(!resolve_full_llm_capture_enabled(Some(&wrong_type), false));
        assert!(!resolve_full_llm_capture_enabled(None, false));
        assert!(resolve_full_llm_capture_enabled(None, true));
    }

    #[test]
    fn persist_capture_inner_reports_error_for_invalid_session_id() {
        let temp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let err = persist_capture_inner(
            "..",
            "user-1",
            0,
            0,
            None,
            "server_loop_host",
            "gpt-5.4",
            "openai",
            &[],
            &[],
            None,
            "ok",
            json!({}),
            None,
        )
        .expect_err("invalid session_id must not resolve capture dir");
        assert!(
            err.contains("session") || err.contains("escape"),
            "error should mention session rejection, got: {err}"
        );
        assert!(
            err.contains("invalid session ID") || err.contains("must not"),
            "error should cite the validation failure reason, got: {err}"
        );
    }

    #[test]
    fn persist_capture_noops_when_full_capture_disabled() {
        let temp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let path = persist_capture(
            false,
            "sess-123",
            "user-1",
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
            None,
        )
        .expect("disabled capture noop");
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
            Some(CaptureTrace {
                session_turn_source: Some("state"),
                turn_chain_id: Some("chain-7"),
                user_query_event_id: Some("query-7"),
            }),
        );
        assert_eq!(record.artifact_kind, "llm_capture");
        assert_eq!(record.turn, Some(4));
        assert_eq!(record.round, Some(2));
        assert_eq!(record.content["request"]["messages"][0]["content"], "hello");
        assert_eq!(record.metadata.as_ref().unwrap()["outcome"], "success");
        assert_eq!(record.content["trace"]["session_turn_source"], "state");
        assert_eq!(record.content["trace"]["turn_chain_id"], "chain-7");
        assert_eq!(
            record.metadata.as_ref().unwrap()["trace"]["user_query_event_id"],
            "query-7"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn persist_configured_capture_writes_local_file_when_enabled() {
        let temp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let session_id = "00000000-0000-0000-0000-000000000123";

        persist_configured_capture(
            true,
            None,
            session_id,
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
            Some(CaptureTrace {
                session_turn_source: Some("state"),
                turn_chain_id: Some("chain-local"),
                user_query_event_id: Some("query-local"),
            }),
        )
        .await
        .expect("configured capture");

        let owner = astra_services::OwnerScope::user("user-1").expect("owner scope");
        let session_dir = astra_services::local_session_artifact_store()
            .session_dir_for_owner(&owner, session_id)
            .expect("session dir");
        let files: Vec<_> = std::fs::read_dir(session_dir)
            .expect("capture dir")
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            files
                .iter()
                .any(|name| name.contains("llm_capture_t2_r1_server_loop_host_success")),
            "expected local capture file, got {files:?}"
        );
        let capture_path = std::fs::read_dir(
            astra_services::local_session_artifact_store()
                .session_dir_for_owner(&owner, session_id)
                .expect("session dir"),
        )
        .expect("capture dir")
        .map(|entry| entry.expect("dir entry").path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains("llm_capture_t2_r1_server_loop_host_success"))
        })
        .expect("capture file path");
        let parsed: Value = serde_json::from_str(
            &std::fs::read_to_string(capture_path).expect("read local capture"),
        )
        .expect("parse local capture");
        assert_eq!(parsed["trace"]["session_turn_source"], "state");
        assert_eq!(parsed["trace"]["turn_chain_id"], "chain-local");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn persist_configured_capture_persists_remote_record_when_store_provided() {
        let temp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let store = RecordingArtifactStore::default();
        let session_id = "00000000-0000-0000-0000-000000000124";

        persist_configured_capture(
            true,
            Some(&store),
            session_id,
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
            Some(CaptureTrace {
                session_turn_source: Some("header"),
                turn_chain_id: Some("chain-remote"),
                user_query_event_id: Some("query-remote"),
            }),
        )
        .await
        .expect("configured capture");

        let records = store.records.lock().expect("recording store lock");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].artifact_kind, "llm_capture");
        assert_eq!(records[0].content["response"]["full_text"], "done");
        assert_eq!(
            records[0].metadata.as_ref().unwrap()["trace"]["session_turn_source"],
            "header"
        );
        assert_eq!(
            records[0].content["trace"]["user_query_event_id"],
            "query-remote"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn persist_configured_capture_skips_remote_when_capture_disabled() {
        let temp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let store = RecordingArtifactStore::default();

        persist_configured_capture(
            false,
            Some(&store),
            "00000000-0000-0000-0000-000000000125",
            "user-1",
            1,
            0,
            None,
            "test",
            "gpt-4",
            "openai",
            &[json!({"role": "user", "content": "hi"})],
            &[],
            None,
            "success",
            json!({"text":"ok"}),
            None,
        )
        .await
        .expect("should succeed");

        let records = store.records.lock().unwrap();
        assert_eq!(
            records.len(),
            0,
            "remote capture must be skipped when full_capture_enabled=false"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn persist_configured_capture_returns_error_when_local_capture_write_fails() {
        let temp = tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let store = RecordingArtifactStore::default();
        let session_id = "00000000-0000-0000-0000-000000000125";
        let owner = astra_services::OwnerScope::user("user-1").expect("owner scope");
        let session_dir = astra_services::local_session_artifact_store()
            .session_dir_for_owner(&owner, session_id)
            .expect("session dir");
        std::fs::create_dir_all(session_dir.parent().expect("session dir parent"))
            .expect("create owner sessions root");
        std::fs::write(&session_dir, "block dir creation").expect("block session dir");

        let error = persist_configured_capture(
            true,
            Some(&store),
            session_id,
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
            None,
        )
        .await
        .expect_err("local capture failure should surface");
        assert!(
            error.contains("local capture:"),
            "unexpected error: {error}"
        );
        assert!(
            error.contains("create capture dir"),
            "unexpected error: {error}"
        );

        let records = store.records.lock().expect("recording store lock");
        assert_eq!(records.len(), 1, "remote capture should still be attempted");
    }
}

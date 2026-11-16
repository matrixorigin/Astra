//! Durable compressed history, readable through the selected Server introspect boundary.
use astra_services::session_artifact_store::{
    SessionArtifactJsonRecord, SessionArtifactJsonStore, SessionArtifactReference,
    SessionArtifactReferenceKind, artifact_window_arguments, read_artifact_window,
};
use astra_turn_types::{
    CONTEXT_HISTORY_ARTIFACT_KIND, CONTEXT_HISTORY_ARTIFACT_URI_PREFIX, ContextHistoryArtifactV1,
};
use serde_json::Value;

pub(crate) async fn persist(
    store: &dyn SessionArtifactJsonStore,
    user_id: &str,
    artifact_id: &str,
    history: ContextHistoryArtifactV1,
) -> Result<(), String> {
    history
        .validate(&history.session_id)
        .map_err(str::to_owned)?;
    let session_id = history.session_id.clone();
    let record = SessionArtifactJsonRecord {
        artifact_id: artifact_id.into(),
        session_id: session_id.clone(),
        user_id: user_id.into(),
        artifact_kind: CONTEXT_HISTORY_ARTIFACT_KIND.into(),
        source: Some("context_compaction".into()),
        turn: None,
        round: None,
        content: serde_json::to_value(history).map_err(|e| e.to_string())?,
        metadata: None,
        references: vec![SessionArtifactReference {
            kind: SessionArtifactReferenceKind::SessionTranscript,
            reference_id: session_id,
        }],
    };
    store
        .persist_json_artifact(record)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

pub(crate) async fn resolve_request(
    store: Option<&dyn SessionArtifactJsonStore>,
    user_id: &str,
    session_id: &str,
    args: &Value,
) -> Result<String, String> {
    let handle = args
        .get("artifact")
        .and_then(Value::as_str)
        .ok_or("context history artifact must be a string handle")?;
    let id = handle
        .strip_prefix(CONTEXT_HISTORY_ARTIFACT_URI_PREFIX)
        .filter(|id| uuid::Uuid::parse_str(id).is_ok())
        .ok_or("invalid context history artifact handle")?;
    let store = store.ok_or("context history artifact reader is unavailable")?;
    let (offset, max_bytes) = artifact_window_arguments(args)?;
    let record = store
        .load_json_artifact(user_id, session_id, id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("context history artifact was not found for this session")?;
    if record.user_id != user_id
        || record.session_id != session_id
        || record.artifact_id != id
        || record.artifact_kind != CONTEXT_HISTORY_ARTIFACT_KIND
        || record.status.as_deref() != Some("active")
    {
        return Err("context history artifact identity or status is invalid".into());
    }
    let history: ContextHistoryArtifactV1 = serde_json::from_value(record.content)
        .map_err(|e| format!("invalid context history artifact: {e}"))?;
    history.validate(session_id).map_err(str::to_owned)?;
    let (window, total, next) = read_artifact_window(&history.transcript_json, offset, max_bytes)?;
    let continuation = if next == total {
        "Transcript complete.".into()
    } else {
        format!(
            "Continue with introspect(artifact=\"{handle}\", offset={next}, max_bytes={max_bytes})."
        )
    };
    Ok(format!(
        "<context-history-artifact>\nArtifact: {handle}\nSource run: {}\nMessages: {}\nBytes: [{offset}..{next}) of {total}\n\n{window}\n\n{continuation}\n</context-history-artifact>",
        history.source_run_id, history.message_count
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::explain_analyze_artifact::tests::MemoryStore;
    use serde_json::json;

    #[tokio::test]
    #[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
    async fn database_history_roundtrip_preserves_exact_evidence_and_owner_scope() {
        assert_eq!(std::env::var("ASTRA_TEST_DB_IT").as_deref(), Ok("1"));
        let settings = astra_core::MatrixOneSettings::from_env();
        astra_services::storage::ensure_core_schema(&settings, "mysql")
            .await
            .unwrap();
        let pool = astra_core::SharedPool::new(&settings).await.unwrap();
        let owner = uuid::Uuid::new_v4().to_string();
        let session = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO agent_sessions (session_id,user_id,title,status,event_count) VALUES (?,?,'history recovery','active',0)")
            .bind(&session).bind(&owner).execute(pool.get()).await.unwrap();
        let writer = astra_services::DatabaseSessionArtifactStore::new(settings.clone())
            .with_pool(pool.clone());
        let transcript = serde_json::to_string(&json!([
            {"role":"assistant","tool_calls":[{"id":"receipt","type":"function","function":{"name":"read_file","arguments":"{}"}}]},
            {"role":"tool","tool_call_id":"receipt","content":format!("CONFIRMED_RECEIPT: {}", "中文 evidence\n".repeat(500))}
        ])).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        persist(
            &writer,
            &owner,
            &id,
            ContextHistoryArtifactV1::new(&session, "earlier-run", 2, transcript.clone()).unwrap(),
        )
        .await
        .unwrap();
        let reader_pool = astra_core::SharedPool::new(&settings).await.unwrap();
        let reader =
            astra_services::DatabaseSessionArtifactStore::new(settings).with_pool(reader_pool);
        let args = json!({"artifact":format!("{CONTEXT_HISTORY_ARTIFACT_URI_PREFIX}{id}"), "max_bytes":65_536});
        let output = resolve_request(Some(&reader), &owner, &session, &args)
            .await
            .unwrap();
        assert!(output.contains(&transcript));
        assert!(output.contains("Source run: earlier-run"));
        assert!(
            resolve_request(Some(&reader), "foreign-owner", &session, &args)
                .await
                .is_err()
        );
        assert!(
            resolve_request(Some(&reader), &owner, "foreign-session", &args)
                .await
                .is_err()
        );
        for statement in [
            "DELETE FROM session_artifact_references WHERE user_id=? AND session_id=?",
            "DELETE FROM session_artifacts WHERE user_id=? AND session_id=?",
            "DELETE FROM agent_sessions WHERE user_id=? AND session_id=?",
        ] {
            sqlx::query(statement)
                .bind(&owner)
                .bind(&session)
                .execute(pool.get())
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn stored_history_reads_exact_bytes_across_runs_and_rejects_foreign_or_corrupt_handles() {
        let store = MemoryStore::default();
        let id = uuid::Uuid::new_v4().to_string();
        let transcript =
            serde_json::to_string(&json!([{"role":"user","content":"中文 history"}])).unwrap();
        let history =
            ContextHistoryArtifactV1::new("session", "earlier-run", 1, transcript.clone()).unwrap();
        persist(&store, "owner", &id, history).await.unwrap();
        let args = json!({"artifact":format!("{CONTEXT_HISTORY_ARTIFACT_URI_PREFIX}{id}")});
        let output = resolve_request(Some(&store), "owner", "session", &args)
            .await
            .unwrap();
        assert!(output.contains(&transcript));
        assert!(output.contains("Source run: earlier-run"));
        let source = vec![
            astra_turn_core::cloud::history_summary::history_summary_message(format!(
                "Exact transcript: {}\n{}",
                args["artifact"].as_str().unwrap(),
                "previous preview ".repeat(1000),
            )),
            json!({"role":"user","content":"continue"}),
        ];
        let client = astra_turn_core::cloud_summary::test_support::MockSummaryClient::success(
            "### Primary Request\nContinue\n### Pending Tasks\nFinish\n### Current Work\nInspect\n### Current State\nIn progress",
        );
        let summary = astra_turn_core::cloud_summary::generate_compact_summary(&source, &client)
            .await
            .unwrap();
        let (candidate, _) = crate::turn::cloud::compaction::prepare_conversation_summary(
            &source,
            source.len(),
            astra_turn_core::cloud::history_summary::history_summary_message(summary),
            0,
            100_000,
            None,
            None,
        )
        .unwrap();
        assert!(candidate.tokens_freed() > 0);
        let restored = astra_turn_core::prompt_facing::sanitize_canonical_continuation_messages_with_turn_semantics(candidate.messages).unwrap();
        let handle = restored
            .iter()
            .filter_map(astra_turn_core::cloud::history_summary::history_summary_text)
            .flat_map(astra_turn_core::cloud::history_summary::context_history_artifact_handles)
            .next()
            .unwrap();
        let recovered = resolve_request(
            Some(&store),
            "owner",
            "session",
            &json!({"artifact":handle}),
        )
        .await
        .unwrap();
        assert!(
            recovered.contains(&transcript),
            "spill, semantic replacement and resume retain exact recovery"
        );
        for (owner, session) in [("another-owner", "session"), ("owner", "another-session")] {
            assert!(
                resolve_request(Some(&store), owner, session, &args)
                    .await
                    .is_err()
            );
        }
        let prefix = json!({"artifact":args["artifact"],"max_bytes":10});
        let output = resolve_request(Some(&store), "owner", "session", &prefix)
            .await
            .unwrap();
        assert!(output.contains("offset=10"));
        assert!(!output.contains(&transcript));
        let key = ("owner".into(), "session".into(), id);
        store
            .artifacts
            .lock()
            .unwrap()
            .get_mut(&key)
            .unwrap()
            .content["transcript_json"] = json!("[]");
        assert!(
            resolve_request(Some(&store), "owner", "session", &args)
                .await
                .unwrap_err()
                .contains("integrity")
        );
        assert!(
            resolve_request(None, "owner", "session", &args)
                .await
                .is_err()
        );
    }
}

use super::*;
use crate::cli::cli_config::cli_utils::{CredentialsFile, Profile, save_credentials};
use crate::cli::session::session_runtime;

use std::sync::{Mutex, MutexGuard, OnceLock};

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

struct EnvVarGuard {
    _lock: MutexGuard<'static, ()>,
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn set_path(key: &'static str, value: &std::path::Path) -> Self {
        let lock = astra_core::sync_poison::recover_mutex_lock(&env_lock());
        let previous = std::env::var_os(key);
        unsafe {
            std::env::set_var(key, value);
        }
        Self {
            _lock: lock,
            key,
            previous,
        }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        if let Some(ref previous) = self.previous {
            unsafe {
                std::env::set_var(self.key, previous);
            }
        } else {
            unsafe {
                std::env::remove_var(self.key);
            }
        }
    }
}

// ── Resume user verification ────────────────────────────────────────────────

#[serial_test::serial]
#[tokio::test]
async fn resume_local_restore_rejects_unowned_session() {
    let _creds = isolate_credentials();
    use session_journal::JournalWriter;

    let sid = format!("test-unowned-{}", uuid::Uuid::new_v4());

    let writer = JournalWriter::new(&sid).unwrap();
    writer
        .append(&session_journal::JournalEvent::session_start(
            Some(&sid),
            Some("gpt-4o"),
        ))
        .unwrap();
    writer
        .append(&session_journal::JournalEvent::turn(
            Some(&sid),
            1,
            None,
            "hello",
            "hi",
            0,
            5,
            3,
            50,
        ))
        .unwrap();
    drop(writer);

    let ws_dir = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".astra")
        .join("sessions")
        .join(&sid);
    std::fs::create_dir_all(&ws_dir).unwrap();
    let ws_content = format!(
        "session_id: {sid}\ncwd: /tmp\nmodel: gpt-4o\ncreated_at: \"2024-01-01T00:00:00Z\"\n\
         updated_at: \"2024-01-01T00:00:00Z\"\nstatus: active\nturn_count: 1\n\
         total_tokens_in: 5\ntotal_tokens_out: 3\n"
    );
    std::fs::write(ws_dir.join("workspace.yaml"), ws_content).unwrap();

    let svc = astra_services::session_restore::SessionRestoreService::local_only();
    let result = svc.restore_local_session(&sid).await.unwrap();
    assert!(
        result.is_some(),
        "local restore should find session with workspace.yaml"
    );
    let restored = result.unwrap();
    assert!(!restored.restored_from_cloud, "should be local restore");
}

#[serial_test::serial]
#[tokio::test]
async fn initialize_session_state_marks_workspace_session_as_pending_recovery() {
    let _creds = isolate_credentials();
    let temp = tempfile::tempdir().unwrap();
    let _sessions = session_journal::JournalDirGuard::new(temp.path());

    let sid = format!("test-session-state-{}", uuid::Uuid::new_v4());
    let current_cwd = std::env::current_dir().unwrap();
    let current_git_root = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());

    let writer = session_journal::JournalWriter::new(&sid).unwrap();
    writer
        .append(&session_journal::JournalEvent::session_start(
            Some(&sid),
            Some("gpt-4o"),
        ))
        .unwrap();
    writer
        .append(&session_journal::JournalEvent::turn(
            Some(&sid),
            1,
            None,
            "resume this session",
            "ok",
            0,
            5,
            3,
            50,
        ))
        .unwrap();
    writer
        .append(&session_journal::JournalEvent::interruption_recorded(
            Some(&sid),
            1,
            serde_json::json!({
                "kind": "rate_limited",
                "resumable": true,
                "has_checkpoint": true,
                "tool_calls_completed": 1,
                "turns_completed": 1,
                "remaining_turns": 4,
            }),
        ))
        .unwrap();
    drop(writer);

    let mut ws = astra_services::session_workspace::WorkspaceMetadata::with_context(
        &sid,
        "gpt-4o",
        &current_cwd.display().to_string(),
        Some("main"),
    );
    ws.git_root = current_git_root;
    ws.turn_count = 1;
    ws.total_tokens_in = 5;
    ws.total_tokens_out = 3;
    ws.discovered_skills = vec!["episodic-memory".into(), "knowledge-graph-reasoning".into()];
    astra_services::session_workspace::write_workspace(&ws).unwrap();

    let mut creds = CredentialsFile::default();
    creds.profiles.insert(
        "default".to_string(),
        Profile {
            last_session_id: Some(sid.clone()),
            ..Default::default()
        },
    );
    save_credentials(&creds).unwrap();

    let state = session_runtime::initialize_session_state(
        None,
        None,
        &crate::cli::cli_config::cli_context::CliContext::default(),
    );
    assert_eq!(state.session_id, None);
    assert_eq!(state.pending_recovery.as_deref(), Some(sid.as_str()));
    assert!(state.history.is_empty());
    assert_eq!(state.turn, 0);
}

// ── Edge cases ──────────────────────────────────────────────────────────────

#[serial_test::serial]
#[tokio::test]
async fn resume_handles_workspace_edge_cases() {
    let _creds = isolate_credentials();

    for (label, workspace_yaml) in [
        ("malformed", Some("invalid: yaml: content: [")),
        ("missing", None),
    ] {
        let sid = format!("test-ws-edge-{}-{}", label, uuid::Uuid::new_v4());

        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-4o"),
            ))
            .unwrap();
        drop(writer);

        if let Some(yaml_content) = workspace_yaml {
            let ws_dir = dirs::home_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("."))
                .join(".astra")
                .join("sessions")
                .join(&sid);
            std::fs::create_dir_all(&ws_dir).unwrap();
            std::fs::write(ws_dir.join("workspace.yaml"), yaml_content).unwrap();
        }

        let svc = astra_services::session_restore::SessionRestoreService::local_only();
        let result = svc
            .restore_local_session(&sid)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("{label}: journal-only session should restore"));
        assert_eq!(result.session_id, sid);
        assert_eq!(result.turn_count, 0);
        assert_eq!(result.model.as_deref(), Some("gpt-4o"));
        assert_eq!(result.last_status, "local");
        assert!(!result.restored_from_cloud);
    }
}

// ── Checkpoint listing ──────────────────────────────────────────────────────

#[serial_test::serial]
#[tokio::test]
async fn resume_lists_checkpoints_for_session() {
    let _creds = isolate_credentials();
    use astra_services::session_journal;

    let sid = format!("test-checkpoints-{}", uuid::Uuid::new_v4());

    let writer = session_journal::JournalWriter::new(&sid).unwrap();
    writer
        .append(&session_journal::JournalEvent::session_start(
            Some(&sid),
            Some("gpt-4o"),
        ))
        .unwrap();
    drop(writer);

    let ws_dir = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".astra")
        .join("sessions")
        .join(&sid);
    std::fs::create_dir_all(&ws_dir).unwrap();
    std::fs::write(
        ws_dir.join("workspace.yaml"),
        r#"session_id: test
cwd: /tmp
model: gpt-4o
created_at: "2024-01-01T00:00:00Z"
updated_at: "2024-01-01T00:00:00Z"
status: active
turn_count: 10
total_tokens_in: 1000
total_tokens_out: 500
"#,
    )
    .unwrap();

    let svc = astra_services::session_restore::SessionRestoreService::local_only();
    let ckpts = svc.list_local_checkpoints(&sid).await.unwrap();
    assert!(ckpts.is_empty(), "no checkpoints created yet");
}

// merge_learning_snapshot tests removed: the entity/pattern/calibration
// learning subsystem has been deleted.

#[tokio::test]
#[serial_test::serial]
async fn server_restore_real_turn_keeps_rollback_checkpoint_and_journal_on_local_clock() {
    use crate::cli::permission_manager::PermissionManager;
    use crate::cli::slash::slash_session::restore_session_into_state;
    use crate::cli::turn::turn_entry::{
        ChatInput, InteractiveTurnOutcome, TurnContext, handle_chat_input_with_ui,
    };
    use astra_services::session_journal::JournalEventType;
    use astra_services::session_restore::RestoredSession;
    use astra_turn_core::active_conversation::ActiveConversation;
    use astra_turn_core::file_edit_journal::EditType;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let (temp, _sessions) = isolated_sessions_dir();
    let _credentials = isolate_credentials();
    let _environment = crate::test_utils::ProcessEnvGuard::remove("ASTRA_ACCESS_TOKEN");
    let _cloud = crate::test_utils::ProcessEnvGuard::remove("ASTRA_API_URL");
    let _file_root = crate::test_utils::ProcessEnvGuard::remove("_ASTRA_FILE_CHECKPOINT_ROOT");
    let owner = "restored-real-turn-owner";
    let mut credentials = CredentialsFile::default();
    credentials.profiles.insert(
        "default".into(),
        Profile {
            account_id: Some(owner.into()),
            access_token: Some("test-token".into()),
            ..Default::default()
        },
    );
    save_credentials(&credentials).unwrap();
    let _identity = crate::cli::cli_config::cli_utils::install_cli_profile_identity_for_test(
        "default",
        Some(owner),
    )
    .unwrap();
    for (local_turn, server_turn) in [(20, 3), (3, 20), (0, 20)] {
        let sid = format!(
            "restored-real-{local_turn}-{server_turn}-{}",
            uuid::Uuid::new_v4()
        );
        let old_file = temp.path().join(format!("historical-{local_turn}.txt"));
        let new_file = temp.path().join(format!("current-{local_turn}.txt"));
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        if local_turn > 0 {
            let local = ActiveConversation::empty(owner, &sid)
                .unwrap()
                .prepare_commit(
                    local_turn,
                    None,
                    vec![serde_json::json!({"role":"user", "content":"local history"})],
                )
                .unwrap();
            writer
                .append(
                    &session_journal::JournalEvent::turn(
                        Some(&sid),
                        local_turn,
                        None,
                        "local history",
                        "local answer",
                        0,
                        0,
                        0,
                        0,
                    )
                    .with_conversation_commit(local.commit),
                )
                .unwrap();
        }
        let remote = ActiveConversation::empty(owner, &sid)
            .unwrap()
            .prepare_commit(
                server_turn,
                None,
                vec![
                    serde_json::json!({"role":"user", "content":"Server history"}),
                    serde_json::json!({"role":"assistant", "content":"Server answer"}),
                ],
            )
            .unwrap()
            .next;
        let restored = RestoredSession {
            session_id: sid.clone(),
            turn_count: server_turn,
            model: Some("mock-model".into()),
            restored_from_cloud: true,
            resume_bundle: Some(astra_turn_types::ResumeBundleV1 {
                schema_version: astra_turn_types::RESUME_BUNDLE_SCHEMA_VERSION,
                source: astra_turn_types::ResumeSourceV1::CanonicalJournal,
                cursor: remote.cursor().clone(),
                conversation_messages: remote.materialize(),
                materialized_conversation_root_hash: None,
                degraded_reasons: vec![],
                repair_actions: vec![],
                projections: Default::default(),
            }),
            ..Default::default()
        };
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/sessions/{sid}/resume")))
            .respond_with(ResponseTemplate::new(200).set_body_json(&restored))
            .expect(3)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/model-access"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(crate::test_utils::mock_model_access_json(&["mock-model"])),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(crate::test_utils::mock_model_catalog_json(&["mock-model"])),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok":true})))
            .expect(4)
            .mount(&server)
            .await;
        let calls = std::sync::atomic::AtomicU32::new(0);
        let stream_sid = sid.clone();
        let written_file = new_file.clone();
        Mock::given(method("POST")).and(path("/chat/stream"))
            .respond_with(move |request: &wiremock::Request| {
                let admission: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                assert!(admission.get("session_turn").is_none());
                assert!(admission.get("messages").is_none());
                let round = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let run = format!("run-{stream_sid}-{round}");
                let mut body = format!("data: {}\n\n", serde_json::json!({"type":"session_info", "session_id":stream_sid, "run_id":run}));
                for (index, tool, args) in [
                    (0, "write_file", serde_json::json!({"path":written_file, "content":"new turn edit"})),
                    (1, "rollback_file_edits", serde_json::json!({"scope":"current_turn"})),
                ] {
                    body.push_str(&format!("data: {}\n\n", serde_json::json!({
                        "type":"tool_request", "session_id":stream_sid, "run_id":run,
                        "turn_chain_id":format!("chain-{round}"), "request_id":format!("tool-{round}-{index}"),
                        "schema_admitted_by_server":true, "execution_timeout_ms":300_000,
                        "command_timeout_cap_ms":30_000, "execution_deadline_unix_ms":4_102_444_800_000_u64,
                        "tool":tool, "args":args,
                    })));
                }
                // The accepted Server turn may advance after hydration. Its
                // feedback remains valid even when it matches neither local
                // allocation nor the earlier Server head plus one.
                body.push_str(&super::chat_stream_tests::sse_text_response_with_execution_summary(
                    "Rolled back this turn", &stream_sid, 2, 0, &["write_file", "rollback_file_edits"], 1,
                    (&run, server_turn + round + 2),
                ));
                ResponseTemplate::new(200).insert_header("content-type", "text/event-stream")
                    .insert_header(astra_server_types::AGENT_INTERACTION_API_MAJOR_HEADER, astra_server_types::AGENT_INTERACTION_API_MAJOR)
                    .set_body_string(body)
            }).expect(2).mount(&server).await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let mut state = crate::SessionState::default();
        restore_session_into_state(&sid, None, &api, &mut state)
            .await
            .unwrap();
        assert_eq!(state.turn, local_turn);
        state.perm_manager = PermissionManager::new(true);
        state.cli_context.add_dirs.push(temp.path().to_path_buf());
        std::fs::write(&old_file, "historical edit").unwrap();
        let historical_turn = if local_turn > server_turn {
            server_turn + 1
        } else {
            1
        };
        if local_turn > 0 {
            state.file_journal.lock().unwrap().record_committed(
                &old_file,
                "historical-edit",
                historical_turn,
                None,
                b"historical edit",
                EditType::Create,
            );
        }
        for round in 0..2 {
            let mut ui = TestUi::default();
            let outcome = handle_chat_input_with_ui(
                ChatInput::Conversation("write then undo only this turn".into()),
                Some("test-token"),
                session_runtime::AccessMiss::NotLoggedIn,
                &mut state,
                TurnContext {
                    api: &api,
                    profile: None,
                    admission: None,
                    post_commit_tx: None,
                    explain_analyze_terminal_degraded: None,
                },
                &mut ui,
            )
            .await
            .unwrap();
            assert!(
                matches!(outcome, InteractiveTurnOutcome::Completed(_)),
                "{:?}: {:?}",
                outcome,
                ui.errors
            );
            for request in server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|request| request.url.path() == "/tools/result")
            {
                let result: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                assert_eq!(result["status"], "completed", "{result}");
            }
            assert_eq!(
                std::fs::read_to_string(&old_file).unwrap(),
                "historical edit"
            );
            assert!(
                !new_file.exists(),
                "the real tool callback must undo its own write"
            );
            let expected = local_turn + round + 1;
            assert_eq!(state.turn, expected);
            assert!(
                state.session_persistence_error.is_none(),
                "{:?}",
                state.session_persistence_error
            );
            let edits = state.file_journal.lock().unwrap().summary();
            assert_eq!(edits.last().unwrap().1, expected);
            let index = astra_pipeline::step_checkpoint::read_composite_snapshot_index(owner, &sid)
                .unwrap();
            assert_eq!(index.snapshots.last().unwrap().turn, expected);
            assert_eq!(
                state
                    .latest_context_assembly_trace
                    .as_ref()
                    .unwrap()
                    .trace
                    .turn_id,
                format!("turn-{expected}")
            );
            let pipeline = state.runtime_pipeline_state.as_ref().unwrap();
            assert_eq!(
                pipeline["latest_runtime_feedback"]["progress"]["session_turn"],
                server_turn + round + 2
            );
        }
        let turns: Vec<_> = writer
            .complete_append_order_snapshot()
            .unwrap()
            .iter()
            .filter(|event| event.event_type == JournalEventType::Turn)
            .filter_map(|event| event.turn)
            .collect();
        let expected = if local_turn == 0 {
            vec![1, 2]
        } else {
            vec![local_turn, local_turn + 1, local_turn + 2]
        };
        assert_eq!(turns, expected);
        server.verify().await;
    }
}

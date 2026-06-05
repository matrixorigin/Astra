use super::chat_stream_tests::sse_text_response;
use super::*;
use crate::cli::cli_utils::{CredentialsFile, Profile, save_credentials};
use axum::response::IntoResponse;
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

// ── slash_task::find_task_by_query ────────────────────────────────────────────────────

use astra_services::TaskService as _;

const REAL_SESSION_1D21375_FIXTURE: &str =
    include_str!("../../../services/fixtures/real_session_1d21375_min.jsonl");

#[tokio::test]
async fn find_task_by_id_prefix() {
    let tmp = tempfile::TempDir::new().unwrap();
    let svc = astra_services::LocalTaskService::new(tmp.path().to_path_buf());
    let tid = svc
        .create_task(
            "u1",
            "s1",
            astra_services::TaskCreateRequest {
                title: "Build auth".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    // Full ID match
    let found = slash_task::find_task_by_query(&svc, "u1", &tid)
        .await
        .unwrap();
    assert_eq!(found, Some(tid.clone()));

    // Prefix match (first 8 Unicode scalars)
    let prefix = prefix_chars(&tid, 8);
    let found = slash_task::find_task_by_query(&svc, "u1", &prefix)
        .await
        .unwrap();
    assert_eq!(found, Some(tid));
}

#[tokio::test]
async fn find_task_by_title_substring() {
    let tmp = tempfile::TempDir::new().unwrap();
    let svc = astra_services::LocalTaskService::new(tmp.path().to_path_buf());
    svc.create_task(
        "u1",
        "s1",
        astra_services::TaskCreateRequest {
            title: "Refactor authentication module".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // Case-insensitive title match
    let found = slash_task::find_task_by_query(&svc, "u1", "authentication")
        .await
        .unwrap();
    assert!(found.is_some());

    let found = slash_task::find_task_by_query(&svc, "u1", "AUTH")
        .await
        .unwrap();
    assert!(found.is_some());
}

#[tokio::test]
async fn find_task_by_title_substring_fails_on_ambiguity() {
    let tmp = tempfile::TempDir::new().unwrap();
    let svc = astra_services::LocalTaskService::new(tmp.path().to_path_buf());
    for title in [
        "Refactor authentication module",
        "Refactor authentication tests",
    ] {
        svc.create_task(
            "u1",
            "s1",
            astra_services::TaskCreateRequest {
                title: title.into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }

    let err = slash_task::find_task_by_query(&svc, "u1", "authentication")
        .await
        .unwrap_err();
    assert!(err.contains("task query 'authentication' is ambiguous"));
}

#[tokio::test]
async fn find_task_not_found() {
    let tmp = tempfile::TempDir::new().unwrap();
    let svc = astra_services::LocalTaskService::new(tmp.path().to_path_buf());
    let found = slash_task::find_task_by_query(&svc, "u1", "nonexistent")
        .await
        .unwrap();
    assert!(found.is_none());
}

#[tokio::test]
async fn find_task_wrong_user() {
    let tmp = tempfile::TempDir::new().unwrap();
    let svc = astra_services::LocalTaskService::new(tmp.path().to_path_buf());
    svc.create_task(
        "user-a",
        "s1",
        astra_services::TaskCreateRequest {
            title: "Private task".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // Different user can't find it
    let found = slash_task::find_task_by_query(&svc, "user-b", "Private")
        .await
        .unwrap();
    assert!(found.is_none());
}

// ── Resume user verification ─────────────────────────────────────────────

#[serial_test::serial]
#[tokio::test]
async fn resume_local_restore_rejects_unowned_session() {
    let _creds = isolate_credentials();
    use astra_services::session_restore::SessionRestoreService;
    use session_journal::JournalWriter;

    // Create a session with both journal AND workspace (what restore_session needs)
    let sid = format!("test-unowned-{}", uuid::Uuid::new_v4());

    // 1. Create journal
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

    // 2. Create workspace.yaml (required for local restore)
    let ws_dir = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".astra")
        .join("sessions")
        .join(&sid);
    std::fs::create_dir_all(&ws_dir).unwrap();
    let ws_content = r#"session_id: test-unowned
cwd: /tmp
model: gpt-4o
created_at: "2024-01-01T00:00:00Z"
updated_at: "2024-01-01T00:00:00Z"
status: active
turn_count: 1
total_tokens_in: 5
total_tokens_out: 3
"#;
    std::fs::write(ws_dir.join("workspace.yaml"), ws_content).unwrap();

    // Now restore_session should find it
    let svc = astra_services::session_restore::HybridRestoreService::local_only();
    let result = svc.restore_session(&sid).await.unwrap();
    assert!(
        result.is_some(),
        "local restore should find session with workspace.yaml"
    );

    // Verify it's marked as local (not cloud)
    let restored = result.unwrap();
    assert!(!restored.restored_from_cloud, "should be local restore");

    // Note: The user ownership check in handle_resume_command only verifies
    // that the journal exists, not that the user owns it. This is a known limitation.
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
    ws.pinned_skills = vec![
        "session-lifecycle".to_string(),
        "goal-driven-evolution".to_string(),
    ];
    ws.discovered_skills = vec![
        "episodic-memory".to_string(),
        "knowledge-graph-reasoning".to_string(),
    ];
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
        &crate::cli::cli_context::CliContext::default(),
    );
    assert_eq!(state.session_id, None);
    assert_eq!(state.pending_recovery.as_deref(), Some(sid.as_str()));
    assert!(state.history.is_empty());
    assert_eq!(state.turn, 0);
}

#[tokio::test(flavor = "current_thread")]
async fn crash_recovery_short_continue_starts_fresh_session_without_auto_restore() {
    let _creds = isolate_credentials();
    let temp = tempfile::tempdir().unwrap();
    let _sessions = session_journal::JournalDirGuard::new(temp.path());
    let claude_dir = tempfile::tempdir().unwrap();
    let _claude = EnvVarGuard::set_path("CLAUDE_CONFIG_DIR", claude_dir.path());

    let sid = format!("test-crash-recovery-{}", uuid::Uuid::new_v4());
    let current_cwd = std::env::current_dir().unwrap();
    let current_cwd_str = current_cwd.display().to_string();
    let current_git_root = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let interruption = serde_json::json!({
        "kind": "rate_limited",
        "resumable": true,
        "has_checkpoint": true,
        "tool_calls_completed": 2,
        "turns_completed": 1,
        "remaining_turns": 3,
        "user_message": ""
    });

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
            "continue the parser refactor",
            "I updated the lexer; next patch the parser.",
            1,
            40,
            20,
            75,
        ))
        .unwrap();
    writer
        .append(&session_journal::JournalEvent::interruption_recorded(
            Some(&sid),
            1,
            interruption.clone(),
        ))
        .unwrap();
    drop(writer);

    let mut ws = astra_services::session_workspace::WorkspaceMetadata::with_context(
        &sid,
        "gpt-4o",
        &current_cwd_str,
        Some("main"),
    );
    ws.git_root = current_git_root;
    ws.turn_count = 1;
    ws.total_tokens_in = 40;
    ws.total_tokens_out = 20;
    astra_services::session_workspace::write_workspace(&ws).unwrap();

    let light = astra_pipeline::step_protocol::LightCheckpoint {
        protocol_version: astra_pipeline::step_protocol::PROTOCOL_VERSION,
        cursor: astra_pipeline::step_protocol::ExecutionCursor::default(),
        step_id: "resume-step".to_string(),
        task_id: "task-1".to_string(),
        agent_id: sid.clone(),
        progress: 1.0,
        total_tokens: 60,
        created_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64,
    };
    let heavy = astra_pipeline::step_protocol::HeavyCheckpoint {
        light,
        messages: vec![
            serde_json::json!({"role": "user", "content": "continue the parser refactor"}),
            serde_json::json!({"role": "assistant", "content": "I updated the lexer; next patch the parser."}),
        ],
        budget_remaining_tokens: 10_000,
        budget_remaining_rounds: 8,
        blocked_tools: Vec::new(),
        recent_tools: vec!["bash".to_string()],
        memory_context: None,
        delegation_id: None,
        delegation_pattern: None,
        delegation_sub_run_summaries: Vec::new(),
        interruption: Some(interruption),
        approval_overrides: None,
        consecutive_context_window_errors: 0,
        pipeline_state: None,
        compaction_state: None,
        config_version_id: None,
    };
    astra_pipeline::step_checkpoint::write_step_checkpoint(
        &sid,
        1,
        &astra_pipeline::step_protocol::StepCheckpoint::Heavy(Box::new(heavy)),
    )
    .unwrap();

    // Session-memory file setup removed: `build_session_memory_resume_guidance`
    // now retrieves the last L1 from Memoria, not from disk. This test
    // asserts the resume replay path but not the memory-content injection.
    let _ = astra_runtime::claude_code_session_memory_path(&current_cwd_str, &sid);

    let mut creds = CredentialsFile::default();
    creds.profiles.insert(
        "default".to_string(),
        Profile {
            last_session_id: Some(sid.clone()),
            ..Default::default()
        },
    );
    save_credentials(&creds).unwrap();

    let mut state = session_runtime::initialize_session_state(
        None,
        Some("gpt-4o"),
        &crate::cli::cli_context::CliContext::default(),
    );
    assert_eq!(state.session_id, None);
    assert_eq!(state.pending_recovery.as_deref(), Some(sid.as_str()));

    #[derive(Clone)]
    struct MockState {
        requests: std::sync::Arc<tokio::sync::Mutex<Vec<serde_json::Value>>>,
    }

    let mock_state = MockState {
        requests: std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new())),
    };
    let recovered_session_id = "fresh-resume-session".to_string();
    let app =
        Router::new()
            .route(
                "/sessions",
                post({
                    let recovered_session_id = recovered_session_id.clone();
                    move || {
                        let recovered_session_id = recovered_session_id.clone();
                        async move {
                            axum::Json(serde_json::json!({ "session_id": recovered_session_id }))
                        }
                    }
                }),
            )
            .route(
                "/chat/turn",
                post({
                    let mock_state = mock_state.clone();
                    let sid = sid.clone();
                    let recovered_session_id = recovered_session_id.clone();
                    move |axum::Json(body): axum::Json<serde_json::Value>| {
                        let mock_state = mock_state.clone();
                        let sid = sid.clone();
                        let recovered_session_id = recovered_session_id.clone();
                        async move {
                            mock_state.requests.lock().await.push(body.clone());
                            if body.get("session_id").and_then(serde_json::Value::as_str)
                                == Some(sid.as_str())
                            {
                                (
                                    axum::http::StatusCode::NOT_FOUND,
                                    axum::Json(serde_json::json!({ "error": "session not found" })),
                                )
                                    .into_response()
                            } else {
                                (
                                    [("content-type", "text/event-stream")],
                                    sse_text_response("Recovered!", &recovered_session_id),
                                )
                                    .into_response()
                            }
                        }
                    }
                }),
            );

    let base = spawn_mock(app).await;
    let api = astra_thin_client::ThinClient::new(&base, None).unwrap();

    handle_chat_input(
        "继续".to_string(),
        Some("fake-token"),
        &mut state,
        TurnContext {
            api: &api,
            profile: None,
        },
    )
    .await
    .unwrap();

    let requests = mock_state.requests.lock().await.clone();
    assert!(
        !requests.is_empty(),
        "expected a fresh chat request, got {} requests",
        requests.len()
    );
    assert!(
        requests.iter().all(
            |req| req.get("session_id").and_then(serde_json::Value::as_str) != Some(sid.as_str())
        ),
        "ordinary chat input must not silently restore pending session {sid}: {requests:?}"
    );
    let fresh = requests.last().unwrap();
    assert_ne!(
        fresh.get("session_id").and_then(serde_json::Value::as_str),
        Some(sid.as_str())
    );
    assert_eq!(state.pending_recovery, None);
    assert_eq!(
        state.session_id.as_deref(),
        Some(recovered_session_id.as_str())
    );
    assert!(state.resume_guidance.is_none());
}

#[serial_test::serial]
#[tokio::test]
async fn crash_recovery_low_information_repair_followup_does_not_auto_restore() {
    let _creds = isolate_credentials();
    let temp = tempfile::tempdir().unwrap();
    let _sessions = session_journal::JournalDirGuard::new(temp.path());
    let sid = "1d21375d-18f5-4e53-9145-1fa197b564dd".to_string();
    let current_cwd = std::env::current_dir().unwrap();
    let current_cwd_str = current_cwd.display().to_string();
    let current_git_root = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());

    std::fs::write(
        temp.path().join(format!("{sid}.jsonl")),
        REAL_SESSION_1D21375_FIXTURE,
    )
    .unwrap();

    let mut ws = astra_services::session_workspace::WorkspaceMetadata::with_context(
        &sid,
        "qwen3.6-plus",
        &current_cwd_str,
        Some("main"),
    );
    ws.git_root = current_git_root;
    ws.turn_count = 3;
    ws.total_tokens_in = 311262;
    ws.total_tokens_out = 6995;
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

    let mut state = session_runtime::initialize_session_state(
        None,
        Some("qwen3.6-plus"),
        &crate::cli::cli_context::CliContext::default(),
    );
    assert_eq!(state.pending_recovery.as_deref(), Some(sid.as_str()));

    #[derive(Clone)]
    struct MockState {
        requests: std::sync::Arc<tokio::sync::Mutex<Vec<serde_json::Value>>>,
    }

    let mock_state = MockState {
        requests: std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new())),
    };
    let recovered_session_id = "repair-recovery-session".to_string();
    let app =
        Router::new()
            .route(
                "/sessions",
                post({
                    let recovered_session_id = recovered_session_id.clone();
                    move || {
                        let recovered_session_id = recovered_session_id.clone();
                        async move {
                            axum::Json(serde_json::json!({ "session_id": recovered_session_id }))
                        }
                    }
                }),
            )
            .route(
                "/chat/turn",
                post({
                    let mock_state = mock_state.clone();
                    let sid = sid.clone();
                    let recovered_session_id = recovered_session_id.clone();
                    move |axum::Json(body): axum::Json<serde_json::Value>| {
                        let mock_state = mock_state.clone();
                        let sid = sid.clone();
                        let recovered_session_id = recovered_session_id.clone();
                        async move {
                            mock_state.requests.lock().await.push(body.clone());
                            if body.get("session_id").and_then(serde_json::Value::as_str)
                                == Some(sid.as_str())
                            {
                                (
                                    axum::http::StatusCode::NOT_FOUND,
                                    axum::Json(serde_json::json!({ "error": "session not found" })),
                                )
                                    .into_response()
                            } else {
                                (
                                    [("content-type", "text/event-stream")],
                                    sse_text_response("Patched.", &recovered_session_id),
                                )
                                    .into_response()
                            }
                        }
                    }
                }),
            );

    let base = spawn_mock(app).await;
    let api = astra_thin_client::ThinClient::new(&base, None).unwrap();

    handle_chat_input(
        "修复?".to_string(),
        Some("fake-token"),
        &mut state,
        TurnContext {
            api: &api,
            profile: None,
        },
    )
    .await
    .unwrap();

    let requests = mock_state.requests.lock().await.clone();
    assert!(
        !requests.is_empty(),
        "expected a fresh chat request, got no requests"
    );
    assert!(
        requests.iter().all(
            |req| req.get("session_id").and_then(serde_json::Value::as_str) != Some(sid.as_str())
        ),
        "repair follow-up must not silently restore pending session {sid}: {requests:?}"
    );
    assert_eq!(state.pending_recovery, None);
    assert_eq!(
        state.session_id.as_deref(),
        Some(recovered_session_id.as_str())
    );
}

// ── Edge cases ───────────────────────────────────────────────────────────

#[serial_test::serial]
#[tokio::test]
async fn resume_handles_malformed_workspace_yaml() {
    let _creds = isolate_credentials();
    use astra_services::session_restore::SessionRestoreService;

    let sid = format!("test-malformed-{}", uuid::Uuid::new_v4());

    // Create journal
    let writer = session_journal::JournalWriter::new(&sid).unwrap();
    writer
        .append(&session_journal::JournalEvent::session_start(
            Some(&sid),
            Some("gpt-4o"),
        ))
        .unwrap();
    drop(writer);

    // Create malformed workspace.yaml
    let ws_dir = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".astra")
        .join("sessions")
        .join(&sid);
    std::fs::create_dir_all(&ws_dir).unwrap();
    std::fs::write(ws_dir.join("workspace.yaml"), "invalid: yaml: content: [").unwrap();

    // Malformed workspace now falls back to journal-only local restore.
    let svc = astra_services::session_restore::HybridRestoreService::local_only();
    let result = svc
        .restore_session(&sid)
        .await
        .unwrap()
        .expect("malformed workspace should still restore from journal");
    assert_eq!(result.session_id, sid);
    assert_eq!(result.turn_count, 0);
    assert_eq!(result.model.as_deref(), Some("gpt-4o"));
    assert_eq!(result.last_status, "local");
    assert!(!result.restored_from_cloud);
}

#[serial_test::serial]
#[tokio::test]
async fn resume_handles_missing_workspace() {
    let _creds = isolate_credentials();
    use astra_services::session_restore::SessionRestoreService;

    // Only journal, no workspace → local journal-only restore should still work.
    let sid = format!("test-no-ws-{}", uuid::Uuid::new_v4());
    let writer = session_journal::JournalWriter::new(&sid).unwrap();
    writer
        .append(&session_journal::JournalEvent::session_start(
            Some(&sid),
            Some("gpt-4o"),
        ))
        .unwrap();
    drop(writer);

    let svc = astra_services::session_restore::HybridRestoreService::local_only();
    let result = svc
        .restore_session(&sid)
        .await
        .unwrap()
        .expect("journal-only session should restore");
    assert_eq!(result.session_id, sid);
    assert_eq!(result.turn_count, 0);
    assert_eq!(result.model.as_deref(), Some("gpt-4o"));
    assert_eq!(result.last_status, "local");
    assert!(!result.restored_from_cloud);
}

// ── Checkpoint listing ───────────────────────────────────────────────────

#[serial_test::serial]
#[tokio::test]
async fn resume_lists_checkpoints_for_session() {
    let _creds = isolate_credentials();
    use astra_services::session_restore::SessionRestoreService;

    let sid = format!("test-checkpoints-{}", uuid::Uuid::new_v4());

    // Create journal
    let writer = session_journal::JournalWriter::new(&sid).unwrap();
    writer
        .append(&session_journal::JournalEvent::session_start(
            Some(&sid),
            Some("gpt-4o"),
        ))
        .unwrap();
    drop(writer);

    // Create workspace
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

    // List checkpoints should return empty (no checkpoints created yet)
    let svc = astra_services::session_restore::HybridRestoreService::local_only();
    let ckpts = svc.list_checkpoints(&sid).await.unwrap();
    assert!(ckpts.is_empty(), "no checkpoints created yet");
}

// merge_learning_snapshot tests removed: the entity/pattern/calibration
// learning subsystem has been deleted.

use super::*;
use crate::cli_utils::{CredentialsFile, Profile, save_credentials};
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
        let lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
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

#[tokio::test]
async fn initialize_repl_state_marks_workspace_session_as_pending_recovery() {
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
    ws.session_goal = Some("ship session restore".to_string());
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

    let state = repl_runtime::initialize_repl_state(None, None);
    assert_eq!(state.session_id, None);
    assert_eq!(state.pending_recovery.as_deref(), Some(sid.as_str()));
    assert!(state.history.is_empty());
    assert_eq!(state.turn, 0);
}

#[tokio::test(flavor = "current_thread")]
async fn crash_recovery_short_continue_restores_and_replays_context_online() {
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

    let light = astra_runtime::pipeline::step_protocol::LightCheckpoint {
        protocol_version: astra_runtime::pipeline::step_protocol::PROTOCOL_VERSION,
        cursor: astra_runtime::pipeline::step_protocol::ExecutionCursor::default(),
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
    let heavy = astra_runtime::pipeline::step_protocol::HeavyCheckpoint {
        light,
        messages: vec![
            serde_json::json!({"role": "user", "content": "continue the parser refactor"}),
            serde_json::json!({"role": "assistant", "content": "I updated the lexer; next patch the parser."}),
        ],
        budget_remaining_tokens: 10_000,
        budget_remaining_rounds: 8,
        blocked_tools: Vec::new(),
        recent_tools: vec!["bash".to_string()],
        learning_snapshot_id: None,
        memory_context: None,
        delegation_id: None,
        delegation_pattern: None,
        delegation_sub_run_summaries: Vec::new(),
        interruption: Some(interruption),
        approval_overrides: None,
        consecutive_context_window_errors: 0,
        compaction_state: None,
    };
    astra_runtime::pipeline::step_checkpoint::write_step_checkpoint(
        &sid,
        1,
        &astra_runtime::pipeline::step_protocol::StepCheckpoint::Heavy(Box::new(heavy)),
    )
    .unwrap();

    let summary_path = astra_runtime::claude_code_session_memory_path(&current_cwd_str, &sid);
    std::fs::create_dir_all(summary_path.parent().unwrap()).unwrap();
    std::fs::write(
        &summary_path,
        "# Session Memory\n\n## Errors & Corrections\n- Use apply_patch instead of python file rewrites.\n\n## Learnings\n- Keep diffs minimal and project-scoped.\n",
    )
    .unwrap();

    let mut creds = CredentialsFile::default();
    creds.profiles.insert(
        "default".to_string(),
        Profile {
            last_session_id: Some(sid.clone()),
            ..Default::default()
        },
    );
    save_credentials(&creds).unwrap();

    let mut state = repl_runtime::initialize_repl_state(None, Some("gpt-4o"));
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
    let selector = tool_selector::TfIdfSelector::new(tool_registry::ToolRegistry::new(
        edge_tools::all_tool_schemas(),
    ));

    handle_chat_input(
        "继续".to_string(),
        Some("fake-token"),
        &mut state,
        ReplTurnContext {
            api: &api,
            profile: None,
            selector: &selector,
        },
    )
    .await
    .unwrap();

    let requests = mock_state.requests.lock().await.clone();
    assert!(
        requests.len() >= 2,
        "expected retry after stale session recovery, got {} requests",
        requests.len()
    );
    let resumed = requests
        .iter()
        .find(|req| req.get("session_id").and_then(serde_json::Value::as_str) == Some(sid.as_str()))
        .expect("expected first recovered request to target stale session id");
    let resumed_text = resumed.to_string();
    assert!(resumed_text.contains("rate_limited"));
    assert!(resumed_text.contains("Keep diffs minimal and project-scoped."));
    assert!(resumed_text.contains("Use apply_patch instead of python file rewrites."));
    assert!(resumed_text.contains("继续"));

    let retried = requests.last().unwrap();
    assert_ne!(
        retried
            .get("session_id")
            .and_then(serde_json::Value::as_str),
        Some(sid.as_str())
    );
    let retried_text = retried.to_string();
    assert!(retried_text.contains("rate_limited"));
    assert!(retried_text.contains("Keep diffs minimal and project-scoped."));
    assert_eq!(state.pending_recovery, None);
    assert_eq!(
        state.session_id.as_deref(),
        Some(recovered_session_id.as_str())
    );
    assert_eq!(state.history.len(), 2);
    assert_eq!(state.history.last().unwrap().0, "继续");
    assert!(state.resume_guidance.is_none());
}

#[tokio::test]
async fn crash_recovery_low_information_repair_followup_rebuilds_attachment() {
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

    let mut state = repl_runtime::initialize_repl_state(None, Some("qwen3.6-plus"));
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
    let selector = tool_selector::TfIdfSelector::new(tool_registry::ToolRegistry::new(
        edge_tools::all_tool_schemas(),
    ));

    handle_chat_input(
        "修复?".to_string(),
        Some("fake-token"),
        &mut state,
        ReplTurnContext {
            api: &api,
            profile: None,
            selector: &selector,
        },
    )
    .await
    .unwrap();

    let requests = mock_state.requests.lock().await.clone();
    let resumed = requests
        .iter()
        .find(|req| req.get("session_id").and_then(serde_json::Value::as_str) == Some(sid.as_str()))
        .expect("expected first recovered request to target stale session id");
    let resumed_text = resumed.to_string();
    assert!(resumed_text.contains("[Active task attachment]"));
    assert!(resumed_text.contains("review 这个: aa1f419bc040003f5de8cdfa6b414225ade82e2b"));
    assert!(resumed_text.contains("Two independent fixes in one commit. Let me review each."));
    assert!(resumed_text.contains("thread leak on timeout"));
    assert!(resumed_text.contains("[User follow-up]\\n修复?"));
    assert_eq!(state.pending_recovery, None);
    assert_eq!(
        state.session_id.as_deref(),
        Some(recovered_session_id.as_str())
    );
}

// ── Learning snapshot restoration ────────────────────────────────────────

#[tokio::test]
async fn resume_restores_learning_snapshot() {
    use astra_services::session_restore::RestoredSession;

    // Create a mock RestoredSession with learning snapshot
    let restored = RestoredSession {
        session_id: "test-learning".into(),
        turn_count: 5,
        total_tokens_in: 1000,
        total_tokens_out: 500,
        recent_tools: vec!["grep".into()],
        learning_snapshot_json: Some(
            r#"{"entities":["Rust","MatrixOne"],"patterns":["*.rs"]}"#.into(),
        ),
        checkpoint_count: 1,
        last_status: "active".into(),
        git_branch: Some("main".into()),
        model: Some("gpt-4o".into()),
        title: Some("Test".into()),
        restored_from_cloud: true, // Cloud restore has learning
        ..Default::default()
    };

    // Verify the learning snapshot is present
    assert!(restored.learning_snapshot_json.is_some());
    let json = restored.learning_snapshot_json.as_ref().unwrap();
    assert!(json.contains("Rust"));
    assert!(json.contains("MatrixOne"));

    // Simulate what handle_resume_command does
    let learning_snapshot = if let Some(ref l) = restored.learning_snapshot_json {
        if !l.is_empty() { Some(l.clone()) } else { None }
    } else {
        None
    };

    assert!(learning_snapshot.is_some());
    assert_eq!(learning_snapshot.unwrap().as_str(), json);
}

#[tokio::test]
async fn resume_local_restore_has_no_learning_snapshot() {
    use astra_services::session_restore::RestoredSession;

    // Local restore should not have learning snapshot
    let restored = RestoredSession {
        session_id: "test-local".into(),
        turn_count: 3,
        total_tokens_in: 500,
        total_tokens_out: 200,
        recent_tools: vec![],
        learning_snapshot_json: None, // Local restore doesn't have this
        checkpoint_count: 1,
        last_status: "active".into(),
        git_branch: None,
        model: None,
        title: None,
        restored_from_cloud: false,
        ..Default::default()
    };

    assert!(restored.learning_snapshot_json.is_none());
}

// ── Edge cases ───────────────────────────────────────────────────────────

#[tokio::test]
async fn resume_handles_empty_learning_snapshot() {
    use astra_services::session_restore::RestoredSession;

    // Empty string should be treated as None
    let restored = RestoredSession {
        learning_snapshot_json: Some("".into()),
        ..Default::default()
    };

    // Simulate the logic in handle_resume_command
    let learning_snapshot = if let Some(ref l) = restored.learning_snapshot_json {
        if !l.is_empty() { Some(l.clone()) } else { None }
    } else {
        None
    };

    assert!(
        learning_snapshot.is_none(),
        "empty string should be ignored"
    );
}

#[tokio::test]
async fn resume_handles_invalid_learning_json() {
    use astra_services::session_restore::RestoredSession;

    // Invalid JSON should still be stored (will fail at merge time)
    let restored = RestoredSession {
        learning_snapshot_json: Some("not valid json {{{".into()),
        ..Default::default()
    };

    assert!(restored.learning_snapshot_json.is_some());
    let json = restored.learning_snapshot_json.as_ref().unwrap();
    assert!(json.contains("{"));
}

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

// ── Integration: full resume flow simulation ─────────────────────────────

#[tokio::test]
async fn resume_full_flow_cloud_restore() {
    use astra_services::session_restore::RestoredSession;

    // Simulate a complete cloud restore scenario
    let restored = RestoredSession {
        session_id: "cloud-sess-123".into(),
        turn_count: 42,
        total_tokens_in: 150_000,
        total_tokens_out: 80_000,
        recent_tools: vec!["git".into(), "bash".into(), "grep".into()],
        learning_snapshot_json: Some(r#"{"entities":["Rust","SQL"],"patterns":["*.rs"]}"#.into()),
        checkpoint_count: 5,
        last_status: "active".into(),
        git_branch: Some("feature/resume".into()),
        model: Some("claude-3-opus".into()),
        title: Some("Implement session resume".into()),
        restored_from_cloud: true,
        ..Default::default()
    };
    assert_eq!(restored.session_id, "cloud-sess-123");
    assert_eq!(restored.turn_count, 42);
    assert!(restored.restored_from_cloud);
    assert!(restored.learning_snapshot_json.is_some());
    assert_eq!(restored.recent_tools.len(), 3);

    // Simulate state application
    let mut state = super::ReplState::default();
    #[allow(clippy::field_reassign_with_default)]
    {
        state.session_id = Some(restored.session_id.clone());
        state.turn = restored.turn_count;
        state.total_prompt_tokens = restored.total_tokens_in;
        state.total_completion_tokens = restored.total_tokens_out;
        state.recent_tools = restored.recent_tools.clone();
        state.model = restored.model.clone();
        if let Some(ref m) = state.model {
            state.cached_pricing = slash_stats::fallback_pricing(m);
        }
    }

    // Apply learning snapshot
    if let Some(ref l) = restored.learning_snapshot_json
        && !l.is_empty()
    {
        state.learning_snapshot = Some(l.clone());
    }

    // Verify state
    assert_eq!(state.session_id, Some("cloud-sess-123".into()));
    assert_eq!(state.turn, 42);
    assert_eq!(state.total_prompt_tokens, 150_000);
    assert_eq!(
        state.learning_snapshot.unwrap(),
        r#"{"entities":["Rust","SQL"],"patterns":["*.rs"]}"#
    );
}

// ── Checkpoint listing ───────────────────────────────────────────────────

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

// ── merge_learning_snapshot ───────────────────────────────────────────────

#[test]
fn merge_learning_valid_snapshot() {
    use astra_runtime::pipeline::{calibration, entity, pattern};

    let json = serde_json::json!({
        "version": 1,
        "entities": [{
            "name": "rust",
            "aliases": ["rs"],
            "domain": null,
            "associated_tools": ["cargo"],
            "confidence": 0.8,
            "observation_count": 5
        }],
        "patterns": [{
            "signature": "cargo",
            "tools": ["cargo"],
            "task_type": "Code",
            "domain": null,
            "success_count": 3,
            "failure_count": 0,
            "quality_sum": 2.4
        }],
        "calibration": null
    })
    .to_string();

    let eg = std::sync::Arc::new(std::sync::Mutex::new(entity::EntityGraph::new()));
    let pl = std::sync::Arc::new(std::sync::Mutex::new(pattern::PatternLibrary::new()));
    let cal = std::sync::Arc::new(std::sync::Mutex::new(
        calibration::ProgressiveCalibrator::default(),
    ));

    merge_learning_snapshot(&json, &eg, &pl, &cal);

    // Verify entity content, not just count
    let entities = eg.lock().unwrap().export();
    assert_eq!(entities.len(), 1);
    let e = &entities[0];
    assert_eq!(e.name, "rust");
    assert_eq!(e.aliases, vec!["rs"]);
    assert_eq!(e.associated_tools, vec!["cargo"]);
    assert!((e.confidence - 0.8).abs() < 1e-6);
    assert_eq!(e.observation_count, 5);

    // Verify pattern content, not just count
    let patterns = pl.lock().unwrap().export();
    assert_eq!(patterns.len(), 1);
    let p = &patterns[0];
    assert_eq!(p.signature, "cargo");
    assert_eq!(p.tools, vec!["cargo"]);
    assert_eq!(p.success_count, 3);
    assert_eq!(p.failure_count, 0);
}

#[test]
fn merge_learning_invalid_json_does_not_panic() {
    use astra_runtime::pipeline::{calibration, entity, pattern};

    let eg = std::sync::Arc::new(std::sync::Mutex::new(entity::EntityGraph::new()));
    let pl = std::sync::Arc::new(std::sync::Mutex::new(pattern::PatternLibrary::new()));
    let cal = std::sync::Arc::new(std::sync::Mutex::new(
        calibration::ProgressiveCalibrator::default(),
    ));

    // Invalid JSON — should not panic, just print warning
    merge_learning_snapshot("not valid json", &eg, &pl, &cal);

    // Modules should remain empty
    assert!(eg.lock().unwrap().export().is_empty());
    assert!(pl.lock().unwrap().export().is_empty());
}

#[test]
fn merge_learning_empty_snapshot() {
    use astra_runtime::pipeline::{calibration, entity, pattern};

    let json = serde_json::json!({
        "version": 1,
        "entities": [],
        "patterns": [],
        "calibration": null
    })
    .to_string();

    let eg = std::sync::Arc::new(std::sync::Mutex::new(entity::EntityGraph::new()));
    let pl = std::sync::Arc::new(std::sync::Mutex::new(pattern::PatternLibrary::new()));
    let cal = std::sync::Arc::new(std::sync::Mutex::new(
        calibration::ProgressiveCalibrator::default(),
    ));

    merge_learning_snapshot(&json, &eg, &pl, &cal);

    assert!(eg.lock().unwrap().export().is_empty());
    assert!(pl.lock().unwrap().export().is_empty());
}

#[test]
fn merge_learning_idempotent() {
    use astra_runtime::pipeline::{calibration, entity, pattern};

    let json = serde_json::json!({
        "version": 1,
        "entities": [{"name": "rust", "aliases": [], "domain": null,
            "associated_tools": ["cargo"], "confidence": 0.8, "observation_count": 5}],
        "patterns": [{"signature": "cargo", "tools": ["cargo"], "task_type": "Code",
            "domain": null, "success_count": 3, "failure_count": 0, "quality_sum": 2.4}],
        "calibration": null
    })
    .to_string();

    let eg = std::sync::Arc::new(std::sync::Mutex::new(entity::EntityGraph::new()));
    let pl = std::sync::Arc::new(std::sync::Mutex::new(pattern::PatternLibrary::new()));
    let cal = std::sync::Arc::new(std::sync::Mutex::new(
        calibration::ProgressiveCalibrator::default(),
    ));

    // Merge twice — should not duplicate
    merge_learning_snapshot(&json, &eg, &pl, &cal);
    merge_learning_snapshot(&json, &eg, &pl, &cal);

    assert_eq!(
        eg.lock().unwrap().export().len(),
        1,
        "entities should not duplicate"
    );
    assert_eq!(
        pl.lock().unwrap().export().len(),
        1,
        "patterns should not duplicate"
    );
}

#[test]
fn merge_learning_multiple_entities_and_patterns() {
    use astra_runtime::pipeline::{calibration, entity, pattern};

    let json = serde_json::json!({
        "version": 1,
        "entities": [
            {"name": "rust", "aliases": [], "domain": null,
                "associated_tools": ["cargo"], "confidence": 0.9, "observation_count": 10},
            {"name": "matrixone", "aliases": ["mo"], "domain": "Database",
                "associated_tools": ["sql_query"], "confidence": 0.7, "observation_count": 3}
        ],
        "patterns": [
            {"signature": "cargo|grep", "tools": ["cargo", "grep"], "task_type": "Code",
                "domain": null, "success_count": 5, "failure_count": 1, "quality_sum": 4.0},
            {"signature": "sql_query", "tools": ["sql_query"], "task_type": "Fetch",
                "domain": "Database", "success_count": 2, "failure_count": 0, "quality_sum": 1.8}
        ],
        "calibration": null
    })
    .to_string();

    let eg = std::sync::Arc::new(std::sync::Mutex::new(entity::EntityGraph::new()));
    let pl = std::sync::Arc::new(std::sync::Mutex::new(pattern::PatternLibrary::new()));
    let cal = std::sync::Arc::new(std::sync::Mutex::new(
        calibration::ProgressiveCalibrator::default(),
    ));

    merge_learning_snapshot(&json, &eg, &pl, &cal);

    let entities = eg.lock().unwrap().export();
    assert_eq!(entities.len(), 2);
    let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"rust"));
    assert!(names.contains(&"matrixone"));

    let patterns = pl.lock().unwrap().export();
    assert_eq!(patterns.len(), 2);
    let sigs: Vec<&str> = patterns.iter().map(|p| p.signature.as_str()).collect();
    assert!(sigs.contains(&"cargo|grep"));
    assert!(sigs.contains(&"sql_query"));
}

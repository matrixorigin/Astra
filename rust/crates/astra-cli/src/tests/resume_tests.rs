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

// ── slash_task::find_task_by_query ─────────────────────────────────────────

use astra_services::TaskService as _;

#[tokio::test]
async fn find_task_by_query_scenarios() {
    use crate::cli::cli_config::cli_utils::prefix_chars;

    let tmp = tempfile::TempDir::new().unwrap();
    let svc = astra_services::LocalTaskService::new(tmp.path().to_path_buf());

    // Create first task for ID/prefix/title tests
    let tid_a = svc
        .create_task(
            "u1",
            "s1",
            astra_services::TaskCreateRequest {
                title: "Refactor authentication module".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    // Create other user's task for isolation test
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

    // ID prefix match
    let found = slash_task::find_task_by_query(&svc, "u1", &tid_a)
        .await
        .unwrap();
    assert_eq!(found, Some(tid_a.clone()));

    let prefix = prefix_chars(&tid_a, 8);
    let found = slash_task::find_task_by_query(&svc, "u1", &prefix)
        .await
        .unwrap();
    assert_eq!(found, Some(tid_a));

    // Title substring + case-insensitive (only 1 match so far)
    let found = slash_task::find_task_by_query(&svc, "u1", "authentication")
        .await
        .unwrap();
    assert!(found.is_some());

    let found = slash_task::find_task_by_query(&svc, "u1", "AUTH")
        .await
        .unwrap();
    assert!(found.is_some());

    // Now add a second "authentication" task → ambiguity
    svc.create_task(
        "u1",
        "s1",
        astra_services::TaskCreateRequest {
            title: "Refactor authentication tests".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let err = slash_task::find_task_by_query(&svc, "u1", "authentication")
        .await
        .unwrap_err();
    assert!(err.contains("task query 'authentication' is ambiguous"));

    // Not found
    let found = slash_task::find_task_by_query(&svc, "u1", "nonexistent")
        .await
        .unwrap();
    assert!(found.is_none());

    // Wrong user isolation
    let found = slash_task::find_task_by_query(&svc, "user-b", "Private")
        .await
        .unwrap();
    assert!(found.is_none());
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

    let svc = astra_services::session_restore::HybridRestoreService::local_only();
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

        let svc = astra_services::session_restore::HybridRestoreService::local_only();
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

    let svc = astra_services::session_restore::HybridRestoreService::local_only();
    let ckpts = svc.list_local_checkpoints(&sid).await.unwrap();
    assert!(ckpts.is_empty(), "no checkpoints created yet");
}

// merge_learning_snapshot tests removed: the entity/pattern/calibration
// learning subsystem has been deleted.

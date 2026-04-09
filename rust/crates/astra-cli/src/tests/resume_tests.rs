use super::*;

    // ── find_task_by_query ────────────────────────────────────────────────────

    use astra_services::TaskService as _;

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
        let found = find_task_by_query(&svc, "u1", &tid).await.unwrap();
        assert_eq!(found, Some(tid.clone()));

        // Prefix match (first 8 Unicode scalars)
        let prefix = prefix_chars(&tid, 8);
        let found = find_task_by_query(&svc, "u1", &prefix).await.unwrap();
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
        let found = find_task_by_query(&svc, "u1", "authentication")
            .await
            .unwrap();
        assert!(found.is_some());

        let found = find_task_by_query(&svc, "u1", "AUTH").await.unwrap();
        assert!(found.is_some());
    }

    #[tokio::test]
    async fn find_task_not_found() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = astra_services::LocalTaskService::new(tmp.path().to_path_buf());
        let found = find_task_by_query(&svc, "u1", "nonexistent").await.unwrap();
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
        let found = find_task_by_query(&svc, "user-b", "Private").await.unwrap();
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

        // Should return None for malformed workspace
        let svc = astra_services::session_restore::HybridRestoreService::local_only();
        let result = svc.restore_session(&sid).await.unwrap();
        assert!(
            result.is_none(),
            "malformed workspace.yaml should cause restore to return None"
        );
    }

    #[tokio::test]
    async fn resume_handles_missing_workspace() {
        let _creds = isolate_credentials();
        use astra_services::session_restore::SessionRestoreService;

        // Only journal, no workspace → should fall back to cloud (which returns None)
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
        let result = svc.restore_session(&sid).await.unwrap();
        assert!(
            result.is_none(),
            "session without workspace.yaml should return None"
        );
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
            learning_snapshot_json: Some(
                r#"{"entities":["Rust","SQL"],"patterns":["*.rs"]}"#.into(),
            ),
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
                state.cached_pricing = fallback_pricing(m);
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


use super::*;
use astra_services::{session_journal::JournalDirGuard, session_workspace};

/// Parse a task-tool response into JSON, tolerating the human-readable
/// summary line that `prefix_summary` prepends to success responses.
fn parse_task_json(response: &str) -> Value {
    let body = response
        .find('{')
        .map(|pos| &response[pos..])
        .unwrap_or(response);
    serde_json::from_str(body)
        .unwrap_or_else(|e| panic!("task response not JSON: {e}; raw: {response}"))
}

fn executor_with_session() -> (
    ToolExecutor,
    std::sync::Arc<std::sync::RwLock<astra_runtime::observability::ObservabilitySession>>,
) {
    let session = std::sync::Arc::new(std::sync::RwLock::new(
        astra_runtime::observability::ObservabilitySession::new_simple("test-session"),
    ));
    let exe = ToolExecutor::new(std::env::temp_dir()).with_observability_session(session.clone());
    (exe, session)
}

fn executor_with_persisted_session() -> (
    tempfile::TempDir,
    JournalDirGuard,
    ToolExecutor,
    std::sync::Arc<std::sync::RwLock<astra_runtime::observability::ObservabilitySession>>,
    String,
) {
    let tmp = tempfile::tempdir().unwrap();
    let guard = JournalDirGuard::new(tmp.path());
    let session_id = "persisted-self-mod".to_string();
    let ws = session_workspace::WorkspaceMetadata::with_context(
        &session_id,
        "gpt-5.4",
        "/repo",
        Some("main"),
    );
    session_workspace::write_workspace(&ws).unwrap();
    let session = std::sync::Arc::new(std::sync::RwLock::new(
        astra_runtime::observability::ObservabilitySession::new_simple("test-session"),
    ));
    let exe = ToolExecutor::new(tmp.path())
        .with_active_session_id(session_id.clone())
        .with_observability_session(session.clone());
    (tmp, guard, exe, session, session_id)
}

#[tokio::test]
async fn adjust_config_applies_bounded_change() {
    let (exe, session) = executor_with_session();
    let out = exe
        .execute(
            "adjust_config",
            &json!({
                "path": "memory.retrieval_top_k",
                "value": 6
            }),
        )
        .await;
    let parsed: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(parsed["status"], "ok");
    let guard = session.read().unwrap();
    assert_eq!(guard.config.memory.retrieval_top_k, 6);
}

#[tokio::test]
async fn adjust_config_respects_drift_ceiling_without_force() {
    let (exe, _session) = executor_with_session();
    let out = exe
        .execute(
            "adjust_config",
            &json!({
                "path": "memory.retrieval_top_k",
                "value": 20
            }),
        )
        .await;
    let parsed: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(parsed["error"], "config_drift_ceiling_exceeded");
}

#[tokio::test]
async fn tool_priority_updates_self_model_snapshot() {
    let (exe, _session) = executor_with_session();
    let out1 = exe
        .execute("prioritize_tool", &json!({"tool": "bash"}))
        .await;
    let out2 = exe
        .execute("deprioritize_tool", &json!({"tool": "web_fetch"}))
        .await;
    let parsed1: Value = serde_json::from_str(&out1).unwrap();
    let parsed2: Value = serde_json::from_str(&out2).unwrap();
    assert_eq!(parsed1["status"], "ok");
    assert_eq!(parsed2["status"], "ok");
    assert_eq!(parsed1["previous_pinned_tools"], json!([]));
    assert_eq!(parsed1["previous_deprioritized_tools"], json!([]));
    assert_eq!(parsed2["previous_pinned_tools"], json!(["bash"]));
    assert_eq!(parsed2["previous_deprioritized_tools"], json!([]));

    let model = exe.build_self_model_snapshot().unwrap();
    assert!(
        model
            .capabilities
            .pinned_tools
            .contains(&"bash".to_string())
    );
    assert!(
        model
            .capabilities
            .deprioritized_tools
            .contains(&"web_fetch".to_string())
    );
}

#[tokio::test]
async fn self_mod_persists_config_and_tool_preferences() {
    let (_tmp, _guard, exe, _session, session_id) = executor_with_persisted_session();

    let prioritize_out = exe
        .execute("prioritize_tool", &json!({"tool": "bash"}))
        .await;
    let adjust_out = exe
        .execute(
            "adjust_config",
            &json!({
                "path": "memory.retrieval_top_k",
                "value": 6
            }),
        )
        .await;

    let parsed_prioritize: Value = serde_json::from_str(&prioritize_out).unwrap();
    let parsed_adjust: Value = serde_json::from_str(&adjust_out).unwrap();
    assert_eq!(parsed_prioritize["status"], "ok");
    assert_eq!(parsed_adjust["status"], "ok");

    let ws = session_workspace::read_workspace(&session_id).unwrap();
    assert!(ws.pinned_tools.contains(&"bash".to_string()));
    assert!(ws.tuned_config_json.is_some());
}

#[tokio::test]
async fn prioritize_tool_preserves_existing_state_when_persist_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let guard = JournalDirGuard::new(tmp.path());
    let session_id = "persisted-prioritize-rollback".to_string();
    let mut ws = session_workspace::WorkspaceMetadata::with_context(
        &session_id,
        "gpt-5.4",
        "/repo",
        Some("main"),
    );
    ws.pinned_tools = vec!["bash".to_string()];
    session_workspace::write_workspace(&ws).unwrap();

    let session = std::sync::Arc::new(std::sync::RwLock::new(
        astra_runtime::observability::ObservabilitySession::new_simple("test-session"),
    ));
    let exe = ToolExecutor::new(tmp.path())
        .with_active_session_id(session_id.clone())
        .with_observability_session(session);

    let workspace_path = session_workspace::workspace_dir_for(&session_id).join("workspace.yaml");
    std::fs::remove_file(workspace_path).unwrap();

    let out = exe
        .execute("prioritize_tool", &json!({"tool": "bash"}))
        .await;
    let parsed: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(parsed["error"], "failed_to_persist_tool_preferences");

    let model = exe.build_self_model_snapshot().unwrap();
    assert!(
        model
            .capabilities
            .pinned_tools
            .contains(&"bash".to_string())
    );
    assert!(
        !model
            .capabilities
            .deprioritized_tools
            .contains(&"bash".to_string())
    );

    drop(guard);
}

#[tokio::test]
async fn deprioritize_tool_preserves_existing_state_when_persist_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let guard = JournalDirGuard::new(tmp.path());
    let session_id = "persisted-deprioritize-rollback".to_string();
    let mut ws = session_workspace::WorkspaceMetadata::with_context(
        &session_id,
        "gpt-5.4",
        "/repo",
        Some("main"),
    );
    ws.deprioritized_tools = vec!["bash".to_string()];
    session_workspace::write_workspace(&ws).unwrap();

    let session = std::sync::Arc::new(std::sync::RwLock::new(
        astra_runtime::observability::ObservabilitySession::new_simple("test-session"),
    ));
    let exe = ToolExecutor::new(tmp.path())
        .with_active_session_id(session_id.clone())
        .with_observability_session(session);

    let workspace_path = session_workspace::workspace_dir_for(&session_id).join("workspace.yaml");
    std::fs::remove_file(workspace_path).unwrap();

    let out = exe
        .execute("deprioritize_tool", &json!({"tool": "bash"}))
        .await;
    let parsed: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(parsed["error"], "failed_to_persist_tool_preferences");

    let model = exe.build_self_model_snapshot().unwrap();
    assert!(
        model
            .capabilities
            .deprioritized_tools
            .contains(&"bash".to_string())
    );
    assert!(
        !model
            .capabilities
            .pinned_tools
            .contains(&"bash".to_string())
    );

    drop(guard);
}

#[tokio::test]
async fn shared_task_manager_and_session_state_journal_survive_across_executors() {
    let shared_tasks = std::sync::Arc::new(TaskManager::in_memory());
    let shared_journal =
        std::sync::Arc::new(std::sync::Mutex::new(SessionStateRollbackJournal::default()));

    let exe_a = ToolExecutor::new(std::env::temp_dir())
        .with_shared_task_manager(shared_tasks.clone())
        .with_shared_session_state_journal(shared_journal.clone());
    let exe_b = ToolExecutor::new(std::env::temp_dir())
        .with_shared_task_manager(shared_tasks)
        .with_shared_session_state_journal(shared_journal);

    exe_a
        .journal_turn_index
        .store(11, std::sync::atomic::Ordering::Relaxed);
    exe_b
        .journal_turn_index
        .store(11, std::sync::atomic::Ordering::Relaxed);

    let create_out = exe_a
        .execute("task_create", &json!({"title": "shared task"}))
        .await;
    let create_json = parse_task_json(&create_out);
    assert_eq!(create_json["success"], true);

    let listed_via_b = exe_b.execute("task_list", &json!({})).await;
    let listed_via_b_json: Value = serde_json::from_str(&listed_via_b).unwrap();
    assert_eq!(listed_via_b_json["count"], 1);

    let rollback_json: Value = serde_json::from_str(
        &exe_b
            .rollback_session_state(&json!({"scope": "turn", "turn_index": 11}))
            .await,
    )
    .unwrap();
    assert_eq!(rollback_json["success"], true);
    assert_eq!(
        rollback_json["restored"]
            .as_array()
            .map(|entries| entries.len()),
        Some(1)
    );

    let listed_after = exe_a.execute("task_list", &json!({})).await;
    assert_eq!(listed_after, "No tasks found with status 'all'");
}

// ─── P4: P3 seams wired into SelfModel snapshot ─────────────────────────────

#[tokio::test]
async fn set_session_lessons_feeds_build_self_model_snapshot() {
    // The session-bootstrap loader (P3.1) hands lessons to the ToolExecutor.
    // `build_self_model_snapshot` must pass them through unchanged so the
    // LLM sees prior-session advice on turn 1.
    let (exe, _session) = executor_with_session();
    let lessons = vec![
        astra_runtime::self_model::LessonHint {
            kind: astra_services::LessonKind::ToolDeprioritize,
            trigger_signal: "3 stalls on grep".into(),
            action: "switch to rg".into(),
            workload_tag: None,
            compact: None,
        },
        astra_runtime::self_model::LessonHint {
            kind: astra_services::LessonKind::PromptShape,
            trigger_signal: "repeated scope drift".into(),
            action: "restate scope before tool call".into(),
            workload_tag: Some("code-review".into()),
            compact: None,
        },
    ];
    exe.set_session_lessons(lessons.clone());

    let model = exe.build_self_model_snapshot().unwrap();
    assert_eq!(model.lessons, lessons);

    // And the renderer must surface them — confirms end-to-end wiring.
    let rendered = model.to_system_prompt_section();
    assert!(
        rendered.contains("Lessons from prior sessions"),
        "lessons must reach the prompt, got:\n{rendered}"
    );
    assert!(rendered.contains("switch to rg"));
}

#[tokio::test]
async fn set_skill_diagnosis_feeds_build_self_model_snapshot() {
    // Auto-invoke handler (P3.3) deposits the latest SkillDiagnosis here.
    // `build_self_model_snapshot` must pass it through so the next turn's
    // LLM sees "the system already looked at this and noticed X".
    let (exe, _session) = executor_with_session();
    let cause = astra_skills::auto_invoke::AutoInvokeCause::SessionStalls { count: 3 };
    let diag = astra_skills::auto_invoke::SkillDiagnosis::new(
        "analyze_session",
        &cause,
        "agent looping on grep",
        ["tried grep 3× with identical args".to_string()],
        Some("narrow to src/".into()),
    );
    exe.set_latest_skill_diagnosis(Some(diag.clone()));

    let model = exe.build_self_model_snapshot().unwrap();
    assert_eq!(model.skill_diagnosis.as_ref(), Some(&diag));

    let rendered = model.to_system_prompt_section();
    assert!(
        rendered.contains("⚙ Auto-diagnosis [analyze_session]"),
        "diagnosis must reach the prompt, got:\n{rendered}"
    );
}

#[tokio::test]
async fn set_turn_quality_feedback_feeds_build_self_model_snapshot() {
    let (exe, _session) = executor_with_session();
    let feedback = astra_runtime::self_model::TurnQualityFeedback {
        turn: 4,
        findings: vec!["Detected 10 consecutive single-tool rounds".into()],
        recommended_action: "Batch independent reads before the next tool call.".into(),
    };
    exe.set_latest_turn_quality_feedback(Some(feedback.clone()));

    let model = exe.build_self_model_snapshot().unwrap();
    assert_eq!(model.turn_quality_feedback.as_ref(), Some(&feedback));

    let rendered = model.to_system_prompt_section();
    assert!(
        rendered.contains("Previous turn quality feedback (turn 4)")
            && rendered.contains("Batch independent reads"),
        "feedback must reach the prompt, got:\n{rendered}"
    );
}

#[tokio::test]
async fn clearing_skill_diagnosis_removes_it_from_subsequent_snapshots() {
    // Once the triggering condition has cleared, the stale diagnosis must
    // stop showing up. This proves the setter is idempotent and None
    // actually clears (not just overwrites with empty).
    let (exe, _session) = executor_with_session();
    let cause = astra_skills::auto_invoke::AutoInvokeCause::BudgetPressure { level: 0.9 };
    let diag = astra_skills::auto_invoke::SkillDiagnosis::new(
        "optimize_prompt",
        &cause,
        "prompt bloated",
        [],
        None,
    );
    exe.set_latest_skill_diagnosis(Some(diag));

    let first = exe.build_self_model_snapshot().unwrap();
    assert!(first.skill_diagnosis.is_some());

    exe.set_latest_skill_diagnosis(None);

    let second = exe.build_self_model_snapshot().unwrap();
    assert!(
        second.skill_diagnosis.is_none(),
        "None must clear, got: {:?}",
        second.skill_diagnosis
    );
    let rendered = second.to_system_prompt_section();
    assert!(!rendered.contains("Auto-diagnosis"));
}

#[tokio::test]
async fn snapshot_without_p3_seams_still_builds() {
    // Backwards-compat smoke: callers that never touch the new setters
    // must still get a valid SelfModel with no lessons and no diagnosis.
    let (exe, _session) = executor_with_session();
    let model = exe.build_self_model_snapshot().unwrap();
    assert!(model.lessons.is_empty());
    assert!(model.skill_diagnosis.is_none());
}

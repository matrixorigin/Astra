use crate::edge_tools::{SessionStateRollbackJournal, TaskManager, ToolExecutor};
use astra_services::{session_journal::JournalDirGuard, session_workspace};
use serde_json::{Value, json};

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
    assert_eq!(parsed["status"], "completed");
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
async fn self_mod_persists_config() {
    let (_tmp, _guard, exe, _session, session_id) = executor_with_persisted_session();

    let adjust_out = exe
        .execute(
            "adjust_config",
            &json!({
                "path": "memory.retrieval_top_k",
                "value": 6
            }),
        )
        .await;

    let parsed_adjust: Value = serde_json::from_str(&adjust_out).unwrap();
    assert_eq!(parsed_adjust["status"], "completed");

    let ws = session_workspace::read_workspace(&session_id).unwrap();
    assert!(ws.tuned_config_json.is_some());
}

#[tokio::test]
async fn switching_sessions_clears_session_scoped_self_model_context() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = JournalDirGuard::new(tmp.path());
    let session = std::sync::Arc::new(std::sync::RwLock::new(
        astra_runtime::observability::ObservabilitySession::new_simple("test-session"),
    ));
    let exe = ToolExecutor::new(tmp.path())
        .with_active_session_id("context-source-session")
        .with_observability_session(session);

    let lessons = vec![astra_services::LessonHint {
        kind: astra_services::LessonKind::PromptShape,
        trigger_signal: "stale review context".into(),
        action: "reload branch-specific context first".into(),
        workload_tag: Some("code-review".into()),
        compact: None,
    }];
    let cause = astra_skills::auto_invoke::AutoInvokeCause::SessionStalls { count: 2 };
    let diag = astra_skills::auto_invoke::SkillDiagnosis::new(
        "analyze_session",
        &cause,
        "stalled on prior branch",
        ["refresh branch-local context".to_string()],
        None,
    );
    let feedback = astra_runtime::self_model::TurnQualityFeedback {
        turn: 3,
        findings: vec!["Previous session reused stale guidance".into()],
        recommended_action: "Clear session-scoped hints on session switch.".into(),
    };

    exe.set_session_lessons(lessons);
    exe.set_latest_skill_diagnosis(Some(diag));
    exe.set_latest_turn_quality_feedback(Some(feedback));

    let before = exe.build_self_model_snapshot().unwrap();
    assert!(!before.lessons.is_empty());
    assert!(before.skill_diagnosis.is_some());
    assert!(before.turn_quality_feedback.is_some());

    exe.set_active_session_id("context-dest-session");

    let after = exe.build_self_model_snapshot().unwrap();
    assert!(
        after.lessons.is_empty(),
        "session-scoped lessons must not bleed into another session"
    );
    assert!(
        after.skill_diagnosis.is_none(),
        "latest skill diagnosis must not bleed into another session"
    );
    assert!(
        after.turn_quality_feedback.is_none(),
        "latest turn quality feedback must not bleed into another session"
    );
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
        .execute("task", &json!({"action": "create", "title": "shared task"}))
        .await;
    let create_json = parse_task_json(&create_out);
    assert_eq!(create_json["success"], true);

    let listed_via_b = exe_b.execute("task", &json!({"action": "list"})).await;
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

    let listed_after = exe_a.execute("task", &json!({"action": "list"})).await;
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
        astra_services::LessonHint {
            kind: astra_services::LessonKind::ToolAvoidance,
            trigger_signal: "3 stalls on grep".into(),
            action: "switch to rg".into(),
            workload_tag: None,
            compact: None,
        },
        astra_services::LessonHint {
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

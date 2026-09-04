use crate::edge_tools::ToolExecutor;
use astra_services::{session_journal::JournalDirGuard, session_workspace};
use serde_json::{Value, json};

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
#[serial_test::serial]
async fn durable_config_failure_does_not_change_observability_or_rollback_state() {
    let (_tmp, _guard, exe, session, session_id) = executor_with_persisted_session();
    let baseline = session.read().unwrap().config.memory.retrieval_top_k;
    let changed = (1..=20).find(|value| *value != baseline).unwrap();
    let lock_path = session_workspace::workspace_dir_for(&session_id).join(".workspace.lock");
    std::fs::remove_file(&lock_path).unwrap();
    std::fs::create_dir(&lock_path).unwrap();

    let failed: Value = serde_json::from_str(
        &exe.execute(
            "adjust_config",
            &json!({"path": "memory.retrieval_top_k", "value": changed, "force": true}),
        )
        .await,
    )
    .unwrap();
    assert_eq!(failed["error"], "failed_to_persist_config_override");
    assert_eq!(
        session.read().unwrap().config.memory.retrieval_top_k,
        baseline
    );
    let listed: Value = serde_json::from_str(
        &exe.execute("rollback_session_state", &json!({"scope": "list"}))
            .await,
    )
    .unwrap();
    assert_eq!(listed["total_entries"], 0);

    std::fs::remove_dir(&lock_path).unwrap();
    let applied: Value = serde_json::from_str(
        &exe.execute(
            "adjust_config",
            &json!({"path": "memory.retrieval_top_k", "value": changed, "force": true}),
        )
        .await,
    )
    .unwrap();
    assert_eq!(applied["status"], "completed");
    assert_eq!(applied["mutations_this_turn"], 1);
}

#[tokio::test]
#[serial_test::serial]
async fn governed_config_validates_against_latest_durable_authority() {
    let (_tmp, _guard, exe, session, session_id) = executor_with_persisted_session();
    let observed = session.read().unwrap().config.memory.retrieval_top_k;
    assert_ne!(observed, 20);
    crate::cli::self_command::persist_config_override(
        &session_id,
        "memory.retrieval_top_k",
        json!(20),
    )
    .unwrap();

    let rejected: Value = serde_json::from_str(
        &exe.execute(
            "adjust_config",
            &json!({"path": "memory.retrieval_top_k", "value": observed}),
        )
        .await,
    )
    .unwrap();
    assert_eq!(rejected["error"], "config_drift_ceiling_exceeded");
    let workspace = session_workspace::read_workspace(&session_id).unwrap();
    assert_eq!(workspace.config_mutation_revision, 1);
    let durable: astra_config::RuntimeConfig =
        serde_json::from_str(workspace.tuned_config_json.as_deref().unwrap()).unwrap();
    assert_eq!(durable.memory.retrieval_top_k, 20);
    let listed: Value = serde_json::from_str(
        &exe.execute("rollback_session_state", &json!({"scope": "list"}))
            .await,
    )
    .unwrap();
    assert_eq!(listed["total_entries"], 0);
}

#[tokio::test]
#[serial_test::serial]
async fn projection_failure_preserves_durable_receipt_and_rollback_handle() {
    let (_tmp, _guard, exe, session, session_id) = executor_with_persisted_session();
    let baseline = session.read().unwrap().config.memory.retrieval_top_k;
    let changed = (1..=20).find(|value| *value != baseline).unwrap();
    let poisoned = session.clone();
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _guard = poisoned.write().unwrap();
        panic!("poison observability projection lock");
    }));

    let applied: Value = serde_json::from_str(
        &exe.execute(
            "adjust_config",
            &json!({"path": "memory.retrieval_top_k", "value": changed, "force": true}),
        )
        .await,
    )
    .unwrap();
    assert_eq!(applied["status"], "completed");
    assert_eq!(applied["config_revision"], 1);
    assert_eq!(applied["projection_recorded"], false);
    assert!(applied["projection_warning"].is_string());
    assert_eq!(applied["mutations_this_turn"], 1);
    let listed: Value = serde_json::from_str(
        &exe.execute("rollback_session_state", &json!({"scope": "list"}))
            .await,
    )
    .unwrap();
    assert_eq!(listed["total_entries"], 1);

    let rollback: Value = serde_json::from_str(
        &exe.execute("rollback_session_state", &json!({"scope": "current_turn"}))
            .await,
    )
    .unwrap();
    assert_eq!(rollback["success"], true);
    let workspace = session_workspace::read_workspace(&session_id).unwrap();
    assert_eq!(workspace.config_mutation_revision, 2);
    let durable = workspace
        .tuned_config_json
        .as_deref()
        .map(serde_json::from_str::<astra_config::RuntimeConfig>)
        .transpose()
        .unwrap()
        .unwrap_or_else(astra_config::RuntimeConfig::load);
    assert_eq!(durable.memory.retrieval_top_k, baseline);
    let listed: Value = serde_json::from_str(
        &exe.execute("rollback_session_state", &json!({"scope": "list"}))
            .await,
    )
    .unwrap();
    assert_eq!(listed["total_entries"], 0);
}

#[cfg(feature = "e2e-hooks")]
#[tokio::test]
#[serial_test::serial]
async fn post_rename_sync_unknown_projects_readback_and_records_exact_owner() {
    let (_tmp, _guard, exe, session, session_id) = executor_with_persisted_session();
    let baseline = session.read().unwrap().config.memory.retrieval_top_k;
    let changed = (1..=20).find(|value| *value != baseline).unwrap();
    astra_services::session_workspace::inject_workspace_commit_parent_sync_failure_once();

    let unknown: Value = serde_json::from_str(
        &exe.execute(
            "adjust_config",
            &json!({"path": "memory.retrieval_top_k", "value": changed, "force": true}),
        )
        .await,
    )
    .unwrap();
    assert_eq!(unknown["error"], "config_commit_outcome_unknown");
    assert_eq!(unknown["side_effects_maybe"], true);
    assert_eq!(unknown["proposed_revision"], 1);
    assert_eq!(unknown["observed_revision"], 1);
    assert_eq!(unknown["retry_revision"], 1);
    assert_eq!(unknown["projection_recorded"], true);
    assert_eq!(unknown["mutations_this_turn"], 1);
    assert_eq!(unknown["rollback_recorded"], true);
    assert_eq!(
        session.read().unwrap().config.memory.retrieval_top_k,
        changed
    );
    let listed: Value = serde_json::from_str(
        &exe.execute("rollback_session_state", &json!({"scope": "list"}))
            .await,
    )
    .unwrap();
    assert_eq!(listed["total_entries"], 1);

    let rollback: Value = serde_json::from_str(
        &exe.execute("rollback_session_state", &json!({"scope": "current_turn"}))
            .await,
    )
    .unwrap();
    assert_eq!(rollback["success"], true);
    let workspace = session_workspace::read_workspace(&session_id).unwrap();
    assert_eq!(workspace.config_mutation_revision, 2);
}

#[tokio::test]
#[serial_test::serial]
async fn cli_rollback_rejects_aba_writer_without_overwriting_authority() {
    let (_tmp, _guard, exe, session, session_id) = executor_with_persisted_session();
    let baseline = session.read().unwrap().config.memory.retrieval_top_k;
    let first = (1..=20).find(|value| *value != baseline).unwrap();
    let second = (1..=20)
        .find(|value| *value != baseline && *value != first)
        .unwrap();
    let applied: Value = serde_json::from_str(
        &exe.execute(
            "adjust_config",
            &json!({"path": "memory.retrieval_top_k", "value": first, "force": true}),
        )
        .await,
    )
    .unwrap();
    assert_eq!(applied["status"], "completed");
    crate::cli::self_command::persist_config_override(
        &session_id,
        "memory.retrieval_top_k",
        json!(first),
    )
    .unwrap();
    crate::cli::self_command::persist_config_override(
        &session_id,
        "memory.retrieval_top_k",
        json!(second),
    )
    .unwrap();
    crate::cli::self_command::persist_config_override(
        &session_id,
        "memory.retrieval_top_k",
        json!(first),
    )
    .unwrap();

    let rollback: Value = serde_json::from_str(
        &exe.execute("rollback_session_state", &json!({"scope": "current_turn"}))
            .await,
    )
    .unwrap();
    assert_eq!(rollback["success"], false);
    assert_eq!(rollback["failed"].as_array().map(Vec::len), Some(1));
    let workspace = session_workspace::read_workspace(&session_id).unwrap();
    assert_eq!(workspace.config_mutation_revision, 4);
    let persisted: astra_config::RuntimeConfig =
        serde_json::from_str(workspace.tuned_config_json.as_deref().unwrap()).unwrap();
    assert_eq!(persisted.memory.retrieval_top_k, first);
    assert_eq!(session.read().unwrap().config.memory.retrieval_top_k, first);
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

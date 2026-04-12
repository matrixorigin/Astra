use super::*;
use astra_services::{session_journal::JournalDirGuard, session_workspace};

fn executor_with_session() -> (
    ToolExecutor,
    std::sync::Arc<
        std::sync::RwLock<astra_runtime::observability_integration::ObservabilitySession>,
    >,
) {
    let session = std::sync::Arc::new(std::sync::RwLock::new(
        astra_runtime::observability_integration::ObservabilitySession::new_simple("test-session"),
    ));
    let exe = ToolExecutor::new(std::env::temp_dir()).with_observability_session(session.clone());
    (exe, session)
}

fn executor_with_persisted_session() -> (
    tempfile::TempDir,
    JournalDirGuard,
    ToolExecutor,
    std::sync::Arc<
        std::sync::RwLock<astra_runtime::observability_integration::ObservabilitySession>,
    >,
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
        astra_runtime::observability_integration::ObservabilitySession::new_simple("test-session"),
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
async fn set_goal_and_compress_context_update_session_state() {
    let (exe, session) = executor_with_session();

    let set_goal_out = exe
        .execute("set_goal", &json!({"goal": "Finish adaptive engine"}))
        .await;
    let parsed_goal: Value = serde_json::from_str(&set_goal_out).unwrap();
    assert_eq!(parsed_goal["status"], "ok");

    let compress_out = exe
        .execute(
            "compress_context",
            &json!({"reason": "manual compression before long run"}),
        )
        .await;
    let parsed_compress: Value = serde_json::from_str(&compress_out).unwrap();
    assert_eq!(parsed_compress["status"], "ok");

    let guard = session.read().unwrap();
    assert_eq!(guard.compressed_turns.len(), 1);
    assert_eq!(
        guard.goal_tracker.as_ref().map(|g| g.goal()),
        Some("Finish adaptive engine")
    );
}

#[tokio::test]
async fn legacy_goal_dimension_exposes_steering_state() {
    let (exe, _session) = executor_with_session();

    let set_goal_out = exe
        .execute("set_goal", &json!({"goal": "Finish adaptive engine"}))
        .await;
    let parsed_goal: Value = serde_json::from_str(&set_goal_out).unwrap();
    assert_eq!(parsed_goal["status"], "ok");

    let goals_out = exe
        .execute("get_agent_info", &json!({"dimension": "goals"}))
        .await;
    let parsed_goals: Value = serde_json::from_str(&goals_out).unwrap();
    assert_eq!(parsed_goals["goal"], "Finish adaptive engine");
    assert_eq!(parsed_goals["session_goal"], "Finish adaptive engine");
    assert_eq!(parsed_goals["tracked_goal"], "Finish adaptive engine");
    assert_eq!(parsed_goals["goal_source"], "session_goal");
    assert_eq!(parsed_goals["tracking_status"], "aligned");
}

#[tokio::test]
async fn persisted_set_goal_appends_goal_steered_journal_event() {
    let (_tmp, _guard, exe, _session, session_id) = executor_with_persisted_session();

    let out = exe
        .execute("set_goal", &json!({"goal": "Persist steering event"}))
        .await;
    let parsed: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(parsed["status"], "ok");

    let events = astra_services::session_journal::read_journal(&session_id).unwrap();
    let goal_event = events
        .iter()
        .find(|event| {
            event.event_type == astra_services::session_journal::JournalEventType::GoalSteered
        })
        .expect("goal_steered event");
    let metadata = goal_event
        .metadata
        .as_ref()
        .expect("goal steering metadata");
    assert_eq!(
        metadata.get("source").and_then(|value| value.as_str()),
        Some("edge_tool:set_goal")
    );
    assert_eq!(
        metadata.get("new_goal").and_then(|value| value.as_str()),
        Some("Persist steering event")
    );
}

#[tokio::test]
async fn self_mod_persists_goal_config_and_tool_preferences() {
    let (_tmp, _guard, exe, _session, session_id) = executor_with_persisted_session();

    let prioritize_out = exe
        .execute("prioritize_tool", &json!({"tool": "bash"}))
        .await;
    let goal_out = exe
        .execute("set_goal", &json!({"goal": "Persist self state"}))
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
    let parsed_goal: Value = serde_json::from_str(&goal_out).unwrap();
    let parsed_adjust: Value = serde_json::from_str(&adjust_out).unwrap();
    assert_eq!(parsed_prioritize["status"], "ok");
    assert_eq!(parsed_goal["status"], "ok");
    assert_eq!(parsed_adjust["status"], "ok");

    let ws = session_workspace::read_workspace(&session_id).unwrap();
    assert!(ws.pinned_tools.contains(&"bash".to_string()));
    assert_eq!(ws.session_goal.as_deref(), Some("Persist self state"));
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
        astra_runtime::observability_integration::ObservabilitySession::new_simple("test-session"),
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
        astra_runtime::observability_integration::ObservabilitySession::new_simple("test-session"),
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
async fn get_agent_info_snapshot_uses_persistent_surface() {
    let (_tmp, _guard, exe, _session, session_id) = executor_with_persisted_session();
    let mut ws = session_workspace::read_workspace(&session_id).unwrap();
    ws.session_goal = Some("Snapshot bridge".to_string());
    session_workspace::write_workspace(&ws).unwrap();

    let out = exe
        .execute("get_agent_info", &json!({"dimension": "snapshot"}))
        .await;
    let parsed: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(parsed["run"]["session_id"], session_id);
    assert_eq!(parsed["run"]["goal"], "Snapshot bridge");
}

#[tokio::test]
async fn get_agent_info_legacy_dimensions_alias_persistent_surfaces() {
    let (_tmp, _guard, exe, _session, session_id) = executor_with_persisted_session();
    let mut ws = session_workspace::read_workspace(&session_id).unwrap();
    ws.session_goal = Some("Legacy alias".to_string());
    session_workspace::write_workspace(&ws).unwrap();

    let goals = exe
        .execute("get_agent_info", &json!({"dimension": "goals"}))
        .await;
    let state = exe
        .execute("get_agent_info", &json!({"dimension": "state"}))
        .await;
    let all = exe
        .execute("get_agent_info", &json!({"dimension": "all"}))
        .await;

    let parsed_goals: Value = serde_json::from_str(&goals).unwrap();
    let parsed_state: Value = serde_json::from_str(&state).unwrap();
    let parsed_all: Value = serde_json::from_str(&all).unwrap();

    assert_eq!(parsed_goals["goal"], "Legacy alias");
    assert_eq!(parsed_state["run"]["session_id"], session_id);
    assert_eq!(parsed_all["run"]["session_id"], session_id);
}

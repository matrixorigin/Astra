use super::*;

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

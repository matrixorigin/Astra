//! Post-commit side effects that run after the primary turn is durable.

use std::time::Instant;

use crate::cli::notifications;
use crate::cli::session::session_improvement;
use crate::cli::session::session_projection::{
    CslCheckpointFields, build_full_session_state_compact,
    rebuild_continuation_anchor_from_live_state,
};
use crate::cli::session::session_runtime;
use crate::cli::session::session_side_effects::close_pending_memory_feedback_at_turn_end;
use crate::cli::session::session_state::SessionState;
use crate::cli::stream::streaming_types::StreamResult;

pub(crate) async fn run_turn_post_commit_tasks(
    state: &mut SessionState,
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    final_messages: Vec<serde_json::Value>,
    csl_checkpoint_fields: CslCheckpointFields,
    turn_start: Instant,
    ui: &mut dyn crate::cli::ui_adapter::ReplUiAdapter,
) {
    let cloud_base = match crate::cli::config_manager::resolve_api_url(None) {
        Ok(base) => Some(base),
        Err(error) => {
            tracing::warn!(
                error = %error,
                "skipping pending recall feedback close because API URL configuration is invalid"
            );
            None
        }
    };
    let report = close_pending_memory_feedback_at_turn_end(
        state.session_id.as_deref(),
        cloud_base,
        session_runtime::current_access_token(profile),
        "cli-turn-end",
    )
    .await;
    if report.attempted > 0 {
        tracing::debug!(
            session_id = ?state.session_id,
            attempted = report.attempted,
            succeeded = report.succeeded,
            failed = report.failed,
            "closed pending recall feedback at turn end"
        );
    }

    rebuild_continuation_anchor_from_live_state(state).await;
    persist_turn_csl_snapshot(state, final_messages, csl_checkpoint_fields).await;
    sync_plan_mode_mirror_after_turn(state, api, profile, ui).await;
    session_improvement::check_skill_improvement_async(state).await;
    maybe_spawn_turn_completion_notification(state, turn_start.elapsed());
}

async fn persist_turn_csl_snapshot(
    state: &mut SessionState,
    final_messages: Vec<serde_json::Value>,
    csl_checkpoint_fields: CslCheckpointFields,
) {
    let turn = state.turn;
    let prev_state = state
        .csl_manager
        .as_ref()
        .map(|manager| manager.last_session_state().clone())
        .unwrap_or_default();
    let session_state = build_full_session_state_compact(state, csl_checkpoint_fields, &prev_state);
    if let Some(manager) = state.csl_manager.as_mut()
        && let Err(error) = manager
            .persist_turn(turn, &final_messages, &session_state)
            .await
    {
        astra_core::agent_warn!("csl", "persist failed: {error}");
    }
}

async fn sync_plan_mode_mirror_after_turn(
    state: &mut SessionState,
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    ui: &mut dyn crate::cli::ui_adapter::ReplUiAdapter,
) {
    if state.plan_mode_active()
        && let Some(token) = session_runtime::current_access_token(profile)
        && let Err(error) =
            crate::cli::plan::plan_lifecycle::sync_remote_plan_mode_state(api, &token, state).await
    {
        state.plan_mode_sync_error = Some(error.clone());
        ui.show_warning(&format!(
            "  Plan mirror sync failed; local plan may be stale. Send another planning turn after the server recovers, or use /plan to exit and re-enter before `go`. ({error})"
        ));
        astra_core::agent_warn!("plan", "failed to sync mirrored plan mode state: {error}");
    }
}

fn maybe_spawn_turn_completion_notification(state: &SessionState, elapsed: std::time::Duration) {
    let notif_config = notifications::NotificationConfig {
        enabled: state.notifications_enabled,
        method: state.notification_method,
        min_duration_secs: state.notification_threshold_secs,
    };
    if notif_config.enabled && notif_config.exceeds_threshold(elapsed) {
        tokio::spawn(async move {
            notifications::notify_completion(&notif_config, "Astra", "Turn completed", elapsed)
                .await;
        });
    }
}

pub(crate) fn extract_csl_fields_from_result(_result: &StreamResult) -> CslCheckpointFields {
    CslCheckpointFields
}

#[cfg(test)]
mod tests {
    use super::{
        build_full_session_state_compact, extract_csl_fields_from_result,
        persist_turn_csl_snapshot, run_turn_post_commit_tasks,
    };
    use crate::cli::session::session_projection::CslCheckpointFields;
    use crate::cli::session::session_state::SessionState;
    use std::time::Instant;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var(key).ok();
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.previous.as_deref() {
                Some(value) => unsafe { std::env::set_var(self.key, value) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    #[test]
    fn extract_csl_fields_from_result_without_checkpoint_returns_empty_projection() {
        let result = crate::tests::stub_stream_result("done");

        let state = SessionState {
            recent_tools: vec!["bash".into()],
            ..Default::default()
        };
        let compact = build_full_session_state_compact(
            &state,
            extract_csl_fields_from_result(&result),
            &Default::default(),
        );

        assert_eq!(compact.recent_tools, vec!["bash".to_string()]);
        assert!(compact.blocked_tools.is_empty());
        assert!(compact.approval_overrides.is_none());
        assert!(compact.interruption.is_none());
        assert!(compact.compaction_tracker.is_none());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn csl_persist_writes_prompt_facing_messages_only() {
        use astra_turn_core::conversation_log::{
            CslStore, file_store::FileCslStore, manager::CslManager,
        };

        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("csl-prompt-facing-{}", uuid::Uuid::new_v4());
        let store = std::sync::Arc::new(FileCslStore::new(
            astra_services::session_journal::local_owner_sessions_dir(),
        ));
        let mgr = CslManager::new(store.clone(), session_id.clone(), Default::default()).unwrap();
        let mut state = SessionState {
            session_id: Some(session_id.clone()),
            csl_manager: Some(mgr),
            turn: 1,
            ..Default::default()
        };
        let final_messages = vec![
            serde_json::json!({"role": "user", "content": "old review"}),
            serde_json::json!({"role": "system", "content": "[Context compacted: older messages were removed to reduce token pressure. The conversation continues below.]"}),
            serde_json::json!({"role": "user", "content": "不要review啊！"}),
            serde_json::json!({"role": "assistant", "reasoning_content": "trace"}),
            serde_json::json!({"role": "tool", "tool_call_id": "c1", "content": "tool output"}),
            serde_json::json!({"role": "assistant", "content": "ok"}),
        ];

        persist_turn_csl_snapshot(&mut state, final_messages, CslCheckpointFields).await;

        let entries = store.load_from_latest_snapshot(&session_id).await.unwrap();
        let mat = astra_turn_core::conversation_log::materialize(&entries).unwrap();
        assert_eq!(mat.messages.len(), 3);
        assert!(
            mat.messages
                .iter()
                .all(|msg| msg["role"] != "tool" && msg.get("reasoning_content").is_none())
        );
        assert!(
            mat.messages
                .iter()
                .all(|msg| !msg["content"].as_str().unwrap_or("").contains("old review"))
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn run_turn_post_commit_tasks_drains_pending_recall_feedback_before_returning() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let server = MockServer::start().await;
        let _api_url = EnvVarGuard::set("ASTRA_API_URL", &server.uri());
        let _token = EnvVarGuard::set("ASTRA_ACCESS_TOKEN", "token");
        Mock::given(method("POST"))
            .and(path("/memory/feedback/m1"))
            .and(header("authorization", "Bearer token"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .expect(1)
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let mut state = SessionState::default();
        let session_id = "sess-memory-drain";
        state.session_id = Some(session_id.to_string());
        astra_tools::memoria::MemoriaClient::reset_recall_ledger(session_id);
        astra_tools::memoria::MemoriaClient::record_recall(session_id, 1, vec!["m1".into()]);
        let mut ui = crate::tests::TestUi::default();

        run_turn_post_commit_tasks(
            &mut state,
            &api,
            None,
            Vec::new(),
            extract_csl_fields_from_result(&crate::tests::stub_stream_result(
                "Used the recalled memory.",
            )),
            Instant::now(),
            &mut ui,
        )
        .await;

        assert_eq!(
            astra_tools::memoria::MemoriaClient::pending_recall_count(session_id),
            0,
            "turn completion must synchronously drain recall feedback so cleanup cannot race it"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn run_turn_post_commit_tasks_warns_and_marks_plan_mirror_stale_when_sync_fails() {
        let (_tmp, _g) = crate::tests::isolated_sessions_dir();
        let _env = EnvVarGuard::set("ASTRA_ACCESS_TOKEN", "token");
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/plans"))
            .and(header("authorization", "Bearer token"))
            .and(query_param("session_id", "sess-1"))
            .and(query_param("phase", "planning"))
            .and(query_param("limit", "1"))
            .respond_with(ResponseTemplate::new(500).set_body_json(serde_json::json!({
                "detail": "boom"
            })))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let mut state = SessionState::default();
        state.session_id = Some("sess-1".to_string());
        state
            .perm_manager
            .set_mode(crate::cli::permission_manager::PermissionMode::Plan);
        state.cloud_plan_mirror = Some(astra_runtime::plan::PlanModeState::new(
            "Ship auth".to_string(),
        ));
        let mut ui = crate::tests::TestUi::default();

        run_turn_post_commit_tasks(
            &mut state,
            &api,
            None,
            Vec::new(),
            CslCheckpointFields,
            Instant::now(),
            &mut ui,
        )
        .await;

        let error = state
            .plan_mode_sync_error
            .as_deref()
            .expect("sync failure should be recorded");
        assert!(error.contains("500"), "got: {error}");
        assert_eq!(
            state
                .cloud_plan_mirror
                .as_ref()
                .map(|plan| plan.goal.as_str()),
            Some("Ship auth"),
        );
        assert!(
            ui.warnings
                .iter()
                .any(|msg| msg.contains("Plan mirror sync failed")),
            "user should see an actionable warning when plan sync fails: {:?}",
            ui.warnings
        );
    }
}

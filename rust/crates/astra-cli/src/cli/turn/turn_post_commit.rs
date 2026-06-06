//! Post-commit side effects that run after the primary turn is durable.

use std::time::Instant;

use crate::StreamResult;
use crate::cli::session::session_improvement;
use crate::cli::session::session_projection::{
    CslCheckpointFields, build_full_session_state_compact,
    rebuild_continuation_anchor_from_live_state,
};
use crate::cli::session::session_recovery;
use crate::cli::session::session_side_effects::close_pending_memory_feedback_at_turn_end;
use crate::cli::session::session_state::SessionState;

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
        super::session_runtime::current_access_token(profile),
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
        && let Some(token) = super::session_runtime::current_access_token(profile)
        && let Err(error) =
            crate::cli::plan_lifecycle::sync_remote_plan_mode_state(api, &token, state).await
    {
        state.plan_mode_sync_error = Some(error.clone());
        ui.show_warning(&format!(
            "  Plan mirror sync failed; local plan may be stale. Send another planning turn after the server recovers, or use /plan to exit and re-enter before `go`. ({error})"
        ));
        astra_core::agent_warn!("plan", "failed to sync mirrored plan mode state: {error}");
    }
}

fn maybe_spawn_turn_completion_notification(state: &SessionState, elapsed: std::time::Duration) {
    let notif_config = super::notifications::NotificationConfig {
        enabled: state.notifications_enabled,
        method: state.notification_method,
        min_duration_secs: state.notification_threshold_secs,
    };
    if notif_config.enabled && notif_config.exceeds_threshold(elapsed) {
        tokio::spawn(async move {
            super::notifications::notify_completion(
                &notif_config,
                "Astra",
                "Turn completed",
                elapsed,
            )
            .await;
        });
    }
}

pub(crate) fn extract_csl_fields_from_result(result: &StreamResult) -> CslCheckpointFields {
    if let Some(astra_pipeline::step_protocol::StepCheckpoint::Heavy(ref heavy)) =
        result.last_heavy_checkpoint
    {
        let delegation = session_recovery::delegation_from_heavy_checkpoint(
            heavy,
            "extract_csl_fields_from_result",
        );
        CslCheckpointFields {
            blocked_tools: Some(heavy.blocked_tools.clone()),
            approval_overrides: Some(heavy.approval_overrides.clone()),
            budget_remaining_tokens: Some(heavy.budget_remaining_tokens),
            budget_remaining_rounds: Some(heavy.budget_remaining_rounds),
            consecutive_ctx_errors: Some(heavy.consecutive_context_window_errors),
            interruption: Some(heavy.interruption.clone()),
            delegation: Some(delegation),
            compaction_tracker: Some(heavy.compaction_state.clone()),
        }
    } else {
        CslCheckpointFields {
            blocked_tools: None,
            approval_overrides: None,
            budget_remaining_tokens: None,
            budget_remaining_rounds: None,
            consecutive_ctx_errors: None,
            interruption: None,
            delegation: None,
            compaction_tracker: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::session_journal;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn isolated_sessions_dir() -> (tempfile::TempDir, session_journal::JournalDirGuard) {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let guard = session_journal::JournalDirGuard::new(&sessions);
        (tmp, guard)
    }

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

    #[derive(Default)]
    struct CollectingUi {
        warnings: Vec<String>,
    }

    impl crate::cli::ui_adapter::ReplUiAdapter for CollectingUi {
        fn show_error(&mut self, _msg: &str) {}
        fn show_warning(&mut self, msg: &str) {
            self.warnings.push(msg.to_string());
        }
        fn show_info(&mut self, _msg: &str) {}
        fn show_status(&mut self, _msg: &str) {}
        fn blank_line(&mut self) {}
    }

    fn stub_stream_result(full_text: &str) -> StreamResult {
        StreamResult {
            session_id: None,
            run_id: None,
            session_persistence_error: None,
            full_text: full_text.to_string(),
            prompt_tokens: 0,
            completion_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            tool_calls_count: 0,
            tools_selected: Vec::new(),
            selected_skills: Vec::new(),
            tools_used: Vec::new(),
            tool_call_records: Vec::new(),
            budget_used: 0,
            budget_pressure: 0.0,
            stall_events: Vec::new(),
            verdict_events: Vec::new(),
            step_recorder_summary: None,
            tool_health_export: Vec::new(),
            last_heavy_checkpoint: None,
            ttft_ms: None,
            context_ms: None,
            memoria_ms: None,
            routing_domain_hint: None,
            entity_learn_skipped_no_domain: false,
            pending_context_assembly_trace: None,
            turn_observability_events: Vec::new(),
            llm_rounds: None,
            interruption: None,
            final_state: "completed".into(),
            interruption_kind: None,
            final_messages: Vec::new(),
            background_agent_results: Vec::new(),
        }
    }

    #[test]
    fn extract_csl_fields_from_result_without_checkpoint_returns_empty_fields() {
        let result = stub_stream_result("done");

        let fields = extract_csl_fields_from_result(&result);

        assert!(fields.blocked_tools.is_none());
        assert!(fields.approval_overrides.is_none());
        assert!(fields.interruption.is_none());
        assert!(fields.compaction_tracker.is_none());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn run_turn_post_commit_tasks_drains_pending_recall_feedback_before_returning() {
        let (_tmp, _g) = isolated_sessions_dir();
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
        let mut ui = CollectingUi::default();

        run_turn_post_commit_tasks(
            &mut state,
            &api,
            None,
            Vec::new(),
            extract_csl_fields_from_result(&stub_stream_result("Used the recalled memory.")),
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
        let (_tmp, _g) = isolated_sessions_dir();
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
        let mut ui = CollectingUi::default();

        run_turn_post_commit_tasks(
            &mut state,
            &api,
            None,
            Vec::new(),
            CslCheckpointFields::default(),
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

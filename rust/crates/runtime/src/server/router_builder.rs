use super::*;

pub(super) fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(meta_handlers::root_handler))
        .route("/health", get(meta_handlers::health_handler))
        .route("/auth/register", post(auth_handlers::auth_register_handler))
        .route("/auth/login", post(auth_handlers::auth_login_handler))
        .route("/auth/refresh", post(auth_handlers::auth_refresh_handler))
        .route("/auth/logout", post(auth_handlers::auth_logout_handler))
        .route("/auth/me", get(auth_handlers::auth_me_handler))
        // Memory proxy — routes edge memory tool calls through cloud for user isolation
        .route(
            "/memory/store",
            post(auth_handlers::memory_proxy_store_handler),
        )
        .route(
            "/memory/retrieve",
            post(auth_handlers::memory_proxy_retrieve_handler),
        )
        .route(
            "/memory/search",
            post(auth_handlers::memory_proxy_search_handler),
        )
        .route(
            "/memory/purge",
            post(auth_handlers::memory_proxy_purge_handler),
        )
        .route("/chat", post(chat_handlers::chat_handler))
        .route("/chat/stream", post(chat_handlers::chat_stream_handler))
        .route("/chat/turn", post(chat_handlers::chat_turn_handler))
        .route("/chat/route", post(chat_handlers::chat_route_handler))
        // Lightweight LLM proxy for verification judge / edge components
        .route(
            "/v1/chat/completions",
            post(completions::completions_handler),
        )
        // §5.5 edge callbacks (thin client / headless orchestration)
        .route(
            "/tools/result",
            post(edge_callback_handlers::post_tool_result_handler),
        )
        .route(
            "/approval/respond",
            post(edge_callback_handlers::post_approval_respond_handler),
        )
        // WebSocket endpoint for browser-based agent access
        .route("/chat/ws", get(ws_handler::ws_chat_handler))
        // WebSocket endpoint for remote edge agent connections (Phase 6)
        .route("/edge/ws", get(edge_ws_handler::edge_ws_handler))
        .route(
            "/chat/runs/{run_id}",
            get(run_handlers::get_run_status_handler).delete(run_handlers::cancel_run_handler),
        )
        .route(
            "/chat/runs/{run_id}/stream",
            get(run_handlers::stream_run_handler),
        )
        .route(
            "/chat/runs/{run_id}/pause",
            post(run_handlers::pause_run_handler),
        )
        .route(
            "/chat/runs/{run_id}/resume",
            post(run_handlers::resume_run_handler),
        )
        .route(
            "/chat/runs/{run_id}/delegate",
            post(delegation_handlers::delegate_run_handler),
        )
        .route(
            "/chat/runs/{run_id}/delegations",
            get(delegation_handlers::list_delegations_handler),
        )
        .route(
            "/chat/runs/{run_id}/delegations/pause",
            post(delegation_handlers::pause_delegations_handler),
        )
        .route(
            "/chat/runs/{run_id}/delegations/resume",
            post(delegation_handlers::resume_delegations_handler),
        )
        // Teams — CRUD + execution history
        .route(
            "/teams",
            get(team_handlers::list_teams_handler).post(team_handlers::upsert_team_handler),
        )
        .route(
            "/teams/{name}",
            get(team_handlers::get_team_handler).delete(team_handlers::delete_team_handler),
        )
        .route(
            "/teams/{name}/executions",
            get(team_handlers::list_executions_handler),
        )
        .route(
            "/teams/{name}/snapshots",
            get(team_handlers::list_snapshots_handler).post(team_handlers::create_snapshot_handler),
        )
        .route(
            "/teams/snapshots/{id}",
            delete(team_handlers::delete_snapshot_handler),
        )
        // Reflect / decision-trace
        .route(
            "/chat/session/{session_id}/reflect",
            get(reflect_handlers::reflect_session_handler),
        )
        .route(
            "/chat/session/{session_id}/decision-trace",
            get(reflect_handlers::decision_trace_handler),
        )
        .route(
            "/sessions",
            post(session_handlers::create_session_handler)
                .get(session_handlers::list_sessions_handler),
        )
        .route(
            "/sessions/{session_id}",
            get(session_handlers::get_session_handler)
                .put(session_handlers::update_session_handler)
                .delete(session_handlers::delete_session_handler),
        )
        .route(
            "/sessions/{session_id}/close",
            post(session_handlers::close_session_handler),
        )
        .route(
            "/sessions/{session_id}/resume",
            post(session_handlers::resume_session_handler),
        )
        .route(
            "/sessions/{session_id}/cancel",
            post(session_handlers::cancel_session_handler),
        )
        .route(
            "/sessions/{session_id}/activity",
            get(session_handlers::session_activity_handler),
        )
        .route("/admin/init", post(admin_handlers::admin_init_handler))
        .route(
            "/admin/audit",
            get(admin_handlers::admin_audit_logs_handler),
        )
        .route(
            "/admin/feedback/stats",
            get(admin_handlers::admin_feedback_stats_handler),
        )
        .route(
            "/admin/feedback/export",
            post(admin_handlers::admin_feedback_export_handler),
        )
        .route(
            "/admin/prompts/optimize",
            post(admin_handlers::admin_prompt_optimize_handler),
        )
        .route(
            "/admin/users/grant-role",
            post(admin_handlers::admin_grant_role_handler),
        )
        .route(
            "/admin/users/revoke-role",
            post(admin_handlers::admin_revoke_role_handler),
        )
        .route(
            "/admin/tokens",
            get(admin_handlers::admin_list_tokens_handler)
                .post(admin_handlers::admin_create_token_handler),
        )
        .route(
            "/admin/cleanup",
            post(admin_handlers::admin_cleanup_handler),
        )
        .route(
            "/api/v1/learning/health",
            get(learning_handlers::learning_health_handler),
        )
        .route(
            "/api/v1/learning/signals",
            get(learning_handlers::learning_signals_handler),
        )
        .route(
            "/api/v1/learning/stats",
            get(learning_handlers::learning_stats_handler),
        )
        .route(
            "/api/v1/learning/trigger",
            post(learning_handlers::learning_trigger_handler),
        )
        .route(
            "/api/v1/learning/feedback",
            post(reflect_handlers::learning_feedback_handler),
        )
        // Edge registry (design §5.5 / Phase 3) — register before `/agents` POST
        .route(
            "/agents/edge/heartbeat",
            post(edge_callback_handlers::post_agents_edge_heartbeat_handler),
        )
        .route(
            "/agents/edge",
            post(edge_callback_handlers::post_agents_edge_register_handler),
        )
        // Agents CRUD
        .route(
            "/agents",
            post(agents::create_agent_handler).get(agents::list_agents_handler),
        )
        .route(
            "/agents/{agent_id}",
            get(agents::get_agent_handler)
                .put(agents::update_agent_handler)
                .delete(agents::delete_agent_handler),
        )
        // Events CRUD
        .route(
            "/events",
            post(events::create_event_handler).get(events::list_events_handler),
        )
        .route(
            "/events/{event_id}",
            get(events::get_event_handler).delete(events::delete_event_handler),
        )
        .route(
            "/events/causal-chain/{causal_chain_id}",
            get(events::get_causal_chain_handler),
        )
        .route(
            "/events/session/{session_id}",
            get(events::get_session_events_handler),
        )
        // Context snapshots
        .route(
            "/context",
            post(context::create_snapshot_handler).get(context::list_snapshots_handler),
        )
        .route(
            "/context/{context_capture_id}",
            get(context::get_snapshot_handler),
        )
        // Decisions audit
        .route(
            "/decisions",
            post(decisions::record_decision_handler).get(decisions::list_decisions_handler),
        )
        .route(
            "/decisions/{decision_id}",
            get(decisions::get_decision_handler),
        )
        .route(
            "/decisions/{decision_id}/audit",
            get(decisions::audit_decision_handler),
        )
        // Models management (admin)
        .route(
            "/models",
            post(models::create_model_handler).get(models::list_models_handler),
        )
        .route(
            "/models/{model_name}",
            get(models::get_model_handler)
                .put(models::update_model_handler)
                .delete(models::delete_model_handler),
        )
        .route(
            "/models/{model_name}/check",
            post(models::check_model_handler),
        )
        // Jobs
        .route("/jobs", post(jobs::submit_job_handler))
        .route("/jobs/webhook", post(jobs::job_webhook_handler))
        .route(
            "/jobs/{job_id}",
            get(jobs::get_job_handler).delete(jobs::cancel_job_handler),
        )
        // Triggers
        .route(
            "/triggers",
            post(triggers::create_trigger_handler).get(triggers::list_triggers_handler),
        )
        .route(
            "/triggers/{trigger_id}",
            delete(triggers::delete_trigger_handler),
        )
        .route(
            "/triggers/{trigger_id}/fire",
            post(triggers::fire_webhook_handler),
        )
        // Workflows
        .route("/workflows", get(workflows::list_workflows_handler))
        .route(
            "/workflows/{workflow_id}",
            get(workflows::get_workflow_handler),
        )
        .route(
            "/workflows/runs/{run_id}",
            get(workflows::get_workflow_run_handler),
        )
        .route(
            "/workflows/runs/{run_id}/resolve",
            post(workflows::resolve_workflow_wait_handler),
        )
        // Sandbox
        .route(
            "/sandbox",
            post(sandbox::create_sandbox_handler).get(sandbox::list_sandboxes_handler),
        )
        .route(
            "/sandbox/{name}",
            get(sandbox::get_sandbox_handler).delete(sandbox::delete_sandbox_handler),
        )
        // Skills
        .route(
            "/skills",
            post(skills::register_skill_handler).get(skills::list_skills_handler),
        )
        .route("/skills/status", get(skills::get_skill_status_handler))
        .route("/skills/publish", post(skills::publish_skill_handler))
        .route(
            "/skills/{skill_name}/info",
            get(skills::get_skill_info_handler),
        )
        .route("/skills/{skill_id}", get(skills::get_skill_handler))
        .route(
            "/skills/{skill_id}/versions",
            get(skills::list_skill_versions_handler),
        )
        .route(
            "/skills/{skill_name}/unpublish",
            post(skills::unpublish_skill_handler),
        )
        // Skill config
        .route(
            "/skills/{skill_name}/config/validate",
            get(skill_config::validate_config_handler),
        )
        .route(
            "/skills/{skill_name}/config",
            get(skill_config::get_effective_config_handler),
        )
        .route(
            "/skills/{skill_name}/config/{setting_name}",
            axum::routing::put(skill_config::set_setting_handler)
                .delete(skill_config::delete_setting_handler),
        )
        .route(
            "/skills/{skill_name}/resources",
            get(skill_config::list_resources_handler),
        )
        .route(
            "/skills/{skill_name}/resources/{resource_key}",
            axum::routing::put(skill_config::bind_resource_handler)
                .delete(skill_config::unbind_resource_handler),
        )
        // Branches
        .route(
            "/branches",
            post(branches::create_branch_handler).delete(branches::delete_branch_handler),
        )
        .route("/branches/diff", post(branches::diff_branch_handler))
        .route("/branches/merge", post(branches::merge_branch_handler))
        .route(
            "/branches/cost-estimate",
            post(branches::estimate_cost_handler),
        )
        // Data versioning
        .route(
            "/data-versioning/checkpoints",
            post(data_versioning::create_checkpoint_handler)
                .get(data_versioning::list_checkpoints_handler),
        )
        .route(
            "/data-versioning/checkpoints/{name}/events",
            get(data_versioning::get_events_at_checkpoint_handler),
        )
        .route(
            "/data-versioning/lineage/{event_id}/chain",
            get(data_versioning::get_causal_chain_handler),
        )
        .route(
            "/data-versioning/lineage/{event_id}/upstream",
            get(data_versioning::trace_upstream_handler),
        )
        .route(
            "/data-versioning/sandbox/{name}/checkpoint",
            post(data_versioning::sandbox_checkpoint_handler),
        )
        .route(
            "/data-versioning/sandbox/{name}/restore",
            post(data_versioning::sandbox_restore_handler),
        )
        // Replay
        .route(
            "/sessions/{session_id}/replay",
            post(replay::replay_session_handler),
        )
        .route(
            "/sessions/{session_id}/replay/compare",
            get(replay::compare_replay_handler),
        )
        // Session Audit
        .route(
            "/sessions/{session_id}/audit/summary",
            get(audit_handlers::audit_summary_handler),
        )
        .route(
            "/sessions/{session_id}/audit/turns",
            get(audit_handlers::audit_turns_handler),
        )
        .route(
            "/sessions/{session_id}/audit/turns/{turn}",
            get(audit_handlers::audit_turn_detail_handler),
        )
        .route(
            "/sessions/{session_id}/audit/tools",
            get(audit_handlers::audit_tools_handler),
        )
        .route(
            "/sessions/{session_id}/audit/errors",
            get(audit_handlers::audit_errors_handler),
        )
        .route(
            "/sessions/{session_id}/audit/mutations",
            get(audit_handlers::audit_mutations_handler),
        )
        .route(
            "/sessions/{session_id}/audit/promotions",
            get(audit_handlers::audit_runtime_promotions_handler),
        )
        .route(
            "/sessions/{session_id}/audit/mutations/{mutation_id}/state",
            post(audit_handlers::audit_mutation_state_handler),
        )
        // Cross-session Analytics
        .route(
            "/audit/sessions",
            get(audit_handlers::list_sessions_handler),
        )
        .route(
            "/audit/stats",
            get(audit_handlers::cross_session_stats_handler),
        )
        .route(
            "/audit/tools",
            get(audit_handlers::cross_session_tools_handler),
        )
        .route(
            "/audit/mutations",
            get(audit_handlers::cross_session_mutations_handler),
        )
        .route(
            "/audit/promotions",
            get(audit_handlers::cross_session_runtime_promotions_handler),
        )
        // Marketplace
        .route(
            "/marketplace/install",
            post(marketplace::install_skill_handler),
        )
        .route(
            "/marketplace/uninstall",
            post(marketplace::uninstall_skill_handler),
        )
        .route(
            "/marketplace/upgrade",
            post(marketplace::upgrade_skill_handler),
        )
        .route(
            "/marketplace/rollback",
            post(marketplace::rollback_skill_handler),
        )
        .route(
            "/marketplace/installed",
            get(marketplace::list_installed_handler),
        )
        .route(
            "/marketplace/credentials",
            post(marketplace::save_credential_handler)
                .delete(marketplace::delete_credential_handler),
        )
        .route(
            "/marketplace/skills/{skill_name}/publish",
            post(marketplace::publish_skill_handler),
        )
        .route(
            "/marketplace/skills/{skill_name}/deprecate",
            post(marketplace::deprecate_skill_handler),
        )
        // Marketplace stats (Phase 3)
        .route(
            "/marketplace/quality-report",
            post(marketplace::submit_quality_report_handler),
        )
        .route(
            "/marketplace/stats/{skill_name}",
            get(marketplace::get_skill_stats_handler),
        )
        .route(
            "/marketplace/search",
            get(marketplace::search_marketplace_handler),
        )
        // Streaming (deprecated)
        .route("/streaming/chat", post(streaming::stream_chat_handler))
        // Evaluation
        .route(
            "/evaluation/quality/trend",
            get(evaluation::quality_trend_handler),
        )
        .route("/evaluation/drift", get(evaluation::drift_handler))
        .route("/evaluation/gates", get(evaluation::gate_history_handler))
        .route(
            "/evaluation/calibration",
            get(evaluation::calibration_handler),
        )
        .route(
            "/evaluation/sessions/scores",
            get(evaluation::session_scores_handler),
        )
        .route(
            "/evaluation/gate/validate",
            post(evaluation::gate_validate_handler),
        )
        .route("/evaluation/drift/run", post(evaluation::drift_run_handler))
        .route("/evaluation/loop", post(evaluation::closed_loop_handler))
        .route(
            "/evaluation/trust-report",
            get(evaluation::trust_report_handler),
        )
        .route(
            "/evaluation/slo/dashboard",
            get(evaluation::slo_dashboard_handler),
        )
        .route(
            "/evaluation/slo/{agent_id}/history",
            get(evaluation::slo_history_handler),
        )
        .route(
            "/evaluation/observability/metrics",
            get(evaluation::observability_metrics_handler),
        )
        .route(
            "/evaluation/memory-health",
            get(evaluation::memory_health_handler),
        )
        .route(
            "/evaluation/memory-metrics",
            get(evaluation::memory_metrics_handler),
        )
        .route(
            "/evaluation/training-data/extract",
            post(evaluation::training_data_extract_handler),
        )
        .route(
            "/evaluation/training-data/{dataset_id}/export",
            get(evaluation::training_data_export_handler),
        )
        // Introspection
        .route(
            "/introspection/memory",
            get(introspection::get_memory_introspection_handler),
        )
        // Platform snapshot (aggregated dashboard data)
        .route(
            "/platform/snapshot",
            get(platform_handlers::platform_snapshot_handler),
        )
        // Run list
        .route("/runs", get(run_handlers::list_runs_handler))
        // Tasks / Plans
        .route(
            "/tasks",
            get(task_handlers::list_tasks_handler).post(task_handlers::create_task_handler),
        )
        .route("/tasks/{task_id}", get(task_handlers::get_task_handler))
        .route(
            "/tasks/{task_id}/lease/claim",
            post(task_handlers::post_task_lease_claim_handler),
        )
        .route(
            "/tasks/{task_id}/lease/release",
            post(task_handlers::post_task_lease_release_handler),
        )
        .route(
            "/tasks/{task_id}/lease/renew",
            post(task_handlers::post_task_lease_renew_handler),
        )
        .route(
            "/tasks/{task_id}/lease",
            get(task_handlers::get_task_lease_handler),
        )
        .route(
            "/tasks/{task_id}/progress",
            get(task_handlers::task_progress_handler),
        )
        .route(
            "/tasks/{task_id}/status",
            axum::routing::put(task_handlers::update_task_status_handler),
        )
        .route(
            "/introspection/skills",
            get(introspection::get_skills_introspection_handler),
        )
        .route(
            "/introspection/context/trend",
            get(introspection::get_context_trend_handler),
        )
        .route(
            "/introspection/context/snapshot",
            get(introspection::get_context_snapshot_handler),
        )
        .route(
            "/introspection/context/retrieval_quality",
            get(introspection::get_retrieval_quality_handler),
        )
        .route(
            "/introspection/memory/recall",
            get(introspection::get_memory_recall_handler),
        )
        .with_state(state)
}

#[cfg(test)]
mod tests {
    /// Regression guard: catch accidental route removal.
    /// The file currently has 116 `.route(` calls plus 1 in this test = 117 via include_str.
    #[test]
    fn route_count_regression() {
        let source = include_str!("router_builder.rs");
        let route_count = source.matches(".route(").count();
        assert!(
            route_count >= 100,
            "Expected at least 100 .route( calls, found {route_count}. Was a route accidentally removed?"
        );
    }

    #[test]
    fn critical_route_paths_exist() {
        let source = include_str!("router_builder.rs");
        let critical_paths = [
            "/health",
            "/auth/register",
            "/auth/login",
            "/auth/refresh",
            "/auth/me",
            "/chat",
            "/chat/stream",
            "/chat/turn",
            "/chat/route",
            "/tools/result",
            "/approval/respond",
            "/chat/ws",
            "/sessions",
            "/admin/init",
            "/skills",
            "/evaluation/drift",
        ];
        for path in &critical_paths {
            assert!(
                source.contains(path),
                "Critical route {path} missing from router"
            );
        }
    }

    #[test]
    fn all_api_groups_have_routes() {
        let source = include_str!("router_builder.rs");
        let groups: &[(&str, &str)] = &[
            ("auth", "/auth/"),
            ("chat", "/chat"),
            ("sessions", "/sessions"),
            ("admin", "/admin/"),
            ("learning", "/api/v1/learning/"),
            ("agents", "/agents"),
            ("events", "/events"),
            ("skills", "/skills"),
            ("evaluation", "/evaluation/"),
            ("introspection", "/introspection/"),
            ("branches", "/branches"),
            ("marketplace", "/marketplace/"),
            ("sandbox", "/sandbox"),
            ("workflows", "/workflows"),
            ("platform", "/platform/"),
            ("runs", "/runs"),
            ("tasks", "/tasks"),
        ];
        for (group, prefix) in groups {
            assert!(
                source.contains(prefix),
                "API group '{group}' (prefix: {prefix}) missing from router"
            );
        }
    }

    #[test]
    fn all_handler_modules_referenced() {
        let source = include_str!("router_builder.rs");
        let modules = [
            "meta_handlers::",
            "auth_handlers::",
            "edge_callback_handlers::",
            "chat_handlers::",
            "session_handlers::",
            "admin_handlers::",
            "learning_handlers::",
            "run_handlers::",
            "ws_handler::",
            "reflect_handlers::",
            "platform_handlers::",
            "task_handlers::",
        ];
        for module in &modules {
            assert!(
                source.contains(module),
                "Handler module {module} not referenced in router"
            );
        }
    }
}

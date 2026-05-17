use super::*;

pub(super) fn add_routes(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/", get(meta_handlers::root_handler))
        .route("/health", get(meta_handlers::health_handler))
        .route("/metrics", get(meta_handlers::metrics_handler))
        .route("/auth/register", post(auth_handlers::auth_register_handler))
        .route("/auth/login", post(auth_handlers::auth_login_handler))
        .route("/auth/refresh", post(auth_handlers::auth_refresh_handler))
        .route("/auth/logout", post(auth_handlers::auth_logout_handler))
        .route("/auth/me", get(auth_handlers::auth_me_handler))
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
        .route(
            "/v1/chat/completions",
            post(completions::completions_handler),
        )
        .route(
            "/tools/result",
            post(edge_callback_handlers::post_tool_result_handler),
        )
        .route(
            "/approval/respond",
            post(edge_callback_handlers::post_approval_respond_handler),
        )
        .route("/chat/ws", get(ws_handler::ws_chat_handler))
        .route("/edge/ws", get(edge_ws_handler::edge_ws_handler))
        .route(
            "/edges/status",
            get(edge_status_handler::edge_status_handler),
        )
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
            "/chat/runs/{run_id}/cancel",
            post(run_handlers::cancel_run_handler),
        )
        .route(
            "/chat/runs/{run_id}/input",
            post(run_handlers::submit_run_input_handler),
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
            "/teams/{name}/execute",
            post(team_handlers::execute_team_handler),
        )
        .route(
            "/teams/{name}/snapshots",
            get(team_handlers::list_snapshots_handler).post(team_handlers::create_snapshot_handler),
        )
        .route(
            "/teams/snapshots/{id}",
            delete(team_handlers::delete_snapshot_handler),
        )
        .route(
            "/chat/session/{session_id}/reflect",
            get(reflect_handlers::reflect_session_handler),
        )
        .route(
            "/chat/session/{session_id}/decision-trace",
            get(reflect_handlers::decision_trace_handler),
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
        .route(
            "/agents/edge/heartbeat",
            post(edge_callback_handlers::post_agents_edge_heartbeat_handler),
        )
        .route(
            "/agents/edge",
            post(edge_callback_handlers::post_agents_edge_register_handler),
        )
}

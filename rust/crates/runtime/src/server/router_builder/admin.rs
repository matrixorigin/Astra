use super::*;

pub(super) fn add_routes(router: Router<AppState>, state: AppState) -> Router<AppState> {
    router
        .nest("/admin/harness", admin_harness_routes(state.clone()))
        .route(
            "/admin/register",
            post(admin_handlers::admin_register_handler),
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
            "/admin/llm/trusted-domains",
            get(llm_trusted_domains_handlers::list_llm_trusted_domains_handler)
                .put(llm_trusted_domains_handlers::upsert_llm_trusted_domain_handler),
        )
        .route(
            "/admin/llm/trusted-domains/{domain_id}",
            delete(llm_trusted_domains_handlers::delete_llm_trusted_domain_handler),
        )
        .route(
            "/admin/config",
            get(config_admin::admin_config::list_admin_config_handler),
        )
        .route(
            "/admin/config/{key}",
            get(config_admin::admin_config::get_admin_config_handler)
                .put(config_admin::admin_config::set_admin_config_handler)
                .delete(config_admin::admin_config::delete_admin_config_handler),
        )
        .route(
            "/admin/resources/limits/{user_id}",
            axum::routing::put(resource_handlers::set_resource_limits_handler),
        )
        .route(
            "/admin/tools/disabled",
            get(admin_handlers::admin_list_disabled_tools_handler)
                .put(admin_handlers::admin_disable_tool_handler),
        )
        .route(
            "/admin/tools/disabled/{tool_name}",
            delete(admin_handlers::admin_enable_tool_handler),
        )
}

#[cfg(feature = "harness")]
fn admin_harness_routes(state: AppState) -> Router<AppState> {
    use axum::routing::get;
    Router::new()
        .route(
            "/sessions",
            get(crate::server::harness::handlers::list_active_harness_sessions),
        )
        .with_state(state)
}

#[cfg(not(feature = "harness"))]
fn admin_harness_routes(_state: AppState) -> Router<AppState> {
    Router::new()
}

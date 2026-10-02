use super::super::*;

pub(super) fn add_routes(router: Router<AppState>) -> Router<AppState> {
    router
        .route(
            "/introspection/decision-trace",
            get(introspection::get_decision_trace_handler),
        )
        .route(
            "/introspection/tool-history",
            get(introspection::get_tool_history_handler),
        )
        .route(
            "/introspection/drift-check",
            get(introspection::get_drift_check_handler),
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
            "/platform/snapshot",
            get(platform_handlers::platform_snapshot_handler),
        )
}

use super::super::*;

pub(super) fn add_routes(router: Router<AppState>) -> Router<AppState> {
    router
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
            "/platform/snapshot",
            get(platform_handlers::platform_snapshot_handler),
        )
}

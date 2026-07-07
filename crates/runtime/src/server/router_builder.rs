use super::*;

mod admin;
mod automation;
mod catalog;
mod operations;
mod realtime;
mod sessions;

pub(super) fn build_router(state: AppState) -> Router {
    let router: Router<AppState> = Router::new();
    let router = realtime::add_routes(router);
    let router = sessions::add_routes(router, state.clone());
    let router = admin::add_routes(router, state.clone());
    let router = automation::add_routes(router);
    let router = catalog::add_routes(router);
    let router = operations::add_routes(router);
    router.with_state(state)
}

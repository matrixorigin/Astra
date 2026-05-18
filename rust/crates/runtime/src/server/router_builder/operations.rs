use super::*;

mod harnesses;
mod insights;
mod work;

pub(super) fn add_routes(router: Router<AppState>) -> Router<AppState> {
    harnesses::add_routes(work::add_routes(insights::add_routes(router)))
}

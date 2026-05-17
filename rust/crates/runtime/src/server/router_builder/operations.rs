use super::*;

mod insights;
mod work;

pub(super) fn add_routes(router: Router<AppState>) -> Router<AppState> {
    work::add_routes(insights::add_routes(router))
}

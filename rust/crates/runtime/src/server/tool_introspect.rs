use std::sync::RwLock;

use serde_json::Value;

pub(crate) fn handle_introspect(
    args: &Value,
    session_id: &str,
    snapshot: &RwLock<Option<astra_turn_core::introspect::IntrospectSnapshot>>,
) -> String {
    let request = astra_turn_core::introspect::IntrospectRequest::from_args(args);
    let snapshot = snapshot
        .read()
        .unwrap_or_else(|poison| {
            tracing::warn!(
                session_id = %session_id,
                "introspect_snapshot lock poisoned (writer panicked), recovering with inner data"
            );
            poison.into_inner()
        })
        .clone();

    let snapshot = snapshot.unwrap_or_default();
    astra_turn_core::introspect::render_introspect_request(&snapshot, &request)
}

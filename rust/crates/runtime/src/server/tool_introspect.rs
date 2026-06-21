use std::sync::RwLock;

use serde_json::Value;

pub(crate) fn handle_introspect(
    args: &Value,
    session_id: &str,
    snapshot: &RwLock<Option<astra_turn_core::introspect::IntrospectSnapshot>>,
) -> String {
    let detail_arg = args
        .get("detail")
        .and_then(Value::as_str)
        .unwrap_or("summary");
    let detail = astra_turn_core::introspect::IntrospectDetail::from_arg(detail_arg);

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

    match snapshot {
        Some(snapshot) => astra_turn_core::introspect::render_introspect(&snapshot, detail),
        None => "No introspection data available yet (first turn).".to_string(),
    }
}

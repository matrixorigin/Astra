use std::sync::OnceLock;

/// Stable id for this process (§5.5 `edge_executor_id`). Override with `MO_EDGE_EXECUTOR_ID`.
static EDGE_EXECUTOR_INSTANCE_ID: OnceLock<String> = OnceLock::new();

pub(crate) fn edge_executor_instance_id() -> &'static str {
    EDGE_EXECUTOR_INSTANCE_ID
        .get_or_init(|| {
            std::env::var("MO_EDGE_EXECUTOR_ID").unwrap_or_else(|_| {
                format!("edge-{}", uuid::Uuid::new_v4())
            })
        })
        .as_str()
}

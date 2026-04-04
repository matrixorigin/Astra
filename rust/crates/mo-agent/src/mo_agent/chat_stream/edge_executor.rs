use std::sync::OnceLock;

use astra_runtime::turn::edge_executor_id::random_edge_executor_instance_id;

/// Stable id for this process (§5.5 `edge_executor_id`). Override with `MO_EDGE_EXECUTOR_ID`.
static EDGE_EXECUTOR_INSTANCE_ID: OnceLock<String> = OnceLock::new();

pub(crate) fn edge_executor_instance_id() -> &'static str {
    EDGE_EXECUTOR_INSTANCE_ID
        .get_or_init(|| {
            std::env::var("MO_EDGE_EXECUTOR_ID")
                .unwrap_or_else(|_| random_edge_executor_instance_id())
        })
        .as_str()
}

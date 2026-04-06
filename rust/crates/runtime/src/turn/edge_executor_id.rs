//! Random `edge_executor_id` values for §5.5 when `ASTRA_EDGE_EXECUTOR_ID` is unset.

#[must_use]
pub fn random_edge_executor_instance_id() -> String {
    format!("edge-{}", uuid::Uuid::new_v4())
}

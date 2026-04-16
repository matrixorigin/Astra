//! Random `edge_executor_id` values for §5.5 when `ASTRA_EDGE_EXECUTOR_ID` is unset.

#[must_use]
pub fn random_edge_executor_instance_id() -> String {
    format!("edge-{}", uuid::Uuid::new_v4())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_edge_prefix() {
        let id = random_edge_executor_instance_id();
        assert!(id.starts_with("edge-"));
    }

    #[test]
    fn unique_each_call() {
        let a = random_edge_executor_instance_id();
        let b = random_edge_executor_instance_id();
        assert_ne!(a, b);
    }

    #[test]
    fn uuid_portion_valid() {
        let id = random_edge_executor_instance_id();
        let uuid_part = &id["edge-".len()..];
        assert!(uuid::Uuid::parse_str(uuid_part).is_ok());
    }
}

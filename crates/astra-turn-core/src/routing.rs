use serde_json::{Map, Value};

pub fn build_skipped_routing_metadata(reason: &str) -> Map<String, Value> {
    Map::from_iter([
        ("skipped".to_string(), Value::Bool(true)),
        ("reason".to_string(), Value::String(reason.to_string())),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_skipped_routing_fields() {
        let meta = build_skipped_routing_metadata("too short");
        assert_eq!(meta.get("skipped").and_then(Value::as_bool), Some(true));
        assert_eq!(
            meta.get("reason").and_then(Value::as_str),
            Some("too short")
        );
    }
}

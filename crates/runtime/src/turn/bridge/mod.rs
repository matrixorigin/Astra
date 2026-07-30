pub mod inprocess;
pub mod llm_stream;
pub mod observability;
pub mod sse_helpers;

pub(crate) const BRIDGE_TRANSPORT_RUN_ID_MAX_BYTES: usize = 64;
pub(crate) const BRIDGE_USER_QUERY_EVENT_ID_MAX_BYTES: usize =
    astra_services::storage::AGENT_EVENT_ID_LEN;

pub(crate) fn is_exact_bridge_identity(value: &str, max_bytes: usize) -> bool {
    !value.is_empty()
        && value.trim() == value
        && value.len() <= max_bytes
        && !value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_bridge_identity_has_one_bounded_non_normalizing_contract() {
        assert!(is_exact_bridge_identity(
            &"i".repeat(BRIDGE_TRANSPORT_RUN_ID_MAX_BYTES),
            BRIDGE_TRANSPORT_RUN_ID_MAX_BYTES,
        ));
        for invalid in [
            String::new(),
            " identity".to_string(),
            "identity ".to_string(),
            "identity\u{0007}".to_string(),
            "i".repeat(BRIDGE_TRANSPORT_RUN_ID_MAX_BYTES + 1),
        ] {
            assert!(!is_exact_bridge_identity(
                &invalid,
                BRIDGE_TRANSPORT_RUN_ID_MAX_BYTES,
            ));
        }
    }
}

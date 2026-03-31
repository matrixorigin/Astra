//! Lightweight **edge executor** helpers — transport + local tools only (design §5.5.2).
//!
//! An edge process should depend on [`crate::ThinClient`] and local tool execution, not on
//! `mo-agent` / `runtime` / cognitive pipelines.

use crate::protocol::{ChatStreamRequest, EdgeRegisterRequest};

/// HTTP header matching design doc §5.5 (`POST /tools/result`).
pub const MO_EDGE_ID_HEADER: &str = "X-Mo-Edge-Id";

/// Default `capabilities` tags for a full local toolkit (coarse buckets; server may refine).
///
/// Aligns with `multi-agent-cloud-runtime.md` chat example
/// `["bash", "fs", "git", "code_intel"]`.
pub fn builtin_capability_preset() -> Vec<String> {
    vec![
        "bash".into(),
        "fs".into(),
        "git".into(),
        "code_intel".into(),
    ]
}

/// Set `edge_executor_id` and, if `capabilities` is empty, fill [`builtin_capability_preset`].
pub fn advertise_executor(req: &mut ChatStreamRequest, executor_id: impl Into<String>) {
    req.edge_executor_id = Some(executor_id.into());
    if req.capabilities.is_empty() {
        req.capabilities = builtin_capability_preset();
    }
}

/// [`EdgeRegisterRequest`] with `capabilities` set to [`builtin_capability_preset`] as JSON (for `POST /agents/edge`).
pub fn edge_register_with_capabilities(executor_id: impl Into<String>) -> EdgeRegisterRequest {
    let mut r = EdgeRegisterRequest::new(executor_id);
    r.capabilities = Some(serde_json::json!(builtin_capability_preset()));
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertise_executor_fills_defaults() {
        let mut r = ChatStreamRequest::new("hi");
        advertise_executor(&mut r, "edge-test");
        assert_eq!(r.edge_executor_id.as_deref(), Some("edge-test"));
        assert_eq!(r.capabilities, builtin_capability_preset());
    }

    #[test]
    fn advertise_executor_respects_existing_capabilities() {
        let mut r = ChatStreamRequest::new("hi");
        r.capabilities = vec!["bash".into()];
        advertise_executor(&mut r, "e1");
        assert_eq!(r.capabilities, vec!["bash"]);
    }

    #[test]
    fn edge_register_with_capabilities_json() {
        let r = edge_register_with_capabilities("my-edge");
        assert_eq!(r.edge_agent_id, "my-edge");
        assert!(r.capabilities.as_ref().unwrap().is_array());
    }
}

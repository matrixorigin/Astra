use super::*;
pub use astra_turn_core::bridge_circuit_breaker as circuit_breaker;
pub use astra_turn_core::bridge_sse_events as sse_events;

pub mod side_effects;

pub use astra_turn_core::bridge_rate_limit_cooldown::{
    CooldownReason, PerModelCooldown, RateLimitAction, RateLimitCooldown, RateLimitMetrics,
    RateLimitState,
};

/// Header allow-list predicate: only `x-mo-*` and `authorization` headers
/// are forwarded to the upstream bridge.
#[cfg(test)]
fn is_allowed_bridge_header(name: &str) -> bool {
    name.starts_with("x-mo-") || name == "authorization"
}

#[cfg(test)]
use std::collections::HashMap;

#[derive(Clone, Debug, Default)]
pub(crate) struct InMemoryTurnReflectionStateStore {
    pub(crate) state: Arc<tokio::sync::Mutex<HashMap<String, TurnReflectionMark>>>,
}

#[derive(Clone, Debug)]
pub(crate) struct NoopTurnReflectionLessonWriter;

#[derive(Clone, Debug)]
pub struct DatabaseTurnReflectionLessonWriter {
    pub(crate) base_url: String,
    pub(crate) master_key: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct NoopTurnObserverWorker;

#[derive(Clone, Debug)]
pub struct DatabaseTurnObserverWorker {
    pub(crate) base_url: String,
    pub(crate) master_key: Option<String>,
}

pub(crate) fn sse_stream_response(status: StatusCode, body: Body) -> Response {
    Response::builder()
        .status(status)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
        .header("x-accel-buffering", "no")
        .body(body)
        .unwrap()
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Phase 6.6: Header filtering security tests ──

    #[test]
    fn allows_x_mo_headers() {
        assert!(is_allowed_bridge_header("x-mo-session-id"));
        assert!(is_allowed_bridge_header("x-mo-user-id"));
        assert!(is_allowed_bridge_header("x-mo-routing-meta-b64"));
    }

    #[test]
    fn allows_authorization() {
        assert!(is_allowed_bridge_header("authorization"));
    }

    #[test]
    fn blocks_dangerous_headers() {
        assert!(!is_allowed_bridge_header("cookie"));
        assert!(!is_allowed_bridge_header("set-cookie"));
        assert!(!is_allowed_bridge_header("host"));
        assert!(!is_allowed_bridge_header("x-forwarded-for"));
        assert!(!is_allowed_bridge_header("x-real-ip"));
        assert!(!is_allowed_bridge_header("origin"));
        assert!(!is_allowed_bridge_header("referer"));
    }

    #[test]
    fn blocks_content_type_override() {
        assert!(!is_allowed_bridge_header("content-type"));
    }

    #[test]
    fn blocks_prefix_spoof() {
        // "x-mobile" starts with "x-mo" but not "x-mo-"
        assert!(!is_allowed_bridge_header("x-mobile"));
        assert!(is_allowed_bridge_header("x-mo-"));
    }
}

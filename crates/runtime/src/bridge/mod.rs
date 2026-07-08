use super::*;
pub use astra_turn_core::bridge_circuit_breaker as circuit_breaker;
pub use astra_turn_core::bridge_sse_events as sse_events;

pub mod side_effects;

pub use astra_turn_core::bridge_rate_limit_cooldown::{
    CooldownReason, PerModelCooldown, RateLimitAction, RateLimitCooldown, RateLimitMetrics,
    RateLimitState,
};

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

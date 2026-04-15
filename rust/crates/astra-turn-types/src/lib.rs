//! Core turn types for astra runtime.
//!
//! This crate provides foundational types used during turn execution,
//! extracted from the monolithic runtime crate for better modularity.

mod counter;
mod implicit_feedback;
mod result_quality;
mod routing_metrics;
mod tool_result_quality;

pub use counter::count_persisted_turn_events;
pub use implicit_feedback::{
    ImplicitSignal, StructuredFeedback, detect_implicit_feedback_signal,
    implicit_feedback_context_injection, implicit_feedback_rating,
};
pub use result_quality::{ResultQuality, classify_result, quality_feedback};
pub use routing_metrics::{
    ConfidenceCalibrator, DisambiguationAction, IntentDisambiguation, RoutingMetricsPlan,
    build_routing_metrics_plan, disambiguate_intents,
};
pub use tool_result_quality::build_tool_result_quality_event_payload;

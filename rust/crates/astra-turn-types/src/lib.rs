//! Core turn types for astra runtime.
//!
//! This crate provides foundational types used during turn execution,
//! extracted from the monolithic runtime crate for better modularity.

pub mod continuity;
mod implicit_feedback;
mod result_quality;
pub mod session_facts;

pub use implicit_feedback::{
    ImplicitSignal, StructuredFeedback, detect_implicit_feedback_signal,
    implicit_feedback_context_injection, implicit_feedback_rating,
};
pub use result_quality::{ResultQuality, classify_result, quality_feedback};

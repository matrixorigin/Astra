//! Core turn types for astra runtime.
//!
//! This crate provides foundational types used during turn execution,
//! extracted from the monolithic runtime crate for better modularity.

mod implicit_feedback;
mod result_quality;

pub use implicit_feedback::{
    detect_implicit_feedback_signal, implicit_feedback_context_injection, implicit_feedback_rating,
    ImplicitSignal,
};
pub use result_quality::{classify_result, quality_feedback, ResultQuality};

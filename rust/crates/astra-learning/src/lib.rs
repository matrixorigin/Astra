//! Learning support primitives.
//!
//! This crate intentionally does not contain an implicit tuning control loop.
//! Durable tuning jobs should live in a dedicated control plane.

pub mod delegation;
pub mod feedback;

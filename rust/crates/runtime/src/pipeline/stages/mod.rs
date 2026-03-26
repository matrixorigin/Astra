//! Concrete pipeline stage implementations.
//!
//! Each stage is a [`PipelineStage`] trait object registered with the
//! [`ExecutionEngine`] for a specific [`AgentPhase`].

pub mod evaluate;
pub mod reflect;

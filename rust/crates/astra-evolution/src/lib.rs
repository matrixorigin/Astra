//! Multi-axis self-evolution engine for Astra.
//!
//! Core types, storage, fast-path proposal generation, and signal collection
//! for the evolution system. Higher-level orchestration (promotion gates,
//! service loop) remains in the runtime crate.

pub mod evolver;
pub mod signal_collector;
pub mod store;
pub mod types;

// Re-export key types at crate root
pub use evolver::{generate_fast_proposals, needs_llm};
pub use signal_collector::SignalCollector;
pub use store::EvolutionStore;
pub use types::*;

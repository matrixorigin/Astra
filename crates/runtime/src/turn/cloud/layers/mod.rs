//! Compression pipeline layers.
//!
//! Each layer implements [`CompressionLayer`] and is composed by
//! [`super::compaction_engine::CompactionEngine`] into an ordered pipeline.

pub mod duplicate_read_elimination;
pub mod reactive_compact;
pub mod tiered_compaction;
pub mod tool_result_truncation;

pub use duplicate_read_elimination::DuplicateReadElimination;
pub use reactive_compact::ReactiveCompact;
pub use tiered_compaction::TieredCompaction;
pub use tool_result_truncation::ToolResultTruncation;

/// Shared boilerplate for [`CompressionLayer`] trait methods.
///
/// Every layer duplicates `name()`, `method()`, and `trigger_pressure()`.
/// This macro centralizes them so new layers only define `compress()`.
macro_rules! compression_layer_boilerplate {
    ($name:literal, $method:ident) => {
        fn name(&self) -> &str {
            $name
        }
        fn method(&self) -> astra_turn_core::context_assembly_trace::CompressionMethod {
            astra_turn_core::context_assembly_trace::CompressionMethod::$method
        }
        fn trigger_pressure(&self) -> f64 {
            self.trigger
        }
    };
}
pub(crate) use compression_layer_boilerplate;

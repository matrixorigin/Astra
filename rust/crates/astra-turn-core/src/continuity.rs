//! Re-export of runtime-owned continuity types.
//!
//! Implementation lives in `astra-turn-types`; this module exists only as a
//! backwards-compatible re-export for callers that still import
//! `astra_turn_core::continuity::*`.

pub use astra_turn_types::continuity::*;

//! Cloud history compaction with one canonical message representation.
//!
//! `compaction` owns mechanical edits and protection rules. `CompactionEngine`
//! selects the fixed progressive pre-turn/retry schedule; request assembly and
//! Memoria retain their separate serialized-budget and memory/summary policy.
//! Required controls, provider message shape and artifact identities survive
//! both paths. Model summaries and durable artifact commits keep their owners.

pub mod compaction;
pub mod compaction_engine;
pub mod memoria_compact;
pub mod session_end_governance;

pub use compaction_engine::CompactionEngine;

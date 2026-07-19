//! Background session-memory (`session-memory.md`) extraction.
//!
//! Fires from [`crate::turn::agentic_loop::finalization::finalize_and_render`]
//! as fire-and-forget background work. See [`service::MemoryExtractionService`]
//! for the coordinator and [`runner::run_extraction`] for the worker
//! body.
//!
//! Observability: every attempt emits exactly one
//! [`astra_services::session_journal::JournalEvent::session_memory_extraction`]
//! event routed through the existing `IngestionSender` →
//! `agent_events` pipeline. UX: [`activity::BackgroundActivityBroker`]
//! fans out lifecycle signals the CLI bridges into `StreamEvent`.

pub mod activity;
pub mod gate;
pub mod health;
pub mod request;
pub mod runner;
pub mod service;

pub use activity::{BackgroundActivity, BackgroundActivityBroker};
pub use request::{ExtractionRequest, SpawnDecision};
pub use service::{
    ConstMemoryInferenceResolver, EXTRACTION_MAX_OUTPUT_TOKENS, LLM_TIMEOUT,
    MemoryExtractionService, MemoryInferenceResolver,
};

//! Stub session-memory extraction runner.
//!
//! The L1 session-memory protocol has been retired (wip-3). This module is
//! retained as a shim so call sites continue to compile while the surrounding
//! subsystem is removed; every extraction attempt simply reports `PersistFailed`
//! so callers do not record phantom writes.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use astra_services::session_journal::{
    SessionMemoryExtractionErrorReason, SessionMemoryExtractionSource,
};

use crate::memory_relevance::LlmConnParams;
use crate::turn::cloud::memoria_compact::MemoriaClient;

/// What the worker produced. Always `PersistFailed` after wip-3.
pub enum ExtractionArtifacts {
    Persisted {
        source: SessionMemoryExtractionSource,
        bytes_written: u64,
        store_attempt: u32,
        content: String,
    },
    LlmFailedPersistedFallback {
        error_reason: SessionMemoryExtractionErrorReason,
        bytes_written: u64,
        store_attempt: u32,
        content: String,
    },
    PersistFailed {
        error_reason: SessionMemoryExtractionErrorReason,
    },
}

/// No-op extraction: the L1 protocol is gone, so this never persists anything.
#[allow(clippy::too_many_arguments)]
pub async fn run_extraction(
    _memoria: &Arc<dyn MemoriaClient>,
    _session_id: &str,
    _messages: &[Value],
    _turn_number: usize,
    _current_tokens: usize,
    _current_memory: &str,
    _selector_params: Option<&LlmConnParams>,
    _llm_timeout: Duration,
    _max_output_tokens: usize,
) -> ExtractionArtifacts {
    ExtractionArtifacts::PersistFailed {
        error_reason: SessionMemoryExtractionErrorReason::WriteFailed,
    }
}

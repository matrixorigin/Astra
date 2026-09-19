//! Exact prompt bytes admitted for standalone and inline conversation summaries.

use serde::{Deserialize, Serialize};

/// Covers history rendering and structured-summary validation semantics.
pub const SUMMARY_PROMPT_RENDERER_VERSION: u32 = 1;

/// Persisted inputs, resolved by the caller before summary execution.
/// No field has a default: a missing template must not silently select current code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SummaryPromptTemplates {
    pub renderer_version: u32,
    pub standalone_system: String,
    pub standalone_user_prefix: String,
    pub standalone_user_suffix: String,
    pub inline_instruction: String,
}

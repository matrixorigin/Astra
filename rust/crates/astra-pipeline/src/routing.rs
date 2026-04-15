//! Unified Routing Decision — task types and domain hints.
//!
//! Core types for routing analysis:
//! - `TaskType` — 8 task classifications
//! - `DomainHint` — 7 domain categories
//! - `ToolFilter` — tool selection strategy

use serde::{Deserialize, Serialize};

// ─── Task Type ───────────────────────────────────────────────────────────────

/// Enriched task classification — 8 types for precise routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TaskType {
    /// Code editing, generation, or file manipulation.
    Code,
    /// Analysis, explanation, comparison.
    Reasoning,
    /// Read-only data retrieval (list PRs, show status, etc.)
    Fetch,
    /// Create/update/delete operations.
    Mutate,
    /// Store or retrieve user preferences (关注/跟踪/bookmark).
    Memory,
    /// Greeting, chit-chat, simple questions.
    Conversational,
    /// Multiple task types combined (e.g., "show me PRs and fix the failing one").
    Compound,
    /// Cannot determine task type.
    Unknown,
}

impl Default for TaskType {
    fn default() -> Self {
        Self::Unknown
    }
}

// ─── Domain Hint ─────────────────────────────────────────────────────────────

/// Domain extracted from signals + memory hints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DomainHint {
    GitHub,
    Git,
    Code,
    Memory,
    Web,
    System,
    Database,
}

/// JSON / journal label for [`DomainHint`].
#[must_use]
pub fn domain_hint_to_label(d: DomainHint) -> &'static str {
    match d {
        DomainHint::GitHub => "github",
        DomainHint::Git => "git",
        DomainHint::Code => "code",
        DomainHint::Memory => "memory",
        DomainHint::Web => "web",
        DomainHint::System => "system",
        DomainHint::Database => "database",
    }
}

// ─── Tool Filter ─────────────────────────────────────────────────────────────

/// Recommended tool selection strategy based on routing analysis.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolFilter {
    /// Low confidence → include all tools, let LLM decide.
    Wide,
    /// Domain-focused → filter to specific tool categories.
    Domain(Vec<String>),
    /// Conversational → minimal tools (only pinned).
    Minimal,
}

// ─── Calibration Axis ────────────────────────────────────────────────────────

/// Which axis a calibration adjustment targets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CalibrationAxis {
    Intent(String),
    Domain(DomainHint),
    Task(TaskType),
}

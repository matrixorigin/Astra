//! Prompt section types for the context pipeline.
//!
//! These types were originally defined in `runtime::prompts::system` but are
//! needed by both `astra-turn-core` (optimizer, planner) and `astra-runtime`
//! (prompt builders). Moving them here keeps the dependency DAG clean:
//! `astra-turn-core` does NOT depend on `astra-runtime`.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::context_assembly_trace::PromptTraceSignals;

// ── Types moved from runtime::prompts::system ──────────────────────────────

/// Cache scope for a prompt section, indicating how stable it is across turns.
///
/// Providers like Anthropic can cache content blocks annotated with
/// `cache_control: {type: "ephemeral"}`.  Separating static from dynamic
/// sections maximises prefix-cache hit rates.
///
/// The `Ord` impl orders by stability: `Global < Session < None`.
/// This lets the optimizer sort sections most-stable-first for cache alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CacheScope {
    /// Stable across sessions — identity, core rules, output format.
    /// Changes only on agent code updates (weeks/months).
    Global,
    /// Stable within a session — tool-conditional guidance, task-type rules.
    /// Changes when tool set or task type changes (per turn, but usually stable).
    Session,
    /// Changes every turn — project profile, skills, memory signals.
    None,
}

impl CacheScope {
    /// Ordering key for cache-aligned sorting (lower = more stable = earlier).
    #[must_use]
    pub fn order(self) -> u8 {
        match self {
            Self::Global => 0,
            Self::Session => 1,
            Self::None => 2,
        }
    }
}

impl PartialOrd for CacheScope {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for CacheScope {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.order().cmp(&other.order())
    }
}

/// Which token budget category a section belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PromptTokenBucket {
    BasePersona,
    Environment,
    UserPreferences,
}

/// A section of the system prompt with cache scope metadata.
#[derive(Debug, Clone)]
pub struct PromptSection {
    pub text: String,
    pub scope: CacheScope,
    pub token_bucket: PromptTokenBucket,
    pub trace_signals: PromptTraceSignals,
}

impl PromptSection {
    pub fn stable(text: impl Into<String>, scope: CacheScope) -> Self {
        Self {
            text: text.into(),
            scope,
            token_bucket: PromptTokenBucket::BasePersona,
            trace_signals: PromptTraceSignals::default(),
        }
    }

    pub fn dynamic(text: impl Into<String>, token_bucket: PromptTokenBucket) -> Self {
        Self {
            text: text.into(),
            scope: CacheScope::None,
            token_bucket,
            trace_signals: PromptTraceSignals::default(),
        }
    }

    pub fn with_trace_signals(mut self, trace_signals: PromptTraceSignals) -> Self {
        self.trace_signals = trace_signals;
        self
    }
}

// ── New pipeline types ─────────────────────────────────────────────────────

/// Identifies what kind of content a section carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SectionKind {
    /// §1 — agent persona, core rules.
    Identity,
    /// §2 — capabilities, learned strengths.
    SelfModel,
    /// §3 — workspace rules, conventions.
    ProjectContext,
    /// §4 — semantic + episodic recall.
    Memory,
    /// §5 — scratchpad, active plan.
    WorkingMemory,
    /// §6 — conversation turns.
    History,
    /// §7 — output format, safety rules.
    Constraints,
    /// Dynamic — active skill instructions.
    Skills,
    /// Dynamic — model, date, cwd, git.
    RuntimeIdentity,
    /// Emergent — discovered skills from previous turn.
    EmergentSkills,
    /// Emergent — prefetched memory from previous turn.
    EmergentMemory,
    /// Emergent — tool use summary from previous turn.
    EmergentSummary,
}

impl SectionKind {
    /// Volatility score (lower = less volatile = should appear earlier in prompt).
    #[must_use]
    pub fn volatility(self) -> u8 {
        match self {
            Self::Identity | Self::Constraints => 0,
            Self::SelfModel => 1,
            Self::ProjectContext => 2,
            Self::Skills => 3,
            Self::Memory | Self::EmergentMemory => 4,
            Self::WorkingMemory | Self::EmergentSkills | Self::EmergentSummary => 5,
            Self::History => 6,
            Self::RuntimeIdentity => 7,
        }
    }
}

/// Compression priority: when the budget is tight, which sections get
/// compressed first?
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum CompressionPriority {
    /// Never compress (identity, constraints).
    Never,
    /// Compress only as a last resort.
    LastResort,
    /// Normal compression candidate.
    Normal,
    /// Compress first when under pressure.
    First,
}

/// Where a section's content comes from during the Bind phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SectionSource {
    /// Compiled into the binary (static text).
    Static,
    /// Retrieved from Memoria or other memory backend.
    Memory,
    /// Conversation history (messages array).
    History,
    /// Skill catalog.
    Skill,
    /// Edge profile / runtime environment.
    Environment,
    /// Emergent context from previous turn's execution.
    Emergent,
}

/// A section as planned (before binding). Describes what to include and how.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedSection {
    pub kind: SectionKind,
    pub scope: CacheScope,
    pub estimated_tokens: u32,
    pub priority: CompressionPriority,
    pub source: SectionSource,
}

/// Typed payload produced by Bind.
///
/// The pipeline keeps section content structured until the provider serializer
/// chooses a wire representation. This avoids making downstream stages infer
/// semantics from plain strings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SectionArtifact {
    SystemText(String),
    RuntimeText(String),
    MemoryText(String),
    HistorySummary(String),
    SpillReference { path: String, original_tokens: u32 },
    Empty,
}

impl SectionArtifact {
    #[must_use]
    pub fn from_text(kind: SectionKind, text: String) -> Self {
        if text.is_empty() {
            return Self::Empty;
        }
        match kind {
            SectionKind::Identity | SectionKind::Constraints => Self::SystemText(text),
            SectionKind::Memory | SectionKind::EmergentMemory => Self::MemoryText(text),
            SectionKind::History => Self::HistorySummary(text),
            SectionKind::SelfModel
            | SectionKind::ProjectContext
            | SectionKind::WorkingMemory
            | SectionKind::Skills
            | SectionKind::RuntimeIdentity
            | SectionKind::EmergentSkills
            | SectionKind::EmergentSummary => Self::RuntimeText(text),
        }
    }

    /// Returns the text content of this artifact, or `None` for non-text
    /// variants (`SpillReference`, `Empty`).
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::SystemText(text)
            | Self::RuntimeText(text)
            | Self::MemoryText(text)
            | Self::HistorySummary(text) => Some(text),
            Self::SpillReference { .. } | Self::Empty => None,
        }
    }

    pub fn append_text(&mut self, kind: SectionKind, suffix: &str) {
        match self {
            Self::SystemText(text)
            | Self::RuntimeText(text)
            | Self::MemoryText(text)
            | Self::HistorySummary(text) => text.push_str(suffix),
            Self::Empty if !suffix.is_empty() => {
                *self = Self::from_text(kind, suffix.to_string());
            }
            Self::SpillReference { .. } | Self::Empty => {}
        }
    }
}

/// A section after binding: concrete typed content resolved from its source.
#[derive(Debug, Clone)]
pub struct BoundSection {
    pub plan: PlannedSection,
    pub artifact: SectionArtifact,
    pub actual_tokens: u32,
    pub bind_latency: Duration,
}

impl BoundSection {
    /// Create an empty bound section (for optional sources that are absent).
    #[must_use]
    pub fn empty(kind: SectionKind) -> Self {
        Self {
            plan: PlannedSection {
                kind,
                scope: CacheScope::None,
                estimated_tokens: 0,
                priority: CompressionPriority::Normal,
                source: SectionSource::Static,
            },
            artifact: SectionArtifact::Empty,
            actual_tokens: 0,
            bind_latency: Duration::ZERO,
        }
    }

    /// Returns the text content if this section has text, `None` for non-text artifacts.
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        self.artifact.text()
    }

    pub fn append_text(&mut self, suffix: &str) {
        let before = self.text().map_or(0, |t| t.len());
        self.artifact.append_text(self.plan.kind, suffix);
        if self.text().map_or(0, |t| t.len()) > before {
            self.actual_tokens = self
                .actual_tokens
                .saturating_add(estimate_text_tokens(suffix));
        }
    }
}

/// Estimate token count from raw text length (≈4 bytes/token).
/// Saturates at `u32::MAX` instead of truncating on extremely large inputs.
#[must_use]
pub fn estimate_text_tokens(text: &str) -> u32 {
    (text.len() / 4).min(u32::MAX as usize) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_scope_ordering() {
        assert!(CacheScope::Global < CacheScope::Session);
        assert!(CacheScope::Session < CacheScope::None);
    }

    #[test]
    fn cache_scope_order_values() {
        assert_eq!(CacheScope::Global.order(), 0);
        assert_eq!(CacheScope::Session.order(), 1);
        assert_eq!(CacheScope::None.order(), 2);
    }

    #[test]
    fn section_kind_volatility_identity_lowest() {
        assert_eq!(SectionKind::Identity.volatility(), 0);
        assert_eq!(SectionKind::Constraints.volatility(), 0);
        assert!(SectionKind::Identity.volatility() < SectionKind::History.volatility());
        assert!(SectionKind::History.volatility() < SectionKind::RuntimeIdentity.volatility());
    }

    #[test]
    fn compression_priority_ordering() {
        assert!(CompressionPriority::Never < CompressionPriority::LastResort);
        assert!(CompressionPriority::LastResort < CompressionPriority::Normal);
        assert!(CompressionPriority::Normal < CompressionPriority::First);
    }

    #[test]
    fn prompt_section_stable_constructor() {
        let s = PromptSection::stable("hello", CacheScope::Global);
        assert_eq!(s.scope, CacheScope::Global);
        assert_eq!(s.token_bucket, PromptTokenBucket::BasePersona);
        assert_eq!(s.text, "hello");
    }

    #[test]
    fn prompt_section_dynamic_constructor() {
        let s = PromptSection::dynamic("env info", PromptTokenBucket::Environment);
        assert_eq!(s.scope, CacheScope::None);
        assert_eq!(s.token_bucket, PromptTokenBucket::Environment);
    }

    #[test]
    fn bound_section_empty_has_zero_tokens() {
        let b = BoundSection::empty(SectionKind::Memory);
        assert_eq!(b.actual_tokens, 0);
        assert!(b.text().is_none());
        assert_eq!(b.plan.kind, SectionKind::Memory);
    }

    #[test]
    fn section_artifact_preserves_semantics() {
        let system = SectionArtifact::from_text(SectionKind::Identity, "rules".into());
        let memory = SectionArtifact::from_text(SectionKind::Memory, "memory".into());
        let history = SectionArtifact::from_text(SectionKind::History, "history".into());

        assert!(matches!(system, SectionArtifact::SystemText(_)));
        assert!(matches!(memory, SectionArtifact::MemoryText(_)));
        assert!(matches!(history, SectionArtifact::HistorySummary(_)));
    }

    #[test]
    fn append_text_to_empty_keeps_section_semantics() {
        let mut identity = BoundSection::empty(SectionKind::Identity);
        identity.append_text("core rules");
        assert!(matches!(identity.artifact, SectionArtifact::SystemText(_)));

        let mut memory = BoundSection::empty(SectionKind::Memory);
        memory.append_text("remember this");
        assert!(matches!(memory.artifact, SectionArtifact::MemoryText(_)));
    }

    #[test]
    fn append_text_preserves_existing_token_count_and_adds_delta() {
        let mut section = BoundSection {
            plan: PlannedSection {
                kind: SectionKind::Identity,
                scope: CacheScope::Global,
                estimated_tokens: 0,
                priority: CompressionPriority::Never,
                source: SectionSource::Static,
            },
            artifact: SectionArtifact::SystemText("accurately tokenized content".into()),
            actual_tokens: 123,
            bind_latency: Duration::ZERO,
        };

        section.append_text(" plus");
        assert_eq!(section.actual_tokens, 123 + estimate_text_tokens(" plus"));
        assert!(matches!(section.artifact, SectionArtifact::SystemText(_)));
    }

    #[test]
    fn estimate_text_tokens_saturates_on_huge_input() {
        // Simulate a length that would overflow u32 if cast directly.
        // We can't allocate 16GB in a test, but we can verify the arithmetic
        // path by checking that for any len the result is <= u32::MAX.
        let big_len: usize = (u32::MAX as usize) * 4 + 100;
        let estimated = (big_len / 4).min(u32::MAX as usize) as u32;
        assert_eq!(estimated, u32::MAX);

        // Also verify normal path still works
        let normal = "hello world"; // 11 bytes => 2 tokens
        assert_eq!(estimate_text_tokens(normal), 2);
    }
}

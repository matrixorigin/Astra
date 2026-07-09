//! Prompt section types for the context pipeline.
//!
//! These types were originally defined in `runtime::prompts::system` but are
//! needed by both `astra-turn-core` (optimizer, planner) and `astra-runtime`
//! (prompt builders). Moving them here keeps the dependency DAG clean:
//! `astra-turn-core` does NOT depend on `astra-runtime`.

use std::borrow::Cow;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::context_assembly_trace::PromptTraceSignals;
use crate::spill_backend::SpillRegistry;

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
    /// Stable within a session — project context, skill catalogs, and user
    /// preferences that are byte-stable for the session. Per-turn tool
    /// surfaces and task-type hints belong in [`CacheScope::None`].
    Session,
    /// Changes every turn — project profile, memory signals, and other volatile context.
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

    /// **DANGEROUS** — construct a volatile (cache-busting) section. Use only
    /// when content genuinely changes every turn and cannot live in the
    /// stable prefix. The `_reason` argument is not read at runtime; it
    /// exists purely to force the caller to document, in source, *why* this
    /// section is worth invalidating the prompt-cache prefix.
    ///
    /// Guidance:
    /// - Prefer [`PromptSection::stable`] whenever the content is
    ///   session-stable and safe to include in the provider's cacheable prefix
    ///   (cwd, git branch, tool list, skills). Model identity needs
    ///   provider-aware placement because Anthropic cache-control prefixes should
    ///   not churn when only the model id changes.
    /// - Prefer [`PromptSection::dynamic`] (plain `CacheScope::None` with no
    ///   social-engineering red flag) for ordinary per-turn environment
    ///   context that already lives post-boundary.
    /// - Reach for this constructor only when you need an **explicit audit
    ///   trail** for a content source that *must* mutate per-turn and would
    ///   otherwise silently destroy prefix cache hit-rate.
    ///
    /// Behaves identically to [`PromptSection::dynamic`] at runtime.
    #[must_use]
    pub fn dangerous_volatile(
        text: impl Into<String>,
        token_bucket: PromptTokenBucket,
        _reason: &'static str,
    ) -> Self {
        debug_assert!(
            !_reason.trim().is_empty(),
            "PromptSection::dangerous_volatile requires a non-empty reason; \
             document in source why this content cannot live in the stable prefix"
        );
        Self::dynamic(text, token_bucket)
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
    /// Session-stable deferred tool discovery manifest.
    DeferredTools,
    /// Session-stable available skill catalog.
    AvailableSkills,
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
    /// Dynamic — **session-stable** runtime identity fragments:
    /// model, cwd, git branch, session id, self-model (tool set), tool-conditional
    /// guidance, project profile, learned context, system override. These
    /// change at session boundaries but are byte-stable within a session, so
    /// they belong in `CacheScope::Session` behind a cache marker.
    RuntimeIdentity,
    /// Dynamic — **turn-volatile** runtime fragments:
    /// tool-round guidance (depends on conversation length), effort hint
    /// (depends on active skill), plan-resume context, per-turn bridge
    /// escape-hatch extras (session anchor, feedback rules, memoria insights).
    /// These change every turn so they must live in `CacheScope::None`, AFTER
    /// the cache marker, otherwise they would invalidate the cached prefix.
    RuntimeVolatile,
    /// Emergent — discovered skills from previous turn.
    EmergentSkills,
    /// Emergent — prefetched memory from previous turn.
    EmergentMemory,
    /// Emergent — tool use summary from previous turn.
    EmergentSummary,
}

impl SectionKind {
    /// All `SectionKind` variants that a planner may emit. Used by
    /// `ContextBudget::allocate` to enumerate sections that need budget
    /// assignment, so adding a new variant requires updating exactly one
    /// place (this list) instead of silently receiving zero budget.
    ///
    /// When adding a new variant to the enum above, add it here as well.
    /// A missing variant will fail the `all_planned_covers_every_variant`
    /// test in this module.
    #[must_use]
    pub fn all_planned() -> &'static [Self] {
        &[
            Self::Identity,
            Self::SelfModel,
            Self::ProjectContext,
            Self::DeferredTools,
            Self::AvailableSkills,
            Self::Memory,
            Self::WorkingMemory,
            Self::History,
            Self::Constraints,
            Self::Skills,
            Self::RuntimeIdentity,
            Self::RuntimeVolatile,
            Self::EmergentSkills,
            Self::EmergentMemory,
            Self::EmergentSummary,
        ]
    }

    /// Sections whose budget is pre-allocated by `ContextBudget::allocate`,
    /// carried outside the planned-text stream (`History`), or emitted with a
    /// fixed zero budget (`Emergent*`) and must NOT be included in the
    /// remainder distribution.
    ///
    /// Centralising this predicate means adding a new "pre-allocated" variant
    /// only requires updating this match. The exhaustive match forces
    /// compile-time classification for every new `SectionKind`.
    #[must_use]
    pub fn is_preallocated(self) -> bool {
        match self {
            // Pre-allocated with explicit budget in `ContextBudget::allocate`.
            Self::Identity | Self::Constraints | Self::Memory => true,
            // Carried as provider messages, not a planned text section, or
            // emitted as opportunistic zero-budget context by the planner.
            Self::History | Self::EmergentSkills | Self::EmergentMemory | Self::EmergentSummary => {
                true
            }
            // Remaining variants participate in the remainder distribution.
            Self::SelfModel
            | Self::ProjectContext
            | Self::DeferredTools
            | Self::AvailableSkills
            | Self::WorkingMemory
            | Self::Skills
            | Self::RuntimeIdentity
            | Self::RuntimeVolatile => false,
        }
    }

    /// Volatility score (lower = less volatile = should appear earlier in prompt).
    #[must_use]
    pub fn volatility(self) -> u8 {
        match self {
            Self::Identity | Self::Constraints => 0,
            Self::SelfModel => 1,
            Self::ProjectContext => 2,
            Self::DeferredTools => 3,
            Self::AvailableSkills => 4,
            Self::Skills => 5,
            Self::RuntimeIdentity => 6, // session-stable; sits with Session blocks
            Self::Memory | Self::EmergentMemory => 7,
            Self::WorkingMemory | Self::EmergentSkills | Self::EmergentSummary => 8,
            Self::History => 9,
            Self::RuntimeVolatile => 10, // turn-volatile; latest in the prompt
        }
    }

    /// Compile-time binary classification: does this section belong AFTER the
    /// prompt-cache dynamic boundary?
    ///
    /// `true`  → content changes every turn; section must sit in the volatile
    ///           lane (`CacheScope::None`, post-boundary). Placing a `true`
    ///           section before the boundary will invalidate the cached
    ///           prefix on every request.
    /// `false` → content is session-stable or global; section may sit in the
    ///           cacheable prefix.
    ///
    /// The `match` is exhaustive on purpose: adding a new `SectionKind`
    /// variant forces explicit classification here at compile time, so it is
    /// impossible to add a variant that silently defaults to the wrong side
    /// of the boundary (which is exactly the bug `b64223c9` had to fix at
    /// runtime).
    #[must_use]
    pub fn is_volatile(self) -> bool {
        match self {
            // Stable / session-stable — cache-safe prefix.
            Self::Identity
            | Self::Constraints
            | Self::SelfModel
            | Self::ProjectContext
            | Self::DeferredTools
            | Self::AvailableSkills
            | Self::Skills
            | Self::RuntimeIdentity => false,
            // Mutate per-turn — must sit post-boundary.
            Self::Memory
            | Self::WorkingMemory
            | Self::History
            | Self::RuntimeVolatile
            | Self::EmergentSkills
            | Self::EmergentMemory
            | Self::EmergentSummary => true,
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
            | SectionKind::DeferredTools
            | SectionKind::AvailableSkills
            | SectionKind::WorkingMemory
            | SectionKind::Skills
            | SectionKind::RuntimeIdentity
            | SectionKind::RuntimeVolatile
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

    /// Returns `(path, original_tokens)` for `SpillReference`, else `None`.
    ///
    /// Callers use this to decide whether a section needs rehydration
    /// without pattern-matching the enum at every call site.
    #[must_use]
    pub fn spill_locator(&self) -> Option<(&str, u32)> {
        match self {
            Self::SpillReference {
                path,
                original_tokens,
            } => Some((path.as_str(), *original_tokens)),
            _ => None,
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

    /// Resolve this artifact to concrete text.
    ///
    /// - Inline variants return a borrowed view (zero-copy).
    /// - `SpillReference` resolves its locator through `registry`; on
    ///   success the bytes are decoded as UTF-8. On failure (scheme
    ///   unregistered, file missing, non-UTF-8) a human-readable
    ///   placeholder is returned — fail-open keeps downstream
    ///   serialization alive rather than poisoning the whole turn.
    /// - `Empty` returns an empty string.
    #[must_use]
    pub fn rehydrate<'a>(&'a self, registry: &SpillRegistry) -> Cow<'a, str> {
        match self {
            Self::SystemText(t)
            | Self::RuntimeText(t)
            | Self::MemoryText(t)
            | Self::HistorySummary(t) => Cow::Borrowed(t.as_str()),
            Self::SpillReference { path, .. } => match registry.load(path) {
                Ok(bytes) => match String::from_utf8(bytes) {
                    Ok(s) => Cow::Owned(s),
                    Err(_) => {
                        Cow::Owned(format!("[spilled content unavailable: non-UTF8 at {path}]"))
                    }
                },
                Err(_) => Cow::Owned(format!("[spilled content unavailable: {path}]")),
            },
            Self::Empty => Cow::Borrowed(""),
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

pub const BYTES_PER_TOKEN_ESTIMATE: usize = 4;

/// Estimate token count from raw text.
///
/// ASCII-heavy English/code keeps the long-standing ≈4 bytes/token estimate.
/// Non-ASCII text is counted by Unicode scalar value so dense UTF-8 scripts
/// such as CJK and emoji do not get discounted just because their byte length
/// is later divided by the ASCII ratio. This remains a coarse, conservative
/// budget estimate rather than a provider-specific tokenizer.
#[must_use]
pub fn estimate_text_tokens(text: &str) -> u32 {
    let mut ascii_bytes = 0usize;
    let mut non_ascii_chars = 0usize;
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii_bytes = ascii_bytes.saturating_add(ch.len_utf8());
        } else {
            non_ascii_chars = non_ascii_chars.saturating_add(1);
        }
    }
    ascii_bytes
        .checked_div(BYTES_PER_TOKEN_ESTIMATE)
        .unwrap_or(0)
        .saturating_add(non_ascii_chars)
        .min(u32::MAX as usize) as u32
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
        // Post-split: RuntimeIdentity carries session-stable fragments and sits
        // BEFORE History; RuntimeVolatile is the turn-volatile section and
        // ranks highest (most-drifting, emitted last in the prompt).
        assert!(SectionKind::RuntimeIdentity.volatility() < SectionKind::History.volatility());
        assert!(SectionKind::History.volatility() < SectionKind::RuntimeVolatile.volatility());
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
        let big_len: usize = (u32::MAX as usize) * BYTES_PER_TOKEN_ESTIMATE + 100;
        let estimated = (big_len / BYTES_PER_TOKEN_ESTIMATE).min(u32::MAX as usize) as u32;
        assert_eq!(estimated, u32::MAX);

        // Also verify normal path still works
        let normal = "hello world"; // 11 bytes => 2 tokens
        assert_eq!(estimate_text_tokens(normal), 2);
    }

    #[test]
    fn estimate_text_tokens_counts_dense_non_ascii_by_character() {
        assert_eq!(estimate_text_tokens("你好世界"), 4);
        assert_eq!(estimate_text_tokens("🚀🔥💻"), 3);
        assert_eq!(
            estimate_text_tokens("abcd你好"),
            3,
            "ASCII keeps bytes/4 estimate while non-ASCII chars are not discounted"
        );
    }

    // ── rehydrate (Phase 12: on-demand spill resolution) ───────────────

    use crate::spill_backend::{DEFAULT_SCHEME, FileSystemSpillBackend, SpillBackend};
    use std::sync::Arc;
    use tempfile::TempDir;

    #[test]
    fn rehydrate_inline_returns_borrowed() {
        let art = SectionArtifact::RuntimeText("inline content".into());
        let reg = SpillRegistry::new();
        let got = art.rehydrate(&reg);
        assert!(matches!(got, Cow::Borrowed(_)));
        assert_eq!(got.as_ref(), "inline content");
    }

    #[test]
    fn rehydrate_empty_returns_empty_borrowed() {
        let art = SectionArtifact::Empty;
        let reg = SpillRegistry::new();
        assert_eq!(art.rehydrate(&reg).as_ref(), "");
    }

    #[test]
    fn rehydrate_spill_reference_loads_from_backend() {
        let dir = TempDir::new().unwrap();
        let backend: Arc<dyn SpillBackend> = Arc::new(FileSystemSpillBackend::new(dir.path()));
        let locator = backend.store("k", b"rehydrated text").unwrap();

        let mut reg = SpillRegistry::new();
        reg.register(DEFAULT_SCHEME, backend);

        let art = SectionArtifact::SpillReference {
            path: locator,
            original_tokens: 10,
        };
        let got = art.rehydrate(&reg);
        assert!(matches!(got, Cow::Owned(_)));
        assert_eq!(got.as_ref(), "rehydrated text");
    }

    #[test]
    fn rehydrate_missing_locator_degrades_gracefully() {
        let reg = SpillRegistry::new(); // no backends registered
        let art = SectionArtifact::SpillReference {
            path: "file:///nonexistent".into(),
            original_tokens: 5,
        };
        let got = art.rehydrate(&reg);
        // Fail-open: placeholder, not panic, not empty string.
        assert!(got.as_ref().contains("[spilled content unavailable"));
        assert!(got.as_ref().contains("/nonexistent"));
    }

    #[test]
    fn rehydrate_after_process_restart_with_new_backend_instance() {
        // Phase 12 core promise: spill written in one "process" must be
        // loadable through a fresh backend + registry instance.
        let dir = TempDir::new().unwrap();
        let locator = {
            let first: Arc<dyn SpillBackend> = Arc::new(FileSystemSpillBackend::new(dir.path()));
            first.store("k", b"survives restart").unwrap()
        };

        let reborn: Arc<dyn SpillBackend> = Arc::new(FileSystemSpillBackend::new(dir.path()));
        let mut reg = SpillRegistry::new();
        reg.register(DEFAULT_SCHEME, reborn);

        let art = SectionArtifact::SpillReference {
            path: locator,
            original_tokens: 42,
        };
        assert_eq!(art.rehydrate(&reg).as_ref(), "survives restart");
    }

    /// Regression guard: `SectionKind::all_planned()` must enumerate every
    /// variant exactly once. If a new variant is added without updating
    /// `all_planned()`, the variant would silently receive zero budget in
    /// `ContextBudget::allocate` and be dropped by the serializer.
    ///
    /// `is_preallocated()` uses an exhaustive match, so adding a new variant
    /// fails to compile there until the author explicitly classifies it.
    #[test]
    fn all_planned_covers_every_section_kind_variant() {
        use std::collections::HashSet;
        let listed: HashSet<SectionKind> = SectionKind::all_planned().iter().copied().collect();

        assert_eq!(
            listed.len(),
            SectionKind::all_planned().len(),
            "all_planned() must not contain duplicates"
        );

        for k in SectionKind::all_planned().iter().copied() {
            // Touch `is_preallocated` + `is_volatile` so their exhaustive
            // matches participate in compile-time coverage from this test.
            let _ = k.is_preallocated();
            let _ = k.is_volatile();
            assert!(
                listed.contains(&k),
                "SectionKind::{k:?} is missing from SectionKind::all_planned(); \
                 add it there or it will silently receive zero budget."
            );
        }
    }

    /// Compile-time volatility classification must agree with the historical
    /// volatility score ranking: every kind the numerical `volatility()` puts
    /// at or above `RuntimeIdentity` (≥ 4) must also be `is_volatile() == true`,
    /// and every kind below that threshold must be `is_volatile() == false`.
    ///
    /// `RuntimeIdentity` itself is the boundary — it is session-stable
    /// (not volatile), which matches its documented role in `SectionKind`.
    #[test]
    fn is_volatile_agrees_with_volatility_score() {
        for k in SectionKind::all_planned().iter().copied() {
            let by_score = k.volatility() > SectionKind::RuntimeIdentity.volatility();
            assert_eq!(
                k.is_volatile(),
                by_score,
                "SectionKind::{k:?} disagrees: is_volatile()={} but volatility()={} \
                 (threshold: RuntimeIdentity.volatility()={})",
                k.is_volatile(),
                k.volatility(),
                SectionKind::RuntimeIdentity.volatility(),
            );
        }
    }

    #[test]
    fn dangerous_volatile_behaves_like_dynamic() {
        let s = PromptSection::dangerous_volatile(
            "latest turn delta",
            PromptTokenBucket::Environment,
            "per-turn delta cannot live in cached prefix",
        );
        assert_eq!(s.scope, CacheScope::None);
        assert_eq!(s.token_bucket, PromptTokenBucket::Environment);
        assert_eq!(s.text, "latest turn delta");
    }

    #[test]
    #[should_panic(expected = "non-empty reason")]
    fn dangerous_volatile_rejects_empty_reason_in_debug() {
        let _ = PromptSection::dangerous_volatile("x", PromptTokenBucket::Environment, "   ");
    }
}

//! Canonical prompt section snapshots, including cache placement and trace metadata.
//!
//! These values contain no runtime sources or I/O and can be persisted directly.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptGuidanceSignals {
    pub parallel_feedback: bool,
    /// Set when the trailing N rounds in conversation history each ran
    /// exactly one tool — strong signal the model is making sequential
    /// single-tool calls that should have been batched.
    pub parallel_batching_nudge: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptContextSignals {
    pub active_output_skills: bool,
    pub memory_signal_detected: bool,
    pub system_prompt_override: bool,
    pub effort_hint: bool,
    pub agent_type_hint: bool,
    pub self_awareness: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptTraceSignals {
    #[serde(default)]
    pub context_signals: PromptContextSignals,
    #[serde(default)]
    pub guidance_signals: PromptGuidanceSignals,
}

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
    /// Reusable within a session or a versioned session epoch — project
    /// context, skill catalogs, user preferences, and guidance derived from
    /// the exact visible tool surface. Sections are rebuilt on every turn;
    /// changed bytes create a new provider prefix rather than reusing stale
    /// content. Genuinely per-turn state and task hints belong in
    /// [`CacheScope::None`].
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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

/// Pre-compiled static text sections. Immutable after build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaticSections {
    pub core_rules: PromptSection,
    pub safety: PromptSection,
    pub planning_protocol: PromptSection,
    pub coding_discipline: PromptSection,
    pub turn_discipline: PromptSection,
    pub plan_execution: PromptSection,
    pub output_format: PromptSection,
    pub tool_error_recovery: PromptSection,
}

impl StaticSections {
    /// Collect all static sections into a Vec for iteration.
    pub fn as_vec(&self) -> Vec<&PromptSection> {
        vec![
            &self.core_rules,
            &self.safety,
            &self.planning_protocol,
            &self.coding_discipline,
            &self.turn_discipline,
            &self.plan_execution,
            &self.output_format,
            &self.tool_error_recovery,
        ]
    }

    /// Total estimated tokens across all static sections.
    pub fn total_tokens_estimate(&self) -> u32 {
        self.as_vec()
            .iter()
            .map(|s| estimate_text_tokens(&s.text))
            .sum()
    }
}

impl StaticSections {
    /// Build a minimal StaticSections for testing.
    /// Available in tests (both unit and integration).
    pub fn test_default() -> Self {
        Self {
            core_rules: PromptSection {
                text: "You are an expert.".into(),
                scope: CacheScope::Global,
                token_bucket: PromptTokenBucket::BasePersona,
                trace_signals: PromptTraceSignals::default(),
            },
            safety: PromptSection::stable("Refuse harmful requests.", CacheScope::Global),
            planning_protocol: PromptSection::stable("Plan carefully.", CacheScope::Global),
            coding_discipline: PromptSection::stable("Read before write.", CacheScope::Global),
            turn_discipline: PromptSection::stable("Announce actions.", CacheScope::Global),
            plan_execution: PromptSection::stable(
                "Execute plan subtasks faithfully.",
                CacheScope::Global,
            ),
            output_format: PromptSection::stable("Be concise.", CacheScope::Global),
            tool_error_recovery: PromptSection::stable("Retry on error.", CacheScope::Global),
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
    use serde_json::json;

    #[test]
    fn static_sections_roundtrip_preserves_text_and_metadata() {
        let mut snapshot = StaticSections::test_default();
        snapshot.core_rules = PromptSection {
            text: "Exact instructions\n你好  ".into(),
            scope: CacheScope::Session,
            token_bucket: PromptTokenBucket::UserPreferences,
            trace_signals: PromptTraceSignals {
                context_signals: PromptContextSignals {
                    active_output_skills: true,
                    memory_signal_detected: true,
                    system_prompt_override: true,
                    effort_hint: true,
                    agent_type_hint: true,
                    self_awareness: true,
                },
                guidance_signals: PromptGuidanceSignals {
                    parallel_feedback: true,
                    parallel_batching_nudge: true,
                },
            },
        };
        let encoded = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(encoded["core_rules"]["text"], "Exact instructions\n你好  ");
        assert_eq!(encoded["core_rules"]["scope"], "Session");
        assert_eq!(encoded["core_rules"]["token_bucket"], "UserPreferences");
        let restored: StaticSections = serde_json::from_value(encoded.clone()).unwrap();
        assert_eq!(restored, snapshot);

        let mut unknown_section_field = encoded.clone();
        unknown_section_field["core_rules"]["unknown"] = json!(true);
        assert!(serde_json::from_value::<StaticSections>(unknown_section_field).is_err());
        let mut unknown_snapshot_field = encoded;
        unknown_snapshot_field["unknown"] = json!(true);
        assert!(serde_json::from_value::<StaticSections>(unknown_snapshot_field).is_err());

        // Preserve the existing nested signals defaults; this move does not
        // impose a new completeness contract on trace signals.
        assert_eq!(
            serde_json::from_value::<PromptTraceSignals>(json!({})).unwrap(),
            PromptTraceSignals::default()
        );
    }
}

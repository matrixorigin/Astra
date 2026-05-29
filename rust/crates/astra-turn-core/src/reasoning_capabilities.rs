//! Reasoning replay capabilities — provider/model-aware policy derivation.
//!
//! Centralizes all provider-specific reasoning replay knowledge in one place,
//! replacing scattered `provider == "deepseek"` string checks across the codebase.
//!
//! # Core concept
//!
//! [`ReasoningReplayMode`] describes *how* a provider handles `reasoning_content`
//! fields when replaying assistant messages. This is a **capability** of the
//! provider/model combination, not a per-request decision.
//!
//! - [`ReasoningReplayMode::AlwaysReplay`] — the provider requires the field on
//!   every assistant message, even when reasoning was empty (DeepSeek, Moonshot/Kimi).
//! - [`ReasoningReplayMode::OnDemand`] — the field is only needed when thinking
//!   was explicitly enabled or the history already contains reasoning (standard
//!   OpenAI, Anthropic, etc.).
//!
//! # Adding a new provider
//!
//! 1. Add a match arm in [`reasoning_capabilities`].
//! 2. If the provider uses a non-empty placeholder for empty reasoning
//!    (e.g. Moonshot uses `" "`), set `reasoning_placeholder` accordingly.
//! 3. No other file needs to change — all callers go through this function.

/// How a provider/model handles `reasoning_content` in replay scenarios.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningReplayMode {
    /// Provider always needs `reasoning_content` on assistant messages when
    /// replaying history. Even empty reasoning must carry the field (possibly
    /// with a provider-specific placeholder).
    ///
    /// Examples: DeepSeek, Moonshot/Kimi.
    AlwaysReplay,

    /// Provider only needs `reasoning_content` when thinking was explicitly
    /// enabled for the request or the history already contains reasoning
    /// blocks. Non-reasoning turns can omit the field entirely.
    ///
    /// Examples: OpenAI (GPT-4o), Anthropic (Claude).
    OnDemand,
}

/// Provider/model-specific reasoning capabilities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReasoningCapabilities {
    /// Whether reasoning replay is always required or on-demand.
    pub replay_mode: ReasoningReplayMode,

    /// The placeholder string to use when an assistant message needs a
    /// `reasoning_content` field but produced no reasoning text.
    ///
    /// Most providers use `""` (empty string). Moonshot/Kimi uses `" "` (single
    /// space) to distinguish "no reasoning" from "field absent".
    pub reasoning_placeholder: &'static str,
}

impl ReasoningCapabilities {
    /// Whether reasoning replay is required for this provider/model.
    #[inline]
    pub fn requires_replay(&self) -> bool {
        self.replay_mode == ReasoningReplayMode::AlwaysReplay
    }
}

/// Query reasoning capabilities for a provider/model combination.
///
/// This is the **single source of truth** for provider-specific reasoning
/// replay behavior. All reasoning replay decisions flow through this function.
///
/// # Arguments
///
/// - `provider`: provider name (e.g. `"deepseek"`, `"moonshot"`, `"openai"`).
///   Case-insensitive. May be empty when only the model name is known.
/// - `model`: model identifier (e.g. `"deepseek-chat"`, `"kimi-k2.5"`).
///   Case-insensitive.
///
/// # Stability
///
/// The matching logic here is part of the wire protocol contract. Changing
/// a provider's mode may cause API errors on replay. Always test with real
/// provider responses after modifying this function.
pub fn reasoning_capabilities(provider: &str, model: &str) -> ReasoningCapabilities {
    let provider_lower = provider.to_ascii_lowercase();
    let model_lower = model.to_ascii_lowercase();

    // Moonshot / Kimi: always replay, uses " " as placeholder for empty reasoning.
    let is_moonshot = provider_lower == "moonshot"
        || model_lower.contains("moonshot")
        || model_lower.contains("kimi");
    if is_moonshot {
        return ReasoningCapabilities {
            replay_mode: ReasoningReplayMode::AlwaysReplay,
            reasoning_placeholder: " ",
        };
    }

    // DeepSeek: always replay, uses "" as placeholder.
    let is_deepseek = provider_lower.contains("deepseek") || model_lower.contains("deepseek");
    if is_deepseek {
        return ReasoningCapabilities {
            replay_mode: ReasoningReplayMode::AlwaysReplay,
            reasoning_placeholder: "",
        };
    }

    // Default: on-demand replay (OpenAI, Anthropic, Bedrock, etc.)
    ReasoningCapabilities {
        replay_mode: ReasoningReplayMode::OnDemand,
        reasoning_placeholder: "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── AlwaysReplay providers ────────────────────────────────────────────

    #[test]
    fn deepseek_always_replay_handles_variants() {
        // By provider name
        let caps = reasoning_capabilities("deepseek", "some-model");
        assert_eq!(caps.replay_mode, ReasoningReplayMode::AlwaysReplay);
        assert_eq!(caps.reasoning_placeholder, "");

        // By model name (when provider doesn't indicate DeepSeek)
        let caps = reasoning_capabilities("openai", "deepseek-v4-pro");
        assert_eq!(caps.replay_mode, ReasoningReplayMode::AlwaysReplay);

        // Case-insensitive
        assert_eq!(
            reasoning_capabilities("DeepSeek", "Chat").replay_mode,
            ReasoningReplayMode::AlwaysReplay
        );
    }

    #[test]
    fn moonshot_kimi_always_replay_handles_variants() {
        // By provider
        let caps = reasoning_capabilities("moonshot", "kimi-k2");
        assert_eq!(caps.replay_mode, ReasoningReplayMode::AlwaysReplay);
        assert_eq!(caps.reasoning_placeholder, " ");

        // By model name
        assert_eq!(
            reasoning_capabilities("other", "kimi-k2.5").replay_mode,
            ReasoningReplayMode::AlwaysReplay
        );
        assert_eq!(
            reasoning_capabilities("", "moonshot-v1-128k").replay_mode,
            ReasoningReplayMode::AlwaysReplay
        );

        // Case-insensitive
        assert_eq!(
            reasoning_capabilities("MOONSHOT", "KIMI-K2").replay_mode,
            ReasoningReplayMode::AlwaysReplay
        );
    }

    // ── OnDemand providers ───────────────────────────────────────────────

    #[test]
    fn standard_providers_are_on_demand() {
        for (provider, model) in [
            ("openai", "gpt-4o"),
            ("anthropic", "claude-sonnet-4-20250514"),
            ("some-new-provider", "model-x"),
            ("", ""),
        ] {
            let caps = reasoning_capabilities(provider, model);
            assert_eq!(
                caps.replay_mode,
                ReasoningReplayMode::OnDemand,
                "expected OnDemand for {provider}/{model}"
            );
            assert_eq!(caps.reasoning_placeholder, "");
        }
    }

    #[test]
    fn requires_replay_helper() {
        assert!(reasoning_capabilities("deepseek", "").requires_replay());
        assert!(reasoning_capabilities("moonshot", "kimi").requires_replay());
        assert!(!reasoning_capabilities("openai", "gpt-4o").requires_replay());
        assert!(!reasoning_capabilities("", "").requires_replay());
    }
}

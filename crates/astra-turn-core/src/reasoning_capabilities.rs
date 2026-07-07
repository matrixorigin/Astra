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
    // Both `moonshot` and `kimi` appear as canonical provider names in real
    // configs (Moonshot's vendor brand vs the model line they ship), so
    // accept either as a provider prefix.
    let is_moonshot = matches_provider_token(&provider_lower, "moonshot")
        || matches_provider_token(&provider_lower, "kimi")
        || model_id_contains_token(&model_lower, "moonshot")
        || model_id_contains_token(&model_lower, "kimi");
    if is_moonshot {
        return ReasoningCapabilities {
            replay_mode: ReasoningReplayMode::AlwaysReplay,
            reasoning_placeholder: " ",
        };
    }

    // DeepSeek: always replay, uses "" as placeholder.
    let is_deepseek = matches_provider_token(&provider_lower, "deepseek")
        || model_id_contains_token(&model_lower, "deepseek");
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

/// Whether the provider string is exactly `needle` or `<needle>-<variant>`.
///
/// Provider strings are short, vendor-controlled identifiers (`"deepseek"`,
/// `"deepseek-anthropic"`, `"moonshot"`, `"openai"`).  Plain `==` misses
/// variant suffixes (`"deepseek-v4-pro-anthropic"`); plain `contains` would
/// match `"openai-deepseek-shim"` against `"deepseek"`.  The shape is the
/// same as `model_id_contains_token` but anchored at the start because
/// variant suffixes are always *after* the canonical name.
///
/// Both arguments are expected to be already lowercased.
#[inline]
fn matches_provider_token(provider: &str, needle: &str) -> bool {
    if provider == needle {
        return true;
    }
    if let Some(rest) = provider.strip_prefix(needle) {
        return rest.chars().next().is_some_and(is_model_id_token_boundary);
    }
    false
}

/// Token-bounded `contains` for model identifiers.
///
/// Returns true when `needle` appears as a whole token inside `haystack` —
/// i.e. surrounded by either start/end of string or one of the model-id
/// separator characters (`-`, `_`, `.`, `/`, `:`, `@`). This rejects
/// false-positive substrings like `kimi-mock`, `deepseek-helper-shim`, or
/// `kimimaru-7b` that should not be classified as the real provider.
///
/// Both arguments are expected to be already lowercased; the helper does
/// not normalize case so callers can amortize the lowercasing.
fn model_id_contains_token(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let mut start = 0;
    while let Some(idx) = haystack[start..].find(needle) {
        let abs = start + idx;
        let end = abs + needle.len();
        let before_ok = abs == 0
            || haystack[..abs]
                .chars()
                .next_back()
                .is_some_and(is_model_id_token_boundary);
        let after_ok = end == haystack.len()
            || haystack[end..]
                .chars()
                .next()
                .is_some_and(is_model_id_token_boundary);
        if before_ok && after_ok {
            return true;
        }
        start = abs + 1;
    }
    false
}

#[inline]
fn is_model_id_token_boundary(ch: char) -> bool {
    matches!(ch, '-' | '_' | '.' | '/' | ':' | '@' | ' ')
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
    fn word_boundary_matching_rejects_accidental_substrings() {
        // Names that *contain* a provider keyword but only as a non-token
        // substring (no model-id separator delimits the keyword) must not
        // trigger AlwaysReplay. Without word-boundary matching, future model
        // catalogs adding e.g. `kimimaru-7b` or `deepseekers-fictional` would
        // silently inherit a reasoning-replay policy that breaks their wire.
        for (provider, model) in [
            ("openai", "kimimaru-7b"),
            ("openai", "deepseekers-fictional"),
            ("openai", "promoonshot-decoy"), // "moonshot" only as suffix of "promoonshot"
        ] {
            let caps = reasoning_capabilities(provider, model);
            assert_eq!(
                caps.replay_mode,
                ReasoningReplayMode::OnDemand,
                "expected OnDemand for {provider}/{model} (substring match must be word-bounded)",
            );
        }
    }

    #[test]
    fn provider_prefix_matching_is_symmetric_across_vendors() {
        // Vendor-routed provider strings often carry a variant suffix
        // (`deepseek-anthropic`, `moonshot-v2`, `kimi-direct`).  Both
        // DeepSeek and Moonshot must accept the same shape, otherwise
        // identical wire policy depends on which vendor's gateway you
        // happen to be talking to today.
        for provider in [
            "deepseek",
            "deepseek-anthropic",
            "deepseek-v4-pro-official",
            "moonshot",
            "moonshot-v2",
            "kimi",
            "kimi-direct",
        ] {
            let caps = reasoning_capabilities(provider, "");
            assert_eq!(
                caps.replay_mode,
                ReasoningReplayMode::AlwaysReplay,
                "provider {provider:?} (no model hint) must classify as AlwaysReplay",
            );
        }
    }

    #[test]
    fn provider_prefix_matching_rejects_collision_substrings() {
        // Strings that contain a vendor name as a non-prefix substring
        // must not match — the prefix shape protects against future
        // wrappers like `openai-deepseek-shim` accidentally inheriting
        // the wire policy.
        for provider in [
            "openai-deepseek-shim",
            "fake-moonshot",
            "kimimaru",
            "deepseekers",
        ] {
            let caps = reasoning_capabilities(provider, "");
            assert_eq!(
                caps.replay_mode,
                ReasoningReplayMode::OnDemand,
                "non-prefix substring {provider:?} must not match the vendor policy",
            );
        }
    }

    #[test]
    fn word_boundary_matching_accepts_real_variants() {
        // Real model names must still match — we are tightening, not breaking.
        for (provider, model) in [
            ("openai", "deepseek-v4-pro-official"),
            ("openai", "deepseek-v4-pro-anthropic"),
            ("openai", "kimi-k2"),
            ("openai", "kimi-k2.5"),
            ("openai", "moonshot-v1-128k"),
            ("openai", "kimi"),     // bare token
            ("openai", "deepseek"), // bare token
        ] {
            let caps = reasoning_capabilities(provider, model);
            assert_eq!(
                caps.replay_mode,
                ReasoningReplayMode::AlwaysReplay,
                "expected AlwaysReplay for real variant {provider}/{model}",
            );
        }
    }

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

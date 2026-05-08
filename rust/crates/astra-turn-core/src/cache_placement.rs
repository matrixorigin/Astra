//! Provider-aware placement of volatile prompt content.
//!
//! Astra's system prompt carries two flavors of content:
//!   - **Stable** — identity, tool list, core rules. Byte-identical round
//!     to round, suitable for caching.
//!   - **Volatile** — Self-Awareness (Turn: N | Tokens: M/K), session
//!     anchor, feedback rules, memoria insights. Changes every round.
//!
//! Anthropic-style providers isolate volatile content behind a
//! `cache_control` marker — the cached prefix ends at the marker so
//! post-marker churn is free. Prefix-only providers have no such
//! mechanism, so volatile bytes in the wrong place poison the whole
//! cache entry.
//!
//! Different providers have different prefix-cache semantics, and
//! getting this wrong is expensive — session 986a553e observed
//! MiniMax's tool-loop cache_read collapse from 7680 to 0 across six
//! rounds because the Self-Awareness block (carrying the live turn
//! counter) lived in a synthetic user-role preamble that re-rendered
//! every round.
//!
//! This module classifies providers along two orthogonal axes:
//!   1. **Protocol** — how the provider signals "end of cacheable
//!      prefix": explicit marker (Anthropic / Bedrock) vs implicit
//!      byte-prefix matching (OpenAI / MiniMax / others).
//!   2. **Volatile placement policy** — given the protocol, where in
//!      the request volatile content may safely live without breaking
//!      cache.
//!
//! The runtime calls [`CacheCapability::for_provider_and_model`] once
//! per round and threads the result through the volatile-placement
//! pipeline.

use serde::{Deserialize, Serialize};

/// How the provider signals "end of cacheable prefix."
///
/// This layer is *narrower* than [`crate::microcompact::PromptCacheProtocol`]
/// because it's asked a different question: not "does the provider
/// accept `cache_control`" but "how does it decide what to cache."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum CacheProtocol {
    /// Anthropic Messages API and compatible endpoints. Cache boundary
    /// is signaled by explicit `cache_control` marker(s). Content after
    /// the marker is not part of the cache key.
    MarkerExplicit,
    /// Bedrock Converse inline `cachePoint` blocks. Same boundary
    /// semantics as `MarkerExplicit` — separated because the wire
    /// encoding differs and some heuristics (e.g. 4-marker cap) are
    /// Anthropic-specific.
    BedrockCachePoint,
    /// OpenAI chat completions auto-prefix cache. Cache boundary
    /// inferred at the longest stable prefix; bytes after the first
    /// diverging position are uncached.
    OpenAiAutoPrefix,
    /// MiniMax observed semantics (session 986a553e, 2026-05-08).
    ///
    /// **Empirically verified** (2026-05-08) via a controlled API probe
    /// at `tests/fixtures/minimax_cache_probe.py`. Results against the
    /// live `api.minimaxi.com/v1` endpoint:
    ///
    /// | Scenario                  | r0  | r1  | r2  | r3  |
    /// | ------------------------- | --- | --- | --- | --- |
    /// | advancing preamble in u[1]| 576 | 0   | 0   | 0   |
    /// | frozen preamble in u[1]   | 443 | 443 | 0*  | 443 |
    ///
    /// Single-byte change at msg[1] **wipes the entire history cache**
    /// for every subsequent round of a tool loop. An unchanged u[1]
    /// keeps the cache warm through appended (assistant_tc, tool_result)
    /// pairs. This is not pure prefix caching — a prefix cache would
    /// still hit everything before the divergence point; MiniMax throws
    /// out the whole history.
    ///
    /// (The `r2=0` in the frozen case is sporadic eviction noise —
    /// MiniMax's auto-prefix cache isn't deterministic at low traffic —
    /// but the trend is unambiguous.)
    ///
    /// Other vendors with the same behavior land here.
    StrictHistoryMatch,
    /// Provider doesn't advertise prompt caching. Placement is
    /// irrelevant; content goes wherever is natural.
    #[default]
    None,
}

/// Where volatile content may live without breaking the provider's
/// prompt cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum VolatilePlacement {
    /// Marker-based providers: volatile content goes AFTER the last
    /// `cache_control` marker. Caller is responsible for marker
    /// placement; this module just asserts the invariant.
    MarkerIsolated,
    /// Auto-prefix providers (OpenAI chat completions): volatile
    /// content must follow the last stable prefix boundary. In
    /// practice: append to the end of the last user message's content
    /// so the cached prefix = everything before the final user turn.
    TailSuffix,
    /// Strict-history providers (MiniMax): any byte change mid-history
    /// destroys the full cache entry. **Volatile content is suppressed
    /// on EVERY round** — even round 0.
    ///
    /// The round-0-only variant was tried and rejected: prepending
    /// volatile to msg[1] on round 0 but not on round 1+ still
    /// produces different bytes at msg[1] across rounds (round 0's
    /// msg[1] = preamble + user_q; round 1's msg[1] = user_q only),
    /// and MiniMax's cache sees that as a total miss. The only way
    /// to keep history byte-stable for strict-history providers is
    /// to never inject volatile at all on this path. The agent
    /// loses Self-Awareness / session-anchor signals in exchange for
    /// usable cache — observed collapse was 100% of cache reads for
    /// six consecutive tool-loop rounds in session 986a553e.
    ///
    /// **Empirical confirmation** (2026-05-08): a controlled API probe
    /// at `tests/fixtures/minimax_cache_probe.py` compared "advancing
    /// preamble" vs "frozen preamble" across 4 rounds of a tool loop.
    /// Advancing: cache_read = 576, 0, 0, 0. Frozen: cache_read = 443,
    /// 443, 0*, 443. Suppression recovers ~75% of possible cache reads
    /// on a 4-round loop. Re-run the probe if you doubt this strategy;
    /// see the `StrictHistoryMatch` variant doc for the full table.
    CurrentUserOnly,
    /// No cache to break. Volatile content goes anywhere convenient —
    /// we pick "in system" for consistency with marker-based output.
    #[default]
    Free,
}

/// The combined classification the runtime consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CacheCapability {
    pub protocol: CacheProtocol,
    pub volatile_placement: VolatilePlacement,
}

impl CacheCapability {
    /// Resolve the capability for a given (provider, model) pair.
    ///
    /// Provider takes precedence over model so a Claude-named model
    /// served through an OpenAI-compatible proxy (e.g., some LiteLLM
    /// deployments) gets the prefix semantics, not the marker ones.
    #[must_use]
    pub fn for_provider_and_model(provider: &str, model: &str) -> Self {
        let provider = provider.trim().to_ascii_lowercase();
        let model_lower = model.trim().to_ascii_lowercase();
        match provider.as_str() {
            "anthropic" => Self {
                protocol: CacheProtocol::MarkerExplicit,
                volatile_placement: VolatilePlacement::MarkerIsolated,
            },
            "bedrock" => Self {
                protocol: CacheProtocol::BedrockCachePoint,
                volatile_placement: VolatilePlacement::MarkerIsolated,
            },
            // Vendor-specific: MiniMax is a known strict-history provider
            // (see session 986a553e regression). Detect via model-id
            // substring so e.g. `MiniMax-M2.7` or future `MiniMax-M3`
            // variants served under provider=openai still get the right
            // placement.
            "openai" if model_lower.contains("minimax") => Self {
                protocol: CacheProtocol::StrictHistoryMatch,
                volatile_placement: VolatilePlacement::CurrentUserOnly,
            },
            "openai" => Self {
                protocol: CacheProtocol::OpenAiAutoPrefix,
                volatile_placement: VolatilePlacement::TailSuffix,
            },
            // Unknown providers: conservative — no cache assumed.
            _ => Self {
                protocol: CacheProtocol::None,
                volatile_placement: VolatilePlacement::Free,
            },
        }
    }

    /// Shortcut used by call sites that only care whether volatile
    /// content should be injected on the current LLM round.
    ///
    /// `MarkerIsolated` / `TailSuffix` / `Free`: always true.
    /// `CurrentUserOnly`: always **false** — see the variant's doc
    /// for why round-0-only didn't work and we had to suppress
    /// volatile entirely for strict-history providers.
    #[must_use]
    pub fn should_inject_volatile_on_round(&self, _round_within_turn: u32) -> bool {
        match self.volatile_placement {
            VolatilePlacement::CurrentUserOnly => false,
            VolatilePlacement::MarkerIsolated
            | VolatilePlacement::TailSuffix
            | VolatilePlacement::Free => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_provider_gets_marker_isolated() {
        let c = CacheCapability::for_provider_and_model("anthropic", "claude-sonnet-4");
        assert_eq!(c.protocol, CacheProtocol::MarkerExplicit);
        assert_eq!(c.volatile_placement, VolatilePlacement::MarkerIsolated);
    }

    #[test]
    fn anthropic_provider_is_case_insensitive() {
        let c = CacheCapability::for_provider_and_model("Anthropic", "claude-sonnet-4");
        assert_eq!(c.volatile_placement, VolatilePlacement::MarkerIsolated);
    }

    #[test]
    fn bedrock_provider_gets_bedrock_cachepoint() {
        let c =
            CacheCapability::for_provider_and_model("bedrock", "us.anthropic.claude-sonnet-4-6");
        assert_eq!(c.protocol, CacheProtocol::BedrockCachePoint);
        assert_eq!(c.volatile_placement, VolatilePlacement::MarkerIsolated);
    }

    #[test]
    fn openai_provider_gets_tail_suffix() {
        let c = CacheCapability::for_provider_and_model("openai", "gpt-4o");
        assert_eq!(c.protocol, CacheProtocol::OpenAiAutoPrefix);
        assert_eq!(c.volatile_placement, VolatilePlacement::TailSuffix);
    }

    #[test]
    fn minimax_model_overrides_openai_provider_to_strict_history() {
        // MiniMax is served under provider=openai in astra's registry.
        // The model-id substring disambiguates.
        let c = CacheCapability::for_provider_and_model("openai", "MiniMax-M2.7");
        assert_eq!(c.protocol, CacheProtocol::StrictHistoryMatch);
        assert_eq!(c.volatile_placement, VolatilePlacement::CurrentUserOnly);
    }

    #[test]
    fn minimax_detected_case_insensitively() {
        let c = CacheCapability::for_provider_and_model("openai", "minimax-m3-preview");
        assert_eq!(c.volatile_placement, VolatilePlacement::CurrentUserOnly);
    }

    #[test]
    fn unknown_provider_defaults_to_none_and_free() {
        let c = CacheCapability::for_provider_and_model("some-new-vendor", "model-xyz");
        assert_eq!(c.protocol, CacheProtocol::None);
        assert_eq!(c.volatile_placement, VolatilePlacement::Free);
    }

    // ── should_inject_volatile_on_round ─────────────────────────────────

    #[test]
    fn current_user_only_never_injects_on_any_round() {
        // Strict-history providers: injecting on round 0 but not after
        // still makes msg[1] bytes differ across rounds (round 0's
        // msg[1] includes the preamble, round 1+ doesn't). MiniMax
        // sees that as a total cache miss. So CurrentUserOnly
        // suppresses volatile entirely.
        let minimax = CacheCapability {
            protocol: CacheProtocol::StrictHistoryMatch,
            volatile_placement: VolatilePlacement::CurrentUserOnly,
        };
        for round in 0..=10 {
            assert!(
                !minimax.should_inject_volatile_on_round(round),
                "CurrentUserOnly must skip round {round}",
            );
        }
    }

    #[test]
    fn marker_isolated_always_injects() {
        let anthropic = CacheCapability {
            protocol: CacheProtocol::MarkerExplicit,
            volatile_placement: VolatilePlacement::MarkerIsolated,
        };
        // Marker providers are safe every round — the marker isolates
        // volatile content from cache.
        for round in 0..=20 {
            assert!(anthropic.should_inject_volatile_on_round(round));
        }
    }

    #[test]
    fn tail_suffix_always_injects() {
        let openai = CacheCapability {
            protocol: CacheProtocol::OpenAiAutoPrefix,
            volatile_placement: VolatilePlacement::TailSuffix,
        };
        // Tail-suffix providers can safely re-append volatile every
        // round since the churn lives at the end. OpenAI's auto-prefix
        // cache will still match the stable prefix.
        for round in 0..=20 {
            assert!(openai.should_inject_volatile_on_round(round));
        }
    }

    #[test]
    fn free_placement_always_injects() {
        let unknown = CacheCapability::default();
        assert_eq!(unknown.volatile_placement, VolatilePlacement::Free);
        assert!(unknown.should_inject_volatile_on_round(0));
        assert!(unknown.should_inject_volatile_on_round(5));
    }

    // ── Regression fingerprints from real sessions ──────────────────────

    #[test]
    fn minimax_m27_session_986a553e_routes_to_current_user_only() {
        // Pin the exact model id observed in the regression session so a
        // future provider/model normalization change doesn't silently
        // route MiniMax back to TailSuffix and reopen the cache hole.
        // With CurrentUserOnly's total-suppression contract every round
        // — including round 0 — must be silent.
        let c = CacheCapability::for_provider_and_model("openai", "MiniMax-M2.7");
        assert_eq!(c.volatile_placement, VolatilePlacement::CurrentUserOnly);
        assert!(!c.should_inject_volatile_on_round(0));
        assert!(!c.should_inject_volatile_on_round(1));
        assert!(!c.should_inject_volatile_on_round(6));
    }
}

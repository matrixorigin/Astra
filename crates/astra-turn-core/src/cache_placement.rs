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
//! Different deployments have different prefix-cache semantics, and
//! getting this wrong is expensive — session 986a553e observed
//! MiniMax's tool-loop cache_read collapse from 7680 to 0 across six
//! rounds because the Self-Awareness block (carrying the live turn
//! counter) lived in a synthetic user-role preamble that re-rendered
//! every round.
//!
//! This module represents deployment capabilities along three orthogonal axes:
//!   1. **Protocol** — how the provider signals "end of cacheable
//!      prefix": explicit marker (Anthropic / Bedrock) vs implicit
//!      byte-prefix matching (OpenAI / MiniMax / others).
//!   2. **Volatile placement policy** — given the protocol, where in
//!      the request volatile content may safely live without breaking
//!      cache.
//!   3. **Volatile delivery policy** — whether optional, round-specific
//!      context should be sent at all. Required lifecycle authority is
//!      never hidden by this policy.
//!
//! The runtime calls [`CacheCapability::for_provider`] once
//! per round and threads the result through the volatile-placement
//! pipeline.

use serde::{Deserialize, Deserializer, Serialize};

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
    /// content must follow the last stable prefix boundary. Runtime-owned
    /// content keeps system authority and is inserted immediately before a
    /// current user/assistant tail, or after a complete trailing
    /// assistant/tool group. This preserves OpenAI tool-call pairing while
    /// letting later tool rounds reuse the accumulated conversation prefix
    /// without rewriting any conversation message.
    TailSuffix,
    /// Append required runtime authority as a provenance-tagged `user` frame
    /// and retain that frame in conversation order. This is an explicit
    /// deployment wire shape for providers which support prefix reuse for
    /// appended conversation messages but treat every `system` message as
    /// part of one global, cache-keyed system header.
    ///
    /// Optional volatile delivery remains controlled independently by
    /// [`VolatileDeliveryPolicy`]. The runtime provenance marker, rather than
    /// the physical provider role, keeps this frame out of human user intent.
    AppendOnlyUserTail,
    /// Put runtime context at the current-user boundary. Providers which
    /// reject mid-history system messages later consolidate required runtime
    /// authority into the leading system message. Whether optional volatile
    /// content is sent is controlled independently by
    /// [`VolatileDeliveryPolicy`].
    CurrentUserOnly,
    /// No cache to break. Volatile content goes anywhere convenient —
    /// we pick "in system" for consistency with marker-based output.
    #[default]
    Free,
}

/// Which runtime-owned volatile classes are projected onto the provider wire.
///
/// Delivery and placement are deliberately separate. A prefix-cached provider
/// may accept a required system tail while still benefiting from suppressing
/// duplicated active-turn/advisory snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum VolatileDeliveryPolicy {
    /// Send both optional and required runtime context.
    #[default]
    All,
    /// Suppress optional/advisory snapshots while retaining every required
    /// lifecycle or authority context. A byte-stable focus policy replaces
    /// the duplicated active-turn frame.
    RequiredOnly,
}

/// How far prompt-cache reuse survives for this deployment path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CacheReuseScope {
    /// Cache can survive across later user turns when the stable prefix matches.
    ConversationTurns,
    /// Cache reuse is only reliable across additional LLM rounds within the same turn.
    IntraTurnRounds,
}

/// The combined classification the runtime consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct CacheCapability {
    pub protocol: CacheProtocol,
    pub volatile_placement: VolatilePlacement,
    pub volatile_delivery: VolatileDeliveryPolicy,
    pub reuse_scope: Option<CacheReuseScope>,
}

/// Deserialize capabilities at the trace/wire boundary. An omitted delivery
/// axis retains the pre-axis behavior (`All`); placement never implies a
/// different delivery policy.
impl<'de> Deserialize<'de> for CacheCapability {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireCapability {
            protocol: CacheProtocol,
            volatile_placement: VolatilePlacement,
            #[serde(default)]
            volatile_delivery: Option<VolatileDeliveryPolicy>,
            #[serde(default)]
            reuse_scope: Option<CacheReuseScope>,
        }

        let wire = WireCapability::deserialize(deserializer)?;
        let volatile_delivery = wire
            .volatile_delivery
            .unwrap_or(VolatileDeliveryPolicy::All);
        if matches!(
            wire.volatile_placement,
            VolatilePlacement::AppendOnlyUserTail
        ) && (!matches!(volatile_delivery, VolatileDeliveryPolicy::RequiredOnly)
            || !matches!(wire.protocol, CacheProtocol::OpenAiAutoPrefix))
        {
            return Err(serde::de::Error::custom(
                "append_only_user_tail requires open_ai_auto_prefix with volatile_delivery=required_only",
            ));
        }
        Ok(Self {
            protocol: wire.protocol,
            volatile_placement: wire.volatile_placement,
            volatile_delivery,
            reuse_scope: wire.reuse_scope,
        })
    }
}

impl CacheCapability {
    /// Append-only history can extend a provider prefix only when every
    /// admitted volatile message is durable. Today only required authority is
    /// durable, so optional volatile delivery is an incoherent combination.
    #[must_use]
    pub fn is_valid(self) -> bool {
        !matches!(
            self.volatile_placement,
            VolatilePlacement::AppendOnlyUserTail
        ) || (matches!(self.protocol, CacheProtocol::OpenAiAutoPrefix)
            && matches!(self.volatile_delivery, VolatileDeliveryPolicy::RequiredOnly))
    }

    /// Resolve the transport-level default for a provider.
    ///
    /// Concrete deployments that differ from this baseline must declare an
    /// explicit capability in model metadata. Model names are intentionally
    /// absent: an alias cannot prove cache semantics, accepted role shapes, or
    /// reuse scope.
    #[must_use]
    pub fn for_provider(provider: &str) -> Self {
        let provider = provider.trim().to_ascii_lowercase();
        match provider.as_str() {
            "anthropic" => Self {
                protocol: CacheProtocol::MarkerExplicit,
                volatile_placement: VolatilePlacement::MarkerIsolated,
                volatile_delivery: VolatileDeliveryPolicy::All,
                reuse_scope: None,
            },
            // Bedrock multiplexes incompatible model families. The provider
            // name alone cannot prove cachePoint support, so the undeclared
            // baseline deliberately emits no cache markers. Claude-on-Bedrock
            // deployments declare `BedrockCachePoint` in model metadata.
            "bedrock" => Self {
                protocol: CacheProtocol::None,
                volatile_placement: VolatilePlacement::Free,
                volatile_delivery: VolatileDeliveryPolicy::All,
                reuse_scope: None,
            },
            // OpenAI-compatible transport defaults to prefix reuse and the
            // role shape accepted by that transport. A strict-history gateway
            // or an operator-selected required-only policy is an explicit
            // deployment capability, never inferred from the model id.
            "openai" => Self {
                protocol: CacheProtocol::OpenAiAutoPrefix,
                volatile_placement: VolatilePlacement::TailSuffix,
                volatile_delivery: VolatileDeliveryPolicy::All,
                reuse_scope: None,
            },
            // Unknown providers: conservative — no cache assumed.
            _ => Self {
                protocol: CacheProtocol::None,
                volatile_placement: VolatilePlacement::Free,
                volatile_delivery: VolatileDeliveryPolicy::All,
                reuse_scope: None,
            },
        }
    }

    /// Resolve an explicit deployment capability or the provider transport
    /// baseline. This never guesses behavior from a model name.
    #[must_use]
    pub fn from_explicit_or_provider(explicit: Option<Self>, provider: &str) -> Self {
        explicit.unwrap_or_else(|| Self::for_provider(provider))
    }

    #[must_use]
    pub fn prefers_intra_turn_batching(&self) -> bool {
        matches!(self.reuse_scope, Some(CacheReuseScope::IntraTurnRounds))
    }

    /// Shortcut used by call sites that only care whether volatile
    /// content should be injected on the current LLM round.
    ///
    /// Required authority contexts are handled separately by the wire
    /// assembler and are never suppressed by this decision.
    #[must_use]
    pub fn should_inject_volatile_on_round(&self, _round_within_turn: u32) -> bool {
        matches!(self.volatile_delivery, VolatileDeliveryPolicy::All)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_provider_gets_marker_isolated() {
        let c = CacheCapability::for_provider("anthropic");
        assert_eq!(c.protocol, CacheProtocol::MarkerExplicit);
        assert_eq!(c.volatile_placement, VolatilePlacement::MarkerIsolated);
    }

    #[test]
    fn anthropic_provider_is_case_insensitive() {
        let c = CacheCapability::for_provider("Anthropic");
        assert_eq!(c.volatile_placement, VolatilePlacement::MarkerIsolated);
    }

    #[test]
    fn undeclared_bedrock_is_conservative_regardless_of_model_alias() {
        let baseline = CacheCapability::for_provider("bedrock");
        assert_eq!(baseline.protocol, CacheProtocol::None);
        assert_eq!(baseline.volatile_placement, VolatilePlacement::Free);
    }

    #[test]
    fn explicit_capability_overrides_provider_baseline() {
        let explicit = CacheCapability {
            protocol: CacheProtocol::StrictHistoryMatch,
            volatile_placement: VolatilePlacement::CurrentUserOnly,
            volatile_delivery: VolatileDeliveryPolicy::RequiredOnly,
            reuse_scope: Some(CacheReuseScope::ConversationTurns),
        };

        let c = CacheCapability::from_explicit_or_provider(Some(explicit), "openai");

        assert_eq!(c, explicit);
    }

    #[test]
    fn missing_explicit_capability_preserves_openai_default() {
        let c = CacheCapability::from_explicit_or_provider(None, "openai");

        assert_eq!(c.protocol, CacheProtocol::OpenAiAutoPrefix);
        assert_eq!(c.volatile_placement, VolatilePlacement::TailSuffix);
    }

    #[test]
    fn openai_provider_gets_tail_suffix() {
        let c = CacheCapability::for_provider("openai");
        assert_eq!(c.protocol, CacheProtocol::OpenAiAutoPrefix);
        assert_eq!(c.volatile_placement, VolatilePlacement::TailSuffix);
    }

    #[test]
    fn openai_transport_baseline_is_prefix_tail_with_full_delivery() {
        let baseline = CacheCapability::for_provider("openai");
        assert_eq!(baseline.protocol, CacheProtocol::OpenAiAutoPrefix);
        assert_eq!(baseline.volatile_placement, VolatilePlacement::TailSuffix);
        assert_eq!(baseline.volatile_delivery, VolatileDeliveryPolicy::All);
    }

    #[test]
    fn omitted_delivery_retains_pre_axis_all_without_placement_inference() {
        let capability: CacheCapability = serde_json::from_value(serde_json::json!({
            "protocol": "StrictHistoryMatch",
            "volatile_placement": "CurrentUserOnly",
            "reuse_scope": "ConversationTurns",
        }))
        .unwrap();

        assert_eq!(capability.volatile_delivery, VolatileDeliveryPolicy::All);
    }

    #[test]
    fn legacy_non_strict_placement_deserializes_to_full_delivery_at_boundary() {
        let capability: CacheCapability = serde_json::from_value(serde_json::json!({
            "protocol": "OpenAiAutoPrefix",
            "volatile_placement": "TailSuffix",
        }))
        .unwrap();

        assert_eq!(capability.volatile_delivery, VolatileDeliveryPolicy::All);
    }

    #[test]
    fn explicit_delivery_is_preserved() {
        let capability: CacheCapability = serde_json::from_value(serde_json::json!({
            "protocol": "StrictHistoryMatch",
            "volatile_placement": "CurrentUserOnly",
            "volatile_delivery": "All",
        }))
        .unwrap();

        assert_eq!(capability.volatile_delivery, VolatileDeliveryPolicy::All);
    }

    #[test]
    fn append_only_requires_prefix_protocol_and_required_only_delivery() {
        for value in [
            serde_json::json!({
                "protocol": "OpenAiAutoPrefix",
                "volatile_placement": "AppendOnlyUserTail",
            }),
            serde_json::json!({
                "protocol": "OpenAiAutoPrefix",
                "volatile_placement": "AppendOnlyUserTail",
                "volatile_delivery": "All",
            }),
            serde_json::json!({
                "protocol": "MarkerExplicit",
                "volatile_placement": "AppendOnlyUserTail",
                "volatile_delivery": "RequiredOnly",
            }),
        ] {
            let error = serde_json::from_value::<CacheCapability>(value).unwrap_err();
            assert!(error.to_string().contains("append_only_user_tail"));
        }
    }

    #[test]
    fn unknown_provider_defaults_to_none_and_free() {
        let c = CacheCapability::for_provider("some-new-vendor");
        assert_eq!(c.protocol, CacheProtocol::None);
        assert_eq!(c.volatile_placement, VolatilePlacement::Free);
    }

    // ── should_inject_volatile_on_round ─────────────────────────────────

    #[test]
    fn required_only_never_injects_optional_volatile_on_any_round() {
        let strict = CacheCapability {
            protocol: CacheProtocol::StrictHistoryMatch,
            volatile_placement: VolatilePlacement::CurrentUserOnly,
            volatile_delivery: VolatileDeliveryPolicy::RequiredOnly,
            reuse_scope: None,
        };
        for round in 0..=10 {
            assert!(
                !strict.should_inject_volatile_on_round(round),
                "RequiredOnly must skip optional volatile on round {round}",
            );
        }
    }

    #[test]
    fn marker_isolated_always_injects() {
        let anthropic = CacheCapability {
            protocol: CacheProtocol::MarkerExplicit,
            volatile_placement: VolatilePlacement::MarkerIsolated,
            volatile_delivery: VolatileDeliveryPolicy::All,
            reuse_scope: None,
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
            volatile_delivery: VolatileDeliveryPolicy::All,
            reuse_scope: None,
        };
        // Tail-suffix providers can safely re-append volatile every
        // round since the churn lives at the end. OpenAI's auto-prefix
        // cache will still match the stable prefix.
        for round in 0..=20 {
            assert!(openai.should_inject_volatile_on_round(round));
        }
    }

    #[test]
    fn intra_turn_reuse_scope_prefers_batching() {
        let capability = CacheCapability {
            protocol: CacheProtocol::OpenAiAutoPrefix,
            volatile_placement: VolatilePlacement::TailSuffix,
            volatile_delivery: VolatileDeliveryPolicy::All,
            reuse_scope: Some(CacheReuseScope::IntraTurnRounds),
        };
        assert!(capability.prefers_intra_turn_batching());
    }

    #[test]
    fn free_placement_always_injects() {
        let unknown = CacheCapability::default();
        assert_eq!(unknown.volatile_placement, VolatilePlacement::Free);
        assert!(unknown.should_inject_volatile_on_round(0));
        assert!(unknown.should_inject_volatile_on_round(5));
    }
}

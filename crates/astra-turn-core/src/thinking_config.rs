//! Runtime adapters, selector parsing and UI policy for the shared thinking configuration.
//!
//! The serializable value and its pure operations live in `astra-turn-types`.

pub use astra_turn_types::{ThinkingConfig, ThinkingEffort, TurnComplexitySignals};
use serde_json::{Value, json};

/// Native wire controls for thinking on OpenAI-compatible endpoints.
///
/// This is endpoint protocol knowledge, not a model-name heuristic.  The
/// runtime uses it only after the route has been admitted, so an ordinary
/// OpenAI-compatible proxy never receives an extension field it did not
/// advertise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiThinkingControl {
    /// DashScope/Qwen-compatible `enable_thinking` flag.
    EnableThinkingFlag,
    /// DeepSeek V4-compatible `thinking: {type: disabled|enabled}` object.
    ThinkingObject,
    /// No known native suppression wire format.
    None,
}

/// Final OpenAI-chat emission shared with the model probe's wire contract.
/// This is deliberately independent of model names and endpoint heuristics.
pub fn apply_openai_protocol(
    thinking: &ThinkingConfig,
    body: &mut Value,
    protocol: astra_core::model_wire::thinking::ThinkingProtocol,
) {
    let effort = match thinking {
        ThinkingConfig::Adaptive { effort } => Some(effort.as_str()),
        _ => None,
    };
    protocol.apply(body, thinking.is_enabled(), effort);
}

/// Apply thinking suppression to an OpenAI-compatible request body.
///
/// For direct HTTP callers (memory extraction, relevance filtering) that
/// bypass the full LLM pipeline in `build_provider_request_body`.
/// Checks both `provider` string and `base_url` to detect DashScope,
/// since many configs use `provider: "openai"` with a DashScope base_url.
pub fn apply_openai_suppression(
    thinking: &ThinkingConfig,
    body: &mut Value,
    provider: &str,
    base_url: &str,
) {
    if !thinking.is_off() {
        return;
    }
    match openai_thinking_control(provider, base_url) {
        OpenAiThinkingControl::EnableThinkingFlag => {
            body["enable_thinking"] = json!(false);
        }
        OpenAiThinkingControl::ThinkingObject => {
            body["thinking"] = json!({"type": "disabled"});
        }
        OpenAiThinkingControl::None => {}
    }
}

/// Convert the effective turn thinking config into the fork-prefix metadata slice.
///
/// Native reasoning models can require replay-safe assistant reasoning fields even
/// when Astra did not surface an explicit `(thinking...)` selector suffix. We still
/// encode those captures as enabled so forked children preserve valid replay.
pub fn fork_capture_thinking_slice(
    thinking: &ThinkingConfig,
    provider: &str,
    model: &str,
) -> Option<crate::fork_prefix::ThinkingConfigSlice> {
    match thinking {
        ThinkingConfig::Off => {
            crate::reasoning_capabilities::reasoning_capabilities(provider, model)
                .requires_replay()
                .then(|| crate::fork_prefix::ThinkingConfigSlice {
                    enabled: true,
                    budget_tokens: 0,
                    kind: "native".to_string(),
                })
        }
        ThinkingConfig::Enabled { budget_tokens } => {
            Some(crate::fork_prefix::ThinkingConfigSlice {
                enabled: true,
                budget_tokens: *budget_tokens,
                kind: "enabled".to_string(),
            })
        }
        ThinkingConfig::Adaptive { effort } => Some(crate::fork_prefix::ThinkingConfigSlice {
            enabled: true,
            budget_tokens: 0,
            // Effort level participates in cache identity: a `low` parent
            // and a `max` parent send different `output_config.effort` on
            // the wire, so reusing one's cache for the other corrupts the
            // child's first-round response. Encode it into `kind` so
            // `ForkPrefix::identity_hash` separates the buckets.
            kind: format!("adaptive:{}", effort.as_str()),
        }),
    }
}

// ─── Model-based thinking inference ─────────────────────────────────────────

/// Encode a ThinkingConfig as a model name suffix for storage in state.model.
pub fn thinking_suffix_for(config: &ThinkingConfig) -> String {
    match config {
        ThinkingConfig::Off => String::new(),
        ThinkingConfig::Enabled { budget_tokens } => {
            format!("(thinking:budget:{})", budget_tokens)
        }
        ThinkingConfig::Adaptive {
            effort: ThinkingEffort::Low,
        } => "(thinking:low)".to_string(),
        ThinkingConfig::Adaptive {
            effort: ThinkingEffort::Medium,
        } => "(thinking:medium)".to_string(),
        ThinkingConfig::Adaptive {
            effort: ThinkingEffort::High,
        } => "(thinking:high)".to_string(),
        ThinkingConfig::Adaptive {
            effort: ThinkingEffort::Max,
        } => "(thinking:max)".to_string(),
    }
}

/// Parse an effort token (`low` | `medium` | `high` | `max`) into `ThinkingEffort`.
fn parse_effort_token(token: &str) -> Option<ThinkingEffort> {
    match token {
        "low" => Some(ThinkingEffort::Low),
        "medium" => Some(ThinkingEffort::Medium),
        "high" => Some(ThinkingEffort::High),
        "max" => Some(ThinkingEffort::Max),
        _ => None,
    }
}

/// Parse a model selector string. If it ends with a thinking suffix, strip it
/// and return the real model name + corresponding ThinkingConfig.
/// Otherwise return the original name + Off.
///
/// Recognized suffixes (longest/most-specific first):
///   - `(thinking:budget:N)` → `Enabled { budget_tokens: N }`
///   - `(thinking:low|medium|high|max)` → `Adaptive { effort }`
///   - `(thinking)` → `Adaptive { effort: High }` (shorthand)
pub fn resolve_model_thinking(model_selector: &str) -> (&str, ThinkingConfig) {
    // Fast path: no trailing ')' → no suffix possible.
    if !model_selector.ends_with(')') {
        return (model_selector, ThinkingConfig::Off);
    }

    // Find the opening '(' for the trailing group and extract its payload.
    let Some(open) = model_selector.rfind('(') else {
        return (model_selector, ThinkingConfig::Off);
    };
    let inner = &model_selector[open + 1..model_selector.len() - 1];
    let base = model_selector[..open].trim_end();

    // "thinking:budget:N" — Enabled with explicit budget.
    if let Some(n_str) = inner.strip_prefix("thinking:budget:") {
        if let Ok(budget_tokens) = n_str.parse::<u32>() {
            return (base, ThinkingConfig::Enabled { budget_tokens });
        }
        return (model_selector, ThinkingConfig::Off);
    }

    // "thinking:<effort>" — Adaptive with explicit effort.
    if let Some(effort_token) = inner.strip_prefix("thinking:") {
        if let Some(effort) = parse_effort_token(effort_token) {
            return (base, ThinkingConfig::Adaptive { effort });
        }
        return (model_selector, ThinkingConfig::Off);
    }

    // Bare "thinking" — shorthand for Adaptive{High}.
    if inner == "thinking" {
        return (
            base,
            ThinkingConfig::Adaptive {
                effort: ThinkingEffort::High,
            },
        );
    }

    (model_selector, ThinkingConfig::Off)
}

// ─── Two-level /model selection ─────────────────────────────────────────────

/// A selectable thinking option shown in the /model second-level prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThinkingOption {
    /// Display label (e.g., "Normal", "Thinking (High)", "Thinking (Max)")
    pub label: &'static str,
    /// The ThinkingConfig to use when this option is selected.
    pub config: ThinkingConfig,
    /// Whether this is the default selection (shown with ← marker).
    pub is_default: bool,
}

/// Returns thinking options for a model based on its probed `thinking_capability`.
///
/// - `"both"` → Normal / Thinking picker (provider-appropriate format).
/// - `"effort_only"` → Low / High / Max effort without Normal.
/// - `"native_only"` → no picker (always thinks, no control).
/// - `"none"` → no picker (doesn't think).
/// - `None` (unprobed) → no picker (safe default until probed).
pub fn thinking_options_with_capability(
    _model_name: &str,
    provider: Option<&str>,
    thinking_capability: Option<&str>,
) -> Vec<ThinkingOption> {
    match thinking_capability {
        Some("both") => {
            if provider_uses_budget_thinking(provider) {
                thinking_options_for_budget_thinking()
            } else {
                thinking_options_for_adaptive_reasoning()
            }
        }
        Some("effort_only") => thinking_options_for_effort_only(),
        None | Some("none") | Some("native_only") => vec![],
        Some(other) => {
            tracing::warn!(
                thinking_capability = %other,
                provider = ?provider,
                "unknown thinking_capability value — no picker",
            );
            vec![]
        }
    }
}

fn thinking_options_for_adaptive_reasoning() -> Vec<ThinkingOption> {
    vec![
        ThinkingOption {
            label: "Normal",
            config: ThinkingConfig::Off,
            is_default: false,
        },
        ThinkingOption {
            label: "Thinking (Low)",
            config: ThinkingConfig::Adaptive {
                effort: ThinkingEffort::Low,
            },
            is_default: false,
        },
        ThinkingOption {
            label: "Thinking (High)",
            config: ThinkingConfig::Adaptive {
                effort: ThinkingEffort::High,
            },
            is_default: true,
        },
        ThinkingOption {
            label: "Thinking (Max)",
            config: ThinkingConfig::Adaptive {
                effort: ThinkingEffort::Max,
            },
            is_default: false,
        },
    ]
}

fn thinking_options_for_effort_only() -> Vec<ThinkingOption> {
    vec![
        ThinkingOption {
            label: "Thinking (Low)",
            config: ThinkingConfig::Adaptive {
                effort: ThinkingEffort::Low,
            },
            is_default: false,
        },
        ThinkingOption {
            label: "Thinking (High)",
            config: ThinkingConfig::Adaptive {
                effort: ThinkingEffort::High,
            },
            is_default: true,
        },
        ThinkingOption {
            label: "Thinking (Max)",
            config: ThinkingConfig::Adaptive {
                effort: ThinkingEffort::Max,
            },
            is_default: false,
        },
    ]
}

fn thinking_options_for_budget_thinking() -> Vec<ThinkingOption> {
    vec![
        ThinkingOption {
            label: "Normal",
            config: ThinkingConfig::Off,
            is_default: false,
        },
        ThinkingOption {
            label: "Thinking",
            config: ThinkingConfig::Enabled {
                budget_tokens: 10_000,
            },
            is_default: true,
        },
    ]
}

fn provider_uses_budget_thinking(provider: Option<&str>) -> bool {
    provider.map(provider_may_think_natively).unwrap_or(false)
}

/// Returns `true` if the provider string alone identifies a DashScope endpoint.
pub fn provider_may_think_natively(provider: &str) -> bool {
    astra_core::model_wire::thinking::is_dashscope_provider(provider)
}

/// Returns `true` if the endpoint needs `enable_thinking` flag.
/// Checks both provider string and base_url since many configs use
/// `provider: "openai"` with a DashScope base_url.
pub fn needs_dashscope_thinking_flag(provider: &str, base_url: &str) -> bool {
    astra_core::model_wire::thinking::canonical_thinking_protocol(provider, base_url, "")
        == astra_core::model_wire::thinking::ThinkingProtocol::EnableThinking
}

/// Compatibility projection for callers without a concrete upstream model.
/// The shared adapter registry is the only endpoint-protocol owner.
pub fn openai_thinking_control(provider: &str, base_url: &str) -> OpenAiThinkingControl {
    use astra_core::model_wire::thinking::{ThinkingProtocol, canonical_thinking_protocol};
    match canonical_thinking_protocol(provider, base_url, "") {
        ThinkingProtocol::EnableThinking => OpenAiThinkingControl::EnableThinkingFlag,
        ThinkingProtocol::ThinkingObject => OpenAiThinkingControl::ThinkingObject,
        _ => OpenAiThinkingControl::None,
    }
}

/// Strip `<think>...</think>` XML tags from model output, returning only
/// the non-reasoning content.
///
/// Safety net for native-thinking models (DeepSeek, Qwen3) that may ignore
/// API-level thinking suppression and still emit reasoning blocks.
pub fn strip_think_tags(text: &str) -> String {
    if !text.contains("<think>") {
        return text.to_string();
    }
    let mut cleaned = String::with_capacity(text.len());
    let mut pos = 0;
    while let Some(start) = text[pos..].find("<think>") {
        let abs_start = pos + start;
        cleaned.push_str(&text[pos..abs_start]);
        if let Some(end) = text[abs_start..].find("</think>") {
            pos = abs_start + end + "</think>".len();
        } else {
            // Unclosed <think> — discard the rest as reasoning
            pos = text.len();
        }
    }
    cleaned.push_str(&text[pos..]);
    cleaned
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_thinking_config_drives_admitted_protocol_without_conversion() {
        use astra_core::model_wire::thinking::ThinkingProtocol;
        let thinking: astra_turn_types::ThinkingConfig =
            serde_json::from_value(json!({"mode": "adaptive", "effort": "low"})).unwrap();
        let mut body = json!({"messages": [], "reasoning_effort": "high"});
        apply_openai_protocol(&thinking, &mut body, ThinkingProtocol::ReasoningEffort);
        assert_eq!(body["reasoning_effort"], "low");
        assert_eq!(body["messages"], json!([]));

        apply_openai_protocol(
            &astra_turn_types::ThinkingConfig::Off,
            &mut body,
            ThinkingProtocol::ThinkingObject,
        );
        assert_eq!(body["thinking"], json!({"type": "disabled"}));
        assert!(body.get("reasoning_effort").is_none());
    }

    // ─── Model inference ────────────────────────────────────────────────

    #[test]
    fn both_dashscope_returns_budget() {
        let opts = thinking_options_with_capability("qwen-plus", Some("dashscope"), Some("both"));
        assert_eq!(opts.len(), 2);
        assert_eq!(opts[0].label, "Normal");
        assert_eq!(opts[1].label, "Thinking");
        assert!(matches!(
            opts[1].config,
            ThinkingConfig::Enabled {
                budget_tokens: 10_000
            }
        ));
    }

    #[test]
    fn effort_only_returns_low_high_and_max_without_normal() {
        let opts =
            thinking_options_with_capability("adaptive-model", Some("openai"), Some("effort_only"));
        assert_eq!(opts.len(), 3);
        assert_eq!(opts[0].label, "Thinking (Low)");
        assert_eq!(opts[1].label, "Thinking (High)");
        assert_eq!(opts[2].label, "Thinking (Max)");
        assert!(opts[1].is_default);
        assert_eq!(
            opts[2].config,
            ThinkingConfig::Adaptive {
                effort: ThinkingEffort::Max
            }
        );
    }

    #[test]
    fn native_only_returns_empty() {
        let opts =
            thinking_options_with_capability("MiniMax-M2.5", Some("openai"), Some("native_only"));
        assert!(opts.is_empty());
    }

    #[test]
    fn none_capability_returns_empty() {
        let opts = thinking_options_with_capability(
            "us.anthropic.claude-sonnet-4-6",
            Some("bedrock"),
            Some("none"),
        );
        assert!(opts.is_empty());
    }

    #[test]
    fn null_capability_returns_empty() {
        let opts = thinking_options_with_capability("qwen-plus", Some("openai"), None);
        assert!(
            opts.is_empty(),
            "unprobed model with no YAML hint should not show picker"
        );
    }

    #[test]
    fn both_bedrock_returns_adaptive() {
        let opts = thinking_options_with_capability(
            "us.anthropic.claude-sonnet-4-6",
            Some("bedrock"),
            Some("both"),
        );
        assert_eq!(opts.len(), 4);
        assert_eq!(opts[0].label, "Normal");
        assert_eq!(opts[1].label, "Thinking (Low)");
        assert_eq!(opts[2].label, "Thinking (High)");
        assert_eq!(opts[3].label, "Thinking (Max)");
        assert!(opts[2].is_default);
        assert_eq!(thinking_suffix_for(&opts[3].config), "(thinking:max)");
    }

    #[test]
    fn both_anthropic_returns_adaptive() {
        let opts =
            thinking_options_with_capability("claude-sonnet-4", Some("anthropic"), Some("both"));
        assert_eq!(opts.len(), 4);
        assert_eq!(opts[2].label, "Thinking (High)");
        assert_eq!(opts[3].label, "Thinking (Max)");
    }

    #[test]
    fn both_openai_returns_adaptive() {
        let opts = thinking_options_with_capability("gpt-5", Some("openai"), Some("both"));
        assert_eq!(opts.len(), 4);
        assert_eq!(opts[2].label, "Thinking (High)");
        assert_eq!(opts[3].label, "Thinking (Max)");
    }

    #[test]
    fn resolve_model_with_thinking_suffix() {
        let (name, cfg) = resolve_model_thinking("us.anthropic.claude-opus-4-6-v1(thinking)");
        assert_eq!(name, "us.anthropic.claude-opus-4-6-v1");
        assert_eq!(
            cfg,
            ThinkingConfig::Adaptive {
                effort: ThinkingEffort::High
            }
        );
    }

    #[test]
    fn resolve_model_with_budget_suffix() {
        let (name, cfg) = resolve_model_thinking("qwen-plus(thinking:budget:8000)");
        assert_eq!(name, "qwen-plus");
        assert_eq!(
            cfg,
            ThinkingConfig::Enabled {
                budget_tokens: 8_000
            }
        );

        // A bare "(thinking:budget)" is not a valid encoding — treated as no
        // recognized suffix, returning the input untouched as Off.
        let (name2, cfg2) = resolve_model_thinking("qwen-plus(thinking:budget)");
        assert_eq!(name2, "qwen-plus(thinking:budget)");
        assert_eq!(cfg2, ThinkingConfig::Off);

        // Non-numeric budget payload is also rejected.
        let (name3, cfg3) = resolve_model_thinking("qwen-plus(thinking:budget:abc)");
        assert_eq!(name3, "qwen-plus(thinking:budget:abc)");
        assert_eq!(cfg3, ThinkingConfig::Off);
    }

    #[test]
    fn resolve_model_without_suffix() {
        let (name, cfg) = resolve_model_thinking("us.anthropic.claude-opus-4-6-v1");
        assert_eq!(name, "us.anthropic.claude-opus-4-6-v1");
        assert_eq!(cfg, ThinkingConfig::Off);
    }

    #[test]
    fn resolve_model_with_explicit_effort_suffix() {
        let (name, cfg) = resolve_model_thinking("some-model(thinking:low)");
        assert_eq!(name, "some-model");
        assert_eq!(
            cfg,
            ThinkingConfig::Adaptive {
                effort: ThinkingEffort::Low
            }
        );

        let (name, cfg) = resolve_model_thinking("some-model(thinking:high)");
        assert_eq!(name, "some-model");
        assert_eq!(
            cfg,
            ThinkingConfig::Adaptive {
                effort: ThinkingEffort::High
            }
        );
    }

    #[test]
    fn suffix_roundtrip_budget() {
        // Round-trip: Enabled{N} → suffix carries N → parse restores N exactly.
        for budget in [1_000u32, 10_000, 16_000, 64_000] {
            let cfg = ThinkingConfig::Enabled {
                budget_tokens: budget,
            };
            let suffix = thinking_suffix_for(&cfg);
            assert_eq!(suffix, format!("(thinking:budget:{budget})"));
            let (_, resolved) = resolve_model_thinking(&format!("model{suffix}"));
            assert_eq!(
                resolved, cfg,
                "round-trip failed for budget_tokens={budget}"
            );
        }
    }

    #[test]
    fn suffix_roundtrip_off() {
        // Off produces an empty suffix; appending it to a model name leaves the
        // name unchanged and resolves back to Off.
        let suffix = thinking_suffix_for(&ThinkingConfig::Off);
        assert!(suffix.is_empty());
        let selector = format!("some-model{suffix}");
        let (name, cfg) = resolve_model_thinking(&selector);
        assert_eq!(name, "some-model");
        assert_eq!(cfg, ThinkingConfig::Off);
    }

    #[test]
    fn suffix_roundtrip_adaptive() {
        for effort in [
            ThinkingEffort::Low,
            ThinkingEffort::Medium,
            ThinkingEffort::High,
            ThinkingEffort::Max,
        ] {
            let cfg = ThinkingConfig::Adaptive { effort };
            let suffix = thinking_suffix_for(&cfg);
            let (_, resolved) = resolve_model_thinking(&format!("model{suffix}"));
            assert_eq!(resolved, cfg, "roundtrip failed for effort {effort:?}");
        }
    }

    #[test]
    fn no_model_without_suffix_gets_thinking() {
        for name in [
            "qwen-plus",
            "us.anthropic.claude-sonnet-4-6",
            "gpt-5",
            "glm-5.1",
        ] {
            let (_, cfg) = resolve_model_thinking(name);
            assert_eq!(
                cfg,
                ThinkingConfig::Off,
                "{name} without suffix should be Off"
            );
        }
    }

    // ─── apply_openai_suppression ──────────────────────────────────────

    #[test]
    fn apply_openai_suppression_behavior() {
        // Dashscope provider sets enable_thinking=false
        let mut body = json!({"model": "qwen3.5-flash", "messages": []});
        apply_openai_suppression(&ThinkingConfig::Off, &mut body, "dashscope", "");
        assert_eq!(body["enable_thinking"], false);
        // Dashscope base_url also triggers
        let mut body = json!({"model": "qwen-plus", "messages": []});
        apply_openai_suppression(
            &ThinkingConfig::Off,
            &mut body,
            "openai",
            "https://dashscope.aliyuncs.com/compatible-mode/v1",
        );
        assert_eq!(body["enable_thinking"], false);
        // Generic provider + Off is noop
        let mut body = json!({"model": "gpt-4o", "messages": []});
        apply_openai_suppression(
            &ThinkingConfig::Off,
            &mut body,
            "openai",
            "https://api.openai.com/v1",
        );
        assert!(body.get("enable_thinking").is_none());
        // DeepSeek V4 uses a typed thinking object rather than the DashScope
        // boolean flag. Endpoint protocol selects the wire shape.
        let mut body = json!({"model": "deepseek-v4-flash", "messages": []});
        apply_openai_suppression(
            &ThinkingConfig::Off,
            &mut body,
            "openai",
            "https://api.deepseek.com",
        );
        assert_eq!(body["thinking"]["type"], "disabled");
        let mut body = json!({"model": "deepseek-v4-flash", "messages": []});
        apply_openai_suppression(&ThinkingConfig::Off, &mut body, "deepseek", "");
        assert_eq!(body["thinking"]["type"], "disabled");
        // Enabled thinking is noop (don't suppress)
        let mut body = json!({"model": "qwen3.5-flash", "messages": []});
        apply_openai_suppression(
            &ThinkingConfig::Enabled {
                budget_tokens: 8000,
            },
            &mut body,
            "dashscope",
            "",
        );
        assert!(body.get("enable_thinking").is_none());
    }

    // ─── strip_think_tags ──────────────────────────────────────────────

    #[test]
    fn strip_think_tags_behavior() {
        assert_eq!(
            strip_think_tags("before\n<think>\nreasoning here\n</think>\nafter"),
            "before\n\nafter"
        );
        assert_eq!(strip_think_tags("just normal text"), "just normal text");
        assert_eq!(
            strip_think_tags("prefix<think>reasoning without end"),
            "prefix"
        );
        assert_eq!(
            strip_think_tags("a<think>x</think>b<think>y</think>c"),
            "abc"
        );
    }

    // ─── provider_may_think_natively ───────────────────────────────────

    #[test]
    fn dashscope_provider_may_think() {
        assert!(provider_may_think_natively("dashscope"));
        assert!(provider_may_think_natively("aliyun"));
        assert!(provider_may_think_natively("alibaba-cloud"));
    }

    #[test]
    fn generic_provider_does_not_think() {
        assert!(!provider_may_think_natively("openai"));
        assert!(!provider_may_think_natively("bedrock"));
        assert!(!provider_may_think_natively("deepseek"));
    }

    // ─── needs_dashscope_thinking_flag ─────────────────────────────────

    #[test]
    fn provider_thinking_and_dashscope_detection() {
        // Think natively
        assert!(provider_may_think_natively("dashscope"));
        assert!(!provider_may_think_natively("openai"));
        assert!(!provider_may_think_natively("bedrock"));
        assert!(!provider_may_think_natively("deepseek"));
        // Dashscope thinking flag detection
        assert!(needs_dashscope_thinking_flag("dashscope", ""));
        assert!(needs_dashscope_thinking_flag(
            "openai",
            "https://dashscope.aliyuncs.com/compatible-mode/v1"
        ));
        assert!(!needs_dashscope_thinking_flag(
            "openai",
            "https://api.openai.com/v1"
        ));
        assert!(!needs_dashscope_thinking_flag(
            "openai",
            "https://api.deepseek.com"
        ));
    }

    #[test]
    fn dashscope_detected_by_base_url() {
        assert!(needs_dashscope_thinking_flag(
            "openai",
            "https://dashscope.aliyuncs.com/compatible-mode/v1"
        ));
    }

    #[test]
    fn not_dashscope_for_generic_openai() {
        assert!(!needs_dashscope_thinking_flag(
            "openai",
            "https://api.openai.com/v1"
        ));
    }

    #[test]
    fn not_dashscope_for_deepseek() {
        assert!(!needs_dashscope_thinking_flag(
            "openai",
            "https://api.deepseek.com"
        ));
    }

    #[test]
    fn openai_thinking_control_is_endpoint_protocol_scoped() {
        assert_eq!(
            openai_thinking_control("openai", "https://api.deepseek.com/v1"),
            OpenAiThinkingControl::ThinkingObject
        );
        assert_eq!(
            openai_thinking_control(
                "openai",
                "https://dashscope.aliyuncs.com/compatible-mode/v1"
            ),
            OpenAiThinkingControl::EnableThinkingFlag
        );
        assert_eq!(
            openai_thinking_control("openai", "https://openai.example.com/v1"),
            OpenAiThinkingControl::None
        );
        assert_eq!(
            openai_thinking_control("deepseek", ""),
            OpenAiThinkingControl::ThinkingObject
        );
        // An unrelated proxy hostname must not opt a route into a provider
        // extension field.
        assert_eq!(
            openai_thinking_control("openai", "https://deepseek-proxy.example.com/v1"),
            OpenAiThinkingControl::None
        );
    }
}

#[cfg(test)]
mod fork_capture_thinking_slice_tests {
    use super::*;

    #[test]
    fn off_mode_returns_none_for_non_replay_model() {
        let result = fork_capture_thinking_slice(&ThinkingConfig::Off, "openai", "gpt-4");
        assert!(result.is_none());
    }

    #[test]
    fn off_mode_returns_slice_for_replay_model() {
        let result =
            fork_capture_thinking_slice(&ThinkingConfig::Off, "deepseek", "deepseek-reasoner");
        assert!(result.is_some());
        let slice = result.unwrap();
        assert!(slice.enabled);
        assert_eq!(slice.budget_tokens, 0);
        assert_eq!(slice.kind, "native");
    }

    #[test]
    fn enabled_mode_returns_slice_with_budget() {
        let result = fork_capture_thinking_slice(
            &ThinkingConfig::Enabled {
                budget_tokens: 10000,
            },
            "openai",
            "gpt-4",
        );
        assert!(result.is_some());
        let slice = result.unwrap();
        assert!(slice.enabled);
        assert_eq!(slice.budget_tokens, 10000);
        assert_eq!(slice.kind, "enabled");
    }

    #[test]
    fn adaptive_mode_returns_slice_with_effort_in_kind() {
        let result = fork_capture_thinking_slice(
            &ThinkingConfig::Adaptive {
                effort: ThinkingEffort::High,
            },
            "anthropic",
            "claude-3-5-sonnet",
        );
        let slice = result.expect("adaptive must capture a slice");
        assert!(slice.enabled);
        assert_eq!(slice.budget_tokens, 0);
        assert_eq!(
            slice.kind, "adaptive:high",
            "effort level participates in cache identity",
        );
    }

    #[test]
    fn adaptive_effort_levels_produce_distinct_slices() {
        // Cache-correctness invariant: two parents that differ only in
        // adaptive effort must NOT collapse to the same ThinkingConfigSlice
        // (and therefore the same ForkPrefix::identity_hash). Without this,
        // a Low-effort parent's cache entry would be reused by a Max-effort
        // child, producing a wire mismatch on the next round.
        let low = fork_capture_thinking_slice(
            &ThinkingConfig::Adaptive {
                effort: ThinkingEffort::Low,
            },
            "anthropic",
            "claude-3-5-sonnet",
        )
        .unwrap();
        let max = fork_capture_thinking_slice(
            &ThinkingConfig::Adaptive {
                effort: ThinkingEffort::Max,
            },
            "anthropic",
            "claude-3-5-sonnet",
        )
        .unwrap();
        assert_ne!(low, max, "adaptive effort must influence the slice");
    }

    #[test]
    fn enabled_mode_works_for_replay_model() {
        let result = fork_capture_thinking_slice(
            &ThinkingConfig::Enabled {
                budget_tokens: 5000,
            },
            "deepseek",
            "deepseek-reasoner",
        );
        assert!(result.is_some());
        let slice = result.unwrap();
        assert!(slice.enabled);
        assert_eq!(slice.budget_tokens, 5000);
        assert_eq!(slice.kind, "enabled");
    }
}

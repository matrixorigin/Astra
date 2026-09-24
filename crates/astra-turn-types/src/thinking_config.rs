//! Shared provider-agnostic thinking values and pure transformations.
//!
//! No model catalog, endpoint resolution, or runtime dependencies belong here.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fmt;

/// Provider-agnostic thinking configuration.
///
/// # Wire format per provider
///
/// | Variant | Bedrock Converse | Anthropic Messages | OpenAI-compatible |
/// |---------|------------------|--------------------|-------------------|
/// | `Off` | (no field) | (no field) | (no field) |
/// | `Enabled{budget}` | `additionalModelRequestFields.thinking` | `thinking` | provider-specific (`enable_thinking` for DashScope/Qwen) |
/// | `Adaptive{effort}` | `additionalModelRequestFields.{thinking,output_config}` | `thinking` + `output_config.effort` | `reasoning_effort` (or provider-specific thinking flag) |
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum ThinkingConfig {
    /// Thinking disabled (default).
    #[default]
    Off,
    /// Fixed budget thinking — model uses up to `budget_tokens` for reasoning.
    /// Compatible with Claude 3.7 Sonnet, Claude 4 Sonnet/Opus/Haiku.
    Enabled { budget_tokens: u32 },
    /// Adaptive thinking — model decides how much to think.
    /// Compatible with Claude Opus 4.6+, Sonnet 4.6+.
    /// For generic OpenAI-compatible providers, maps to `reasoning_effort`.
    Adaptive {
        #[serde(default = "default_effort")]
        effort: ThinkingEffort,
    },
}

/// Effort level for adaptive thinking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingEffort {
    Low,
    Medium,
    High,
    Max,
}

fn default_effort() -> ThinkingEffort {
    ThinkingEffort::High
}

impl ThinkingConfig {
    pub fn is_off(&self) -> bool {
        matches!(self, Self::Off)
    }

    pub fn is_enabled(&self) -> bool {
        !self.is_off()
    }

    /// Apply thinking config to a Bedrock Converse request body.
    /// Sets `additionalModelRequestFields.thinking` and removes incompatible fields.
    pub fn apply_bedrock(&self, body: &mut Value) {
        match self {
            Self::Off => {}
            Self::Enabled { budget_tokens } => {
                body["additionalModelRequestFields"] = json!({
                    "thinking": {
                        "type": "enabled",
                        "budget_tokens": budget_tokens
                    }
                });
                // Thinking is incompatible with temperature
                remove_temperature_from_inference_config(body);
            }
            Self::Adaptive { effort } => {
                // Opus 4.7+ defaults display to "omitted" (thinking block present
                // but text empty). Explicitly request "summarized" so the CLI can
                // show a thinking preview.
                body["additionalModelRequestFields"] = json!({
                    "thinking": {
                        "type": "adaptive",
                        "display": "summarized"
                    },
                    "output_config": {
                        "effort": effort_str(*effort)
                    }
                });
                remove_temperature_from_inference_config(body);
            }
        }
    }

    /// Apply thinking config to an Anthropic Messages API request body.
    /// Sets top-level `thinking` field and removes incompatible fields.
    pub fn apply_anthropic(&self, body: &mut Value) {
        match self {
            Self::Off => {}
            Self::Enabled { budget_tokens } => {
                body["thinking"] = json!({
                    "type": "enabled",
                    "budget_tokens": budget_tokens
                });
                // Thinking is incompatible with temperature/top_p/top_k
                remove_key(body, "temperature");
                remove_key(body, "top_p");
                remove_key(body, "top_k");
            }
            Self::Adaptive { effort } => {
                body["thinking"] = json!({
                    "type": "adaptive",
                    "display": "summarized"
                });
                body["output_config"] = json!({
                    "effort": effort_str(*effort)
                });
                remove_key(body, "temperature");
                remove_key(body, "top_p");
                remove_key(body, "top_k");
            }
        }
    }

    /// Apply thinking config to an OpenAI-compatible request body.
    /// Only Adaptive maps to `reasoning_effort`; Enabled is a no-op for OpenAI.
    pub fn apply_openai(&self, body: &mut Value) {
        match self {
            Self::Off => {}
            Self::Enabled { .. } => {
                // OpenAI doesn't have a budget-based thinking mode.
                // Some providers (DeepSeek) use <think> tags automatically.
                // No-op for now; extensible for future providers.
            }
            Self::Adaptive { effort } => {
                body["reasoning_effort"] = json!(effort_str(*effort));
            }
        }
    }

    /// Serialize to JSON for inclusion in the chat payload sent to the server.
    pub fn to_payload_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(json!("off"))
    }

    /// Deserialize from the `thinking` field in the chat payload.
    ///
    /// Only the current tagged wire shape is accepted. Malformed or unknown
    /// control input is an admission error, never an implicit request to turn
    /// reasoning off.
    pub fn from_payload_value(v: &Value) -> Result<Self, String> {
        serde_json::from_value::<Self>(v.clone())
            .map_err(|error| format!("invalid thinking configuration: {error}"))
    }
}

impl ThinkingEffort {
    pub fn as_str(self) -> &'static str {
        effort_str(self)
    }

    /// Order used for softening/escalation. Higher ordinal = more tokens.
    fn ordinal(self) -> u8 {
        match self {
            ThinkingEffort::Low => 0,
            ThinkingEffort::Medium => 1,
            ThinkingEffort::High => 2,
            ThinkingEffort::Max => 3,
        }
    }

    fn from_ordinal(o: u8) -> ThinkingEffort {
        match o {
            0 => ThinkingEffort::Low,
            1 => ThinkingEffort::Medium,
            2 => ThinkingEffort::High,
            _ => ThinkingEffort::Max,
        }
    }

    /// Cap effort at `ceiling` — if current is stronger than ceiling, drop to ceiling.
    /// Used by the per-turn dampener: user's picked effort is the ceiling, not the floor.
    pub fn capped_at(self, ceiling: ThinkingEffort) -> ThinkingEffort {
        Self::from_ordinal(self.ordinal().min(ceiling.ordinal()))
    }
}

impl fmt::Display for ThinkingEffort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for ThinkingConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ThinkingConfig::Off => write!(f, "off"),
            ThinkingConfig::Enabled { budget_tokens } => {
                write!(f, "enabled(budget:{})", budget_tokens)
            }
            ThinkingConfig::Adaptive { effort } => write!(f, "adaptive({})", effort),
        }
    }
}

/// Signals the runtime uses to decide how much thinking a turn actually warrants.
///
/// The user's choice of `thinking:high` via `/model` encodes an INTENT ceiling
/// ("I'm willing to spend this much"), not a command to burn the full budget on
/// every turn regardless of question. A short "why does X do Y?" question does
/// not need 30k reasoning tokens. This struct feeds `ThinkingConfig::scale_for_turn`.
#[derive(Debug, Clone, Copy, Default)]
pub struct TurnComplexitySignals {
    /// Length of the user's input in characters. Shorter = lower complexity prior.
    pub input_char_len: usize,
    /// True only when the typed LLM intent judge classified the turn as a
    /// lightweight, read-only request. Judge absence stays false.
    pub typed_lightweight: bool,
    /// True when the typed objective relation keeps working on the current
    /// objective. Natural-language phrases are never inspected here.
    pub continues_current_objective: bool,
}

impl TurnComplexitySignals {
    /// Returns true when the turn is short, read-only, and not a continuation —
    /// the profile where full high/max thinking budget is almost always wasted.
    fn is_lightweight(&self) -> bool {
        self.input_char_len > 0
            && self.input_char_len <= 120
            && self.typed_lightweight
            && !self.continues_current_objective
    }
}

impl ThinkingConfig {
    /// Return a per-turn dampened copy of this config based on observed signals.
    ///
    /// Philosophy: the user's pick via `/model` is a **ceiling** on spend, not a
    /// floor. For a short interrogative question, burning a full `max` or `high`
    /// reasoning budget is pure waste — empirically this was the immediate cause
    /// of the session-36500dd9 spiral where a 37-token question produced 30k+
    /// output tokens and triggered the circuit breaker.
    ///
    /// What this does NOT do:
    /// - never INCREASES effort (the user's pick is the ceiling)
    /// - never turns thinking OFF if the user explicitly enabled it
    /// - never changes the user's stored preference (caller must use the
    ///   returned value for THIS turn only)
    ///
    /// Conservative fallback: when signals don't clearly indicate lightweight
    /// work, returns self unchanged so multi-step / implementation turns are
    /// unaffected.
    pub fn scale_for_turn(&self, signals: TurnComplexitySignals) -> ThinkingConfig {
        if !signals.is_lightweight() {
            return self.clone();
        }
        match self {
            ThinkingConfig::Off => ThinkingConfig::Off,
            ThinkingConfig::Enabled { budget_tokens } => {
                // Cap at 4k for lightweight turns. This covers Anthropic's minimum
                // viable thinking budget (1024) with headroom, without wasting spend
                // on turns that will produce a short answer.
                let capped = (*budget_tokens).min(4_000);
                ThinkingConfig::Enabled {
                    budget_tokens: capped,
                }
            }
            ThinkingConfig::Adaptive { effort } => {
                // Drop effort by one level with a Low floor. The user still sees
                // "thinking" behaviour (model still reasons), just doesn't burn
                // high/max-level budget on trivial questions.
                ThinkingConfig::Adaptive {
                    effort: effort.capped_at(ThinkingEffort::Medium),
                }
            }
        }
    }
}

fn effort_str(e: ThinkingEffort) -> &'static str {
    match e {
        ThinkingEffort::Low => "low",
        ThinkingEffort::Medium => "medium",
        ThinkingEffort::High => "high",
        ThinkingEffort::Max => "max",
    }
}

fn remove_temperature_from_inference_config(body: &mut Value) {
    if let Some(ic) = body
        .get_mut("inferenceConfig")
        .and_then(Value::as_object_mut)
    {
        ic.remove("temperature");
    }
}

fn remove_key(body: &mut Value, key: &str) {
    if let Some(obj) = body.as_object_mut() {
        obj.remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── Bedrock Converse wire format ───────────────────────────────────

    #[test]
    fn bedrock_off_no_fields() {
        let mut body = json!({"messages": [], "inferenceConfig": {"maxTokens": 4096}});
        ThinkingConfig::Off.apply_bedrock(&mut body);
        assert!(body.get("additionalModelRequestFields").is_none());
    }

    #[test]
    fn bedrock_enabled_full_body() {
        let mut body = json!({
            "messages": [{"role": "user", "content": [{"text": "hello"}]}],
            "inferenceConfig": {"maxTokens": 8192, "temperature": 0.7},
            "toolConfig": {"tools": []}
        });
        ThinkingConfig::Enabled {
            budget_tokens: 5000,
        }
        .apply_bedrock(&mut body);

        // Thinking field present
        assert_eq!(
            body["additionalModelRequestFields"]["thinking"],
            json!({"type": "enabled", "budget_tokens": 5000})
        );
        // Temperature removed (incompatible)
        assert!(body["inferenceConfig"].get("temperature").is_none());
        // maxTokens preserved
        assert_eq!(body["inferenceConfig"]["maxTokens"], 8192);
        // Other fields untouched
        assert!(body.get("messages").is_some());
        assert!(body.get("toolConfig").is_some());
    }

    #[test]
    fn bedrock_adaptive_full_body() {
        let mut body = json!({
            "messages": [],
            "inferenceConfig": {"maxTokens": 16000, "temperature": 1.0}
        });
        ThinkingConfig::Adaptive {
            effort: ThinkingEffort::Low,
        }
        .apply_bedrock(&mut body);

        assert_eq!(
            body["additionalModelRequestFields"]["thinking"],
            json!({"type": "adaptive", "display": "summarized"})
        );
        assert_eq!(
            body["additionalModelRequestFields"]["output_config"],
            json!({"effort": "low"})
        );
        assert!(body["inferenceConfig"].get("temperature").is_none());
    }

    // ─── Anthropic Messages wire format ─────────────────────────────────

    #[test]
    fn anthropic_off_no_fields() {
        let mut body =
            json!({"model": "claude-sonnet-4-20250514", "messages": [], "max_tokens": 4096});
        ThinkingConfig::Off.apply_anthropic(&mut body);
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn anthropic_enabled_full_body() {
        let mut body = json!({
            "model": "claude-sonnet-4-20250514",
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 8192,
            "temperature": 0.5,
            "stream": true
        });
        ThinkingConfig::Enabled {
            budget_tokens: 4000,
        }
        .apply_anthropic(&mut body);

        assert_eq!(
            body["thinking"],
            json!({"type": "enabled", "budget_tokens": 4000})
        );
        // Temperature removed
        assert!(body.get("temperature").is_none());
        // Other fields preserved
        assert_eq!(body["model"], "claude-sonnet-4-20250514");
        assert_eq!(body["max_tokens"], 8192);
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn anthropic_adaptive_full_body() {
        let mut body = json!({
            "model": "claude-opus-4-6",
            "messages": [],
            "max_tokens": 16000,
            "temperature": 1.0
        });
        ThinkingConfig::Adaptive {
            effort: ThinkingEffort::Medium,
        }
        .apply_anthropic(&mut body);

        assert_eq!(
            body["thinking"],
            json!({"type": "adaptive", "display": "summarized"})
        );
        assert_eq!(body["output_config"], json!({"effort": "medium"}));
        assert!(body.get("temperature").is_none());
    }

    // ─── OpenAI-compatible wire format ──────────────────────────────────

    #[test]
    fn openai_off_no_fields() {
        let mut body = json!({"model": "gpt-4", "messages": []});
        ThinkingConfig::Off.apply_openai(&mut body);
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn openai_enabled_noop() {
        let mut body = json!({"model": "gpt-4", "messages": [], "temperature": 0.7});
        ThinkingConfig::Enabled {
            budget_tokens: 5000,
        }
        .apply_openai(&mut body);
        // No reasoning_effort added, temperature untouched
        assert!(body.get("reasoning_effort").is_none());
        assert_eq!(body["temperature"], 0.7);
    }

    #[test]
    fn openai_adaptive_sets_reasoning_effort() {
        let mut body = json!({"model": "o3", "messages": []});
        ThinkingConfig::Adaptive {
            effort: ThinkingEffort::Medium,
        }
        .apply_openai(&mut body);
        assert_eq!(body["reasoning_effort"], "medium");
    }

    // ─── Serde round-trip ───────────────────────────────────────────────

    #[test]
    fn serde_roundtrip() {
        let mut cases = vec![
            (ThinkingConfig::Off, json!({"mode": "off"})),
            (
                ThinkingConfig::Enabled {
                    budget_tokens: 8192,
                },
                json!({"mode": "enabled", "budget_tokens": 8192}),
            ),
        ];
        for effort in [
            ThinkingEffort::Low,
            ThinkingEffort::Medium,
            ThinkingEffort::High,
            ThinkingEffort::Max,
        ] {
            cases.push((
                ThinkingConfig::Adaptive { effort },
                json!({"mode": "adaptive", "effort": effort.as_str()}),
            ));
        }
        for (config, expected) in cases {
            assert_eq!(config.to_payload_value(), expected);
            assert_eq!(
                ThinkingConfig::from_payload_value(&expected).unwrap(),
                config
            );
        }
        assert_eq!(
            ThinkingConfig::from_payload_value(&json!({"mode": "adaptive"})).unwrap(),
            ThinkingConfig::Adaptive {
                effort: ThinkingEffort::High
            },
        );
    }

    // === TDD fix tests ===

    /// Adaptive models expect the effort outside the `thinking` object.
    #[test]
    fn bedrock_adaptive_high_uses_output_config_effort() {
        let cfg = ThinkingConfig::Adaptive {
            effort: ThinkingEffort::High,
        };
        let mut body = json!({ "inferenceConfig": { "temperature": 0.5 } });
        cfg.apply_bedrock(&mut body);
        assert_eq!(
            body["additionalModelRequestFields"]["thinking"],
            json!({"type": "adaptive", "display": "summarized"})
        );
        assert_eq!(
            body["additionalModelRequestFields"]["output_config"],
            json!({"effort": "high"})
        );
    }

    /// Anthropic Messages rejects `thinking.adaptive.effort`; effort belongs in
    /// `output_config.effort`.
    #[test]
    fn anthropic_adaptive_high_uses_output_config_effort() {
        let cfg = ThinkingConfig::Adaptive {
            effort: ThinkingEffort::High,
        };
        let mut body = json!({ "temperature": 0.5 });
        cfg.apply_anthropic(&mut body);
        assert_eq!(
            body["thinking"],
            json!({"type": "adaptive", "display": "summarized"})
        );
        assert_eq!(body["output_config"], json!({"effort": "high"}));
    }

    #[test]
    fn from_payload_value_rejects_removed_bare_budget_shape() {
        let retired = json!({ "budget_tokens": 8000 });
        assert!(ThinkingConfig::from_payload_value(&retired).is_err());
    }

    #[test]
    fn from_payload_value_new_format_still_works() {
        let new = json!({ "mode": "enabled", "budget_tokens": 12000 });
        let cfg = ThinkingConfig::from_payload_value(&new).unwrap();
        assert_eq!(
            cfg,
            ThinkingConfig::Enabled {
                budget_tokens: 12000
            }
        );
    }

    #[test]
    fn from_payload_value_unknown_shape_is_rejected() {
        for invalid in [
            json!({"foo": "bar"}),
            json!({"mode": "automatic"}),
            json!({"mode": "adaptive", "effort": "extreme"}),
        ] {
            assert!(ThinkingConfig::from_payload_value(&invalid).is_err());
        }
    }

    // ─── Dynamic budget scaling ─────────────────────────────────────────

    fn complexity_signals(
        message: &str,
        typed_lightweight: bool,
        continues_current_objective: bool,
    ) -> TurnComplexitySignals {
        TurnComplexitySignals {
            input_char_len: message.trim().chars().count(),
            typed_lightweight,
            continues_current_objective,
        }
    }

    #[test]
    fn scale_for_turn_short_conceptual_question_drops_high_to_medium() {
        let cfg = ThinkingConfig::Adaptive {
            effort: ThinkingEffort::High,
        };
        let sig = complexity_signals("why does the circuit breaker abort?", true, false);
        assert!(sig.is_lightweight());
        let scaled = cfg.scale_for_turn(sig);
        assert_eq!(
            scaled,
            ThinkingConfig::Adaptive {
                effort: ThinkingEffort::Medium
            },
            "short conceptual Q should drop high → medium"
        );
    }

    #[test]
    fn scale_for_turn_chinese_conceptual_question_dampens() {
        let cfg = ThinkingConfig::Adaptive {
            effort: ThinkingEffort::Max,
        };
        let sig = complexity_signals("为啥其他model看不到thinking和不thinking?", true, false);
        let scaled = cfg.scale_for_turn(sig);
        assert_eq!(
            scaled,
            ThinkingConfig::Adaptive {
                effort: ThinkingEffort::Medium
            }
        );
    }

    #[test]
    fn scale_for_turn_non_lightweight_typed_intent_is_not_dampened() {
        let cfg = ThinkingConfig::Adaptive {
            effort: ThinkingEffort::High,
        };
        let sig = complexity_signals("arbitrary short input", false, false);
        assert!(!sig.is_lightweight());
        let scaled = cfg.scale_for_turn(sig);
        assert_eq!(
            scaled, cfg,
            "non-lightweight typed intent should not dampen"
        );
    }

    #[test]
    fn scale_for_turn_typed_continuation_is_not_dampened() {
        let cfg = ThinkingConfig::Adaptive {
            effort: ThinkingEffort::High,
        };
        let sig = complexity_signals("arbitrary short input", true, true);
        assert!(sig.continues_current_objective);
        assert!(!sig.is_lightweight());
        let scaled = cfg.scale_for_turn(sig);
        assert_eq!(scaled, cfg, "typed continuation should retain the ceiling");
    }

    #[test]
    fn scale_for_turn_unjudged_short_input_fails_safe_without_dampening() {
        let cfg = ThinkingConfig::Adaptive {
            effort: ThinkingEffort::High,
        };
        let sig = complexity_signals("fix explain continue 修复 为什么", false, false);
        assert!(!sig.is_lightweight());
        assert_eq!(cfg.scale_for_turn(sig), cfg);
    }

    #[test]
    fn scale_for_turn_long_message_not_dampened() {
        let cfg = ThinkingConfig::Adaptive {
            effort: ThinkingEffort::High,
        };
        let long = "why is this happening? ".repeat(20); // > 120 chars
        let sig = complexity_signals(&long, true, false);
        let scaled = cfg.scale_for_turn(sig);
        assert_eq!(scaled, cfg, "long message should not dampen");
    }

    #[test]
    fn scale_for_turn_enabled_budget_capped_at_4k() {
        let cfg = ThinkingConfig::Enabled {
            budget_tokens: 10_000,
        };
        let sig = complexity_signals("what is a session id?", true, false);
        let scaled = cfg.scale_for_turn(sig);
        assert_eq!(
            scaled,
            ThinkingConfig::Enabled {
                budget_tokens: 4_000
            }
        );
    }

    #[test]
    fn scale_for_turn_off_stays_off() {
        let sig = complexity_signals("why?", true, false);
        assert_eq!(ThinkingConfig::Off.scale_for_turn(sig), ThinkingConfig::Off);
    }

    #[test]
    fn scale_for_turn_low_effort_stays_low() {
        let cfg = ThinkingConfig::Adaptive {
            effort: ThinkingEffort::Low,
        };
        let sig = complexity_signals("why?", true, false);
        let scaled = cfg.scale_for_turn(sig);
        assert_eq!(
            scaled,
            ThinkingConfig::Adaptive {
                effort: ThinkingEffort::Low
            },
            "Low effort should not escalate"
        );
    }

    #[test]
    fn capped_at_never_increases() {
        assert_eq!(
            ThinkingEffort::Low.capped_at(ThinkingEffort::High),
            ThinkingEffort::Low
        );
        assert_eq!(
            ThinkingEffort::Max.capped_at(ThinkingEffort::Medium),
            ThinkingEffort::Medium
        );
    }
}

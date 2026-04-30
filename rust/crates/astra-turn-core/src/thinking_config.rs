//! Thinking/extended-reasoning configuration for LLM requests.
//!
//! Provides a provider-agnostic [`ThinkingConfig`] that each provider maps to its
//! native wire format. Designed for extensibility — new modes (e.g., future
//! "auto" heuristic) can be added as variants without breaking existing callers.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

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
#[serde(tag = "mode", rename_all = "snake_case")]
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
    /// Accepts both:
    /// - New format: `{"mode": "enabled", "budget_tokens": N}` / `{"mode": "adaptive", ...}` / `"off"`
    /// - Legacy format: `{"budget_tokens": N}` (treated as Enabled)
    pub fn from_payload_value(v: &Value) -> Self {
        // Try new tagged format first.
        if let Ok(cfg) = serde_json::from_value::<Self>(v.clone()) {
            return cfg;
        }
        // Legacy fallback: bare `{"budget_tokens": N}` from older CLIs.
        if let Some(n) = v.get("budget_tokens").and_then(Value::as_u64) {
            return Self::Enabled {
                budget_tokens: n as u32,
            };
        }
        Self::default()
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
    /// True when the user's message indicates a workspace modification intent —
    /// "implement / fix / refactor / 修复 / 实现" etc. Modification implies
    /// multi-step reasoning, so we do NOT dampen in that case.
    pub has_modification_intent: bool,
    /// True when this turn is a mid-task continuation (e.g. "继续" / "continue")
    /// where the real complexity lives in the ongoing task, not in the literal
    /// user input. Conservative: we keep full effort here so we don't starve
    /// multi-round work of reasoning budget.
    pub is_continuation: bool,
}

impl TurnComplexitySignals {
    /// Heuristic factory from a raw user message. Callers needing more precise
    /// signals (e.g. plan executor) can construct the struct directly.
    pub fn from_message(message: &str) -> Self {
        let trimmed = message.trim();
        let lower = trimmed.to_lowercase();
        let has_modification_intent = [
            "implement",
            "fix",
            "refactor",
            "optimize",
            "build",
            "rewrite",
            "修复",
            "实现",
            "重构",
            "优化",
            "修改",
            "重写",
        ]
        .iter()
        .any(|kw| lower.contains(kw));
        let is_continuation = matches!(
            trimmed,
            "continue" | "Continue" | "go on" | "keep going" | "继续" | "接着" | "go" | "next"
        );
        Self {
            input_char_len: trimmed.chars().count(),
            has_modification_intent,
            is_continuation,
        }
    }

    /// Returns true when the turn is short, read-only, and not a continuation —
    /// the profile where full high/max thinking budget is almost always wasted.
    fn is_lightweight(&self) -> bool {
        self.input_char_len > 0
            && self.input_char_len <= 120
            && !self.has_modification_intent
            && !self.is_continuation
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
    /// Display label (e.g., "Normal", "Thinking (Low)", "Thinking (High)")
    pub label: &'static str,
    /// The ThinkingConfig to use when this option is selected.
    pub config: ThinkingConfig,
    /// Whether this is the default selection (shown with ← marker).
    pub is_default: bool,
}

/// Returns thinking options for a model based on its declared `thinking_mode` and provider.
///
/// - `thinking_mode = Some("controllable")`: provider-appropriate picker.
///   - `bedrock` / `anthropic` → adaptive (Low / High).
///   - `dashscope` / `aliyun` / `alibaba` → budget (on/off).
///   - other providers → adaptive reasoning (`reasoning_effort`).
/// - `thinking_mode = Some("native")` → empty (model thinks by default, no picker).
/// - `thinking_mode = None` → empty (no thinking support).
pub fn thinking_options_with_capability(
    _model_name: &str,
    provider: Option<&str>,
    thinking_mode: Option<&str>,
) -> Vec<ThinkingOption> {
    match thinking_mode {
        Some("controllable") => {
            if provider_uses_budget_thinking(provider) {
                thinking_options_for_budget_thinking()
            } else {
                thinking_options_for_adaptive_reasoning()
            }
        }
        // Known no-picker modes.
        None | Some("native") => vec![],
        // Unknown value — warn so operators notice typos in YAML/DB
        // (e.g. "Controllable", "enabled") rather than silently disabling
        // the picker.
        Some(other) => {
            tracing::warn!(
                thinking_mode = %other,
                provider = ?provider,
                "unknown thinking_mode value; expected one of \"controllable\", \"native\", or null — picker disabled",
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
    provider
        .map(|p| {
            let p = p.to_ascii_lowercase();
            p.contains("dashscope") || p.contains("aliyun") || p.contains("alibaba")
        })
        .unwrap_or(false)
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
        let configs = vec![
            ThinkingConfig::Off,
            ThinkingConfig::Enabled {
                budget_tokens: 8192,
            },
            ThinkingConfig::Adaptive {
                effort: ThinkingEffort::Low,
            },
        ];
        for cfg in configs {
            let v = cfg.to_payload_value();
            let restored = ThinkingConfig::from_payload_value(&v);
            assert_eq!(cfg, restored);
        }
    }

    // ─── Model inference ────────────────────────────────────────────────

    #[test]
    fn controllable_dashscope_returns_budget() {
        let opts =
            thinking_options_with_capability("qwen-plus", Some("dashscope"), Some("controllable"));
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
    fn native_mode_returns_empty() {
        let opts = thinking_options_with_capability("glm-5.1", Some("openai"), Some("native"));
        assert!(opts.is_empty());
    }

    #[test]
    fn none_mode_returns_empty() {
        let opts = thinking_options_with_capability(
            "us.anthropic.claude-sonnet-4-6",
            Some("bedrock"),
            None,
        );
        assert!(opts.is_empty());
    }

    #[test]
    fn controllable_bedrock_returns_adaptive() {
        let opts = thinking_options_with_capability(
            "us.anthropic.claude-sonnet-4-6",
            Some("bedrock"),
            Some("controllable"),
        );
        assert_eq!(opts.len(), 3);
        assert_eq!(opts[0].label, "Normal");
        assert_eq!(opts[1].label, "Thinking (Low)");
        assert_eq!(opts[2].label, "Thinking (High)");
        assert!(opts[2].is_default);
    }

    #[test]
    fn controllable_anthropic_returns_adaptive() {
        let opts = thinking_options_with_capability(
            "claude-sonnet-4",
            Some("anthropic"),
            Some("controllable"),
        );
        assert_eq!(opts.len(), 3);
        assert_eq!(opts[2].label, "Thinking (High)");
    }

    #[test]
    fn controllable_openai_returns_adaptive() {
        let opts = thinking_options_with_capability("gpt-5", Some("openai"), Some("controllable"));
        assert_eq!(opts.len(), 3);
        assert_eq!(opts[2].label, "Thinking (High)");
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

    /// Fix #2: `from_payload_value` must gracefully accept the legacy wire format
    /// `{"thinking_budget_tokens": N}` emitted by older CLIs, mapping it to
    /// `Enabled { budget_tokens: N }`. The new format MUST still take precedence.
    #[test]
    fn from_payload_value_supports_legacy_budget_field() {
        // Legacy format: just a number (seen in older payloads as top-level field)
        let legacy = json!({ "budget_tokens": 8000 });
        let cfg = ThinkingConfig::from_payload_value(&legacy);
        assert_eq!(
            cfg,
            ThinkingConfig::Enabled {
                budget_tokens: 8000
            },
            "Legacy {{budget_tokens}} object must map to Enabled"
        );
    }

    #[test]
    fn from_payload_value_new_format_still_works() {
        let new = json!({ "mode": "enabled", "budget_tokens": 12000 });
        let cfg = ThinkingConfig::from_payload_value(&new);
        assert_eq!(
            cfg,
            ThinkingConfig::Enabled {
                budget_tokens: 12000
            }
        );
    }

    #[test]
    fn from_payload_value_unknown_shape_defaults_off() {
        let garbage = json!({ "foo": "bar" });
        assert_eq!(
            ThinkingConfig::from_payload_value(&garbage),
            ThinkingConfig::Off
        );
    }

    // ─── Dynamic budget scaling ─────────────────────────────────────────

    #[test]
    fn scale_for_turn_short_conceptual_question_drops_high_to_medium() {
        let cfg = ThinkingConfig::Adaptive {
            effort: ThinkingEffort::High,
        };
        let sig = TurnComplexitySignals::from_message("why does the circuit breaker abort?");
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
        let sig = TurnComplexitySignals::from_message("为啥其他model看不到thinking和不thinking?");
        let scaled = cfg.scale_for_turn(sig);
        assert_eq!(
            scaled,
            ThinkingConfig::Adaptive {
                effort: ThinkingEffort::Medium
            }
        );
    }

    #[test]
    fn scale_for_turn_modification_intent_not_dampened() {
        let cfg = ThinkingConfig::Adaptive {
            effort: ThinkingEffort::High,
        };
        let sig = TurnComplexitySignals::from_message("fix the bug in auth.rs");
        assert!(!sig.is_lightweight());
        let scaled = cfg.scale_for_turn(sig);
        assert_eq!(scaled, cfg, "modification intent should not dampen");
    }

    #[test]
    fn scale_for_turn_continuation_not_dampened() {
        let cfg = ThinkingConfig::Adaptive {
            effort: ThinkingEffort::High,
        };
        let sig = TurnComplexitySignals::from_message("继续");
        assert!(!sig.is_lightweight());
        let scaled = cfg.scale_for_turn(sig);
        assert_eq!(scaled, cfg, "continuation should not dampen");
    }

    #[test]
    fn scale_for_turn_long_message_not_dampened() {
        let cfg = ThinkingConfig::Adaptive {
            effort: ThinkingEffort::High,
        };
        let long = "why is this happening? ".repeat(20); // > 120 chars
        let sig = TurnComplexitySignals::from_message(&long);
        let scaled = cfg.scale_for_turn(sig);
        assert_eq!(scaled, cfg, "long message should not dampen");
    }

    #[test]
    fn scale_for_turn_enabled_budget_capped_at_4k() {
        let cfg = ThinkingConfig::Enabled {
            budget_tokens: 10_000,
        };
        let sig = TurnComplexitySignals::from_message("what is a session id?");
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
        let sig = TurnComplexitySignals::from_message("why?");
        assert_eq!(ThinkingConfig::Off.scale_for_turn(sig), ThinkingConfig::Off);
    }

    #[test]
    fn scale_for_turn_low_effort_stays_low() {
        let cfg = ThinkingConfig::Adaptive {
            effort: ThinkingEffort::Low,
        };
        let sig = TurnComplexitySignals::from_message("why?");
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

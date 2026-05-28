//! Config overlay + edit surface.
//!
//! Three responsibilities, one module — they all operate on
//! [`RuntimeConfig`] and share the same merge-if-non-default semantics:
//!
//! 1. `apply_settings_json` — partial JSON overlay onto a base config.
//!    Backs the `--settings <JSON-or-path>` CLI flag. Partial means any
//!    field omitted from the JSON keeps its base value; this matches
//!    operator intent when the flag is used as a one-shot override
//!    ("just raise the token budget for this one invocation").
//!
//! 2. `effective_budget_for_model` — resolves the model-aware input-
//!    token budget for `/config` display. Bridges `RuntimeLimits`'
//!    knowledge of model context windows with the operator's view of
//!    the config so the reported number matches reality.
//!
//! 3. `build_settings_catalog` + `filter_settings` + `apply_edit` —
//!    the pure-model layer behind an interactive `/config edit` UI.
//!    Catalog mirrors the reference implementation's Config.tsx model:
//!    flat list of { id, label, kind, value } items, each pointing at a
//!    single field in `RuntimeConfig`. The UI dispatches per `kind`, the
//!    write-back goes through `apply_edit`, the two ends close a loop
//!    that's regression-guarded by `every_catalog_item_is_editable_via_apply_edit`.

use crate::runtime_config::{
    RuntimeConfig, TraceCategory, TraceLevelSerde, TraceProfile, TraceSink,
};
use astra_core::runtime_limits::{context_window_for_model, RuntimeLimits};
use serde_json::Value;
use std::path::Path;

// ─── A. --settings overlay ───────────────────────────────────────────────

/// Errors produced by the overlay / edit surface.
#[derive(Debug, thiserror::Error)]
pub enum OverlayError {
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("cannot read --settings file {path}: {source}")]
    FileRead {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("unknown config path: {0}")]
    UnknownPath(String),
    #[error("type mismatch for {path}: expected {expected}, got {got}")]
    TypeMismatch {
        path: String,
        expected: String,
        got: String,
    },
    #[error("invalid range for {path}: {value} is not in [{min}, {max}]")]
    InvalidRange {
        path: String,
        value: f64,
        min: f64,
        max: f64,
    },
    #[error("invalid config invariant: {0}")]
    InvalidInvariant(String),
}

/// Interpret a `--settings` argument.
///
/// Heuristic: a value that starts with `{` (optionally after whitespace)
/// is inline JSON; anything else is treated as a filesystem path and
/// read. This matches the shape every operator actually produces —
/// `--settings '{...}'` or `--settings path/to/file.json`. A pathological
/// filename beginning with `{` is rejected by this rule on purpose: the
/// ambiguity is not worth resolving.
pub fn parse_settings_source(raw: &str) -> Result<String, OverlayError> {
    if raw.trim_start().starts_with('{') {
        Ok(raw.to_string())
    } else {
        std::fs::read_to_string(Path::new(raw)).map_err(|source| OverlayError::FileRead {
            path: raw.to_string(),
            source,
        })
    }
}

/// Apply a JSON overlay onto `base`. The JSON is deserialized as a
/// [`RuntimeConfig`] (every field defaults — see the 91 `#[serde(default)]`
/// attributes in `runtime_config.rs`), then merged via
/// `RuntimeConfig::merge` which only copies non-default fields. Net
/// effect: anything the operator omitted stays as-is; anything they set
/// wins. The one caveat — setting a field *to its default* looks like
/// "not set" to merge — is acceptable for `--settings`, whose typical
/// use is raising or lowering away from defaults.
pub fn apply_settings_json(base: RuntimeConfig, json: &str) -> Result<RuntimeConfig, OverlayError> {
    let overlay: RuntimeConfig = serde_json::from_str(json)?;
    Ok(base.merge(overlay))
}

// ─── B. effective budget ─────────────────────────────────────────────────

/// Resolve the input-token budget a turn will actually see for the
/// given model. Falls back to `config.token_budget.max_turn_input_tokens`
/// when the model is unknown or unspecified.
///
/// Mirrors [`RuntimeLimits::effective_max_turn_input_tokens`] but reads
/// the configured fallback from `RuntimeConfig` rather than the global
/// env-tuned singleton, because `/config` operates on a specific
/// loaded config, not on `RuntimeLimits::global()`.
pub fn effective_budget_for_model(config: &RuntimeConfig, model: Option<&str>) -> u64 {
    let configured = config.token_budget.max_turn_input_tokens as u64;
    if let Some(window) = model.and_then(context_window_for_model) {
        (window as f64 * 0.80) as u64
    } else {
        // Keep the local-limit fallback consistent with RuntimeLimits:
        // env can override the configured value, so consult it too.
        let env_limit = RuntimeLimits::global().max_turn_input_tokens;
        if env_limit > 0 {
            env_limit
        } else {
            configured
        }
    }
}

// ─── C. settings catalog + apply_edit ───────────────────────────────────

/// What kind of editor the UI should spawn for this knob.
#[derive(Debug, Clone, PartialEq)]
pub enum SettingKind {
    Bool,
    Number {
        min: f64,
        max: f64,
        allow_fraction: bool,
    },
    Enum {
        options: Vec<String>,
    },
}

/// One row in the `/config edit` list.
#[derive(Debug, Clone)]
pub struct SettingItem {
    pub id: String,
    pub label: String,
    pub kind: SettingKind,
    pub value: Value,
}

impl SettingItem {
    pub fn value_as_bool(&self) -> Option<bool> {
        self.value.as_bool()
    }
    pub fn value_as_number(&self) -> Option<f64> {
        self.value.as_f64()
    }
    pub fn value_as_string(&self) -> Option<String> {
        self.value.as_str().map(|s| s.to_string())
    }
}

/// The single source of truth for what `/config edit` can reach.
///
/// Adding a knob: push a new entry here AND handle the same `id` in
/// `apply_edit`. The `every_catalog_item_is_editable_via_apply_edit`
/// test will refuse to pass until both sides exist.
pub fn build_settings_catalog(config: &RuntimeConfig) -> Vec<SettingItem> {
    vec![
        // ── Token budget ──
        SettingItem {
            id: "token_budget.max_turn_input_tokens".to_string(),
            label: "Max turn input tokens".to_string(),
            kind: SettingKind::Number {
                min: 8_000.0,
                max: 2_000_000.0,
                allow_fraction: false,
            },
            value: Value::from(config.token_budget.max_turn_input_tokens),
        },
        SettingItem {
            id: "token_budget.system_prompt_reserve".to_string(),
            label: "System prompt reserve tokens".to_string(),
            kind: SettingKind::Number {
                min: 500.0,
                max: 32_000.0,
                allow_fraction: false,
            },
            value: Value::from(config.token_budget.system_prompt_reserve),
        },
        SettingItem {
            id: "token_budget.tools_reserve".to_string(),
            label: "Tools reserve tokens".to_string(),
            kind: SettingKind::Number {
                min: 1_000.0,
                max: 64_000.0,
                allow_fraction: false,
            },
            value: Value::from(config.token_budget.tools_reserve),
        },
        // ── Context window / adaptive ──
        SettingItem {
            id: "context_window.adaptive".to_string(),
            label: "Adaptive context-window tuning".to_string(),
            kind: SettingKind::Bool,
            value: Value::from(config.context_window.adaptive),
        },
        SettingItem {
            id: "context_window.adaptive_budget_reduction".to_string(),
            label: "Shrink budget under pressure (off = avoid spiral)".to_string(),
            kind: SettingKind::Bool,
            value: Value::from(config.context_window.adaptive_budget_reduction),
        },
        SettingItem {
            id: "context_window.dynamic_compression".to_string(),
            label: "Dynamic compression threshold".to_string(),
            kind: SettingKind::Bool,
            value: Value::from(config.context_window.dynamic_compression),
        },
        SettingItem {
            id: "context_window.compression_threshold_min".to_string(),
            label: "Compression threshold (min)".to_string(),
            kind: SettingKind::Number {
                min: 0.0,
                max: 1.0,
                allow_fraction: true,
            },
            value: Value::from(config.context_window.compression_threshold_min),
        },
        SettingItem {
            id: "context_window.compression_threshold_max".to_string(),
            label: "Compression threshold (max)".to_string(),
            kind: SettingKind::Number {
                min: 0.0,
                max: 1.0,
                allow_fraction: true,
            },
            value: Value::from(config.context_window.compression_threshold_max),
        },
        // ── Compression pipeline ──
        SettingItem {
            id: "compression.compression_threshold".to_string(),
            label: "Compression trigger fraction".to_string(),
            kind: SettingKind::Number {
                min: 0.0,
                max: 1.0,
                allow_fraction: true,
            },
            value: Value::from(config.compression.compression_threshold),
        },
        SettingItem {
            id: "compression.preserve_recent_turns".to_string(),
            label: "Preserve recent turns".to_string(),
            kind: SettingKind::Number {
                min: 1.0,
                max: 50.0,
                allow_fraction: false,
            },
            value: Value::from(config.compression.preserve_recent_turns),
        },
        SettingItem {
            id: "compression.preserve_tool_calls".to_string(),
            label: "Preserve tool calls during compaction".to_string(),
            kind: SettingKind::Bool,
            value: Value::from(config.compression.preserve_tool_calls),
        },
        // ── Memory retrieval ──
        SettingItem {
            id: "memory.retrieval_top_k".to_string(),
            label: "Memory retrieval top-k".to_string(),
            kind: SettingKind::Number {
                min: 1.0,
                max: 50.0,
                allow_fraction: false,
            },
            value: Value::from(config.memory.retrieval_top_k),
        },
        // ── Tool selection ──
        SettingItem {
            id: "tool_selection.max_tools".to_string(),
            label: "Max tools surfaced to the model".to_string(),
            kind: SettingKind::Number {
                min: 1.0,
                max: 200.0,
                allow_fraction: false,
            },
            value: Value::from(config.tool_selection.max_tools),
        },
        SettingItem {
            id: "tool_selection.prefer_recent_tools".to_string(),
            label: "Prefer recently-used tools".to_string(),
            kind: SettingKind::Bool,
            value: Value::from(config.tool_selection.prefer_recent_tools),
        },
        // ── Trace ──
        SettingItem {
            id: "trace.profile".to_string(),
            label: "Trace profile (production/dev/custom)".to_string(),
            kind: SettingKind::Enum {
                options: vec!["production".into(), "dev".into(), "custom".into()],
            },
            value: Value::from(format!("{:?}", config.trace.profile).to_lowercase()),
        },
        SettingItem {
            id: "trace.min_level".to_string(),
            label: "Minimum trace level (error/warn/info/debug/trace)".to_string(),
            kind: SettingKind::Enum {
                options: vec![
                    "error".into(),
                    "warn".into(),
                    "info".into(),
                    "debug".into(),
                    "trace".into(),
                ],
            },
            value: Value::from(format!("{:?}", config.trace.min_level).to_lowercase()),
        },
        SettingItem {
            id: "trace.tool_calls".to_string(),
            label: "Trace tool calls".to_string(),
            kind: SettingKind::Bool,
            value: Value::from(config.trace.category_enabled(TraceCategory::ToolCalls)),
        },
        SettingItem {
            id: "trace.llm_exchanges".to_string(),
            label: "Capture full LLM request/response payloads".to_string(),
            kind: SettingKind::Bool,
            value: Value::from(config.trace.category_enabled(TraceCategory::LlmExchanges)),
        },
        SettingItem {
            id: "trace.thinking".to_string(),
            label: "Trace LLM thinking/reasoning".to_string(),
            kind: SettingKind::Bool,
            value: Value::from(config.trace.category_enabled(TraceCategory::Thinking)),
        },
        // ── Runtime limits (per-turn agentic budget) ──
        SettingItem {
            id: "runtime_limits.max_turns".to_string(),
            label: "Max tool calls per user message (0 = inherit env / built-in 150)".to_string(),
            kind: SettingKind::Number {
                min: 0.0,
                max: 2000.0,
                allow_fraction: false,
            },
            value: Value::from(config.runtime_limits.max_turns),
        },
        SettingItem {
            id: "runtime_limits.plan_subtask_max_turns".to_string(),
            label: "Max tool calls per plan subtask (0 = fall back to max_turns)".to_string(),
            kind: SettingKind::Number {
                min: 0.0,
                max: 2000.0,
                allow_fraction: false,
            },
            value: Value::from(config.runtime_limits.plan_subtask_max_turns),
        },
    ]
}

/// Free-text filter over catalog items. Matches on `id` or `label`
/// substring, case-insensitive. Empty query returns the whole catalog.
pub fn filter_settings(items: &[SettingItem], query: &str) -> Vec<SettingItem> {
    if query.trim().is_empty() {
        return items.to_vec();
    }
    let needle = query.to_lowercase();
    items
        .iter()
        .filter(|i| {
            i.id.to_lowercase().contains(&needle) || i.label.to_lowercase().contains(&needle)
        })
        .cloned()
        .collect()
}

/// Add or remove a category from the vec.
fn toggle_category(cats: &mut Vec<TraceCategory>, cat: TraceCategory, enable: bool) {
    if enable {
        if !cats.contains(&cat) {
            cats.push(cat);
        }
    } else {
        cats.retain(|c| *c != cat);
    }
}

/// Add or remove a sink from the vec.
fn toggle_trace_sink(sinks: &mut Vec<TraceSink>, sink: TraceSink, enable: bool) {
    if enable {
        if !sinks.contains(&sink) {
            sinks.push(sink);
        }
    } else {
        sinks.retain(|s| *s != sink);
    }
}

/// Write `new_value` into the field identified by `id`.
///
/// Returns a new `RuntimeConfig` (the edit is value-level; we don't
/// mutate the caller's copy — the caller decides when to persist).
pub fn apply_edit(
    mut config: RuntimeConfig,
    id: &str,
    new_value: Value,
) -> Result<RuntimeConfig, OverlayError> {
    // Small helpers to keep the big match below readable.
    fn as_bool(v: &Value, path: &str) -> Result<bool, OverlayError> {
        v.as_bool().ok_or_else(|| OverlayError::TypeMismatch {
            path: path.to_string(),
            expected: "bool".to_string(),
            got: describe(v),
        })
    }
    fn as_u32(v: &Value, path: &str) -> Result<u32, OverlayError> {
        // Accept integer-valued floats so the UI can round-trip values
        // it read via `value_as_number()` (serde_json::Value only carries
        // one numeric type once a `.` appears). Reject actual fractional
        // values so a 500.5 doesn't silently round.
        let u = v.as_u64().or_else(|| {
            v.as_f64().and_then(|f| {
                if f.is_finite() && f >= 0.0 && f.fract() == 0.0 {
                    Some(f as u64)
                } else {
                    None
                }
            })
        });
        u.and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| OverlayError::TypeMismatch {
                path: path.to_string(),
                expected: "u32".to_string(),
                got: describe(v),
            })
    }
    fn as_f64(v: &Value, path: &str) -> Result<f64, OverlayError> {
        v.as_f64().ok_or_else(|| OverlayError::TypeMismatch {
            path: path.to_string(),
            expected: "f64".to_string(),
            got: describe(v),
        })
    }
    fn ensure_range(value: f64, min: f64, max: f64, path: &str) -> Result<(), OverlayError> {
        if !value.is_finite() || value < min || value > max {
            return Err(OverlayError::InvalidRange {
                path: path.to_string(),
                value,
                min,
                max,
            });
        }
        Ok(())
    }
    fn ensure_threshold_order(min: f64, max: f64) -> Result<(), OverlayError> {
        if min > max {
            return Err(OverlayError::InvalidInvariant(format!(
                "context_window.compression_threshold_min ({min}) must be <= context_window.compression_threshold_max ({max})"
            )));
        }
        Ok(())
    }
    fn describe(v: &Value) -> String {
        match v {
            Value::Null => "null".into(),
            Value::Bool(_) => "bool".into(),
            Value::Number(_) => "number".into(),
            Value::String(_) => "string".into(),
            Value::Array(_) => "array".into(),
            Value::Object(_) => "object".into(),
        }
    }

    match id {
        "token_budget.max_turn_input_tokens" => {
            let n = as_u32(&new_value, id)?;
            ensure_range(n as f64, 8_000.0, 2_000_000.0, id)?;
            config.token_budget.max_turn_input_tokens = n;
        }
        "token_budget.system_prompt_reserve" => {
            let n = as_u32(&new_value, id)?;
            ensure_range(n as f64, 500.0, 32_000.0, id)?;
            config.token_budget.system_prompt_reserve = n;
        }
        "token_budget.tools_reserve" => {
            let n = as_u32(&new_value, id)?;
            ensure_range(n as f64, 1_000.0, 64_000.0, id)?;
            config.token_budget.tools_reserve = n;
        }
        "context_window.adaptive" => {
            config.context_window.adaptive = as_bool(&new_value, id)?;
        }
        "context_window.adaptive_budget_reduction" => {
            config.context_window.adaptive_budget_reduction = as_bool(&new_value, id)?;
        }
        "context_window.dynamic_compression" => {
            config.context_window.dynamic_compression = as_bool(&new_value, id)?;
        }
        "context_window.compression_threshold_min" => {
            let n = as_f64(&new_value, id)?;
            ensure_range(n, 0.0, 1.0, id)?;
            ensure_threshold_order(n, config.context_window.compression_threshold_max)?;
            config.context_window.compression_threshold_min = n;
        }
        "context_window.compression_threshold_max" => {
            let n = as_f64(&new_value, id)?;
            ensure_range(n, 0.0, 1.0, id)?;
            ensure_threshold_order(config.context_window.compression_threshold_min, n)?;
            config.context_window.compression_threshold_max = n;
        }
        "compression.compression_threshold" => {
            let n = as_f64(&new_value, id)?;
            ensure_range(n, 0.0, 1.0, id)?;
            config.compression.compression_threshold = n;
        }
        "compression.preserve_recent_turns" => {
            let n = as_u32(&new_value, id)?;
            ensure_range(n as f64, 1.0, 50.0, id)?;
            config.compression.preserve_recent_turns = n;
        }
        "compression.preserve_tool_calls" => {
            config.compression.preserve_tool_calls = as_bool(&new_value, id)?;
        }
        "memory.retrieval_top_k" => {
            let n = as_u32(&new_value, id)?;
            ensure_range(n as f64, 1.0, 50.0, id)?;
            config.memory.retrieval_top_k = n;
        }
        "tool_selection.max_tools" => {
            let n = as_u32(&new_value, id)?;
            ensure_range(n as f64, 1.0, 200.0, id)?;
            config.tool_selection.max_tools = n;
        }
        "tool_selection.prefer_recent_tools" => {
            config.tool_selection.prefer_recent_tools = as_bool(&new_value, id)?;
        }
        "trace.profile" => {
            if let Some(s) = new_value.as_str() {
                let profile = match s {
                    "production" => TraceProfile::Production,
                    "dev" => TraceProfile::Dev,
                    _ => TraceProfile::Custom,
                };
                // Re-apply full profile effects (min_level, categories, sinks)
                config.trace = std::mem::take(&mut config.trace).apply_profile(profile);
            }
        }
        "trace.min_level" => {
            if let Some(s) = new_value.as_str() {
                config.trace.min_level = match s {
                    "error" => TraceLevelSerde::Error,
                    "warn" => TraceLevelSerde::Warn,
                    "info" => TraceLevelSerde::Info,
                    "debug" => TraceLevelSerde::Debug,
                    "trace" => TraceLevelSerde::Trace,
                    _ => return Ok(config),
                };
            }
        }
        "trace.tool_calls" => {
            toggle_category(
                &mut config.trace.enabled_categories,
                TraceCategory::ToolCalls,
                as_bool(&new_value, id)?,
            );
            return Ok(config);
        }
        "trace.llm_exchanges" => {
            toggle_category(
                &mut config.trace.enabled_categories,
                TraceCategory::LlmExchanges,
                as_bool(&new_value, id)?,
            );
            return Ok(config);
        }
        "trace.thinking" => {
            toggle_category(
                &mut config.trace.enabled_categories,
                TraceCategory::Thinking,
                as_bool(&new_value, id)?,
            );
            return Ok(config);
        }
        "trace.context_assembly" => {
            toggle_category(
                &mut config.trace.enabled_categories,
                TraceCategory::ContextAssembly,
                as_bool(&new_value, id)?,
            );
            return Ok(config);
        }
        "trace.decision_explain" => {
            toggle_category(
                &mut config.trace.enabled_categories,
                TraceCategory::DecisionExplain,
                as_bool(&new_value, id)?,
            );
            return Ok(config);
        }
        "trace.phase_transition" => {
            toggle_category(
                &mut config.trace.enabled_categories,
                TraceCategory::PhaseTransition,
                as_bool(&new_value, id)?,
            );
            return Ok(config);
        }
        "trace.budget" => {
            toggle_category(
                &mut config.trace.enabled_categories,
                TraceCategory::Budget,
                as_bool(&new_value, id)?,
            );
            return Ok(config);
        }
        "trace.reflection" => {
            toggle_category(
                &mut config.trace.enabled_categories,
                TraceCategory::Reflection,
                as_bool(&new_value, id)?,
            );
            return Ok(config);
        }
        "trace.verification" => {
            toggle_category(
                &mut config.trace.enabled_categories,
                TraceCategory::Verification,
                as_bool(&new_value, id)?,
            );
            return Ok(config);
        }
        "trace.memory_retrieval" => {
            toggle_category(
                &mut config.trace.enabled_categories,
                TraceCategory::MemoryRetrieval,
                as_bool(&new_value, id)?,
            );
            return Ok(config);
        }
        "trace.skill_execution" => {
            toggle_category(
                &mut config.trace.enabled_categories,
                TraceCategory::SkillExecution,
                as_bool(&new_value, id)?,
            );
            return Ok(config);
        }
        "trace.prompt_assembly" => {
            toggle_category(
                &mut config.trace.enabled_categories,
                TraceCategory::PromptAssembly,
                as_bool(&new_value, id)?,
            );
            return Ok(config);
        }
        "trace.guard_evaluation" => {
            toggle_category(
                &mut config.trace.enabled_categories,
                TraceCategory::GuardEvaluation,
                as_bool(&new_value, id)?,
            );
            return Ok(config);
        }
        "trace.sampling_rate" => {
            let n = as_f64(&new_value, id)?;
            ensure_range(n, 0.0, 1.0, id)?;
            config.trace.sampling_rate = n;
        }
        "trace.sinks.journal" => {
            toggle_trace_sink(
                &mut config.trace.sinks,
                TraceSink::Journal,
                as_bool(&new_value, id)?,
            );
            return Ok(config);
        }
        "trace.sinks.stderr" => {
            toggle_trace_sink(
                &mut config.trace.sinks,
                TraceSink::Stderr,
                as_bool(&new_value, id)?,
            );
            return Ok(config);
        }
        "runtime_limits.max_turns" => {
            let n = as_u32(&new_value, id)?;
            ensure_range(n as f64, 0.0, 2000.0, id)?;
            config.runtime_limits.max_turns = n;
        }
        "runtime_limits.plan_subtask_max_turns" => {
            let n = as_u32(&new_value, id)?;
            ensure_range(n as f64, 0.0, 2000.0, id)?;
            config.runtime_limits.plan_subtask_max_turns = n;
        }
        unknown => return Err(OverlayError::UnknownPath(unknown.to_string())),
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_includes_llm_exchanges_toggle() {
        let config = RuntimeConfig::default();
        let catalog = build_settings_catalog(&config);
        let item = catalog
            .iter()
            .find(|item| item.id == "trace.llm_exchanges")
            .expect("catalog must expose the LLM exchanges trace toggle");
        assert_eq!(item.label, "Capture full LLM request/response payloads");
        assert_eq!(item.value, Value::Bool(false));
    }

    #[test]
    fn apply_edit_updates_llm_exchanges_toggle() {
        let updated = apply_edit(
            RuntimeConfig::default(),
            "trace.llm_exchanges",
            Value::Bool(true),
        )
        .expect("toggle edit should succeed");
        assert!(updated
            .trace
            .enabled_categories
            .contains(&TraceCategory::LlmExchanges));
    }
}

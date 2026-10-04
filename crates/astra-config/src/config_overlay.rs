//! Config overlay + edit surface.
//!
//! Two responsibilities, one module — they all operate on
//! [`RuntimeConfig`] and preserve explicitly supplied values:
//!
//! 1. `RuntimeConfigLayer` — validated JSON/TOML fields over a base config.
//!    Backs the `--settings <JSON-or-path>` CLI flag. Partial means any
//!    field omitted from the JSON keeps its base value; this matches
//!    operator intent when the flag is used as a one-shot override
//!    ("just adjust memory retrieval for this one invocation").
//!
//! 2. `build_settings_catalog` + `filter_settings` + `apply_edit` —
//!    the pure-model layer behind an interactive `/config edit` UI.
//!    Catalog mirrors the reference implementation's Config.tsx model:
//!    flat list of { id, label, kind, value } items, each pointing at a
//!    single field in `RuntimeConfig`. The UI dispatches per `kind`, the
//!    write-back goes through `apply_edit`, the two ends close a loop
//!    that's regression-guarded by `every_catalog_item_is_editable_via_apply_edit`.

use crate::runtime_config::{
    ExplainReportFormat, RuntimeConfig, TraceCategory, TraceLevel, TraceProfile, TraceSink,
};
use serde_json::Value;
use std::path::Path;

// ─── A. --settings overlay ───────────────────────────────────────────────

/// Errors produced by the overlay / edit surface.
#[derive(Debug, thiserror::Error)]
pub enum OverlayError {
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid TOML: {0}")]
    Toml(#[from] toml::de::Error),
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

/// A validated configuration layer that retains which fields were supplied.
/// Complete session snapshots are RuntimeConfig values, not layers.
#[derive(Debug, Clone)]
pub struct RuntimeConfigLayer(Value);

impl Default for RuntimeConfigLayer {
    fn default() -> Self {
        Self(serde_json::json!({}))
    }
}

impl RuntimeConfigLayer {
    pub fn from_json(json: &str) -> Result<Self, OverlayError> {
        // Typed parsing also rejects duplicate declared fields before Value
        // parsing could collapse them.
        serde_json::from_str::<RuntimeConfig>(json)?;
        let fields: serde_json::Map<String, Value> = serde_json::from_str(json)?;
        Ok(Self(Value::Object(fields)))
    }

    pub fn from_toml(source: &str) -> Result<Self, OverlayError> {
        let config: RuntimeConfig = toml::from_str(source)?;
        let mut value: toml::Value = toml::from_str(source)?;
        // TOML supports non-finite floats, JSON does not. Preserve the existing
        // trace normalization before conversion instead of turning them into null.
        if let Some(rate) = value
            .get_mut("trace")
            .and_then(|trace| trace.get_mut("sampling_rate"))
        {
            *rate = toml::Value::Float(config.trace.normalize().sampling_rate);
        }
        Self::from_value(serde_json::to_value(value)?)
    }

    fn from_value(value: Value) -> Result<Self, OverlayError> {
        if !value.is_object() {
            return Err(OverlayError::InvalidInvariant(
                "configuration layer must be an object".into(),
            ));
        }
        // Validate the layer by itself, including complete routing policy pairs.
        // Keep its original fields rather than the deserializer's default values.
        serde_json::from_value::<RuntimeConfig>(value.clone())?;
        Ok(Self(value))
    }

    pub fn apply_to(&self, base: &RuntimeConfig) -> Result<RuntimeConfig, OverlayError> {
        let mut value = serde_json::to_value(base)?;
        merge_config_fields(&mut value, &self.0);
        Ok(serde_json::from_value(value)?)
    }

    /// Apply explicit CLI trace selections after settings, without overriding
    /// lower-layer trace fields absent from both settings and the flags.
    pub fn with_trace_cli_overrides(
        mut self,
        profile: Option<&str>,
        level: Option<&str>,
        categories: Option<&str>,
    ) -> Result<Self, OverlayError> {
        if profile.is_none() && level.is_none() && categories.is_none() {
            return Ok(self);
        }
        let trace = self
            .apply_to(&RuntimeConfig::default())?
            .trace
            .with_cli_overrides(profile, level, categories)
            .map_err(OverlayError::InvalidInvariant)?;
        let trace = serde_json::to_value(trace)?;
        let patch = if matches!(profile, Some("production" | "dev")) {
            trace
        } else {
            let mut patch = serde_json::json!({"profile": trace["profile"]});
            if level.is_some() {
                patch["min_level"] = trace["min_level"].clone();
            }
            if categories.is_some() {
                patch["enabled_categories"] = trace["enabled_categories"].clone();
            }
            patch
        };
        merge_config_fields(&mut self.0, &serde_json::json!({"trace": patch}));
        Self::from_value(self.0)
    }
}

fn merge_config_fields(base: &mut Value, layer: &Value) {
    match (base, layer) {
        (Value::Object(base), Value::Object(layer)) => {
            for (key, value) in layer {
                merge_config_fields(base.entry(key.clone()).or_insert(Value::Null), value);
            }
        }
        (base, layer) => *base = layer.clone(),
    }
}

// ─── B. settings catalog + apply_edit ───────────────────────────────────

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
        SettingItem {
            id: "trace.harness_snapshots".to_string(),
            label: "Persist harness snapshot diagnostics".to_string(),
            kind: SettingKind::Bool,
            value: Value::from(
                config
                    .trace
                    .category_enabled(TraceCategory::HarnessSnapshots),
            ),
        },
        // ── Explain Analyze ──
        SettingItem {
            id: "explain.live_rows".to_string(),
            label: "Live Explain Analyze rows (1–5)".to_string(),
            kind: SettingKind::Number {
                min: 1.0,
                max: 5.0,
                allow_fraction: false,
            },
            value: Value::from(config.explain.effective_live_rows()),
        },
        SettingItem {
            id: "explain.report_format".to_string(),
            label: "Explain Analyze report format".to_string(),
            kind: SettingKind::Enum {
                options: vec!["html".into(), "markdown".into(), "text".into()],
            },
            value: Value::from(config.explain.effective_report_format().as_str()),
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
    if cats.contains(&TraceCategory::All) {
        *cats = TraceCategory::individual_categories().to_vec();
    }
    if enable {
        if !cats.contains(&cat) {
            cats.push(cat);
        }
    } else {
        cats.retain(|c| *c != cat);
    }
    cats.sort();
    cats.dedup();
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
    fn mark_trace_custom(config: &mut RuntimeConfig) {
        config.trace.profile = TraceProfile::Custom;
        config.trace = std::mem::take(&mut config.trace).normalize();
    }

    match id {
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
                    "error" => TraceLevel::Error,
                    "warn" => TraceLevel::Warn,
                    "info" => TraceLevel::Info,
                    "debug" => TraceLevel::Debug,
                    "trace" => TraceLevel::Trace,
                    _ => return Ok(config),
                };
                mark_trace_custom(&mut config);
            }
        }
        "trace.tool_calls" => {
            toggle_category(
                &mut config.trace.enabled_categories,
                TraceCategory::ToolCalls,
                as_bool(&new_value, id)?,
            );
            mark_trace_custom(&mut config);
            return Ok(config);
        }
        "trace.llm_exchanges" => {
            toggle_category(
                &mut config.trace.enabled_categories,
                TraceCategory::LlmExchanges,
                as_bool(&new_value, id)?,
            );
            mark_trace_custom(&mut config);
            return Ok(config);
        }
        "trace.thinking" => {
            toggle_category(
                &mut config.trace.enabled_categories,
                TraceCategory::Thinking,
                as_bool(&new_value, id)?,
            );
            mark_trace_custom(&mut config);
            return Ok(config);
        }
        "trace.context_assembly" => {
            toggle_category(
                &mut config.trace.enabled_categories,
                TraceCategory::ContextAssembly,
                as_bool(&new_value, id)?,
            );
            mark_trace_custom(&mut config);
            return Ok(config);
        }
        "trace.decision_explain" => {
            toggle_category(
                &mut config.trace.enabled_categories,
                TraceCategory::DecisionExplain,
                as_bool(&new_value, id)?,
            );
            mark_trace_custom(&mut config);
            return Ok(config);
        }
        "trace.phase_transition" => {
            toggle_category(
                &mut config.trace.enabled_categories,
                TraceCategory::PhaseTransition,
                as_bool(&new_value, id)?,
            );
            mark_trace_custom(&mut config);
            return Ok(config);
        }
        "trace.budget" => {
            toggle_category(
                &mut config.trace.enabled_categories,
                TraceCategory::Budget,
                as_bool(&new_value, id)?,
            );
            mark_trace_custom(&mut config);
            return Ok(config);
        }
        "trace.reflection" => {
            toggle_category(
                &mut config.trace.enabled_categories,
                TraceCategory::Reflection,
                as_bool(&new_value, id)?,
            );
            mark_trace_custom(&mut config);
            return Ok(config);
        }
        "trace.verification" => {
            toggle_category(
                &mut config.trace.enabled_categories,
                TraceCategory::Verification,
                as_bool(&new_value, id)?,
            );
            mark_trace_custom(&mut config);
            return Ok(config);
        }
        "trace.memory_retrieval" => {
            toggle_category(
                &mut config.trace.enabled_categories,
                TraceCategory::MemoryRetrieval,
                as_bool(&new_value, id)?,
            );
            mark_trace_custom(&mut config);
            return Ok(config);
        }
        "trace.skill_execution" => {
            toggle_category(
                &mut config.trace.enabled_categories,
                TraceCategory::SkillExecution,
                as_bool(&new_value, id)?,
            );
            mark_trace_custom(&mut config);
            return Ok(config);
        }
        "trace.harness_snapshots" => {
            toggle_category(
                &mut config.trace.enabled_categories,
                TraceCategory::HarnessSnapshots,
                as_bool(&new_value, id)?,
            );
            mark_trace_custom(&mut config);
            return Ok(config);
        }
        "trace.prompt_assembly" => {
            toggle_category(
                &mut config.trace.enabled_categories,
                TraceCategory::PromptAssembly,
                as_bool(&new_value, id)?,
            );
            mark_trace_custom(&mut config);
            return Ok(config);
        }
        "trace.guard_evaluation" => {
            toggle_category(
                &mut config.trace.enabled_categories,
                TraceCategory::GuardEvaluation,
                as_bool(&new_value, id)?,
            );
            mark_trace_custom(&mut config);
            return Ok(config);
        }
        "trace.sampling_rate" => {
            let n = as_f64(&new_value, id)?;
            ensure_range(n, 0.0, 1.0, id)?;
            config.trace.sampling_rate = n;
            mark_trace_custom(&mut config);
        }
        "trace.sinks.journal" => {
            toggle_trace_sink(
                &mut config.trace.sinks,
                TraceSink::Journal,
                as_bool(&new_value, id)?,
            );
            mark_trace_custom(&mut config);
            return Ok(config);
        }
        "trace.sinks.stderr" => {
            toggle_trace_sink(
                &mut config.trace.sinks,
                TraceSink::Stderr,
                as_bool(&new_value, id)?,
            );
            mark_trace_custom(&mut config);
            return Ok(config);
        }
        "explain.live_rows" => {
            let n = as_u32(&new_value, id)?;
            ensure_range(n as f64, 1.0, 5.0, id)?;
            config.explain.live_rows = Some(n as u8);
        }
        "explain.report_format" => {
            let value = new_value
                .as_str()
                .ok_or_else(|| OverlayError::TypeMismatch {
                    path: id.to_string(),
                    expected: "string".to_string(),
                    got: describe(&new_value),
                })?;
            config.explain.report_format =
                Some(ExplainReportFormat::parse(value).map_err(OverlayError::InvalidInvariant)?);
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
    fn catalog_includes_harness_snapshots_toggle() {
        let config = RuntimeConfig::default();
        let catalog = build_settings_catalog(&config);
        let item = catalog
            .iter()
            .find(|item| item.id == "trace.harness_snapshots")
            .expect("catalog must expose the harness snapshots trace toggle");
        assert_eq!(item.label, "Persist harness snapshot diagnostics");
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
        assert!(
            updated
                .trace
                .enabled_categories
                .contains(&TraceCategory::LlmExchanges)
        );
    }

    #[test]
    fn apply_edit_updates_harness_snapshots_toggle() {
        let updated = apply_edit(
            RuntimeConfig::default(),
            "trace.harness_snapshots",
            Value::Bool(true),
        )
        .expect("toggle edit should succeed");
        assert!(
            updated
                .trace
                .enabled_categories
                .contains(&TraceCategory::HarnessSnapshots)
        );
    }

    #[test]
    fn explain_live_rows_is_catalogued_and_bounded() {
        let config = RuntimeConfig::default();
        let item = build_settings_catalog(&config)
            .into_iter()
            .find(|item| item.id == "explain.live_rows")
            .expect("catalog must expose live Explain Analyze rows");
        assert_eq!(item.value, Value::from(5));
        let updated = apply_edit(config, "explain.live_rows", Value::from(3))
            .expect("valid Explain Analyze row count should apply");
        assert_eq!(updated.explain.live_rows, Some(3));
        assert!(apply_edit(updated, "explain.live_rows", Value::from(6)).is_err());
    }

    #[test]
    fn explain_report_format_is_catalogued_and_editable() {
        let config = RuntimeConfig::default();
        let item = build_settings_catalog(&config)
            .into_iter()
            .find(|item| item.id == "explain.report_format")
            .expect("catalog must expose Explain Analyze report format");
        assert_eq!(item.value, Value::from("html"));
        assert_eq!(
            item.kind,
            SettingKind::Enum {
                options: vec!["html".into(), "markdown".into(), "text".into()]
            }
        );
        let updated = apply_edit(config, "explain.report_format", Value::from("markdown"))
            .expect("valid Explain Analyze report format should apply");
        assert_eq!(
            updated.explain.effective_report_format(),
            ExplainReportFormat::Markdown
        );
        assert!(apply_edit(updated, "explain.report_format", Value::from("pdf")).is_err());
        assert!(
            apply_edit(
                RuntimeConfig::default(),
                "explain.report_format",
                Value::Bool(true)
            )
            .is_err()
        );
    }

    #[test]
    fn apply_edit_on_trace_toggle_breaks_out_of_preset_profile() {
        let config = RuntimeConfig {
            trace: RuntimeConfig::default()
                .trace
                .apply_profile(TraceProfile::Dev),
            ..RuntimeConfig::default()
        };
        let updated = apply_edit(config, "trace.llm_exchanges", Value::Bool(false))
            .expect("toggle edit should succeed");
        assert_eq!(updated.trace.profile, TraceProfile::Custom);
        assert!(
            !updated
                .trace
                .enabled_categories
                .contains(&TraceCategory::LlmExchanges)
        );
    }
}

#[cfg(test)]
mod layer_tests {
    use super::*;

    #[test]
    fn invalid_layers_are_rejected_before_they_can_override_configuration() {
        for source in [
            "[]",
            "null",
            "1",
            r#""config""#,
            r#"{"memory":{"retrieval_top_k":5},"memory":{"retrieval_top_k":7}}"#,
            r#"{"memory":{"retrieval_top_k":null}}"#,
            r#"{"memory":{"retrieval_top_k":"5"}}"#,
            r#"{"model_routing":{"revision":"incomplete"}}"#,
            r#"{"safety":{"trust_mode":"unsafe"}}"#,
        ] {
            assert!(RuntimeConfigLayer::from_json(source).is_err(), "{source}");
        }
    }

    #[test]
    fn toml_trace_normalization_does_not_expand_profile_presets() {
        for rate in ["nan", "inf", "-inf"] {
            let source = format!("[trace]\nprofile = 'dev'\nsampling_rate = {rate}\n");
            let selected = RuntimeConfigLayer::from_toml(&source)
                .unwrap()
                .apply_to(&RuntimeConfig::default())
                .unwrap();
            assert_eq!(selected.trace.sampling_rate, 1.0);
            assert_eq!(selected.trace.profile, TraceProfile::Dev);
            assert_eq!(
                selected.trace.enabled_categories,
                RuntimeConfig::default().trace.enabled_categories
            );
        }
    }

    #[test]
    fn trace_flags_override_settings_without_filling_omitted_fields() {
        let base = RuntimeConfig {
            trace: crate::runtime_config::SessionTraceConfig::default()
                .apply_profile(TraceProfile::Dev),
            ..RuntimeConfig::default()
        };
        let layer =
            RuntimeConfigLayer::from_json(r#"{"runtime_limits":{"max_turns":17}}"#).unwrap();
        let unchanged = layer
            .clone()
            .with_trace_cli_overrides(None, None, None)
            .unwrap()
            .apply_to(&base)
            .unwrap();
        assert_eq!(unchanged.trace, base.trace);
        let selected = layer
            .with_trace_cli_overrides(None, Some("info"), None)
            .unwrap()
            .apply_to(&base)
            .unwrap();
        assert_eq!(selected.runtime_limits.max_turns, 17);
        assert_eq!(selected.trace.min_level, TraceLevel::Info);
        assert_eq!(selected.trace.profile, TraceProfile::Custom);
        assert_eq!(
            selected.trace.enabled_categories,
            base.trace.enabled_categories
        );
        assert_eq!(selected.trace.sinks, base.trace.sinks);
        let cleared =
            RuntimeConfigLayer::from_json(r#"{"trace":{"enabled_categories":[],"sinks":[]}}"#)
                .unwrap()
                .apply_to(&base)
                .unwrap();
        assert!(cleared.trace.enabled_categories.is_empty());
        assert!(cleared.trace.sinks.is_empty());
    }
}

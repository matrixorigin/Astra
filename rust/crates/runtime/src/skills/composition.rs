//! Skill composition — context tracking, schema validation, and depth/timeout enforcement.
//!
//! When skills invoke other skills (nested composition), the [`CompositionContext`]
//! tracks nesting depth, timeout budgets, side effects, and the parent chain.
//! Schema validation functions verify inputs against `input_schema` before execution
//! and optionally validate outputs against `output_schema` post-execution.

use serde_json::Value;
use std::time::{Duration, Instant};

/// Maximum nesting depth for composed skill calls.
pub const MAX_COMPOSITION_DEPTH: u32 = 3;

/// Tracks the composition chain during nested skill execution.
#[derive(Clone, Debug)]
pub struct CompositionContext {
    /// Current nesting depth (0 = top-level, direct user invocation).
    pub depth: u32,
    /// Maximum allowed depth before rejecting further nesting.
    pub max_depth: u32,
    /// Name of the parent skill that initiated this call (None at top level).
    pub parent_skill: Option<String>,
    /// Accumulated side effects from this skill and all children.
    pub side_effects: Vec<String>,
    /// When composition started (for timeout enforcement).
    pub start_time: Instant,
    /// Remaining timeout budget in seconds (None = no limit).
    pub timeout_secs: Option<u64>,
}

impl CompositionContext {
    /// Create a root context for a direct user invocation.
    ///
    /// Depth is 0 (not nested), no parent, no timeout.
    pub fn root() -> Self {
        Self {
            depth: 0,
            max_depth: MAX_COMPOSITION_DEPTH,
            parent_skill: None,
            side_effects: Vec::new(),
            start_time: Instant::now(),
            timeout_secs: None,
        }
    }

    /// Create a root context with a custom depth limit.
    ///
    /// Used when a skill declares `max_depth` in its composition metadata.
    pub fn root_with_max_depth(max_depth: u32) -> Self {
        Self {
            depth: 0,
            max_depth,
            parent_skill: None,
            side_effects: Vec::new(),
            start_time: Instant::now(),
            timeout_secs: None,
        }
    }

    /// Create a child context for a nested skill invocation.
    ///
    /// Inherits the parent's timeout budget (minus elapsed time) and increments depth.
    pub fn child(&self, parent_name: &str, child_timeout_secs: Option<u32>) -> Self {
        let remaining = self.remaining_timeout();
        // Take the minimum of: parent's remaining budget, child's declared timeout
        let effective_timeout = match (remaining, child_timeout_secs) {
            (Some(parent_rem), Some(child_max)) => Some(parent_rem.as_secs().min(child_max as u64)),
            (Some(parent_rem), None) => Some(parent_rem.as_secs()),
            (None, Some(child_max)) => Some(child_max as u64),
            (None, None) => None,
        };

        Self {
            depth: self.depth + 1,
            max_depth: self.max_depth,
            parent_skill: Some(parent_name.to_string()),
            side_effects: self.side_effects.clone(),
            start_time: Instant::now(),
            timeout_secs: effective_timeout,
        }
    }

    /// Check if the current depth is within the allowed limit.
    pub fn check_depth(&self) -> Result<(), CompositionError> {
        if self.depth >= self.max_depth {
            Err(CompositionError::MaxDepthExceeded {
                depth: self.depth,
                max: self.max_depth,
            })
        } else {
            Ok(())
        }
    }

    /// Check if the timeout has expired.
    pub fn check_timeout(&self) -> Result<(), CompositionError> {
        if let Some(timeout) = self.timeout_secs {
            let elapsed = self.start_time.elapsed().as_secs();
            if elapsed >= timeout {
                return Err(CompositionError::Timeout {
                    elapsed_secs: elapsed,
                    limit_secs: timeout,
                });
            }
        }
        Ok(())
    }

    /// Get the remaining timeout as a Duration (for tokio::time::timeout).
    pub fn remaining_timeout(&self) -> Option<Duration> {
        self.timeout_secs.map(|limit| {
            let elapsed = self.start_time.elapsed().as_secs();
            if elapsed >= limit {
                Duration::ZERO
            } else {
                Duration::from_secs(limit - elapsed)
            }
        })
    }

    /// Whether we are in a nested skill context (depth > 0).
    pub fn is_nested(&self) -> bool {
        self.depth > 0
    }

    /// Record side effects from a child skill execution.
    pub fn record_side_effects(&mut self, effects: &[String]) {
        for e in effects {
            if !self.side_effects.contains(e) {
                self.side_effects.push(e.clone());
            }
        }
    }
}

/// Errors during composition enforcement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompositionError {
    /// Nesting depth exceeded the maximum.
    MaxDepthExceeded { depth: u32, max: u32 },
    /// Execution exceeded the timeout budget.
    Timeout { elapsed_secs: u64, limit_secs: u64 },
    /// Skill is not marked as composable.
    NotComposable { skill_name: String },
    /// Input schema validation failed.
    InputValidation { errors: Vec<String> },
}

impl std::fmt::Display for CompositionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MaxDepthExceeded { depth, max } => {
                write!(f, "composition depth {depth} exceeds maximum {max}")
            }
            Self::Timeout {
                elapsed_secs,
                limit_secs,
            } => {
                write!(
                    f,
                    "composition timed out ({elapsed_secs}s elapsed, {limit_secs}s limit)"
                )
            }
            Self::NotComposable { skill_name } => {
                write!(
                    f,
                    "skill '{skill_name}' is not composable (set composable: true in manifest)"
                )
            }
            Self::InputValidation { errors } => {
                write!(f, "input validation failed: {}", errors.join("; "))
            }
        }
    }
}

impl std::error::Error for CompositionError {}

// ── Schema validation ────────────────────────────────────────────────────────

/// Validate arguments against a JSON Schema (subset).
///
/// Checks:
/// - `required` fields are present
/// - `type` matches (string, integer, number, boolean, array, object)
/// - `enum` constraints (value must be one of the listed options)
///
/// Returns a list of validation errors (empty = valid).
pub fn validate_input(schema: &Value, args: &Value) -> Vec<String> {
    let mut errors = Vec::new();

    let props = schema.get("properties").and_then(Value::as_object);
    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .unwrap_or_default();

    // Check required fields
    for field in &required {
        if args.get(*field).is_none() || args.get(*field) == Some(&Value::Null) {
            errors.push(format!("missing required field: '{field}'"));
        }
    }

    // Check type constraints for present fields
    if let (Some(props), Some(args_obj)) = (props, args.as_object()) {
        for (key, val) in args_obj {
            if let Some(prop_schema) = props.get(key) {
                if let Some(expected_type) = prop_schema.get("type").and_then(Value::as_str) {
                    if !type_matches(val, expected_type) {
                        errors.push(format!(
                            "field '{key}': expected type '{expected_type}', got {}",
                            json_type_name(val)
                        ));
                    }
                }

                // Check enum constraint
                if let Some(allowed) = prop_schema.get("enum").and_then(Value::as_array) {
                    if !allowed.contains(val) {
                        errors.push(format!(
                            "field '{key}': value not in allowed set {:?}",
                            allowed
                        ));
                    }
                }
            }
        }
    }

    errors
}

/// Validate output against a JSON Schema (advisory — returns warnings, not errors).
///
/// Attempts to parse `output` as JSON; if it's not valid JSON, returns a single warning.
/// If it is JSON, validates top-level type/required constraints.
pub fn validate_output(schema: &Value, output: &str) -> Vec<String> {
    let parsed: Value = match serde_json::from_str(output) {
        Ok(v) => v,
        Err(_) => return vec!["output is not valid JSON".to_string()],
    };
    validate_input(schema, &parsed)
}

fn type_matches(val: &Value, expected: &str) -> bool {
    match expected {
        "string" => val.is_string(),
        "integer" => val.is_i64() || val.is_u64(),
        "number" => val.is_number(),
        "boolean" => val.is_boolean(),
        "array" => val.is_array(),
        "object" => val.is_object(),
        "null" => val.is_null(),
        _ => true, // unknown type → pass
    }
}

fn json_type_name(val: &Value) -> &'static str {
    match val {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn root_context_is_not_nested() {
        let ctx = CompositionContext::root();
        assert_eq!(ctx.depth, 0);
        assert!(!ctx.is_nested());
        assert!(ctx.parent_skill.is_none());
    }

    #[test]
    fn child_context_increments_depth() {
        let root = CompositionContext::root();
        let child = root.child("parent-skill", None);
        assert_eq!(child.depth, 1);
        assert!(child.is_nested());
        assert_eq!(child.parent_skill.as_deref(), Some("parent-skill"));
    }

    #[test]
    fn depth_check_at_limit() {
        let mut ctx = CompositionContext::root();
        ctx.depth = MAX_COMPOSITION_DEPTH;
        assert!(ctx.check_depth().is_err());

        ctx.depth = MAX_COMPOSITION_DEPTH - 1;
        assert!(ctx.check_depth().is_ok());
    }

    #[test]
    fn timeout_check_expired() {
        let ctx = CompositionContext {
            depth: 0,
            max_depth: MAX_COMPOSITION_DEPTH,
            parent_skill: None,
            side_effects: Vec::new(),
            start_time: Instant::now() - Duration::from_secs(100),
            timeout_secs: Some(10),
        };
        assert!(ctx.check_timeout().is_err());
    }

    #[test]
    fn timeout_check_no_limit() {
        let ctx = CompositionContext::root();
        assert!(ctx.check_timeout().is_ok());
    }

    #[test]
    fn child_inherits_min_timeout() {
        let mut root = CompositionContext::root();
        root.timeout_secs = Some(60);
        let child = root.child("p", Some(30));
        assert_eq!(child.timeout_secs, Some(30)); // child's 30 < parent's ~60
    }

    #[test]
    fn side_effect_dedup() {
        let mut ctx = CompositionContext::root();
        ctx.record_side_effects(&["filesystem".into(), "network".into()]);
        ctx.record_side_effects(&["filesystem".into(), "database".into()]);
        assert_eq!(ctx.side_effects.len(), 3);
    }

    // ── Schema validation tests ──────────────────────────────────────────────

    #[test]
    fn validate_input_required_missing() {
        let schema = json!({
            "properties": { "name": { "type": "string" } },
            "required": ["name"]
        });
        let args = json!({});
        let errs = validate_input(&schema, &args);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("missing required"));
    }

    #[test]
    fn validate_input_type_mismatch() {
        let schema = json!({
            "properties": {
                "count": { "type": "integer" },
                "name": { "type": "string" }
            }
        });
        let args = json!({ "count": "not_a_number", "name": 42 });
        let errs = validate_input(&schema, &args);
        assert_eq!(errs.len(), 2);
    }

    #[test]
    fn validate_input_enum_constraint() {
        let schema = json!({
            "properties": {
                "level": { "type": "string", "enum": ["low", "medium", "high"] }
            }
        });
        let args = json!({ "level": "extreme" });
        let errs = validate_input(&schema, &args);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("not in allowed set"));
    }

    #[test]
    fn validate_input_passes() {
        let schema = json!({
            "properties": {
                "path": { "type": "string" },
                "count": { "type": "integer" }
            },
            "required": ["path"]
        });
        let args = json!({ "path": "/src", "count": 5 });
        let errs = validate_input(&schema, &args);
        assert!(errs.is_empty());
    }

    #[test]
    fn validate_output_not_json() {
        let schema = json!({ "properties": { "result": { "type": "string" } } });
        let warnings = validate_output(&schema, "plain text output");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("not valid JSON"));
    }

    #[test]
    fn validate_output_valid_json() {
        let schema = json!({
            "properties": { "status": { "type": "string" } },
            "required": ["status"]
        });
        let warnings = validate_output(&schema, r#"{"status": "ok"}"#);
        assert!(warnings.is_empty());
    }

    // ── Configurable depth tests ─────────────────────────────────────────────

    #[test]
    fn root_with_max_depth_uses_custom_limit() {
        let ctx = CompositionContext::root_with_max_depth(5);
        assert_eq!(ctx.max_depth, 5);
        assert_eq!(ctx.depth, 0);
        assert!(ctx.check_depth().is_ok());
    }

    #[test]
    fn custom_depth_limit_enforced() {
        let mut ctx = CompositionContext::root_with_max_depth(2);
        ctx.depth = 2;
        assert!(ctx.check_depth().is_err());
        ctx.depth = 1;
        assert!(ctx.check_depth().is_ok());
    }

    #[test]
    fn child_inherits_parent_max_depth() {
        let root = CompositionContext::root_with_max_depth(5);
        let child = root.child("skill-a", None);
        assert_eq!(child.max_depth, 5);
        assert_eq!(child.depth, 1);
    }

    #[test]
    fn deeper_nesting_allowed_with_custom_depth() {
        let root = CompositionContext::root_with_max_depth(5);
        let c1 = root.child("a", None);
        let c2 = c1.child("b", None);
        let c3 = c2.child("c", None);
        let c4 = c3.child("d", None);
        assert!(c4.check_depth().is_ok()); // depth 4 < max 5
        let c5 = c4.child("e", None);
        assert!(c5.check_depth().is_err()); // depth 5 >= max 5
    }
}

//! Tool composition: chain tools together with data flow.
//!
//! A `ToolChain` is a named sequence of `ChainStep`s. Each step executes a tool
//! and captures its output. Subsequent steps can reference previous outputs via
//! template variables:
//!
//! - `$prev` — output of the immediately preceding step
//! - `$step.{key}` — output of a named step
//! - `$input.{key}` — value from the original chain input
//!
//! Chains are defined declaratively and can be:
//! - Predefined in skill manifests
//! - Generated dynamically by the LLM
//! - Built programmatically via the builder API

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

// ─── Chain Step ─────────────────────────────────────────────────────────────

/// A single step in a tool chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainStep {
    /// Tool to execute (must be registered in catalog or plugin registry)
    pub tool: String,
    /// Arguments template — may contain `$prev`, `$step.{key}`, `$input.{key}`
    pub args: Value,
    /// Key to store this step's output under (default: "step{N}")
    #[serde(default)]
    pub output_key: Option<String>,
    /// If set, skip this step when condition matches (simple string equality)
    #[serde(default)]
    pub skip_if_prev_contains: Option<String>,
}

// ─── Tool Chain ─────────────────────────────────────────────────────────────

/// A named sequence of tool calls with data flow between them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolChain {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub rollback_on_failure: bool,
    pub steps: Vec<ChainStep>,
}

impl ToolChain {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            rollback_on_failure: false,
            steps: Vec::new(),
        }
    }

    /// Enable or disable automatic bounded rollback when the chain fails.
    pub fn with_rollback_on_failure(mut self, rollback_on_failure: bool) -> Self {
        self.rollback_on_failure = rollback_on_failure;
        self
    }

    /// Add a step to the chain (builder pattern).
    pub fn step(mut self, tool: impl Into<String>, args: Value) -> Self {
        self.steps.push(ChainStep {
            tool: tool.into(),
            args,
            output_key: None,
            skip_if_prev_contains: None,
        });
        self
    }

    /// Add a named step (output can be referenced by key).
    pub fn named_step(
        mut self,
        key: impl Into<String>,
        tool: impl Into<String>,
        args: Value,
    ) -> Self {
        self.steps.push(ChainStep {
            tool: tool.into(),
            args,
            output_key: Some(key.into()),
            skip_if_prev_contains: None,
        });
        self
    }

    /// Validate the chain: check all tools exist in the provided tool names set.
    pub fn validate(&self, known_tools: &[&str]) -> Result<(), Vec<String>> {
        let missing: Vec<String> = self
            .steps
            .iter()
            .filter(|s| !known_tools.contains(&s.tool.as_str()))
            .map(|s| s.tool.clone())
            .collect();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(missing)
        }
    }
}

// ─── Chain Context ──────────────────────────────────────────────────────────

/// Accumulated context during chain execution.
/// Tracks outputs from each step for variable resolution.
#[derive(Debug, Default, Clone)]
pub struct ChainContext {
    /// Output from the most recent step
    pub prev_output: String,
    /// All step outputs indexed by their output_key
    pub outputs: HashMap<String, String>,
    /// Original input provided to the chain
    pub input: Value,
    /// Execution trace: (step_index, tool_name, success)
    pub trace: Vec<(usize, String, bool)>,
}

impl ChainContext {
    pub fn new(input: Value) -> Self {
        Self {
            input,
            ..Default::default()
        }
    }

    /// Record a step's completion.
    pub fn record_step(
        &mut self,
        step_idx: usize,
        tool_name: &str,
        output: String,
        output_key: Option<&str>,
        success: bool,
    ) {
        let key = output_key
            .map(String::from)
            .unwrap_or_else(|| format!("step{}", step_idx));
        self.outputs.insert(key, output.clone());
        self.prev_output = output;
        self.trace.push((step_idx, tool_name.to_string(), success));
    }

    /// Check if the chain should skip a step based on its condition.
    pub fn should_skip(&self, step: &ChainStep) -> bool {
        if let Some(pattern) = &step.skip_if_prev_contains {
            self.prev_output.contains(pattern.as_str())
        } else {
            false
        }
    }
}

// ─── Variable Resolution ────────────────────────────────────────────────────

/// Resolve template variables in a Value tree.
///
/// Supported variables:
/// - `$prev` — replaced with output of the previous step
/// - `$step.{key}` — replaced with output of a named step
/// - `$input.{key}` — replaced with a value from the chain input
pub fn resolve_args(args: &Value, ctx: &ChainContext) -> Value {
    match args {
        Value::String(s) => {
            let resolved = resolve_string(s, ctx);
            Value::String(resolved)
        }
        Value::Object(map) => {
            let mut new_map = serde_json::Map::new();
            for (k, v) in map {
                new_map.insert(k.clone(), resolve_args(v, ctx));
            }
            Value::Object(new_map)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(|v| resolve_args(v, ctx)).collect()),
        other => other.clone(),
    }
}

fn resolve_string(s: &str, ctx: &ChainContext) -> String {
    let mut result = s.to_string();

    // Replace $prev
    if result.contains("$prev") {
        result = result.replace("$prev", &ctx.prev_output);
    }

    // Replace $step.{key} references
    for (key, val) in &ctx.outputs {
        let placeholder = format!("$step.{}", key);
        if result.contains(&placeholder) {
            result = result.replace(&placeholder, val);
        }
    }

    // Replace $input.{key} references
    if result.contains("$input.")
        && let Value::Object(map) = &ctx.input
    {
        for (k, v) in map {
            let placeholder = format!("$input.{}", k);
            if result.contains(&placeholder) {
                let replacement = match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                result = result.replace(&placeholder, &replacement);
            }
        }
    }

    result
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── Builder tests ──

    #[test]
    fn chain_builder_creates_steps() {
        let chain = ToolChain::new("test_chain", "A test chain")
            .step("bash", json!({"command": "echo hello"}))
            .step("grep", json!({"pattern": "$prev", "path": "."}));

        assert_eq!(chain.name, "test_chain");
        assert_eq!(chain.steps.len(), 2);
        assert_eq!(chain.steps[0].tool, "bash");
        assert_eq!(chain.steps[1].tool, "grep");
    }

    #[test]
    fn chain_named_step() {
        let chain = ToolChain::new("named", "Named steps")
            .named_step("files", "list_dir", json!({"path": "."}))
            .step("bash", json!({"command": "wc -l $step.files"}));

        assert_eq!(chain.steps[0].output_key.as_deref(), Some("files"));
        assert!(chain.steps[1].output_key.is_none());
    }

    // ── Validation tests ──

    #[test]
    fn validate_known_tools() {
        let chain = ToolChain::new("valid", "Valid chain")
            .step("bash", json!({}))
            .step("grep", json!({}));
        let known = vec!["bash", "grep", "read_file"];
        assert!(chain.validate(&known).is_ok());
    }

    #[test]
    fn validate_detects_unknown_tools() {
        let chain = ToolChain::new("invalid", "Invalid chain")
            .step("bash", json!({}))
            .step("nonexistent_tool", json!({}));
        let known = vec!["bash", "grep"];
        let result = chain.validate(&known);
        assert!(result.is_err());
        let missing = result.unwrap_err();
        assert_eq!(missing, vec!["nonexistent_tool"]);
    }

    // ── Variable resolution tests ──

    #[test]
    fn resolve_prev_variable() {
        let mut ctx = ChainContext::new(json!({}));
        ctx.prev_output = "hello world".to_string();

        let args = json!({"command": "echo $prev"});
        let resolved = resolve_args(&args, &ctx);
        assert_eq!(resolved["command"].as_str().unwrap(), "echo hello world");
    }

    #[test]
    fn resolve_step_variable() {
        let mut ctx = ChainContext::new(json!({}));
        ctx.outputs
            .insert("files".to_string(), "/tmp/data.txt".to_string());

        let args = json!({"path": "$step.files"});
        let resolved = resolve_args(&args, &ctx);
        assert_eq!(resolved["path"].as_str().unwrap(), "/tmp/data.txt");
    }

    #[test]
    fn resolve_input_variable() {
        let ctx = ChainContext::new(json!({"repo": "mo-dev-agent", "branch": "main"}));

        let args = json!({"command": "git log $input.repo --branch $input.branch"});
        let resolved = resolve_args(&args, &ctx);
        assert_eq!(
            resolved["command"].as_str().unwrap(),
            "git log mo-dev-agent --branch main"
        );
    }

    #[test]
    fn resolve_mixed_variables() {
        let mut ctx = ChainContext::new(json!({"file": "test.rs"}));
        ctx.prev_output = "42".to_string();
        ctx.outputs.insert("count".to_string(), "100".to_string());

        let args = json!({"command": "head -$prev $input.file | tail -$step.count"});
        let resolved = resolve_args(&args, &ctx);
        assert_eq!(
            resolved["command"].as_str().unwrap(),
            "head -42 test.rs | tail -100"
        );
    }

    #[test]
    fn resolve_nested_objects() {
        let mut ctx = ChainContext::new(json!({}));
        ctx.prev_output = "result".to_string();

        let args = json!({
            "outer": {
                "inner": "$prev",
                "list": ["$prev", "static"]
            }
        });
        let resolved = resolve_args(&args, &ctx);
        assert_eq!(resolved["outer"]["inner"].as_str().unwrap(), "result");
        assert_eq!(resolved["outer"]["list"][0].as_str().unwrap(), "result");
        assert_eq!(resolved["outer"]["list"][1].as_str().unwrap(), "static");
    }

    #[test]
    fn resolve_preserves_non_string_values() {
        let ctx = ChainContext::new(json!({}));
        let args = json!({"count": 42, "flag": true, "items": [1, 2, 3]});
        let resolved = resolve_args(&args, &ctx);
        assert_eq!(resolved["count"], 42);
        assert_eq!(resolved["flag"], true);
        assert_eq!(resolved["items"], json!([1, 2, 3]));
    }

    // ── ChainContext tests ──

    #[test]
    fn context_records_steps() {
        let mut ctx = ChainContext::new(json!({}));
        ctx.record_step(0, "bash", "output1".into(), Some("first"), true);
        ctx.record_step(1, "grep", "output2".into(), None, true);

        assert_eq!(ctx.prev_output, "output2");
        assert_eq!(ctx.outputs["first"], "output1");
        assert_eq!(ctx.outputs["step1"], "output2");
        assert_eq!(ctx.trace.len(), 2);
        assert!(ctx.trace[0].2); // success
    }

    #[test]
    fn context_skip_condition() {
        let mut ctx = ChainContext::new(json!({}));
        ctx.prev_output = "Error: file not found".to_string();

        let skip_step = ChainStep {
            tool: "grep".into(),
            args: json!({}),
            output_key: None,
            skip_if_prev_contains: Some("Error:".into()),
        };
        let normal_step = ChainStep {
            tool: "grep".into(),
            args: json!({}),
            output_key: None,
            skip_if_prev_contains: None,
        };

        assert!(ctx.should_skip(&skip_step));
        assert!(!ctx.should_skip(&normal_step));
    }

    // ── Serialization tests ──

    #[test]
    fn chain_roundtrip_json() {
        let chain = ToolChain::new("roundtrip", "Test serialization")
            .step("bash", json!({"command": "echo $prev"}))
            .named_step("files", "list_dir", json!({"path": "$input.dir"}));

        let json_str = serde_json::to_string(&chain).unwrap();
        let deserialized: ToolChain = serde_json::from_str(&json_str).unwrap();

        assert_eq!(deserialized.name, "roundtrip");
        assert_eq!(deserialized.steps.len(), 2);
        assert_eq!(deserialized.steps[1].output_key.as_deref(), Some("files"));
    }

    // ── Predefined chain patterns ──

    #[test]
    fn git_investigation_chain_pattern() {
        // Pattern: blame a file → get contributor → search related commits
        let chain = ToolChain::new("git_investigate", "Deep-dive a suspicious file change")
            .named_step("blame", "git_blame", json!({"file": "$input.file"}))
            .named_step(
                "history",
                "git_file_history",
                json!({"file": "$input.file"}),
            )
            .step("git_log_search", json!({"query": "$input.concern"}));

        assert_eq!(chain.steps.len(), 3);
        let known_tools = vec!["git_blame", "git_file_history", "git_log_search"];
        assert!(chain.validate(&known_tools).is_ok());
    }
}

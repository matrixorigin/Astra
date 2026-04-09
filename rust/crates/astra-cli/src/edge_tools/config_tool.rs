//! Config tool: get/set CLI configuration settings.

use serde_json::{Value, json};

use super::{ToolExecutor, global_output_limit, tool_output_limit};

impl ToolExecutor {
    // ── Config tool: get/set CLI configuration ────────────────────────────────

    /// Get or set CLI configuration.
    pub(super) fn config_tool(&self, args: &Value) -> String {
        let setting = match args.get("setting").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return json!({ "error": "Missing required parameter: setting" }).to_string(),
        };
        let value = args.get("value").and_then(|v| v.as_str());

        // Available settings
        let available = [
            ("model", "Current model (env: MO_MODEL)"),
            (
                "api_key",
                "API key status (env: MO_API_KEY, OPENAI_API_KEY, ANTHROPIC_API_KEY)",
            ),
            (
                "output_limit",
                "Global output limit in bytes (env: MO_GLOBAL_OUTPUT_LIMIT)",
            ),
            (
                "tool_output_limit",
                "Per-tool output limit (env: MO_TOOL_OUTPUT_LIMIT)",
            ),
            (
                "sandbox_mode",
                "Sandbox mode: off, permissive, strict (env: MO_SANDBOX_MODE)",
            ),
            ("auto_approve", "Auto-approve tools (env: MO_AUTO_APPROVE)"),
            (
                "turn_limit",
                "Max turns per conversation (env: MO_MAX_TURNS)",
            ),
            ("list", "Show all available settings"),
        ];

        if setting == "list" {
            // Skip the "list" entry itself when displaying available settings
            let settings: Vec<Value> = available
                .iter()
                .take(available.len() - 1) // Exclude the "list" entry
                .map(|(k, desc)| json!({ "setting": k, "description": desc }))
                .collect();
            return json!({
                "available_settings": settings
            })
            .to_string();
        }

        match setting {
            "model" => {
                if let Some(v) = value {
                    // Set model (this would need integration with RuntimeLimits)
                    json!({
                        "note": format!("To change model, set MO_MODEL={} environment variable", v),
                        "setting": "model",
                        "hint": "Use env tool to set MO_MODEL"
                    }).to_string()
                } else {
                    let current = std::env::var("MO_MODEL").unwrap_or_else(|_| "default".to_string());
                    json!({
                        "setting": "model",
                        "value": current
                    }).to_string()
                }
            }
            "api_key" => {
                // Never show actual key, just status
                let has_mo = std::env::var("MO_API_KEY").is_ok();
                let has_openai = std::env::var("OPENAI_API_KEY").is_ok();
                let has_anthropic = std::env::var("ANTHROPIC_API_KEY").is_ok();
                json!({
                    "setting": "api_key",
                    "status": {
                        "MO_API_KEY": if has_mo { "set" } else { "not set" },
                        "OPENAI_API_KEY": if has_openai { "set" } else { "not set" },
                        "ANTHROPIC_API_KEY": if has_anthropic { "set" } else { "not set" }
                    }
                }).to_string()
            }
            "output_limit" => {
                if value.is_some() {
                    json!({
                        "note": "To change output limit, set MO_GLOBAL_OUTPUT_LIMIT environment variable",
                        "setting": "output_limit"
                    }).to_string()
                } else {
                    json!({
                        "setting": "output_limit",
                        "value": global_output_limit(),
                        "env_var": "MO_GLOBAL_OUTPUT_LIMIT"
                    }).to_string()
                }
            }
            "tool_output_limit" => {
                json!({
                    "setting": "tool_output_limit",
                    "value": tool_output_limit(),
                    "env_var": "MO_TOOL_OUTPUT_LIMIT"
                }).to_string()
            }
            "sandbox_mode" => {
                let current = std::env::var("MO_SANDBOX_MODE").unwrap_or_else(|_| "permissive".to_string());
                if let Some(v) = value {
                    if !["off", "permissive", "strict"].contains(&v) {
                        return json!({ "error": "sandbox_mode must be: off, permissive, or strict" }).to_string();
                    }
                    json!({
                        "note": format!("To change sandbox mode, set MO_SANDBOX_MODE={}", v),
                        "setting": "sandbox_mode",
                        "current": current
                    }).to_string()
                } else {
                    json!({
                        "setting": "sandbox_mode",
                        "value": current,
                        "options": ["off", "permissive", "strict"]
                    }).to_string()
                }
            }
            "auto_approve" => {
                let current = std::env::var("MO_AUTO_APPROVE").unwrap_or_else(|_| "false".to_string());
                json!({
                    "setting": "auto_approve",
                    "value": current,
                    "env_var": "MO_AUTO_APPROVE"
                }).to_string()
            }
            "turn_limit" => {
                let current = std::env::var("MO_MAX_TURNS").unwrap_or_else(|_| "50".to_string());
                json!({
                    "setting": "turn_limit",
                    "value": current,
                    "env_var": "MO_MAX_TURNS"
                }).to_string()
            }
            _ => json!({
                "error": format!("Unknown setting: {}. Use setting='list' to see available settings.", setting)
            }).to_string(),
        }
    }
}

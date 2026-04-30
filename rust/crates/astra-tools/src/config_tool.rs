#![allow(dead_code)]
//! Config tool: show server-level output-limit settings.
//!
//! LLM model / API key / base URL are managed via the admin CLI against the server's
//! `infra_llm_models` + `admin_config` tables. Use `astra-admin model list` and
//! `astra-admin config list` to inspect them.

use serde_json::{Value, json};

/// Configuration query handler.
///
/// `global_limit` and `tool_limit` are the current output limits in bytes.
pub fn config_tool(global_limit: usize, tool_limit: usize, args: &Value) -> String {
    let setting = match args.get("setting").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return json!({ "error": "Missing required parameter: setting" }).to_string(),
    };

    let available = [
        (
            "output_limit",
            "Global output limit in bytes (env: MO_GLOBAL_OUTPUT_LIMIT)",
        ),
        (
            "tool_output_limit",
            "Per-tool output limit (env: MO_TOOL_OUTPUT_LIMIT)",
        ),
        (
            "auto_approve",
            "Auto-approve tools (env: ASTRA_CLI_AUTO_APPROVE)",
        ),
        (
            "turn_limit",
            "Max turns per conversation (env: ASTRA_CLI_MAX_TURNS)",
        ),
        ("list", "Show all available settings"),
    ];

    if setting == "list" {
        let settings: Vec<Value> = available
            .iter()
            .take(available.len() - 1)
            .map(|(k, desc)| json!({ "setting": k, "description": desc }))
            .collect();
        return json!({ "available_settings": settings }).to_string();
    }

    match setting {
        "output_limit" => json!({
            "setting": "output_limit",
            "value": global_limit,
            "env_var": "MO_GLOBAL_OUTPUT_LIMIT"
        })
        .to_string(),
        "tool_output_limit" => json!({
            "setting": "tool_output_limit",
            "value": tool_limit,
            "env_var": "MO_TOOL_OUTPUT_LIMIT"
        })
        .to_string(),
        "auto_approve" => {
            let current =
                std::env::var("ASTRA_CLI_AUTO_APPROVE").unwrap_or_else(|_| "false".to_string());
            json!({
                "setting": "auto_approve",
                "value": current,
                "env_var": "ASTRA_CLI_AUTO_APPROVE"
            })
            .to_string()
        }
        "turn_limit" => {
            let current = std::env::var("ASTRA_CLI_MAX_TURNS").unwrap_or_else(|_| "50".to_string());
            json!({
                "setting": "turn_limit",
                "value": current,
                "env_var": "ASTRA_CLI_MAX_TURNS"
            })
            .to_string()
        }
        _ => json!({
            "error": format!(
                "Unknown setting: {}. Use setting='list' to see available settings. \
                 LLM model settings live in the server DB — use `astra-admin model list` / `astra-admin config list`.",
                setting
            )
        })
        .to_string(),
    }
}

//! Thread-safe environment variable overlay and env tool implementation.
//!
//! Instead of calling unsafe `std::env::set_var` (which is unsound in
//! multi-threaded programs under Rust 2024 edition), the env tool writes to
//! an in-process overlay. Reads merge overlay values with the real environment.
//! Child processes receive overlay values via `Command::envs()`.
//!
//! **Visibility caveat**: overlay values are visible through [`session_env_overlay_get`],
//! [`overlay_all`], and child processes (via [`apply_overlay`]), but
//! NOT to `std::env::var` calls elsewhere in the current process. Code
//! outside this module that reads env vars directly will see the real
//! environment, not the overlay. This is intentional — mutating the real
//! process env is unsound under Rust 2024 edition.
//!
//! Storage lives in [`astra_core::session_env_overlay`] so lightweight crates
//! (e.g. `astra-skills`) can participate without depending on `astra-tools`.

#![allow(dead_code)]
use std::process::Command;

use astra_core::session_env_overlay;
use serde_json::{Value, json};

// ─── Public overlay API (delegates to astra-core) ───────────────────────────

/// Read an env var, checking the overlay first then falling back to real `std::env`.
#[must_use]
pub fn session_env_overlay_get(name: &str) -> Option<String> {
    session_env_overlay::get(name)
}

/// Set an env var in the thread-safe overlay (does not call `std::env::set_var`).
/// Child processes must use [`apply_overlay`] on their [`Command`] to inherit values.
pub fn session_env_overlay_set(name: &str, value: &str) {
    session_env_overlay::set(name, value);
}

/// Remove a key from the overlay.
pub fn session_env_overlay_remove(name: &str) {
    session_env_overlay::remove(name);
}

/// Read an env var, checking the overlay first then falling back to real env.
pub(crate) fn overlay_get(name: &str) -> Option<String> {
    session_env_overlay::get(name)
}

/// Set an env var in the overlay (does NOT touch the real process env).
pub(crate) fn overlay_set(name: &str, value: &str) {
    session_env_overlay::set(name, value);
}

/// Remove an env var in the overlay (marks it as deleted without touching real env).
pub(crate) fn overlay_remove(name: &str) {
    session_env_overlay::remove(name);
}

/// Collect all env vars: real env merged with overlay (overlay wins).
pub(crate) fn overlay_all() -> Vec<(String, String)> {
    session_env_overlay::merged_pairs()
}

/// Apply overlay env vars to a `Command` so child processes inherit them.
pub fn apply_overlay(cmd: &mut Command) {
    session_env_overlay::apply_to_command(cmd);
}

// ─── Env tool functions ──────────────────────────────────────────────────────

/// Dispatch an env tool call to the appropriate sub-command.
pub fn env_tool(args: &Value) -> String {
    let operation = args
        .get("operation")
        .and_then(|v| v.as_str())
        .unwrap_or("list");

    match operation {
        "list" => env_list(args),
        "get" => env_get(args),
        "set" => env_set(args),
        "unset" => env_unset(args),
        "search" => env_search(args),
        _ => json!({
            "error": format!("Unknown env operation: {}. Use: list, get, set, unset, search", operation)
        })
        .to_string(),
    }
}

/// List all environment variables (values masked by default for security).
fn env_list(args: &Value) -> String {
    // The `show_values` parameter is intentionally not honored on the LLM
    // tool path: a malicious / drifted model must not be able to exfiltrate
    // env values by passing show_values=true. Direct CLI callers should use
    // the CLI surface (which can still print raw values when appropriate).
    let _ = args;
    let vars = overlay_all();

    let entries: Vec<Value> = vars
        .into_iter()
        .map(|(name, value)| {
            let display_value = if is_sensitive_var(&name) {
                format!("***MASKED*** ({} chars)", value.len())
            } else {
                format!("({} chars)", value.len())
            };
            json!({
                "name": name,
                "value": display_value
            })
        })
        .collect();

    json!({
        "count": entries.len(),
        "variables": entries
    })
    .to_string()
}

/// Get a specific environment variable.
fn env_get(args: &Value) -> String {
    let name = match args.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return json!({ "error": "Missing required parameter: name" }).to_string(),
    };

    match overlay_get(name) {
        Some(value) => {
            let display_value = if is_sensitive_var(name) {
                format!("***MASKED*** ({} chars)", value.len())
            } else {
                value
            };
            json!({
                "name": name,
                "value": display_value,
                "exists": true
            })
            .to_string()
        }
        None => json!({
            "name": name,
            "exists": false
        })
        .to_string(),
    }
}

/// Set an environment variable (for this session only).
fn env_set(args: &Value) -> String {
    let name = match args.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return json!({ "error": "Missing required parameter: name" }).to_string(),
    };
    let value = match args.get("value").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => return json!({ "error": "Missing required parameter: value" }).to_string(),
    };

    // Validate variable name (alphanumeric + underscore, not starting with digit)
    if name.is_empty()
        || name
            .chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false)
    {
        return json!({ "error": "Invalid variable name: cannot be empty or start with a digit" })
            .to_string();
    }
    if !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return json!({ "error": "Invalid variable name: must contain only alphanumeric characters and underscores" }).to_string();
    }

    // Store in thread-safe overlay (no unsafe set_var needed).
    // Child processes receive overlay values via apply_overlay().
    overlay_set(name, value);

    let display_value = if is_sensitive_var(name) {
        format!("***MASKED*** ({} chars)", value.len())
    } else {
        value.to_string()
    };

    json!({
        "success": true,
        "name": name,
        "value": display_value,
        "note": "Variable set for this session only"
    })
    .to_string()
}

/// Unset (remove) an environment variable.
fn env_unset(args: &Value) -> String {
    let name = match args.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return json!({ "error": "Missing required parameter: name" }).to_string(),
    };

    let existed = overlay_get(name).is_some();

    // Mark as removed in thread-safe overlay (no unsafe remove_var needed).
    overlay_remove(name);

    json!({
        "success": true,
        "name": name,
        "existed": existed,
        "note": "Variable unset for this session only"
    })
    .to_string()
}

/// Search environment variables by regex pattern.
fn env_search(args: &Value) -> String {
    let pattern = match args.get("pattern").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return json!({ "error": "Missing required parameter: pattern" }).to_string(),
    };
    // `show_values` intentionally not honored — see env_list().

    // ReDoS protection: limit pattern length
    if pattern.len() > 500 {
        return json!({ "error": "Pattern too long (max 500 characters)" }).to_string();
    }

    let regex = match regex::Regex::new(&format!("(?i){}", pattern)) {
        Ok(r) => r,
        Err(e) => return json!({ "error": format!("Invalid regex pattern: {}", e) }).to_string(),
    };

    let vars = overlay_all();
    let mut matches: Vec<Value> = Vec::new();

    for (name, value) in vars {
        if regex.is_match(&name) || regex.is_match(&value) {
            let display_value = if is_sensitive_var(&name) {
                format!("***MASKED*** ({} chars)", value.len())
            } else {
                format!("({} chars)", value.len())
            };
            matches.push(json!({
                "name": name,
                "value": display_value
            }));
        }
    }

    matches.sort_by(|a, b| {
        let a_name = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let b_name = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
        a_name.cmp(b_name)
    });

    json!({
        "pattern": pattern,
        "count": matches.len(),
        "matches": matches
    })
    .to_string()
}

/// Check if a variable name suggests it contains sensitive data.
pub fn is_sensitive_var(name: &str) -> bool {
    let upper = name.to_uppercase();
    // Core patterns
    upper.contains("KEY")
        || upper.contains("TOKEN")
        || upper.contains("SECRET")
        || upper.contains("PASSWORD")
        || upper.contains("PASSWD")
        || upper.contains("CREDENTIAL")
        || upper.contains("PRIVATE")
        || upper.starts_with("API_")
        || upper.ends_with("_API")
        || upper.contains("AUTH")
        || upper.contains("BEARER")
        || upper.contains("JWT")
        // Cloud providers
        || upper.starts_with("AWS_")
        || upper.starts_with("AZURE_")
        || upper.starts_with("GCP_")
        || upper.starts_with("GOOGLE_")
        // AI providers
        || upper.contains("OPENAI")
        || upper.contains("ANTHROPIC")
        || upper.contains("CLAUDE")
        || upper.contains("GEMINI")
        // Code hosting
        || upper.starts_with("GITHUB_")
        || upper.starts_with("GITLAB_")
        || upper.starts_with("GH_")
        // Databases
        || upper.contains("DATABASE_URL")
        || upper.contains("DB_PASS")
        || upper.starts_with("REDIS_")
        || upper.starts_with("MONGO")
        // Other services
        || upper.starts_with("SLACK_")
        || upper.starts_with("STRIPE_")
        || upper.starts_with("SENDGRID")
        || upper.starts_with("TWILIO")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn env_list_ignores_show_values_arg_from_llm() {
        overlay_set("R5_TEST_NONSENSITIVE", "abcdefgh");
        let out = env_list(&json!({ "show_values": true }));
        assert!(
            !out.contains("\"abcdefgh\""),
            "env_list leaked raw value despite show_values=true: {out}"
        );
        assert!(
            out.contains("(8 chars)"),
            "expected char-count format in output: {out}"
        );
        overlay_remove("R5_TEST_NONSENSITIVE");
    }

    #[test]
    fn env_search_ignores_show_values_arg_from_llm() {
        overlay_set("R5_TEST_SEARCHABLE", "matchvalue");
        let out = env_search(&json!({
            "pattern": "R5_TEST_SEARCHABLE",
            "show_values": true,
        }));
        assert!(
            !out.contains("\"matchvalue\""),
            "env_search leaked raw value despite show_values=true: {out}"
        );
        assert!(out.contains("(10 chars)"));
        overlay_remove("R5_TEST_SEARCHABLE");
    }
}

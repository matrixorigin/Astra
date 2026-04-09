//! Thread-safe environment variable overlay and env tool implementation.
//!
//! Instead of calling unsafe `std::env::set_var` (which is unsound in
//! multi-threaded programs under Rust 2024 edition), the env tool writes to
//! an in-process overlay. Reads merge overlay values with the real environment.
//! Child processes receive overlay values via `Command::envs()`.
//!
//! **Visibility caveat**: overlay values are visible to [`overlay_get`],
//! [`overlay_all`], and child processes (via [`apply_overlay`]), but
//! NOT to `std::env::var` calls elsewhere in the current process. Code
//! outside this module that reads env vars directly will see the real
//! environment, not the overlay. This is intentional — mutating the real
//! process env is unsound under Rust 2024 edition.

use std::collections::HashMap;
use std::process::Command;
use std::sync::RwLock;

use serde_json::{json, Value};

// ─── Overlay storage ─────────────────────────────────────────────────────────

static ENV_OVERLAY: RwLock<Option<HashMap<String, Option<String>>>> = RwLock::new(None);

/// Acquire the overlay read lock, logging a warning on poison recovery.
fn overlay_read() -> std::sync::RwLockReadGuard<'static, Option<HashMap<String, Option<String>>>> {
    ENV_OVERLAY.read().unwrap_or_else(|p| {
        astra_core::agent_warn!("edge_tools", "ENV_OVERLAY read lock poisoned, recovering");
        p.into_inner()
    })
}

/// Acquire the overlay write lock, logging a warning on poison recovery.
fn overlay_write() -> std::sync::RwLockWriteGuard<'static, Option<HashMap<String, Option<String>>>> {
    ENV_OVERLAY.write().unwrap_or_else(|p| {
        astra_core::agent_warn!("edge_tools", "ENV_OVERLAY write lock poisoned, recovering");
        p.into_inner()
    })
}

// ─── Public overlay API ──────────────────────────────────────────────────────

/// Read an env var, checking the overlay first then falling back to real env.
pub(crate) fn overlay_get(name: &str) -> Option<String> {
    let guard = overlay_read();
    if let Some(ref map) = *guard {
        if let Some(entry) = map.get(name) {
            return entry.clone(); // Some(val) = set, None = removed
        }
    }
    std::env::var(name).ok()
}

/// Set an env var in the overlay (does NOT touch the real process env).
pub(crate) fn overlay_set(name: &str, value: &str) {
    overlay_write()
        .get_or_insert_with(HashMap::new)
        .insert(name.to_string(), Some(value.to_string()));
}

/// Remove an env var in the overlay (marks it as deleted without touching real env).
pub(crate) fn overlay_remove(name: &str) {
    overlay_write()
        .get_or_insert_with(HashMap::new)
        .insert(name.to_string(), None);
}

/// Collect all env vars: real env merged with overlay (overlay wins).
///
/// Acquires the read lock first, then snapshots both sources under the lock
/// to avoid TOCTOU inconsistency between `std::env::vars()` and the overlay.
pub(crate) fn overlay_all() -> Vec<(String, String)> {
    let guard = overlay_read();
    let mut result: HashMap<String, String> = std::env::vars().collect();
    if let Some(ref map) = *guard {
        for (k, v) in map {
            match v {
                Some(val) => {
                    result.insert(k.clone(), val.clone());
                }
                None => {
                    result.remove(k);
                }
            }
        }
    }
    let mut pairs: Vec<_> = result.into_iter().collect();
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    pairs
}

/// Apply overlay env vars to a `Command` so child processes inherit them.
pub fn apply_overlay(cmd: &mut Command) {
    let guard = overlay_read();
    if let Some(ref map) = *guard {
        for (k, v) in map {
            match v {
                Some(val) => {
                    cmd.env(k, val);
                }
                None => {
                    cmd.env_remove(k);
                }
            }
        }
    }
}

// ─── Env tool functions ──────────────────────────────────────────────────────

/// Dispatch an env tool call to the appropriate sub-command.
pub(crate) fn env_tool(args: &Value) -> String {
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
    let show_values = args
        .get("show_values")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let vars = overlay_all();

    let entries: Vec<Value> = vars
        .into_iter()
        .map(|(name, value)| {
            let display_value = if is_sensitive_var(&name) {
                format!("***MASKED*** ({} chars)", value.len())
            } else if show_values {
                value
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
    let show_values = args
        .get("show_values")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // ReDoS protection: limit pattern length
    if pattern.len() > 500 {
        return json!({ "error": "Pattern too long (max 500 characters)" }).to_string();
    }

    let regex = match regex::Regex::new(&format!("(?i){}", pattern)) {
        Ok(r) => r,
        Err(e) => {
            return json!({ "error": format!("Invalid regex pattern: {}", e) }).to_string()
        }
    };

    let vars = overlay_all();
    let mut matches: Vec<Value> = Vec::new();

    for (name, value) in vars {
        if regex.is_match(&name) || regex.is_match(&value) {
            let display_value = if is_sensitive_var(&name) {
                format!("***MASKED*** ({} chars)", value.len())
            } else if show_values {
                value
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
pub(crate) fn is_sensitive_var(name: &str) -> bool {
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

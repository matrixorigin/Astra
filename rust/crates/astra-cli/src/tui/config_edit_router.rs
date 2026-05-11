//! Glue between `ConfigEditView`'s completion token and the runtime.
//!
//! The TUI view cannot do I/O from its `completion()` callback (it's
//! `&self`, and we're running inside the render thread). Instead it
//! packages its final action + config snapshot into a string token,
//! the outer event loop in `tui/mod.rs` splits the token, and this
//! module does the blocking work: TOML write + in-memory overlay
//! refresh so subsequent turns see the new values.

use astra_config::runtime_config::{RuntimeConfig, set_cli_overlay};
use std::path::PathBuf;

/// Resolve a completion token into user-facing text for the scrollback.
///
/// `action` is one of: `save_user` | `save_project` | `discard` | `cancel`.
/// `toml_body` is the serialized `RuntimeConfig` for save_* actions; the
/// other two ignore it.
pub(crate) fn finalize(action: &str, toml_body: &str) -> Result<String, String> {
    match action {
        "save_user" => save_and_report("user", toml_body),
        "save_project" => save_and_report("project", toml_body),
        "discard" => Ok("Discarded config edits. Nothing written.".to_string()),
        "cancel" => Ok("Config edit cancelled.".to_string()),
        other => Err(format!("Unknown config-edit action: {other}")),
    }
}

fn save_and_report(scope: &str, toml_body: &str) -> Result<String, String> {
    let parsed: RuntimeConfig =
        toml::from_str(toml_body).map_err(|e| format!("Could not parse edited config: {e}"))?;
    let path = scope_path(scope)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Could not create {}: {e}", parent.display()))?;
    }
    let pretty = toml::to_string_pretty(&parsed)
        .map_err(|e| format!("Could not serialize config back to TOML: {e}"))?;
    std::fs::write(&path, pretty).map_err(|e| format!("Write failed {}: {e}", path.display()))?;

    // Refresh the process-wide overlay so the current session's next
    // turn sees the edits without having to restart. load() merges the
    // new TOML from disk; set_cli_overlay with `None` lets the normal
    // precedence ladder take over from here.
    set_cli_overlay(None);

    Ok(format!("Saved config to {}", path.display()))
}

fn scope_path(scope: &str) -> Result<PathBuf, String> {
    match scope {
        "user" => {
            let home = dirs::home_dir().ok_or_else(|| "Home directory not found".to_string())?;
            Ok(home.join(".astra/config/runtime.toml"))
        }
        "project" => {
            let cwd = std::env::current_dir().map_err(|e| format!("No working dir: {e}"))?;
            Ok(cwd.join(".astra/config/runtime.toml"))
        }
        other => Err(format!("Unknown scope: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discard_and_cancel_produce_friendly_messages() {
        let a = finalize("discard", "").unwrap();
        assert!(a.to_lowercase().contains("discard"));
        let b = finalize("cancel", "").unwrap();
        assert!(b.to_lowercase().contains("cancel"));
    }

    #[test]
    fn unknown_action_is_an_error() {
        assert!(finalize("what", "").is_err());
    }

    #[test]
    fn save_with_malformed_toml_is_an_error_not_a_panic() {
        let err = finalize("save_user", "not valid toml =").unwrap_err();
        assert!(
            err.to_lowercase().contains("parse") || err.to_lowercase().contains("expected"),
            "err: {err}"
        );
    }
}

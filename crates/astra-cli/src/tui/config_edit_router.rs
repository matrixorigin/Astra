//! Glue between `ConfigEditView`'s typed completion and the runtime.
//!
//! The TUI view cannot do I/O from its `completion()` callback (it's
//! `&self`, and we're running inside the render thread). Instead it
//! packages its final action + config snapshot into a typed result, and this
//! module does the blocking work: TOML write + in-memory overlay
//! refresh so subsequent turns see the new values.

use crate::tui::bottom_pane::view::ConfigEditDisposition;
use astra_config::config_versions::{ConfigVersionStore, LocalFileStore, PutMetadata, VersionId};
use astra_config::runtime_config::{RuntimeConfig, set_cli_overlay};
use std::path::PathBuf;

/// Result of resolving a config-editor completion.
///
/// `message` goes to the scrollback as-is. `save` is populated only
/// when the save succeeded; the caller uses it to stamp the SessionState
/// pointer and emit a `ConfigChange` journal event carrying
/// `from → to`. None means cancel / discard / error — no state change.
#[derive(Debug)]
pub(crate) struct FinalizeOutcome {
    pub message: String,
    pub save: Option<SaveRecord>,
}

#[derive(Debug)]
pub(crate) struct SaveRecord {
    pub new_version_id: String,
    pub source: &'static str,
    /// Canonical TOML that landed on disk and in the version store.
    /// Carried here so the cloud-push path in `tui/mod.rs` can hand
    /// the full payload to `enqueue_config_version_push` without
    /// re-reading from the store.
    pub toml_body: String,
}

/// Resolve a typed completion. `toml_body` is the serialized `RuntimeConfig`
/// for save actions; discard and cancel intentionally ignore it.
pub(crate) fn finalize(
    disposition: ConfigEditDisposition,
    toml_body: &str,
) -> Result<FinalizeOutcome, String> {
    match disposition {
        ConfigEditDisposition::SaveUser => save_and_report("user", "slash_config_edit", toml_body),
        ConfigEditDisposition::SaveProject => {
            save_and_report("project", "slash_config_edit", toml_body)
        }
        ConfigEditDisposition::Discard => Ok(FinalizeOutcome {
            message: "Discarded config edits. Nothing written.".to_string(),
            save: None,
        }),
        ConfigEditDisposition::Cancel => Ok(FinalizeOutcome {
            message: "Config edit cancelled.".to_string(),
            save: None,
        }),
    }
}

pub(crate) async fn finalize_async(
    disposition: ConfigEditDisposition,
    toml_body: String,
) -> Result<FinalizeOutcome, String> {
    tokio::task::spawn_blocking(move || finalize(disposition, &toml_body))
        .await
        .map_err(|error| format!("config save task failed: {error}"))?
}

fn save_and_report(
    scope: &str,
    source: &'static str,
    toml_body: &str,
) -> Result<FinalizeOutcome, String> {
    let parsed: RuntimeConfig =
        toml::from_str(toml_body).map_err(|e| format!("Could not parse edited config: {e}"))?;
    let path = scope_path(scope)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Could not create {}: {e}", parent.display()))?;
    }
    let pretty = toml::to_string_pretty(&parsed)
        .map_err(|e| format!("Could not serialize config back to TOML: {e}"))?;
    std::fs::write(&path, &pretty).map_err(|e| format!("Write failed {}: {e}", path.display()))?;

    // Content-addressed put: computes the new id and dedups on repeat
    // saves. Best-effort — if the store is unavailable (no HOME, disk
    // full) we still report the file-system save and fall back to a
    // pure-hash id for the journal row.
    let new_id = match LocalFileStore::at_default_root() {
        Some(store) => {
            let meta = PutMetadata {
                source_session: None, // caller stamps this when emitting the journal event
                parent: None,
            };
            store
                .put(&parsed, meta)
                .map(|id| id.as_str().to_string())
                .unwrap_or_else(|_| {
                    VersionId::from_toml_bytes(pretty.as_bytes())
                        .as_str()
                        .to_string()
                })
        }
        None => VersionId::from_toml_bytes(pretty.as_bytes())
            .as_str()
            .to_string(),
    };

    // Refresh the process-wide overlay so the current session's next
    // turn sees the edits without having to restart. load() merges the
    // new TOML from disk; set_cli_overlay with `None` lets the normal
    // precedence ladder take over from here.
    set_cli_overlay(None);

    Ok(FinalizeOutcome {
        message: format!("Saved config to {}", path.display()),
        save: Some(SaveRecord {
            new_version_id: new_id,
            source,
            toml_body: pretty,
        }),
    })
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
    use super::finalize;
    use crate::tui::bottom_pane::view::ConfigEditDisposition;

    #[test]
    fn discard_and_cancel_produce_friendly_messages() {
        let a = finalize(ConfigEditDisposition::Discard, "").unwrap();
        assert!(a.message.to_lowercase().contains("discard"));
        assert!(a.save.is_none());
        let b = finalize(ConfigEditDisposition::Cancel, "").unwrap();
        assert!(b.message.to_lowercase().contains("cancel"));
        assert!(b.save.is_none());
    }

    #[test]
    fn save_with_malformed_toml_is_an_error_not_a_panic() {
        let err = finalize(ConfigEditDisposition::SaveUser, "not valid toml =").unwrap_err();
        assert!(
            err.to_lowercase().contains("parse") || err.to_lowercase().contains("expected"),
            "err: {err}"
        );
    }
}

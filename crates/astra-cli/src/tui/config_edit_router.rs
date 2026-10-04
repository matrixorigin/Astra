//! Glue between `ConfigEditView`'s typed completion and the runtime.
//!
//! The TUI view cannot do I/O from its `completion()` callback (it's
//! `&self`, and we're running inside the render thread). Instead it
//! packages its final action + config snapshot into a typed result, and this
//! module saves the user/project defaults. Session configuration selection
//! remains with the caller; invocation settings retain their precedence.

use crate::cli::session::{session_startup, session_state::SessionState};
use crate::tui::bottom_pane::view::ConfigEditDisposition;
use astra_config::config_versions::{ConfigVersionStore, LocalFileStore, PutMetadata, VersionId};
use astra_config::runtime_config::{RuntimeConfig, user_runtime_config_path};
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
    state: &mut SessionState,
) -> Result<FinalizeOutcome, String> {
    let mut outcome = tokio::task::spawn_blocking(move || finalize(disposition, &toml_body))
        .await
        .map_err(|error| format!("config save task failed: {error}"))??;
    if let Some(save) = &outcome.save {
        let (has_snapshot, warning) = refresh_session_after_save(state, save).map_err(|error| {
            format!(
                "{}. Current session configuration was not changed: {error}",
                outcome.message
            )
        })?;
        if has_snapshot {
            outcome.message.push_str(". This session retains its saved configuration; new conversations use the updated defaults.");
        }
        if let Some(warning) = warning {
            outcome.message.push_str(&format!(". Warning: {warning}"));
        }
    }
    Ok(outcome)
}

fn refresh_session_after_save(
    state: &mut SessionState,
    save: &SaveRecord,
) -> Result<(bool, Option<String>), String> {
    let workspace = state
        .session_id
        .as_deref()
        .map(astra_services::session_workspace::read_workspace_optional)
        .transpose()
        .map_err(|error| format!("read session configuration: {error}"))?
        .flatten();
    let saved = workspace
        .as_ref()
        .and_then(|workspace| workspace.tuned_config_json.as_deref())
        .map(serde_json::from_str::<RuntimeConfig>)
        .transpose()
        .map_err(|error| format!("invalid saved configuration: {error}"))?;
    let has_snapshot = saved.is_some();
    let (config, version) = session_startup::prepare_session_runtime_config(state, saved)?;
    if version != save.new_version_id
        && let Some(store) = LocalFileStore::at_default_root()
    {
        store
            .put(
                &config,
                PutMetadata {
                    source_session: state.session_id.clone(),
                    parent: state
                        .config_version_id
                        .clone()
                        .map(VersionId::from_wire_string),
                },
            )
            .map_err(|error| format!("store effective configuration: {error}"))?;
    }
    let previous = state.config_version_id.clone();
    session_startup::apply_session_runtime_config(state, config, version.clone());
    let mut warning = None;
    if previous.as_deref() != Some(version.as_str())
        && let (Some(journal), Some(session_id)) = (&state.journal, &state.session_id)
    {
        let event = astra_services::session_journal::JournalEvent::config_version_change(
            Some(session_id),
            state.turn,
            previous.as_deref(),
            &version,
            save.source,
        );
        if let Err(error) = journal.append(&event) {
            let detail =
                format!("configuration applied, but recording its version failed: {error}");
            session_startup::record_session_persistence_error(state, &detail);
            warning = Some(detail);
        }
    }
    Ok((has_snapshot, warning))
}

fn save_and_report(
    scope: &str,
    source: &'static str,
    toml_body: &str,
) -> Result<FinalizeOutcome, String> {
    let parsed: RuntimeConfig =
        toml::from_str(toml_body).map_err(|e| format!("Could not parse edited config: {e}"))?;
    astra_config::validate_governed_config_candidate(&parsed)
        .map_err(|error| format!("Invalid edited configuration: {}", error.to_json()))?;
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

    Ok(FinalizeOutcome {
        message: format!("Saved config to {}", path.display()),
        save: Some(SaveRecord {
            new_version_id: new_id,
            source,
        }),
    })
}

fn scope_path(scope: &str) -> Result<PathBuf, String> {
    match scope {
        "user" => user_runtime_config_path()
            .ok_or_else(|| "User configuration directory not found".to_string()),
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
    #[serial_test::serial]
    fn user_save_preserves_invocation_settings_and_uses_the_loader_local_root() {
        use astra_config::RuntimeConfig;
        use astra_config::config_versions::{ConfigVersionStore, LocalFileStore, VersionId};

        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let _home_env = crate::test_utils::ProcessEnvGuard::remove("HOME");
        unsafe {
            std::env::set_var("HOME", home.path());
        }
        let _env = crate::test_utils::ProcessEnvGuard::remove("ASTRA_LOCAL_STATE_ROOT");
        unsafe {
            std::env::set_var("ASTRA_LOCAL_STATE_ROOT", root.path());
        }
        let _retrieval_env = crate::test_utils::ProcessEnvGuard::remove("ASTRA_RETRIEVAL_TOP_K");
        unsafe {
            std::env::set_var("ASTRA_RETRIEVAL_TOP_K", "7");
        }
        struct ClearOverlay;
        impl Drop for ClearOverlay {
            fn drop(&mut self) {
                astra_config::runtime_config::set_cli_overlay(None);
            }
        }
        let _overlay = ClearOverlay;
        astra_config::runtime_config::set_cli_overlay(Some(
            astra_config::config_overlay::RuntimeConfigLayer::from_json(
                r#"{"memory":{"retrieval_top_k":5}}"#,
            )
            .unwrap(),
        ));
        let config = RuntimeConfig::load();
        assert_eq!(config.memory.retrieval_top_k, 5);
        let body = toml::to_string_pretty(&config).unwrap();
        let outcome = finalize(ConfigEditDisposition::SaveUser, &body).unwrap();
        let path = root.path().join("config/runtime.toml");
        let saved = std::fs::read_to_string(&path).unwrap();
        assert_eq!(saved, body);
        assert!(outcome.message.contains(&path.display().to_string()));
        let id = VersionId::from_toml_bytes(saved.as_bytes());
        assert_eq!(outcome.save.unwrap().new_version_id, id.as_str());
        assert!(root.path().join("config/versions").is_dir());
        let stored = LocalFileStore::at_default_root()
            .unwrap()
            .get_toml(&id)
            .unwrap()
            .unwrap();
        assert_eq!(stored, saved);
        assert_eq!(RuntimeConfig::load().memory.retrieval_top_k, 5);
        assert!(!home.path().join(".astra/config/runtime.toml").exists());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn save_refreshes_effective_configuration_and_preserves_session_snapshot() {
        use super::{SessionState, finalize_async};
        use astra_config::{RuntimeConfig, runtime_config::ExplainReportFormat};
        use astra_services::session_workspace::{self, WorkspaceMetadata};
        let (root, _sessions) = crate::tests::isolated_sessions_dir();
        let _root_env = crate::test_utils::ProcessEnvGuard::remove("ASTRA_LOCAL_STATE_ROOT");
        let _retrieval_env = crate::test_utils::ProcessEnvGuard::remove("ASTRA_RETRIEVAL_TOP_K");
        unsafe {
            std::env::set_var("ASTRA_LOCAL_STATE_ROOT", root.path());
        }
        let sid = "config-save-snapshot";
        let mut state = SessionState {
            session_id: Some(sid.into()),
            ..Default::default()
        };
        state.journal = Some(astra_services::session_journal::JournalWriter::new(sid).unwrap());
        state.set_explain_report_format_override(ExplainReportFormat::Text);
        state.observability_session = Some(std::sync::Arc::new(std::sync::RwLock::new(
            astra_runtime::observability::ObservabilitySession::new_simple(sid),
        )));
        let mut defaults = RuntimeConfig::default();
        defaults.memory.retrieval_top_k = 9;
        defaults.memory.max_memory_tokens = 1234;
        let body = toml::to_string_pretty(&defaults).unwrap();
        finalize_async(ConfigEditDisposition::SaveUser, body.clone(), &mut state)
            .await
            .unwrap();
        assert_eq!(state.runtime_config.memory.retrieval_top_k, 9);
        assert_eq!(state.context_budget.memory_budget_chars, 1234 * 4);
        let default_version = state.config_version_id.clone();
        let mut snapshot = defaults.clone();
        snapshot.memory.retrieval_top_k = 5;
        snapshot.memory.max_memory_tokens = 4321;
        let mut workspace = WorkspaceMetadata::new(sid, "model");
        workspace.tuned_config_json = Some(serde_json::to_string(&snapshot).unwrap());
        session_workspace::write_workspace(&workspace).unwrap();
        let before = std::fs::read(session_workspace::workspace_file_path(sid).unwrap()).unwrap();
        let outcome = finalize_async(ConfigEditDisposition::SaveUser, body.clone(), &mut state)
            .await
            .unwrap();
        assert!(outcome.message.contains("retains its saved configuration"));
        snapshot.explain.report_format = Some(ExplainReportFormat::Text);
        let expected = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(
            serde_json::to_value(&state.runtime_config).unwrap(),
            expected
        );
        assert_eq!(
            serde_json::to_value(
                &state
                    .observability_session
                    .as_ref()
                    .unwrap()
                    .read()
                    .unwrap()
                    .config
            )
            .unwrap(),
            expected
        );
        assert_eq!(state.context_budget.memory_budget_chars, 4321 * 4);
        let version = astra_config::config_versions::VersionId::from_toml_bytes(
            toml::to_string_pretty(&snapshot).unwrap().as_bytes(),
        );
        assert_eq!(state.config_version_id.as_deref(), Some(version.as_str()));
        assert_eq!(
            std::fs::read(session_workspace::workspace_file_path(sid).unwrap()).unwrap(),
            before
        );
        assert_ne!(outcome.save.unwrap().new_version_id, version.as_str());
        let events = astra_services::session_journal::read_journal(sid).unwrap();
        let change = events.last().unwrap().metadata.as_ref().unwrap();
        assert_eq!(change["config_version"]["from"], default_version.unwrap());
        assert_eq!(change["config_version"]["to"], version.as_str());
        // Simulate persisted corruption; ordinary workspace writes preserve the
        // configuration owned by the fenced mutation path.
        workspace.tuned_config_json = Some("invalid json".into());
        std::fs::write(
            session_workspace::workspace_file_path(sid).unwrap(),
            serde_yaml_ng::to_string(&workspace).unwrap(),
        )
        .unwrap();
        let error = finalize_async(ConfigEditDisposition::SaveUser, body.clone(), &mut state)
            .await
            .unwrap_err();
        assert!(error.contains("Current session configuration was not changed"));
        assert_eq!(
            serde_json::to_value(&state.runtime_config).unwrap(),
            expected
        );
        assert_eq!(state.config_version_id.as_deref(), Some(version.as_str()));

        // A real append failure is reported without discarding the selected config.
        let failed_sid = "config-save-journal-failure";
        let journal = astra_services::session_journal::JournalWriter::new(failed_sid).unwrap();
        std::fs::create_dir(astra_services::session_journal::journal_file_path(
            failed_sid,
        ))
        .unwrap();
        let mut failed_state = SessionState {
            session_id: Some(failed_sid.into()),
            journal: Some(journal),
            ..Default::default()
        };
        let outcome = finalize_async(ConfigEditDisposition::SaveUser, body, &mut failed_state)
            .await
            .unwrap();
        assert!(outcome.message.contains("recording its version failed"));
        assert!(
            failed_state
                .session_persistence_error
                .as_deref()
                .unwrap()
                .contains("recording its version failed")
        );
        assert_eq!(failed_state.runtime_config.memory.retrieval_top_k, 9);
        assert_eq!(failed_state.context_budget.memory_budget_chars, 1234 * 4);
        assert_eq!(
            failed_state.config_version_id.as_deref(),
            Some(outcome.save.unwrap().new_version_id.as_str())
        );
    }

    #[test]
    #[serial_test::serial]
    fn invalid_edits_are_rejected_before_writing_defaults_or_versions() {
        let root = tempfile::tempdir().unwrap();
        let _env = crate::test_utils::ProcessEnvGuard::remove("ASTRA_LOCAL_STATE_ROOT");
        unsafe {
            std::env::set_var("ASTRA_LOCAL_STATE_ROOT", root.path());
        }
        let mut invalid = astra_config::RuntimeConfig::default();
        invalid.compression.compression_threshold = 1.2;
        for body in [
            "not valid toml =".to_string(),
            toml::to_string_pretty(&invalid).unwrap(),
        ] {
            assert!(finalize(ConfigEditDisposition::SaveUser, &body).is_err());
            assert!(!root.path().join("config").exists());
        }
    }
}

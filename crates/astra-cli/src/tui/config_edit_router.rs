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
use astra_config::runtime_config::{RuntimeConfig, USER_ONLY_SECTIONS, user_runtime_config_path};
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
    /// Present after successfully applying the user/project defaults.
    pub show_tips: Option<bool>,
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
            show_tips: None,
        }),
        ConfigEditDisposition::Cancel => Ok(FinalizeOutcome {
            message: "Config edit cancelled.".to_string(),
            save: None,
            show_tips: None,
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
        let defaults = RuntimeConfig::load();
        let show_tips = defaults.tui.show_tips();
        let (has_snapshot, warning) =
            refresh_session_after_save(state, save, defaults).map_err(|error| {
                format!(
                    "{}. Current session configuration was not changed: {error}",
                    outcome.message
                )
            })?;
        outcome.show_tips = Some(show_tips);
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
    defaults: RuntimeConfig,
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
    let (config, version) =
        session_startup::prepare_session_runtime_config(state, saved.or(Some(defaults)))?;
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
    if scope == "project"
        && let Some(user_path) = user_runtime_config_path()
        && same_config_target(&path, &user_path)
    {
        return Err("Project and user configuration refer to the same file; save as user configuration instead".into());
    }
    let complete = toml::to_string_pretty(&parsed)
        .map_err(|e| format!("Could not serialize config back to TOML: {e}"))?;
    let pretty = if scope == "project" {
        let mut value: toml::Value = toml::from_str(&complete)
            .map_err(|e| format!("Could not prepare project configuration: {e}"))?;
        if let Some(table) = value.as_table_mut() {
            for section in USER_ONLY_SECTIONS {
                table.remove(*section);
            }
        }
        toml::to_string_pretty(&value)
            .map_err(|e| format!("Could not serialize project configuration: {e}"))?
    } else {
        complete.clone()
    };
    if scope == "user" {
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&path)
            .map_err(|e| format!("Write failed {}: {e}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|e| format!("Could not protect {}: {e}", path.display()))?;
        }
        file.set_len(0)
            .and_then(|_| file.write_all(pretty.as_bytes()))
            .map_err(|e| format!("Write failed {}: {e}", path.display()))?;
    } else {
        std::fs::write(&path, &pretty)
            .map_err(|e| format!("Write failed {}: {e}", path.display()))?;
    }

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
                    VersionId::from_toml_bytes(complete.as_bytes())
                        .as_str()
                        .to_string()
                })
        }
        None => VersionId::from_toml_bytes(complete.as_bytes())
            .as_str()
            .to_string(),
    };

    Ok(FinalizeOutcome {
        message: format!("Saved config to {}", path.display()),
        show_tips: None,
        save: Some(SaveRecord {
            new_version_id: new_id,
            source,
        }),
    })
}

fn same_config_target(left: &std::path::Path, right: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(left), Ok(right)) = (std::fs::metadata(left), std::fs::metadata(right))
            && left.dev() == right.dev()
            && left.ino() == right.ino()
        {
            return true;
        }
    }
    fn resolve(path: &std::path::Path) -> Option<PathBuf> {
        std::fs::canonicalize(path).ok().or_else(|| {
            Some(
                std::fs::canonicalize(path.parent()?)
                    .ok()?
                    .join(path.file_name()?),
            )
        })
    }
    matches!((resolve(left), resolve(right)), (Some(left), Some(right)) if left == right)
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
    fn project_save_omits_user_only_policy_and_rejects_shared_target() {
        const CHILD: &str = "ASTRA_TEST_PROJECT_CONFIG_SAVE";
        if std::env::var_os(CHILD).is_some() {
            let trusted_root = std::env::var_os("ASTRA_LOCAL_STATE_ROOT").unwrap();
            let mut config = astra_config::RuntimeConfig::default();
            config.memory.retrieval_top_k = 7;
            config.web_fetch.doh_resolvers =
                vec!["https://dns.example/private-token?key=secret".into()];
            let body = toml::to_string_pretty(&config).unwrap();
            finalize(ConfigEditDisposition::SaveProject, &body).unwrap();
            let project = std::fs::read_to_string(".astra/config/runtime.toml").unwrap();
            assert!(!project.contains("web_fetch"));
            assert!(!project.contains("private-token"));
            assert_eq!(
                toml::from_str::<astra_config::RuntimeConfig>(&project)
                    .unwrap()
                    .memory
                    .retrieval_top_k,
                7
            );
            unsafe {
                std::env::set_var("ASTRA_LOCAL_STATE_ROOT", ".astra");
            }
            assert!(
                finalize(ConfigEditDisposition::SaveProject, &body)
                    .unwrap_err()
                    .contains("same file")
            );
            assert_eq!(
                std::fs::read_to_string(".astra/config/runtime.toml").unwrap(),
                project
            );
            #[cfg(unix)]
            {
                let user_file = std::path::PathBuf::from(&trusted_root).join("config/runtime.toml");
                std::fs::create_dir_all(user_file.parent().unwrap()).unwrap();
                std::fs::write(&user_file, "user policy").unwrap();
                std::fs::remove_file(".astra/config/runtime.toml").unwrap();
                std::fs::hard_link(&user_file, ".astra/config/runtime.toml").unwrap();
                unsafe {
                    std::env::set_var("ASTRA_LOCAL_STATE_ROOT", trusted_root);
                }
                assert!(
                    finalize(ConfigEditDisposition::SaveProject, &body)
                        .unwrap_err()
                        .contains("same file")
                );
                assert_eq!(std::fs::read_to_string(user_file).unwrap(), "user policy");
            }
            return;
        }
        let workspace = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tui::config_edit_router::tests::project_save_omits_user_only_policy_and_rejects_shared_target", "--nocapture"])
            .env(CHILD, "1")
            .env("ASTRA_LOCAL_STATE_ROOT", state.path())
            .current_dir(workspace.path())
            .output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn user_save_protects_new_and_existing_configuration_files() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let _env = crate::test_utils::ProcessEnvGuard::remove("ASTRA_LOCAL_STATE_ROOT");
        unsafe {
            std::env::set_var("ASTRA_LOCAL_STATE_ROOT", root.path());
        }
        let mut config = astra_config::RuntimeConfig::default();
        config.web_fetch.doh_resolvers = vec!["https://dns.example/private-token".into()];
        let body = toml::to_string_pretty(&config).unwrap();
        finalize(ConfigEditDisposition::SaveUser, &body).unwrap();
        let file = root.path().join("config/runtime.toml");
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        finalize(ConfigEditDisposition::SaveUser, &body).unwrap();
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

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
        defaults.tui.show_tips = Some(false);
        let body = toml::to_string_pretty(&defaults).unwrap();
        let mut snapshot = defaults.clone();
        snapshot.tui.show_tips = Some(true);
        snapshot.memory.retrieval_top_k = 5;
        snapshot.memory.max_memory_tokens = 4321;
        let mut workspace = WorkspaceMetadata::new(sid, "model");
        workspace.tuned_config_json = Some(serde_json::to_string(&snapshot).unwrap());
        session_workspace::write_workspace(&workspace).unwrap();
        let before = std::fs::read(session_workspace::workspace_file_path(sid).unwrap()).unwrap();
        let outcome = finalize_async(ConfigEditDisposition::SaveUser, body.clone(), &mut state)
            .await
            .unwrap();
        assert_eq!(outcome.show_tips, Some(false));
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

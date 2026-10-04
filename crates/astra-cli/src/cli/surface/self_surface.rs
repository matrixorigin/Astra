use crate::cli::self_command::{
    IdentityView, cli_provider_visible_tool_names, identity_view, to_json, verify_runtime_config,
};
use astra_config::runtime_config::RuntimeConfig;
use astra_runtime::self_model::ConstraintSet;
use astra_services::self_surface::{
    LoadedSelfSurfaceArtifacts, LocalSelfSurfaceService, PersistentSelfSnapshot,
    SelfSurfaceArtifactLoader, SelfSurfaceCheck, SelfSurfaceDimension, SelfSurfaceResponse,
    SelfSurfaceRuntimeSupport, SelfSurfaceService, SurfaceConstraints,
};
use astra_services::session_journal;
use astra_services::session_restore::HybridRestoreService;
use astra_services::session_workspace;
use async_trait::async_trait;
use serde::Serialize;
use std::sync::Arc;

use crate::cli::session::session_restore_client;

#[derive(Debug, Serialize)]
struct SnapshotEnvelope {
    identity: IdentityView,
    #[serde(flatten)]
    snapshot: PersistentSelfSnapshot,
}

struct CliSelfSurfaceArtifactLoader {
    profile: Option<String>,
}

struct CliSelfSurfaceRuntimeSupport;

impl SelfSurfaceRuntimeSupport for CliSelfSurfaceRuntimeSupport {
    fn tool_names(&self) -> Vec<String> {
        cli_provider_visible_tool_names()
    }

    fn constraints(&self) -> SurfaceConstraints {
        let constraints = ConstraintSet::default();
        SurfaceConstraints {
            max_mutations_per_turn: constraints.max_mutations_per_turn,
            config_drift_ceiling: constraints.config_drift_ceiling,
            min_available_tool_count: constraints.min_available_tool_count,
            token_reserve_fraction: constraints.token_reserve_fraction,
        }
    }

    fn compression_threshold(&self, tuned_config_json: Option<&str>) -> Result<f64, String> {
        let config = runtime_config_from_json(tuned_config_json)?;
        Ok(config.compression.compression_threshold)
    }

    fn runtime_checks(&self, tuned_config_json: Option<&str>) -> Vec<SelfSurfaceCheck> {
        verify_runtime_config(tuned_config_json)
            .into_iter()
            .map(|check| SelfSurfaceCheck {
                name: check.name,
                ok: check.ok,
                detail: check.detail,
            })
            .collect()
    }
}

pub(crate) async fn render_surface_for_session_with_profile(
    session_id: &str,
    surface: &str,
    journal_limit: usize,
    profile: Option<&str>,
) -> Result<String, String> {
    if surface == "identity" {
        return to_json(&identity_view());
    }

    let dimension = dimension_from_str(surface)?;
    let service = LocalSelfSurfaceService::new()
        .with_runtime_support(Arc::new(CliSelfSurfaceRuntimeSupport))
        .with_artifact_loader(Arc::new(CliSelfSurfaceArtifactLoader {
            profile: profile.map(str::to_string),
        }));
    let response = service
        .surface(session_id, dimension, journal_limit.max(1))
        .await?;

    match response {
        SelfSurfaceResponse::Snapshot(snapshot) => to_json(&SnapshotEnvelope {
            identity: identity_view(),
            snapshot,
        }),
        SelfSurfaceResponse::Profile(profile) => to_json(&profile),
        SelfSurfaceResponse::Goal(goal) => to_json(&goal),
        SelfSurfaceResponse::Trace(trace) => to_json(&trace),
        SelfSurfaceResponse::Budget(budget) => to_json(&budget),
        SelfSurfaceResponse::Signals(signals) => to_json(&signals),
        SelfSurfaceResponse::Health(health) => to_json(&health),
        SelfSurfaceResponse::Journal(journal) => to_json(&journal),
        SelfSurfaceResponse::Verify(verify) => to_json(&verify),
    }
}

pub(crate) async fn load_artifacts(
    session_id: &str,
    profile: Option<&str>,
) -> Result<LoadedSelfSurfaceArtifacts, String> {
    CliSelfSurfaceArtifactLoader {
        profile: profile.map(str::to_string),
    }
    .load_artifacts(session_id)
    .await
}

#[async_trait]
impl SelfSurfaceArtifactLoader for CliSelfSurfaceArtifactLoader {
    async fn load_artifacts(&self, session_id: &str) -> Result<LoadedSelfSurfaceArtifacts, String> {
        session_journal::validate_session_id(session_id)
            .map_err(|error| format!("invalid session id '{session_id}': {error}"))?;
        let mut workspace =
            session_workspace::read_workspace_optional(session_id).map_err(|error| {
                format!("failed to read workspace for session {session_id}: {error}")
            })?;
        let journal_events = session_journal::read_journal(session_id).map_err(|error| {
            format!("failed to read session journal for session {session_id}: {error}")
        })?;
        let restored = match session_restore_client::fetch_cloud_session_snapshot(
            self.profile.as_deref(),
            session_id,
        )
        .await?
        {
            Some(restored) => Some(restored),
            None => {
                HybridRestoreService::local_only()
                    .restore_local_session(session_id)
                    .await?
            }
        };
        workspace = merge_optional_restored_workspace(session_id, workspace, restored.as_ref());
        if workspace.is_none() && restored.is_none() && journal_events.is_empty() {
            return Err(format!(
                "no persistent local or cloud state found for session {session_id}"
            ));
        }
        let latest_full_context_trace = journal_events
            .iter()
            .rev()
            .find_map(|event| event.context_assembly_trace.clone());
        Ok(LoadedSelfSurfaceArtifacts {
            session_id: session_id.to_string(),
            workspace,
            restored,
            journal_events,
            latest_full_context_trace,
        })
    }
}

fn merge_workspace_with_restored(
    mut workspace: session_workspace::WorkspaceMetadata,
    restored: &astra_services::session_restore::RestoredSession,
) -> session_workspace::WorkspaceMetadata {
    let restored_persistence_error = restored
        .workspace
        .as_ref()
        .and_then(|workspace| workspace.last_persistence_error.as_deref());
    workspace.turn_count = workspace.turn_count.max(restored.turn_count);
    workspace.total_tokens_in = workspace.total_tokens_in.max(restored.total_tokens_in);
    workspace.total_tokens_out = workspace.total_tokens_out.max(restored.total_tokens_out);
    workspace.total_cache_read_tokens = workspace
        .total_cache_read_tokens
        .max(restored.total_cache_read_tokens);
    workspace.total_cache_creation_tokens = workspace
        .total_cache_creation_tokens
        .max(restored.total_cache_creation_tokens);
    if workspace.status.is_empty() {
        workspace.status = restored.last_status.clone();
    }
    if workspace.model.is_none() {
        workspace.model = restored.model.clone();
    }
    if workspace.git_branch.is_none() {
        workspace.git_branch = restored.git_branch.clone();
    }
    if workspace.last_context_trace.is_none() {
        workspace.last_context_trace = restored.last_context_trace.clone();
    }
    workspace.last_persistence_error = merge_persistence_errors(
        workspace.last_persistence_error.as_deref(),
        restored_persistence_error,
    );
    workspace
}

fn merge_optional_restored_workspace(
    session_id: &str,
    workspace: Option<session_workspace::WorkspaceMetadata>,
    restored: Option<&astra_services::session_restore::RestoredSession>,
) -> Option<session_workspace::WorkspaceMetadata> {
    let Some(restored) = restored else {
        return workspace;
    };
    Some(merge_workspace_with_restored(
        workspace.unwrap_or_else(|| {
            restored.workspace.clone().unwrap_or_else(|| {
                session_workspace::WorkspaceMetadata::with_context(
                    session_id,
                    restored.model.as_deref().unwrap_or("default"),
                    ".",
                    restored.git_branch.as_deref(),
                )
            })
        }),
        restored,
    ))
}

/// LocalOnly never consults restore providers. Journal IO is bounded by the
/// shared observation reader; workspace metadata is not a cloud substitute.
pub(crate) fn load_local_observation_artifacts(
    session_id: &str,
    journal_events: Vec<session_journal::JournalEvent>,
) -> Result<LoadedSelfSurfaceArtifacts, String> {
    session_journal::validate_session_id(session_id)?;
    let workspace = session_workspace::read_workspace_optional(session_id)
        .map_err(|_| "failed to read local workspace metadata".to_string())?;
    let latest_full_context_trace = journal_events
        .iter()
        .rev()
        .find_map(|event| event.context_assembly_trace.clone());
    Ok(LoadedSelfSurfaceArtifacts {
        session_id: session_id.to_string(),
        workspace,
        restored: None,
        journal_events,
        latest_full_context_trace,
    })
}

/// Reflection already owns a bounded, owner-authorized observation window.
/// Preserve the existing cloud snapshot fallback without rereading the whole
/// local journal or reconstructing a second local history projection.
pub(crate) async fn load_observation_artifacts_with_profile(
    session_id: &str,
    profile: Option<&str>,
    journal_events: Vec<session_journal::JournalEvent>,
) -> Result<LoadedSelfSurfaceArtifacts, String> {
    let mut artifacts = load_local_observation_artifacts(session_id, journal_events)?;
    artifacts.restored =
        session_restore_client::fetch_cloud_session_snapshot(profile, session_id).await?;
    artifacts.workspace = merge_optional_restored_workspace(
        session_id,
        artifacts.workspace.take(),
        artifacts.restored.as_ref(),
    );
    if artifacts.workspace.is_none()
        && artifacts.restored.is_none()
        && artifacts.journal_events.is_empty()
    {
        return Err(format!(
            "no persistent local or cloud state found for session {session_id}"
        ));
    }
    Ok(artifacts)
}

/// A source policy is an I/O boundary, not merely a display filter.
pub(crate) async fn load_cloud_observation_artifacts(
    session_id: &str,
    profile: Option<&str>,
) -> Result<LoadedSelfSurfaceArtifacts, String> {
    session_journal::validate_session_id(session_id)?;
    let restored =
        session_restore_client::fetch_cloud_session_snapshot(profile, session_id).await?;
    let workspace = merge_optional_restored_workspace(session_id, None, restored.as_ref());
    Ok(LoadedSelfSurfaceArtifacts {
        session_id: session_id.to_string(),
        workspace,
        restored,
        journal_events: vec![],
        latest_full_context_trace: None,
    })
}

pub(crate) fn unavailable_observation_artifacts(
    session_id: &str,
) -> Result<LoadedSelfSurfaceArtifacts, String> {
    session_journal::validate_session_id(session_id)?;
    Ok(LoadedSelfSurfaceArtifacts {
        session_id: session_id.to_string(),
        workspace: None,
        restored: None,
        journal_events: vec![],
        latest_full_context_trace: None,
    })
}

fn merge_persistence_errors(local: Option<&str>, restored: Option<&str>) -> Option<String> {
    let normalize = |value: Option<&str>| {
        value
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };

    match (normalize(local), normalize(restored)) {
        (None, None) => None,
        (Some(error), None) | (None, Some(error)) => Some(error),
        (Some(local), Some(restored)) if local == restored => Some(local),
        (Some(local), Some(restored)) => Some(format!("{local}; restored snapshot: {restored}")),
    }
}

fn dimension_from_str(surface: &str) -> Result<SelfSurfaceDimension, String> {
    match surface {
        "snapshot" => Ok(SelfSurfaceDimension::Snapshot),
        "profile" => Ok(SelfSurfaceDimension::Profile),
        "goal" => Ok(SelfSurfaceDimension::Goal),
        "trace" => Ok(SelfSurfaceDimension::Trace),
        "budget" => Ok(SelfSurfaceDimension::Budget),
        "signals" => Ok(SelfSurfaceDimension::Signals),
        "health" => Ok(SelfSurfaceDimension::Health),
        "journal" => Ok(SelfSurfaceDimension::Journal),
        "verify" => Ok(SelfSurfaceDimension::Verify),
        other => Err(format!("unsupported self surface '{other}'")),
    }
}

fn runtime_config_from_json(tuned_config_json: Option<&str>) -> Result<RuntimeConfig, String> {
    match tuned_config_json {
        Some(json) => serde_json::from_str(json).map_err(|e| e.to_string()),
        None => Ok(RuntimeConfig::load()),
    }
}

#[cfg(test)]
mod tests {
    use super::{merge_optional_restored_workspace, merge_workspace_with_restored};
    use astra_services::session_workspace;

    #[test]
    fn missing_cloud_snapshot_keeps_local_workspace() {
        let local = session_workspace::WorkspaceMetadata::with_context(
            "sid",
            "gpt-5.4",
            "/repo",
            Some("main"),
        );
        let merged = merge_optional_restored_workspace("sid", Some(local.clone()), None);
        assert_eq!(merged.unwrap().model, local.model);
    }

    #[test]
    fn merge_workspace_with_restored_adopts_restored_persistence_error() {
        let local = session_workspace::WorkspaceMetadata::with_context(
            "sid",
            "gpt-5.4",
            "/repo",
            Some("main"),
        );
        let restored_workspace = session_workspace::WorkspaceMetadata::with_context(
            "sid",
            "gpt-5.4",
            "/repo",
            Some("main"),
        );
        let restored = astra_services::session_restore::RestoredSession {
            workspace: Some(session_workspace::WorkspaceMetadata {
                last_persistence_error: Some("failed to write workspace metadata".to_string()),
                ..restored_workspace
            }),
            ..Default::default()
        };

        let merged = merge_workspace_with_restored(local, &restored);

        assert_eq!(
            merged.last_persistence_error.as_deref(),
            Some("failed to write workspace metadata")
        );
    }

    #[test]
    fn merge_workspace_with_restored_combines_distinct_persistence_errors() {
        let local = session_workspace::WorkspaceMetadata {
            last_persistence_error: Some("failed to append turn event".to_string()),
            ..session_workspace::WorkspaceMetadata::with_context(
                "sid",
                "gpt-5.4",
                "/repo",
                Some("main"),
            )
        };
        let restored_workspace = session_workspace::WorkspaceMetadata {
            last_persistence_error: Some("failed to write workspace metadata".to_string()),
            ..session_workspace::WorkspaceMetadata::with_context(
                "sid",
                "gpt-5.4",
                "/repo",
                Some("main"),
            )
        };
        let restored = astra_services::session_restore::RestoredSession {
            workspace: Some(restored_workspace),
            ..Default::default()
        };

        let merged = merge_workspace_with_restored(local, &restored);
        let merged_error = merged
            .last_persistence_error
            .expect("merged persistence error");

        assert!(merged_error.contains("failed to append turn event"));
        assert!(merged_error.contains("restored snapshot: failed to write workspace metadata"));
    }
}

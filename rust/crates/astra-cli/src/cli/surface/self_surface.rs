use crate::cli::self_command::{IdentityView, identity_view, to_json, verify_runtime_config};
use astra_config::runtime_config::RuntimeConfig;
use astra_runtime::self_model::ConstraintSet;
use astra_runtime::tool_registry::ToolRegistry;
use astra_services::self_surface::{
    BudgetConfig, LoadedSelfSurfaceArtifacts, LocalSelfSurfaceService, PersistentSelfSnapshot,
    SelfSurfaceArtifactLoader, SelfSurfaceCheck, SelfSurfaceDimension, SelfSurfaceResponse,
    SelfSurfaceRuntimeSupport, SelfSurfaceService, SurfaceConstraints,
};
use astra_services::session_journal;
use astra_services::session_restore::{HybridRestoreService, SessionRestoreService};
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
        ToolRegistry::all_tool_names()
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    fn constraints(&self) -> SurfaceConstraints {
        let constraints = ConstraintSet::default();
        SurfaceConstraints {
            max_mutations_per_turn: constraints.max_mutations_per_turn,
            config_drift_ceiling: constraints.config_drift_ceiling,
            min_tool_pool_size: constraints.min_tool_pool_size,
            token_reserve_fraction: constraints.token_reserve_fraction,
        }
    }

    fn budget_config(&self, tuned_config_json: Option<&str>) -> Result<BudgetConfig, String> {
        let config = runtime_config_from_json(tuned_config_json)?;
        Ok(BudgetConfig {
            tool_budget_tokens: config.tool_selection.tool_budget_tokens,
            compression_threshold: config.compression.compression_threshold,
            max_turn_input_tokens: config.token_budget.max_turn_input_tokens,
            compression_threshold_min: config.context_window.compression_threshold_min,
            compression_threshold_max: config.context_window.compression_threshold_max,
        })
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

pub(crate) async fn render_surface_for_session(
    session_id: &str,
    surface: &str,
    journal_limit: usize,
) -> Result<String, String> {
    render_surface_for_session_with_profile(session_id, surface, journal_limit, None).await
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
                    .restore_session(session_id)
                    .await?
            }
        };
        if let Some(restored) = restored.as_ref() {
            workspace = Some(match workspace {
                Some(existing) => merge_workspace_with_restored(existing, restored),
                None => merge_workspace_with_restored(
                    restored.workspace.clone().unwrap_or_else(|| {
                        session_workspace::WorkspaceMetadata::with_context(
                            session_id,
                            restored.model.as_deref().unwrap_or("default"),
                            ".",
                            restored.git_branch.as_deref(),
                        )
                    }),
                    restored,
                ),
            });
        }
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
    if workspace.plan_goal.is_none() {
        workspace.plan_goal = restored.plan_goal.clone();
    }
    if workspace.plan_config_json.is_none() {
        workspace.plan_config_json = restored.plan_config_json.clone();
    }
    if workspace.contract_json.is_none() {
        workspace.contract_json = restored.contract_json.clone();
    }
    if workspace.plan_corrections.is_empty() {
        workspace.plan_corrections = restored.plan_corrections.clone();
    }
    if workspace.last_context_trace.is_none() {
        workspace.last_context_trace = restored.last_context_trace.clone();
    }
    if workspace.executing_plan_json.is_none() {
        workspace.executing_plan_json = restored.executing_plan_json.clone();
    }
    workspace.plan_execution_rounds = workspace
        .plan_execution_rounds
        .max(restored.plan_execution_rounds);
    workspace.last_persistence_error = merge_persistence_errors(
        workspace.last_persistence_error.as_deref(),
        restored_persistence_error,
    );
    workspace
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
    use super::merge_workspace_with_restored;
    use astra_services::session_workspace;

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

use super::*;
use astra_services::self_surface::{
    BudgetConfig, LocalSelfSurfaceService, PersistentSelfSnapshot, SelfSurfaceCheck,
    SelfSurfaceDimension, SelfSurfaceResponse, SelfSurfaceRuntimeSupport, SelfSurfaceService,
    SurfaceConstraints,
};
use serde::Serialize;
use std::sync::Arc;

#[derive(Debug, Serialize)]
struct SnapshotEnvelope {
    identity: IdentityView,
    #[serde(flatten)]
    snapshot: PersistentSelfSnapshot,
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
        super::verify_runtime_config(tuned_config_json)
            .into_iter()
            .map(|check| SelfSurfaceCheck {
                name: check.name,
                ok: check.ok,
                detail: check.detail,
            })
            .collect()
    }
}

pub(super) async fn render_surface_for_session(
    session_id: &str,
    surface: &str,
    journal_limit: usize,
) -> Result<String, String> {
    if surface == "identity" {
        return to_json(&identity_view());
    }

    let dimension = dimension_from_str(surface)?;
    let service =
        LocalSelfSurfaceService::new().with_runtime_support(Arc::new(CliSelfSurfaceRuntimeSupport));
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

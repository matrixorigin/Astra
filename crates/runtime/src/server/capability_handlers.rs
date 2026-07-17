use axum::{Json, extract::State, http::HeaderMap};
use serde::Serialize;

use super::*;

#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct RuntimeCapabilitiesResponse {
    tools: Vec<OptionalToolAvailability>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct OptionalToolAvailability {
    name: String,
    providers: Vec<AvailableProvider>,
}

#[derive(Debug, Serialize, PartialEq, Eq, PartialOrd, Ord)]
struct AvailableProvider {
    provider_id: String,
    kind: &'static str,
    display_name: String,
    status: &'static str,
}

pub(crate) async fn runtime_capabilities_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<RuntimeCapabilitiesResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let providers_by_tool = state
        .tool_execution_service
        .optional_tool_providers_for_user(&user.user_id)
        .await;

    Ok(Json(RuntimeCapabilitiesResponse {
        tools: providers_by_tool
            .into_iter()
            .map(|(name, providers)| OptionalToolAvailability {
                name,
                providers: providers
                    .into_iter()
                    .map(|provider| AvailableProvider {
                        provider_id: provider.provider_id,
                        kind: provider.kind.as_str(),
                        display_name: provider.display_name,
                        status: "ready",
                    })
                    .collect(),
            })
            .collect(),
    }))
}

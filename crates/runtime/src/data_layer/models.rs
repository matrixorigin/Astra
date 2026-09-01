use astra_services::{MAX_API_LIST_LIMIT, models::*};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::AppState;
use astra_core::{ErrorResponse, error_response, internal_error};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};

const DEFAULT_MODEL_CATALOG_LIMIT: u32 = 50;

#[derive(Debug, Default, Deserialize)]
pub struct ModelCatalogQuery {
    #[serde(default = "default_model_catalog_limit")]
    pub limit: u32,
    pub after_provider: Option<String>,
    pub after_name: Option<String>,
    pub after_offering_id: Option<String>,
}

impl ModelCatalogQuery {
    fn cursor(&self) -> Result<Option<ModelListCursor>, (StatusCode, Json<ErrorResponse>)> {
        match (
            self.after_provider.as_deref(),
            self.after_name.as_deref(),
            self.after_offering_id.as_deref(),
        ) {
            (None, None, None) => Ok(None),
            (Some(provider), Some(name), Some(offering_id)) => {
                if provider.trim().is_empty()
                    || name.trim().is_empty()
                    || offering_id.trim().is_empty()
                {
                    return Err(error_response(
                        StatusCode::BAD_REQUEST,
                        "model catalog cursor fields must be non-empty",
                    ));
                }
                Ok(Some(ModelListCursor {
                    provider: provider.trim().to_string(),
                    model_name: name.trim().to_string(),
                    model_id: offering_id.trim().to_string(),
                }))
            }
            _ => Err(error_response(
                StatusCode::BAD_REQUEST,
                "model catalog cursor requires after_provider, after_name, and after_offering_id",
            )),
        }
    }
}

fn default_model_catalog_limit() -> u32 {
    DEFAULT_MODEL_CATALOG_LIMIT
}

pub async fn create_model_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ModelCreateRequest>,
) -> Result<(StatusCode, Json<ModelResponse>), (StatusCode, Json<ErrorResponse>)> {
    let admin = state.admin.authorizer.require_admin(&headers).await?;

    let model = state
        .model_service
        .create_model(
            admin.user_id,
            ModelCreateRequestData {
                name: request.name,
                provider: request.provider,
                api_key: request.api_key,
                base_url: request.base_url,
                description: request.description,
                context_window: request.context_window,
                max_completion_tokens: request.max_completion_tokens,
                input_modalities: request.input_modalities,
                output_modalities: request.output_modalities,
                supported_parameters: request.supported_parameters,
                pricing: request.pricing,
                architecture: request.architecture,
                tags: request.tags,
                quirks: request.quirks,
            },
        )
        .await?;
    Ok((StatusCode::CREATED, Json(ModelResponse::from(model))))
}

pub async fn list_models_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ModelCatalogQuery>,
) -> Result<Json<ModelListPageResponse>, (StatusCode, Json<ErrorResponse>)> {
    let catalog = effective_model_catalog(&state, &headers, &query, false).await?;
    Ok(Json(ModelListPageResponse {
        items: catalog
            .offerings
            .into_iter()
            .map(ModelListItemResponse::from)
            .collect(),
        next_cursor: catalog.next_cursor,
        limit: catalog.limit,
        total: catalog.total,
        catalog_revision: catalog.catalog_revision,
    }))
}

struct EffectiveModelCatalog {
    declared: Vec<DeclaredModelAccess>,
    offerings: Vec<ModelListItem>,
    provider_default: Option<ModelDefaultCandidate>,
    default_catalog: Option<Vec<ModelListItemResponse>>,
    next_cursor: Option<ModelListCursor>,
    limit: u32,
    total: u32,
    catalog_revision: String,
}

async fn effective_model_catalog(
    state: &AppState,
    headers: &HeaderMap,
    query: &ModelCatalogQuery,
    active_only: bool,
) -> Result<EffectiveModelCatalog, (StatusCode, Json<ErrorResponse>)> {
    let cursor = query.cursor()?;
    let principal = state.auth_service.current_principal(headers).await?;
    if principal.is_edge_registration() {
        let catalog = state
            .auth_service
            .external_catalog_by_scope(&principal)
            .await?;
        let provider_default = catalog
            .default_model_id
            .map(|offering_id| ModelDefaultCandidate {
                offering_id,
                source: ModelDefaultSource::ExternalProvider,
                scope: ModelDefaultScope::EffectiveCatalog,
            });
        let mut offerings = catalog
            .models
            .into_iter()
            .map(ModelListItem::from)
            .filter(|item| !active_only || item.is_active)
            .collect::<Vec<_>>();
        offerings.sort_by(|left, right| {
            (
                left.provider.as_str(),
                left.name.as_str(),
                left.offering_id.as_str(),
            )
                .cmp(&(
                    right.provider.as_str(),
                    right.name.as_str(),
                    right.offering_id.as_str(),
                ))
        });
        let default_catalog = offerings
            .iter()
            .cloned()
            .map(ModelListItemResponse::from)
            .collect();
        let catalog_revision = model_catalog_revision(&offerings);
        let page = paginate_model_items(offerings, query.limit, cursor)?;
        return Ok(EffectiveModelCatalog {
            declared: vec![DeclaredModelAccess {
                id: "this-device".to_string(),
                kind: ModelAccessKind::ThisDevice,
                label: "This device".to_string(),
                execution_placement: ModelExecutionPlacement::Edge,
                availability: ModelAccessAvailability::Ready,
            }],
            offerings: page.items,
            provider_default,
            default_catalog: Some(default_catalog),
            next_cursor: page.next_cursor,
            limit: page.limit,
            total: page.total,
            catalog_revision,
        });
    }
    let user = principal.user;
    let is_admin = !active_only && state.admin.authorizer.require_admin(headers).await.is_ok();
    let page = state
        .model_service
        .list_models_page(user.user_id.clone(), is_admin, query.limit, cursor)
        .await?;
    let catalog_revision = state
        .model_service
        .model_catalog_revision(user.user_id, is_admin)
        .await?;
    Ok(EffectiveModelCatalog {
        declared: vec![DeclaredModelAccess {
            id: "self-hosted".to_string(),
            kind: ModelAccessKind::SelfHosted,
            label: "Self-hosted".to_string(),
            execution_placement: ModelExecutionPlacement::Server,
            availability: ModelAccessAvailability::Ready,
        }],
        offerings: page.items,
        provider_default: None,
        default_catalog: None,
        next_cursor: page.next_cursor,
        limit: page.limit,
        total: page.total,
        catalog_revision,
    })
}

fn paginate_model_items(
    mut items: Vec<ModelListItem>,
    requested_limit: u32,
    cursor: Option<ModelListCursor>,
) -> Result<ModelListPage, (StatusCode, Json<ErrorResponse>)> {
    let limit = requested_limit.clamp(1, MAX_API_LIST_LIMIT);
    let mut offering_ids = HashSet::with_capacity(items.len());
    for item in &items {
        if item.provider.trim() != item.provider
            || item.provider.is_empty()
            || item.name.trim() != item.name
            || item.name.is_empty()
            || item.offering_id.trim() != item.offering_id
            || item.offering_id.is_empty()
        {
            return Err(error_response(
                StatusCode::BAD_GATEWAY,
                format!(
                    "edge model catalog contains an invalid seek identity for Offering '{}'",
                    item.offering_id
                ),
            ));
        }
        if !offering_ids.insert(item.offering_id.clone()) {
            return Err(error_response(
                StatusCode::BAD_GATEWAY,
                format!(
                    "edge model catalog contains duplicate Offering identity '{}'",
                    item.offering_id
                ),
            ));
        }
    }
    let total = u32::try_from(items.len()).map_err(|error| {
        internal_error(format!("edge model catalog total exceeds u32: {error}"))
    })?;
    if let Some(cursor) = cursor {
        items.retain(|item| {
            (
                item.provider.as_str(),
                item.name.as_str(),
                item.offering_id.as_str(),
            ) > (
                cursor.provider.as_str(),
                cursor.model_name.as_str(),
                cursor.model_id.as_str(),
            )
        });
    }
    let take = usize::try_from(limit).unwrap_or(usize::MAX);
    let has_more = items.len() > take;
    items.truncate(take);
    let next_cursor = has_more.then(|| {
        let item = items.last().expect("limit is positive when page has more");
        ModelListCursor {
            provider: item.provider.clone(),
            model_name: item.name.clone(),
            model_id: item.offering_id.clone(),
        }
    });
    Ok(ModelListPage {
        items,
        next_cursor,
        limit,
        total,
    })
}

pub async fn get_model_access_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ModelCatalogQuery>,
) -> Result<Json<ModelAccessProjectionResponse>, (StatusCode, Json<ErrorResponse>)> {
    let is_first_page = query.after_provider.is_none()
        && query.after_name.is_none()
        && query.after_offering_id.is_none();
    let catalog = effective_model_catalog(&state, &headers, &query, true).await?;
    let offerings = catalog
        .offerings
        .into_iter()
        .filter(|offering| offering.is_active)
        .map(ModelListItemResponse::from)
        .collect::<Vec<_>>();
    let default_catalog = catalog.default_catalog.unwrap_or_else(|| offerings.clone());
    let mut projection = project_model_access_page_with_default_catalog(
        catalog.declared,
        offerings,
        Some(catalog.total),
        &default_catalog,
        catalog.provider_default,
        chrono::Utc::now().to_rfc3339(),
    )
    .map_err(internal_error)?;
    projection.next_cursor = catalog.next_cursor;
    projection.limit = catalog.limit;
    projection.total = catalog.total;
    projection.catalog_revision = catalog.catalog_revision;
    if !is_first_page {
        projection.default_offering_id = None;
        projection.default_resolution = None;
    }
    Ok(Json(projection))
}

pub async fn get_model_handler(
    State(state): State<AppState>,
    Path(model_name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<ModelResponse>, (StatusCode, Json<ErrorResponse>)> {
    let _user = state.auth_service.current_user(&headers).await?;
    let model = state.model_service.get_model(model_name).await?;
    Ok(Json(ModelResponse::from(model)))
}

pub async fn get_memory_model_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<MemoryInferenceOfferingsResponse>, (StatusCode, Json<ErrorResponse>)> {
    let _user = state.auth_service.current_user(&headers).await?;
    let matrixone = crate::matrix_cloud_runtime::matrix_settings_from_env().map_err(|e| {
        error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("MatrixOne configuration unavailable: {e}"),
        )
    })?;
    let pool_ref = state.shared_pool.as_ref().map(|sp| sp.get());
    let resolved = resolve_memory_offerings(&matrixone, &state.fernet_encryptor, pool_ref)
        .await
        .map_err(|e| {
            error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("Memory model resolution failed: {e}"),
            )
        })?;
    let offerings = resolved
        .into_iter()
        .map(|offering| MemoryInferenceOfferingResponse {
            offering_id: offering.offering_id,
            model_name: offering.model.model_name,
            thinking_capability: offering.model.thinking_capability,
        })
        .collect::<Vec<_>>();
    if offerings.is_empty() {
        return Err(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "No active LLM model configured.",
        ));
    }
    Ok(Json(MemoryInferenceOfferingsResponse { offerings }))
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MemoryInferenceOfferingsResponse {
    /// Ordered candidates. The first entry is the current governed default;
    /// later entries are explicit failover candidates for optional memory
    /// inference only.
    pub offerings: Vec<MemoryInferenceOfferingResponse>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MemoryInferenceOfferingResponse {
    pub offering_id: String,
    /// Display/diagnostic fact; clients must never use it to select a route.
    pub model_name: String,
    pub thinking_capability: Option<ThinkingCapability>,
}

pub async fn update_model_handler(
    State(state): State<AppState>,
    Path(model_name): Path<String>,
    headers: HeaderMap,
    Json(request): Json<ModelUpdateRequest>,
) -> Result<Json<ModelResponse>, (StatusCode, Json<ErrorResponse>)> {
    let _admin = state.admin.authorizer.require_admin(&headers).await?;
    let model = state
        .model_service
        .update_model(
            model_name,
            ModelUpdateRequestData {
                api_key: request.api_key,
                base_url: request.base_url,
                provider: request.provider,
                description: request.description,
                context_window: request.context_window,
                max_completion_tokens: request.max_completion_tokens,
                input_modalities: request.input_modalities,
                output_modalities: request.output_modalities,
                supported_parameters: request.supported_parameters,
                pricing: request.pricing,
                architecture: request.architecture,
                tags: request.tags,
                is_active: request.is_active,
                quirks: request.quirks,
            },
        )
        .await?;
    Ok(Json(ModelResponse::from(model)))
}

pub async fn delete_model_handler(
    State(state): State<AppState>,
    Path(model_name): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    let _admin = state.admin.authorizer.require_admin(&headers).await?;
    state.model_service.delete_model(model_name).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn check_model_handler(
    State(state): State<AppState>,
    Path(model_name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<ModelResponse>, (StatusCode, Json<ErrorResponse>)> {
    let _admin = state.admin.authorizer.require_admin(&headers).await?;
    let model = state.model_service.check_model(model_name).await?;
    Ok(Json(ModelResponse::from(model)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge_item(id: &str, name: &str) -> ModelListItem {
        ModelListItem {
            offering_id: id.to_string(),
            access_id: "this-device".to_string(),
            access_kind: ModelAccessKind::ThisDevice,
            access_label: "This device".to_string(),
            execution_placement: ModelExecutionPlacement::Edge,
            name: name.to_string(),
            provider: "external".to_string(),
            description: None,
            is_active: true,
            context_window: 8_192,
            max_completion_tokens: None,
            architecture: None,
            thinking_capability: None,
        }
    }

    #[test]
    fn model_access_resolves_external_default_against_the_complete_catalog() {
        let full_catalog = vec![
            ModelListItemResponse::from(edge_item("model-alpha", "Alpha")),
            ModelListItemResponse::from(edge_item("model-beta", "Beta")),
        ];
        let projection = project_model_access_page_with_default_catalog(
            vec![DeclaredModelAccess {
                id: "this-device".to_string(),
                kind: ModelAccessKind::ThisDevice,
                label: "This device".to_string(),
                execution_placement: ModelExecutionPlacement::Edge,
                availability: ModelAccessAvailability::Ready,
            }],
            vec![full_catalog[0].clone()],
            Some(2),
            &full_catalog,
            Some(ModelDefaultCandidate {
                offering_id: "model-beta".to_string(),
                source: ModelDefaultSource::ExternalProvider,
                scope: ModelDefaultScope::EffectiveCatalog,
            }),
            "2026-08-06T00:00:00Z".to_string(),
        )
        .expect("a default on a later page must remain valid");

        assert_eq!(
            projection.default_offering_id.as_deref(),
            Some("model-beta")
        );
        assert!(matches!(
            projection.default_resolution,
            Some(ModelDefaultResolution::Selected {
                source: ModelDefaultSource::ExternalProvider,
                ..
            })
        ));
    }

    #[test]
    fn invalid_external_catalog_default_keeps_manual_offerings_visible() {
        let offerings = vec![ModelListItemResponse::from(edge_item(
            "model-valid",
            "Valid",
        ))];
        let projection = project_model_access_page_with_default_catalog(
            vec![DeclaredModelAccess {
                id: "this-device".to_string(),
                kind: ModelAccessKind::ThisDevice,
                label: "This device".to_string(),
                execution_placement: ModelExecutionPlacement::Edge,
                availability: ModelAccessAvailability::Ready,
            }],
            offerings.clone(),
            Some(1),
            &offerings,
            Some(ModelDefaultCandidate {
                offering_id: "model-disabled".to_string(),
                source: ModelDefaultSource::ExternalProvider,
                scope: ModelDefaultScope::EffectiveCatalog,
            }),
            "2026-08-10T00:00:00Z".to_string(),
        )
        .expect("invalid default must not poison the effective catalog");

        assert_eq!(projection.offerings.len(), 1);
        assert!(projection.default_offering_id.is_none());
        assert!(matches!(
            projection.default_resolution,
            Some(ModelDefaultResolution::Invalid {
                reason: ModelDefaultInvalidReason::NotEffectiveOffering,
                ..
            })
        ));
    }

    #[test]
    fn edge_catalog_without_provider_ref_drains_across_pages() {
        let items = vec![
            edge_item("edge-a", "model-a"),
            edge_item("edge-b", "model-b"),
        ];
        let first = paginate_model_items(items, 1, None).expect("first page");
        let cursor = first.next_cursor.clone().expect("continuation cursor");
        assert_eq!(cursor.provider, "external");
        assert_eq!(first.items.len(), 1);

        let second = paginate_model_items(
            vec![
                edge_item("edge-a", "model-a"),
                edge_item("edge-b", "model-b"),
            ],
            1,
            Some(cursor),
        )
        .expect("second page");
        assert_eq!(second.items.len(), 1);
        assert_eq!(second.items[0].offering_id, "edge-b");
        assert!(second.next_cursor.is_none());
    }

    #[test]
    fn edge_catalog_rejects_unusable_seek_identity() {
        let mut item = edge_item("edge-a", "model-a");
        item.name = " ".to_string();
        let error = paginate_model_items(vec![item], 1, None).expect_err("invalid identity");
        assert_eq!(error.0, StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn edge_catalog_rejects_duplicate_offering_identity_before_paging() {
        let first = edge_item("edge-duplicate", "model-a");
        let mut second = edge_item("edge-duplicate", "model-b");
        second.provider = "another-provider".to_string();

        let error = paginate_model_items(vec![first, second], 1, None)
            .expect_err("duplicate identity cannot be split across pages");

        assert_eq!(error.0, StatusCode::BAD_GATEWAY);
    }
}

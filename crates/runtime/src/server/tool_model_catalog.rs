//! Explicit refresh/pagination of the same authenticated catalog observation.

use serde_json::Value;

use astra_turn_core::model_catalog::{
    CatalogError, ModelCatalogPage, ModelCatalogRequest, catalog_page, unavailable_page,
};

#[cfg(test)]
#[path = "tool_model_catalog_tests.rs"]
mod tests;

pub(super) fn catalog_read_error(status: axum::http::StatusCode) -> CatalogError {
    use axum::http::StatusCode;
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => CatalogError::Unauthorized,
        StatusCode::NOT_IMPLEMENTED => CatalogError::Unsupported,
        status if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS => {
            CatalogError::Unavailable
        }
        _ => CatalogError::InvalidCatalog,
    }
}

pub(crate) async fn handle_model_catalog(
    args: &Value,
    user_id: &str,
    reader: Option<&astra_services::models::AuthorizedModelCatalogReader>,
) -> astra_tools::ToolResult {
    let parsed = ModelCatalogRequest::from_args(args);
    let scope = reader.map_or("unbound", |reader| reader.scope());
    let result = match parsed {
        Ok(request) => match reader {
            Some(reader) if reader.user_id() != user_id => Err(CatalogError::Unauthorized),
            Some(reader) => {
                match tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    reader.read_fresh_items(),
                )
                .await
                {
                    Ok(Ok(items)) => catalog_page(items, &request, scope),
                    Ok(Err((status, _))) => Err(catalog_read_error(status)),
                    Err(_) => Err(CatalogError::Unavailable),
                }
            }
            None => Err(CatalogError::Unsupported),
        },
        Err(error) => Err(error),
    };
    match result {
        Ok(page) => {
            astra_tools::ToolResult::text(page_json(&page)).with_source_bounded_model_projection()
        }
        Err(error) => {
            let result = astra_tools::ToolResult::error(page_json(&unavailable_page(error, scope)));
            if matches!(
                error,
                CatalogError::InvalidRequest | CatalogError::InvalidCursor
            ) {
                result.with_failure_evidence(astra_core::ToolFailureEvidence::new(
                    astra_core::ErrorKind::ToolInvalidArgs,
                    astra_core::ToolFailureCause::InvalidArguments,
                    false,
                    vec![astra_core::ToolRecoveryAction::CorrectArguments],
                ))
            } else {
                result
            }
        }
    }
}

fn page_json(page: &ModelCatalogPage) -> String {
    page.to_json()
}

//! Authenticated on-demand Chat model discovery. No ordinary-turn catalog I/O.

use serde_json::Value;

use astra_turn_core::model_catalog::{
    CatalogError, ModelCatalogPage, ModelCatalogRequest, catalog_page, unavailable_page,
};

#[cfg(test)]
#[path = "tool_model_catalog_tests.rs"]
mod tests;

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
                match tokio::time::timeout(std::time::Duration::from_secs(5), reader.read()).await {
                    Ok(Ok(catalog)) => catalog_page(catalog.items, &request, scope),
                    Ok(Err((status, _))) => Err(match status {
                        axum::http::StatusCode::UNAUTHORIZED
                        | axum::http::StatusCode::FORBIDDEN => CatalogError::Unauthorized,
                        axum::http::StatusCode::NOT_IMPLEMENTED => CatalogError::Unsupported,
                        status
                            if status.is_server_error()
                                || status == axum::http::StatusCode::TOO_MANY_REQUESTS =>
                        {
                            CatalogError::Unavailable
                        }
                        _ => CatalogError::InvalidCatalog,
                    }),
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
        Err(error) => astra_tools::ToolResult::error(page_json(&unavailable_page(error, scope))),
    }
}

fn page_json(page: &ModelCatalogPage) -> String {
    page.to_json()
}

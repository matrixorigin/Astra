//! Version negotiation for the existing authorized model catalog.
//!
//! Unversioned published 0.2.12 consumers require the complete v2 field set.
//! Never infer a client version from identity or strip execution metadata.

use astra_core::{ErrorResponse, error_response_coded};
use astra_server_types::{
    MODEL_CATALOG_VERSION, MODEL_CATALOG_VERSION_HEADER, MODEL_CATALOG_VERSION_V2,
};
use astra_services::models::{
    ModelAccessProjectionResponse, ModelAccessProjectionResponseV2, ModelListPageResponse,
    ModelListPageResponseV2,
};
use axum::{
    Json,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Serialize;

/// Negotiated catalog response format. Version 2 is the frozen published
/// 0.2.12 contract; version 3 is the current production projection, which
/// adds `supported_reasoning_efforts` and newer thinking-protocol values.
#[derive(Clone, Copy)]
pub(super) enum CatalogVersion {
    V2,
    V3,
}

impl CatalogVersion {
    pub(super) fn from_headers(
        headers: &HeaderMap,
    ) -> Result<Self, (StatusCode, Json<ErrorResponse>)> {
        let mut values = headers.get_all(MODEL_CATALOG_VERSION_HEADER).iter();
        let first = values.next();
        if values.next().is_none() {
            match first.map(HeaderValue::as_bytes) {
                None => return Ok(Self::V2),
                Some(value) if value == MODEL_CATALOG_VERSION_V2.as_bytes() => {
                    return Ok(Self::V2);
                }
                Some(value) if value == MODEL_CATALOG_VERSION.as_bytes() => return Ok(Self::V3),
                _ => {}
            }
        }
        Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            "Unsupported model catalog response version. This server supports versions 2 and 3; update Astra to use its supported catalog format.",
            "model_catalog_version_unsupported",
        ))
    }

    fn response(self, version: &'static str, value: impl Serialize) -> Response {
        let mut response = Json(value).into_response();
        let headers = response.headers_mut();
        headers.insert(
            MODEL_CATALOG_VERSION_HEADER,
            HeaderValue::from_static(version),
        );
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        headers.append(
            header::VARY,
            HeaderValue::from_static(MODEL_CATALOG_VERSION_HEADER),
        );
        response
    }

    pub(super) fn models(self, page: ModelListPageResponse) -> Response {
        match self {
            Self::V2 => self.response(
                MODEL_CATALOG_VERSION_V2,
                ModelListPageResponseV2::from(page),
            ),
            Self::V3 => self.response(MODEL_CATALOG_VERSION, page),
        }
    }

    pub(super) fn access(self, page: ModelAccessProjectionResponse) -> Response {
        match self {
            Self::V2 => self.response(
                MODEL_CATALOG_VERSION_V2,
                ModelAccessProjectionResponseV2::from(page),
            ),
            Self::V3 => self.response(MODEL_CATALOG_VERSION, page),
        }
    }
}

//! Per-request trace id: `x-request-id` header echo + JSON error body enrichment.

use axum::body::Body;
use axum::extract::Request;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;
use http_body_util::BodyExt;
use uuid::Uuid;

/// Maximum length for a client-supplied `x-request-id` (after trim).
const MAX_REQUEST_ID_LEN: usize = 128;

/// Populated by [`request_trace_middleware`] for handlers that need explicit access.
#[derive(Clone, Debug)]
pub struct RequestTrace {
    pub request_id: String,
}

fn is_safe_request_id_token(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_REQUEST_ID_LEN
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')
        })
}

/// Resolve the effective request id: honor a safe client header, otherwise generate.
fn resolve_request_id(headers: &HeaderMap) -> String {
    let Some(raw) = headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Uuid::new_v4().to_string();
    };

    let truncated: String = raw.chars().take(MAX_REQUEST_ID_LEN).collect();
    if is_safe_request_id_token(&truncated) && HeaderValue::from_str(&truncated).is_ok() {
        truncated
    } else {
        Uuid::new_v4().to_string()
    }
}

/// Ensures every request has a `RequestTrace` extension and echoes `x-request-id`.
///
/// For `4xx`/`5xx` responses with `Content-Type: application/json`, if the body
/// deserializes as [`astra_core::ErrorResponse`] and `request_id` is unset, the
/// middleware injects the current request id so clients can correlate with logs.
pub async fn request_trace_middleware(mut req: Request, next: Next) -> Response {
    let request_id = resolve_request_id(req.headers());

    req.extensions_mut().insert(RequestTrace {
        request_id: request_id.clone(),
    });

    let mut res = next.run(req).await;

    if let Ok(val) = HeaderValue::from_str(&request_id) {
        res.headers_mut()
            .insert(axum::http::HeaderName::from_static("x-request-id"), val);
    }

    let status = res.status();
    if !status.is_client_error() && !status.is_server_error() {
        return res;
    }

    let is_json = res
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.to_ascii_lowercase().starts_with("application/json"))
        .unwrap_or(false);

    if !is_json {
        return res;
    }

    let (mut parts, body) = res.into_parts();
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    let collected = match body.collect().await {
        Ok(c) => c.to_bytes(),
        Err(e) => {
            tracing::warn!(
                target: "astra_runtime::request_trace",
                error = %e,
                status = %parts.status,
                "failed to buffer JSON error response body; returning synthetic payload"
            );
            let fallback = astra_core::ErrorResponse::new(
                "error response body could not be buffered for request tracing",
            )
            .with_error_code("body_buffer_failed")
            .with_request_id(request_id.clone());
            let bytes = serde_json::to_vec(&fallback).unwrap_or_else(|_| {
                br#"{"detail":"internal server error","error_code":"body_buffer_failed"}"#
                    .to_vec()
            });
            return Response::from_parts(parts, Body::from(bytes));
        }
    };

    if let Ok(mut err) = serde_json::from_slice::<astra_core::ErrorResponse>(&collected) {
        if err.request_id.is_none() {
            err.request_id = Some(request_id);
        }
        if let Ok(bytes) = serde_json::to_vec(&err) {
            return Response::from_parts(parts, Body::from(bytes));
        }
    }

    Response::from_parts(parts, Body::from(collected))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Json;
    use axum::http::StatusCode;
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    fn test_app() -> Router {
        Router::new()
            .route(
                "/err",
                get(|| async {
                    (
                        StatusCode::NOT_FOUND,
                        Json(astra_core::ErrorResponse::new("missing")),
                    )
                }),
            )
            .layer(axum::middleware::from_fn(request_trace_middleware))
    }

    #[tokio::test]
    async fn injects_request_id_into_json_error_body() {
        let app = test_app();
        let req = Request::builder()
            .uri("/err")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let hdr = res
            .headers()
            .get("x-request-id")
            .expect("x-request-id header")
            .to_str()
            .unwrap()
            .to_string();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["detail"], "missing");
        assert_eq!(v["request_id"].as_str().unwrap(), hdr);
    }

    #[tokio::test]
    async fn does_not_overwrite_existing_request_id_in_body() {
        let app = Router::new()
            .route(
                "/err",
                get(|| async {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(
                            astra_core::ErrorResponse::new("bad")
                                .with_request_id("client-rid-1"),
                        ),
                    )
                }),
            )
            .layer(axum::middleware::from_fn(request_trace_middleware));
        let req = Request::builder()
            .uri("/err")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["request_id"].as_str().unwrap(), "client-rid-1");
    }

    #[tokio::test]
    async fn long_client_request_id_is_truncated_and_echoed() {
        let app = test_app();
        let long = "a".repeat(200);
        let req = Request::builder()
            .uri("/err")
            .header("x-request-id", &long)
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        let rid = res.headers().get("x-request-id").unwrap().to_str().unwrap();
        assert_eq!(rid.len(), MAX_REQUEST_ID_LEN);
        assert!(rid.chars().all(|c| c == 'a'));
    }

    #[tokio::test]
    async fn unsafe_request_id_is_replaced() {
        let app = test_app();
        // Slashes are not allowed by [`is_safe_request_id_token`]; header value is still valid HTTP.
        let req = Request::builder()
            .uri("/err")
            .header("x-request-id", "bad/id")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        let rid = res.headers().get("x-request-id").unwrap().to_str().unwrap();
        assert!(uuid::Uuid::parse_str(rid).is_ok());
    }
}

//! Per-request trace id: `x-request-id` header echo + JSON error body enrichment.
//!
//! For `4xx`/`5xx` with `Content-Type: application/json`, the middleware may:
//! - Fill `request_id` on [`astra_core::ErrorResponse`] bodies, or
//! - Insert `request_id` on top-level JSON **objects** that omit it (generic errors).

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

/// `application/json` or `application/json; charset=...` without allocating a full lowercase copy.
fn response_media_type_is_json(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| {
            let mime = ct.split(';').next().unwrap_or("").trim();
            mime.eq_ignore_ascii_case("application/json")
        })
        .unwrap_or(false)
}

fn synthetic_error_bytes(request_id: &str, code: &'static str, detail: &'static str) -> Vec<u8> {
    let fb = astra_core::ErrorResponse::new(detail)
        .with_error_code(code)
        .with_request_id(request_id.to_string());
    serde_json::to_vec(&fb).unwrap_or_else(|_| {
        br#"{"detail":"internal server error","error_code":"body_buffer_failed"}"#.to_vec()
    })
}

fn json_body_needs_request_id(obj: &serde_json::Map<String, serde_json::Value>) -> bool {
    match obj.get("request_id") {
        None | Some(serde_json::Value::Null) => true,
        Some(serde_json::Value::String(s)) if s.is_empty() => true,
        _ => false,
    }
}

/// Returns `Some(new_body)` when the payload was rewritten; `None` to keep `collected` as-is.
fn try_enrich_json_body(collected: &[u8], request_id: &str) -> Option<Vec<u8>> {
    if let Ok(err_in) = serde_json::from_slice::<astra_core::ErrorResponse>(collected) {
        if err_in.request_id.is_some() {
            return None;
        }
        let mut err = err_in;
        err.request_id = Some(request_id.to_string());
        return match serde_json::to_vec(&err) {
            Ok(bytes) => Some(bytes),
            Err(e) => {
                tracing::warn!(
                    target: "astra_runtime::request_trace",
                    error = %e,
                    "serialize enriched ErrorResponse failed; using synthetic body"
                );
                Some(synthetic_error_bytes(
                    request_id,
                    "error_response_encode_failed",
                    "error response could not be re-encoded for request tracing",
                ))
            }
        };
    }

    let mut v: serde_json::Value = serde_json::from_slice(collected).ok()?;
    let obj = v.as_object_mut()?;
    if !json_body_needs_request_id(obj) {
        return None;
    }
    obj.insert(
        "request_id".into(),
        serde_json::Value::String(request_id.to_string()),
    );
    match serde_json::to_vec(&v) {
        Ok(bytes) => Some(bytes),
        Err(e) => {
            tracing::warn!(
                target: "astra_runtime::request_trace",
                error = %e,
                "serialize generic JSON error with request_id failed; using synthetic body"
            );
            Some(synthetic_error_bytes(
                request_id,
                "generic_json_encode_failed",
                "error response could not be re-encoded for request tracing",
            ))
        }
    }
}

/// Ensures every request has a `RequestTrace` extension and echoes `x-request-id`.
///
/// For `4xx`/`5xx` responses with `Content-Type: application/json`, attempts to attach
/// `request_id` to structured error bodies (see module docs).
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

    if !response_media_type_is_json(res.headers()) {
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
            let bytes = synthetic_error_bytes(
                &request_id,
                "body_buffer_failed",
                "error response body could not be buffered for request tracing",
            );
            return Response::from_parts(parts, Body::from(bytes));
        }
    };

    if let Some(bytes) = try_enrich_json_body(&collected, &request_id) {
        return Response::from_parts(parts, Body::from(bytes));
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
        let req = Request::builder()
            .uri("/err")
            .header("x-request-id", "bad/id")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        let rid = res.headers().get("x-request-id").unwrap().to_str().unwrap();
        assert!(uuid::Uuid::parse_str(rid).is_ok());
    }

    #[tokio::test]
    async fn skips_body_rewrite_on_success_json() {
        let app = Router::new()
            .route(
                "/ok",
                get(|| async { Json(serde_json::json!({ "status": "ok" })) }),
            )
            .layer(axum::middleware::from_fn(request_trace_middleware));
        let req = Request::builder()
            .uri("/ok")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(res.headers().get("x-request-id").is_some());
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v, serde_json::json!({ "status": "ok" }));
        assert!(v.get("request_id").is_none());
    }

    #[tokio::test]
    async fn injects_request_id_into_generic_json_object_error() {
        let app = Router::new()
            .route(
                "/err",
                get(|| async {
                    (
                        StatusCode::NOT_FOUND,
                        Json(serde_json::json!({ "message": "nope" })),
                    )
                }),
            )
            .layer(axum::middleware::from_fn(request_trace_middleware));
        let req = Request::builder()
            .uri("/err")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        let hdr = res
            .headers()
            .get("x-request-id")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["message"], "nope");
        assert_eq!(v["request_id"].as_str().unwrap(), hdr.as_str());
    }

    #[tokio::test]
    async fn skips_non_json_error_body() {
        let app = Router::new()
            .route(
                "/txt",
                get(|| async { (StatusCode::NOT_FOUND, "plain-not-found") }),
            )
            .layer(axum::middleware::from_fn(request_trace_middleware));
        let req = Request::builder()
            .uri("/txt")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&bytes[..], b"plain-not-found");
    }
}

//! Per-request trace id: `x-request-id` header echo + JSON error body enrichment.

use axum::body::Body;
use axum::extract::Request;
use axum::http::header::CONTENT_TYPE;
use axum::http::HeaderValue;
use axum::middleware::Next;
use axum::response::Response;
use http_body_util::BodyExt;
use uuid::Uuid;

/// Populated by [`request_trace_middleware`] for handlers that need explicit access.
#[derive(Clone, Debug)]
pub struct RequestTrace {
    pub request_id: String,
}

/// Ensures every request has a `RequestTrace` extension and echoes `x-request-id`.
///
/// For `4xx`/`5xx` responses with `Content-Type: application/json`, if the body
/// deserializes as [`astra_core::ErrorResponse`] and `request_id` is unset, the
/// middleware injects the current request id so clients can correlate with logs.
pub async fn request_trace_middleware(mut req: Request, next: Next) -> Response {
    let request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| Uuid::new_v4().to_string());

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
        Err(_) => {
            return Response::from_parts(parts, Body::empty());
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

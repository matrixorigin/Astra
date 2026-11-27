//! Per-request trace id: `x-request-id` header echo + JSON error body enrichment.
//!
//! For `4xx`/`5xx` with `Content-Type: application/json`, the middleware may:
//! - Fill `request_id` on [`astra_core::ErrorResponse`] bodies, or
//! - Insert `request_id` on top-level JSON **objects** that omit it (generic errors).

use std::borrow::Cow;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{MatchedPath, Request};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;
use http_body_util::BodyExt;
use sha2::{Digest, Sha256};
use tracing::Instrument;
use uuid::Uuid;

/// Maximum length for a client-supplied `x-request-id` (after trim).
const MAX_REQUEST_ID_LEN: usize = 128;

/// Routine successful polling is available at DEBUG; slow or failed calls
/// remain visible at INFO. This measures response-header latency, not the
/// lifetime of a streaming response.
fn quiet_success(
    method: &axum::http::Method,
    route: &str,
    status: axum::http::StatusCode,
    elapsed: Duration,
) -> bool {
    status.is_success()
        && elapsed < Duration::from_secs(1)
        && matches!(
            (method.as_str(), route),
            ("GET", "/health" | "/sessions/{session_id}/runs") | ("POST", "/agents/edge/heartbeat")
        )
}

fn log_method(method: &axum::http::Method) -> &'static str {
    match method.as_str() {
        "GET" => "GET",
        "POST" => "POST",
        "PUT" => "PUT",
        "PATCH" => "PATCH",
        "DELETE" => "DELETE",
        "HEAD" => "HEAD",
        "OPTIONS" => "OPTIONS",
        _ => "OTHER",
    }
}

/// Populated by [`request_trace_middleware`] for handlers that need explicit access.
#[derive(Clone, Debug)]
pub struct RequestTrace {
    pub request_id: String,
}

impl RequestTrace {
    pub(crate) fn log_id(&self) -> Cow<'_, str> {
        log_request_id(&self.request_id)
    }
}

/// Keep correlation without placing arbitrary client-provided IDs in logs.
pub(crate) fn log_request_id(request_id: &str) -> Cow<'_, str> {
    if request_id.is_empty() || Uuid::parse_str(request_id).is_ok() {
        Cow::Borrowed(request_id)
    } else {
        Cow::Owned(format!(
            "sha256:{:x}",
            Sha256::digest(request_id.as_bytes())
        ))
    }
}

fn is_safe_request_id_token(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_REQUEST_ID_LEN
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
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
/// Installs a per-request [`tracing::Span`] (`http.request`) with `request_id`, HTTP method,
/// and path so nested `tracing` events correlate with the request. Emits one access line per
/// request at target `astra.http.access` (latency + status).
///
/// For `4xx`/`5xx` responses with `Content-Type: application/json`, attempts to attach
/// `request_id` to structured error bodies (see module docs).
pub async fn request_trace_middleware(mut req: Request, next: Next) -> Response {
    let request_id = resolve_request_id(req.headers());
    let method = req.method().clone();
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map_or("<unmatched>", MatchedPath::as_str)
        .to_owned();
    let trace = RequestTrace {
        request_id: request_id.clone(),
    };
    let log_request_id = trace.log_id();
    req.extensions_mut().insert(trace.clone());

    let span = tracing::info_span!(
        "http.request",
        request_id = %log_request_id,
        http.method = log_method(&method),
        http.route = %route,
    );

    let start = Instant::now();
    let carrier = inbound_carrier(req.headers());
    let mut res = super::run_trace::scope_inbound(carrier, next.run(req).instrument(span)).await;
    let elapsed = start.elapsed();
    let latency_ms = elapsed.as_secs_f64() * 1000.0;

    if quiet_success(&method, &route, res.status(), elapsed) {
        tracing::debug!(
            target: "astra.http.access",
            request_id = %log_request_id,
            http.method = log_method(&method),
            http.route = %route,
            status = res.status().as_u16(),
            latency_ms,
            "http request"
        );
    } else {
        tracing::info!(
            target: "astra.http.access",
            request_id = %log_request_id,
            http.method = log_method(&method),
            http.route = %route,
            status = res.status().as_u16(),
            latency_ms,
            "http request"
        );
    }

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

fn inbound_carrier(headers: &axum::http::HeaderMap) -> astra_logging::trace_context::Carrier {
    use astra_logging::trace_context::Carrier;

    let traceparent = headers
        .get("traceparent")
        .and_then(|value| value.to_str().ok());
    match headers
        .get_all("tracestate")
        .iter()
        .map(|value| value.to_str())
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(fields) => Carrier::from_header_fields(traceparent, fields),
        Err(_) => Carrier::from_header_values(traceparent, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Json;
    use axum::Router;
    use axum::http::StatusCode;
    use axum::routing::get;
    use tower::ServiceExt;

    #[derive(Clone, Default)]
    struct AccessCapture(std::sync::Arc<std::sync::Mutex<Vec<(tracing::Level, String)>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for AccessCapture {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if matches!(
                event.metadata().target(),
                "astra.http.access" | "test.request_id"
            ) {
                struct Fields(String);
                impl tracing::field::Visit for Fields {
                    fn record_debug(
                        &mut self,
                        field: &tracing::field::Field,
                        value: &dyn std::fmt::Debug,
                    ) {
                        self.0.push_str(&format!("{}={value:?};", field.name()));
                    }
                }
                let mut fields = Fields(String::new());
                event.record(&mut fields);
                self.0
                    .lock()
                    .unwrap()
                    .push((*event.metadata().level(), fields.0));
            }
        }
    }

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

    #[test]
    fn repeated_tracestate_headers_combine() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "traceparent",
            "00-0123456789abcdef0123456789abcdef-0123456789abcdef-01"
                .parse()
                .unwrap(),
        );
        headers.append("tracestate", "rojo=00f067aa0ba902b7".parse().unwrap());
        headers.append("tracestate", "congo=t61rcWkgMzE".parse().unwrap());
        let carrier = inbound_carrier(&headers);
        assert!(carrier.traceparent.is_some());
        assert_eq!(
            carrier.tracestate.as_deref(),
            Some("rojo=00f067aa0ba902b7,congo=t61rcWkgMzE")
        );
    }

    #[test]
    fn only_fast_successful_polling_is_quiet() {
        use axum::http::Method;
        let fast = Duration::from_millis(5);
        let slow = Duration::from_secs(1);
        assert!(quiet_success(&Method::GET, "/health", StatusCode::OK, fast));
        assert!(quiet_success(
            &Method::GET,
            "/sessions/{session_id}/runs",
            StatusCode::OK,
            fast
        ));
        assert!(quiet_success(
            &Method::POST,
            "/agents/edge/heartbeat",
            StatusCode::OK,
            fast
        ));
        assert!(!quiet_success(
            &Method::GET,
            "/health",
            StatusCode::SERVICE_UNAVAILABLE,
            fast
        ));
        assert!(!quiet_success(
            &Method::GET,
            "/health",
            StatusCode::OK,
            slow
        ));
        assert!(!quiet_success(
            &Method::POST,
            "/health",
            StatusCode::OK,
            fast
        ));
        assert!(!quiet_success(
            &Method::GET,
            "/sessions/{session_id}/runs/admissions",
            StatusCode::NOT_IMPLEMENTED,
            fast
        ));
    }

    #[tokio::test]
    async fn routed_access_logs_are_bounded_and_keep_failures() {
        use tracing::instrument::WithSubscriber;
        use tracing_subscriber::prelude::*;
        let capture = AccessCapture::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        let app = Router::new()
            .route(
                "/sessions/{session_id}/runs",
                get(|| async { StatusCode::OK }),
            )
            .route("/health", get(|| async { StatusCode::SERVICE_UNAVAILABLE }))
            .layer(axum::middleware::from_fn(request_trace_middleware));
        async {
            for (method, path) in [
                ("GET", "/sessions/private-session/runs"),
                ("POST", "/sessions/private-session/runs"),
                ("GET", "/health"),
                ("GET", "/private-path"),
            ] {
                let request = Request::builder()
                    .method(method)
                    .uri(path)
                    .header("x-request-id", "sk-private-credential")
                    .body(Body::empty())
                    .unwrap();
                let response = app.clone().oneshot(request).await.unwrap();
                assert_eq!(response.headers()["x-request-id"], "sk-private-credential");
            }
        }
        .with_subscriber(subscriber)
        .await;
        let events = capture.0.lock().unwrap();
        assert_eq!(events.len(), 4);
        assert_eq!(events[0].0, tracing::Level::DEBUG);
        assert!(events[0].1.contains("sha256:"));
        assert!(
            events[0].1.contains("/sessions/{session_id}/runs"),
            "{:?}",
            events[0]
        );
        assert!(
            events[1..]
                .iter()
                .all(|event| event.0 == tracing::Level::INFO)
        );
        let all = format!("{events:?}");
        assert!(!all.contains("private-session"));
        assert!(!all.contains("private-path"));
        assert!(!all.contains("sk-private-credential"));
    }

    #[tokio::test]
    async fn handler_and_access_logs_share_sanitized_request_id() {
        use tracing::instrument::WithSubscriber;
        use tracing_subscriber::prelude::*;
        let capture = AccessCapture::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        let app = Router::new()
            .route("/test", get(|axum::Extension(trace): axum::Extension<RequestTrace>| async move {
                tracing::info!(target: "test.request_id", request_id = %trace.log_id(), "handler");
                StatusCode::OK
            }))
            .layer(axum::middleware::from_fn(request_trace_middleware));
        async {
            let response = app
                .oneshot(
                    Request::builder()
                        .uri("/test")
                        .header("x-request-id", "sk-private-credential")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.headers()["x-request-id"], "sk-private-credential");
        }
        .with_subscriber(subscriber)
        .await;
        let events = capture.0.lock().unwrap();
        let expected = log_request_id("sk-private-credential");
        assert_eq!(events.len(), 2);
        assert!(
            events
                .iter()
                .all(|event| event.1.contains(expected.as_ref()))
        );
        assert!(!format!("{events:?}").contains("sk-private-credential"));
    }

    #[tokio::test]
    async fn injects_request_id_into_json_error_body() {
        let app = test_app();
        let req = Request::builder().uri("/err").body(Body::empty()).unwrap();
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
                        Json(astra_core::ErrorResponse::new("bad").with_request_id("client-rid-1")),
                    )
                }),
            )
            .layer(axum::middleware::from_fn(request_trace_middleware));
        let req = Request::builder().uri("/err").body(Body::empty()).unwrap();
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
        let req = Request::builder().uri("/ok").body(Body::empty()).unwrap();
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
        let req = Request::builder().uri("/err").body(Body::empty()).unwrap();
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
        let req = Request::builder().uri("/txt").body(Body::empty()).unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&bytes[..], b"plain-not-found");
    }
}

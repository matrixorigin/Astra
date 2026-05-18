mod test_support;

use std::sync::Arc;

use astra_runtime::{AppState, HealthChecker, ServiceInfo, build_app, build_test_router};
use async_trait::async_trait;
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use tower::util::ServiceExt;

#[derive(Clone)]
struct AlwaysHealthy;

#[async_trait]
impl HealthChecker for AlwaysHealthy {
    async fn database_healthy(&self) -> bool {
        true
    }
}

fn build_guardrail_state() -> AppState {
    AppState::new(ServiceInfo::default(), Arc::new(AlwaysHealthy))
        .with_auth_service(Arc::new(astra_services::auth::StubAuthService))
}

async fn request_status(router: Router, request: Request<Body>) -> StatusCode {
    router.oneshot(request).await.unwrap().status()
}

fn request(method: &str, path: &str, headers: &[(&str, &str)], body: Body) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(body).unwrap()
}

#[tokio::test]
async fn build_app_keeps_core_http_and_websocket_surfaces_registered() {
    let app = build_app(build_guardrail_state());

    let root = request_status(app.clone(), request("GET", "/", &[], Body::empty())).await;
    assert_eq!(root, StatusCode::OK);

    let health = request_status(app.clone(), request("GET", "/health", &[], Body::empty())).await;
    assert_eq!(health, StatusCode::OK);

    let metrics = request_status(app.clone(), request("GET", "/metrics", &[], Body::empty())).await;
    assert_eq!(metrics, StatusCode::OK);

    let chat_ws = request_status(app.clone(), request("GET", "/chat/ws", &[], Body::empty())).await;
    assert_ne!(chat_ws, StatusCode::NOT_FOUND);

    let edge_ws = request_status(app.clone(), request("GET", "/edge/ws", &[], Body::empty())).await;
    assert_ne!(edge_ws, StatusCode::NOT_FOUND);

    let chat_stream = request_status(
        app.clone(),
        request(
            "POST",
            "/chat/stream",
            &[
                ("authorization", "Bearer test-token"),
                ("content-type", "application/json"),
            ],
            Body::from("{}"),
        ),
    )
    .await;
    assert_ne!(chat_stream, StatusCode::NOT_FOUND);

    let chat_turn = request_status(
        app,
        request(
            "POST",
            "/chat/turn",
            &[
                ("authorization", "Bearer test-token"),
                ("content-type", "application/json"),
                ("x-mo-bridge-test-secret", "guardrail-secret"),
            ],
            Body::from("{}"),
        ),
    )
    .await;
    assert_ne!(chat_turn, StatusCode::NOT_FOUND);

    let delegate = request_status(
        build_app(build_guardrail_state()),
        request(
            "POST",
            "/chat/runs/guardrail-run/delegate",
            &[
                ("authorization", "Bearer test-token"),
                ("content-type", "application/json"),
            ],
            Body::from("{}"),
        ),
    )
    .await;
    assert_ne!(delegate, StatusCode::NOT_FOUND);

    let oversized_chat_turn = request_status(
        build_app(build_guardrail_state()),
        request(
            "POST",
            "/chat/turn",
            &[
                ("authorization", "Bearer test-token"),
                ("content-type", "application/json"),
                ("x-mo-bridge-test-secret", "guardrail-secret"),
            ],
            Body::from(vec![b'a'; 4 * 1024 * 1024 + 1]),
        ),
    )
    .await;
    assert_eq!(
        oversized_chat_turn,
        StatusCode::PAYLOAD_TOO_LARGE,
        "build_app must keep the 4 MiB request body limit on /chat/turn"
    );
}

#[tokio::test]
async fn build_test_router_keeps_core_routes_reachable_for_oneshot_tests() {
    let app = build_test_router(build_guardrail_state());

    let root = request_status(app.clone(), request("GET", "/", &[], Body::empty())).await;
    assert_eq!(root, StatusCode::OK);

    let health = request_status(app.clone(), request("GET", "/health", &[], Body::empty())).await;
    assert_eq!(health, StatusCode::OK);

    let chat_ws = request_status(app.clone(), request("GET", "/chat/ws", &[], Body::empty())).await;
    assert_ne!(chat_ws, StatusCode::NOT_FOUND);

    let edge_ws = request_status(app, request("GET", "/edge/ws", &[], Body::empty())).await;
    assert_ne!(edge_ws, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn build_test_router_keeps_representative_domain_routes_registered() {
    let app = build_test_router(build_guardrail_state());
    let auth_headers = &[("authorization", "Bearer test-token")];

    let teams = request_status(
        app.clone(),
        request("GET", "/teams", auth_headers, Body::empty()),
    )
    .await;
    assert_ne!(teams, StatusCode::NOT_FOUND);

    let admin_init = request_status(
        app.clone(),
        request("POST", "/admin/init", auth_headers, Body::empty()),
    )
    .await;
    assert_ne!(admin_init, StatusCode::NOT_FOUND);

    let skills = request_status(
        app.clone(),
        request("GET", "/skills", auth_headers, Body::empty()),
    )
    .await;
    assert_ne!(skills, StatusCode::NOT_FOUND);

    let evaluation = request_status(
        app.clone(),
        request("GET", "/evaluation/drift", auth_headers, Body::empty()),
    )
    .await;
    assert_ne!(evaluation, StatusCode::NOT_FOUND);

    let introspection = request_status(
        app.clone(),
        request("GET", "/introspection/memory", auth_headers, Body::empty()),
    )
    .await;
    assert_ne!(introspection, StatusCode::NOT_FOUND);

    let tasks = request_status(
        app.clone(),
        request("GET", "/tasks", auth_headers, Body::empty()),
    )
    .await;
    assert_ne!(tasks, StatusCode::NOT_FOUND);

    let plans = request_status(
        app.clone(),
        request("GET", "/plans", auth_headers, Body::empty()),
    )
    .await;
    assert_ne!(plans, StatusCode::NOT_FOUND);

    let marketplace = request_status(
        app.clone(),
        request("GET", "/marketplace/search", auth_headers, Body::empty()),
    )
    .await;
    assert_ne!(marketplace, StatusCode::NOT_FOUND);

    let preferences = request_status(
        app,
        request("GET", "/preferences", auth_headers, Body::empty()),
    )
    .await;
    assert_ne!(preferences, StatusCode::NOT_FOUND);
}

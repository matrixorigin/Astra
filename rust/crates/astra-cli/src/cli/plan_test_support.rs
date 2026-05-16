use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use axum::extract::State as AxumState;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::post;
use axum::{Router, body::Body};
use tokio::net::TcpListener;

#[derive(Clone)]
struct ScriptedResponse {
    status: StatusCode,
    content_type: &'static str,
    body: String,
}

impl ScriptedResponse {
    fn sse(body: String) -> Self {
        Self {
            status: StatusCode::OK,
            content_type: "text/event-stream",
            body,
        }
    }
}

#[derive(Clone)]
struct ScriptedState {
    responses: Arc<Mutex<VecDeque<ScriptedResponse>>>,
}

async fn scripted_sse_handler(AxumState(state): AxumState<ScriptedState>) -> Response<Body> {
    let response = {
        let mut guard = state.responses.lock().expect("scripted responses lock");
        guard.pop_front().unwrap_or(ScriptedResponse {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            content_type: "text/plain; charset=utf-8",
            body: "unexpected extra /chat/turn request".to_string(),
        })
    };
    Response::builder()
        .status(response.status)
        .header("content-type", response.content_type)
        .header("cache-control", "no-cache")
        .body(Body::from(response.body))
        .expect("valid scripted response")
}

pub(crate) struct ScriptedSseServer {
    pub(crate) base_url: String,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

impl ScriptedSseServer {
    pub(crate) async fn start(responses: Vec<String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let app = Router::new()
            .route("/chat/turn", post(scripted_sse_handler))
            .with_state(ScriptedState {
                responses: Arc::new(Mutex::new(
                    responses
                        .into_iter()
                        .map(ScriptedResponse::sse)
                        .collect::<VecDeque<_>>(),
                )),
            });
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = rx.await;
                })
                .await
                .ok();
        });
        tokio::task::yield_now().await;
        Self {
            base_url: format!("http://127.0.0.1:{}", addr.port()),
            _shutdown: tx,
        }
    }
}

pub(crate) fn sse_line(event: serde_json::Value) -> String {
    format!("data: {event}\n\n")
}

pub(crate) fn sse_text_response(full_text: &str) -> String {
    let mut out = String::new();
    out.push_str(&sse_line(serde_json::json!({
        "type": "session_info",
        "session_id": "mock-session",
        "run_id": "mock-run-1",
    })));
    out.push_str(&sse_line(serde_json::json!({
        "type": "text_delta",
        "content": full_text,
    })));
    out.push_str(&sse_line(serde_json::json!({
        "type": "text_done",
        "full_text": full_text,
    })));
    out.push_str(&sse_line(serde_json::json!({
        "type": "usage",
        "input_tokens": 10,
        "cached_input_tokens": 0,
        "cache_creation_tokens": 0,
        "output_tokens": 20,
        "total_tokens": 30,
    })));
    out.push_str(&sse_line(serde_json::json!({
        "type": "done",
        "tokens_used": 30,
        "usage": {
            "input_tokens": 10,
            "cached_input_tokens": 0,
            "cache_creation_tokens": 0,
            "output_tokens": 20,
            "total_tokens": 30,
        }
    })));
    out
}

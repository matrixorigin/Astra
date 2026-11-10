use std::{
    collections::HashMap,
    convert::Infallible,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{Method, Response, StatusCode, header},
    routing::any,
};
use bytes::Bytes;
use futures_util::{StreamExt, stream};
use rmcp::{
    model::{ClientJsonRpcMessage, ClientRequest, PingRequest, RequestId, TaskSupport},
    transport::streamable_http_client::{
        StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
    },
};
use serde_json::{Value, json};

use crate::streamable_http::{
    BoundedHttpClient, MAX_ERROR_BYTES, MAX_JSON_BYTES, MAX_SSE_EVENT_BYTES,
};
use crate::{McpClientManager, McpError, McpServerConfig, RetryConfig, Transport};

#[derive(Default)]
struct Fixture {
    calls: AtomicUsize,
}

fn response(status: StatusCode, content_type: &str, body: Body) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .body(body)
        .unwrap()
}

fn never_ending(data: Vec<u8>) -> Body {
    Body::from_stream(stream::iter([Ok::<_, Infallible>(data)]).chain(stream::pending()))
}

async fn handler(State(fixture): State<Arc<Fixture>>, request: Request) -> Response<Body> {
    let path = request.uri().path().to_owned();
    if path == "/mcp" {
        if request
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            != Some("Bearer test-token")
            || request
                .headers()
                .get("x-test")
                .and_then(|v| v.to_str().ok())
                != Some("present")
        {
            return response(StatusCode::UNAUTHORIZED, "text/plain", Body::empty());
        }
        if request.method() == Method::GET || request.method() == Method::DELETE {
            return response(StatusCode::METHOD_NOT_ALLOWED, "text/plain", Body::empty());
        }
        let bytes = to_bytes(request.into_body(), 1024 * 1024).await.unwrap();
        let message: Value = serde_json::from_slice(&bytes).unwrap();
        let id = message["id"].clone();
        let result = match message["method"].as_str().unwrap() {
            "initialize" => {
                json!({"protocolVersion":"2025-03-26", "capabilities":{"tools":{}}, "serverInfo":{"name":"fixture", "version":"1"}})
            }
            "notifications/initialized" => {
                return response(StatusCode::ACCEPTED, "text/plain", Body::empty());
            }
            "tools/list" => {
                json!({"tools":[{"name":"test", "inputSchema":{"type":"object"}, "execution":{"taskSupport":"optional"}}]})
            }
            "tools/call" => {
                if fixture.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    json!({"content":[{"type":"text", "text":"ok"}]})
                } else {
                    return response(
                        StatusCode::OK,
                        "application/json",
                        never_ending(vec![b' '; MAX_JSON_BYTES + 1]),
                    );
                }
            }
            other => panic!("unexpected method: {other}"),
        };
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .header("mcp-session-id", "session-1")
            .body(Body::from(
                json!({"jsonrpc":"2.0", "id":id, "result":result}).to_string(),
            ))
            .unwrap();
    }
    match path.as_str() {
        "/large-length" => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::CONTENT_LENGTH, MAX_JSON_BYTES + 1)
            .body(Body::from_stream(stream::pending::<
                Result<Vec<u8>, Infallible>,
            >()))
            .unwrap(),
        "/large-json" => response(
            StatusCode::OK,
            "application/json",
            never_ending(vec![b' '; MAX_JSON_BYTES + 1]),
        ),
        "/large-error" => response(
            StatusCode::BAD_REQUEST,
            "text/plain",
            never_ending(vec![b'x'; MAX_ERROR_BYTES + 1]),
        ),
        "/rpc-error" => response(
            StatusCode::BAD_REQUEST,
            "application/json",
            Body::from(
                r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"bad parameters"}}"#,
            ),
        ),
        "/bad-json" => response(StatusCode::OK, "application/json", Body::from("invalid")),
        "/unauthorized" => Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header(header::WWW_AUTHENTICATE, "Bearer realm=fixture")
            .body(Body::empty())
            .unwrap(),
        "/forbidden" => Response::builder()
            .status(StatusCode::FORBIDDEN)
            .header(header::WWW_AUTHENTICATE, "Bearer scope=\"read write\"")
            .body(Body::empty())
            .unwrap(),
        "/missing" => response(StatusCode::NOT_FOUND, "text/plain", Body::empty()),
        "/unsupported" => response(StatusCode::METHOD_NOT_ALLOWED, "text/plain", Body::empty()),
        "/sse" => {
            let mut data = b"data: one\r\n\r\ndata: two\n\n".to_vec();
            data.extend_from_slice(b"data: ");
            data.extend(vec![b'x'; MAX_SSE_EVENT_BYTES + 1]);
            response(StatusCode::OK, "text/event-stream", never_ending(data))
        }
        _ => response(StatusCode::NOT_FOUND, "text/plain", Body::empty()),
    }
}

async fn server() -> (String, Arc<Fixture>, tokio::task::JoinHandle<()>) {
    let fixture = Arc::new(Fixture::default());
    let router = Router::new()
        .fallback(any(handler))
        .with_state(fixture.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (url, fixture, task)
}

fn ping() -> ClientJsonRpcMessage {
    ClientJsonRpcMessage::request(
        ClientRequest::PingRequest(PingRequest::default()),
        RequestId::Number(1),
    )
}

async fn post(
    client: &BoundedHttpClient,
    url: String,
    session: Option<&str>,
) -> Result<StreamableHttpPostResponse, StreamableHttpError<rmcp_http::Error>> {
    tokio::time::timeout(
        Duration::from_secs(5),
        client.post_message(
            Arc::from(url),
            ping(),
            session.map(Arc::from),
            None,
            HashMap::new(),
        ),
    )
    .await
    .expect("response limit must be enforced before EOF")
}

#[tokio::test]
async fn http_bodies_are_bounded_without_losing_protocol_errors() {
    let (base, _, task) = server().await;
    let client = BoundedHttpClient::new().unwrap();
    for path in ["large-length", "large-json", "large-error"] {
        let error = post(&client, format!("{base}/{path}"), None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("limit"), "{path}: {error}");
    }
    assert!(matches!(
        post(&client, format!("{base}/rpc-error"), None)
            .await
            .unwrap(),
        StreamableHttpPostResponse::Json(rmcp::model::JsonRpcMessage::Error(_), _)
    ));
    assert!(matches!(
        post(&client, format!("{base}/bad-json"), None)
            .await
            .unwrap(),
        StreamableHttpPostResponse::Accepted
    ));
    assert!(matches!(
        post(&client, format!("{base}/unauthorized"), None)
            .await
            .unwrap_err(),
        StreamableHttpError::AuthRequired(_)
    ));
    match post(&client, format!("{base}/forbidden"), None)
        .await
        .unwrap_err()
    {
        StreamableHttpError::InsufficientScope(error) => {
            assert_eq!(error.required_scope.as_deref(), Some("read write"))
        }
        other => panic!("unexpected error: {other}"),
    }
    assert!(matches!(
        post(&client, format!("{base}/missing"), Some("old"))
            .await
            .unwrap_err(),
        StreamableHttpError::SessionExpired
    ));
    let url: Arc<str> = Arc::from(format!("{base}/unsupported"));
    assert!(matches!(
        client
            .get_stream(
                url.clone(),
                Arc::from("session"),
                None,
                None,
                HashMap::new()
            )
            .await
            .err()
            .unwrap(),
        StreamableHttpError::ServerDoesNotSupportSse
    ));
    client
        .delete_session(url, Arc::from("session"), None, HashMap::new())
        .await
        .unwrap();
    let mut headers = HashMap::new();
    headers.insert(header::ACCEPT, "text/plain".parse().unwrap());
    assert!(matches!(
        client
            .post_message(Arc::from(base), ping(), None, None, headers)
            .await
            .unwrap_err(),
        StreamableHttpError::ReservedHeaderConflict(_)
    ));
    task.abort();
}

#[tokio::test]
async fn sse_events_are_bounded_individually() {
    let (base, _, task) = server().await;
    let client = BoundedHttpClient::new().unwrap();
    let mut events = client
        .get_stream(
            Arc::from(format!("{base}/sse")),
            Arc::from("session"),
            None,
            None,
            HashMap::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        events.next().await.unwrap().unwrap().data.as_deref(),
        Some("one")
    );
    assert_eq!(
        events.next().await.unwrap().unwrap().data.as_deref(),
        Some("two")
    );
    let error = tokio::time::timeout(Duration::from_secs(5), events.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("limit"), "{error}");
    task.abort();
}

#[tokio::test]
async fn oversized_sse_chunk_keeps_earlier_complete_events() {
    let mut bytes = b"data: first\n\ndata: ".to_vec();
    bytes.extend(vec![b'x'; MAX_SSE_EVENT_BYTES + 1]);
    let source = stream::iter([Ok::<_, std::io::Error>(Bytes::from(bytes))]);
    let mut events = super::event_stream(source);
    assert_eq!(
        events.next().await.unwrap().unwrap().data.as_deref(),
        Some("first")
    );
    assert!(
        events
            .next()
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("limit")
    );
}

#[tokio::test]
async fn fragmented_sse_keeps_comments_crlf_and_multiline_messages() {
    let chunks = vec![
        Bytes::from_static(b"\xef"),
        Bytes::from_static(b"\xbb\xbf: keepalive\r"),
        Bytes::from_static(b"\ndata: first\r"),
        Bytes::from_static(b"\ndata: second\r\n\r"),
        Bytes::from_static(b"\n"),
        Bytes::from_static(b"data: next\n\n"),
    ];
    let source = stream::iter(chunks.into_iter().map(Ok::<_, std::io::Error>));
    let mut events = super::event_stream(source);
    assert_eq!(
        events.next().await.unwrap().unwrap().data.as_deref(),
        Some("first\nsecond")
    );
    assert_eq!(
        events.next().await.unwrap().unwrap().data.as_deref(),
        Some("next")
    );
    assert!(events.next().await.is_none());
}

#[tokio::test]
async fn public_connection_preserves_discovery_and_reports_unknown_after_dispatch() {
    let (base, fixture, task) = server().await;
    let config = McpServerConfig {
        name: "fixture".into(),
        transport: Transport::StreamableHttp {
            url: format!("{base}/mcp"),
            auth_token: Some("test-token".into()),
            headers: HashMap::from([("x-test".into(), "present".into())]),
        },
        description: String::new(),
        enabled: true,
        retry: RetryConfig {
            max_retries: 0,
            initial_delay_ms: 1,
            max_delay_ms: 1,
        },
    };
    let prepared = McpClientManager::prepare_connection(config, Default::default())
        .await
        .unwrap()
        .unwrap();
    let connection = prepared.connection();
    assert_eq!(
        connection.tools()[0]
            .execution
            .as_ref()
            .unwrap()
            .task_support,
        Some(TaskSupport::Optional)
    );
    assert!(connection.call_tool("test", json!({})).await.is_ok());
    let error = tokio::time::timeout(
        Duration::from_secs(5),
        connection.call_tool("test", json!({})),
    )
    .await
    .expect("must reject response before EOF")
    .unwrap_err();
    assert!(McpError::Service(error).side_effects_maybe());
    assert_eq!(
        fixture.calls.load(Ordering::SeqCst),
        2,
        "no duplicate tool dispatch"
    );
    task.abort();
}

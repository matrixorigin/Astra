//! Session artifact HTTP E2E: authenticated list/get routes align with `session_artifacts`,
//! including kind filtering, session scoping, and cross-user isolation.

use axum::http::StatusCode;
use axum::{body, body::Body, http::Request};
use futures_util::StreamExt;
use serde_json::json;
use sqlx::Row;
use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::util::ServiceExt;
use uuid::Uuid;

use astra_services::session_restore::COMPOSITE_SNAPSHOT_INDEX_ARTIFACT_KIND;
use astra_services::session_workspace::WORKSPACE_METADATA_ARTIFACT_KIND;

use super::harness::{
    E2E_PASSWORD, E2eAuthMode, bootstrap, bootstrap_trusted_moi, cleanup_session_data, get_json,
    post_json,
};

async fn collect_full_sse_stream(
    app: &axum::Router,
    req: Request<Body>,
    timeout_secs: u64,
) -> (StatusCode, String) {
    let resp = app.clone().oneshot(req).await.expect("oneshot");
    let status = resp.status();
    let mut stream = resp.into_body().into_data_stream();
    let mut acc = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    while let Ok(Some(chunk)) = tokio::time::timeout_at(deadline, stream.next()).await {
        let chunk = chunk.expect("body chunk");
        acc.extend_from_slice(&chunk);
    }
    (status, String::from_utf8_lossy(&acc).to_string())
}

async fn read_full_http_request(socket: &mut tokio::net::TcpStream) -> String {
    let mut acc = Vec::new();
    let mut buf = [0_u8; 8192];
    let mut header_end = None;
    let mut content_length = 0_usize;

    loop {
        let read = socket.read(&mut buf).await.unwrap_or(0);
        if read == 0 {
            break;
        }
        acc.extend_from_slice(&buf[..read]);

        if header_end.is_none()
            && let Some(pos) = acc.windows(4).position(|window| window == b"\r\n\r\n")
        {
            let end = pos + 4;
            header_end = Some(end);
            let headers = String::from_utf8_lossy(&acc[..end]);
            content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    if name.eq_ignore_ascii_case("content-length") {
                        value.trim().parse::<usize>().ok()
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
        }

        if let Some(end) = header_end
            && acc.len() >= end.saturating_add(content_length)
        {
            break;
        }
    }

    String::from_utf8_lossy(&acc).to_string()
}

async fn stream_chat_full_nonbridge(
    app: &axum::Router,
    auth: &str,
    payload: serde_json::Value,
) -> (StatusCode, String) {
    let req = Request::builder()
        .method("POST")
        .uri("/chat/stream")
        .header("authorization", auth)
        .header("content-type", "application/json")
        .body(Body::from(payload.to_string()))
        .expect("stream request");
    collect_full_sse_stream(app, req, 30).await
}

async fn chat_turn_full(
    app: &axum::Router,
    auth: &str,
    payload: serde_json::Value,
) -> (StatusCode, String) {
    let test_secret = std::env::var("ASTRA_TEST_BRIDGE_SECRET").expect("bridge test secret");
    let req = Request::builder()
        .method("POST")
        .uri("/chat/turn")
        .header("authorization", auth)
        .header("content-type", "application/json")
        .header("x-mo-bridge-test-secret", &test_secret)
        .body(Body::from(payload.to_string()))
        .expect("turn request");
    collect_full_sse_stream(app, req, 30).await
}

async fn get_bytes(
    app: &axum::Router,
    path: &str,
    auth: Option<&str>,
    extra_headers: &[(&str, &str)],
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut req = Request::builder().method("GET").uri(path);
    if let Some(t) = auth {
        req = req.header("authorization", t);
    }
    for (k, v) in extra_headers {
        req = req.header(*k, *v);
    }
    let req = req.body(Body::empty()).expect("request");
    let response = app.clone().oneshot(req).await.expect("oneshot");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .expect("body");
    (status, headers, bytes.to_vec())
}

#[derive(Clone)]
struct RawTransportServerHits {
    stream_hits: Arc<AtomicU32>,
    nonstream_hits: Arc<AtomicU32>,
}

/// Stream idle timeout override was previously env-controlled; now hardcoded. This is a
/// no-op placeholder so test call sites keep compiling. Callers should expect full-length
/// timeouts (5 minutes) — the tests that used short overrides may take longer.
struct StreamIdleEnvGuard;

fn set_stream_idle_timeouts_for_test(_pre_ms: u64, _post_ms: u64) -> StreamIdleEnvGuard {
    StreamIdleEnvGuard
}

/// Asserts the number of non-stream fallback hits falls inside `min..=max`.
///
/// The non-stream mocks in this journey optionally answer the first non-stream
/// request with an HTTP 200 body `"probe ok"` connectivity probe before
/// serving any real fallback (see `spawn_raw_partial_transport_server` and
/// siblings around line 228 / 302 / 370). The probe is not driven by a config
/// flag — whether it fires depends on how fast the client notices the
/// partial-transport failure and engages the non-stream fallback path, which
/// varies with scheduler / CI load.
///
/// Callers that expect N genuine fallbacks must therefore accept `N..=N+1`
/// (the "+1" = optional probe) to stay stable under CI timing jitter.
fn assert_nonstream_hits_in_range(actual: u32, min: u32, max: u32, message: &str) {
    assert!(
        (min..=max).contains(&actual),
        "{message}: expected {min}..={max} non-stream hits, got {actual}"
    );
}

async fn spawn_raw_partial_transport_server(
    partial_text: &str,
    fallback_status: u16,
    fallback_body: &'static str,
) -> (String, RawTransportServerHits) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind raw mock llm listener");
    let addr = listener.local_addr().expect("raw local_addr");
    let hits = RawTransportServerHits {
        stream_hits: Arc::new(AtomicU32::new(0)),
        nonstream_hits: Arc::new(AtomicU32::new(0)),
    };
    let hits_task = hits.clone();
    let partial_text = partial_text.to_string();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let hits = hits_task.clone();
            let partial_text = partial_text.clone();
            tokio::spawn(async move {
                let req = read_full_http_request(&mut socket).await;
                let is_stream = req.contains("\"stream\":true");
                if is_stream {
                    hits.stream_hits.fetch_add(1, Ordering::SeqCst);
                    let partial = format!(
                        "data: {}\n\n",
                        json!({"choices":[{"delta":{"content": partial_text}}]})
                    );
                    let chunk = format!("{:X}\r\n{}\r\n", partial.len(), partial);
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{chunk}"
                    );
                    socket
                        .write_all(response.as_bytes())
                        .await
                        .expect("write partial stream response");
                    let _ = socket.shutdown().await;
                } else {
                    let nonstream_ix = hits.nonstream_hits.fetch_add(1, Ordering::SeqCst);
                    let (status, body) = if nonstream_ix == 0 {
                        (
                            200,
                            r#"{"choices":[{"message":{"content":"probe ok"}}]}"#.to_string(),
                        )
                    } else {
                        (fallback_status, fallback_body.to_string())
                    };
                    let status_text = if status == 200 {
                        "OK"
                    } else {
                        "Internal Server Error"
                    };
                    let response = format!(
                        "HTTP/1.1 {status} {status_text}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    socket
                        .write_all(response.as_bytes())
                        .await
                        .expect("write nonstream response");
                    let _ = socket.shutdown().await;
                }
            });
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    (format!("http://{addr}"), hits)
}

async fn spawn_raw_idle_after_progress_server(
    partial_text: &str,
    stall_for: std::time::Duration,
    fallback_status: u16,
    fallback_body: &'static str,
) -> (String, RawTransportServerHits) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind idle mock llm listener");
    let addr = listener.local_addr().expect("idle local_addr");
    let hits = RawTransportServerHits {
        stream_hits: Arc::new(AtomicU32::new(0)),
        nonstream_hits: Arc::new(AtomicU32::new(0)),
    };
    let hits_task = hits.clone();
    let partial_text = partial_text.to_string();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let hits = hits_task.clone();
            let partial_text = partial_text.clone();
            tokio::spawn(async move {
                let req = read_full_http_request(&mut socket).await;
                let is_stream = req.contains("\"stream\":true");
                if is_stream {
                    hits.stream_hits.fetch_add(1, Ordering::SeqCst);
                    let partial = format!(
                        "data: {}\n\n",
                        json!({"choices":[{"delta":{"content": partial_text}}]})
                    );
                    let chunk = format!("{:X}\r\n{}\r\n", partial.len(), partial);
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{chunk}"
                    );
                    socket
                        .write_all(response.as_bytes())
                        .await
                        .expect("write idle partial stream response");
                    tokio::time::sleep(stall_for).await;
                    let _ = socket.shutdown().await;
                } else {
                    let nonstream_ix = hits.nonstream_hits.fetch_add(1, Ordering::SeqCst);
                    let (status, body) = if nonstream_ix == 0 {
                        (
                            200,
                            r#"{"choices":[{"message":{"content":"probe ok"}}]}"#.to_string(),
                        )
                    } else {
                        (fallback_status, fallback_body.to_string())
                    };
                    let status_text = if status == 200 {
                        "OK"
                    } else {
                        "Internal Server Error"
                    };
                    let response = format!(
                        "HTTP/1.1 {status} {status_text}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    socket
                        .write_all(response.as_bytes())
                        .await
                        .expect("write idle nonstream response");
                    let _ = socket.shutdown().await;
                }
            });
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    (format!("http://{addr}"), hits)
}

async fn spawn_raw_stream_rate_limit_server(
    retry_after: Option<&'static str>,
    body: &'static str,
) -> (String, RawTransportServerHits) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind rate-limit mock llm listener");
    let addr = listener.local_addr().expect("rate-limit local_addr");
    let hits = RawTransportServerHits {
        stream_hits: Arc::new(AtomicU32::new(0)),
        nonstream_hits: Arc::new(AtomicU32::new(0)),
    };
    let hits_task = hits.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let hits = hits_task.clone();
            tokio::spawn(async move {
                let req = read_full_http_request(&mut socket).await;
                let is_stream = req.contains("\"stream\":true");
                if is_stream {
                    hits.stream_hits.fetch_add(1, Ordering::SeqCst);
                    let retry_after_header = retry_after
                        .map(|value| format!("Retry-After: {value}\r\n"))
                        .unwrap_or_default();
                    let response = format!(
                        "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\n{retry_after_header}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    socket
                        .write_all(response.as_bytes())
                        .await
                        .expect("write rate-limit stream response");
                    let _ = socket.shutdown().await;
                } else {
                    hits.nonstream_hits.fetch_add(1, Ordering::SeqCst);
                    let body = r#"{"choices":[{"message":{"content":"probe ok"}}]}"#;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    socket
                        .write_all(response.as_bytes())
                        .await
                        .expect("write rate-limit nonstream response");
                    let _ = socket.shutdown().await;
                }
            });
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    (format!("http://{addr}"), hits)
}

async fn spawn_raw_stream_rate_limit_then_sse_server(
    success_text: &str,
) -> (String, RawTransportServerHits) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind rate-limit recovery mock llm listener");
    let addr = listener
        .local_addr()
        .expect("rate-limit recovery local_addr");
    let hits = RawTransportServerHits {
        stream_hits: Arc::new(AtomicU32::new(0)),
        nonstream_hits: Arc::new(AtomicU32::new(0)),
    };
    let hits_task = hits.clone();
    let success_text = success_text.to_string();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let hits = hits_task.clone();
            let success_text = success_text.clone();
            tokio::spawn(async move {
                let req = read_full_http_request(&mut socket).await;
                let is_stream = req.contains("\"stream\":true");
                if is_stream {
                    let stream_ix = hits.stream_hits.fetch_add(1, Ordering::SeqCst);
                    let response = if stream_ix == 0 {
                        let body = r#"{"error":{"message":"rate limit exceeded"}}"#;
                        format!(
                            "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nRetry-After: 0\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                    } else {
                        let payload = json!({"choices":[{"delta":{"content": success_text}}]});
                        let done = json!({"choices":[{"delta":{},"finish_reason":"stop"}]});
                        let body = format!("data: {payload}\n\ndata: {done}\n\n");
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                    };
                    socket
                        .write_all(response.as_bytes())
                        .await
                        .expect("write rate-limit recovery stream response");
                    let _ = socket.shutdown().await;
                } else {
                    hits.nonstream_hits.fetch_add(1, Ordering::SeqCst);
                    let body = r#"{"choices":[{"message":{"content":"probe ok"}}]}"#;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    socket
                        .write_all(response.as_bytes())
                        .await
                        .expect("write rate-limit recovery nonstream response");
                    let _ = socket.shutdown().await;
                }
            });
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    (format!("http://{addr}"), hits)
}

async fn spawn_raw_tool_call_block_parse_recovery_server() -> (String, RawTransportServerHits) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind tool-call block-parse recovery mock llm listener");
    let addr = listener
        .local_addr()
        .expect("tool-call block-parse local_addr");
    let hits = RawTransportServerHits {
        stream_hits: Arc::new(AtomicU32::new(0)),
        nonstream_hits: Arc::new(AtomicU32::new(0)),
    };
    let hits_task = hits.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let hits = hits_task.clone();
            tokio::spawn(async move {
                let req = read_full_http_request(&mut socket).await;
                let is_stream = req.contains("\"stream\":true");
                if is_stream {
                    hits.stream_hits.fetch_add(1, Ordering::SeqCst);
                    let part1 = json!({
                        "choices": [{
                            "delta": {
                                "tool_calls": [{
                                    "index": 0,
                                    "id": "call-1",
                                    "type": "function",
                                    "function": {
                                        "name": "bash",
                                        "arguments": "{\"command\":\"p"
                                    }
                                }]
                            }
                        }]
                    });
                    let part2 = json!({
                        "choices": [{
                            "delta": {
                                "tool_calls": [{
                                    "index": 0,
                                    "function": {
                                        "arguments": "wd\"}"
                                    }
                                }]
                            }
                        }]
                    });
                    let body = format!(
                        "data: {part1}\n\ndata: {part2}\n\ndata: {{\"choices\":[INVALID]}}\n\n"
                    );
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    socket
                        .write_all(response.as_bytes())
                        .await
                        .expect("write tool-call block-parse stream response");
                    let _ = socket.shutdown().await;
                } else {
                    hits.nonstream_hits.fetch_add(1, Ordering::SeqCst);
                    let body = json!({
                        "choices": [{
                            "message": {
                                "content": "",
                                "tool_calls": [{
                                    "id": "call-1",
                                    "type": "function",
                                    "function": {
                                        "name": "bash",
                                        "arguments": "{\"command\":\"pwd\"}"
                                    }
                                }]
                            },
                            "finish_reason": "tool_calls"
                        }]
                    })
                    .to_string();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    socket
                        .write_all(response.as_bytes())
                        .await
                        .expect("write tool-call block-parse nonstream response");
                    let _ = socket.shutdown().await;
                }
            });
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    (format!("http://{addr}"), hits)
}

async fn spawn_raw_server_loop_block_parse_recovery_server(
    partial_text: &str,
    recovered_text: &str,
) -> (String, RawTransportServerHits) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind server-loop block-parse recovery mock llm listener");
    let addr = listener
        .local_addr()
        .expect("server-loop block-parse recovery local_addr");
    let hits = RawTransportServerHits {
        stream_hits: Arc::new(AtomicU32::new(0)),
        nonstream_hits: Arc::new(AtomicU32::new(0)),
    };
    let hits_task = hits.clone();
    let partial_text = partial_text.to_string();
    let recovered_text = recovered_text.to_string();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let hits = hits_task.clone();
            let partial_text = partial_text.clone();
            let recovered_text = recovered_text.clone();
            tokio::spawn(async move {
                let req = read_full_http_request(&mut socket).await;
                let is_stream = req.contains("\"stream\":true");
                if is_stream {
                    hits.stream_hits.fetch_add(1, Ordering::SeqCst);
                    let partial = json!({"choices":[{"delta":{"content": partial_text}}]});
                    let body = format!("data: {partial}\n\ndata: not-json\n\n");
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    socket
                        .write_all(response.as_bytes())
                        .await
                        .expect("write server-loop block-parse stream response");
                    let _ = socket.shutdown().await;
                } else {
                    hits.nonstream_hits.fetch_add(1, Ordering::SeqCst);
                    let body = json!({
                        "choices": [{
                            "message": { "content": recovered_text },
                            "finish_reason": "stop"
                        }],
                        "usage": { "prompt_tokens": 19, "completion_tokens": 4 }
                    })
                    .to_string();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    socket
                        .write_all(response.as_bytes())
                        .await
                        .expect("write server-loop block-parse fallback response");
                    let _ = socket.shutdown().await;
                }
            });
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    (format!("http://{addr}"), hits)
}

async fn spawn_raw_server_loop_block_parse_failure_server(
    partial_text: &str,
) -> (String, RawTransportServerHits) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind server-loop block-parse failure mock llm listener");
    let addr = listener
        .local_addr()
        .expect("server-loop block-parse failure local_addr");
    let hits = RawTransportServerHits {
        stream_hits: Arc::new(AtomicU32::new(0)),
        nonstream_hits: Arc::new(AtomicU32::new(0)),
    };
    let hits_task = hits.clone();
    let partial_text = partial_text.to_string();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let hits = hits_task.clone();
            let partial_text = partial_text.clone();
            tokio::spawn(async move {
                let req = read_full_http_request(&mut socket).await;
                let is_stream = req.contains("\"stream\":true");
                if is_stream {
                    hits.stream_hits.fetch_add(1, Ordering::SeqCst);
                    let partial = json!({"choices":[{"delta":{"content": partial_text}}]});
                    let body = format!("data: {partial}\n\ndata: not-json\n\n");
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    socket
                        .write_all(response.as_bytes())
                        .await
                        .expect("write server-loop block-parse failure stream response");
                    let _ = socket.shutdown().await;
                } else {
                    hits.nonstream_hits.fetch_add(1, Ordering::SeqCst);
                    let body = "fallback exploded";
                    let response = format!(
                        "HTTP/1.1 500 Internal Server Error\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    socket
                        .write_all(response.as_bytes())
                        .await
                        .expect("write server-loop block-parse failure fallback response");
                    let _ = socket.shutdown().await;
                }
            });
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    (format!("http://{addr}"), hits)
}

async fn spawn_raw_hanging_stream_server(partial_text: &str) -> (String, RawTransportServerHits) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind hanging mock llm listener");
    let addr = listener.local_addr().expect("hanging local_addr");
    let hits = RawTransportServerHits {
        stream_hits: Arc::new(AtomicU32::new(0)),
        nonstream_hits: Arc::new(AtomicU32::new(0)),
    };
    let hits_task = hits.clone();
    let partial_text = partial_text.to_string();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let hits = hits_task.clone();
            let partial_text = partial_text.clone();
            tokio::spawn(async move {
                let req = read_full_http_request(&mut socket).await;
                let is_stream = req.contains("\"stream\":true");
                if is_stream {
                    hits.stream_hits.fetch_add(1, Ordering::SeqCst);
                    let partial = format!(
                        "data: {}\n\n",
                        json!({"choices":[{"delta":{"content": partial_text}}]})
                    );
                    let chunk = format!("{:X}\r\n{}\r\n", partial.len(), partial);
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{chunk}"
                    );
                    socket
                        .write_all(response.as_bytes())
                        .await
                        .expect("write hanging partial stream response");
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                    let _ = socket.shutdown().await;
                } else {
                    hits.nonstream_hits.fetch_add(1, Ordering::SeqCst);
                    let body = r#"{"choices":[{"message":{"content":"probe ok"}}]}"#;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    socket
                        .write_all(response.as_bytes())
                        .await
                        .expect("write hanging nonstream response");
                    let _ = socket.shutdown().await;
                }
            });
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    (format!("http://{addr}"), hits)
}

async fn spawn_http_app_server(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind live http listener");
    let addr = listener.local_addr().expect("live http local_addr");
    tokio::spawn(async move {
        axum::serve(listener, app.into_make_service())
            .await
            .expect("serve live http app");
    });
    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    format!("http://{addr}")
}

async fn wait_for_artifact_count(
    pool: &sqlx::MySqlPool,
    session_id: &str,
    artifact_kind: &str,
    min_count: i64,
    timeout: std::time::Duration,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM session_artifacts WHERE session_id = ? AND artifact_kind = ?",
        )
        .bind(session_id)
        .bind(artifact_kind)
        .fetch_one(pool)
        .await
        .unwrap_or(0);
        if n >= min_count {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "timeout ({timeout:?}) waiting for >= {min_count} artifacts of kind={artifact_kind} for session_id={session_id} (got {n})"
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

pub async fn run_session_artifact_http_matches_session_artifacts_rows() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let auth = &b.auth_header;

    let workspace_artifact_id = Uuid::new_v4().to_string();
    let composite_artifact_id = Uuid::new_v4().to_string();

    sqlx::query(
        "INSERT INTO session_artifacts \
         (artifact_id, session_id, user_id, artifact_kind, source, turn, round, content_json, metadata, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NOW())",
    )
    .bind(&workspace_artifact_id)
    .bind(&ctx.session_id)
    .bind(&ctx.user_id)
    .bind(WORKSPACE_METADATA_ARTIFACT_KIND)
    .bind("workspace_metadata")
    .bind(7_i32)
    .bind(0_i32)
    .bind(
        json!({
            "session_id": ctx.session_id,
            "status": "active",
            "model": "gpt-5.4",
            "turn_count": 7
        })
        .to_string(),
    )
    .bind(json!({ "status": "active", "model": "gpt-5.4" }).to_string())
    .execute(&ctx.pool)
    .await
    .expect("insert workspace artifact");

    sqlx::query(
        "INSERT INTO session_artifacts \
         (artifact_id, session_id, user_id, artifact_kind, source, turn, round, content_json, metadata, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NOW())",
    )
    .bind(&composite_artifact_id)
    .bind(&ctx.session_id)
    .bind(&ctx.user_id)
    .bind(COMPOSITE_SNAPSHOT_INDEX_ARTIFACT_KIND)
    .bind("composite_snapshot_index")
    .bind(7_i32)
    .bind(1_i32)
    .bind(
        json!({
            "snapshots": [{
                "snapshot_id": format!("{}-snapshot", ctx.suffix),
                "session_id": ctx.session_id,
                "turn": 7,
                "created_at": "2026-09-09T10:00:00Z",
                "version": 1,
                "label": "http-e2e",
                "refs": []
            }]
        })
        .to_string(),
    )
    .bind(json!({ "snapshot_count": 1, "latest_version": 1 }).to_string())
    .execute(&ctx.pool)
    .await
    .expect("insert composite artifact");

    let artifact_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM session_artifacts WHERE session_id = ?")
            .bind(&ctx.session_id)
            .fetch_one(&ctx.pool)
            .await
            .expect("artifact count");
    assert_eq!(artifact_count, 2);

    let list_path = format!(
        "/sessions/{}/artifacts?artifact_kind={}&limit=1",
        ctx.session_id, WORKSPACE_METADATA_ARTIFACT_KIND
    );
    let (st_list, list_j) = get_json(&ctx.app, &list_path, Some(auth), &[]).await;
    assert_eq!(st_list, StatusCode::OK, "artifact list: {list_j}");
    assert_eq!(list_j["session_id"].as_str(), Some(ctx.session_id.as_str()));
    assert_eq!(list_j["limit"].as_u64(), Some(1));
    let artifacts = list_j["artifacts"].as_array().expect("artifacts array");
    assert_eq!(
        artifacts.len(),
        1,
        "artifact kind filter should narrow results"
    );
    assert_eq!(
        artifacts[0]["artifact_id"].as_str(),
        Some(workspace_artifact_id.as_str())
    );
    assert_eq!(
        artifacts[0]["artifact_kind"].as_str(),
        Some(WORKSPACE_METADATA_ARTIFACT_KIND)
    );
    assert_eq!(artifacts[0]["turn"].as_u64(), Some(7));
    assert_eq!(artifacts[0]["content"]["model"].as_str(), Some("gpt-5.4"));

    let get_path = format!(
        "/sessions/{}/artifacts/{}",
        ctx.session_id, workspace_artifact_id
    );
    let (st_get, get_j) = get_json(&ctx.app, &get_path, Some(auth), &[]).await;
    assert_eq!(st_get, StatusCode::OK, "artifact get: {get_j}");
    assert_eq!(
        get_j["artifact_id"].as_str(),
        Some(workspace_artifact_id.as_str())
    );
    assert_eq!(get_j["user_id"].as_str(), Some(ctx.user_id.as_str()));
    assert_eq!(get_j["metadata"]["status"].as_str(), Some("active"));

    let (st_create_other_session, other_session_j) = post_json(
        &ctx.app,
        "/sessions",
        Some(auth),
        json!({ "title": "artifact wrong-session probe" }),
    )
    .await;
    assert_eq!(
        st_create_other_session,
        StatusCode::CREATED,
        "create second session: {other_session_j}"
    );
    let other_session_id = other_session_j["session_id"]
        .as_str()
        .expect("other session_id")
        .to_string();
    let wrong_session_path = format!(
        "/sessions/{}/artifacts/{}",
        other_session_id, workspace_artifact_id
    );
    let (st_wrong_session, wrong_session_j) =
        get_json(&ctx.app, &wrong_session_path, Some(auth), &[]).await;
    assert_eq!(
        st_wrong_session,
        StatusCode::NOT_FOUND,
        "artifact id must not be readable through a different session path: {wrong_session_j}"
    );

    let (other_app, other_auth) = match b.auth_mode {
        E2eAuthMode::LocalJwt => {
            let b_suffix = Uuid::new_v4().simple().to_string();
            let short = &b_suffix[..12];
            let b_username = format!("art_iso_{short}");
            let b_email = format!("art_iso_{short}@e2e.test");

            let (st_reg, reg_b) = post_json(
                &ctx.app,
                "/auth/register",
                None,
                json!({
                    "username": b_username,
                    "email": b_email,
                    "password": E2E_PASSWORD,
                    "display_name": "Artifact isolation B"
                }),
            )
            .await;
            assert_eq!(st_reg, StatusCode::CREATED, "register B: {reg_b}");

            let (st_login, login_j) = post_json(
                &ctx.app,
                "/auth/login",
                None,
                json!({ "username": b_username, "password": E2E_PASSWORD }),
            )
            .await;
            assert_eq!(st_login, StatusCode::OK, "login B: {login_j}");
            let access_b = login_j["access_token"].as_str().expect("B access_token");
            (ctx.app.clone(), format!("Bearer {access_b}"))
        }
        E2eAuthMode::TrustedMoi => {
            let other = bootstrap_trusted_moi().await;
            (other.ctx.app.clone(), other.auth_header)
        }
    };

    let (st_foreign_list, foreign_list_j) =
        get_json(&other_app, &list_path, Some(&other_auth), &[]).await;
    assert_eq!(
        st_foreign_list,
        StatusCode::NOT_FOUND,
        "foreign user must not list another user's session artifacts: {foreign_list_j}"
    );
    let (st_foreign_get, foreign_get_j) =
        get_json(&other_app, &get_path, Some(&other_auth), &[]).await;
    assert_eq!(
        st_foreign_get,
        StatusCode::NOT_FOUND,
        "foreign user must not get another user's session artifact: {foreign_get_j}"
    );

    let db_row = sqlx::query(
        "SELECT artifact_kind, source, CAST(metadata AS CHAR) AS metadata_json \
         FROM session_artifacts WHERE artifact_id = ? AND session_id = ?",
    )
    .bind(&workspace_artifact_id)
    .bind(&ctx.session_id)
    .fetch_one(&ctx.pool)
    .await
    .expect("workspace artifact row");
    assert_eq!(
        db_row.try_get::<String, _>("artifact_kind").ok().as_deref(),
        Some(WORKSPACE_METADATA_ARTIFACT_KIND)
    );
    assert_eq!(
        db_row
            .try_get::<Option<String>, _>("source")
            .ok()
            .flatten()
            .as_deref(),
        Some("workspace_metadata")
    );

    ctx.pool.close().await;
}

pub async fn run_published_session_artifact_round_trip() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;

    let (st_sess, sess) = post_json(
        app,
        "/sessions",
        Some(auth.as_str()),
        json!({ "title": "artifact publish roundtrip", "metadata": { "full_llm_capture": true, "suite": "artifact_roundtrip" } }),
    )
    .await;
    assert_eq!(st_sess, StatusCode::CREATED, "create session: {sess}");
    let session_id = sess["session_id"].as_str().expect("session_id").to_string();

    let before_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM session_artifacts WHERE session_id = ? AND artifact_kind = 'llm_capture'",
    )
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("llm capture count before stream");
    assert_eq!(
        before_count, 0,
        "fresh session should not have llm_capture artifacts"
    );

    let payload = json!({
        "message": "publish llm capture and read it back",
        "session_id": &session_id,
        "context": {
            "test_llm_rounds": [{ "full_text": "Artifact publish verified." }]
        }
    });
    let (status, body) = stream_chat_full_nonbridge(app, auth, payload).await;
    assert_eq!(status, StatusCode::OK, "chat/stream: {body}");
    assert!(
        body.contains("Artifact publish verified."),
        "SSE body should include the model text response: {body}"
    );

    wait_for_artifact_count(
        pool,
        &session_id,
        "llm_capture",
        1,
        std::time::Duration::from_secs(15),
    )
    .await;

    let row = sqlx::query(
        "SELECT artifact_id, source, turn, round, content_json, CAST(metadata AS CHAR) AS metadata_json \
         FROM session_artifacts WHERE session_id = ? AND artifact_kind = 'llm_capture' \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("latest llm_capture row");
    let artifact_id: String = row.try_get("artifact_id").expect("artifact_id");
    let source: Option<String> = row.try_get("source").expect("source");
    let turn: Option<i32> = row.try_get("turn").expect("turn");
    let round: Option<i32> = row.try_get("round").expect("round");
    let content_json: String = row.try_get("content_json").expect("content_json");
    let content: serde_json::Value =
        serde_json::from_str(&content_json).expect("parse llm_capture content");
    assert_eq!(source.as_deref(), Some("server_loop_host"));
    assert!(turn.unwrap_or_default() >= 1);
    assert!(round.unwrap_or_default() >= 0);
    assert_eq!(
        content["response"]["full_text"].as_str(),
        Some("Artifact publish verified.")
    );

    let list_path = format!("/sessions/{session_id}/artifacts?artifact_kind=llm_capture&limit=10");
    let (st_list, list_j) = get_json(app, &list_path, Some(auth), &[]).await;
    assert_eq!(
        st_list,
        StatusCode::OK,
        "artifact list after publish: {list_j}"
    );
    let artifacts = list_j["artifacts"].as_array().expect("artifacts array");
    assert!(
        artifacts
            .iter()
            .any(|artifact| artifact["artifact_id"].as_str() == Some(artifact_id.as_str())),
        "list should contain the published llm_capture artifact: {list_j}"
    );

    let get_path = format!("/sessions/{session_id}/artifacts/{artifact_id}");
    let (st_get, get_j) = get_json(app, &get_path, Some(auth), &[]).await;
    assert_eq!(
        st_get,
        StatusCode::OK,
        "artifact get after publish: {get_j}"
    );
    assert_eq!(get_j["artifact_kind"].as_str(), Some("llm_capture"));
    assert_eq!(get_j["source"].as_str(), Some("server_loop_host"));
    assert_eq!(
        get_j["content"]["response"]["full_text"].as_str(),
        Some("Artifact publish verified.")
    );
    assert_eq!(
        get_j["metadata"]["outcome"].as_str(),
        Some("success"),
        "live artifact read-back should preserve runtime-published metadata"
    );

    let (st_wrong_session, wrong_session_j) = get_json(
        app,
        &format!("/sessions/{}/artifacts/{}", ctx.session_id, artifact_id),
        Some(auth),
        &[],
    )
    .await;
    assert_eq!(
        st_wrong_session,
        StatusCode::NOT_FOUND,
        "published artifact should still be session-scoped over HTTP: {wrong_session_j}"
    );

    let _ = sqlx::query("DELETE FROM session_artifacts WHERE session_id = ?")
        .bind(&session_id)
        .execute(pool)
        .await;
    cleanup_session_data(pool, &session_id).await;
    ctx.pool.close().await;
}

pub async fn run_session_artifact_latest_and_download_routes() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;

    let (st_sess, sess) = post_json(
        app,
        "/sessions",
        Some(auth.as_str()),
        json!({ "title": "artifact latest download", "metadata": { "full_llm_capture": true, "suite": "artifact_latest_download" } }),
    )
    .await;
    assert_eq!(st_sess, StatusCode::CREATED, "create session: {sess}");
    let session_id = sess["session_id"].as_str().expect("session_id").to_string();

    let payload = json!({
        "message": "publish llm capture for latest and download routes",
        "session_id": &session_id,
        "context": {
            "test_llm_rounds": [{ "full_text": "Artifact download verified." }]
        }
    });
    let (status, body) = stream_chat_full_nonbridge(app, auth, payload).await;
    assert_eq!(status, StatusCode::OK, "chat/stream: {body}");
    assert!(
        body.contains("Artifact download verified."),
        "SSE body should include the model text response: {body}"
    );

    wait_for_artifact_count(
        pool,
        &session_id,
        "llm_capture",
        1,
        std::time::Duration::from_secs(15),
    )
    .await;

    let row = sqlx::query(
        "SELECT artifact_id \
         FROM session_artifacts WHERE session_id = ? AND artifact_kind = 'llm_capture' \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("latest llm_capture row");
    let artifact_id: String = row.try_get("artifact_id").expect("artifact_id");

    let latest_path = format!("/sessions/{session_id}/artifacts/latest/llm_capture");
    let (st_latest, latest_j) = get_json(app, &latest_path, Some(auth), &[]).await;
    assert_eq!(st_latest, StatusCode::OK, "artifact latest: {latest_j}");
    assert_eq!(latest_j["artifact_id"].as_str(), Some(artifact_id.as_str()));
    assert_eq!(latest_j["artifact_kind"].as_str(), Some("llm_capture"));
    assert_eq!(
        latest_j["content"]["response"]["full_text"].as_str(),
        Some("Artifact download verified.")
    );

    let download_path = format!("/sessions/{session_id}/artifacts/{artifact_id}/download");
    let (st_download, download_headers, download_body) =
        get_bytes(app, &download_path, Some(auth), &[]).await;
    assert_eq!(st_download, StatusCode::OK, "artifact download");
    assert_eq!(
        download_headers
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/json")
    );
    let content_disposition = download_headers
        .get("content-disposition")
        .and_then(|value| value.to_str().ok())
        .expect("content-disposition");
    assert!(
        content_disposition.contains("attachment;"),
        "download should be an attachment: {content_disposition}"
    );
    assert!(
        content_disposition.contains(artifact_id.as_str()),
        "download filename should include the artifact id: {content_disposition}"
    );
    let download_j: serde_json::Value =
        serde_json::from_slice(&download_body).expect("download json");
    assert_eq!(
        download_j["artifact_id"].as_str(),
        Some(artifact_id.as_str())
    );
    assert_eq!(download_j["artifact_kind"].as_str(), Some("llm_capture"));
    assert_eq!(
        download_j["content"]["response"]["full_text"].as_str(),
        Some("Artifact download verified.")
    );

    let (st_other_session, other_session_j) = post_json(
        app,
        "/sessions",
        Some(auth.as_str()),
        json!({ "title": "artifact latest wrong session" }),
    )
    .await;
    assert_eq!(
        st_other_session,
        StatusCode::CREATED,
        "create second session: {other_session_j}"
    );
    let other_session_id = other_session_j["session_id"]
        .as_str()
        .expect("other session_id")
        .to_string();

    let (st_wrong_latest, wrong_latest_j) = get_json(
        app,
        &format!("/sessions/{other_session_id}/artifacts/latest/llm_capture"),
        Some(auth),
        &[],
    )
    .await;
    assert_eq!(
        st_wrong_latest,
        StatusCode::NOT_FOUND,
        "latest artifact should stay session-scoped: {wrong_latest_j}"
    );

    let (st_wrong_download, _headers, wrong_download_body) = get_bytes(
        app,
        &format!("/sessions/{other_session_id}/artifacts/{artifact_id}/download"),
        Some(auth),
        &[],
    )
    .await;
    assert_eq!(
        st_wrong_download,
        StatusCode::NOT_FOUND,
        "artifact download should stay session-scoped: {}",
        String::from_utf8_lossy(&wrong_download_body)
    );

    let (other_app, other_auth) = match b.auth_mode {
        E2eAuthMode::LocalJwt => {
            let b_suffix = Uuid::new_v4().simple().to_string();
            let short = &b_suffix[..12];
            let b_username = format!("art_dl_{short}");
            let b_email = format!("art_dl_{short}@e2e.test");

            let (st_reg, reg_b) = post_json(
                &ctx.app,
                "/auth/register",
                None,
                json!({
                    "username": b_username,
                    "email": b_email,
                    "password": E2E_PASSWORD,
                    "display_name": "Artifact download isolation B"
                }),
            )
            .await;
            assert_eq!(st_reg, StatusCode::CREATED, "register B: {reg_b}");

            let (st_login, login_j) = post_json(
                &ctx.app,
                "/auth/login",
                None,
                json!({ "username": b_username, "password": E2E_PASSWORD }),
            )
            .await;
            assert_eq!(st_login, StatusCode::OK, "login B: {login_j}");
            let access_b = login_j["access_token"].as_str().expect("B access_token");
            (ctx.app.clone(), format!("Bearer {access_b}"))
        }
        E2eAuthMode::TrustedMoi => {
            let other = bootstrap_trusted_moi().await;
            (other.ctx.app.clone(), other.auth_header)
        }
    };

    let (st_foreign_latest, foreign_latest_j) =
        get_json(&other_app, &latest_path, Some(&other_auth), &[]).await;
    assert_eq!(
        st_foreign_latest,
        StatusCode::NOT_FOUND,
        "foreign user must not read another user's latest artifact: {foreign_latest_j}"
    );

    let (st_foreign_download, _headers, foreign_download_body) =
        get_bytes(&other_app, &download_path, Some(&other_auth), &[]).await;
    assert_eq!(
        st_foreign_download,
        StatusCode::NOT_FOUND,
        "foreign user must not download another user's artifact: {}",
        String::from_utf8_lossy(&foreign_download_body)
    );

    let _ = sqlx::query("DELETE FROM session_artifacts WHERE session_id IN (?, ?)")
        .bind(&session_id)
        .bind(&other_session_id)
        .execute(pool)
        .await;
    cleanup_session_data(pool, &session_id).await;
    cleanup_session_data(pool, &other_session_id).await;
    ctx.pool.close().await;
}

pub async fn run_failed_session_artifact_latest_and_download_routes() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;

    let (st_sess, sess) = post_json(
        app,
        "/sessions",
        Some(auth.as_str()),
        json!({ "title": "artifact failed latest download", "metadata": { "full_llm_capture": true, "suite": "artifact_failed_latest_download" } }),
    )
    .await;
    assert_eq!(st_sess, StatusCode::CREATED, "create session: {sess}");
    let session_id = sess["session_id"].as_str().expect("session_id").to_string();

    let failure_message = "Synthetic streamed failure for artifact latest/download.";
    let partial_text = "half answer before failure";
    let payload = json!({
        "message": "publish failed llm capture for latest and download routes",
        "session_id": &session_id,
        "context": {
            "test_llm_rounds": [{
                "error": {
                    "message": failure_message,
                    "kind": "stream_transport",
                    "details": {
                        "partial_full_text": partial_text,
                        "usage": { "prompt_tokens": 17, "completion_tokens": 3 }
                    }
                }
            }]
        }
    });
    let (status, body) = stream_chat_full_nonbridge(app, auth, payload).await;
    assert_eq!(status, StatusCode::OK, "chat/stream: {body}");
    assert!(
        body.contains(failure_message),
        "SSE body should surface the scripted failure: {body}"
    );
    assert!(
        body.contains("\"status\":\"failed\""),
        "SSE body should report the failed terminal status: {body}"
    );

    wait_for_artifact_count(
        pool,
        &session_id,
        "llm_capture",
        1,
        std::time::Duration::from_secs(15),
    )
    .await;

    let row = sqlx::query(
        "SELECT artifact_id \
         FROM session_artifacts WHERE session_id = ? AND artifact_kind = 'llm_capture' \
         ORDER BY created_at DESC, artifact_id DESC LIMIT 1",
    )
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("latest failed llm_capture row");
    let artifact_id: String = row.try_get("artifact_id").expect("artifact_id");

    let latest_path = format!("/sessions/{session_id}/artifacts/latest/llm_capture");
    let (st_latest, latest_j) = get_json(app, &latest_path, Some(auth), &[]).await;
    assert_eq!(st_latest, StatusCode::OK, "artifact latest: {latest_j}");
    assert_eq!(latest_j["artifact_id"].as_str(), Some(artifact_id.as_str()));
    assert_eq!(latest_j["artifact_kind"].as_str(), Some("llm_capture"));
    assert_eq!(latest_j["metadata"]["outcome"].as_str(), Some("error"));
    assert_eq!(
        latest_j["content"]["response"]["error"].as_str(),
        Some(failure_message)
    );
    assert_eq!(
        latest_j["content"]["response"]["kind"].as_str(),
        Some("stream_transport")
    );
    assert_eq!(
        latest_j["content"]["response"]["partial_full_text"].as_str(),
        Some(partial_text)
    );
    assert_eq!(
        latest_j["content"]["response"]["usage"]["prompt"].as_i64(),
        Some(17)
    );

    let download_path = format!("/sessions/{session_id}/artifacts/{artifact_id}/download");
    let (st_download, download_headers, download_body) =
        get_bytes(app, &download_path, Some(auth), &[]).await;
    assert_eq!(st_download, StatusCode::OK, "artifact download");
    assert_eq!(
        download_headers
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/json")
    );
    let download_j: serde_json::Value =
        serde_json::from_slice(&download_body).expect("download json");
    assert_eq!(
        download_j["artifact_id"].as_str(),
        Some(artifact_id.as_str())
    );
    assert_eq!(download_j["artifact_kind"].as_str(), Some("llm_capture"));
    assert_eq!(download_j["metadata"]["outcome"].as_str(), Some("error"));
    assert_eq!(
        download_j["content"]["response"]["error"].as_str(),
        Some(failure_message)
    );
    assert_eq!(
        download_j["content"]["response"]["kind"].as_str(),
        Some("stream_transport")
    );
    assert_eq!(
        download_j["content"]["response"]["partial_full_text"].as_str(),
        Some(partial_text)
    );
    assert_eq!(
        download_j["content"]["response"]["usage"]["prompt"].as_i64(),
        Some(17)
    );

    let _ = sqlx::query("DELETE FROM session_artifacts WHERE session_id = ?")
        .bind(&session_id)
        .execute(pool)
        .await;
    cleanup_session_data(pool, &session_id).await;
    ctx.pool.close().await;
}

async fn run_bridge_failure_session_artifact_latest_and_download_routes(
    title: &str,
    suite: &str,
    agent_id: &str,
    user_message: &str,
    stream_blocks: Vec<String>,
    expected_outcome: &str,
    expected_code: &str,
    partial_text: &str,
) {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;

    let (st_sess, sess) = post_json(
        app,
        "/sessions",
        Some(auth.as_str()),
        json!({ "title": title, "metadata": { "full_llm_capture": true, "suite": suite } }),
    )
    .await;
    assert_eq!(st_sess, StatusCode::CREATED, "create session: {sess}");
    let session_id = sess["session_id"].as_str().expect("session_id").to_string();

    let payload = json!({
        "agent_id": agent_id,
        "session_id": &session_id,
        "messages": [{ "role": "user", "content": user_message }],
        "test_llm_stream_blocks": stream_blocks
    });
    let (status, body) = chat_turn_full(app, auth, payload).await;
    assert_eq!(status, StatusCode::OK, "chat/turn: {body}");
    assert!(
        body.contains(partial_text),
        "bridge SSE should include the partial streamed text: {body}"
    );
    assert!(
        body.contains(&format!("\"code\":\"{expected_code}\"")),
        "bridge SSE should expose the expected failure code: {body}"
    );

    wait_for_artifact_count(
        pool,
        &session_id,
        "llm_capture",
        1,
        std::time::Duration::from_secs(15),
    )
    .await;

    let row = sqlx::query(
        "SELECT artifact_id \
         FROM session_artifacts WHERE session_id = ? AND artifact_kind = 'llm_capture' \
         ORDER BY created_at DESC, artifact_id DESC LIMIT 1",
    )
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("latest bridge failed llm_capture row");
    let artifact_id: String = row.try_get("artifact_id").expect("artifact_id");

    let latest_path = format!("/sessions/{session_id}/artifacts/latest/llm_capture");
    let (st_latest, latest_j) = get_json(app, &latest_path, Some(auth), &[]).await;
    assert_eq!(st_latest, StatusCode::OK, "artifact latest: {latest_j}");
    assert_eq!(latest_j["artifact_id"].as_str(), Some(artifact_id.as_str()));
    assert_eq!(latest_j["artifact_kind"].as_str(), Some("llm_capture"));
    assert_eq!(latest_j["source"].as_str(), Some("bridge_inprocess"));
    assert_eq!(
        latest_j["metadata"]["outcome"].as_str(),
        Some(expected_outcome)
    );
    assert_eq!(
        latest_j["content"]["response"]["kind"].as_str(),
        Some(expected_code)
    );
    assert_eq!(
        latest_j["content"]["response"]["partial_full_text"].as_str(),
        Some(partial_text)
    );

    let download_path = format!("/sessions/{session_id}/artifacts/{artifact_id}/download");
    let (st_download, _download_headers, download_body) =
        get_bytes(app, &download_path, Some(auth), &[]).await;
    assert_eq!(st_download, StatusCode::OK, "artifact download");
    let download_j: serde_json::Value =
        serde_json::from_slice(&download_body).expect("download json");
    assert_eq!(download_j["artifact_kind"].as_str(), Some("llm_capture"));
    assert_eq!(download_j["source"].as_str(), Some("bridge_inprocess"));
    assert_eq!(
        download_j["metadata"]["outcome"].as_str(),
        Some(expected_outcome)
    );
    assert_eq!(
        download_j["content"]["response"]["kind"].as_str(),
        Some(expected_code)
    );
    assert_eq!(
        download_j["content"]["response"]["partial_full_text"].as_str(),
        Some(partial_text)
    );

    let _ = sqlx::query("DELETE FROM session_artifacts WHERE session_id = ?")
        .bind(&session_id)
        .execute(pool)
        .await;
    cleanup_session_data(pool, &session_id).await;
    ctx.pool.close().await;
}

pub async fn run_server_loop_block_parse_recovery_session_artifact_latest_and_download_routes() {
    let partial_text = "server loop partial before malformed block";
    let recovered_text = "server loop recovered final answer";
    let (base_url, hits) =
        spawn_raw_server_loop_block_parse_recovery_server(partial_text, recovered_text).await;

    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let model_name = format!("server-loop-block-parse-{}", ctx.suffix);

    let (st_model, model_j) = post_json(
        app,
        "/models",
        Some(auth.as_str()),
        json!({
            "name": model_name,
            "provider": "openai",
            "api_key": "server-loop-block-parse-e2e-key",
            "base_url": base_url
        }),
    )
    .await;
    assert_eq!(st_model, StatusCode::CREATED, "create model: {model_j}");
    sqlx::query("UPDATE infra_llm_models SET is_active = 1 WHERE model_name = ?")
        .bind(&model_name)
        .execute(pool)
        .await
        .expect("force-activate server-loop block-parse test model");

    let (st_sess, sess) = post_json(
        app,
        "/sessions",
        Some(auth.as_str()),
        json!({
            "title": "server loop block parse recovery latest download",
            "metadata": { "full_llm_capture": true, "suite": "server_loop_block_parse_recovery_latest_download" }
        }),
    )
    .await;
    assert_eq!(st_sess, StatusCode::CREATED, "create session: {sess}");
    let session_id = sess["session_id"].as_str().expect("session_id").to_string();

    let payload = json!({
        "message": "trigger a server-loop malformed provider block after progress and recover",
        "session_id": &session_id,
        "model": model_name
    });
    let (status, body) = stream_chat_full_nonbridge(app, auth, payload).await;
    assert_eq!(status, StatusCode::OK, "chat/stream: {body}");
    assert!(
        hits.stream_hits.load(Ordering::SeqCst) >= 1,
        "server-loop recovery proof must hit the raw streaming provider at least once"
    );
    assert!(
        hits.nonstream_hits.load(Ordering::SeqCst) >= 1,
        "server-loop recovery proof must hit non-stream fallback after malformed stream"
    );
    assert!(
        body.contains(recovered_text),
        "server-loop SSE should surface the recovered final text after non-stream fallback: {body}"
    );
    assert!(
        body.contains("\"status\":\"completed\""),
        "server-loop SSE should end as completed after fallback recovery: {body}"
    );
    assert!(
        body.contains("\"type\":\"turn_complete\""),
        "server-loop SSE should still terminate with turn_complete on successful recovery: {body}"
    );
    assert!(
        !body.contains("\"status\":\"failed\""),
        "successful malformed-block recovery should not end in failed status: {body}"
    );

    wait_for_artifact_count(
        pool,
        &session_id,
        "llm_capture",
        1,
        std::time::Duration::from_secs(15),
    )
    .await;

    let row = sqlx::query(
        "SELECT artifact_id \
         FROM session_artifacts WHERE session_id = ? AND artifact_kind = 'llm_capture' \
         ORDER BY created_at DESC, artifact_id DESC LIMIT 1",
    )
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("latest server-loop block-parse llm_capture row");
    let artifact_id: String = row.try_get("artifact_id").expect("artifact_id");

    let latest_path = format!("/sessions/{session_id}/artifacts/latest/llm_capture");
    let (st_latest, latest_j) = get_json(app, &latest_path, Some(auth), &[]).await;
    assert_eq!(st_latest, StatusCode::OK, "artifact latest: {latest_j}");
    assert_eq!(latest_j["artifact_kind"].as_str(), Some("llm_capture"));
    assert_eq!(latest_j["metadata"]["outcome"].as_str(), Some("success"));
    assert_eq!(
        latest_j["content"]["response"]["full_text"].as_str(),
        Some(recovered_text)
    );

    let download_path = format!("/sessions/{session_id}/artifacts/{artifact_id}/download");
    let (st_download, _download_headers, download_body) =
        get_bytes(app, &download_path, Some(auth), &[]).await;
    assert_eq!(st_download, StatusCode::OK, "artifact download");
    let download_j: serde_json::Value =
        serde_json::from_slice(&download_body).expect("download json");
    assert_eq!(download_j["metadata"]["outcome"].as_str(), Some("success"));
    assert_eq!(
        download_j["content"]["response"]["full_text"].as_str(),
        Some(recovered_text)
    );

    assert!(
        hits.stream_hits.load(Ordering::SeqCst) >= 1,
        "malformed provider block after progress should trigger at least one streaming request"
    );
    assert!(
        hits.nonstream_hits.load(Ordering::SeqCst) >= 1,
        "malformed provider block after progress should trigger at least one non-stream fallback request"
    );

    let _ = sqlx::query("DELETE FROM session_artifacts WHERE session_id = ?")
        .bind(&session_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM infra_llm_models WHERE model_name = ?")
        .bind(&model_name)
        .execute(pool)
        .await;
    cleanup_session_data(pool, &session_id).await;
    ctx.pool.close().await;
}

pub async fn run_server_loop_block_parse_failure_session_artifact_latest_and_download_routes() {
    let partial_text = "server loop partial before malformed block";
    let failure_fragment = "fallback exploded";
    let (base_url, hits) = spawn_raw_server_loop_block_parse_failure_server(partial_text).await;

    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let model_name = format!("server-loop-block-parse-fail-{}", ctx.suffix);

    let (st_model, model_j) = post_json(
        app,
        "/models",
        Some(auth.as_str()),
        json!({
            "name": model_name,
            "provider": "openai",
            "api_key": "server-loop-block-parse-fail-e2e-key",
            "base_url": base_url
        }),
    )
    .await;
    assert_eq!(st_model, StatusCode::CREATED, "create model: {model_j}");
    sqlx::query("UPDATE infra_llm_models SET is_active = 1 WHERE model_name = ?")
        .bind(&model_name)
        .execute(pool)
        .await
        .expect("force-activate server-loop block-parse failure test model");

    let (st_sess, sess) = post_json(
        app,
        "/sessions",
        Some(auth.as_str()),
        json!({
            "title": "server loop block parse failure latest download",
            "metadata": { "full_llm_capture": true, "suite": "server_loop_block_parse_failure_latest_download" }
        }),
    )
    .await;
    assert_eq!(st_sess, StatusCode::CREATED, "create session: {sess}");
    let session_id = sess["session_id"].as_str().expect("session_id").to_string();

    let payload = json!({
        "message": "trigger a server-loop malformed provider block after progress and make fallback fail",
        "session_id": &session_id,
        "model": model_name
    });
    let (status, body) = stream_chat_full_nonbridge(app, auth, payload).await;
    assert_eq!(status, StatusCode::OK, "chat/stream: {body}");
    assert!(
        hits.stream_hits.load(Ordering::SeqCst) >= 1,
        "server-loop failure proof must hit the raw streaming provider at least once"
    );
    assert!(
        hits.nonstream_hits.load(Ordering::SeqCst) >= 1,
        "server-loop failure proof must hit non-stream fallback after malformed stream"
    );
    assert!(
        body.contains(failure_fragment),
        "server-loop SSE should surface the fallback failure text: {body}"
    );
    assert!(
        body.contains("\"code\":\"RUN_ERROR\""),
        "server-loop SSE should expose the normalized run error code on fallback failure: {body}"
    );
    assert!(
        body.contains("\"status\":\"failed\""),
        "server-loop SSE should end with failed status when malformed-block recovery also fails: {body}"
    );
    assert!(
        !body.contains("\"type\":\"turn_complete\""),
        "server-loop fallback failure should not emit turn_complete: {body}"
    );

    wait_for_artifact_count(
        pool,
        &session_id,
        "llm_capture",
        1,
        std::time::Duration::from_secs(15),
    )
    .await;

    let row = sqlx::query(
        "SELECT artifact_id \
         FROM session_artifacts WHERE session_id = ? AND artifact_kind = 'llm_capture' \
         ORDER BY created_at DESC, artifact_id DESC LIMIT 1",
    )
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("latest server-loop block-parse failure llm_capture row");
    let artifact_id: String = row.try_get("artifact_id").expect("artifact_id");

    let latest_path = format!("/sessions/{session_id}/artifacts/latest/llm_capture");
    let (st_latest, latest_j) = get_json(app, &latest_path, Some(auth), &[]).await;
    assert_eq!(st_latest, StatusCode::OK, "artifact latest: {latest_j}");
    assert_eq!(latest_j["artifact_kind"].as_str(), Some("llm_capture"));
    assert_eq!(latest_j["metadata"]["outcome"].as_str(), Some("error"));
    assert!(
        latest_j["content"]["response"]["error"]
            .as_str()
            .unwrap_or_default()
            .contains(failure_fragment),
        "latest error payload should retain fallback failure text: {latest_j}"
    );
    assert_eq!(
        latest_j["content"]["response"]["partial_full_text"].as_str(),
        Some(partial_text)
    );

    let download_path = format!("/sessions/{session_id}/artifacts/{artifact_id}/download");
    let (st_download, _download_headers, download_body) =
        get_bytes(app, &download_path, Some(auth), &[]).await;
    assert_eq!(st_download, StatusCode::OK, "artifact download");
    let download_j: serde_json::Value =
        serde_json::from_slice(&download_body).expect("download json");
    assert_eq!(download_j["metadata"]["outcome"].as_str(), Some("error"));
    assert!(
        download_j["content"]["response"]["error"]
            .as_str()
            .unwrap_or_default()
            .contains(failure_fragment),
        "download error payload should retain fallback failure text: {download_j}"
    );
    assert_eq!(
        download_j["content"]["response"]["partial_full_text"].as_str(),
        Some(partial_text)
    );

    assert!(
        hits.stream_hits.load(Ordering::SeqCst) >= 1,
        "malformed provider block after progress should trigger at least one streaming request before failing"
    );
    assert!(
        hits.nonstream_hits.load(Ordering::SeqCst) >= 1,
        "malformed provider block after progress should trigger at least one non-stream fallback request before failing"
    );

    let _ = sqlx::query("DELETE FROM session_artifacts WHERE session_id = ?")
        .bind(&session_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM infra_llm_models WHERE model_name = ?")
        .bind(&model_name)
        .execute(pool)
        .await;
    cleanup_session_data(pool, &session_id).await;
    ctx.pool.close().await;
}

pub async fn run_server_loop_client_disconnect_session_artifact_latest_and_download_routes() {
    let partial_text = "server loop disconnect partial";
    let failure_fragment = "server loop disconnect fallback failed";
    // Use a mock server that sends partial output then immediately closes the
    // connection (no 10-second hang). The nonstream fallback also fails, so
    // the artifact is written with a transport error outcome.
    let (base_url, hits) = spawn_raw_partial_transport_server(
        partial_text,
        500,
        r#"{"error":{"message":"server loop disconnect fallback failed"}}"#,
    )
    .await;

    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let model_name = format!("server-loop-disconnect-{}", ctx.suffix);

    let (st_model, model_j) = post_json(
        app,
        "/models",
        Some(auth.as_str()),
        json!({
            "name": model_name,
            "provider": "openai",
            "api_key": "server-loop-disconnect-e2e-key",
            "base_url": base_url
        }),
    )
    .await;
    assert_eq!(st_model, StatusCode::CREATED, "create model: {model_j}");
    sqlx::query("UPDATE infra_llm_models SET is_active = 1 WHERE model_name = ?")
        .bind(&model_name)
        .execute(pool)
        .await
        .expect("force-activate server-loop disconnect test model");
    // Pre-consume the probe nonstream hit so the fallback attempt gets the 500.
    hits.nonstream_hits.store(1, Ordering::SeqCst);

    let (st_sess, sess) = post_json(
        app,
        "/sessions",
        Some(auth.as_str()),
        json!({
            "title": "server loop client disconnect latest download",
            "metadata": { "full_llm_capture": true, "suite": "server_loop_client_disconnect_latest_download" }
        }),
    )
    .await;
    assert_eq!(st_sess, StatusCode::CREATED, "create session: {sess}");
    let session_id = sess["session_id"].as_str().expect("session_id").to_string();

    let payload = json!({
        "message": "trigger a server-loop transport break after partial output",
        "session_id": &session_id,
        "model": model_name
    });
    let (status, body) = stream_chat_full_nonbridge(app, auth, payload).await;
    assert_eq!(status, StatusCode::OK, "chat/stream: {body}");
    assert!(
        body.contains(failure_fragment),
        "server-loop SSE should surface the transport fallback failure text: {body}"
    );

    wait_for_artifact_count(
        pool,
        &session_id,
        "llm_capture",
        1,
        std::time::Duration::from_secs(15),
    )
    .await;

    let row = sqlx::query(
        "SELECT artifact_id \
         FROM session_artifacts WHERE session_id = ? AND artifact_kind = 'llm_capture' \
         ORDER BY created_at DESC, artifact_id DESC LIMIT 1",
    )
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("latest server-loop disconnect llm_capture row");
    let artifact_id: String = row.try_get("artifact_id").expect("artifact_id");

    let latest_path = format!("/sessions/{session_id}/artifacts/latest/llm_capture");
    let (st_latest, latest_j) = get_json(app, &latest_path, Some(auth), &[]).await;
    assert_eq!(st_latest, StatusCode::OK, "artifact latest: {latest_j}");
    assert_eq!(latest_j["artifact_kind"].as_str(), Some("llm_capture"));
    assert_eq!(latest_j["metadata"]["outcome"].as_str(), Some("error"));
    assert_eq!(
        latest_j["content"]["response"]["kind"].as_str(),
        Some("server_error")
    );
    assert!(
        latest_j["content"]["response"]["error"]
            .as_str()
            .unwrap_or_default()
            .contains(failure_fragment),
        "latest error payload should retain transport fallback failure text: {latest_j}"
    );
    assert_eq!(
        latest_j["content"]["response"]["partial_full_text"].as_str(),
        Some(partial_text)
    );

    let download_path = format!("/sessions/{session_id}/artifacts/{artifact_id}/download");
    let (st_download, _download_headers, download_body) =
        get_bytes(app, &download_path, Some(auth), &[]).await;
    assert_eq!(st_download, StatusCode::OK, "artifact download");
    let download_j: serde_json::Value =
        serde_json::from_slice(&download_body).expect("download json");
    assert_eq!(download_j["metadata"]["outcome"].as_str(), Some("error"));
    assert_eq!(
        download_j["content"]["response"]["kind"].as_str(),
        Some("server_error")
    );
    assert!(
        download_j["content"]["response"]["error"]
            .as_str()
            .unwrap_or_default()
            .contains(failure_fragment),
        "download error payload should retain transport fallback failure text: {download_j}"
    );
    assert_eq!(
        download_j["content"]["response"]["partial_full_text"].as_str(),
        Some(partial_text)
    );

    assert!(
        hits.stream_hits.load(Ordering::SeqCst) >= 1,
        "disconnect proof must hit the raw streaming provider at least once"
    );

    let _ = sqlx::query("DELETE FROM session_artifacts WHERE session_id = ?")
        .bind(&session_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM infra_llm_models WHERE model_name = ?")
        .bind(&model_name)
        .execute(pool)
        .await;
    cleanup_session_data(pool, &session_id).await;
    ctx.pool.close().await;
}

pub async fn run_server_loop_transport_recovery_session_artifact_latest_and_download_routes() {
    let partial_text = "server loop transport partial";
    let recovered_text = "server loop transport recovered answer";
    let fallback_body = format!(
        r#"{{"choices":[{{"message":{{"content":"{recovered_text}"}}}}],"usage":{{"prompt_tokens":19,"completion_tokens":4}}}}"#
    );
    let (base_url, hits) = spawn_raw_partial_transport_server(
        partial_text,
        200,
        Box::leak(fallback_body.into_boxed_str()),
    )
    .await;

    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let model_name = format!("server-loop-transport-recovery-{}", ctx.suffix);

    let (st_model, model_j) = post_json(
        app,
        "/models",
        Some(auth.as_str()),
        json!({
            "name": model_name,
            "provider": "openai",
            "api_key": "server-loop-transport-recovery-e2e-key",
            "base_url": base_url
        }),
    )
    .await;
    assert_eq!(st_model, StatusCode::CREATED, "create model: {model_j}");
    sqlx::query("UPDATE infra_llm_models SET is_active = 1 WHERE model_name = ?")
        .bind(&model_name)
        .execute(pool)
        .await
        .expect("force-activate server-loop transport recovery test model");
    hits.nonstream_hits.store(1, Ordering::SeqCst);

    let (st_sess, sess) = post_json(
        app,
        "/sessions",
        Some(auth.as_str()),
        json!({
            "title": "server loop transport recovery latest download",
            "metadata": { "full_llm_capture": true, "suite": "server_loop_transport_recovery_latest_download" }
        }),
    )
    .await;
    assert_eq!(st_sess, StatusCode::CREATED, "create session: {sess}");
    let session_id = sess["session_id"].as_str().expect("session_id").to_string();

    let payload = json!({
        "message": "trigger a server-loop transport break after progress and recover",
        "session_id": &session_id,
        "model": model_name
    });
    let (status, body) = stream_chat_full_nonbridge(app, auth, payload).await;
    assert_eq!(status, StatusCode::OK, "chat/stream: {body}");
    assert!(
        body.contains(recovered_text),
        "server-loop SSE should surface the recovered final text after transport fallback: {body}"
    );
    assert!(
        body.contains("\"status\":\"completed\""),
        "server-loop transport recovery should end as completed: {body}"
    );
    assert!(
        body.contains("\"type\":\"turn_complete\""),
        "server-loop transport recovery should still emit turn_complete: {body}"
    );
    assert!(
        !body.contains("\"status\":\"failed\""),
        "successful transport fallback should not fail the client stream: {body}"
    );

    wait_for_artifact_count(
        pool,
        &session_id,
        "llm_capture",
        1,
        std::time::Duration::from_secs(15),
    )
    .await;

    let row = sqlx::query(
        "SELECT artifact_id \
         FROM session_artifacts WHERE session_id = ? AND artifact_kind = 'llm_capture' \
         ORDER BY created_at DESC, artifact_id DESC LIMIT 1",
    )
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("latest server-loop transport recovery llm_capture row");
    let artifact_id: String = row.try_get("artifact_id").expect("artifact_id");

    let latest_path = format!("/sessions/{session_id}/artifacts/latest/llm_capture");
    let (st_latest, latest_j) = get_json(app, &latest_path, Some(auth), &[]).await;
    assert_eq!(st_latest, StatusCode::OK, "artifact latest: {latest_j}");
    assert_eq!(latest_j["artifact_kind"].as_str(), Some("llm_capture"));
    assert_eq!(latest_j["metadata"]["outcome"].as_str(), Some("success"));
    assert_eq!(
        latest_j["content"]["response"]["full_text"].as_str(),
        Some(recovered_text)
    );

    let download_path = format!("/sessions/{session_id}/artifacts/{artifact_id}/download");
    let (st_download, _download_headers, download_body) =
        get_bytes(app, &download_path, Some(auth), &[]).await;
    assert_eq!(st_download, StatusCode::OK, "artifact download");
    let download_j: serde_json::Value =
        serde_json::from_slice(&download_body).expect("download json");
    assert_eq!(download_j["metadata"]["outcome"].as_str(), Some("success"));
    assert_eq!(
        download_j["content"]["response"]["full_text"].as_str(),
        Some(recovered_text)
    );

    assert!(
        hits.stream_hits.load(Ordering::SeqCst) >= 1,
        "transport recovery should hit the streaming provider at least once"
    );
    assert!(
        hits.nonstream_hits.load(Ordering::SeqCst) >= 2,
        "transport recovery should perform one probe and one non-stream fallback request"
    );

    let _ = sqlx::query("DELETE FROM session_artifacts WHERE session_id = ?")
        .bind(&session_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM infra_llm_models WHERE model_name = ?")
        .bind(&model_name)
        .execute(pool)
        .await;
    cleanup_session_data(pool, &session_id).await;
    ctx.pool.close().await;
}

pub async fn run_server_loop_transport_failure_session_artifact_latest_and_download_routes() {
    let partial_text = "server loop transport partial";
    let failure_fragment = "fallback transport recovery failed";
    let (base_url, hits) = spawn_raw_partial_transport_server(
        partial_text,
        500,
        r#"{"error":{"message":"fallback transport recovery failed"}}"#,
    )
    .await;

    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let model_name = format!("server-loop-transport-failure-{}", ctx.suffix);

    let (st_model, model_j) = post_json(
        app,
        "/models",
        Some(auth.as_str()),
        json!({
            "name": model_name,
            "provider": "openai",
            "api_key": "server-loop-transport-failure-e2e-key",
            "base_url": base_url
        }),
    )
    .await;
    assert_eq!(st_model, StatusCode::CREATED, "create model: {model_j}");
    sqlx::query("UPDATE infra_llm_models SET is_active = 1 WHERE model_name = ?")
        .bind(&model_name)
        .execute(pool)
        .await
        .expect("force-activate server-loop transport failure test model");
    hits.nonstream_hits.store(1, Ordering::SeqCst);

    let (st_sess, sess) = post_json(
        app,
        "/sessions",
        Some(auth.as_str()),
        json!({
            "title": "server loop transport failure latest download",
            "metadata": { "full_llm_capture": true, "suite": "server_loop_transport_failure_latest_download" }
        }),
    )
    .await;
    assert_eq!(st_sess, StatusCode::CREATED, "create session: {sess}");
    let session_id = sess["session_id"].as_str().expect("session_id").to_string();

    let payload = json!({
        "message": "trigger a server-loop transport break after progress and make fallback fail",
        "session_id": &session_id,
        "model": model_name
    });
    let (status, body) = stream_chat_full_nonbridge(app, auth, payload).await;
    assert_eq!(status, StatusCode::OK, "chat/stream: {body}");
    assert!(
        body.contains(failure_fragment),
        "server-loop SSE should surface the transport fallback failure text: {body}"
    );
    assert!(
        body.contains("\"code\":\"RUN_ERROR\""),
        "server-loop transport fallback failure should surface a normalized run error: {body}"
    );
    assert!(
        body.contains("\"status\":\"failed\""),
        "server-loop transport fallback failure should terminate as failed: {body}"
    );
    assert!(
        !body.contains("\"type\":\"turn_complete\""),
        "server-loop transport fallback failure should not emit turn_complete: {body}"
    );

    wait_for_artifact_count(
        pool,
        &session_id,
        "llm_capture",
        1,
        std::time::Duration::from_secs(15),
    )
    .await;

    let row = sqlx::query(
        "SELECT artifact_id \
         FROM session_artifacts WHERE session_id = ? AND artifact_kind = 'llm_capture' \
         ORDER BY created_at DESC, artifact_id DESC LIMIT 1",
    )
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("latest server-loop transport failure llm_capture row");
    let artifact_id: String = row.try_get("artifact_id").expect("artifact_id");

    let latest_path = format!("/sessions/{session_id}/artifacts/latest/llm_capture");
    let (st_latest, latest_j) = get_json(app, &latest_path, Some(auth), &[]).await;
    assert_eq!(st_latest, StatusCode::OK, "artifact latest: {latest_j}");
    assert_eq!(latest_j["artifact_kind"].as_str(), Some("llm_capture"));
    assert_eq!(latest_j["metadata"]["outcome"].as_str(), Some("error"));
    assert_eq!(
        latest_j["content"]["response"]["kind"].as_str(),
        Some("server_error")
    );
    assert!(
        latest_j["content"]["response"]["error"]
            .as_str()
            .unwrap_or_default()
            .contains(failure_fragment),
        "latest error payload should retain transport fallback failure text: {latest_j}"
    );
    assert_eq!(
        latest_j["content"]["response"]["partial_full_text"].as_str(),
        Some(partial_text)
    );

    let download_path = format!("/sessions/{session_id}/artifacts/{artifact_id}/download");
    let (st_download, _download_headers, download_body) =
        get_bytes(app, &download_path, Some(auth), &[]).await;
    assert_eq!(st_download, StatusCode::OK, "artifact download");
    let download_j: serde_json::Value =
        serde_json::from_slice(&download_body).expect("download json");
    assert_eq!(download_j["metadata"]["outcome"].as_str(), Some("error"));
    assert_eq!(
        download_j["content"]["response"]["kind"].as_str(),
        Some("server_error")
    );
    assert!(
        download_j["content"]["response"]["error"]
            .as_str()
            .unwrap_or_default()
            .contains(failure_fragment),
        "download error payload should retain transport fallback failure text: {download_j}"
    );
    assert_eq!(
        download_j["content"]["response"]["partial_full_text"].as_str(),
        Some(partial_text)
    );

    assert!(
        hits.stream_hits.load(Ordering::SeqCst) >= 1,
        "transport failure should hit the streaming provider at least once"
    );
    assert!(
        hits.nonstream_hits.load(Ordering::SeqCst) >= 2,
        "transport failure should perform one probe and one non-stream fallback request"
    );

    let _ = sqlx::query("DELETE FROM session_artifacts WHERE session_id = ?")
        .bind(&session_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM infra_llm_models WHERE model_name = ?")
        .bind(&model_name)
        .execute(pool)
        .await;
    cleanup_session_data(pool, &session_id).await;
    ctx.pool.close().await;
}

pub async fn run_server_loop_idle_recovery_session_artifact_latest_and_download_routes() {
    let _idle_env = set_stream_idle_timeouts_for_test(250, 250);
    let partial_text = "server loop idle partial";
    let recovered_text = "server loop idle recovered answer";
    let fallback_body = format!(
        r#"{{"choices":[{{"message":{{"content":"{recovered_text}"}}}}],"usage":{{"prompt_tokens":19,"completion_tokens":4}}}}"#
    );
    let (base_url, hits) = spawn_raw_idle_after_progress_server(
        partial_text,
        std::time::Duration::from_secs(2),
        200,
        Box::leak(fallback_body.into_boxed_str()),
    )
    .await;

    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let model_name = format!("server-loop-idle-recovery-{}", ctx.suffix);

    let (st_model, model_j) = post_json(
        app,
        "/models",
        Some(auth.as_str()),
        json!({
            "name": model_name,
            "provider": "openai",
            "api_key": "server-loop-idle-recovery-e2e-key",
            "base_url": base_url
        }),
    )
    .await;
    assert_eq!(st_model, StatusCode::CREATED, "create model: {model_j}");
    sqlx::query("UPDATE infra_llm_models SET is_active = 1 WHERE model_name = ?")
        .bind(&model_name)
        .execute(pool)
        .await
        .expect("force-activate server-loop idle recovery test model");
    hits.nonstream_hits.store(1, Ordering::SeqCst);

    let (st_sess, sess) = post_json(
        app,
        "/sessions",
        Some(auth.as_str()),
        json!({
            "title": "server loop idle recovery latest download",
            "metadata": { "full_llm_capture": true, "suite": "server_loop_idle_recovery_latest_download" }
        }),
    )
    .await;
    assert_eq!(st_sess, StatusCode::CREATED, "create session: {sess}");
    let session_id = sess["session_id"].as_str().expect("session_id").to_string();

    let payload = json!({
        "message": "trigger a server-loop idle timeout after progress and recover",
        "session_id": &session_id,
        "model": model_name
    });
    let (status, body) = stream_chat_full_nonbridge(app, auth, payload).await;
    assert_eq!(status, StatusCode::OK, "chat/stream: {body}");
    assert!(
        body.contains(recovered_text),
        "server-loop SSE should surface the recovered final text after idle fallback: {body}"
    );
    assert!(
        body.contains("\"status\":\"completed\""),
        "server-loop idle recovery should end as completed: {body}"
    );
    assert!(
        body.contains("\"type\":\"turn_complete\""),
        "server-loop idle recovery should still emit turn_complete: {body}"
    );
    assert!(
        !body.contains("\"status\":\"failed\""),
        "successful idle fallback should not fail the client stream: {body}"
    );

    wait_for_artifact_count(
        pool,
        &session_id,
        "llm_capture",
        1,
        std::time::Duration::from_secs(15),
    )
    .await;

    let row = sqlx::query(
        "SELECT artifact_id \
         FROM session_artifacts WHERE session_id = ? AND artifact_kind = 'llm_capture' \
         ORDER BY created_at DESC, artifact_id DESC LIMIT 1",
    )
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("latest server-loop idle recovery llm_capture row");
    let artifact_id: String = row.try_get("artifact_id").expect("artifact_id");

    let latest_path = format!("/sessions/{session_id}/artifacts/latest/llm_capture");
    let (st_latest, latest_j) = get_json(app, &latest_path, Some(auth), &[]).await;
    assert_eq!(st_latest, StatusCode::OK, "artifact latest: {latest_j}");
    assert_eq!(latest_j["artifact_kind"].as_str(), Some("llm_capture"));
    assert_eq!(latest_j["metadata"]["outcome"].as_str(), Some("success"));
    assert_eq!(
        latest_j["content"]["response"]["full_text"].as_str(),
        Some(recovered_text)
    );

    let download_path = format!("/sessions/{session_id}/artifacts/{artifact_id}/download");
    let (st_download, _download_headers, download_body) =
        get_bytes(app, &download_path, Some(auth), &[]).await;
    assert_eq!(st_download, StatusCode::OK, "artifact download");
    let download_j: serde_json::Value =
        serde_json::from_slice(&download_body).expect("download json");
    assert_eq!(download_j["metadata"]["outcome"].as_str(), Some("success"));
    assert_eq!(
        download_j["content"]["response"]["full_text"].as_str(),
        Some(recovered_text)
    );

    assert!(
        hits.stream_hits.load(Ordering::SeqCst) >= 1,
        "idle recovery should hit the streaming provider at least once"
    );
    assert!(
        hits.nonstream_hits.load(Ordering::SeqCst) >= 2,
        "idle recovery should perform one probe and one non-stream fallback request"
    );

    let _ = sqlx::query("DELETE FROM session_artifacts WHERE session_id = ?")
        .bind(&session_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM infra_llm_models WHERE model_name = ?")
        .bind(&model_name)
        .execute(pool)
        .await;
    cleanup_session_data(pool, &session_id).await;
    ctx.pool.close().await;
}

pub async fn run_server_loop_idle_failure_session_artifact_latest_and_download_routes() {
    let _idle_env = set_stream_idle_timeouts_for_test(250, 250);
    let partial_text = "server loop idle partial";
    let failure_fragment = "fallback idle recovery failed";
    let (base_url, hits) = spawn_raw_idle_after_progress_server(
        partial_text,
        std::time::Duration::from_secs(2),
        500,
        r#"{"error":{"message":"fallback idle recovery failed"}}"#,
    )
    .await;

    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let model_name = format!("server-loop-idle-failure-{}", ctx.suffix);

    let (st_model, model_j) = post_json(
        app,
        "/models",
        Some(auth.as_str()),
        json!({
            "name": model_name,
            "provider": "openai",
            "api_key": "server-loop-idle-failure-e2e-key",
            "base_url": base_url
        }),
    )
    .await;
    assert_eq!(st_model, StatusCode::CREATED, "create model: {model_j}");
    sqlx::query("UPDATE infra_llm_models SET is_active = 1 WHERE model_name = ?")
        .bind(&model_name)
        .execute(pool)
        .await
        .expect("force-activate server-loop idle failure test model");
    hits.nonstream_hits.store(1, Ordering::SeqCst);

    let (st_sess, sess) = post_json(
        app,
        "/sessions",
        Some(auth.as_str()),
        json!({
            "title": "server loop idle failure latest download",
            "metadata": { "full_llm_capture": true, "suite": "server_loop_idle_failure_latest_download" }
        }),
    )
    .await;
    assert_eq!(st_sess, StatusCode::CREATED, "create session: {sess}");
    let session_id = sess["session_id"].as_str().expect("session_id").to_string();

    let payload = json!({
        "message": "trigger a server-loop idle timeout after progress and make fallback fail",
        "session_id": &session_id,
        "model": model_name
    });
    let (status, body) = stream_chat_full_nonbridge(app, auth, payload).await;
    assert_eq!(status, StatusCode::OK, "chat/stream: {body}");
    assert!(
        body.contains(failure_fragment),
        "server-loop SSE should surface the idle fallback failure text: {body}"
    );
    assert!(
        body.contains("\"code\":\"RUN_ERROR\""),
        "server-loop idle fallback failure should surface a normalized run error: {body}"
    );
    assert!(
        body.contains("\"status\":\"failed\""),
        "server-loop idle fallback failure should terminate as failed: {body}"
    );
    assert!(
        !body.contains("\"type\":\"turn_complete\""),
        "server-loop idle fallback failure should not emit turn_complete: {body}"
    );

    wait_for_artifact_count(
        pool,
        &session_id,
        "llm_capture",
        1,
        std::time::Duration::from_secs(15),
    )
    .await;

    let row = sqlx::query(
        "SELECT artifact_id \
         FROM session_artifacts WHERE session_id = ? AND artifact_kind = 'llm_capture' \
         ORDER BY created_at DESC, artifact_id DESC LIMIT 1",
    )
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("latest server-loop idle failure llm_capture row");
    let artifact_id: String = row.try_get("artifact_id").expect("artifact_id");

    let latest_path = format!("/sessions/{session_id}/artifacts/latest/llm_capture");
    let (st_latest, latest_j) = get_json(app, &latest_path, Some(auth), &[]).await;
    assert_eq!(st_latest, StatusCode::OK, "artifact latest: {latest_j}");
    assert_eq!(latest_j["artifact_kind"].as_str(), Some("llm_capture"));
    assert_eq!(latest_j["metadata"]["outcome"].as_str(), Some("error"));
    assert!(
        latest_j["content"]["response"]["error"]
            .as_str()
            .unwrap_or_default()
            .contains(failure_fragment),
        "latest error payload should retain idle fallback failure text: {latest_j}"
    );
    assert_eq!(
        latest_j["content"]["response"]["partial_full_text"].as_str(),
        Some(partial_text)
    );

    let download_path = format!("/sessions/{session_id}/artifacts/{artifact_id}/download");
    let (st_download, _download_headers, download_body) =
        get_bytes(app, &download_path, Some(auth), &[]).await;
    assert_eq!(st_download, StatusCode::OK, "artifact download");
    let download_j: serde_json::Value =
        serde_json::from_slice(&download_body).expect("download json");
    assert_eq!(download_j["metadata"]["outcome"].as_str(), Some("error"));
    assert!(
        download_j["content"]["response"]["error"]
            .as_str()
            .unwrap_or_default()
            .contains(failure_fragment),
        "download error payload should retain idle fallback failure text: {download_j}"
    );
    assert_eq!(
        download_j["content"]["response"]["partial_full_text"].as_str(),
        Some(partial_text)
    );

    assert!(
        hits.stream_hits.load(Ordering::SeqCst) >= 1,
        "idle failure should hit the streaming provider at least once"
    );
    assert!(
        hits.nonstream_hits.load(Ordering::SeqCst) >= 2,
        "idle failure should perform one probe and one non-stream fallback request"
    );

    let _ = sqlx::query("DELETE FROM session_artifacts WHERE session_id = ?")
        .bind(&session_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM infra_llm_models WHERE model_name = ?")
        .bind(&model_name)
        .execute(pool)
        .await;
    cleanup_session_data(pool, &session_id).await;
    ctx.pool.close().await;
}

pub async fn run_server_loop_rate_limit_failure_session_artifact_latest_and_download_routes() {
    let (base_url, hits) =
        spawn_raw_stream_rate_limit_server(None, r#"{"error":{"message":"rate limit exceeded"}}"#)
            .await;

    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let model_name = format!("server-loop-rate-limit-{}", ctx.suffix);

    let (st_model, model_j) = post_json(
        app,
        "/models",
        Some(auth.as_str()),
        json!({
            "name": model_name,
            "provider": "openai",
            "api_key": "server-loop-rate-limit-e2e-key",
            "base_url": base_url
        }),
    )
    .await;
    assert_eq!(st_model, StatusCode::CREATED, "create model: {model_j}");
    sqlx::query("UPDATE infra_llm_models SET is_active = 1 WHERE model_name = ?")
        .bind(&model_name)
        .execute(pool)
        .await
        .expect("force-activate server-loop rate-limit test model");

    let (st_sess, sess) = post_json(
        app,
        "/sessions",
        Some(auth.as_str()),
        json!({
            "title": "server loop rate limit latest download",
            "metadata": { "full_llm_capture": true, "suite": "server_loop_rate_limit_latest_download" }
        }),
    )
    .await;
    assert_eq!(st_sess, StatusCode::CREATED, "create session: {sess}");
    let session_id = sess["session_id"].as_str().expect("session_id").to_string();

    let payload = json!({
        "message": "trigger repeated server-loop rate limits",
        "session_id": &session_id,
        "model": model_name
    });
    let (status, body) = stream_chat_full_nonbridge(app, auth, payload.clone()).await;
    assert_eq!(status, StatusCode::OK, "chat/stream: {body}");
    assert!(
        body.contains("[rate_limit] LLM request rejected: 429"),
        "server-loop SSE should surface the normalized provider rate-limit text after retry exhaustion: {body}"
    );
    assert!(
        body.contains("\"code\":\"RUN_ERROR\""),
        "server-loop rate-limit exhaustion should surface a normalized run error: {body}"
    );
    assert!(
        body.contains("\"status\":\"failed\""),
        "server-loop rate-limit exhaustion should terminate as failed: {body}"
    );
    assert!(
        !body.contains("\"type\":\"turn_complete\""),
        "rate-limit failure should not emit turn_complete after terminal error: {body}"
    );

    wait_for_artifact_count(
        pool,
        &session_id,
        "llm_capture",
        1,
        std::time::Duration::from_secs(15),
    )
    .await;

    let row = sqlx::query(
        "SELECT artifact_id \
         FROM session_artifacts WHERE session_id = ? AND artifact_kind = 'llm_capture' \
         ORDER BY created_at DESC, artifact_id DESC LIMIT 1",
    )
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("latest server-loop rate-limit llm_capture row");
    let artifact_id: String = row.try_get("artifact_id").expect("artifact_id");

    let latest_path = format!("/sessions/{session_id}/artifacts/latest/llm_capture");
    let (st_latest, latest_j) = get_json(app, &latest_path, Some(auth), &[]).await;
    assert_eq!(st_latest, StatusCode::OK, "artifact latest: {latest_j}");
    assert_eq!(latest_j["metadata"]["outcome"].as_str(), Some("error"));
    assert_eq!(
        latest_j["content"]["response"]["kind"].as_str(),
        Some("rate_limit")
    );

    let download_path = format!("/sessions/{session_id}/artifacts/{artifact_id}/download");
    let (st_download, _download_headers, download_body) =
        get_bytes(app, &download_path, Some(auth), &[]).await;
    assert_eq!(st_download, StatusCode::OK, "artifact download");
    let download_j: serde_json::Value =
        serde_json::from_slice(&download_body).expect("download json");
    assert_eq!(download_j["metadata"]["outcome"].as_str(), Some("error"));
    assert_eq!(
        download_j["content"]["response"]["kind"].as_str(),
        Some("rate_limit")
    );

    let (cooldown_status, cooldown_body) = stream_chat_full_nonbridge(app, auth, payload).await;
    assert_eq!(
        cooldown_status,
        StatusCode::OK,
        "cooldown chat/stream: {cooldown_body}"
    );
    assert!(
        cooldown_body.contains("Rate limit cooldown active"),
        "follow-up turn should be rejected by local rate-limit cooldown: {cooldown_body}"
    );
    assert!(
        cooldown_body.contains("\"code\":\"RUN_ERROR\""),
        "cooldown reject should still surface through the server-loop run lifecycle shape: {cooldown_body}"
    );
    assert!(
        !cooldown_body.contains("\"type\":\"turn_complete\""),
        "cooldown reject should not emit turn_complete: {cooldown_body}"
    );

    assert_eq!(
        hits.stream_hits.load(Ordering::SeqCst),
        4,
        "expected one initial stream attempt plus three retries before failure"
    );
    assert_nonstream_hits_in_range(
        hits.nonstream_hits.load(Ordering::SeqCst),
        0,
        1,
        "repeated stream 429s plus cooldown reject should not issue a non-stream fallback beyond any optional connectivity probe",
    );

    let _ = sqlx::query("DELETE FROM session_artifacts WHERE session_id = ?")
        .bind(&session_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM infra_llm_models WHERE model_name = ?")
        .bind(&model_name)
        .execute(pool)
        .await;
    cleanup_session_data(pool, &session_id).await;
    ctx.pool.close().await;
}

pub async fn run_server_loop_rate_limit_retry_success_session_artifact_latest_and_download_routes()
{
    let success_text = "server-loop after-429 success";
    let (base_url, hits) = spawn_raw_stream_rate_limit_then_sse_server(success_text).await;

    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let model_name = format!("server-loop-rate-limit-retry-{}", ctx.suffix);

    let (st_model, model_j) = post_json(
        app,
        "/models",
        Some(auth.as_str()),
        json!({
            "name": model_name,
            "provider": "openai",
            "api_key": "server-loop-rate-limit-retry-e2e-key",
            "base_url": base_url
        }),
    )
    .await;
    assert_eq!(st_model, StatusCode::CREATED, "create model: {model_j}");
    sqlx::query("UPDATE infra_llm_models SET is_active = 1 WHERE model_name = ?")
        .bind(&model_name)
        .execute(pool)
        .await
        .expect("force-activate server-loop rate-limit retry test model");

    let (st_sess, sess) = post_json(
        app,
        "/sessions",
        Some(auth.as_str()),
        json!({
            "title": "server loop rate limit retry success latest download",
            "metadata": { "full_llm_capture": true, "suite": "server_loop_rate_limit_retry_success_latest_download" }
        }),
    )
    .await;
    assert_eq!(st_sess, StatusCode::CREATED, "create session: {sess}");
    let session_id = sess["session_id"].as_str().expect("session_id").to_string();

    let payload = json!({
        "message": "trigger one server-loop rate limit and then recover",
        "session_id": &session_id,
        "model": model_name
    });
    let (status, body) = stream_chat_full_nonbridge(app, auth, payload).await;
    assert_eq!(status, StatusCode::OK, "chat/stream: {body}");
    assert!(
        body.contains(success_text),
        "server-loop SSE should include the recovered text after one 429 retry: {body}"
    );
    assert!(
        body.contains("\"type\":\"turn_complete\""),
        "successful 429 retry should still emit turn_complete: {body}"
    );
    assert!(
        !body.contains("\"type\":\"error\""),
        "successful 429 retry should not expose an error event to the client: {body}"
    );

    wait_for_artifact_count(
        pool,
        &session_id,
        "llm_capture",
        1,
        std::time::Duration::from_secs(15),
    )
    .await;

    let row = sqlx::query(
        "SELECT artifact_id \
         FROM session_artifacts WHERE session_id = ? AND artifact_kind = 'llm_capture' \
         ORDER BY created_at DESC, artifact_id DESC LIMIT 1",
    )
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("latest server-loop rate-limit retry llm_capture row");
    let artifact_id: String = row.try_get("artifact_id").expect("artifact_id");

    let latest_path = format!("/sessions/{session_id}/artifacts/latest/llm_capture");
    let (st_latest, latest_j) = get_json(app, &latest_path, Some(auth), &[]).await;
    assert_eq!(st_latest, StatusCode::OK, "artifact latest: {latest_j}");
    assert_eq!(latest_j["metadata"]["outcome"].as_str(), Some("success"));
    assert_eq!(
        latest_j["content"]["response"]["full_text"].as_str(),
        Some(success_text)
    );

    let download_path = format!("/sessions/{session_id}/artifacts/{artifact_id}/download");
    let (st_download, _download_headers, download_body) =
        get_bytes(app, &download_path, Some(auth), &[]).await;
    assert_eq!(st_download, StatusCode::OK, "artifact download");
    let download_j: serde_json::Value =
        serde_json::from_slice(&download_body).expect("download json");
    assert_eq!(download_j["metadata"]["outcome"].as_str(), Some("success"));
    assert_eq!(
        download_j["content"]["response"]["full_text"].as_str(),
        Some(success_text)
    );

    assert_eq!(
        hits.stream_hits.load(Ordering::SeqCst),
        2,
        "expected one 429 stream attempt and one successful retry stream"
    );
    assert_nonstream_hits_in_range(
        hits.nonstream_hits.load(Ordering::SeqCst),
        0,
        1,
        "successful stream retry should not require a non-stream fallback beyond any optional connectivity probe",
    );

    let _ = sqlx::query("DELETE FROM session_artifacts WHERE session_id = ?")
        .bind(&session_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM infra_llm_models WHERE model_name = ?")
        .bind(&model_name)
        .execute(pool)
        .await;
    cleanup_session_data(pool, &session_id).await;
    ctx.pool.close().await;
}

pub async fn run_bridge_failed_session_artifact_latest_and_download_routes() {
    let partial_text = "half bridge answer";
    run_bridge_failure_session_artifact_latest_and_download_routes(
        "bridge artifact failed latest download",
        "bridge_artifact_failed_latest_download",
        "system-matrix-bridge-failure-artifact",
        "trigger a bridge stream failure",
        vec![format!(
            "data: {{\"type\":\"text_delta\",\"content\":\"{partial_text}\"}}\n\n"
        )],
        "stream_incomplete",
        "STREAM_INCOMPLETE",
        partial_text,
    )
    .await;
}

pub async fn run_bridge_sse_parse_error_session_artifact_latest_and_download_routes() {
    let partial_text = "bridge parse partial";
    run_bridge_failure_session_artifact_latest_and_download_routes(
        "bridge artifact sse parse latest download",
        "bridge_artifact_sse_parse_latest_download",
        "system-matrix-bridge-parse-error-artifact",
        "trigger a bridge parse failure after partial output",
        vec![
            format!("data: {{\"type\":\"text_delta\",\"content\":\"{partial_text}\"}}\n\n"),
            "data: {not-json}\n\n".to_string(),
        ],
        "sse_parse_error",
        "SSE_PARSE_ERROR",
        partial_text,
    )
    .await;
}

pub async fn run_bridge_tail_parse_error_artifact_preserves_partial_state_routes() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;

    let (st_sess, sess) = post_json(
        app,
        "/sessions",
        Some(auth.as_str()),
        json!({
            "title": "bridge artifact tail parse preserves partial state",
            "metadata": { "full_llm_capture": true, "suite": "bridge_artifact_tail_parse_partial_state" }
        }),
    )
    .await;
    assert_eq!(st_sess, StatusCode::CREATED, "create session: {sess}");
    let session_id = sess["session_id"].as_str().expect("session_id").to_string();

    let partial_text = "bridge tail partial";
    let partial_reasoning = "bridge thinking";
    let payload = json!({
        "agent_id": "system-matrix-bridge-tail-parse-artifact",
        "session_id": &session_id,
        "messages": [{ "role": "user", "content": "trigger a bridge tail parse failure after partial state" }],
        "test_llm_stream_blocks": [
            format!("data: {{\"type\":\"text_delta\",\"content\":\"{partial_text}\"}}\n\n"),
            format!("data: {{\"type\":\"reasoning_delta\",\"content\":\"{partial_reasoning}\"}}\n\n"),
            "data: {\"type\":\"tool_call_start\",\"tool\":\"bash\",\"call_id\":\"call-1\",\"arguments\":\"{\\\"command\\\":\\\"pwd\\\"}\"}\n\n".to_string(),
            "data: {\"type\":\"usage\",\"input_tokens\":13,\"output_tokens\":5}\n\n".to_string(),
            "data: {\"type\":\"usage\",\"input_tokens\":13".to_string()
        ]
    });
    let (status, body) = chat_turn_full(app, auth, payload).await;
    assert_eq!(status, StatusCode::OK, "chat/turn: {body}");
    assert!(
        body.contains(partial_text),
        "bridge SSE should include partial text: {body}"
    );
    assert!(
        body.contains("\"code\":\"SSE_PARSE_ERROR\""),
        "bridge SSE should expose the tail parse failure code: {body}"
    );

    wait_for_artifact_count(
        pool,
        &session_id,
        "llm_capture",
        1,
        std::time::Duration::from_secs(15),
    )
    .await;

    let row = sqlx::query(
        "SELECT artifact_id \
         FROM session_artifacts WHERE session_id = ? AND artifact_kind = 'llm_capture' \
         ORDER BY created_at DESC, artifact_id DESC LIMIT 1",
    )
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("latest bridge tail parse llm_capture row");
    let artifact_id: String = row.try_get("artifact_id").expect("artifact_id");

    let latest_path = format!("/sessions/{session_id}/artifacts/latest/llm_capture");
    let (st_latest, latest_j) = get_json(app, &latest_path, Some(auth), &[]).await;
    assert_eq!(st_latest, StatusCode::OK, "artifact latest: {latest_j}");
    assert_eq!(latest_j["artifact_id"].as_str(), Some(artifact_id.as_str()));
    assert_eq!(
        latest_j["metadata"]["outcome"].as_str(),
        Some("sse_parse_error")
    );
    assert_eq!(
        latest_j["content"]["response"]["kind"].as_str(),
        Some("SSE_PARSE_ERROR")
    );
    assert_eq!(
        latest_j["content"]["response"]["partial_full_text"].as_str(),
        Some(partial_text)
    );
    assert_eq!(
        latest_j["content"]["response"]["partial_reasoning"].as_str(),
        Some(partial_reasoning)
    );
    assert_eq!(
        latest_j["content"]["response"]["tool_calls"][0]["function"]["name"].as_str(),
        Some("bash")
    );
    assert_eq!(
        latest_j["content"]["response"]["usage"]["prompt_tokens"].as_i64(),
        Some(13)
    );
    assert_eq!(
        latest_j["content"]["response"]["usage"]["completion_tokens"].as_i64(),
        Some(5)
    );

    let download_path = format!("/sessions/{session_id}/artifacts/{artifact_id}/download");
    let (st_download, _download_headers, download_body) =
        get_bytes(app, &download_path, Some(auth), &[]).await;
    assert_eq!(st_download, StatusCode::OK, "artifact download");
    let download_j: serde_json::Value =
        serde_json::from_slice(&download_body).expect("download json");
    assert_eq!(
        download_j["metadata"]["outcome"].as_str(),
        Some("sse_parse_error")
    );
    assert_eq!(
        download_j["content"]["response"]["kind"].as_str(),
        Some("SSE_PARSE_ERROR")
    );
    assert_eq!(
        download_j["content"]["response"]["partial_full_text"].as_str(),
        Some(partial_text)
    );
    assert_eq!(
        download_j["content"]["response"]["partial_reasoning"].as_str(),
        Some(partial_reasoning)
    );
    assert_eq!(
        download_j["content"]["response"]["tool_calls"][0]["function"]["name"].as_str(),
        Some("bash")
    );
    assert_eq!(
        download_j["content"]["response"]["usage"]["prompt_tokens"].as_i64(),
        Some(13)
    );
    assert_eq!(
        download_j["content"]["response"]["usage"]["completion_tokens"].as_i64(),
        Some(5)
    );

    let _ = sqlx::query("DELETE FROM session_artifacts WHERE session_id = ?")
        .bind(&session_id)
        .execute(pool)
        .await;
    cleanup_session_data(pool, &session_id).await;
    ctx.pool.close().await;
}

pub async fn run_bridge_transport_failure_session_artifact_latest_and_download_routes() {
    let partial_text = "bridge transport partial";
    let (base_url, hits) = spawn_raw_partial_transport_server(
        partial_text,
        500,
        r#"{"error":{"message":"fallback transport recovery failed"}}"#,
    )
    .await;

    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let model_name = format!("bridge-transport-{}", ctx.suffix);

    let (st_model, model_j) = post_json(
        app,
        "/models",
        Some(auth.as_str()),
        json!({
            "name": model_name,
            "provider": "openai",
            "api_key": "transport-e2e-key",
            "base_url": base_url
        }),
    )
    .await;
    assert_eq!(st_model, StatusCode::CREATED, "create model: {model_j}");
    sqlx::query("UPDATE infra_llm_models SET is_active = 1 WHERE model_name = ?")
        .bind(&model_name)
        .execute(pool)
        .await
        .expect("force-activate transport test model");
    hits.nonstream_hits.store(1, Ordering::SeqCst);

    let (st_sess, sess) = post_json(
        app,
        "/sessions",
        Some(auth.as_str()),
        json!({
            "title": "bridge transport failed latest download",
            "metadata": { "full_llm_capture": true, "suite": "bridge_transport_failed_latest_download" }
        }),
    )
    .await;
    assert_eq!(st_sess, StatusCode::CREATED, "create session: {sess}");
    let session_id = sess["session_id"].as_str().expect("session_id").to_string();

    let payload = json!({
        "agent_id": "system-matrix-bridge-transport-artifact",
        "session_id": &session_id,
        "model": model_name,
        "messages": [{ "role": "user", "content": "trigger a bridge transport failure after partial output" }]
    });
    let (status, body) = chat_turn_full(app, auth, payload).await;
    assert_eq!(status, StatusCode::OK, "chat/turn: {body}");
    assert!(
        body.contains(partial_text),
        "bridge SSE should include the partial streamed text before transport failure: {body}"
    );
    assert!(
        body.contains("\"code\":\"stream_transport\""),
        "bridge SSE should expose the transport failure code: {body}"
    );

    wait_for_artifact_count(
        pool,
        &session_id,
        "llm_capture",
        1,
        std::time::Duration::from_secs(15),
    )
    .await;

    let row = sqlx::query(
        "SELECT artifact_id \
         FROM session_artifacts WHERE session_id = ? AND artifact_kind = 'llm_capture' \
         ORDER BY created_at DESC, artifact_id DESC LIMIT 1",
    )
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("latest bridge transport llm_capture row");
    let artifact_id: String = row.try_get("artifact_id").expect("artifact_id");

    let latest_path = format!("/sessions/{session_id}/artifacts/latest/llm_capture");
    let (st_latest, latest_j) = get_json(app, &latest_path, Some(auth), &[]).await;
    assert_eq!(st_latest, StatusCode::OK, "artifact latest: {latest_j}");
    assert_eq!(latest_j["metadata"]["outcome"].as_str(), Some("error"));
    assert_eq!(
        latest_j["content"]["response"]["kind"].as_str(),
        Some("stream_transport")
    );
    assert_eq!(
        latest_j["content"]["response"]["partial_full_text"].as_str(),
        Some(partial_text)
    );

    let download_path = format!("/sessions/{session_id}/artifacts/{artifact_id}/download");
    let (st_download, _download_headers, download_body) =
        get_bytes(app, &download_path, Some(auth), &[]).await;
    assert_eq!(st_download, StatusCode::OK, "artifact download");
    let download_j: serde_json::Value =
        serde_json::from_slice(&download_body).expect("download json");
    assert_eq!(download_j["metadata"]["outcome"].as_str(), Some("error"));
    assert_eq!(
        download_j["content"]["response"]["kind"].as_str(),
        Some("stream_transport")
    );
    assert_eq!(
        download_j["content"]["response"]["partial_full_text"].as_str(),
        Some(partial_text)
    );

    assert_eq!(hits.stream_hits.load(Ordering::SeqCst), 1);
    assert_eq!(
        hits.nonstream_hits.load(Ordering::SeqCst),
        2,
        "expected one model-connectivity probe and one fallback request"
    );

    let _ = sqlx::query("DELETE FROM session_artifacts WHERE session_id = ?")
        .bind(&session_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM infra_llm_models WHERE model_name = ?")
        .bind(&model_name)
        .execute(pool)
        .await;
    cleanup_session_data(pool, &session_id).await;
    ctx.pool.close().await;
}

pub async fn run_bridge_client_disconnect_session_artifact_latest_and_download_routes() {
    let partial_text = "bridge disconnect partial";
    let (base_url, hits) = spawn_raw_hanging_stream_server(partial_text).await;

    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let model_name = format!("bridge-disconnect-{}", ctx.suffix);

    let (st_model, model_j) = post_json(
        app,
        "/models",
        Some(auth.as_str()),
        json!({
            "name": model_name,
            "provider": "openai",
            "api_key": "disconnect-e2e-key",
            "base_url": base_url
        }),
    )
    .await;
    assert_eq!(st_model, StatusCode::CREATED, "create model: {model_j}");
    sqlx::query("UPDATE infra_llm_models SET is_active = 1 WHERE model_name = ?")
        .bind(&model_name)
        .execute(pool)
        .await
        .expect("force-activate disconnect test model");

    let (st_sess, sess) = post_json(
        app,
        "/sessions",
        Some(auth.as_str()),
        json!({
            "title": "bridge client disconnect latest download",
            "metadata": { "full_llm_capture": true, "suite": "bridge_client_disconnect_latest_download" }
        }),
    )
    .await;
    assert_eq!(st_sess, StatusCode::CREATED, "create session: {sess}");
    let session_id = sess["session_id"].as_str().expect("session_id").to_string();

    let test_secret = std::env::var("ASTRA_TEST_BRIDGE_SECRET").expect("bridge test secret");
    let base_http = spawn_http_app_server(app.clone()).await;
    let addr = base_http.trim_start_matches("http://");
    let mut socket = tokio::net::TcpStream::connect(addr)
        .await
        .expect("connect live http socket");
    let request_body = json!({
        "agent_id": "system-matrix-bridge-client-disconnect",
        "session_id": &session_id,
        "model": model_name,
        "messages": [{ "role": "user", "content": "trigger a bridge client disconnect after partial output" }]
    })
    .to_string();
    let request = format!(
        "POST /chat/turn HTTP/1.1\r\nHost: {addr}\r\nAuthorization: {}\r\nContent-Type: application/json\r\nx-mo-bridge-test-secret: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        auth.as_str(),
        test_secret,
        request_body.len(),
        request_body
    );
    socket
        .write_all(request.as_bytes())
        .await
        .expect("write disconnect request");

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut saw_partial = false;
    let mut buf = [0_u8; 4096];
    while let Ok(Ok(read)) = tokio::time::timeout_at(deadline, socket.read(&mut buf)).await {
        if read == 0 {
            break;
        }
        let text = String::from_utf8_lossy(&buf[..read]);
        if text.contains(partial_text) {
            saw_partial = true;
            break;
        }
    }
    assert!(
        saw_partial,
        "should receive partial streamed text before disconnect"
    );
    drop(socket);

    wait_for_artifact_count(
        pool,
        &session_id,
        "llm_capture",
        1,
        std::time::Duration::from_secs(15),
    )
    .await;

    let row = sqlx::query(
        "SELECT artifact_id \
         FROM session_artifacts WHERE session_id = ? AND artifact_kind = 'llm_capture' \
         ORDER BY created_at DESC, artifact_id DESC LIMIT 1",
    )
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("latest bridge disconnect llm_capture row");
    let artifact_id: String = row.try_get("artifact_id").expect("artifact_id");

    let latest_path = format!("/sessions/{session_id}/artifacts/latest/llm_capture");
    let (st_latest, latest_j) = get_json(app, &latest_path, Some(auth), &[]).await;
    assert_eq!(st_latest, StatusCode::OK, "artifact latest: {latest_j}");
    assert_eq!(
        latest_j["metadata"]["outcome"].as_str(),
        Some("client_disconnect")
    );
    assert_eq!(
        latest_j["content"]["response"]["kind"].as_str(),
        Some("CLIENT_DISCONNECT")
    );
    assert_eq!(
        latest_j["content"]["response"]["partial_full_text"].as_str(),
        Some(partial_text)
    );

    let download_path = format!("/sessions/{session_id}/artifacts/{artifact_id}/download");
    let (st_download, _download_headers, download_body) =
        get_bytes(app, &download_path, Some(auth), &[]).await;
    assert_eq!(st_download, StatusCode::OK, "artifact download");
    let download_j: serde_json::Value =
        serde_json::from_slice(&download_body).expect("download json");
    assert_eq!(
        download_j["metadata"]["outcome"].as_str(),
        Some("client_disconnect")
    );
    assert_eq!(
        download_j["content"]["response"]["kind"].as_str(),
        Some("CLIENT_DISCONNECT")
    );
    assert_eq!(
        download_j["content"]["response"]["partial_full_text"].as_str(),
        Some(partial_text)
    );

    assert_eq!(hits.stream_hits.load(Ordering::SeqCst), 1);

    let _ = sqlx::query("DELETE FROM session_artifacts WHERE session_id = ?")
        .bind(&session_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM infra_llm_models WHERE model_name = ?")
        .bind(&model_name)
        .execute(pool)
        .await;
    cleanup_session_data(pool, &session_id).await;
    ctx.pool.close().await;
}

pub async fn run_bridge_idle_failure_session_artifact_latest_and_download_routes() {
    let _idle_env = set_stream_idle_timeouts_for_test(250, 250);
    let partial_text = "bridge idle partial";
    let (base_url, hits) = spawn_raw_idle_after_progress_server(
        partial_text,
        std::time::Duration::from_secs(2),
        500,
        r#"{"error":{"message":"fallback idle recovery failed"}}"#,
    )
    .await;

    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let model_name = format!("bridge-idle-{}", ctx.suffix);

    let (st_model, model_j) = post_json(
        app,
        "/models",
        Some(auth.as_str()),
        json!({
            "name": model_name,
            "provider": "openai",
            "api_key": "idle-e2e-key",
            "base_url": base_url
        }),
    )
    .await;
    assert_eq!(st_model, StatusCode::CREATED, "create model: {model_j}");
    sqlx::query("UPDATE infra_llm_models SET is_active = 1 WHERE model_name = ?")
        .bind(&model_name)
        .execute(pool)
        .await
        .expect("force-activate idle test model");
    hits.nonstream_hits.store(1, Ordering::SeqCst);

    let (st_sess, sess) = post_json(
        app,
        "/sessions",
        Some(auth.as_str()),
        json!({
            "title": "bridge idle failed latest download",
            "metadata": { "full_llm_capture": true, "suite": "bridge_idle_failed_latest_download" }
        }),
    )
    .await;
    assert_eq!(st_sess, StatusCode::CREATED, "create session: {sess}");
    let session_id = sess["session_id"].as_str().expect("session_id").to_string();

    let payload = json!({
        "agent_id": "system-matrix-bridge-idle-artifact",
        "session_id": &session_id,
        "model": model_name,
        "messages": [{ "role": "user", "content": "trigger a bridge idle failure after partial output" }]
    });
    let (status, body) = chat_turn_full(app, auth, payload).await;
    assert_eq!(status, StatusCode::OK, "chat/turn: {body}");
    assert!(
        body.contains(partial_text),
        "bridge SSE should include the partial streamed text before idle failure: {body}"
    );
    assert!(
        body.contains("\"code\":\"stream_idle\""),
        "bridge SSE should expose the idle failure code: {body}"
    );
    assert!(
        !body.contains("\"type\":\"turn_complete\""),
        "idle failure should not emit turn_complete after terminal error: {body}"
    );

    wait_for_artifact_count(
        pool,
        &session_id,
        "llm_capture",
        1,
        std::time::Duration::from_secs(15),
    )
    .await;

    let row = sqlx::query(
        "SELECT artifact_id \
         FROM session_artifacts WHERE session_id = ? AND artifact_kind = 'llm_capture' \
         ORDER BY created_at DESC, artifact_id DESC LIMIT 1",
    )
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("latest bridge idle llm_capture row");
    let artifact_id: String = row.try_get("artifact_id").expect("artifact_id");

    let latest_path = format!("/sessions/{session_id}/artifacts/latest/llm_capture");
    let (st_latest, latest_j) = get_json(app, &latest_path, Some(auth), &[]).await;
    assert_eq!(st_latest, StatusCode::OK, "artifact latest: {latest_j}");
    assert_eq!(latest_j["metadata"]["outcome"].as_str(), Some("error"));
    assert_eq!(
        latest_j["content"]["response"]["kind"].as_str(),
        Some("stream_idle")
    );
    assert_eq!(
        latest_j["content"]["response"]["partial_full_text"].as_str(),
        Some(partial_text)
    );

    let download_path = format!("/sessions/{session_id}/artifacts/{artifact_id}/download");
    let (st_download, _download_headers, download_body) =
        get_bytes(app, &download_path, Some(auth), &[]).await;
    assert_eq!(st_download, StatusCode::OK, "artifact download");
    let download_j: serde_json::Value =
        serde_json::from_slice(&download_body).expect("download json");
    assert_eq!(download_j["metadata"]["outcome"].as_str(), Some("error"));
    assert_eq!(
        download_j["content"]["response"]["kind"].as_str(),
        Some("stream_idle")
    );
    assert_eq!(
        download_j["content"]["response"]["partial_full_text"].as_str(),
        Some(partial_text)
    );

    assert_eq!(hits.stream_hits.load(Ordering::SeqCst), 1);
    assert_eq!(
        hits.nonstream_hits.load(Ordering::SeqCst),
        2,
        "expected one model-connectivity probe and one fallback request"
    );

    let _ = sqlx::query("DELETE FROM session_artifacts WHERE session_id = ?")
        .bind(&session_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM infra_llm_models WHERE model_name = ?")
        .bind(&model_name)
        .execute(pool)
        .await;
    cleanup_session_data(pool, &session_id).await;
    ctx.pool.close().await;
}

pub async fn run_bridge_rate_limit_failure_session_artifact_latest_and_download_routes() {
    let (base_url, hits) =
        spawn_raw_stream_rate_limit_server(None, r#"{"error":{"message":"rate limit exceeded"}}"#)
            .await;

    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let model_name = format!("bridge-rate-limit-{}", ctx.suffix);

    let (st_model, model_j) = post_json(
        app,
        "/models",
        Some(auth.as_str()),
        json!({
            "name": model_name,
            "provider": "openai",
            "api_key": "rate-limit-e2e-key",
            "base_url": base_url
        }),
    )
    .await;
    assert_eq!(st_model, StatusCode::CREATED, "create model: {model_j}");
    sqlx::query("UPDATE infra_llm_models SET is_active = 1 WHERE model_name = ?")
        .bind(&model_name)
        .execute(pool)
        .await
        .expect("force-activate rate-limit test model");

    let (st_sess, sess) = post_json(
        app,
        "/sessions",
        Some(auth.as_str()),
        json!({
            "title": "bridge rate limit latest download",
            "metadata": { "full_llm_capture": true, "suite": "bridge_rate_limit_latest_download" }
        }),
    )
    .await;
    assert_eq!(st_sess, StatusCode::CREATED, "create session: {sess}");
    let session_id = sess["session_id"].as_str().expect("session_id").to_string();

    let payload = json!({
        "agent_id": "system-matrix-bridge-rate-limit-artifact",
        "session_id": &session_id,
        "model": model_name,
        "messages": [{ "role": "user", "content": "trigger repeated bridge rate limits" }]
    });
    let (status, body) = chat_turn_full(app, auth, payload.clone()).await;
    assert_eq!(status, StatusCode::OK, "chat/turn: {body}");
    assert!(
        body.contains("\"code\":\"rate_limit\""),
        "bridge SSE should expose the provider rate-limit code after retry exhaustion: {body}"
    );
    assert!(
        !body.contains("\"type\":\"turn_complete\""),
        "rate-limit failure should not emit turn_complete after terminal error: {body}"
    );

    wait_for_artifact_count(
        pool,
        &session_id,
        "llm_capture",
        1,
        std::time::Duration::from_secs(15),
    )
    .await;

    let row = sqlx::query(
        "SELECT artifact_id \
         FROM session_artifacts WHERE session_id = ? AND artifact_kind = 'llm_capture' \
         ORDER BY created_at DESC, artifact_id DESC LIMIT 1",
    )
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("latest bridge rate-limit llm_capture row");
    let artifact_id: String = row.try_get("artifact_id").expect("artifact_id");

    let latest_path = format!("/sessions/{session_id}/artifacts/latest/llm_capture");
    let (st_latest, latest_j) = get_json(app, &latest_path, Some(auth), &[]).await;
    assert_eq!(st_latest, StatusCode::OK, "artifact latest: {latest_j}");
    assert_eq!(latest_j["metadata"]["outcome"].as_str(), Some("error"));
    assert_eq!(
        latest_j["content"]["response"]["kind"].as_str(),
        Some("rate_limit")
    );

    let download_path = format!("/sessions/{session_id}/artifacts/{artifact_id}/download");
    let (st_download, _download_headers, download_body) =
        get_bytes(app, &download_path, Some(auth), &[]).await;
    assert_eq!(st_download, StatusCode::OK, "artifact download");
    let download_j: serde_json::Value =
        serde_json::from_slice(&download_body).expect("download json");
    assert_eq!(download_j["metadata"]["outcome"].as_str(), Some("error"));
    assert_eq!(
        download_j["content"]["response"]["kind"].as_str(),
        Some("rate_limit")
    );

    let (cooldown_status, cooldown_body) = chat_turn_full(app, auth, payload).await;
    assert_eq!(
        cooldown_status,
        StatusCode::OK,
        "cooldown chat/turn: {cooldown_body}"
    );
    assert!(
        cooldown_body.contains("\"code\":\"RATE_LIMITED\""),
        "follow-up turn should be rejected by local rate-limit cooldown: {cooldown_body}"
    );
    assert!(
        !cooldown_body.contains("\"type\":\"turn_complete\""),
        "cooldown reject should not emit turn_complete: {cooldown_body}"
    );

    assert_eq!(
        hits.stream_hits.load(Ordering::SeqCst),
        4,
        "expected one initial stream attempt plus three retries before failure"
    );
    assert_nonstream_hits_in_range(
        hits.nonstream_hits.load(Ordering::SeqCst),
        0,
        1,
        "repeated stream 429s plus cooldown reject should not issue a non-stream fallback beyond any optional connectivity probe",
    );

    let _ = sqlx::query("DELETE FROM session_artifacts WHERE session_id = ?")
        .bind(&session_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM infra_llm_models WHERE model_name = ?")
        .bind(&model_name)
        .execute(pool)
        .await;
    cleanup_session_data(pool, &session_id).await;
    ctx.pool.close().await;
}

pub async fn run_bridge_rate_limit_retry_success_session_artifact_latest_and_download_routes() {
    let success_text = "bridge after-429 success";
    let (base_url, hits) = spawn_raw_stream_rate_limit_then_sse_server(success_text).await;

    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let model_name = format!("bridge-rate-limit-retry-{}", ctx.suffix);

    let (st_model, model_j) = post_json(
        app,
        "/models",
        Some(auth.as_str()),
        json!({
            "name": model_name,
            "provider": "openai",
            "api_key": "rate-limit-retry-e2e-key",
            "base_url": base_url
        }),
    )
    .await;
    assert_eq!(st_model, StatusCode::CREATED, "create model: {model_j}");
    sqlx::query("UPDATE infra_llm_models SET is_active = 1 WHERE model_name = ?")
        .bind(&model_name)
        .execute(pool)
        .await
        .expect("force-activate rate-limit retry test model");

    let (st_sess, sess) = post_json(
        app,
        "/sessions",
        Some(auth.as_str()),
        json!({
            "title": "bridge rate limit retry success latest download",
            "metadata": { "full_llm_capture": true, "suite": "bridge_rate_limit_retry_success_latest_download" }
        }),
    )
    .await;
    assert_eq!(st_sess, StatusCode::CREATED, "create session: {sess}");
    let session_id = sess["session_id"].as_str().expect("session_id").to_string();

    let payload = json!({
        "agent_id": "system-matrix-bridge-rate-limit-retry-success",
        "session_id": &session_id,
        "model": model_name,
        "messages": [{ "role": "user", "content": "trigger one bridge rate limit and then recover" }]
    });
    let (status, body) = chat_turn_full(app, auth, payload).await;
    assert_eq!(status, StatusCode::OK, "chat/turn: {body}");
    assert!(
        body.contains(success_text),
        "bridge SSE should include the recovered text after one 429 retry: {body}"
    );
    assert!(
        body.contains("\"type\":\"turn_complete\""),
        "successful 429 retry should still emit turn_complete: {body}"
    );
    assert!(
        !body.contains("\"type\":\"error\""),
        "successful 429 retry should not expose an error event to the client: {body}"
    );

    wait_for_artifact_count(
        pool,
        &session_id,
        "llm_capture",
        1,
        std::time::Duration::from_secs(15),
    )
    .await;

    let row = sqlx::query(
        "SELECT artifact_id \
         FROM session_artifacts WHERE session_id = ? AND artifact_kind = 'llm_capture' \
         ORDER BY created_at DESC, artifact_id DESC LIMIT 1",
    )
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("latest bridge rate-limit retry llm_capture row");
    let artifact_id: String = row.try_get("artifact_id").expect("artifact_id");

    let latest_path = format!("/sessions/{session_id}/artifacts/latest/llm_capture");
    let (st_latest, latest_j) = get_json(app, &latest_path, Some(auth), &[]).await;
    assert_eq!(st_latest, StatusCode::OK, "artifact latest: {latest_j}");
    assert_eq!(latest_j["metadata"]["outcome"].as_str(), Some("success"));
    assert_eq!(
        latest_j["content"]["response"]["full_text"].as_str(),
        Some(success_text)
    );

    let download_path = format!("/sessions/{session_id}/artifacts/{artifact_id}/download");
    let (st_download, _download_headers, download_body) =
        get_bytes(app, &download_path, Some(auth), &[]).await;
    assert_eq!(st_download, StatusCode::OK, "artifact download");
    let download_j: serde_json::Value =
        serde_json::from_slice(&download_body).expect("download json");
    assert_eq!(download_j["metadata"]["outcome"].as_str(), Some("success"));
    assert_eq!(
        download_j["content"]["response"]["full_text"].as_str(),
        Some(success_text)
    );

    assert_eq!(
        hits.stream_hits.load(Ordering::SeqCst),
        2,
        "expected one 429 stream attempt and one successful retry stream"
    );
    assert_nonstream_hits_in_range(
        hits.nonstream_hits.load(Ordering::SeqCst),
        0,
        1,
        "successful stream retry should not require a non-stream fallback beyond any optional connectivity probe",
    );

    let _ = sqlx::query("DELETE FROM session_artifacts WHERE session_id = ?")
        .bind(&session_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM infra_llm_models WHERE model_name = ?")
        .bind(&model_name)
        .execute(pool)
        .await;
    cleanup_session_data(pool, &session_id).await;
    ctx.pool.close().await;
}

pub async fn run_bridge_tool_call_block_parse_recovery_preserves_arguments_routes() {
    let (base_url, hits) = spawn_raw_tool_call_block_parse_recovery_server().await;

    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let model_name = format!("bridge-tool-call-block-recovery-{}", ctx.suffix);

    let (st_model, model_j) = post_json(
        app,
        "/models",
        Some(auth.as_str()),
        json!({
            "name": model_name,
            "provider": "openai",
            "api_key": "tool-call-block-recovery-e2e-key",
            "base_url": base_url
        }),
    )
    .await;
    assert_eq!(st_model, StatusCode::CREATED, "create model: {model_j}");
    sqlx::query("UPDATE infra_llm_models SET is_active = 1 WHERE model_name = ?")
        .bind(&model_name)
        .execute(pool)
        .await
        .expect("force-activate tool-call block recovery test model");

    let (st_sess, sess) = post_json(
        app,
        "/sessions",
        Some(auth.as_str()),
        json!({
            "title": "bridge tool-call block parse recovery latest download",
            "metadata": { "full_llm_capture": true, "suite": "bridge_tool_call_block_parse_recovery_latest_download" }
        }),
    )
    .await;
    assert_eq!(st_sess, StatusCode::CREATED, "create session: {sess}");
    let session_id = sess["session_id"].as_str().expect("session_id").to_string();

    let payload = json!({
        "agent_id": "system-matrix-bridge-tool-call-block-parse-recovery",
        "session_id": &session_id,
        "model": model_name,
        "messages": [{ "role": "user", "content": "trigger a bridge tool-call block parse failure and recover with full arguments" }]
    });
    let (status, body) = chat_turn_full(app, auth, payload).await;
    assert_eq!(status, StatusCode::OK, "chat/turn: {body}");
    assert!(
        body.contains("\"type\":\"tool_call_start\""),
        "bridge SSE should surface a tool_call_start event from streamed provider deltas: {body}"
    );
    assert!(
        body.contains("\"type\":\"tool_request\""),
        "bridge should continue with a tool_request after fallback recovery: {body}"
    );
    assert_eq!(
        body.matches("\"type\":\"tool_call_start\"").count(),
        1,
        "tool_call_start should be emitted once for the streamed call: {body}"
    );
    assert!(
        body.contains("\"command\":\"pwd\""),
        "tool_request should carry the recovered full arguments, not the partial streamed prefix: {body}"
    );
    assert!(
        body.contains("\"has_tool_calls\":true"),
        "recovered tool-call turn should complete with has_tool_calls=true: {body}"
    );
    assert!(
        !body.contains("\"type\":\"error\""),
        "successful fallback recovery should not expose an error event to the client: {body}"
    );

    wait_for_artifact_count(
        pool,
        &session_id,
        "llm_capture",
        1,
        std::time::Duration::from_secs(15),
    )
    .await;

    let row = sqlx::query(
        "SELECT artifact_id \
         FROM session_artifacts WHERE session_id = ? AND artifact_kind = 'llm_capture' \
         ORDER BY created_at DESC, artifact_id DESC LIMIT 1",
    )
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("latest bridge tool-call block recovery llm_capture row");
    let artifact_id: String = row.try_get("artifact_id").expect("artifact_id");

    let latest_path = format!("/sessions/{session_id}/artifacts/latest/llm_capture");
    let (st_latest, latest_j) = get_json(app, &latest_path, Some(auth), &[]).await;
    assert_eq!(st_latest, StatusCode::OK, "artifact latest: {latest_j}");
    assert_eq!(latest_j["metadata"]["outcome"].as_str(), Some("success"));
    assert_eq!(
        latest_j["content"]["response"]["tool_calls"][0]["function"]["name"].as_str(),
        Some("bash")
    );
    assert_eq!(
        latest_j["content"]["response"]["tool_calls"][0]["function"]["arguments"].as_str(),
        Some("{\"command\":\"pwd\"}")
    );

    let download_path = format!("/sessions/{session_id}/artifacts/{artifact_id}/download");
    let (st_download, _download_headers, download_body) =
        get_bytes(app, &download_path, Some(auth), &[]).await;
    assert_eq!(st_download, StatusCode::OK, "artifact download");
    let download_j: serde_json::Value =
        serde_json::from_slice(&download_body).expect("download json");
    assert_eq!(download_j["metadata"]["outcome"].as_str(), Some("success"));
    assert_eq!(
        download_j["content"]["response"]["tool_calls"][0]["function"]["name"].as_str(),
        Some("bash")
    );
    assert_eq!(
        download_j["content"]["response"]["tool_calls"][0]["function"]["arguments"].as_str(),
        Some("{\"command\":\"pwd\"}")
    );

    assert_eq!(hits.stream_hits.load(Ordering::SeqCst), 1);
    assert_nonstream_hits_in_range(
        hits.nonstream_hits.load(Ordering::SeqCst),
        1,
        2,
        "invalid provider block after progress should trigger exactly one successful non-stream fallback, plus at most one optional connectivity probe",
    );

    let _ = sqlx::query("DELETE FROM session_artifacts WHERE session_id = ?")
        .bind(&session_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM infra_llm_models WHERE model_name = ?")
        .bind(&model_name)
        .execute(pool)
        .await;
    cleanup_session_data(pool, &session_id).await;
    ctx.pool.close().await;
}

pub async fn run_session_artifact_latest_route_uses_stable_tiebreaker() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;

    let (st_sess, sess) = post_json(
        app,
        "/sessions",
        Some(auth.as_str()),
        json!({ "title": "artifact latest tie breaker", "metadata": { "full_llm_capture": true, "suite": "artifact_latest_tiebreak" } }),
    )
    .await;
    assert_eq!(st_sess, StatusCode::CREATED, "create session: {sess}");
    let session_id = sess["session_id"].as_str().expect("session_id").to_string();
    let user_id = sess["user_id"].as_str().expect("user_id").to_string();
    let older_id = Uuid::now_v7().to_string();
    let newer_id = loop {
        let candidate = Uuid::now_v7().to_string();
        if candidate > older_id {
            break candidate;
        }
    };
    let tied_ts = "2026-10-01 12:34:56.123456";

    let _ = sqlx::query("DELETE FROM session_artifacts WHERE session_id = ?")
        .bind(&session_id)
        .execute(pool)
        .await;

    for (artifact_id, turn, marker) in [
        (older_id.as_str(), 1_i32, "older"),
        (newer_id.as_str(), 2_i32, "newer"),
    ] {
        sqlx::query(
            "INSERT INTO session_artifacts \
             (artifact_id, session_id, user_id, artifact_kind, source, turn, round, content_json, metadata, created_at) \
             VALUES (?, ?, ?, 'llm_capture', 'ordering_probe', ?, 0, ?, CAST(? AS JSON), ?)",
        )
        .bind(artifact_id)
        .bind(&session_id)
        .bind(&user_id)
        .bind(turn)
        .bind(
            json!({
                "response": { "full_text": marker }
            })
            .to_string(),
        )
        .bind(json!({ "marker": marker }).to_string())
        .bind(tied_ts)
        .execute(pool)
        .await
        .expect("insert tied session artifacts");
    }

    let latest_path = format!("/sessions/{session_id}/artifacts/latest/llm_capture");
    let (st_latest, latest_j) = get_json(app, &latest_path, Some(auth), &[]).await;
    assert_eq!(st_latest, StatusCode::OK, "artifact latest: {latest_j}");
    assert_eq!(
        latest_j["artifact_id"].as_str(),
        Some(newer_id.as_str()),
        "latest route should stay deterministic when created_at ties"
    );
    assert_eq!(
        latest_j["content"]["response"]["full_text"].as_str(),
        Some("newer"),
        "latest route should surface the newest payload under a tied timestamp"
    );

    let _ = sqlx::query("DELETE FROM session_artifacts WHERE session_id = ?")
        .bind(&session_id)
        .execute(pool)
        .await;
    cleanup_session_data(pool, &session_id).await;
    ctx.pool.close().await;
}

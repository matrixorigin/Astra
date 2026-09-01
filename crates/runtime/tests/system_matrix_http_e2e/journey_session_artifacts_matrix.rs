//! Session artifact HTTP E2E: authenticated list/get routes align with `session_artifacts`,
//! including kind filtering, session scoping, and cross-user isolation.

use axum::http::StatusCode;
use axum::{body, body::Body, http::Request};
use futures_util::StreamExt;
use serde_json::{Value, json};
use sqlx::Row;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU32, Ordering},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::util::ServiceExt;
use uuid::Uuid;

use astra_services::session_restore::COMPOSITE_SNAPSHOT_INDEX_ARTIFACT_KIND;
use astra_services::session_workspace::WORKSPACE_METADATA_ARTIFACT_KIND;

use super::harness::{
    E2E_PASSWORD, bootstrap, cleanup_session_data, get_json, model_selection,
    offering_id_from_model_response, post_json, seeded_model_selection,
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
    loop {
        match tokio::time::timeout_at(deadline, stream.next()).await {
            Ok(Some(chunk)) => {
                let chunk = chunk.expect("body chunk");
                acc.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(_) => panic!(
                "SSE stream did not terminate within {timeout_secs}s; collected {} bytes",
                acc.len()
            ),
        }
    }
    (status, String::from_utf8_lossy(&acc).into_owned())
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

    String::from_utf8_lossy(&acc).into_owned()
}

async fn stream_chat_full_server_owned(
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

fn assert_presigned_artifact_download(
    session_id: &str,
    artifact_id: &str,
    body: &[u8],
) -> serde_json::Value {
    let download_j: serde_json::Value =
        serde_json::from_slice(body).expect("presigned artifact download json");
    assert_eq!(
        download_j["artifact_id"].as_str(),
        Some(artifact_id),
        "download descriptor should identify the requested artifact: {download_j}"
    );
    assert_eq!(
        download_j["method"].as_str(),
        Some("GET"),
        "download descriptor should use GET: {download_j}"
    );
    assert!(
        download_j["expires_at"]
            .as_str()
            .is_some_and(|expires_at| !expires_at.is_empty()),
        "download descriptor should include a non-empty expiry: {download_j}"
    );
    assert!(
        download_j["signature"]
            .as_str()
            .is_some_and(|signature| signature.starts_with("sha256:")),
        "download descriptor should include a sha256 signature: {download_j}"
    );
    let download_url = download_j["download_url"]
        .as_str()
        .expect("download descriptor URL");
    assert!(
        download_url.contains(&format!(
            "/sessions/{session_id}/artifacts/{artifact_id}/download/presigned"
        )),
        "download descriptor should target the scoped presigned route: {download_j}"
    );
    assert!(
        download_url.contains("expires_at=") && download_url.contains("signature=sha256:"),
        "download descriptor URL should carry expiry and signature query parameters: {download_j}"
    );
    download_j
}

#[derive(Clone)]
struct RawTransportServerHits {
    stream_hits: Arc<AtomicU32>,
    nonstream_hits: Arc<AtomicU32>,
    stream_request_shapes: Arc<Mutex<Vec<Value>>>,
    primary_nonstream_fallback_hits: Arc<AtomicU32>,
    primary_nonstream_fallback_requests: Arc<Mutex<Vec<String>>>,
}

impl RawTransportServerHits {
    fn new() -> Self {
        Self {
            stream_hits: Arc::new(AtomicU32::new(0)),
            nonstream_hits: Arc::new(AtomicU32::new(0)),
            stream_request_shapes: Arc::new(Mutex::new(Vec::new())),
            primary_nonstream_fallback_hits: Arc::new(AtomicU32::new(0)),
            primary_nonstream_fallback_requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn record_stream(&self, req: &str) {
        self.stream_hits.fetch_add(1, Ordering::SeqCst);
        if let Some(shape) = provider_request_shape(req) {
            self.stream_request_shapes
                .lock()
                .expect("stream request shape lock")
                .push(shape);
        }
    }

    fn record_nonstream(&self, req: &str) -> u32 {
        let previous = self.nonstream_hits.fetch_add(1, Ordering::SeqCst);
        let matches_stream_request = provider_request_shape(req).is_some_and(|shape| {
            self.stream_request_shapes
                .lock()
                .expect("stream request shape lock")
                .contains(&shape)
        });
        if matches_stream_request {
            self.primary_nonstream_fallback_hits
                .fetch_add(1, Ordering::SeqCst);
            self.primary_nonstream_fallback_requests
                .lock()
                .expect("primary nonstream fallback request log lock")
                .push(summarize_provider_request(req));
        }
        previous
    }

    fn primary_nonstream_fallback_request_summary(&self) -> String {
        let requests = self
            .primary_nonstream_fallback_requests
            .lock()
            .expect("primary nonstream fallback request log lock");
        if requests.is_empty() {
            return "<none>".to_string();
        }
        requests.join(" | ")
    }
}

fn provider_request_shape(req: &str) -> Option<Value> {
    let body = req.split("\r\n\r\n").nth(1)?;
    let mut value: Value = serde_json::from_str(body).ok()?;
    let object = value.as_object_mut()?;
    object.remove("stream");
    object.remove("stream_options");
    Some(value)
}

fn summarize_provider_request(req: &str) -> String {
    let body = req
        .split("\r\n\r\n")
        .nth(1)
        .unwrap_or(req)
        .replace('\n', " ");
    body.chars().take(260).collect()
}

type StreamIdleEnvGuard = astra_runtime::turn::stream_idle_test_hooks::StreamIdleTimeoutGuard;

fn set_stream_idle_timeouts_for_test(pre_ms: u64, post_ms: u64) -> StreamIdleEnvGuard {
    astra_runtime::turn::stream_idle_test_hooks::set_stream_idle_timeouts_for_test(pre_ms, post_ms)
}

#[test]
fn stream_idle_timeout_guard_sets_runtime_override_for_integration_tests() {
    let _guard = set_stream_idle_timeouts_for_test(123, 456);
    assert_eq!(
        astra_runtime::turn::stream_idle_test_hooks::current_stream_idle_timeouts_for_test(),
        (
            Some(std::time::Duration::from_millis(123)),
            Some(std::time::Duration::from_millis(456))
        )
    );
}

#[test]
fn provider_request_shape_identifies_only_the_matching_stream_fallback() {
    let streaming = "POST /v1/chat/completions HTTP/1.1\r\n\r\n{\"model\":\"m\",\"messages\":[{\"role\":\"user\",\"content\":\"task\"}],\"stream\":true,\"stream_options\":{\"include_usage\":true}}";
    let matching_fallback = "POST /v1/chat/completions HTTP/1.1\r\n\r\n{\"model\":\"m\",\"messages\":[{\"role\":\"user\",\"content\":\"task\"}],\"stream\":false}";
    let auxiliary_call = "POST /v1/chat/completions HTTP/1.1\r\n\r\n{\"model\":\"m\",\"messages\":[{\"role\":\"system\",\"content\":\"auxiliary\"}],\"stream\":false}";

    assert_eq!(
        provider_request_shape(streaming),
        provider_request_shape(matching_fallback)
    );
    assert_ne!(
        provider_request_shape(streaming),
        provider_request_shape(auxiliary_call)
    );
}

fn assert_no_primary_nonstream_fallback(hits: &RawTransportServerHits, message: &str) {
    let actual = hits.primary_nonstream_fallback_hits.load(Ordering::SeqCst);
    assert_eq!(
        actual,
        0,
        "{message}: expected 0 primary non-stream fallback hits, got {actual}; total_nonstream={}; fallback_requests={}",
        hits.nonstream_hits.load(Ordering::SeqCst),
        hits.primary_nonstream_fallback_request_summary()
    );
}

async fn spawn_raw_partial_transport_server(
    partial_text: &str,
) -> (String, RawTransportServerHits) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind raw mock llm listener");
    let addr = listener.local_addr().expect("raw local_addr");
    let hits = RawTransportServerHits::new();
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
                    hits.record_stream(&req);
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
                    hits.record_nonstream(&req);
                    let body = r#"{"choices":[{"message":{"content":"probe ok"}}]}"#;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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
) -> (String, RawTransportServerHits) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind idle mock llm listener");
    let addr = listener.local_addr().expect("idle local_addr");
    let hits = RawTransportServerHits::new();
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
                    hits.record_stream(&req);
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
                    hits.record_nonstream(&req);
                    let body = r#"{"choices":[{"message":{"content":"probe ok"}}]}"#;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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
    let hits = RawTransportServerHits::new();
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
                    hits.record_stream(&req);
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
                    hits.record_nonstream(&req);
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
    let hits = RawTransportServerHits::new();
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
                    hits.record_nonstream(&req);
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

async fn spawn_raw_server_loop_block_parse_server(
    partial_text: &str,
) -> (String, RawTransportServerHits) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind server-loop block-parse recovery mock llm listener");
    let addr = listener
        .local_addr()
        .expect("server-loop block-parse recovery local_addr");
    let hits = RawTransportServerHits::new();
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
                    hits.record_stream(&req);
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
                    hits.record_nonstream(&req);
                    let body = r#"{"choices":[{"message":{"content":"probe ok"}}]}"#;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    socket
                        .write_all(response.as_bytes())
                        .await
                        .expect("write server-loop block-parse probe response");
                    let _ = socket.shutdown().await;
                }
            });
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    (format!("http://{addr}"), hits)
}

async fn wait_for_artifact_count(
    pool: &sqlx::MySqlPool,
    user_id: &str,
    session_id: &str,
    artifact_kind: &str,
    min_count: i64,
    timeout: std::time::Duration,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM session_artifacts WHERE user_id = ? AND session_id = ? AND artifact_kind = ?",
        )
        .bind(user_id)
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
                "timeout ({timeout:?}) waiting for >= {min_count} artifacts of kind={artifact_kind} for user_id={user_id} session_id={session_id} (got {n})"
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

async fn latest_llm_capture_artifact_id(
    pool: &sqlx::MySqlPool,
    user_id: &str,
    session_id: &str,
    expect: &str,
) -> String {
    let row = sqlx::query(
        "SELECT artifact_id \
         FROM session_artifacts WHERE user_id = ? AND session_id = ? AND artifact_kind = 'llm_capture' \
         ORDER BY created_at DESC, artifact_id DESC LIMIT 1",
    )
    .bind(user_id)
    .bind(session_id)
    .fetch_one(pool)
    .await
    .expect(expect);
    row.try_get("artifact_id").expect("artifact_id")
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

    let artifact_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM session_artifacts WHERE user_id = ? AND session_id = ?",
    )
    .bind(&ctx.user_id)
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
    let other_app = ctx.app.clone();
    let other_auth = format!("Bearer {access_b}");

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
         FROM session_artifacts WHERE user_id = ? AND session_id = ? AND artifact_id = ?",
    )
    .bind(&ctx.user_id)
    .bind(&ctx.session_id)
    .bind(&workspace_artifact_id)
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
    cleanup_session_data(&ctx.shared_pool, &ctx.user_id, &ctx.session_id).await;
    cleanup_session_data(&ctx.shared_pool, &ctx.user_id, &other_session_id).await;
    ctx.close().await;
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
        "SELECT COUNT(*) FROM session_artifacts WHERE user_id = ? AND session_id = ? AND artifact_kind = 'llm_capture'",
    )
    .bind(&ctx.user_id)
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
        "model_selection": seeded_model_selection(ctx),
        "context": {
            "test_llm_rounds": [{ "full_text": "Artifact publish verified." }]
        }
    });
    let (status, body) = stream_chat_full_server_owned(app, auth, payload).await;
    assert_eq!(status, StatusCode::OK, "chat/stream: {body}");
    assert!(
        body.contains("Artifact publish verified."),
        "SSE body should include the model text response: {body}"
    );

    wait_for_artifact_count(
        pool,
        &ctx.user_id,
        &session_id,
        "llm_capture",
        1,
        std::time::Duration::from_secs(15),
    )
    .await;

    let row = sqlx::query(
        "SELECT artifact_id, source, turn, round, content_json, CAST(metadata AS CHAR) AS metadata_json \
         FROM session_artifacts WHERE user_id = ? AND session_id = ? AND artifact_kind = 'llm_capture' \
         ORDER BY created_at DESC, artifact_id DESC LIMIT 1",
    )
    .bind(&ctx.user_id)
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
    let round = round.expect("published llm_capture artifact should persist round");
    assert!(
        round >= 0,
        "artifact round should be non-negative, got {round}"
    );
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
    cleanup_session_data(&ctx.shared_pool, &ctx.user_id, &session_id).await;
    ctx.close().await;
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
        "model_selection": seeded_model_selection(ctx),
        "context": {
            "test_llm_rounds": [{ "full_text": "Artifact download verified." }]
        }
    });
    let (status, body) = stream_chat_full_server_owned(app, auth, payload).await;
    assert_eq!(status, StatusCode::OK, "chat/stream: {body}");
    assert!(
        body.contains("Artifact download verified."),
        "SSE body should include the model text response: {body}"
    );

    wait_for_artifact_count(
        pool,
        &ctx.user_id,
        &session_id,
        "llm_capture",
        1,
        std::time::Duration::from_secs(15),
    )
    .await;

    let artifact_id =
        latest_llm_capture_artifact_id(pool, &ctx.user_id, &session_id, "latest llm_capture row")
            .await;

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
    let _download_descriptor =
        assert_presigned_artifact_download(&session_id, &artifact_id, &download_body);
    let download_j = latest_j.clone();
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
    let other_app = ctx.app.clone();
    let other_auth = format!("Bearer {access_b}");

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
    cleanup_session_data(&ctx.shared_pool, &ctx.user_id, &session_id).await;
    cleanup_session_data(&ctx.shared_pool, &ctx.user_id, &other_session_id).await;
    ctx.close().await;
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
        "model_selection": seeded_model_selection(ctx),
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
    let (status, body) = stream_chat_full_server_owned(app, auth, payload).await;
    assert_eq!(status, StatusCode::OK, "chat/stream: {body}");
    assert!(
        body.contains(failure_message),
        "SSE body should surface the scripted failure: {body}"
    );
    assert!(
        body.contains("\"status\":\"paused\""),
        "a transport failure must leave the user a resumable paused run: {body}"
    );
    assert!(
        body.contains("\"resumable\":true"),
        "SSE body should tell the client the interrupted run can resume: {body}"
    );

    wait_for_artifact_count(
        pool,
        &ctx.user_id,
        &session_id,
        "llm_capture",
        1,
        std::time::Duration::from_secs(15),
    )
    .await;

    let artifact_id = latest_llm_capture_artifact_id(
        pool,
        &ctx.user_id,
        &session_id,
        "latest failed llm_capture row",
    )
    .await;

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
    // Artifacts carry the canonical token-usage schema
    // (`input_tokens` / `output_tokens`) regardless of which wire dialect the
    // upstream provider spoke. `llm_capture_error_response` is responsible
    // for normalizing `ClassifiedError.details.usage` from OpenAI-style
    // (`prompt_tokens`/`completion_tokens`) into the canonical form before
    // flattening. If this assertion regresses, error artifacts have drifted
    // away from the canonical schema shared by the server-owned SSE path.
    assert_eq!(
        latest_j["content"]["response"]["usage"]["input_tokens"].as_i64(),
        Some(17)
    );
    assert_eq!(
        latest_j["content"]["response"]["usage"]["output_tokens"].as_i64(),
        Some(3)
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
    let _download_descriptor =
        assert_presigned_artifact_download(&session_id, &artifact_id, &download_body);
    let download_j = latest_j.clone();
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
    // Same canonical-schema rationale as the `latest_j` assertion above.
    assert_eq!(
        download_j["content"]["response"]["usage"]["input_tokens"].as_i64(),
        Some(17)
    );
    assert_eq!(
        download_j["content"]["response"]["usage"]["output_tokens"].as_i64(),
        Some(3)
    );
    cleanup_session_data(&ctx.shared_pool, &ctx.user_id, &session_id).await;
    ctx.close().await;
}

pub async fn run_server_loop_block_parse_preserves_partial_without_replay_routes() {
    let partial_text = "server loop partial before malformed block";
    let (base_url, hits) = spawn_raw_server_loop_block_parse_server(partial_text).await;

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
            "context_window": 200000,
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
            "title": "server loop malformed stream without replay",
            "metadata": { "full_llm_capture": true, "suite": "server_loop_block_parse_no_replay" }
        }),
    )
    .await;
    assert_eq!(st_sess, StatusCode::CREATED, "create session: {sess}");
    let session_id = sess["session_id"].as_str().expect("session_id").to_string();

    let payload = json!({
        "message": "trigger a server-loop malformed provider block after progress",
        "session_id": &session_id,
        "model_selection": model_selection(offering_id_from_model_response(&model_j))
    });
    let (status, body) = stream_chat_full_server_owned(app, auth, payload).await;
    assert_eq!(status, StatusCode::OK, "chat/stream: {body}");
    assert!(
        body.contains(partial_text),
        "server-loop SSE should preserve text delivered before the malformed block: {body}"
    );
    assert!(
        body.contains("\"error_kind\":\"stream_transport\""),
        "server-loop SSE should expose the original typed stream failure: {body}"
    );
    assert!(
        body.contains("\"status\":\"paused\"") && body.contains("\"resumable\":true"),
        "a partial stream failure must preserve a resumable run: {body}"
    );
    assert!(
        body.contains("\"type\":\"run_interrupted\"")
            && body.contains("\"type\":\"turn_complete\""),
        "the server must publish the interruption and one terminal summary: {body}"
    );

    wait_for_artifact_count(
        pool,
        &ctx.user_id,
        &session_id,
        "llm_capture",
        1,
        std::time::Duration::from_secs(15),
    )
    .await;

    let artifact_id = latest_llm_capture_artifact_id(
        pool,
        &ctx.user_id,
        &session_id,
        "latest server-loop block-parse llm_capture row",
    )
    .await;

    let latest_path = format!("/sessions/{session_id}/artifacts/latest/llm_capture");
    let (st_latest, latest_j) = get_json(app, &latest_path, Some(auth), &[]).await;
    assert_eq!(st_latest, StatusCode::OK, "artifact latest: {latest_j}");
    assert_eq!(latest_j["artifact_kind"].as_str(), Some("llm_capture"));
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
    let _download_descriptor =
        assert_presigned_artifact_download(&session_id, &artifact_id, &download_body);
    let download_j = latest_j.clone();
    assert_eq!(download_j["metadata"]["outcome"].as_str(), Some("error"));
    assert_eq!(
        download_j["content"]["response"]["kind"].as_str(),
        Some("stream_transport")
    );
    assert_eq!(
        download_j["content"]["response"]["partial_full_text"].as_str(),
        Some(partial_text)
    );

    assert_eq!(
        hits.stream_hits.load(Ordering::SeqCst),
        1,
        "a malformed response after visible output must not replay the stream"
    );
    assert_no_primary_nonstream_fallback(
        &hits,
        "a malformed response after visible output must not replay as non-stream",
    );
    let _ = sqlx::query("DELETE FROM infra_llm_models WHERE model_name = ?")
        .bind(&model_name)
        .execute(pool)
        .await;
    cleanup_session_data(&ctx.shared_pool, &ctx.user_id, &session_id).await;
    ctx.close().await;
}

pub async fn run_server_loop_transport_preserves_partial_without_replay_routes() {
    let partial_text = "server loop transport partial";
    let (base_url, hits) = spawn_raw_partial_transport_server(partial_text).await;

    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let model_name = format!("server-loop-transport-no-replay-{}", ctx.suffix);

    let (st_model, model_j) = post_json(
        app,
        "/models",
        Some(auth.as_str()),
        json!({
            "name": model_name,
            "provider": "openai",
            "context_window": 200000,
            "api_key": "server-loop-transport-no-replay-e2e-key",
            "base_url": base_url
        }),
    )
    .await;
    assert_eq!(st_model, StatusCode::CREATED, "create model: {model_j}");
    sqlx::query("UPDATE infra_llm_models SET is_active = 1 WHERE model_name = ?")
        .bind(&model_name)
        .execute(pool)
        .await
        .expect("force-activate server-loop transport no-replay test model");

    let (st_sess, sess) = post_json(
        app,
        "/sessions",
        Some(auth.as_str()),
        json!({
            "title": "server loop transport failure without replay",
            "metadata": { "full_llm_capture": true, "suite": "server_loop_transport_no_replay" }
        }),
    )
    .await;
    assert_eq!(st_sess, StatusCode::CREATED, "create session: {sess}");
    let session_id = sess["session_id"].as_str().expect("session_id").to_string();

    let payload = json!({
        "message": "trigger a server-loop transport break after progress",
        "session_id": &session_id,
        "model_selection": model_selection(offering_id_from_model_response(&model_j))
    });
    let (status, body) = stream_chat_full_server_owned(app, auth, payload).await;
    assert_eq!(status, StatusCode::OK, "chat/stream: {body}");
    assert!(
        body.contains(partial_text),
        "server-loop SSE should retain text delivered before the transport break: {body}"
    );
    assert!(
        body.contains("\"error_kind\":\"stream_transport\""),
        "server-loop SSE should retain the typed transport failure: {body}"
    );
    assert!(
        body.contains("\"status\":\"paused\"") && body.contains("\"resumable\":true"),
        "server-loop transport failure should leave a resumable paused run: {body}"
    );
    assert!(
        body.contains("\"type\":\"run_interrupted\"")
            && body.contains("\"type\":\"turn_complete\""),
        "the server must publish the interruption and one terminal summary: {body}"
    );

    wait_for_artifact_count(
        pool,
        &ctx.user_id,
        &session_id,
        "llm_capture",
        1,
        std::time::Duration::from_secs(15),
    )
    .await;

    let artifact_id = latest_llm_capture_artifact_id(
        pool,
        &ctx.user_id,
        &session_id,
        "latest server-loop transport no-replay llm_capture row",
    )
    .await;

    let latest_path = format!("/sessions/{session_id}/artifacts/latest/llm_capture");
    let (st_latest, latest_j) = get_json(app, &latest_path, Some(auth), &[]).await;
    assert_eq!(st_latest, StatusCode::OK, "artifact latest: {latest_j}");
    assert_eq!(latest_j["artifact_kind"].as_str(), Some("llm_capture"));
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
    let _download_descriptor =
        assert_presigned_artifact_download(&session_id, &artifact_id, &download_body);
    let download_j = latest_j.clone();
    assert_eq!(download_j["metadata"]["outcome"].as_str(), Some("error"));
    assert_eq!(
        download_j["content"]["response"]["kind"].as_str(),
        Some("stream_transport")
    );
    assert_eq!(
        download_j["content"]["response"]["partial_full_text"].as_str(),
        Some(partial_text)
    );

    assert_eq!(
        hits.stream_hits.load(Ordering::SeqCst),
        1,
        "a transport break after visible output must not replay the stream"
    );
    assert_no_primary_nonstream_fallback(
        &hits,
        "a transport break after visible output must not replay as non-stream",
    );
    let _ = sqlx::query("DELETE FROM infra_llm_models WHERE model_name = ?")
        .bind(&model_name)
        .execute(pool)
        .await;
    cleanup_session_data(&ctx.shared_pool, &ctx.user_id, &session_id).await;
    ctx.close().await;
}

pub async fn run_server_loop_idle_preserves_partial_without_replay_routes() {
    let _idle_env = set_stream_idle_timeouts_for_test(250, 250);
    let partial_text = "server loop idle partial";
    let (base_url, hits) =
        spawn_raw_idle_after_progress_server(partial_text, std::time::Duration::from_secs(2)).await;

    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let model_name = format!("server-loop-idle-no-replay-{}", ctx.suffix);

    let (st_model, model_j) = post_json(
        app,
        "/models",
        Some(auth.as_str()),
        json!({
            "name": model_name,
            "provider": "openai",
            "context_window": 200000,
            "api_key": "server-loop-idle-no-replay-e2e-key",
            "base_url": base_url
        }),
    )
    .await;
    assert_eq!(st_model, StatusCode::CREATED, "create model: {model_j}");
    sqlx::query("UPDATE infra_llm_models SET is_active = 1 WHERE model_name = ?")
        .bind(&model_name)
        .execute(pool)
        .await
        .expect("force-activate server-loop idle no-replay test model");

    let (st_sess, sess) = post_json(
        app,
        "/sessions",
        Some(auth.as_str()),
        json!({
            "title": "server loop idle failure without replay",
            "metadata": { "full_llm_capture": true, "suite": "server_loop_idle_no_replay" }
        }),
    )
    .await;
    assert_eq!(st_sess, StatusCode::CREATED, "create session: {sess}");
    let session_id = sess["session_id"].as_str().expect("session_id").to_string();

    let payload = json!({
        "message": "trigger a server-loop idle timeout after progress",
        "session_id": &session_id,
        "model_selection": model_selection(offering_id_from_model_response(&model_j))
    });
    let (status, body) = stream_chat_full_server_owned(app, auth, payload).await;
    assert_eq!(status, StatusCode::OK, "chat/stream: {body}");
    assert!(
        body.contains(partial_text),
        "server-loop SSE should retain text delivered before the idle timeout: {body}"
    );
    assert!(
        body.contains("\"error_kind\":\"stream_idle\""),
        "server-loop SSE should retain the typed idle failure: {body}"
    );
    assert!(
        body.contains("\"status\":\"paused\"") && body.contains("\"resumable\":true"),
        "server-loop idle timeout should leave a resumable paused run: {body}"
    );
    assert!(
        body.contains("\"type\":\"run_interrupted\"")
            && body.contains("\"type\":\"turn_complete\""),
        "the server must publish the interruption and one terminal summary: {body}"
    );

    wait_for_artifact_count(
        pool,
        &ctx.user_id,
        &session_id,
        "llm_capture",
        1,
        std::time::Duration::from_secs(15),
    )
    .await;

    let artifact_id = latest_llm_capture_artifact_id(
        pool,
        &ctx.user_id,
        &session_id,
        "latest server-loop idle no-replay llm_capture row",
    )
    .await;

    let latest_path = format!("/sessions/{session_id}/artifacts/latest/llm_capture");
    let (st_latest, latest_j) = get_json(app, &latest_path, Some(auth), &[]).await;
    assert_eq!(st_latest, StatusCode::OK, "artifact latest: {latest_j}");
    assert_eq!(latest_j["artifact_kind"].as_str(), Some("llm_capture"));
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
    let _download_descriptor =
        assert_presigned_artifact_download(&session_id, &artifact_id, &download_body);
    let download_j = latest_j.clone();
    assert_eq!(download_j["metadata"]["outcome"].as_str(), Some("error"));
    assert_eq!(
        download_j["content"]["response"]["kind"].as_str(),
        Some("stream_idle")
    );
    assert_eq!(
        download_j["content"]["response"]["partial_full_text"].as_str(),
        Some(partial_text)
    );

    assert_eq!(
        hits.stream_hits.load(Ordering::SeqCst),
        1,
        "an idle timeout after visible output must not replay the stream"
    );
    assert_no_primary_nonstream_fallback(
        &hits,
        "an idle timeout after visible output must not replay as non-stream",
    );
    let _ = sqlx::query("DELETE FROM infra_llm_models WHERE model_name = ?")
        .bind(&model_name)
        .execute(pool)
        .await;
    cleanup_session_data(&ctx.shared_pool, &ctx.user_id, &session_id).await;
    ctx.close().await;
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
            "context_window": 200000,
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
        "model_selection": model_selection(offering_id_from_model_response(&model_j))
    });
    let (status, body) = stream_chat_full_server_owned(app, auth, payload.clone()).await;
    assert_eq!(status, StatusCode::OK, "chat/stream: {body}");
    assert!(
        body.contains("\"kind\":\"rate_limited\"")
            && body.contains("Please wait ~30s before retrying."),
        "server-loop SSE should surface an actionable typed rate-limit interruption: {body}"
    );
    assert!(
        body.contains("\"status\":\"paused\"") && body.contains("\"resumable\":true"),
        "rate-limit exhaustion should leave a resumable paused run: {body}"
    );
    assert!(
        body.contains("\"type\":\"run_interrupted\"")
            && body.contains("\"type\":\"turn_complete\""),
        "the server must publish the interruption and one terminal summary: {body}"
    );

    wait_for_artifact_count(
        pool,
        &ctx.user_id,
        &session_id,
        "llm_capture",
        1,
        std::time::Duration::from_secs(15),
    )
    .await;

    let artifact_id = latest_llm_capture_artifact_id(
        pool,
        &ctx.user_id,
        &session_id,
        "latest server-loop rate-limit llm_capture row",
    )
    .await;

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
    let _download_descriptor =
        assert_presigned_artifact_download(&session_id, &artifact_id, &download_body);
    let download_j = latest_j.clone();
    assert_eq!(download_j["metadata"]["outcome"].as_str(), Some("error"));
    assert_eq!(
        download_j["content"]["response"]["kind"].as_str(),
        Some("rate_limit")
    );

    let (cooldown_status, cooldown_body) = stream_chat_full_server_owned(app, auth, payload).await;
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
        cooldown_body.contains("\"kind\":\"rate_limited\"")
            && cooldown_body.contains("\"resume_action\":{\"wait_and_retry\":"),
        "cooldown reject should give an actionable typed rate-limit interruption: {cooldown_body}"
    );
    assert!(
        cooldown_body.contains("\"status\":\"paused\"")
            && cooldown_body.contains("\"type\":\"turn_complete\""),
        "cooldown reject should settle a resumable paused turn: {cooldown_body}"
    );

    assert_eq!(
        hits.stream_hits.load(Ordering::SeqCst),
        3,
        "the third consecutive rate limit must enter cooldown without a fourth provider call"
    );
    assert_no_primary_nonstream_fallback(
        &hits,
        "repeated stream 429s plus cooldown reject should not issue a non-stream fallback",
    );
    let _ = sqlx::query("DELETE FROM infra_llm_models WHERE model_name = ?")
        .bind(&model_name)
        .execute(pool)
        .await;
    cleanup_session_data(&ctx.shared_pool, &ctx.user_id, &session_id).await;
    ctx.close().await;
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
            "context_window": 200000,
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
        "model_selection": model_selection(offering_id_from_model_response(&model_j))
    });
    let (status, body) = stream_chat_full_server_owned(app, auth, payload).await;
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
        &ctx.user_id,
        &session_id,
        "llm_capture",
        1,
        std::time::Duration::from_secs(15),
    )
    .await;

    let artifact_id = latest_llm_capture_artifact_id(
        pool,
        &ctx.user_id,
        &session_id,
        "latest server-loop rate-limit retry llm_capture row",
    )
    .await;

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
    let _download_descriptor =
        assert_presigned_artifact_download(&session_id, &artifact_id, &download_body);
    let download_j = latest_j.clone();
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
    assert_no_primary_nonstream_fallback(
        &hits,
        "successful stream retry should not require a non-stream fallback",
    );
    let _ = sqlx::query("DELETE FROM infra_llm_models WHERE model_name = ?")
        .bind(&model_name)
        .execute(pool)
        .await;
    cleanup_session_data(&ctx.shared_pool, &ctx.user_id, &session_id).await;
    ctx.close().await;
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
    cleanup_session_data(&ctx.shared_pool, &user_id, &session_id).await;
    ctx.close().await;
}

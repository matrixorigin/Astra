use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::StatusCode;
use axum::routing::post;
use serde_json::json;

use super::*;

struct Provider {
    endpoint: String,
    task: tokio::task::JoinHandle<()>,
}

impl Provider {
    async fn start(app: Router) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .unwrap()
        });
        Self { endpoint, task }
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn transport() -> ProviderTransport {
    ProviderTransport::build(reqwest::Client::builder().no_proxy()).unwrap()
}

fn request(transport: &ProviderTransport, endpoint: &str) -> PreparedHttpAttempt {
    let body = ExactProviderRequest::compile(
        &json!({"model":"fixture","stream":true}),
        ProviderProtocol::OpenAiCompatible,
        1024,
    )
    .unwrap();
    transport
        .prepare(
            endpoint,
            provider_headers(ProviderProtocol::OpenAiCompatible, "canary-secret", []).unwrap(),
            &body,
            None,
        )
        .unwrap()
}

#[tokio::test]
async fn rejected_or_redirected_requests_never_redispatch_or_forward_auth() {
    for status in [
        StatusCode::TEMPORARY_REDIRECT,
        StatusCode::PERMANENT_REDIRECT,
        StatusCode::SERVICE_UNAVAILABLE,
    ] {
        let hits = Arc::new(AtomicUsize::new(0));
        let forwarded = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route(
                "/",
                post({
                    let hits = hits.clone();
                    move |headers: HeaderMap, body: bytes::Bytes| {
                        hits.fetch_add(1, Ordering::SeqCst);
                        assert_eq!(headers["authorization"], "Bearer canary-secret");
                        assert_eq!(
                            body,
                            serde_json::to_vec(&json!({"model":"fixture","stream":true})).unwrap()
                        );
                        async move { (status, [("location", "/forwarded")]) }
                    }
                }),
            )
            .route(
                "/forwarded",
                post({
                    let forwarded = forwarded.clone();
                    move || {
                        forwarded.fetch_add(1, Ordering::SeqCst);
                        async { "unexpected" }
                    }
                }),
            );
        let provider = Provider::start(app).await;
        let transport = transport();
        let (events, mut receiver) = mpsc::channel(1);
        let terminal = transport
            .execute(
                request(&transport, &provider.endpoint),
                ResponseMode::Sse,
                ExecutionLimits::default(),
                Instant::now() + Duration::from_secs(120),
                &CancellationToken::new(),
                &events,
            )
            .await;
        assert_eq!(
            terminal.status,
            ExecutionStatus::HttpStatus(status.as_u16())
        );
        assert_eq!(terminal.delivery, DeliveryEvidence::ResponseHeaders);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert_eq!(forwarded.load(Ordering::SeqCst), 0);
        assert!(receiver.try_recv().is_err());
    }
}

#[tokio::test]
async fn connect_failure_is_positive_not_dispatched_evidence() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let transport = transport();
    let (events, mut receiver) = mpsc::channel(1);

    let terminal = transport
        .execute(
            request(&transport, &endpoint),
            ResponseMode::Sse,
            ExecutionLimits::default(),
            Instant::now() + Duration::from_secs(5),
            &CancellationToken::new(),
            &events,
        )
        .await;

    assert_eq!(terminal.status, ExecutionStatus::Transport);
    assert_eq!(terminal.delivery, DeliveryEvidence::NotDispatched);
    assert_eq!(terminal.provider_bytes, 0);
    assert!(receiver.try_recv().is_err());
}

#[tokio::test]
async fn cancel_and_deadline_preserve_partial_evidence_after_accepted_provider_bytes() {
    for cancel in [true, false] {
        let hits = Arc::new(AtomicUsize::new(0));
        let app = Router::new().route("/", post({
            let hits = hits.clone();
            move || {
                hits.fetch_add(1, Ordering::SeqCst);
                async { Body::from_stream(async_stream::stream! {
                    yield Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"data: {\"text\":\"partial\"}\n\n"));
                    std::future::pending::<()>().await;
                }) }
            }
        }));
        let provider = Provider::start(app).await;
        let transport = transport();
        let attempt = request(&transport, &provider.endpoint);
        let token = CancellationToken::new();
        let execution_token = token.clone();
        let (events, mut receiver) = mpsc::channel(1);
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut task = tokio::spawn(async move {
            transport
                .execute(
                    attempt,
                    ResponseMode::Sse,
                    ExecutionLimits::default(),
                    deadline,
                    &execution_token,
                    &events,
                )
                .await
        });
        let event = tokio::time::timeout(Duration::from_secs(30), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(event, ProviderEvent::Json(value) if value["text"] == "partial"));
        if cancel {
            token.cancel();
        } else {
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(121)).await;
        }
        let terminal = tokio::time::timeout(Duration::from_secs(1), &mut task).await;
        if !cancel {
            tokio::time::resume();
        }
        let terminal = terminal.unwrap().unwrap();
        assert_eq!(
            terminal.status,
            if cancel {
                ExecutionStatus::Cancelled
            } else {
                ExecutionStatus::Deadline
            }
        );
        assert_eq!(terminal.delivery, DeliveryEvidence::ResponseHeaders);
        assert!(terminal.provider_bytes > 0);
        assert_eq!(terminal.events_delivered, 1);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn consumer_backpressure_is_cancelable_and_preserves_delivered_count() {
    let provider = Provider::start(Router::new().route(
        "/",
        post(|| async { "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n" }),
    ))
    .await;
    let transport = transport();
    let attempt = request(&transport, &provider.endpoint);
    let (events, receiver) = mpsc::channel(1);
    let token = CancellationToken::new();
    let execution_token = token.clone();
    let task = tokio::spawn(async move {
        transport
            .execute(
                attempt,
                ResponseMode::Sse,
                ExecutionLimits::default(),
                Instant::now() + Duration::from_secs(120),
                &execution_token,
                &events,
            )
            .await
    });
    // Observe queue occupancy without draining it, so the second event must
    // remain blocked on the capacity-one queue when cancellation occurs.
    tokio::time::timeout(Duration::from_secs(30), async {
        while receiver.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    token.cancel();
    let terminal = task.await.unwrap();
    assert_eq!(terminal.status, ExecutionStatus::Cancelled);
    assert_eq!(terminal.events_delivered, 1);
    assert!(terminal.provider_bytes > 0);
}

#[tokio::test]
async fn preflight_cancel_and_expiry_open_no_request() {
    let transport = transport();
    for cancelled in [true, false] {
        let token = CancellationToken::new();
        if cancelled {
            token.cancel();
        }
        let (events, mut receiver) = mpsc::channel(1);
        let terminal = transport
            .execute(
                request(&transport, "http://127.0.0.1:1"),
                ResponseMode::Sse,
                ExecutionLimits::default(),
                Instant::now(),
                &token,
                &events,
            )
            .await;
        assert_eq!(
            terminal.status,
            if cancelled {
                ExecutionStatus::Cancelled
            } else {
                ExecutionStatus::Deadline
            }
        );
        assert_eq!(terminal.delivery, DeliveryEvidence::NotDispatched);
        assert_eq!(terminal.provider_bytes, 0);
        assert!(receiver.try_recv().is_err());
    }
}

#[tokio::test]
async fn malformed_tail_and_total_limit_keep_prior_events() {
    for (body, limits, expected) in [
        (
            b"data: {}\n\ndata: \xff\n\n".to_vec(),
            ExecutionLimits::default(),
            ExecutionStatus::Protocol,
        ),
        (
            b"data: 12345678901234567890\n\n".to_vec(),
            ExecutionLimits {
                event_bytes: 8,
                ..ExecutionLimits::default()
            },
            ExecutionStatus::Limit,
        ),
        (
            b"data: {}\n\n".repeat(20),
            ExecutionLimits {
                total_bytes: 8,
                ..ExecutionLimits::default()
            },
            ExecutionStatus::Limit,
        ),
    ] {
        let provider = Provider::start(Router::new().route(
            "/",
            post(move || {
                let body = body.clone();
                async move { body }
            }),
        ))
        .await;
        let transport = transport();
        let (events, mut receiver) = mpsc::channel(4);
        let terminal = transport
            .execute(
                request(&transport, &provider.endpoint),
                ResponseMode::Sse,
                limits,
                Instant::now() + Duration::from_secs(120),
                &CancellationToken::new(),
                &events,
            )
            .await;
        assert_eq!(terminal.status, expected);
        if expected == ExecutionStatus::Protocol {
            assert!(matches!(receiver.try_recv(), Ok(ProviderEvent::Json(_))));
        }
    }
}

#[test]
fn private_material_never_enters_debug_or_errors() {
    let transport = transport();
    let body = ExactProviderRequest::compile(
        &json!({"prompt":"private-canary"}),
        ProviderProtocol::OpenAiCompatible,
        1024,
    )
    .unwrap();
    let error = transport
        .prepare(
            "https://private-canary:private-canary@provider.example/path",
            HeaderMap::new(),
            &body,
            None,
        )
        .unwrap_err();
    assert_eq!(error, PreparationError::InvalidRequest);
    assert!(!error.to_string().contains("private-canary"));
    let prepared = transport
        .prepare(
            "https://private-canary.example/path",
            provider_headers(ProviderProtocol::OpenAiCompatible, "private-canary", []).unwrap(),
            &body,
            None,
        )
        .unwrap();
    for diagnostic in [
        format!("{transport:?}"),
        format!("{prepared:?}"),
        format!(
            "{:?}",
            ProviderEvent::Json(json!({"text":"private-canary"}))
        ),
        provider_headers(ProviderProtocol::OpenAiCompatible, "private-canary\n", [])
            .unwrap_err()
            .to_string(),
    ] {
        assert!(!diagnostic.contains("private-canary"));
    }
}

#[tokio::test]
async fn successful_requests_reuse_the_same_connection_and_preserve_exact_bytes() {
    let (accepted, mut peers) = mpsc::unbounded_channel();
    let provider = Provider::start(Router::new().route(
        "/",
        post(
            move |axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<
                std::net::SocketAddr,
            >,
                  body: bytes::Bytes| {
                accepted.send(peer).unwrap();
                async move { body }
            },
        ),
    ))
    .await;
    let transport = transport();
    let (events, mut receiver) = mpsc::channel(4);
    for _ in 0..2 {
        let terminal = transport
            .execute(
                request(&transport, &provider.endpoint),
                ResponseMode::Json,
                ExecutionLimits::default(),
                Instant::now() + Duration::from_secs(120),
                &CancellationToken::new(),
                &events,
            )
            .await;
        assert_eq!(terminal.status, ExecutionStatus::Complete);
        assert_eq!(terminal.events_delivered, 2);
        assert!(
            matches!(receiver.try_recv(), Ok(ProviderEvent::Json(value)) if value == json!({"model":"fixture","stream":true}))
        );
        assert!(matches!(receiver.try_recv(), Ok(ProviderEvent::Eof)));
    }
    assert_eq!(
        peers.recv().await.unwrap(),
        peers.recv().await.unwrap(),
        "both physical requests must use the same accepted TCP connection"
    );
}

#[tokio::test]
async fn body_transport_failure_after_acknowledged_partial_event_never_redispatches() {
    let (release, released) = tokio::sync::oneshot::channel();
    let released = Arc::new(std::sync::Mutex::new(Some(released)));
    let hits = Arc::new(AtomicUsize::new(0));
    let provider = Provider::start(Router::new().route("/", post({
        let hits = hits.clone();
        move || {
            hits.fetch_add(1, Ordering::SeqCst);
            let released = released.lock().unwrap().take().expect("one physical request");
            async move { Body::from_stream(async_stream::stream! {
                yield Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"data: {\"text\":\"partial\"}\n\n"));
                released.await.unwrap();
                yield Err(std::io::Error::other("private-canary-transport-detail"));
            }) }
        }
    }))).await;
    let transport = transport();
    let attempt = request(&transport, &provider.endpoint);
    let (events, mut receiver) = mpsc::channel(2);
    let task = tokio::spawn(async move {
        transport
            .execute(
                attempt,
                ResponseMode::Sse,
                ExecutionLimits::default(),
                Instant::now() + Duration::from_secs(120),
                &CancellationToken::new(),
                &events,
            )
            .await
    });
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(30), receiver.recv())
            .await
            .unwrap(),
        Some(ProviderEvent::Json(_))
    ));
    release.send(()).unwrap();
    let terminal = task.await.unwrap();
    assert_eq!(terminal.status, ExecutionStatus::Transport);
    assert_eq!(terminal.events_delivered, 1);
    assert!(terminal.provider_bytes > 0);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert!(!format!("{terminal:?}").contains("private-canary"));
}

#[test]
fn native_headers_preserve_protocol_and_explicit_override_without_leaking_debug() {
    let headers = provider_headers(
        ProviderProtocol::AnthropicMessages,
        "default-canary",
        [
            ("X-Api-Key", "override-canary"),
            ("anthropic-version", "2024-01-01"),
        ],
    )
    .unwrap();
    assert_eq!(headers["x-api-key"], "override-canary");
    assert_eq!(headers["anthropic-version"], "2024-01-01");
    assert!(!headers.contains_key("authorization"));
    assert!(!format!("{headers:?}").contains("canary"));
    assert!(
        provider_headers(
            ProviderProtocol::OpenAiCompatible,
            "",
            [("authorization", "secret\ncanary")]
        )
        .is_err()
    );
}

#[tokio::test]
async fn bounded_json_reader_reports_exact_observed_bytes_and_never_reads_after_failure() {
    let prefix = bytes::Bytes::from_static(b"{\"text\":\"partial");
    let expected_bytes = prefix.len() as u64;
    let stream = futures_util::stream::iter(vec![Ok(prefix), Err("private-canary")]).chain(
        futures_util::stream::poll_fn(|_| {
            panic!("must not read after the terminal transport failure")
        }),
    );
    let error = read_json_stream(stream, 1024, |_| ResponseReadErrorKind::Transport)
        .await
        .unwrap_err();
    assert_eq!(error.kind, ResponseReadErrorKind::Transport);
    assert_eq!(error.provider_bytes, expected_bytes);
    assert!(!format!("{error:?}").contains("private-canary"));

    for (body, limit, expected) in [
        (
            b"{\"text\":\"oversized\"}".as_slice(),
            8,
            ResponseReadErrorKind::Limit,
        ),
        (
            b"{\"text\":\"partial".as_slice(),
            1024,
            ResponseReadErrorKind::MalformedJson,
        ),
        (
            b"{\"text\":\"\xff\"}".as_slice(),
            1024,
            ResponseReadErrorKind::MalformedJson,
        ),
    ] {
        let stream =
            futures_util::stream::iter(vec![Ok::<_, ()>(bytes::Bytes::copy_from_slice(body))]);
        let error = read_json_stream(stream, limit, |_| ResponseReadErrorKind::Transport)
            .await
            .unwrap_err();
        assert_eq!(error.kind, expected);
        assert_eq!(error.provider_bytes, body.len() as u64);
    }
    let body = "{\"text\":\"你好\"}";
    let stream = futures_util::stream::iter(
        body.as_bytes()
            .iter()
            .map(|byte| Ok::<_, ()>(bytes::Bytes::copy_from_slice(&[*byte]))),
    );
    let decoded = read_json_stream(stream, body.len(), |_| ResponseReadErrorKind::Transport)
        .await
        .unwrap();
    assert_eq!(decoded.value, json!({"text":"你好"}));
    assert_eq!(decoded.provider_bytes, body.len() as u64);
}

#[tokio::test]
async fn public_nonstream_reader_bounds_http_json_and_preserves_malformed_evidence() {
    for (body, limit, expected) in [
        ("{\"text\":\"oversized\"}", 8, ResponseReadErrorKind::Limit),
        ("{\"text\":", 1024, ResponseReadErrorKind::MalformedJson),
    ] {
        let provider =
            Provider::start(Router::new().route("/", post(move || async move { body }))).await;
        let transport = transport();
        let response = transport
            .send_once(request(&transport, &provider.endpoint))
            .await
            .unwrap();
        let error = read_json_response(response, limit).await.unwrap_err();
        assert_eq!(error.kind, expected);
        assert_eq!(error.provider_bytes, body.len() as u64);
    }
}

#[tokio::test]
async fn request_timeout_and_client_backstop_each_bound_an_accepted_stalled_request() {
    for request_timeout in [Some(Duration::from_secs(120)), None] {
        let (accepted, mut acceptance) = mpsc::unbounded_channel();
        let provider = Provider::start(Router::new().route(
            "/",
            post(move || {
                accepted.send(()).unwrap();
                async {
                    std::future::pending::<()>().await;
                    StatusCode::OK
                }
            }),
        ))
        .await;
        let transport = ProviderTransport::build(reqwest::Client::builder().no_proxy().timeout(
            if request_timeout.is_some() {
                Duration::from_secs(600)
            } else {
                Duration::from_secs(120)
            },
        ))
        .unwrap();
        let mut attempt = request(&transport, &provider.endpoint);
        if let Some(timeout) = request_timeout {
            attempt = attempt.constrain_timeout(timeout);
        }
        let mut sending = tokio::spawn(async move { transport.send_once(attempt).await });
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(30), acceptance.recv())
                .await
                .unwrap(),
            Some(())
        );
        assert!(!sending.is_finished());
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(121)).await;
        let result = tokio::time::timeout(Duration::from_secs(1), &mut sending).await;
        tokio::time::resume();
        assert!(result.unwrap().unwrap().unwrap_err().is_timeout());
    }
}

#[path = "support/memoria_observer_auth.rs"]
mod memoria_observer_auth;

use astra_memoria::{MemoriaMemory, MemoriaOperationError, MemoriaPort, MemoriaToolTransport};
use astra_runtime::{
    DatabaseTurnObserverWorker, TurnObserverOutcome, TurnObserverRequest, TurnObserverWorker,
};
use async_trait::async_trait;
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU8, Ordering},
};

#[derive(Clone)]
struct AuthorityPort {
    base_url: String,
    bound_owner: Option<String>,
    allowed: Result<bool, String>,
    transport_error: Option<String>,
    transport_missing: bool,
    master: bool,
    calls: Arc<Mutex<Vec<String>>>,
    changing_grant: Option<Arc<AtomicU8>>,
}

#[async_trait]
impl MemoriaPort for AuthorityPort {
    fn bind_owner(&self, user_id: &str) -> Result<Arc<dyn MemoriaPort>, String> {
        if self
            .bound_owner
            .as_deref()
            .is_some_and(|owner| owner != user_id)
        {
            return Err("owner mismatch".into());
        }
        self.calls.lock().unwrap().push(format!("bind:{user_id}"));
        let mut bound = self.clone();
        bound.bound_owner = Some(user_id.to_owned());
        Ok(Arc::new(bound))
    }
    async fn admits_operation(&self, write: bool) -> Result<bool, String> {
        assert!(write);
        assert!(self.bound_owner.is_some());
        self.calls.lock().unwrap().push("admit-write".into());
        if let Some(grant) = &self.changing_grant {
            return Ok(grant.load(Ordering::SeqCst) != 1);
        }
        self.allowed.clone()
    }
    async fn resolve_tool_transport(
        &self,
        write: bool,
    ) -> Result<Option<MemoriaToolTransport>, String> {
        assert!(write);
        self.calls.lock().unwrap().push("resolve-write".into());
        if self
            .changing_grant
            .as_ref()
            .is_some_and(|grant| grant.load(Ordering::SeqCst) == 0)
        {
            std::future::pending::<()>().await;
        }
        if let Some(error) = &self.transport_error {
            return Err(error.clone());
        }
        if self.transport_missing {
            return Ok(None);
        }
        Ok(Some(MemoriaToolTransport {
            base_url: self.base_url.clone(),
            credential: if self.master {
                "synthetic-master-key"
            } else {
                "synthetic-scoped-key"
            }
            .into(),
            owner_user_id: format!("memory-{}", self.bound_owner.as_ref().unwrap()),
            owner_scoped_master: self.master,
        }))
    }
    async fn retrieve_ext(
        &self,
        _: &str,
        _: Option<&str>,
        _: usize,
        _: bool,
    ) -> Result<Vec<MemoriaMemory>, MemoriaOperationError> {
        panic!("observer must not retrieve")
    }
    async fn store(
        &self,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<String, String> {
        panic!("observer must not issue raw stores")
    }
    async fn purge_working(&self, _: &str) -> Result<u64, String> {
        panic!("observer must not purge")
    }
}

type Requests = Arc<Mutex<Vec<(HeaderMap, Value)>>>;

async fn backend(status: StatusCode) -> (String, Requests, tokio::task::JoinHandle<()>) {
    let requests: Requests = Arc::default();
    let app = Router::new()
        .route(
            "/v1/observe",
            post(
                move |State(requests): State<Requests>,
                      headers: HeaderMap,
                      Json(body): Json<Value>| async move {
                    let authorized = memoria_observer_auth::validate_observer_auth(
                        &headers,
                        "synthetic-master-key",
                        Some("synthetic-scoped-key"),
                    );
                    requests.lock().unwrap().push((headers, body));
                    authorized.err().unwrap_or(status)
                },
            ),
        )
        .with_state(requests.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (url, requests, task)
}

fn port(base_url: String) -> AuthorityPort {
    AuthorityPort {
        base_url,
        bound_owner: None,
        allowed: Ok(true),
        transport_error: None,
        transport_missing: false,
        master: false,
        calls: Arc::default(),
        changing_grant: None,
    }
}

fn request(user_id: &str) -> TurnObserverRequest {
    TurnObserverRequest {
        user_id: user_id.into(),
        session_id: format!("session-{user_id}"),
        messages: vec![
            json!({"role": "user", "content": "synthetic test preference"})
                .as_object()
                .unwrap()
                .clone(),
        ],
        turn_count: 1,
        session_start: None,
    }
}

#[tokio::test]
async fn observer_uses_fresh_write_authority_and_memoria_owner_for_each_user() {
    for master in [false, true] {
        let (url, requests, server) = backend(StatusCode::NO_CONTENT).await;
        let mut authority = port(url);
        authority.master = master;
        let calls = authority.calls.clone();
        let worker = DatabaseTurnObserverWorker::new(Some(Arc::new(authority)));
        for user in ["astra-a", "astra-b"] {
            assert_eq!(
                worker.run(request(user)).await.unwrap(),
                TurnObserverOutcome::Observed
            );
        }
        let received = requests.lock().unwrap();
        assert_eq!(received.len(), 2);
        for ((headers, body), user) in received.iter().zip(["astra-a", "astra-b"]) {
            let scheme = if master { "Memoria-Owner" } else { "Bearer" };
            assert_eq!(
                headers["authorization"],
                format!(
                    "{scheme} {}",
                    if master {
                        "synthetic-master-key"
                    } else {
                        "synthetic-scoped-key"
                    }
                )
            );
            assert_eq!(headers["x-user-id"], format!("memory-{user}"));
            assert_eq!(headers.get_all("x-user-id").iter().count(), 1);
            assert!(!headers.contains_key("x-impersonate-user"));
            assert_eq!(
                body,
                &json!({"messages": request(user).messages, "session_id": format!("session-{user}")})
            );
        }
        assert_eq!(
            *calls.lock().unwrap(),
            [
                "bind:astra-a",
                "admit-write",
                "resolve-write",
                "bind:astra-b",
                "admit-write",
                "resolve-write"
            ]
        );
        server.abort();
    }
}

#[tokio::test]
async fn observer_skips_disabled_or_read_only_access_without_contacting_backend() {
    let (url, requests, server) = backend(StatusCode::NO_CONTENT).await;
    let mut authority = port(url);
    authority.allowed = Ok(false);
    let calls = authority.calls.clone();
    let worker = DatabaseTurnObserverWorker::new(Some(Arc::new(authority)));
    assert_eq!(
        worker.run(request("astra-a")).await.unwrap(),
        TurnObserverOutcome::AccessDisabled
    );
    assert_eq!(*calls.lock().unwrap(), ["bind:astra-a", "admit-write"]);
    assert!(requests.lock().unwrap().is_empty());
    server.abort();
}

#[tokio::test]
async fn observer_fails_closed_on_authority_error_or_revocation_after_admission() {
    for revoked_after_admission in [false, true] {
        let (url, requests, server) = backend(StatusCode::NO_CONTENT).await;
        let mut authority = port(url);
        if revoked_after_admission {
            authority.transport_error = Some("grant revoked".into());
        } else {
            authority.allowed = Err("authority unavailable".into());
        }
        let worker = DatabaseTurnObserverWorker::new(Some(Arc::new(authority)));
        assert!(worker.run(request("astra-a")).await.is_err());
        assert!(requests.lock().unwrap().is_empty());
        server.abort();
    }
}

#[tokio::test]
async fn observer_rejects_an_admitted_port_without_transport() {
    let (url, requests, server) = backend(StatusCode::NO_CONTENT).await;
    let mut authority = port(url);
    authority.transport_missing = true;
    let worker = DatabaseTurnObserverWorker::new(Some(Arc::new(authority)));
    assert_eq!(
        worker.run(request("astra-a")).await.unwrap_err(),
        "Memoria observer requires an authorized HTTP transport"
    );
    assert!(requests.lock().unwrap().is_empty());
    server.abort();
}

#[tokio::test]
async fn observer_rejects_rebinding_another_owner() {
    let (url, requests, server) = backend(StatusCode::NO_CONTENT).await;
    let mut authority = port(url);
    authority.bound_owner = Some("astra-a".into());
    let worker = DatabaseTurnObserverWorker::new(Some(Arc::new(authority)));
    assert!(worker.run(request("astra-b")).await.is_err());
    assert!(requests.lock().unwrap().is_empty());
    server.abort();
}

#[tokio::test]
async fn observer_missing_port_and_empty_input_are_not_success() {
    let worker = DatabaseTurnObserverWorker::new(None);
    assert_eq!(
        worker.run(request("astra-a")).await.unwrap(),
        TurnObserverOutcome::NotConfigured
    );
    let mut empty = request("astra-a");
    empty.messages.clear();
    assert_eq!(worker.run(empty).await.unwrap(), TurnObserverOutcome::Empty);
}

#[tokio::test]
async fn observer_reports_backend_rejection_without_response_body() {
    let (url, requests, server) = backend(StatusCode::FORBIDDEN).await;
    let worker = DatabaseTurnObserverWorker::new(Some(Arc::new(port(url))));
    let error = worker.run(request("astra-a")).await.unwrap_err();
    assert_eq!(error, "memoria observer run failed: status=403 Forbidden");
    assert_eq!(requests.lock().unwrap().len(), 1);
    server.abort();
}

#[tokio::test]
async fn observer_does_not_follow_redirects_with_owner_authority() {
    let (destination, received, destination_task) = backend(StatusCode::NO_CONTENT).await;
    let app = Router::new().route(
        "/v1/observe",
        post(move || {
            let target = format!("{destination}/v1/observe");
            async move {
                (
                    StatusCode::TEMPORARY_REDIRECT,
                    [(axum::http::header::LOCATION, target)],
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let source_task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let worker = DatabaseTurnObserverWorker::new(Some(Arc::new(port(url))));
    assert!(
        worker
            .run(request("astra-a"))
            .await
            .unwrap_err()
            .contains("307")
    );
    assert!(received.lock().unwrap().is_empty());
    source_task.abort();
    destination_task.abort();
}

#[tokio::test]
async fn owner_auth_rejects_legacy_missing_duplicate_and_invalid_owner_headers() {
    let (url, _, server) = backend(StatusCode::NO_CONTENT).await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let url = format!("{url}/v1/observe");
    let oversized_owner = "a".repeat(65);
    for owners in [
        vec![],
        vec!["owner-a", "owner-b"],
        vec![""],
        vec!["grp_owner"],
        vec![oversized_owner.as_str()],
    ] {
        let mut call = client
            .post(&url)
            .header("Authorization", "Memoria-Owner synthetic-master-key")
            .header("X-Impersonate-User", "legacy-owner")
            .json(&json!({"messages": [], "session_id": "synthetic-session"}));
        for owner in owners {
            call = call.header("X-User-Id", owner);
        }
        assert_eq!(call.send().await.unwrap().status(), StatusCode::BAD_REQUEST);
    }
    server.abort();
}

#[tokio::test]
async fn observer_connection_error_is_classified_without_url_or_credential() {
    // Keep the port reserved without listening. Dropping a listener here lets
    // another concurrently running test claim the address before the request.
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let endpoint = format!("http://{}", socket.local_addr().unwrap());
    let worker = DatabaseTurnObserverWorker::new(Some(Arc::new(port(endpoint.clone()))));
    let error = worker.run(request("astra-a")).await.unwrap_err();
    // A bound, non-listening socket may reject the connection or time it out,
    // depending on the host network stack. Both must remain classified and
    // sanitized; the OS behavior is not an application protocol contract.
    assert!(
        matches!(
            error.as_str(),
            "memoria observer connection failed" | "memoria observer request timed out"
        ),
        "unexpected network failure category: {error}"
    );
    assert!(!error.contains(&endpoint));
    assert!(!error.contains("synthetic-scoped-key"));
}

#[tokio::test(start_paused = true)]
async fn observer_retry_after_authority_timeout_resolves_current_grant() {
    let (url, requests, server) = backend(StatusCode::NO_CONTENT).await;
    let mut authority = port(url);
    let grant = Arc::new(AtomicU8::new(0));
    authority.changing_grant = Some(grant.clone());
    let calls = authority.calls.clone();
    let worker = DatabaseTurnObserverWorker::new(Some(Arc::new(authority)));
    let started = tokio::time::Instant::now();
    assert!(
        worker
            .run(request("astra-a"))
            .await
            .unwrap_err()
            .contains("authority lookup timed out")
    );
    assert_eq!(started.elapsed(), std::time::Duration::from_secs(10));
    assert!(requests.lock().unwrap().is_empty());
    grant.store(1, Ordering::SeqCst);
    assert_eq!(
        worker.run(request("astra-a")).await.unwrap(),
        TurnObserverOutcome::AccessDisabled
    );
    assert!(
        requests.lock().unwrap().is_empty(),
        "a revoked retry cannot reuse old authority"
    );
    grant.store(2, Ordering::SeqCst);
    tokio::time::resume();
    assert_eq!(
        worker.run(request("astra-a")).await.unwrap(),
        TurnObserverOutcome::Observed
    );
    assert_eq!(requests.lock().unwrap().len(), 1);
    assert_eq!(
        *calls.lock().unwrap(),
        [
            "bind:astra-a",
            "admit-write",
            "resolve-write",
            "bind:astra-a",
            "admit-write",
            "bind:astra-a",
            "admit-write",
            "resolve-write"
        ]
    );
    server.abort();
}

#[tokio::test]
async fn observer_rechecks_authority_after_transcript_admission() {
    let (url, requests, server) = backend(StatusCode::NO_CONTENT).await;
    let mut authority = port(url);
    let grant = Arc::new(AtomicU8::new(2));
    authority.changing_grant = Some(grant.clone());
    let worker = DatabaseTurnObserverWorker::new(Some(Arc::new(authority)));
    assert!(
        worker
            .admits_observation("astra-a", "session-a")
            .await
            .unwrap()
    );
    grant.store(1, Ordering::SeqCst);
    assert_eq!(
        worker.run(request("astra-a")).await.unwrap(),
        TurnObserverOutcome::AccessDisabled
    );
    assert!(requests.lock().unwrap().is_empty());
    server.abort();
}

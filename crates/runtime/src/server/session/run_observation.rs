//! Delivery of durable Run admission hints. The Run tree remains authoritative.
use std::{
    collections::HashMap,
    convert::Infallible,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use astra_services::runs::RunTreeNotice;
use axum::response::sse::{Event, Sse};
use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;

use super::*;

type Scope = (String, String);
type Sender = broadcast::Sender<RunTreeNotice>;

#[derive(Default)]
pub(crate) struct RunObservation {
    sessions: Mutex<HashMap<Scope, Weak<Sender>>>,
    gap: watch::Sender<()>,
    live_peers: AtomicUsize,
    delivery_active: AtomicBool,
    pub(crate) shutdown: CancellationToken,
}

impl RunObservation {
    fn has_coverage(&self, peers: &Peers) -> bool {
        self.delivery_active.load(Ordering::Acquire)
            && self.live_peers.load(Ordering::Acquire) == peers.origins.len()
    }

    fn subscribe(self: &Arc<Self>, scope: Scope) -> Subscription {
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let sender = sessions
            .get(&scope)
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| {
                let sender = Arc::new(broadcast::channel(16).0);
                sessions.insert(scope.clone(), Arc::downgrade(&sender));
                sender
            });
        let receiver = sender.subscribe();
        Subscription {
            observation: Arc::clone(self),
            scope,
            sender,
            receiver,
            gap: self.gap.subscribe(),
        }
    }

    pub(crate) fn publish(&self, notice: RunTreeNotice) {
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let scope = (notice.user_id.clone(), notice.session_id.clone());
        if let Some(sender) = sessions.get(&scope).and_then(Weak::upgrade) {
            let _ = sender.send(notice);
        }
    }
}

struct Subscription {
    observation: Arc<RunObservation>,
    scope: Scope,
    sender: Arc<Sender>,
    receiver: broadcast::Receiver<RunTreeNotice>,
    gap: watch::Receiver<()>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let mut sessions = self
            .observation
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Publish and subscribe hold the same lock when upgrading the weak ref.
        if Arc::strong_count(&self.sender) == 1 {
            sessions.remove(&self.scope);
        }
    }
}

pub(crate) async fn session_run_admissions(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<
    Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>>,
    (StatusCode, Json<ErrorResponse>),
> {
    let observation = &state.run_observation;
    let Some(peers) = state.run_observation_peers.as_ref() else {
        return Err(error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Run observation peers are not configured",
        ));
    };
    if !observation.has_coverage(peers) {
        return Err(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Run observation peers unavailable",
        ));
    }
    let user = state.auth_service.current_user(&headers).await?;
    let session_id = state
        .session_service
        .resolve_owned_session_id(session_id, user.user_id.clone())
        .await?;
    let mut subscription = state
        .run_observation
        .subscribe((user.user_id.clone(), session_id));
    if !observation.has_coverage(peers) {
        return Err(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Run observation peers unavailable",
        ));
    }
    let events = async_stream::stream! {
        // A client reads its baseline after receiving this acknowledgment.
        yield Ok(Event::default().data(r#"{"type":"run_observation_ready"}"#));
        let interval = Duration::from_secs(15);
        let mut heartbeat = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let received = tokio::select! {
                biased;
                _ = subscription.observation.shutdown.cancelled() => break,
                _ = subscription.gap.changed() => {
                    yield Ok(Event::default().data(r#"{"type":"run_observation_gap"}"#));
                    break;
                }
                _ = heartbeat.tick() => {
                    yield Ok(Event::default().data(r#"{"type":"run_observation_heartbeat"}"#));
                    continue;
                }
                notice = subscription.receiver.recv() => notice,
            };
            match received {
                Ok(notice) => {
                    // A keepalive conveys no Session fact. Reauthorize before
                    // delivering each fact, including on long-lived connections.
                    if !tokio::time::timeout(Duration::from_secs(5), state.auth_service.current_user(&headers))
                        .await.is_ok_and(|result| result.is_ok_and(|current| current.user_id == user.user_id)) {
                        break;
                    }
                    let event = match (notice.confirmed, &notice.run_id) {
                        (true, Some(_)) => "admitted",
                        (true, None) => "deleted",
                        (false, _) => "uncertain",
                    };
                    let data = serde_json::json!({
                        "type": "run_observation_notice",
                        "event": event,
                        "run_id": notice.run_id,
                    });
                    yield Ok(Event::default().data(data.to_string()));
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    yield Ok(Event::default().data(r#"{"type":"run_observation_gap"}"#));
                    break;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    Ok(Sse::new(events))
}

pub(crate) fn spawn_local_delivery(
    state: &AppState,
    cancel: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let observation = Arc::clone(&state.run_observation);
    let mut admissions = state.run_admissions.subscribe();
    observation.delivery_active.store(true, Ordering::Release);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                notice = admissions.recv() => match notice {
                    Ok(notice) => observation.publish(notice),
                    Err(broadcast::error::RecvError::Lagged(_)) => { observation.gap.send_replace(()); },
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            }
        }
        observation.delivery_active.store(false, Ordering::Release);
        observation.gap.send_replace(());
    })
}

const PEER_ADMISSIONS_PATH: &str = "/service/run-admissions";
const PEER_HEARTBEAT: Duration = Duration::from_secs(10);
const MAX_PEER_NOTICE_BYTES: usize = 2048;

#[derive(Clone)]
pub(crate) struct Peers {
    origins: Vec<reqwest::Url>,
    credential: String,
    client: reqwest::Client,
}

impl Peers {
    pub(crate) fn configured(key: [u8; 32]) -> Result<Option<Self>, String> {
        use base64::Engine;

        let Some(origins) = std::env::var_os("ASTRA_RUN_OBSERVATION_PEERS") else {
            return Ok(None);
        };
        let origins = origins
            .into_string()
            .map_err(|_| "Run observation peer origins must be UTF-8")?;
        let origins = if origins.trim().is_empty() {
            Vec::new()
        } else {
            origins
                .split(',')
                .map(str::trim)
                .map(parse_peer_origin)
                .collect::<Result<Vec<_>, _>>()?
        };
        if origins.len() > 16
            || origins
                .iter()
                .enumerate()
                .any(|(i, origin)| origins[..i].contains(origin))
        {
            return Err("Run observation requires at most 16 distinct peer origins".into());
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .read_timeout(PEER_HEARTBEAT * 3)
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Some(Self {
            origins,
            credential: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key),
            client,
        }))
    }

    fn authorized(&self, headers: &HeaderMap) -> bool {
        let Some(provided) = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|header| header.as_bytes().strip_prefix(b"Bearer "))
        else {
            return false;
        };
        let expected = self.credential.as_bytes();
        provided.len() == expected.len()
            && provided
                .iter()
                .zip(expected)
                .fold(0, |diff, (a, b)| diff | (a ^ b))
                == 0
    }
}

fn parse_peer_origin(origin: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(origin).map_err(|_| "Invalid Run observation peer origin")?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err(
            "Run observation peers require HTTPS origins without credentials or paths".into(),
        );
    }
    Ok(url)
}

pub(crate) async fn peer_run_admissions(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<axum::response::Response, StatusCode> {
    if !state
        .run_observation_peers
        .as_ref()
        .is_some_and(|peers| peers.authorized(&headers))
    {
        return Err(StatusCode::UNAUTHORIZED);
    }
    // Export only this Server's own commit notices. Received peer events are
    // delivered to local subscribers and never fed into this channel.
    let mut notices = state.run_admissions.subscribe();
    let shutdown = state.run_observation.shutdown.clone();
    let events = async_stream::stream! {
        yield Ok::<_, Infallible>("null\n".to_owned());
        let mut heartbeat = tokio::time::interval_at(
            tokio::time::Instant::now() + PEER_HEARTBEAT, PEER_HEARTBEAT,
        );
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => break,
                notice = notices.recv() => {
                    // Stream termination is the gap signal on overflow.
                    let Ok(notice) = notice else { break };
                    let Ok(mut line) = serde_json::to_string(&notice) else { break };
                    if line.len() >= MAX_PEER_NOTICE_BYTES { break }
                    line.push('\n');
                    yield Ok(line);
                }
                _ = heartbeat.tick() => yield Ok("null\n".to_owned()),
            }
        }
    };
    Ok(axum::response::Response::builder()
        .header(axum::http::header::CONTENT_TYPE, "application/x-ndjson")
        .header(axum::http::header::CACHE_CONTROL, "no-store")
        .body(axum::body::Body::from_stream(events))
        .expect("static Run observation response headers"))
}

pub(crate) fn spawn_peer_delivery(
    state: &AppState,
    cancel: CancellationToken,
) -> Vec<tokio::task::JoinHandle<()>> {
    let Some(peers) = state.run_observation_peers.as_ref() else {
        return Vec::new();
    };
    peers.origins.iter().map(|origin| {
        let client = peers.client.clone();
        let credential = peers.credential.clone();
        let endpoint = origin.join(PEER_ADMISSIONS_PATH).expect("validated peer URL");
        let observation = Arc::clone(&state.run_observation);
        let cancel = cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return,
                    () = receive_peer_notices(&client, &endpoint, &credential, &observation) => {}
                }
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                }
            }
        })
    }).collect()
}

async fn receive_peer_notices(
    client: &reqwest::Client,
    endpoint: &reqwest::Url,
    credential: &str,
    observation: &RunObservation,
) {
    let Ok(response) = client
        .get(endpoint.clone())
        .bearer_auth(credential)
        .send()
        .await
    else {
        return;
    };
    if response.status() != reqwest::StatusCode::OK
        || !response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .is_some_and(|value| value == "application/x-ndjson")
    {
        return;
    }
    let mut response = response;
    let mut line = Vec::new();
    let mut connected = None;
    while let Ok(Some(chunk)) = response.chunk().await {
        for fragment in chunk.split_inclusive(|byte| *byte == b'\n') {
            if line.len() + fragment.len() > MAX_PEER_NOTICE_BYTES {
                return;
            }
            line.extend_from_slice(fragment);
            if fragment.last() == Some(&b'\n') {
                match serde_json::from_slice::<Option<RunTreeNotice>>(&line) {
                    Ok(Some(notice)) if connected.is_some() => observation.publish(notice),
                    Ok(Some(_)) => return,
                    Ok(None) if connected.is_none() => {
                        observation.live_peers.fetch_add(1, Ordering::AcqRel);
                        connected = Some(LivePeer(observation));
                    }
                    Ok(None) => {}
                    Err(_) => return,
                }
                line.clear();
            }
        }
    }
}

struct LivePeer<'a>(&'a RunObservation);

impl Drop for LivePeer<'_> {
    fn drop(&mut self) {
        self.0.live_peers.fetch_sub(1, Ordering::AcqRel);
        self.0.gap.send_replace(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn admission_notices_only_wake_subscribers_of_the_same_owner_and_session() {
        let observation = Arc::new(RunObservation::default());
        let mut own = observation.subscribe(("owner".into(), "session".into()));
        let mut foreign = observation.subscribe(("other".into(), "session".into()));
        let mut unrelated = observation.subscribe(("owner".into(), "second".into()));
        let notice = RunTreeNotice {
            user_id: "owner".into(),
            session_id: "session".into(),
            run_id: Some("run".into()),
            confirmed: false,
        };
        observation.publish(notice.clone());
        assert_eq!(own.receiver.try_recv().unwrap(), notice);
        assert!(matches!(
            foreign.receiver.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            unrelated.receiver.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        drop((own, foreign, unrelated));
        assert!(observation.sessions.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn peer_stream_delivers_scoped_notice_and_reports_disconnect() {
        struct Healthy;
        #[async_trait::async_trait]
        impl crate::HealthChecker for Healthy {
            async fn database_healthy(&self) -> bool {
                true
            }
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url =
            reqwest::Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let credential = "test-peer-credential".to_owned();
        let mut source = AppState::new(crate::ServiceInfo::default(), Arc::new(Healthy));
        source.run_observation_peers = Some(Peers {
            origins: vec![],
            credential: credential.clone(),
            client: client.clone(),
        });
        let source_shutdown = source.run_observation.shutdown.clone();
        let admissions = source.run_admissions.clone();
        let app = axum::Router::new()
            .route(
                PEER_ADMISSIONS_PATH,
                axum::routing::get(peer_run_admissions),
            )
            .with_state(source);
        let serve = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut target = AppState::new(crate::ServiceInfo::default(), Arc::new(Healthy));
        target.run_observation_peers = Some(Peers {
            origins: vec![url.clone()],
            credential,
            client: client.clone(),
        });
        let reject = client
            .get(url.join(PEER_ADMISSIONS_PATH).unwrap())
            .send()
            .await
            .unwrap();
        assert_eq!(reject.status(), StatusCode::UNAUTHORIZED);

        let mut subscription = target
            .run_observation
            .subscribe(("owner".into(), "session".into()));
        let cancel = CancellationToken::new();
        let peers = spawn_peer_delivery(&target, cancel.clone());
        tokio::time::timeout(Duration::from_secs(3), async {
            while target.run_observation.live_peers.load(Ordering::Acquire) != 1 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("authenticated peer became available");
        let notice = RunTreeNotice {
            user_id: "owner".into(),
            session_id: "session".into(),
            run_id: Some("run".into()),
            confirmed: true,
        };
        admissions.send(notice.clone()).unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), subscription.receiver.recv())
                .await
                .unwrap()
                .unwrap(),
            notice
        );

        source_shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(3), subscription.gap.changed())
            .await
            .expect("closed peer is visible")
            .unwrap();
        assert_eq!(target.run_observation.live_peers.load(Ordering::Acquire), 0);
        cancel.cancel();
        for peer in peers {
            peer.await.unwrap();
        }
        serve.abort();
    }

    #[test]
    fn peer_origin_requires_a_direct_https_endpoint() {
        assert!(parse_peer_origin("https://peer.example").is_ok());
        for invalid in [
            "http://peer.example",
            "https://user:password@peer.example",
            "https://peer.example/path",
            "https://peer.example/?foo=bar",
        ] {
            assert!(parse_peer_origin(invalid).is_err(), "{invalid}");
        }
    }
}

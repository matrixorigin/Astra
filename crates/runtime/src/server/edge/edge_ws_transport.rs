//! Edge WebSocket upgrade and write-idle observation.
//!
//! A flush only reports completion for the whole frame. Observe positive writes
//! below framing so a slow transfer and a stalled writer are distinguishable.

use axum::extract::ws::rejection::{
    ConnectionNotUpgradable, WebSocketKeyHeaderMissing, WebSocketUpgradeRejection,
};
use axum::extract::{FromRequestParts, WebSocketUpgrade};
use axum::http::{HeaderValue, StatusCode, header, request::Parts};
use axum::response::{IntoResponse, Response};
use edge_websocket::WebSocketStream;
use edge_websocket::tungstenite::handshake::derive_accept_key;
use edge_websocket::tungstenite::protocol::{Role, WebSocketConfig};
use hyper::upgrade::{OnUpgrade, Upgraded};
use hyper_util::rt::TokioIo;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::io::InspectWriter;

type WriteObserver = Box<dyn FnMut(&[u8]) + Send + Sync>;
pub(super) type EdgeWebSocket = WebSocketStream<InspectWriter<TokioIo<Upgraded>, WriteObserver>>;

pub(super) async fn prepare_upgrade(
    parts: &mut Parts,
) -> Result<(Response, OnUpgrade), WebSocketUpgradeRejection> {
    // Use the standard extractor for the wire contract. Retain a handle to
    // the same upgrade so the socket can be wrapped before creating the WS.
    let upgrade = parts.extensions.get::<OnUpgrade>().cloned();
    drop(WebSocketUpgrade::from_request_parts(parts, &()).await?);
    let upgrade = upgrade.ok_or(ConnectionNotUpgradable::default())?;
    let key = parts
        .headers
        .get(header::SEC_WEBSOCKET_KEY)
        .ok_or(WebSocketKeyHeaderMissing::default())?;

    let mut response = StatusCode::SWITCHING_PROTOCOLS.into_response();
    response
        .headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
    response
        .headers_mut()
        .insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    response.headers_mut().insert(
        header::SEC_WEBSOCKET_ACCEPT,
        HeaderValue::from_str(&derive_accept_key(key.as_bytes()))
            .expect("WebSocket accept key is base64"),
    );
    Ok((response, upgrade))
}

pub(super) async fn open_socket(
    upgrade: OnUpgrade,
) -> Result<(EdgeWebSocket, Arc<WriteProgress>), hyper::Error> {
    let progress = Arc::new(WriteProgress {
        origin: Instant::now(),
        last_write_ms: AtomicU64::new(0),
    });
    let upstream = TokioIo::new(upgrade.await?);
    let observed = Arc::clone(&progress);
    let observe: WriteObserver = Box::new(move |_| {
        observed.last_write_ms.store(
            observed.origin.elapsed().as_millis() as u64,
            Ordering::Relaxed,
        );
    });
    let io = InspectWriter::new(upstream, observe);
    let config = WebSocketConfig::default().max_message_size(Some(
        astra_server_types::edge_ws_protocol::MAX_EDGE_MESSAGE_BYTES,
    ));
    let socket = WebSocketStream::from_raw_socket(io, Role::Server, Some(config)).await;
    Ok((socket, progress))
}

pub(super) struct WriteProgress {
    origin: Instant,
    last_write_ms: AtomicU64,
}

impl WriteProgress {
    pub(super) async fn wait_until_idle(&self, idle_limit: Duration) {
        let started = Instant::now();
        let mut deadline = started + idle_limit;
        loop {
            tokio::time::sleep_until(deadline).await;
            let elapsed = Duration::from_millis(self.last_write_ms.load(Ordering::Relaxed));
            let last_write = started.max(self.origin + elapsed);
            let next_deadline = last_write + idle_limit;
            if next_deadline <= Instant::now() {
                return;
            }
            deadline = next_deadline;
        }
    }
}

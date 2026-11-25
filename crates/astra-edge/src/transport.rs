//! Observe incoming bytes before WebSocket framing for connection liveness.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;

use tokio::{net::TcpStream, time::Instant};
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, client_async_tls_with_config,
    tungstenite::{Error, client::IntoClientRequest, error::UrlError, http::Request},
};
use tokio_util::io::InspectReader;

type ReceiveObserver = Box<dyn FnMut(&[u8]) + Send + Sync>;
type ObservedStream = InspectReader<TcpStream, ReceiveObserver>;

pub struct EdgeSocket {
    pub(super) websocket: WebSocketStream<MaybeTlsStream<ObservedStream>>,
    pub(super) received: Arc<ReceiveProgress>,
}

pub(super) struct ReceiveProgress {
    origin: Instant,
    last_read_ms: AtomicU64,
}

impl ReceiveProgress {
    pub(super) fn last_read(&self) -> Instant {
        self.origin + Duration::from_millis(self.last_read_ms.load(Ordering::Relaxed))
    }
}

fn observe(stream: TcpStream) -> (ObservedStream, Arc<ReceiveProgress>) {
    let received = Arc::new(ReceiveProgress {
        origin: Instant::now(),
        last_read_ms: AtomicU64::new(0),
    });
    let observer = received.clone();
    let record: ReceiveObserver = Box::new(move |bytes| {
        if !bytes.is_empty() {
            observer.last_read_ms.store(
                observer.origin.elapsed().as_millis() as u64,
                Ordering::Relaxed,
            );
        }
    });
    (InspectReader::new(stream, record), received)
}

pub async fn connect_edge_socket(request: impl IntoClientRequest) -> Result<EdgeSocket, Error> {
    let request = request.into_client_request()?;
    let uri = request.uri();
    let host = uri.host().ok_or(Error::Url(UrlError::NoHostName))?;
    let port = uri
        .port_u16()
        .or_else(|| match uri.scheme_str() {
            Some("wss") => Some(443),
            Some("ws") => Some(80),
            _ => None,
        })
        .ok_or(Error::Url(UrlError::UnsupportedUrlScheme))?;
    let stream = TcpStream::connect(format!("{host}:{port}")).await?;
    upgrade_edge_socket(request, stream).await
}

pub async fn upgrade_edge_socket(
    request: Request<()>,
    stream: TcpStream,
) -> Result<EdgeSocket, Error> {
    let (stream, received) = observe(stream);
    let (websocket, _) = client_async_tls_with_config(
        request,
        stream,
        None,
        Some(Connector::Rustls(astra_core::net::public_root_tls_config())),
    )
    .await?;
    Ok(EdgeSocket {
        websocket,
        received,
    })
}

#[cfg(test)]
pub(super) async fn accept_edge_socket(stream: TcpStream) -> EdgeSocket {
    let (stream, received) = observe(stream);
    let websocket = tokio_tungstenite::accept_async(MaybeTlsStream::Plain(stream))
        .await
        .unwrap();
    EdgeSocket {
        websocket,
        received,
    }
}

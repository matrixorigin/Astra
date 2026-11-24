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
        Some(
            tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
                .max_message_size(Some(
                    astra_server_types::edge_ws_protocol::MAX_EDGE_MESSAGE_BYTES,
                ))
                .max_frame_size(Some(
                    astra_server_types::edge_ws_protocol::MAX_EDGE_MESSAGE_BYTES,
                )),
        ),
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

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::{Message, error::CapacityError};

    #[tokio::test]
    async fn observed_socket_retains_the_edge_frame_size_limit() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            websocket.send(Message::Text("ready".into())).await.unwrap();
            websocket
                .send(Message::Binary(
                    vec![0; astra_server_types::edge_ws_protocol::MAX_EDGE_MESSAGE_BYTES + 1]
                        .into(),
                ))
                .await
                .unwrap();
        });
        let mut socket = connect_edge_socket(format!("ws://{address}"))
            .await
            .unwrap();
        assert_eq!(
            socket.websocket.next().await.unwrap().unwrap(),
            Message::Text("ready".into())
        );
        assert!(matches!(
            socket.websocket.next().await.unwrap(),
            Err(Error::Capacity(CapacityError::MessageTooLong { .. }))
        ));
        peer.await.unwrap();
    }
}

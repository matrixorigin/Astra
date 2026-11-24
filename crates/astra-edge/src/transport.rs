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
        // Tool requests have their own server-side bound and may exceed the
        // inference control-message budget. Preserve tungstenite's established
        // receive limits for this shared tool/inference transport.
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

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::Message;

    #[tokio::test]
    async fn observed_socket_accepts_large_tool_requests() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            websocket.send(Message::Text("ready".into())).await.unwrap();
            let request = serde_json::json!({
                "type": "tool_request", "id": "large-request", "tool": "write_file",
                "args": {"path": "output.txt", "content": "x".repeat(300 * 1024)}
            });
            websocket
                .send(Message::Text(
                    serde_json::to_string(&request).unwrap().into(),
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
        let frame = socket.websocket.next().await.unwrap().unwrap();
        let request: serde_json::Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
        assert_eq!(
            request["args"]["content"].as_str().unwrap().len(),
            300 * 1024
        );
        peer.await.unwrap();
    }
}

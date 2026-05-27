use std::sync::Arc;

use futures_util::StreamExt;
use reqwest::{
    StatusCode, Url,
    header::{ACCEPT, ACCEPT_ENCODING, CONTENT_TYPE, HeaderMap},
};
use rmcp::{
    RoleClient,
    model::ServerJsonRpcMessage,
    service::{RxJsonRpcMessage, TxJsonRpcMessage},
    transport::Transport as RmcpTransport,
};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

use crate::MCP_CONNECT_TIMEOUT_SECS;

const EVENT_STREAM_MIME: &str = "text/event-stream";
const JSON_MIME: &str = "application/json";

#[derive(Debug, thiserror::Error)]
pub(crate) enum ClassicSseError {
    #[error("invalid SSE url: {0}")]
    InvalidUrl(String),
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("SSE endpoint closed before sending a message endpoint")]
    MissingEndpoint,
    #[error("SSE message endpoint wait timed out after {0}s")]
    EndpointTimeout(u64),
    #[error("SSE reader stopped before endpoint negotiation completed")]
    EndpointNegotiationStopped,
    #[error("SSE endpoint is cross-origin: {0}")]
    CrossOriginEndpoint(String),
    #[error("SSE message POST returned HTTP {status}: {body}")]
    MessagePostStatus { status: StatusCode, body: String },
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ClassicSseEvent {
    event: Option<String>,
    data: Option<String>,
}

/// MCP classic SSE client transport compatible with mcp-go's SSE server.
pub(crate) struct ClassicSseTransport {
    name: String,
    client: reqwest::Client,
    message_endpoint: Arc<str>,
    headers: HeaderMap,
    incoming: mpsc::Receiver<ServerJsonRpcMessage>,
    read_handle: JoinHandle<()>,
}

impl ClassicSseTransport {
    pub(crate) async fn connect(
        name: &str,
        url: &str,
        headers: HeaderMap,
    ) -> Result<Self, ClassicSseError> {
        let base_url =
            Url::parse(url).map_err(|error| ClassicSseError::InvalidUrl(error.to_string()))?;
        let client = reqwest::Client::builder()
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .build()?;
        let response = client
            .get(base_url.clone())
            .headers(headers.clone())
            .header(ACCEPT, EVENT_STREAM_MIME)
            .header(ACCEPT_ENCODING, "identity")
            .send()
            .await?
            .error_for_status()?;

        let mut byte_stream = response.bytes_stream();
        let (endpoint_tx, endpoint_rx) = oneshot::channel();
        let (incoming_tx, incoming_rx) = mpsc::channel(64);
        let reader_name = name.to_string();
        let read_handle = tokio::spawn(async move {
            let mut endpoint_tx = Some(endpoint_tx);
            let mut buffer = Vec::new();

            loop {
                match read_next_sse_event(&mut byte_stream, &mut buffer).await {
                    Ok(Some(event)) => {
                        if event.event.as_deref() == Some("endpoint") {
                            let result = event
                                .data
                                .as_deref()
                                .ok_or(ClassicSseError::MissingEndpoint)
                                .and_then(|data| resolve_message_endpoint(&base_url, data));
                            match result {
                                Ok(endpoint) => {
                                    if let Some(tx) = endpoint_tx.take() {
                                        let _ = tx.send(Ok(endpoint));
                                    }
                                }
                                Err(error) => {
                                    if let Some(tx) = endpoint_tx.take() {
                                        let _ = tx.send(Err(error));
                                    }
                                    break;
                                }
                            }
                            continue;
                        }

                        let is_message_event =
                            matches!(event.event.as_deref(), None | Some("") | Some("message"));
                        if !is_message_event {
                            continue;
                        }

                        if let Some(data) = event.data {
                            match serde_json::from_str::<ServerJsonRpcMessage>(&data) {
                                Ok(message) => {
                                    if incoming_tx.send(message).await.is_err() {
                                        break;
                                    }
                                }
                                Err(error) => {
                                    tracing::debug!(
                                        "MCP classic SSE message parse error [{reader_name}]: {error}"
                                    );
                                }
                            }
                        }
                    }
                    Ok(None) => {
                        if let Some(tx) = endpoint_tx.take() {
                            let _ = tx.send(Err(ClassicSseError::MissingEndpoint));
                        }
                        break;
                    }
                    Err(error) => {
                        if let Some(tx) = endpoint_tx.take() {
                            let _ = tx.send(Err(error));
                        } else {
                            tracing::warn!("MCP classic SSE read error [{reader_name}]: {error}");
                        }
                        break;
                    }
                }
            }
        });

        let message_endpoint = tokio::time::timeout(
            std::time::Duration::from_secs(MCP_CONNECT_TIMEOUT_SECS),
            endpoint_rx,
        )
        .await
        .map_err(|_| ClassicSseError::EndpointTimeout(MCP_CONNECT_TIMEOUT_SECS))?
        .map_err(|_| ClassicSseError::EndpointNegotiationStopped)??;

        Ok(Self {
            name: name.to_string(),
            client,
            message_endpoint,
            headers,
            incoming: incoming_rx,
            read_handle,
        })
    }
}

impl Drop for ClassicSseTransport {
    fn drop(&mut self) {
        self.read_handle.abort();
    }
}

impl RmcpTransport<RoleClient> for ClassicSseTransport {
    type Error = ClassicSseError;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let client = self.client.clone();
        let endpoint = self.message_endpoint.clone();
        let headers = self.headers.clone();

        async move {
            let response = client
                .post(endpoint.as_ref())
                .headers(headers)
                .header(ACCEPT, JSON_MIME)
                .header(ACCEPT_ENCODING, "identity")
                .header(CONTENT_TYPE, JSON_MIME)
                .json(&item)
                .send()
                .await?;

            if response.status().is_success() {
                return Ok(());
            }

            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            Err(ClassicSseError::MessagePostStatus { status, body })
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        self.incoming.recv().await
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        tracing::debug!("closing MCP classic SSE transport [{}]", self.name);
        self.read_handle.abort();
        Ok(())
    }
}

async fn read_next_sse_event<S, B>(
    stream: &mut S,
    buffer: &mut Vec<u8>,
) -> Result<Option<ClassicSseEvent>, ClassicSseError>
where
    S: futures_util::Stream<Item = Result<B, reqwest::Error>> + Unpin,
    B: AsRef<[u8]>,
{
    loop {
        if let Some((index, delimiter_len)) = find_sse_boundary(buffer) {
            let frame = buffer[..index].to_vec();
            buffer.drain(..index + delimiter_len);
            return Ok(Some(parse_sse_event(&frame)));
        }

        match stream.next().await {
            Some(Ok(chunk)) => buffer.extend_from_slice(chunk.as_ref()),
            Some(Err(error)) => return Err(ClassicSseError::Http(error)),
            None => {
                if buffer.is_empty() {
                    return Ok(None);
                }
                let frame = std::mem::take(buffer);
                return Ok(Some(parse_sse_event(&frame)));
            }
        }
    }
}

fn find_sse_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    for i in 0..buffer.len().saturating_sub(1) {
        if i + 3 < buffer.len() && &buffer[i..i + 4] == b"\r\n\r\n" {
            return Some((i, 4));
        }
        if &buffer[i..i + 2] == b"\n\n" || &buffer[i..i + 2] == b"\r\r" {
            return Some((i, 2));
        }
    }
    None
}

fn parse_sse_event(frame: &[u8]) -> ClassicSseEvent {
    let text = String::from_utf8_lossy(frame);
    let mut event = None;
    let mut data_lines = Vec::new();

    for raw_line in text.lines() {
        let line = raw_line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with(':') {
            continue;
        }

        let Some((field, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => event = Some(value.to_string()),
            "data" => data_lines.push(value.to_string()),
            _ => {}
        }
    }

    ClassicSseEvent {
        event,
        data: (!data_lines.is_empty()).then(|| data_lines.join("\n")),
    }
}

fn resolve_message_endpoint(base_url: &Url, data: &str) -> Result<Arc<str>, ClassicSseError> {
    let endpoint = base_url
        .join(data.trim())
        .map_err(|error| ClassicSseError::InvalidUrl(error.to_string()))?;
    let same_origin = endpoint.scheme() == base_url.scheme()
        && endpoint.host_str() == base_url.host_str()
        && endpoint.port_or_known_default() == base_url.port_or_known_default();
    if !same_origin {
        return Err(ClassicSseError::CrossOriginEndpoint(
            endpoint.as_str().to_string(),
        ));
    }
    Ok(Arc::from(endpoint.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_endpoint_event_with_crlf() {
        let event = parse_sse_event(b"event: endpoint\r\ndata: /mcp/message?sessionId=abc\r\n");
        assert_eq!(
            event,
            ClassicSseEvent {
                event: Some("endpoint".to_string()),
                data: Some("/mcp/message?sessionId=abc".to_string()),
            }
        );
    }

    #[test]
    fn parse_multiline_data_event() {
        let event = parse_sse_event(b"event: message\ndata: {\"a\":1\ndata: }\n");
        assert_eq!(event.event.as_deref(), Some("message"));
        assert_eq!(event.data.as_deref(), Some("{\"a\":1\n}"));
    }

    #[test]
    fn find_sse_boundary_prefers_full_crlf_separator() {
        assert_eq!(find_sse_boundary(b"a\r\n\r\nb"), Some((1, 4)));
        assert_eq!(find_sse_boundary(b"a\n\nb"), Some((1, 2)));
        assert_eq!(find_sse_boundary(b"a\r\rb"), Some((1, 2)));
    }

    #[test]
    fn resolve_relative_endpoint_against_base_origin() {
        let base = Url::parse("http://localhost:8081/api/v1/workspaces/ws/mcp").unwrap();
        let endpoint =
            resolve_message_endpoint(&base, "/api/v1/workspaces/ws/mcp/message?sessionId=s")
                .unwrap();
        assert_eq!(
            endpoint.as_ref(),
            "http://localhost:8081/api/v1/workspaces/ws/mcp/message?sessionId=s"
        );
    }

    #[test]
    fn reject_cross_origin_endpoint() {
        let base = Url::parse("http://localhost:8081/mcp").unwrap();
        let error = resolve_message_endpoint(&base, "http://example.com/mcp/message").unwrap_err();
        assert!(matches!(error, ClassicSseError::CrossOriginEndpoint(_)));
    }
}

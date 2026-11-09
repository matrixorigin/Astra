//! Bounded HTTP backend for rmcp's streamable transport.
//!
//! rmcp 1.6 buffers JSON and error responses without a limit. Keep session
//! handling in rmcp and bound bytes before its HTTP backend parses them.

use std::{borrow::Cow, collections::HashMap, io, sync::Arc};

use bytes::Bytes;
use futures_util::{
    Stream, StreamExt,
    stream::{self, BoxStream},
};
use rmcp::{
    model::{ClientJsonRpcMessage, JsonRpcMessage, ServerJsonRpcMessage},
    transport::{
        common::http_header::{
            EVENT_STREAM_MIME_TYPE, HEADER_LAST_EVENT_ID, HEADER_SESSION_ID, JSON_MIME_TYPE,
        },
        streamable_http_client::{
            AuthRequiredError, InsufficientScopeError, SseError, StreamableHttpClient,
            StreamableHttpError, StreamableHttpPostResponse,
        },
    },
};
use rmcp_http::{
    Client, RequestBuilder, Response, StatusCode,
    header::{ACCEPT, CONTENT_TYPE, HeaderName, HeaderValue, WWW_AUTHENTICATE},
};
use sse_stream::{Sse, SseStream};

#[cfg(test)]
#[path = "streamable_http_tests.rs"]
mod tests;

const MAX_JSON_BYTES: usize = 16 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 64 * 1024;
const MAX_SSE_EVENT_BYTES: usize = 16 * 1024 * 1024;

type HttpError = StreamableHttpError<rmcp_http::Error>;
type EventStream = BoxStream<'static, Result<Sse, SseError>>;

#[derive(Clone)]
pub(crate) struct BoundedHttpClient(Client);

impl BoundedHttpClient {
    pub(crate) fn new() -> Result<Self, rmcp_http::Error> {
        // Match rmcp's client: pooled connections can cause delayed ACK stalls
        // when an SSE response is not consumed completely.
        Client::builder()
            .pool_max_idle_per_host(0)
            .build()
            .map(Self)
    }
}

fn client_error(error: rmcp_http::Error) -> HttpError {
    HttpError::Client(error)
}

fn add_headers(
    mut request: RequestBuilder,
    auth: Option<String>,
    headers: HashMap<HeaderName, HeaderValue>,
) -> Result<RequestBuilder, HttpError> {
    if let Some(token) = auth {
        request = request.bearer_auth(token);
    }
    for (name, value) in headers {
        if matches!(name.as_str(), "accept" | "mcp-session-id" | "last-event-id") {
            return Err(HttpError::ReservedHeaderConflict(name.to_string()));
        }
        request = request.header(name, value);
    }
    Ok(request)
}

// A length hint is only an early rejection: chunked and decoded bodies must
// still be counted while reading, before the bytes are retained.
async fn read_body(mut response: Response, limit: usize) -> Result<Vec<u8>, HttpError> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(too_large("HTTP response", limit));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(client_error)? {
        if chunk.len() > limit - bytes.len() {
            return Err(too_large("HTTP response", limit));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn too_large(kind: &str, limit: usize) -> HttpError {
    HttpError::UnexpectedServerResponse(Cow::Owned(format!(
        "{kind} exceeds the {limit}-byte limit"
    )))
}

// Count a single SSE event, including lines split across transport chunks.
// Comment lines do not accumulate in the parser; a blank line ends an event.
#[derive(Default)]
struct SseLimit {
    retained: usize,
    line: usize,
    comment: bool,
    previous_cr: bool,
}

impl SseLimit {
    fn observe(&mut self, chunk: &[u8]) -> Result<(), usize> {
        for (index, &byte) in chunk.iter().enumerate() {
            if self.previous_cr {
                self.previous_cr = false;
                if byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\r' | b'\n' => {
                    if self.line == 0 {
                        self.retained = 0;
                    } else if !self.comment {
                        self.retained = self.retained.saturating_add(self.line).saturating_add(1);
                    }
                    self.line = 0;
                    self.comment = false;
                    self.previous_cr = byte == b'\r';
                }
                _ => {
                    if self.line == 0 {
                        self.comment = byte == b':';
                    }
                    self.line = self.line.saturating_add(1);
                }
            }
            if self.retained.saturating_add(self.line) > MAX_SSE_EVENT_BYTES {
                return Err(index);
            }
        }
        Ok(())
    }
}

fn event_stream<S, E>(source: S) -> EventStream
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    let stream = stream::unfold(
        (Box::pin(source), SseLimit::default(), false, false),
        |(mut source, mut limit, pending_error, done)| async move {
            if done {
                return None;
            }
            let too_large = || {
                io::Error::other(format!(
                    "SSE event exceeds the {MAX_SSE_EVENT_BYTES}-byte limit"
                ))
            };
            if pending_error {
                return Some((Err(too_large()), (source, limit, false, true)));
            }
            match source.next().await? {
                Ok(chunk) => match limit.observe(&chunk) {
                    Ok(()) => Some((Ok(chunk), (source, limit, false, false))),
                    Err(0) => Some((Err(too_large()), (source, limit, false, true))),
                    // Deliver already-completed events from this chunk first.
                    Err(index) => Some((Ok(chunk.slice(..index)), (source, limit, true, false))),
                },
                Err(error) => Some((Err(io::Error::other(error)), (source, limit, false, true))),
            }
        },
    );
    SseStream::from_bytes_stream(stream).boxed()
}

fn scope_from_challenge(challenge: &str) -> Option<String> {
    let start = challenge.to_ascii_lowercase().find("scope=")? + "scope=".len();
    let value = &challenge[start..];
    if let Some(quoted) = value.strip_prefix('"') {
        Some(quoted.split_once('"')?.0.to_owned())
    } else {
        let value = value.split([',', ';', ' ', '\t']).next()?;
        (!value.is_empty()).then(|| value.to_owned())
    }
}

impl StreamableHttpClient for BoundedHttpClient {
    type Error = rmcp_http::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, HttpError> {
        let request = self.0.post(uri.as_ref()).header(
            ACCEPT,
            format!("{EVENT_STREAM_MIME_TYPE}, {JSON_MIME_TYPE}"),
        );
        let mut request = add_headers(request, auth, headers)?;
        if let Some(session) = &session_id {
            request = request.header(HEADER_SESSION_ID, session.as_ref());
        }
        let response = request.json(&message).send().await.map_err(client_error)?;
        let status = response.status();
        if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
            && let Some(challenge) = response.headers().get(WWW_AUTHENTICATE)
        {
            let challenge = challenge
                .to_str()
                .map_err(|_| {
                    HttpError::UnexpectedServerResponse(
                        "invalid www-authenticate header value".into(),
                    )
                })?
                .to_owned();
            if status == StatusCode::UNAUTHORIZED {
                return Err(HttpError::AuthRequired(AuthRequiredError::new(challenge)));
            }
            let scope = scope_from_challenge(&challenge);
            return Err(HttpError::InsufficientScope(InsufficientScopeError::new(
                challenge, scope,
            )));
        }
        if matches!(status, StatusCode::ACCEPTED | StatusCode::NO_CONTENT) {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        if status == StatusCode::NOT_FOUND && session_id.is_some() {
            return Err(HttpError::SessionExpired);
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned());
        let session = response
            .headers()
            .get(HEADER_SESSION_ID)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);

        if !status.is_success() {
            let body = read_body(response, MAX_ERROR_BYTES).await?;
            if content_type
                .as_deref()
                .is_some_and(|ct| ct.starts_with(JSON_MIME_TYPE))
                && let Ok(message @ JsonRpcMessage::Error(_)) = serde_json::from_slice(&body)
            {
                return Ok(StreamableHttpPostResponse::Json(message, session));
            }
            return Err(HttpError::UnexpectedServerResponse(Cow::Owned(format!(
                "HTTP {status}: {}",
                String::from_utf8_lossy(&body)
            ))));
        }
        match content_type.as_deref() {
            Some(ct) if ct.starts_with(EVENT_STREAM_MIME_TYPE) => Ok(
                StreamableHttpPostResponse::Sse(event_stream(response.bytes_stream()), session),
            ),
            Some(ct) if ct.starts_with(JSON_MIME_TYPE) => {
                let body = read_body(response, MAX_JSON_BYTES).await?;
                match serde_json::from_slice::<ServerJsonRpcMessage>(&body) {
                    Ok(message) => Ok(StreamableHttpPostResponse::Json(message, session)),
                    Err(error) => {
                        tracing::warn!(
                            "could not parse JSON response as ServerJsonRpcMessage, treating as accepted: {error}"
                        );
                        Ok(StreamableHttpPostResponse::Accepted)
                    }
                }
            }
            _ => Err(HttpError::UnexpectedContentType(content_type)),
        }
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session: Arc<str>,
        last_event_id: Option<String>,
        auth: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<EventStream, HttpError> {
        let mut request = self
            .0
            .get(uri.as_ref())
            .header(
                ACCEPT,
                format!("{EVENT_STREAM_MIME_TYPE}, {JSON_MIME_TYPE}"),
            )
            .header(HEADER_SESSION_ID, session.as_ref());
        if let Some(id) = last_event_id {
            request = request.header(HEADER_LAST_EVENT_ID, id);
        }
        let response = add_headers(request, auth, headers)?
            .send()
            .await
            .map_err(client_error)?;
        if response.status() == StatusCode::METHOD_NOT_ALLOWED {
            return Err(HttpError::ServerDoesNotSupportSse);
        }
        let response = response.error_for_status().map_err(client_error)?;
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned());
        if !content_type.as_deref().is_some_and(|ct| {
            ct.starts_with(EVENT_STREAM_MIME_TYPE) || ct.starts_with(JSON_MIME_TYPE)
        }) {
            return Err(HttpError::UnexpectedContentType(content_type));
        }
        Ok(event_stream(response.bytes_stream()))
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session: Arc<str>,
        auth: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), HttpError> {
        let request = self
            .0
            .delete(uri.as_ref())
            .header(HEADER_SESSION_ID, session.as_ref());
        let response = add_headers(request, auth, headers)?
            .send()
            .await
            .map_err(client_error)?;
        if response.status() != StatusCode::METHOD_NOT_ALLOWED {
            response.error_for_status().map_err(client_error)?;
        }
        Ok(())
    }
}

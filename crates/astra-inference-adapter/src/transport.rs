//! One physical provider request. Admission, replay fencing, retries, semantic
//! tool admission, and durable custody belong to the caller, never this module.

use std::fmt;
use std::time::Duration;

use futures_util::{Stream, StreamExt};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::sse::{ParsedSseEvent, SseDecodeError, decode_provider_sse};
use crate::{ExactProviderRequest, ProviderProtocol};

/// Pooled connections are scoped to one immutable local transport configuration.
/// The builder carries explicit proxy/CA policy from the hosting boundary. No
/// environment is inspected here; Runner must pass a no-proxy or explicit-proxy
/// builder rather than inheriting whichever terminal happened to launch it.
#[derive(Clone)]
pub struct ProviderTransport {
    client: reqwest::Client,
}

impl fmt::Debug for ProviderTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProviderTransport(<local configuration>)")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreparationError {
    ClientConfiguration,
    InvalidRequest,
    InvalidHeader,
}

impl fmt::Display for PreparationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ClientConfiguration => "provider transport configuration is invalid",
            Self::InvalidRequest => "provider request configuration is invalid",
            Self::InvalidHeader => "provider authorization/header configuration is invalid",
        })
    }
}

impl std::error::Error for PreparationError {}

/// A consumed send capability, not a replayable RequestBuilder. Its body cannot
/// be replaced after preparation, and Debug never includes endpoint or headers.
pub struct PreparedHttpAttempt {
    request: reqwest::Request,
}

impl fmt::Debug for PreparedHttpAttempt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PreparedHttpAttempt(<exact request>)")
    }
}

impl PreparedHttpAttempt {
    /// Admission may consume time after preparation. Tighten the transport
    /// backstop immediately before send without changing any request bytes.
    pub fn constrain_timeout(mut self, remaining: Duration) -> Self {
        let timeout = self.request.timeout_mut();
        *timeout = Some(timeout.map_or(remaining, |current| current.min(remaining)));
        self
    }
}

/// Content-free transport classification; reqwest sources may contain private
/// URLs and are deliberately not retained as public error sources.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SendError {
    connect: bool,
    timeout: bool,
}

impl SendError {
    pub fn is_connect(&self) -> bool {
        self.connect
    }

    pub fn is_timeout(&self) -> bool {
        self.timeout
    }
}

impl From<reqwest::Error> for SendError {
    fn from(error: reqwest::Error) -> Self {
        Self {
            connect: error.is_connect(),
            timeout: error.is_timeout(),
        }
    }
}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.connect {
            "provider connection failed"
        } else if self.timeout {
            "provider request timed out"
        } else {
            "provider request transport failed"
        })
    }
}

impl std::error::Error for SendError {}

impl ProviderTransport {
    pub fn build(builder: reqwest::ClientBuilder) -> Result<Self, PreparationError> {
        builder
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map(|client| Self { client })
            .map_err(|_| PreparationError::ClientConfiguration)
    }

    pub fn prepare(
        &self,
        endpoint: &str,
        headers: HeaderMap,
        body: &ExactProviderRequest,
        timeout: Option<Duration>,
    ) -> Result<PreparedHttpAttempt, PreparationError> {
        if headers.contains_key("content-length") || headers.contains_key("transfer-encoding") {
            return Err(PreparationError::InvalidHeader);
        }
        // Check before reqwest extracts URL userinfo into implicit auth and
        // removes it from the resulting request URL.
        let endpoint =
            reqwest::Url::parse(endpoint).map_err(|_| PreparationError::InvalidRequest)?;
        if !matches!(endpoint.scheme(), "https" | "http")
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.fragment().is_some()
        {
            return Err(PreparationError::InvalidRequest);
        }
        let mut builder = self
            .client
            .post(endpoint)
            .headers(headers)
            .body(body.body());
        if let Some(timeout) = timeout {
            builder = builder.timeout(timeout);
        }
        let request = builder
            .build()
            .map_err(|_| PreparationError::InvalidRequest)?;
        Ok(PreparedHttpAttempt { request })
    }

    /// Execute exactly once. Server uses the response for its canonical cache,
    /// semantic-progress, and durable attempt instrumentation. Runner may use
    /// `execute` below to obtain bounded, cancelable normalized framing.
    pub async fn send_once(
        &self,
        attempt: PreparedHttpAttempt,
    ) -> Result<reqwest::Response, SendError> {
        self.client
            .execute(attempt.request)
            .await
            .map_err(Into::into)
    }
}

/// Canonical native authorization and explicit overrides. Invalid material fails
/// before admission/send; it must not silently drop an authentication header.
pub fn provider_headers<'a>(
    protocol: ProviderProtocol,
    api_key: &str,
    overrides: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Result<HeaderMap, PreparationError> {
    let mut headers = HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    if protocol == ProviderProtocol::AnthropicMessages {
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    }
    for (name, value) in overrides {
        if name.starts_with("__astra_") {
            continue;
        }
        let name =
            HeaderName::from_bytes(name.as_bytes()).map_err(|_| PreparationError::InvalidHeader)?;
        let mut value =
            HeaderValue::from_str(value).map_err(|_| PreparationError::InvalidHeader)?;
        value.set_sensitive(true);
        headers.insert(name, value);
    }
    let auth_name = if protocol == ProviderProtocol::AnthropicMessages {
        "x-api-key"
    } else {
        "authorization"
    };
    if !headers.contains_key(auth_name) && !api_key.is_empty() {
        let value = if protocol == ProviderProtocol::AnthropicMessages {
            api_key.to_owned()
        } else {
            format!("Bearer {api_key}")
        };
        let mut value =
            HeaderValue::from_str(&value).map_err(|_| PreparationError::InvalidHeader)?;
        value.set_sensitive(true);
        headers.insert(auth_name, value);
    }
    Ok(headers)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponseMode {
    Sse,
    Json,
}

#[derive(Clone, Copy, Debug)]
pub struct ExecutionLimits {
    pub event_bytes: usize,
    pub total_bytes: usize,
    pub events: usize,
}

impl Default for ExecutionLimits {
    fn default() -> Self {
        Self {
            event_bytes: crate::DEFAULT_SSE_EVENT_LIMIT_BYTES,
            total_bytes: 64 * 1024 * 1024,
            events: 16_384,
        }
    }
}

#[derive(Clone)]
pub enum ProviderEvent {
    Json(Value),
    Done,
    Eof,
}

impl fmt::Debug for ProviderEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Json(_) => "Json(<provider payload>)",
            Self::Done => "Done",
            Self::Eof => "Eof",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryEvidence {
    NotDispatched,
    MayHaveDispatched,
    ResponseHeaders,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionStatus {
    /// Framing completed, not authorization to continue or proof of semantic
    /// success. Consumers must distinguish `Done` from ordinary `Eof` events.
    Complete,
    Cancelled,
    Deadline,
    Transport,
    Protocol,
    Limit,
    ConsumerClosed,
    HttpStatus(u16),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecutionTerminal {
    pub status: ExecutionStatus,
    pub delivery: DeliveryEvidence,
    pub provider_bytes: u64,
    pub events_delivered: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponseReadErrorKind {
    Transport,
    Deadline,
    MalformedJson,
    Limit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResponseReadError {
    pub kind: ResponseReadErrorKind,
    pub provider_bytes: u64,
}

impl ResponseReadError {
    pub fn is_timeout(&self) -> bool {
        self.kind == ResponseReadErrorKind::Deadline
    }
}

impl fmt::Display for ResponseReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.kind {
            ResponseReadErrorKind::Transport => "provider response transport failed",
            ResponseReadErrorKind::Deadline => "provider response deadline elapsed",
            ResponseReadErrorKind::MalformedJson => "provider response contains malformed JSON",
            ResponseReadErrorKind::Limit => "provider response exceeds admitted byte limit",
        })
    }
}

impl std::error::Error for ResponseReadError {}

pub struct DecodedJsonResponse {
    pub value: Value,
    pub provider_bytes: u64,
}

/// Error diagnostics are intentionally much smaller than generated content.
pub const DEFAULT_ERROR_RESPONSE_LIMIT_BYTES: usize = 64 * 1024;

pub struct BoundedResponseBody {
    pub bytes: bytes::Bytes,
    pub provider_bytes: u64,
}

impl fmt::Debug for BoundedResponseBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoundedResponseBody")
            .field("provider_bytes", &self.provider_bytes)
            .finish()
    }
}

pub async fn read_response_body(
    response: reqwest::Response,
    limit: usize,
) -> Result<BoundedResponseBody, ResponseReadError> {
    read_body_stream(response.bytes_stream(), limit, classify_body_transport).await
}

fn classify_body_transport(error: reqwest::Error) -> ResponseReadErrorKind {
    if error.is_timeout() {
        ResponseReadErrorKind::Deadline
    } else {
        ResponseReadErrorKind::Transport
    }
}

impl fmt::Debug for DecodedJsonResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DecodedJsonResponse")
            .field("provider_bytes", &self.provider_bytes)
            .finish()
    }
}

/// Shared bounded nonstream reader. A failed decode retains delivery evidence,
/// but never the malformed body or a secret-bearing transport source.
pub async fn read_json_response(
    response: reqwest::Response,
    limit: usize,
) -> Result<DecodedJsonResponse, ResponseReadError> {
    read_json_stream(response.bytes_stream(), limit, classify_body_transport).await
}

async fn read_json_stream<E>(
    stream: impl Stream<Item = Result<bytes::Bytes, E>> + Unpin,
    limit: usize,
    classify: impl Fn(E) -> ResponseReadErrorKind,
) -> Result<DecodedJsonResponse, ResponseReadError> {
    let body = read_body_stream(stream, limit, classify).await?;
    let provider_bytes = body.provider_bytes;
    let value = serde_json::from_slice(&body.bytes).map_err(|_| ResponseReadError {
        kind: ResponseReadErrorKind::MalformedJson,
        provider_bytes,
    })?;
    Ok(DecodedJsonResponse {
        value,
        provider_bytes,
    })
}

async fn read_body_stream<E>(
    mut stream: impl Stream<Item = Result<bytes::Bytes, E>> + Unpin,
    limit: usize,
    classify: impl Fn(E) -> ResponseReadErrorKind,
) -> Result<BoundedResponseBody, ResponseReadError> {
    let mut body = Vec::new();
    let mut provider_bytes = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| ResponseReadError {
            kind: classify(error),
            provider_bytes,
        })?;
        provider_bytes = provider_bytes.saturating_add(chunk.len() as u64);
        if chunk.len() > limit.saturating_sub(body.len()) {
            return Err(ResponseReadError {
                kind: ResponseReadErrorKind::Limit,
                provider_bytes,
            });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(BoundedResponseBody {
        bytes: bytes::Bytes::from(body),
        provider_bytes,
    })
}

impl ProviderTransport {
    /// Backpressure is bounded by the caller's bounded channel plus one decoded
    /// event. Cancellation/deadline also interrupt a blocked consumer send.
    /// Terminal evidence is returned separately so a full queue cannot lose it.
    pub async fn execute(
        &self,
        attempt: PreparedHttpAttempt,
        mode: ResponseMode,
        limits: ExecutionLimits,
        deadline: Instant,
        cancellation: &CancellationToken,
        events: &mpsc::Sender<ProviderEvent>,
    ) -> ExecutionTerminal {
        let mut terminal = ExecutionTerminal {
            status: ExecutionStatus::Complete,
            delivery: DeliveryEvidence::NotDispatched,
            provider_bytes: 0,
            events_delivered: 0,
        };
        if cancellation.is_cancelled() {
            terminal.status = ExecutionStatus::Cancelled;
            return terminal;
        }
        if deadline <= Instant::now() {
            terminal.status = ExecutionStatus::Deadline;
            return terminal;
        }
        if limits.event_bytes == 0 || limits.total_bytes == 0 || limits.events == 0 {
            terminal.status = ExecutionStatus::Limit;
            return terminal;
        }
        let byte_count = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let work = async {
            terminal.delivery = DeliveryEvidence::MayHaveDispatched;
            let response = self.send_once(attempt).await.map_err(|error| {
                // reqwest's connect classification covers DNS, TCP and TLS
                // establishment failures before an HTTP request can reach the
                // provider. Only that positive evidence can restore safe
                // not-dispatched semantics; upload/header/body ambiguity stays
                // conservative as MayHaveDispatched.
                if error.is_connect() {
                    terminal.delivery = DeliveryEvidence::NotDispatched;
                }
                if error.is_timeout() {
                    ExecutionStatus::Deadline
                } else {
                    ExecutionStatus::Transport
                }
            })?;
            terminal.delivery = DeliveryEvidence::ResponseHeaders;
            if !response.status().is_success() {
                return Err(ExecutionStatus::HttpStatus(response.status().as_u16()));
            }
            let mut stream = response.bytes_stream();
            let count = byte_count.clone();
            let bounded = async_stream::stream! {
                let mut total = 0usize;
                while let Some(chunk) = stream.next().await {
                    match chunk {
                        Ok(chunk) => {
                            count.fetch_add(chunk.len() as u64, std::sync::atomic::Ordering::Relaxed);
                            if chunk.len() > limits.total_bytes.saturating_sub(total) {
                                let retained = limits.total_bytes.saturating_sub(total);
                                if retained > 0 { yield Ok(chunk.slice(..retained)); }
                                yield Err(ExecutionStatus::Limit);
                                return;
                            }
                            total += chunk.len();
                            yield Ok(chunk);
                        }
                        Err(_) => { yield Err(ExecutionStatus::Transport); return; }
                    }
                }
            };
            let bounded = Box::pin(bounded);
            async {
                match mode {
                    ResponseMode::Sse => {
                        let mut decoded =
                            Box::pin(decode_provider_sse(bounded, limits.event_bytes));
                        while let Some(event) = decoded.next().await {
                            let event = match event {
                                Ok(ParsedSseEvent::Data(value)) => ProviderEvent::Json(value),
                                Ok(ParsedSseEvent::Done) => ProviderEvent::Done,
                                Err(SseDecodeError::Transport) => {
                                    return Err(
                                        if byte_count.load(std::sync::atomic::Ordering::Relaxed)
                                            > limits.total_bytes as u64
                                        {
                                            ExecutionStatus::Limit
                                        } else {
                                            ExecutionStatus::Transport
                                        },
                                    );
                                }
                                Err(SseDecodeError::EventTooLarge { .. }) => {
                                    return Err(ExecutionStatus::Limit);
                                }
                                Err(_) => return Err(ExecutionStatus::Protocol),
                            };
                            deliver(events, event, &mut terminal.events_delivered, limits.events)
                                .await?;
                        }
                        // `[DONE]` is provider-level semantic framing; EOF is
                        // transport-level evidence. Retain both so durable
                        // consumers can distinguish a complete response from
                        // a stream that ended immediately after `[DONE]` was
                        // observed but before the body actually closed.
                        deliver(
                            events,
                            ProviderEvent::Eof,
                            &mut terminal.events_delivered,
                            limits.events,
                        )
                        .await?;
                    }
                    ResponseMode::Json => {
                        let decoded =
                            read_json_stream(bounded, limits.event_bytes, |status| match status {
                                ExecutionStatus::Limit => ResponseReadErrorKind::Limit,
                                ExecutionStatus::Deadline => ResponseReadErrorKind::Deadline,
                                _ => ResponseReadErrorKind::Transport,
                            })
                            .await
                            .map_err(|error| match error.kind {
                                ResponseReadErrorKind::Limit => ExecutionStatus::Limit,
                                ResponseReadErrorKind::Deadline => ExecutionStatus::Deadline,
                                ResponseReadErrorKind::MalformedJson => ExecutionStatus::Protocol,
                                ResponseReadErrorKind::Transport => ExecutionStatus::Transport,
                            })?;
                        deliver(
                            events,
                            ProviderEvent::Json(decoded.value),
                            &mut terminal.events_delivered,
                            limits.events,
                        )
                        .await?;
                        deliver(
                            events,
                            ProviderEvent::Eof,
                            &mut terminal.events_delivered,
                            limits.events,
                        )
                        .await?;
                    }
                }
                Ok(())
            }
            .await
        };
        let result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(ExecutionStatus::Cancelled),
            _ = tokio::time::sleep_until(deadline) => Err(ExecutionStatus::Deadline),
            result = work => result,
        };
        // Snapshot after the selected work future has completed or been
        // dropped. Cancellation must retain bytes already observed, including
        // when that future was blocked publishing its next event.
        terminal.provider_bytes = byte_count.load(std::sync::atomic::Ordering::Relaxed);
        if let Err(status) = result {
            terminal.status = status;
        }
        terminal
    }
}

async fn deliver(
    events: &mpsc::Sender<ProviderEvent>,
    event: ProviderEvent,
    delivered: &mut u64,
    limit: usize,
) -> Result<(), ExecutionStatus> {
    if *delivered >= limit as u64 {
        return Err(ExecutionStatus::Limit);
    }
    events
        .send(event)
        .await
        .map_err(|_| ExecutionStatus::ConsumerClosed)?;
    *delivered += 1;
    Ok(())
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;

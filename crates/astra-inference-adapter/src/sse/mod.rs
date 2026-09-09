//! Incremental provider framing. EOF remains distinct from provider completion;
//! semantic text/tool/usage accumulation is owned by the calling decoder.

pub mod blocks;
pub mod data_lines;

use blocks::SseBlankLineUtf8Buf;
use bytes::Bytes;
use data_lines::{LineAction, process_trimmed_line_strict};
use futures_util::{Stream, StreamExt};
use serde_json::Value;
use std::fmt;

#[derive(Clone, PartialEq)]
pub enum ParsedSseEvent {
    Data(Value),
    Done,
}

impl fmt::Debug for ParsedSseEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Data(_) => "Data(<provider payload>)",
            Self::Done => "Done",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseDecodeError {
    Transport,
    InvalidUtf8,
    InvalidJson,
    EventTooLarge { limit: usize },
}

impl fmt::Display for SseDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport => f.write_str("provider SSE transport failed"),
            Self::InvalidUtf8 => f.write_str("invalid UTF-8 in model SSE response"),
            Self::InvalidJson => f.write_str("invalid JSON in SSE data line"),
            Self::EventTooLarge { limit } => {
                write!(f, "provider SSE event exceeds {limit} byte limit")
            }
        }
    }
}

impl std::error::Error for SseDecodeError {}

/// Deliver events before reporting any subsequent malformed data or transport
/// failure, including errors in the same HTTP chunk. The retained event limit
/// bounds partial data; stream length is not a retained-memory limit.
/// This decoder never opens a request, retries, or treats socket EOF as Done.
pub fn decode_provider_sse<E: Send + 'static>(
    stream: impl Stream<Item = Result<Bytes, E>> + Unpin + Send + 'static,
    limit: usize,
) -> impl Stream<Item = Result<ParsedSseEvent, SseDecodeError>> + Send + 'static {
    async_stream::stream! {
        let mut input = SseBlankLineUtf8Buf::new();
        futures_util::pin_mut!(stream);
        while let Some(chunk) = stream.next().await {
            let bytes = match chunk {
                Ok(bytes) => bytes,
                Err(_) => { yield Err(SseDecodeError::Transport); return; }
            };
            let mut remaining = bytes.as_ref();
            while !remaining.is_empty() {
                // At most four separator bytes can trail an event at the limit.
                let available = limit.saturating_add(4).saturating_sub(input.buffered_bytes());
                if available == 0 { yield Err(SseDecodeError::EventTooLarge { limit }); return; }
                let take = available.min(remaining.len()).min(4096);
                input.append_bytes(&remaining[..take]);
                remaining = &remaining[take..];
                loop {
                    let block = match input.next_block() {
                        Ok(Some(block)) => block,
                        Ok(None) => break,
                        Err(_) => { yield Err(SseDecodeError::InvalidUtf8); return; }
                    };
                    if block.len() > limit { yield Err(SseDecodeError::EventTooLarge { limit }); return; }
                    for line in block.lines() {
                        match process_trimmed_line_strict(line.trim()) {
                            Ok(LineAction::Json(value)) => yield Ok(ParsedSseEvent::Data(value)),
                            Ok(LineAction::Done) => { yield Ok(ParsedSseEvent::Done); return; }
                            Ok(LineAction::Skip) => {}
                            Err(_) => { yield Err(SseDecodeError::InvalidJson); return; }
                        }
                    }
                }
            }
        }
        if input.buffered_bytes() > limit { yield Err(SseDecodeError::EventTooLarge { limit }); return; }
        let tail = match input.into_inner() {
            Ok(tail) => tail,
            Err(_) => { yield Err(SseDecodeError::InvalidUtf8); return; }
        };
        for line in tail.lines() {
            match process_trimmed_line_strict(line.trim()) {
                Ok(LineAction::Json(value)) => yield Ok(ParsedSseEvent::Data(value)),
                Ok(LineAction::Done) => { yield Ok(ParsedSseEvent::Done); return; }
                Ok(LineAction::Skip) => {}
                Err(_) => { yield Err(SseDecodeError::InvalidJson); return; }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;
    use serde_json::json;

    #[tokio::test]
    async fn chunk_boundaries_preserve_utf8_events_and_done() {
        let bytes = "data: {\"text\":\"你好\"}\r\n\r\ndata: [DONE]\n\n".as_bytes();
        for split in 0..=bytes.len() {
            let chunks = vec![
                Ok::<_, ()>(Bytes::copy_from_slice(&bytes[..split])),
                Ok(Bytes::copy_from_slice(&bytes[split..])),
            ];
            let events = decode_provider_sse(stream::iter(chunks), 128)
                .collect::<Vec<_>>()
                .await;
            assert_eq!(
                events,
                vec![
                    Ok(ParsedSseEvent::Data(json!({"text":"你好"}))),
                    Ok(ParsedSseEvent::Done)
                ]
            );
        }
    }

    #[tokio::test]
    async fn malformed_json_or_utf8_preserves_prior_events_in_same_chunk() {
        for suffix in [
            b"data: broken\n\n".as_slice(),
            b"data: \xff\n\n".as_slice(),
            b"data: broken".as_slice(),
        ] {
            let mut bytes = b"data: {\"text\":\"partial\"}\n\n".to_vec();
            bytes.extend_from_slice(suffix);
            let events =
                decode_provider_sse(stream::iter(vec![Ok::<_, ()>(Bytes::from(bytes))]), 128)
                    .collect::<Vec<_>>()
                    .await;
            assert_eq!(
                events[0],
                Ok(ParsedSseEvent::Data(json!({"text":"partial"})))
            );
            assert!(events[1].is_err());
            assert_eq!(events.len(), 2);
        }
    }

    #[tokio::test]
    async fn transport_failure_preserves_progress_and_never_polls_for_redispatch() {
        let chunks = stream::iter(vec![
            Ok(Bytes::from_static(b"data: {}\n\n")),
            Err("https://private:secret@canary.invalid/path"),
        ])
        .chain(stream::poll_fn(|_| {
            panic!("decoder must stop after transport failure")
        }));
        let events = decode_provider_sse(chunks, 64).collect::<Vec<_>>().await;
        assert_eq!(
            events,
            vec![
                Ok(ParsedSseEvent::Data(json!({}))),
                Err(SseDecodeError::Transport)
            ]
        );
        assert!(!format!("{events:?}").contains("secret"));
    }

    #[tokio::test]
    async fn incomplete_event_is_bounded_but_total_stream_can_exceed_limit() {
        let events = decode_provider_sse(
            stream::iter(vec![Ok::<_, ()>(Bytes::from_static(
                b"data: 12345678901234567890",
            ))]),
            8,
        )
        .collect::<Vec<_>>()
        .await;
        assert_eq!(
            events,
            vec![Err(SseDecodeError::EventTooLarge { limit: 8 })]
        );
        let events = decode_provider_sse(
            stream::iter(vec![Ok::<_, ()>(Bytes::from("data: {}\n\n".repeat(100)))]),
            8,
        )
        .collect::<Vec<_>>()
        .await;
        assert_eq!(events.len(), 100);
        assert!(
            events
                .iter()
                .all(|event| matches!(event, Ok(ParsedSseEvent::Data(_))))
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Ok(ParsedSseEvent::Done)))
        );
    }
}

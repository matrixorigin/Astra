//! Strict machine-readable observation stream for `--output-format stream-json`.
//!
//! This is deliberately separate from the UI-oriented [`super::stream_events_writer`].
//! Every record here is an exact protocol or lifecycle fact from one logical
//! `/chat/turn` exchange. The emitter writes each line immediately, so ordinary
//! CLI paths do not retain a second copy of streamed token payloads.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use astra_turn_core::chat_turn_sse_dispatch::ChatTurnSseAccum;
use serde_json::{Map, Value, json};

trait JsonLineSink: Send + Sync + std::fmt::Debug {
    fn write_line(&self, line: &str) -> Result<(), String>;
}

#[derive(Debug)]
struct StdoutJsonLineSink;

impl JsonLineSink for StdoutJsonLineSink {
    fn write_line(&self, line: &str) -> Result<(), String> {
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        writeln!(lock, "{line}")
            .and_then(|()| lock.flush())
            .map_err(|error| format!("failed to write stream-json output: {error}"))
    }
}

/// One CLI-local execution and its ordered logical `/chat/turn` exchanges.
#[derive(Debug)]
pub(crate) struct StreamJsonEmitter {
    sink: Arc<dyn JsonLineSink>,
    execution_id: String,
    user_query_event_id: String,
    next_request_ordinal: AtomicU32,
    latest_session_turn: AtomicU32,
}

impl StreamJsonEmitter {
    pub(crate) fn stdout(session_turn: u32) -> Result<Arc<Self>, String> {
        if session_turn == 0 {
            return Err("stream-json execution has invalid zero session_turn".to_string());
        }
        Ok(Arc::new(Self {
            sink: Arc::new(StdoutJsonLineSink),
            execution_id: format!("run-{}", uuid::Uuid::new_v4()),
            user_query_event_id: uuid::Uuid::now_v7().to_string(),
            next_request_ordinal: AtomicU32::new(0),
            latest_session_turn: AtomicU32::new(session_turn),
        }))
    }

    pub(crate) fn execution_id(&self) -> &str {
        &self.execution_id
    }

    pub(crate) fn user_query_event_id(&self) -> &str {
        &self.user_query_event_id
    }

    pub(crate) fn start_exchange(
        self: &Arc<Self>,
        session_id: Option<&str>,
        session_turn: u32,
        round_index: u32,
    ) -> Result<StreamJsonExchange, String> {
        if session_turn == 0 {
            return Err("stream-json exchange has invalid zero session_turn".to_string());
        }
        let session_id = session_id.map(validate_session_id).transpose()?;
        self.latest_session_turn
            .store(session_turn, Ordering::Release);
        let request_ordinal = self
            .next_request_ordinal
            .fetch_add(1, Ordering::AcqRel)
            .checked_add(1)
            .ok_or_else(|| "stream-json request ordinal overflow".to_string())?;
        let exchange_id = uuid::Uuid::now_v7().to_string();
        let identity = ExchangeIdentity {
            session_id,
            session_turn,
            round_index,
            exchange_id,
            request_ordinal,
        };
        self.emit_exchange_record("exchange_started", &identity, Map::new())?;
        Ok(StreamJsonExchange {
            emitter: Arc::clone(self),
            identity,
            event_seq: 0,
            finished: false,
        })
    }

    pub(crate) fn emit_result(
        &self,
        session_id: Option<&str>,
        result: Value,
    ) -> Result<(), String> {
        let session_turn = self.latest_session_turn.load(Ordering::Acquire);
        debug_assert_ne!(session_turn, 0);
        let session_id = session_id.map(validate_session_id).transpose()?;
        let mut record = self.execution_envelope(session_id, session_turn);
        record.insert("type".to_string(), json!("result"));
        record.insert("result".to_string(), result);
        self.emit(record)
    }

    fn emit_exchange_record(
        &self,
        record_type: &str,
        identity: &ExchangeIdentity,
        mut fields: Map<String, Value>,
    ) -> Result<(), String> {
        let mut record =
            self.execution_envelope(identity.session_id.clone(), identity.session_turn);
        record.insert("type".to_string(), json!(record_type));
        record.insert("exchange_id".to_string(), json!(identity.exchange_id));
        record.insert(
            "request_ordinal".to_string(),
            json!(identity.request_ordinal),
        );
        record.insert("round_index".to_string(), json!(identity.round_index));
        record.append(&mut fields);
        self.emit(record)
    }

    fn execution_envelope(
        &self,
        session_id: Option<String>,
        session_turn: u32,
    ) -> Map<String, Value> {
        Map::from_iter([
            ("schema".to_string(), json!("astra.cli.stream_json.v1")),
            ("execution_id".to_string(), json!(self.execution_id)),
            ("durable".to_string(), json!(false)),
            ("session_id".to_string(), json!(session_id)),
            ("session_turn".to_string(), json!(session_turn)),
            ("turn_chain_id".to_string(), json!(self.execution_id)),
            (
                "user_query_event_id".to_string(),
                json!(self.user_query_event_id),
            ),
        ])
    }

    fn emit(&self, record: Map<String, Value>) -> Result<(), String> {
        let line = serde_json::to_string(&Value::Object(record))
            .map_err(|error| format!("failed to serialize stream-json record: {error}"))?;
        self.sink.write_line(&line)
    }
}

#[derive(Debug)]
struct ExchangeIdentity {
    session_id: Option<String>,
    session_turn: u32,
    round_index: u32,
    exchange_id: String,
    request_ordinal: u32,
}

/// Mutable observer for exactly one logical `/chat/turn` response stream.
#[derive(Debug)]
pub(crate) struct StreamJsonExchange {
    emitter: Arc<StreamJsonEmitter>,
    identity: ExchangeIdentity,
    event_seq: u64,
    finished: bool,
}

impl StreamJsonExchange {
    pub(crate) fn accepted_event(&mut self, event: &Value) -> Result<(), String> {
        if self.finished {
            return Err("stream-json received an event after exchange_finished".to_string());
        }
        let event_type = event
            .as_object()
            .and_then(|object| object.get("type"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                "stream-json requires every SSE data event to be an object with a non-empty type"
                    .to_string()
            })?;
        validate_event_type(event_type)?;
        if event_type == "session_info" {
            if let Some(session_id) = event.get("session_id").and_then(Value::as_str) {
                let session_id = validate_session_id(session_id)?;
                if let Some(expected) = self.identity.session_id.as_deref()
                    && expected != session_id.as_str()
                {
                    return Err(
                        "stream-json session_info changed session_id within one exchange"
                            .to_string(),
                    );
                }
                self.identity.session_id = Some(session_id);
            }
            if let Some(bridge_run_id) = event.get("run_id").and_then(Value::as_str) {
                validate_bounded_identity("bridge_run_id", bridge_run_id, 64)?;
                if bridge_run_id != self.emitter.execution_id {
                    return Err(
                        "stream-json bridge_run_id differs from the execution turn_chain_id"
                            .to_string(),
                    );
                }
            }
        }
        self.event_seq = self
            .event_seq
            .checked_add(1)
            .ok_or_else(|| "stream-json event sequence overflow".to_string())?;
        let mut fields = Map::new();
        fields.insert("event_seq".to_string(), json!(self.event_seq));
        fields.insert("event".to_string(), event.clone());
        self.emitter
            .emit_exchange_record("sse_event", &self.identity, fields)
    }

    pub(crate) fn finish(&mut self, accum: &ChatTurnSseAccum) -> Result<(), String> {
        if self.finished {
            return Err("stream-json exchange_finished emitted more than once".to_string());
        }
        if !accum.stream_complete {
            return Err(
                "stream-json exchange cannot finish before the SSE [DONE] marker".to_string(),
            );
        }
        if let Some(session_id) = accum.session_id.as_deref() {
            let session_id = validate_session_id(session_id)?;
            if let Some(expected) = self.identity.session_id.as_deref()
                && expected != session_id.as_str()
            {
                return Err(
                    "stream-json accumulator changed session_id within one exchange".to_string(),
                );
            }
            self.identity.session_id = Some(session_id);
        }
        let bridge_run_id = accum.run_id.as_deref().ok_or_else(|| {
            "stream-json exchange reached [DONE] without a bridge run_id".to_string()
        })?;
        validate_bounded_identity("bridge_run_id", bridge_run_id, 64)?;
        if bridge_run_id != self.emitter.execution_id {
            return Err(
                "stream-json bridge_run_id differs from the execution turn_chain_id".to_string(),
            );
        }
        let usage = accum.has_usage.then(|| {
            json!({
                "fresh_input_tokens": accum.prompt_tokens,
                "cached_input_tokens": accum.cache_read_tokens,
                "cache_creation_tokens": accum.cache_creation_tokens,
                "output_tokens": accum.completion_tokens,
            })
        });
        let error = accum.error_message.as_ref().map(|message| {
            json!({
                "message": message,
                "code": accum.error_code.as_deref(),
                "kind": accum.error_kind.map(|kind| kind.as_str()),
                "metadata": accum.error_metadata.as_ref(),
            })
        });
        let mut fields = Map::new();
        fields.insert("event_count".to_string(), json!(self.event_seq));
        fields.insert("bridge_run_id".to_string(), json!(bridge_run_id));
        fields.insert("stream_complete".to_string(), json!(true));
        fields.insert("usage".to_string(), json!(usage));
        fields.insert(
            "context_manifest_trace".to_string(),
            json!(accum.context_manifest_trace.as_ref()),
        );
        fields.insert("compactions".to_string(), json!(accum.context_compactions));
        fields.insert("error".to_string(), json!(error));
        self.emitter
            .emit_exchange_record("exchange_finished", &self.identity, fields)?;
        self.finished = true;
        Ok(())
    }
}

fn validate_session_id(value: &str) -> Result<String, String> {
    if value != value.trim() {
        return Err("stream-json session_id must not contain surrounding whitespace".to_string());
    }
    astra_core::session_id::validate(value)
        .map_err(|error| format!("invalid stream-json session_id: {error}"))?;
    Ok(value.to_string())
}

fn validate_bounded_identity(name: &str, value: &str, max_bytes: usize) -> Result<(), String> {
    if value.is_empty()
        || value != value.trim()
        || value.len() > max_bytes
        || value.chars().any(char::is_control)
    {
        return Err(format!("invalid stream-json {name}"));
    }
    Ok(())
}

fn validate_event_type(value: &str) -> Result<(), String> {
    const MAX_EVENT_TYPE_BYTES: usize = 128;
    validate_bounded_identity("SSE event type", value, MAX_EVENT_TYPE_BYTES)
}

#[cfg(test)]
mod tests {
    use super::{JsonLineSink, StreamJsonEmitter};
    use astra_turn_core::chat_turn_sse_dispatch::ChatTurnSseAccum;
    use serde_json::{Value, json};
    use std::sync::atomic::AtomicU32;
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Default)]
    struct MemorySink {
        lines: Mutex<Vec<String>>,
    }

    impl JsonLineSink for MemorySink {
        fn write_line(&self, line: &str) -> Result<(), String> {
            self.lines.lock().unwrap().push(line.to_string());
            Ok(())
        }
    }

    fn emitter(sink: Arc<MemorySink>) -> Arc<StreamJsonEmitter> {
        Arc::new(StreamJsonEmitter {
            sink,
            execution_id: "run-execution".to_string(),
            user_query_event_id: "query-event".to_string(),
            next_request_ordinal: AtomicU32::new(0),
            latest_session_turn: AtomicU32::new(0),
        })
    }

    fn parsed_lines(sink: &MemorySink) -> Vec<Value> {
        sink.lines
            .lock()
            .unwrap()
            .iter()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn emits_strict_ordered_exchange_and_result_records() {
        let sink = Arc::new(MemorySink::default());
        let emitter = emitter(Arc::clone(&sink));
        let mut exchange = emitter.start_exchange(None, 3, 0).unwrap();
        exchange
            .accepted_event(&json!({
                "type": "session_info",
                "session_id": "session-canonical",
                "run_id": "run-execution"
            }))
            .unwrap();
        exchange
            .accepted_event(&json!({
                "type": "usage",
                "input_tokens": 7,
                "output_tokens": 2
            }))
            .unwrap();
        let accum = ChatTurnSseAccum {
            session_id: Some("session-canonical".to_string()),
            run_id: Some("run-execution".to_string()),
            prompt_tokens: 7,
            completion_tokens: 2,
            has_usage: true,
            stream_complete: true,
            ..Default::default()
        };
        exchange.finish(&accum).unwrap();
        emitter
            .emit_result(
                Some("session-canonical"),
                json!({"success": true, "text": "ok"}),
            )
            .unwrap();

        let records = parsed_lines(&sink);
        assert_eq!(
            records
                .iter()
                .map(|record| record["type"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec![
                "exchange_started",
                "sse_event",
                "sse_event",
                "exchange_finished",
                "result"
            ]
        );
        assert_eq!(records[1]["event_seq"], 1);
        assert_eq!(records[2]["event_seq"], 2);
        assert_eq!(records[3]["event_count"], 2);
        assert_eq!(records[3]["bridge_run_id"], "run-execution");
        assert_eq!(records[3]["session_id"], "session-canonical");
        assert_eq!(records[4]["execution_id"], "run-execution");
        assert_eq!(records[4]["turn_chain_id"], "run-execution");
        assert_eq!(records[4]["durable"], false);
        assert!(
            records
                .iter()
                .all(|record| record["schema"] == "astra.cli.stream_json.v1")
        );
        assert!(records.iter().all(|record| record["session_turn"] == 3));
    }

    #[test]
    fn malformed_typed_event_and_finish_before_done_fail_closed() {
        let sink = Arc::new(MemorySink::default());
        let emitter = emitter(Arc::clone(&sink));
        let mut exchange = emitter.start_exchange(None, 1, 0).unwrap();

        assert!(
            exchange
                .accepted_event(&json!({"content": "missing type"}))
                .is_err()
        );
        assert!(
            exchange
                .accepted_event(&json!({"type": "   ", "content": "empty type"}))
                .is_err()
        );
        assert!(exchange.finish(&ChatTurnSseAccum::default()).is_err());
        assert_eq!(parsed_lines(&sink).len(), 1);
    }

    #[test]
    fn dropped_request_observer_never_fabricates_exchange_finished() {
        let sink = Arc::new(MemorySink::default());
        let emitter = emitter(Arc::clone(&sink));
        let exchange = emitter.start_exchange(Some("session"), 1, 0).unwrap();
        drop(exchange);

        let records = parsed_lines(&sink);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["type"], "exchange_started");
    }

    #[test]
    fn identity_and_event_type_boundaries_reject_non_exact_values() {
        let sink = Arc::new(MemorySink::default());
        let emitter = emitter(Arc::clone(&sink));
        assert!(emitter.start_exchange(Some(" session "), 1, 0).is_err());
        assert!(
            emitter
                .start_exchange(Some("session\ncontrol"), 1, 0)
                .is_err()
        );

        let mut exchange = emitter.start_exchange(Some("session"), 1, 0).unwrap();
        assert!(exchange.accepted_event(&json!({"type": " usage"})).is_err());
        assert!(
            exchange
                .accepted_event(&json!({"type": "usage\n"}))
                .is_err()
        );
        assert_eq!(parsed_lines(&sink).len(), 1);
    }

    #[test]
    fn session_and_bridge_identity_changes_fail_closed() {
        let sink = Arc::new(MemorySink::default());
        let emitter = emitter(Arc::clone(&sink));
        let mut exchange = emitter.start_exchange(Some("session-a"), 1, 0).unwrap();
        assert!(
            exchange
                .accepted_event(&json!({
                    "type": "session_info",
                    "session_id": "session-b",
                    "run_id": "run-execution"
                }))
                .is_err()
        );
        assert!(
            exchange
                .accepted_event(&json!({
                    "type": "session_info",
                    "session_id": "session-a",
                    "run_id": "different-run"
                }))
                .is_err()
        );
        assert!(
            exchange
                .finish(&ChatTurnSseAccum {
                    session_id: Some("session-a".to_string()),
                    run_id: Some("different-run".to_string()),
                    stream_complete: true,
                    ..Default::default()
                })
                .is_err()
        );
        assert_eq!(parsed_lines(&sink).len(), 1);
    }

    #[test]
    fn preserves_all_exchange_ordinals_without_latest_only_collapse() {
        let sink = Arc::new(MemorySink::default());
        let emitter = emitter(Arc::clone(&sink));
        for round in 0..3 {
            let mut exchange = emitter.start_exchange(Some("session"), 2, round).unwrap();
            exchange
                .accepted_event(&json!({"type": "context_meta", "round": round}))
                .unwrap();
            exchange
                .finish(&ChatTurnSseAccum {
                    run_id: Some("run-execution".to_string()),
                    stream_complete: true,
                    ..Default::default()
                })
                .unwrap();
        }

        let records = parsed_lines(&sink);
        let started = records
            .iter()
            .filter(|record| record["type"] == "exchange_started")
            .collect::<Vec<_>>();
        assert_eq!(started.len(), 3);
        assert_eq!(started[0]["request_ordinal"], 1);
        assert_eq!(started[1]["request_ordinal"], 2);
        assert_eq!(started[2]["request_ordinal"], 3);
        assert_eq!(started[2]["round_index"], 2);
    }
}

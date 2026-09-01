//! Structured JSONL event writer for `--stream-events` mode.
//!
//! When enabled, reads from the `StreamEventTx` channel and writes one JSON
//! object per line to an explicitly named machine-event file. stderr remains
//! diagnostic output and can never contaminate this protocol.

use crate::cli::chat_stream::{StreamEvent, StreamEventRx};
use std::io::Write;
use std::path::Path;

const THINKING_CHUNK_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
const THINKING_CHUNK_FLUSH_BYTES: usize = 8 * 1024;

pub(crate) fn spawn_file_writer(
    mut rx: StreamEventRx,
    path: &Path,
) -> std::io::Result<tokio::task::JoinHandle<std::io::Result<()>>> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "stream-event parent directory does not exist",
        ));
    }
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    Ok(tokio::spawn(async move {
        let mut writer = std::io::BufWriter::new(file);
        let mut write_error = None;
        write_stream_events(&mut rx, |json, flush_after| {
            if let Err(error) = writeln!(writer, "{json}") {
                write_error = Some(error);
                return false;
            }
            if flush_after && let Err(error) = writer.flush() {
                write_error = Some(error);
                return false;
            }
            true
        })
        .await;
        if let Some(error) = write_error {
            return Err(error);
        }
        writer.flush()
    }))
}

async fn write_stream_events(rx: &mut StreamEventRx, mut emit: impl FnMut(String, bool) -> bool) {
    let mut thinking = String::new();
    let mut flush = tokio::time::interval_at(
        tokio::time::Instant::now() + THINKING_CHUNK_FLUSH_INTERVAL,
        THINKING_CHUNK_FLUSH_INTERVAL,
    );
    flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            event = rx.recv() => {
                let Some(event) = event else {
                    if !thinking.is_empty() {
                        let _ = emit(event_to_json(&StreamEvent::ThinkingChunk(std::mem::take(&mut thinking))), false);
                    }
                    break;
                };
                match event {
                    StreamEvent::ThinkingChunk(chunk) => {
                        thinking.push_str(&chunk);
                        if thinking.len() >= THINKING_CHUNK_FLUSH_BYTES {
                            if !emit(event_to_json(&StreamEvent::ThinkingChunk(std::mem::take(&mut thinking))), false) {
                                break;
                            }
                        }
                    }
                    event => {
                        // Preserve causal order at every structural/lifecycle
                        // boundary while collapsing only adjacent preview
                        // fragments.
                        if !thinking.is_empty() {
                            if !emit(event_to_json(&StreamEvent::ThinkingChunk(std::mem::take(&mut thinking))), false) {
                                break;
                            }
                        }
                        // Make lifecycle/structural evidence promptly visible
                        // to timeout observers without turning token streaming
                        // into one filesystem flush per model delta.
                        let flush_after = !matches!(event, StreamEvent::Token(_));
                        if !emit(event_to_json(&event), flush_after) {
                            break;
                        }
                    }
                }
            }
            _ = flush.tick(), if !thinking.is_empty() => {
                if !emit(event_to_json(&StreamEvent::ThinkingChunk(std::mem::take(&mut thinking))), false) {
                    break;
                }
            }
        }
    }
}

fn event_to_json(event: &StreamEvent) -> String {
    let value = match event {
        StreamEvent::SessionBound(session_id) => {
            serde_json::json!({"type": "session_bound", "session_id": session_id})
        }
        StreamEvent::RunBound(run_id) => {
            serde_json::json!({"type": "run_bound", "run_id": run_id})
        }
        StreamEvent::ContextWindowPolicy {
            raw_window_tokens,
            usable_input_tokens,
        } => serde_json::json!({
            "type": "context_window_policy",
            "raw_window_tokens": raw_window_tokens,
            "usable_input_tokens": usable_input_tokens,
        }),
        StreamEvent::ContextWindowEstimated(usage) => {
            serde_json::json!({
                "type": "context_window_estimated",
                "used_tokens": usage.used_tokens,
                "limit_tokens": usage.limit_tokens,
                "source": usage.source,
            })
        }
        StreamEvent::ContextSystemPromptTokens(tokens) => {
            serde_json::json!({"type": "context_system_prompt_tokens", "tokens": tokens})
        }
        StreamEvent::ContextWindowMeasured(tokens) => {
            serde_json::json!({"type": "context_window_measured", "used_tokens": tokens})
        }
        StreamEvent::RequestTokenUsage(usage) => serde_json::json!({
            "type": "request_token_usage",
            "fresh_input_tokens": usage.fresh_input_tokens,
            "cache_read_tokens": usage.cache_read_tokens,
            "cache_creation_tokens": usage.cache_creation_tokens,
            "output_tokens": usage.output_tokens,
        }),
        StreamEvent::RuntimeFeedback(frame) => serde_json::json!({
            "type": "runtime_feedback",
            "runtime_feedback": frame,
        }),
        StreamEvent::Token(text) => {
            serde_json::json!({"type": "token", "text": text})
        }
        StreamEvent::Thinking(active) => {
            serde_json::json!({"type": "thinking", "active": active})
        }
        StreamEvent::ThinkingChunk(text) => {
            serde_json::json!({"type": "thinking_chunk", "text": text})
        }
        StreamEvent::ToolStarted {
            name,
            description,
            tool_use_id,
            parent_tool_use_id,
        } => {
            serde_json::json!({
                "type": "tool_started",
                "name": name,
                "description": description,
                "tool_use_id": tool_use_id,
                "parent_tool_use_id": parent_tool_use_id,
            })
        }
        StreamEvent::AgentControlStarted {
            action,
            label,
            tool_use_id,
            agent_id,
            fanout_slot,
            fanout_title,
        } => {
            serde_json::json!({
                "type": "agent_control_started",
                "action": action,
                "label": label,
                "tool_use_id": tool_use_id,
                "agent_id": agent_id,
                "fanout_slot": fanout_slot,
                "fanout_title": fanout_title,
            })
        }
        StreamEvent::ToolCompleted {
            name,
            description,
            status,
            duration_ms,
            output_summary,
            output,
            tool_use_id,
            parent_tool_use_id,
        } => {
            serde_json::json!({
                "type": "tool_completed",
                "name": name,
                "description": description,
                "status": status,
                "duration_ms": duration_ms,
                "output_summary": output_summary,
                "output": output,
                "tool_use_id": tool_use_id,
                "parent_tool_use_id": parent_tool_use_id,
            })
        }
        StreamEvent::WorkTaskBoardUpdate(update) => serde_json::json!({
            "type": astra_server_types::WORK_TASK_BOARD_UPDATE_EVENT_TYPE,
            "task_board_update": update,
        }),
        StreamEvent::AskUserPrompted { request_id, prompt } => {
            serde_json::json!({
                "type": "ask_user_prompted",
                "request_id": request_id,
                "prompt": prompt,
            })
        }
        StreamEvent::AskUserResolved {
            request_id,
            resolution,
        } => {
            serde_json::json!({
                "type": "ask_user_resolved",
                "request_id": request_id,
                "resolution": resolution,
            })
        }
        StreamEvent::AgentControlCompleted {
            action,
            label,
            status,
            duration_ms,
            output,
            tool_use_id,
            agent_id,
        } => {
            serde_json::json!({
                "type": "agent_control_completed",
                "action": action,
                "label": label,
                "status": status,
                "duration_ms": duration_ms,
                "output": output,
                "tool_use_id": tool_use_id,
                "agent_id": agent_id,
            })
        }
        StreamEvent::WaitingForModel => {
            serde_json::json!({"type": "waiting_for_model"})
        }
        StreamEvent::ModelResponding => {
            serde_json::json!({"type": "model_responding"})
        }
        StreamEvent::AssistantOutputSettled => {
            serde_json::json!({"type": "assistant_output_settled"})
        }
        StreamEvent::StatusLine(text) => {
            serde_json::json!({"type": "status", "text": text})
        }
        StreamEvent::UserIntentApplied {
            intent_id,
            delivery,
            status,
            event_index,
            content,
        } => {
            serde_json::json!({
                "type": "user_intent_applied",
                "intent_id": intent_id,
                "delivery": delivery,
                "status": status,
                "event_index": event_index,
                "content": content,
            })
        }
        StreamEvent::UserIntentReturned {
            intent_id,
            delivery,
            status,
            event_index,
            content,
        } => {
            serde_json::json!({
                "type": "user_intent_returned",
                "intent_id": intent_id,
                "delivery": delivery,
                "status": status,
                "event_index": event_index,
                "content": content,
            })
        }
        StreamEvent::AgentLive(event) => {
            serde_json::json!({
                "type": "agent_live",
                "event": event,
            })
        }
        StreamEvent::AgentLiveGap(gap) => {
            serde_json::json!({
                "type": "agent_live_gap",
                "gap": gap,
            })
        }
        StreamEvent::AgentCommunication(event) => {
            let mut value = serde_json::to_value(event).unwrap_or_default();
            if let Some(object) = value.as_object_mut() {
                object.insert(
                    "type".to_string(),
                    serde_json::Value::String("agent_communication".to_string()),
                );
            }
            value
        }
        StreamEvent::ToolOutput { name, lines, bytes } => {
            serde_json::json!({
                "type": "tool_output",
                "name": name,
                "lines": lines,
                "bytes": bytes,
            })
        }
        StreamEvent::PermissionAutoApproved { tool, reason } => {
            serde_json::json!({"type": "permission_auto_approved", "tool": tool, "reason": reason})
        }
        StreamEvent::ExplainText(text) => {
            serde_json::json!({"type": "explain", "format": "dag", "text": text})
        }
        StreamEvent::ExplainReport(_) | StreamEvent::VerdictReport(_) => {
            serde_json::json!({"type": "ignored"})
        }
        StreamEvent::Compaction(event) => {
            serde_json::json!({
                "type": "compaction",
                "kind": event.kind,
                "pressure": event.pressure,
                "tokens_freed": event.tokens_freed,
                "tokens_before": event.tokens_before,
                "tokens_after": event.tokens_after,
                "max_tokens": event.max_tokens,
                "summary": event.summary,
            })
        }
    };
    serde_json::to_string(&value).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{event_to_json, spawn_file_writer, write_stream_events};
    use crate::cli::chat_stream::StreamEvent;
    use astra_turn_core::compaction_types::{CompactionEvent, CompactionKind};

    #[tokio::test]
    async fn adjacent_thinking_chunks_are_coalesced_before_structural_events() {
        let (tx, mut rx) = crate::cli::chat_stream::stream_event_channel();
        tx.send(StreamEvent::ThinkingChunk("a".into()))
            .await
            .unwrap();
        tx.send(StreamEvent::ThinkingChunk("b".into()))
            .await
            .unwrap();
        tx.send(StreamEvent::Thinking(false)).await.unwrap();
        drop(tx);

        let mut lines = Vec::new();
        write_stream_events(&mut rx, |line, _| {
            lines.push(line);
            true
        })
        .await;

        assert_eq!(lines.len(), 2, "{lines:?}");
        let chunk: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        let boundary: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();
        assert_eq!(chunk["type"], "thinking_chunk");
        assert_eq!(chunk["text"], "ab");
        assert_eq!(boundary["type"], "thinking");
        assert_eq!(boundary["active"], false);
    }

    #[tokio::test]
    async fn closed_optional_event_sink_stops_only_its_writer() {
        let (tx, mut rx) = crate::cli::chat_stream::stream_event_channel();
        tx.send(StreamEvent::Token("first".into())).await.unwrap();
        tx.send(StreamEvent::Token("second".into())).await.unwrap();
        drop(tx);

        let mut writes = 0;
        write_stream_events(&mut rx, |_, _| {
            writes += 1;
            false
        })
        .await;

        assert_eq!(writes, 1, "a closed optional sink must not be retried");
    }

    fn runtime_feedback_frame() -> astra_turn_core::context_feedback::RuntimeFeedbackFrame {
        serde_json::from_value(serde_json::json!({
            "schema_version": 4,
            "identity": {
                "session_id": "session-live",
                "run_id": "run-live",
                "agent_id": "orchestrator",
                "model_id": "deepseek-v4-flash",
                "topology": "cli_server"
            },
            "progress": {
                "session_turn": 1,
                "agentic_round_index": 2,
                "llm_rounds_completed": 3,
                "slice_round_limit": 60,
                "slice_rounds_remaining": 57
            },
            "context": {"compaction_tier": "normal"},
            "was_truncated": false,
            "policy_feedback": {"state": "not_evaluated"}
        }))
        .unwrap()
    }

    #[test]
    fn token_event_serializes() {
        let json = event_to_json(&StreamEvent::Token("hello".into()));
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "token");
        assert_eq!(v["text"], "hello");
    }

    #[test]
    fn session_bound_event_serializes_server_identity() {
        let json = event_to_json(&StreamEvent::SessionBound(
            "550e8400-e29b-41d4-a716-446655440000".into(),
        ));
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["type"], "session_bound");
        assert_eq!(value["session_id"], "550e8400-e29b-41d4-a716-446655440000");
    }

    #[test]
    fn thinking_event_serializes() {
        let json = event_to_json(&StreamEvent::Thinking(true));
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "thinking");
        assert_eq!(v["active"], true);
    }

    #[test]
    fn context_window_events_preserve_measurement_provenance() {
        let estimated = event_to_json(&StreamEvent::ContextWindowEstimated(
            astra_turn_types::ContextWindowUsage::estimated(12_000, 160_000),
        ));
        let estimated: serde_json::Value = serde_json::from_str(&estimated).unwrap();
        assert_eq!(estimated["type"], "context_window_estimated");
        assert_eq!(estimated["used_tokens"], 12_000);
        assert_eq!(estimated["source"], "estimated");

        let measured = event_to_json(&StreamEvent::ContextWindowMeasured(18_000));
        let measured: serde_json::Value = serde_json::from_str(&measured).unwrap();
        assert_eq!(measured["type"], "context_window_measured");
        assert_eq!(measured["used_tokens"], 18_000);

        let lanes = event_to_json(&StreamEvent::RequestTokenUsage(
            astra_turn_types::RequestTokenUsage {
                fresh_input_tokens: 200,
                cache_read_tokens: 800,
                cache_creation_tokens: 100,
                output_tokens: 50,
            },
        ));
        let lanes: serde_json::Value = serde_json::from_str(&lanes).unwrap();
        assert_eq!(lanes["type"], "request_token_usage");
        assert_eq!(lanes["fresh_input_tokens"], 200);
        assert_eq!(lanes["cache_read_tokens"], 800);
        assert_eq!(lanes["cache_creation_tokens"], 100);
        assert_eq!(lanes["output_tokens"], 50);
    }

    #[test]
    fn runtime_feedback_event_preserves_the_canonical_frame() {
        let frame = runtime_feedback_frame();
        let json = event_to_json(&StreamEvent::RuntimeFeedback(Box::new(frame.clone())));
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["type"], "runtime_feedback");
        assert_eq!(value["runtime_feedback"], serde_json::json!(frame));
    }

    #[test]
    fn explain_text_event_serializes() {
        let json = event_to_json(&StreamEvent::ExplainText(
            "Explain Analyze DAG — turn-1".into(),
        ));
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "explain");
        assert_eq!(v["format"], "dag");
        assert_eq!(v["text"], "Explain Analyze DAG — turn-1");
    }

    #[test]
    fn tool_started_event_serializes() {
        let json = event_to_json(&StreamEvent::ToolStarted {
            name: "bash".into(),
            description: "ls -la".into(),
            tool_use_id: "tu_01H000000000000000000001".into(),
            parent_tool_use_id: None,
        });
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "tool_started");
        assert_eq!(v["name"], "bash");
        assert_eq!(v["tool_use_id"], "tu_01H000000000000000000001");
        // Parent field is present as null when absent so consumers can rely on the key
        assert!(v.get("parent_tool_use_id").is_some());
        assert!(v["parent_tool_use_id"].is_null());
    }

    #[test]
    fn tool_started_event_with_parent_serializes() {
        let json = event_to_json(&StreamEvent::ToolStarted {
            name: "bash".into(),
            description: "ls -la".into(),
            tool_use_id: "tu_child".into(),
            parent_tool_use_id: Some("tu_parent".into()),
        });
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["parent_tool_use_id"], "tu_parent");
    }

    #[test]
    fn agent_control_started_serializes_fanout_slot() {
        use astra_turn_core::orchestration_fanout_group::AgentFanoutSlotIdentity;

        let json = event_to_json(&StreamEvent::AgentControlStarted {
            action: "spawn".into(),
            label: "auth reviewer".into(),
            tool_use_id: "spawn-tu".into(),
            agent_id: None,
            fanout_slot: Some(
                AgentFanoutSlotIdentity::new("review-1", 3, 1, Some("storage".into())).unwrap(),
            ),
            fanout_title: Some("review fanout".into()),
        });
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "agent_control_started");
        assert_eq!(v["fanout_title"], "review fanout");
        assert_eq!(v["fanout_slot"]["group_id"], "review-1");
        assert_eq!(v["fanout_slot"]["target_count"], 3);
        assert_eq!(v["fanout_slot"]["slot_index"], 1);
        assert_eq!(v["fanout_slot"]["slot_id"], "storage");
    }

    #[test]
    fn tool_completed_event_serializes() {
        let json = event_to_json(&StreamEvent::ToolCompleted {
            name: "read_file".into(),
            description: "src/main.rs".into(),
            status: "completed".into(),
            duration_ms: 42,
            output_summary: Some("150 lines".into()),
            output: None,
            tool_use_id: "tu_01H000000000000000000002".into(),
            parent_tool_use_id: None,
        });
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "tool_completed");
        assert_eq!(v["duration_ms"], 42);
        assert_eq!(v["output_summary"], "150 lines");
        assert_eq!(v["tool_use_id"], "tu_01H000000000000000000002");
    }

    #[test]
    fn ask_user_prompted_event_serializes() {
        let json = event_to_json(&StreamEvent::AskUserPrompted {
            request_id: "ask_1".into(),
            prompt: serde_json::json!({
                "prompt": {"question_count": 2, "headers": ["Scope", "Notes"]},
                "source": "tui"
            }),
        });
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "ask_user_prompted");
        assert_eq!(v["request_id"], "ask_1");
        assert_eq!(v["prompt"]["prompt"]["question_count"], 2);
        assert_eq!(v["prompt"]["source"], "tui");
    }

    #[test]
    fn ask_user_resolved_event_serializes() {
        let json = event_to_json(&StreamEvent::AskUserResolved {
            request_id: "ask_1".into(),
            resolution: serde_json::json!({
                "response": {"outcome": "submitted", "answered_question_count": 2}
            }),
        });
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "ask_user_resolved");
        assert_eq!(v["request_id"], "ask_1");
        assert_eq!(v["resolution"]["response"]["outcome"], "submitted");
    }

    #[test]
    fn all_event_types_produce_valid_json() {
        let events = vec![
            StreamEvent::Token("x".into()),
            StreamEvent::Thinking(false),
            StreamEvent::ThinkingChunk("hmm".into()),
            StreamEvent::ToolStarted {
                name: "t".into(),
                description: "d".into(),
                tool_use_id: "tu_test".into(),
                parent_tool_use_id: None,
            },
            StreamEvent::AgentControlStarted {
                action: "spawn".into(),
                label: "agent".into(),
                tool_use_id: "tu_agent".into(),
                agent_id: None,
                fanout_slot: None,
                fanout_title: None,
            },
            StreamEvent::ToolCompleted {
                name: "t".into(),
                description: "d".into(),
                status: "completed".into(),
                duration_ms: 0,
                output_summary: None,
                output: None,
                tool_use_id: "tu_test".into(),
                parent_tool_use_id: None,
            },
            StreamEvent::AskUserPrompted {
                request_id: "ask_evt".into(),
                prompt: serde_json::json!({"prompt": {"question_count": 1}}),
            },
            StreamEvent::AskUserResolved {
                request_id: "ask_evt".into(),
                resolution: serde_json::json!({"response": {"outcome": "submitted"}}),
            },
            StreamEvent::WaitingForModel,
            StreamEvent::ModelResponding,
            StreamEvent::StatusLine("done".into()),
            StreamEvent::UserIntentApplied {
                intent_id: "input-1".into(),
                delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                status: astra_turn_types::UserIntentStatus::Applied,
                event_index: 1,
                content: "guide".into(),
            },
            StreamEvent::Compaction(CompactionEvent {
                kind: CompactionKind::Microcompact,
                pressure: 0.5,
                tokens_freed: 1000,
                tokens_before: 30000,
                tokens_after: 29000,
                max_tokens: 64000,
                messages_removed: 2,
                messages_after: 18,
                layer_descriptions: vec![],
                summary: "test".into(),
            }),
        ];
        for event in &events {
            let json = event_to_json(event);
            let v: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert!(v["type"].is_string(), "missing type field: {json}");
        }
    }

    #[tokio::test]
    async fn file_writer_drains_only_valid_json_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let (tx, rx) = crate::cli::chat_stream::stream_event_channel();
        let handle = spawn_file_writer(rx, &path).unwrap();
        tx.send(StreamEvent::ToolStarted {
            name: "bash".into(),
            description: "dangerous command warning remains on stderr".into(),
            tool_use_id: "call-1".into(),
            parent_tool_use_id: None,
        })
        .await
        .unwrap();
        tx.send(StreamEvent::ToolCompleted {
            name: "bash".into(),
            description: "dangerous command was allowed by permissive mode".into(),
            status: "completed".into(),
            duration_ms: 1,
            output_summary: Some("done".into()),
            output: None,
            tool_use_id: "call-1".into(),
            parent_tool_use_id: None,
        })
        .await
        .unwrap();
        drop(tx);
        handle.await.unwrap().unwrap();

        let contents = std::fs::read_to_string(path).unwrap();
        let lines = contents.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        for line in &lines {
            let event: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(event.get("type").is_some());
        }
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(lines[0]).unwrap()["tool_use_id"],
            "call-1"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(lines[1]).unwrap()["tool_use_id"],
            "call-1"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(dir.path().join("events.jsonl"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[tokio::test]
    async fn file_writer_refuses_existing_destination() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        std::fs::write(&path, "user-owned").unwrap();
        let (_tx, rx) = crate::cli::chat_stream::stream_event_channel();

        let error = spawn_file_writer(rx, &path).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(path).unwrap(), "user-owned");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn file_writer_refuses_symlink_destination() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let path = dir.path().join("events.jsonl");
        std::fs::write(&target, "user-owned").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        let (_tx, rx) = crate::cli::chat_stream::stream_event_channel();

        let error = spawn_file_writer(rx, &path).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(target).unwrap(), "user-owned");
    }

    #[test]
    fn token_with_chinese_and_emoji() {
        let json = event_to_json(&StreamEvent::Token("你好世界 🌍".into()));
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["text"], "你好世界 🌍");
    }

    #[test]
    fn token_empty_string() {
        let json = event_to_json(&StreamEvent::Token("".into()));
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["text"], "");
    }

    #[test]
    fn tool_name_with_special_chars() {
        let json = event_to_json(&StreamEvent::ToolStarted {
            name: "mcp__memoria/search".into(),
            description: "query=\"test\"".into(),
            tool_use_id: "tu_test".into(),
            parent_tool_use_id: None,
        });
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["name"], "mcp__memoria/search");
        assert!(v["description"].as_str().unwrap().contains("query="));
    }

    #[test]
    fn tool_completed_null_summary() {
        let json = event_to_json(&StreamEvent::ToolCompleted {
            name: "bash".into(),
            description: "ls".into(),
            status: "completed".into(),
            duration_ms: 5,
            output_summary: None,
            output: None,
            tool_use_id: "tu_test".into(),
            parent_tool_use_id: None,
        });
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(v["output_summary"].is_null());
    }

    #[test]
    fn status_line_preserves_content() {
        let json = event_to_json(&StreamEvent::StatusLine("⚠️ warning: 1 clippy lint".into()));
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "status");
        assert!(v["text"].as_str().unwrap().contains("clippy"));
    }

    #[test]
    fn user_intent_applied_serializes_typed_identity() {
        let json = event_to_json(&StreamEvent::UserIntentApplied {
            intent_id: "input-7".into(),
            delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
            status: astra_turn_types::UserIntentStatus::Applied,
            event_index: 7,
            content: "change course".into(),
        });
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["type"], "user_intent_applied");
        assert_eq!(value["intent_id"], "input-7");
        assert_eq!(value["delivery"], "guide_current_run");
        assert_eq!(value["status"], "applied");
        assert_eq!(value["event_index"], 7);
        assert_eq!(value["content"], "change course");
    }

    #[test]
    fn compaction_event_serializes() {
        let event = CompactionEvent {
            kind: CompactionKind::Microcompact,
            pressure: 0.82,
            tokens_freed: 4500,
            tokens_before: 32000,
            tokens_after: 27500,
            max_tokens: 64000,
            messages_removed: 5,
            messages_after: 25,
            layer_descriptions: vec!["microcompact: ~4500 tokens".into()],
            summary: "freed 4500 tokens".into(),
        };
        let json = event_to_json(&StreamEvent::Compaction(event));
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "compaction");
        assert_eq!(v["kind"], "microcompact");
        assert_eq!(v["pressure"], 0.82);
        assert_eq!(v["tokens_freed"], 4500);
        assert_eq!(v["tokens_before"], 32000);
        assert_eq!(v["tokens_after"], 27500);
        assert_eq!(v["max_tokens"], 64000);
        assert_eq!(v["summary"], "freed 4500 tokens");
    }

    #[test]
    fn agent_live_event_uses_stable_structured_json() {
        let json = event_to_json(&StreamEvent::AgentLive(
            astra_turn_core::agent_live_event::AgentLiveEvent {
                run_id: "test-run".into(),
                agent_id: "reviewer@abc12345".into(),
                kind: astra_turn_core::agent_live_event::AgentLiveEventKind::ToolCompleted {
                    name: "bash".into(),
                    description: "cargo test".into(),
                    status: "completed".into(),
                    duration_ms: 42,
                    output_summary: Some("ok".into()),
                    output: None,
                    tool_use_id: "tool-1".into(),
                },
            },
        ));
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "agent_live");
        assert_eq!(v["event"]["agent_id"], "reviewer@abc12345");
        assert_eq!(v["event"]["kind"]["type"], "tool_completed");
        assert_eq!(v["event"]["kind"]["tool_use_id"], "tool-1");
    }

    #[test]
    fn agent_live_thinking_delta_is_jsonl_safe() {
        let json = event_to_json(&StreamEvent::AgentLive(
            astra_turn_core::agent_live_event::AgentLiveEvent {
                run_id: "child-run".into(),
                agent_id: "reviewer@child-run".into(),
                kind: astra_turn_core::agent_live_event::AgentLiveEventKind::ThinkingDelta(
                    "checking the client boundary".into(),
                ),
            },
        ));
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid JSONL event");
        assert_eq!(value["type"], "agent_live");
        assert_eq!(value["event"]["kind"]["type"], "thinking_delta");
        assert_eq!(
            value["event"]["kind"]["text"],
            "checking the client boundary"
        );
    }
}

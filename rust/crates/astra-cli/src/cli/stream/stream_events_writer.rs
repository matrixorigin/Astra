//! Structured JSONL event writer for `--stream-events` mode.
//!
//! When enabled, reads from the `StreamEventTx` channel and writes one
//! JSON object per line to stderr.  Gateway reads these lines to drive
//! progressive WeChat delivery (text deltas, tool status, thinking state).

use crate::cli::chat_stream::StreamEvent;
use tokio::sync::mpsc;

pub(crate) fn spawn_stderr_writer(
    mut rx: mpsc::UnboundedReceiver<StreamEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            let json = event_to_json(&event);
            eprintln!("{json}");
        }
    })
}

fn event_to_json(event: &StreamEvent) -> String {
    let value = match event {
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
        } => {
            serde_json::json!({
                "type": "agent_control_started",
                "action": action,
                "label": label,
                "tool_use_id": tool_use_id,
                "agent_id": agent_id,
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
        StreamEvent::StatusLine(text) => {
            serde_json::json!({"type": "status", "text": text})
        }
        StreamEvent::AgentLive(event) => {
            serde_json::json!({
                "type": "agent_live",
                "event": event,
            })
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
    use super::*;
    use astra_turn_core::compaction_types::{CompactionEvent, CompactionKind};

    #[test]
    fn token_event_serializes() {
        let json = event_to_json(&StreamEvent::Token("hello".into()));
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "token");
        assert_eq!(v["text"], "hello");
    }

    #[test]
    fn thinking_event_serializes() {
        let json = event_to_json(&StreamEvent::Thinking(true));
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "thinking");
        assert_eq!(v["active"], true);
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
    fn tool_completed_event_serializes() {
        let json = event_to_json(&StreamEvent::ToolCompleted {
            name: "read_file".into(),
            description: "src/main.rs".into(),
            status: "ok".into(),
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
            StreamEvent::ToolCompleted {
                name: "t".into(),
                description: "d".into(),
                status: "ok".into(),
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
            StreamEvent::Compaction(CompactionEvent {
                kind: CompactionKind::Microcompact,
                pressure: 0.5,
                tokens_freed: 1000,
                tokens_before: 30000,
                tokens_after: 29000,
                max_tokens: 64000,
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
    async fn spawn_writer_drains_channel() {
        let (tx, rx) = mpsc::unbounded_channel();
        let handle = spawn_stderr_writer(rx);
        tx.send(StreamEvent::Token("a".into())).unwrap();
        tx.send(StreamEvent::Token("b".into())).unwrap();
        drop(tx);
        handle.await.unwrap();
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
            status: "ok".into(),
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
    fn compaction_event_serializes() {
        let event = CompactionEvent {
            kind: CompactionKind::Microcompact,
            pressure: 0.82,
            tokens_freed: 4500,
            tokens_before: 32000,
            tokens_after: 27500,
            max_tokens: 64000,
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
                agent_id: "reviewer@abc12345".into(),
                kind: astra_turn_core::agent_live_event::AgentLiveEventKind::ToolCompleted {
                    name: "bash".into(),
                    description: "cargo test".into(),
                    status: "success".into(),
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
}

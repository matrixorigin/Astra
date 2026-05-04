//! Structured JSONL event writer for `--stream-events` mode.
//!
//! When enabled, reads from the `StreamEventTx` channel and writes one
//! JSON object per line to stderr.  Gateway reads these lines to drive
//! progressive WeChat delivery (text deltas, tool status, thinking state).

use super::chat_stream::StreamEvent;
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
        StreamEvent::ToolStarted { name, description } => {
            serde_json::json!({"type": "tool_started", "name": name, "description": description})
        }
        StreamEvent::ToolCompleted {
            name,
            description,
            status,
            duration_ms,
            output_summary,
        } => {
            serde_json::json!({
                "type": "tool_completed",
                "name": name,
                "description": description,
                "status": status,
                "duration_ms": duration_ms,
                "output_summary": output_summary,
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
    };
    serde_json::to_string(&value).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn tool_started_event_serializes() {
        let json = event_to_json(&StreamEvent::ToolStarted {
            name: "bash".into(),
            description: "ls -la".into(),
        });
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "tool_started");
        assert_eq!(v["name"], "bash");
    }

    #[test]
    fn tool_completed_event_serializes() {
        let json = event_to_json(&StreamEvent::ToolCompleted {
            name: "read_file".into(),
            description: "src/main.rs".into(),
            status: "ok".into(),
            duration_ms: 42,
            output_summary: Some("150 lines".into()),
        });
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "tool_completed");
        assert_eq!(v["duration_ms"], 42);
        assert_eq!(v["output_summary"], "150 lines");
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
            },
            StreamEvent::ToolCompleted {
                name: "t".into(),
                description: "d".into(),
                status: "ok".into(),
                duration_ms: 0,
                output_summary: None,
            },
            StreamEvent::WaitingForModel,
            StreamEvent::ModelResponding,
            StreamEvent::StatusLine("done".into()),
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
}

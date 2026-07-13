//! Canonical journal hydration for a resumed TUI session.

use std::collections::HashSet;

use super::ChatWidget;
use crate::tui::turn_event::{ToolStatus, TurnEvent};

/// Read the root's canonical append-only transcript lane away from the UI
/// worker, then rebuild the compact-chat scrollback. Presentation-only system
/// rows and old TUI JSONL projections intentionally do not participate: the
/// durable transcript browser and resumed chat now share one source.
pub(crate) async fn load(session_id: impl Into<String>) -> ChatWidget {
    let session_id = session_id.into();
    let id_for_read = session_id.clone();
    let journal_dir_override = astra_services::session_journal::current_journal_dir_override();
    let events = tokio::task::spawn_blocking(move || {
        let _scope = journal_dir_override
            .as_deref()
            .map(astra_services::session_journal::JournalDirGuard::new);
        astra_services::session_journal::read_journal_append_order(&id_for_read)
    })
    .await
    .ok()
    .and_then(Result::ok)
    .unwrap_or_default();
    let mut widget = ChatWidget::new(session_id);
    widget.replay(canonical_root_turn_events(events));
    widget
}

/// Convert only typed, root-owned transcript payloads into the compact chat
/// cells. The full-fidelity browser consumes the same payload directly; this
/// conversion is intentionally a lossy visual projection, never a second
/// durable record or a prompt-history reconstruction.
fn canonical_root_turn_events(
    events: Vec<astra_services::session_journal::JournalEvent>,
) -> Vec<TurnEvent> {
    let mut seen_source_ids = HashSet::new();
    let mut out = Vec::new();
    for event in events {
        let Some(item) = event.transcript_item else {
            continue;
        };
        if item.agent_id != "root" {
            continue;
        }
        let source_id = if item.source_event_id.trim().is_empty() {
            format!("{}:{}", item.run_id, item.item_seq)
        } else {
            item.source_event_id
        };
        if !seen_source_ids.insert(source_id) {
            continue;
        }
        let message = item.message;
        let role = message.get("role").and_then(serde_json::Value::as_str);
        match role {
            Some("user") => {
                if let Some(content) = message.get("content").and_then(serde_json::Value::as_str)
                    && !content.is_empty()
                {
                    out.push(TurnEvent::User {
                        ts: Some(event.ts),
                        text: content.to_string(),
                    });
                }
            }
            Some("assistant") => {
                if let Some(reasoning) = message
                    .get("reasoning_content")
                    .or_else(|| message.get("reasoning"))
                    .and_then(serde_json::Value::as_str)
                    .filter(|text| !text.is_empty())
                {
                    out.push(TurnEvent::Thinking {
                        ts: Some(event.ts.clone()),
                        text: reasoning.to_string(),
                        duration_ms: message
                            .get("reasoning_duration_ms")
                            .and_then(serde_json::Value::as_u64),
                    });
                }
                if let Some(content) = message
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                    .filter(|text| !text.is_empty())
                {
                    out.push(TurnEvent::Assistant {
                        ts: Some(event.ts),
                        markdown: content.to_string(),
                    });
                }
            }
            Some("tool") => {
                let content = message
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                out.push(TurnEvent::Tool {
                    ts: Some(event.ts),
                    name: message
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("tool")
                        .to_string(),
                    description: String::new(),
                    status: match message.get("status").and_then(serde_json::Value::as_str) {
                        Some("uncertain") => ToolStatus::Uncertain,
                        Some("failed" | "error") => ToolStatus::Failed,
                        _ => ToolStatus::Success,
                    },
                    duration_ms: message
                        .get("duration_ms")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or_default(),
                    output_summary: (!content.is_empty()).then_some(content.clone()),
                    output: (!content.is_empty()).then_some(content),
                });
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::session_journal::{self, JournalDirGuard, JournalEvent};

    #[tokio::test]
    #[serial_test::serial]
    async fn canonical_root_journal_hydrates_chat_without_child_or_retry_duplicates() {
        session_journal::set_journal_content_redact_override(Some(false));
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let session_id = "sess_resume_canonical";
        let event = |run_id: &str, agent_id: &str, seq, message| {
            JournalEvent::transcript_item(session_id, run_id, agent_id, seq, &message)
                .expect("valid transcript message")
        };
        let journal = session_journal::JournalWriter::new(session_id).unwrap();
        journal
            .append_bulk(&[
                event(
                    "root-run",
                    "root",
                    1,
                    serde_json::json!({"role": "user", "content": "what's up"}),
                ),
                event(
                    "root-run",
                    "root",
                    2,
                    serde_json::json!({
                        "role": "assistant",
                        "reasoning_content": "inspect the state",
                        "content": "all good",
                    }),
                ),
                event(
                    "child-run",
                    "reviewer",
                    1,
                    serde_json::json!({"role": "assistant", "content": "child-only"}),
                ),
                event(
                    "root-run",
                    "root",
                    2,
                    serde_json::json!({"role": "assistant", "content": "retry must not win"}),
                ),
            ])
            .unwrap();

        let widget = load(session_id).await;

        assert_eq!(widget.session_id(), session_id);
        assert_eq!(widget.history().len(), 3);
        let text = widget
            .history()
            .iter()
            .flat_map(|cell| cell.display_lines(100))
            .flat_map(|line| line.spans)
            .map(|span| span.content.into_owned())
            .collect::<String>();
        assert!(text.contains("what's up"), "{text}");
        assert!(text.contains("all good"), "{text}");
        assert!(!text.contains("child-only"), "{text}");
        assert!(!text.contains("retry must not win"), "{text}");
        session_journal::set_journal_content_redact_override(None);
    }
}

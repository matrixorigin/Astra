//! Unified multi-agent trace timeline.
//!
//! Reads a flat `Vec<JournalEvent>` from a session journal and produces
//! a chronologically-ordered timeline where parent LLM rounds and child
//! agent rounds are interleaved by timestamp, rendered as a tree with
//! agent-id prefixes.
//!
//! The renderer is a pure function with no I/O — it takes already-parsed
//! events and produces a `String`. Callers (`/trace` slash command,
//! `astra analyze`) are responsible for reading the journal file.

use std::fmt::Write;

use serde::{Deserialize, Serialize};

/// A single entry in the unified timeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineEntry {
    pub ts: String,
    pub agent_id: Option<String>,
    pub kind: TimelineEntryKind,
}

/// Discriminated entry kinds that the timeline tracks.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TimelineEntryKind {
    LlmRound {
        turn: u32,
        round: u32,
        tokens_in: u64,
        tokens_out: u64,
        duration_ms: u64,
        tools: Vec<String>,
        finish_reason: String,
    },
    AgentSpawned {
        child_agent_id: String,
        agent_type: String,
        description: String,
    },
    AgentCompleted {
        child_agent_id: String,
        turns: u32,
        tools: u32,
        tokens: u64,
        duration_ms: u64,
    },
    AgentFailed {
        child_agent_id: String,
        error: String,
        duration_ms: u64,
    },
    TurnError {
        error: String,
    },
    Interruption {
        kind: String,
        stall_signal: Option<String>,
    },
}

/// Assembled unified timeline (ordered by timestamp).
#[derive(Debug, Clone, Default)]
pub struct UnifiedTimeline {
    pub entries: Vec<TimelineEntry>,
}

/// Build a `UnifiedTimeline` from raw journal event JSON values.
///
/// Each event is a serde_json::Value matching the `JournalEvent` shape.
/// We extract only the timeline-relevant events and skip the rest.
pub fn build_timeline(events: &[serde_json::Value]) -> UnifiedTimeline {
    let mut entries = Vec::new();

    for evt in events {
        let ts = evt.get("ts").and_then(|v| v.as_str()).unwrap_or("");
        let event_type = evt.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let metadata = evt.get("metadata");
        let agent_id = metadata
            .and_then(|m| m.get("agent_id"))
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(|| {
                evt.get("agent_id")
                    .and_then(|v| v.as_str())
                    .map(String::from)
            });

        match event_type {
            "llm_round" => {
                let turn = evt.get("turn").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                let round = evt
                    .get("round")
                    .and_then(|v| v.as_u64())
                    .or_else(|| {
                        metadata
                            .and_then(|m| m.get("round"))
                            .and_then(|v| v.as_u64())
                    })
                    .unwrap_or(0) as u32;
                let tokens_in = evt.get("tokens_in").and_then(|v| v.as_u64()).unwrap_or(0);
                let tokens_out = evt.get("tokens_out").and_then(|v| v.as_u64()).unwrap_or(0);
                let duration_ms = evt.get("duration_ms").and_then(|v| v.as_u64()).unwrap_or(0);
                let tools: Vec<String> = metadata
                    .and_then(|m| m.get("tool_call_names"))
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let finish_reason = metadata
                    .and_then(|m| m.get("finish_reason"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                entries.push(TimelineEntry {
                    ts: ts.to_string(),
                    agent_id,
                    kind: TimelineEntryKind::LlmRound {
                        turn,
                        round,
                        tokens_in,
                        tokens_out,
                        duration_ms,
                        tools,
                        finish_reason,
                    },
                });
            }
            "agent_spawned" | "AgentSpawned" | "DelegationSubRunStarted" => {
                let child_agent_id = metadata
                    .and_then(|m| m.get("agent_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let agent_type = metadata
                    .and_then(|m| m.get("agent_type"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let description = metadata
                    .and_then(|m| m.get("description"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                entries.push(TimelineEntry {
                    ts: ts.to_string(),
                    agent_id: None,
                    kind: TimelineEntryKind::AgentSpawned {
                        child_agent_id,
                        agent_type,
                        description,
                    },
                });
            }
            "AgentTerminated" | "agent_terminated" | "DelegationSubRunCompleted" => {
                let child_agent_id = metadata
                    .and_then(|m| m.get("agent_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let status = metadata
                    .and_then(|m| m.get("status"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let turns = metadata
                    .and_then(|m| m.get("turns_completed"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;
                let tools = metadata
                    .and_then(|m| m.get("tool_calls"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;
                let tokens = metadata
                    .and_then(|m| m.get("prompt_tokens"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0)
                    + metadata
                        .and_then(|m| m.get("completion_tokens"))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                let duration_ms = metadata
                    .and_then(|m| m.get("duration_ms"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);

                if status == "failed" || status == "error" {
                    let error = metadata
                        .and_then(|m| m.get("error"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown error")
                        .to_string();
                    entries.push(TimelineEntry {
                        ts: ts.to_string(),
                        agent_id: None,
                        kind: TimelineEntryKind::AgentFailed {
                            child_agent_id,
                            error,
                            duration_ms,
                        },
                    });
                } else {
                    entries.push(TimelineEntry {
                        ts: ts.to_string(),
                        agent_id: None,
                        kind: TimelineEntryKind::AgentCompleted {
                            child_agent_id,
                            turns,
                            tools,
                            tokens,
                            duration_ms,
                        },
                    });
                }
            }
            "turn_error" => {
                let error = evt
                    .get("error")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if !error.is_empty() {
                    entries.push(TimelineEntry {
                        ts: ts.to_string(),
                        agent_id: None,
                        kind: TimelineEntryKind::TurnError { error },
                    });
                }
            }
            "interruption_recorded" => {
                let kind = metadata
                    .and_then(|m| m.get("interruption"))
                    .and_then(|m| m.get("kind"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                let stall_signal = metadata
                    .and_then(|m| m.get("interruption"))
                    .and_then(|m| m.get("stall_signal"))
                    .and_then(|v| v.as_str())
                    .map(String::from);
                entries.push(TimelineEntry {
                    ts: ts.to_string(),
                    agent_id: None,
                    kind: TimelineEntryKind::Interruption { kind, stall_signal },
                });
            }
            _ => {}
        }
    }

    UnifiedTimeline { entries }
}

fn format_tokens(n: u64) -> String {
    if n >= 1000 {
        format!("{:.1}K", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

fn format_duration(ms: u64) -> String {
    if ms >= 1000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{ms}ms")
    }
}

/// Render the timeline to a human-readable tree string.
///
/// Parent rounds appear at the left margin; child rounds are indented
/// under the parent round that spawned them (by timestamp ordering).
pub fn render_timeline(timeline: &UnifiedTimeline, limit: usize) -> String {
    let entries = if timeline.entries.len() > limit {
        &timeline.entries[timeline.entries.len() - limit..]
    } else {
        &timeline.entries
    };

    let mut out = String::new();
    let _ = writeln!(
        out,
        "## Unified Timeline ({} events{})",
        timeline.entries.len(),
        if timeline.entries.len() > limit {
            format!(", showing last {limit}")
        } else {
            String::new()
        }
    );

    for entry in entries {
        match &entry.kind {
            TimelineEntryKind::LlmRound {
                turn,
                round,
                tokens_in,
                tokens_out,
                duration_ms,
                tools,
                finish_reason,
            } => {
                let prefix = if let Some(ref aid) = entry.agent_id {
                    let short = aid.split('@').next().unwrap_or(aid);
                    format!("  ├─ [{short}]")
                } else {
                    format!("T{turn} r{round}")
                };
                let tool_str = if tools.is_empty() {
                    String::new()
                } else {
                    format!(" {}", tools.join(","))
                };
                let _ = writeln!(
                    out,
                    "{prefix}{tool_str} ({} in, {} out, {}) finish={finish_reason}",
                    format_tokens(*tokens_in),
                    format_tokens(*tokens_out),
                    format_duration(*duration_ms),
                );
            }
            TimelineEntryKind::AgentSpawned {
                child_agent_id,
                agent_type,
                description,
            } => {
                let short = child_agent_id.split('@').next().unwrap_or(child_agent_id);
                let desc_preview: String = description.chars().take(60).collect();
                let _ = writeln!(out, "  ┌─ [{short}] spawned ({agent_type}) {desc_preview}");
            }
            TimelineEntryKind::AgentCompleted {
                child_agent_id,
                turns,
                tools,
                tokens,
                duration_ms,
            } => {
                let short = child_agent_id.split('@').next().unwrap_or(child_agent_id);
                let _ = writeln!(
                    out,
                    "  └─ [{short}] ✓ DONE ({turns}r, {tools} tools, {} tok, {})",
                    format_tokens(*tokens),
                    format_duration(*duration_ms),
                );
            }
            TimelineEntryKind::AgentFailed {
                child_agent_id,
                error,
                duration_ms,
            } => {
                let short = child_agent_id.split('@').next().unwrap_or(child_agent_id);
                let err_preview: String = error.chars().take(60).collect();
                let _ = writeln!(
                    out,
                    "  └─ [{short}] ✗ FAILED ({}) {err_preview}",
                    format_duration(*duration_ms),
                );
            }
            TimelineEntryKind::TurnError { error } => {
                let err_preview: String = error.chars().take(80).collect();
                let _ = writeln!(out, "  ⚠ ERROR: {err_preview}");
            }
            TimelineEntryKind::Interruption { kind, stall_signal } => {
                let signal = stall_signal
                    .as_deref()
                    .map(|s| format!(" ({s})"))
                    .unwrap_or_default();
                let _ = writeln!(out, "  ⛔ INTERRUPTED: {kind}{signal}");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn build_timeline_extracts_llm_rounds() {
        let events = vec![json!({
            "type": "llm_round",
            "ts": "2026-05-09T10:00:00Z",
            "turn": 1,
            "round": 0,
            "tokens_in": 5000,
            "tokens_out": 200,
            "duration_ms": 3000,
            "metadata": {
                "tool_call_names": ["bash", "read_file"],
                "finish_reason": "tool_calls"
            }
        })];
        let tl = build_timeline(&events);
        assert_eq!(tl.entries.len(), 1);
        assert!(tl.entries[0].agent_id.is_none());
        match &tl.entries[0].kind {
            TimelineEntryKind::LlmRound {
                turn, round, tools, ..
            } => {
                assert_eq!(*turn, 1);
                assert_eq!(*round, 0);
                assert_eq!(tools, &["bash", "read_file"]);
            }
            _ => panic!("expected LlmRound"),
        }
    }

    #[test]
    fn build_timeline_extracts_child_round_with_agent_id() {
        let events = vec![json!({
            "type": "llm_round",
            "ts": "2026-05-09T10:01:00Z",
            "turn": 1,
            "round": 0,
            "tokens_in": 1200,
            "tokens_out": 80,
            "duration_ms": 2100,
            "metadata": {
                "agent_id": "reviewer-correctness@abc123",
                "tool_call_names": ["grep"],
                "finish_reason": "tool_calls"
            }
        })];
        let tl = build_timeline(&events);
        assert_eq!(
            tl.entries[0].agent_id.as_deref(),
            Some("reviewer-correctness@abc123")
        );
    }

    #[test]
    fn build_timeline_extracts_agent_terminated() {
        let events = vec![json!({
            "type": "AgentTerminated",
            "ts": "2026-05-09T10:02:00Z",
            "metadata": {
                "agent_id": "reviewer-tests@xyz",
                "status": "completed",
                "turns_completed": 3,
                "tool_calls": 6,
                "prompt_tokens": 2000,
                "completion_tokens": 500,
                "duration_ms": 12000
            }
        })];
        let tl = build_timeline(&events);
        match &tl.entries[0].kind {
            TimelineEntryKind::AgentCompleted {
                child_agent_id,
                turns,
                tools,
                tokens,
                duration_ms,
            } => {
                assert_eq!(child_agent_id, "reviewer-tests@xyz");
                assert_eq!(*turns, 3);
                assert_eq!(*tools, 6);
                assert_eq!(*tokens, 2500);
                assert_eq!(*duration_ms, 12000);
            }
            _ => panic!("expected AgentCompleted"),
        }
    }

    #[test]
    fn build_timeline_failed_agent() {
        let events = vec![json!({
            "type": "AgentTerminated",
            "ts": "2026-05-09T10:02:30Z",
            "metadata": {
                "agent_id": "reviewer-tests@xyz",
                "status": "failed",
                "error": "timeout after 120s",
                "duration_ms": 120000
            }
        })];
        let tl = build_timeline(&events);
        match &tl.entries[0].kind {
            TimelineEntryKind::AgentFailed {
                child_agent_id,
                error,
                ..
            } => {
                assert_eq!(child_agent_id, "reviewer-tests@xyz");
                assert!(error.contains("timeout"));
            }
            _ => panic!("expected AgentFailed"),
        }
    }

    #[test]
    fn render_timeline_interleaves_parent_and_child() {
        let events = vec![
            json!({"type": "llm_round", "ts": "2026-05-09T10:00:00Z", "turn": 3, "round": 4, "tokens_in": 42000, "tokens_out": 380, "duration_ms": 5200, "metadata": {"tool_call_names": ["spawn_agent","spawn_agent","spawn_agent"], "finish_reason": "tool_calls"}}),
            json!({"type": "llm_round", "ts": "2026-05-09T10:00:01Z", "turn": 1, "round": 0, "tokens_in": 1100, "tokens_out": 89, "duration_ms": 3200, "metadata": {"agent_id": "reviewer-correctness@a1", "tool_call_names": ["bash","read_file"], "finish_reason": "tool_calls"}}),
            json!({"type": "llm_round", "ts": "2026-05-09T10:00:02Z", "turn": 1, "round": 0, "tokens_in": 1000, "tokens_out": 200, "duration_ms": 5100, "metadata": {"agent_id": "reviewer-architecture@a2", "tool_call_names": ["git","read_file"], "finish_reason": "tool_calls"}}),
            json!({"type": "AgentTerminated", "ts": "2026-05-09T10:00:10Z", "metadata": {"agent_id": "reviewer-correctness@a1", "status": "completed", "turns_completed": 3, "tool_calls": 6, "prompt_tokens": 2000, "completion_tokens": 100, "duration_ms": 12100}}),
            json!({"type": "AgentTerminated", "ts": "2026-05-09T10:00:12Z", "metadata": {"agent_id": "reviewer-architecture@a2", "status": "completed", "turns_completed": 2, "tool_calls": 4, "prompt_tokens": 1100, "completion_tokens": 100, "duration_ms": 8300}}),
            json!({"type": "llm_round", "ts": "2026-05-09T10:00:15Z", "turn": 3, "round": 5, "tokens_in": 42500, "tokens_out": 1200, "duration_ms": 22800, "metadata": {"tool_call_names": [], "finish_reason": "stop"}}),
        ];
        let tl = build_timeline(&events);
        let rendered = render_timeline(&tl, 50);
        assert!(
            rendered.contains("T3 r4"),
            "parent round must appear: {rendered}"
        );
        assert!(
            rendered.contains("[reviewer-correctness]"),
            "child round: {rendered}"
        );
        assert!(
            rendered.contains("[reviewer-architecture]"),
            "child round: {rendered}"
        );
        assert!(rendered.contains("✓ DONE"), "completion marker: {rendered}");
        assert!(rendered.contains("T3 r5"), "final parent round: {rendered}");
        assert!(rendered.contains("finish=stop"), "stop reason: {rendered}");
    }

    #[test]
    fn render_timeline_truncates_to_limit() {
        let events: Vec<serde_json::Value> = (0..20)
            .map(|i| json!({"type": "llm_round", "ts": format!("2026-05-09T10:00:{i:02}Z"), "turn": 1, "round": i, "tokens_in": 100, "tokens_out": 10, "duration_ms": 500, "metadata": {"tool_call_names": [], "finish_reason": "tool_calls"}}))
            .collect();
        let tl = build_timeline(&events);
        let rendered = render_timeline(&tl, 5);
        assert!(
            rendered.contains("showing last 5"),
            "truncation note: {rendered}"
        );
        assert!(
            rendered.contains("r19"),
            "last round must be present: {rendered}"
        );
        assert!(
            !rendered.contains("r0 "),
            "first round must be truncated: {rendered}"
        );
    }
}

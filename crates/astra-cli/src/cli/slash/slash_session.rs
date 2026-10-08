use std::io::Write;

use astra_services::session_restore::RestoredSession;
use astra_services::{session_journal, session_workspace};

use crate::cli::permission_manager::PermissionMode;
use crate::cli::session::session_restore_client;
use crate::cli::session::session_runtime;
use crate::cli::surface::session_source_surface::session_source_surface;
use crate::cli::tool_call_groups;
use crate::cli::{
    cli_config::cli_utils::{
        clear_profile_last_session_if_matches_or_warn, normalize_model_override,
        persist_profile_last_session_or_warn,
    },
    session::session_state::SessionState,
    session::{session_continuation, session_projection, session_startup},
    stream::stream_render,
    theme,
};
use crossterm::style::Stylize;

fn resume_persistence_warning(error: Option<&str>) -> Option<String> {
    error
        .map(str::trim)
        .filter(|error| !error.is_empty())
        .map(|error| format!("Session persistence degraded: {}", ellipsize(error, 96)))
}

fn ellipsize(s: &str, max_chars: usize) -> String {
    let t: String = s.chars().take(max_chars).collect();
    if s.chars().count() > max_chars {
        format!("{t}…")
    } else {
        t
    }
}

// ── Session export ──────────────────────────────────────────────────────────

/// Format tool call records as a summarized markdown block.
fn format_tool_calls_md(calls: &[session_journal::ToolCallRecord]) -> String {
    let mut out = String::new();
    out.push_str("\n<details>\n<summary>Tool calls</summary>\n\n");
    for group in tool_call_groups::group_tool_calls(calls) {
        let mut header = match group.round {
            Some(round) => format!("Round {round}"),
            None => "Round ?".to_string(),
        };
        if let Some(batch_id) = group.batch_id {
            header.push_str(&format!(" · batch {batch_id}"));
        }
        if group.parallel || group.calls.len() > 1 {
            header.push_str(&format!(" · {} parallel calls", group.calls.len()));
        }
        out.push_str(&format!("- **{header}**\n"));
        for tc in &group.calls {
            let status = if tc.ok { "✓" } else { "✗" };
            let display = stream_render::format_tool_display_from_preview(
                &tc.name,
                tc.args_preview.as_deref(),
            );
            out.push_str(&format!("  - `{display}` {status} ({}ms)\n", tc.ms));
            if let Some(ref err) = tc.error {
                out.push_str(&format!("    > Error: {err}\n"));
            }
            if let Some(ref preview) = tc.result_preview {
                let short = if preview.len() > 200 {
                    format!("{}…", &preview[..preview.floor_char_boundary(200)])
                } else {
                    preview.clone()
                };
                out.push_str(&format!(
                    "    > ```\n    > {}\n    > ```\n",
                    short.replace('\n', "\n    > ")
                ));
            }
        }
    }
    out.push_str("\n</details>\n\n");
    out
}

/// Build a markdown export from journal events.
pub(crate) fn build_export_markdown(
    session_id: &str,
    workspace: Option<&session_workspace::WorkspaceMetadata>,
    events: &[session_journal::JournalEvent],
) -> String {
    let mut md = format!("# Session: {session_id}\n\n");
    if let Some(error) = workspace
        .and_then(|workspace| workspace.last_persistence_error.as_deref())
        .map(str::trim)
        .filter(|error| !error.is_empty())
    {
        md.push_str(&format!(
            "> Warning: Session persistence degraded: {error}\n\n"
        ));
    }
    for evt in events {
        let ts_short = evt.ts.get(..19).unwrap_or(&evt.ts);
        match evt.event_type {
            session_journal::JournalEventType::SessionStart => {
                md.push_str(&format!(
                    "## Session Start\n- **Time:** {ts_short}\n- **Model:** {}\n\n",
                    evt.model.as_deref().unwrap_or("default")
                ));
            }
            session_journal::JournalEventType::Turn => {
                md.push_str(&format!(
                    "### Turn {}\n- **Time:** {ts_short}\n- **Duration:** {}ms\n- **Tokens:** {} → {}\n- **Tools used:** {}\n\n",
                    evt.turn.unwrap_or(0),
                    evt.duration_ms.unwrap_or(0),
                    evt.tokens_in.unwrap_or(0),
                    evt.tokens_out.unwrap_or(0),
                    evt.tool_count.unwrap_or(0),
                ));

                if let Some(ref input) = evt.user_input {
                    if !input.is_empty() {
                        md.push_str(&format!("**User:**\n\n{input}\n\n"));
                    }
                }

                // Tool call details (collapsed)
                if let Some(ref calls) = evt.tool_calls {
                    if !calls.is_empty() {
                        md.push_str(&format_tool_calls_md(calls));
                    }
                }

                if let Some(ref output) = evt.assistant_output {
                    if !output.is_empty() {
                        md.push_str(&format!("**Assistant:**\n\n{output}\n\n"));
                    }
                }
                md.push_str("---\n\n");
            }
            session_journal::JournalEventType::TurnError => {
                md.push_str(&format!(
                    "### Turn {} ❌ Error\n- **Time:** {ts_short}\n- **Error:** {}\n- **Tokens:** {} fresh + {} cache-read + {} cache-write → {} out\n- **Tools:** {}\n- **Duration:** {:.1}s\n\n---\n\n",
                    evt.turn.unwrap_or(0),
                    evt.error.as_deref().unwrap_or("(no details)"),
                    evt.tokens_in.unwrap_or(0),
                    evt.cache_read_tokens.unwrap_or(0),
                    evt.cache_creation_tokens.unwrap_or(0),
                    evt.tokens_out.unwrap_or(0),
                    evt.tool_count.unwrap_or(0),
                    evt.duration_ms.unwrap_or(0) as f64 / 1000.0,
                ));
            }
            session_journal::JournalEventType::Compact => {
                let summary_line = evt
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("compact_summary"))
                    .and_then(|v| v.as_str())
                    .map(|s| format!("- **Summary:** {s}\n"))
                    .unwrap_or_default();
                md.push_str(&format!(
                    "### Compact\n- **Time:** {ts_short}\n- **Turns compacted:** {}\n- **Facts stored:** {}\n{summary_line}\n",
                    evt.turns_compacted.unwrap_or(0),
                    evt.facts_stored.unwrap_or(0),
                ));
            }
            session_journal::JournalEventType::ConfigChange => {
                md.push_str(&format!(
                    "- ⚙️ {ts_short}: {} → {}\n",
                    evt.config_key.as_deref().unwrap_or("?"),
                    evt.config_value.as_deref().unwrap_or("?"),
                ));
            }
            session_journal::JournalEventType::SessionEnd => {
                md.push_str(&format!(
                    "## Session End\n- **Time:** {ts_short}\n- **Total turns:** {}\n",
                    evt.turn.unwrap_or(0),
                ));
            }
            session_journal::JournalEventType::ExecutionBoundaryOpened => {
                let boundary = evt
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("execution_boundary"));
                let kind = boundary
                    .and_then(|m| m.get("kind"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("boundary");
                let transaction_id = boundary
                    .and_then(|m| m.get("transaction_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("-");
                md.push_str(&format!(
                    "### Execution boundary opened\n- **Time:** {ts_short}\n- **Kind:** {kind}\n- **Transaction:** {transaction_id}\n\n"
                ));
            }
            session_journal::JournalEventType::ExecutionBoundaryCommitted => {
                let boundary = evt
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("execution_boundary"));
                let kind = boundary
                    .and_then(|m| m.get("kind"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("boundary");
                let transaction_id = boundary
                    .and_then(|m| m.get("transaction_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("-");
                md.push_str(&format!(
                    "### Execution boundary committed\n- **Time:** {ts_short}\n- **Kind:** {kind}\n- **Transaction:** {transaction_id}\n\n"
                ));
            }
            session_journal::JournalEventType::ExecutionBoundaryAborted => {
                let boundary = evt
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("execution_boundary"));
                let kind = boundary
                    .and_then(|m| m.get("kind"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("boundary");
                let transaction_id = boundary
                    .and_then(|m| m.get("transaction_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("-");
                let reason = boundary
                    .and_then(|m| m.get("reason"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("aborted");
                md.push_str(&format!(
                    "### Execution boundary aborted\n- **Time:** {ts_short}\n- **Kind:** {kind}\n- **Transaction:** {transaction_id}\n- **Reason:** {reason}\n\n"
                ));
            }
            session_journal::JournalEventType::SessionFork => {
                let parent = evt
                    .session_lineage
                    .as_ref()
                    .map(|l| l.parent_session_id.as_str())
                    .unwrap_or("?");
                md.push_str(&format!(
                    "### Session fork\n- **Time:** {ts_short}\n- **Parent:** {parent}\n- **Note:** {}\n\n",
                    evt.user_input.as_deref().unwrap_or(""),
                ));
            }
            session_journal::JournalEventType::SyncMarker => {
                md.push_str(&format!(
                    "### Sync marker\n- **Time:** {ts_short}\n- **Note:** {}\n\n",
                    evt.user_input.as_deref().unwrap_or(""),
                ));
            }
            session_journal::JournalEventType::AdaptiveScenarioApplied => {
                let meta = evt.metadata.as_ref();
                let scenario = meta
                    .and_then(|m| m.get("scenario"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let confidence = meta
                    .and_then(|m| m.get("confidence"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                md.push_str(&format!(
                    "### Adaptive scenario applied\n- **Time:** {ts_short}\n- **Scenario:** {scenario} (confidence {confidence:.2})\n\n"
                ));
            }
            session_journal::JournalEventType::AdaptivePerTurnApplied => {
                let n = evt
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("changes"))
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                if n > 0 {
                    md.push_str(&format!(
                        "### Per-turn adaptation (T{})\n- **Time:** {ts_short}\n- **Changes:** {n}\n\n",
                        evt.turn.unwrap_or(0),
                    ));
                }
            }
            session_journal::JournalEventType::AskUserPrompted => {
                let ask_user = evt.metadata.as_ref().and_then(|m| m.get("ask_user"));
                let request_id = ask_user
                    .and_then(|m| m.get("request_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let prompt = ask_user.and_then(|m| m.get("prompt"));
                let question_count = prompt
                    .and_then(|p| p.get("questions"))
                    .and_then(|v| v.as_array())
                    .map(|questions| questions.len())
                    .unwrap_or(0);
                let context = prompt
                    .and_then(|p| p.get("context"))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| format!("- **Context:** {s}\n"))
                    .unwrap_or_default();
                md.push_str(&format!(
                    "### Ask user prompted\n- **Time:** {ts_short}\n- **Request:** {request_id}\n- **Questions:** {question_count}\n{context}\n"
                ));
            }
            session_journal::JournalEventType::AskUserResponse => {
                let ask_user = evt.metadata.as_ref().and_then(|m| m.get("ask_user"));
                let request_id = ask_user
                    .and_then(|m| m.get("request_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let status = ask_user
                    .and_then(|m| m.get("status"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let answer_count = ask_user
                    .and_then(|m| m.get("answers"))
                    .and_then(|v| v.get("answers"))
                    .and_then(|v| v.as_array())
                    .map(|answers| answers.len())
                    .unwrap_or(0);
                md.push_str(&format!(
                    "### Ask user response\n- **Time:** {ts_short}\n- **Request:** {request_id}\n- **Status:** {status}\n- **Answers:** {answer_count}\n\n"
                ));
            }
            _ => {}
        }
    }
    md
}

#[cfg(test)]
mod export_tests {
    use super::{build_export_markdown, format_tool_calls_md};
    use astra_services::session_journal::{JournalEvent, ToolCallRecord};
    use astra_services::session_workspace;

    /// Construct a JournalEvent from a JSON value — avoids listing all fields.
    fn evt_from_json(json: serde_json::Value) -> JournalEvent {
        serde_json::from_value(json).expect("valid JournalEvent JSON")
    }

    #[test]
    fn build_export_includes_session_start() {
        let evt = evt_from_json(serde_json::json!({
            "type": "session_start",
            "ts": "2025-01-15T10:30:00Z",
            "model": "gpt-4o",
        }));
        let md = build_export_markdown("abc123", None, &[evt]);
        assert!(md.contains("# Session: abc123"));
        assert!(md.contains("## Session Start"));
        assert!(md.contains("gpt-4o"));
    }

    #[test]
    fn build_export_turn_with_tool_calls() {
        let evt = evt_from_json(serde_json::json!({
            "type": "turn",
            "ts": "2025-01-15T10:31:00Z",
            "turn": 1,
            "duration_ms": 1500,
            "tokens_in": 100,
            "tokens_out": 50,
            "tool_count": 2,
            "user_input": "Hello",
            "assistant_output": "Hi there",
            "tool_calls": [
                {
                    "name": "read_file",
                    "ok": true,
                    "ms": 50,
                    "args_preview": "src/main.rs",
                    "result_preview": "fn main() { ... }",
                },
                {
                    "name": "bash",
                    "ok": false,
                    "ms": 200,
                    "error": "exit code 1",
                    "args_preview": "cargo test",
                },
            ],
        }));

        let md = build_export_markdown("test-sid", None, &[evt]);
        assert!(md.contains("### Turn 1"));
        assert!(md.contains("**User:**"));
        assert!(md.contains("Hello"));
        assert!(md.contains("<details>"));
        assert!(md.contains("`Reading: src/main.rs` ✓"));
        assert!(md.contains("`$ cargo test` ✗"));
        assert!(md.contains("exit code 1"));
        assert!(md.contains("**Assistant:**"));
        assert!(md.contains("Hi there"));
    }

    #[test]
    fn build_export_turn_without_tool_calls_omits_details() {
        let evt = evt_from_json(serde_json::json!({
            "type": "turn",
            "ts": "2025-01-15T10:31:00Z",
            "turn": 1,
            "user_input": "hi",
            "assistant_output": "hello",
        }));

        let md = build_export_markdown("sid", None, &[evt]);
        assert!(!md.contains("<details>"));
        assert!(md.contains("hello"));
    }

    #[test]
    fn format_tool_calls_md_produces_collapsed_block() {
        let calls = vec![ToolCallRecord {
            name: "grep".into(),
            ok: true,
            ms: 10,
            error: None,
            input_bytes: None,
            output_bytes: None,
            args_preview: Some("pattern in src/".into()),
            result_preview: None,
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        }];
        let block = format_tool_calls_md(&calls);
        assert!(block.contains("<details>"));
        assert!(block.contains("</details>"));
        assert!(block.contains("Round ?"));
        assert!(block.contains("`Grep: pattern in src/` ✓ (10ms)"));
    }

    #[test]
    fn format_tool_calls_md_groups_parallel_batch() {
        let mut first = ToolCallRecord {
            name: "read_file".into(),
            ok: true,
            ms: 11,
            args_preview: Some("src/lib.rs".into()),
            batch_id: Some("b-0-0".into()),
            parallel: Some(true),
            round: Some(0),
            ..Default::default()
        };
        first.result_preview = Some("mod app;".into());

        let second = ToolCallRecord {
            name: "grep".into(),
            ok: true,
            ms: 7,
            args_preview: Some("SessionState".into()),
            batch_id: Some("b-0-0".into()),
            parallel: Some(true),
            round: Some(0),
            ..Default::default()
        };

        let block = format_tool_calls_md(&[first, second]);
        assert!(block.contains("Round 0 · batch b-0-0 · 2 parallel calls"));
        assert!(block.contains("`Reading: src/lib.rs` ✓ (11ms)"));
        assert!(block.contains("`Grep: SessionState` ✓ (7ms)"));
    }

    // ── /export edge case tests ──

    #[test]
    fn build_export_empty_events_only_header() {
        let md = build_export_markdown("empty-sid", None, &[]);
        assert!(md.contains("# Session: empty-sid"));
        // Should not contain any section headers
        assert!(!md.contains("## Session Start"));
        assert!(!md.contains("### Turn"));
    }

    #[test]
    fn build_export_turn_error_event() {
        let evt = evt_from_json(serde_json::json!({
            "type": "turn_error",
            "ts": "2025-01-15T10:31:00Z",
            "turn": 3,
            "error": "rate limit exceeded",
        }));
        let md = build_export_markdown("sid", None, &[evt]);
        assert!(md.contains("Turn 3 ❌ Error"));
        assert!(md.contains("rate limit exceeded"));
    }

    #[test]
    fn build_export_compact_event() {
        let evt = evt_from_json(serde_json::json!({
            "type": "compact",
            "ts": "2025-01-15T10:35:00Z",
            "turns_compacted": 8,
            "facts_stored": 2,
        }));
        let md = build_export_markdown("sid", None, &[evt]);
        assert!(md.contains("### Compact"));
        assert!(md.contains("Turns compacted:** 8"));
        assert!(md.contains("Facts stored:** 2"));
    }

    #[test]
    fn build_export_config_change_event() {
        let evt = evt_from_json(serde_json::json!({
            "type": "config_change",
            "ts": "2025-01-15T10:33:00Z",
            "config_key": "model",
            "config_value": "gpt-4o-mini",
        }));
        let md = build_export_markdown("sid", None, &[evt]);
        assert!(md.contains("⚙️"));
        assert!(md.contains("model → gpt-4o-mini"));
    }

    #[test]
    fn build_export_session_end_event() {
        let evt = evt_from_json(serde_json::json!({
            "type": "session_end",
            "ts": "2025-01-15T11:00:00Z",
            "turn": 15,
        }));
        let md = build_export_markdown("sid", None, &[evt]);
        assert!(md.contains("## Session End"));
        assert!(md.contains("Total turns:** 15"));
    }

    #[test]
    fn build_export_sync_marker_event() {
        let evt = evt_from_json(serde_json::json!({
            "type": "sync_marker",
            "ts": "2025-01-15T10:40:00Z",
            "user_input": "manual checkpoint",
        }));
        let md = build_export_markdown("sid", None, &[evt]);
        assert!(md.contains("### Sync marker"));
        assert!(md.contains("manual checkpoint"));
    }

    #[test]
    fn build_export_non_ascii_content_preserved() {
        let evt = evt_from_json(serde_json::json!({
            "type": "turn",
            "ts": "2025-01-15T10:31:00Z",
            "turn": 1,
            "user_input": "请帮我修改代码 🔧",
            "assistant_output": "好的，我已经修改了。",
        }));
        let md = build_export_markdown("sid", None, &[evt]);
        assert!(md.contains("请帮我修改代码 🔧"));
        assert!(md.contains("好的，我已经修改了。"));
    }

    #[test]
    fn build_export_multiple_event_types_in_order() {
        let events = vec![
            evt_from_json(serde_json::json!({
                "type": "session_start",
                "ts": "2025-01-15T10:30:00Z",
                "model": "gpt-4o",
            })),
            evt_from_json(serde_json::json!({
                "type": "turn",
                "ts": "2025-01-15T10:31:00Z",
                "turn": 1,
                "user_input": "hello",
                "assistant_output": "world",
            })),
            evt_from_json(serde_json::json!({
                "type": "turn_error",
                "ts": "2025-01-15T10:32:00Z",
                "turn": 2,
                "error": "timeout",
            })),
            evt_from_json(serde_json::json!({
                "type": "compact",
                "ts": "2025-01-15T10:33:00Z",
                "turns_compacted": 5,
                "facts_stored": 1,
            })),
            evt_from_json(serde_json::json!({
                "type": "session_end",
                "ts": "2025-01-15T11:00:00Z",
                "turn": 10,
            })),
        ];
        let md = build_export_markdown("multi", None, &events);
        // Check order: session_start before turns before session_end
        let start_pos = md.find("## Session Start").unwrap();
        let turn_pos = md.find("### Turn 1").unwrap();
        let error_pos = md.find("Turn 2 ❌").unwrap();
        let compact_pos = md.find("### Compact").unwrap();
        let end_pos = md.find("## Session End").unwrap();
        assert!(start_pos < turn_pos);
        assert!(turn_pos < error_pos);
        assert!(error_pos < compact_pos);
        assert!(compact_pos < end_pos);
    }

    #[test]
    fn build_export_turn_with_empty_user_input_omits_user() {
        let evt = evt_from_json(serde_json::json!({
            "type": "turn",
            "ts": "2025-01-15T10:31:00Z",
            "turn": 1,
            "user_input": "",
            "assistant_output": "some output",
        }));
        let md = build_export_markdown("sid", None, &[evt]);
        assert!(!md.contains("**User:**"));
        assert!(md.contains("**Assistant:**"));
    }

    #[test]
    fn build_export_ts_truncation() {
        // Timestamps longer than 19 chars are truncated
        let evt = evt_from_json(serde_json::json!({
            "type": "session_start",
            "ts": "2025-01-15T10:30:00.123456789Z",
            "model": "test",
        }));
        let md = build_export_markdown("sid", None, &[evt]);
        assert!(md.contains("2025-01-15T10:30:00"));
        // Should not include the fractional seconds
        assert!(!md.contains(".123456789Z"));
    }

    #[test]
    fn build_export_header_surfaces_persistence_degradation() {
        let mut workspace = session_workspace::WorkspaceMetadata::new("sid", "gpt-5");
        workspace.last_persistence_error = Some("failed to append turn event".to_string());

        let md = build_export_markdown("sid", Some(&workspace), &[]);

        assert!(md.contains("Warning: Session persistence degraded"));
        assert!(md.contains("failed to append turn event"));
    }
}

// ═══════════════════════════════════════════════════════════ Resume ═══════

#[derive(Clone)]
struct PreparedWorkspaceRestore {
    workspace: Option<session_workspace::WorkspaceMetadata>,
    session_persistence_error: Option<String>,
    runtime_config: astra_config::RuntimeConfig,
    config_version_id: String,
}

fn prepared_workspace_restore_from_workspace(
    ws: Option<session_workspace::WorkspaceMetadata>,
    state: &SessionState,
) -> Result<PreparedWorkspaceRestore, String> {
    let saved = ws
        .as_ref()
        .and_then(|workspace| workspace.tuned_config_json.as_deref())
        .map(|json| {
            serde_json::from_str::<astra_config::RuntimeConfig>(json)
                .map_err(|error| format!("saved runtime configuration is invalid: {error}"))
        })
        .transpose()?;
    let (runtime_config, config_version_id) =
        session_startup::prepare_session_runtime_config(state, saved)
            .map_err(|error| format!("saved {error}"))?;
    Ok(PreparedWorkspaceRestore {
        session_persistence_error: ws.as_ref().and_then(|ws| ws.last_persistence_error.clone()),
        runtime_config,
        config_version_id,
        workspace: ws,
    })
}

fn load_prepared_workspace_restore(
    restored: &RestoredSession,
    state: &SessionState,
) -> Result<PreparedWorkspaceRestore, String> {
    if let Some(workspace) = restored.workspace.clone() {
        return prepared_workspace_restore_from_workspace(Some(workspace), state);
    }
    match session_workspace::read_workspace(&restored.session_id) {
        Ok(ws) => prepared_workspace_restore_from_workspace(Some(ws), state),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            prepared_workspace_restore_from_workspace(None, state)
        }
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
            let backup = session_workspace::backup_invalid_workspace_file(&restored.session_id)
                .map_err(|backup_error| {
                    format!(
                        "read workspace state for session {}: {e}; failed to move invalid workspace aside: {backup_error}",
                        restored.session_id
                    )
                })?;
            tracing::warn!(
                session_id = %restored.session_id,
                error = %e,
                backup_path = ?backup,
                "workspace metadata unreadable during resume; rebuilding from restored session"
            );
            let model_for_new = restored.model.as_deref().unwrap_or("default");
            let mut workspace =
                session_workspace::WorkspaceMetadata::new(&restored.session_id, model_for_new);
            workspace.last_persistence_error = Some(format!(
                "workspace metadata unreadable during resume; rebuilt from journal/checkpoint ({e})"
            ));
            prepared_workspace_restore_from_workspace(Some(workspace), state)
        }
        Err(e) => Err(format!(
            "read workspace state for session {}: {e}",
            restored.session_id
        )),
    }
}

fn apply_prepared_workspace_restore(state: &mut SessionState, prepared: &PreparedWorkspaceRestore) {
    state.session_persistence_error = prepared.session_persistence_error.clone();
    session_startup::apply_session_runtime_config(
        state,
        prepared.runtime_config.clone(),
        prepared.config_version_id.clone(),
    );
}

fn persist_resumed_workspace_metadata(
    restored: &RestoredSession,
    total_cache_read_tokens: u64,
    total_cache_creation_tokens: u64,
    existing_workspace: Option<&session_workspace::WorkspaceMetadata>,
) -> Result<(), String> {
    let model_for_new = restored.model.as_deref().unwrap_or("default");
    let mut ws = existing_workspace
        .cloned()
        .or_else(|| restored.workspace.clone())
        .unwrap_or_else(|| {
            session_workspace::WorkspaceMetadata::new(&restored.session_id, model_for_new)
        });
    ws.turn_count = restored.turn_count;
    ws.total_tokens_in = restored.total_tokens_in;
    ws.total_tokens_out = restored.total_tokens_out;
    ws.total_cache_read_tokens = total_cache_read_tokens;
    ws.total_cache_creation_tokens = total_cache_creation_tokens;
    ws.status = restored.last_status.clone();
    if let Some(ref branch) = restored.git_branch {
        ws.git_branch = Some(branch.clone());
    }
    ws.model = astra_core::model_override::normalize_model_override_owned(restored.model.clone());
    if restored.permission_mode.is_some() {
        ws.permission_mode = restored.permission_mode.clone();
    }
    ws.last_context_trace = restored.last_context_trace.clone();
    astra_services::session_workspace::write_workspace(&ws)
        .map_err(|e| format!("write workspace during resume: {e}"))
}

fn parse_restored_permission_mode(
    restored: &RestoredSession,
) -> Result<Option<PermissionMode>, String> {
    restored
        .permission_mode
        .as_deref()
        .map(str::parse::<PermissionMode>)
        .transpose()
        .map_err(|error| {
            format!(
                "Session {} has invalid persisted permission mode: {error}",
                restored.session_id
            )
        })
}

fn build_step_resume_guidance(
    interruption: Option<&serde_json::Value>,
    compaction_state: Option<&serde_json::Value>,
) -> Option<String> {
    let compaction_ctx = compaction_resume_context_from_checkpoint_state(compaction_state);
    interruption.and_then(|irj| {
        astra_turn_core::interruption::build_resume_guidance_with_context(
            irj,
            compaction_ctx.as_ref(),
        )
    })
}

fn compaction_resume_context_from_checkpoint_state(
    compaction_state: Option<&serde_json::Value>,
) -> Option<astra_turn_core::interruption::CompactionResumeContext> {
    compaction_state.map(
        |cs| astra_turn_core::interruption::CompactionResumeContext {
            compaction_attempts: cs
                .get("attempt_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32,
            total_tokens_freed: cs
                .get("cumulative_tokens_freed")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            last_was_insufficient: cs
                .get("last_was_insufficient")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        },
    )
}

fn apply_resume_recovery_state(
    state: &mut SessionState,
    interruption: Option<&serde_json::Value>,
    compaction_state: Option<&serde_json::Value>,
) {
    state.last_turn_interrupted = interruption.is_some();
    state.resume_guidance = build_step_resume_guidance(interruption, compaction_state);
    state.resume_restricted_tools = interruption
        .map(astra_turn_core::interruption::resume_restricted_tools_from_interruption_json)
        .unwrap_or_default();
}

fn apply_runtime_recovery_state(
    state: &mut SessionState,
    pipeline_state: Option<&serde_json::Value>,
    compaction_state: Option<&serde_json::Value>,
    consecutive_context_window_errors: u32,
) {
    state.runtime_pipeline_state = pipeline_state.cloned();
    state.runtime_compaction_state = compaction_state.cloned();
    state.runtime_consecutive_context_window_errors = consecutive_context_window_errors;
}

/// Baseline row for a blocked tool when we have no persisted health metrics yet (same defaults as
/// cloud preference seeding in `cloud_sync.rs`).
fn blocked_tool_health_entry(
    name: String,
) -> astra_turn_core::tool_health_persistence::ToolHealthEntry {
    astra_turn_core::tool_health_persistence::ToolHealthEntry {
        name,
        total_calls: 0,
        total_failures: 0,
        input_validation_failures: 0,
        failure_rate: 0.0,
        last_updated_epoch: 0,
        recent_outcomes: vec![],
    }
}

fn apply_heavy_state_fallback(
    state: &mut SessionState,
    blocked_tools: &[String],
    recent_tools: &[String],
    messages: &[serde_json::Value],
    approval_overrides: Option<&serde_json::Value>,
) {
    for tool in blocked_tools {
        if !state.tool_health_entries.iter().any(|e| e.name == *tool) {
            state
                .tool_health_entries
                .push(blocked_tool_health_entry(tool.clone()));
        }
    }
    if let Some(ao_json) = approval_overrides {
        state.perm_manager.merge_restored_overrides(ao_json);
    }
    if state.recent_tools.is_empty() {
        state.recent_tools = recent_tools.to_vec();
    }
    if state.history.is_empty() {
        let pairs = session_continuation::history_pairs_from_messages(
            &session_continuation::sanitize_continuation_messages(
                session_continuation::materialize_cli_continuation_messages(
                    astra_core::history_work::HistoryWorkSite::CliResumeCanonicalHistoryClone,
                    messages,
                ),
            ),
        );
        if !pairs.is_empty() {
            state.history = pairs;
        }
    }
}

#[cfg(test)]
fn apply_heavy_checkpoint_fallback(
    state: &mut SessionState,
    heavy: &astra_pipeline::step_protocol::HeavyCheckpoint,
) {
    apply_heavy_state_fallback(
        state,
        &heavy.blocked_tools,
        &heavy.recent_tools,
        &heavy.messages,
        heavy.approval_overrides.as_ref(),
    );
}

fn apply_restored_cloud_heavy_state(
    state: &mut SessionState,
    restored: &RestoredSession,
    resume_messages: &[serde_json::Value],
) {
    apply_heavy_state_fallback(
        state,
        &restored.blocked_tools,
        &restored.recent_tools,
        resume_messages,
        restored.approval_overrides.as_ref(),
    );
}

struct PreparedSessionHistory {
    history: Vec<(String, String)>,
    active_conversation: Option<astra_turn_core::active_conversation::ActiveConversation>,
    resume: Option<astra_turn_types::ResumeDescriptorV1>,
    recent_tools: Vec<String>,
    deferred_tool_activations: Vec<astra_turn_types::DeferredToolActivation>,
    csl_manager: Option<astra_turn_core::conversation_log::manager::CslManager>,
}

fn materialize_prepared_session_history(
    mgr: astra_turn_core::conversation_log::manager::CslManager,
    mat: Option<astra_turn_core::conversation_log::MaterializedState>,
    restored_journal: session_runtime::RestoredJournalState,
    session_id: &str,
) -> PreparedSessionHistory {
    let has_csl_materialization = mat.is_some();
    let mut history = crate::cli::history_work::clone_pair_history(
        astra_core::history_work::HistoryWorkSite::CliSessionRestoreJournalHistoryClone,
        &restored_journal.session.history,
    );
    let mut recent_tools = restored_journal.session.recent_tools.clone();
    let mut deferred_tool_activations = Vec::new();
    let mut active_conversation = None;
    if let Some(ref materialized) = mat {
        let canonical_messages = session_continuation::sanitize_continuation_messages(
            session_continuation::materialize_cli_continuation_messages(
                astra_core::history_work::HistoryWorkSite::CliSessionRestoreCanonicalHistoryClone,
                &materialized.messages,
            ),
        );
        history = session_continuation::history_pairs_from_messages(&canonical_messages);
        active_conversation =
            astra_turn_core::active_conversation::ActiveConversation::from_projection(
                &crate::cli::cli_config::cli_utils::cli_user_id(),
                session_id,
                canonical_messages,
                materialized.last_turn,
                astra_turn_core::active_conversation::ActiveConversationSource::CslProjection,
            )
            .ok();
        if !materialized.session_state.recent_tools.is_empty() {
            recent_tools = materialized.session_state.recent_tools.clone();
        }
        deferred_tool_activations =
            astra_turn_core::tool::deferred_activation::merged_deferred_tool_activations(
                &materialized.messages,
                materialized.session_state.deferred_tool_activations.clone(),
            );
    }
    PreparedSessionHistory {
        history,
        active_conversation,
        resume: None,
        recent_tools,
        deferred_tool_activations,
        csl_manager: has_csl_materialization.then_some(mgr),
    }
}

async fn prepare_session_history(session_id: &str) -> Result<PreparedSessionHistory, String> {
    let restored_journal = session_runtime::restored_journal_state(session_id)?;
    // Try CSL first — full-fidelity message history via CslManager.
    let base_dir = session_journal::local_owner_sessions_dir();
    let store = std::sync::Arc::new(
        astra_turn_core::conversation_log::file_store::FileCslStore::new(base_dir),
    );
    let mut mgr = astra_turn_core::conversation_log::manager::CslManager::new(
        store,
        session_id.to_string(),
        Default::default(),
    )
    .map_err(|e| format!("initialize CSL state for session {session_id}: {e}"))?;
    let mut prepared = match mgr.load().await {
        Ok(materialized) => {
            materialize_prepared_session_history(mgr, materialized, restored_journal, session_id)
        }
        Err(e) => {
            return Err(format!("load CSL state for session {session_id}: {e}"));
        }
    };

    // The canonical journal lane is authoritative even when the asynchronous
    // CSL projection has not caught up yet.
    if let Some(continuation) =
        session_continuation::load_session_continuation_for_recovery(session_id)
    {
        let canonical_history =
            session_continuation::history_pairs_from_messages(&continuation.messages);
        if canonical_history.len() > prepared.history.len() || prepared.history.is_empty() {
            prepared.history = canonical_history;
        }
        prepared.deferred_tool_activations = continuation.deferred_tool_activations;
        prepared.active_conversation = Some(continuation.active_conversation);
        prepared.resume = Some(continuation.resume);
    }
    Ok(prepared)
}

async fn apply_restored_session(
    profile: Option<&str>,
    api: &astra_thin_client::ThinClient,
    state: &mut SessionState,
    mut restored: RestoredSession,
) -> Result<(), String> {
    let resume_bundle = restored.resume_bundle.take();
    if restored.restored_from_cloud && resume_bundle.is_none() {
        return Err("cloud restore omitted required versioned ResumeBundle".to_string());
    }
    let had_resume_bundle = resume_bundle.is_some();
    let typed_continuation =
        resume_bundle.and_then(session_continuation::continuation_from_resume_bundle);
    if had_resume_bundle && typed_continuation.is_none() {
        return Err("restored ResumeBundle failed causal validation".to_string());
    }
    let local_journal = session_runtime::restored_journal_state(&restored.session_id)?;
    if !restored.restored_from_cloud && !local_journal.exists {
        return Err(format!(
            "Session {} not found or not owned by user",
            restored.session_id
        ));
    }
    let local_state = local_journal.session;
    let total_cache_read_tokens = restored
        .total_cache_read_tokens
        .max(local_state.total_cache_read_tokens);
    let total_cache_creation_tokens = restored
        .total_cache_creation_tokens
        .max(local_state.total_cache_creation_tokens);
    let prepared_workspace = load_prepared_workspace_restore(&restored, state)?;
    let prepared_history = prepare_session_history(&restored.session_id).await?;
    let use_typed_continuation = match (
        prepared_history.resume.as_ref(),
        typed_continuation.as_ref(),
    ) {
        (_, None) => false,
        (None, Some(_)) => true,
        (_, Some(remote))
            if restored.restored_from_cloud
                && remote.resume.source == astra_turn_types::ResumeSourceV1::CanonicalJournal =>
        {
            true
        }
        (Some(local), Some(remote)) => {
            let candidates = [
                session_continuation::portable_resume_descriptor(local.clone()),
                session_continuation::portable_resume_descriptor(remote.resume.clone()),
            ];
            astra_turn_types::select_resume_candidate_index(None, &candidates).map_err(|error| {
                format!("TUI resume candidates are causally inconsistent: {error}")
            })? == 1
        }
    };
    let selected_cursor = if use_typed_continuation {
        typed_continuation
            .as_ref()
            .map(|continuation| continuation.resume.cursor.clone())
    } else {
        prepared_history
            .resume
            .as_ref()
            .map(|resume| resume.cursor.clone())
    };
    let use_restored_projection = use_typed_continuation;
    if !use_restored_projection {
        // The local canonical generation won causal selection. Do not retain
        // independently persisted cloud/checkpoint controls from a different
        // generation.
        restored.recent_tools.clear();
        restored.blocked_tools.clear();
        restored.approval_overrides = None;
        restored.interruption = None;
        restored.compaction_state = None;
        restored.pipeline_state = None;
        restored.model = None;
        restored.permission_mode = None;
    }
    let restored_permission_mode = parse_restored_permission_mode(&restored)?;
    let restored_resume_messages = if use_typed_continuation {
        typed_continuation
            .as_ref()
            .map(|continuation| continuation.messages.as_slice())
            .unwrap_or_default()
    } else if let Some(active) = prepared_history.active_conversation.as_ref() {
        active.messages()
    } else {
        &[]
    };
    let mut restored_activation = if use_restored_projection {
        restored.deferred_tool_activations.clone()
    } else {
        Vec::new()
    };
    let last_turn_event = local_journal.last_turn_event;
    let user_id = state
        .ingestion_user_id
        .as_deref()
        .filter(|user_id| !user_id.is_empty())
        .map(str::to_string)
        .unwrap_or_else(crate::cli::cli_config::cli_utils::cli_user_id);

    let local_checkpoint_is_admissible = match selected_cursor.as_ref() {
        None => true,
        Some(selected) => {
            match astra_pipeline::step_checkpoint::read_latest_heavy_checkpoint(
                &user_id,
                &restored.session_id,
            ) {
                Ok(None) => true,
                Ok(Some(checkpoint)) => {
                    checkpoint
                        .conversation_cursor
                        .as_ref()
                        .is_some_and(|source| {
                            session_continuation::cursor_is_exact_for_attached_account(
                                source, selected,
                            )
                        })
                }
                // Let the established recovery path surface the concrete I/O
                // or decode error instead of replacing it with a causal one.
                Err(_) => true,
            }
        }
    };
    // Local recovery validates checkpoint and journal facts; it does not
    // re-execute tools. Missing state needs no restore; malformed state fails closed.
    let step_restored = if !local_checkpoint_is_admissible {
        tracing::warn!(
            session_id = %restored.session_id,
            "skipping crash recovery from a checkpoint outside the selected conversation generation"
        );
        None
    } else {
        match astra_pipeline::crash_recovery::recover_from_crash(&user_id, &restored.session_id) {
            Ok(Some(astra_pipeline::crash_recovery::RecoveryOutcome::AutoRecovered {
                restored: cr_restored,
                ..
            })) => {
                tracing::info!("crash recovery: restored validated checkpoint");
                Some(cr_restored)
            }
            Ok(Some(astra_pipeline::crash_recovery::RecoveryOutcome::RequiresUserInput {
                pending_decisions,
                restored: cr_restored,
                ..
            })) => {
                tracing::info!(
                    pending = ?pending_decisions,
                    "crash recovery: requires user input, presenting options"
                );

                // Present each pending decision to the user
                eprintln!();
                eprintln!("  {}", "Crash Recovery Requires User Input".bold().yellow());
                eprintln!("  {}", "The following tool calls need your decision:".dim());

                for (i, (tool_name, decision)) in pending_decisions.iter().enumerate() {
                    eprintln!();
                    eprintln!(
                        "    {} {}",
                        format!("[{}]", i + 1).bold(),
                        tool_name.clone().bold()
                    );

                    eprintln!("      Reason: {}", decision.as_str().dim());
                }

                eprintln!();
                eprintln!("  {}", "Options:".bold());
                eprintln!(
                    "    [c] Continue - Accept unknown tool effects and resume the conversation"
                );
                eprintln!("        No tool is re-executed or skipped by recovery");
                eprintln!("    [a] Abort - Abort recovery and fail");
                eprintln!();
                eprint!("  {} ", "Choose whether to continue or abort (c/a):".bold());
                let _ = std::io::stderr().flush();

                let mut input = String::new();
                if std::io::stdin().read_line(&mut input).is_err() {
                    return Err("Failed to read user input for crash recovery".to_string());
                }

                let choice = input.trim().to_lowercase();
                match choice.as_str() {
                    "c" | "continue" => {
                        eprintln!(
                            "  {} Continuing with acknowledged unknown tool effects",
                            "✓".green()
                        );
                        Some(cr_restored)
                    }
                    "a" | "abort" => {
                        eprintln!("  {} User chose to abort recovery", "✗".red());
                        return Err("User aborted crash recovery".to_string());
                    }
                    _ => {
                        eprintln!("  {} Invalid choice, aborting recovery", "✗".red());
                        return Err(format!("Invalid user choice: {}", choice));
                    }
                }
            }
            Ok(None) => None,
            Err(error) => {
                return Err(format!(
                    "Crash recovery failed for user_id={} session_id={}: {}",
                    user_id, restored.session_id, error
                ));
            }
        }
    };
    let step_restored = step_restored.filter(|checkpoint| {
        let admissible = match (
            selected_cursor.as_ref(),
            checkpoint.conversation_cursor.as_ref(),
        ) {
            (None, _) => true,
            (Some(selected), Some(source)) => {
                session_continuation::cursor_is_exact_for_attached_account(source, selected)
            }
            _ => false,
        };
        if !admissible {
            tracing::warn!(
                session_id = %restored.session_id,
                "ignoring a local step checkpoint outside the selected conversation generation"
            );
        }
        admissible
    });
    let has_cloud_heavy_fallback = use_restored_projection
        && (!restored_resume_messages.is_empty()
            || !restored.blocked_tools.is_empty()
            || restored.approval_overrides.is_some()
            || restored.interruption.is_some()
            || restored.compaction_state.is_some());
    if let Some(step_restored) = step_restored.as_ref() {
        restored_activation.extend(step_restored.deferred_tool_activations.iter().cloned());
    }
    let prepared_activation_is_admissible =
        match (selected_cursor.as_ref(), prepared_history.resume.as_ref()) {
            (None, _) => true,
            (Some(selected), Some(source)) => {
                session_continuation::cursor_is_exact_for_attached_account(&source.cursor, selected)
            }
            _ => false,
        };
    if prepared_activation_is_admissible {
        restored_activation.extend(prepared_history.deferred_tool_activations.iter().cloned());
    }
    let session_memory = super::slash_memory::load_current_session_memory_body_with_profile(
        api,
        profile,
        &restored.session_id,
    )
    .await;
    persist_resumed_workspace_metadata(
        &restored,
        total_cache_read_tokens,
        total_cache_creation_tokens,
        prepared_workspace.workspace.as_ref(),
    )?;

    state.prepare_for_session_rebind().await;
    state.reset_for_session_restore();
    state.set_session_id(restored.session_id.clone());
    state.turn = restored.turn_count;
    state.total_prompt_tokens = restored.total_tokens_in;
    state.total_completion_tokens = restored.total_tokens_out;
    state.total_cache_read_tokens = total_cache_read_tokens;
    state.total_cache_creation_tokens = total_cache_creation_tokens;
    state.recent_tools = restored.recent_tools.clone();
    if let Some(mode) = restored_permission_mode {
        state.perm_manager.set_mode(mode);
    }
    // Restored effective selection is a baseline, not a new explicit request.
    if !matches!(
        state.cli_context.requested_model_policy,
        Some(astra_turn_types::RequestedModelPolicy::Fixed { .. })
    ) {
        state.model = normalize_model_override(restored.model.as_deref())
            .map(|model| model.to_string().into());
    }
    apply_prepared_workspace_restore(state, &prepared_workspace);

    if let Some(step_restored) = step_restored {
        let summary = astra_pipeline::step_restore::restore_summary(&step_restored);
        state.workspace_observation_quarantine =
            step_restored.workspace_observation_quarantine.clone();
        for tool in &step_restored.blocked_tools {
            if !state.tool_health_entries.iter().any(|e| e.name == *tool) {
                state
                    .tool_health_entries
                    .push(blocked_tool_health_entry(tool.clone()));
            }
        }
        if state.recent_tools.is_empty() {
            state.recent_tools = step_restored.recent_tools;
        }
        apply_resume_recovery_state(
            state,
            step_restored.interruption.as_ref(),
            step_restored.compaction_state.as_ref(),
        );
        apply_runtime_recovery_state(
            state,
            step_restored.pipeline_state.as_ref(),
            step_restored.compaction_state.as_ref(),
            step_restored.consecutive_context_window_errors,
        );
        if let Some(ref ao_json) = step_restored.approval_overrides {
            state.perm_manager.merge_restored_overrides(ao_json);
        }
        eprintln!("  {} {}", "↻".magenta(), summary.dim());
    } else if has_cloud_heavy_fallback {
        apply_restored_cloud_heavy_state(state, &restored, restored_resume_messages);
        apply_resume_recovery_state(
            state,
            restored.interruption.as_ref(),
            restored.compaction_state.as_ref(),
        );
        apply_runtime_recovery_state(
            state,
            restored.pipeline_state.as_ref(),
            restored.compaction_state.as_ref(),
            0,
        );
        eprintln!("  {} Restored step checkpoint from cloud", "☁".magenta());
    }

    if use_typed_continuation {
        state.history = session_continuation::history_pairs_from_messages(restored_resume_messages);
    } else if prepared_history.history.len() > state.history.len() || state.history.is_empty() {
        state.history = prepared_history.history;
    }
    if prepared_activation_is_admissible && !prepared_history.recent_tools.is_empty() {
        state.recent_tools = prepared_history.recent_tools;
    }
    state.csl_manager = prepared_activation_is_admissible
        .then_some(prepared_history.csl_manager)
        .flatten();
    state.active_conversation = if use_typed_continuation {
        typed_continuation
            .as_ref()
            .map(|continuation| continuation.active_conversation.clone())
    } else {
        prepared_history.active_conversation.clone()
    };
    state.last_response = state.history.last().map(|(_, resp)| resp.clone());
    state.last_turn_event = last_turn_event;
    let fallback_resume_messages;
    let canonical_resume_messages = if !restored_resume_messages.is_empty() {
        restored_resume_messages
    } else {
        fallback_resume_messages =
            session_continuation::load_session_messages_for_continuation(&restored.session_id)
                .unwrap_or_else(|| {
                    session_projection::history_as_messages_for(
                        astra_core::history_work::HistoryWorkSite::CliResumeHistoryMaterialization,
                        &state.history,
                    )
                });
        fallback_resume_messages.as_slice()
    };
    state.deferred_tool_activations =
        astra_turn_core::tool::deferred_activation::merged_deferred_tool_activations(
            canonical_resume_messages,
            restored_activation,
        );
    session_projection::seed_continuation_objective_from_messages(state, canonical_resume_messages);
    session_projection::rebuild_continuation_anchor_from_live_state(state);
    state.continuation_anchor = session_projection::merge_continuation_anchor_with_session_memory(
        state.continuation_anchor.take(),
        session_memory.as_deref(),
    );
    let session_resume_hydration =
        match astra_turn_core::resume_hydration::build_resume_hydration_hint_from_messages(
            canonical_resume_messages,
        ) {
            Ok(Some(hint)) => hint,
            Ok(None) => astra_turn_core::resume_hydration::build_resume_hydration_failure_hint(
                "resume restored session metadata but no prompt-facing transcript/history",
            ),
            Err(error) => {
                tracing::warn!(
                    session_id = %restored.session_id,
                    error = %error,
                    "resume hydration degraded: restored typed turn metadata is invalid"
                );
                astra_turn_core::resume_hydration::build_resume_hydration_failure_hint(
                    "restored session contains invalid typed turn metadata",
                )
            }
        };
    state.resume_guidance = astra_turn_core::resume_hydration::merge_resume_hints(
        Some(session_resume_hydration),
        state.resume_guidance.take(),
    );

    session_startup::initialize_journal_pub(state, &restored.session_id);
    persist_profile_last_session_or_warn(
        profile,
        &restored.session_id,
        "slash_session:restore_session_into_state",
    );

    let source = session_source_surface(&restored.last_status, restored.restored_from_cloud, false);
    eprintln!(
        "  {} Resumed session {} ({}, {} turns, {} checkpoints)",
        theme::icon_ok(),
        restored.session_id[..8.min(restored.session_id.len())].magenta(),
        source.label(),
        restored.turn_count,
        restored.checkpoint_count,
    );
    if let Some(warning) = resume_persistence_warning(state.session_persistence_error.as_deref()) {
        eprintln!("  {}", warning.yellow());
    }
    if let Some(ref trace) = restored.last_context_trace {
        let preview = trace.preview();
        if !preview.is_empty() {
            eprintln!("    {} {}", "Last trace:".dim(), preview.dim());
        }
    }

    Ok(())
}

pub(crate) async fn restore_session_into_state(
    session_id: &str,
    profile: Option<&str>,
    api: &astra_thin_client::ThinClient,
    state: &mut SessionState,
) -> Result<(), String> {
    let restored =
        session_restore_client::restore_session_snapshot_with_client(profile, api, session_id)
            .await?;
    let Some(restored) = restored else {
        if session_restore_client::has_server_auth(profile) {
            clear_profile_last_session_if_matches_or_warn(
                profile,
                session_id,
                "slash_session:restore_session_snapshot",
            );
            return Err(format!(
                "Session {session_id} no longer exists or is not owned by the authenticated user."
            ));
        }
        return Err(format!(
            "Session {session_id} has no resumable workspace/checkpoint state. Use /resume to inspect available sessions."
        ));
    };
    apply_restored_session(profile, api, state, restored).await
}

#[cfg(test)]
mod resume_tests {
    use super::{
        apply_heavy_checkpoint_fallback, apply_restored_session, apply_resume_recovery_state,
        build_step_resume_guidance, prepare_session_history, restore_session_into_state,
        resume_persistence_warning, session_restore_client,
    };
    use crate::cli::permission_manager::PermissionMode;
    use crate::cli::session::session_state::SessionState;
    use astra_services::session_journal;
    use astra_services::session_restore::RestoredSession;
    use astra_services::session_workspace;
    use wiremock::matchers::{header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    struct EnvGuard {
        key: &'static str,
        old: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let old = std::env::var(key).ok();
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, old }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.old {
                Some(value) => unsafe {
                    std::env::set_var(self.key, value);
                },
                None => unsafe {
                    std::env::remove_var(self.key);
                },
            }
        }
    }

    fn typed_resume_bundle(
        session_id: &str,
        turn_count: u32,
        messages: Vec<serde_json::Value>,
    ) -> astra_turn_types::ResumeBundleV1 {
        let sequence = u64::from(turn_count);
        let cursor = astra_turn_types::SessionCursorV1 {
            schema_version: astra_turn_types::SESSION_CURSOR_SCHEMA_VERSION,
            owner_id: crate::cli::cli_config::cli_utils::cli_user_id(),
            session_id: session_id.to_string(),
            branch_id: astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID.to_string(),
            completed_turn: turn_count,
            journal_event_seq: sequence,
            conversation_seq: sequence,
            canonical_root_hash: astra_turn_types::canonical_conversation_root(&messages),
            projection_schema: astra_turn_types::CONVERSATION_PROJECTION_SCHEMA_VERSION,
            compaction_generation: 0,
            config_version_id: None,
        };
        astra_turn_types::select_resume_bundle(
            None,
            [astra_turn_types::ResumeCandidateV1 {
                source: astra_turn_types::ResumeSourceV1::Checkpoint,
                cursor,
                conversation_messages: messages,
                materialized_conversation_root_hash: None,
                degraded_reasons: Vec::new(),
                repair_actions: Vec::new(),
                projections: Default::default(),
            }],
        )
        .expect("valid test resume bundle")
    }

    async fn mock_canonical_resume(server: &MockServer, session_id: &str, turn: u32) {
        let mut bundle = typed_resume_bundle(
            session_id,
            turn,
            vec![
                serde_json::json!({"role":"user","content":"continue"}),
                serde_json::json!({"role":"assistant","content":"remote restored"}),
            ],
        );
        bundle.source = astra_turn_types::ResumeSourceV1::CanonicalJournal;
        Mock::given(method("POST"))
            .and(path(format!("/sessions/{session_id}/resume")))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(RestoredSession {
                session_id: session_id.into(),
                turn_count: turn,
                restored_from_cloud: true,
                resume_bundle: Some(bundle),
                total_tokens_in: 15,
                total_tokens_out: 7,
                total_cache_read_tokens: 9,
                total_cache_creation_tokens: 3,
                ..Default::default()
            }))
            .expect(1)
            .mount(server)
            .await;
    }

    fn write_local_resumable_session(session_id: &str, turn_count: u32) {
        let writer = session_journal::JournalWriter::new(session_id).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(session_id),
                Some("gpt-5"),
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::turn(
                Some(session_id),
                turn_count,
                Some("gpt-5"),
                "continue",
                "restored",
                0,
                15,
                7,
                8,
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::interruption_recorded(
                Some(session_id),
                turn_count,
                serde_json::json!({
                    "kind": "rate_limited",
                    "resumable": true,
                    "has_checkpoint": true,
                    "tool_calls_completed": 1,
                    "turns_completed": turn_count,
                    "remaining_turns": 4,
                }),
            ))
            .unwrap();

        let cwd = std::env::current_dir().unwrap();
        let mut ws = session_workspace::WorkspaceMetadata::with_context(
            session_id,
            "gpt-5",
            &cwd.display().to_string(),
            Some("main"),
        );
        ws.turn_count = turn_count;
        ws.total_tokens_in = 15;
        ws.total_tokens_out = 7;
        ws.total_cache_read_tokens = 9;
        ws.total_cache_creation_tokens = 3;
        ws.status = "active".to_string();
        session_workspace::write_workspace(&ws).unwrap();
    }

    fn attach_checkpoint_to_canonical_test_generation(
        session_id: &str,
        turn_count: u32,
        heavy: &mut astra_pipeline::step_protocol::HeavyCheckpoint,
    ) {
        let owner_id = crate::cli::cli_config::cli_utils::cli_user_id();
        let active =
            astra_turn_core::active_conversation::ActiveConversation::empty(&owner_id, session_id)
                .unwrap();
        let prepared = active
            .prepare_commit(turn_count, None, heavy.messages.clone())
            .unwrap();
        heavy.conversation_cursor = Some(prepared.next.cursor().clone());
        session_journal::JournalWriter::new(session_id)
            .unwrap()
            .append(
                &session_journal::JournalEvent::turn(
                    Some(session_id),
                    turn_count,
                    Some("gpt-5"),
                    "display user",
                    "display assistant",
                    0,
                    0,
                    0,
                    0,
                )
                .with_conversation_commit(prepared.commit),
            )
            .unwrap();
    }

    fn write_local_step_checkpoint_with_interruption(session_id: &str, turn_count: u32) {
        let mut heavy = match astra_pipeline::step_protocol::StepCheckpoint::heavy(
            format!("step-{turn_count}"),
            format!("task-{turn_count}"),
            session_id.to_string(),
            astra_pipeline::step_protocol::ExecutionCursor::default(),
        ) {
            astra_pipeline::step_protocol::StepCheckpoint::Heavy(heavy) => *heavy,
            _ => unreachable!("heavy checkpoint constructor should yield Heavy"),
        };
        heavy.messages = vec![
            serde_json::json!({"role": "user", "content": "continue"}),
            serde_json::json!({"role": "assistant", "content": "restoring interrupted lifecycle work"}),
        ];
        heavy.recent_tools = vec!["bash".into(), "introspect".into()];
        heavy.interruption = Some(serde_json::json!({
            "kind": "rate_limited",
            "resumable": true,
            "has_checkpoint": true,
            "tool_calls_completed": 1,
            "turns_completed": turn_count,
            "remaining_turns": 2,
        }));
        attach_checkpoint_to_canonical_test_generation(session_id, turn_count, &mut heavy);
        let user_id = crate::cli::cli_config::cli_utils::cli_user_id();
        astra_pipeline::step_checkpoint::write_step_checkpoint(
            &user_id,
            session_id,
            turn_count,
            &astra_pipeline::step_protocol::StepCheckpoint::Heavy(Box::new(heavy)),
        )
        .unwrap();
    }

    fn write_local_step_checkpoint_with_approval_overrides(
        session_id: &str,
        turn_count: u32,
        approval_overrides: serde_json::Value,
    ) {
        let mut heavy = match astra_pipeline::step_protocol::StepCheckpoint::heavy(
            format!("step-{turn_count}"),
            format!("task-{turn_count}"),
            session_id.to_string(),
            astra_pipeline::step_protocol::ExecutionCursor::default(),
        ) {
            astra_pipeline::step_protocol::StepCheckpoint::Heavy(heavy) => *heavy,
            _ => unreachable!("heavy checkpoint constructor should yield Heavy"),
        };
        heavy.messages = vec![
            serde_json::json!({"role": "user", "content": "continue"}),
            serde_json::json!({"role": "assistant", "content": "restoring session approvals"}),
        ];
        heavy.approval_overrides = Some(approval_overrides);
        attach_checkpoint_to_canonical_test_generation(session_id, turn_count, &mut heavy);
        let user_id = crate::cli::cli_config::cli_utils::cli_user_id();
        astra_pipeline::step_checkpoint::write_step_checkpoint(
            &user_id,
            session_id,
            turn_count,
            &astra_pipeline::step_protocol::StepCheckpoint::Heavy(Box::new(heavy)),
        )
        .unwrap();
    }

    fn write_profile_with_token(session_id: &str) {
        let mut creds = crate::cli::cli_config::cli_utils::CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            crate::cli::cli_config::cli_utils::Profile {
                access_token: Some("test-token".into()),
                last_session_id: Some(session_id.to_string()),
                ..Default::default()
            },
        );
        crate::cli::cli_config::cli_utils::save_credentials(&creds).unwrap();
    }

    fn write_completed_read_step_event(session_id: &str, turn_count: u32, created_at: u64) {
        let args = serde_json::json!({"path": "src/lib.rs"});
        let idem_key = astra_pipeline::step_protocol::IdempotencyKey::semantic("read_file", &args);
        let user_id = crate::cli::cli_config::cli_utils::cli_user_id();
        let mut event_store =
            astra_pipeline::step_checkpoint::FileBackedEventStore::empty(&user_id, session_id);
        let _ = <astra_pipeline::step_checkpoint::FileBackedEventStore as astra_pipeline::step_protocol::StepEventStore>::append(
            &mut event_store,
            astra_pipeline::step_protocol::StepEvent {
                event_id: format!("completed-read-{turn_count}"),
                run_id: format!("local-{session_id}-{turn_count}"),
                canonical_event_id: None,
                step_id: format!("step-{turn_count}"),
                event_type: astra_pipeline::step_protocol::StepEventType::ToolCallCompleted,
                agent_id: None,
                caused_by: vec![],
                payload: Some(serde_json::json!({
                    "tool_name": "read_file",
                    "idempotency_key": idem_key.cache_key(),
                    "output": "cached src/lib.rs",
                    "is_error": false,
                })),
                created_at,
            },
        );
    }

    fn write_local_step_checkpoint_with_compaction_state(session_id: &str, turn_count: u32) {
        let mut heavy = match astra_pipeline::step_protocol::StepCheckpoint::heavy(
            format!("step-{turn_count}"),
            format!("task-{turn_count}"),
            session_id.to_string(),
            astra_pipeline::step_protocol::ExecutionCursor::default(),
        ) {
            astra_pipeline::step_protocol::StepCheckpoint::Heavy(heavy) => *heavy,
            _ => unreachable!("heavy checkpoint constructor should yield Heavy"),
        };
        heavy.messages = vec![
            serde_json::json!({"role": "user", "content": "continue"}),
            serde_json::json!({"role": "assistant", "content": "context was compacted"}),
        ];
        heavy.interruption = Some(serde_json::json!({
            "kind": "context_overflow",
            "resumable": true,
            "has_checkpoint": true,
            "tool_calls_completed": 2,
            "turns_completed": turn_count,
            "remaining_turns": 1,
        }));
        heavy.compaction_state = Some(serde_json::json!({
            "attempt_count": 3,
            "cumulative_tokens_freed": 15000,
            "last_tokens_freed": 4000,
            "last_was_insufficient": true,
        }));
        heavy.pipeline_state = Some(serde_json::json!({
            "stats": {"cache_hit_ratio_ema": 0.42},
            "recovery": {"ptl_error_count": 2},
        }));
        heavy.consecutive_context_window_errors = 2;
        attach_checkpoint_to_canonical_test_generation(session_id, turn_count, &mut heavy);
        let completed_event_created_at = heavy.light.created_at.saturating_add(1);
        let user_id = crate::cli::cli_config::cli_utils::cli_user_id();
        astra_pipeline::step_checkpoint::write_step_checkpoint(
            &user_id,
            session_id,
            turn_count,
            &astra_pipeline::step_protocol::StepCheckpoint::Heavy(Box::new(heavy)),
        )
        .unwrap();
        write_completed_read_step_event(session_id, turn_count, completed_event_created_at);
    }

    fn write_invalid_local_step_checkpoint(session_id: &str, turn_count: u32) {
        let mut heavy = match astra_pipeline::step_protocol::StepCheckpoint::heavy(
            format!("step-{turn_count}"),
            format!("task-{turn_count}"),
            session_id.to_string(),
            astra_pipeline::step_protocol::ExecutionCursor::default(),
        ) {
            astra_pipeline::step_protocol::StepCheckpoint::Heavy(heavy) => *heavy,
            _ => unreachable!("heavy checkpoint constructor should yield Heavy"),
        };
        heavy.light.protocol_version = 0;
        let user_id = crate::cli::cli_config::cli_utils::cli_user_id();
        astra_pipeline::step_checkpoint::write_step_checkpoint(
            &user_id,
            session_id,
            turn_count,
            &astra_pipeline::step_protocol::StepCheckpoint::Heavy(Box::new(heavy)),
        )
        .unwrap();
    }

    #[test]
    fn apply_heavy_checkpoint_fallback_restores_history_and_approval_overrides() {
        use astra_turn_core::approval_fingerprint::{ApprovalFingerprint, FingerprintedOverrides};

        let mut state = SessionState::default();
        let mut overrides = FingerprintedOverrides::default();
        overrides.insert(
            ApprovalFingerprint::shell("bash", "git commit -m 'wip'", false),
            true,
        );
        let approval_json = overrides.to_json().expect("non-empty overrides");

        let mut heavy = match astra_pipeline::step_protocol::StepCheckpoint::heavy(
            "step-1".into(),
            "task-1".into(),
            "agent-1".into(),
            Default::default(),
        ) {
            astra_pipeline::step_protocol::StepCheckpoint::Heavy(heavy) => *heavy,
            _ => unreachable!("heavy checkpoint constructor should yield Heavy"),
        };
        heavy.messages = vec![
            serde_json::json!({"role": "user", "content": "continue"}),
            serde_json::json!({"role": "assistant", "content": "done"}),
        ];
        heavy.recent_tools = vec!["rg".into()];
        heavy.blocked_tools = vec!["bash".into()];
        heavy.approval_overrides = Some(approval_json.clone());

        apply_heavy_checkpoint_fallback(&mut state, &heavy);

        assert_eq!(
            state.history,
            vec![("continue".to_string(), "done".to_string())]
        );
        assert_eq!(state.recent_tools, vec!["rg".to_string()]);
        assert!(
            state
                .tool_health_entries
                .iter()
                .any(|entry| entry.name == "bash")
        );
        let exported = state
            .perm_manager
            .export_session_overrides()
            .expect("restored approval overrides");
        assert_eq!(serde_json::to_value(exported).unwrap(), approval_json);
    }

    #[test]
    fn apply_resume_recovery_state_ignores_stall_derived_resume_restricted_tools() {
        let mut state = SessionState::default();
        apply_resume_recovery_state(
            &mut state,
            Some(&serde_json::json!({
                "kind": "budget_exhausted",
                "resumable": true,
                "stall_signal": "redundant_reads=4",
                "resume_restricted_tools": ["view", "read_file", "view"]
            })),
            None,
        );

        assert!(
            state.resume_restricted_tools.is_empty(),
            "stall-derived resume restrictions are soft guidance, not hard tool blocks"
        );
    }

    #[test]
    fn build_step_resume_guidance_decodes_compaction_state_schema() {
        let guidance = build_step_resume_guidance(
            Some(&serde_json::json!({
                "kind": "context_overflow",
                "resumable": true,
                "has_checkpoint": true,
                "tool_calls_completed": 1,
                "turns_completed": 2,
                "remaining_turns": 1,
            })),
            Some(&serde_json::json!({
                "attempt_count": 3,
                "cumulative_tokens_freed": 15000,
                "last_was_insufficient": true,
            })),
        )
        .expect("guidance");

        assert!(guidance.contains("3 attempt(s)"), "{guidance}");
        assert!(guidance.contains("15000 tokens freed"), "{guidance}");
        assert!(guidance.contains("insufficient"), "{guidance}");
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn cloud_only_typed_resume_installs_the_selected_active_conversation() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("resume-typed-cloud-{}", uuid::Uuid::new_v4());
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        let messages = vec![
            serde_json::json!({"role": "user", "content": "retain typed history"}),
            serde_json::json!({
                "role": "assistant",
                "tool_calls": [{
                    "id": "call-1",
                    "type": "function",
                    "function": {"name": "read_file", "arguments": "{\"path\":\"a\"}"}
                }]
            }),
            serde_json::json!({
                "role": "tool",
                "tool_call_id": "call-1",
                "content": "file body"
            }),
            serde_json::json!({"role": "assistant", "content": "done"}),
        ];
        let cursor = astra_turn_types::SessionCursorV1 {
            schema_version: astra_turn_types::SESSION_CURSOR_SCHEMA_VERSION,
            owner_id: crate::cli::cli_config::cli_utils::cli_user_id(),
            session_id: session_id.clone(),
            branch_id: astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID.into(),
            completed_turn: 3,
            journal_event_seq: 3,
            conversation_seq: 3,
            canonical_root_hash: astra_turn_types::canonical_conversation_root(&messages),
            projection_schema: astra_turn_types::CONVERSATION_PROJECTION_SCHEMA_VERSION,
            compaction_generation: 0,
            config_version_id: None,
        };
        let bundle = astra_turn_types::ResumeBundleV1 {
            schema_version: astra_turn_types::RESUME_BUNDLE_SCHEMA_VERSION,
            cursor: cursor.clone(),
            source: astra_turn_types::ResumeSourceV1::Checkpoint,
            conversation_messages: messages.clone(),
            materialized_conversation_root_hash: None,
            degraded_reasons: vec![astra_turn_types::ResumeDegradedReasonV1::CheckpointFallback],
            repair_actions: Vec::new(),
            projections: Default::default(),
        };
        let restored = RestoredSession {
            session_id: session_id.clone(),
            turn_count: 3,
            restored_from_cloud: true,
            resume_bundle: Some(bundle),
            last_status: "active".into(),
            ..Default::default()
        };
        let mut server_restored = restored.clone();
        server_restored.resume_bundle.as_mut().unwrap().source =
            astra_turn_types::ResumeSourceV1::CanonicalJournal;
        server_restored
            .resume_bundle
            .as_mut()
            .unwrap()
            .cursor
            .journal_event_seq = 100;
        let mut state = SessionState::default();

        apply_restored_session(None, &api, &mut state, restored)
            .await
            .expect("apply typed cloud resume");

        let active = state
            .active_conversation
            .as_ref()
            .expect("typed cloud resume must install active conversation");
        assert_eq!(active.cursor(), &cursor);
        assert_eq!(active.messages(), messages);
        assert!(
            state
                .history
                .iter()
                .any(|(_, assistant)| assistant == "done")
        );
        write_local_resumable_session(&session_id, 20);
        apply_restored_session(None, &api, &mut state, server_restored)
            .await
            .expect("Server journal authority must not compare replica clocks");
        assert_eq!(state.turn, 3);
        assert_eq!(
            state.active_conversation.as_ref().unwrap().messages(),
            messages
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn apply_restored_session_uses_checkpoint_compaction_state_for_resume_guidance() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("resume-compact-{}", uuid::Uuid::new_v4());
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        write_local_resumable_session(&session_id, 2);
        write_local_step_checkpoint_with_compaction_state(&session_id, 2);

        let restored = RestoredSession {
            session_id: session_id.clone(),
            turn_count: 2,
            model: Some("gpt-5".into()),
            last_status: "active".into(),
            ..Default::default()
        };
        let mut state = SessionState::default();
        apply_restored_session(None, &api, &mut state, restored)
            .await
            .expect("apply restored session");

        let guidance = state.resume_guidance.expect("resume guidance");
        assert!(guidance.contains("3 attempt(s)"), "{guidance}");
        assert!(guidance.contains("15000 tokens freed"), "{guidance}");
        assert!(guidance.contains("insufficient"), "{guidance}");
        assert_eq!(
            state.runtime_compaction_state,
            Some(serde_json::json!({
                "attempt_count": 3,
                "cumulative_tokens_freed": 15000,
                "last_tokens_freed": 4000,
                "last_was_insufficient": true,
            }))
        );
        assert_eq!(
            state.runtime_pipeline_state,
            Some(serde_json::json!({
                "stats": {"cache_hit_ratio_ema": 0.42},
                "recovery": {"ptl_error_count": 2},
            }))
        );
        assert_eq!(state.runtime_consecutive_context_window_errors, 2);
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn apply_restored_session_replaces_live_session_overrides() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("resume-overrides-{}", uuid::Uuid::new_v4());
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        write_local_resumable_session(&session_id, 2);

        let mut restored_overrides =
            astra_turn_core::approval_fingerprint::FingerprintedOverrides::default();
        restored_overrides.insert(
            astra_turn_core::approval_fingerprint::ApprovalFingerprint::bare("read_file"),
            true,
        );
        write_local_step_checkpoint_with_approval_overrides(
            &session_id,
            2,
            restored_overrides
                .to_json()
                .expect("restored overrides should serialize"),
        );

        let restored = RestoredSession {
            session_id: session_id.clone(),
            turn_count: 2,
            model: Some("gpt-5".into()),
            last_status: "active".into(),
            ..Default::default()
        };
        let mut state = SessionState::default();
        state.set_session_id("live-session");
        state.perm_manager.record_approval("bash", None, false);

        apply_restored_session(None, &api, &mut state, restored)
            .await
            .expect("apply restored session");

        let restored_session_overrides = state
            .perm_manager
            .export_session_overrides()
            .expect("checkpoint overrides should be restored");
        let old_bash = astra_turn_core::approval_fingerprint::ApprovalFingerprint::bare("bash");
        let restored_read_file =
            astra_turn_core::approval_fingerprint::ApprovalFingerprint::bare("read_file");
        assert_eq!(restored_session_overrides.check(&old_bash), None);
        assert_eq!(
            restored_session_overrides.check(&restored_read_file),
            Some(true)
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn apply_restored_session_rebuilds_corrupt_workspace_and_resumes() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("resume-bad-workspace-{}", uuid::Uuid::new_v4());
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        write_local_resumable_session(&session_id, 2);

        let workspace_path = astra_services::session_workspace::workspace_dir_for(&session_id)
            .join("workspace.yaml");
        std::fs::write(&workspace_path, ":\nnot-valid-yaml").unwrap();

        let restored = RestoredSession {
            session_id: session_id.clone(),
            turn_count: 2,
            model: Some("gpt-5".into()),
            last_status: "active".into(),
            ..Default::default()
        };
        let mut state = SessionState {
            session_id: Some("existing-session".into()),
            turn: 7,
            history: vec![("old".into(), "state".into())],

            ..Default::default()
        };

        apply_restored_session(None, &api, &mut state, restored)
            .await
            .expect("corrupt workspace should be rebuilt, not block resume");

        assert_eq!(state.session_id.as_deref(), Some(session_id.as_str()));
        assert_eq!(state.turn, 2);
        let warning = state
            .session_persistence_error
            .as_deref()
            .expect("rebuilt workspace should surface a degradation warning");
        assert!(
            warning.contains("workspace metadata unreadable during resume"),
            "{warning}"
        );

        let rebuilt = session_workspace::read_workspace(&session_id)
            .expect("resume should rewrite readable workspace metadata");
        assert_eq!(rebuilt.session_id, session_id);
        assert_eq!(rebuilt.turn_count, 2);
        assert!(
            rebuilt
                .last_persistence_error
                .as_deref()
                .is_some_and(|error| error.contains("workspace metadata unreadable during resume")),
            "{rebuilt:?}"
        );
        let backup_count = std::fs::read_dir(workspace_path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("workspace.yaml.corrupt-")
            })
            .count();
        assert_eq!(backup_count, 1, "corrupt workspace should be preserved");
    }

    #[test]
    fn resume_persistence_warning_formats_user_visible_notice() {
        let warning =
            resume_persistence_warning(Some("failed to append turn event: Is a directory"))
                .expect("warning");
        assert!(
            warning.contains("Session persistence degraded"),
            "{warning}"
        );
        assert!(warning.contains("failed to append turn event"), "{warning}");
        assert!(resume_persistence_warning(None).is_none());
        assert!(resume_persistence_warning(Some("   ")).is_none());
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn resume_rejects_invalid_configuration_without_rebinding_or_rewriting_workspace() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let _token_guard = crate::test_utils::ProcessEnvGuard::remove("ASTRA_ACCESS_TOKEN");
        let _creds_guard = crate::tests::isolate_credentials();
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        for invalid in [
            r#"{"verification":{"strictness":0.8}}"#,
            r#"{"token_budget":{"max_turn_input_tokens":16000}}"#,
            r#"{"tool_selection":{"max_tools_per_turn":15}}"#,
            r#"{"compression":{"compression_threshold":1.1}}"#,
        ] {
            let session_id = format!("resume-invalid-config-{}", uuid::Uuid::new_v4());
            write_local_resumable_session(&session_id, 2);
            session_workspace::update_existing_workspace_config(
                &session_id,
                |workspace| {
                    workspace.tuned_config_json = Some(invalid.into());
                    Ok::<_, std::io::Error>(std::ops::ControlFlow::<(), _>::Continue(()))
                },
                |_, _, _| Ok(()),
            )
            .unwrap();
            assert_eq!(
                session_workspace::read_workspace(&session_id)
                    .unwrap()
                    .tuned_config_json
                    .as_deref(),
                Some(invalid)
            );
            let mut state = SessionState {
                session_id: Some("current-session".into()),
                turn: 7,
                history: vec![("current query".into(), "current answer".into())],
                ..Default::default()
            };
            let error = restore_session_into_state(&session_id, None, &api, &mut state)
                .await
                .unwrap_err();
            assert!(
                error.contains("saved runtime configuration is invalid"),
                "{error}"
            );
            assert_eq!(state.session_id.as_deref(), Some("current-session"));
            assert_eq!(state.turn, 7);
            assert_eq!(
                state.history,
                vec![("current query".to_string(), "current answer".to_string())]
            );
            assert_eq!(
                session_workspace::read_workspace(&session_id)
                    .unwrap()
                    .tuned_config_json
                    .as_deref(),
                Some(invalid)
            );
            assert!(!state.observability_config_pending);
        }
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn apply_restored_session_ignores_corrupt_checkpoint_when_journal_is_recoverable() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("resume-bad-step-{}", uuid::Uuid::new_v4());
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        write_local_resumable_session(&session_id, 2);
        write_invalid_local_step_checkpoint(&session_id, 2);

        let restored = RestoredSession {
            session_id: session_id.clone(),
            turn_count: 2,
            model: Some("gpt-5".into()),
            last_status: "active".into(),
            ..Default::default()
        };
        let mut state = SessionState {
            session_id: Some("existing-session".into()),
            turn: 7,
            history: vec![("old".into(), "state".into())],
            ..Default::default()
        };

        apply_restored_session(None, &api, &mut state, restored)
            .await
            .expect("the journal is a valid degraded recovery source");

        assert_eq!(state.session_id.as_deref(), Some(session_id.as_str()));
        assert_eq!(state.turn, 2);
        assert_eq!(
            state.history,
            vec![("continue".to_string(), "restored".to_string())]
        );
        assert!(state.runtime_compaction_state.is_none());
        assert!(state.runtime_pipeline_state.is_none());
        assert!(state.tool_health_entries.is_empty());
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn apply_restored_session_surfaces_unreadable_local_journal() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("resume-bad-journal-{}", uuid::Uuid::new_v4());
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        std::fs::create_dir_all(session_journal::journal_file_path(&session_id)).unwrap();

        let restored = RestoredSession {
            session_id: session_id.clone(),
            turn_count: 2,
            model: Some("gpt-5".into()),
            last_status: "active".into(),
            restored_from_cloud: false,
            ..Default::default()
        };
        let mut state = SessionState::default();
        let error = apply_restored_session(None, &api, &mut state, restored)
            .await
            .expect_err("unreadable local journal should abort restore");

        assert!(error.contains("failed to read session journal"), "{error}");
        assert!(!error.contains("not found or not owned"), "{error}");
    }
    #[tokio::test]
    async fn apply_restored_session_keeps_local_canonical_generation_when_step_checkpoint_is_invalid()
     {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("resume-cloud-fallback-{}", uuid::Uuid::new_v4());
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        write_local_resumable_session(&session_id, 2);
        write_invalid_local_step_checkpoint(&session_id, 2);
        let mut objective =
            serde_json::json!({"role": "user", "content": "repair session lifecycle"});
        astra_turn_types::mark_user_turn_semantics(
            &mut objective,
            astra_turn_types::UserTurnSemantics::new(
                astra_turn_types::ObjectiveRelation::Replace,
                None,
            ),
        );

        let conversation_messages = vec![
            objective,
            serde_json::json!({"role": "assistant", "content": "cloud fallback"}),
        ];
        let bundle = typed_resume_bundle(&session_id, 2, conversation_messages.clone());
        let restored = RestoredSession {
            session_id: session_id.clone(),
            turn_count: 2,
            model: Some("gpt-5".into()),
            last_status: "active".into(),
            restored_from_cloud: true,
            resume_bundle: Some(bundle),
            interruption: Some(serde_json::json!({
                "kind": "context_overflow",
                "resumable": true,
                "has_checkpoint": true,
                "tool_calls_completed": 2,
                "turns_completed": 2,
                "remaining_turns": 1,
            })),
            compaction_state: Some(serde_json::json!({
                "attempt_count": 3,
                "cumulative_tokens_freed": 15000,
                "last_was_insufficient": true,
            })),
            ..Default::default()
        };
        let mut state = SessionState::default();
        apply_restored_session(None, &api, &mut state, restored)
            .await
            .expect("the canonical local journal should keep resume working");

        assert_eq!(state.session_id.as_deref(), Some(session_id.as_str()));
        let anchor = state.continuation_anchor.as_ref().expect("restored anchor");
        assert!(anchor.objective_context.is_empty());
        let guidance = state.resume_guidance.expect("local recovery guidance");
        assert!(!guidance.contains("objective: repair session lifecycle"));
        assert!(!guidance.contains("3 attempt(s)"));
        assert_eq!(state.runtime_compaction_state, None);
    }

    // ── CSL resume tests ─────────────────────────────────────────────────

    #[tokio::test]
    async fn restore_from_csl_populates_history_and_state() {
        use astra_turn_core::conversation_log::{
            AppendMeta, CslEntry, CslStore, SessionStateCompact, file_store::FileCslStore,
        };
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("csl-resume-{}", uuid::Uuid::new_v4());

        let store = FileCslStore::new(session_journal::local_owner_sessions_dir());
        let snapshot = CslEntry::Snapshot {
            seq: 0,
            turn: 1,
            messages: vec![
                serde_json::json!({"role": "user", "content": "hello"}),
                serde_json::json!({"role": "assistant", "content": "hi there"}),
            ],
            session_state: SessionStateCompact {
                recent_tools: vec!["bash".into(), "read_file".into()],
                deferred_tool_activations: vec![astra_turn_types::DeferredToolActivation {
                    name: "write_file".into(),
                    schema_digest: "sha256:write-file".into(),
                    descriptor: None,
                }],
                ..Default::default()
            },
        };
        store
            .append(&session_id, &snapshot, &AppendMeta::default())
            .await
            .unwrap();

        let delta = CslEntry::TurnDelta {
            seq: 1,
            turn: 2,
            appended: vec![
                serde_json::json!({"role": "user", "content": "what next?"}),
                serde_json::json!({"role": "assistant", "content": "let's continue"}),
            ],
            state_patch: None,
        };
        store
            .append(&session_id, &delta, &AppendMeta::default())
            .await
            .unwrap();

        let state = prepare_session_history(&session_id).await.unwrap();

        assert_eq!(state.history.len(), 2, "should have 2 user/assistant pairs");
        assert_eq!(state.history[0].0, "hello");
        assert_eq!(state.history[0].1, "hi there");
        assert_eq!(state.history[1].0, "what next?");
        assert_eq!(state.history[1].1, "let's continue");
        assert!(state.csl_manager.is_some(), "CSL manager should be set");
        assert_eq!(
            state.recent_tools,
            vec!["bash".to_string(), "read_file".to_string()],
            "should restore recent_tools from snapshot state"
        );
        assert_eq!(state.deferred_tool_activations.len(), 1);
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn restore_without_csl_falls_back_to_journal() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("no-csl-{}", uuid::Uuid::new_v4());
        write_local_resumable_session(&session_id, 3);

        let state = prepare_session_history(&session_id).await.unwrap();

        assert!(
            !state.history.is_empty(),
            "should fall back to journal history"
        );
        assert!(
            state.csl_manager.is_none(),
            "CSL manager should remain None"
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn restore_without_csl_restores_recent_tools_from_journal() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("no-csl-tools-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&session_id).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&session_id),
                Some("gpt-5"),
            ))
            .unwrap();
        writer
            .append(
                &session_journal::JournalEvent::turn(
                    Some(&session_id),
                    1,
                    Some("gpt-5"),
                    "continue",
                    "restored",
                    0,
                    10,
                    5,
                    5,
                )
                .with_tool_surface(
                    vec![],
                    vec![],
                    vec!["bash".into(), "grep".into()],
                    0,
                ),
            )
            .unwrap();

        let state = prepare_session_history(&session_id).await.unwrap();

        assert_eq!(
            state.recent_tools,
            vec!["bash".to_string(), "grep".to_string()]
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn restore_from_corrupt_csl_returns_error_instead_of_falling_back() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("corrupt-csl-{}", uuid::Uuid::new_v4());
        let store = astra_services::local_session_artifact_store();
        let session_dir = astra_services::SessionArtifactStore::session_dir(&store, &session_id)
            .expect("owner-bound test session dir");
        std::fs::create_dir_all(&session_dir).unwrap();
        write_local_resumable_session(&session_id, 2);
        std::fs::write(
            session_dir.join("conversation_log.jsonl"),
            "{\"type\":\"snapshot\",\"seq\":1,\"turn\":1,\"messages\":[]\n{\"type\":\"snapshot\",\"seq\":2,\"turn\":1,\"messages\":[],\"session_state\":{}}\n",
        )
        .unwrap();

        let error = prepare_session_history(&session_id)
            .await
            .err()
            .expect("corrupt csl should fail");

        assert!(error.contains("load CSL state"), "{error}");
    }

    // ── derive_history_pairs_from_messages tests ─────────────────────────

    #[test]
    fn derive_history_pairs_simple_conversation() {
        let messages = vec![
            serde_json::json!({"role": "user", "content": "q1"}),
            serde_json::json!({"role": "assistant", "content": "a1"}),
            serde_json::json!({"role": "user", "content": "q2"}),
            serde_json::json!({"role": "assistant", "content": "a2"}),
        ];
        let pairs =
            crate::cli::session::session_continuation::history_pairs_from_messages(&messages);
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0], ("q1".into(), "a1".into()));
        assert_eq!(pairs[1], ("q2".into(), "a2".into()));
    }

    #[test]
    fn derive_history_pairs_skips_tool_messages() {
        let messages = vec![
            serde_json::json!({"role": "user", "content": "fix the bug"}),
            serde_json::json!({"role": "assistant", "content": "I'll read the file"}),
            serde_json::json!({"role": "tool", "content": "file contents here"}),
            serde_json::json!({"role": "assistant", "content": "done, fixed it"}),
            serde_json::json!({"role": "user", "content": "thanks"}),
            serde_json::json!({"role": "assistant", "content": "you're welcome"}),
        ];
        let pairs =
            crate::cli::session::session_continuation::history_pairs_from_messages(&messages);
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].0, "fix the bug");
        assert_eq!(pairs[0].1, "I'll read the file\n\ndone, fixed it");
        assert_eq!(pairs[1].0, "thanks");
        assert_eq!(pairs[1].1, "you're welcome");
    }

    #[test]
    fn derive_history_pairs_empty_messages() {
        let pairs = crate::cli::session::session_continuation::history_pairs_from_messages(&[]);
        assert!(pairs.is_empty());
    }

    #[test]
    fn derive_history_pairs_user_only_no_assistant() {
        let messages = vec![serde_json::json!({"role": "user", "content": "hello"})];
        let pairs =
            crate::cli::session::session_continuation::history_pairs_from_messages(&messages);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0], ("hello".into(), String::new()));
    }

    #[test]
    fn derive_history_pairs_structured_content_preserves_text() {
        let messages = vec![
            serde_json::json!({"role": "user", "content": "question"}),
            serde_json::json!({"role": "assistant", "content": [{"type": "text", "text": "answer"}]}),
        ];
        let pairs =
            crate::cli::session::session_continuation::history_pairs_from_messages(&messages);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0, "question");
        assert_eq!(pairs[0].1, "answer");
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn restore_session_into_state_restores_unauthenticated_local_session() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();
        let _token_guard = crate::test_utils::ProcessEnvGuard::remove("ASTRA_ACCESS_TOKEN");
        let session_id = format!("resume-stale-{}", uuid::Uuid::new_v4());
        write_local_resumable_session(&session_id, 3);

        let server = MockServer::start().await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let mut state = SessionState {
            model: Some(
                crate::cli::session::session_state::SessionModelChoice::Selected(
                    crate::cli::session::session_runtime::ServerModelSelection {
                        name: "chosen-model(thinking:high)".into(),
                        offering_id: "chosen-offering".into(),
                        context_window: Some(64_000),
                        pricing: None,
                    },
                ),
            ),
            ..SessionState::default()
        };
        state
            .cli_context
            .select_model(Some("chosen-model(thinking:high)"));
        restore_session_into_state(&session_id, None, &api, &mut state)
            .await
            .expect("unauthenticated local journal/workspace should restore");

        assert_eq!(state.session_id.as_deref(), Some(session_id.as_str()));
        assert_eq!(state.turn, 3);
        assert_eq!(state.model.as_deref(), Some("chosen-model(thinking:high)"));
        assert_eq!(
            state.model.as_ref().and_then(|model| model.offering_id()),
            Some("chosen-offering")
        );
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn authenticated_restore_does_not_fall_back_to_local_state_on_server_failure() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();
        let session_id = format!("resume-stale-{}", uuid::Uuid::new_v4());
        write_local_resumable_session(&session_id, 3);
        let server = MockServer::start().await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        for status in [404, 403, 503] {
            write_profile_with_token(&session_id);
            Mock::given(method("POST"))
                .and(path(format!("/sessions/{session_id}/resume")))
                .and(header_exists("authorization"))
                .respond_with(
                    ResponseTemplate::new(status)
                        .set_body_json(serde_json::json!({"detail":"Session unavailable"})),
                )
                .expect(1)
                .mount(&server)
                .await;
            let mut state = SessionState {
                session_id: Some("current-session".into()),
                turn: 7,
                ..Default::default()
            };
            restore_session_into_state(&session_id, None, &api, &mut state)
                .await
                .expect_err("local replica cannot override Server failure");
            assert_eq!(state.session_id.as_deref(), Some("current-session"));
            assert_eq!(state.turn, 7);
            let credentials = crate::cli::cli_config::cli_utils::load_credentials();
            assert_eq!(
                credentials
                    .profiles
                    .get("default")
                    .and_then(|profile| profile.last_session_id.as_deref()),
                if status == 404 {
                    None
                } else {
                    Some(session_id.as_str())
                }
            );
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
            server.verify().await;
            server.reset().await;
        }
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn list_cloud_resumable_sessions_uses_server_restore_payload() {
        let _creds_guard = crate::tests::isolate_credentials();
        let _token_guard = EnvGuard::set("ASTRA_ACCESS_TOKEN", "test-token");

        let session_id = format!("cloud-list-{}", uuid::Uuid::new_v4());
        let workspace = session_workspace::WorkspaceMetadata::with_context(
            &session_id,
            "gpt-5",
            "/srv/cloud-project",
            Some("feature/cloud"),
        );

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/sessions/resumable"))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "sessions": [{
                    "session_id": session_id,
                    "turn_count": 5,
                    "total_tokens_in": 120,
                    "total_tokens_out": 45,
                    "total_cache_read_tokens": 22,
                    "total_cache_creation_tokens": 7,
                    "recent_tools": ["bash", "grep"],
                    "checkpoint_count": 2,
                    "last_status": "active",
                    "git_branch": "feature/cloud",
                    "model": "gpt-5",
                    "title": "Cloud only session",
                    "restored_from_cloud": true,
                    "workspace": workspace,
                }],
                "limit": 20
            })))
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let sessions = session_restore_client::list_cloud_resumable_sessions(None, &api)
            .await
            .expect("cloud resumable list");

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, session_id);
        assert_eq!(sessions[0].turn_count, 5);
        assert!(sessions[0].restored_from_cloud);
        assert_eq!(
            sessions[0]
                .workspace
                .as_ref()
                .map(|workspace| workspace.cwd.as_str()),
            Some("/srv/cloud-project")
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn resume_restores_full_configuration_including_explicit_defaults() {
        let _identity = crate::cli::cli_config::cli_utils::install_cli_profile_identity_for_test(
            "default", None,
        )
        .unwrap();
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let _home = crate::test_utils::HomeGuard::temp();
        let _creds_guard = crate::tests::isolate_credentials();
        let _token_guard = crate::test_utils::ProcessEnvGuard::remove("ASTRA_ACCESS_TOKEN");
        let _top_k = EnvGuard::set("ASTRA_RETRIEVAL_TOP_K", "7");
        assert_eq!(
            astra_config::RuntimeConfig::load().memory.retrieval_top_k,
            7
        );
        let session_id = format!("resume-default-config-{}", uuid::Uuid::new_v4());
        write_local_resumable_session(&session_id, 2);
        let mut saved = astra_config::RuntimeConfig::default();
        saved.memory.max_memory_tokens = 1300;
        saved.compression.preserve_recent_turns = 7;
        assert_eq!(saved.memory.retrieval_top_k, 5);
        let saved_json = serde_json::to_string(&saved).unwrap();
        session_workspace::update_existing_workspace_config(
            &session_id,
            |workspace| {
                workspace.tuned_config_json = Some(saved_json.clone());
                Ok::<_, std::io::Error>(std::ops::ControlFlow::<(), _>::Continue(()))
            },
            |_, _, _| Ok(()),
        )
        .unwrap();
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        let mut state = SessionState::default();
        state.set_session_id("current-session");
        state.config_version_id = Some("stale-config-version".into());
        let hub = std::sync::Arc::new(astra_runtime::observability::ObservabilityHub::new());
        state.observability_hub = Some(hub);
        restore_session_into_state(&session_id, None, &api, &mut state)
            .await
            .unwrap();
        assert!(!state.observability_config_pending);
        assert_eq!(
            serde_json::to_value(&state.runtime_config).unwrap(),
            serde_json::to_value(&saved).unwrap()
        );
        assert_eq!(state.context_budget.keep_recent_turns, 7);
        assert_eq!(state.context_budget.memory_budget_chars, 5200);
        let expected_version = astra_config::config_versions::VersionId::from_toml_bytes(
            toml::to_string_pretty(&saved).unwrap().as_bytes(),
        );
        assert_eq!(
            state.config_version_id.as_deref(),
            Some(expected_version.as_str())
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/memory/search"))
            .and(wiremock::matchers::body_json(
                serde_json::json!({"query": "restored config", "top_k": 5}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .expect(1)
            .mount(&server)
            .await;
        let memory_api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        crate::cli::slash::slash_memory::handle_memory_domain_command(
            "/memory",
            "search restored config",
            &memory_api,
            &mut state,
            Some("fixture-token"),
        )
        .await
        .unwrap();
        server.verify().await;
        assert_eq!(
            serde_json::to_value(
                &state
                    .observability_session
                    .as_ref()
                    .unwrap()
                    .read()
                    .unwrap()
                    .config
            )
            .unwrap(),
            serde_json::to_value(&saved).unwrap(),
        );
        // A replacement observability instance must also use the restored config.
        state.observability_session = None;
        crate::cli::session::session_startup::initialize_journal_pub(&mut state, &session_id);
        assert_eq!(
            serde_json::to_value(
                &state
                    .observability_session
                    .as_ref()
                    .unwrap()
                    .read()
                    .unwrap()
                    .config
            )
            .unwrap(),
            serde_json::to_value(&saved).unwrap()
        );
        state.set_explain_report_format_override(
            astra_config::runtime_config::ExplainReportFormat::Text,
        );
        restore_session_into_state(&session_id, None, &api, &mut state)
            .await
            .unwrap();
        saved.explain.report_format = Some(astra_config::runtime_config::ExplainReportFormat::Text);
        assert_eq!(
            serde_json::to_value(&state.runtime_config).unwrap(),
            serde_json::to_value(&saved).unwrap()
        );
        assert_eq!(
            state.config_version_id.as_deref(),
            Some(
                astra_config::config_versions::VersionId::from_toml_bytes(
                    toml::to_string_pretty(&saved).unwrap().as_bytes()
                )
                .as_str()
            )
        );

        // A rejected new-session admission must leave A's selected configuration intact.
        let fresh_server = MockServer::start().await;
        let fresh_api = astra_thin_client::ThinClient::new(&fresh_server.uri(), None).unwrap();
        Mock::given(method("POST"))
            .and(path("/sessions"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&fresh_server)
            .await;
        let previous_version = state.config_version_id.clone();
        crate::cli::slash::slash_state::start_fresh_session(
            &fresh_api,
            None,
            "fixture-token",
            &mut state,
        )
        .await
        .unwrap_err();
        fresh_server.verify().await;
        assert_eq!(state.session_id.as_deref(), Some(session_id.as_str()));
        assert_eq!(
            serde_json::to_value(&state.runtime_config).unwrap(),
            serde_json::to_value(&saved).unwrap()
        );
        assert_eq!(state.config_version_id, previous_version);
        assert_eq!(state.context_budget.keep_recent_turns, 7);
        assert_eq!(state.context_budget.memory_budget_chars, 5200);
        fresh_server.reset().await;

        let fresh_id = format!("fresh-config-{}", uuid::Uuid::new_v4());
        Mock::given(method("POST"))
            .and(path("/sessions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"session_id":fresh_id})),
            )
            .expect(1)
            .mount(&fresh_server)
            .await;
        Mock::given(method("POST"))
            .and(path("/memory/search"))
            .and(wiremock::matchers::body_json(
                serde_json::json!({"query":"fresh config","top_k":7}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .expect(2)
            .mount(&fresh_server)
            .await;
        let mut fresh_config = astra_config::RuntimeConfig::load();
        fresh_config.explain.report_format =
            Some(astra_config::runtime_config::ExplainReportFormat::Text);
        let fresh_version = astra_config::config_versions::VersionId::from_toml_bytes(
            toml::to_string_pretty(&fresh_config).unwrap().as_bytes(),
        );
        state.model = Some(
            crate::cli::session::session_state::SessionModelChoice::Selected(
                crate::cli::session::session_runtime::ServerModelSelection {
                    name: "gpt-5".into(),
                    context_window: Some(77777),
                    offering_id: "selected-offering".into(),
                    pricing: None,
                },
            ),
        );
        crate::cli::slash::slash_state::start_fresh_session(
            &fresh_api,
            None,
            "fixture-token",
            &mut state,
        )
        .await
        .unwrap();
        assert_eq!(state.context_budget.model_limit, 77777);
        assert!(
            session_workspace::read_workspace(&fresh_id)
                .unwrap()
                .tuned_config_json
                .is_none()
        );
        let events = session_journal::read_journal(&fresh_id).unwrap();
        assert!(events.iter().any(|event| event.event_type
            == session_journal::JournalEventType::ConfigChange
            && event.metadata.as_ref().unwrap()["config_version"]["to"] == fresh_version.as_str()));
        for phase in 0..2 {
            if phase == 1 {
                write_local_resumable_session(&fresh_id, 1);
                restore_session_into_state(&fresh_id, None, &api, &mut state)
                    .await
                    .unwrap();
            }
            assert_eq!(
                serde_json::to_value(&state.runtime_config).unwrap(),
                serde_json::to_value(&fresh_config).unwrap()
            );
            assert_eq!(
                state.config_version_id.as_deref(),
                Some(fresh_version.as_str())
            );
            assert_eq!(
                state.context_budget.keep_recent_turns,
                fresh_config.compression.preserve_recent_turns as usize
            );
            assert_eq!(
                state.context_budget.memory_budget_chars,
                fresh_config.memory.max_memory_tokens as usize * 4
            );
            assert_eq!(
                serde_json::to_value(
                    &state
                        .observability_session
                        .as_ref()
                        .unwrap()
                        .read()
                        .unwrap()
                        .config
                )
                .unwrap(),
                serde_json::to_value(&fresh_config).unwrap()
            );
            crate::cli::slash::slash_memory::handle_memory_domain_command(
                "/memory",
                "search fresh config",
                &fresh_api,
                &mut state,
                Some("fixture-token"),
            )
            .await
            .unwrap();
        }
        fresh_server.verify().await;
        assert_eq!(
            session_workspace::read_workspace(&session_id)
                .unwrap()
                .tuned_config_json
                .as_deref(),
            Some(saved_json.as_str())
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn restore_session_into_state_restores_workspace_scoped_state() {
        let _identity = crate::cli::cli_config::cli_utils::install_cli_profile_identity_for_test(
            "default", None,
        )
        .unwrap();
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let process_config = astra_config::RuntimeConfig::load();
        let _creds_guard = crate::tests::isolate_credentials();
        let _token_guard = crate::test_utils::ProcessEnvGuard::remove("ASTRA_ACCESS_TOKEN");
        let session_id = format!("switch-restore-{}", uuid::Uuid::new_v4());
        write_local_resumable_session(&session_id, 2);

        let mut ws = session_workspace::read_workspace(&session_id).unwrap();
        ws.discovered_skills = vec!["session-recovery".to_string()];
        ws.last_persistence_error = Some("failed to append turn event".to_string());
        session_workspace::write_workspace(&ws).unwrap();

        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        let mut state = SessionState {
            session_id: Some("current-session".into()),
            history: vec![("old".into(), "state".into())],

            ..Default::default()
        };
        state.set_session_id("current-session");
        state.runtime_config.memory.retrieval_top_k = 42;
        state.config_version_id = Some("stale-config-version".into());
        let hub = std::sync::Arc::new(astra_runtime::observability::ObservabilityHub::new());
        state.observability_hub = Some(hub);

        restore_session_into_state(&session_id, None, &api, &mut state)
            .await
            .expect("switch should reuse strict resume restore");

        assert_eq!(state.session_id.as_deref(), Some(session_id.as_str()));
        assert_eq!(state.turn, 2);
        assert_eq!(
            state.history,
            vec![("continue".to_string(), "restored".to_string())]
        );
        assert_eq!(
            session_workspace::read_workspace(&session_id)
                .unwrap()
                .discovered_skills,
            vec!["session-recovery".to_string()]
        );
        assert_eq!(
            state.session_persistence_error.as_deref(),
            Some("failed to append turn event")
        );
        assert!(!state.observability_config_pending);
        assert_eq!(
            serde_json::to_value(&state.runtime_config).unwrap(),
            serde_json::to_value(&process_config).unwrap()
        );
        assert_ne!(
            state.config_version_id.as_deref(),
            Some("stale-config-version")
        );
        assert!(
            state.journal.is_some(),
            "switch should initialize a journal"
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn restore_session_into_state_restores_cloud_only_session_and_workspace_metadata() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();
        let session_id = format!("resume-cloud-only-{}", uuid::Uuid::new_v4());
        write_profile_with_token(&session_id);

        let mut workspace = session_workspace::WorkspaceMetadata::with_context(
            &session_id,
            "gpt-5",
            "/srv/cloud-project",
            Some("feature/cloud"),
        );
        workspace.git_root = Some("/srv/cloud-project".to_string());
        workspace.git_head = Some("abc1234".to_string());
        workspace.turn_count = 3;
        workspace.total_tokens_in = 120;
        workspace.total_tokens_out = 45;
        workspace.total_cache_read_tokens = 22;
        workspace.total_cache_creation_tokens = 7;
        workspace.status = "active".to_string();
        workspace.discovered_skills = vec!["cloud-recovery".to_string()];
        workspace.last_persistence_error = Some("failed to write workspace metadata".to_string());

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/sessions/{session_id}")))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "session_id": session_id,
                "status": "active"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("/sessions/{session_id}/resume")))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "session_id": session_id,
                "turn_count": 3,
                "total_tokens_in": 120,
                "total_tokens_out": 45,
                "total_cache_read_tokens": 22,
                "total_cache_creation_tokens": 7,
                "recent_tools": ["bash", "grep"],
                "checkpoint_count": 0,
                "last_status": "active",
                "git_branch": "feature/cloud",
                "model": "gpt-5",
                "restored_from_cloud": true,
                "workspace": workspace,
                "resume_bundle": typed_resume_bundle(
                    &session_id,
                    3,
                    vec![
                        serde_json::json!({"role":"user","content":"continue"}),
                        serde_json::json!({"role":"assistant","content":"cloud restored"}),
                    ],
                ),
            })))
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let mut state = SessionState::default();
        state.set_session_id(session_id.clone());
        state.total_session_cost = Some(1.25);
        restore_session_into_state(&session_id, None, &api, &mut state)
            .await
            .expect("cloud-only restore should succeed");

        assert_eq!(state.total_session_cost, None);
        assert_eq!(state.session_id.as_deref(), Some(session_id.as_str()));
        assert_eq!(state.turn, 3);
        assert_eq!(state.total_prompt_tokens, 120);
        assert_eq!(state.total_completion_tokens, 45);
        assert_eq!(state.total_cache_read_tokens, 22);
        assert_eq!(state.total_cache_creation_tokens, 7);
        assert_eq!(
            state.session_persistence_error.as_deref(),
            Some("failed to write workspace metadata")
        );
        assert!(state.journal.is_some());

        let persisted = session_workspace::read_workspace(&session_id)
            .expect("cloud resume should persist workspace metadata");
        assert_eq!(persisted.cwd, "/srv/cloud-project");
        assert_eq!(persisted.git_root.as_deref(), Some("/srv/cloud-project"));
        assert_eq!(persisted.git_branch.as_deref(), Some("feature/cloud"));
        assert_eq!(persisted.git_head.as_deref(), Some("abc1234"));
        assert_eq!(persisted.turn_count, 3);
        assert_eq!(persisted.total_cache_read_tokens, 22);
        assert_eq!(
            persisted.discovered_skills,
            vec!["cloud-recovery".to_string()]
        );
        assert_eq!(
            persisted.last_persistence_error.as_deref(),
            Some("failed to write workspace metadata")
        );

        // The next real commit updates its own facts without replacing
        // workspace-owned skill history with a new CLI default.
        state.turn += 1;
        let mut result = crate::tests::stub_stream_result("continued");
        let learning = crate::cli::turn::turn_learning::consume_chat_turn_learning(&result);
        let primary = crate::cli::turn::turn_commit::commit_primary_turn(
            &mut state,
            "continue again",
            &mut result,
            &learning,
            std::time::Instant::now(),
        );
        assert!(primary.outcome.turn_persisted);
        primary
            .deferred_sidecars
            .expect("real deferred workspace projection")
            .execute(false)
            .unwrap();
        let committed = session_workspace::read_workspace(&session_id).unwrap();
        assert_eq!(committed.turn_count, 4);
        assert_eq!(
            committed.discovered_skills,
            vec!["cloud-recovery".to_string()]
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn restore_session_into_state_restores_live_remote_session_from_local_workspace() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();
        let session_id = format!("resume-live-{}", uuid::Uuid::new_v4());
        write_local_resumable_session(&session_id, 2);
        write_profile_with_token(&session_id);

        let server = MockServer::start().await;
        mock_canonical_resume(&server, &session_id, 3).await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let mut state = SessionState::default();
        restore_session_into_state(&session_id, None, &api, &mut state)
            .await
            .unwrap();

        assert_eq!(state.session_id.as_deref(), Some(session_id.as_str()));
        assert_eq!(state.turn, 3);
        assert!(
            state
                .history
                .iter()
                .any(|(_, answer)| answer == "remote restored")
        );
        assert_eq!(
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|request| request.url.path() == format!("/sessions/{session_id}/resume"))
                .count(),
            1
        );
        assert_eq!(state.total_prompt_tokens, 15);
        assert_eq!(state.total_completion_tokens, 7);
        assert_eq!(state.total_cache_read_tokens, 9);
        assert_eq!(state.total_cache_creation_tokens, 3);
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn apply_restored_session_uses_remote_cache_totals_without_local_journal() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        let mut state = SessionState::default();
        let session_id = format!("resume-remote-cache-{}", uuid::Uuid::new_v4());

        let conversation_messages = vec![
            serde_json::json!({"role":"user","content":"continue"}),
            serde_json::json!({"role":"assistant","content":"remote restored"}),
        ];
        let restored = astra_services::session_restore::RestoredSession {
            session_id: session_id.clone(),
            turn_count: 4,
            total_tokens_in: 120,
            total_tokens_out: 30,
            total_cache_read_tokens: 44,
            total_cache_creation_tokens: 11,
            last_status: "active".to_string(),
            restored_from_cloud: true,
            resume_bundle: Some(typed_resume_bundle(
                &session_id,
                4,
                conversation_messages.clone(),
            )),
            ..Default::default()
        };

        apply_restored_session(None, &api, &mut state, restored)
            .await
            .expect("remote restore should succeed without local journal");

        assert_eq!(state.session_id.as_deref(), Some(session_id.as_str()));
        assert_eq!(state.total_prompt_tokens, 120);
        assert_eq!(state.total_completion_tokens, 30);
        assert_eq!(state.total_cache_read_tokens, 44);
        assert_eq!(state.total_cache_creation_tokens, 11);

        let workspace = astra_services::session_workspace::read_workspace(&session_id)
            .expect("resume should recreate workspace metadata");
        assert_eq!(workspace.turn_count, 4);
        assert_eq!(workspace.total_cache_read_tokens, 44);
        assert_eq!(workspace.total_cache_creation_tokens, 11);
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn restore_session_into_state_merges_session_memory_into_anchor() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();
        let session_id = format!("resume-memory-anchor-{}", uuid::Uuid::new_v4());
        write_local_resumable_session(&session_id, 2);
        write_profile_with_token(&session_id);

        let memory_body = "# Session Memory

## Active Goals
- Improve prompt cache behavior

## Pending Todos
- Add shutdown flush

## Current State
- Resume should carry session memory forward

## Completed
- Removed legacy memory extraction
";
        let encoded = astra_runtime::session_memory::runner::encode_session_memory_entry(
            &session_id,
            memory_body,
        );

        let server = MockServer::start().await;
        mock_canonical_resume(&server, &session_id, 3).await;
        Mock::given(method("POST"))
            .and(path("/memory/retrieve"))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "memories": [{
                    "memory_id": "mem-1",
                    "content": encoded,
                    "memory_type": "working",
                    "session_id": session_id
                }]
            })))
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let mut state = SessionState::default();
        restore_session_into_state(&session_id, None, &api, &mut state)
            .await
            .unwrap();

        let anchor = state.continuation_anchor.expect("continuation anchor");
        assert!(anchor.contains("[Session memory recap]"), "{anchor}");
        assert!(anchor.contains("Improve prompt cache behavior"), "{anchor}");
        assert!(anchor.contains("Add shutdown flush"), "{anchor}");
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn restore_session_into_state_does_not_promote_workspace_model_into_canonical_resume() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();
        let session_id = format!("resume-default-model-{}", uuid::Uuid::new_v4());
        write_local_resumable_session(&session_id, 2);
        let mut ws = session_workspace::read_workspace(&session_id).unwrap();
        ws.model = Some("default".to_string());
        session_workspace::write_workspace(&ws).unwrap();
        write_profile_with_token(&session_id);

        let server = MockServer::start().await;
        mock_canonical_resume(&server, &session_id, 3).await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let mut state = SessionState {
            model: Some(("default".to_string()).into()),
            ..SessionState::default()
        };
        restore_session_into_state(&session_id, None, &api, &mut state)
            .await
            .unwrap();

        assert_eq!(state.session_id.as_deref(), Some(session_id.as_str()));
        assert_eq!(state.model.as_deref(), None);
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn restore_session_into_state_does_not_promote_workspace_permission_mode() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();
        let session_id = format!("resume-mode-{}", uuid::Uuid::new_v4());
        write_local_resumable_session(&session_id, 2);
        let mut ws = session_workspace::read_workspace(&session_id).unwrap();
        ws.permission_mode = Some("plan".to_string());
        session_workspace::write_workspace(&ws).unwrap();
        write_profile_with_token(&session_id);

        let server = MockServer::start().await;
        mock_canonical_resume(&server, &session_id, 3).await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let mut state = SessionState::default();
        state.perm_manager.set_mode(PermissionMode::Auto);
        restore_session_into_state(&session_id, None, &api, &mut state)
            .await
            .unwrap();

        assert_eq!(state.session_id.as_deref(), Some(session_id.as_str()));
        assert_eq!(state.perm_manager.mode(), PermissionMode::Auto);
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn restore_session_into_state_ignores_noncanonical_workspace_permission_mode() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();
        let session_id = format!("resume-invalid-mode-{}", uuid::Uuid::new_v4());
        write_local_resumable_session(&session_id, 2);
        let mut ws = session_workspace::read_workspace(&session_id).unwrap();
        ws.permission_mode = Some("yolo".to_string());
        session_workspace::write_workspace(&ws).unwrap();
        write_profile_with_token(&session_id);

        let server = MockServer::start().await;
        mock_canonical_resume(&server, &session_id, 3).await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let mut state = SessionState {
            session_id: Some("current-session".into()),
            model: Some("gpt-4o".into()),
            ..SessionState::default()
        };
        state.perm_manager.set_mode(PermissionMode::Auto);

        restore_session_into_state(&session_id, None, &api, &mut state)
            .await
            .expect("workspace metadata is not canonical resume authority");

        assert_eq!(state.session_id.as_deref(), Some(session_id.as_str()));
        assert_eq!(state.model.as_deref(), None);
        assert_eq!(state.perm_manager.mode(), PermissionMode::Auto);
    }
}

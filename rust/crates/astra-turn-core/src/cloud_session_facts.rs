//! Journal ingestion adapter for shared session facts.
//!
//! The pure `SessionFacts` data model lives in `astra-turn-types`; this module
//! keeps service-layer journal record ingestion in `astra-turn-core` so the type
//! crate remains independent of restore/journal services.

pub use astra_turn_types::session_facts::*;

use astra_services::session_journal::{JournalEvent, ToolCallRecord};

const MAX_ACTIVE_FILES: usize = 20;
const MAX_RECENT_TOOLS: usize = 10;

/// Incremental update from a single turn's journal event.
pub fn update_from_journal_event(facts: &mut SessionFacts, event: &JournalEvent) {
    if let Some(t) = event.turn {
        facts.turn = t;
    }
    if let Some(tokens) = event.tokens_in {
        facts.estimated_tokens += tokens;
    }

    if let Some(tool_calls) = &event.tool_calls {
        for tc in tool_calls {
            if tc.is_synthetic_placeholder() {
                continue;
            }
            if let Some(path) = extract_file_path(tc) {
                upsert_file(facts, path, action_for_tool(&tc.name), facts.turn);
            }
            facts.recent_tool_calls.push(ToolFact {
                name: tc.name.clone(),
                ok: tc.ok,
                turn: facts.turn,
            });
            if facts.recent_tool_calls.len() > MAX_RECENT_TOOLS {
                facts.recent_tool_calls.remove(0);
            }
        }
    }

    if let Some(err) = &event.error {
        facts.error_state.total_errors += 1;
        facts.error_state.last_error = Some(truncate(err, 200));
        facts.error_state.last_error_turn = Some(facts.turn);
    }
}

fn upsert_file(facts: &mut SessionFacts, path: String, action: String, turn: u32) {
    if let Some(entry) = facts.active_files.iter_mut().find(|f| f.path == path) {
        entry.last_action = action;
        entry.turn = turn;
    } else {
        facts.active_files.push(FileEntry {
            path,
            last_action: action,
            turn,
        });
        if facts.active_files.len() > MAX_ACTIVE_FILES {
            facts.active_files.remove(0);
        }
    }
}

/// Extract file path from a ToolCallRecord.
/// Uses `file_path` field if available, falls back to parsing `args_full` (untruncated)
/// and finally `args_preview` (which may be truncated mid-path).
fn extract_file_path(tc: &ToolCallRecord) -> Option<String> {
    if let Some(fp) = &tc.file_path
        && !fp.is_empty()
    {
        return Some(fp.clone());
    }
    if let Some(full) = tc.args_full.as_deref()
        && let Some(path) = parse_path_from_json_preview(full)
    {
        return Some(path);
    }
    let preview = tc.args_preview.as_deref()?;
    parse_path_from_json_preview(preview)
}

/// Best-effort extraction of "path" field from a truncated JSON preview.
fn parse_path_from_json_preview(preview: &str) -> Option<String> {
    let idx = preview.find("\"path\"")?;
    let rest = &preview[idx + 6..];
    let colon = rest.find(':')?;
    let after_colon = rest[colon + 1..].trim_start();
    if !after_colon.starts_with('"') {
        return None;
    }
    let content = &after_colon[1..];
    let end = content.find('"')?;
    let path = &content[..end];
    if path.is_empty() || path.contains("\\n") {
        return None;
    }
    Some(path.to_string())
}

fn action_for_tool(tool_name: &str) -> String {
    match tool_name {
        "create_file" => "create".to_string(),
        "write_to_file" | "write_file" => "write".to_string(),
        "edit_file" | "str_replace" | "str_replace_editor" | "multi_edit" | "delete_file" => {
            "write".to_string()
        }
        "insert_content" | "append_to_file" => "write".to_string(),
        _ => "read".to_string(),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let boundary = s.floor_char_boundary(max);
        format!("{}…", &s[..boundary])
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::session_journal::{JournalEvent, JournalEventType, ToolCallRecord};

    fn make_tc(
        name: &str,
        ok: bool,
        file_path: Option<&str>,
        args: Option<&str>,
    ) -> ToolCallRecord {
        ToolCallRecord {
            name: name.to_string(),
            ok,
            ms: 100,
            error: None,
            input_bytes: None,
            output_bytes: None,
            args_preview: args.map(|s| s.to_string()),
            result_preview: None,
            file_path: file_path.map(|s| s.to_string()),
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        }
    }

    fn make_event(turn: u32, tool_calls: Vec<ToolCallRecord>) -> JournalEvent {
        let mut event = JournalEvent::base_public(JournalEventType::Turn, None);
        event.turn = Some(turn);
        event.tokens_in = Some(1000);
        event.tool_calls = Some(tool_calls);
        event
    }

    #[test]
    fn update_tracks_files_from_file_path_field() {
        let mut facts = SessionFacts::default();
        let event = make_event(
            1,
            vec![
                make_tc("read_file", true, Some("src/main.rs"), None),
                make_tc("str_replace", true, Some("src/lib.rs"), None),
            ],
        );
        update_from_journal_event(&mut facts, &event);

        assert_eq!(facts.active_files.len(), 2);
        assert_eq!(facts.active_files[0].path, "src/main.rs");
        assert_eq!(facts.active_files[0].last_action, "read");
        assert_eq!(facts.active_files[1].path, "src/lib.rs");
        assert_eq!(facts.active_files[1].last_action, "write");
    }

    #[test]
    fn update_tracks_create_and_recent_file_mutation_actions() {
        let mut facts = SessionFacts::default();
        let event = make_event(
            1,
            vec![
                make_tc("create_file", true, Some("new.rs"), None),
                make_tc("write_file", true, Some("existing.rs"), None),
                make_tc("multi_edit", true, Some("batch.rs"), None),
                make_tc("delete_file", true, Some("gone.rs"), None),
            ],
        );
        update_from_journal_event(&mut facts, &event);

        assert_eq!(facts.active_files.len(), 4);
        assert_eq!(facts.active_files[0].last_action, "create");
        assert_eq!(facts.active_files[1].last_action, "write");
        assert_eq!(facts.active_files[2].last_action, "write");
        assert_eq!(facts.active_files[3].last_action, "write");
    }

    #[test]
    fn update_falls_back_to_args_preview() {
        let mut facts = SessionFacts::default();
        let event = make_event(
            1,
            vec![make_tc(
                "read_file",
                true,
                None,
                Some(r#"{"path":"src/foo.rs"}"#),
            )],
        );
        update_from_journal_event(&mut facts, &event);

        assert_eq!(facts.active_files.len(), 1);
        assert_eq!(facts.active_files[0].path, "src/foo.rs");
    }

    #[test]
    fn update_prefers_args_full_over_truncated_args_preview() {
        // Repro of a real journal hazard: when `file_path` is missing
        // (legacy/older record) and `args_preview` was truncated mid-path,
        // the fallback parser used to return a wrong/partial path. Records
        // that carry the untruncated `args_full` must use that instead.
        let mut facts = SessionFacts::default();
        let truncated_preview = r#"{"path":"rust/crates/astra-cli/src/edge_tools/file_state_legac"#;
        let full = r#"{"path":"rust/crates/astra-cli/src/edge_tools/file_state_legacy_helpers.rs","old_str":"foo","new_str":"bar"}"#;
        let mut tc = make_tc("str_replace", true, None, Some(truncated_preview));
        tc.args_full = Some(full.to_string());
        let event = make_event(1, vec![tc]);
        update_from_journal_event(&mut facts, &event);

        assert_eq!(facts.active_files.len(), 1);
        assert_eq!(
            facts.active_files[0].path,
            "rust/crates/astra-cli/src/edge_tools/file_state_legacy_helpers.rs",
            "extractor must read the untruncated args_full, not the truncated preview"
        );
    }

    #[test]
    fn upsert_updates_existing_file() {
        let mut facts = SessionFacts::default();
        let e1 = make_event(1, vec![make_tc("read_file", true, Some("a.rs"), None)]);
        let e2 = make_event(2, vec![make_tc("str_replace", true, Some("a.rs"), None)]);
        update_from_journal_event(&mut facts, &e1);
        update_from_journal_event(&mut facts, &e2);

        assert_eq!(facts.active_files.len(), 1);
        assert_eq!(facts.active_files[0].last_action, "write");
        assert_eq!(facts.active_files[0].turn, 2);
    }

    #[test]
    fn caps_active_files_at_max() {
        let mut facts = SessionFacts::default();
        for i in 0..25 {
            let event = make_event(
                i,
                vec![make_tc(
                    "read_file",
                    true,
                    Some(&format!("file_{i}.rs")),
                    None,
                )],
            );
            update_from_journal_event(&mut facts, &event);
        }
        assert_eq!(facts.active_files.len(), MAX_ACTIVE_FILES);
        // Oldest should be dropped
        assert_eq!(facts.active_files[0].path, "file_5.rs");
    }

    #[test]
    fn tracks_errors() {
        let mut facts = SessionFacts::default();
        let mut event = make_event(3, vec![]);
        event.error = Some("sqlx migration failed".to_string());
        update_from_journal_event(&mut facts, &event);

        assert_eq!(facts.error_state.total_errors, 1);
        assert_eq!(
            facts.error_state.last_error.as_deref(),
            Some("sqlx migration failed")
        );
        assert_eq!(facts.error_state.last_error_turn, Some(3));
    }

    #[test]
    fn tracks_tool_outcomes() {
        let mut facts = SessionFacts::default();
        let event = make_event(
            1,
            vec![
                make_tc("read_file", true, None, None),
                make_tc("bash", false, None, None),
            ],
        );
        update_from_journal_event(&mut facts, &event);

        assert_eq!(facts.recent_tool_calls.len(), 2);
        assert!(facts.recent_tool_calls[0].ok);
        assert!(!facts.recent_tool_calls[1].ok);
    }

    #[test]
    fn caps_recent_tools_at_max() {
        let mut facts = SessionFacts::default();
        for i in 0..15 {
            let event = make_event(i, vec![make_tc("bash", true, None, None)]);
            update_from_journal_event(&mut facts, &event);
        }
        assert_eq!(facts.recent_tool_calls.len(), MAX_RECENT_TOOLS);
    }

    #[test]
    fn is_active_file_respects_recency() {
        let mut facts = SessionFacts::default();
        let e1 = make_event(1, vec![make_tc("read_file", true, Some("old.rs"), None)]);
        let e2 = make_event(10, vec![make_tc("read_file", true, Some("new.rs"), None)]);
        update_from_journal_event(&mut facts, &e1);
        update_from_journal_event(&mut facts, &e2);

        assert!(facts.is_active_file("new.rs", 5));
        assert!(!facts.is_active_file("old.rs", 5));
        assert!(facts.is_active_file("old.rs", 20)); // wider window
    }

    #[test]
    fn pending_relevant_file_matches_current_subtask() {
        let facts = SessionFacts {
            plan_state: Some(PlanFact {
                goal: "fix compaction".to_string(),
                completed: 1,
                total: 3,
                current_subtask: Some(
                    "preserve rust/crates/runtime/src/server/run_lifecycle.rs while validating"
                        .to_string(),
                ),
            }),
            ..Default::default()
        };

        assert!(facts.is_pending_relevant_file("rust/crates/runtime/src/server/run_lifecycle.rs"));
        assert!(!facts.is_pending_relevant_file("rust/crates/runtime/src/other.rs"));
    }

    #[test]
    fn to_injection_format() {
        let facts = SessionFacts {
            turn: 5,
            estimated_tokens: 25000,
            active_files: vec![FileEntry {
                path: "src/main.rs".to_string(),
                last_action: "write".to_string(),
                turn: 5,
            }],
            error_state: ErrorFact {
                total_errors: 1,
                last_error: Some("compile error".to_string()),
                ..Default::default()
            },
            blocked_tools: vec!["web_fetch".to_string()],
            ..Default::default()
        };

        let injection = facts.to_injection();
        assert!(injection.contains("Turn 5, ~25K tokens"));
        assert!(injection.contains("write src/main.rs (t5)"));
        assert!(injection.contains("Errors: 1 total, last: compile error"));
        assert!(injection.contains("Blocked tools: web_fetch"));
    }

    #[test]
    fn working_set_injection_has_stable_order_and_preserves_key_facts() {
        let facts = SessionFacts {
            turn: 5,
            active_files: vec![
                FileEntry {
                    path: "src/z.rs".to_string(),
                    last_action: "read".to_string(),
                    turn: 4,
                },
                FileEntry {
                    path: "src/a.rs".to_string(),
                    last_action: "write".to_string(),
                    turn: 5,
                },
            ],
            recent_tool_calls: vec![
                ToolFact {
                    name: "read_file".to_string(),
                    ok: true,
                    turn: 4,
                },
                ToolFact {
                    name: "str_replace".to_string(),
                    ok: false,
                    turn: 5,
                },
            ],
            plan_state: Some(PlanFact {
                goal: "fix context continuity".to_string(),
                completed: 1,
                total: 3,
                current_subtask: Some("add canonical working set".to_string()),
            }),
            blocked_tools: vec!["str_replace".to_string(), "web_fetch".to_string()],
            error_state: ErrorFact {
                total_errors: 2,
                last_error: Some("old_str not found".to_string()),
                last_error_turn: Some(5),
            },
            estimated_tokens: 0,
        };

        let injection = facts.to_working_set_injection("fallback goal");
        assert!(injection.starts_with("[working-set:v1]\n"));
        assert!(injection.contains("goal: fix context continuity\n"));
        assert!(injection.contains("pending_work: add canonical working set\n"));
        assert!(
            injection.find("- src/a.rs").unwrap() < injection.find("- src/z.rs").unwrap(),
            "active files should be sorted for deterministic rendering: {injection}"
        );
        assert!(injection.contains("- read_file [ok t4]"));
        assert!(injection.contains("- str_replace [error t5]"));
        assert!(injection.contains("- blocked: str_replace, web_fetch"));
        assert!(injection.contains("- errors: 2 total, last: old_str not found"));
    }

    #[test]
    fn working_set_injection_uses_none_placeholders_for_empty_sections() {
        let facts = SessionFacts::default();
        let injection = facts.to_working_set_injection("");
        assert!(injection.contains("goal: none\n"));
        assert!(injection.contains("pending_work: none\n"));
        assert!(injection.contains("active_files:\n- none\n"));
        assert!(injection.contains("recent_tools:\n- none\n"));
        assert!(injection.contains("tool_risks:\n- none\n"));
    }

    #[test]
    fn parse_path_from_json_preview_works() {
        assert_eq!(
            parse_path_from_json_preview(r#"{"path":"src/lib.rs","old_str":"fn main"}"#),
            Some("src/lib.rs".to_string())
        );
        assert_eq!(
            parse_path_from_json_preview(r#"{"path": "a/b.rs"}"#),
            Some("a/b.rs".to_string())
        );
        // Truncated — no closing quote
        assert_eq!(
            parse_path_from_json_preview(r#"{"path":"src/very/long/path/that/gets/trun"#),
            None
        );
        // No path field
        assert_eq!(parse_path_from_json_preview(r#"{"query":"test"}"#), None);
    }

    #[test]
    fn accumulates_tokens() {
        let mut facts = SessionFacts::default();
        let e1 = make_event(1, vec![]);
        let e2 = make_event(2, vec![]);
        update_from_journal_event(&mut facts, &e1);
        update_from_journal_event(&mut facts, &e2);
        assert_eq!(facts.estimated_tokens, 2000);
    }
}

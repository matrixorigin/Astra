use std::time::Duration;

use astra_text_utils::str_preview::truncate_str;
use astra_tools::task_mgmt::{SessionTask, unresolved_task_blocker_ids};

use crate::cli::session::session_state::{ContinuationAnchor, SessionState};
use crate::cli::stream::streaming_types::StreamResult;
use crate::cli::surface::session_task_surface::session_task_active_priority;
use astra_services::session_journal;

/// The task board is useful continuation context, not part of the turn's
/// durable commit. In Edge+Server it is an HTTP projection and must never
/// leave the user waiting for the request timeout after model output arrived.
const ACTIVE_TASK_ANCHOR_REFRESH_BUDGET: Duration = Duration::from_millis(250);

fn summarize_assistant_for_anchor(full_text: &str) -> Option<String> {
    let mut lines = Vec::new();
    let mut total_chars = 0usize;

    for line in full_text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if lines.len() >= 3 || total_chars >= 420 {
            break;
        }
        let clipped = truncate_str(line, 160);
        total_chars += clipped.chars().count();
        lines.push(clipped);
    }

    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

fn summarize_anchor_artifacts(result: &StreamResult) -> Vec<String> {
    let mut lines = Vec::new();
    if !result.tools_used.is_empty() {
        lines.push(format!(
            "Recent tools: {}",
            result
                .tools_used
                .iter()
                .take(5)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    for call in result.tool_call_records.iter().take(3) {
        if let Some(preview) = call
            .args_preview
            .as_deref()
            .filter(|preview| !preview.trim().is_empty())
        {
            lines.push(format!(
                "Artifact: {} -> {}",
                call.name,
                truncate_str(preview.trim(), 120)
            ));
        }
    }

    lines
}

fn summarize_event_anchor_artifacts(event: Option<&session_journal::JournalEvent>) -> Vec<String> {
    let Some(event) = event else {
        return Vec::new();
    };

    let mut lines = Vec::new();
    if let Some(tools_used) = event.tools_used.as_ref()
        && !tools_used.is_empty()
    {
        lines.push(format!(
            "Recent tools: {}",
            tools_used
                .iter()
                .take(5)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    if let Some(tool_calls) = event.tool_calls.as_ref() {
        for call in tool_calls.iter().take(3) {
            if let Some(preview) = call
                .args_preview
                .as_deref()
                .filter(|preview| !preview.trim().is_empty())
            {
                lines.push(format!(
                    "Artifact: {} -> {}",
                    call.name,
                    truncate_str(preview.trim(), 120)
                ));
            }
        }
    }

    lines
}

fn prioritize_active_tasks_for_anchor(tasks: Vec<SessionTask>) -> Vec<SessionTask> {
    let mut active = tasks
        .iter()
        .filter(|task| task.status.is_open_work())
        .map(|task| {
            let mut projected = task.clone();
            projected.blocked_by = unresolved_task_blocker_ids(&tasks, task);
            projected
        })
        .collect::<Vec<_>>();
    active.sort_by_key(|task| {
        (
            session_task_active_priority(task.status),
            !task.blocked_by.is_empty(),
        )
    });
    active
}

fn compact_blocker_ids(blockers: &[String]) -> String {
    const MAX_IDS: usize = 3;
    let mut ids = blockers.iter().take(MAX_IDS).cloned().collect::<Vec<_>>();
    if blockers.len() > MAX_IDS {
        ids.push(format!("+{} more", blockers.len() - MAX_IDS));
    }
    ids.join(", ")
}

fn active_task_anchor_items(active_tasks: &[SessionTask]) -> Vec<String> {
    active_tasks
        .iter()
        .take(3)
        .map(|task| {
            let blocked = if task.blocked_by.is_empty() {
                String::new()
            } else {
                format!(" [blocked by: {}]", compact_blocker_ids(&task.blocked_by))
            };
            format!(
                "[{}] {}: {}{}",
                task.status,
                task.id,
                truncate_str(&task.title, 120),
                blocked
            )
        })
        .collect()
}

fn active_task_anchor_section(active_tasks: &[SessionTask]) -> Option<String> {
    if active_tasks.is_empty() {
        return None;
    }
    let items = active_task_anchor_items(active_tasks);
    let mut lines = active_task_anchor_section_from_items(&items)?
        .lines()
        .map(str::to_string)
        .collect::<Vec<_>>();
    if active_tasks.len() > 3 {
        lines.push(format!(
            "- ... {} more active task(s)",
            active_tasks.len() - 3
        ));
    }
    Some(lines.join("\n"))
}

fn active_task_anchor_section_from_items(items: &[String]) -> Option<String> {
    if items.is_empty() {
        return None;
    }
    let mut lines = vec!["Active task board:".to_string()];
    for item in items {
        lines.push(format!("- {item}"));
    }
    Some(lines.join("\n"))
}

fn extract_session_memory_section(md: &str, section_name: &str) -> Option<String> {
    let header = format!("## {section_name}");
    let start = md.find(&header)?;
    let content_start = md[start..].find('\n').map(|i| start + i + 1)?;
    let rest = &md[content_start..];
    let next_section = rest
        .find("\n## ")
        .map(|i| content_start + i)
        .unwrap_or(md.len());
    Some(md[content_start..next_section].to_string())
}

fn session_memory_recap(memory_md: &str) -> Option<String> {
    const SECTIONS: &[(&str, &str, usize)] = &[
        ("Active Goals", "Session goals", 2),
        ("Pending Todos", "Session pending", 3),
        ("Current State", "Session state", 2),
        ("Errors & Corrections", "Session corrections", 2),
        ("Completed", "Session completed", 2),
    ];
    let mut blocks = Vec::new();
    let mut total_chars = 0usize;
    for (section, label, max_lines) in SECTIONS {
        let Some(content) = extract_session_memory_section(memory_md, section) else {
            continue;
        };
        let lines: Vec<String> = content
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with("<!--"))
            .map(|line| line.trim_start_matches("- ").to_string())
            .take(*max_lines)
            .collect();
        if lines.is_empty() {
            continue;
        }
        let block = format!("{label}:\n- {}", lines.join("\n- "));
        total_chars += block.len();
        if total_chars > 700 {
            break;
        }
        blocks.push(block);
    }
    if blocks.is_empty() {
        None
    } else {
        Some(blocks.join("\n"))
    }
}

pub(crate) fn merge_continuation_anchor_with_session_memory(
    anchor: Option<ContinuationAnchor>,
    session_memory_markdown: Option<&str>,
) -> Option<ContinuationAnchor> {
    let Some(recap) = session_memory_markdown.and_then(session_memory_recap) else {
        return anchor;
    };
    if anchor
        .as_deref()
        .is_some_and(|existing| existing.contains("[Session memory recap]"))
    {
        return anchor;
    }
    let merged = match anchor {
        Some(anchor) if !anchor.trim().is_empty() => {
            return Some(ContinuationAnchor::from_parts(
                truncate_str(
                    &format!("{}\n\n[Session memory recap]\n{recap}", anchor.text),
                    900,
                ),
                anchor.latest_user_task,
                anchor.assistant_direction,
                anchor.active_task_board,
            ));
        }
        _ => format!("[Session memory recap]\n{recap}"),
    };
    Some(ContinuationAnchor::from_parts(
        truncate_str(&merged, 900),
        None,
        None,
        Vec::new(),
    ))
}

async fn load_active_tasks_for_anchor(state: &SessionState) -> Result<Vec<SessionTask>, String> {
    state
        .task_manager
        .load_tasks()
        .await
        .map(prioritize_active_tasks_for_anchor)
}

fn build_continuation_anchor_with_active_tasks(
    state: &SessionState,
    line: &str,
    result: &StreamResult,
    active_tasks: &[SessionTask],
) -> Option<ContinuationAnchor> {
    let user_line = line.trim();
    if user_line.is_empty() {
        return state.continuation_anchor.clone();
    }

    let latest_user_task = anchor_worthy_user_input(user_line)
        .then(|| truncate_str(user_line, 220).to_string())
        .or_else(|| {
            state
                .continuation_anchor
                .as_ref()
                .and_then(|anchor| anchor.latest_user_task.clone())
                .filter(|task| anchor_worthy_user_input(task))
        });
    let mut sections = Vec::new();
    if let Some(user_summary) = latest_user_task.as_deref() {
        sections.push(format!("Latest user task: {user_summary}"));
    }
    let active_task_board = active_task_anchor_items(active_tasks);
    if let Some(task_section) = active_task_anchor_section(active_tasks) {
        sections.push(task_section);
    }

    let assistant_summary = summarize_assistant_for_anchor(&result.full_text);
    let assistant_direction = assistant_summary
        .as_deref()
        .map(|summary| summary.lines().collect::<Vec<_>>().join(" "));
    if let Some(assistant_summary) = assistant_summary {
        sections.push(format!("Latest assistant summary:\n{assistant_summary}"));
    }

    let artifact_lines = summarize_anchor_artifacts(result);
    if !artifact_lines.is_empty() {
        sections.extend(artifact_lines);
    }

    if sections.is_empty() {
        None
    } else {
        Some(ContinuationAnchor::from_parts(
            sections.join("\n"),
            latest_user_task,
            assistant_direction,
            active_task_board,
        ))
    }
}

pub(crate) fn build_continuation_anchor(
    state: &SessionState,
    line: &str,
    result: &StreamResult,
) -> Option<ContinuationAnchor> {
    build_continuation_anchor_with_active_tasks(state, line, result, &[])
}

fn rebuild_continuation_anchor_from_state_with_active_tasks(
    state: &mut SessionState,
    active_tasks: &[SessionTask],
) {
    rebuild_continuation_anchor_from_state_with_active_task_items(
        state,
        active_task_anchor_items(active_tasks),
    );
}

fn rebuild_continuation_anchor_from_state_with_active_task_items(
    state: &mut SessionState,
    active_task_board: Vec<String>,
) {
    state.last_response = state.history.last().map(|(_, assistant)| assistant.clone());

    let Some((user_line, assistant_text)) = state.history.last() else {
        state.continuation_anchor = None;
        return;
    };
    if user_line.trim().is_empty() {
        state.continuation_anchor = None;
        return;
    }

    let latest_user_task = anchor_worthy_user_input(user_line)
        .then(|| truncate_str(user_line, 220).to_string())
        .or_else(|| {
            state
                .continuation_anchor
                .as_ref()
                .and_then(|anchor| anchor.latest_user_task.clone())
                .filter(|task| anchor_worthy_user_input(task))
        });
    let mut sections = Vec::new();
    if let Some(user_summary) = latest_user_task.as_deref() {
        sections.push(format!("Latest user task: {user_summary}"));
    }
    if let Some(task_section) = active_task_anchor_section_from_items(&active_task_board) {
        sections.push(task_section);
    }

    let assistant_summary = summarize_assistant_for_anchor(assistant_text);
    let assistant_direction = assistant_summary
        .as_deref()
        .map(|summary| summary.lines().collect::<Vec<_>>().join(" "));
    if let Some(assistant_summary) = assistant_summary {
        sections.push(format!("Latest assistant summary:\n{assistant_summary}"));
    }

    sections.extend(summarize_event_anchor_artifacts(
        state.last_turn_event.as_ref(),
    ));
    state.continuation_anchor = Some(ContinuationAnchor::from_parts(
        sections.join("\n"),
        latest_user_task,
        assistant_direction,
        active_task_board,
    ));
}

fn anchor_worthy_user_input(user_line: &str) -> bool {
    astra_turn_types::should_store_in_memory(
        &serde_json::json!({"role": "user", "content": user_line}),
    )
}

pub(crate) async fn rebuild_continuation_anchor_from_live_state(state: &mut SessionState) {
    let preserved_task_board = state
        .continuation_anchor
        .as_ref()
        .map(|anchor| anchor.active_task_board.clone())
        .unwrap_or_default();
    match tokio::time::timeout(
        ACTIVE_TASK_ANCHOR_REFRESH_BUDGET,
        load_active_tasks_for_anchor(state),
    )
    .await
    {
        Ok(Ok(active_tasks)) => {
            rebuild_continuation_anchor_from_state_with_active_tasks(state, &active_tasks);
        }
        Ok(Err(error)) => {
            tracing::warn!(
                session_id = %state.task_manager.session_id(),
                error = %error,
                "failed to refresh active task board for continuation anchor; preserving previous anchor task board"
            );
            rebuild_continuation_anchor_from_state_with_active_task_items(
                state,
                preserved_task_board,
            );
        }
        Err(_) => {
            tracing::debug!(
                session_id = %state.task_manager.session_id(),
                budget_ms = ACTIVE_TASK_ANCHOR_REFRESH_BUDGET.as_millis() as u64,
                "active task board refresh exceeded post-output budget; preserving previous anchor task board"
            );
            rebuild_continuation_anchor_from_state_with_active_task_items(
                state,
                preserved_task_board,
            );
        }
    }
}

pub(crate) fn history_as_messages(history: &[(String, String)]) -> Vec<serde_json::Value> {
    history
        .iter()
        .flat_map(|(user, assistant)| {
            let mut pair = Vec::with_capacity(2);
            if !user.is_empty() {
                pair.push(serde_json::json!({"role":"user","content":user}));
            }
            if !assistant.is_empty() {
                pair.push(serde_json::json!({"role":"assistant","content":assistant}));
            }
            pair
        })
        .collect()
}

/// Checkpoint-derived metadata available to CSL projection.
///
/// This is intentionally empty: the conversation state log is prompt material,
/// not execution policy. Runtime controls such as blocked tools, approvals,
/// interruptions, budget pressure, and compaction counters belong to explicit
/// heavy checkpoints and must not accumulate in CSL across turns.
#[derive(Default)]
pub(crate) struct CslCheckpointFields;

/// Build the prompt-facing CSL state for the current turn.
///
/// Runtime-control fields are intentionally reset instead of falling back to
/// previous CSL materialization. Long-running sessions must not let a temporary
/// blocked tool, approval, budget, interruption, or compaction failure become a
/// durable instruction for later turns.
pub(crate) fn build_full_session_state_compact(
    state: &SessionState,
    _cp: CslCheckpointFields,
    _prev_state: &astra_turn_core::conversation_log::SessionStateCompact,
) -> astra_turn_core::conversation_log::SessionStateCompact {
    astra_turn_core::conversation_log::SessionStateCompact {
        recent_tools: state.recent_tools.clone(),
        activated_deferred_tool_names: state.activated_deferred_tool_names.clone(),
        blocked_tools: Vec::new(),
        approval_overrides: None,
        budget_remaining_tokens: 0,
        budget_remaining_rounds: 0,
        consecutive_ctx_errors: 0,
        interruption: None,
        delegation: None,
        compaction_tracker: None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CslCheckpointFields, build_continuation_anchor, build_full_session_state_compact,
        history_as_messages, merge_continuation_anchor_with_session_memory,
        rebuild_continuation_anchor_from_live_state,
    };
    use crate::cli::session::session_input::build_effective_line;
    use crate::cli::session::session_state::{ContinuationAnchor, SessionState};
    use crate::cli::turn::turn_reporting::build_history_text;
    use astra_services::session_journal;

    fn make_record(
        name: &str,
        ok: bool,
        file_path: Option<&str>,
    ) -> session_journal::ToolCallRecord {
        session_journal::ToolCallRecord {
            name: name.into(),
            ok,
            file_path: file_path.map(|path| path.into()),
            ..Default::default()
        }
    }

    #[test]
    fn continuation_anchor_builder_truncates_long_content() {
        let long_user_input = "a".repeat(300);
        let long_assistant = format!(
            "{}\nSecond line of detail\nThird line of detail\nFourth line should be dropped",
            "b".repeat(300)
        );

        let state = SessionState::default();
        let mut result = crate::tests::stub_stream_result(&long_assistant);
        result.tools_used = vec!["read_file".into(), "str_replace".into()];
        result.tool_call_records = vec![session_journal::ToolCallRecord {
            name: "read_file".into(),
            ok: true,
            ms: 10,
            error: None,
            input_bytes: None,
            output_bytes: None,
            args_preview: Some("crates/runtime/src/server/run_lifecycle.rs".into()),
            result_preview: None,
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        }];

        let anchor = build_continuation_anchor(&state, &long_user_input, &result)
            .expect("should produce anchor");

        assert!(anchor.contains("Latest user task: "));
        let user_part = anchor
            .split("Latest user task: ")
            .nth(1)
            .unwrap()
            .split('\n')
            .next()
            .unwrap();
        assert_eq!(user_part.chars().count(), 221);

        assert!(anchor.contains("Latest assistant summary:\n"));
        assert!(anchor.contains("Second line of detail"));
        assert!(anchor.contains("Third line of detail"));
        assert!(!anchor.contains("Fourth line should be dropped"));
        assert!(anchor.contains("Recent tools: read_file, str_replace"));
        assert!(
            anchor.contains("Artifact: read_file -> crates/runtime/src/server/run_lifecycle.rs")
        );
    }

    #[test]
    fn continuation_anchor_preserves_on_empty_input() {
        let state = SessionState {
            continuation_anchor: Some("Previous anchor content".into()),
            ..SessionState::default()
        };
        let result = crate::tests::stub_stream_result("new response");

        let anchor = build_continuation_anchor(&state, "", &result);
        assert_eq!(anchor.as_deref(), Some("Previous anchor content"));
    }

    #[test]
    fn continuation_anchor_does_not_replace_task_with_low_information_followup() {
        let state = SessionState {
            continuation_anchor: Some(ContinuationAnchor::from_parts(
                "Latest user task: fix tool closure telemetry",
                Some("fix tool closure telemetry".into()),
                None,
                Vec::new(),
            )),
            ..SessionState::default()
        };
        let result = crate::tests::stub_stream_result("Updated telemetry to use final edge_tools.");

        let anchor = build_continuation_anchor(&state, "继续", &result).expect("anchor");

        assert!(anchor.contains("Latest user task: fix tool closure telemetry"));
        assert!(!anchor.contains("Latest user task: 继续"), "{anchor}");
        assert!(anchor.contains("Updated telemetry to use final edge_tools."));
        assert_eq!(
            anchor.latest_user_task.as_deref(),
            Some("fix tool closure telemetry")
        );
    }

    #[tokio::test]
    async fn rebuild_continuation_anchor_from_live_state_includes_active_task_board() {
        let mut state = SessionState::default();
        state.task_manager.rebind("sess-anchor");
        state.history.push((
            "继续".into(),
            "Patched slash parsing and prepared tests.".into(),
        ));
        let create = state
            .task_manager
            .create(&serde_json::json!({"title": "Finish slash command repair"}))
            .await;
        assert!(!create.starts_with("Error:"), "{create}");
        let task_id = state
            .task_manager
            .snapshot()
            .await
            .unwrap()
            .into_iter()
            .next()
            .expect("task")
            .id;
        let update = state
            .task_manager
            .update(&serde_json::json!({"task_id": task_id, "new_status": "in_progress"}))
            .await;
        assert!(!update.starts_with("Error:"), "{update}");

        rebuild_continuation_anchor_from_live_state(&mut state).await;

        let anchor = state.continuation_anchor.expect("anchor");
        assert!(anchor.contains("Active task board:"), "{anchor}");
        assert!(anchor.contains("Finish slash command repair"), "{anchor}");
        assert!(anchor.contains("[in_progress]"), "{anchor}");
    }

    #[tokio::test]
    async fn continuation_anchor_does_not_call_completed_dependency_blocked() {
        let mut state = SessionState::default();
        state.task_manager.rebind("sess-anchor-resolved");
        state
            .history
            .push(("继续".into(), "Finished the prerequisite.".into()));
        state
            .task_manager
            .create(&serde_json::json!({"title": "prerequisite"}))
            .await;
        state
            .task_manager
            .update(&serde_json::json!({
                "task_id": "task-1",
                "new_status": "completed"
            }))
            .await;
        state
            .task_manager
            .create(&serde_json::json!({
                "title": "ready dependent",
                "add_blocked_by": ["task-1"]
            }))
            .await;

        rebuild_continuation_anchor_from_live_state(&mut state).await;

        let anchor = state.continuation_anchor.expect("anchor");
        assert!(anchor.contains("ready dependent"), "{anchor}");
        assert!(
            !anchor.contains("blocked by"),
            "completed edges are history, not active blockers: {anchor}"
        );
    }

    #[tokio::test]
    async fn rebuild_continuation_anchor_from_live_state_includes_paused_open_work() {
        let mut state = SessionState::default();
        state.task_manager.rebind("sess-anchor-paused");
        state.history.push((
            "继续".into(),
            "Paused investigation for operator input.".into(),
        ));
        let create = state
            .task_manager
            .create(&serde_json::json!({"title": "Wait for API credentials"}))
            .await;
        assert!(!create.starts_with("Error:"), "{create}");
        let task_id = state
            .task_manager
            .snapshot()
            .await
            .unwrap()
            .into_iter()
            .next()
            .expect("task")
            .id;
        let update = state
            .task_manager
            .update(&serde_json::json!({"task_id": task_id, "new_status": "paused"}))
            .await;
        assert!(!update.starts_with("Error:"), "{update}");

        rebuild_continuation_anchor_from_live_state(&mut state).await;

        let anchor = state.continuation_anchor.expect("anchor");
        assert!(anchor.contains("Active task board:"), "{anchor}");
        assert!(anchor.contains("Wait for API credentials"), "{anchor}");
        assert!(anchor.contains("[paused]"), "{anchor}");
    }

    #[tokio::test]
    async fn rebuild_continuation_anchor_preserves_previous_task_board_when_load_fails() {
        struct LoadFailsTaskStore;

        #[async_trait::async_trait]
        impl astra_tools::task_mgmt::TaskStore for LoadFailsTaskStore {
            async fn load(
                &self,
                _session_id: &str,
            ) -> Result<Vec<astra_tools::task_mgmt::SessionTask>, String> {
                Err("forced active-task load failure".to_string())
            }

            async fn save(
                &self,
                _session_id: &str,
                _tasks: Vec<astra_tools::task_mgmt::SessionTask>,
            ) -> Result<(), String> {
                Ok(())
            }

            async fn next_task_id(&self, _session_id: &str) -> Result<u32, String> {
                Ok(1)
            }

            async fn peek_next_task_id(&self, _session_id: &str) -> Result<u32, String> {
                Ok(1)
            }
        }

        let mut state = SessionState {
            task_manager: std::sync::Arc::new(crate::edge_tools::TaskManager::new(
                "sess-anchor-fail",
                std::sync::Arc::new(LoadFailsTaskStore),
            )),
            continuation_anchor: Some(ContinuationAnchor::from_parts(
                "Latest user task: 继续\nActive task board:\n- [in_progress] task-1: Preserve me",
                Some("继续".into()),
                None,
                vec!["[in_progress] task-1: Preserve me".into()],
            )),
            ..SessionState::default()
        };
        state.history.push((
            "继续".into(),
            "Kept working on the task board recovery path.".into(),
        ));

        rebuild_continuation_anchor_from_live_state(&mut state).await;

        let anchor = state.continuation_anchor.expect("anchor");
        assert!(anchor.contains("Active task board:"), "{anchor}");
        assert!(
            anchor.contains("[in_progress] task-1: Preserve me"),
            "{anchor}"
        );
        assert_eq!(
            anchor.active_task_board,
            vec!["[in_progress] task-1: Preserve me".to_string()]
        );
    }

    #[tokio::test]
    async fn rebuild_continuation_anchor_does_not_wait_for_a_stalled_remote_task_board() {
        struct StalledTaskStore;

        #[async_trait::async_trait]
        impl astra_tools::task_mgmt::TaskStore for StalledTaskStore {
            async fn load(
                &self,
                _session_id: &str,
            ) -> Result<Vec<astra_tools::task_mgmt::SessionTask>, String> {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                Ok(Vec::new())
            }

            async fn save(
                &self,
                _session_id: &str,
                _tasks: Vec<astra_tools::task_mgmt::SessionTask>,
            ) -> Result<(), String> {
                Ok(())
            }

            async fn next_task_id(&self, _session_id: &str) -> Result<u32, String> {
                Ok(1)
            }

            async fn peek_next_task_id(&self, _session_id: &str) -> Result<u32, String> {
                Ok(1)
            }
        }

        let mut state = SessionState {
            task_manager: std::sync::Arc::new(crate::edge_tools::TaskManager::new(
                "sess-anchor-stalled",
                std::sync::Arc::new(StalledTaskStore),
            )),
            continuation_anchor: Some(ContinuationAnchor::from_parts(
                "Latest user task: finish durable transcript loading\nActive task board:\n- [in_progress] task-1: Preserve live work",
                Some("finish durable transcript loading".into()),
                None,
                vec!["[in_progress] task-1: Preserve live work".into()],
            )),
            ..SessionState::default()
        };
        state.history.push((
            "继续".into(),
            "The durable transcript remains available while the task service recovers.".into(),
        ));

        let completed = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            rebuild_continuation_anchor_from_live_state(&mut state),
        )
        .await;
        assert!(
            completed.is_ok(),
            "continuation context must not wait for the remote task-store timeout"
        );

        let anchor = state.continuation_anchor.expect("anchor");
        assert_eq!(
            anchor.active_task_board,
            vec!["[in_progress] task-1: Preserve live work".to_string()]
        );
        assert!(anchor.contains("task-1: Preserve live work"), "{anchor}");
    }

    #[test]
    fn merge_continuation_anchor_with_session_memory_adds_recap() {
        let memory = "# Session Memory

## Active Goals
- Improve prompt cache

## Pending Todos
- Add shutdown flush

## Current State
- Investigating resume behavior

## Errors & Corrections
- Fixed model override poisoning

## Completed
- Removed legacy extractor
";
        let merged = merge_continuation_anchor_with_session_memory(
            Some(
                "Latest user task: tighten session memory"
                    .to_string()
                    .into(),
            ),
            Some(memory),
        )
        .expect("merged anchor");
        assert!(merged.contains("Latest user task: tighten session memory"));
        assert!(merged.contains("[Session memory recap]"));
        assert!(merged.contains("Session pending"));
        assert!(merged.contains("Add shutdown flush"));
    }

    #[test]
    fn failed_turn_excluded_from_history_preserves_continuity() {
        let mut state = SessionState::default();

        state.history.push((
            "explain ownership".into(),
            "Ownership in Rust means each value has exactly one owner...".into(),
        ));
        state.turn = 1;
        state.continuation_anchor = Some(
            "Latest user task: explain ownership\nLatest assistant summary:\nOwnership in Rust means each value has exactl"
                .to_string()
                .into(),
        );

        state.history.push((
            "now explain borrowing".into(),
            "Borrowing lets you reference data without taking ownership...".into(),
        ));
        state.turn = 2;

        assert_eq!(state.history.len(), 2);
        assert_eq!(state.history[0].0, "explain ownership");
        assert_eq!(state.history[1].0, "now explain borrowing");

        let messages = history_as_messages(&state.history);
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0]["content"], "explain ownership");
        assert_eq!(messages[2]["content"], "now explain borrowing");

        state.continuation_anchor = Some(
            "Latest user task: now explain borrowing\nLatest assistant summary:\nBorrowing lets you reference data"
                .to_string()
                .into(),
        );
        let effective = build_effective_line(
            "continue",
            &state,
            &mut crate::cli::ui_adapter::LineUiAdapter,
        );
        assert_eq!(effective, "continue");

        let user_messages: Vec<_> = messages
            .iter()
            .filter(|message| message["role"] == "user")
            .collect();
        assert_eq!(
            user_messages.len(),
            2,
            "failed turn must not create extra user message"
        );
    }

    #[test]
    fn multi_turn_path_continuity_review_then_fix() {
        let mut history: Vec<(String, String)> = Vec::new();

        let t1_text = "I can help you with that project.";
        let t1_records: Vec<session_journal::ToolCallRecord> = vec![];
        history.push(("hello".into(), build_history_text(t1_text, &t1_records)));

        let t2_text = "## Code Review\n\n**permission_manager.rs:978** — boundary check incomplete\n**safety_middleware.rs:8** — missing UPDATE keyword\n**journal_digest.rs:241** — use enum instead of String";
        let t2_records = vec![
            make_record("skill", true, None),
            make_record("git", true, None),
            make_record("git", true, None),
            make_record(
                "read_file",
                true,
                Some("crates/astra-cli/src/cli/permission_manager.rs"),
            ),
            make_record(
                "read_file",
                true,
                Some("crates/astra-turn-core/src/safety_middleware.rs"),
            ),
            make_record(
                "read_file",
                true,
                Some("crates/astra-cli/src/cli/journal_digest.rs"),
            ),
            make_record("grep", true, None),
        ];
        history.push((
            "review latest commit".into(),
            build_history_text(t2_text, &t2_records),
        ));

        let messages = history_as_messages(&history);
        let mut full_messages = messages;
        full_messages.push(serde_json::json!({"role": "user", "content": "修复和优化"}));

        let t2_assistant = full_messages[3]["content"].as_str().unwrap();
        assert!(
            t2_assistant.contains("crates/astra-cli/src/cli/permission_manager.rs"),
            "Turn 3 prompt must contain permission_manager.rs full path.\nActual Turn 2 assistant:\n{t2_assistant}"
        );
        assert!(
            t2_assistant.contains("crates/astra-turn-core/src/safety_middleware.rs"),
            "Turn 3 prompt must contain safety_middleware.rs full path.\nActual Turn 2 assistant:\n{t2_assistant}"
        );
        assert!(
            t2_assistant.contains("crates/astra-cli/src/cli/journal_digest.rs"),
            "Turn 3 prompt must contain journal_digest.rs full path.\nActual Turn 2 assistant:\n{t2_assistant}"
        );
        assert!(
            !t2_text.contains("crates/"),
            "review text itself should NOT have full paths"
        );
    }

    #[test]
    fn four_turn_session_context_accumulation() {
        let mut history: Vec<(String, String)> = Vec::new();

        let t1_records = vec![
            make_record("skill", true, None),
            make_record("git", true, None),
            make_record("read_file", true, Some("src/cli/permission_manager.rs")),
            make_record("read_file", false, Some("src/safety_middleware.rs")),
            make_record("grep", true, None),
        ];
        history.push((
            "review latest commit".into(),
            build_history_text(
                "## Review\nIssues found in permission_manager.rs",
                &t1_records,
            ),
        ));

        let t2_records = vec![
            make_record("read_file", true, Some("src/cli/permission_manager.rs")),
            make_record("read_file", true, Some("src/safety_middleware.rs")),
            make_record("str_replace", true, Some("src/cli/permission_manager.rs")),
            make_record("str_replace", true, Some("src/safety_middleware.rs")),
            make_record("str_replace", true, Some("src/cli/journal_digest.rs")),
            make_record("bash", true, None),
            make_record("bash", false, None),
        ];
        history.push((
            "修复和优化".into(),
            build_history_text("## Done\nFixed 3 files.", &t2_records),
        ));

        let t3_records = vec![
            make_record("skill", true, None),
            make_record("git", true, None),
        ];
        history.push((
            "review changes".into(),
            build_history_text(
                "## Review\nLGTM. Suggest adding Default to ErrorCategory.",
                &t3_records,
            ),
        ));

        let messages = history_as_messages(&history);
        let mut full_messages = messages;
        full_messages.push(serde_json::json!({"role": "user", "content": "按照建议优化"}));

        let all_text: String = full_messages
            .iter()
            .filter_map(|message| message["content"].as_str())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(all_text.contains("src/cli/permission_manager.rs"));
        assert!(all_text.contains("src/cli/journal_digest.rs"));
        assert!(all_text.contains("src/safety_middleware.rs"));
        assert!(all_text.contains("failed: read_file"));
        assert!(all_text.contains("failed: bash"));
    }

    #[test]
    fn tool_summary_survives_in_compacted_history() {
        let mut history: Vec<(String, String)> = Vec::new();

        history.push((
            String::new(),
            "[Prior context — 3 turns compacted]\nUser worked on fixing permission_manager.rs and safety_middleware.rs.\n\n[Turn context: files: src/permission_manager.rs, src/safety_middleware.rs | tool_calls: 12]".into(),
        ));

        let records = vec![
            make_record("read_file", true, Some("src/journal_digest.rs")),
            make_record("str_replace", true, Some("src/journal_digest.rs")),
        ];
        history.push((
            "add Default derive".into(),
            build_history_text("Added #[derive(Default)] to ErrorCategory.", &records),
        ));

        let messages = history_as_messages(&history);
        let mut full = messages;
        full.push(serde_json::json!({"role": "user", "content": "run tests"}));

        let all_text: String = full
            .iter()
            .filter_map(|message| message["content"].as_str())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(all_text.contains("src/permission_manager.rs"));
        assert!(all_text.contains("src/journal_digest.rs"));
    }

    #[test]
    fn csl_projection_preserves_tool_continuity_state_only() {
        let state = &SessionState {
            recent_tools: vec!["exec".into()],
            activated_deferred_tool_names: vec!["write_file".into()],
            ..Default::default()
        };
        let prev = astra_turn_core::conversation_log::SessionStateCompact {
            blocked_tools: vec!["old_bash".into()],
            activated_deferred_tool_names: vec!["old_deferred".into()],
            approval_overrides: Some(serde_json::json!({"old": true})),
            delegation: Some(astra_turn_core::conversation_log::DelegationCompact {
                id: "old_d".into(),
                pattern: "old_p".into(),
                completed_sub_runs: vec![],
            }),
            compaction_tracker: Some(serde_json::json!({"old": 1})),
            budget_remaining_tokens: 99_999,
            budget_remaining_rounds: 99,
            consecutive_ctx_errors: 99,
            interruption: Some(serde_json::json!({"kind": "budget_exhausted"})),
            ..Default::default()
        };

        let result = build_full_session_state_compact(state, CslCheckpointFields, &prev);
        assert_eq!(result.recent_tools, vec!["exec"]);
        assert_eq!(result.activated_deferred_tool_names, vec!["write_file"]);
        assert!(result.blocked_tools.is_empty());
        assert!(result.approval_overrides.is_none());
        assert_eq!(result.budget_remaining_tokens, 0);
        assert_eq!(result.budget_remaining_rounds, 0);
        assert_eq!(result.consecutive_ctx_errors, 0);
        assert!(result.interruption.is_none());
        assert!(result.delegation.is_none());
        assert!(result.compaction_tracker.is_none());
    }

    #[test]
    fn csl_projection_ignores_checkpoint_runtime_controls() {
        let state = &SessionState::default();
        let prev = astra_turn_core::conversation_log::SessionStateCompact {
            blocked_tools: vec!["bash".into()],
            approval_overrides: Some(serde_json::json!({"tool": "bash"})),
            delegation: Some(astra_turn_core::conversation_log::DelegationCompact {
                id: "d1".into(),
                pattern: "p1".into(),
                completed_sub_runs: vec![],
            }),
            compaction_tracker: Some(serde_json::json!({"v": 1})),
            ..Default::default()
        };

        let result = build_full_session_state_compact(state, CslCheckpointFields, &prev);
        assert!(result.blocked_tools.is_empty());
        assert!(result.approval_overrides.is_none());
        assert!(result.interruption.is_none());
        assert!(result.delegation.is_none());
        assert!(result.compaction_tracker.is_none());
        assert_eq!(result.budget_remaining_tokens, 0);
    }

    #[tokio::test]
    async fn csl_first_turn_writes_snapshot_and_advances_seq() {
        use astra_turn_core::conversation_log::{
            CslStore, SessionStateCompact, file_store::FileCslStore, manager::CslManager,
            materialize,
        };

        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("csl-first-{}", uuid::Uuid::new_v4());

        let store = std::sync::Arc::new(FileCslStore::new(
            astra_services::session_journal::local_owner_sessions_dir(),
        ));
        let mut mgr =
            CslManager::new(store.clone(), session_id.clone(), Default::default()).unwrap();

        let full_messages = vec![
            serde_json::json!({"role": "user", "content": "hello"}),
            serde_json::json!({"role": "assistant", "content": "hi"}),
        ];

        let session_state = SessionStateCompact {
            recent_tools: vec!["bash".into()],
            ..Default::default()
        };

        mgr.persist_turn(1, &full_messages, &session_state)
            .await
            .unwrap();

        assert!(mgr.last_seq() > 0, "seq should advance after first turn");

        let entries = store.load_from_latest_snapshot(&session_id).await.unwrap();
        assert!(
            !entries.is_empty(),
            "should have written at least one entry"
        );
        assert!(entries[0].is_snapshot(), "first entry must be a Snapshot");

        let mat = materialize(&entries).unwrap();
        assert_eq!(mat.messages.len(), 2);
        assert_eq!(mat.session_state.recent_tools, vec!["bash".to_string()]);
    }

    #[tokio::test]
    async fn csl_subsequent_turn_writes_delta_not_snapshot() {
        use astra_turn_core::conversation_log::{
            CslEntry, CslStore, SessionStateCompact, file_store::FileCslStore, manager::CslManager,
            materialize,
        };

        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("csl-delta-{}", uuid::Uuid::new_v4());

        let store = std::sync::Arc::new(FileCslStore::new(
            astra_services::session_journal::local_owner_sessions_dir(),
        ));
        let mut mgr =
            CslManager::new(store.clone(), session_id.clone(), Default::default()).unwrap();

        let t1_msgs = vec![
            serde_json::json!({"role": "user", "content": "q1"}),
            serde_json::json!({"role": "assistant", "content": "a1"}),
        ];
        mgr.persist_turn(1, &t1_msgs, &SessionStateCompact::default())
            .await
            .unwrap();
        let seq_after_t1 = mgr.last_seq();

        mgr.mark_turn_start(t1_msgs.len());
        let t2_full = vec![
            serde_json::json!({"role": "user", "content": "q1"}),
            serde_json::json!({"role": "assistant", "content": "a1"}),
            serde_json::json!({"role": "user", "content": "q2"}),
            serde_json::json!({"role": "assistant", "content": "a2"}),
        ];
        mgr.persist_turn(2, &t2_full, &SessionStateCompact::default())
            .await
            .unwrap();
        let seq_after_t2 = mgr.last_seq();
        assert!(
            seq_after_t2 > seq_after_t1,
            "seq should advance: t1={seq_after_t1}, t2={seq_after_t2}"
        );

        let entries = store.load_from_latest_snapshot(&session_id).await.unwrap();
        let snapshot_count = entries.iter().filter(|e| e.is_snapshot()).count();
        let delta_count = entries
            .iter()
            .filter(|e| matches!(e, CslEntry::TurnDelta { .. }))
            .count();
        assert_eq!(snapshot_count, 1, "should have exactly 1 snapshot");
        assert_eq!(delta_count, 1, "should have exactly 1 delta");

        let mat = materialize(&entries).unwrap();
        assert_eq!(mat.messages.len(), 4);
        assert_eq!(mat.messages[2]["content"], "q2");
        assert_eq!(mat.messages[3]["content"], "a2");
    }

    #[tokio::test]
    async fn csl_periodic_snapshot_every_5_turns() {
        use astra_turn_core::conversation_log::{
            CslStore, SessionStateCompact, file_store::FileCslStore, manager::CslManager,
            materialize,
        };

        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("csl-snap5-{}", uuid::Uuid::new_v4());

        let store = std::sync::Arc::new(FileCslStore::new(
            astra_services::session_journal::local_owner_sessions_dir(),
        ));
        let mut mgr =
            CslManager::new(store.clone(), session_id.clone(), Default::default()).unwrap();

        for t in 1..=5u32 {
            let full: Vec<serde_json::Value> = (1..=t)
                .map(|i| serde_json::json!({"role": "user", "content": format!("turn {i}")}))
                .collect();
            mgr.mark_turn_start(if t == 1 { 0 } else { (t - 1) as usize });
            mgr.persist_turn(t, &full, &SessionStateCompact::default())
                .await
                .unwrap();
        }

        let entries = store.load_from_latest_snapshot(&session_id).await.unwrap();
        assert_eq!(entries.len(), 1, "only the latest snapshot should remain");
        assert!(entries[0].is_snapshot());
        assert_eq!(entries[0].turn(), 5);

        let mat = materialize(&entries).unwrap();
        assert_eq!(mat.messages.len(), 5, "snapshot should contain all 5 turns");
        assert_eq!(mat.messages[4]["content"], "turn 5");

        assert_eq!(mgr.last_seq(), 6);

        let all_entries = store.load_after(&session_id, 0).await.unwrap();
        let total_snapshots = all_entries.iter().filter(|e| e.is_snapshot()).count();
        assert_eq!(
            total_snapshots, 2,
            "should have initial + periodic snapshot"
        );
    }

    #[tokio::test]
    async fn csl_persist_and_resume_roundtrip() {
        use astra_turn_core::conversation_log::{
            SessionStateCompact, file_store::FileCslStore, manager::CslManager,
        };

        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("csl-rt-{}", uuid::Uuid::new_v4());

        let store = std::sync::Arc::new(FileCslStore::new(
            astra_services::session_journal::local_owner_sessions_dir(),
        ));
        let mut mgr =
            CslManager::new(store.clone(), session_id.clone(), Default::default()).unwrap();

        for t in 1..=3u32 {
            let session_state = SessionStateCompact {
                recent_tools: vec![format!("tool_{t}")],
                ..Default::default()
            };
            let full: Vec<serde_json::Value> = (1..=t)
                .flat_map(|i| {
                    vec![
                        serde_json::json!({"role": "user", "content": format!("q{i}")}),
                        serde_json::json!({"role": "assistant", "content": format!("a{i}")}),
                    ]
                })
                .collect();
            mgr.mark_turn_start(if t == 1 { 0 } else { ((t - 1) * 2) as usize });
            mgr.persist_turn(t, &full, &session_state).await.unwrap();
        }

        let saved_seq = mgr.last_seq();
        let mut mgr2 = CslManager::new(store, session_id.clone(), Default::default()).unwrap();
        let mat = mgr2.load().await.unwrap().expect("should have entries");

        assert_eq!(mat.messages.len(), 6, "3 turns x 2 messages");
        assert_eq!(mat.messages[0]["content"], "q1");
        assert_eq!(mat.messages[5]["content"], "a3");
        assert_eq!(mat.last_seq, saved_seq);
        assert_eq!(
            mat.session_state.recent_tools,
            vec!["tool_3".to_string()],
            "should have last turn's recent_tools"
        );
    }

    #[tokio::test]
    async fn csl_undo_resets_seq_and_next_turn_writes_fresh_snapshot() {
        use astra_turn_core::conversation_log::{
            CslStore, SessionStateCompact, file_store::FileCslStore, manager::CslManager,
            materialize,
        };

        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("csl-undo-{}", uuid::Uuid::new_v4());

        let store = std::sync::Arc::new(FileCslStore::new(
            astra_services::session_journal::local_owner_sessions_dir(),
        ));
        let mut mgr =
            CslManager::new(store.clone(), session_id.clone(), Default::default()).unwrap();

        for t in 1..=2u32 {
            let full: Vec<serde_json::Value> = (1..=t)
                .map(|i| serde_json::json!({"role": "user", "content": format!("q{i}")}))
                .collect();
            mgr.mark_turn_start(if t == 1 { 0 } else { (t - 1) as usize });
            mgr.persist_turn(t, &full, &SessionStateCompact::default())
                .await
                .unwrap();
        }
        assert!(mgr.last_seq() > 0, "seq should be > 0 after 2 turns");

        mgr.reset().await.unwrap();
        assert_eq!(mgr.last_seq(), 0, "seq should be 0 after reset");

        let post_undo_msgs = vec![serde_json::json!({"role": "user", "content": "after-undo"})];
        mgr.persist_turn(2, &post_undo_msgs, &SessionStateCompact::default())
            .await
            .unwrap();

        let entries = store.load_from_latest_snapshot(&session_id).await.unwrap();
        let mat = materialize(&entries).unwrap();
        assert_eq!(mat.messages.len(), 1, "fresh snapshot should have 1 msg");
        assert_eq!(mat.messages[0]["content"], "after-undo");
    }
}

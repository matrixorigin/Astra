//! Session finalization and knowledge extraction.
//!
//! This module handles cleanup tasks when a REPL session ends:
//! - Writing session end journal events
//! - Finalizing workspace state
//! - Ending observability sessions
//! - Extracting learnings to `.astra/knowledge.md`
//! - Triggering Memoria governance and consolidation
//! - Clearing panic guards

use astra_services::session_journal;
use crossterm::style::Stylize;
use std::path::Path;

use super::ReplState;
use super::clear_panic_guard;
use super::edge_tools;
use super::repl_turn::enqueue_ingestion_pub;
use super::theme;

/// Finalize a REPL session: journal end event, persist state, extract learnings.
pub(super) async fn finalize_session(state: &ReplState) {
    // 1. Journal: session end event
    if let Some(ref j) = state.journal {
        let end_event =
            session_journal::JournalEvent::session_end(state.session_id.as_deref(), state.turn);
        let _ = j.append(&end_event);
        enqueue_ingestion_pub(state, &end_event);
    }
    // 2. Finalize workspace: persist compact summary + mark completed
    if state.turn > 0 {
        if let Some(ref sid) = state.session_id {
            astra_services::session_workspace::finalize_workspace_on_end(sid);
        }
    }
    // 2b. End observability session and collect summary
    if let (Some(hub), Some(session_id)) = (&state.observability_hub, &state.session_id) {
        // End the session silently — summary is collected but not displayed
        // (could be exposed via /telemetry command in future)
        let _ = hub.end_session(session_id);
    }
    // 3. Session-end knowledge extraction (opt-in, async with timeout)
    let knowledge_handle = session_end_extract_learnings(&state.history);
    // 3b. Trigger Memoria governance + consolidation (best-effort, fire-and-forget)
    tokio::spawn(edge_tools::memoria::memoria_governance_fire_and_forget());
    tokio::spawn(edge_tools::memoria::memoria_consolidate_fire_and_forget());
    // 4. Graceful ingestion shutdown: await worker flush
    if let Some(mc) = state.matrix_runtime.as_ref() {
        mc.shutdown_ingestion_and_wait().await;
    }
    // 5. Await knowledge extraction with timeout
    await_knowledge_extraction(knowledge_handle).await;
    // 6. Clear panic guard
    clear_panic_guard();
}

// ---------------------------------------------------------------------------
// Session-end knowledge extraction → .astra/knowledge.md
// ---------------------------------------------------------------------------

/// Extract learnings from the session and append to `.astra/knowledge.md`.
///
/// This is opt-in: gated behind `MO_SESSION_KNOWLEDGE_EXTRACT_ON_END=true`.
/// Returns `Option<JoinHandle>` — callers should await with timeout at exit.
pub(super) fn session_end_extract_learnings(
    history: &[(String, String)],
) -> Option<tokio::task::JoinHandle<()>> {
    // Gate: explicit opt-in required (involves extra LLM call)
    if std::env::var("MO_SESSION_KNOWLEDGE_EXTRACT_ON_END")
        .unwrap_or_default()
        .to_lowercase()
        != "true"
    {
        return None;
    }

    // Need LLM params and at least a few turns of history
    let params = astra_runtime::turn::cloud::summary::LlmConnParams::from_env()?;
    if history.len() < 3 {
        return None;
    }

    // Convert history to messages for the prompt
    let recent_messages: Vec<serde_json::Value> = history
        .iter()
        .rev()
        .take(20)
        .rev()
        .flat_map(|(user, asst)| {
            vec![
                serde_json::json!({"role": "user", "content": user}),
                serde_json::json!({"role": "assistant", "content": asst}),
            ]
        })
        .collect();

    // Build a summary of the conversation as "session memory" input.
    // Include both user messages and truncated assistant responses to capture
    // problem-solution pairs (most knowledge lives in assistant responses).
    let session_summary = history
        .iter()
        .enumerate()
        .map(|(i, (u, a))| {
            let user_trunc: String = u.chars().take(200).collect();
            let asst_trunc: String = a.chars().take(300).collect();
            format!(
                "Turn {}: User: {user_trunc}\n  Assistant: {asst_trunc}",
                i + 1
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    Some(tokio::spawn(async move {
        use astra_runtime::turn::cloud::session_memory_extract::{
            build_learnings_extraction_prompt, parse_learnings_response,
        };

        let prompt = build_learnings_extraction_prompt(&session_summary, &recent_messages);

        // Call LLM
        let client = astra_runtime::turn::cloud::summary::HttpSummaryClient::new(
            astra_runtime::turn::cloud::summary::LlmConnParams {
                max_output_tokens: 2048,
                ..params
            },
        );
        use astra_runtime::turn::cloud::summary::SummaryLlmClient;
        match client.summarize(&prompt).await {
            Ok(resp) => {
                if let Some(learnings) = parse_learnings_response(&resp.text) {
                    if let Err(e) = append_to_knowledge_md(&learnings) {
                        eprintln!(
                            "  {} Failed to write .astra/knowledge.md: {e}",
                            theme::icon_warn()
                        );
                    } else {
                        let count = learnings
                            .lines()
                            .filter(|l| l.trim_start().starts_with("- "))
                            .count();
                        eprintln!(
                            "{}",
                            format!("  ⊕ {count} learnings saved to .astra/knowledge.md").dim()
                        );
                    }
                }
            }
            Err(e) => {
                eprintln!("  {} Knowledge extraction failed: {e}", theme::icon_warn());
            }
        }
    }))
}

/// Await a knowledge extraction handle with timeout.
pub(super) async fn await_knowledge_extraction(handle: Option<tokio::task::JoinHandle<()>>) {
    let Some(handle) = handle else { return };
    match tokio::time::timeout(std::time::Duration::from_secs(30), handle).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            eprintln!(
                "  {} Knowledge extraction task failed: {e}",
                theme::icon_warn()
            );
        }
        Err(_) => {
            eprintln!("{}", "  Knowledge extraction timed out, skipping.".dim());
        }
    }
}

/// Append new learnings to `.astra/knowledge.md`, deduplicating against existing content.
pub(super) fn append_to_knowledge_md(new_learnings: &str) -> std::io::Result<()> {
    let cwd = std::env::current_dir()?;
    let knowledge_path = cwd.join(".astra").join("knowledge.md");
    append_to_knowledge_md_at(&knowledge_path, new_learnings)
}

/// Core logic: append learnings to a specific knowledge file path (testable).
/// Note: read-dedup-write is not atomic. Concurrent session endings for the same
/// project may produce duplicate lines. This is acceptable since the file is
/// append-only and duplicates are cosmetic (capped at 8KB on injection).
pub(super) fn append_to_knowledge_md_at(
    knowledge_path: &Path,
    new_learnings: &str,
) -> std::io::Result<()> {
    // Read existing content (if any)
    let existing = std::fs::read_to_string(knowledge_path).unwrap_or_default();

    // Dedup: skip lines that already exist (normalized comparison)
    let existing_normalized: std::collections::HashSet<String> =
        existing.lines().map(|l| l.trim().to_lowercase()).collect();
    let new_lines: Vec<&str> = new_learnings
        .lines()
        .filter(|l| {
            let norm = l.trim().to_lowercase();
            !norm.is_empty() && !existing_normalized.contains(&norm)
        })
        .collect();

    if new_lines.is_empty() {
        return Ok(());
    }

    // Ensure .astra/ directory exists
    if let Some(parent) = knowledge_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Build content to append
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let mut append_text = String::new();

    // Add header if file is new
    if existing.trim().is_empty() {
        append_text.push_str("# Project Knowledge\n\n");
        append_text.push_str("<!-- Auto-generated by astra session knowledge extraction. -->\n");
        append_text.push_str("<!-- You can edit this file. It is injected into every session as project context. -->\n\n");
    }

    append_text.push_str(&format!("## Session learnings ({date})\n\n"));
    for line in &new_lines {
        append_text.push_str(line);
        append_text.push('\n');
    }
    append_text.push('\n');

    // Append (don't replace)
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(knowledge_path)?;
    file.write_all(append_text.as_bytes())?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_to_knowledge_md_creates_file() {
        let knowledge_path =
            std::env::temp_dir().join(format!("knowledge-test-{}.md", uuid::Uuid::new_v4()));
        append_to_knowledge_md_at(&knowledge_path, "- Learning one\n- Learning two").unwrap();
        let content = std::fs::read_to_string(&knowledge_path).unwrap();
        assert!(content.contains("- Learning one"));
        assert!(content.contains("- Learning two"));
        std::fs::remove_file(&knowledge_path).ok();
    }

    #[test]
    fn append_to_knowledge_md_deduplicates() {
        let knowledge_path =
            std::env::temp_dir().join(format!("knowledge-dedup-{}.md", uuid::Uuid::new_v4()));
        // First write
        append_to_knowledge_md_at(&knowledge_path, "- Learning one").unwrap();
        // Second write with overlap
        append_to_knowledge_md_at(&knowledge_path, "- Learning one\n- Learning two").unwrap();
        let content = std::fs::read_to_string(&knowledge_path).unwrap();
        let count = content.matches("- Learning one").count();
        assert_eq!(count, 1, "Duplicate line should not appear twice");
        assert!(content.contains("- Learning two"));
        std::fs::remove_file(&knowledge_path).ok();
    }

    #[serial_test::serial]
    #[test]
    fn session_end_extract_learnings_returns_none_without_env_var() {
        unsafe {
            std::env::remove_var("MO_SESSION_KNOWLEDGE_EXTRACT_ON_END");
        }
        let history: Vec<(String, String)> = vec![
            ("Q1".into(), "A1".into()),
            ("Q2".into(), "A2".into()),
            ("Q3".into(), "A3".into()),
        ];
        assert!(
            session_end_extract_learnings(&history).is_none(),
            "Should return None when env var is not set"
        );
    }

    #[test]
    fn session_end_extract_learnings_returns_none_for_short_history() {
        let short_history: Vec<(String, String)> = vec![("Q".into(), "A".into())];
        let result = session_end_extract_learnings(&short_history);
        assert!(
            result.is_none(),
            "Should return None for history shorter than 3 turns"
        );
    }
}

//! Session finalization.
//!
//! This module handles cleanup tasks when a REPL session ends:
//! - Writing session end journal events
//! - Finalizing workspace state
//! - Ending observability sessions
//! - Triggering Memoria governance and consolidation
//! - Clearing panic guards
//!
//! The session-end knowledge extraction feature (auto-append to `.astra/knowledge.md`) was
//! removed during the env-var cleanup. The helper functions are retained for future use.

use astra_services::session_journal;
use crossterm::style::Stylize;
use futures_util::FutureExt;
use std::path::Path;
use std::time::Duration;

use super::ReplState;
use super::edge_tools;
use super::repl_turn::enqueue_ingestion_pub;
use super::session_guard::clear_panic_guard;
use super::theme;

/// Finalize a REPL session: journal end event, persist state, extract learnings.
pub(super) async fn finalize_session(state: &ReplState) {
    // 1. Journal: session end event (idempotent — panic hook may have already written it)
    if let Some(ref j) = state.journal {
        let wrote =
            super::session_guard::try_write_session_end(j, state.session_id.as_deref(), state.turn);
        if wrote {
            let end_event =
                session_journal::JournalEvent::session_end(state.session_id.as_deref(), state.turn);
            enqueue_ingestion_pub(state, &end_event);
        }
    }
    // 2. Finalize workspace: persist compact summary + mark completed
    if state.turn > 0 {
        if let Some(ref sid) = state.session_id {
            astra_services::session_workspace::finalize_workspace_on_end(sid);
        }
    }
    // 3. Session-end knowledge extraction (opt-in, async with timeout)
    let knowledge_handle = session_end_extract_learnings(&state.history);
    // 3b. Trigger Memoria governance + consolidation (best-effort with timeout)
    let gov_handle = tokio::spawn(edge_tools::memoria::memoria_governance_fire_and_forget());
    let con_handle = tokio::spawn(edge_tools::memoria::memoria_consolidate_fire_and_forget());
    // 3c. P3.2 seam: persist cross-session lessons derived from this session.
    //     Best-effort: silent no-op if MatrixOne / user_id is unavailable,
    //     or if the session didn't run long enough to collect signals.
    //     Wrapped in catch_unwind so a panic here cannot prevent the
    //     critical cleanup steps below (ingestion flush, panic guard).
    if state.turn > 0
        && let Some(ref mc) = state.matrix_runtime
        && let Some(ref user_id) = state.ingestion_user_id
    {
        let lesson_fut =
            std::panic::AssertUnwindSafe(persist_lessons_best_effort(state, mc, user_id))
                .catch_unwind();
        match tokio::time::timeout(Duration::from_secs(5), lesson_fut).await {
            Ok(Ok(())) => {}
            Ok(Err(_panic)) => {
                tracing::error!(
                    target: "session_cleanup",
                    "lesson extraction panicked; continuing with critical cleanup",
                );
            }
            Err(_elapsed) => {
                tracing::warn!(
                    target: "session_cleanup",
                    "lesson extraction timed out after 5s; continuing cleanup",
                );
            }
        }
    }
    // 3d. End observability only after session-derived lessons/outcomes have
    // been persisted so the lifecycle boundary matches the data flow.
    if let (Some(hub), Some(session_id)) = (&state.observability_hub, &state.session_id) {
        let _ = hub.end_session(session_id);
    }
    // 4. Graceful ingestion shutdown: await worker flush
    if let Some(mc) = state.matrix_runtime.as_ref() {
        mc.shutdown_ingestion_and_wait().await;
    }
    // 5. Await knowledge extraction with timeout
    await_knowledge_extraction(knowledge_handle).await;
    // 5b. Await Memoria maintenance (bounded 5s so we don't hang on exit)
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        let _ = gov_handle.await;
        let _ = con_handle.await;
    })
    .await;
    // 6. Clear panic guard
    clear_panic_guard();
}

// ---------------------------------------------------------------------------
// Lesson extraction — isolated from finalize_session so a panic cannot
// prevent the critical cleanup steps (ingestion flush, panic guard clear).
// ---------------------------------------------------------------------------

async fn persist_lessons_best_effort(
    state: &ReplState,
    mc: &astra_runtime::MatrixCloudRuntime,
    user_id: &str,
) {
    let svc = mc.agent_lessons_service();
    // Build the summary under a scoped read lock so the summarise call
    // doesn't hold the lock across the persist await. ObservabilitySession
    // isn't Clone, so `summarise_from_runtime` runs inside the guard.
    let summary = match state
        .observability_session
        .as_ref()
        .and_then(|arc| match arc.read() {
            Ok(guard) => Some(guard),
            Err(_poison) => {
                tracing::warn!(
                    target: "session_cleanup",
                    "observability_session lock poisoned; \
                     lesson extraction will omit stall/correction signals",
                );
                None
            }
        }) {
        Some(guard) => astra_runtime::lesson_extractor::summarise_from_runtime(
            &state.tool_health_entries,
            Some(&*guard),
        ),
        None => astra_runtime::lesson_extractor::summarise_from_runtime(
            &state.tool_health_entries,
            None,
        ),
    };
    let tool_failures: u32 = summary.tool_failures.values().copied().sum();
    let _ = astra_runtime::lesson_extractor::persist_session_lessons(
        svc.clone(),
        &summary,
        user_id,
        "generic",
        None,
    )
    .await;
    astra_runtime::lesson_extractor::weaken_rehabilitated_tools(
        svc.clone(),
        &summary,
        user_id,
        "generic",
        None,
    )
    .await;
    if let Some(ref session_id) = state.session_id
        && let Err(e) = svc
            .record_outcome(astra_services::LessonOutcome {
                session_id: session_id.clone(),
                user_id: user_id.to_string(),
                stall_events: summary.stall_events,
                user_corrections: summary.user_corrections.len() as u32,
                tool_failures,
                unmet_postconditions: summary.unmet_postconditions,
                diagnosis_criteria_met: state.diagnosis_criteria_met,
                diagnosis_criteria_failed: state.diagnosis_criteria_failed,
            })
            .await
    {
        tracing::warn!(
            target: "session_cleanup",
            session_id = session_id,
            user_id = user_id,
            error = %e,
            "failed to record lesson exposure outcomes; continuing cleanup",
        );
    }
    if let Err(e) = svc.prune(user_id, 30).await {
        tracing::warn!(
            target: "session_cleanup",
            user_id = user_id,
            error = %e,
            "failed to prune stale agent lessons; continuing cleanup",
        );
    }
}

// ---------------------------------------------------------------------------
// Session-end knowledge extraction → .astra/knowledge.md
// ---------------------------------------------------------------------------

/// Extract learnings from the session (disabled — always no-op).
pub(super) fn session_end_extract_learnings(
    _history: &[(String, String)],
) -> Option<tokio::task::JoinHandle<()>> {
    None
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

    #[test]
    fn session_end_extract_learnings_is_always_none() {
        let history: Vec<(String, String)> = vec![
            ("Q1".into(), "A1".into()),
            ("Q2".into(), "A2".into()),
            ("Q3".into(), "A3".into()),
        ];
        assert!(
            session_end_extract_learnings(&history).is_none(),
            "feature disabled — should always return None"
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

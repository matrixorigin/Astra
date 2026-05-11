//! Session-end governance — cleanup + episode persistence + reflection.
//!
//! At session end we:
//!
//! 1. **Purge working memory** tied to the session (same as before).
//! 2. **Write an `episodic` memory** summarising what happened — derived
//!    from [`SessionFacts`] (deterministic, no LLM call). Replaces the
//!    retired L1b narrative protocol.
//! 3. **Trigger reflection** so Memoria's graph-consolidation picks up
//!    recent memories into scene nodes. Respects the backend's cooldown
//!    (v1 defaults to 1h), so hot sessions won't thrash.
//!
//! All three are best-effort — a failure in any step logs a warning and
//! moves on; the rest still runs.

use astra_turn_types::session_facts::SessionFacts;

/// Knowledge extracted from a session for cross-session persistence.
#[derive(Debug, Clone, Default)]
pub struct SessionKnowledge {
    /// User corrections (highest priority — explicit preferences).
    pub corrections: Vec<String>,
    /// Learnings (patterns, gotchas, conventions).
    pub learnings: Vec<String>,
    /// Key decisions with rationale.
    pub decisions: Vec<String>,
    /// Error patterns to avoid (from facts).
    pub error_patterns: Vec<String>,
}

/// Extract reusable knowledge from session facts.
pub fn extract_session_knowledge(facts: &SessionFacts) -> SessionKnowledge {
    let mut knowledge = SessionKnowledge::default();

    if facts.error_state.total_errors > 0
        && let Some(err) = &facts.error_state.last_error
    {
        knowledge
            .error_patterns
            .push(format!("Error encountered: {err}"));
    }

    knowledge
}

/// Build the episodic summary content for a finished session. Pure
/// function — deterministic, no LLM call. ~200-500 chars.
///
/// Shape (matches the category layout in `astra_prompts::memory_types`):
/// ```text
/// [episode] turn=N, ~Kt tokens
/// Files touched: <k1>, <k2>, ...
/// Tools: <n1>:ok, <n2>:fail, ...
/// Errors: <last_error>
/// ```
pub fn build_episode_overview(facts: &SessionFacts) -> Option<String> {
    // Skip trivial sessions (nothing happened worth remembering).
    if facts.turn == 0 && facts.active_files.is_empty() && facts.recent_tool_calls.is_empty() {
        return None;
    }

    let mut s = String::with_capacity(512);
    s.push_str("[episode] ");
    s.push_str(&format!(
        "turn={}, ~{}K tokens\n",
        facts.turn,
        facts.estimated_tokens / 1000
    ));

    // Files (most recent last — keep final 8 for brevity).
    if !facts.active_files.is_empty() {
        let recent: Vec<&str> = facts
            .active_files
            .iter()
            .rev()
            .take(8)
            .map(|f| f.path.as_str())
            .collect();
        s.push_str("Files: ");
        s.push_str(&recent.iter().rev().copied().collect::<Vec<_>>().join(", "));
        s.push('\n');
    }

    // Tools: a compact ok/fail tally by name.
    if !facts.recent_tool_calls.is_empty() {
        use std::collections::BTreeMap;
        let mut by_name: BTreeMap<&str, (u32, u32)> = BTreeMap::new();
        for tc in &facts.recent_tool_calls {
            let e = by_name.entry(tc.name.as_str()).or_insert((0, 0));
            if tc.ok {
                e.0 += 1;
            } else {
                e.1 += 1;
            }
        }
        let parts: Vec<String> = by_name
            .iter()
            .take(6)
            .map(|(name, (ok, fail))| match (*ok, *fail) {
                (o, 0) => format!("{name}:{o}ok"),
                (0, f) => format!("{name}:{f}fail"),
                (o, f) => format!("{name}:{o}ok/{f}fail"),
            })
            .collect();
        if !parts.is_empty() {
            s.push_str("Tools: ");
            s.push_str(&parts.join(", "));
            s.push('\n');
        }
    }

    // Errors (last one is the most informative; cap at 120 chars).
    if facts.error_state.total_errors > 0
        && let Some(err) = &facts.error_state.last_error
    {
        let err_snip: String = err.chars().take(120).collect();
        s.push_str("Last error: ");
        s.push_str(&err_snip);
        s.push('\n');
    }

    if s.trim().is_empty() { None } else { Some(s) }
}

/// Full session-end governance:
///
/// 1. Purge working memory tied to this session.
/// 2. Persist an `episodic` memory with a deterministic overview.
/// 3. Trigger Memoria reflection (cooldown-respecting).
pub async fn run_session_end_governance(
    facts: &SessionFacts,
    session_id: &str,
    client: &dyn super::memoria_compact::MemoriaClient,
) -> Result<SessionEndReport, String> {
    let mut report = SessionEndReport::default();

    // ── 1. Purge working memory ────────────────────────────────────
    match client.purge_working(session_id).await {
        Ok(n) => {
            report.working_purged = n;
            eprintln!("[session-end] Purged {n} working memories for session {session_id}");
        }
        Err(e) => {
            eprintln!("[session-end] Failed to purge working memory: {e}");
        }
    }

    // ── 2. Persist episodic summary ────────────────────────────────
    if let Some(overview) = build_episode_overview(facts) {
        match client.store_episode(session_id, &overview).await {
            Ok(memory_id) if !memory_id.is_empty() => {
                report.episode_memory_id = Some(memory_id);
                report.episode_chars = overview.chars().count();
                eprintln!(
                    "[session-end] Stored episode ({} chars) for session {session_id}",
                    report.episode_chars
                );
            }
            Ok(_) => {
                // store succeeded but response didn't include a memory_id;
                // count it as a write but leave id blank.
                report.episode_chars = overview.chars().count();
            }
            Err(e) => {
                eprintln!("[session-end] store_episode failed: {e}");
            }
        }
    }

    // ── 3. Reflect (cooldown-respecting — the backend enforces it) ─
    match client.reflect_session(session_id, false).await {
        Ok(summary) => {
            report.reflect_candidates = summary.candidates;
            report.reflect_synthesized = summary.synthesized;
        }
        Err(e) => {
            // Cooldown rejection is expected under hot activity; log at
            // warn and keep going.
            eprintln!("[session-end] reflect skipped/failed: {e}");
        }
    }

    Ok(report)
}

/// Report from session-end governance.
#[derive(Debug, Clone, Default)]
pub struct SessionEndReport {
    pub learnings_stored: usize,
    pub working_purged: u64,
    /// Memoria memory_id of the persisted episode, if any.
    pub episode_memory_id: Option<String>,
    /// Characters written to the episode content (0 = no episode stored).
    pub episode_chars: usize,
    /// Number of scene / cluster candidates produced by reflect.
    pub reflect_candidates: u64,
    /// Whether reflect synthesized new scene nodes (v2 only).
    pub reflect_synthesized: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_turn_types::session_facts::{ErrorFact, FileEntry, ToolFact};

    #[test]
    fn extract_knowledge_includes_error_patterns() {
        let mut facts = SessionFacts::default();
        facts.error_state = ErrorFact {
            total_errors: 3,
            last_error: Some("sqlx column not found".to_string()),
            last_error_turn: Some(5),
        };
        let knowledge = extract_session_knowledge(&facts);
        assert_eq!(knowledge.error_patterns.len(), 1);
        assert!(knowledge.error_patterns[0].contains("sqlx"));
    }

    #[test]
    fn extract_knowledge_empty_session() {
        let facts = SessionFacts::default();
        let knowledge = extract_session_knowledge(&facts);
        assert!(knowledge.corrections.is_empty());
        assert!(knowledge.learnings.is_empty());
        assert!(knowledge.decisions.is_empty());
        assert!(knowledge.error_patterns.is_empty());
    }

    #[test]
    fn episode_overview_none_for_trivial_session() {
        let facts = SessionFacts::default();
        assert!(build_episode_overview(&facts).is_none());
    }

    #[test]
    fn episode_overview_captures_files_tools_errors() {
        let facts = SessionFacts {
            active_files: vec![
                FileEntry {
                    path: "src/main.rs".into(),
                    last_action: "read".into(),
                    turn: 1,
                },
                FileEntry {
                    path: "src/lib.rs".into(),
                    last_action: "write".into(),
                    turn: 2,
                },
            ],
            recent_tool_calls: vec![
                ToolFact {
                    name: "read_file".into(),
                    ok: true,
                    turn: 1,
                },
                ToolFact {
                    name: "read_file".into(),
                    ok: true,
                    turn: 2,
                },
                ToolFact {
                    name: "bash".into(),
                    ok: false,
                    turn: 3,
                },
            ],
            error_state: ErrorFact {
                total_errors: 1,
                last_error: Some("cargo build failed: missing dep".into()),
                last_error_turn: Some(3),
            },
            turn: 3,
            estimated_tokens: 12_000,
            blocked_tools: vec![],
        };
        let overview = build_episode_overview(&facts).expect("non-trivial session");
        assert!(overview.starts_with("[episode]"));
        assert!(overview.contains("turn=3"));
        assert!(overview.contains("~12K tokens"));
        assert!(overview.contains("src/main.rs"));
        assert!(overview.contains("src/lib.rs"));
        assert!(overview.contains("read_file:2ok"));
        assert!(overview.contains("bash:1fail"));
        assert!(overview.contains("cargo build failed"));
    }

    #[test]
    fn episode_overview_truncates_long_error() {
        let mut facts = SessionFacts {
            turn: 1,
            estimated_tokens: 500,
            ..Default::default()
        };
        facts.error_state = ErrorFact {
            total_errors: 1,
            last_error: Some("x".repeat(500)),
            last_error_turn: Some(1),
        };
        let overview = build_episode_overview(&facts).expect("non-trivial session");
        // 120-char cap on error snippet.
        assert!(!overview.contains(&"x".repeat(121)));
    }
}

//! Session-end governance — cleanup pass that purges working memory.
//!
//! The L1b narrative extraction has been retired (wip-3); session-end now
//! only persists fact-derived error patterns and purges working memory.

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

/// Full session-end governance: purge working memory.
pub async fn run_session_end_governance(
    _facts: &SessionFacts,
    session_id: &str,
    client: &dyn super::memoria_compact::MemoriaClient,
) -> Result<SessionEndReport, String> {
    let mut report = SessionEndReport {
        learnings_stored: 0,
        working_purged: 0,
    };

    match client.purge_working(session_id).await {
        Ok(n) => {
            report.working_purged = n;
            eprintln!("[session-end] Purged {n} working memories for session {session_id}");
        }
        Err(e) => {
            eprintln!("[session-end] Failed to purge working memory: {e}");
        }
    }

    Ok(report)
}

/// Report from session-end governance.
#[derive(Debug, Clone, Default)]
pub struct SessionEndReport {
    pub learnings_stored: usize,
    pub working_purged: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_knowledge_includes_error_patterns() {
        use astra_turn_types::session_facts::ErrorFact;
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
}

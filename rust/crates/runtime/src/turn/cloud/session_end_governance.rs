//! P7: Session-end governance — learnings backflow + working memory cleanup.
//!
//! At session end:
//! 1. Extract reusable knowledge from L1b (User Corrections + Learnings)
//!    → store as semantic memory (cross-session)
//! 2. Purge working memory for this session
//!
//! See `docs/design/session-memory-protocol.md` Section 6.2.

use super::session_facts::SessionFacts;
use super::session_memory_protocol::SessionMemory;

/// Knowledge extracted from a session for cross-session persistence.
#[derive(Debug, Clone)]
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

/// Extract reusable knowledge from session facts + narrative.
/// This is called at session end to persist cross-session knowledge.
pub fn extract_session_knowledge(
    facts: &SessionFacts,
    narrative: Option<&SessionMemory>,
) -> SessionKnowledge {
    let mut knowledge = SessionKnowledge {
        corrections: Vec::new(),
        learnings: Vec::new(),
        decisions: Vec::new(),
        error_patterns: Vec::new(),
    };

    if let Some(n) = narrative {
        // User Corrections — highest priority, always persist
        if let Some(corrections) = n.section("User Corrections") {
            knowledge.corrections = extract_bullet_items(corrections);
        }
        // Learnings — reusable patterns
        if let Some(learnings) = n.section("Learnings") {
            knowledge.learnings = extract_bullet_items(learnings);
        }
        // Decisions — key technical choices
        if let Some(decisions) = n.section("Decisions") {
            knowledge.decisions = extract_bullet_items(decisions);
        }
        // Also check v1 "Errors & Corrections" section for backward compat
        if let Some(errors) = n.section("Errors & Corrections") {
            for item in extract_bullet_items(errors) {
                // Only keep items that look like corrections (contain "should", "use", "don't")
                let lower = item.to_lowercase();
                if lower.contains("should")
                    || lower.contains("use ")
                    || lower.contains("don't")
                    || lower.contains("prefer")
                    || lower.contains("avoid")
                {
                    knowledge.corrections.push(item);
                }
            }
        }
    }

    // Error patterns from system facts
    if facts.error_state.total_errors > 0 {
        if let Some(err) = &facts.error_state.last_error {
            knowledge
                .error_patterns
                .push(format!("Error encountered: {err}"));
        }
    }

    knowledge
}

/// Format extracted knowledge as a single markdown string for Memoria storage.
pub fn format_knowledge_for_storage(
    knowledge: &SessionKnowledge,
    session_id: &str,
) -> Option<String> {
    if knowledge.corrections.is_empty()
        && knowledge.learnings.is_empty()
        && knowledge.decisions.is_empty()
    {
        return None; // Nothing worth persisting
    }

    let mut out = format!("[session-knowledge:{session_id}]\n");

    if !knowledge.corrections.is_empty() {
        out.push_str("## User Corrections\n");
        for c in &knowledge.corrections {
            out.push_str(&format!("- {c}\n"));
        }
    }
    if !knowledge.learnings.is_empty() {
        out.push_str("## Learnings\n");
        for l in &knowledge.learnings {
            out.push_str(&format!("- {l}\n"));
        }
    }
    if !knowledge.decisions.is_empty() {
        out.push_str("## Decisions\n");
        for d in &knowledge.decisions {
            out.push_str(&format!("- {d}\n"));
        }
    }

    Some(out)
}

/// Full session-end governance: extract knowledge, store to Memoria, purge working memory.
pub async fn run_session_end_governance(
    facts: &SessionFacts,
    narrative: Option<&SessionMemory>,
    session_id: &str,
    client: &dyn super::memoria_compact::MemoriaClient,
) -> Result<SessionEndReport, String> {
    let knowledge = extract_session_knowledge(facts, narrative);
    let mut report = SessionEndReport {
        learnings_stored: 0,
        working_purged: 0,
    };

    // Store knowledge as semantic memory (cross-session)
    if let Some(content) = format_knowledge_for_storage(&knowledge, session_id) {
        let items =
            knowledge.corrections.len() + knowledge.learnings.len() + knowledge.decisions.len();
        match client
            .store(&content, "semantic", Some(session_id), Some("T2"))
            .await
        {
            Ok(_) => {
                report.learnings_stored = items;
                eprintln!("[session-end] Stored {items} knowledge items for session {session_id}");
            }
            Err(e) => {
                eprintln!("[session-end] Failed to store knowledge: {e}");
            }
        }
    }

    // Purge working memory
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

fn extract_bullet_items(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            trimmed
                .strip_prefix("- ")
                .map(|content| content.to_string())
        })
        .filter(|s| !s.is_empty())
        .collect()
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_narrative(sections: &[(&str, &str)]) -> SessionMemory {
        let mut text = String::from("[session-memory:v1]\n");
        for (name, content) in sections {
            text.push_str(&format!("# {name}\n{content}\n"));
        }
        SessionMemory::parse(&text).unwrap()
    }

    #[test]
    fn extract_knowledge_from_narrative() {
        let facts = SessionFacts::default();
        let narrative = make_narrative(&[
            (
                "User Corrections",
                "- Use RS256 not HS256\n- Don't use rm -rf",
            ),
            (
                "Learnings",
                "- CJK needs char_indices\n- floor_char_boundary for truncation",
            ),
            ("Decisions", "- Use axum over actix\n- Use sqlx for DB"),
        ]);
        let knowledge = extract_session_knowledge(&facts, Some(&narrative));
        assert_eq!(knowledge.corrections.len(), 2);
        assert_eq!(knowledge.learnings.len(), 2);
        assert_eq!(knowledge.decisions.len(), 2);
        assert!(knowledge.corrections[0].contains("RS256"));
        assert!(knowledge.learnings[0].contains("CJK"));
    }

    #[test]
    fn extract_knowledge_includes_error_patterns() {
        use super::super::session_facts::ErrorFact;
        let mut facts = SessionFacts::default();
        facts.error_state = ErrorFact {
            total_errors: 3,
            last_error: Some("sqlx column not found".to_string()),
            last_error_turn: Some(5),
        };
        let knowledge = extract_session_knowledge(&facts, None);
        assert_eq!(knowledge.error_patterns.len(), 1);
        assert!(knowledge.error_patterns[0].contains("sqlx"));
    }

    #[test]
    fn extract_knowledge_empty_session() {
        let facts = SessionFacts::default();
        let knowledge = extract_session_knowledge(&facts, None);
        assert!(knowledge.corrections.is_empty());
        assert!(knowledge.learnings.is_empty());
        assert!(knowledge.decisions.is_empty());
        assert!(knowledge.error_patterns.is_empty());
    }

    #[test]
    fn format_knowledge_returns_none_when_empty() {
        let knowledge = SessionKnowledge {
            corrections: vec![],
            learnings: vec![],
            decisions: vec![],
            error_patterns: vec![],
        };
        assert!(format_knowledge_for_storage(&knowledge, "sess1").is_none());
    }

    #[test]
    fn format_knowledge_includes_all_sections() {
        let knowledge = SessionKnowledge {
            corrections: vec!["Use RS256".to_string()],
            learnings: vec!["CJK needs char_indices".to_string()],
            decisions: vec!["Use axum".to_string()],
            error_patterns: vec![],
        };
        let formatted = format_knowledge_for_storage(&knowledge, "sess1").unwrap();
        assert!(formatted.starts_with("[session-knowledge:sess1]"));
        assert!(formatted.contains("## User Corrections"));
        assert!(formatted.contains("- Use RS256"));
        assert!(formatted.contains("## Learnings"));
        assert!(formatted.contains("## Decisions"));
    }

    #[test]
    fn v1_errors_corrections_backcompat() {
        let facts = SessionFacts::default();
        let narrative = make_narrative(&[(
            "Errors & Corrections",
            "- User said: should use RS256\n- sqlx error on migration\n- Prefer axum over actix",
        )]);
        let knowledge = extract_session_knowledge(&facts, Some(&narrative));
        // "should use RS256" and "Prefer axum" match correction heuristics
        assert!(knowledge.corrections.iter().any(|c| c.contains("RS256")));
        assert!(knowledge.corrections.iter().any(|c| c.contains("Prefer")));
        // "sqlx error" doesn't match correction heuristics
        assert!(!knowledge.corrections.iter().any(|c| c.contains("sqlx")));
    }

    #[tokio::test]
    async fn run_session_end_stores_and_purges() {
        use std::sync::{Arc, Mutex};

        struct MockClient {
            stored: Arc<Mutex<Vec<(String, String)>>>,
            purged: Arc<Mutex<Vec<String>>>,
        }

        #[async_trait::async_trait]
        impl super::super::memoria_compact::MemoriaClient for MockClient {
            async fn retrieve_ext(
                &self,
                _q: &str,
                _sid: Option<&str>,
                _k: usize,
                _filter: bool,
            ) -> Result<Vec<super::super::memoria_compact::MemoriaMemory>, String> {
                Ok(vec![])
            }
            async fn store(
                &self,
                content: &str,
                mem_type: &str,
                _sid: Option<&str>,
                _tier: Option<&str>,
            ) -> Result<String, String> {
                self.stored
                    .lock()
                    .unwrap()
                    .push((content.to_string(), mem_type.to_string()));
                Ok("id1".to_string())
            }
            async fn purge_working(&self, sid: &str) -> Result<u64, String> {
                self.purged.lock().unwrap().push(sid.to_string());
                Ok(2)
            }
            async fn delete(&self, _id: &str) -> Result<(), String> {
                Ok(())
            }
        }

        let facts = SessionFacts::default();
        let narrative = make_narrative(&[
            ("User Corrections", "- Use RS256"),
            ("Learnings", "- CJK handling"),
        ]);
        let stored = Arc::new(Mutex::new(Vec::new()));
        let purged = Arc::new(Mutex::new(Vec::new()));
        let client = MockClient {
            stored: stored.clone(),
            purged: purged.clone(),
        };

        let report = run_session_end_governance(&facts, Some(&narrative), "sess1", &client)
            .await
            .unwrap();

        assert_eq!(report.learnings_stored, 2); // 1 correction + 1 learning
        assert_eq!(report.working_purged, 2);

        let stored = stored.lock().unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].1, "semantic");
        assert!(stored[0].0.contains("[session-knowledge:sess1]"));
        assert!(stored[0].0.contains("RS256"));

        let purged = purged.lock().unwrap();
        assert_eq!(purged.len(), 1);
        assert_eq!(purged[0], "sess1");
    }
}

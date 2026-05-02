//! Memory relevance filtering via a cheap selector model.
//!
//! Filters retrieved memories/lessons to only those clearly relevant
//! to the current task, reducing prompt noise and token waste.
//! Uses the cheapest `selector`-tagged model from the registry.

/// Prompt for the selector model to judge memory relevance.
pub const RELEVANCE_FILTER_PROMPT: &str = "\
You are filtering retrieved memories for relevance to a user's task.
Return ONLY a JSON array of indices for memories that are CLEARLY useful.
If unsure whether a memory is relevant, EXCLUDE it — false negatives
are better than noise. Return [] if nothing is relevant.";

/// Build the user-turn content for relevance filtering.
#[must_use]
pub fn build_relevance_query(user_message: &str, memories: &[String]) -> String {
    let mut prompt = format!("User task: {}\n\nMemories:\n", truncate(user_message, 200));
    for (i, m) in memories.iter().enumerate() {
        prompt.push_str(&format!("[{}] {}\n", i, truncate(m, 150)));
    }
    prompt.push_str("\nRelevant indices (JSON array):");
    prompt
}

/// Parse the selector model's response into a list of indices.
/// Handles: `[0, 2]`, `[0,2]`, bare `0, 2`, and markdown-wrapped responses.
#[must_use]
pub fn parse_relevance_response(response: &str, memory_count: usize) -> Vec<usize> {
    let trimmed = response.trim();

    // Strip markdown code fences if present
    let clean = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|s| s.strip_suffix("```"))
        .unwrap_or(trimmed)
        .trim();

    // Try JSON array parse
    if let Ok(indices) = serde_json::from_str::<Vec<usize>>(clean) {
        return indices.into_iter().filter(|&i| i < memory_count).collect();
    }

    // Fallback: extract numbers from the string
    clean
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|s| s.parse::<usize>().ok())
        .filter(|&i| i < memory_count)
        .collect()
}

/// Filter memories by the indices returned from the selector model.
/// Returns only the memories at the given indices, preserving order.
#[must_use]
pub fn filter_by_indices<T: Clone>(items: &[T], indices: &[usize]) -> Vec<T> {
    indices
        .iter()
        .filter_map(|&i| items.get(i).cloned())
        .collect()
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_json_array() {
        assert_eq!(parse_relevance_response("[0, 2, 4]", 5), vec![0, 2, 4]);
    }

    #[test]
    fn parse_json_array_filters_out_of_bounds() {
        assert_eq!(parse_relevance_response("[0, 2, 10]", 3), vec![0, 2]);
    }

    #[test]
    fn parse_markdown_wrapped() {
        assert_eq!(
            parse_relevance_response("```json\n[1, 3]\n```", 5),
            vec![1, 3]
        );
    }

    #[test]
    fn parse_bare_numbers() {
        assert_eq!(parse_relevance_response("0, 2", 5), vec![0, 2]);
    }

    #[test]
    fn parse_empty_array() {
        assert!(parse_relevance_response("[]", 5).is_empty());
    }

    #[test]
    fn parse_garbage_returns_empty() {
        assert!(parse_relevance_response("no relevant memories", 5).is_empty());
    }

    #[test]
    fn filter_by_indices_preserves_order() {
        let items = vec!["a", "b", "c", "d", "e"];
        let filtered = filter_by_indices(&items, &[4, 1]);
        assert_eq!(filtered, vec!["e", "b"]);
    }

    #[test]
    fn filter_empty_indices_returns_empty() {
        let items = vec!["a", "b"];
        assert!(filter_by_indices(&items, &[]).is_empty());
    }

    #[test]
    fn build_query_includes_all_memories() {
        let query = build_relevance_query(
            "fix auth bug",
            &["use rg not grep".into(), "RS256 for JWT".into()],
        );
        assert!(query.contains("fix auth bug"));
        assert!(query.contains("[0] use rg not grep"));
        assert!(query.contains("[1] RS256 for JWT"));
        assert!(query.contains("Relevant indices"));
    }

    #[test]
    fn build_query_truncates_long_inputs() {
        let long_msg = "x".repeat(500);
        let query = build_relevance_query(&long_msg, &["short".into()]);
        assert!(query.len() < 500 + 200); // truncated message + memory
    }
}

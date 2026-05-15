//! Shared relevance scoring for keyword-based discovery.
//!
//! Used by `tool_search` (keyword mode) and `discover_skills` to rank
//! items by query relevance. Weights are tuned for short tool/skill names
//! and one-line descriptions.

/// Trait for items that can be scored by keyword relevance.
pub trait Scoreable {
    fn score_name(&self) -> &str;
    fn score_description(&self) -> &str;
    fn score_extra(&self) -> Option<&str> {
        None
    }
}

/// Score a single item against pre-split, lowercased query terms.
///
/// Weights:
/// - Exact name match: +20
/// - Name contains term: +10
/// - Name part (camelCase/snake_case split) exact: +8
/// - Name part contains term: +4
/// - Description contains term: +2
/// - Extra text contains term: +1
pub fn relevance_score(item: &dyn Scoreable, query_terms: &[&str]) -> usize {
    if query_terms.is_empty() {
        return 0;
    }

    let name_lower = item.score_name().to_lowercase();
    let desc_lower = item.score_description().to_lowercase();
    let extra_lower = item.score_extra().map(|s| s.to_lowercase());

    let name_parts = split_name_parts(item.score_name());

    let mut score = 0usize;

    for term in query_terms {
        if name_lower == *term {
            score += 20;
        } else if name_lower.contains(term) {
            score += 10;
        }

        for part in &name_parts {
            if part == *term {
                score += 8;
            } else if part.contains(term) {
                score += 4;
            }
        }

        if desc_lower.contains(term) {
            score += 2;
        }

        if let Some(ref extra) = extra_lower
            && extra.contains(term)
        {
            score += 1;
        }
    }

    score
}

/// Score and rank items by query relevance. Returns `(original_index, score)` pairs
/// sorted descending by score, capped at `max_results`. Only items with score > 0
/// are included.
pub fn rank_by_relevance<T: Scoreable>(
    items: &[T],
    query: &str,
    max_results: usize,
) -> Vec<(usize, usize)> {
    let query_lower = query.to_lowercase();
    let query_terms: Vec<&str> = query_lower.split_whitespace().collect();
    if query_terms.is_empty() {
        return Vec::new();
    }

    let mut scored: Vec<(usize, usize)> = items
        .iter()
        .enumerate()
        .filter_map(|(idx, item)| {
            let s = relevance_score(item, &query_terms);
            if s > 0 { Some((idx, s)) } else { None }
        })
        .collect();

    scored.sort_by_key(|b| std::cmp::Reverse(b.1));
    scored.truncate(max_results);
    scored
}

/// Split a name into lowercase parts by camelCase boundaries and underscores.
fn split_name_parts(name: &str) -> Vec<String> {
    name.replace('_', " ")
        .chars()
        .fold(String::new(), |mut acc, c| {
            if c.is_uppercase() && !acc.is_empty() {
                acc.push(' ');
            }
            acc.push(c);
            acc
        })
        .to_lowercase()
        .split_whitespace()
        .map(String::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestItem {
        name: &'static str,
        desc: &'static str,
        extra: Option<&'static str>,
    }

    impl Scoreable for TestItem {
        fn score_name(&self) -> &str {
            self.name
        }
        fn score_description(&self) -> &str {
            self.desc
        }
        fn score_extra(&self) -> Option<&str> {
            self.extra
        }
    }

    fn item(name: &'static str, desc: &'static str) -> TestItem {
        TestItem {
            name,
            desc,
            extra: None,
        }
    }

    fn item_with_extra(name: &'static str, desc: &'static str, extra: &'static str) -> TestItem {
        TestItem {
            name,
            desc,
            extra: Some(extra),
        }
    }

    #[test]
    fn exact_name_match_scores_20() {
        let i = item("bash", "Execute commands");
        assert_eq!(relevance_score(&i, &["bash"]), 20 + 8); // exact name + exact part
    }

    #[test]
    fn name_contains_term_scores_10() {
        let i = item("read_file", "Read file contents");
        // "read" is contained in "read_file" (+10), and "read" is exact part (+8), desc has "read" (+2)
        let s = relevance_score(&i, &["read"]);
        assert!(s >= 10, "expected at least 10, got {s}");
    }

    #[test]
    fn snake_case_parts_match_individually() {
        let i = item("git_log_search", "Search git log");
        // "log" → name contains (+10), part exact "log" (+8), desc contains (+2)
        let s = relevance_score(&i, &["log"]);
        assert!(s >= 18, "expected >=18, got {s}");
    }

    #[test]
    fn camel_case_parts_match_individually() {
        let i = item("readFile", "Read file contents");
        // "file" → name contains (+10), part exact "file" (+8), desc has "file" (+2)
        let s = relevance_score(&i, &["file"]);
        assert!(s >= 18, "expected >=18, got {s}");
    }

    #[test]
    fn description_match_scores_2() {
        let i = item("xyz", "Deploy application to kubernetes cluster");
        let s = relevance_score(&i, &["kubernetes"]);
        assert_eq!(s, 2);
    }

    #[test]
    fn extra_text_match_scores_1() {
        let i = item_with_extra(
            "verify",
            "Run checks",
            "User wants to validate code quality",
        );
        // "validate" only in extra → +1
        let s = relevance_score(&i, &["validate"]);
        assert_eq!(s, 1);
    }

    #[test]
    fn no_match_returns_zero() {
        let i = item("bash", "Execute shell commands");
        assert_eq!(relevance_score(&i, &["kubernetes"]), 0);
    }

    #[test]
    fn multi_term_accumulates_scores() {
        let i = item("read_file", "Read file contents from workspace");
        // "read" → name contains(10) + part exact(8) + desc(2) = 20
        // "workspace" → desc(2) = 2
        let s = relevance_score(&i, &["read", "workspace"]);
        assert_eq!(s, 22);
    }

    #[test]
    fn matching_is_case_insensitive() {
        let i = item("ReadFile", "Read file contents");
        let s = relevance_score(&i, &["readfile"]);
        // exact name match (case insensitive) → 20 + parts: "read"(4 partial) + "file"(4 partial)
        assert!(s >= 20, "expected >=20, got {s}");
    }

    #[test]
    fn empty_query_returns_zero() {
        let i = item("bash", "Execute commands");
        assert_eq!(relevance_score(&i, &[]), 0);
    }

    #[test]
    fn rank_returns_sorted_and_capped() {
        let items = vec![
            item("bash", "Execute commands"),
            item("read_file", "Read file contents"),
            item("write_file", "Write file contents"),
            item("git_log", "Show git log"),
        ];
        let ranked = rank_by_relevance(&items, "file", 2);
        assert_eq!(ranked.len(), 2, "capped at max_results=2");
        // read_file and write_file should be top 2
        let indices: Vec<usize> = ranked.iter().map(|(idx, _)| *idx).collect();
        assert!(indices.contains(&1), "read_file should be in top 2");
        assert!(indices.contains(&2), "write_file should be in top 2");
        // First result has higher or equal score
        assert!(ranked[0].1 >= ranked[1].1);
    }

    #[test]
    fn rank_empty_query_returns_empty() {
        let items = vec![item("bash", "Execute commands")];
        let ranked = rank_by_relevance(&items, "", 10);
        assert!(ranked.is_empty());
    }

    #[test]
    fn rank_no_matches_returns_empty() {
        let items = vec![item("bash", "Execute commands")];
        let ranked = rank_by_relevance(&items, "kubernetes", 10);
        assert!(ranked.is_empty());
    }
}

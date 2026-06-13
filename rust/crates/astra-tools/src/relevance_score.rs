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
    use super::{Scoreable, rank_by_relevance, relevance_score};

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
    fn relevance_score_cases() {
        // (name, desc, extra, query_terms, expected)
        #[allow(clippy::type_complexity)]
        let score_cases: &[(&str, &str, Option<&str>, &[&str], usize)] = &[
            ("bash", "Execute commands", None, &["bash"], 28), // exact name(20)+exact part(8)
            ("read_file", "Read file contents", None, &["read"], 20), // name contains(10)+part exact(8)+desc(2)
            ("git_log_search", "Search git log", None, &["log"], 20), // name contains(10)+part exact(8)+desc(2)
            ("readFile", "Read file contents", None, &["file"], 20), // camelCase part exact(8)+name contains(10)+desc(2)
            (
                "xyz",
                "Deploy application to kubernetes cluster",
                None,
                &["kubernetes"],
                2,
            ), // desc only
            (
                "verify",
                "Run checks",
                Some("User wants to validate code quality"),
                &["validate"],
                1,
            ), // extra only
            ("bash", "Execute shell commands", None, &["kubernetes"], 0), // no match
            (
                "read_file",
                "Read file contents from workspace",
                None,
                &["read", "workspace"],
                22,
            ), // multi-term
            ("ReadFile", "Read file contents", None, &["readfile"], 20), // case insensitive exact match
            ("bash", "Execute commands", None, &[], 0),                  // empty query
        ];
        for (name, desc, extra, terms, expected) in score_cases {
            let item = if let Some(e) = extra {
                item_with_extra(name, desc, e)
            } else {
                item(name, desc)
            };
            assert_eq!(
                relevance_score(&item, terms),
                *expected,
                "name={name}, terms={terms:?}"
            );
        }
    }

    #[test]
    fn rank_by_relevance_cases() {
        let items = vec![
            item("bash", "Execute commands"),
            item("read_file", "Read file contents"),
            item("write_file", "Write file contents"),
            item("git_log", "Show git log"),
        ];

        // sorted and capped
        let ranked = rank_by_relevance(&items, "file", 2);
        assert_eq!(ranked.len(), 2);
        let indices: Vec<usize> = ranked.iter().map(|(idx, _)| *idx).collect();
        assert!(indices.contains(&1)); // read_file
        assert!(indices.contains(&2)); // write_file
        assert!(ranked[0].1 >= ranked[1].1);

        // empty query → empty
        assert!(rank_by_relevance(&items, "", 10).is_empty());

        // no match → empty
        assert!(rank_by_relevance(&items, "kubernetes", 10).is_empty());
    }
}

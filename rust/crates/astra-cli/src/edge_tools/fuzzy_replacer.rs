//! Fuzzy replacer strategies for str_replace.
//!
//! When LLMs produce `old_str` that doesn't exactly match the file content
//! (wrong indentation, extra whitespace, escape chars, etc.), these replacers
//! try progressively looser matching to auto-fix the replacement.
//!
//! Cascade order:
//!   1. LineTrimmedReplacer — trim each line before comparing
//!   2. BlockAnchorReplacer — anchor first+last line, Levenshtein middle
//!   3. WhitespaceNormalizedReplacer — collapse all whitespace
//!   4. IndentationFlexibleReplacer — remove common indent, compare dedented
//!   5. EscapeNormalizedReplacer — handle \n \t \' \" etc.
//!
//! Each replacer returns the *actual* substring from the file content that
//! should be replaced, so the caller can do `content.replacen(actual, new_str, 1)`.
//!
//! Inspired by opencode (anomalyco/opencode) and Cline's replacer strategies.

/// Result of a fuzzy match: the actual content substring and which strategy matched.
pub(crate) struct FuzzyMatch<'a> {
    /// The actual substring from the file content to replace.
    pub actual: &'a str,
    /// Human-readable name of the strategy that matched.
    pub strategy: &'static str,
}

/// Try all fuzzy replacer strategies in cascade order.
/// Returns the first unique match, or None if no strategy finds exactly one match.
pub(crate) fn fuzzy_find_replacement<'a>(
    content: &'a str,
    old_str: &str,
    replace_all: bool,
) -> Option<FuzzyMatch<'a>> {
    // Cascade through strategies in priority order.
    // Each returns Vec of actual content substrings that match.
    let strategies: &[(&str, fn(&str, &str) -> Vec<String>)] = &[
        ("line-trimmed", |c, s| line_trimmed_find(c, s)),
        ("block-anchor", |c, s| block_anchor_find(c, s)),
        ("whitespace-normalized", |c, s| {
            whitespace_normalized_find(c, s)
        }),
        ("indentation-flexible", |c, s| {
            indentation_flexible_find(c, s)
        }),
        ("escape-normalized", |c, s| escape_normalized_find(c, s)),
    ];

    for (name, strategy_fn) in strategies {
        let matches = strategy_fn(content, old_str);
        if replace_all && !matches.is_empty() {
            // For replace_all, verify the match exists in content
            if content.contains(&matches[0]) {
                // Find the actual slice in content
                if let Some(pos) = content.find(&matches[0]) {
                    return Some(FuzzyMatch {
                        actual: &content[pos..pos + matches[0].len()],
                        strategy: name,
                    });
                }
            }
        }
        if matches.len() == 1 {
            // Verify uniqueness: the matched string should appear exactly once
            if let Some(pos) = content.find(&matches[0]) {
                let remaining = &content[pos + matches[0].len()..];
                let is_unique = !remaining.contains(&matches[0]);
                if is_unique || replace_all {
                    return Some(FuzzyMatch {
                        actual: &content[pos..pos + matches[0].len()],
                        strategy: name,
                    });
                }
            }
        }
        // 0 matches → try next; >1 matches → ambiguous, try next
    }
    None
}

// ─── Strategy 1: LineTrimmedReplacer ────────────────────────────────────────

/// Match by trimming each line before comparing.
/// Handles: LLM adding/removing leading/trailing whitespace per line.
fn line_trimmed_find(content: &str, old_str: &str) -> Vec<String> {
    let content_lines: Vec<&str> = content.lines().collect();
    let mut search_lines: Vec<&str> = old_str.lines().collect();

    // Need at least 2 lines for line-trimmed matching to be meaningful
    if search_lines.len() < 2 {
        return vec![];
    }

    // Remove trailing empty line (common in LLM output)
    if search_lines.last().map_or(false, |l| l.trim().is_empty()) {
        search_lines.pop();
    }
    if search_lines.is_empty() {
        return vec![];
    }

    let mut results = Vec::new();

    if content_lines.len() < search_lines.len() {
        return results;
    }

    for i in 0..=content_lines.len() - search_lines.len() {
        let mut all_match = true;
        for (j, search_line) in search_lines.iter().enumerate() {
            if content_lines[i + j].trim() != search_line.trim() {
                all_match = false;
                break;
            }
        }
        if all_match {
            let block: String = content_lines[i..i + search_lines.len()].join("\n");
            results.push(block);
        }
    }
    results
}

// ─── Strategy 2: BlockAnchorReplacer ────────────────────────────────────────

/// Match by anchoring first and last lines, then scoring middle via Levenshtein.
/// Handles: LLM modifying middle lines slightly while keeping boundaries correct.
fn block_anchor_find(content: &str, old_str: &str) -> Vec<String> {
    let content_lines: Vec<&str> = content.lines().collect();
    let mut search_lines: Vec<&str> = old_str.lines().collect();

    // Need at least 3 lines (first + middle + last)
    if search_lines.len() < 3 {
        return vec![];
    }

    // Remove trailing empty line
    if search_lines.last().map_or(false, |l| l.trim().is_empty()) {
        search_lines.pop();
    }
    if search_lines.len() < 3 {
        return vec![];
    }

    let first_search = search_lines[0].trim();
    let last_search = search_lines.last().expect("search_lines has >= 3 elements").trim();

    if first_search.is_empty() || last_search.is_empty() {
        return vec![];
    }

    // Find candidate blocks: first line matches at position i, last line matches at j
    struct Candidate {
        start: usize,
        end: usize, // inclusive
        similarity: f64,
    }

    let mut candidates: Vec<Candidate> = Vec::new();

    for i in 0..content_lines.len() {
        if content_lines[i].trim() != first_search {
            continue;
        }
        // Look for matching last line
        for j in (i + 2)..content_lines.len() {
            if content_lines[j].trim() == last_search {
                // Score middle lines
                let actual_block_size = j - i + 1;
                let lines_to_check =
                    (search_lines.len() - 2).min(actual_block_size.saturating_sub(2));

                let similarity = if lines_to_check > 0 {
                    let mut total = 0.0;
                    for k in 1..=lines_to_check.min(search_lines.len() - 1) {
                        if i + k >= content_lines.len() {
                            break;
                        }
                        let orig = content_lines[i + k].trim();
                        let search = search_lines[k].trim();
                        let max_len = orig.len().max(search.len());
                        if max_len == 0 {
                            continue;
                        }
                        let dist = levenshtein(orig, search);
                        total += 1.0 - (dist as f64 / max_len as f64);
                    }
                    total / lines_to_check as f64
                } else {
                    1.0 // No middle lines → anchors are enough
                };

                candidates.push(Candidate {
                    start: i,
                    end: j,
                    similarity,
                });
                break; // First matching last-line per start position
            }
        }
    }

    if candidates.is_empty() {
        return vec![];
    }

    // Thresholds: relaxed for single candidate, stricter for multiple
    let threshold = if candidates.len() == 1 { 0.0 } else { 0.3 };

    let best = candidates
        .iter()
        .filter(|c| c.similarity >= threshold)
        .max_by(|a, b| a.similarity.partial_cmp(&b.similarity).unwrap_or(std::cmp::Ordering::Equal));

    match best {
        Some(c) => {
            let block: String = content_lines[c.start..=c.end].join("\n");
            vec![block]
        }
        None => vec![],
    }
}

// ─── Strategy 3: WhitespaceNormalizedReplacer ───────────────────────────────

/// Match after collapsing all whitespace to single spaces.
/// Handles: tabs vs spaces, multiple spaces, trailing whitespace.
fn whitespace_normalized_find(content: &str, old_str: &str) -> Vec<String> {
    let content_lines: Vec<&str> = content.lines().collect();
    let search_lines: Vec<&str> = old_str.lines().collect();

    // Single-line: find lines whose normalized form matches
    if search_lines.len() <= 1 {
        let norm_search = normalize_ws(old_str);
        if norm_search.is_empty() {
            return vec![];
        }
        let mut results = Vec::new();
        for line in content_lines.iter() {
            if normalize_ws(line) == norm_search {
                results.push(line.to_string());
            }
        }
        return results;
    }

    // Multi-line: sliding window with normalized comparison
    let mut results = Vec::new();
    let norm_search = normalize_ws(old_str);

    if content_lines.len() < search_lines.len() {
        return results;
    }

    for i in 0..=content_lines.len() - search_lines.len() {
        let block: String = content_lines[i..i + search_lines.len()].join("\n");
        if normalize_ws(&block) == norm_search {
            results.push(block);
        }
    }
    results
}

// ─── Strategy 4: IndentationFlexibleReplacer ────────────────────────────────

/// Match after removing common indentation from both content block and search.
/// Handles: LLM producing code at wrong indentation level.
fn indentation_flexible_find(content: &str, old_str: &str) -> Vec<String> {
    let content_lines: Vec<&str> = content.lines().collect();
    let search_lines: Vec<&str> = old_str.lines().collect();

    if search_lines.len() < 2 {
        return vec![];
    }

    let dedented_search = remove_common_indent(old_str);
    let mut results = Vec::new();

    if content_lines.len() < search_lines.len() {
        return results;
    }

    for i in 0..=content_lines.len() - search_lines.len() {
        let block: String = content_lines[i..i + search_lines.len()].join("\n");
        if remove_common_indent(&block) == dedented_search {
            results.push(block);
        }
    }
    results
}

// ─── Strategy 5: EscapeNormalizedReplacer ───────────────────────────────────

/// Match after unescaping common escape sequences in the search string.
/// Handles: LLM using literal \n instead of newline, \t instead of tab, etc.
fn escape_normalized_find(content: &str, old_str: &str) -> Vec<String> {
    let unescaped = unescape_str(old_str);
    if unescaped == old_str {
        return vec![]; // No escape sequences found, skip
    }

    // Direct substring match
    let mut results = Vec::new();
    let mut search_start = 0;
    while let Some(pos) = content[search_start..].find(&unescaped) {
        let abs_pos = search_start + pos;
        results.push(content[abs_pos..abs_pos + unescaped.len()].to_string());
        search_start = abs_pos + 1;
        if results.len() > 2 {
            break; // Too many matches, won't be unique
        }
    }
    results
}

// ─── Helper functions ───────────────────────────────────────────────────────

/// Collapse all whitespace to single spaces and trim.
fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Remove common leading indentation from all non-empty lines.
fn remove_common_indent(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let min_indent = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);

    if min_indent == 0 {
        return text.to_string();
    }

    lines
        .iter()
        .map(|l| {
            if l.trim().is_empty() {
                *l
            } else {
                &l[min_indent..]
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Unescape common escape sequences: \n → newline, \t → tab, etc.
fn unescape_str(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.peek() {
                Some('n') => {
                    chars.next();
                    result.push('\n');
                }
                Some('t') => {
                    chars.next();
                    result.push('\t');
                }
                Some('r') => {
                    chars.next();
                    result.push('\r');
                }
                Some('\'') => {
                    chars.next();
                    result.push('\'');
                }
                Some('"') => {
                    chars.next();
                    result.push('"');
                }
                Some('\\') => {
                    chars.next();
                    result.push('\\');
                }
                _ => result.push(ch),
            }
        } else {
            result.push(ch);
        }
    }
    result
}

/// Levenshtein edit distance between two strings.
fn levenshtein(a: &str, b: &str) -> usize {
    let a_len = a.len();
    let b_len = b.len();

    if a_len == 0 {
        return b_len;
    }
    if b_len == 0 {
        return a_len;
    }

    // Use single-row optimization: O(min(a,b)) space
    let a_bytes = a.as_bytes();
    let b_bytes = b.as_bytes();
    let mut prev: Vec<usize> = (0..=b_len).collect();
    let mut curr = vec![0usize; b_len + 1];

    for i in 1..=a_len {
        curr[0] = i;
        for j in 1..=b_len {
            let cost = if a_bytes[i - 1] == b_bytes[j - 1] {
                0
            } else {
                1
            };
            curr[j] = (prev[j] + 1) // deletion
                .min(curr[j - 1] + 1) // insertion
                .min(prev[j - 1] + cost); // substitution
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b_len]
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── Levenshtein ────────────────────────────────────────────────────────

    #[test]
    fn levenshtein_identical() {
        assert_eq!(levenshtein("hello", "hello"), 0);
    }

    #[test]
    fn levenshtein_basic() {
        assert_eq!(levenshtein("kitten", "sitting"), 3);
    }

    #[test]
    fn levenshtein_empty() {
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("abc", ""), 3);
    }

    // ─── LineTrimmedReplacer ────────────────────────────────────────────────

    #[test]
    fn line_trimmed_ignores_leading_whitespace() {
        let content = "    fn foo() {\n        bar();\n    }";
        let search = "fn foo() {\n    bar();\n}";
        let matches = line_trimmed_find(content, search);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0], content);
    }

    #[test]
    fn line_trimmed_no_match() {
        let content = "fn foo() {\n    bar();\n}";
        let search = "fn baz() {\n    bar();\n}";
        let matches = line_trimmed_find(content, search);
        assert!(matches.is_empty());
    }

    #[test]
    fn line_trimmed_trailing_empty_line() {
        let content = "  a\n  b";
        let search = "a\nb\n";
        let matches = line_trimmed_find(content, search);
        assert_eq!(matches.len(), 1);
    }

    // ─── BlockAnchorReplacer ────────────────────────────────────────────────

    #[test]
    fn block_anchor_matching_anchors_fuzzy_middle() {
        let content = "fn foo() {\n    let x = 1;\n    let y = 2;\n}";
        // LLM changed middle line slightly
        let search = "fn foo() {\n    let x = 10;\n    let y = 20;\n}";
        let matches = block_anchor_find(content, search);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0], content);
    }

    #[test]
    fn block_anchor_no_last_line_match() {
        let content = "fn foo() {\n    bar();\n}";
        let search = "fn foo() {\n    bar();\nend";
        let matches = block_anchor_find(content, search);
        assert!(matches.is_empty());
    }

    // ─── WhitespaceNormalizedReplacer ───────────────────────────────────────

    #[test]
    fn whitespace_normalized_single_line() {
        let content = "    let   x  =   42;";
        let search = "let x = 42;";
        let matches = whitespace_normalized_find(content, search);
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn whitespace_normalized_multiline() {
        let content = "  fn foo()  {\n    bar() ; \n  }";
        let search = "fn foo() {\nbar() ;\n}";
        let matches = whitespace_normalized_find(content, search);
        assert_eq!(matches.len(), 1);
    }

    // ─── IndentationFlexibleReplacer ────────────────────────────────────────

    #[test]
    fn indentation_flexible_different_indent() {
        let content = "        fn foo() {\n            bar();\n        }";
        let search = "    fn foo() {\n        bar();\n    }";
        let matches = indentation_flexible_find(content, search);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0], content);
    }

    #[test]
    fn indentation_flexible_no_match() {
        let content = "    fn foo() {\n        bar();\n    }";
        let search = "    fn baz() {\n        bar();\n    }";
        let matches = indentation_flexible_find(content, search);
        assert!(matches.is_empty());
    }

    // ─── EscapeNormalizedReplacer ───────────────────────────────────────────

    #[test]
    fn escape_normalized_newline() {
        let content = "hello\nworld";
        let search = "hello\\nworld";
        let matches = escape_normalized_find(content, search);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0], content);
    }

    #[test]
    fn escape_normalized_tab() {
        let content = "col1\tcol2";
        let search = "col1\\tcol2";
        let matches = escape_normalized_find(content, search);
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn escape_normalized_no_escapes() {
        let content = "hello world";
        let search = "hello world";
        let matches = escape_normalized_find(content, search);
        assert!(matches.is_empty()); // No escapes → skip strategy
    }

    // ─── unescape_str ──────────────────────────────────────────────────────

    #[test]
    fn unescape_basic() {
        assert_eq!(unescape_str("a\\nb"), "a\nb");
        assert_eq!(unescape_str("a\\tb"), "a\tb");
        assert_eq!(unescape_str("a\\\\b"), "a\\b");
        assert_eq!(unescape_str("a\\'b"), "a'b");
        assert_eq!(unescape_str("no escapes"), "no escapes");
    }

    // ─── remove_common_indent ───────────────────────────────────────────────

    #[test]
    fn remove_indent_basic() {
        assert_eq!(remove_common_indent("    a\n    b\n    c"), "a\nb\nc");
    }

    #[test]
    fn remove_indent_mixed() {
        assert_eq!(remove_common_indent("    a\n      b\n    c"), "a\n  b\nc");
    }

    #[test]
    fn remove_indent_with_empty_lines() {
        assert_eq!(remove_common_indent("    a\n\n    c"), "a\n\nc");
    }

    // ─── fuzzy_find_replacement (integration) ───────────────────────────────

    #[test]
    fn cascade_prefers_earlier_strategy() {
        // LineTrimmed should match before BlockAnchor
        let content = "  fn foo() {\n    bar();\n  }";
        let search = "fn foo() {\n  bar();\n}";
        let result = fuzzy_find_replacement(content, search, false);
        assert!(result.is_some());
        let m = result.unwrap();
        assert_eq!(m.strategy, "line-trimmed");
        assert_eq!(m.actual, content);
    }

    #[test]
    fn cascade_no_match_returns_none() {
        let content = "completely different content";
        let search = "fn foo() {\n    bar();\n}";
        let result = fuzzy_find_replacement(content, search, false);
        assert!(result.is_none());
    }

    #[test]
    fn cascade_ambiguous_skips_to_next() {
        // Two identical blocks → LineTrimmed finds 2 → skips to next strategies
        let content = "  foo\n  bar\n\n  foo\n  bar";
        let search = "foo\nbar";
        let result = fuzzy_find_replacement(content, search, false);
        // All strategies should find 2 matches → None
        assert!(result.is_none());
    }
}

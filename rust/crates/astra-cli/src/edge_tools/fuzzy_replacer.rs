//! Fuzzy replacer strategies for str_replace.
//!
//! When LLMs produce `old_str` that doesn't exactly match the file content
//! (wrong indentation, extra whitespace, escape chars, etc.), these replacers
//! try progressively looser matching to auto-fix the replacement.
//!
//! Cascade order:
//!   0. LineNumberStrippedReplacer — strip read_file-style line-number prefixes
//!   0.5 QuoteNormalizedReplacer — normalize curly quotes to ASCII
//!   1. LineTrimmedReplacer — trim each line before comparing
//!   2. BlockAnchorReplacer — anchor first+last line, Levenshtein middle
//!   3. WhitespaceNormalizedReplacer — collapse all whitespace
//!   4. IndentationFlexibleReplacer — remove common indent, compare dedented
//!   5. EscapeNormalizedReplacer — handle \n \t \' \" etc.
//!   6. SequenceSimilarityReplacer — sliding-window similarity fallback
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
    type Strategy = (&'static str, fn(&str, &str) -> Vec<String>);
    let strategies: &[Strategy] = &[
        ("line-number-stripped", |c, s| {
            line_number_stripped_find(c, s)
        }),
        ("quote-normalized", |c, s| quote_normalized_find(c, s)),
        ("line-trimmed", |c, s| line_trimmed_find(c, s)),
        ("block-anchor", |c, s| block_anchor_find(c, s)),
        ("whitespace-normalized", |c, s| {
            whitespace_normalized_find(c, s)
        }),
        ("indentation-flexible", |c, s| {
            indentation_flexible_find(c, s)
        }),
        ("escape-normalized", |c, s| escape_normalized_find(c, s)),
        ("sequence-similarity", |c, s| sequence_similarity_find(c, s)),
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

// ─── Strategy 0: LineNumberStrippedReplacer ──────────────────────────────────

/// Match after stripping line-number prefixes from old_str lines.
/// Handles: LLM copying `read_file` output that includes `123. ` or `42: ` prefixes.
/// Only activates if ≥50% of non-empty old_str lines have a line-number prefix.
fn line_number_stripped_find(content: &str, old_str: &str) -> Vec<String> {
    let search_lines: Vec<&str> = old_str.lines().collect();
    if search_lines.is_empty() {
        return vec![];
    }

    // Count how many non-empty lines have a line-number prefix
    let non_empty: Vec<&&str> = search_lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .collect();
    if non_empty.is_empty() {
        return vec![];
    }

    let prefixed_count = non_empty
        .iter()
        .filter(|l| has_line_number_prefix(l))
        .count();

    // Only activate if ≥50% of non-empty lines have prefixes
    if prefixed_count * 2 < non_empty.len() {
        return vec![];
    }

    // Strip prefixes and reconstruct
    let stripped: String = search_lines
        .iter()
        .map(|l| strip_line_number_prefix(l))
        .collect::<Vec<_>>()
        .join("\n");

    // Try exact match with stripped version
    let mut results = Vec::new();
    let mut search_start = 0;
    while let Some(pos) = content[search_start..].find(&stripped) {
        let abs_pos = search_start + pos;
        results.push(content[abs_pos..abs_pos + stripped.len()].to_string());
        search_start = abs_pos + 1;
        if results.len() > 2 {
            break;
        }
    }
    results
}

/// Check if a line starts with a line-number prefix pattern.
/// Matches: `123. `, `42: `, `100| `, `  7. `, `15.` (with optional trailing space)
fn has_line_number_prefix(line: &str) -> bool {
    let trimmed = line.trim_start();
    let mut chars = trimmed.chars().peekable();

    // Must start with a digit
    if !chars.peek().is_some_and(|c| c.is_ascii_digit()) {
        return false;
    }

    // Consume digits
    while chars.peek().is_some_and(|c| c.is_ascii_digit()) {
        chars.next();
    }

    // Must be followed by one of: . : |
    matches!(chars.peek(), Some('.' | ':' | '|'))
}

/// Strip the line-number prefix from a line, preserving remaining content.
fn strip_line_number_prefix(line: &str) -> &str {
    let leading_ws = line.len() - line.trim_start().len();
    let trimmed = &line[leading_ws..];

    let mut idx = 0;
    let bytes = trimmed.as_bytes();

    // Skip digits
    while idx < bytes.len() && bytes[idx].is_ascii_digit() {
        idx += 1;
    }

    // Skip delimiter (. : |)
    if idx < bytes.len() && matches!(bytes[idx], b'.' | b':' | b'|') {
        idx += 1;
    } else {
        // No delimiter found — return original
        return line;
    }

    // Skip optional single space after delimiter
    if idx < bytes.len() && bytes[idx] == b' ' {
        idx += 1;
    }

    &trimmed[idx..]
}

// ─── Strategy 0.5: QuoteNormalizedReplacer ──────────────────────────────────

/// Match after normalizing curly/smart quotes to ASCII quotes.
/// Handles: LLMs and copy-paste from docs/web often producing U+2018/2019/201C/201D.
fn quote_normalized_find(content: &str, old_str: &str) -> Vec<String> {
    if quote_normalized_match_count(content, old_str) != 1 {
        return vec![];
    }

    quote_normalized_actual(content, old_str)
        .map(|actual| vec![actual.to_string()])
        .unwrap_or_default()
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
    if search_lines.last().is_some_and(|l| l.trim().is_empty()) {
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
    if search_lines.last().is_some_and(|l| l.trim().is_empty()) {
        search_lines.pop();
    }
    if search_lines.len() < 3 {
        return vec![];
    }

    let first_search = search_lines[0].trim();
    let last_search = search_lines
        .last()
        .expect("search_lines has >= 3 elements")
        .trim();

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
        .max_by(|a, b| {
            a.similarity
                .partial_cmp(&b.similarity)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

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

// ─── Strategy 6: SequenceSimilarityReplacer ──────────────────────────────────

/// Match by scoring line-by-line similarity across a sliding window.
/// Handles: LLM hallucinating small changes across multiple lines (variable names,
/// values, comments) where no single structural anchor matches exactly.
/// This is the broadest and most expensive strategy — runs last in the cascade.
fn sequence_similarity_find(content: &str, old_str: &str) -> Vec<String> {
    const SIMILARITY_THRESHOLD: f64 = 0.85;

    let content_lines: Vec<&str> = content.lines().collect();
    let mut search_lines: Vec<&str> = old_str.lines().collect();

    // Need at least 2 lines for meaningful similarity
    if search_lines.len() < 2 {
        return vec![];
    }

    // Remove trailing empty line (common in LLM output)
    if search_lines.last().is_some_and(|l| l.trim().is_empty()) {
        search_lines.pop();
    }
    if search_lines.len() < 2 || content_lines.len() < search_lines.len() {
        return vec![];
    }

    struct ScoredBlock {
        start: usize,
        similarity: f64,
    }

    let mut candidates: Vec<ScoredBlock> = Vec::new();

    for i in 0..=content_lines.len() - search_lines.len() {
        let mut total_sim = 0.0;
        let mut scored_lines = 0;

        for (j, search_line) in search_lines.iter().enumerate() {
            let content_line = content_lines[i + j];
            let a = content_line.trim();
            let b = search_line.trim();

            // Empty line matches empty line perfectly
            if a.is_empty() && b.is_empty() {
                total_sim += 1.0;
                scored_lines += 1;
                continue;
            }

            let max_len = a.len().max(b.len());
            if max_len == 0 {
                total_sim += 1.0;
                scored_lines += 1;
                continue;
            }

            let dist = levenshtein(a, b);
            let sim = 1.0 - (dist as f64 / max_len as f64);
            total_sim += sim;
            scored_lines += 1;
        }

        if scored_lines == 0 {
            continue;
        }

        let avg_sim = total_sim / scored_lines as f64;
        if avg_sim >= SIMILARITY_THRESHOLD {
            candidates.push(ScoredBlock {
                start: i,
                similarity: avg_sim,
            });
        }
    }

    if candidates.is_empty() {
        return vec![];
    }

    // Return best match only if it's clearly the winner
    candidates.sort_by(|a, b| {
        b.similarity
            .partial_cmp(&a.similarity)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // If multiple candidates are very close in score (within 0.02), it's ambiguous
    if candidates.len() > 1 && (candidates[0].similarity - candidates[1].similarity) < 0.02 {
        return vec![];
    }

    let best = &candidates[0];
    let block: String = content_lines[best.start..best.start + search_lines.len()].join("\n");
    vec![block]
}

// ─── Helper functions ───────────────────────────────────────────────────────

fn normalize_quotes(s: &str) -> String {
    s.replace(['\u{2018}', '\u{2019}'], "'")
        .replace(['\u{201C}', '\u{201D}'], "\"")
}

fn quote_normalized_actual<'a>(content: &'a str, search: &str) -> Option<&'a str> {
    let norm_search = normalize_quotes(search);
    let norm_content = normalize_quotes(content);
    let pos = norm_content.find(&norm_search)?;

    let mut content_indices: Vec<usize> = content.char_indices().map(|(i, _)| i).collect();
    content_indices.push(content.len());
    let mut norm_indices: Vec<usize> = norm_content.char_indices().map(|(i, _)| i).collect();
    norm_indices.push(norm_content.len());
    let start_char = norm_indices
        .partition_point(|&i| i <= pos)
        .saturating_sub(1);
    let end_pos = pos + norm_search.len();
    let end_char = norm_indices.partition_point(|&i| i < end_pos);
    let content_start = *content_indices.get(start_char)?;
    let content_end = *content_indices.get(end_char)?;
    Some(&content[content_start..content_end])
}

pub(crate) fn quote_normalized_match_count(content: &str, search: &str) -> usize {
    let norm_search = normalize_quotes(search);
    let norm_content = normalize_quotes(content);
    norm_content.matches(&norm_search).count()
}

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

    // ─── LineNumberStrippedReplacer ────────────────────────────────────────

    #[test]
    fn has_line_number_prefix_recognizes_supported_prefixes() {
        assert!(has_line_number_prefix("1. hello"));
        assert!(has_line_number_prefix("42: world"));
        assert!(has_line_number_prefix("  7| test"));
        assert!(has_line_number_prefix("15."));
    }

    #[test]
    fn has_line_number_prefix_rejects_non_prefixes() {
        assert!(!has_line_number_prefix("hello"));
        assert!(!has_line_number_prefix("v1.2.3"));
        assert!(!has_line_number_prefix("  abc: test"));
    }

    #[test]
    fn strip_line_number_prefix_removes_prefix() {
        assert_eq!(strip_line_number_prefix("12. hello"), "hello");
        assert_eq!(strip_line_number_prefix("  42: world"), "world");
        assert_eq!(strip_line_number_prefix("7| value"), "value");
        assert_eq!(strip_line_number_prefix("no prefix"), "no prefix");
    }

    #[test]
    fn line_number_stripped_finds_exact_match() {
        let content = "fn demo() {\n    println!(\"hi\");\n}";
        let search = "1. fn demo() {\n2.     println!(\"hi\");\n3. }";
        let matches = line_number_stripped_find(content, search);
        assert_eq!(matches, vec![content.to_string()]);
    }

    #[test]
    fn line_number_stripped_requires_majority_prefixed_lines() {
        let content = "fn demo() {\n    println!(\"hi\");\n}";
        let search = "1. fn demo() {\n    println!(\"hi\");\n}";
        let matches = line_number_stripped_find(content, search);
        assert!(matches.is_empty());
    }

    #[test]
    fn line_number_stripped_handles_colon_and_pipe_prefixes() {
        let content = "alpha\nbeta\ngamma";
        let search = "10: alpha\n11| beta\n12: gamma";
        let matches = line_number_stripped_find(content, search);
        assert_eq!(matches, vec![content.to_string()]);
    }

    #[test]
    fn normalize_quotes_curly_to_straight() {
        assert_eq!(
            normalize_quotes("say \u{201C}hello\u{201D}"),
            "say \"hello\""
        );
        assert_eq!(normalize_quotes("it\u{2019}s"), "it's");
    }

    #[test]
    fn quote_normalized_actual_returns_original_slice() {
        let content = "let x = \u{201C}hello\u{201D};";
        let search = "\"hello\"";
        let found = quote_normalized_actual(content, search);
        assert_eq!(found, Some("\u{201C}hello\u{201D}"));
    }

    #[test]
    fn quote_normalized_match_count_handles_curly_quotes() {
        let content = "let x = \"hello\";\nlet y = \"hello\";";
        let search = "let x = \u{201C}hello\u{201D};";
        assert_eq!(quote_normalized_match_count(content, search), 1);
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

    // ─── SequenceSimilarityReplacer ────────────────────────────────────────

    #[test]
    fn sequence_similarity_matches_near_block_without_exact_anchors() {
        let content = "let count = 1;\nprintln!(\"hello\");";
        let search = "let count = 2;\nprintln!(\"hullo\");";
        let matches = sequence_similarity_find(content, search);
        assert_eq!(matches, vec![content.to_string()]);
    }

    #[test]
    fn sequence_similarity_below_threshold_returns_none() {
        let content = "let count = 1;\nprintln!(\"hello\");";
        let search = "totally different\ncompletely unrelated";
        let matches = sequence_similarity_find(content, search);
        assert!(matches.is_empty());
    }

    #[test]
    fn sequence_similarity_ambiguous_returns_none() {
        let content =
            "let count = 1;\nprintln!(\"hello\");\n\nlet count = 1;\nprintln!(\"hello\");";
        let search = "let count = 2;\nprintln!(\"hullo\");";
        let matches = sequence_similarity_find(content, search);
        assert!(matches.is_empty());
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

    #[test]
    fn cascade_uses_line_number_stripped_before_other_strategies() {
        let content = "fn demo() {\n    println!(\"hi\");\n}";
        let search = "1. fn demo() {\n2.     println!(\"hi\");\n3. }";
        let result = fuzzy_find_replacement(content, search, false).unwrap();
        assert_eq!(result.strategy, "line-number-stripped");
        assert_eq!(result.actual, content);
    }

    #[test]
    fn cascade_uses_sequence_similarity_as_last_resort() {
        let content = "let count = 1;\nprintln!(\"hello\");";
        let search = "let count = 2;\nprintln!(\"hullo\");";
        let result = fuzzy_find_replacement(content, search, false).unwrap();
        assert_eq!(result.strategy, "sequence-similarity");
        assert_eq!(result.actual, content);
    }
}

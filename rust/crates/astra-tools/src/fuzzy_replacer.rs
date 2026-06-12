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

#![allow(dead_code)]

pub const STRATEGY_QUOTE_NORMALIZED: &str = "quote-normalized";

/// Result of a fuzzy match: the actual content substring and which strategy matched.
#[derive(Debug)]
pub struct FuzzyMatch<'a> {
    /// The actual substring from the file content to replace.
    pub actual: &'a str,
    /// Human-readable name of the strategy that matched.
    pub strategy: &'static str,
}

impl FuzzyMatch<'_> {
    pub fn is_quote_normalized(&self) -> bool {
        self.strategy == STRATEGY_QUOTE_NORMALIZED
    }
}

/// Try all fuzzy replacer strategies in cascade order.
/// Returns the first unique match, or None if no strategy finds exactly one match.
pub fn fuzzy_find_replacement<'a>(
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
        (STRATEGY_QUOTE_NORMALIZED, |c, s| {
            quote_normalized_find(c, s)
        }),
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
        let mut matches = strategy_fn(content, old_str);
        matches.dedup();
        if matches.is_empty() {
            continue;
        }
        if matches.len() > 1 {
            if replace_all {
                return None;
            }
            continue;
        }
        let actual = &matches[0];
        if let Some(pos) = content.find(actual) {
            let is_unique = !content[pos + actual.len()..].contains(actual);
            if is_unique || replace_all {
                return Some(FuzzyMatch {
                    actual: &content[pos..pos + actual.len()],
                    strategy: name,
                });
            }
        }
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
    let norm_search = normalize_quotes(old_str);
    let norm_content = normalize_quotes(content);
    if !norm_content.contains(&norm_search) {
        return vec![];
    }

    let content_indices: Vec<usize> = content
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(content.len()))
        .collect();
    let norm_indices: Vec<usize> = norm_content
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(norm_content.len()))
        .collect();

    let mut results = Vec::new();
    let mut search_from = 0;
    while let Some(rel_pos) = norm_content[search_from..].find(&norm_search) {
        let pos = search_from + rel_pos;
        let start_char = norm_indices
            .partition_point(|&i| i <= pos)
            .saturating_sub(1);
        let end_pos = pos + norm_search.len();
        let end_char = norm_indices.partition_point(|&i| i < end_pos);
        if let (Some(&cs), Some(&ce)) = (
            content_indices.get(start_char),
            content_indices.get(end_char),
        ) {
            let actual = &content[cs..ce];
            if !results.contains(&actual.to_string()) {
                results.push(actual.to_string());
            }
        }
        search_from = pos + norm_search.len();
        if results.len() > 2 {
            break;
        }
    }
    results
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

pub fn quote_normalized_match_count(content: &str, search: &str) -> usize {
    let norm_search = normalize_quotes(search);
    let norm_content = normalize_quotes(content);
    norm_content.matches(&norm_search).count()
}

pub fn preserve_quote_style(old_str: &str, actual_old_str: &str, new_str: &str) -> String {
    if old_str == actual_old_str {
        return new_str.to_string();
    }

    let has_double_quotes =
        actual_old_str.contains('\u{201C}') || actual_old_str.contains('\u{201D}');
    let has_single_quotes =
        actual_old_str.contains('\u{2018}') || actual_old_str.contains('\u{2019}');

    if !has_double_quotes && !has_single_quotes {
        return new_str.to_string();
    }

    let mut result = new_str.to_string();
    if has_double_quotes {
        result = apply_curly_double_quotes(&result);
    }
    if has_single_quotes {
        result = apply_curly_single_quotes(&result);
    }
    result
}

/// Collapse all whitespace to single spaces and trim.
pub fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_opening_quote_context(chars: &[char], index: usize) -> bool {
    if index == 0 {
        return true;
    }

    matches!(
        chars[index - 1],
        ' ' | '\t' | '\n' | '\r' | '(' | '[' | '{' | '\u{2014}' | '\u{2013}'
    )
}

fn apply_curly_double_quotes(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut result = String::with_capacity(s.len());
    for (idx, ch) in chars.iter().enumerate() {
        if *ch == '"' {
            result.push(if is_opening_quote_context(&chars, idx) {
                '\u{201C}'
            } else {
                '\u{201D}'
            });
        } else {
            result.push(*ch);
        }
    }
    result
}

fn apply_curly_single_quotes(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut result = String::with_capacity(s.len());
    for (idx, ch) in chars.iter().enumerate() {
        if *ch == '\'' {
            let prev = idx.checked_sub(1).and_then(|i| chars.get(i)).copied();
            let next = chars.get(idx + 1).copied();
            let is_contraction =
                prev.is_some_and(char::is_alphabetic) && next.is_some_and(char::is_alphabetic);
            let curly = if is_contraction {
                '\u{2019}'
            } else if is_opening_quote_context(&chars, idx) {
                '\u{2018}'
            } else {
                '\u{2019}'
            };
            result.push(curly);
        } else {
            result.push(*ch);
        }
    }
    result
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
    fn levenshtein_cases() {
        assert_eq!(levenshtein("hello", "hello"), 0);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
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
    fn has_line_number_prefix_cases() {
        // recognizes supported prefixes
        assert!(has_line_number_prefix("1. hello"));
        assert!(has_line_number_prefix("42: world"));
        assert!(has_line_number_prefix("  7| test"));
        assert!(has_line_number_prefix("15."));
        // rejects non-prefixes
        assert!(!has_line_number_prefix("hello"));
        assert!(!has_line_number_prefix("v1.2.3"));
        assert!(!has_line_number_prefix("  abc: test"));
    }

    #[test]
    fn strip_line_number_prefix_and_stripped_find_cases() {
        // strip_line_number_prefix
        assert_eq!(strip_line_number_prefix("12. hello"), "hello");
        assert_eq!(strip_line_number_prefix("  42: world"), "world");
        assert_eq!(strip_line_number_prefix("7| value"), "value");
        assert_eq!(strip_line_number_prefix("no prefix"), "no prefix");

        // line_number_stripped_find: exact match
        let content = "fn demo() {\n    println!(\"hi\");\n}";
        let search = "1. fn demo() {\n2.     println!(\"hi\");\n3. }";
        let matches = line_number_stripped_find(content, search);
        assert_eq!(matches, vec![content.to_string()]);

        // requires majority prefixed lines
        let search2 = "1. fn demo() {\n    println!(\"hi\");\n}";
        assert!(line_number_stripped_find(content, search2).is_empty());

        // handles colon and pipe prefixes
        let c2 = "alpha\nbeta\ngamma";
        let s3 = "10: alpha\n11| beta\n12: gamma";
        assert_eq!(line_number_stripped_find(c2, s3), vec![c2.to_string()]);
    }

    #[test]
    fn normalize_quotes_and_match_cases() {
        assert_eq!(
            normalize_quotes("say \u{201C}hello\u{201D}"),
            "say \"hello\""
        );
        assert_eq!(normalize_quotes("it\u{2019}s"), "it's");

        let content = "let x = \"hello\";\nlet y = \"hello\";";
        let search = "let x = \u{201C}hello\u{201D};";
        assert_eq!(quote_normalized_match_count(content, search), 1);
    }

    #[test]
    fn preserve_quote_style_cases() {
        // curly double quotes
        let result = preserve_quote_style(
            "let x = \"hello\";",
            "let x = \u{201C}hello\u{201D};",
            "let x = \"world\";",
        );
        assert_eq!(result, "let x = \u{201C}world\u{201D};");

        // keeps apostrophes as right single quotes
        let result = preserve_quote_style("\"don't\"", "\u{201C}don\u{2019}t\u{201D}", "\"won't\"");
        assert_eq!(result, "\u{201C}won\u{2019}t\u{201D}");

        // both double and single quotes
        let result = preserve_quote_style(
            "She said \"don't\"",
            "She said \u{201C}don\u{2019}t\u{201D}",
            "She said \"won't\"",
        );
        assert_eq!(result, "She said \u{201C}won\u{2019}t\u{201D}");

        // no curly quotes → unchanged
        assert_eq!(preserve_quote_style("hello", "hello", "world"), "world");
    }
    #[test]
    fn replace_all_and_quote_normalized_cases() {
        // abort on distinct actual matches (different indentation)
        let content = "  fn hi() {\n    a();\n  }\n\n\tfn hi() {\n\t  a();\n\t}\n";
        assert!(fuzzy_find_replacement(content, "fn hi() {\n  a();\n}", true).is_none());

        // quote-normalized with identical matches → succeeds
        let content = "say \u{201C}hi\u{201D} and \u{201C}hi\u{201D} again";
        let result = fuzzy_find_replacement(content, "\"hi\"", true).unwrap();
        assert_eq!(result.strategy, STRATEGY_QUOTE_NORMALIZED);

        // quote_normalized_find: returns all distinct curly-quote forms
        let content = "say \u{201C}a\u{201D} and \u{201C}a\u{201C} done";
        let matches = quote_normalized_find(content, "\"a\"");
        assert_eq!(
            matches.len(),
            2,
            "should find both distinct curly-quote forms, got: {matches:?}"
        );

        // replace_all with distinct curly-quote forms → abort
        assert!(fuzzy_find_replacement(
            "say \u{201C}a\u{201D} and \u{201C}a\u{201C} done",
            "\"a\"",
            true
        )
        .is_none());

        // replace_all with duplicate identical strategy results → succeeds
        let content = "  foo()\n  bar()\n\n  foo()\n  bar()";
        let result = fuzzy_find_replacement(content, "foo()\nbar()", true).unwrap();
        assert_eq!(result.strategy, "line-trimmed");
    }

    // ─── LineTrimmedReplacer ────────────────────────────────────────────────

    #[test]
    fn line_trimmed_cases() {
        let content = "    fn foo() {\n        bar();\n    }";
        let search = "fn foo() {\n    bar();\n}";
        let matches = line_trimmed_find(content, search);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0], content);

        // no match
        let matches = line_trimmed_find("fn foo() {\n    bar();\n}", "fn baz() {\n    bar();\n}");
        assert!(matches.is_empty());

        // trailing empty line in search
        let matches = line_trimmed_find("  a\n  b", "a\nb\n");
        assert_eq!(matches.len(), 1);
    }

    // ─── BlockAnchorReplacer ────────────────────────────────────────────────

    #[test]
    fn block_anchor_cases() {
        let content = "fn foo() {\n    let x = 1;\n    let y = 2;\n}";
        let search = "fn foo() {\n    let x = 10;\n    let y = 20;\n}";
        let matches = block_anchor_find(content, search);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0], content);

        // no last line match
        let matches = block_anchor_find("fn foo() {\n    bar();\n}", "fn foo() {\n    bar();\nend");
        assert!(matches.is_empty());
    }

    // ─── WhitespaceNormalizedReplacer ───────────────────────────────────────

    #[test]
    fn whitespace_normalized_cases() {
        let matches = whitespace_normalized_find("    let   x  =   42;", "let x = 42;");
        assert_eq!(matches.len(), 1);
        let matches = whitespace_normalized_find(
            "  fn foo()  {\n    bar() ; \n  }",
            "fn foo() {\nbar() ;\n}",
        );
        assert_eq!(matches.len(), 1);
    }

    // ─── IndentationFlexibleReplacer ────────────────────────────────────────

    #[test]
    fn indentation_flexible_cases() {
        let content = "        fn foo() {\n            bar();\n        }";
        let search = "    fn foo() {\n        bar();\n    }";
        let matches = indentation_flexible_find(content, search);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0], content);

        // no match (different function name)
        let matches = indentation_flexible_find(
            "    fn foo() {\n        bar();\n    }",
            "    fn baz() {\n        bar();\n    }",
        );
        assert!(matches.is_empty());
    }

    // ─── EscapeNormalizedReplacer ───────────────────────────────────────────

    #[test]
    fn escape_normalized_cases() {
        let content = "hello\nworld";
        let matches = escape_normalized_find(content, "hello\\nworld");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0], content);

        let matches = escape_normalized_find("col1\tcol2", "col1\\tcol2");
        assert_eq!(matches.len(), 1);

        // no escapes → skip strategy
        assert!(escape_normalized_find("hello world", "hello world").is_empty());
    }

    // ─── SequenceSimilarityReplacer ────────────────────────────────────────

    #[test]
    fn sequence_similarity_cases() {
        let content = "let count = 1;\nprintln!(\"hello\");";
        let matches = sequence_similarity_find(content, "let count = 2;\nprintln!(\"hullo\");");
        assert_eq!(matches, vec![content.to_string()]);

        // below threshold → none
        assert!(
            sequence_similarity_find(content, "totally different\ncompletely unrelated").is_empty()
        );

        // ambiguous → none
        let dup = "let count = 1;\nprintln!(\"hello\");\n\nlet count = 1;\nprintln!(\"hello\");";
        assert!(sequence_similarity_find(dup, "let count = 2;\nprintln!(\"hullo\");").is_empty());
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
    fn remove_indent_cases() {
        assert_eq!(remove_common_indent("    a\n    b\n    c"), "a\nb\nc");
        assert_eq!(remove_common_indent("    a\n      b\n    c"), "a\n  b\nc");
        assert_eq!(remove_common_indent("    a\n\n    c"), "a\n\nc");
    }

    // ─── fuzzy_find_replacement (integration) ───────────────────────────────

    #[test]
    fn cascade_cases() {
        // prefers line-trimmed over block-anchor
        let content = "  fn foo() {\n    bar();\n  }";
        let search = "fn foo() {\n  bar();\n}";
        let result = fuzzy_find_replacement(content, search, false).unwrap();
        assert_eq!(result.strategy, "line-trimmed");
        assert_eq!(result.actual, content);

        // no match → none
        assert!(fuzzy_find_replacement("completely different content", search, false).is_none());

        // ambiguous (2 identical blocks) → skip to next strategies → none
        let amb = "  foo\n  bar\n\n  foo\n  bar";
        assert!(fuzzy_find_replacement(amb, "foo\nbar", false).is_none());

        // line-number-stripped before other strategies
        let content = "fn demo() {\n    println!(\"hi\");\n}";
        let search = "1. fn demo() {\n2.     println!(\"hi\");\n3. }";
        let result = fuzzy_find_replacement(content, search, false).unwrap();
        assert_eq!(result.strategy, "line-number-stripped");
        assert_eq!(result.actual, content);

        // sequence-similarity as last resort
        let content = "let count = 1;\nprintln!(\"hello\");";
        let result =
            fuzzy_find_replacement(content, "let count = 2;\nprintln!(\"hullo\");", false).unwrap();
        assert_eq!(result.strategy, "sequence-similarity");
        assert_eq!(result.actual, content);
    }
}

//! Structured CLI output helpers.
//!
//! Provides consistent output patterns for success, warning, error, and progress messages.
//! All output goes to stderr (since stdout is reserved for piping/scripting).
//!
//! ## Usage
//!
//! ```ignore
//! cli_ok!("Session saved");
//! cli_ok!("Saved {} skills", count);
//! cli_warn!("No files found");
//! cli_err!("Failed to connect: {}", err);
//! cli_info!("Loading configuration...");
//! cli_section!("Settings");
//! cli_list!([("Name", "John"), ("Age", "30")]);
//! ```

use crate::theme;
use crossterm::style::Stylize;

/// Print a success message with ✓ icon.
///
/// Example: `  ✓ Session saved`
#[macro_export]
macro_rules! cli_ok {
    ($($arg:tt)*) => {{
        use $crate::theme::icon_ok;
        eprintln!("  {} {}", icon_ok(), format!($($arg)*));
    }};
}

/// Print a warning message with ⚠ icon (yellow).
///
/// Example: `  ⚠ No files found`
#[macro_export]
macro_rules! cli_warn {
    ($($arg:tt)*) => {{
        use $crate::theme::icon_warn;
        use crossterm::style::Stylize;
        eprintln!("  {} {}", icon_warn(), format!($($arg)*).yellow());
    }};
}

/// Print an error message with ✗ icon (red).
///
/// Example: `  ✗ Failed to connect: timeout`
#[macro_export]
macro_rules! cli_err {
    ($($arg:tt)*) => {{
        use $crate::theme::icon_err;
        use crossterm::style::Stylize;
        eprintln!("  {} {}", icon_err(), format!($($arg)*).red());
    }};
}

/// Print an info message with ℹ icon.
///
/// Example: `  ℹ Loading configuration...`
#[macro_export]
macro_rules! cli_info {
    ($($arg:tt)*) => {{
        use $crate::theme::icon_info;
        eprintln!("  {} {}", icon_info(), format!($($arg)*));
    }};
}

/// Print a section header (cyan, bold).
///
/// Example: `  ═══ Settings ═══`
#[macro_export]
macro_rules! cli_section {
    ($title:expr) => {{
        use crossterm::style::Stylize;
        use $crate::theme::section;
        eprintln!();
        eprintln!("  {}", section($title).bold());
    }};
}

/// Print a dimmed secondary line (indented).
///
/// Example: `     This is additional context`
#[macro_export]
macro_rules! cli_dim {
    ($($arg:tt)*) => {{
        use crossterm::style::Stylize;
        eprintln!("     {}", format!($($arg)*).dim());
    }};
}

/// Print a labeled value on a single line.
///
/// Example: `  Profile: default`
pub fn cli_kv(label: &str, value: &str) {
    eprintln!("  {}: {}", label.dim(), value);
}

/// Print a table with headers and rows.
///
/// Headers are bold, values are styled based on theme.
pub fn cli_table(headers: &[&str], rows: &[Vec<String>]) {
    if headers.is_empty() || rows.is_empty() {
        return;
    }

    // Calculate column widths
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(cell.len());
            }
        }
    }

    // Print header
    let header_line: String = headers
        .iter()
        .zip(&widths)
        .map(|(h, w)| format!("{:width$}", h, width = w))
        .collect::<Vec<_>>()
        .join("  ");
    eprintln!("  {}", header_line.bold());

    // Separator
    let sep: String = widths
        .iter()
        .map(|w| "─".repeat(*w))
        .collect::<Vec<_>>()
        .join("──");
    eprintln!("  {}", sep.dim());

    // Rows
    for row in rows {
        let row_line: String = row
            .iter()
            .zip(&widths)
            .map(|(cell, w)| format!("{:width$}", cell, width = w))
            .collect::<Vec<_>>()
            .join("  ");
        eprintln!("  {}", row_line);
    }
}

/// Print a simple key-value list (two columns).
///
/// Example:
/// ```text
///   Name     John
///   Age      30
/// ```
pub fn cli_kvlist(items: &[(&str, &str)]) {
    if items.is_empty() {
        return;
    }
    let max_key_len = items.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    for (key, value) in items {
        eprintln!("  {:width$}  {}", key.dim(), value, width = max_key_len);
    }
}

/// Print an indented bullet item.
///
/// Example: `  • skill-name`
pub fn cli_bullet(text: &str) {
    eprintln!("  {} {}", "•".dim(), text);
}

/// Print a numbered item (1-indexed).
///
/// Example: `  1. First item`
pub fn cli_numbered(index: usize, text: &str) {
    eprintln!("  {}. {}", format!("{:>2}", index).dim(), text);
}
// ─────────────────────────────────────────────────────────────────────────────
// Layered Error Formatting — "Did you mean?" suggestions + actionable hints
// ─────────────────────────────────────────────────────────────────────────────

/// Calculate fuzzy match score between two strings (higher = better match).
/// Uses Jaro-Winkler-like scoring optimized for CLI suggestions.
pub fn fuzzy_score(needle: &str, haystack: &str) -> usize {
    let n = needle.to_ascii_lowercase();
    let h = haystack.to_ascii_lowercase();

    // Exact prefix match: highest priority
    if h.starts_with(&n) {
        return 200 + (30_usize.saturating_sub(haystack.len()));
    }

    // Exact substring match
    if h.contains(&n) {
        return 100 + (30_usize.saturating_sub(haystack.len()));
    }

    // Character-level matching for typos
    let mut score = 0;
    let mut last_match_pos = 0;
    for ch in n.chars() {
        if let Some(pos) = h[last_match_pos..].find(ch) {
            score += 10;
            // Bonus for consecutive matches
            if pos == 0 {
                score += 5;
            }
            last_match_pos += pos + 1;
        }
    }

    // Bonus for similar length (catches transpositions)
    if (haystack.len() as i32 - needle.len() as i32).abs() <= 2 {
        score += 15;
    }

    score
}

/// Find best matches from a list of candidates.
pub fn find_suggestions<'a>(input: &str, candidates: &[&'a str], limit: usize) -> Vec<&'a str> {
    let mut scored: Vec<(usize, &'a str)> = candidates
        .iter()
        .map(|c| (fuzzy_score(input, c), *c))
        .filter(|(score, _)| *score > 20) // Minimum threshold
        .collect();

    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.len().cmp(&b.1.len())));

    scored.into_iter().take(limit).map(|(_, c)| c).collect()
}

/// Format a "not found" error with suggestions and next steps.
///
/// # Example output
/// ```text
///   ✗ Model 'gpt-4x' not found
///     Did you mean: gpt-4, gpt-4o, gpt-4-turbo?
///     Try: /model to see available models
/// ```
pub fn format_not_found_error(
    entity_type: &str,          // "Model", "Session", "Skill", etc.
    name: &str,                 // The name that wasn't found
    suggestions: &[&str],       // Fuzzy-matched candidates
    hint_command: Option<&str>, // e.g. "/model", "/session list"
) {
    eprintln!(
        "  {} {} '{}' not found",
        theme::icon_err(),
        entity_type,
        name.red()
    );

    if !suggestions.is_empty() {
        let suggestion_text = suggestions
            .iter()
            .take(3)
            .copied()
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!("    {} {}", "Did you mean:".dim(), suggestion_text.cyan());
    }

    if let Some(cmd) = hint_command {
        eprintln!("    {} {}", "Try:".dim(), cmd.cyan());
    }
}

/// Format an invalid value error with valid options.
///
/// # Example output
/// ```text
///   ✗ Invalid verbosity: 'loud'
///     Valid options: quiet, normal, verbose, debug
/// ```
pub fn format_invalid_value_error(field: &str, value: &str, valid_options: &[&str]) {
    eprintln!(
        "  {} Invalid {}: '{}'",
        theme::icon_err(),
        field,
        value.red()
    );

    if !valid_options.is_empty() {
        eprintln!(
            "    {} {}",
            "Valid options:".dim(),
            valid_options.join(", ").cyan()
        );
    }
}
/// Suggest models from a list (for /model command validation).
pub fn suggest_models(input: &str, available: &[String]) -> Vec<String> {
    let refs: Vec<&str> = available.iter().map(|s| s.as_str()).collect();
    find_suggestions(input, &refs, 3)
        .into_iter()
        .map(|s| s.to_string())
        .collect()
}
/// Suggest skills from a list.
pub fn suggest_skills(input: &str, available: &[String]) -> Vec<String> {
    let refs: Vec<&str> = available.iter().map(|s| s.as_str()).collect();
    find_suggestions(input, &refs, 3)
        .into_iter()
        .map(|s| s.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_kv_prints_formatted() {
        // Just verify no panic
        cli_kv("Label", "Value");
    }

    #[test]
    fn cli_table_basic() {
        cli_table(
            &["Name", "Value"],
            &[
                vec!["foo".to_string(), "bar".to_string()],
                vec!["baz".to_string(), "qux".to_string()],
            ],
        );
    }

    #[test]
    fn cli_table_empty_no_panic() {
        cli_table(&[], &[]);
        cli_table(&["A"], &[]);
    }

    #[test]
    fn cli_kvlist_formats_columns() {
        cli_kvlist(&[("short", "value"), ("longerkey", "val2")]);
    }

    #[test]
    fn cli_bullet_numbered() {
        cli_bullet("item one");
        cli_numbered(1, "first");
        cli_numbered(10, "tenth");
    }

    // ─── Fuzzy matching tests ───

    #[test]
    fn fuzzy_score_exact_prefix_highest() {
        // Exact prefix should score highest
        assert!(fuzzy_score("gpt", "gpt-4") > fuzzy_score("gpt", "claude-gpt"));
        assert!(fuzzy_score("mod", "model") > fuzzy_score("mod", "remodel"));
    }

    #[test]
    fn fuzzy_score_substring_match() {
        // Substring should score lower than prefix
        let prefix_score = fuzzy_score("ses", "session");
        let contains_score = fuzzy_score("ses", "obsession");
        assert!(prefix_score > contains_score);
    }

    #[test]
    fn fuzzy_score_typo_tolerance() {
        // Should detect character-level matches for typos
        let score = fuzzy_score("gtp-4", "gpt-4"); // transposition
        assert!(score > 0);
    }

    #[test]
    fn find_suggestions_returns_best_matches() {
        let candidates = &["gpt-4", "gpt-4o", "gpt-4-turbo", "claude-3", "gemini-pro"];
        let suggestions = find_suggestions("gpt", candidates, 3);
        assert!(suggestions.contains(&"gpt-4"));
        assert!(suggestions.contains(&"gpt-4o"));
        assert!(!suggestions.contains(&"claude-3"));
    }

    #[test]
    fn find_suggestions_limits_results() {
        let candidates = &["a1", "a2", "a3", "a4", "a5"];
        let suggestions = find_suggestions("a", candidates, 2);
        assert_eq!(suggestions.len(), 2);
    }

    #[test]
    fn suggest_models_wrapper() {
        let models = vec![
            "gpt-4".to_string(),
            "gpt-4o".to_string(),
            "claude-3".to_string(),
        ];
        let suggestions = suggest_models("gpt", &models);
        assert!(suggestions.contains(&"gpt-4".to_string()));
    }

    #[test]
    fn format_not_found_no_panic() {
        // Just verify no panic with various inputs
        format_not_found_error("Model", "gpt-999", &["gpt-4", "gpt-4o"], Some("/model"));
        format_not_found_error("Session", "abc", &[], None);
    }

    #[test]
    fn format_invalid_value_no_panic() {
        format_invalid_value_error("verbosity", "loud", &["quiet", "normal", "verbose"]);
        format_invalid_value_error("mode", "x", &[]);
    }
}

//! Structured parsing of build and test output.
//!
//! Detects framework (cargo, pytest, jest, go test, etc.) and extracts:
//! - Pass/fail status
//! - Error count and messages
//! - Error locations (file:line:col) for direct navigation
//! - Test summary (passed, failed, skipped counts)
//!
//! Enables automated change→test→fix cycles by providing structured feedback.

use regex::Regex;
use std::collections::HashSet;
use std::sync::LazyLock;

/// A precise error location extracted from compiler/test output.
#[derive(Debug, Clone)]
pub struct ErrorLocation {
    /// File path (relative to project root)
    pub file: String,
    /// Line number (1-based)
    pub line: usize,
    /// Column number (1-based, 0 = unknown)
    pub col: usize,
    /// Error code (e.g., "E0425") if available
    pub error_code: String,
    /// Error message
    pub message: String,
    /// Severity: "error", "warning", "note"
    #[allow(dead_code)]
    pub severity: String,
    /// Classification for auto-fix prioritization
    pub class: ErrorClass,
    /// Quick-fix hint for the LLM (empty if no suggestion)
    pub hint: String,
    /// Containing function/class scope (from tree-sitter, empty if unavailable)
    pub scope: String,
}

/// A concrete, applicable fix suggestion for an error.
#[derive(Debug, Clone)]
pub struct FixSuggestion {
    /// The file to edit
    pub file: String,
    /// What to do: "insert_line", "replace", "delete_line", "add_import"
    pub action: String,
    /// Target line number (for insert: where to insert before)
    pub line: usize,
    /// Text to insert or replace with (empty for delete)
    pub new_text: String,
    /// Human-readable explanation of the fix
    pub explanation: String,
    /// Confidence: 0.0-1.0 (1.0 = certain, 0.5 = likely, below 0.3 = speculative)
    pub confidence: f64,
}

impl FixSuggestion {
    fn new(
        file: &str,
        action: &str,
        line: usize,
        new_text: &str,
        explanation: &str,
        confidence: f64,
    ) -> Self {
        Self {
            file: file.to_string(),
            action: action.to_string(),
            line,
            new_text: new_text.to_string(),
            explanation: explanation.to_string(),
            confidence,
        }
    }
}

/// Minimum confidence threshold for auto-fix application.
pub const AUTO_FIX_CONFIDENCE_THRESHOLD: f64 = 0.8;
/// Maximum number of auto-fix iterations to prevent infinite loops.
pub const AUTO_FIX_MAX_ITERATIONS: usize = 3;

/// Result of applying a single fix.
#[derive(Debug, Clone)]
pub struct AppliedFix {
    pub file: String,
    pub action: String,
    pub line: usize,
    pub explanation: String,
}

/// Apply a single fix to a file. Returns Ok(description) on success.
pub fn apply_fix(
    fix: &FixSuggestion,
    project_root: &std::path::Path,
) -> Result<AppliedFix, String> {
    let file_path = if std::path::Path::new(&fix.file).is_absolute() {
        std::path::PathBuf::from(&fix.file)
    } else {
        project_root.join(&fix.file)
    };

    let content = std::fs::read_to_string(&file_path)
        .map_err(|e| format!("Failed to read {}: {}", fix.file, e))?;
    let lines: Vec<&str> = content.lines().collect();

    let new_content = match fix.action.as_str() {
        "delete_line" => {
            if fix.line == 0 || fix.line > lines.len() {
                return Err(format!(
                    "Line {} out of range (file has {} lines)",
                    fix.line,
                    lines.len()
                ));
            }
            let mut result: Vec<&str> = Vec::with_capacity(lines.len());
            for (i, line) in lines.iter().enumerate() {
                if i + 1 != fix.line {
                    result.push(line);
                }
            }
            result.join("\n") + if content.ends_with('\n') { "\n" } else { "" }
        }
        "replace" => {
            if fix.line == 0 || fix.line > lines.len() {
                return Err(format!(
                    "Line {} out of range (file has {} lines)",
                    fix.line,
                    lines.len()
                ));
            }
            let mut result: Vec<String> = Vec::with_capacity(lines.len());
            for (i, line) in lines.iter().enumerate() {
                if i + 1 == fix.line {
                    result.push(fix.new_text.clone());
                } else {
                    result.push(line.to_string());
                }
            }
            result.join("\n") + if content.ends_with('\n') { "\n" } else { "" }
        }
        "insert_line" => {
            if fix.line == 0 || fix.line > lines.len() + 1 {
                return Err(format!(
                    "Insert line {} out of range (file has {} lines)",
                    fix.line,
                    lines.len()
                ));
            }
            let mut result: Vec<String> = Vec::with_capacity(lines.len() + 1);
            for (i, line) in lines.iter().enumerate() {
                if i + 1 == fix.line {
                    result.push(fix.new_text.clone());
                }
                result.push(line.to_string());
            }
            // insert after last line
            if fix.line == lines.len() + 1 {
                result.push(fix.new_text.clone());
            }
            result.join("\n") + if content.ends_with('\n') { "\n" } else { "" }
        }
        "add_import" => {
            // Insert at top of file (line 1) or after existing use/import block
            let insert_at = find_import_insertion_point(&lines);
            let mut result: Vec<String> = Vec::with_capacity(lines.len() + 1);
            for (i, line) in lines.iter().enumerate() {
                if i == insert_at {
                    result.push(fix.new_text.clone());
                }
                result.push(line.to_string());
            }
            if insert_at >= lines.len() {
                result.push(fix.new_text.clone());
            }
            result.join("\n") + if content.ends_with('\n') { "\n" } else { "" }
        }
        other => {
            return Err(format!("Unknown fix action: {}", other));
        }
    };

    std::fs::write(&file_path, new_content)
        .map_err(|e| format!("Failed to write {}: {}", fix.file, e))?;

    Ok(AppliedFix {
        file: fix.file.clone(),
        action: fix.action.clone(),
        line: fix.line,
        explanation: fix.explanation.clone(),
    })
}

/// Find the best insertion point for a new import/use statement.
/// Returns the line index (0-based) where the new import should be inserted.
fn find_import_insertion_point(lines: &[&str]) -> usize {
    let mut last_import = 0;
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("use ")
            || trimmed.starts_with("import ")
            || trimmed.starts_with("from ")
            || trimmed.starts_with("require(")
            || trimmed.starts_with("const ") && trimmed.contains("require(")
        {
            last_import = i + 1; // insert after this line
        }
    }
    last_import
}

/// Apply all high-confidence fixes from a list. Returns applied fixes.
/// Fixes are applied in reverse line order within each file to preserve line numbers.
pub fn apply_auto_fixes(
    fixes: &[FixSuggestion],
    project_root: &std::path::Path,
) -> (Vec<AppliedFix>, Vec<String>) {
    let mut applied = Vec::new();
    let mut errors = Vec::new();

    // Filter to high-confidence only
    let mut eligible: Vec<&FixSuggestion> = fixes
        .iter()
        .filter(|f| f.confidence >= AUTO_FIX_CONFIDENCE_THRESHOLD)
        .collect();

    // Sort by file, then by line descending (so deletions don't shift later lines)
    eligible.sort_by(|a, b| a.file.cmp(&b.file).then(b.line.cmp(&a.line)));

    for fix in eligible {
        match apply_fix(fix, project_root) {
            Ok(af) => applied.push(af),
            Err(e) => errors.push(e),
        }
    }

    (applied, errors)
}

/// Format applied fixes into a human-readable report section.
pub fn format_auto_fix_report(
    applied: &[AppliedFix],
    errors: &[String],
    iteration: usize,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("\n── Auto-Fix Iteration {} ──\n", iteration));
    if applied.is_empty() {
        out.push_str("  No fixes applied.\n");
    } else {
        for af in applied {
            out.push_str(&format!(
                "  ✓ {} L{}: {} ({})\n",
                af.file, af.line, af.explanation, af.action
            ));
        }
    }
    for e in errors {
        out.push_str(&format!("  ✗ {}\n", e));
    }
    out
}

/// Generate concrete fix suggestions for an error, given source context.
///
/// Returns zero or more suggestions ranked by confidence. The source_lines
/// parameter should contain the relevant file content for context.
pub fn suggest_fix(error: &ErrorLocation, source_lines: &[&str]) -> Vec<FixSuggestion> {
    let mut fixes = Vec::new();

    match error.error_code.as_str() {
        // ── Rust: Missing import (E0425, E0433, E0412) ──
        "E0425" | "E0433" | "E0412" => {
            if let Some(name) = extract_identifier(&error.message) {
                // Suggest common std imports
                if let Some(import) = suggest_rust_import(name) {
                    fixes.push(FixSuggestion::new(
                        &error.file,
                        "add_import",
                        1,
                        &format!("use {};", import),
                        &format!("Add missing import for `{}`", name),
                        0.8,
                    ));
                }
            }
        }

        // ── Rust: Missing field (E0063) ──
        "E0063" => {
            if let Some(field) = extract_identifier(&error.message) {
                let err_line = error.line.saturating_sub(1);
                if err_line < source_lines.len() {
                    fixes.push(FixSuggestion::new(
                        &error.file,
                        "insert_line",
                        error.line,
                        &format!("            {}: Default::default(),", field),
                        &format!("Add missing field `{}` with default value", field),
                        0.6,
                    ));
                }
            }
        }

        // ── Rust: Unused variable ──
        _ if error.message.contains("unused variable") => {
            if let Some(name) = extract_identifier(&error.message) {
                let err_idx = error.line.saturating_sub(1);
                if err_idx < source_lines.len() {
                    let line = source_lines[err_idx];
                    let new_line = line.replace(name, &format!("_{}", name));
                    fixes.push(FixSuggestion::new(
                        &error.file,
                        "replace",
                        error.line,
                        &new_line,
                        &format!("Prefix unused variable `{}` with underscore", name),
                        0.9,
                    ));
                }
            }
        }

        // ── Rust: Unused import ──
        _ if error.message.contains("unused import") => {
            fixes.push(FixSuggestion::new(
                &error.file,
                "delete_line",
                error.line,
                "",
                "Remove unused import",
                0.9,
            ));
        }

        // ── Rust: String/&str mismatch (E0308) ──
        "E0308" if error.message.contains("&str") && error.message.contains("String") => {
            let err_idx = error.line.saturating_sub(1);
            if err_idx < source_lines.len() {
                let line = source_lines[err_idx];
                if error.message.contains("expected `String`")
                    || error.message.contains("expected struct `String`")
                {
                    // Need String, got &str → add .to_string()
                    fixes.push(FixSuggestion::new(
                        &error.file,
                        "replace",
                        error.line,
                        &format!("{}  // consider adding .to_string()", line.trim_end()),
                        "Add .to_string() to convert &str to String",
                        0.5,
                    ));
                } else {
                    // Need &str, got String → add .as_str() or &
                    fixes.push(FixSuggestion::new(
                        &error.file,
                        "replace",
                        error.line,
                        &format!("{}  // consider adding .as_str() or &", line.trim_end()),
                        "Add .as_str() or & to convert String to &str",
                        0.5,
                    ));
                }
            }
        }

        // ── Rust: Missing trait method (E0046) ──
        "E0046" => {
            if let Some(method) = extract_identifier(&error.message) {
                fixes.push(FixSuggestion::new(
                    &error.file,
                    "insert_line",
                    error.line,
                    &format!("    fn {}(&self) {{ todo!() }}", method),
                    &format!("Add stub for missing trait method `{}`", method),
                    0.6,
                ));
            }
        }

        // ── TypeScript/JavaScript: Cannot find name (TS2304) ──
        "TS2304" => {
            if let Some(name) = extract_identifier(&error.message) {
                fixes.push(FixSuggestion::new(
                    &error.file,
                    "add_import",
                    1,
                    &format!(
                        "import {{ {} }} from './';  // TODO: specify module path",
                        name
                    ),
                    &format!("Add import for `{}`", name),
                    0.4,
                ));
            }
        }

        _ => {}
    }

    // Sort by confidence descending
    fixes.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    fixes
}

/// Suggest a Rust import path for common standard library types.
fn suggest_rust_import(name: &str) -> Option<&'static str> {
    match name {
        "HashMap" => Some("std::collections::HashMap"),
        "HashSet" => Some("std::collections::HashSet"),
        "BTreeMap" => Some("std::collections::BTreeMap"),
        "BTreeSet" => Some("std::collections::BTreeSet"),
        "VecDeque" => Some("std::collections::VecDeque"),
        "BinaryHeap" => Some("std::collections::BinaryHeap"),
        "Arc" => Some("std::sync::Arc"),
        "Mutex" => Some("std::sync::Mutex"),
        "RwLock" => Some("std::sync::RwLock"),
        "Rc" => Some("std::rc::Rc"),
        "RefCell" => Some("std::cell::RefCell"),
        "Cell" => Some("std::cell::Cell"),
        "Pin" => Some("std::pin::Pin"),
        "Path" => Some("std::path::Path"),
        "PathBuf" => Some("std::path::PathBuf"),
        "File" => Some("std::fs::File"),
        "Read" | "Write" | "BufReader" | "BufWriter" => Some("std::io"),
        "Sender" | "Receiver" => Some("std::sync::mpsc"),
        "Duration" | "Instant" | "SystemTime" => Some("std::time"),
        "Display" | "Formatter" => Some("std::fmt"),
        "Error" => Some("std::error::Error"),
        "Cow" => Some("std::borrow::Cow"),
        "NonZeroU32" | "NonZeroU64" | "NonZeroUsize" => Some("std::num"),
        _ => None,
    }
}

/// Error classification for auto-fix prioritization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// Trivial: can be fixed mechanically (missing import, unused var prefix)
    Trivial,
    /// Fixable: LLM can likely fix with context (type mismatch, wrong args)
    Fixable,
    /// Complex: requires deep understanding (borrow checker, trait bounds, logic)
    Complex,
}

impl ErrorLocation {
    /// Create a new ErrorLocation with automatic classification and hints.
    fn new(
        file: String,
        line: usize,
        col: usize,
        error_code: String,
        message: String,
        severity: String,
    ) -> Self {
        let (class, hint) = classify_and_hint(&error_code, &message);
        Self {
            file,
            line,
            col,
            error_code,
            message,
            severity,
            class,
            hint,
            scope: String::new(),
        }
    }
}

/// A group of related errors (same file or cascading from a root cause).
#[derive(Debug)]
pub struct ErrorGroup {
    /// The root-cause error (first/most upstream in the cascade)
    pub root_index: usize,
    /// Indices of all errors in this group (including root)
    pub member_indices: Vec<usize>,
    /// Whether this is a suspected cascade (many errors from one root)
    pub is_cascade: bool,
    /// Description of the cascade pattern (e.g., "missing import causes 4 not-found errors")
    pub cascade_hint: String,
}

/// Parsed result from a build or test command.
#[derive(Debug, Clone, Default)]
pub struct BuildTestResult {
    /// Overall pass/fail status
    pub passed: bool,
    /// Command exit code (if available)
    pub exit_code: Option<i32>,
    /// Detected framework (cargo, pytest, jest, go, npm, make, etc.)
    pub framework: String,
    /// Number of errors/failures
    pub error_count: usize,
    /// Top error messages (max 5)
    pub error_messages: Vec<String>,
    /// Precise error locations (file:line:col) for navigation
    pub error_locations: Vec<ErrorLocation>,
    /// Tests passed (if test command)
    pub tests_passed: usize,
    /// Tests failed (if test command)
    pub tests_failed: usize,
    /// Tests skipped/ignored (if test command)
    pub tests_skipped: usize,
    /// One-line summary
    pub summary: String,
    /// Whether output was truncated
    pub truncated: bool,
}

impl BuildTestResult {
    /// Analyze error locations and group cascading errors by root cause.
    ///
    /// Cascade patterns detected:
    /// 1. **Import cascade**: E0425/E0433/E0412 (not found) in same file from one missing import
    /// 2. **Type cascade**: E0308 (type mismatch) following E0412/E0425 in same scope
    /// 3. **Same-file cluster**: 3+ errors in one file → suggest fixing first
    /// 4. **Trait cascade**: E0599 (method not found) following E0277 (trait not satisfied)
    pub fn analyze_error_groups(&self) -> Vec<ErrorGroup> {
        use std::collections::HashMap;

        if self.error_locations.is_empty() {
            return Vec::new();
        }

        // Group by file
        let mut by_file: HashMap<&str, Vec<usize>> = HashMap::new();
        for (i, loc) in self.error_locations.iter().enumerate() {
            by_file.entry(&loc.file).or_default().push(i);
        }

        let mut groups = Vec::new();
        for indices in by_file.values() {
            if indices.len() < 2 {
                // Single error in file — trivial group, no cascade
                groups.push(ErrorGroup {
                    root_index: indices[0],
                    member_indices: indices.clone(),
                    is_cascade: false,
                    cascade_hint: String::new(),
                });
                continue;
            }

            // Look for cascade patterns within this file
            let locs: Vec<&ErrorLocation> =
                indices.iter().map(|&i| &self.error_locations[i]).collect();

            // Pattern 1: Import cascade — one missing import causes multiple "not found"
            let import_codes = ["E0425", "E0433", "E0412", "E0432"];
            let import_errors: Vec<usize> = indices
                .iter()
                .filter(|&&i| import_codes.contains(&self.error_locations[i].error_code.as_str()))
                .copied()
                .collect();

            if import_errors.len() >= 2 {
                // Check if they reference the same identifier
                let identifiers: Vec<Option<&str>> = import_errors
                    .iter()
                    .map(|&i| extract_identifier(&self.error_locations[i].message))
                    .collect();
                let first_id = identifiers[0];
                let same_id_count = identifiers
                    .iter()
                    .filter(|id| **id == first_id && id.is_some())
                    .count();

                if same_id_count >= 2 {
                    let id_name = first_id.unwrap_or("unknown");
                    groups.push(ErrorGroup {
                        root_index: import_errors[0],
                        member_indices: import_errors,
                        is_cascade: true,
                        cascade_hint: format!(
                            "Missing import for `{}` causes {} cascading errors — add the import to fix all",
                            id_name, same_id_count
                        ),
                    });
                    continue;
                }
            }

            // Pattern 2: First error is trivial/import, rest are downstream
            let first = &locs[0];
            if first.class == ErrorClass::Trivial && locs.len() >= 3 {
                groups.push(ErrorGroup {
                    root_index: indices[0],
                    member_indices: indices.clone(),
                    is_cascade: true,
                    cascade_hint: format!(
                        "{} errors likely cascade from line {} — fix `{}` first",
                        indices.len(),
                        first.line,
                        first.message.chars().take(50).collect::<String>()
                    ),
                });
                continue;
            }

            // Pattern 3: Same scope cascade — errors in the same function
            if !locs[0].scope.is_empty() {
                let same_scope: Vec<usize> = indices
                    .iter()
                    .filter(|&&i| self.error_locations[i].scope == locs[0].scope)
                    .copied()
                    .collect();
                if same_scope.len() >= 3 {
                    groups.push(ErrorGroup {
                        root_index: same_scope[0],
                        member_indices: same_scope.clone(),
                        is_cascade: true,
                        cascade_hint: format!(
                            "{} errors in `{}` — fix earliest (line {}) first",
                            same_scope.len(),
                            locs[0].scope,
                            self.error_locations[same_scope[0]].line
                        ),
                    });
                    continue;
                }
            }

            // Generic cluster: 3+ errors in same file
            groups.push(ErrorGroup {
                root_index: indices[0],
                member_indices: indices.clone(),
                is_cascade: indices.len() >= 3,
                cascade_hint: if indices.len() >= 3 {
                    format!(
                        "{} errors in file — fix line {} first, others may resolve",
                        indices.len(),
                        locs[0].line
                    )
                } else {
                    String::new()
                },
            });
        }

        // Sort groups: cascades first (most impactful), then by member count desc
        groups.sort_by(|a, b| {
            b.is_cascade
                .cmp(&a.is_cascade)
                .then(b.member_indices.len().cmp(&a.member_indices.len()))
        });

        groups
    }

    /// Generate a fix ordering: returns error indices sorted by dependency.
    /// Root-cause errors come first, downstream/cascading errors come last.
    pub fn fix_order(&self) -> Vec<usize> {
        let groups = self.analyze_error_groups();
        let mut ordered = Vec::new();
        let mut seen = HashSet::new();

        // First: root causes from cascade groups
        for g in &groups {
            if g.is_cascade && !seen.contains(&g.root_index) {
                ordered.push(g.root_index);
                seen.insert(g.root_index);
            }
        }

        // Then: trivial non-cascade errors
        for (i, loc) in self.error_locations.iter().enumerate() {
            if !seen.contains(&i) && loc.class == ErrorClass::Trivial {
                ordered.push(i);
                seen.insert(i);
            }
        }

        // Then: fixable
        for (i, loc) in self.error_locations.iter().enumerate() {
            if !seen.contains(&i) && loc.class == ErrorClass::Fixable {
                ordered.push(i);
                seen.insert(i);
            }
        }

        // Finally: complex
        for i in 0..self.error_locations.len() {
            if !seen.contains(&i) {
                ordered.push(i);
                seen.insert(i);
            }
        }

        ordered
    }
    /// Format as enhanced output with summary at top.
    pub fn to_enhanced_output(&self, raw_output: &str) -> String {
        let status_icon = if self.passed { "✓" } else { "✗" };
        let mut parts = Vec::new();

        // Summary line with exit code if non-zero
        let exit_suffix = if let Some(code) = self.exit_code {
            if code != 0 {
                format!(" (exit {})", code)
            } else {
                String::new()
            }
        } else {
            String::new()
        };

        if self.tests_passed > 0 || self.tests_failed > 0 {
            parts.push(format!(
                "{} {} | {} passed, {} failed{}{}",
                status_icon,
                self.framework,
                self.tests_passed,
                self.tests_failed,
                if self.tests_skipped > 0 {
                    format!(", {} skipped", self.tests_skipped)
                } else {
                    String::new()
                },
                exit_suffix,
            ));
        } else if !self.summary.is_empty() {
            parts.push(format!(
                "{} {} | {}{}",
                status_icon, self.framework, self.summary, exit_suffix
            ));
        } else {
            parts.push(format!(
                "{} {} | {}{}",
                status_icon,
                self.framework,
                if self.passed { "success" } else { "failed" },
                exit_suffix,
            ));
        }

        // Truncation warning
        if self.truncated {
            parts.push("⚠ Output was truncated".to_string());
        }

        // Error messages (top 5)
        if !self.error_messages.is_empty() {
            parts.push(String::new());
            parts.push("Errors:".to_string());
            for (i, msg) in self.error_messages.iter().take(5).enumerate() {
                parts.push(format!("  {}. {}", i + 1, msg));
            }
            if self.error_messages.len() > 5 {
                parts.push(format!("  ... and {} more", self.error_messages.len() - 5));
            }
        }

        // Error locations for direct navigation
        if !self.error_locations.is_empty() {
            parts.push(String::new());

            // Classify errors for the LLM: show fixable count
            let trivial_count = self
                .error_locations
                .iter()
                .filter(|l| l.class == ErrorClass::Trivial)
                .count();
            let fixable_count = self
                .error_locations
                .iter()
                .filter(|l| l.class == ErrorClass::Fixable)
                .count();
            let complex_count = self
                .error_locations
                .iter()
                .filter(|l| l.class == ErrorClass::Complex)
                .count();

            if trivial_count + fixable_count > 0 {
                parts.push(format!(
                    "Fix priority: {} trivial, {} fixable, {} complex",
                    trivial_count, fixable_count, complex_count
                ));
            }

            // Cascading error analysis — detect root causes and group related errors
            let groups = self.analyze_error_groups();
            let cascades: Vec<&ErrorGroup> = groups.iter().filter(|g| g.is_cascade).collect();
            if !cascades.is_empty() {
                parts.push(String::new());
                parts.push("⚡ Cascading errors — fix root cause FIRST:".to_string());
                for g in cascades.iter().take(3) {
                    let root = &self.error_locations[g.root_index];
                    parts.push(format!(
                        "  → {}:{} — {} ({} downstream errors will likely resolve)",
                        root.file,
                        root.line,
                        g.cascade_hint,
                        g.member_indices.len().saturating_sub(1)
                    ));
                }
            }

            // Show errors in fix order (root causes first)
            let fix_order = self.fix_order();
            parts.push("Locations (fix order):".to_string());
            for (rank, &idx) in fix_order.iter().take(10).enumerate() {
                let loc = &self.error_locations[idx];
                let code_part = if loc.error_code.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", loc.error_code)
                };
                let col_part = if loc.col > 0 {
                    format!(":{}", loc.col)
                } else {
                    String::new()
                };
                let class_tag = match loc.class {
                    ErrorClass::Trivial => " 🔧",
                    ErrorClass::Fixable => " 🔨",
                    ErrorClass::Complex => "",
                };
                let scope_part = if loc.scope.is_empty() {
                    String::new()
                } else {
                    format!(" (in {})", loc.scope)
                };
                let root_marker = if cascades.iter().any(|g| g.root_index == idx) {
                    " ← ROOT CAUSE"
                } else {
                    ""
                };
                parts.push(format!(
                    "  {}. {}:{}{}{} {}{}{}{}",
                    rank + 1,
                    loc.file,
                    loc.line,
                    col_part,
                    code_part,
                    loc.message,
                    class_tag,
                    scope_part,
                    root_marker
                ));
                if !loc.hint.is_empty() {
                    parts.push(format!("     💡 {}", loc.hint));
                }
            }
            if self.error_locations.len() > 10 {
                parts.push(format!(
                    "  ... and {} more locations",
                    self.error_locations.len() - 10
                ));
            }
        }

        // Include raw output for failed builds (full tail) and successful builds
        // (compact tail with warnings). Without this, successful `cargo check`
        // returns only "✓ unknown | completed" — the agent can't see warnings
        // or verify what was compiled.
        if !self.passed && raw_output.len() > 200 {
            parts.push(String::new());
            parts.push("─── Raw output (last 2000 chars) ───".to_string());
            let tail = if raw_output.len() > 2000 {
                &raw_output[raw_output.len() - 2000..]
            } else {
                raw_output
            };
            parts.push(tail.to_string());
        } else if !self.passed {
            parts.push(String::new());
            parts.push(raw_output.to_string());
        } else if !raw_output.is_empty() && self.error_messages.is_empty() {
            // Successful build/test — include compact tail so agent can see
            // warnings, compilation summary, and "Finished ..." line.
            let tail = if raw_output.len() > 500 {
                &raw_output[raw_output.len() - 500..]
            } else {
                raw_output
            };
            // Only include if there's something beyond whitespace
            let trimmed = tail.trim();
            if !trimmed.is_empty() {
                parts.push(String::new());
                parts.push(trimmed.to_string());
            }
        }

        parts.join("\n")
    }

    /// Enrich error locations with tree-sitter scope context.
    ///
    /// For each error location, reads the source file and uses tree-sitter's
    /// `scope_at_line()` to find the containing function/class. This adds
    /// context like "in fn process_data" or "in impl Foo > fn bar" to each error.
    pub fn enrich_with_scope(&mut self, project_root: &std::path::Path) {
        use super::code_intel::{detect_language, scope_at_line};
        use std::collections::HashMap;

        // Cache file contents to avoid re-reading for multiple errors in same file
        let mut file_cache: HashMap<String, Option<(String, super::code_intel::Language)>> =
            HashMap::new();

        for loc in self.error_locations.iter_mut() {
            let file_path = project_root.join(&loc.file);
            let key = loc.file.clone();

            let cached = file_cache.entry(key).or_insert_with(|| {
                let lang = detect_language(&file_path)?;
                let source = std::fs::read_to_string(&file_path).ok()?;
                Some((source, lang))
            });

            if let Some((source, lang)) = cached {
                let ctx = scope_at_line(source, *lang, loc.line);
                loc.scope = if ctx.breadcrumbs.len() > 1 {
                    ctx.breadcrumbs.join(" > ")
                } else if let Some(ref sym) = ctx.symbol {
                    sym.name.clone()
                } else {
                    String::new()
                };
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Build/Test Iteration Tracker — delta analysis across fix cycles
// ═══════════════════════════════════════════════════════════════════════════

/// Signature for an error location (fuzzy enough to survive line shifts).
/// Format: "file::error_code::message_prefix" — ignores line numbers
/// so that fixing above the error doesn't cause a false "new error".
fn error_signature(loc: &ErrorLocation) -> String {
    let msg_prefix: String = loc.message.chars().take(60).collect();
    format!("{}::{}::{}", loc.file, loc.error_code, msg_prefix)
}

/// Delta between two build/test iterations.
#[derive(Debug, Clone)]
pub struct BuildTestDelta {
    /// Which iteration this is (0 = first run)
    pub iteration: usize,
    /// Errors that are new since last run
    pub new_errors: Vec<String>,
    /// Errors that were fixed since last run
    pub fixed_errors: Vec<String>,
    /// Errors that persist from last run
    pub persistent_errors: Vec<String>,
    /// True if error count increased (regression)
    pub regressed: bool,
    /// Overall progress: errors fixed vs original count
    pub progress_pct: f64,
    /// Command that was run
    #[allow(dead_code)]
    pub command: String,
}

impl BuildTestDelta {
    /// Format the delta as a compact summary for the LLM.
    pub fn to_summary(&self) -> String {
        if self.iteration == 0 {
            return String::new(); // First run, no delta to show
        }
        let mut parts = Vec::new();

        // Compact header: iteration N: prev_count → current_count
        let prev = self.fixed_errors.len() + self.persistent_errors.len();
        let cur = self.new_errors.len() + self.persistent_errors.len();
        parts.push(format!(
            "─── Iteration {} ({} → {} errors) ───",
            self.iteration, prev, cur
        ));

        if !self.fixed_errors.is_empty() {
            parts.push(format!("✅ Fixed {} error(s)", self.fixed_errors.len()));
        }
        if !self.new_errors.is_empty() {
            parts.push(format!("🆕 {} new error(s)", self.new_errors.len()));
        }
        if !self.persistent_errors.is_empty() {
            parts.push(format!("⏳ {} still present", self.persistent_errors.len()));
        }
        if self.regressed {
            parts.push(
                "⚠ REGRESSION — more errors than before. Revert your last change and try a different approach.".into(),
            );
        }
        if self.progress_pct > 0.0 {
            parts.push(format!(
                "Progress: {:.0}% of original errors resolved",
                self.progress_pct
            ));
        }
        parts.join("\n")
    }
}

/// Tracks build/test results across iterations within a session.
///
/// Provides delta analysis: which errors were fixed, which are new,
/// which persist. Helps the LLM understand the impact of each fix
/// and avoid chasing regressions.
#[derive(Debug)]
pub struct BuildTestTracker {
    /// Previous result's error signatures
    previous_sigs: HashSet<String>,
    /// Original error signatures from the very first failed run
    original_sigs: HashSet<String>,
    /// Current iteration count
    iteration: usize,
    /// Last command that was run
    last_command: String,
}

impl BuildTestTracker {
    pub fn new() -> Self {
        Self {
            previous_sigs: HashSet::new(),
            original_sigs: HashSet::new(),
            iteration: 0,
            last_command: String::new(),
        }
    }

    /// Record a new build/test result and compute the delta.
    ///
    /// Call this each time `run_build_test` completes. The returned
    /// `BuildTestDelta` should be prepended to the tool output so the
    /// LLM can see what changed since its last fix attempt.
    pub fn record(&mut self, result: &BuildTestResult, command: &str) -> BuildTestDelta {
        let current_sigs: HashSet<String> =
            result.error_locations.iter().map(error_signature).collect();

        let delta = if self.iteration == 0 && !self.previous_sigs.is_empty() || self.iteration > 0 {
            // Subsequent run — compute delta
            let new_errors: Vec<String> = current_sigs
                .difference(&self.previous_sigs)
                .cloned()
                .collect();
            let fixed_errors: Vec<String> = self
                .previous_sigs
                .difference(&current_sigs)
                .cloned()
                .collect();
            let persistent_errors: Vec<String> = current_sigs
                .intersection(&self.previous_sigs)
                .cloned()
                .collect();
            let regressed = current_sigs.len() > self.previous_sigs.len();

            // Progress vs original errors
            let progress_pct = if self.original_sigs.is_empty() {
                0.0
            } else {
                let fixed_from_original = self.original_sigs.difference(&current_sigs).count();
                (fixed_from_original as f64 / self.original_sigs.len() as f64) * 100.0
            };

            BuildTestDelta {
                iteration: self.iteration,
                new_errors,
                fixed_errors,
                persistent_errors,
                regressed,
                progress_pct,
                command: command.to_string(),
            }
        } else {
            // First run — store baseline
            self.original_sigs = current_sigs.clone();
            BuildTestDelta {
                iteration: 0,
                new_errors: Vec::new(),
                fixed_errors: Vec::new(),
                persistent_errors: Vec::new(),
                regressed: false,
                progress_pct: 0.0,
                command: command.to_string(),
            }
        };

        self.previous_sigs = current_sigs;
        self.last_command = command.to_string();
        self.iteration += 1;
        delta
    }

    /// How many iterations have been recorded.
    #[allow(dead_code)]
    pub fn iterations(&self) -> usize {
        self.iteration
    }

    /// Reset tracker (e.g., when switching to a different command).
    pub fn reset(&mut self) {
        self.previous_sigs.clear();
        self.original_sigs.clear();
        self.iteration = 0;
        self.last_command.clear();
    }

    /// Whether the command changed since last run (triggers a reset).
    pub fn command_changed(&self, command: &str) -> bool {
        !self.last_command.is_empty() && self.last_command != command
    }
}

/// Classify an error and generate a quick-fix hint for the LLM.
fn classify_and_hint(error_code: &str, message: &str) -> (ErrorClass, String) {
    // Rust error codes
    match error_code {
        // Trivial: mechanical fixes
        "E0425" => {
            // cannot find value — usually missing import
            if let Some(name) = extract_identifier(message) {
                return (
                    ErrorClass::Trivial,
                    format!("Add `use` import for `{name}`, or check spelling"),
                );
            }
            (
                ErrorClass::Trivial,
                "Add missing import or check spelling".into(),
            )
        }
        "E0433" => (
            ErrorClass::Trivial,
            "Add missing `use` or crate dependency in Cargo.toml".into(),
        ),
        "E0432" => (
            ErrorClass::Trivial,
            "Fix import path — module or item doesn't exist at that path".into(),
        ),
        "E0412" => (
            ErrorClass::Trivial,
            "Type not found — add missing `use` import".into(),
        ),
        "E0603" => (
            ErrorClass::Trivial,
            "Item is private — add `pub` to definition or use a public re-export".into(),
        ),

        // Fixable: LLM can reason about these
        "E0308" => {
            // Mismatched types
            if message.contains("&str") && message.contains("String") {
                (
                    ErrorClass::Fixable,
                    "String/&str mismatch — use `.to_string()` or `&*s` / `.as_str()`".into(),
                )
            } else if message.contains("Option") {
                (
                    ErrorClass::Fixable,
                    "Wrap in Some() or unwrap with .unwrap_or()".into(),
                )
            } else {
                (
                    ErrorClass::Fixable,
                    "Check expected vs actual type — may need conversion or different return"
                        .into(),
                )
            }
        }
        "E0277" => (
            ErrorClass::Fixable,
            "Trait not satisfied — implement the trait or add a bound".into(),
        ),
        "E0599" => (
            ErrorClass::Fixable,
            "Method not found — check spelling, or the type may need a different impl/import"
                .into(),
        ),
        "E0061" => (
            ErrorClass::Fixable,
            "Wrong number of arguments — check function signature".into(),
        ),
        "E0063" => (
            ErrorClass::Fixable,
            "Missing struct fields — add the required fields".into(),
        ),
        "E0609" => (
            ErrorClass::Fixable,
            "No field on type — check struct definition for correct field name".into(),
        ),
        "E0107" => (
            ErrorClass::Fixable,
            "Wrong number of type arguments — check generic parameters".into(),
        ),
        "E0369" => (
            ErrorClass::Fixable,
            "Operator not implemented — derive trait or implement manually".into(),
        ),
        "E0046" => (
            ErrorClass::Fixable,
            "Missing trait method — implement the required method".into(),
        ),

        // Complex: requires deep understanding
        "E0382" | "E0505" | "E0502" => (
            ErrorClass::Complex,
            "Borrow/move error — restructure ownership or use .clone()".into(),
        ),
        "E0597" => (
            ErrorClass::Complex,
            "Value doesn't live long enough — restructure lifetimes".into(),
        ),
        "E0106" => (
            ErrorClass::Complex,
            "Missing lifetime — add explicit lifetime annotations".into(),
        ),
        "E0495" => (
            ErrorClass::Complex,
            "Conflicting lifetime requirements — simplify borrowing structure".into(),
        ),

        _ => {
            // Fallback: classify by message patterns
            classify_by_message(message)
        }
    }
}

/// Classify non-Rust errors or errors without codes by message content.
fn classify_by_message(message: &str) -> (ErrorClass, String) {
    let lower = message.to_lowercase();

    // Python / TypeScript / Go common patterns
    if lower.contains("import")
        && (lower.contains("not found")
            || lower.contains("cannot find")
            || lower.contains("no module"))
    {
        return (
            ErrorClass::Trivial,
            "Missing import — add the correct import statement".into(),
        );
    }
    if lower.contains("undefined") || lower.contains("is not defined") {
        return (
            ErrorClass::Fixable,
            "Undefined variable/function — check spelling or add import".into(),
        );
    }
    if lower.contains("type") && lower.contains("not assignable") {
        return (
            ErrorClass::Fixable,
            "Type mismatch — check expected vs actual type".into(),
        );
    }
    if lower.contains("unused") {
        return (
            ErrorClass::Trivial,
            "Unused variable — prefix with _ or remove".into(),
        );
    }
    if lower.contains("syntax error") || lower.contains("unexpected token") {
        return (
            ErrorClass::Fixable,
            "Syntax error — check for missing brackets, semicolons, or typos".into(),
        );
    }
    if lower.contains("assertion") || lower.contains("expected") && lower.contains("got") {
        return (
            ErrorClass::Complex,
            "Test assertion failure — check logic and expected values".into(),
        );
    }

    (ErrorClass::Fixable, String::new())
}

/// Extract an identifier name from an error message like "cannot find value `foo`".
fn extract_identifier(message: &str) -> Option<&str> {
    // Try backtick-quoted (Rust), then single-quoted (TypeScript/Go)
    for quote in ['`', '\''] {
        if let Some(start_pos) = message.find(quote) {
            let start = start_pos + 1;
            if let Some(end_offset) = message[start..].find(quote) {
                let end = start + end_offset;
                let candidate = &message[start..end];
                if !candidate.is_empty() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

/// Detect if a command is a build or test command.
pub fn is_build_test_command(command: &str) -> bool {
    let lower = command.to_lowercase();

    // Cargo
    if lower.contains("cargo build")
        || lower.contains("cargo test")
        || lower.contains("cargo check")
        || lower.contains("cargo clippy")
    {
        return true;
    }

    // Python
    if lower.contains("pytest")
        || lower.contains("python -m pytest")
        || lower.contains("python -m unittest")
    {
        return true;
    }

    // Node/JS
    if lower.contains("npm test")
        || lower.contains("npm run test")
        || lower.contains("yarn test")
        || lower.contains("jest")
        || lower.contains("vitest")
        || lower.contains("mocha")
    {
        return true;
    }

    // Go
    if lower.contains("go build") || lower.contains("go test") {
        return true;
    }

    // Make
    if lower.contains("make test") || lower.contains("make check") {
        return true;
    }

    // Generic
    lower.contains("npm run build") || lower.contains("yarn build")
}

/// Parse build/test output and return structured result.
pub fn parse_build_test_output(output: &str, exit_code: Option<i32>) -> BuildTestResult {
    let lower = output.to_lowercase();
    let truncated = output.contains("[truncated]");

    // Detect framework
    let framework = detect_framework(output);

    // Default passed status from exit code
    let passed_from_exit = exit_code.map(|c| c == 0).unwrap_or(true);

    match framework.as_str() {
        "cargo" => parse_cargo_output(output, exit_code, truncated),
        "pytest" => parse_pytest_output(output, exit_code, truncated),
        "jest" | "vitest" => parse_jest_output(output, exit_code, truncated),
        "go" => parse_go_output(output, exit_code, truncated),
        _ => {
            // Generic fallback
            let error_count = count_generic_errors(&lower);
            let (error_messages, error_locations) = extract_generic_errors(output);
            let passed = passed_from_exit && error_count == 0;
            BuildTestResult {
                passed,
                exit_code,
                framework,
                error_count,
                error_messages,
                error_locations,
                summary: if passed {
                    "completed".to_string()
                } else {
                    format!("{} error(s)", error_count)
                },
                truncated,
                ..Default::default()
            }
        }
    }
}

fn detect_framework(output: &str) -> String {
    let lower = output.to_lowercase();

    if lower.contains("compiling") && lower.contains("cargo")
        || lower.contains("error[e")
        || lower.contains("test result:")
        || lower.contains("running ")
            && (lower.contains(" tests") || lower.contains(" test"))
            && lower.contains("passed")
    {
        return "cargo".to_string();
    }

    if lower.contains("pytest") || lower.contains("===") && lower.contains("passed") {
        return "pytest".to_string();
    }

    if lower.contains("jest") || lower.contains("test suites:") {
        return "jest".to_string();
    }

    if lower.contains("vitest") {
        return "vitest".to_string();
    }

    if lower.contains("--- pass:") || lower.contains("--- fail:") || lower.contains("ok  \t") {
        return "go".to_string();
    }

    if lower.contains("mocha") {
        return "mocha".to_string();
    }

    "unknown".to_string()
}

// ─── Cargo Parser ────────────────────────────────────────────────────────────

static CARGO_ERROR_CODE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"error\[(E\d+)\]: (.+)").expect("valid regex"));

static CARGO_LOCATION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*--> (.+):(\d+):(\d+)").expect("valid regex"));

static CARGO_TEST_RESULT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored").expect("valid regex")
});

fn parse_cargo_output(output: &str, exit_code: Option<i32>, truncated: bool) -> BuildTestResult {
    let mut result = BuildTestResult {
        framework: "cargo".to_string(),
        exit_code,
        truncated,
        ..Default::default()
    };

    let lines: Vec<&str> = output.lines().collect();

    // Extract compilation errors with locations
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];

        // Match error[EXXXX]: message
        if let Some(cap) = CARGO_ERROR_CODE_RE.captures(line) {
            let code = cap.get(1).map(|m| m.as_str()).unwrap_or("");
            let msg = cap.get(2).map(|m| m.as_str().trim()).unwrap_or("");

            if !msg.is_empty() && result.error_messages.len() < 10 {
                result.error_messages.push(msg.to_string());
            }

            // Look for location on the next few lines (usually line i+1)
            for line in lines.iter().take(lines.len().min(i + 4)).skip(i + 1) {
                if let Some(loc_cap) = CARGO_LOCATION_RE.captures(line) {
                    let file = loc_cap.get(1).map(|m| m.as_str()).unwrap_or("");
                    let line_num: usize = loc_cap
                        .get(2)
                        .and_then(|m| m.as_str().parse().ok())
                        .unwrap_or(0);
                    let col: usize = loc_cap
                        .get(3)
                        .and_then(|m| m.as_str().parse().ok())
                        .unwrap_or(0);

                    if !file.is_empty() && line_num > 0 && result.error_locations.len() < 20 {
                        result.error_locations.push(ErrorLocation::new(
                            file.to_string(),
                            line_num,
                            col,
                            code.to_string(),
                            msg.to_string(),
                            "error".to_string(),
                        ));
                    }
                    break;
                }
            }
        } else if line.starts_with("error:")
            && !line.contains("could not compile")
            && !line.contains("aborting due to")
        {
            // Plain error: without code
            let msg = line.strip_prefix("error:").unwrap_or(line).trim();
            if !msg.is_empty() && result.error_messages.len() < 10 {
                result.error_messages.push(msg.to_string());
            }
            // Check for location
            for line in lines.iter().take(lines.len().min(i + 4)).skip(i + 1) {
                if let Some(loc_cap) = CARGO_LOCATION_RE.captures(line) {
                    let file = loc_cap.get(1).map(|m| m.as_str()).unwrap_or("");
                    let line_num: usize = loc_cap
                        .get(2)
                        .and_then(|m| m.as_str().parse().ok())
                        .unwrap_or(0);
                    let col: usize = loc_cap
                        .get(3)
                        .and_then(|m| m.as_str().parse().ok())
                        .unwrap_or(0);
                    if !file.is_empty() && line_num > 0 && result.error_locations.len() < 20 {
                        result.error_locations.push(ErrorLocation::new(
                            file.to_string(),
                            line_num,
                            col,
                            String::new(),
                            msg.to_string(),
                            "error".to_string(),
                        ));
                    }
                    break;
                }
            }
        }
        i += 1;
    }

    result.error_count = result.error_messages.len();

    // Check for test results
    if let Some(cap) = CARGO_TEST_RESULT_RE.captures(output) {
        let status = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        result.tests_passed = cap
            .get(2)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0);
        result.tests_failed = cap
            .get(3)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0);
        result.tests_skipped = cap
            .get(4)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0);

        result.passed = status == "ok";
        result.summary = format!(
            "{} passed, {} failed",
            result.tests_passed, result.tests_failed
        );

        // Extract failed test names with panic locations
        if result.tests_failed > 0 {
            let mut failed_tests = extract_cargo_failed_tests(output, &mut result.error_locations);
            result.error_messages.append(&mut failed_tests);
            result.error_count = result.tests_failed;
        }
    } else {
        // Build-only (no test result line)
        result.passed = result.error_count == 0 && exit_code.map(|c| c == 0).unwrap_or(true);

        if output.contains("Finished") {
            result.summary = "build succeeded".to_string();
            result.passed = true;
        } else if result.error_count > 0 {
            result.summary = format!("{} compilation error(s)", result.error_count);
        } else if !result.passed {
            result.summary = "build failed".to_string();
        } else {
            result.summary = "completed".to_string();
        }
    }

    result
}

static PANIC_LOCATION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"panicked at .+,?\s+(.+):(\d+):(\d+)"#).expect("valid regex"));

fn extract_cargo_failed_tests(output: &str, locations: &mut Vec<ErrorLocation>) -> Vec<String> {
    let mut failed = Vec::new();
    let lines: Vec<&str> = output.lines().collect();

    for (i, line) in lines.iter().enumerate() {
        // Look for "---- test_name stdout ----" pattern
        if line.contains("---- ")
            && line.contains(" stdout ----")
            && let Some(name) = line
                .strip_prefix("---- ")
                .and_then(|s| s.strip_suffix(" stdout ----"))
        {
            let mut msg = name.to_string();
            // Scan next lines for assertion/panic details
            for next_line in lines.iter().skip(i + 1).take(10) {
                if next_line.contains("assertion") || next_line.contains("panicked at") {
                    msg = format!("{}: {}", name, next_line.trim());

                    // Extract panic location
                    if let Some(pcap) = PANIC_LOCATION_RE.captures(next_line) {
                        let file = pcap.get(1).map(|m| m.as_str()).unwrap_or("");
                        let line_num: usize = pcap
                            .get(2)
                            .and_then(|m| m.as_str().parse().ok())
                            .unwrap_or(0);
                        let col: usize = pcap
                            .get(3)
                            .and_then(|m| m.as_str().parse().ok())
                            .unwrap_or(0);
                        if !file.is_empty() && line_num > 0 && locations.len() < 20 {
                            locations.push(ErrorLocation::new(
                                file.to_string(),
                                line_num,
                                col,
                                String::new(),
                                format!("test {name} panicked"),
                                "error".to_string(),
                            ));
                        }
                    }
                    break;
                }
            }
            if failed.len() < 10 {
                failed.push(msg);
            }
        }
    }

    failed
}

// ─── Pytest Parser ───────────────────────────────────────────────────────────

static PYTEST_SUMMARY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(\d+) passed.*?(\d+) failed|(\d+) passed").expect("valid regex"));

// Matches Python tracebacks: "  File "path.py", line 42, in func"
static PYTHON_TRACEBACK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"File "(.+?)", line (\d+)(?:, in (.+))?"#).expect("valid regex"));

fn parse_pytest_output(output: &str, exit_code: Option<i32>, truncated: bool) -> BuildTestResult {
    let mut result = BuildTestResult {
        framework: "pytest".to_string(),
        exit_code,
        truncated,
        ..Default::default()
    };

    // Parse summary line
    if let Some(cap) = PYTEST_SUMMARY_RE.captures(output) {
        if let Some(p) = cap.get(1) {
            result.tests_passed = p.as_str().parse().unwrap_or(0);
        } else if let Some(p) = cap.get(3) {
            result.tests_passed = p.as_str().parse().unwrap_or(0);
        }
        if let Some(f) = cap.get(2) {
            result.tests_failed = f.as_str().parse().unwrap_or(0);
        }
    }

    result.passed = result.tests_failed == 0 && exit_code.map(|c| c == 0).unwrap_or(true);
    result.error_count = result.tests_failed;

    // Extract failed test names and traceback locations
    let lines: Vec<&str> = output.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if line.starts_with("FAILED ") {
            let test_name = line.trim_start_matches("FAILED ").trim();
            if result.error_messages.len() < 10 {
                result.error_messages.push(test_name.to_string());
            }
        }

        // Extract traceback file locations (last File line before assertion error)
        if let Some(cap) = PYTHON_TRACEBACK_RE.captures(line) {
            let file = cap.get(1).map(|m| m.as_str()).unwrap_or("");
            let line_num: usize = cap
                .get(2)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);
            let func = cap.get(3).map(|m| m.as_str()).unwrap_or("");

            // Only include if the next few lines contain an assertion or error
            let is_relevant = lines.iter().skip(i + 1).take(3).any(|l| {
                l.contains("AssertionError")
                    || l.contains("assert ")
                    || l.contains("Error")
                    || l.contains("raise ")
            });

            if is_relevant && !file.is_empty() && line_num > 0 && result.error_locations.len() < 20
            {
                result.error_locations.push(ErrorLocation::new(
                    file.to_string(),
                    line_num,
                    0,
                    String::new(),
                    if func.is_empty() {
                        String::new()
                    } else {
                        format!("in {func}")
                    },
                    "error".to_string(),
                ));
            }
        }
    }

    result.summary = format!(
        "{} passed, {} failed",
        result.tests_passed, result.tests_failed
    );

    result
}

// ─── Jest/Vitest Parser ──────────────────────────────────────────────────────

static JEST_SUMMARY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"Tests:\s+(\d+) failed.*?(\d+) passed|Tests:\s+(\d+) passed").expect("valid regex")
});

fn parse_jest_output(output: &str, exit_code: Option<i32>, truncated: bool) -> BuildTestResult {
    let mut result = BuildTestResult {
        framework: if output.to_lowercase().contains("vitest") {
            "vitest"
        } else {
            "jest"
        }
        .to_string(),
        exit_code,
        truncated,
        ..Default::default()
    };

    if let Some(cap) = JEST_SUMMARY_RE.captures(output) {
        if let Some(f) = cap.get(1) {
            result.tests_failed = f.as_str().parse().unwrap_or(0);
        }
        if let Some(p) = cap.get(2) {
            result.tests_passed = p.as_str().parse().unwrap_or(0);
        } else if let Some(p) = cap.get(3) {
            result.tests_passed = p.as_str().parse().unwrap_or(0);
        }
    }

    result.passed = result.tests_failed == 0 && exit_code.map(|c| c == 0).unwrap_or(true);
    result.error_count = result.tests_failed;

    // Extract failed test descriptions
    for line in output.lines() {
        if (line.trim().starts_with("✕") || line.trim().starts_with("×"))
            && result.error_messages.len() < 10
        {
            result.error_messages.push(line.trim().to_string());
        }
    }

    result.summary = format!(
        "{} passed, {} failed",
        result.tests_passed, result.tests_failed
    );

    result
}

// ─── Go Test Parser ──────────────────────────────────────────────────────────

// Matches go test failure locations: "    main_test.go:25: expected true, got false"
static GO_TEST_LOCATION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s+(\S+\.go):(\d+): (.+)").expect("valid regex"));

fn parse_go_output(output: &str, exit_code: Option<i32>, truncated: bool) -> BuildTestResult {
    let mut result = BuildTestResult {
        framework: "go".to_string(),
        exit_code,
        truncated,
        ..Default::default()
    };

    let mut current_test = String::new();
    for line in output.lines() {
        if line.starts_with("--- PASS:") {
            result.tests_passed += 1;
        } else if line.starts_with("--- FAIL:") {
            result.tests_failed += 1;
            current_test = line
                .trim_start_matches("--- FAIL: ")
                .split(' ')
                .next()
                .unwrap_or("")
                .to_string();
            if result.error_messages.len() < 10 {
                result.error_messages.push(line.trim().to_string());
            }
        } else if line.starts_with("--- SKIP:") {
            result.tests_skipped += 1;
        } else if let Some(cap) = GO_TEST_LOCATION_RE.captures(line) {
            let file = cap.get(1).map(|m| m.as_str()).unwrap_or("");
            let line_num: usize = cap
                .get(2)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);
            let msg = cap.get(3).map(|m| m.as_str()).unwrap_or("");
            if !file.is_empty() && line_num > 0 && result.error_locations.len() < 20 {
                let full_msg = if current_test.is_empty() {
                    msg.to_string()
                } else {
                    format!("{}: {}", current_test, msg)
                };
                result.error_locations.push(ErrorLocation::new(
                    file.to_string(),
                    line_num,
                    0,
                    String::new(),
                    full_msg,
                    "error".to_string(),
                ));
            }
        }
    }

    result.passed = result.tests_failed == 0 && exit_code.map(|c| c == 0).unwrap_or(true);
    result.error_count = result.tests_failed;
    result.summary = format!(
        "{} passed, {} failed",
        result.tests_passed, result.tests_failed
    );

    result
}

// ─── Generic Fallback ────────────────────────────────────────────────────────

// Matches TypeScript errors: "src/app.ts(10,5): error TS2304: Cannot find name 'foo'"
static TS_ERROR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(.+)\((\d+),(\d+)\): error (TS\d+): (.+)").expect("valid regex"));

// Matches generic file:line:col: error patterns
static GENERIC_LOCATION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.+?):(\d+):(\d+):\s*(error|warning):\s*(.+)").expect("valid regex"));

fn count_generic_errors(lower: &str) -> usize {
    let mut count = 0;
    count += lower.matches("error:").count();
    count += lower.matches("error[").count();
    count += lower.matches("failed").count().min(5);
    count += lower.matches("exception").count();
    count
}

fn extract_generic_errors(output: &str) -> (Vec<String>, Vec<ErrorLocation>) {
    let mut errors = Vec::new();
    let mut locations = Vec::new();

    for line in output.lines() {
        let lower = line.to_lowercase();

        // Try TypeScript error format first
        if let Some(cap) = TS_ERROR_RE.captures(line) {
            let file = cap.get(1).map(|m| m.as_str()).unwrap_or("");
            let line_num: usize = cap
                .get(2)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);
            let col: usize = cap
                .get(3)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);
            let code = cap.get(4).map(|m| m.as_str()).unwrap_or("");
            let msg = cap.get(5).map(|m| m.as_str()).unwrap_or("");
            if errors.len() < 10 {
                errors.push(format!("[{code}] {msg}"));
            }
            if locations.len() < 20 {
                locations.push(ErrorLocation::new(
                    file.to_string(),
                    line_num,
                    col,
                    code.to_string(),
                    msg.to_string(),
                    "error".to_string(),
                ));
            }
            continue;
        }

        // Try generic file:line:col: error format
        if let Some(cap) = GENERIC_LOCATION_RE.captures(line) {
            let file = cap.get(1).map(|m| m.as_str()).unwrap_or("");
            let line_num: usize = cap
                .get(2)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);
            let col: usize = cap
                .get(3)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);
            let severity = cap.get(4).map(|m| m.as_str()).unwrap_or("error");
            let msg = cap.get(5).map(|m| m.as_str()).unwrap_or("");
            if errors.len() < 10 {
                errors.push(msg.to_string());
            }
            if locations.len() < 20 {
                locations.push(ErrorLocation::new(
                    file.to_string(),
                    line_num,
                    col,
                    String::new(),
                    msg.to_string(),
                    severity.to_string(),
                ));
            }
            continue;
        }

        // Fall through to simple text-matching
        if lower.contains("error:") || lower.contains("failed:") || lower.contains("exception:") {
            let trimmed = line.trim();
            if !trimmed.is_empty() && errors.len() < 10 {
                if trimmed.len() > 200 {
                    let mut end = 200;
                    while !trimmed.is_char_boundary(end) && end > 0 {
                        end -= 1;
                    }
                    errors.push(format!("{}...", &trimmed[..end]));
                } else {
                    errors.push(trimmed.to_string());
                }
            }
        }
    }

    (errors, locations)
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn detect_cargo_build() {
        assert!(is_build_test_command("cargo build"));
        assert!(is_build_test_command("cargo test --workspace"));
        assert!(is_build_test_command("RUST_BACKTRACE=1 cargo test"));
    }

    #[test]
    fn detect_pytest() {
        assert!(is_build_test_command("pytest tests/"));
        assert!(is_build_test_command("python -m pytest -v"));
    }

    #[test]
    fn detect_npm() {
        assert!(is_build_test_command("npm test"));
        assert!(is_build_test_command("npm run test"));
        assert!(is_build_test_command("yarn test"));
    }

    #[test]
    fn parse_cargo_test_success() {
        let output = r#"
   Compiling myproject v0.1.0
    Finished test [unoptimized + debuginfo] target(s) in 2.34s
     Running unittests src/lib.rs

running 10 tests
test tests::test_one ... ok
test tests::test_two ... ok
test result: ok. 10 passed; 0 failed; 2 ignored; 0 measured; 0 filtered out
"#;
        let result = parse_build_test_output(output, Some(0));
        assert!(result.passed);
        assert_eq!(result.framework, "cargo");
        assert_eq!(result.tests_passed, 10);
        assert_eq!(result.tests_failed, 0);
        assert_eq!(result.tests_skipped, 2);
    }

    #[test]
    fn parse_cargo_test_failure() {
        let output = r#"
running 3 tests
test tests::test_one ... ok
test tests::test_two ... FAILED
test tests::test_three ... ok

failures:

---- tests::test_two stdout ----
thread 'tests::test_two' panicked at 'assertion failed: false'

test result: FAILED. 2 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out
"#;
        let result = parse_build_test_output(output, Some(101));
        assert!(!result.passed);
        assert_eq!(result.tests_passed, 2);
        assert_eq!(result.tests_failed, 1);
        assert!(result.error_messages.iter().any(|m| m.contains("test_two")));
    }

    #[test]
    fn parse_cargo_build_error() {
        let output = r#"
   Compiling myproject v0.1.0
error[E0425]: cannot find value `foo` in this scope
 --> src/main.rs:10:5
  |
10 |     foo
  |     ^^^ not found in this scope

error[E0308]: mismatched types
 --> src/lib.rs:20:10
  |
20 |     return "hello";
  |            ^^^^^^^ expected i32, found &str

error: could not compile `myproject` due to 2 previous errors
"#;
        let result = parse_build_test_output(output, Some(101));
        assert!(!result.passed);
        assert_eq!(result.framework, "cargo");
        assert_eq!(result.error_count, 2);
        assert!(
            result
                .error_messages
                .iter()
                .any(|m| m.contains("cannot find value"))
        );

        // Verify error locations extracted
        assert_eq!(
            result.error_locations.len(),
            2,
            "should extract 2 error locations"
        );
        let loc0 = &result.error_locations[0];
        assert_eq!(loc0.file, "src/main.rs");
        assert_eq!(loc0.line, 10);
        assert_eq!(loc0.col, 5);
        assert_eq!(loc0.error_code, "E0425");
        assert!(loc0.message.contains("cannot find value"));

        let loc1 = &result.error_locations[1];
        assert_eq!(loc1.file, "src/lib.rs");
        assert_eq!(loc1.line, 20);
        assert_eq!(loc1.col, 10);
        assert_eq!(loc1.error_code, "E0308");
    }

    #[test]
    fn parse_pytest_success() {
        let output = r#"
============================= test session starts ==============================
collected 5 items

tests/test_main.py .....                                                  [100%]

============================== 5 passed in 0.12s ===============================
"#;
        let result = parse_build_test_output(output, Some(0));
        assert!(result.passed);
        assert_eq!(result.framework, "pytest");
        assert_eq!(result.tests_passed, 5);
        assert_eq!(result.tests_failed, 0);
    }

    #[test]
    fn parse_jest_output() {
        let output = r#"
PASS  src/__tests__/app.test.js
  ✓ should render (5 ms)
  ✓ should handle click (3 ms)

Test Suites: 1 passed, 1 total
Tests:       2 passed, 2 total
"#;
        let result = parse_build_test_output(output, Some(0));
        assert!(result.passed);
        assert_eq!(result.tests_passed, 2);
    }

    #[test]
    fn parse_go_test() {
        let output = r#"
=== RUN   TestMain
--- PASS: TestMain (0.00s)
=== RUN   TestHelper
--- PASS: TestHelper (0.01s)
=== RUN   TestError
--- FAIL: TestError (0.00s)
    main_test.go:25: expected true, got false
FAIL
exit status 1
"#;
        let result = parse_build_test_output(output, Some(1));
        assert!(!result.passed);
        assert_eq!(result.framework, "go");
        assert_eq!(result.tests_passed, 2);
        assert_eq!(result.tests_failed, 1);
    }

    #[test]
    fn enhanced_output_format() {
        let result = BuildTestResult {
            passed: false,
            framework: "cargo".to_string(),
            tests_passed: 45,
            tests_failed: 2,
            error_messages: vec![
                "test_auth: assertion failed".to_string(),
                "test_db: timeout".to_string(),
            ],
            ..Default::default()
        };
        let enhanced = result.to_enhanced_output("");
        assert!(enhanced.contains("✗"));
        assert!(enhanced.contains("45 passed"));
        assert!(enhanced.contains("2 failed"));
        assert!(enhanced.contains("Errors:"));
    }

    #[test]
    fn framework_detection() {
        assert_eq!(detect_framework("error[E0425]: cannot find"), "cargo");
        assert_eq!(detect_framework("test result: ok. 5 passed"), "cargo");
        assert_eq!(detect_framework("===== 5 passed ====="), "pytest");
        assert_eq!(detect_framework("Test Suites: 1 passed"), "jest");
        assert_eq!(detect_framework("--- PASS: TestMain"), "go");
    }

    // ─── Error Location Tests ─────────────────────────────────────────────

    #[test]
    fn cargo_error_locations_in_enhanced_output() {
        let result = BuildTestResult {
            passed: false,
            framework: "cargo".to_string(),
            error_count: 1,
            error_messages: vec!["cannot find value `foo`".to_string()],
            error_locations: vec![ErrorLocation::new(
                "src/main.rs".to_string(),
                10,
                5,
                "E0425".to_string(),
                "cannot find value `foo`".to_string(),
                "error".to_string(),
            )],
            summary: "1 compilation error(s)".to_string(),
            ..Default::default()
        };
        let output = result.to_enhanced_output("");
        assert!(
            output.contains("Locations"),
            "should have Locations section: {output}"
        );
        assert!(
            output.contains("src/main.rs:10:5"),
            "should show file:line:col: {output}"
        );
        assert!(
            output.contains("[E0425]"),
            "should show error code: {output}"
        );
    }

    #[test]
    fn go_test_extracts_locations() {
        let output = r#"
=== RUN   TestMain
--- PASS: TestMain (0.00s)
=== RUN   TestHelper
--- PASS: TestHelper (0.01s)
=== RUN   TestError
    main_test.go:25: expected true, got false
    main_test.go:30: another assertion failed
--- FAIL: TestError (0.00s)
FAIL
exit status 1
"#;
        let result = parse_build_test_output(output, Some(1));
        assert!(!result.passed);
        assert_eq!(result.framework, "go");
        assert_eq!(result.tests_failed, 1);
        assert!(
            !result.error_locations.is_empty(),
            "should extract go test locations"
        );
        assert_eq!(result.error_locations[0].file, "main_test.go");
        assert_eq!(result.error_locations[0].line, 25);
    }

    #[test]
    fn typescript_error_locations() {
        let output = r#"
src/app.ts(10,5): error TS2304: Cannot find name 'foo'
src/utils.ts(20,15): error TS2345: Argument of type 'string' is not assignable
"#;
        let result = parse_build_test_output(output, Some(1));
        assert!(!result.passed);
        assert_eq!(result.error_locations.len(), 2);
        assert_eq!(result.error_locations[0].file, "src/app.ts");
        assert_eq!(result.error_locations[0].line, 10);
        assert_eq!(result.error_locations[0].col, 5);
        assert_eq!(result.error_locations[0].error_code, "TS2304");
        assert_eq!(result.error_locations[1].file, "src/utils.ts");
        assert_eq!(result.error_locations[1].line, 20);
    }

    #[test]
    fn generic_file_line_col_error_locations() {
        let output = r#"
src/main.c:42:10: error: use of undeclared identifier 'x'
src/helper.c:15:3: warning: implicit conversion
"#;
        let result = parse_build_test_output(output, Some(1));
        assert_eq!(result.error_locations.len(), 2);
        assert_eq!(result.error_locations[0].file, "src/main.c");
        assert_eq!(result.error_locations[0].line, 42);
        assert_eq!(result.error_locations[0].severity, "error");
        assert_eq!(result.error_locations[1].severity, "warning");
    }

    #[test]
    fn enhanced_output_no_locations_when_empty() {
        let result = BuildTestResult {
            passed: true,
            framework: "cargo".to_string(),
            tests_passed: 10,
            summary: "10 passed, 0 failed".to_string(),
            ..Default::default()
        };
        let output = result.to_enhanced_output("");
        assert!(
            !output.contains("Locations"),
            "should not show Locations when empty"
        );
    }

    #[test]
    fn parse_cargo_panic_location_extraction() {
        let output = r#"
running 1 test
test tests::my_test ... FAILED

failures:

---- tests::my_test stdout ----
thread 'tests::my_test' panicked at 'assertion failed: x > 0', src/lib.rs:42:9

test result: FAILED. 0 passed; 1 failed; 0 ignored
"#;
        let result = parse_build_test_output(output, None);
        assert!(!result.passed);
        assert_eq!(result.tests_failed, 1);
        // Verify panic location was correctly extracted
        assert!(
            !result.error_locations.is_empty(),
            "should extract panic location, got: {:?}",
            result.error_locations
        );
        let loc = &result.error_locations[0];
        assert_eq!(loc.file, "src/lib.rs", "file should be src/lib.rs");
        assert_eq!(loc.line, 42, "line should be 42");
        assert_eq!(loc.col, 9, "col should be 9");
        assert!(
            loc.message.contains("my_test"),
            "message should reference test name"
        );
    }

    // ─── Error classification tests ────────────────────────────────────

    #[test]
    fn classify_rust_trivial_missing_import() {
        let (class, hint) = classify_and_hint("E0425", "cannot find value `HashMap` in this scope");
        assert_eq!(class, ErrorClass::Trivial);
        assert!(
            hint.contains("HashMap"),
            "hint should mention the identifier: {hint}"
        );
        assert!(
            hint.contains("import"),
            "hint should suggest import: {hint}"
        );
    }

    #[test]
    fn classify_rust_trivial_missing_crate() {
        let (class, hint) = classify_and_hint(
            "E0433",
            "failed to resolve: use of undeclared crate or module `serde`",
        );
        assert_eq!(class, ErrorClass::Trivial);
        assert!(
            hint.contains("Cargo.toml"),
            "hint should mention Cargo.toml: {hint}"
        );
    }

    #[test]
    fn classify_rust_fixable_type_mismatch() {
        let (class, hint) =
            classify_and_hint("E0308", "mismatched types: expected &str, found String");
        assert_eq!(class, ErrorClass::Fixable);
        assert!(
            hint.contains("to_string") || hint.contains("as_str"),
            "hint should suggest conversion: {hint}"
        );
    }

    #[test]
    fn classify_rust_fixable_option_mismatch() {
        let (class, hint) = classify_and_hint("E0308", "expected Option<i32>, found i32");
        assert_eq!(class, ErrorClass::Fixable);
        assert!(hint.contains("Some"), "hint should suggest Some: {hint}");
    }

    #[test]
    fn classify_rust_complex_borrow() {
        for code in &["E0382", "E0505", "E0502"] {
            let (class, _hint) = classify_and_hint(code, "value moved here");
            assert_eq!(class, ErrorClass::Complex, "code {code} should be Complex");
        }
    }

    #[test]
    fn classify_rust_complex_lifetime() {
        let (class, hint) = classify_and_hint("E0597", "borrowed value does not live long enough");
        assert_eq!(class, ErrorClass::Complex);
        assert!(
            hint.contains("lifetime"),
            "hint should mention lifetime: {hint}"
        );
    }

    #[test]
    fn classify_unknown_code_by_message() {
        let (class, _) = classify_and_hint("", "unused variable: `x`");
        assert_eq!(class, ErrorClass::Trivial);

        let (class, _) = classify_and_hint("", "import 'foo' not found");
        assert_eq!(class, ErrorClass::Trivial);

        let (class, _) = classify_and_hint("", "syntax error: unexpected token '}'");
        assert_eq!(class, ErrorClass::Fixable);

        let (class, _) = classify_and_hint("", "assertion failed: expected 5, got 3");
        assert_eq!(class, ErrorClass::Complex);
    }

    #[test]
    fn classify_extract_identifier() {
        assert_eq!(
            extract_identifier("cannot find value `foo` in scope"),
            Some("foo")
        );
        assert_eq!(extract_identifier("no backticks here"), None);
        assert_eq!(extract_identifier("found `Bar` not defined"), Some("Bar"));
    }

    #[test]
    fn enhanced_output_shows_classification_summary() {
        let result = BuildTestResult {
            passed: false,
            error_count: 3,
            error_messages: vec!["E0425".into(), "E0308".into(), "E0382".into()],
            error_locations: vec![
                ErrorLocation::new(
                    "src/a.rs".into(),
                    1,
                    0,
                    "E0425".into(),
                    "cannot find value `x`".into(),
                    "error".into(),
                ),
                ErrorLocation::new(
                    "src/b.rs".into(),
                    2,
                    0,
                    "E0308".into(),
                    "mismatched types".into(),
                    "error".into(),
                ),
                ErrorLocation::new(
                    "src/c.rs".into(),
                    3,
                    0,
                    "E0382".into(),
                    "value moved".into(),
                    "error".into(),
                ),
            ],
            summary: "3 errors".into(),
            ..Default::default()
        };
        let output = result.to_enhanced_output("");
        assert!(
            output.contains("trivial"),
            "should show trivial count: {output}"
        );
        assert!(
            output.contains("fixable"),
            "should show fixable count: {output}"
        );
        assert!(
            output.contains("complex"),
            "should show complex count: {output}"
        );
    }

    #[test]
    fn enhanced_output_shows_hints() {
        let result = BuildTestResult {
            passed: false,
            error_count: 1,
            error_messages: vec!["cannot find value `foo`".into()],
            error_locations: vec![ErrorLocation::new(
                "src/main.rs".into(),
                10,
                5,
                "E0425".into(),
                "cannot find value `foo`".into(),
                "error".into(),
            )],
            summary: "1 error".into(),
            ..Default::default()
        };
        let output = result.to_enhanced_output("");
        assert!(output.contains("💡"), "should show hint icon: {output}");
        assert!(
            output.contains("import"),
            "hint should mention import: {output}"
        );
        assert!(
            output.contains("🔧"),
            "trivial should get wrench icon: {output}"
        );
    }

    #[test]
    fn enhanced_output_cascading_detection() {
        let result = BuildTestResult {
            passed: false,
            error_count: 4,
            error_messages: vec!["err1".into(), "err2".into(), "err3".into(), "err4".into()],
            error_locations: vec![
                ErrorLocation::new(
                    "src/same.rs".into(),
                    10,
                    0,
                    "E0425".into(),
                    "err1".into(),
                    "error".into(),
                ),
                ErrorLocation::new(
                    "src/same.rs".into(),
                    20,
                    0,
                    "E0308".into(),
                    "err2".into(),
                    "error".into(),
                ),
                ErrorLocation::new(
                    "src/same.rs".into(),
                    30,
                    0,
                    "E0599".into(),
                    "err3".into(),
                    "error".into(),
                ),
            ],
            summary: "3 errors in same file".into(),
            ..Default::default()
        };
        let output = result.to_enhanced_output("");
        assert!(
            output.contains("Cascading") || output.contains("cascade"),
            "should detect cascading errors in same file: {output}"
        );
        assert!(
            output.contains("ROOT CAUSE"),
            "should mark root cause: {output}"
        );
    }

    #[test]
    fn errorlocation_new_auto_classifies() {
        let loc = ErrorLocation::new(
            "f.rs".into(),
            1,
            0,
            "E0425".into(),
            "cannot find value `x`".into(),
            "error".into(),
        );
        assert_eq!(loc.class, ErrorClass::Trivial);
        assert!(!loc.hint.is_empty());

        let loc2 = ErrorLocation::new(
            "f.rs".into(),
            1,
            0,
            "E0382".into(),
            "value moved".into(),
            "error".into(),
        );
        assert_eq!(loc2.class, ErrorClass::Complex);

        let loc3 = ErrorLocation::new(
            "f.rs".into(),
            1,
            0,
            "".into(),
            "random error".into(),
            "error".into(),
        );
        assert_eq!(loc3.class, ErrorClass::Fixable);
    }

    #[test]
    fn rust_error_gets_classified_in_parse() {
        let output = r#"
error[E0425]: cannot find value `nonexistent` in this scope
  --> src/main.rs:10:5
   |
10 |     nonexistent;
   |     ^^^^^^^^^^^ not found in this scope
"#;
        let result = parse_build_test_output(output, Some(101));
        assert!(!result.error_locations.is_empty());
        let loc = &result.error_locations[0];
        assert_eq!(loc.error_code, "E0425");
        assert_eq!(loc.class, ErrorClass::Trivial, "E0425 should be trivial");
        assert!(!loc.hint.is_empty(), "should have a hint");
    }

    #[test]
    fn enrich_with_scope_fills_scope_field() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut result = BuildTestResult {
            error_locations: vec![ErrorLocation::new(
                "src/edge_tools/code_intel.rs".into(),
                138,
                5,
                "E0425".into(),
                "cannot find value".into(),
                "error".into(),
            )],
            ..Default::default()
        };
        // Scope should be empty initially
        assert!(result.error_locations[0].scope.is_empty());
        // Enrich
        result.enrich_with_scope(root);
        // After enrichment, scope should have the containing function
        let scope = &result.error_locations[0].scope;
        assert!(
            !scope.is_empty(),
            "scope should be filled after enrichment for a valid Rust file"
        );
    }

    #[test]
    fn enrich_with_scope_handles_nonexistent_file() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut result = BuildTestResult {
            error_locations: vec![ErrorLocation::new(
                "nonexistent_file.rs".into(),
                10,
                0,
                "".into(),
                "some error".into(),
                "error".into(),
            )],
            ..Default::default()
        };
        result.enrich_with_scope(root);
        // Should not crash, scope should remain empty
        assert!(result.error_locations[0].scope.is_empty());
    }

    #[test]
    fn scope_field_appears_in_enhanced_output() {
        let result = BuildTestResult {
            passed: false,
            framework: "cargo".into(),
            error_count: 1,
            error_locations: vec![ErrorLocation {
                file: "src/lib.rs".into(),
                line: 42,
                col: 5,
                error_code: "E0308".into(),
                message: "type mismatch".into(),
                severity: "error".into(),
                class: ErrorClass::Fixable,
                hint: "check types".into(),
                scope: "impl Foo > fn process".into(),
            }],
            summary: "Build failed".into(),
            ..Default::default()
        };
        let output = result.to_enhanced_output("");
        assert!(
            output.contains("(in impl Foo > fn process)"),
            "scope should appear in enhanced output: {output}"
        );
    }

    // ═══════════════════════ Cascading Analysis Tests ═══════════════════════

    #[test]
    fn analyze_import_cascade_same_identifier() {
        let result = BuildTestResult {
            error_locations: vec![
                ErrorLocation::new(
                    "src/a.rs".into(),
                    10,
                    1,
                    "E0425".into(),
                    "cannot find value `Foo`".into(),
                    "error".into(),
                ),
                ErrorLocation::new(
                    "src/a.rs".into(),
                    20,
                    1,
                    "E0425".into(),
                    "cannot find value `Foo`".into(),
                    "error".into(),
                ),
                ErrorLocation::new(
                    "src/a.rs".into(),
                    30,
                    1,
                    "E0433".into(),
                    "cannot find `Foo` in this scope".into(),
                    "error".into(),
                ),
            ],
            ..Default::default()
        };
        let groups = result.analyze_error_groups();
        assert!(!groups.is_empty());
        let cascade = groups.iter().find(|g| g.is_cascade);
        assert!(cascade.is_some(), "should detect import cascade");
        let c = cascade.unwrap();
        assert!(
            c.cascade_hint.contains("Foo"),
            "hint should mention the identifier: {}",
            c.cascade_hint
        );
    }

    #[test]
    fn analyze_trivial_first_cascade() {
        let result = BuildTestResult {
            error_locations: vec![
                ErrorLocation::new(
                    "src/b.rs".into(),
                    5,
                    1,
                    "E0432".into(),
                    "unresolved import".into(),
                    "error".into(),
                ),
                ErrorLocation::new(
                    "src/b.rs".into(),
                    15,
                    1,
                    "E0308".into(),
                    "mismatched types".into(),
                    "error".into(),
                ),
                ErrorLocation::new(
                    "src/b.rs".into(),
                    25,
                    1,
                    "E0599".into(),
                    "method not found".into(),
                    "error".into(),
                ),
            ],
            ..Default::default()
        };
        let groups = result.analyze_error_groups();
        let cascade = groups.iter().find(|g| g.is_cascade);
        assert!(
            cascade.is_some(),
            "trivial-first with 3 errors should cascade"
        );
        let c = cascade.unwrap();
        assert_eq!(c.root_index, 0, "root should be the first (trivial) error");
    }

    #[test]
    fn analyze_no_cascade_two_errors() {
        let result = BuildTestResult {
            error_locations: vec![
                ErrorLocation::new(
                    "src/c.rs".into(),
                    10,
                    1,
                    "E0308".into(),
                    "type mismatch".into(),
                    "error".into(),
                ),
                ErrorLocation::new(
                    "src/c.rs".into(),
                    20,
                    1,
                    "E0277".into(),
                    "trait not satisfied".into(),
                    "error".into(),
                ),
            ],
            ..Default::default()
        };
        let groups = result.analyze_error_groups();
        // 2 errors, first is Fixable (not Trivial), so no cascade
        assert!(
            groups.iter().all(|g| !g.is_cascade),
            "2 non-trivial errors should not be cascade"
        );
    }

    #[test]
    fn analyze_multi_file_groups() {
        let result = BuildTestResult {
            error_locations: vec![
                ErrorLocation::new(
                    "src/a.rs".into(),
                    10,
                    1,
                    "E0425".into(),
                    "not found".into(),
                    "error".into(),
                ),
                ErrorLocation::new(
                    "src/b.rs".into(),
                    20,
                    1,
                    "E0308".into(),
                    "type mismatch".into(),
                    "error".into(),
                ),
                ErrorLocation::new(
                    "src/c.rs".into(),
                    30,
                    1,
                    "E0599".into(),
                    "method not found".into(),
                    "error".into(),
                ),
            ],
            ..Default::default()
        };
        let groups = result.analyze_error_groups();
        assert_eq!(groups.len(), 3, "each file should be its own group");
        assert!(
            groups.iter().all(|g| !g.is_cascade),
            "single error per file = no cascade"
        );
    }

    #[test]
    fn fix_order_root_causes_first() {
        let result = BuildTestResult {
            error_locations: vec![
                // File A: cascade (trivial root)
                ErrorLocation::new(
                    "src/a.rs".into(),
                    5,
                    1,
                    "E0425".into(),
                    "cannot find value `X`".into(),
                    "error".into(),
                ),
                ErrorLocation::new(
                    "src/a.rs".into(),
                    15,
                    1,
                    "E0308".into(),
                    "type mismatch".into(),
                    "error".into(),
                ),
                ErrorLocation::new(
                    "src/a.rs".into(),
                    25,
                    1,
                    "E0599".into(),
                    "method not found".into(),
                    "error".into(),
                ),
                // File B: standalone complex error
                ErrorLocation::new(
                    "src/b.rs".into(),
                    10,
                    1,
                    "E0382".into(),
                    "value moved".into(),
                    "error".into(),
                ),
            ],
            ..Default::default()
        };
        let order = result.fix_order();
        assert_eq!(order.len(), 4);
        // Root cause (index 0, E0425 cascade root) should be first
        assert_eq!(order[0], 0, "cascade root should be first");
        // Complex error should be last
        assert_eq!(*order.last().unwrap(), 3, "complex error should be last");
    }

    #[test]
    fn fix_order_trivial_before_fixable() {
        let result = BuildTestResult {
            error_locations: vec![
                ErrorLocation::new(
                    "src/a.rs".into(),
                    10,
                    1,
                    "E0308".into(),
                    "type mismatch".into(),
                    "error".into(),
                ),
                ErrorLocation::new(
                    "src/b.rs".into(),
                    20,
                    1,
                    "E0425".into(),
                    "not found".into(),
                    "error".into(),
                ),
                ErrorLocation::new(
                    "src/c.rs".into(),
                    30,
                    1,
                    "E0382".into(),
                    "value moved".into(),
                    "error".into(),
                ),
            ],
            ..Default::default()
        };
        let order = result.fix_order();
        // Index 1 (Trivial E0425) should come before index 0 (Fixable E0308)
        let trivial_pos = order.iter().position(|&i| i == 1).unwrap();
        let fixable_pos = order.iter().position(|&i| i == 0).unwrap();
        let complex_pos = order.iter().position(|&i| i == 2).unwrap();
        assert!(
            trivial_pos < fixable_pos,
            "trivial should be before fixable"
        );
        assert!(
            fixable_pos < complex_pos,
            "fixable should be before complex"
        );
    }

    #[test]
    fn enhanced_output_shows_fix_order_numbers() {
        let result = BuildTestResult {
            passed: false,
            error_locations: vec![
                ErrorLocation::new(
                    "src/a.rs".into(),
                    10,
                    5,
                    "E0308".into(),
                    "type mismatch".into(),
                    "error".into(),
                ),
                ErrorLocation::new(
                    "src/b.rs".into(),
                    20,
                    1,
                    "E0425".into(),
                    "not found".into(),
                    "error".into(),
                ),
            ],
            ..Default::default()
        };
        let output = result.to_enhanced_output("");
        // Should show numbered list
        assert!(
            output.contains("1."),
            "should have numbered errors: {output}"
        );
        assert!(output.contains("2."), "should have second error: {output}");
    }

    #[test]
    fn enhanced_output_marks_root_cause() {
        let result = BuildTestResult {
            passed: false,
            error_locations: vec![
                ErrorLocation::new(
                    "src/a.rs".into(),
                    5,
                    1,
                    "E0425".into(),
                    "cannot find `X`".into(),
                    "error".into(),
                ),
                ErrorLocation::new(
                    "src/a.rs".into(),
                    15,
                    1,
                    "E0308".into(),
                    "type mismatch".into(),
                    "error".into(),
                ),
                ErrorLocation::new(
                    "src/a.rs".into(),
                    25,
                    1,
                    "E0599".into(),
                    "method not found".into(),
                    "error".into(),
                ),
            ],
            ..Default::default()
        };
        let output = result.to_enhanced_output("");
        assert!(
            output.contains("ROOT CAUSE"),
            "should mark root cause in cascade: {output}"
        );
        assert!(
            output.contains("cascade"),
            "should show cascade hint: {output}"
        );
    }

    // ═══════════════════════ BuildTestTracker Tests ═══════════════════════

    fn make_errors(specs: &[(&str, &str, &str)]) -> Vec<ErrorLocation> {
        specs
            .iter()
            .map(|(file, code, msg)| {
                ErrorLocation::new(
                    file.to_string(),
                    10,
                    1,
                    code.to_string(),
                    msg.to_string(),
                    "error".into(),
                )
            })
            .collect()
    }

    fn make_result(errors: Vec<ErrorLocation>) -> BuildTestResult {
        BuildTestResult {
            passed: errors.is_empty(),
            error_count: errors.len(),
            error_locations: errors,
            framework: "cargo".into(),
            ..Default::default()
        }
    }

    #[test]
    fn tracker_first_run_no_delta() {
        let mut tracker = BuildTestTracker::new();
        let result = make_result(make_errors(&[
            ("src/a.rs", "E0425", "cannot find value `foo`"),
            ("src/b.rs", "E0308", "mismatched types"),
        ]));
        let delta = tracker.record(&result, "cargo build");
        assert_eq!(delta.iteration, 0);
        assert!(delta.new_errors.is_empty());
        assert!(delta.fixed_errors.is_empty());
        assert!(!delta.regressed);
        assert_eq!(delta.to_summary(), ""); // No summary for first run
    }

    #[test]
    fn tracker_detects_fixed_errors() {
        let mut tracker = BuildTestTracker::new();
        // First run: 2 errors
        let r1 = make_result(make_errors(&[
            ("src/a.rs", "E0425", "cannot find value `foo`"),
            ("src/b.rs", "E0308", "mismatched types"),
        ]));
        tracker.record(&r1, "cargo build");

        // Second run: only 1 error (fixed E0425)
        let r2 = make_result(make_errors(&[("src/b.rs", "E0308", "mismatched types")]));
        let delta = tracker.record(&r2, "cargo build");

        assert_eq!(delta.iteration, 1);
        assert_eq!(delta.fixed_errors.len(), 1);
        assert!(delta.fixed_errors[0].contains("E0425"));
        assert!(delta.persistent_errors.len() == 1);
        assert!(!delta.regressed);
        assert!(delta.progress_pct > 0.0);
    }

    #[test]
    fn tracker_detects_new_errors() {
        let mut tracker = BuildTestTracker::new();
        let r1 = make_result(make_errors(&[(
            "src/a.rs",
            "E0425",
            "cannot find value `foo`",
        )]));
        tracker.record(&r1, "cargo build");

        // Fixed old error but introduced a new one
        let r2 = make_result(make_errors(&[("src/c.rs", "E0277", "trait not satisfied")]));
        let delta = tracker.record(&r2, "cargo build");

        assert_eq!(delta.new_errors.len(), 1);
        assert_eq!(delta.fixed_errors.len(), 1);
        assert!(delta.persistent_errors.is_empty());
        assert!(!delta.regressed); // Same count, not regression
    }

    #[test]
    fn tracker_detects_regression() {
        let mut tracker = BuildTestTracker::new();
        let r1 = make_result(make_errors(&[(
            "src/a.rs",
            "E0425",
            "cannot find value `foo`",
        )]));
        tracker.record(&r1, "cargo build");

        // Introduced MORE errors than before
        let r2 = make_result(make_errors(&[
            ("src/a.rs", "E0425", "cannot find value `foo`"),
            ("src/b.rs", "E0308", "mismatched types"),
            ("src/c.rs", "E0277", "trait not satisfied"),
        ]));
        let delta = tracker.record(&r2, "cargo build");

        assert!(delta.regressed);
        assert_eq!(delta.new_errors.len(), 2);
        assert_eq!(delta.persistent_errors.len(), 1);
        let summary = delta.to_summary();
        assert!(
            summary.contains("REGRESSION"),
            "should warn about regression: {summary}"
        );
    }

    #[test]
    fn tracker_full_resolution_shows_100_percent() {
        let mut tracker = BuildTestTracker::new();
        let r1 = make_result(make_errors(&[
            ("src/a.rs", "E0425", "cannot find value `foo`"),
            ("src/b.rs", "E0308", "mismatched types"),
        ]));
        tracker.record(&r1, "cargo build");

        // All errors fixed!
        let r2 = make_result(Vec::new());
        let delta = tracker.record(&r2, "cargo build");

        assert_eq!(delta.fixed_errors.len(), 2);
        assert!(delta.new_errors.is_empty());
        assert!((delta.progress_pct - 100.0).abs() < 0.001);
    }

    #[test]
    fn tracker_command_change_resets() {
        let mut tracker = BuildTestTracker::new();
        let r1 = make_result(make_errors(&[(
            "src/a.rs",
            "E0425",
            "cannot find value `foo`",
        )]));
        tracker.record(&r1, "cargo build");
        assert_eq!(tracker.iterations(), 1);

        // Different command — should reset
        assert!(tracker.command_changed("cargo test"));
        tracker.reset();
        let r2 = make_result(make_errors(&[(
            "src/a.rs",
            "E0425",
            "cannot find value `foo`",
        )]));
        let delta = tracker.record(&r2, "cargo test");
        assert_eq!(delta.iteration, 0); // Fresh start
    }

    #[test]
    fn tracker_multi_iteration_progress() {
        let mut tracker = BuildTestTracker::new();
        // 4 errors initially
        let r1 = make_result(make_errors(&[
            ("src/a.rs", "E0425", "cannot find value `foo`"),
            ("src/b.rs", "E0308", "mismatched types"),
            ("src/c.rs", "E0277", "trait not satisfied"),
            ("src/d.rs", "E0599", "method not found"),
        ]));
        tracker.record(&r1, "cargo build");

        // Fix 2
        let r2 = make_result(make_errors(&[
            ("src/c.rs", "E0277", "trait not satisfied"),
            ("src/d.rs", "E0599", "method not found"),
        ]));
        let d2 = tracker.record(&r2, "cargo build");
        assert_eq!(d2.fixed_errors.len(), 2);
        assert!((d2.progress_pct - 50.0).abs() < 0.001);

        // Fix 1 more
        let r3 = make_result(make_errors(&[("src/d.rs", "E0599", "method not found")]));
        let d3 = tracker.record(&r3, "cargo build");
        assert_eq!(d3.fixed_errors.len(), 1);
        assert!((d3.progress_pct - 75.0).abs() < 0.001);
        assert_eq!(d3.iteration, 2);
    }

    #[test]
    fn tracker_error_signature_ignores_line_number() {
        let loc1 = ErrorLocation::new(
            "src/a.rs".into(),
            10,
            1,
            "E0425".into(),
            "cannot find value `foo`".into(),
            "error".into(),
        );
        let loc2 = ErrorLocation::new(
            "src/a.rs".into(),
            20,
            1,
            "E0425".into(),
            "cannot find value `foo`".into(),
            "error".into(),
        );
        assert_eq!(error_signature(&loc1), error_signature(&loc2));
    }

    #[test]
    fn delta_summary_format() {
        let delta = BuildTestDelta {
            iteration: 2,
            new_errors: vec!["new1".into()],
            fixed_errors: vec!["fix1".into(), "fix2".into()],
            persistent_errors: vec!["persist1".into()],
            regressed: false,
            progress_pct: 66.7,
            command: "cargo build".into(),
        };
        let summary = delta.to_summary();
        assert!(summary.contains("Iteration 2"));
        assert!(summary.contains("Fixed 2"));
        assert!(summary.contains("1 new"));
        assert!(summary.contains("1 still present"));
        assert!(summary.contains("67%")); // 66.7 rounds to 67
    }

    // ────────────────────────────────────────────────────────
    // suggest_fix tests
    // ────────────────────────────────────────────────────────

    fn make_error(file: &str, line: usize, code: &str, msg: &str) -> ErrorLocation {
        ErrorLocation {
            file: file.to_string(),
            line,
            col: 0,
            error_code: code.to_string(),
            message: msg.to_string(),
            severity: "error".to_string(),
            class: ErrorClass::Fixable,
            hint: String::new(),
            scope: String::new(),
        }
    }

    #[test]
    fn fix_unused_variable() {
        let err = make_error("src/main.rs", 3, "", "unused variable: `count`");
        let source = vec!["fn main() {", "    let x = 1;", "    let count = 42;", "}"];
        let fixes = suggest_fix(&err, &source);
        assert_eq!(fixes.len(), 1);
        assert_eq!(fixes[0].action, "replace");
        assert!(fixes[0].new_text.contains("_count"));
        assert!(fixes[0].confidence >= 0.9);
    }

    #[test]
    fn fix_unused_import() {
        let err = make_error("src/lib.rs", 2, "", "unused import: `HashMap`");
        let source = vec![
            "use std::io;",
            "use std::collections::HashMap;",
            "",
            "fn main() {}",
        ];
        let fixes = suggest_fix(&err, &source);
        assert_eq!(fixes.len(), 1);
        assert_eq!(fixes[0].action, "delete_line");
        assert_eq!(fixes[0].line, 2);
        assert!(fixes[0].confidence >= 0.9);
    }

    #[test]
    fn fix_missing_import_hashmap() {
        let err = make_error(
            "src/main.rs",
            5,
            "E0425",
            "cannot find value `HashMap` in this scope",
        );
        let source = vec!["fn main() {", "    let m = HashMap::new();", "}"];
        let fixes = suggest_fix(&err, &source);
        assert_eq!(fixes.len(), 1);
        assert_eq!(fixes[0].action, "add_import");
        assert!(fixes[0].new_text.contains("std::collections::HashMap"));
        assert!(fixes[0].confidence >= 0.7);
    }

    #[test]
    fn fix_missing_import_arc() {
        let err = make_error(
            "src/lib.rs",
            1,
            "E0433",
            "failed to resolve: use of undeclared type `Arc`",
        );
        let source = vec!["let a = Arc::new(42);"];
        let fixes = suggest_fix(&err, &source);
        assert_eq!(fixes.len(), 1);
        assert!(fixes[0].new_text.contains("std::sync::Arc"));
    }

    #[test]
    fn fix_missing_field() {
        let err = make_error(
            "src/config.rs",
            10,
            "E0063",
            "missing field `name` in initializer",
        );
        let source = vec![""; 20];
        let fixes = suggest_fix(&err, &source);
        assert_eq!(fixes.len(), 1);
        assert_eq!(fixes[0].action, "insert_line");
        assert!(fixes[0].new_text.contains("name"));
        assert!(fixes[0].new_text.contains("Default::default()"));
    }

    #[test]
    fn fix_string_str_mismatch_need_string() {
        let err = make_error(
            "src/lib.rs",
            3,
            "E0308",
            "mismatched types: expected `String`, found `&str`",
        );
        let source = vec![
            "fn f() {",
            "    let s: &str = \"hi\";",
            "    takes_string(s);",
            "}",
        ];
        let fixes = suggest_fix(&err, &source);
        assert_eq!(fixes.len(), 1);
        assert!(fixes[0].new_text.contains(".to_string()"));
    }

    #[test]
    fn fix_string_str_mismatch_need_ref() {
        let err = make_error(
            "src/lib.rs",
            3,
            "E0308",
            "mismatched types: expected `&str`, found struct `String`",
        );
        let source = vec![
            "fn f() {",
            "    let s = String::new();",
            "    takes_ref(s);",
            "}",
        ];
        let fixes = suggest_fix(&err, &source);
        assert_eq!(fixes.len(), 1);
        assert!(fixes[0].new_text.contains(".as_str()"));
    }

    #[test]
    fn fix_missing_trait_method() {
        let err = make_error(
            "src/impl.rs",
            5,
            "E0046",
            "not all trait items implemented, missing: `process`",
        );
        let source = vec!["impl Handler for MyType {", "}"];
        let fixes = suggest_fix(&err, &source);
        assert_eq!(fixes.len(), 1);
        assert!(fixes[0].new_text.contains("fn process"));
        assert!(fixes[0].new_text.contains("todo!()"));
    }

    #[test]
    fn fix_ts_missing_name() {
        let err = make_error("src/app.ts", 3, "TS2304", "Cannot find name 'Router'");
        let source = vec!["const app = new Router();"];
        let fixes = suggest_fix(&err, &source);
        assert_eq!(fixes.len(), 1);
        assert!(fixes[0].new_text.contains("import { Router }"));
    }

    #[test]
    fn fix_unknown_error_no_suggestion() {
        let err = make_error("src/main.rs", 1, "E9999", "something weird happened");
        let source = vec!["fn main() {}"];
        let fixes = suggest_fix(&err, &source);
        assert!(fixes.is_empty());
    }

    #[test]
    fn fix_sorted_by_confidence() {
        // The unused variable fix should have high confidence
        let err = make_error("src/main.rs", 1, "", "unused variable: `x`");
        let source = vec!["let x = 1;"];
        let fixes = suggest_fix(&err, &source);
        if fixes.len() > 1 {
            for w in fixes.windows(2) {
                assert!(w[0].confidence >= w[1].confidence);
            }
        }
    }

    #[test]
    fn fix_suggest_rust_import_coverage() {
        // Check several common types have import suggestions
        for name in &[
            "HashMap", "HashSet", "Arc", "Mutex", "PathBuf", "File", "Cow", "Rc",
        ] {
            assert!(
                suggest_rust_import(name).is_some(),
                "Expected import suggestion for {}",
                name
            );
        }
        // Unknown type returns None
        assert!(suggest_rust_import("MyCustomType").is_none());
    }

    // ── Auto-Fix Application Tests ────────────────────────────────────

    #[test]
    fn apply_fix_delete_line() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("test.rs");
        std::fs::write(&file, "use std::io;\nuse std::fs;\nfn main() {}\n").unwrap();

        let fix = FixSuggestion::new("test.rs", "delete_line", 1, "", "Remove unused import", 0.9);
        let result = apply_fix(&fix, dir.path());
        assert!(result.is_ok());
        let content = std::fs::read_to_string(&file).unwrap();
        assert!(!content.contains("use std::io;"));
        assert!(content.contains("use std::fs;"));
        assert!(content.contains("fn main()"));
    }

    #[test]
    fn apply_fix_replace_line() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("test.rs");
        std::fs::write(
            &file,
            "fn main() {\n    let x = 42;\n    println!(\"{}\", x);\n}\n",
        )
        .unwrap();

        let fix = FixSuggestion::new(
            "test.rs",
            "replace",
            2,
            "    let _x = 42;",
            "Prefix unused var",
            0.9,
        );
        let result = apply_fix(&fix, dir.path());
        assert!(result.is_ok());
        let content = std::fs::read_to_string(&file).unwrap();
        assert!(content.contains("let _x = 42;"));
        assert!(!content.contains("let x = 42;"));
    }

    #[test]
    fn apply_fix_insert_line() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("test.rs");
        std::fs::write(&file, "fn main() {\n    let x = HashMap::new();\n}\n").unwrap();

        let fix = FixSuggestion::new(
            "test.rs",
            "insert_line",
            1,
            "use std::collections::HashMap;",
            "Add missing import",
            0.8,
        );
        let result = apply_fix(&fix, dir.path());
        assert!(result.is_ok());
        let content = std::fs::read_to_string(&file).unwrap();
        assert!(content.starts_with("use std::collections::HashMap;\nfn main()"));
    }

    #[test]
    fn apply_fix_add_import_after_existing() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("test.rs");
        std::fs::write(
            &file,
            "use std::io;\n\nfn main() {\n    let x = HashMap::new();\n}\n",
        )
        .unwrap();

        let fix = FixSuggestion::new(
            "test.rs",
            "add_import",
            0,
            "use std::collections::HashMap;",
            "Add HashMap import",
            0.8,
        );
        let result = apply_fix(&fix, dir.path());
        assert!(result.is_ok());
        let content = std::fs::read_to_string(&file).unwrap();
        // New import should be after existing import
        let io_pos = content.find("use std::io;").unwrap();
        let hm_pos = content.find("use std::collections::HashMap;").unwrap();
        assert!(
            hm_pos > io_pos,
            "New import should be after existing imports"
        );
    }

    #[test]
    fn apply_fix_line_out_of_range() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("test.rs");
        std::fs::write(&file, "line1\nline2\n").unwrap();

        let fix = FixSuggestion::new("test.rs", "delete_line", 99, "", "Bad line", 0.9);
        let result = apply_fix(&fix, dir.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("out of range"));
    }

    #[test]
    fn apply_fix_unknown_action() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("test.rs");
        std::fs::write(&file, "line1\n").unwrap();

        let fix = FixSuggestion::new("test.rs", "magic", 1, "x", "Bad action", 0.9);
        let result = apply_fix(&fix, dir.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Unknown fix action"));
    }

    #[test]
    fn apply_fix_missing_file() {
        let dir = tempdir().unwrap();
        let fix = FixSuggestion::new("nonexistent.rs", "delete_line", 1, "", "Bad file", 0.9);
        let result = apply_fix(&fix, dir.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Failed to read"));
    }

    #[test]
    fn apply_auto_fixes_filters_low_confidence() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("test.rs");
        std::fs::write(&file, "use std::io;\nuse std::fs;\nfn main() {}\n").unwrap();

        let fixes = vec![
            FixSuggestion::new("test.rs", "delete_line", 1, "", "High confidence", 0.9),
            FixSuggestion::new("test.rs", "delete_line", 2, "", "Low confidence", 0.5),
        ];

        let (applied, errors) = apply_auto_fixes(&fixes, dir.path());
        assert_eq!(
            applied.len(),
            1,
            "Only high-confidence fix should be applied"
        );
        assert!(errors.is_empty());
        let content = std::fs::read_to_string(&file).unwrap();
        assert!(
            !content.contains("use std::io;"),
            "High-confidence fix should delete line 1"
        );
        assert!(
            content.contains("use std::fs;"),
            "Low-confidence line 2 should remain"
        );
    }

    #[test]
    fn apply_auto_fixes_reverse_line_order() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("test.rs");
        std::fs::write(
            &file,
            "use std::io;\nuse std::fs;\nuse std::net;\nfn main() {}\n",
        )
        .unwrap();

        let fixes = vec![
            FixSuggestion::new("test.rs", "delete_line", 1, "", "Delete line 1", 0.9),
            FixSuggestion::new("test.rs", "delete_line", 3, "", "Delete line 3", 0.9),
        ];

        let (applied, errors) = apply_auto_fixes(&fixes, dir.path());
        assert_eq!(applied.len(), 2);
        assert!(errors.is_empty());
        let content = std::fs::read_to_string(&file).unwrap();
        assert!(!content.contains("use std::io;"));
        assert!(content.contains("use std::fs;"));
        assert!(!content.contains("use std::net;"));
    }

    #[test]
    fn apply_auto_fixes_empty_when_all_low_confidence() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("test.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();

        let fixes = vec![FixSuggestion::new(
            "test.rs",
            "replace",
            1,
            "fn main() { todo!() }",
            "Low",
            0.3,
        )];

        let (applied, _) = apply_auto_fixes(&fixes, dir.path());
        assert!(applied.is_empty());
        // File unchanged
        let content = std::fs::read_to_string(&file).unwrap();
        assert!(content.contains("fn main() {}"));
    }

    #[test]
    fn format_auto_fix_report_applied() {
        let applied = vec![AppliedFix {
            file: "src/main.rs".to_string(),
            action: "delete_line".to_string(),
            line: 3,
            explanation: "Remove unused import".to_string(),
        }];
        let report = format_auto_fix_report(&applied, &[], 1);
        assert!(report.contains("Auto-Fix Iteration 1"));
        assert!(report.contains("✓ src/main.rs L3"));
        assert!(report.contains("Remove unused import"));
    }

    #[test]
    fn format_auto_fix_report_no_fixes() {
        let report = format_auto_fix_report(&[], &[], 1);
        assert!(report.contains("No fixes applied"));
    }

    #[test]
    fn format_auto_fix_report_with_errors() {
        let errors = vec!["Failed to read foo.rs: not found".to_string()];
        let report = format_auto_fix_report(&[], &errors, 2);
        assert!(report.contains("Auto-Fix Iteration 2"));
        assert!(report.contains("✗ Failed to read foo.rs"));
    }

    #[test]
    fn find_import_insertion_point_after_use() {
        let lines = vec!["use std::io;", "use std::fs;", "", "fn main() {}"];
        assert_eq!(find_import_insertion_point(&lines), 2);
    }

    #[test]
    fn find_import_insertion_point_no_imports() {
        let lines = vec!["fn main() {}", "  println!(\"hi\");", "}"];
        assert_eq!(find_import_insertion_point(&lines), 0);
    }

    #[test]
    fn find_import_insertion_point_python() {
        let lines = vec!["import os", "from pathlib import Path", "", "def main():"];
        assert_eq!(find_import_insertion_point(&lines), 2);
    }

    #[test]
    fn auto_fix_constants_are_sane() {
        let threshold = std::hint::black_box(AUTO_FIX_CONFIDENCE_THRESHOLD);
        let max_iterations = std::hint::black_box(AUTO_FIX_MAX_ITERATIONS);
        assert!(threshold >= 0.7);
        assert!(threshold <= 1.0);
        assert!(max_iterations >= 1);
        assert!(max_iterations <= 5);
    }

    #[test]
    fn apply_fix_preserves_trailing_newline() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("test.rs");
        std::fs::write(&file, "use std::io;\nfn main() {}\n").unwrap();

        let fix = FixSuggestion::new("test.rs", "delete_line", 1, "", "Remove import", 0.9);
        apply_fix(&fix, dir.path()).unwrap();
        let content = std::fs::read_to_string(&file).unwrap();
        assert!(content.ends_with('\n'), "Should preserve trailing newline");
    }

    // ── C2 Enhancement Tests: Build/Test Loop ────────────────────────────

    #[test]
    fn delta_summary_shows_error_count_transition() {
        let delta = BuildTestDelta {
            iteration: 2,
            new_errors: vec!["new1".into()],
            fixed_errors: vec!["fix1".into(), "fix2".into()],
            persistent_errors: vec!["persist1".into()],
            regressed: false,
            progress_pct: 40.0,
            command: "cargo test".into(),
        };
        let summary = delta.to_summary();
        // Should show prev→cur error count: prev = fixed + persistent = 3, cur = new + persistent = 2
        assert!(summary.contains("3 → 2 errors"), "got: {summary}");
        assert!(summary.contains("✅ Fixed 2"));
        assert!(summary.contains("🆕 1 new"));
        assert!(summary.contains("⏳ 1 still present"));
        assert!(summary.contains("40%"));
    }

    #[test]
    fn delta_summary_regression_directive() {
        let delta = BuildTestDelta {
            iteration: 1,
            new_errors: vec!["a".into(), "b".into(), "c".into()],
            fixed_errors: vec![],
            persistent_errors: vec!["p1".into()],
            regressed: true,
            progress_pct: 0.0,
            command: "cargo test".into(),
        };
        let summary = delta.to_summary();
        assert!(summary.contains("REGRESSION"), "got: {summary}");
        assert!(
            summary.contains("Revert"),
            "Should tell LLM to revert: {summary}"
        );
    }

    #[test]
    fn cascade_output_includes_root_cause_directive() {
        // Build a result with import-cascade pattern
        let result = BuildTestResult {
            passed: false,
            exit_code: Some(1),
            framework: "cargo".into(),
            error_count: 4,
            error_messages: vec![],
            error_locations: vec![
                ErrorLocation::new(
                    "src/main.rs".into(),
                    5,
                    0,
                    "E0425".into(),
                    "cannot find value `Foo`".into(),
                    "error".into(),
                ),
                ErrorLocation::new(
                    "src/main.rs".into(),
                    10,
                    0,
                    "E0425".into(),
                    "cannot find value `Foo`".into(),
                    "error".into(),
                ),
                ErrorLocation::new(
                    "src/main.rs".into(),
                    15,
                    0,
                    "E0425".into(),
                    "cannot find value `Foo`".into(),
                    "error".into(),
                ),
                ErrorLocation::new(
                    "src/main.rs".into(),
                    20,
                    0,
                    "E0308".into(),
                    "type mismatch".into(),
                    "error".into(),
                ),
            ],
            tests_passed: 0,
            tests_failed: 0,
            tests_skipped: 0,
            summary: "4 errors".into(),
            truncated: false,
        };
        let output = result.to_enhanced_output("");
        assert!(
            output.contains("fix root cause FIRST"),
            "Should include root-cause directive: {output}"
        );
        assert!(
            output.contains("downstream errors"),
            "Should mention downstream: {output}"
        );
    }

    #[test]
    fn tracker_records_error_count_in_delta() {
        let mut tracker = BuildTestTracker::new();

        // First run: 3 errors
        let r1 = BuildTestResult {
            error_count: 3,
            error_locations: vec![
                ErrorLocation::new(
                    "a.rs".into(),
                    1,
                    0,
                    "E0425".into(),
                    "err1".into(),
                    "error".into(),
                ),
                ErrorLocation::new(
                    "a.rs".into(),
                    2,
                    0,
                    "E0308".into(),
                    "err2".into(),
                    "error".into(),
                ),
                ErrorLocation::new(
                    "b.rs".into(),
                    1,
                    0,
                    "E0599".into(),
                    "err3".into(),
                    "error".into(),
                ),
            ],
            ..Default::default()
        };
        let d1 = tracker.record(&r1, "cargo test");
        assert_eq!(d1.iteration, 0); // baseline

        // Second run: 2 errors (one fixed, one new)
        let r2 = BuildTestResult {
            error_count: 2,
            error_locations: vec![
                ErrorLocation::new(
                    "a.rs".into(),
                    1,
                    0,
                    "E0425".into(),
                    "err1".into(),
                    "error".into(),
                ),
                ErrorLocation::new(
                    "c.rs".into(),
                    5,
                    0,
                    "E0277".into(),
                    "err4".into(),
                    "error".into(),
                ),
            ],
            ..Default::default()
        };
        let d2 = tracker.record(&r2, "cargo test");
        assert_eq!(d2.iteration, 1);
        assert_eq!(d2.fixed_errors.len(), 2); // err2 and err3 gone
        assert_eq!(d2.new_errors.len(), 1); // err4 is new
        assert!(!d2.regressed); // 2 < 3

        // Third run: regression (4 errors)
        let r3 = BuildTestResult {
            error_count: 4,
            error_locations: vec![
                ErrorLocation::new(
                    "a.rs".into(),
                    1,
                    0,
                    "E0425".into(),
                    "err1".into(),
                    "error".into(),
                ),
                ErrorLocation::new(
                    "c.rs".into(),
                    5,
                    0,
                    "E0277".into(),
                    "err4".into(),
                    "error".into(),
                ),
                ErrorLocation::new(
                    "d.rs".into(),
                    1,
                    0,
                    "E0412".into(),
                    "err5".into(),
                    "error".into(),
                ),
                ErrorLocation::new(
                    "d.rs".into(),
                    2,
                    0,
                    "E0433".into(),
                    "err6".into(),
                    "error".into(),
                ),
            ],
            ..Default::default()
        };
        let d3 = tracker.record(&r3, "cargo test");
        assert!(d3.regressed, "Should detect regression");
        assert_eq!(d3.new_errors.len(), 2);
    }

    #[test]
    fn fix_order_root_cause_first() {
        let result = BuildTestResult {
            passed: false,
            error_count: 5,
            error_locations: vec![
                // Complex error
                ErrorLocation::new(
                    "a.rs".into(),
                    100,
                    0,
                    "E0277".into(),
                    "trait not satisfied".into(),
                    "error".into(),
                ),
                // Trivial import errors (cascade root at index 1)
                ErrorLocation::new(
                    "b.rs".into(),
                    1,
                    0,
                    "E0425".into(),
                    "cannot find `Vec`".into(),
                    "error".into(),
                ),
                ErrorLocation::new(
                    "b.rs".into(),
                    5,
                    0,
                    "E0425".into(),
                    "cannot find `Vec`".into(),
                    "error".into(),
                ),
                ErrorLocation::new(
                    "b.rs".into(),
                    10,
                    0,
                    "E0425".into(),
                    "cannot find `Vec`".into(),
                    "error".into(),
                ),
                // Fixable error
                ErrorLocation::new(
                    "c.rs".into(),
                    20,
                    0,
                    "E0308".into(),
                    "type mismatch".into(),
                    "error".into(),
                ),
            ],
            ..Default::default()
        };
        let order = result.fix_order();
        // Root cause (cascade root in b.rs) should come before complex error
        assert_eq!(order[0], 1, "Cascade root (b.rs:1) should be first");
        // Complex error should be last
        let complex_pos = order.iter().position(|&i| i == 0).unwrap();
        assert!(
            complex_pos > 2,
            "Complex error should come after trivial/fixable"
        );
    }
}

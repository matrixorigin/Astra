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
    fn new(file: String, line: usize, col: usize, error_code: String, message: String, severity: String) -> Self {
        let (class, hint) = classify_and_hint(&error_code, &message);
        Self { file, line, col, error_code, message, severity, class, hint, scope: String::new() }
    }
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
            let trivial_count = self.error_locations.iter().filter(|l| l.class == ErrorClass::Trivial).count();
            let fixable_count = self.error_locations.iter().filter(|l| l.class == ErrorClass::Fixable).count();
            let complex_count = self.error_locations.iter().filter(|l| l.class == ErrorClass::Complex).count();

            if trivial_count + fixable_count > 0 {
                parts.push(format!(
                    "Fix priority: {} trivial, {} fixable, {} complex",
                    trivial_count, fixable_count, complex_count
                ));
            }

            // Detect cascading: multiple errors in same file likely cascade from first
            let first_file = &self.error_locations[0].file;
            let same_file_count = self.error_locations.iter().filter(|l| l.file == *first_file).count();
            if same_file_count > 2 {
                parts.push(format!(
                    "⚡ {} errors in {} — fix first error, others may resolve",
                    same_file_count, first_file
                ));
            }

            parts.push("Locations:".to_string());
            for loc in self.error_locations.iter().take(10) {
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
                parts.push(format!(
                    "  → {}:{}{}{} {}{}{}",
                    loc.file, loc.line, col_part, code_part, loc.message, class_tag, scope_part
                ));
                if !loc.hint.is_empty() {
                    parts.push(format!("    💡 {}", loc.hint));
                }
            }
            if self.error_locations.len() > 10 {
                parts.push(format!("  ... and {} more locations", self.error_locations.len() - 10));
            }
        }

        // For failed builds, include relevant raw output (truncated)
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

/// Classify an error and generate a quick-fix hint for the LLM.
fn classify_and_hint(error_code: &str, message: &str) -> (ErrorClass, String) {
    // Rust error codes
    match error_code {
        // Trivial: mechanical fixes
        "E0425" => {
            // cannot find value — usually missing import
            if let Some(name) = extract_identifier(message) {
                return (ErrorClass::Trivial, format!("Add `use` import for `{name}`, or check spelling"));
            }
            (ErrorClass::Trivial, "Add missing import or check spelling".into())
        }
        "E0433" => (ErrorClass::Trivial, "Add missing `use` or crate dependency in Cargo.toml".into()),
        "E0432" => (ErrorClass::Trivial, "Fix import path — module or item doesn't exist at that path".into()),
        "E0412" => (ErrorClass::Trivial, "Type not found — add missing `use` import".into()),
        "E0603" => (ErrorClass::Trivial, "Item is private — add `pub` to definition or use a public re-export".into()),

        // Fixable: LLM can reason about these
        "E0308" => {
            // Mismatched types
            if message.contains("&str") && message.contains("String") {
                (ErrorClass::Fixable, "String/&str mismatch — use `.to_string()` or `&*s` / `.as_str()`".into())
            } else if message.contains("Option") {
                (ErrorClass::Fixable, "Wrap in Some() or unwrap with .unwrap_or()".into())
            } else {
                (ErrorClass::Fixable, "Check expected vs actual type — may need conversion or different return".into())
            }
        }
        "E0277" => (ErrorClass::Fixable, "Trait not satisfied — implement the trait or add a bound".into()),
        "E0599" => (ErrorClass::Fixable, "Method not found — check spelling, or the type may need a different impl/import".into()),
        "E0061" => (ErrorClass::Fixable, "Wrong number of arguments — check function signature".into()),
        "E0063" => (ErrorClass::Fixable, "Missing struct fields — add the required fields".into()),
        "E0609" => (ErrorClass::Fixable, "No field on type — check struct definition for correct field name".into()),
        "E0107" => (ErrorClass::Fixable, "Wrong number of type arguments — check generic parameters".into()),
        "E0369" => (ErrorClass::Fixable, "Operator not implemented — derive trait or implement manually".into()),
        "E0046" => (ErrorClass::Fixable, "Missing trait method — implement the required method".into()),

        // Complex: requires deep understanding
        "E0382" | "E0505" | "E0502" => (ErrorClass::Complex, "Borrow/move error — restructure ownership or use .clone()".into()),
        "E0597" => (ErrorClass::Complex, "Value doesn't live long enough — restructure lifetimes".into()),
        "E0106" => (ErrorClass::Complex, "Missing lifetime — add explicit lifetime annotations".into()),
        "E0495" => (ErrorClass::Complex, "Conflicting lifetime requirements — simplify borrowing structure".into()),

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
    if lower.contains("import") && (lower.contains("not found") || lower.contains("cannot find") || lower.contains("no module")) {
        return (ErrorClass::Trivial, "Missing import — add the correct import statement".into());
    }
    if lower.contains("undefined") || lower.contains("is not defined") {
        return (ErrorClass::Fixable, "Undefined variable/function — check spelling or add import".into());
    }
    if lower.contains("type") && lower.contains("not assignable") {
        return (ErrorClass::Fixable, "Type mismatch — check expected vs actual type".into());
    }
    if lower.contains("unused") {
        return (ErrorClass::Trivial, "Unused variable — prefix with _ or remove".into());
    }
    if lower.contains("syntax error") || lower.contains("unexpected token") {
        return (ErrorClass::Fixable, "Syntax error — check for missing brackets, semicolons, or typos".into());
    }
    if lower.contains("assertion") || lower.contains("expected") && lower.contains("got") {
        return (ErrorClass::Complex, "Test assertion failure — check logic and expected values".into());
    }

    (ErrorClass::Fixable, String::new())
}

/// Extract an identifier name from an error message like "cannot find value `foo`".
fn extract_identifier(message: &str) -> Option<&str> {
    let start = message.find('`')? + 1;
    let end = message[start..].find('`')? + start;
    Some(&message[start..end])
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
    LazyLock::new(|| Regex::new(r"error\[(E\d+)\]: (.+)").unwrap());

static CARGO_LOCATION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*--> (.+):(\d+):(\d+)").unwrap());

static CARGO_TEST_RESULT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored").unwrap()
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
            for j in (i + 1)..lines.len().min(i + 4) {
                if let Some(loc_cap) = CARGO_LOCATION_RE.captures(lines[j]) {
                    let file = loc_cap.get(1).map(|m| m.as_str()).unwrap_or("");
                    let line_num: usize = loc_cap.get(2).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
                    let col: usize = loc_cap.get(3).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);

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
        } else if line.starts_with("error:") && !line.contains("could not compile") && !line.contains("aborting due to") {
            // Plain error: without code
            let msg = line.strip_prefix("error:").unwrap_or(line).trim();
            if !msg.is_empty() && result.error_messages.len() < 10 {
                result.error_messages.push(msg.to_string());
            }
            // Check for location
            for j in (i + 1)..lines.len().min(i + 4) {
                if let Some(loc_cap) = CARGO_LOCATION_RE.captures(lines[j]) {
                    let file = loc_cap.get(1).map(|m| m.as_str()).unwrap_or("");
                    let line_num: usize = loc_cap.get(2).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
                    let col: usize = loc_cap.get(3).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
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
    LazyLock::new(|| Regex::new(r#"panicked at .+,?\s+(.+):(\d+):(\d+)"#).unwrap());

fn extract_cargo_failed_tests(output: &str, locations: &mut Vec<ErrorLocation>) -> Vec<String> {
    let mut failed = Vec::new();
    let lines: Vec<&str> = output.lines().collect();

    for (i, line) in lines.iter().enumerate() {
        // Look for "---- test_name stdout ----" pattern
        if line.contains("---- ") && line.contains(" stdout ----") {
            if let Some(name) = line
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
                            let line_num: usize = pcap.get(2).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
                            let col: usize = pcap.get(3).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
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
    }

    failed
}

// ─── Pytest Parser ───────────────────────────────────────────────────────────

static PYTEST_SUMMARY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(\d+) passed.*?(\d+) failed|(\d+) passed").unwrap());

// Matches Python tracebacks: "  File "path.py", line 42, in func"
static PYTHON_TRACEBACK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"File "(.+?)", line (\d+)(?:, in (.+))?"#).unwrap());

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
            let line_num: usize = cap.get(2).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
            let func = cap.get(3).map(|m| m.as_str()).unwrap_or("");

            // Only include if the next few lines contain an assertion or error
            let is_relevant = lines.iter().skip(i + 1).take(3).any(|l| {
                l.contains("AssertionError") || l.contains("assert ") || l.contains("Error") || l.contains("raise ")
            });

            if is_relevant && !file.is_empty() && line_num > 0 && result.error_locations.len() < 20 {
                result.error_locations.push(ErrorLocation::new(
                    file.to_string(),
                    line_num,
                    0,
                    String::new(),
                    if func.is_empty() { String::new() } else { format!("in {func}") },
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
    Regex::new(r"Tests:\s+(\d+) failed.*?(\d+) passed|Tests:\s+(\d+) passed").unwrap()
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
        if line.trim().starts_with("✕") || line.trim().starts_with("×") {
            if result.error_messages.len() < 10 {
                result.error_messages.push(line.trim().to_string());
            }
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
    LazyLock::new(|| Regex::new(r"^\s+(\S+\.go):(\d+): (.+)").unwrap());

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
            current_test = line.trim_start_matches("--- FAIL: ").split(' ').next().unwrap_or("").to_string();
            if result.error_messages.len() < 10 {
                result.error_messages.push(line.trim().to_string());
            }
        } else if line.starts_with("--- SKIP:") {
            result.tests_skipped += 1;
        } else if let Some(cap) = GO_TEST_LOCATION_RE.captures(line) {
            let file = cap.get(1).map(|m| m.as_str()).unwrap_or("");
            let line_num: usize = cap.get(2).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
            let msg = cap.get(3).map(|m| m.as_str()).unwrap_or("");
            if !file.is_empty() && line_num > 0 && result.error_locations.len() < 20 {
                let full_msg = if current_test.is_empty() {
                    msg.to_string()
                } else {
                    format!("{}: {}", current_test, msg)
                };
                result.error_locations.push(ErrorLocation::new(file.to_string(), line_num, 0, String::new(), full_msg, "error".to_string()));
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
    LazyLock::new(|| Regex::new(r"(.+)\((\d+),(\d+)\): error (TS\d+): (.+)").unwrap());

// Matches generic file:line:col: error patterns
static GENERIC_LOCATION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.+?):(\d+):(\d+):\s*(error|warning):\s*(.+)").unwrap());

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
            let line_num: usize = cap.get(2).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
            let col: usize = cap.get(3).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
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
            let line_num: usize = cap.get(2).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
            let col: usize = cap.get(3).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
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
                    while !trimmed.is_char_boundary(end) && end > 0 { end -= 1; }
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
        assert!(result.error_messages.iter().any(|m| m.contains("cannot find value")));

        // Verify error locations extracted
        assert_eq!(result.error_locations.len(), 2, "should extract 2 error locations");
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
        assert!(output.contains("Locations:"), "should have Locations section: {output}");
        assert!(output.contains("→ src/main.rs:10:5"), "should show file:line:col: {output}");
        assert!(output.contains("[E0425]"), "should show error code: {output}");
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
        assert!(!result.error_locations.is_empty(), "should extract go test locations");
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
        assert!(!output.contains("Locations:"), "should not show Locations when empty");
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
        assert!(loc.message.contains("my_test"), "message should reference test name");
    }

    // ─── Error classification tests ────────────────────────────────────

    #[test]
    fn classify_rust_trivial_missing_import() {
        let (class, hint) = classify_and_hint("E0425", "cannot find value `HashMap` in this scope");
        assert_eq!(class, ErrorClass::Trivial);
        assert!(hint.contains("HashMap"), "hint should mention the identifier: {hint}");
        assert!(hint.contains("import"), "hint should suggest import: {hint}");
    }

    #[test]
    fn classify_rust_trivial_missing_crate() {
        let (class, hint) = classify_and_hint("E0433", "failed to resolve: use of undeclared crate or module `serde`");
        assert_eq!(class, ErrorClass::Trivial);
        assert!(hint.contains("Cargo.toml"), "hint should mention Cargo.toml: {hint}");
    }

    #[test]
    fn classify_rust_fixable_type_mismatch() {
        let (class, hint) = classify_and_hint("E0308", "mismatched types: expected &str, found String");
        assert_eq!(class, ErrorClass::Fixable);
        assert!(hint.contains("to_string") || hint.contains("as_str"), "hint should suggest conversion: {hint}");
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
        assert!(hint.contains("lifetime"), "hint should mention lifetime: {hint}");
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
        assert_eq!(extract_identifier("cannot find value `foo` in scope"), Some("foo"));
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
                ErrorLocation::new("src/a.rs".into(), 1, 0, "E0425".into(), "cannot find value `x`".into(), "error".into()),
                ErrorLocation::new("src/b.rs".into(), 2, 0, "E0308".into(), "mismatched types".into(), "error".into()),
                ErrorLocation::new("src/c.rs".into(), 3, 0, "E0382".into(), "value moved".into(), "error".into()),
            ],
            summary: "3 errors".into(),
            ..Default::default()
        };
        let output = result.to_enhanced_output("");
        assert!(output.contains("trivial"), "should show trivial count: {output}");
        assert!(output.contains("fixable"), "should show fixable count: {output}");
        assert!(output.contains("complex"), "should show complex count: {output}");
    }

    #[test]
    fn enhanced_output_shows_hints() {
        let result = BuildTestResult {
            passed: false,
            error_count: 1,
            error_messages: vec!["cannot find value `foo`".into()],
            error_locations: vec![ErrorLocation::new(
                "src/main.rs".into(), 10, 5, "E0425".into(),
                "cannot find value `foo`".into(), "error".into(),
            )],
            summary: "1 error".into(),
            ..Default::default()
        };
        let output = result.to_enhanced_output("");
        assert!(output.contains("💡"), "should show hint icon: {output}");
        assert!(output.contains("import"), "hint should mention import: {output}");
        assert!(output.contains("🔧"), "trivial should get wrench icon: {output}");
    }

    #[test]
    fn enhanced_output_cascading_detection() {
        let result = BuildTestResult {
            passed: false,
            error_count: 4,
            error_messages: vec!["err1".into(), "err2".into(), "err3".into(), "err4".into()],
            error_locations: vec![
                ErrorLocation::new("src/same.rs".into(), 10, 0, "E0425".into(), "err1".into(), "error".into()),
                ErrorLocation::new("src/same.rs".into(), 20, 0, "E0308".into(), "err2".into(), "error".into()),
                ErrorLocation::new("src/same.rs".into(), 30, 0, "E0599".into(), "err3".into(), "error".into()),
            ],
            summary: "3 errors in same file".into(),
            ..Default::default()
        };
        let output = result.to_enhanced_output("");
        assert!(output.contains("cascading") || output.contains("first error"),
            "should detect cascading errors in same file: {output}");
    }

    #[test]
    fn errorlocation_new_auto_classifies() {
        let loc = ErrorLocation::new("f.rs".into(), 1, 0, "E0425".into(), "cannot find value `x`".into(), "error".into());
        assert_eq!(loc.class, ErrorClass::Trivial);
        assert!(!loc.hint.is_empty());

        let loc2 = ErrorLocation::new("f.rs".into(), 1, 0, "E0382".into(), "value moved".into(), "error".into());
        assert_eq!(loc2.class, ErrorClass::Complex);

        let loc3 = ErrorLocation::new("f.rs".into(), 1, 0, "".into(), "random error".into(), "error".into());
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
                100,
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
}

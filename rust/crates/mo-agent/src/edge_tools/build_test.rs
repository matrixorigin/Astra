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
                parts.push(format!(
                    "  → {}:{}{}{} {}",
                    loc.file, loc.line, col_part, code_part, loc.message
                ));
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
                        result.error_locations.push(ErrorLocation {
                            file: file.to_string(),
                            line: line_num,
                            col,
                            error_code: code.to_string(),
                            message: msg.to_string(),
                            severity: "error".to_string(),
                        });
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
                        result.error_locations.push(ErrorLocation {
                            file: file.to_string(),
                            line: line_num,
                            col,
                            error_code: String::new(),
                            message: msg.to_string(),
                            severity: "error".to_string(),
                        });
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
                            let panic_msg = pcap.get(1).map(|m| m.as_str()).unwrap_or("");
                            let file = pcap.get(2).map(|m| m.as_str()).unwrap_or("");
                            let line_num: usize = pcap.get(3).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
                            let col: usize = pcap.get(4).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
                            if !file.is_empty() && line_num > 0 && locations.len() < 20 {
                                locations.push(ErrorLocation {
                                    file: file.to_string(),
                                    line: line_num,
                                    col,
                                    error_code: String::new(),
                                    message: format!("test {name}: {panic_msg}"),
                                    severity: "error".to_string(),
                                });
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
                result.error_locations.push(ErrorLocation {
                    file: file.to_string(),
                    line: line_num,
                    col: 0,
                    error_code: String::new(),
                    message: if func.is_empty() { String::new() } else { format!("in {func}") },
                    severity: "error".to_string(),
                });
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
                result.error_locations.push(ErrorLocation {
                    file: file.to_string(),
                    line: line_num,
                    col: 0,
                    error_code: String::new(),
                    message: full_msg,
                    severity: "error".to_string(),
                });
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
                locations.push(ErrorLocation {
                    file: file.to_string(),
                    line: line_num,
                    col,
                    error_code: code.to_string(),
                    message: msg.to_string(),
                    severity: "error".to_string(),
                });
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
                locations.push(ErrorLocation {
                    file: file.to_string(),
                    line: line_num,
                    col,
                    error_code: String::new(),
                    message: msg.to_string(),
                    severity: severity.to_string(),
                });
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
            error_locations: vec![ErrorLocation {
                file: "src/main.rs".to_string(),
                line: 10,
                col: 5,
                error_code: "E0425".to_string(),
                message: "cannot find value `foo`".to_string(),
                severity: "error".to_string(),
            }],
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
}

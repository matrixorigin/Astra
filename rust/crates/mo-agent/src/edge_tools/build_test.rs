//! Structured parsing of build and test output.
//!
//! Detects framework (cargo, pytest, jest, go test, etc.) and extracts:
//! - Pass/fail status
//! - Error count and messages
//! - Test summary (passed, failed, skipped counts)
//!
//! Enables automated change→test→fix cycles by providing structured feedback.

use regex::Regex;
use std::sync::LazyLock;

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
            let passed = passed_from_exit && error_count == 0;
            BuildTestResult {
                passed,
                exit_code,
                framework,
                error_count,
                error_messages: extract_generic_errors(output),
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

static CARGO_ERROR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"error\[E\d+\]: (.+)").unwrap());

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

    // Extract compilation errors
    for cap in CARGO_ERROR_RE.captures_iter(output) {
        if let Some(msg) = cap.get(1) {
            let err_msg = msg.as_str().trim();
            if !err_msg.is_empty() && result.error_messages.len() < 10 {
                result.error_messages.push(err_msg.to_string());
            }
        }
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

        // Extract failed test names
        if result.tests_failed > 0 {
            let mut failed_tests = extract_cargo_failed_tests(output);
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

fn extract_cargo_failed_tests(output: &str) -> Vec<String> {
    let mut failed = Vec::new();
    let lines: Vec<&str> = output.lines().collect();

    for (i, line) in lines.iter().enumerate() {
        // Look for "---- test_name stdout ----" pattern
        if line.contains("---- ") && line.contains(" stdout ----") {
            if let Some(name) = line
                .strip_prefix("---- ")
                .and_then(|s| s.strip_suffix(" stdout ----"))
            {
                // Get the assertion/panic message from following lines
                let mut msg = name.to_string();
                for next_line in lines.iter().skip(i + 1).take(5) {
                    if next_line.contains("assertion") || next_line.contains("panicked at") {
                        msg = format!("{}: {}", name, next_line.trim());
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

    // Extract failed test names
    for line in output.lines() {
        if line.starts_with("FAILED ") {
            let test_name = line.trim_start_matches("FAILED ").trim();
            if result.error_messages.len() < 10 {
                result.error_messages.push(test_name.to_string());
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

fn parse_go_output(output: &str, exit_code: Option<i32>, truncated: bool) -> BuildTestResult {
    let mut result = BuildTestResult {
        framework: "go".to_string(),
        exit_code,
        truncated,
        ..Default::default()
    };

    for line in output.lines() {
        if line.starts_with("--- PASS:") {
            result.tests_passed += 1;
        } else if line.starts_with("--- FAIL:") {
            result.tests_failed += 1;
            if result.error_messages.len() < 10 {
                result.error_messages.push(line.trim().to_string());
            }
        } else if line.starts_with("--- SKIP:") {
            result.tests_skipped += 1;
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

fn count_generic_errors(lower: &str) -> usize {
    let mut count = 0;

    // Count error-like patterns
    count += lower.matches("error:").count();
    count += lower.matches("error[").count();
    count += lower.matches("failed").count().min(5); // Cap to avoid overcounting
    count += lower.matches("exception").count();

    count
}

fn extract_generic_errors(output: &str) -> Vec<String> {
    let mut errors = Vec::new();

    for line in output.lines() {
        let lower = line.to_lowercase();
        if lower.contains("error:") || lower.contains("failed:") || lower.contains("exception:") {
            let trimmed = line.trim();
            if !trimmed.is_empty() && errors.len() < 10 {
                // Truncate long error lines
                if trimmed.len() > 200 {
                    errors.push(format!("{}...", &trimmed[..200]));
                } else {
                    errors.push(trimmed.to_string());
                }
            }
        }
    }

    errors
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
}

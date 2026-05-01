//! # astra-test-harness
//!
//! Declarative CLI testing framework for astra. Runs YAML cases
//! against one or more models, captures session state (journal +
//! stderr), evaluates success criteria (deterministic matchers +
//! optional LLM judger), and emits a scored report.
//!
//! ## Why a dedicated framework
//!
//! Unit and integration tests prove code correctness; this harness
//! proves *end-to-end behavior* — that a model, wired through the
//! astra CLI against a running server, produces the expected
//! tool-call sequence and session state. The existing runtime
//! tests exercise components in isolation; the harness exercises
//! the whole binary against real provider keys.
//!
//! ## Design principles
//!
//! 1. **Cases are data, not code** — YAML. New cases don't require
//!    recompiling.
//! 2. **Criteria stack**: cheap deterministic checks first
//!    (tool_called, stderr_contains, exit_code), expensive LLM
//!    judger last. Saves provider calls when a case obviously
//!    passed or failed.
//! 3. **Model matrix**: each case runs once per model in its
//!    `models:` list (or the CLI-provided fallback list). Output
//!    groups by case for readability.
//! 4. **Debug is opt-in per case**: `debug_log: true` captures
//!    stderr verbatim in the report; default compresses to pass/
//!    fail + counts to keep reports scannable.
//! 5. **Session state is a first-class artifact**: after each run
//!    the harness loads the session's local journal (via
//!    session_id from the JSON output) and makes it available to
//!    criteria evaluators. Supports reasoning like "verify the
//!    session's delegation tree has exactly 2 children".
//!
//! This module exposes the types needed to extend the harness
//! programmatically (custom criteria, custom judger backends)
//! without building from the `astra-test` binary.

pub mod case;
pub mod criteria;
pub mod digest;
pub mod exec;
pub mod judger;
pub mod report;
pub mod runner;
pub mod session_capture;
pub mod suite;

/// Well-known prefix for every stderr line this harness emits.
/// Grepping `stderr_matches { pattern: '^\[astra-test\]' }` finds
/// ALL harness self-log lines; anything else in stderr came from the
/// subprocess under test. A pin test in `lib.rs` asserts every
/// `eprintln!` in the crate starts with this exact prefix.
pub const HARNESS_STDERR_PREFIX: &str = "[astra-test]";

#[cfg(test)]
mod harness_stderr_prefix_pin {
    use super::HARNESS_STDERR_PREFIX;

    /// Source-level invariant: every `eprintln!` in the crate's src
    /// directory must begin with `HARNESS_STDERR_PREFIX`. This guards
    /// against future drift — if someone adds a warning with a
    /// different prefix, cases using `stderr_matches
    /// { pattern: '^\[astra-test\]' }` would miss it, and worse, a
    /// bare `[skip]` / `[warn]` line could be false-matched by a
    /// case looking for astra observability.
    ///
    /// The test globs `src/*.rs` at test time (not build time) so
    /// it catches additions without requiring a rebuild.
    #[test]
    fn every_eprintln_uses_harness_prefix() {
        // Where this test file lives → crate root is two levels up.
        let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut violators: Vec<String> = Vec::new();
        for entry in std::fs::read_dir(&src_dir).expect("read_dir src") {
            let entry = entry.expect("entry");
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let src = std::fs::read_to_string(&path).expect("read src");
            // Skip test modules — only production eprintlns matter for
            // the invariant. #[cfg(test)] blocks may print whatever.
            // We scan line-by-line and track a depth counter for
            // #[cfg(test)] mod { ... } — coarse but adequate.
            let mut depth: i32 = 0;
            let mut in_test_mod = false;
            for (lineno, line) in src.lines().enumerate() {
                if line.contains("#[cfg(test)]")
                    || line.contains("mod tests")
                    || line.contains("mod harness_stderr_prefix_pin")
                {
                    in_test_mod = true;
                }
                // Track brace depth so we can exit the test mod.
                depth += line.matches('{').count() as i32;
                depth -= line.matches('}').count() as i32;
                if in_test_mod && depth <= 0 {
                    in_test_mod = false;
                    depth = 0;
                }
                if !in_test_mod
                    && line.contains("eprintln!")
                {
                    // Grab the literal that follows `eprintln!(`.
                    // Tolerate multi-line strings: the next non-
                    // comment token should be a `"` opening quote;
                    // peek at the following literal content.
                    let snippet: String = src
                        .lines()
                        .skip(lineno)
                        .take(4)
                        .collect::<Vec<_>>()
                        .join(" ");
                    // Require the canonical prefix to appear within
                    // the first 80 chars of the call. Generous enough
                    // for formatting variations; tight enough to
                    // catch drift.
                    let window: String = snippet
                        .chars()
                        .skip_while(|&c| c != '"')
                        .take(80)
                        .collect();
                    if !window.contains(HARNESS_STDERR_PREFIX) {
                        violators.push(format!(
                            "{}:{}: eprintln without {HARNESS_STDERR_PREFIX}: {}",
                            path.file_name().unwrap().to_string_lossy(),
                            lineno + 1,
                            line.trim(),
                        ));
                    }
                }
            }
        }
        assert!(
            violators.is_empty(),
            "Every harness eprintln must be prefixed with {HARNESS_STDERR_PREFIX} \
             so a case's `stderr_matches` regex can distinguish harness logs \
             from subprocess observability. Violators:\n  {}",
            violators.join("\n  ")
        );
    }
}

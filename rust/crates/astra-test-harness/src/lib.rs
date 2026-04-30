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

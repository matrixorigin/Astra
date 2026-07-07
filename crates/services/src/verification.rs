//! Verification types shared across task orchestration and durable task execution.
//!
//! Extracted from `durable_task` to break the circular dependency where
//! `task_orchestrator::SubtaskPlan` needed `VerifierKind` from `durable_task`,
//! while `durable_task` needed `TaskPlan` from `task_orchestrator`.

use serde::{Deserialize, Serialize};

// ─── Verification Criterion ─────────────────────────────────────────────────

/// Machine-executable acceptance criterion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationCriterion {
    pub id: String,
    pub description: String,
    pub verifier: VerifierKind,
    /// Must pass for subtask to be considered verified
    #[serde(default = "default_true")]
    pub required: bool,
    /// Max seconds for this verification to run
    #[serde(default = "default_timeout")]
    pub timeout_sec: u32,
    /// If true, only runs during global verification (not per-subtask).
    /// Used for expensive checks like full build/test/lint.
    #[serde(default)]
    pub global_only: bool,
}

fn default_true() -> bool {
    true
}
fn default_timeout() -> u32 {
    120
}

// ─── Verifier Kind ──────────────────────────────────────────────────────────

/// The kind of verification to perform.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VerifierKind {
    /// Run a shell command, check exit code
    Command {
        cmd: String,
        #[serde(default)]
        expected_exit: i32,
    },
    /// Run a command, check stdout content
    CommandOutput {
        cmd: String,
        #[serde(default)]
        contains: Vec<String>,
        #[serde(default)]
        not_contains: Vec<String>,
    },
    /// Check that files exist
    FileExists { paths: Vec<String> },
    /// Grep a pattern in a file
    GrepCheck {
        file: String,
        pattern: String,
        #[serde(default = "default_true")]
        should_match: bool,
    },
    /// Build must pass (exit 0)
    BuildPass { cmd: String },
    /// Tests must pass with minimum pass rate
    TestPass {
        cmd: String,
        #[serde(default = "default_min_pass_rate")]
        min_pass_rate: f64,
    },
    /// Read a file and check its content for expected/forbidden strings.
    /// Safer than CommandOutput with `cat` — avoids shell execution of file paths.
    ReadFileContains {
        path: String,
        #[serde(default)]
        contains: Vec<String>,
        #[serde(default)]
        not_contains: Vec<String>,
    },
    /// LLM-based semantic judgment (can run on cloud, no local fs needed)
    LlmJudge {
        prompt: String,
        #[serde(default = "default_pass_threshold")]
        pass_threshold: f64,
    },
    /// Composite: AND/OR of sub-criteria
    Composite {
        criteria: Vec<VerificationCriterion>,
        #[serde(default = "default_true")]
        require_all: bool,
    },
}

fn default_min_pass_rate() -> f64 {
    1.0
}
fn default_pass_threshold() -> f64 {
    0.7
}

// ─── Verification Result ────────────────────────────────────────────────────

/// Result of running a single verification criterion.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VerificationResult {
    pub criterion_id: String,
    pub passed: bool,
    pub evidence: String,
    pub expected: String,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Report for all verifications on a subtask.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubtaskVerificationReport {
    pub subtask_id: String,
    pub all_required_passed: bool,
    pub results: Vec<VerificationResult>,
    pub timestamp: String,
}

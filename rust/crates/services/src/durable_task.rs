//! Durable long-term task system: contract-driven, verifiable, multi-session tasks.
//!
//! # Architecture
//!
//! ```text
//! TaskContract (goal + scope + verification criteria)
//!   └─ DurableSubtask[] (each with SubtaskStage state machine)
//!        ├─ VerificationCriterion[] (machine-executable acceptance checks)
//!        ├─ Git4Data isolation (per-task snapshot + per-agent branches)
//!        └─ VerificationResult[] (audit trail of pass/fail evidence)
//! ```
//!
//! Tasks survive session boundaries. Cloud (MatrixOne) is the source of truth.
//! Edge executes; cloud coordinates, verifies, and persists.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::task_orchestrator::{TaskCheckpoint, TaskPlan};

// ─── Contract ───────────────────────────────────────────────────────────────

/// A durable task contract with verifiable acceptance criteria.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskContract {
    pub contract_id: String,
    pub task_id: String,
    pub goal: String,
    pub scope: TaskScope,
    pub subtasks: Vec<DurableSubtask>,
    pub global_verification: Vec<VerificationCriterion>,
    pub version: u32,
    pub status: ContractStatus,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskScope {
    pub in_scope: Vec<String>,
    pub out_of_scope: Vec<String>,
    pub assumptions: Vec<String>,
}

impl Default for TaskScope {
    fn default() -> Self {
        Self {
            in_scope: Vec::new(),
            out_of_scope: Vec::new(),
            assumptions: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContractStatus {
    Draft,
    Active,
    Amended,
    Completed,
    Abandoned,
}

impl ContractStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Active => "active",
            Self::Amended => "amended",
            Self::Completed => "completed",
            Self::Abandoned => "abandoned",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "draft" => Self::Draft,
            "active" => Self::Active,
            "amended" => Self::Amended,
            "completed" => Self::Completed,
            "abandoned" => Self::Abandoned,
            _ => Self::Draft,
        }
    }
}

// ─── Durable Subtask ────────────────────────────────────────────────────────

/// A subtask with verification gate and git4data isolation support.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DurableSubtask {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,

    pub stage: SubtaskStage,
    #[serde(default)]
    pub criteria: Vec<VerificationCriterion>,
    #[serde(default)]
    pub max_retries: u32,
    #[serde(default)]
    pub retry_count: u32,

    /// MatrixOne snapshot taken before execution (for rollback)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_name: Option<String>,
    /// MatrixOne data branch for isolated work
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_branch: Option<String>,
    /// Diff summary captured after execution (vs pre-execution snapshot)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_summary: Option<DiffSummary>,
}

impl Default for DurableSubtask {
    fn default() -> Self {
        Self {
            id: String::new(),
            title: String::new(),
            description: None,
            depends_on: Vec::new(),
            effort: None,
            files: Vec::new(),
            stage: SubtaskStage::Pending,
            criteria: Vec::new(),
            max_retries: 2,
            retry_count: 0,
            snapshot_name: None,
            data_branch: None,
            diff_summary: None,
        }
    }
}

// ─── Subtask State Machine ──────────────────────────────────────────────────

/// Full lifecycle state for a durable subtask.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SubtaskStage {
    Pending,
    Blocked {
        reason: String,
    },
    Executing,
    ExecutionFailed {
        error: String,
    },
    AwaitingVerification,
    Verifying,
    VerificationFailed {
        results: Vec<VerificationResult>,
    },
    Verified,
    Completed,
    Skipped {
        reason: String,
    },
    Abandoned {
        reason: String,
    },
}

impl Default for SubtaskStage {
    fn default() -> Self {
        Self::Pending
    }
}

impl SubtaskStage {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Skipped { .. } | Self::Abandoned { .. }
        )
    }

    pub fn is_success(&self) -> bool {
        matches!(self, Self::Completed | Self::Verified)
    }

    /// Can transition to executing?
    pub fn can_start(&self) -> bool {
        matches!(self, Self::Pending | Self::VerificationFailed { .. })
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Blocked { .. } => "blocked",
            Self::Executing => "executing",
            Self::ExecutionFailed { .. } => "execution_failed",
            Self::AwaitingVerification => "awaiting_verification",
            Self::Verifying => "verifying",
            Self::VerificationFailed { .. } => "verification_failed",
            Self::Verified => "verified",
            Self::Completed => "completed",
            Self::Skipped { .. } => "skipped",
            Self::Abandoned { .. } => "abandoned",
        }
    }
}

// ─── Verification ───────────────────────────────────────────────────────────

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

// ─── Verification Runner ────────────────────────────────────────────────────

/// Executes verification criteria (edge-side: commands, files, grep, build/test).
pub struct VerificationRunner {
    pub work_dir: std::path::PathBuf,
}

impl VerificationRunner {
    pub fn new(work_dir: std::path::PathBuf) -> Self {
        Self { work_dir }
    }

    /// Verify all criteria for a subtask.
    pub async fn verify_subtask(&self, subtask: &DurableSubtask) -> SubtaskVerificationReport {
        self.verify_subtask_filtered(subtask, false).await
    }

    /// Verify only lightweight (non-global-only) criteria for a subtask.
    /// Skips `global_only` criteria and `LlmJudge` (not yet implemented).
    /// Used during per-subtask verification in the REPL loop for fast feedback.
    pub async fn verify_subtask_local(&self, subtask: &DurableSubtask) -> SubtaskVerificationReport {
        self.verify_subtask_filtered(subtask, true).await
    }

    async fn verify_subtask_filtered(
        &self,
        subtask: &DurableSubtask,
        skip_heavy: bool,
    ) -> SubtaskVerificationReport {
        let mut results = Vec::new();
        let criteria_to_run: Vec<_> = subtask
            .criteria
            .iter()
            .filter(|c| {
                if skip_heavy && c.global_only {
                    return false;
                }
                if skip_heavy && matches!(c.verifier, VerifierKind::LlmJudge { .. }) {
                    return false;
                }
                true
            })
            .collect();

        for criterion in &criteria_to_run {
            let result = self.run_criterion(criterion).await;
            results.push(result);
        }
        let all_required_passed = criteria_to_run
            .iter()
            .zip(results.iter())
            .all(|(c, r)| !c.required || r.passed);

        SubtaskVerificationReport {
            subtask_id: subtask.id.clone(),
            all_required_passed,
            results,
            timestamp: chrono::Utc::now().to_rfc3339(),
        }
    }

    /// Run a single verification criterion.
    pub async fn run_criterion(&self, criterion: &VerificationCriterion) -> VerificationResult {
        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(criterion.timeout_sec as u64);

        let result = tokio::time::timeout(timeout, self.execute_verifier(&criterion.verifier)).await;

        let duration_ms = start.elapsed().as_millis() as u64;

        match result {
            Ok(Ok((passed, evidence, expected))) => VerificationResult {
                criterion_id: criterion.id.clone(),
                passed,
                evidence,
                expected,
                duration_ms,
                error: None,
            },
            Ok(Err(e)) => VerificationResult {
                criterion_id: criterion.id.clone(),
                passed: false,
                evidence: String::new(),
                expected: String::new(),
                duration_ms,
                error: Some(e),
            },
            Err(_) => VerificationResult {
                criterion_id: criterion.id.clone(),
                passed: false,
                evidence: String::new(),
                expected: format!("completed within {}s", criterion.timeout_sec),
                duration_ms,
                error: Some("verification timed out".into()),
            },
        }
    }

    async fn execute_verifier(
        &self,
        verifier: &VerifierKind,
    ) -> Result<(bool, String, String), String> {
        match verifier {
            VerifierKind::Command { cmd, expected_exit } => {
                let cmd = cmd.clone();
                let expected = *expected_exit;
                let dir = self.work_dir.clone();
                let (code, stdout, stderr) = run_shell_cmd(&cmd, &dir).await?;
                let evidence = if stderr.is_empty() {
                    stdout
                } else {
                    format!("{stdout}\n--- stderr ---\n{stderr}")
                };
                Ok((
                    code == expected,
                    truncate(&evidence, 4096),
                    format!("exit code == {expected}"),
                ))
            }

            VerifierKind::CommandOutput {
                cmd,
                contains,
                not_contains,
            } => {
                let cmd = cmd.clone();
                let dir = self.work_dir.clone();
                let (_code, stdout, _stderr) = run_shell_cmd(&cmd, &dir).await?;
                let has_all = contains.iter().all(|s| stdout.contains(s));
                let has_none = not_contains.iter().all(|s| !stdout.contains(s));
                let passed = has_all && has_none;
                Ok((
                    passed,
                    truncate(&stdout, 4096),
                    format!("contains: {:?}, not_contains: {:?}", contains, not_contains),
                ))
            }

            VerifierKind::FileExists { paths } => {
                let mut missing = Vec::new();
                for p in paths {
                    let full = self.work_dir.join(p);
                    if !full.exists() {
                        missing.push(p.clone());
                    }
                }
                let passed = missing.is_empty();
                let evidence = if passed {
                    format!("all {} files exist", paths.len())
                } else {
                    format!("missing: {:?}", missing)
                };
                Ok((passed, evidence, format!("files exist: {:?}", paths)))
            }

            VerifierKind::GrepCheck {
                file,
                pattern,
                should_match,
            } => {
                let full = self.work_dir.join(file);
                let content = std::fs::read_to_string(&full)
                    .map_err(|e| format!("read {file}: {e}"))?;
                let found = content.contains(pattern);
                let passed = found == *should_match;
                let evidence = if found {
                    format!("pattern '{pattern}' found in {file}")
                } else {
                    format!("pattern '{pattern}' NOT found in {file}")
                };
                let expected = if *should_match {
                    format!("'{pattern}' should be in {file}")
                } else {
                    format!("'{pattern}' should NOT be in {file}")
                };
                Ok((passed, evidence, expected))
            }

            VerifierKind::BuildPass { cmd } => {
                let cmd = cmd.clone();
                let dir = self.work_dir.clone();
                let (code, _stdout, stderr) = run_shell_cmd(&cmd, &dir).await?;
                Ok((code == 0, truncate(&stderr, 4096), "exit code == 0".into()))
            }

            VerifierKind::TestPass { cmd, min_pass_rate } => {
                let cmd = cmd.clone();
                let dir = self.work_dir.clone();
                let (_code, stdout, stderr) = run_shell_cmd(&cmd, &dir).await?;
                let combined = format!("{stdout}\n{stderr}");
                let passed = _code == 0;
                Ok((
                    passed,
                    truncate(&combined, 4096),
                    format!("pass rate >= {:.0}%", min_pass_rate * 100.0),
                ))
            }

            VerifierKind::LlmJudge { prompt, .. } => {
                Ok((
                    false,
                    "LLM judge must run on cloud".into(),
                    format!("LLM evaluation: {}", truncate(prompt, 200)),
                ))
            }

            VerifierKind::Composite {
                criteria,
                require_all,
            } => {
                let mut results = Vec::new();
                for c in criteria {
                    // Box::pin to handle recursive async
                    let result = Box::pin(self.run_criterion(c)).await;
                    results.push(result);
                }
                let passed = if *require_all {
                    results.iter().all(|r| r.passed)
                } else {
                    results.iter().any(|r| r.passed)
                };
                let evidence = results
                    .iter()
                    .map(|r| format!("{}: {}", r.criterion_id, if r.passed { "✓" } else { "✗" }))
                    .collect::<Vec<_>>()
                    .join(", ");
                let logic = if *require_all { "ALL" } else { "ANY" };
                Ok((passed, evidence, format!("{logic} of {} criteria", criteria.len())))
            }
        }
    }
}

/// Run a shell command via spawn_blocking (no tokio::process feature needed).
async fn run_shell_cmd(
    cmd: &str,
    dir: &std::path::Path,
) -> Result<(i32, String, String), String> {
    let cmd = cmd.to_string();
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .current_dir(&dir)
            .output()
            .map_err(|e| format!("command failed: {e}"))?;
        let code = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        Ok((code, stdout, stderr))
    })
    .await
    .map_err(|e| format!("spawn_blocking: {e}"))?
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…[truncated]", &s[..max])
    }
}

// ─── Git4Data Task Branching ────────────────────────────────────────────────

/// Abstract interface for per-task data branching.
/// Enables testability without a real database connection.
#[async_trait]
pub trait TaskBranchOps: Send + Sync {
    /// Create a snapshot before subtask execution (for rollback).
    async fn create_snapshot(&self, task_id: &str, subtask_id: &str, version: u32)
        -> Result<String, String>;

    /// Diff agent's work against a pre-execution snapshot.
    async fn diff_since_snapshot(&self, snapshot: &str) -> Result<DiffSummary, String>;

    /// Rollback to a pre-execution snapshot.
    async fn rollback_to_snapshot(&self, snapshot: &str) -> Result<(), String>;

    /// Clean up a snapshot after successful verification.
    async fn cleanup_snapshot(&self, snapshot: &str) -> Result<(), String>;
}

/// Production implementation: MatrixOne git4data snapshots.
pub struct TaskBranchService {
    pool: sqlx::Pool<sqlx::MySql>,
}

impl TaskBranchService {
    pub fn new(pool: sqlx::Pool<sqlx::MySql>) -> Self {
        Self { pool }
    }
}

/// Validate a snapshot name is safe for SQL embedding (alphanumeric + underscore only).
fn validate_snapshot_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("empty snapshot name".into());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(format!(
            "invalid snapshot name '{}': only [a-zA-Z0-9_] allowed",
            name
        ));
    }
    Ok(())
}

#[async_trait]
impl TaskBranchOps for TaskBranchService {
    async fn create_snapshot(
        &self,
        task_id: &str,
        subtask_id: &str,
        version: u32,
    ) -> Result<String, String> {
        let name = format!("task_{task_id}_{subtask_id}_v{version}");
        validate_snapshot_name(&name)?;
        let sql = format!("CREATE SNAPSHOT {name} FOR ACCOUNT");
        sqlx::query(&sql)
            .execute(&self.pool)
            .await
            .map_err(|e| format!("create_snapshot: {e}"))?;
        Ok(name)
    }

    async fn diff_since_snapshot(&self, snapshot: &str) -> Result<DiffSummary, String> {
        validate_snapshot_name(snapshot)?;
        let sql = format!("SELECT COUNT(*) AS cnt FROM mo_diff('{snapshot}', 'CURRENT')");
        let row = sqlx::query(&sql)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| format!("diff: {e}"))?;
        let count: i64 = sqlx::Row::try_get(&row, "cnt").unwrap_or(0);
        Ok(DiffSummary {
            snapshot: snapshot.to_string(),
            changed_rows: count,
        })
    }

    async fn rollback_to_snapshot(&self, snapshot: &str) -> Result<(), String> {
        validate_snapshot_name(snapshot)?;
        let sql = format!("RESTORE ACCOUNT FROM SNAPSHOT {snapshot}");
        sqlx::query(&sql)
            .execute(&self.pool)
            .await
            .map_err(|e| format!("rollback: {e}"))?;
        Ok(())
    }

    async fn cleanup_snapshot(&self, snapshot: &str) -> Result<(), String> {
        validate_snapshot_name(snapshot)?;
        let sql = format!("DROP SNAPSHOT IF EXISTS {snapshot}");
        sqlx::query(&sql)
            .execute(&self.pool)
            .await
            .map_err(|e| format!("cleanup_snapshot: {e}"))?;
        Ok(())
    }
}

/// File-based branch ops for local/offline mode.
/// Creates directory snapshots by copying the work directory.
pub struct LocalFileBranchOps {
    snapshots_dir: std::path::PathBuf,
    work_dir: std::path::PathBuf,
}

impl LocalFileBranchOps {
    pub fn new(snapshots_dir: std::path::PathBuf, work_dir: std::path::PathBuf) -> Self {
        Self {
            snapshots_dir,
            work_dir,
        }
    }

    fn snapshot_path(&self, name: &str) -> std::path::PathBuf {
        self.snapshots_dir.join(name)
    }
}

#[async_trait]
impl TaskBranchOps for LocalFileBranchOps {
    async fn create_snapshot(
        &self,
        task_id: &str,
        subtask_id: &str,
        version: u32,
    ) -> Result<String, String> {
        let name = format!("task_{task_id}_{subtask_id}_v{version}");
        let snap_path = self.snapshot_path(&name);
        let work = self.work_dir.clone();
        let snap = snap_path.clone();
        tokio::task::spawn_blocking(move || {
            if snap.exists() {
                std::fs::remove_dir_all(&snap).ok();
            }
            copy_dir_recursive(&work, &snap)
        })
        .await
        .map_err(|e| format!("spawn: {e}"))?
        .map_err(|e| format!("snapshot copy: {e}"))?;
        Ok(name)
    }

    async fn diff_since_snapshot(&self, snapshot: &str) -> Result<DiffSummary, String> {
        let snap_path = self.snapshot_path(snapshot);
        let work = self.work_dir.clone();
        let changed = tokio::task::spawn_blocking(move || count_changed_files(&snap_path, &work))
            .await
            .map_err(|e| format!("spawn: {e}"))?
            .map_err(|e| format!("diff: {e}"))?;
        Ok(DiffSummary {
            snapshot: snapshot.to_string(),
            changed_rows: changed,
        })
    }

    async fn rollback_to_snapshot(&self, snapshot: &str) -> Result<(), String> {
        let snap_path = self.snapshot_path(snapshot);
        if !snap_path.exists() {
            return Err(format!("snapshot '{snapshot}' not found"));
        }
        let work = self.work_dir.clone();
        let snap = snap_path.clone();
        tokio::task::spawn_blocking(move || {
            // Clear work dir and copy snapshot back
            if work.exists() {
                std::fs::remove_dir_all(&work).map_err(|e| format!("clear work: {e}"))?;
            }
            copy_dir_recursive(&snap, &work)
        })
        .await
        .map_err(|e| format!("spawn: {e}"))?
        .map_err(|e| format!("rollback: {e}"))?;
        Ok(())
    }

    async fn cleanup_snapshot(&self, snapshot: &str) -> Result<(), String> {
        let snap_path = self.snapshot_path(snapshot);
        if snap_path.exists() {
            tokio::task::spawn_blocking(move || std::fs::remove_dir_all(&snap_path))
                .await
                .map_err(|e| format!("spawn: {e}"))?
                .map_err(|e| format!("cleanup: {e}"))?;
        }
        Ok(())
    }
}

/// No-op branch ops for when git4data is unavailable.
pub struct NoopBranchOps;

#[async_trait]
impl TaskBranchOps for NoopBranchOps {
    async fn create_snapshot(&self, _: &str, _: &str, _: u32) -> Result<String, String> {
        Ok(String::new()) // empty name signals "no snapshot"
    }
    async fn diff_since_snapshot(&self, snapshot: &str) -> Result<DiffSummary, String> {
        Ok(DiffSummary {
            snapshot: snapshot.to_string(),
            changed_rows: 0,
        })
    }
    async fn rollback_to_snapshot(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
    async fn cleanup_snapshot(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
}

// ── Filesystem helpers for LocalFileBranchOps ──

fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| format!("mkdir {}: {e}", dst.display()))?;
    if !src.exists() {
        return Ok(());
    }
    for entry in
        std::fs::read_dir(src).map_err(|e| format!("readdir {}: {e}", src.display()))?
    {
        let entry = entry.map_err(|e| format!("entry: {e}"))?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)
                .map_err(|e| format!("copy {}: {e}", src_path.display()))?;
        }
    }
    Ok(())
}

fn count_changed_files(
    snap: &std::path::Path,
    work: &std::path::Path,
) -> Result<i64, String> {
    if !snap.exists() || !work.exists() {
        return Ok(0);
    }
    let mut changed = 0i64;
    for entry in
        std::fs::read_dir(work).map_err(|e| format!("readdir {}: {e}", work.display()))?
    {
        let entry = entry.map_err(|e| format!("entry: {e}"))?;
        let work_path = entry.path();
        let snap_path = snap.join(entry.file_name());
        if work_path.is_dir() {
            changed += count_changed_files(&snap_path, &work_path)?;
        } else if !snap_path.exists() {
            changed += 1; // new file
        } else {
            let w = std::fs::read(&work_path).unwrap_or_default();
            let s = std::fs::read(&snap_path).unwrap_or_default();
            if w != s {
                changed += 1;
            }
        }
    }
    // Also count files deleted from snapshot
    if snap.exists() {
        for entry in
            std::fs::read_dir(snap).map_err(|e| format!("readdir {}: {e}", snap.display()))?
        {
            let entry = entry.map_err(|e| format!("entry: {e}"))?;
            let work_path = work.join(entry.file_name());
            if !work_path.exists() {
                changed += 1;
            }
        }
    }
    Ok(changed)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffSummary {
    pub snapshot: String,
    pub changed_rows: i64,
}

// ─── Delivery Report ────────────────────────────────────────────────────────

/// Final delivery report for a completed durable task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskDeliveryReport {
    pub task_id: String,
    pub contract_id: String,
    pub goal: String,
    pub subtask_summaries: Vec<SubtaskDeliverySummary>,
    pub global_verification: Vec<VerificationResult>,
    pub total_turns: u32,
    pub total_tokens: u64,
    pub total_verifications: u32,
    pub risks: Vec<String>,
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubtaskDeliverySummary {
    pub id: String,
    pub title: String,
    pub stage: String,
    pub criteria_passed: u32,
    pub criteria_total: u32,
    pub retry_count: u32,
}

// ─── Task Learning Bridge ────────────────────────────────────────────────────

/// Outcome of a completed durable task, used to feed learning signals.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskOutcomeSignal {
    pub task_id: String,
    pub contract_id: String,
    pub goal: String,
    /// Whether the task was successfully completed (all required criteria passed)
    pub success: bool,
    /// User-provided rating 0–100 (None if no feedback)
    pub user_rating: Option<u8>,
    /// Tools used across all subtasks
    pub tools_used: Vec<String>,
    /// Per-subtask outcome summaries
    pub subtask_outcomes: Vec<SubtaskOutcomeSignal>,
    /// Total verification attempts
    pub total_verification_attempts: u32,
    /// Total retries across subtasks
    pub total_retries: u32,
    /// Execution turns
    pub total_turns: u32,
    /// Free-form domain hint (e.g., "code", "database", "github")
    pub domain_hint: Option<String>,
    /// Task classification (e.g., "code", "fetch", "mutate")
    pub task_type: Option<String>,
}

/// Per-subtask learning signal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubtaskOutcomeSignal {
    pub subtask_id: String,
    pub title: String,
    pub success: bool,
    pub retry_count: u32,
    /// Tools specifically used for this subtask
    pub tools_used: Vec<String>,
    /// Verification pass rate: passed / total criteria
    pub verification_pass_rate: Option<f64>,
    /// Files modified (for entity extraction)
    pub files_modified: Vec<String>,
}

/// Trait for feeding task outcomes into a learning system.
///
/// Defined in `services` to keep durable_task decoupled from the concrete
/// learning pipeline types (EntityGraph, PatternLibrary, ProgressiveCalibrator)
/// which live in the `runtime` crate. The runtime implements this trait and
/// injects it into the lifecycle service.
#[async_trait]
pub trait TaskLearningBridge: Send + Sync {
    /// Record a completed task's outcome as a learning signal.
    ///
    /// Implementations should:
    /// 1. Extract entities from goal/titles → EntityGraph::learn()
    /// 2. Record tool chain patterns → PatternLibrary::record_outcome()
    /// 3. Calibrate routing thresholds → ProgressiveCalibrator::record()
    /// 4. Extract reusable templates from successful contracts
    async fn learn_from_task_outcome(&self, signal: &TaskOutcomeSignal) -> Result<(), String>;

    /// Extract a reusable plan template from a successful task contract.
    ///
    /// If the contract's pattern is novel enough, store it for future plan generation.
    async fn extract_template(
        &self,
        contract: &TaskContract,
        report: &TaskDeliveryReport,
    ) -> Result<Option<String>, String>;

    /// Suggest tools for an upcoming subtask based on learned patterns.
    ///
    /// Returns tool names sorted by relevance (most relevant first).
    async fn suggest_tools(
        &self,
        goal: &str,
        domain_hint: Option<&str>,
        task_type: Option<&str>,
    ) -> Result<Vec<String>, String>;

    /// Query historical performance for a given task pattern.
    ///
    /// Returns (success_rate, avg_retries, avg_turns) or None if no data.
    async fn task_pattern_stats(
        &self,
        goal_pattern: &str,
    ) -> Result<Option<TaskPatternStats>, String>;
}

/// Historical performance stats for a task pattern.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskPatternStats {
    pub pattern: String,
    pub total_attempts: u32,
    pub success_rate: f64,
    pub avg_retries: f64,
    pub avg_turns: f64,
    pub avg_verification_pass_rate: f64,
}

/// No-op implementation when learning is not available.
pub struct NoopTaskLearningBridge;

#[async_trait]
impl TaskLearningBridge for NoopTaskLearningBridge {
    async fn learn_from_task_outcome(&self, _signal: &TaskOutcomeSignal) -> Result<(), String> {
        Ok(()) // silently ignore
    }
    async fn extract_template(
        &self,
        _contract: &TaskContract,
        _report: &TaskDeliveryReport,
    ) -> Result<Option<String>, String> {
        Ok(None)
    }
    async fn suggest_tools(
        &self,
        _goal: &str,
        _domain: Option<&str>,
        _task_type: Option<&str>,
    ) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }
    async fn task_pattern_stats(
        &self,
        _pattern: &str,
    ) -> Result<Option<TaskPatternStats>, String> {
        Ok(None)
    }
}

/// Helper to build a TaskOutcomeSignal from a completed contract + delivery report.
pub fn build_outcome_signal(
    contract: &TaskContract,
    report: &TaskDeliveryReport,
    tools_used: Vec<String>,
    user_rating: Option<u8>,
    domain_hint: Option<String>,
    task_type: Option<String>,
) -> TaskOutcomeSignal {
    let subtask_outcomes: Vec<SubtaskOutcomeSignal> = contract
        .subtasks
        .iter()
        .map(|s| {
            let summary = report
                .subtask_summaries
                .iter()
                .find(|sum| sum.id == s.id);
            let pass_rate = if s.criteria.is_empty() {
                None
            } else {
                let total = s.criteria.len() as f64;
                let passed = summary.map(|sm| sm.criteria_passed as f64).unwrap_or(0.0);
                Some(passed / total)
            };
            SubtaskOutcomeSignal {
                subtask_id: s.id.clone(),
                title: s.title.clone(),
                success: s.stage.is_success(),
                retry_count: s.retry_count,
                tools_used: Vec::new(), // per-subtask tools not tracked yet
                verification_pass_rate: pass_rate,
                files_modified: s.files.clone(),
            }
        })
        .collect();

    let total_retries: u32 = contract.subtasks.iter().map(|s| s.retry_count).sum();

    TaskOutcomeSignal {
        task_id: report.task_id.clone(),
        contract_id: report.contract_id.clone(),
        goal: contract.goal.clone(),
        success: report
            .subtask_summaries
            .iter()
            .all(|s| s.stage == "verified" || s.stage == "completed" || s.stage == "skipped"),
        user_rating,
        tools_used,
        subtask_outcomes,
        total_verification_attempts: report.total_verifications,
        total_retries,
        total_turns: report.total_turns,
        domain_hint,
        task_type,
    }
}

// ─── DurableTaskLifecycle Trait ──────────────────────────────────────────────

/// Context returned when beginning a subtask execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubtaskExecutionContext {
    pub subtask_id: String,
    pub title: String,
    pub description: Option<String>,
    pub files: Vec<String>,
    pub criteria: Vec<VerificationCriterion>,
    pub snapshot_name: Option<String>,
}

/// Context for resuming a paused task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResumeContext {
    pub task_id: String,
    pub contract: TaskContract,
    pub active_subtask: Option<String>,
    pub checkpoint: Option<TaskCheckpoint>,
    pub verification_history: Vec<SubtaskVerificationReport>,
}

/// Amendment to an active contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContractAmendment {
    pub reason: String,
    pub updated_subtasks: Option<Vec<DurableSubtask>>,
    pub updated_global_verification: Option<Vec<VerificationCriterion>>,
    pub updated_scope: Option<TaskScope>,
}

/// Main lifecycle orchestrator for durable tasks.
#[async_trait]
pub trait DurableTaskLifecycle: Send + Sync {
    // ── Contract Phase ──
    async fn create_contract(
        &self,
        user_id: &str,
        session_id: &str,
        goal: &str,
        plan: &TaskPlan,
        scope: TaskScope,
    ) -> Result<TaskContract, String>;

    async fn amend_contract(
        &self,
        contract_id: &str,
        amendment: ContractAmendment,
    ) -> Result<TaskContract, String>;

    async fn get_contract(&self, contract_id: &str) -> Result<Option<TaskContract>, String>;

    // ── Execution Phase ──
    async fn begin_subtask(
        &self,
        task_id: &str,
        subtask_id: &str,
    ) -> Result<SubtaskExecutionContext, String>;

    async fn complete_subtask_execution(
        &self,
        task_id: &str,
        subtask_id: &str,
    ) -> Result<(), String>;

    async fn fail_subtask(
        &self,
        task_id: &str,
        subtask_id: &str,
        error: &str,
    ) -> Result<(), String>;

    // ── Verification Phase ──
    async fn verify_subtask(
        &self,
        task_id: &str,
        subtask_id: &str,
    ) -> Result<SubtaskVerificationReport, String>;

    async fn verify_global(&self, task_id: &str) -> Result<Vec<VerificationResult>, String>;

    // ── Resume / Recovery ──
    async fn pause_task(&self, task_id: &str) -> Result<(), String>;

    async fn resume_task(
        &self,
        task_id: &str,
        session_id: &str,
    ) -> Result<TaskResumeContext, String>;

    // ── Delivery ──
    async fn deliver_task(&self, task_id: &str) -> Result<TaskDeliveryReport, String>;

    // ── Git4Data ──
    async fn snapshot_task_state(&self, task_id: &str) -> Result<String, String>;

    async fn rollback_task(&self, task_id: &str, snapshot: &str) -> Result<(), String>;
}

// ─── MatrixOne Implementation ───────────────────────────────────────────────

/// Production implementation backed by MatrixOne SQL.
pub struct MatrixOneDurableTaskLifecycle {
    pool: sqlx::Pool<sqlx::MySql>,
    branch_ops: Arc<dyn TaskBranchOps>,
    work_dir: std::path::PathBuf,
}

impl MatrixOneDurableTaskLifecycle {
    pub fn new(pool: sqlx::Pool<sqlx::MySql>, work_dir: std::path::PathBuf) -> Self {
        let branch_ops: Arc<dyn TaskBranchOps> =
            Arc::new(TaskBranchService::new(pool.clone()));
        Self {
            pool,
            branch_ops,
            work_dir,
        }
    }

    pub fn from_shared(shared: &mo_agent_core::SharedPool, work_dir: std::path::PathBuf) -> Self {
        Self::new(shared.get().clone(), work_dir)
    }

    /// Create with a custom branch ops implementation (for testing).
    pub fn with_branch_ops(
        pool: sqlx::Pool<sqlx::MySql>,
        branch_ops: Arc<dyn TaskBranchOps>,
        work_dir: std::path::PathBuf,
    ) -> Self {
        Self {
            pool,
            branch_ops,
            work_dir,
        }
    }

    fn runner(&self) -> VerificationRunner {
        VerificationRunner::new(self.work_dir.clone())
    }

    // ── Private Helpers ──

    async fn load_contract_by_id(&self, contract_id: &str) -> Result<Option<TaskContract>, String> {
        let row = sqlx::query(
            "SELECT contract_id, task_id, user_id, session_id, goal, \
             scope_json, subtasks_json, criteria_json, version, status, \
             CAST(created_at AS CHAR) AS created_at, \
             CAST(updated_at AS CHAR) AS updated_at \
             FROM task_contracts WHERE contract_id = ?",
        )
        .bind(contract_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| format!("load_contract: {e}"))?;

        match row {
            None => Ok(None),
            Some(row) => {
                let contract = self.parse_contract_row(&row)?;
                Ok(Some(contract))
            }
        }
    }

    async fn load_contract_by_task(&self, task_id: &str) -> Result<Option<TaskContract>, String> {
        let row = sqlx::query(
            "SELECT contract_id, task_id, user_id, session_id, goal, \
             scope_json, subtasks_json, criteria_json, version, status, \
             CAST(created_at AS CHAR) AS created_at, \
             CAST(updated_at AS CHAR) AS updated_at \
             FROM task_contracts WHERE task_id = ? AND status != 'abandoned' \
             ORDER BY version DESC LIMIT 1",
        )
        .bind(task_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| format!("load_contract_by_task: {e}"))?;

        match row {
            None => Ok(None),
            Some(row) => {
                let contract = self.parse_contract_row(&row)?;
                Ok(Some(contract))
            }
        }
    }

    fn parse_contract_row(&self, row: &sqlx::mysql::MySqlRow) -> Result<TaskContract, String> {
        use sqlx::Row;
        let contract_id: String = row.try_get("contract_id").map_err(|e| e.to_string())?;
        let task_id: String = row.try_get("task_id").map_err(|e| e.to_string())?;
        let goal: String = row.try_get("goal").map_err(|e| e.to_string())?;
        let version: i32 = row.try_get("version").unwrap_or(1);
        let status_str: String = row.try_get("status").unwrap_or_default();
        let created_at: String = row.try_get("created_at").unwrap_or_default();
        let updated_at: String = row.try_get("updated_at").unwrap_or_default();

        let scope_json: Option<String> = row.try_get("scope_json").ok().flatten();
        let scope: TaskScope = scope_json
            .as_deref()
            .and_then(|j| serde_json::from_str(j).ok())
            .unwrap_or_default();

        let subtasks_json: String = row.try_get("subtasks_json").map_err(|e| e.to_string())?;
        let subtasks: Vec<DurableSubtask> =
            serde_json::from_str(&subtasks_json).map_err(|e| format!("parse subtasks: {e}"))?;

        let criteria_json: String = row.try_get("criteria_json").map_err(|e| e.to_string())?;
        let global_verification: Vec<VerificationCriterion> =
            serde_json::from_str(&criteria_json).map_err(|e| format!("parse criteria: {e}"))?;

        Ok(TaskContract {
            contract_id,
            task_id,
            goal,
            scope,
            subtasks,
            global_verification,
            version: version as u32,
            status: ContractStatus::parse(&status_str),
            created_at,
            updated_at,
        })
    }

    async fn persist_contract(&self, contract: &TaskContract) -> Result<(), String> {
        let scope_json =
            serde_json::to_string(&contract.scope).map_err(|e| format!("scope json: {e}"))?;
        let subtasks_json = serde_json::to_string(&contract.subtasks)
            .map_err(|e| format!("subtasks json: {e}"))?;
        let criteria_json = serde_json::to_string(&contract.global_verification)
            .map_err(|e| format!("criteria json: {e}"))?;

        sqlx::query(
            "INSERT INTO task_contracts \
             (contract_id, task_id, session_id, user_id, goal, scope_json, \
              subtasks_json, criteria_json, version, status, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(), NOW()) \
             ON DUPLICATE KEY UPDATE \
             subtasks_json = VALUES(subtasks_json), criteria_json = VALUES(criteria_json), \
             scope_json = VALUES(scope_json), version = VALUES(version), \
             status = VALUES(status), updated_at = NOW()",
        )
        .bind(&contract.contract_id)
        .bind(&contract.task_id)
        .bind("")  // session_id filled by caller context
        .bind("")  // user_id filled by caller context
        .bind(&contract.goal)
        .bind(&scope_json)
        .bind(&subtasks_json)
        .bind(&criteria_json)
        .bind(contract.version as i32)
        .bind(contract.status.as_str())
        .execute(&self.pool)
        .await
        .map_err(|e| format!("persist_contract: {e}"))?;
        Ok(())
    }

    async fn persist_contract_with_user(
        &self,
        contract: &TaskContract,
        user_id: &str,
        session_id: &str,
    ) -> Result<(), String> {
        let scope_json =
            serde_json::to_string(&contract.scope).map_err(|e| format!("scope json: {e}"))?;
        let subtasks_json = serde_json::to_string(&contract.subtasks)
            .map_err(|e| format!("subtasks json: {e}"))?;
        let criteria_json = serde_json::to_string(&contract.global_verification)
            .map_err(|e| format!("criteria json: {e}"))?;

        sqlx::query(
            "INSERT INTO task_contracts \
             (contract_id, task_id, session_id, user_id, goal, scope_json, \
              subtasks_json, criteria_json, version, status, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(), NOW()) \
             ON DUPLICATE KEY UPDATE \
             subtasks_json = VALUES(subtasks_json), criteria_json = VALUES(criteria_json), \
             scope_json = VALUES(scope_json), version = VALUES(version), \
             status = VALUES(status), updated_at = NOW()",
        )
        .bind(&contract.contract_id)
        .bind(&contract.task_id)
        .bind(session_id)
        .bind(user_id)
        .bind(&contract.goal)
        .bind(&scope_json)
        .bind(&subtasks_json)
        .bind(&criteria_json)
        .bind(contract.version as i32)
        .bind(contract.status.as_str())
        .execute(&self.pool)
        .await
        .map_err(|e| format!("persist_contract: {e}"))?;
        Ok(())
    }

    async fn save_verification_result(
        &self,
        task_id: &str,
        contract_id: &str,
        session_id: &str,
        subtask_id: &str,
        result: &VerificationResult,
        attempt: u32,
    ) -> Result<(), String> {
        let result_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO task_verification_results \
             (result_id, contract_id, task_id, subtask_id, criterion_id, \
              session_id, passed, evidence, expected, duration_ms, error_message, attempt) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&result_id)
        .bind(contract_id)
        .bind(task_id)
        .bind(subtask_id)
        .bind(&result.criterion_id)
        .bind(session_id)
        .bind(if result.passed { 1i32 } else { 0i32 })
        .bind(&result.evidence)
        .bind(&result.expected)
        .bind(result.duration_ms as i64)
        .bind(&result.error)
        .bind(attempt as i32)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("save_verification: {e}"))?;
        Ok(())
    }

    async fn load_verification_history(
        &self,
        task_id: &str,
    ) -> Result<Vec<SubtaskVerificationReport>, String> {
        let rows = sqlx::query(
            "SELECT subtask_id, criterion_id, passed, evidence, expected, \
             duration_ms, error_message, CAST(created_at AS CHAR) AS created_at \
             FROM task_verification_results \
             WHERE task_id = ? ORDER BY created_at",
        )
        .bind(task_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| format!("load_verification_history: {e}"))?;

        use sqlx::Row;
        use std::collections::BTreeMap;
        let mut grouped: BTreeMap<String, Vec<(VerificationResult, String)>> = BTreeMap::new();

        for row in &rows {
            let subtask_id: String = row.try_get("subtask_id").unwrap_or_default();
            let ts: String = row.try_get("created_at").unwrap_or_default();
            let vr = VerificationResult {
                criterion_id: row.try_get("criterion_id").unwrap_or_default(),
                passed: row.try_get::<i32, _>("passed").unwrap_or(0) != 0,
                evidence: row.try_get("evidence").ok().flatten().unwrap_or_default(),
                expected: row.try_get("expected").ok().flatten().unwrap_or_default(),
                duration_ms: row.try_get::<i64, _>("duration_ms").unwrap_or(0) as u64,
                error: row.try_get("error_message").ok().flatten(),
            };
            grouped.entry(subtask_id).or_default().push((vr, ts));
        }

        let reports: Vec<SubtaskVerificationReport> = grouped
            .into_iter()
            .map(|(subtask_id, items)| {
                let ts = items.last().map(|(_, t)| t.clone()).unwrap_or_default();
                let results: Vec<VerificationResult> = items.into_iter().map(|(r, _)| r).collect();
                let all_required_passed = results.iter().all(|r| r.passed);
                SubtaskVerificationReport {
                    subtask_id,
                    all_required_passed,
                    results,
                    timestamp: ts,
                }
            })
            .collect();
        Ok(reports)
    }

    fn find_subtask_mut<'a>(
        contract: &'a mut TaskContract,
        subtask_id: &str,
    ) -> Result<&'a mut DurableSubtask, String> {
        contract
            .subtasks
            .iter_mut()
            .find(|s| s.id == subtask_id)
            .ok_or_else(|| format!("subtask '{subtask_id}' not found"))
    }

    fn find_subtask<'a>(
        contract: &'a TaskContract,
        subtask_id: &str,
    ) -> Result<&'a DurableSubtask, String> {
        contract
            .subtasks
            .iter()
            .find(|s| s.id == subtask_id)
            .ok_or_else(|| format!("subtask '{subtask_id}' not found"))
    }
}

#[async_trait]
impl DurableTaskLifecycle for MatrixOneDurableTaskLifecycle {
    async fn create_contract(
        &self,
        user_id: &str,
        session_id: &str,
        goal: &str,
        plan: &TaskPlan,
        scope: TaskScope,
    ) -> Result<TaskContract, String> {
        let contract_id = uuid::Uuid::new_v4().to_string();
        let task_id = uuid::Uuid::new_v4().to_string();

        let subtasks: Vec<DurableSubtask> = plan
            .subtasks
            .iter()
            .map(|sp| DurableSubtask {
                id: sp.id.clone(),
                title: sp.title.clone(),
                description: sp.description.clone(),
                depends_on: sp.depends_on.clone(),
                effort: sp.effort.clone(),
                files: sp.files.clone(),
                stage: SubtaskStage::Pending,
                criteria: Vec::new(), // populated by LLM or user later
                max_retries: 2,
                retry_count: 0,
                snapshot_name: None,
                data_branch: None,
                diff_summary: None,
            })
            .collect();

        let now = chrono::Utc::now().to_rfc3339();
        let contract = TaskContract {
            contract_id: contract_id.clone(),
            task_id,
            goal: goal.to_string(),
            scope,
            subtasks,
            global_verification: Vec::new(),
            version: 1,
            status: ContractStatus::Draft,
            created_at: now.clone(),
            updated_at: now,
        };

        self.persist_contract_with_user(&contract, user_id, session_id)
            .await?;
        Ok(contract)
    }

    async fn amend_contract(
        &self,
        contract_id: &str,
        amendment: ContractAmendment,
    ) -> Result<TaskContract, String> {
        let mut contract = self
            .load_contract_by_id(contract_id)
            .await?
            .ok_or_else(|| format!("contract '{contract_id}' not found"))?;

        if let Some(subtasks) = amendment.updated_subtasks {
            contract.subtasks = subtasks;
        }
        if let Some(global) = amendment.updated_global_verification {
            contract.global_verification = global;
        }
        if let Some(scope) = amendment.updated_scope {
            contract.scope = scope;
        }
        contract.version += 1;
        contract.status = ContractStatus::Amended;
        contract.updated_at = chrono::Utc::now().to_rfc3339();

        self.persist_contract(&contract).await?;
        Ok(contract)
    }

    async fn get_contract(&self, contract_id: &str) -> Result<Option<TaskContract>, String> {
        self.load_contract_by_id(contract_id).await
    }

    async fn begin_subtask(
        &self,
        task_id: &str,
        subtask_id: &str,
    ) -> Result<SubtaskExecutionContext, String> {
        let mut contract = self
            .load_contract_by_task(task_id)
            .await?
            .ok_or_else(|| format!("no contract for task '{task_id}'"))?;

        // Check startability first (immutable borrow)
        {
            let subtask = Self::find_subtask(&contract, subtask_id)?;
            if !subtask.stage.can_start() {
                return Err(format!(
                    "subtask '{}' in stage '{}' cannot start",
                    subtask_id,
                    subtask.stage.as_str()
                ));
            }
        }

        // Git4Data: create snapshot before execution for rollback support
        let version = contract.version;
        let snapshot_name = match self
            .branch_ops
            .create_snapshot(task_id, subtask_id, version)
            .await
        {
            Ok(name) if !name.is_empty() => Some(name),
            Ok(_) => None, // empty = noop branch ops
            Err(e) => {
                // Non-fatal: log but continue without snapshot
                eprintln!("warn: snapshot failed for {subtask_id}: {e}");
                None
            }
        };

        // Now mutably update
        let subtask = Self::find_subtask_mut(&mut contract, subtask_id)?;
        subtask.snapshot_name = snapshot_name.clone();
        let ctx = SubtaskExecutionContext {
            subtask_id: subtask.id.clone(),
            title: subtask.title.clone(),
            description: subtask.description.clone(),
            files: subtask.files.clone(),
            criteria: subtask.criteria.clone(),
            snapshot_name,
        };

        subtask.stage = SubtaskStage::Executing;
        self.persist_contract(&contract).await?;
        Ok(ctx)
    }

    async fn complete_subtask_execution(
        &self,
        task_id: &str,
        subtask_id: &str,
    ) -> Result<(), String> {
        let mut contract = self
            .load_contract_by_task(task_id)
            .await?
            .ok_or_else(|| format!("no contract for task '{task_id}'"))?;

        let subtask = Self::find_subtask_mut(&mut contract, subtask_id)?;
        if !matches!(subtask.stage, SubtaskStage::Executing) {
            return Err(format!(
                "subtask '{}' not executing (stage: {})",
                subtask_id,
                subtask.stage.as_str()
            ));
        }

        // Git4Data: capture diff if we have a snapshot
        if let Some(snap) = &subtask.snapshot_name {
            match self.branch_ops.diff_since_snapshot(snap).await {
                Ok(diff) => {
                    subtask.diff_summary = Some(diff);
                }
                Err(e) => {
                    eprintln!("warn: diff failed for {subtask_id}: {e}");
                }
            }
        }

        subtask.stage = if subtask.criteria.is_empty() {
            // No criteria → auto-verify
            SubtaskStage::Verified
        } else {
            SubtaskStage::AwaitingVerification
        };

        self.persist_contract(&contract).await?;
        Ok(())
    }

    async fn fail_subtask(
        &self,
        task_id: &str,
        subtask_id: &str,
        error: &str,
    ) -> Result<(), String> {
        let mut contract = self
            .load_contract_by_task(task_id)
            .await?
            .ok_or_else(|| format!("no contract for task '{task_id}'"))?;

        let subtask = Self::find_subtask_mut(&mut contract, subtask_id)?;
        subtask.stage = SubtaskStage::ExecutionFailed {
            error: error.to_string(),
        };

        self.persist_contract(&contract).await?;
        Ok(())
    }

    async fn verify_subtask(
        &self,
        task_id: &str,
        subtask_id: &str,
    ) -> Result<SubtaskVerificationReport, String> {
        let mut contract = self
            .load_contract_by_task(task_id)
            .await?
            .ok_or_else(|| format!("no contract for task '{task_id}'"))?;

        let subtask = Self::find_subtask(&contract, subtask_id)?;
        // State guard: only verify subtasks ready for verification
        if !matches!(
            subtask.stage,
            SubtaskStage::AwaitingVerification | SubtaskStage::Verifying
        ) {
            return Err(format!(
                "subtask '{}' not ready for verification (stage: {})",
                subtask_id,
                subtask.stage.as_str()
            ));
        }
        let durable_st = subtask.clone();

        // Run verification
        let runner = self.runner();
        let report = runner.verify_subtask(&durable_st).await;

        // Persist each result
        for r in &report.results {
            let _ = self
                .save_verification_result(
                    task_id,
                    &contract.contract_id,
                    "",  // session from context
                    subtask_id,
                    r,
                    durable_st.retry_count + 1,
                )
                .await;
        }

        // Update stage + git4data actions
        let snapshot_name = durable_st.snapshot_name.clone();
        let subtask = Self::find_subtask_mut(&mut contract, subtask_id)?;
        if report.all_required_passed {
            subtask.stage = SubtaskStage::Verified;
            // Git4Data: cleanup snapshot after successful verification
            if let Some(snap) = &snapshot_name {
                if let Err(e) = self.branch_ops.cleanup_snapshot(snap).await {
                    eprintln!("warn: snapshot cleanup failed for {subtask_id}: {e}");
                }
            }
        } else {
            subtask.retry_count += 1;
            if subtask.retry_count >= subtask.max_retries {
                // Git4Data: rollback on max-retry abandonment
                if let Some(snap) = &snapshot_name {
                    if let Err(e) = self.branch_ops.rollback_to_snapshot(snap).await {
                        eprintln!("warn: rollback failed for {subtask_id}: {e}");
                    }
                    // Clean up the snapshot after rollback
                    if let Err(e) = self.branch_ops.cleanup_snapshot(snap).await {
                        eprintln!("warn: post-rollback cleanup failed for {subtask_id}: {e}");
                    }
                }
                subtask.stage = SubtaskStage::Abandoned {
                    reason: format!(
                        "verification failed after {} attempts",
                        subtask.retry_count
                    ),
                };
            } else {
                subtask.stage = SubtaskStage::VerificationFailed {
                    results: report.results.clone(),
                };
            }
        }

        self.persist_contract(&contract).await?;
        Ok(report)
    }

    async fn verify_global(&self, task_id: &str) -> Result<Vec<VerificationResult>, String> {
        let contract = self
            .load_contract_by_task(task_id)
            .await?
            .ok_or_else(|| format!("no contract for task '{task_id}'"))?;

        // All required subtasks must be verified
        let unverified: Vec<&DurableSubtask> = contract
            .subtasks
            .iter()
            .filter(|s| !s.stage.is_terminal() && !matches!(s.stage, SubtaskStage::Verified))
            .collect();
        if !unverified.is_empty() {
            let ids: Vec<&str> = unverified.iter().map(|s| s.id.as_str()).collect();
            return Err(format!("subtasks not ready for global verification: {:?}", ids));
        }

        let runner = self.runner();
        let mut results = Vec::new();
        for criterion in &contract.global_verification {
            let result = runner.run_criterion(criterion).await;
            results.push(result);
        }
        Ok(results)
    }

    async fn pause_task(&self, task_id: &str) -> Result<(), String> {
        let mut contract = self
            .load_contract_by_task(task_id)
            .await?
            .ok_or_else(|| format!("no contract for task '{task_id}'"))?;
        contract.updated_at = chrono::Utc::now().to_rfc3339();
        self.persist_contract(&contract).await?;
        Ok(())
    }

    async fn resume_task(
        &self,
        task_id: &str,
        _session_id: &str,
    ) -> Result<TaskResumeContext, String> {
        let contract = self
            .load_contract_by_task(task_id)
            .await?
            .ok_or_else(|| format!("no contract for task '{task_id}'"))?;

        let active_subtask = contract
            .subtasks
            .iter()
            .find(|s| matches!(s.stage, SubtaskStage::Executing))
            .map(|s| s.id.clone());

        let verification_history = self.load_verification_history(task_id).await?;

        Ok(TaskResumeContext {
            task_id: task_id.to_string(),
            contract,
            active_subtask,
            checkpoint: None, // loaded from agent_tasks separately
            verification_history,
        })
    }

    async fn deliver_task(&self, task_id: &str) -> Result<TaskDeliveryReport, String> {
        let mut contract = self
            .load_contract_by_task(task_id)
            .await?
            .ok_or_else(|| format!("no contract for task '{task_id}'"))?;

        let subtask_summaries: Vec<SubtaskDeliverySummary> = contract
            .subtasks
            .iter()
            .map(|s| {
                let criteria_total = s.criteria.len() as u32;
                SubtaskDeliverySummary {
                    id: s.id.clone(),
                    title: s.title.clone(),
                    stage: s.stage.as_str().to_string(),
                    criteria_passed: 0, // filled from verification_history
                    criteria_total,
                    retry_count: s.retry_count,
                }
            })
            .collect();

        let global_results = if contract
            .subtasks
            .iter()
            .all(|s| s.stage.is_success() || s.stage.is_terminal())
        {
            let runner = self.runner();
            let mut results = Vec::new();
            for c in &contract.global_verification {
                results.push(runner.run_criterion(c).await);
            }
            results
        } else {
            Vec::new()
        };

        let report = TaskDeliveryReport {
            task_id: task_id.to_string(),
            contract_id: contract.contract_id.clone(),
            goal: contract.goal.clone(),
            subtask_summaries,
            global_verification: global_results,
            total_turns: 0,
            total_tokens: 0,
            total_verifications: 0,
            risks: contract
                .scope
                .assumptions
                .iter()
                .map(|a| format!("Assumption: {a}"))
                .collect(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        };

        contract.status = ContractStatus::Completed;
        contract.updated_at = chrono::Utc::now().to_rfc3339();
        self.persist_contract(&contract).await?;

        Ok(report)
    }

    async fn snapshot_task_state(&self, task_id: &str) -> Result<String, String> {
        let contract = self
            .load_contract_by_task(task_id)
            .await?
            .ok_or_else(|| format!("no contract for task '{task_id}'"))?;
        self.branch_ops
            .create_snapshot(task_id, "global", contract.version)
            .await
    }

    async fn rollback_task(&self, _task_id: &str, snapshot: &str) -> Result<(), String> {
        self.branch_ops.rollback_to_snapshot(snapshot).await
    }
}

// ─── Local File-based Implementation ────────────────────────────────────────

/// File-based implementation for development/offline mode.
pub struct LocalDurableTaskLifecycle {
    contracts_dir: std::path::PathBuf,
    branch_ops: Arc<dyn TaskBranchOps>,
    work_dir: std::path::PathBuf,
}

impl LocalDurableTaskLifecycle {
    pub fn new(data_dir: std::path::PathBuf, work_dir: std::path::PathBuf) -> Self {
        let branch_ops: Arc<dyn TaskBranchOps> = Arc::new(LocalFileBranchOps::new(
            data_dir.join("snapshots"),
            work_dir.clone(),
        ));
        Self {
            contracts_dir: data_dir.join("contracts"),
            branch_ops,
            work_dir,
        }
    }

    /// Create with a custom branch ops implementation (for testing).
    pub fn with_branch_ops(
        data_dir: std::path::PathBuf,
        branch_ops: Arc<dyn TaskBranchOps>,
        work_dir: std::path::PathBuf,
    ) -> Self {
        Self {
            contracts_dir: data_dir.join("contracts"),
            branch_ops,
            work_dir,
        }
    }

    fn contract_path(&self, contract_id: &str) -> std::path::PathBuf {
        self.contracts_dir.join(format!("{contract_id}.json"))
    }

    fn load_local(&self, contract_id: &str) -> Result<Option<TaskContract>, String> {
        let path = self.contract_path(contract_id);
        if !path.exists() {
            return Ok(None);
        }
        let data = std::fs::read_to_string(&path).map_err(|e| format!("read contract: {e}"))?;
        let c: TaskContract = serde_json::from_str(&data).map_err(|e| format!("parse: {e}"))?;
        Ok(Some(c))
    }

    fn save_local(&self, contract: &TaskContract) -> Result<(), String> {
        std::fs::create_dir_all(&self.contracts_dir)
            .map_err(|e| format!("mkdir contracts: {e}"))?;
        let json =
            serde_json::to_string_pretty(contract).map_err(|e| format!("serialize: {e}"))?;
        let path = self.contract_path(&contract.contract_id);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &json).map_err(|e| format!("write: {e}"))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("rename: {e}"))?;
        Ok(())
    }

    /// Scan contracts dir for a task_id match.
    fn find_by_task(&self, task_id: &str) -> Result<Option<TaskContract>, String> {
        if !self.contracts_dir.exists() {
            return Ok(None);
        }
        let entries = std::fs::read_dir(&self.contracts_dir)
            .map_err(|e| format!("readdir: {e}"))?;
        for entry in entries.flatten() {
            if entry.path().extension().map(|e| e == "json").unwrap_or(false) {
                if let Ok(data) = std::fs::read_to_string(entry.path()) {
                    if let Ok(c) = serde_json::from_str::<TaskContract>(&data) {
                        if c.task_id == task_id && c.status != ContractStatus::Abandoned {
                            return Ok(Some(c));
                        }
                    }
                }
            }
        }
        Ok(None)
    }
}

#[async_trait]
impl DurableTaskLifecycle for LocalDurableTaskLifecycle {
    async fn create_contract(
        &self,
        _user_id: &str,
        _session_id: &str,
        goal: &str,
        plan: &TaskPlan,
        scope: TaskScope,
    ) -> Result<TaskContract, String> {
        let contract_id = uuid::Uuid::new_v4().to_string();
        let task_id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now().to_rfc3339();

        let subtasks = plan
            .subtasks
            .iter()
            .map(|sp| DurableSubtask {
                id: sp.id.clone(),
                title: sp.title.clone(),
                description: sp.description.clone(),
                depends_on: sp.depends_on.clone(),
                effort: sp.effort.clone(),
                files: sp.files.clone(),
                ..Default::default()
            })
            .collect();

        let contract = TaskContract {
            contract_id,
            task_id,
            goal: goal.to_string(),
            scope,
            subtasks,
            global_verification: Vec::new(),
            version: 1,
            status: ContractStatus::Draft,
            created_at: now.clone(),
            updated_at: now,
        };
        self.save_local(&contract)?;
        Ok(contract)
    }

    async fn amend_contract(
        &self,
        contract_id: &str,
        amendment: ContractAmendment,
    ) -> Result<TaskContract, String> {
        let mut contract = self
            .load_local(contract_id)?
            .ok_or_else(|| format!("contract '{contract_id}' not found"))?;
        if let Some(subtasks) = amendment.updated_subtasks {
            contract.subtasks = subtasks;
        }
        if let Some(global) = amendment.updated_global_verification {
            contract.global_verification = global;
        }
        if let Some(scope) = amendment.updated_scope {
            contract.scope = scope;
        }
        contract.version += 1;
        contract.status = ContractStatus::Amended;
        contract.updated_at = chrono::Utc::now().to_rfc3339();
        self.save_local(&contract)?;
        Ok(contract)
    }

    async fn get_contract(&self, contract_id: &str) -> Result<Option<TaskContract>, String> {
        self.load_local(contract_id)
    }

    async fn begin_subtask(
        &self,
        task_id: &str,
        subtask_id: &str,
    ) -> Result<SubtaskExecutionContext, String> {
        let mut contract = self
            .find_by_task(task_id)?
            .ok_or_else(|| format!("no contract for task '{task_id}'"))?;
        let subtask = contract
            .subtasks
            .iter_mut()
            .find(|s| s.id == subtask_id)
            .ok_or_else(|| format!("subtask '{subtask_id}' not found"))?;

        if !subtask.stage.can_start() {
            return Err(format!("subtask '{}' cannot start", subtask_id));
        }

        // Git4Data: create snapshot before execution
        let snapshot_name = match self
            .branch_ops
            .create_snapshot(task_id, subtask_id, contract.version)
            .await
        {
            Ok(name) if !name.is_empty() => Some(name),
            Ok(_) => None,
            Err(e) => {
                eprintln!("warn: local snapshot failed for {subtask_id}: {e}");
                None
            }
        };

        subtask.snapshot_name = snapshot_name.clone();
        let ctx = SubtaskExecutionContext {
            subtask_id: subtask.id.clone(),
            title: subtask.title.clone(),
            description: subtask.description.clone(),
            files: subtask.files.clone(),
            criteria: subtask.criteria.clone(),
            snapshot_name,
        };
        subtask.stage = SubtaskStage::Executing;
        self.save_local(&contract)?;
        Ok(ctx)
    }

    async fn complete_subtask_execution(
        &self,
        task_id: &str,
        subtask_id: &str,
    ) -> Result<(), String> {
        let mut contract = self
            .find_by_task(task_id)?
            .ok_or_else(|| format!("no contract for task '{task_id}'"))?;
        let subtask = contract
            .subtasks
            .iter_mut()
            .find(|s| s.id == subtask_id)
            .ok_or_else(|| format!("subtask '{subtask_id}' not found"))?;

        // Git4Data: capture diff if we have a snapshot
        if let Some(snap) = &subtask.snapshot_name {
            match self.branch_ops.diff_since_snapshot(snap).await {
                Ok(diff) => {
                    subtask.diff_summary = Some(diff);
                }
                Err(e) => {
                    eprintln!("warn: local diff failed for {subtask_id}: {e}");
                }
            }
        }

        subtask.stage = if subtask.criteria.is_empty() {
            SubtaskStage::Verified
        } else {
            SubtaskStage::AwaitingVerification
        };
        self.save_local(&contract)?;
        Ok(())
    }

    async fn fail_subtask(
        &self,
        task_id: &str,
        subtask_id: &str,
        error: &str,
    ) -> Result<(), String> {
        let mut contract = self
            .find_by_task(task_id)?
            .ok_or_else(|| format!("no contract for task '{task_id}'"))?;
        let subtask = contract
            .subtasks
            .iter_mut()
            .find(|s| s.id == subtask_id)
            .ok_or_else(|| format!("subtask '{subtask_id}' not found"))?;
        subtask.stage = SubtaskStage::ExecutionFailed {
            error: error.to_string(),
        };
        self.save_local(&contract)?;
        Ok(())
    }

    async fn verify_subtask(
        &self,
        task_id: &str,
        subtask_id: &str,
    ) -> Result<SubtaskVerificationReport, String> {
        let mut contract = self
            .find_by_task(task_id)?
            .ok_or_else(|| format!("no contract for task '{task_id}'"))?;

        let durable_st = contract
            .subtasks
            .iter()
            .find(|s| s.id == subtask_id)
            .ok_or_else(|| format!("subtask '{subtask_id}' not found"))?
            .clone();

        // State guard: only verify subtasks ready for verification
        if !matches!(
            durable_st.stage,
            SubtaskStage::AwaitingVerification | SubtaskStage::Verifying
        ) {
            return Err(format!(
                "subtask '{}' not ready for verification (stage: {})",
                subtask_id,
                durable_st.stage.as_str()
            ));
        }

        let runner = VerificationRunner::new(self.work_dir.clone());
        // Per-subtask: use local verification (skips global_only & LlmJudge)
        let report = runner.verify_subtask_local(&durable_st).await;

        let snapshot_name = durable_st.snapshot_name.clone();
        let subtask = contract
            .subtasks
            .iter_mut()
            .find(|s| s.id == subtask_id)
            .ok_or_else(|| format!("subtask '{subtask_id}' disappeared during verification"))?;
        if report.all_required_passed {
            subtask.stage = SubtaskStage::Verified;
            // Git4Data: cleanup snapshot after successful verification
            if let Some(snap) = &snapshot_name {
                let _ = self.branch_ops.cleanup_snapshot(snap).await;
            }
        } else {
            subtask.retry_count += 1;
            if subtask.retry_count >= subtask.max_retries {
                // Git4Data: rollback on max-retry abandonment
                if let Some(snap) = &snapshot_name {
                    let _ = self.branch_ops.rollback_to_snapshot(snap).await;
                    let _ = self.branch_ops.cleanup_snapshot(snap).await;
                }
                subtask.stage = SubtaskStage::Abandoned {
                    reason: format!("failed after {} attempts", subtask.retry_count),
                };
            } else {
                subtask.stage = SubtaskStage::VerificationFailed {
                    results: report.results.clone(),
                };
            }
        }
        self.save_local(&contract)?;
        Ok(report)
    }

    async fn verify_global(&self, task_id: &str) -> Result<Vec<VerificationResult>, String> {
        let contract = self
            .find_by_task(task_id)?
            .ok_or_else(|| format!("no contract for task '{task_id}'"))?;
        let runner = VerificationRunner::new(self.work_dir.clone());
        let mut results = Vec::new();
        for c in &contract.global_verification {
            results.push(runner.run_criterion(c).await);
        }
        Ok(results)
    }

    async fn pause_task(&self, task_id: &str) -> Result<(), String> {
        let mut contract = self
            .find_by_task(task_id)?
            .ok_or_else(|| format!("no contract for task '{task_id}'"))?;
        contract.updated_at = chrono::Utc::now().to_rfc3339();
        self.save_local(&contract)?;
        Ok(())
    }

    async fn resume_task(
        &self,
        task_id: &str,
        _session_id: &str,
    ) -> Result<TaskResumeContext, String> {
        let contract = self
            .find_by_task(task_id)?
            .ok_or_else(|| format!("no contract for task '{task_id}'"))?;
        let active_subtask = contract
            .subtasks
            .iter()
            .find(|s| matches!(s.stage, SubtaskStage::Executing))
            .map(|s| s.id.clone());
        Ok(TaskResumeContext {
            task_id: task_id.to_string(),
            contract,
            active_subtask,
            checkpoint: None,
            verification_history: Vec::new(),
        })
    }

    async fn deliver_task(&self, task_id: &str) -> Result<TaskDeliveryReport, String> {
        let mut contract = self
            .find_by_task(task_id)?
            .ok_or_else(|| format!("no contract for task '{task_id}'"))?;

        let summaries: Vec<SubtaskDeliverySummary> = contract
            .subtasks
            .iter()
            .map(|s| SubtaskDeliverySummary {
                id: s.id.clone(),
                title: s.title.clone(),
                stage: s.stage.as_str().to_string(),
                criteria_passed: 0,
                criteria_total: s.criteria.len() as u32,
                retry_count: s.retry_count,
            })
            .collect();

        contract.status = ContractStatus::Completed;
        contract.updated_at = chrono::Utc::now().to_rfc3339();
        self.save_local(&contract)?;

        Ok(TaskDeliveryReport {
            task_id: task_id.to_string(),
            contract_id: contract.contract_id.clone(),
            goal: contract.goal.clone(),
            subtask_summaries: summaries,
            global_verification: Vec::new(),
            total_turns: 0,
            total_tokens: 0,
            total_verifications: 0,
            risks: Vec::new(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        })
    }

    async fn snapshot_task_state(&self, task_id: &str) -> Result<String, String> {
        let contract = self
            .find_by_task(task_id)?
            .ok_or_else(|| format!("no contract for task '{task_id}'"))?;
        self.branch_ops
            .create_snapshot(task_id, "global", contract.version)
            .await
    }

    async fn rollback_task(&self, _task_id: &str, snapshot: &str) -> Result<(), String> {
        self.branch_ops.rollback_to_snapshot(snapshot).await
    }
}

// ─── Unconfigured (safe default) ────────────────────────────────────────────

/// Returns errors for all operations. Used when no backend is configured.
pub struct UnconfiguredDurableTaskLifecycle;

#[async_trait]
impl DurableTaskLifecycle for UnconfiguredDurableTaskLifecycle {
    async fn create_contract(
        &self, _: &str, _: &str, _: &str, _: &TaskPlan, _: TaskScope,
    ) -> Result<TaskContract, String> {
        Err("durable task service not configured".into())
    }
    async fn amend_contract(
        &self, _: &str, _: ContractAmendment,
    ) -> Result<TaskContract, String> {
        Err("durable task service not configured".into())
    }
    async fn get_contract(&self, _: &str) -> Result<Option<TaskContract>, String> {
        Err("durable task service not configured".into())
    }
    async fn begin_subtask(
        &self, _: &str, _: &str,
    ) -> Result<SubtaskExecutionContext, String> {
        Err("durable task service not configured".into())
    }
    async fn complete_subtask_execution(&self, _: &str, _: &str) -> Result<(), String> {
        Err("durable task service not configured".into())
    }
    async fn fail_subtask(&self, _: &str, _: &str, _: &str) -> Result<(), String> {
        Err("durable task service not configured".into())
    }
    async fn verify_subtask(
        &self, _: &str, _: &str,
    ) -> Result<SubtaskVerificationReport, String> {
        Err("durable task service not configured".into())
    }
    async fn verify_global(&self, _: &str) -> Result<Vec<VerificationResult>, String> {
        Err("durable task service not configured".into())
    }
    async fn pause_task(&self, _: &str) -> Result<(), String> {
        Err("durable task service not configured".into())
    }
    async fn resume_task(
        &self, _: &str, _: &str,
    ) -> Result<TaskResumeContext, String> {
        Err("durable task service not configured".into())
    }
    async fn deliver_task(&self, _: &str) -> Result<TaskDeliveryReport, String> {
        Err("durable task service not configured".into())
    }
    async fn snapshot_task_state(&self, _: &str) -> Result<String, String> {
        Err("durable task service not configured".into())
    }
    async fn rollback_task(&self, _: &str, _: &str) -> Result<(), String> {
        Err("durable task service not configured".into())
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtask_stage_transitions() {
        let stage = SubtaskStage::Pending;
        assert!(stage.can_start());
        assert!(!stage.is_terminal());
        assert!(!stage.is_success());

        let stage = SubtaskStage::Executing;
        assert!(!stage.can_start());

        let stage = SubtaskStage::VerificationFailed {
            results: vec![],
        };
        assert!(stage.can_start()); // can retry

        let stage = SubtaskStage::Completed;
        assert!(stage.is_terminal());
        assert!(stage.is_success());

        let stage = SubtaskStage::Abandoned {
            reason: "max retries".into(),
        };
        assert!(stage.is_terminal());
        assert!(!stage.is_success());
    }

    #[test]
    fn contract_status_roundtrip() {
        for status in &[
            ContractStatus::Draft,
            ContractStatus::Active,
            ContractStatus::Amended,
            ContractStatus::Completed,
            ContractStatus::Abandoned,
        ] {
            assert_eq!(ContractStatus::parse(status.as_str()), *status);
        }
    }

    #[test]
    fn verifier_kind_serde_roundtrip() {
        let criterion = VerificationCriterion {
            id: "build-check".into(),
            description: "Build must pass".into(),
            verifier: VerifierKind::BuildPass {
                cmd: "cargo build".into(),
            },
            required: true,
            timeout_sec: 300,
            global_only: false,
        };
        let json = serde_json::to_string(&criterion).unwrap();
        let parsed: VerificationCriterion = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "build-check");
        assert!(parsed.required);
    }

    #[test]
    fn composite_verifier_serde() {
        let criterion = VerificationCriterion {
            id: "all-checks".into(),
            description: "All checks must pass".into(),
            verifier: VerifierKind::Composite {
                criteria: vec![
                    VerificationCriterion {
                        id: "file-exists".into(),
                        description: "File exists".into(),
                        verifier: VerifierKind::FileExists {
                            paths: vec!["src/main.rs".into()],
                        },
                        required: true,
                        timeout_sec: 10,
                        global_only: false,
                    },
                    VerificationCriterion {
                        id: "build".into(),
                        description: "Build passes".into(),
                        verifier: VerifierKind::BuildPass {
                            cmd: "cargo check".into(),
                        },
                        required: true,
                        timeout_sec: 120,
                        global_only: false,
                    },
                ],
                require_all: true,
            },
            required: true,
            timeout_sec: 300,
            global_only: false,
        };
        let json = serde_json::to_string_pretty(&criterion).unwrap();
        let parsed: VerificationCriterion = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "all-checks");
    }

    #[test]
    fn durable_subtask_default() {
        let st = DurableSubtask::default();
        assert_eq!(st.stage, SubtaskStage::Pending);
        assert_eq!(st.max_retries, 2);
        assert_eq!(st.retry_count, 0);
    }

    #[test]
    fn subtask_stage_as_str() {
        assert_eq!(SubtaskStage::Pending.as_str(), "pending");
        assert_eq!(SubtaskStage::Executing.as_str(), "executing");
        assert_eq!(SubtaskStage::AwaitingVerification.as_str(), "awaiting_verification");
        assert_eq!(SubtaskStage::Verified.as_str(), "verified");
        assert_eq!(SubtaskStage::Completed.as_str(), "completed");
        assert_eq!(
            SubtaskStage::Abandoned {
                reason: "x".into()
            }
            .as_str(),
            "abandoned"
        );
    }

    #[test]
    fn task_contract_serde_roundtrip() {
        let contract = TaskContract {
            contract_id: "c-1".into(),
            task_id: "t-1".into(),
            goal: "Implement auth".into(),
            scope: TaskScope::default(),
            subtasks: vec![DurableSubtask {
                id: "s-1".into(),
                title: "Add JWT".into(),
                criteria: vec![VerificationCriterion {
                    id: "v-1".into(),
                    description: "Tests pass".into(),
                    verifier: VerifierKind::TestPass {
                        cmd: "cargo test".into(),
                        min_pass_rate: 1.0,
                    },
                    required: true,
                    timeout_sec: 120,
                    global_only: false,
                }],
                ..Default::default()
            }],
            global_verification: vec![],
            version: 1,
            status: ContractStatus::Active,
            created_at: "2026-04-01T00:00:00Z".into(),
            updated_at: "2026-04-01T00:00:00Z".into(),
        };
        let json = serde_json::to_string(&contract).unwrap();
        let parsed: TaskContract = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.contract_id, "c-1");
        assert_eq!(parsed.subtasks.len(), 1);
        assert_eq!(parsed.subtasks[0].criteria.len(), 1);
    }

    #[tokio::test]
    async fn verification_runner_file_exists() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Create a test file
        std::fs::write(tmp.path().join("hello.txt"), "world").unwrap();

        let runner = VerificationRunner::new(tmp.path().to_path_buf());

        // Should pass: file exists
        let criterion = VerificationCriterion {
            id: "f1".into(),
            description: "file exists".into(),
            verifier: VerifierKind::FileExists {
                paths: vec!["hello.txt".into()],
            },
            required: true,
            timeout_sec: 10,
            global_only: false,
        };
        let result = runner.run_criterion(&criterion).await;
        assert!(result.passed, "file should exist: {:?}", result);

        // Should fail: file doesn't exist
        let criterion = VerificationCriterion {
            id: "f2".into(),
            description: "missing file".into(),
            verifier: VerifierKind::FileExists {
                paths: vec!["missing.txt".into()],
            },
            required: true,
            timeout_sec: 10,
            global_only: false,
        };
        let result = runner.run_criterion(&criterion).await;
        assert!(!result.passed, "file should NOT exist");
    }

    #[tokio::test]
    async fn verification_runner_command() {
        let tmp = tempfile::TempDir::new().unwrap();
        let runner = VerificationRunner::new(tmp.path().to_path_buf());

        // Should pass: true
        let criterion = VerificationCriterion {
            id: "c1".into(),
            description: "true".into(),
            verifier: VerifierKind::Command {
                cmd: "true".into(),
                expected_exit: 0,
            },
            required: true,
            timeout_sec: 10,
            global_only: false,
        };
        let result = runner.run_criterion(&criterion).await;
        assert!(result.passed);

        // Should fail: false
        let criterion = VerificationCriterion {
            id: "c2".into(),
            description: "false".into(),
            verifier: VerifierKind::Command {
                cmd: "false".into(),
                expected_exit: 0,
            },
            required: true,
            timeout_sec: 10,
            global_only: false,
        };
        let result = runner.run_criterion(&criterion).await;
        assert!(!result.passed);
    }

    #[tokio::test]
    async fn verification_runner_grep_check() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("code.rs"), "fn main() { println!(\"hello\"); }").unwrap();

        let runner = VerificationRunner::new(tmp.path().to_path_buf());

        // Should pass: pattern found
        let criterion = VerificationCriterion {
            id: "g1".into(),
            description: "has main".into(),
            verifier: VerifierKind::GrepCheck {
                file: "code.rs".into(),
                pattern: "fn main".into(),
                should_match: true,
            },
            required: true,
            timeout_sec: 10,
            global_only: false,
        };
        let result = runner.run_criterion(&criterion).await;
        assert!(result.passed);

        // Should pass: pattern NOT found (should_match=false)
        let criterion = VerificationCriterion {
            id: "g2".into(),
            description: "no unsafe".into(),
            verifier: VerifierKind::GrepCheck {
                file: "code.rs".into(),
                pattern: "unsafe".into(),
                should_match: false,
            },
            required: true,
            timeout_sec: 10,
            global_only: false,
        };
        let result = runner.run_criterion(&criterion).await;
        assert!(result.passed);
    }

    #[tokio::test]
    async fn verification_runner_command_output() {
        let tmp = tempfile::TempDir::new().unwrap();
        let runner = VerificationRunner::new(tmp.path().to_path_buf());

        let criterion = VerificationCriterion {
            id: "co1".into(),
            description: "echo contains hello".into(),
            verifier: VerifierKind::CommandOutput {
                cmd: "echo hello world".into(),
                contains: vec!["hello".into()],
                not_contains: vec!["error".into()],
            },
            required: true,
            timeout_sec: 10,
            global_only: false,
        };
        let result = runner.run_criterion(&criterion).await;
        assert!(result.passed);
    }

    #[tokio::test]
    async fn verification_runner_subtask_report() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("test.txt"), "content").unwrap();

        let runner = VerificationRunner::new(tmp.path().to_path_buf());

        let subtask = DurableSubtask {
            id: "st-1".into(),
            title: "test subtask".into(),
            criteria: vec![
                VerificationCriterion {
                    id: "v1".into(),
                    description: "file exists".into(),
                    verifier: VerifierKind::FileExists {
                        paths: vec!["test.txt".into()],
                    },
                    required: true,
                    timeout_sec: 10,
                    global_only: false,
                },
                VerificationCriterion {
                    id: "v2".into(),
                    description: "echo ok".into(),
                    verifier: VerifierKind::Command {
                        cmd: "true".into(),
                        expected_exit: 0,
                    },
                    required: true,
                    timeout_sec: 10,
                    global_only: false,
                },
            ],
            ..Default::default()
        };

        let report = runner.verify_subtask(&subtask).await;
        assert!(report.all_required_passed);
        assert_eq!(report.results.len(), 2);
        assert!(report.results.iter().all(|r| r.passed));
    }

    #[tokio::test]
    async fn verification_timeout() {
        let tmp = tempfile::TempDir::new().unwrap();
        let runner = VerificationRunner::new(tmp.path().to_path_buf());

        let criterion = VerificationCriterion {
            id: "timeout".into(),
            description: "should timeout".into(),
            verifier: VerifierKind::Command {
                cmd: "sleep 30".into(),
                expected_exit: 0,
            },
            required: true,
            timeout_sec: 1, // 1 second timeout
            global_only: false,
        };
        let result = runner.run_criterion(&criterion).await;
        assert!(!result.passed);
        assert!(result.error.as_deref().unwrap_or("").contains("timed out"));
    }

    #[test]
    fn delivery_report_serde() {
        let report = TaskDeliveryReport {
            task_id: "t-1".into(),
            contract_id: "c-1".into(),
            goal: "test".into(),
            subtask_summaries: vec![SubtaskDeliverySummary {
                id: "s-1".into(),
                title: "step 1".into(),
                stage: "completed".into(),
                criteria_passed: 3,
                criteria_total: 3,
                retry_count: 0,
            }],
            global_verification: vec![],
            total_turns: 5,
            total_tokens: 10000,
            total_verifications: 3,
            risks: vec!["rate limiting not implemented".into()],
            timestamp: "2026-04-01T00:00:00Z".into(),
        };
        let json = serde_json::to_string(&report).unwrap();
        let parsed: TaskDeliveryReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.subtask_summaries.len(), 1);
        assert_eq!(parsed.risks.len(), 1);
    }

    // ── Local Lifecycle Integration Tests ──

    fn make_test_plan() -> TaskPlan {
        use crate::task_orchestrator::TaskStatus;
        TaskPlan {
            subtasks: vec![
                crate::task_orchestrator::SubtaskPlan {
                    id: "sub-1".into(),
                    title: "First subtask".into(),
                    description: Some("Do the first thing".into()),
                    depends_on: vec![],
                    status: TaskStatus::Pending,
                    acceptance: Some("Files exist".into()),
                    effort: None,
                    files: vec![],
                },
                crate::task_orchestrator::SubtaskPlan {
                    id: "sub-2".into(),
                    title: "Second subtask".into(),
                    description: None,
                    depends_on: vec!["sub-1".into()],
                    status: TaskStatus::Pending,
                    acceptance: None,
                    effort: None,
                    files: vec![],
                },
            ],
            notes: None,
        }
    }

    #[tokio::test]
    async fn local_lifecycle_create_and_get() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = LocalDurableTaskLifecycle::new(
            tmp.path().join("data"),
            tmp.path().join("work"),
        );

        let plan = make_test_plan();
        let contract = svc
            .create_contract("user-1", "session-1", "Build something", &plan, TaskScope::default())
            .await
            .unwrap();

        assert!(!contract.task_id.is_empty());
        assert_eq!(contract.subtasks.len(), 2);
        assert_eq!(contract.status, ContractStatus::Draft);

        // Can retrieve by id
        let loaded = svc.get_contract(&contract.contract_id).await.unwrap();
        assert!(loaded.is_some());
        assert_eq!(loaded.unwrap().goal, "Build something");
    }

    #[tokio::test]
    async fn local_lifecycle_begin_complete_verify() {
        let tmp = tempfile::TempDir::new().unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();

        let svc = LocalDurableTaskLifecycle::new(
            tmp.path().join("data"),
            work.clone(),
        );

        let plan = make_test_plan();
        let contract = svc
            .create_contract("u", "s", "test", &plan, TaskScope::default())
            .await
            .unwrap();

        // Begin subtask
        let ctx = svc.begin_subtask(&contract.task_id, "sub-1").await.unwrap();
        assert_eq!(ctx.title, "First subtask");

        // Complete (no criteria → auto-verified)
        svc.complete_subtask_execution(&contract.task_id, "sub-1")
            .await
            .unwrap();

        // Check state persisted
        let c = svc.get_contract(&contract.contract_id).await.unwrap().unwrap();
        assert!(matches!(c.subtasks[0].stage, SubtaskStage::Verified));
    }

    #[tokio::test]
    async fn local_lifecycle_fail_subtask() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = LocalDurableTaskLifecycle::new(
            tmp.path().join("data"),
            tmp.path().join("work"),
        );

        let plan = make_test_plan();
        let contract = svc
            .create_contract("u", "s", "test", &plan, TaskScope::default())
            .await
            .unwrap();

        svc.begin_subtask(&contract.task_id, "sub-1").await.unwrap();
        svc.fail_subtask(&contract.task_id, "sub-1", "compilation error")
            .await
            .unwrap();

        let c = svc.get_contract(&contract.contract_id).await.unwrap().unwrap();
        assert!(matches!(c.subtasks[0].stage, SubtaskStage::ExecutionFailed { .. }));
    }

    #[tokio::test]
    async fn local_lifecycle_amend_contract() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = LocalDurableTaskLifecycle::new(
            tmp.path().join("data"),
            tmp.path().join("work"),
        );

        let plan = make_test_plan();
        let contract = svc
            .create_contract("u", "s", "test", &plan, TaskScope::default())
            .await
            .unwrap();
        assert_eq!(contract.version, 1);

        let amended = svc
            .amend_contract(
                &contract.contract_id,
                ContractAmendment {
                    reason: "add scope".into(),
                    updated_subtasks: None,
                    updated_global_verification: None,
                    updated_scope: Some(TaskScope {
                        in_scope: vec!["auth module".into()],
                        out_of_scope: vec!["UI".into()],
                        assumptions: vec![],
                    }),
                },
            )
            .await
            .unwrap();

        assert_eq!(amended.version, 2);
        assert_eq!(amended.status, ContractStatus::Amended);
        assert_eq!(amended.scope.in_scope, vec!["auth module"]);
    }

    #[tokio::test]
    async fn local_lifecycle_verify_with_criteria() {
        let tmp = tempfile::TempDir::new().unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(work.join("output.txt"), "hello world").unwrap();

        let svc = LocalDurableTaskLifecycle::new(tmp.path().join("data"), work);

        // Create plan with a subtask that has criteria
        let mut plan = make_test_plan();
        plan.subtasks[0].id = "check-sub".into();
        plan.subtasks.truncate(1);

        let mut contract = svc
            .create_contract("u", "s", "test", &plan, TaskScope::default())
            .await
            .unwrap();

        // Amend to add verification criteria
        contract.subtasks[0].criteria = vec![
            VerificationCriterion {
                id: "file-check".into(),
                description: "output.txt exists".into(),
                verifier: VerifierKind::FileExists {
                    paths: vec!["output.txt".into()],
                },
                required: true,
                timeout_sec: 10,
                global_only: false,
            },
            VerificationCriterion {
                id: "grep-check".into(),
                description: "contains hello".into(),
                verifier: VerifierKind::GrepCheck {
                    file: "output.txt".into(),
                    pattern: "hello".into(),
                    should_match: true,
                },
                required: true,
                timeout_sec: 10,
                global_only: false,
            },
        ];
        svc.amend_contract(
            &contract.contract_id,
            ContractAmendment {
                reason: "add criteria".into(),
                updated_subtasks: Some(contract.subtasks.clone()),
                updated_global_verification: None,
                updated_scope: None,
            },
        )
        .await
        .unwrap();

        // Execute flow: begin → complete → verify
        svc.begin_subtask(&contract.task_id, "check-sub").await.unwrap();
        svc.complete_subtask_execution(&contract.task_id, "check-sub")
            .await
            .unwrap();

        let report = svc.verify_subtask(&contract.task_id, "check-sub").await.unwrap();
        assert!(report.all_required_passed, "should pass: {:?}", report.results);
        assert_eq!(report.results.len(), 2);

        // Check stage is Verified
        let c = svc.get_contract(&contract.contract_id).await.unwrap().unwrap();
        assert!(matches!(c.subtasks[0].stage, SubtaskStage::Verified));
    }

    #[tokio::test]
    async fn local_lifecycle_deliver() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = LocalDurableTaskLifecycle::new(
            tmp.path().join("data"),
            tmp.path().join("work"),
        );

        let plan = make_test_plan();
        let contract = svc.create_contract("u", "s", "deliver test", &plan, TaskScope::default())
            .await
            .unwrap();

        // Complete both subtasks (no criteria → auto-verified)
        svc.begin_subtask(&contract.task_id, "sub-1").await.unwrap();
        svc.complete_subtask_execution(&contract.task_id, "sub-1").await.unwrap();
        svc.begin_subtask(&contract.task_id, "sub-2").await.unwrap();
        svc.complete_subtask_execution(&contract.task_id, "sub-2").await.unwrap();

        let report = svc.deliver_task(&contract.task_id).await.unwrap();
        assert_eq!(report.goal, "deliver test");
        assert_eq!(report.subtask_summaries.len(), 2);
    }

    #[tokio::test]
    async fn local_lifecycle_resume() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = LocalDurableTaskLifecycle::new(
            tmp.path().join("data"),
            tmp.path().join("work"),
        );

        let plan = make_test_plan();
        let contract = svc.create_contract("u", "s", "resume test", &plan, TaskScope::default())
            .await
            .unwrap();

        svc.begin_subtask(&contract.task_id, "sub-1").await.unwrap();

        let ctx = svc.resume_task(&contract.task_id, "new-session").await.unwrap();
        assert_eq!(ctx.active_subtask, Some("sub-1".into()));
        assert_eq!(ctx.contract.subtasks.len(), 2);
    }

    #[tokio::test]
    async fn unconfigured_lifecycle_returns_errors() {
        let svc = UnconfiguredDurableTaskLifecycle;
        assert!(svc.get_contract("x").await.is_err());
        assert!(svc.pause_task("x").await.is_err());
        assert!(svc.deliver_task("x").await.is_err());
    }

    // ── Learning Bridge Tests ──

    #[tokio::test]
    async fn noop_learning_bridge_is_safe() {
        let bridge = NoopTaskLearningBridge;
        let signal = TaskOutcomeSignal {
            task_id: "t1".into(),
            contract_id: "c1".into(),
            goal: "test".into(),
            success: true,
            user_rating: Some(85),
            tools_used: vec!["read_file".into(), "bash".into()],
            subtask_outcomes: vec![],
            total_verification_attempts: 1,
            total_retries: 0,
            total_turns: 5,
            domain_hint: Some("code".into()),
            task_type: Some("code".into()),
        };
        assert!(bridge.learn_from_task_outcome(&signal).await.is_ok());
        assert!(bridge.suggest_tools("test", None, None).await.unwrap().is_empty());
        assert!(bridge.task_pattern_stats("test").await.unwrap().is_none());
    }

    #[test]
    fn build_outcome_signal_from_contract_and_report() {
        let contract = TaskContract {
            contract_id: "c1".into(),
            task_id: "t1".into(),
            goal: "Implement auth".into(),
            scope: TaskScope::default(),
            subtasks: vec![
                DurableSubtask {
                    id: "s1".into(),
                    title: "Add JWT".into(),
                    stage: SubtaskStage::Verified,
                    criteria: vec![VerificationCriterion {
                        id: "v1".into(),
                        description: "tests pass".into(),
                        verifier: VerifierKind::Command {
                            cmd: "cargo test".into(),
                            expected_exit: 0,
                        },
                        required: true,
                        timeout_sec: 60,
                        global_only: false,
                    }],
                    retry_count: 1,
                    files: vec!["src/auth.rs".into()],
                    ..Default::default()
                },
                DurableSubtask {
                    id: "s2".into(),
                    title: "Add routes".into(),
                    stage: SubtaskStage::Completed,
                    retry_count: 0,
                    ..Default::default()
                },
            ],
            global_verification: vec![],
            version: 1,
            status: ContractStatus::Completed,
            created_at: "2026-04-01".into(),
            updated_at: "2026-04-01".into(),
        };

        let report = TaskDeliveryReport {
            task_id: "t1".into(),
            contract_id: "c1".into(),
            goal: "Implement auth".into(),
            subtask_summaries: vec![
                SubtaskDeliverySummary {
                    id: "s1".into(),
                    title: "Add JWT".into(),
                    stage: "verified".into(),
                    criteria_passed: 1,
                    criteria_total: 1,
                    retry_count: 1,
                },
                SubtaskDeliverySummary {
                    id: "s2".into(),
                    title: "Add routes".into(),
                    stage: "completed".into(),
                    criteria_passed: 0,
                    criteria_total: 0,
                    retry_count: 0,
                },
            ],
            global_verification: vec![],
            total_turns: 10,
            total_tokens: 50000,
            total_verifications: 2,
            risks: vec![],
            timestamp: "2026-04-01".into(),
        };

        let signal = build_outcome_signal(
            &contract,
            &report,
            vec!["read_file".into(), "bash".into(), "str_replace".into()],
            Some(90),
            Some("code".into()),
            Some("code".into()),
        );

        assert!(signal.success);
        assert_eq!(signal.user_rating, Some(90));
        assert_eq!(signal.tools_used.len(), 3);
        assert_eq!(signal.total_retries, 1);
        assert_eq!(signal.subtask_outcomes.len(), 2);
        assert!(signal.subtask_outcomes[0].success);
        assert_eq!(signal.subtask_outcomes[0].retry_count, 1);
        // s1 has 1 criterion, 1 passed → rate = 1.0
        assert_eq!(signal.subtask_outcomes[0].verification_pass_rate, Some(1.0));
        // s2 has 0 criteria → None
        assert!(signal.subtask_outcomes[1].verification_pass_rate.is_none());
    }

    #[test]
    fn task_outcome_signal_serde() {
        let signal = TaskOutcomeSignal {
            task_id: "t1".into(),
            contract_id: "c1".into(),
            goal: "test".into(),
            success: true,
            user_rating: None,
            tools_used: vec!["bash".into()],
            subtask_outcomes: vec![SubtaskOutcomeSignal {
                subtask_id: "s1".into(),
                title: "sub".into(),
                success: true,
                retry_count: 0,
                tools_used: vec![],
                verification_pass_rate: Some(1.0),
                files_modified: vec!["a.rs".into()],
            }],
            total_verification_attempts: 1,
            total_retries: 0,
            total_turns: 3,
            domain_hint: None,
            task_type: None,
        };
        let json = serde_json::to_string(&signal).unwrap();
        let parsed: TaskOutcomeSignal = serde_json::from_str(&json).unwrap();
        assert!(parsed.success);
        assert_eq!(parsed.subtask_outcomes.len(), 1);
    }

    #[test]
    fn task_pattern_stats_serde() {
        let stats = TaskPatternStats {
            pattern: "rust-feature".into(),
            total_attempts: 10,
            success_rate: 0.8,
            avg_retries: 0.5,
            avg_turns: 12.0,
            avg_verification_pass_rate: 0.95,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let parsed: TaskPatternStats = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.total_attempts, 10);
    }

    // ── MockBranchOps for git4data integration tests ──

    use std::sync::Mutex;

    #[derive(Debug, Default)]
    struct BranchOpsLog {
        snapshots_created: Vec<(String, String, u32)>, // (task_id, subtask_id, version)
        diffs_requested: Vec<String>,                   // snapshot names
        rollbacks: Vec<String>,                         // snapshot names
        cleanups: Vec<String>,                          // snapshot names
    }

    struct MockBranchOps {
        log: Mutex<BranchOpsLog>,
        fail_snapshot: bool,
        fail_diff: bool,
        fail_rollback: bool,
        diff_rows: i64,
    }

    impl MockBranchOps {
        fn new() -> Self {
            Self {
                log: Mutex::new(BranchOpsLog::default()),
                fail_snapshot: false,
                fail_diff: false,
                fail_rollback: false,
                diff_rows: 5,
            }
        }

        fn failing_snapshot() -> Self {
            Self {
                fail_snapshot: true,
                ..Self::new()
            }
        }

        fn with_diff_rows(rows: i64) -> Self {
            Self {
                diff_rows: rows,
                ..Self::new()
            }
        }

        fn log(&self) -> BranchOpsLog {
            let guard = self.log.lock().unwrap();
            BranchOpsLog {
                snapshots_created: guard.snapshots_created.clone(),
                diffs_requested: guard.diffs_requested.clone(),
                rollbacks: guard.rollbacks.clone(),
                cleanups: guard.cleanups.clone(),
            }
        }
    }

    #[async_trait]
    impl TaskBranchOps for MockBranchOps {
        async fn create_snapshot(
            &self,
            task_id: &str,
            subtask_id: &str,
            version: u32,
        ) -> Result<String, String> {
            if self.fail_snapshot {
                return Err("mock snapshot failure".into());
            }
            let name = format!("task_{task_id}_{subtask_id}_v{version}");
            self.log
                .lock()
                .unwrap()
                .snapshots_created
                .push((task_id.into(), subtask_id.into(), version));
            Ok(name)
        }

        async fn diff_since_snapshot(&self, snapshot: &str) -> Result<DiffSummary, String> {
            if self.fail_diff {
                return Err("mock diff failure".into());
            }
            self.log
                .lock()
                .unwrap()
                .diffs_requested
                .push(snapshot.into());
            Ok(DiffSummary {
                snapshot: snapshot.into(),
                changed_rows: self.diff_rows,
            })
        }

        async fn rollback_to_snapshot(&self, snapshot: &str) -> Result<(), String> {
            if self.fail_rollback {
                return Err("mock rollback failure".into());
            }
            self.log.lock().unwrap().rollbacks.push(snapshot.into());
            Ok(())
        }

        async fn cleanup_snapshot(&self, snapshot: &str) -> Result<(), String> {
            self.log.lock().unwrap().cleanups.push(snapshot.into());
            Ok(())
        }
    }

    // ── Git4Data Integration Tests ──

    fn make_local_svc_with_mock(
        tmp: &tempfile::TempDir,
        mock: Arc<dyn TaskBranchOps>,
    ) -> LocalDurableTaskLifecycle {
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        LocalDurableTaskLifecycle::with_branch_ops(
            tmp.path().join("data"),
            mock,
            work,
        )
    }

    #[tokio::test]
    async fn git4data_begin_subtask_creates_snapshot() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mock = Arc::new(MockBranchOps::new());
        let svc = make_local_svc_with_mock(&tmp, mock.clone());

        let plan = make_test_plan();
        let contract = svc
            .create_contract("u", "s", "test", &plan, TaskScope::default())
            .await
            .unwrap();

        let ctx = svc.begin_subtask(&contract.task_id, "sub-1").await.unwrap();

        // Snapshot was created
        let log = mock.log();
        assert_eq!(log.snapshots_created.len(), 1);
        assert_eq!(log.snapshots_created[0].0, contract.task_id);
        assert_eq!(log.snapshots_created[0].1, "sub-1");
        assert_eq!(log.snapshots_created[0].2, 1); // version 1

        // snapshot_name is populated in context
        assert!(ctx.snapshot_name.is_some());
        let snap_name = ctx.snapshot_name.unwrap();
        assert!(snap_name.contains(&contract.task_id));
        assert!(snap_name.contains("sub-1"));

        // snapshot_name is persisted in contract
        let c = svc.get_contract(&contract.contract_id).await.unwrap().unwrap();
        assert_eq!(c.subtasks[0].snapshot_name.as_deref(), Some(snap_name.as_str()));
    }

    #[tokio::test]
    async fn git4data_snapshot_failure_is_nonfatal() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mock = Arc::new(MockBranchOps::failing_snapshot());
        let svc = make_local_svc_with_mock(&tmp, mock);

        let plan = make_test_plan();
        let contract = svc
            .create_contract("u", "s", "test", &plan, TaskScope::default())
            .await
            .unwrap();

        // Should succeed even though snapshot fails
        let ctx = svc.begin_subtask(&contract.task_id, "sub-1").await.unwrap();
        assert!(ctx.snapshot_name.is_none());

        // subtask still transitions to Executing
        let c = svc.get_contract(&contract.contract_id).await.unwrap().unwrap();
        assert!(matches!(c.subtasks[0].stage, SubtaskStage::Executing));
    }

    #[tokio::test]
    async fn git4data_complete_captures_diff() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mock = Arc::new(MockBranchOps::with_diff_rows(42));
        let svc = make_local_svc_with_mock(&tmp, mock.clone());

        let plan = make_test_plan();
        let contract = svc
            .create_contract("u", "s", "test", &plan, TaskScope::default())
            .await
            .unwrap();

        svc.begin_subtask(&contract.task_id, "sub-1").await.unwrap();
        svc.complete_subtask_execution(&contract.task_id, "sub-1")
            .await
            .unwrap();

        // Diff was captured
        let log = mock.log();
        assert_eq!(log.diffs_requested.len(), 1);

        // Diff summary is persisted
        let c = svc.get_contract(&contract.contract_id).await.unwrap().unwrap();
        let diff = c.subtasks[0].diff_summary.as_ref().unwrap();
        assert_eq!(diff.changed_rows, 42);
    }

    #[tokio::test]
    async fn git4data_verify_success_cleans_up_snapshot() {
        let tmp = tempfile::TempDir::new().unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(work.join("output.txt"), "hello").unwrap();

        let mock = Arc::new(MockBranchOps::new());
        let svc = LocalDurableTaskLifecycle::with_branch_ops(
            tmp.path().join("data"),
            mock.clone(),
            work,
        );

        // Create plan with a single subtask with file-exists criterion
        let mut plan = make_test_plan();
        plan.subtasks.truncate(1);
        let contract = svc
            .create_contract("u", "s", "test", &plan, TaskScope::default())
            .await
            .unwrap();

        // Amend to add criteria
        let mut c = svc.get_contract(&contract.contract_id).await.unwrap().unwrap();
        c.subtasks[0].criteria = vec![VerificationCriterion {
            id: "f1".into(),
            description: "output exists".into(),
            verifier: VerifierKind::FileExists {
                paths: vec!["output.txt".into()],
            },
            required: true,
            timeout_sec: 10,
            global_only: false,
        }];
        svc.amend_contract(
            &contract.contract_id,
            ContractAmendment {
                reason: "add criteria".into(),
                updated_subtasks: Some(c.subtasks),
                updated_global_verification: None,
                updated_scope: None,
            },
        )
        .await
        .unwrap();

        // Execute flow: begin → complete → verify
        svc.begin_subtask(&contract.task_id, "sub-1").await.unwrap();
        svc.complete_subtask_execution(&contract.task_id, "sub-1")
            .await
            .unwrap();
        let report = svc.verify_subtask(&contract.task_id, "sub-1").await.unwrap();
        assert!(report.all_required_passed);

        // Snapshot should be cleaned up on success
        let log = mock.log();
        assert_eq!(log.cleanups.len(), 1);
        assert_eq!(log.rollbacks.len(), 0);
    }

    #[tokio::test]
    async fn git4data_verify_failure_triggers_rollback_on_max_retries() {
        let tmp = tempfile::TempDir::new().unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();

        let mock = Arc::new(MockBranchOps::new());
        let svc = LocalDurableTaskLifecycle::with_branch_ops(
            tmp.path().join("data"),
            mock.clone(),
            work,
        );

        let mut plan = make_test_plan();
        plan.subtasks.truncate(1);
        let contract = svc
            .create_contract("u", "s", "test", &plan, TaskScope::default())
            .await
            .unwrap();

        // Add a criterion that will always fail
        let mut c = svc.get_contract(&contract.contract_id).await.unwrap().unwrap();
        c.subtasks[0].criteria = vec![VerificationCriterion {
            id: "missing".into(),
            description: "nonexistent file".into(),
            verifier: VerifierKind::FileExists {
                paths: vec!["does_not_exist.txt".into()],
            },
            required: true,
            timeout_sec: 10,
            global_only: false,
        }];
        c.subtasks[0].max_retries = 2; // allow 1 retry before abandonment
        svc.amend_contract(
            &contract.contract_id,
            ContractAmendment {
                reason: "add criteria".into(),
                updated_subtasks: Some(c.subtasks),
                updated_global_verification: None,
                updated_scope: None,
            },
        )
        .await
        .unwrap();

        // Execute and verify (will fail)
        svc.begin_subtask(&contract.task_id, "sub-1").await.unwrap();
        svc.complete_subtask_execution(&contract.task_id, "sub-1")
            .await
            .unwrap();
        let report = svc.verify_subtask(&contract.task_id, "sub-1").await.unwrap();
        assert!(!report.all_required_passed);

        // First failure: retry_count < max_retries → VerificationFailed, no rollback
        let c = svc.get_contract(&contract.contract_id).await.unwrap().unwrap();
        assert!(matches!(c.subtasks[0].stage, SubtaskStage::VerificationFailed { .. }));
        assert_eq!(mock.log().rollbacks.len(), 0);

        // Re-execute and verify again (will fail again → max retries → abandoned + rollback)
        svc.begin_subtask(&contract.task_id, "sub-1").await.unwrap();
        svc.complete_subtask_execution(&contract.task_id, "sub-1")
            .await
            .unwrap();
        let report2 = svc.verify_subtask(&contract.task_id, "sub-1").await.unwrap();
        assert!(!report2.all_required_passed);

        // Second failure: retry_count >= max_retries → Abandoned + rollback + cleanup
        let c = svc.get_contract(&contract.contract_id).await.unwrap().unwrap();
        assert!(matches!(c.subtasks[0].stage, SubtaskStage::Abandoned { .. }));

        let log = mock.log();
        assert!(log.rollbacks.len() >= 1, "should have rolled back");
        assert!(log.cleanups.len() >= 1, "should have cleaned up after rollback");
    }

    #[tokio::test]
    async fn git4data_diff_summary_in_subtask_serde() {
        let subtask = DurableSubtask {
            id: "s1".into(),
            title: "test".into(),
            diff_summary: Some(DiffSummary {
                snapshot: "snap_1".into(),
                changed_rows: 15,
            }),
            ..Default::default()
        };
        let json = serde_json::to_string(&subtask).unwrap();
        assert!(json.contains("diff_summary"));
        assert!(json.contains("changed_rows"));
        let parsed: DurableSubtask = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.diff_summary.unwrap().changed_rows, 15);
    }

    #[tokio::test]
    async fn noop_branch_ops_returns_empty() {
        let noop = NoopBranchOps;
        let name = noop.create_snapshot("t", "s", 1).await.unwrap();
        assert!(name.is_empty());
        let diff = noop.diff_since_snapshot("x").await.unwrap();
        assert_eq!(diff.changed_rows, 0);
        assert!(noop.rollback_to_snapshot("x").await.is_ok());
        assert!(noop.cleanup_snapshot("x").await.is_ok());
    }

    // ── LocalFileBranchOps Direct Tests ──

    #[tokio::test]
    async fn local_file_branch_ops_snapshot_and_diff() {
        let tmp = tempfile::TempDir::new().unwrap();
        let work = tmp.path().join("work");
        let snaps = tmp.path().join("snaps");
        std::fs::create_dir_all(&work).unwrap();

        // Create initial file
        std::fs::write(work.join("file1.txt"), "original").unwrap();

        let ops = LocalFileBranchOps::new(snaps.clone(), work.clone());

        // Create snapshot
        let name = ops.create_snapshot("t1", "s1", 1).await.unwrap();
        assert!(snaps.join(&name).exists());

        // No changes yet → diff should be 0
        let diff = ops.diff_since_snapshot(&name).await.unwrap();
        assert_eq!(diff.changed_rows, 0);

        // Modify file
        std::fs::write(work.join("file1.txt"), "modified").unwrap();
        let diff = ops.diff_since_snapshot(&name).await.unwrap();
        assert_eq!(diff.changed_rows, 1);

        // Add new file
        std::fs::write(work.join("file2.txt"), "new").unwrap();
        let diff = ops.diff_since_snapshot(&name).await.unwrap();
        assert_eq!(diff.changed_rows, 2); // 1 modified + 1 new
    }

    #[tokio::test]
    async fn local_file_branch_ops_rollback() {
        let tmp = tempfile::TempDir::new().unwrap();
        let work = tmp.path().join("work");
        let snaps = tmp.path().join("snaps");
        std::fs::create_dir_all(&work).unwrap();

        // Create initial state
        std::fs::write(work.join("file1.txt"), "original").unwrap();

        let ops = LocalFileBranchOps::new(snaps, work.clone());

        // Snapshot
        let name = ops.create_snapshot("t1", "s1", 1).await.unwrap();

        // Modify + add
        std::fs::write(work.join("file1.txt"), "damaged").unwrap();
        std::fs::write(work.join("extra.txt"), "extra").unwrap();

        // Rollback
        ops.rollback_to_snapshot(&name).await.unwrap();

        // Verify rollback restored original state
        let content = std::fs::read_to_string(work.join("file1.txt")).unwrap();
        assert_eq!(content, "original");
        assert!(!work.join("extra.txt").exists());
    }

    #[tokio::test]
    async fn local_file_branch_ops_cleanup() {
        let tmp = tempfile::TempDir::new().unwrap();
        let work = tmp.path().join("work");
        let snaps = tmp.path().join("snaps");
        std::fs::create_dir_all(&work).unwrap();

        let ops = LocalFileBranchOps::new(snaps.clone(), work);
        let name = ops.create_snapshot("t1", "s1", 1).await.unwrap();
        assert!(snaps.join(&name).exists());

        ops.cleanup_snapshot(&name).await.unwrap();
        assert!(!snaps.join(&name).exists());
    }

    #[tokio::test]
    async fn local_lifecycle_with_real_snapshots_full_flow() {
        let tmp = tempfile::TempDir::new().unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();

        // Initial file
        std::fs::write(work.join("data.txt"), "initial").unwrap();

        let svc = LocalDurableTaskLifecycle::new(
            tmp.path().join("data"),
            work.clone(),
        );

        let mut plan = make_test_plan();
        plan.subtasks.truncate(1);
        let contract = svc
            .create_contract("u", "s", "snapshot flow", &plan, TaskScope::default())
            .await
            .unwrap();

        // Begin → snapshot created
        let ctx = svc.begin_subtask(&contract.task_id, "sub-1").await.unwrap();
        assert!(ctx.snapshot_name.is_some());

        // Modify work dir during "execution"
        std::fs::write(work.join("data.txt"), "modified by agent").unwrap();
        std::fs::write(work.join("new_file.rs"), "fn main() {}").unwrap();

        // Complete → diff captured
        svc.complete_subtask_execution(&contract.task_id, "sub-1")
            .await
            .unwrap();

        let c = svc.get_contract(&contract.contract_id).await.unwrap().unwrap();
        let diff = c.subtasks[0].diff_summary.as_ref().unwrap();
        assert!(diff.changed_rows >= 1, "should detect changes: {:?}", diff);
    }

    // ── Security & State Guard Tests ──

    #[test]
    fn validate_snapshot_name_rejects_injection() {
        assert!(validate_snapshot_name("task_abc_sub1_v1").is_ok());
        assert!(validate_snapshot_name("task123").is_ok());
        // SQL injection attempts
        assert!(validate_snapshot_name("test'; DROP TABLE--").is_err());
        assert!(validate_snapshot_name("snap name with spaces").is_err());
        assert!(validate_snapshot_name("snap;DELETE").is_err());
        assert!(validate_snapshot_name("").is_err());
        assert!(validate_snapshot_name("snap/path").is_err());
        assert!(validate_snapshot_name("snap\x00null").is_err());
    }

    #[tokio::test]
    async fn verify_subtask_rejects_wrong_state() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mock = Arc::new(MockBranchOps::new());
        let svc = make_local_svc_with_mock(&tmp, mock);

        let plan = make_test_plan();
        let contract = svc
            .create_contract("u", "s", "test", &plan, TaskScope::default())
            .await
            .unwrap();

        // Try to verify a Pending subtask → should fail
        let err = svc
            .verify_subtask(&contract.task_id, "sub-1")
            .await
            .unwrap_err();
        assert!(
            err.contains("not ready for verification"),
            "unexpected error: {err}"
        );

        // Begin subtask (now Executing) → still not verifiable
        svc.begin_subtask(&contract.task_id, "sub-1").await.unwrap();
        let err = svc
            .verify_subtask(&contract.task_id, "sub-1")
            .await
            .unwrap_err();
        assert!(
            err.contains("not ready for verification"),
            "unexpected error: {err}"
        );
    }
}

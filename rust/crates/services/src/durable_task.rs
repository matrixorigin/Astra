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

use crate::event_ingestion::{IngestionEvent, IngestionSender};
use crate::task_orchestrator::{TaskCheckpoint, TaskPlan};

/// Build a reqwest client that skips the system proxy for localhost/loopback URLs.
fn build_client_for_url(url: &str) -> reqwest::Client {
    let is_local = url.contains("127.0.0.1")
        || url.contains("localhost")
        || url.contains("[::1]")
        || url.contains("0.0.0.0");
    let mut builder = reqwest::Client::builder();
    if is_local {
        builder = builder.no_proxy();
    }
    builder.build().unwrap_or_else(|_| reqwest::Client::new())
}

// ─── LLM Judge Trait ────────────────────────────────────────────────────────

/// Trait for LLM-based semantic verification.
///
/// Implementors call an LLM with the criterion prompt and return a confidence
/// score (0.0–1.0). The `VerificationRunner` compares the score against
/// `pass_threshold` to determine pass/fail.
///
/// This trait lives in `services` so that higher-level crates (`runtime`,
/// `astra`) can provide concrete implementations with access to LLM APIs.
#[async_trait]
pub trait LlmJudge: Send + Sync {
    /// Evaluate a verification criterion using an LLM.
    ///
    /// `prompt` — the evaluation question (from `VerifierKind::LlmJudge`)
    /// `context` — optional extra context (e.g. git diff, file contents)
    ///
    /// Returns `Ok(score)` where score is 0.0–1.0, or `Err(message)` on failure.
    async fn evaluate(&self, prompt: &str, context: &str) -> Result<f64, String>;
}

// ─── Cloud LLM Judge ────────────────────────────────────────────────────────

/// Cloud-side [`LlmJudge`] that evaluates criteria without consuming the edge
/// agent's context window.
///
/// Key differences from edge-side `HttpLlmJudge`:
/// - Persists evaluation results in the cloud `task_verification_results` table
/// - Can use a separate (cheaper/faster) model configured for cloud verification
/// - Results are immediately available for cross-session auditing
///
/// # Usage
///
/// ```rust,ignore
/// let judge = CloudLlmJudge::new(cloud_config, pool);
/// let runner = VerificationRunner::with_llm_judge(work_dir, Arc::new(judge));
/// // `verify_subtask()` (not _local) will now use the cloud LLM for LlmJudge criteria
/// ```
pub struct CloudLlmJudge {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
    model: String,
    pool: Option<sqlx::Pool<sqlx::MySql>>,
    /// Context for persistence (contract/session tracking).
    persist_context: std::sync::Mutex<CloudJudgePersistContext>,
}

/// Mutable context for result persistence, set before running verification.
#[derive(Default, Clone)]
pub struct CloudJudgePersistContext {
    pub contract_id: Option<String>,
    pub task_id: Option<String>,
    pub subtask_id: Option<String>,
    pub session_id: Option<String>,
}

/// Configuration for the cloud LLM judge.
#[derive(Debug, Clone)]
pub struct CloudLlmConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
}

impl CloudLlmConfig {
    /// Try to create from environment variables with `MO_CLOUD_LLM_` prefix.
    ///
    /// Falls back to `MO_LLM_` / `OPENAI_` prefixes if cloud-specific vars aren't set.
    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("MO_CLOUD_LLM_API_KEY")
            .or_else(|_| std::env::var("MO_LLM_API_KEY"))
            .or_else(|_| std::env::var("OPENAI_API_KEY"))
            .ok()?;
        let base_url = std::env::var("MO_CLOUD_LLM_BASE_URL")
            .or_else(|_| std::env::var("MO_LLM_BASE_URL"))
            .or_else(|_| std::env::var("OPENAI_BASE_URL"))
            .unwrap_or_else(|_| "https://api.openai.com/v1".into());
        let model = std::env::var("MO_CLOUD_LLM_MODEL")
            .or_else(|_| std::env::var("MO_LLM_MODEL"))
            .unwrap_or_else(|_| "gpt-4o-mini".into());
        Some(Self {
            api_key,
            base_url,
            model,
        })
    }
}

impl CloudLlmJudge {
    /// Create a cloud judge with optional database persistence.
    pub fn new(config: CloudLlmConfig, pool: Option<sqlx::Pool<sqlx::MySql>>) -> Self {
        let client = build_client_for_url(&config.base_url);
        Self {
            client,
            api_key: config.api_key,
            base_url: config.base_url,
            model: config.model,
            pool,
            persist_context: std::sync::Mutex::new(CloudJudgePersistContext::default()),
        }
    }

    /// Set the persistence context before running a batch of evaluations.
    pub fn set_persist_context(&self, ctx: CloudJudgePersistContext) {
        if let Ok(mut guard) = self.persist_context.lock() {
            *guard = ctx;
        }
    }

    /// Persist an evaluation result to the cloud database.
    async fn persist_result(
        &self,
        criterion_id: &str,
        passed: bool,
        score: f64,
        evidence: &str,
        duration_ms: u64,
        error: Option<&str>,
    ) {
        let pool = match &self.pool {
            Some(p) => p,
            None => return,
        };

        let ctx = self
            .persist_context
            .lock()
            .ok()
            .map(|g| g.clone())
            .unwrap_or_default();
        let contract_id = ctx.contract_id.as_deref().unwrap_or("unknown");
        let task_id = ctx.task_id.as_deref().unwrap_or("unknown");
        let subtask_id = ctx.subtask_id.as_deref().unwrap_or("unknown");
        let session_id = ctx.session_id.as_deref().unwrap_or("unknown");

        let result_id = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            criterion_id.hash(&mut h);
            session_id.hash(&mut h);
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
                .hash(&mut h);
            format!("cj-{:016x}", h.finish())
        };

        let evidence_with_score = format!("score={score:.2}; {evidence}");
        let _ = sqlx::query(
            "INSERT INTO task_verification_results \
             (result_id, contract_id, task_id, subtask_id, criterion_id, \
              session_id, passed, evidence, expected, duration_ms, error_message) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&result_id)
        .bind(contract_id)
        .bind(task_id)
        .bind(subtask_id)
        .bind(criterion_id)
        .bind(session_id)
        .bind(i16::from(passed))
        .bind(&evidence_with_score)
        .bind("cloud_llm_judge")
        .bind(duration_ms as i64)
        .bind(error)
        .execute(pool)
        .await;
    }

    /// Call the LLM API and parse the response score.
    async fn call_llm(&self, prompt: &str, context: &str) -> Result<f64, String> {
        let system_msg = serde_json::json!({
            "role": "system",
            "content": "You are a verification judge running on the cloud. Evaluate whether \
                        an acceptance criterion is met based on the provided context. \
                        Respond with ONLY a JSON object: \
                        {\"score\": <0.0-1.0>, \"reason\": \"<brief explanation>\"}. \
                        Score 1.0 = fully met, 0.0 = not met at all."
        });
        let user_msg = serde_json::json!({
            "role": "user",
            "content": format!(
                "Criterion: {prompt}\n\nContext:\n{context}\n\n\
                 Evaluate and respond with {{\"score\": <0.0-1.0>, \"reason\": \"...\"}}."
            )
        });

        let body = serde_json::json!({
            "model": self.model,
            "messages": [system_msg, user_msg],
            "max_tokens": 200,
            "temperature": 0.1,
        });

        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Cloud LLM request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!(
                "Cloud LLM API error {status}: {}",
                &text[..text.len().min(200)]
            ));
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("Cloud LLM response parse failed: {e}"))?;

        let content = json["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("");

        parse_judge_score(content)
    }
}

#[async_trait]
impl LlmJudge for CloudLlmJudge {
    async fn evaluate(&self, prompt: &str, context: &str) -> Result<f64, String> {
        let start = std::time::Instant::now();
        let result = self.call_llm(prompt, context).await;
        let duration_ms = start.elapsed().as_millis() as u64;

        match &result {
            Ok(score) => {
                let passed = *score >= 0.7; // default threshold for persistence
                self.persist_result(
                    &format!(
                        "llm-{}",
                        &prompt[..prompt.len().min(32)]
                            .replace(' ', "-")
                            .to_lowercase()
                    ),
                    passed,
                    *score,
                    &format!("Cloud LLM score: {score:.2}"),
                    duration_ms,
                    None,
                )
                .await;
            }
            Err(e) => {
                self.persist_result(
                    &format!(
                        "llm-{}",
                        &prompt[..prompt.len().min(32)]
                            .replace(' ', "-")
                            .to_lowercase()
                    ),
                    false,
                    0.0,
                    "Cloud LLM evaluation failed",
                    duration_ms,
                    Some(e),
                )
                .await;
            }
        }

        result
    }
}

/// Parse test output from common test frameworks and return (passed, failed).
///
/// Recognizes patterns from: Rust (cargo test), Python (pytest), Node (jest/mocha),
/// Go, Java (JUnit/Maven), and generic "N passed, M failed" output.
/// Returns `None` if no recognizable test summary is found.
fn parse_test_output(output: &str) -> Option<(u64, u64)> {
    // We scan lines in reverse because final summaries appear last.
    // Collect all lines so we can iterate from the end.
    let lines: Vec<&str> = output.lines().collect();

    for line in lines.iter().rev() {
        let line = line.trim();

        // Rust / cargo test: "test result: ok. 42 passed; 1 failed; 0 ignored"
        if line.starts_with("test result:") {
            let passed = extract_number_before(line, "passed");
            let failed = extract_number_before(line, "failed");
            if passed.is_some() || failed.is_some() {
                return Some((passed.unwrap_or(0), failed.unwrap_or(0)));
            }
        }

        // pytest: "10 passed, 2 failed" or "10 passed" or "2 failed"
        // Also handles "10 passed, 1 warning" etc.
        if (line.contains(" passed") || line.contains(" failed"))
            && (line.starts_with('=') || line.starts_with("FAILED") || line.contains("passed"))
        {
            let passed = extract_number_before(line, "passed");
            let failed = extract_number_before(line, "failed");
            if passed.is_some() || failed.is_some() {
                return Some((passed.unwrap_or(0), failed.unwrap_or(0)));
            }
        }

        // Jest / Vitest: "Tests:  2 failed, 10 passed, 12 total"
        if line.starts_with("Tests:") && line.contains("total") {
            let passed = extract_number_before(line, "passed");
            let failed = extract_number_before(line, "failed");
            if passed.is_some() || failed.is_some() {
                return Some((passed.unwrap_or(0), failed.unwrap_or(0)));
            }
        }

        // Go: "ok" or "FAIL" with "--- FAIL:" counted
        if line.starts_with("ok") && line.contains("\t") {
            // Go test passed — count FAIL lines in entire output
            let fail_count = output
                .lines()
                .filter(|l| l.trim_start().starts_with("--- FAIL:"))
                .count() as u64;
            let pass_count = output
                .lines()
                .filter(|l| l.trim_start().starts_with("--- PASS:"))
                .count() as u64;
            // If we found specific pass/fail markers, use them
            if pass_count > 0 || fail_count > 0 {
                return Some((pass_count, fail_count));
            }
        }

        // JUnit / Maven: "Tests run: 10, Failures: 2, Errors: 1, Skipped: 0"
        if line.contains("Tests run:") && line.contains("Failures:") {
            let total = extract_number_after(line, "Tests run:");
            let failures = extract_number_after(line, "Failures:");
            let errors = extract_number_after(line, "Errors:");
            if let Some(t) = total {
                let f = failures.unwrap_or(0) + errors.unwrap_or(0);
                return Some((t.saturating_sub(f), f));
            }
        }

        // Generic: "N tests passed, M tests failed" or "N passing, M failing"
        // (mocha/jasmine: these can be on separate lines, so we handle per-line)
        if line.contains("passing")
            && let Some(p) = extract_number_before(line, "passing")
        {
            let failed = lines.iter().rev().find_map(|l| {
                if l.contains("failing") {
                    extract_number_before(l.trim(), "failing")
                } else {
                    None
                }
            });
            return Some((p, failed.unwrap_or(0)));
        }
        if line.contains("failing")
            && let Some(f) = extract_number_before(line, "failing")
        {
            let passed = lines.iter().rev().find_map(|l| {
                if l.contains("passing") {
                    extract_number_before(l.trim(), "passing")
                } else {
                    None
                }
            });
            return Some((passed.unwrap_or(0), f));
        }
    }

    None
}

/// Extract the number immediately before a keyword, e.g. "42 passed" → 42
fn extract_number_before(line: &str, keyword: &str) -> Option<u64> {
    let idx = line.find(keyword)?;
    let before = line[..idx].trim();
    // Take the last whitespace-separated token before the keyword
    let num_str = before
        .rsplit_once(|c: char| !c.is_ascii_digit())
        .map_or(before, |(_, n)| n);
    num_str.parse().ok()
}

/// Extract the number immediately after a keyword (with optional colon/space),
/// e.g. "Tests run: 10" → 10
fn extract_number_after(line: &str, keyword: &str) -> Option<u64> {
    let idx = line.find(keyword)?;
    let after = line[idx + keyword.len()..].trim_start_matches([' ', ':']);
    let num_str = after
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .unwrap_or("");
    num_str.parse().ok()
}

/// Parse a score from LLM response text.
///
/// Tries (in order): direct JSON parse → embedded JSON in markdown → fallback
/// decimal number extraction. Scores are clamped to `[0.0, 1.0]`.
fn parse_judge_score(text: &str) -> Result<f64, String> {
    // Try JSON parse first
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(text)
        && let Some(score) = v["score"].as_f64()
    {
        return Ok(score.clamp(0.0, 1.0));
    }

    // Try to find JSON embedded in text (e.g., wrapped with markdown)
    if let Some(start) = text.find('{')
        && let Some(end) = text[start..].rfind('}')
    {
        let json_str = &text[start..=start + end];
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(json_str)
            && let Some(score) = v["score"].as_f64()
        {
            return Ok(score.clamp(0.0, 1.0));
        }
    }

    // Fallback: find any decimal number between 0 and 1
    for word in text.split_whitespace() {
        let clean = word.trim_matches(|c: char| !c.is_ascii_digit() && c != '.');
        if let Ok(n) = clean.parse::<f64>()
            && (0.0..=1.0).contains(&n)
        {
            return Ok(n);
        }
    }

    Err(format!(
        "Could not extract score from LLM response: {}",
        &text[..text.len().min(200)]
    ))
}

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
    /// Routing domain hint (e.g. "database", "web") — set during plan generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain_hint: Option<String>,
    /// Routing task type (e.g. "code_generation", "debugging") — set during plan generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_type: Option<String>,
    /// Results of the most recent global verification run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub last_global_results: Vec<VerificationResult>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskScope {
    pub in_scope: Vec<String>,
    pub out_of_scope: Vec<String>,
    pub assumptions: Vec<String>,
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
    /// Last verification result (populated after verify_subtask runs)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_verification: Option<SubtaskVerificationReport>,
    /// Tools used during this subtask's execution (populated by REPL)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools_used: Vec<String>,
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
            last_verification: None,
            tools_used: Vec::new(),
        }
    }
}

// ─── Subtask State Machine ──────────────────────────────────────────────────

/// Full lifecycle state for a durable subtask.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SubtaskStage {
    #[default]
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
// Types live in crate::verification; re-export for backward compatibility.
pub use crate::verification::{
    SubtaskVerificationReport, VerificationCriterion, VerificationResult, VerifierKind,
};

// ─── Verification Runner ────────────────────────────────────────────────────

/// Callback type for streaming command output lines to the caller (e.g. terminal).
pub type OutputSink = Arc<dyn Fn(&str) + Send + Sync>;

/// Executes verification criteria (edge-side: commands, files, grep, build/test).
/// When an `LlmJudge` implementation is provided, also handles semantic verification.
/// When an `output_sink` is provided, streams command stderr/stdout lines live.
pub struct VerificationRunner {
    pub work_dir: std::path::PathBuf,
    pub llm_judge: Option<Arc<dyn LlmJudge>>,
    pub output_sink: Option<OutputSink>,
}

impl VerificationRunner {
    pub fn new(work_dir: std::path::PathBuf) -> Self {
        Self {
            work_dir,
            llm_judge: None,
            output_sink: None,
        }
    }

    /// Create a runner with LLM judge support for semantic verification.
    pub fn with_llm_judge(work_dir: std::path::PathBuf, judge: Arc<dyn LlmJudge>) -> Self {
        Self {
            work_dir,
            llm_judge: Some(judge),
            output_sink: None,
        }
    }

    /// Set an output sink that receives live command output lines.
    pub fn with_output_sink(mut self, sink: OutputSink) -> Self {
        self.output_sink = Some(sink);
        self
    }

    /// Verify all criteria for a subtask.
    pub async fn verify_subtask(&self, subtask: &DurableSubtask) -> SubtaskVerificationReport {
        self.verify_subtask_filtered(subtask, false).await
    }

    /// Verify only lightweight (non-global-only) criteria for a subtask.
    /// Skips `global_only` criteria and `LlmJudge` (not yet implemented).
    /// Used during per-subtask verification in the REPL loop for fast feedback.
    pub async fn verify_subtask_local(
        &self,
        subtask: &DurableSubtask,
    ) -> SubtaskVerificationReport {
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

        let result =
            tokio::time::timeout(timeout, self.execute_verifier(&criterion.verifier)).await;

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
        let sink = &self.output_sink;
        match verifier {
            VerifierKind::Command { cmd, expected_exit } => {
                let cmd = cmd.clone();
                let expected = *expected_exit;
                let dir = self.work_dir.clone();
                let (code, stdout, stderr) = run_shell_cmd(&cmd, &dir, sink).await?;
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
                let (_code, stdout, _stderr) = run_shell_cmd(&cmd, &dir, sink).await?;
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
                let content =
                    std::fs::read_to_string(&full).map_err(|e| format!("read {file}: {e}"))?;
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

            VerifierKind::ReadFileContains {
                path,
                contains,
                not_contains,
            } => {
                let full = self.work_dir.join(path);
                // Security: prevent path traversal (e.g. "../../../etc/passwd")
                let canonical = full
                    .canonicalize()
                    .map_err(|e| format!("invalid path {path}: {e}"))?;
                let work_canonical = self
                    .work_dir
                    .canonicalize()
                    .map_err(|e| format!("work_dir canonicalization failed: {e}"))?;
                if !canonical.starts_with(&work_canonical) {
                    return Err(format!(
                        "path '{path}' escapes work directory boundary"
                    ));
                }
                let content = std::fs::read_to_string(&canonical)
                    .map_err(|e| format!("read {path}: {e}"))?;
                let has_all = contains.iter().all(|s| content.contains(s));
                let has_none = not_contains.iter().all(|s| !content.contains(s));
                let passed = has_all && has_none;
                let evidence = if passed {
                    format!("file {path}: all checks passed")
                } else {
                    let mut parts = Vec::new();
                    for s in contains {
                        if !content.contains(s) {
                            parts.push(format!("missing: \"{s}\""));
                        }
                    }
                    for s in not_contains {
                        if content.contains(s) {
                            parts.push(format!("unwanted: \"{s}\""));
                        }
                    }
                    format!("file {path}: {}", parts.join(", "))
                };
                Ok((
                    passed,
                    evidence,
                    format!("file {path} contains: {:?}, not_contains: {:?}", contains, not_contains),
                ))
            }

            VerifierKind::BuildPass { cmd } => {
                let cmd = cmd.clone();
                let dir = self.work_dir.clone();
                let (code, _stdout, stderr) = run_shell_cmd(&cmd, &dir, sink).await?;
                Ok((code == 0, truncate(&stderr, 4096), "exit code == 0".into()))
            }

            VerifierKind::TestPass { cmd, min_pass_rate } => {
                let cmd = cmd.clone();
                let dir = self.work_dir.clone();
                let (code, stdout, stderr) = run_shell_cmd(&cmd, &dir, sink).await?;
                let combined = format!("{stdout}\n{stderr}");

                // Try to parse structured test output for actual pass rate
                let (passed, evidence) = if let Some((p, f)) = parse_test_output(&combined) {
                    let total = p + f;
                    let rate = if total > 0 {
                        p as f64 / total as f64
                    } else {
                        0.0
                    };
                    let meets_threshold = rate >= *min_pass_rate;
                    let detail = format!(
                        "{p} passed, {f} failed ({:.0}% pass rate, threshold {:.0}%)\n{}",
                        rate * 100.0,
                        min_pass_rate * 100.0,
                        truncate(&combined, 3800),
                    );
                    (meets_threshold, detail)
                } else {
                    // Fallback: if we can't parse output, use exit code
                    let detail = format!(
                        "exit code {code} (could not parse test counts)\n{}",
                        truncate(&combined, 3900),
                    );
                    (code == 0, detail)
                };

                Ok((
                    passed,
                    truncate(&evidence, 4096),
                    format!("pass rate >= {:.0}%", min_pass_rate * 100.0),
                ))
            }

            VerifierKind::LlmJudge {
                prompt,
                pass_threshold,
            } => {
                if let Some(judge) = &self.llm_judge {
                    let context = self.build_judge_context(prompt).await;
                    match judge.evaluate(prompt, &context).await {
                        Ok(score) => {
                            let passed = score >= *pass_threshold;
                            Ok((
                                passed,
                                format!("LLM score: {score:.2} (threshold: {pass_threshold:.2})"),
                                format!("LLM evaluation: {}", truncate(prompt, 200)),
                            ))
                        }
                        Err(e) => Ok((
                            false,
                            format!("LLM judge error: {e}"),
                            format!("LLM evaluation: {}", truncate(prompt, 200)),
                        )),
                    }
                } else {
                    // No LLM judge available — skip with informative message
                    Ok((
                        false,
                        "LLM judge not available (no provider configured)".into(),
                        format!("LLM evaluation: {}", truncate(prompt, 200)),
                    ))
                }
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
                Ok((
                    passed,
                    evidence,
                    format!("{logic} of {} criteria", criteria.len()),
                ))
            }
        }
    }

    /// Build rich context for LLM judge evaluation.
    ///
    /// Gathers: git diff (recent changes), relevant file snippets, directory listing.
    /// Capped at ~8KB to stay within token budgets.
    async fn build_judge_context(&self, prompt: &str) -> String {
        let mut parts = Vec::new();
        let dir = self.work_dir.clone();

        parts.push(format!("Work directory: {}", dir.display()));

        // 1. Git diff (uncommitted changes) — most relevant for "did the code change correctly?"
        if let Ok(diff) = Self::git_diff(&dir).await
            && !diff.is_empty()
        {
            parts.push(format!(
                "## Recent changes (git diff):\n```\n{}\n```",
                truncate(&diff, 4096)
            ));
        }

        // 2. Extract file paths mentioned in the prompt and include snippets
        let mentioned_files = extract_paths_from_text(prompt);
        for path_str in mentioned_files.iter().take(3) {
            let full_path = dir.join(path_str);
            if full_path.exists()
                && full_path.is_file()
                && let Ok(content) = std::fs::read_to_string(&full_path)
            {
                parts.push(format!(
                    "## File: {path_str}\n```\n{}\n```",
                    truncate(&content, 2048)
                ));
            }
        }

        // 3. Directory listing (top-level) for orientation
        if let Ok(entries) = std::fs::read_dir(&dir) {
            let listing: Vec<String> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .take(30)
                .collect();
            if !listing.is_empty() {
                parts.push(format!("## Directory contents:\n{}", listing.join(", ")));
            }
        }

        parts.join("\n\n")
    }

    /// Get git diff for uncommitted changes.
    async fn git_diff(dir: &std::path::Path) -> Result<String, String> {
        let dir = dir.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let output = std::process::Command::new("git")
                .args(["diff", "--stat", "--no-color", "HEAD"])
                .current_dir(&dir)
                .output()
                .map_err(|e| format!("git diff failed: {e}"))?;
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        })
        .await
        .map_err(|e| format!("spawn: {e}"))?
    }
}

/// Extract file paths from text (heuristic: words with '/' or known extensions).
fn extract_paths_from_text(text: &str) -> Vec<String> {
    let extensions = [
        ".rs", ".py", ".ts", ".tsx", ".js", ".jsx", ".go", ".java", ".rb", ".cpp", ".c", ".h",
        ".toml", ".yaml", ".yml", ".json", ".sql", ".sh",
    ];
    text.split_whitespace()
        .map(|w| w.trim_matches(|c: char| c == '`' || c == '\'' || c == '"' || c == ','))
        .filter(|w| w.contains('/') || extensions.iter().any(|ext| w.ends_with(ext)))
        .map(String::from)
        .collect()
}

/// Run a shell command, optionally streaming output lines to a sink.
///
/// When `sink` is provided, uses `tokio::process::Command` to stream stderr
/// lines live (for build/test feedback). Otherwise, falls back to the
/// blocking `std::process::Command::output()` path.
async fn run_shell_cmd(
    cmd: &str,
    dir: &std::path::Path,
    sink: &Option<OutputSink>,
) -> Result<(i32, String, String), String> {
    if let Some(sink) = sink {
        run_shell_cmd_streaming(cmd, dir, sink).await
    } else {
        run_shell_cmd_buffered(cmd, dir).await
    }
}

/// Streaming variant: pipes stderr line-by-line to the sink while accumulating output.
async fn run_shell_cmd_streaming(
    cmd: &str,
    dir: &std::path::Path,
    sink: &OutputSink,
) -> Result<(i32, String, String), String> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::process::Command;

    let mut child = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn failed: {e}"))?;

    let stderr_pipe = child.stderr.take();
    let stdout_pipe = child.stdout.take();

    // Stream stderr lines to the sink while accumulating
    let sink2 = sink.clone();
    let stderr_handle = tokio::spawn(async move {
        let mut acc = String::new();
        if let Some(pipe) = stderr_pipe {
            let mut lines = BufReader::new(pipe).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                sink2(&line);
                acc.push_str(&line);
                acc.push('\n');
            }
        }
        acc
    });

    // Also stream stdout lines
    let sink3 = sink.clone();
    let stdout_handle = tokio::spawn(async move {
        let mut acc = String::new();
        if let Some(pipe) = stdout_pipe {
            let mut lines = BufReader::new(pipe).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                sink3(&line);
                acc.push_str(&line);
                acc.push('\n');
            }
        }
        acc
    });

    let status = child
        .wait()
        .await
        .map_err(|e| format!("wait failed: {e}"))?;

    let stderr = stderr_handle
        .await
        .map_err(|e| format!("stderr join: {e}"))?;
    let stdout = stdout_handle
        .await
        .map_err(|e| format!("stdout join: {e}"))?;

    Ok((status.code().unwrap_or(-1), stdout, stderr))
}

/// Original blocking variant (no streaming).
async fn run_shell_cmd_buffered(
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
    async fn create_snapshot(
        &self,
        task_id: &str,
        subtask_id: &str,
        version: u32,
    ) -> Result<String, String>;

    /// Diff agent's work against a pre-execution snapshot.
    async fn diff_since_snapshot(&self, snapshot: &str) -> Result<DiffSummary, String>;

    /// Rollback to a pre-execution snapshot.
    async fn rollback_to_snapshot(&self, snapshot: &str) -> Result<(), String>;

    /// Clean up a snapshot after successful verification.
    async fn cleanup_snapshot(&self, snapshot: &str) -> Result<(), String>;
}

/// Production implementation: MatrixOne git4data snapshots.
///
/// Uses database-level snapshots for fine-grained rollback:
///   CREATE SNAPSHOT name FOR DATABASE db;
///   RESTORE ACCOUNT acc DATABASE db FROM SNAPSHOT name;
///   DROP SNAPSHOT IF EXISTS name;
pub struct TaskBranchService {
    pool: sqlx::Pool<sqlx::MySql>,
    /// Database to snapshot.
    database: String,
}

impl TaskBranchService {
    pub fn new(
        pool: sqlx::Pool<sqlx::MySql>,
        database: impl Into<String>,
    ) -> Self {
        Self {
            pool,
            database: database.into(),
        }
    }
}

/// Validate a snapshot name is safe for SQL embedding (alphanumeric + underscore only).
/// Sanitize a snapshot name to only contain `[a-zA-Z0-9_]`.
/// Replaces any other character (notably `-`) with `_`.
fn sanitize_snapshot_name(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn validate_snapshot_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("empty snapshot name".into());
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
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
        let name = sanitize_snapshot_name(&format!("task_{task_id}_{subtask_id}_v{version}"));
        validate_snapshot_name(&name)?;
        let sql = crate::snapshot_sql::create_snapshot_for_db_sql(&name, &self.database);
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
        let account = crate::snapshot_sql::resolve_account_name(&self.pool).await?;
        let sql = crate::snapshot_sql::restore_snapshot_db_sql(snapshot, &account, &self.database);
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

/// Git-based branch ops that use `git stash` for near-instant snapshots.
/// Falls back to NoopBranchOps semantics on non-git repos.
pub struct GitBranchOps {
    work_dir: std::path::PathBuf,
    /// Maps snapshot_name → git stash ref (commit SHA)
    refs: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

impl GitBranchOps {
    pub fn new(work_dir: std::path::PathBuf) -> Self {
        Self {
            work_dir,
            refs: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Check if the work_dir is inside a git repository with at least one commit.
    pub fn is_git_repo(work_dir: &std::path::Path) -> bool {
        // Verify HEAD exists (repo has at least one commit) — prevents false positives
        // from bare/empty repos (e.g. stray `.git` dirs in /tmp).
        std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(work_dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

#[async_trait]
impl TaskBranchOps for GitBranchOps {
    async fn create_snapshot(
        &self,
        task_id: &str,
        subtask_id: &str,
        version: u32,
    ) -> Result<String, String> {
        let name = sanitize_snapshot_name(&format!("task_{task_id}_{subtask_id}_v{version}"));
        let work = self.work_dir.clone();

        // Record current HEAD as the snapshot reference point.
        // This is instant — no file copying.
        let sha = tokio::task::spawn_blocking(move || {
            let output = std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&work)
                .output()
                .map_err(|e| format!("git rev-parse: {e}"))?;
            if output.status.success() {
                Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
            } else {
                Err("git rev-parse HEAD failed".to_string())
            }
        })
        .await
        .map_err(|e| format!("spawn: {e}"))??;

        self.refs.lock().unwrap().insert(name.clone(), sha);
        Ok(name)
    }

    async fn diff_since_snapshot(&self, snapshot: &str) -> Result<DiffSummary, String> {
        let sha = self
            .refs
            .lock()
            .unwrap()
            .get(snapshot)
            .cloned()
            .unwrap_or_default();
        if sha.is_empty() {
            return Ok(DiffSummary {
                snapshot: snapshot.to_string(),
                changed_rows: 0,
            });
        }
        let work = self.work_dir.clone();
        let sha_clone = sha.clone();
        let changed = tokio::task::spawn_blocking(move || {
            // Count changed files since snapshot ref (staged + unstaged + untracked)
            let output = std::process::Command::new("git")
                .args(["diff", "--name-only", &sha_clone])
                .current_dir(&work)
                .output()
                .map_err(|e| format!("git diff: {e}"))?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            let count = stdout.lines().filter(|l| !l.is_empty()).count() as i64;
            Ok::<i64, String>(count)
        })
        .await
        .map_err(|e| format!("spawn: {e}"))??;

        Ok(DiffSummary {
            snapshot: snapshot.to_string(),
            changed_rows: changed,
        })
    }

    async fn rollback_to_snapshot(&self, snapshot: &str) -> Result<(), String> {
        let sha = self
            .refs
            .lock()
            .unwrap()
            .get(snapshot)
            .cloned()
            .ok_or_else(|| format!("snapshot '{snapshot}' not found"))?;
        let work = self.work_dir.clone();
        tokio::task::spawn_blocking(move || {
            // Hard reset to the snapshot commit
            let output = std::process::Command::new("git")
                .args(["reset", "--hard", &sha])
                .current_dir(&work)
                .output()
                .map_err(|e| format!("git reset: {e}"))?;
            if output.status.success() {
                Ok(())
            } else {
                Err(format!(
                    "git reset --hard: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ))
            }
        })
        .await
        .map_err(|e| format!("spawn: {e}"))?
    }

    async fn cleanup_snapshot(&self, snapshot: &str) -> Result<(), String> {
        // Just remove from in-memory map — no disk I/O needed
        self.refs.lock().unwrap().remove(snapshot);
        Ok(())
    }
}

/// File-based branch ops for local/offline mode (non-git repos).
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
        let name = sanitize_snapshot_name(&format!("task_{task_id}_{subtask_id}_v{version}"));
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
        // Returns empty name to signal "no snapshot" — callers handle this gracefully.
        // Intentionally silent: NoopBranchOps is used when no DB backend is configured.
        Ok(String::new())
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

/// Directories to skip during snapshot copy / diff — these are either internal
/// state, VCS data, or build artifacts that should never be part of a snapshot.
const SNAPSHOT_EXCLUDED_DIRS: &[&str] = &[
    ".mo-session",
    ".git",
    "target",
    "node_modules",
    "__pycache__",
    ".venv",
    "dist",
    "build",
    ".tox",
];

fn is_snapshot_excluded(name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .is_some_and(|n| SNAPSHOT_EXCLUDED_DIRS.contains(&n))
}

fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| format!("mkdir {}: {e}", dst.display()))?;
    if !src.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(src).map_err(|e| format!("readdir {}: {e}", src.display()))? {
        let entry = entry.map_err(|e| format!("entry: {e}"))?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            if is_snapshot_excluded(&entry.file_name()) {
                continue;
            }
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)
                .map_err(|e| format!("copy {}: {e}", src_path.display()))?;
        }
    }
    Ok(())
}

fn count_changed_files(snap: &std::path::Path, work: &std::path::Path) -> Result<i64, String> {
    if !snap.exists() || !work.exists() {
        return Ok(0);
    }
    let mut changed = 0i64;
    for entry in std::fs::read_dir(work).map_err(|e| format!("readdir {}: {e}", work.display()))? {
        let entry = entry.map_err(|e| format!("entry: {e}"))?;
        let work_path = entry.path();
        let snap_path = snap.join(entry.file_name());
        if work_path.is_dir() {
            if is_snapshot_excluded(&entry.file_name()) {
                continue;
            }
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

    /// Record a real-time verification result for incremental learning.
    ///
    /// Called after each subtask verification (not just at delivery). Allows the
    /// learning system to detect failure patterns early and adjust tool routing
    /// before the task completes.
    ///
    /// Default: no-op (override in real implementations).
    async fn learn_from_verification(
        &self,
        _signal: &VerificationLearningSignal,
    ) -> Result<(), String> {
        Ok(())
    }

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

/// Signal emitted after each subtask verification for incremental learning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationLearningSignal {
    pub task_id: String,
    pub subtask_id: String,
    pub subtask_title: String,
    pub goal: String,
    /// True if all required criteria passed.
    pub all_passed: bool,
    /// Number of criteria that passed / total criteria.
    pub pass_rate: f64,
    /// Which attempt this was (1-based).
    pub attempt: u32,
    /// Individual criterion results for fine-grained pattern learning.
    pub criteria_results: Vec<CriterionLearningResult>,
    /// Files involved in the subtask (for entity extraction).
    pub files: Vec<String>,
    /// Domain hint from the task contract (e.g. "github", "code").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain_hint: Option<String>,
    /// Task type from the task contract (e.g. "code", "fetch").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_type: Option<String>,
}

/// Per-criterion result for learning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CriterionLearningResult {
    pub criterion_id: String,
    pub verifier_kind: String,
    pub passed: bool,
    pub duration_ms: u64,
}

/// Historical performance stats for a task pattern.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskPatternStats {
    pub pattern: String,
    pub total_attempts: u32,
    pub success_rate: f64,
    /// Average retries per task. Currently 0.0 — requires per-task retry tracking
    /// in the pattern library (future: record from TaskOutcomeSignal.total_retries).
    pub avg_retries: f64,
    /// Average turns per task. Currently 0.0 — requires per-task turn tracking
    /// in the pattern library (future: record from TaskOutcomeSignal.total_turns).
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
    async fn task_pattern_stats(&self, _pattern: &str) -> Result<Option<TaskPatternStats>, String> {
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
            let summary = report.subtask_summaries.iter().find(|sum| sum.id == s.id);
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
                tools_used: s.tools_used.clone(),
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
    llm_judge: Option<Arc<dyn LlmJudge>>,
    event_sender: Option<IngestionSender>,
    /// Session ID for event attribution.
    session_id: String,
    /// User ID for event attribution.
    user_id: String,
    /// Learning bridge for feeding verification patterns into the learning system.
    learning_bridge: Option<Arc<dyn TaskLearningBridge>>,
    /// Optional callback that receives live command output lines during verification.
    output_sink: Option<OutputSink>,
}

impl MatrixOneDurableTaskLifecycle {
    pub fn new(pool: sqlx::Pool<sqlx::MySql>, work_dir: std::path::PathBuf) -> Self {
        // Default: database-level snapshot for the configured database.
        let database = std::env::var("MATRIXONE_DATABASE").unwrap_or_else(|_| "astra_runtime".into());
        let branch_ops: Arc<dyn TaskBranchOps> =
            Arc::new(TaskBranchService::new(pool.clone(), database));
        Self {
            pool,
            branch_ops,
            work_dir,
            llm_judge: None,
            event_sender: None,
            session_id: String::new(),
            user_id: String::new(),
            learning_bridge: None,
            output_sink: None,
        }
    }

    /// Create with database-level snapshots for finer granularity.
    pub fn with_database(
        pool: sqlx::Pool<sqlx::MySql>,
        work_dir: std::path::PathBuf,
        database: impl Into<String>,
    ) -> Self {
        let database = database.into();
        let branch_ops: Arc<dyn TaskBranchOps> = Arc::new(TaskBranchService::new(
            pool.clone(),
            database,
        ));
        Self {
            pool,
            branch_ops,
            work_dir,
            llm_judge: None,
            event_sender: None,
            session_id: String::new(),
            user_id: String::new(),
            learning_bridge: None,
            output_sink: None,
        }
    }

    pub fn from_shared(shared: &astra_core::SharedPool, work_dir: std::path::PathBuf) -> Self {
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
            llm_judge: None,
            event_sender: None,
            session_id: String::new(),
            user_id: String::new(),
            learning_bridge: None,
            output_sink: None,
        }
    }

    /// Set the LLM judge for semantic verification.
    pub fn set_llm_judge(&mut self, judge: Arc<dyn LlmJudge>) {
        self.llm_judge = Some(judge);
    }

    /// Set the event sender for pushing verification events to the cloud stream.
    pub fn set_event_sender(&mut self, sender: IngestionSender) {
        self.event_sender = Some(sender);
    }

    /// Set session context for event attribution.
    pub fn set_session_context(&mut self, session_id: &str, user_id: &str) {
        self.session_id = session_id.to_string();
        self.user_id = user_id.to_string();
    }

    /// Set the learning bridge for feeding verification patterns into the learning system.
    pub fn set_learning_bridge(&mut self, bridge: Arc<dyn TaskLearningBridge>) {
        self.learning_bridge = Some(bridge);
    }

    /// Set the output sink for live command output during verification.
    pub fn set_output_sink(&mut self, sink: OutputSink) {
        self.output_sink = Some(sink);
    }

    fn runner(&self) -> VerificationRunner {
        let mut runner = match &self.llm_judge {
            Some(j) => VerificationRunner::with_llm_judge(self.work_dir.clone(), j.clone()),
            None => VerificationRunner::new(self.work_dir.clone()),
        };
        if let Some(ref sink) = self.output_sink {
            runner.output_sink = Some(sink.clone());
        }
        runner
    }

    /// Emit a verification-related event to the cloud event stream.
    /// No-op if event_sender is not configured.
    fn emit_event(&self, event_type: &str, metadata: serde_json::Value) {
        let Some(sender) = &self.event_sender else {
            return;
        };
        let event_id = {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut hasher = DefaultHasher::new();
            self.session_id.hash(&mut hasher);
            event_type.hash(&mut hasher);
            chrono::Utc::now().timestamp_nanos_opt().hash(&mut hasher);
            format!("vfy-{:016x}", hasher.finish())
        };
        let content = metadata
            .get("subtask_id")
            .and_then(|v| v.as_str())
            .map(|id| format!("{event_type}: {id}"))
            .unwrap_or_else(|| event_type.to_string());
        sender.enqueue(IngestionEvent {
            event_id,
            session_id: self.session_id.clone(),
            user_id: self.user_id.clone(),
            event_type: event_type.to_string(),
            content: Some(content),
            token_usage: None,
            llm_model_used: None,
            skill_name: None,
            metadata: Some(metadata),
            created_at: chrono::Utc::now().to_rfc3339(),
            parent_event_id: None,
            causal_chain_id: None,
        });
    }

    // ── Private Helpers ──

    async fn load_contract_by_id(&self, contract_id: &str) -> Result<Option<TaskContract>, String> {
        let row = sqlx::query(
            "SELECT contract_id, task_id, user_id, session_id, goal, \
             CAST(scope_json AS CHAR) AS scope_json, \
             CAST(subtasks_json AS CHAR) AS subtasks_json, \
             CAST(criteria_json AS CHAR) AS criteria_json, \
             version, status, \
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
             CAST(scope_json AS CHAR) AS scope_json, \
             CAST(subtasks_json AS CHAR) AS subtasks_json, \
             CAST(criteria_json AS CHAR) AS criteria_json, \
             version, status, \
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
            domain_hint: None,
            task_type: None,
            last_global_results: Vec::new(),
        })
    }

    async fn persist_contract(&self, contract: &TaskContract) -> Result<(), String> {
        let scope_json =
            serde_json::to_string(&contract.scope).map_err(|e| format!("scope json: {e}"))?;
        let subtasks_json =
            serde_json::to_string(&contract.subtasks).map_err(|e| format!("subtasks json: {e}"))?;
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
        .bind("") // session_id filled by caller context
        .bind("") // user_id filled by caller context
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
        let subtasks_json =
            serde_json::to_string(&contract.subtasks).map_err(|e| format!("subtasks json: {e}"))?;
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
            .map(|sp| {
                let criteria = crate::contract_generator::acceptance_checks_to_criteria(
                    &sp.id,
                    &sp.acceptance_checks,
                    &sp.files,
                );
                DurableSubtask {
                    id: sp.id.clone(),
                    title: sp.title.clone(),
                    description: sp.description.clone(),
                    depends_on: sp.depends_on.clone(),
                    effort: sp.effort.clone(),
                    files: sp.files.clone(),
                    stage: SubtaskStage::Pending,
                    criteria,
                    max_retries: 2,
                    retry_count: 0,
                    snapshot_name: None,
                    data_branch: None,
                    diff_summary: None,
                    last_verification: None,
                    tools_used: Vec::new(),
                }
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
            domain_hint: None,
            task_type: None,
            last_global_results: Vec::new(),
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

        self.emit_event(
            "subtask_started",
            serde_json::json!({
                "task_id": task_id,
                "subtask_id": subtask_id,
                "title": ctx.title,
                "criteria_count": ctx.criteria.len(),
            }),
        );

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

        self.emit_event(
            "verification_started",
            serde_json::json!({
                "task_id": task_id,
                "subtask_id": subtask_id,
                "criteria_count": durable_st.criteria.len(),
                "attempt": durable_st.retry_count + 1,
            }),
        );

        // Run verification
        let runner = self.runner();
        let report = runner.verify_subtask(&durable_st).await;

        // Persist verification results + contract update atomically
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| format!("begin tx: {e}"))?;

        for r in &report.results {
            let result_id = uuid::Uuid::new_v4().to_string();
            sqlx::query(
                "INSERT INTO task_verification_results \
                 (result_id, contract_id, task_id, subtask_id, criterion_id, \
                  session_id, passed, evidence, expected, duration_ms, error_message, attempt) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&result_id)
            .bind(&contract.contract_id)
            .bind(task_id)
            .bind(subtask_id)
            .bind(&r.criterion_id)
            .bind(&self.session_id)
            .bind(if r.passed { 1i32 } else { 0i32 })
            .bind(&r.evidence)
            .bind(&r.expected)
            .bind(r.duration_ms as i64)
            .bind(&r.error)
            .bind((durable_st.retry_count + 1) as i32)
            .execute(&mut *tx)
            .await
            .map_err(|e| format!("save_verification in tx: {e}"))?;
        }

        // Update stage + git4data actions
        let snapshot_name = durable_st.snapshot_name.clone();
        let subtask = Self::find_subtask_mut(&mut contract, subtask_id)?;
        // Store verification results for delivery report
        subtask.last_verification = Some(report.clone());
        if report.all_required_passed {
            subtask.stage = SubtaskStage::Verified;
            // Git4Data: cleanup snapshot after successful verification
            if let Some(snap) = &snapshot_name
                && let Err(e) = self.branch_ops.cleanup_snapshot(snap).await
            {
                eprintln!("warn: snapshot cleanup failed for {subtask_id}: {e}");
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
                    reason: format!("verification failed after {} attempts", subtask.retry_count),
                };
            } else {
                subtask.stage = SubtaskStage::VerificationFailed {
                    results: report.results.clone(),
                };
            }
        }

        // Persist contract update inside the same transaction
        {
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
            .bind("")
            .bind("")
            .bind(&contract.goal)
            .bind(&scope_json)
            .bind(&subtasks_json)
            .bind(&criteria_json)
            .bind(contract.version as i32)
            .bind(contract.status.as_str())
            .execute(&mut *tx)
            .await
            .map_err(|e| format!("persist_contract in tx: {e}"))?;
        }

        tx.commit()
            .await
            .map_err(|e| format!("commit verify tx: {e}"))?;

        // Emit verification result event
        let passed_count = report.results.iter().filter(|r| r.passed).count();
        let total_count = report.results.len();
        let event_type = if report.all_required_passed {
            "verification_passed"
        } else {
            "verification_failed"
        };
        self.emit_event(
            event_type,
            serde_json::json!({
                "task_id": task_id,
                "subtask_id": subtask_id,
                "passed": passed_count,
                "total": total_count,
                "all_required_passed": report.all_required_passed,
                "attempt": durable_st.retry_count + 1,
            }),
        );

        // Feed verification result into learning system for real-time pattern detection
        if let Some(bridge) = &self.learning_bridge {
            let criteria_results: Vec<CriterionLearningResult> = durable_st
                .criteria
                .iter()
                .zip(report.results.iter())
                .map(|(c, r)| CriterionLearningResult {
                    criterion_id: c.id.clone(),
                    verifier_kind: format!("{:?}", c.verifier)
                        .split_whitespace()
                        .next()
                        .unwrap_or("unknown")
                        .to_string(),
                    passed: r.passed,
                    duration_ms: r.duration_ms,
                })
                .collect();
            let signal = VerificationLearningSignal {
                task_id: task_id.to_string(),
                subtask_id: subtask_id.to_string(),
                subtask_title: durable_st.title.clone(),
                goal: contract.goal.clone(),
                all_passed: report.all_required_passed,
                pass_rate: if total_count > 0 {
                    passed_count as f64 / total_count as f64
                } else {
                    1.0
                },
                attempt: durable_st.retry_count + 1,
                criteria_results,
                files: durable_st.files.clone(),
                domain_hint: contract.domain_hint.clone(),
                task_type: contract.task_type.clone(),
            };
            let _ = bridge.learn_from_verification(&signal).await;
        }

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
            return Err(format!(
                "subtasks not ready for global verification: {:?}",
                ids
            ));
        }

        self.emit_event(
            "global_verification_started",
            serde_json::json!({
                "task_id": task_id,
                "criteria_count": contract.global_verification.len(),
            }),
        );

        let runner = self.runner();
        let mut results = Vec::new();
        for criterion in &contract.global_verification {
            let result = runner.run_criterion(criterion).await;
            results.push(result);
        }

        let all_passed = results.iter().all(|r| r.passed);
        self.emit_event(
            "global_verification_completed",
            serde_json::json!({
                "task_id": task_id,
                "passed": results.iter().filter(|r| r.passed).count(),
                "total": results.len(),
                "all_passed": all_passed,
            }),
        );

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
                let (passed, total) = match &s.last_verification {
                    Some(report) => {
                        let passed = report.results.iter().filter(|r| r.passed).count() as u32;
                        let total = report.results.len() as u32;
                        (passed, total)
                    }
                    None => (0, s.criteria.len() as u32),
                };
                SubtaskDeliverySummary {
                    id: s.id.clone(),
                    title: s.title.clone(),
                    stage: s.stage.as_str().to_string(),
                    criteria_passed: passed,
                    criteria_total: total,
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

        let total_retries: u32 = contract.subtasks.iter().map(|s| s.retry_count).sum();
        let total_verifications = contract
            .subtasks
            .iter()
            .filter(|s| s.last_verification.is_some())
            .count() as u32
            + total_retries;

        let report = TaskDeliveryReport {
            task_id: task_id.to_string(),
            contract_id: contract.contract_id.clone(),
            goal: contract.goal.clone(),
            subtask_summaries,
            global_verification: global_results,
            total_turns: 0,
            total_tokens: 0,
            total_verifications,
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

        self.emit_event("task_delivered", serde_json::json!({
            "task_id": task_id,
            "contract_id": report.contract_id,
            "goal": report.goal,
            "subtasks_completed": report.subtask_summaries.len(),
            "total_verifications": report.total_verifications,
            "global_checks_passed": report.global_verification.iter().filter(|r| r.passed).count(),
            "global_checks_total": report.global_verification.len(),
        }));

        // Feed completed task into learning system for pattern extraction
        if let Some(bridge) = &self.learning_bridge {
            let all_tools: Vec<String> = contract
                .subtasks
                .iter()
                .flat_map(|s| s.tools_used.iter().cloned())
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect();
            let outcome = build_outcome_signal(
                &contract,
                &report,
                all_tools,
                None,
                contract.domain_hint.clone(),
                contract.task_type.clone(),
            );
            let _ = bridge.learn_from_task_outcome(&outcome).await;

            if contract.status == ContractStatus::Completed {
                let _ = bridge.extract_template(&contract, &report).await;
            }
        }

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
    llm_judge: Option<Arc<dyn LlmJudge>>,
    /// Optional cloud event sender for pushing verification events.
    event_sender: Option<IngestionSender>,
    session_id: String,
    user_id: String,
    /// Learning bridge for feeding verification patterns into the learning system.
    learning_bridge: Option<Arc<dyn TaskLearningBridge>>,
    /// Optional callback that receives live command output lines during verification.
    output_sink: Option<OutputSink>,
}

impl LocalDurableTaskLifecycle {
    pub fn new(data_dir: std::path::PathBuf, work_dir: std::path::PathBuf) -> Self {
        // Prefer git-based snapshots (instant) over file copies (slow)
        let branch_ops: Arc<dyn TaskBranchOps> = if GitBranchOps::is_git_repo(&work_dir) {
            Arc::new(GitBranchOps::new(work_dir.clone()))
        } else {
            Arc::new(LocalFileBranchOps::new(
                data_dir.join("snapshots"),
                work_dir.clone(),
            ))
        };
        Self {
            contracts_dir: data_dir.join("contracts"),
            branch_ops,
            work_dir,
            llm_judge: None,
            event_sender: None,
            session_id: String::new(),
            user_id: String::new(),
            learning_bridge: None,
            output_sink: None,
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
            llm_judge: None,
            event_sender: None,
            session_id: String::new(),
            user_id: String::new(),
            learning_bridge: None,
            output_sink: None,
        }
    }

    /// Set the LLM judge for semantic verification.
    pub fn set_llm_judge(&mut self, judge: Arc<dyn LlmJudge>) {
        self.llm_judge = Some(judge);
    }

    /// Set the event sender for pushing verification events to the cloud stream.
    pub fn set_event_sender(&mut self, sender: IngestionSender) {
        self.event_sender = Some(sender);
    }

    /// Set session context for event attribution.
    pub fn set_session_context(&mut self, session_id: &str, user_id: &str) {
        self.session_id = session_id.to_string();
        self.user_id = user_id.to_string();
    }

    /// Set the learning bridge for feeding verification patterns into learning.
    pub fn set_learning_bridge(&mut self, bridge: Arc<dyn TaskLearningBridge>) {
        self.learning_bridge = Some(bridge);
    }

    /// Set the output sink for streaming live command output during verification.
    pub fn set_output_sink(&mut self, sink: OutputSink) {
        self.output_sink = Some(sink);
    }

    /// Emit a verification-related event to the cloud event stream.
    /// No-op if event_sender is not configured.
    fn emit_event(&self, event_type: &str, metadata: serde_json::Value) {
        let Some(sender) = &self.event_sender else {
            return;
        };
        let event_id = {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut hasher = DefaultHasher::new();
            self.session_id.hash(&mut hasher);
            event_type.hash(&mut hasher);
            chrono::Utc::now().timestamp_nanos_opt().hash(&mut hasher);
            format!("vfy-{:016x}", hasher.finish())
        };
        let content = metadata
            .get("subtask_id")
            .and_then(|v| v.as_str())
            .map(|id| format!("{event_type}: {id}"))
            .unwrap_or_else(|| event_type.to_string());
        sender.enqueue(IngestionEvent {
            event_id,
            session_id: self.session_id.clone(),
            user_id: self.user_id.clone(),
            event_type: event_type.to_string(),
            content: Some(content),
            token_usage: None,
            llm_model_used: None,
            skill_name: None,
            metadata: Some(metadata),
            created_at: chrono::Utc::now().to_rfc3339(),
            parent_event_id: None,
            causal_chain_id: None,
        });
    }

    fn make_runner(&self) -> VerificationRunner {
        let mut runner = match &self.llm_judge {
            Some(j) => VerificationRunner::with_llm_judge(self.work_dir.clone(), j.clone()),
            None => VerificationRunner::new(self.work_dir.clone()),
        };
        if let Some(ref sink) = self.output_sink {
            runner.output_sink = Some(sink.clone());
        }
        runner
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
        let json = serde_json::to_string_pretty(contract).map_err(|e| format!("serialize: {e}"))?;
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
        let entries =
            std::fs::read_dir(&self.contracts_dir).map_err(|e| format!("readdir: {e}"))?;
        for entry in entries.flatten() {
            if entry
                .path()
                .extension()
                .map(|e| e == "json")
                .unwrap_or(false)
                && let Ok(data) = std::fs::read_to_string(entry.path())
                && let Ok(c) = serde_json::from_str::<TaskContract>(&data)
                && c.task_id == task_id
                && c.status != ContractStatus::Abandoned
            {
                return Ok(Some(c));
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
            .map(|sp| {
                let criteria = crate::contract_generator::acceptance_checks_to_criteria(
                    &sp.id,
                    &sp.acceptance_checks,
                    &sp.files,
                );
                DurableSubtask {
                    id: sp.id.clone(),
                    title: sp.title.clone(),
                    description: sp.description.clone(),
                    depends_on: sp.depends_on.clone(),
                    effort: sp.effort.clone(),
                    files: sp.files.clone(),
                    criteria,
                    ..Default::default()
                }
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
            domain_hint: None,
            task_type: None,
            last_global_results: Vec::new(),
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
        self.emit_event(
            "subtask_started",
            serde_json::json!({
                "task_id": task_id,
                "subtask_id": subtask_id,
                "contract_id": contract.contract_id,
            }),
        );
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

        self.emit_event(
            "verification_started",
            serde_json::json!({
                "task_id": task_id,
                "subtask_id": subtask_id,
                "contract_id": contract.contract_id,
            }),
        );

        let runner = self.make_runner();
        // Per-subtask: use local verification (skips global_only & LlmJudge)
        let report = runner.verify_subtask_local(&durable_st).await;

        let event_type = if report.all_required_passed {
            "verification_passed"
        } else {
            "verification_failed"
        };
        self.emit_event(
            event_type,
            serde_json::json!({
                "task_id": task_id,
                "subtask_id": subtask_id,
                "contract_id": contract.contract_id,
                "passed": report.all_required_passed,
                "results_count": report.results.len(),
            }),
        );

        let snapshot_name = durable_st.snapshot_name.clone();
        let subtask = contract
            .subtasks
            .iter_mut()
            .find(|s| s.id == subtask_id)
            .ok_or_else(|| format!("subtask '{subtask_id}' disappeared during verification"))?;
        // Store verification results for delivery report
        subtask.last_verification = Some(report.clone());
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

        // Feed verification result into learning system for real-time pattern detection
        if let Some(bridge) = &self.learning_bridge {
            let passed_count = report.results.iter().filter(|r| r.passed).count();
            let total_count = report.results.len();
            let criteria_results: Vec<CriterionLearningResult> = durable_st
                .criteria
                .iter()
                .zip(report.results.iter())
                .map(|(c, r)| CriterionLearningResult {
                    criterion_id: c.id.clone(),
                    verifier_kind: format!("{:?}", c.verifier)
                        .split_whitespace()
                        .next()
                        .unwrap_or("unknown")
                        .to_string(),
                    passed: r.passed,
                    duration_ms: r.duration_ms,
                })
                .collect();
            let signal = VerificationLearningSignal {
                task_id: task_id.to_string(),
                subtask_id: subtask_id.to_string(),
                subtask_title: durable_st.title.clone(),
                goal: contract.goal.clone(),
                all_passed: report.all_required_passed,
                pass_rate: if total_count > 0 {
                    passed_count as f64 / total_count as f64
                } else {
                    1.0
                },
                attempt: durable_st.retry_count + 1,
                criteria_results,
                files: durable_st.files.clone(),
                domain_hint: contract.domain_hint.clone(),
                task_type: contract.task_type.clone(),
            };
            let _ = bridge.learn_from_verification(&signal).await;
        }

        Ok(report)
    }

    async fn verify_global(&self, task_id: &str) -> Result<Vec<VerificationResult>, String> {
        let mut contract = self
            .find_by_task(task_id)?
            .ok_or_else(|| format!("no contract for task '{task_id}'"))?;
        self.emit_event(
            "global_verification_started",
            serde_json::json!({
                "task_id": task_id,
                "contract_id": contract.contract_id,
                "criteria_count": contract.global_verification.len(),
            }),
        );
        let runner = self.make_runner();
        let mut results = Vec::new();
        for c in &contract.global_verification {
            results.push(runner.run_criterion(c).await);
        }
        let all_passed = results.iter().all(|r| r.passed);
        self.emit_event(
            "global_verification_completed",
            serde_json::json!({
                "task_id": task_id,
                "contract_id": contract.contract_id,
                "all_passed": all_passed,
                "results_count": results.len(),
            }),
        );

        // Persist global results on contract so deliver_task() can include them
        contract.last_global_results = results.clone();
        let _ = self.save_local(&contract);

        // Feed global verification into learning system
        if !results.is_empty()
            && let Some(bridge) = &self.learning_bridge
        {
            let passed_count = results.iter().filter(|r| r.passed).count();
            let total_count = results.len();
            let criteria_results: Vec<CriterionLearningResult> = contract
                .global_verification
                .iter()
                .zip(results.iter())
                .map(|(c, r)| CriterionLearningResult {
                    criterion_id: c.id.clone(),
                    verifier_kind: format!("{:?}", c.verifier)
                        .split_whitespace()
                        .next()
                        .unwrap_or("unknown")
                        .to_string(),
                    passed: r.passed,
                    duration_ms: r.duration_ms,
                })
                .collect();
            let signal = VerificationLearningSignal {
                task_id: task_id.to_string(),
                subtask_id: "__global__".to_string(),
                subtask_title: "Global verification".to_string(),
                goal: contract.goal.clone(),
                all_passed,
                pass_rate: if total_count > 0 {
                    passed_count as f64 / total_count as f64
                } else {
                    1.0
                },
                attempt: 1,
                criteria_results,
                files: Vec::new(),
                domain_hint: contract.domain_hint.clone(),
                task_type: contract.task_type.clone(),
            };
            let _ = bridge.learn_from_verification(&signal).await;
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
            .map(|s| {
                let (passed, total) = match &s.last_verification {
                    Some(report) => {
                        let passed = report.results.iter().filter(|r| r.passed).count() as u32;
                        let total = report.results.len() as u32;
                        (passed, total)
                    }
                    None => {
                        // No verification ran: count only locally-runnable criteria
                        let local = s
                            .criteria
                            .iter()
                            .filter(|c| {
                                !c.global_only
                                    && !matches!(c.verifier, VerifierKind::LlmJudge { .. })
                            })
                            .count() as u32;
                        (0, local)
                    }
                };
                SubtaskDeliverySummary {
                    id: s.id.clone(),
                    title: s.title.clone(),
                    stage: s.stage.as_str().to_string(),
                    criteria_passed: passed,
                    criteria_total: total,
                    retry_count: s.retry_count,
                }
            })
            .collect();

        let total_retries: u32 = contract.subtasks.iter().map(|s| s.retry_count).sum();
        let total_verifications = contract
            .subtasks
            .iter()
            .filter(|s| s.last_verification.is_some())
            .count() as u32
            + total_retries;

        contract.status = ContractStatus::Completed;
        contract.updated_at = chrono::Utc::now().to_rfc3339();
        self.save_local(&contract)?;

        self.emit_event(
            "task_delivered",
            serde_json::json!({
                "task_id": task_id,
                "contract_id": contract.contract_id,
                "goal": contract.goal,
                "total_verifications": total_verifications,
            }),
        );

        let report = TaskDeliveryReport {
            task_id: task_id.to_string(),
            contract_id: contract.contract_id.clone(),
            goal: contract.goal.clone(),
            subtask_summaries: summaries,
            global_verification: contract.last_global_results.clone(),
            total_turns: 0,
            total_tokens: 0,
            total_verifications,
            risks: Vec::new(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        };

        // Feed completed task into learning system for pattern extraction
        if let Some(bridge) = &self.learning_bridge {
            // Collect all tools used across subtasks for task-level signal.
            let all_tools: Vec<String> = contract
                .subtasks
                .iter()
                .flat_map(|s| s.tools_used.iter().cloned())
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect();
            let outcome = build_outcome_signal(
                &contract,
                &report,
                all_tools,
                None, // user_rating — populated post-delivery feedback
                contract.domain_hint.clone(),
                contract.task_type.clone(),
            );
            let _ = bridge.learn_from_task_outcome(&outcome).await;

            // Extract reusable template from successful contracts
            if contract.status == ContractStatus::Completed {
                let _ = bridge.extract_template(&contract, &report).await;
            }
        }

        Ok(report)
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
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &TaskPlan,
        _: TaskScope,
    ) -> Result<TaskContract, String> {
        Err("durable task service not configured".into())
    }
    async fn amend_contract(&self, _: &str, _: ContractAmendment) -> Result<TaskContract, String> {
        Err("durable task service not configured".into())
    }
    async fn get_contract(&self, _: &str) -> Result<Option<TaskContract>, String> {
        Err("durable task service not configured".into())
    }
    async fn begin_subtask(&self, _: &str, _: &str) -> Result<SubtaskExecutionContext, String> {
        Err("durable task service not configured".into())
    }
    async fn complete_subtask_execution(&self, _: &str, _: &str) -> Result<(), String> {
        Err("durable task service not configured".into())
    }
    async fn fail_subtask(&self, _: &str, _: &str, _: &str) -> Result<(), String> {
        Err("durable task service not configured".into())
    }
    async fn verify_subtask(&self, _: &str, _: &str) -> Result<SubtaskVerificationReport, String> {
        Err("durable task service not configured".into())
    }
    async fn verify_global(&self, _: &str) -> Result<Vec<VerificationResult>, String> {
        Err("durable task service not configured".into())
    }
    async fn pause_task(&self, _: &str) -> Result<(), String> {
        Err("durable task service not configured".into())
    }
    async fn resume_task(&self, _: &str, _: &str) -> Result<TaskResumeContext, String> {
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

        let stage = SubtaskStage::VerificationFailed { results: vec![] };
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

    #[tokio::test]
    async fn read_file_contains_passes_when_strings_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("f.txt"), "alpha beta").expect("write");
        let runner = VerificationRunner::new(dir.path().to_path_buf());
        let crit = VerificationCriterion {
            id: "rc1".into(),
            description: "contains alpha".into(),
            verifier: VerifierKind::ReadFileContains {
                path: "f.txt".into(),
                contains: vec!["alpha".into(), "beta".into()],
                not_contains: vec![],
            },
            required: true,
            timeout_sec: 10,
            global_only: false,
        };
        let res = runner.run_criterion(&crit).await;
        assert!(res.passed, "evidence: {}", res.evidence);
        assert!(res.error.is_none());
    }

    #[tokio::test]
    async fn read_file_contains_fails_on_missing_substring() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("g.txt"), "only this").expect("write");
        let runner = VerificationRunner::new(dir.path().to_path_buf());
        let crit = VerificationCriterion {
            id: "rc2".into(),
            description: "missing needle".into(),
            verifier: VerifierKind::ReadFileContains {
                path: "g.txt".into(),
                contains: vec!["needle".into()],
                not_contains: vec![],
            },
            required: true,
            timeout_sec: 10,
            global_only: false,
        };
        let res = runner.run_criterion(&crit).await;
        assert!(!res.passed);
        assert!(
            res.evidence.contains("missing"),
            "evidence: {}",
            res.evidence
        );
    }

    #[tokio::test]
    async fn read_file_contains_respects_not_contains() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("h.txt"), "clean").expect("write");
        let runner = VerificationRunner::new(dir.path().to_path_buf());
        let crit = VerificationCriterion {
            id: "rc3".into(),
            description: "no bad".into(),
            verifier: VerifierKind::ReadFileContains {
                path: "h.txt".into(),
                contains: vec!["clean".into()],
                not_contains: vec!["bad".into()],
            },
            required: true,
            timeout_sec: 10,
            global_only: false,
        };
        let res = runner.run_criterion(&crit).await;
        assert!(res.passed);
    }

    #[tokio::test]
    async fn read_file_contains_blocks_path_traversal() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Create a file in work_dir — then try to escape via ../
        std::fs::write(dir.path().join("legit.txt"), "ok").expect("write");
        let runner = VerificationRunner::new(dir.path().to_path_buf());
        let crit = VerificationCriterion {
            id: "escape".into(),
            description: "attempt to read /etc/passwd".into(),
            verifier: VerifierKind::ReadFileContains {
                path: "../../../etc/passwd".into(),
                contains: vec!["root".into()],
                not_contains: vec![],
            },
            required: true,
            timeout_sec: 10,
            global_only: false,
        };
        let res = runner.run_criterion(&crit).await;
        // Must fail with boundary escape error, NOT succeed in reading /etc/passwd
        assert!(!res.passed, "path traversal should be blocked");
        assert!(
            res.error
                .as_ref()
                .map(|e| e.contains("escapes work directory"))
                .unwrap_or(false),
            "expected boundary escape error, got: {:?}",
            res.error
        );
    }

    #[test]
    fn subtask_stage_as_str() {
        assert_eq!(SubtaskStage::Pending.as_str(), "pending");
        assert_eq!(SubtaskStage::Executing.as_str(), "executing");
        assert_eq!(
            SubtaskStage::AwaitingVerification.as_str(),
            "awaiting_verification"
        );
        assert_eq!(SubtaskStage::Verified.as_str(), "verified");
        assert_eq!(SubtaskStage::Completed.as_str(), "completed");
        assert_eq!(
            SubtaskStage::Abandoned { reason: "x".into() }.as_str(),
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
            domain_hint: None,
            task_type: None,
            last_global_results: Vec::new(),
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
        std::fs::write(
            tmp.path().join("code.rs"),
            "fn main() { println!(\"hello\"); }",
        )
        .unwrap();

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
                    acceptance_checks: vec![VerifierKind::FileExists {
                        paths: vec!["test.txt".into()],
                    }],
                    effort: None,
                    files: vec![],
                },
                crate::task_orchestrator::SubtaskPlan {
                    id: "sub-2".into(),
                    title: "Second subtask".into(),
                    description: None,
                    depends_on: vec!["sub-1".into()],
                    status: TaskStatus::Pending,
                    acceptance_checks: vec![],
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
        let svc = LocalDurableTaskLifecycle::new(tmp.path().join("data"), tmp.path().join("work"));

        let plan = make_test_plan();
        let contract = svc
            .create_contract(
                "user-1",
                "session-1",
                "Build something",
                &plan,
                TaskScope::default(),
            )
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

        let svc = LocalDurableTaskLifecycle::new(tmp.path().join("data"), work.clone());

        let plan = make_test_plan();
        let contract = svc
            .create_contract("u", "s", "test", &plan, TaskScope::default())
            .await
            .unwrap();

        // Begin subtask
        let ctx = svc.begin_subtask(&contract.task_id, "sub-1").await.unwrap();
        assert_eq!(ctx.title, "First subtask");

        // Create the file that the acceptance_check expects
        std::fs::write(work.join("test.txt"), "content").unwrap();

        // Complete execution
        svc.complete_subtask_execution(&contract.task_id, "sub-1")
            .await
            .unwrap();

        // Verify (now has criteria from acceptance_checks)
        let report = svc
            .verify_subtask(&contract.task_id, "sub-1")
            .await
            .unwrap();
        assert!(report.all_required_passed);

        // Check state persisted
        let c = svc
            .get_contract(&contract.contract_id)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(c.subtasks[0].stage, SubtaskStage::Verified));
    }

    #[tokio::test]
    async fn local_lifecycle_fail_subtask() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = LocalDurableTaskLifecycle::new(tmp.path().join("data"), tmp.path().join("work"));

        let plan = make_test_plan();
        let contract = svc
            .create_contract("u", "s", "test", &plan, TaskScope::default())
            .await
            .unwrap();

        svc.begin_subtask(&contract.task_id, "sub-1").await.unwrap();
        svc.fail_subtask(&contract.task_id, "sub-1", "compilation error")
            .await
            .unwrap();

        let c = svc
            .get_contract(&contract.contract_id)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            c.subtasks[0].stage,
            SubtaskStage::ExecutionFailed { .. }
        ));
    }

    #[tokio::test]
    async fn local_lifecycle_amend_contract() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = LocalDurableTaskLifecycle::new(tmp.path().join("data"), tmp.path().join("work"));

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
        svc.begin_subtask(&contract.task_id, "check-sub")
            .await
            .unwrap();
        svc.complete_subtask_execution(&contract.task_id, "check-sub")
            .await
            .unwrap();

        let report = svc
            .verify_subtask(&contract.task_id, "check-sub")
            .await
            .unwrap();
        assert!(
            report.all_required_passed,
            "should pass: {:?}",
            report.results
        );
        assert_eq!(report.results.len(), 2);

        // Check stage is Verified
        let c = svc
            .get_contract(&contract.contract_id)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(c.subtasks[0].stage, SubtaskStage::Verified));
    }

    #[tokio::test]
    async fn local_lifecycle_deliver() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = LocalDurableTaskLifecycle::new(tmp.path().join("data"), tmp.path().join("work"));

        let plan = make_test_plan();
        let contract = svc
            .create_contract("u", "s", "deliver test", &plan, TaskScope::default())
            .await
            .unwrap();

        // Complete both subtasks (no criteria → auto-verified)
        svc.begin_subtask(&contract.task_id, "sub-1").await.unwrap();
        svc.complete_subtask_execution(&contract.task_id, "sub-1")
            .await
            .unwrap();
        svc.begin_subtask(&contract.task_id, "sub-2").await.unwrap();
        svc.complete_subtask_execution(&contract.task_id, "sub-2")
            .await
            .unwrap();

        let report = svc.deliver_task(&contract.task_id).await.unwrap();
        assert_eq!(report.goal, "deliver test");
        assert_eq!(report.subtask_summaries.len(), 2);
    }

    #[tokio::test]
    async fn local_lifecycle_resume() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = LocalDurableTaskLifecycle::new(tmp.path().join("data"), tmp.path().join("work"));

        let plan = make_test_plan();
        let contract = svc
            .create_contract("u", "s", "resume test", &plan, TaskScope::default())
            .await
            .unwrap();

        svc.begin_subtask(&contract.task_id, "sub-1").await.unwrap();

        let ctx = svc
            .resume_task(&contract.task_id, "new-session")
            .await
            .unwrap();
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
        assert!(
            bridge
                .suggest_tools("test", None, None)
                .await
                .unwrap()
                .is_empty()
        );
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
            domain_hint: None,
            task_type: None,
            last_global_results: Vec::new(),
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
        diffs_requested: Vec<String>,                  // snapshot names
        rollbacks: Vec<String>,                        // snapshot names
        cleanups: Vec<String>,                         // snapshot names
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
            let name = sanitize_snapshot_name(&format!("task_{task_id}_{subtask_id}_v{version}"));
            self.log.lock().unwrap().snapshots_created.push((
                task_id.into(),
                subtask_id.into(),
                version,
            ));
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
        LocalDurableTaskLifecycle::with_branch_ops(tmp.path().join("data"), mock, work)
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
        // Hyphens in task_id/subtask_id are sanitized to underscores in snapshot names
        let sanitized_id = contract.task_id.replace('-', "_");
        assert!(snap_name.contains(&sanitized_id));
        assert!(snap_name.contains("sub_1"));

        // snapshot_name is persisted in contract
        let c = svc
            .get_contract(&contract.contract_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            c.subtasks[0].snapshot_name.as_deref(),
            Some(snap_name.as_str())
        );
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
        let c = svc
            .get_contract(&contract.contract_id)
            .await
            .unwrap()
            .unwrap();
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
        let c = svc
            .get_contract(&contract.contract_id)
            .await
            .unwrap()
            .unwrap();
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
        let svc =
            LocalDurableTaskLifecycle::with_branch_ops(tmp.path().join("data"), mock.clone(), work);

        // Create plan with a single subtask with file-exists criterion
        let mut plan = make_test_plan();
        plan.subtasks.truncate(1);
        let contract = svc
            .create_contract("u", "s", "test", &plan, TaskScope::default())
            .await
            .unwrap();

        // Amend to add criteria
        let mut c = svc
            .get_contract(&contract.contract_id)
            .await
            .unwrap()
            .unwrap();
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
        let report = svc
            .verify_subtask(&contract.task_id, "sub-1")
            .await
            .unwrap();
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
        let svc =
            LocalDurableTaskLifecycle::with_branch_ops(tmp.path().join("data"), mock.clone(), work);

        let mut plan = make_test_plan();
        plan.subtasks.truncate(1);
        let contract = svc
            .create_contract("u", "s", "test", &plan, TaskScope::default())
            .await
            .unwrap();

        // Add a criterion that will always fail
        let mut c = svc
            .get_contract(&contract.contract_id)
            .await
            .unwrap()
            .unwrap();
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
        let report = svc
            .verify_subtask(&contract.task_id, "sub-1")
            .await
            .unwrap();
        assert!(!report.all_required_passed);

        // First failure: retry_count < max_retries → VerificationFailed, no rollback
        let c = svc
            .get_contract(&contract.contract_id)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            c.subtasks[0].stage,
            SubtaskStage::VerificationFailed { .. }
        ));
        assert_eq!(mock.log().rollbacks.len(), 0);

        // Re-execute and verify again (will fail again → max retries → abandoned + rollback)
        svc.begin_subtask(&contract.task_id, "sub-1").await.unwrap();
        svc.complete_subtask_execution(&contract.task_id, "sub-1")
            .await
            .unwrap();
        let report2 = svc
            .verify_subtask(&contract.task_id, "sub-1")
            .await
            .unwrap();
        assert!(!report2.all_required_passed);

        // Second failure: retry_count >= max_retries → Abandoned + rollback + cleanup
        let c = svc
            .get_contract(&contract.contract_id)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            c.subtasks[0].stage,
            SubtaskStage::Abandoned { .. }
        ));

        let log = mock.log();
        assert!(!log.rollbacks.is_empty(), "should have rolled back");
        assert!(
            !log.cleanups.is_empty(),
            "should have cleaned up after rollback"
        );
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
    async fn git_branch_ops_snapshot_and_diff() {
        let tmp = tempfile::TempDir::new().unwrap();
        let work = tmp.path().join("repo");
        std::fs::create_dir_all(&work).unwrap();

        // Initialize a git repo
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&work)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&work)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&work)
            .output()
            .unwrap();

        // Create initial commit
        std::fs::write(work.join("file.txt"), "initial").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&work)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&work)
            .output()
            .unwrap();

        assert!(GitBranchOps::is_git_repo(&work));
        let ops = GitBranchOps::new(work.clone());

        // Snapshot
        let name = ops.create_snapshot("t1", "s1", 1).await.unwrap();
        assert!(!name.is_empty());

        // Modify file + add new file (simulate subtask work)
        std::fs::write(work.join("file.txt"), "modified").unwrap();
        std::fs::write(work.join("new.rs"), "fn main() {}").unwrap();
        // Stage changes for git diff to detect
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&work)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "subtask work"])
            .current_dir(&work)
            .output()
            .unwrap();

        // Diff should detect changes
        let diff = ops.diff_since_snapshot(&name).await.unwrap();
        assert!(diff.changed_rows >= 1, "should detect changes: {:?}", diff);

        // Cleanup is a no-op (removes from in-memory map)
        ops.cleanup_snapshot(&name).await.unwrap();
    }

    #[tokio::test]
    async fn local_lifecycle_with_real_snapshots_full_flow() {
        let tmp = tempfile::TempDir::new().unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();

        // Initial file
        std::fs::write(work.join("data.txt"), "initial").unwrap();

        let svc = LocalDurableTaskLifecycle::new(tmp.path().join("data"), work.clone());

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

        let c = svc
            .get_contract(&contract.contract_id)
            .await
            .unwrap()
            .unwrap();
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

    #[test]
    fn sanitize_snapshot_name_replaces_hyphens() {
        // UUID-based task IDs have hyphens that must be replaced
        assert_eq!(
            sanitize_snapshot_name("task_abc-def_sub-1_v1"),
            "task_abc_def_sub_1_v1"
        );
        // Already clean names pass through unchanged
        assert_eq!(
            sanitize_snapshot_name("task_abc_sub1_v1"),
            "task_abc_sub1_v1"
        );
        // Sanitized names must pass validation
        let sanitized = sanitize_snapshot_name("task_7bfcf9b1-1234_api-auth_v2");
        assert!(validate_snapshot_name(&sanitized).is_ok());
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

    // ── LlmJudge trait tests ──

    struct MockLlmJudge {
        fixed_score: f64,
    }

    #[async_trait]
    impl LlmJudge for MockLlmJudge {
        async fn evaluate(&self, _prompt: &str, _context: &str) -> Result<f64, String> {
            Ok(self.fixed_score)
        }
    }

    /// Mock that captures the context string for inspection.
    struct ContextCapturingJudge {
        captured: std::sync::Mutex<Option<String>>,
    }

    #[async_trait]
    impl LlmJudge for ContextCapturingJudge {
        async fn evaluate(&self, _prompt: &str, context: &str) -> Result<f64, String> {
            *self.captured.lock().unwrap() = Some(context.to_string());
            Ok(0.9)
        }
    }

    #[tokio::test]
    async fn llm_judge_passes_when_score_above_threshold() {
        let tmp = tempfile::TempDir::new().unwrap();
        let judge = Arc::new(MockLlmJudge { fixed_score: 0.85 });
        let runner = VerificationRunner::with_llm_judge(tmp.path().to_path_buf(), judge);

        let criterion = VerificationCriterion {
            id: "llm-1".into(),
            description: "code quality is good".into(),
            verifier: VerifierKind::LlmJudge {
                prompt: "Is the code quality good?".into(),
                pass_threshold: 0.7,
            },
            required: true,
            timeout_sec: 30,
            global_only: true,
        };

        let result = runner.run_criterion(&criterion).await;
        assert!(result.passed, "score 0.85 >= threshold 0.7 should pass");
        assert!(result.evidence.contains("0.85"));
        assert!(result.error.is_none());
    }

    #[tokio::test]
    async fn llm_judge_fails_when_score_below_threshold() {
        let tmp = tempfile::TempDir::new().unwrap();
        let judge = Arc::new(MockLlmJudge { fixed_score: 0.3 });
        let runner = VerificationRunner::with_llm_judge(tmp.path().to_path_buf(), judge);

        let criterion = VerificationCriterion {
            id: "llm-2".into(),
            description: "tests are comprehensive".into(),
            verifier: VerifierKind::LlmJudge {
                prompt: "Are tests comprehensive?".into(),
                pass_threshold: 0.7,
            },
            required: true,
            timeout_sec: 30,
            global_only: true,
        };

        let result = runner.run_criterion(&criterion).await;
        assert!(!result.passed, "score 0.3 < threshold 0.7 should fail");
        assert!(result.evidence.contains("0.30"));
    }

    #[tokio::test]
    async fn llm_judge_without_provider_returns_false() {
        let tmp = tempfile::TempDir::new().unwrap();
        let runner = VerificationRunner::new(tmp.path().to_path_buf());

        let criterion = VerificationCriterion {
            id: "llm-3".into(),
            description: "semantic check".into(),
            verifier: VerifierKind::LlmJudge {
                prompt: "Is the code clean?".into(),
                pass_threshold: 0.7,
            },
            required: true,
            timeout_sec: 30,
            global_only: true,
        };

        let result = runner.run_criterion(&criterion).await;
        assert!(!result.passed, "no judge configured → should fail");
        assert!(result.evidence.contains("not available"));
    }

    struct FailingLlmJudge;

    #[async_trait]
    impl LlmJudge for FailingLlmJudge {
        async fn evaluate(&self, _prompt: &str, _context: &str) -> Result<f64, String> {
            Err("API rate limited".into())
        }
    }

    #[tokio::test]
    async fn llm_judge_handles_api_error_gracefully() {
        let tmp = tempfile::TempDir::new().unwrap();
        let judge: Arc<dyn LlmJudge> = Arc::new(FailingLlmJudge);
        let runner = VerificationRunner::with_llm_judge(tmp.path().to_path_buf(), judge);

        let criterion = VerificationCriterion {
            id: "llm-4".into(),
            description: "check quality".into(),
            verifier: VerifierKind::LlmJudge {
                prompt: "Is quality high?".into(),
                pass_threshold: 0.7,
            },
            required: true,
            timeout_sec: 30,
            global_only: true,
        };

        let result = runner.run_criterion(&criterion).await;
        assert!(!result.passed, "API error → should fail");
        assert!(result.evidence.contains("rate limited"));
    }

    #[tokio::test]
    async fn llm_judge_context_includes_file_contents() {
        let tmp = tempfile::TempDir::new().unwrap();

        // Create a file that the prompt references
        let src_dir = tmp.path().join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(
            src_dir.join("auth.rs"),
            "pub fn authenticate() { /* ... */ }",
        )
        .unwrap();

        let judge = Arc::new(ContextCapturingJudge {
            captured: std::sync::Mutex::new(None),
        });
        let runner = VerificationRunner::with_llm_judge(tmp.path().to_path_buf(), judge.clone());

        let criterion = VerificationCriterion {
            id: "ctx-1".into(),
            description: "check auth quality".into(),
            verifier: VerifierKind::LlmJudge {
                prompt: "Does src/auth.rs follow best practices?".into(),
                pass_threshold: 0.5,
            },
            required: true,
            timeout_sec: 30,
            global_only: true,
        };

        let result = runner.run_criterion(&criterion).await;
        assert!(result.passed);

        let context = judge.captured.lock().unwrap().clone().unwrap();
        assert!(
            context.contains("authenticate"),
            "context should include file contents: {context}"
        );
        assert!(
            context.contains("src/auth.rs"),
            "context should reference the file path: {context}"
        );
        assert!(
            context.contains("Directory contents"),
            "context should include directory listing: {context}"
        );
    }

    // ─── parse_judge_score tests ─────────────────────────────────────────────

    #[test]
    fn parse_judge_score_from_json() {
        let score = parse_judge_score(r#"{"score": 0.85, "reason": "looks good"}"#).unwrap();
        assert!((score - 0.85).abs() < 0.001);
    }

    #[test]
    fn parse_judge_score_from_markdown_json() {
        let text =
            "Here is my evaluation:\n```json\n{\"score\": 0.7, \"reason\": \"mostly ok\"}\n```";
        let score = parse_judge_score(text).unwrap();
        assert!((score - 0.7).abs() < 0.001);
    }

    #[test]
    fn parse_judge_score_from_plain_number() {
        let score = parse_judge_score("The score is 0.9 out of 1.0").unwrap();
        assert!((score - 0.9).abs() < 0.001);
    }

    #[test]
    fn parse_judge_score_clamps_above_one() {
        let score = parse_judge_score(r#"{"score": 1.5}"#).unwrap();
        assert!((score - 1.0).abs() < 0.001);
    }

    #[test]
    fn parse_judge_score_no_number_is_err() {
        let result = parse_judge_score("This criterion is fully met");
        assert!(result.is_err());
    }

    // ─── CloudLlmConfig tests ────────────────────────────────────────────────

    #[test]
    fn cloud_llm_config_from_env_requires_api_key() {
        let config = CloudLlmConfig::from_env();
        // We can't assert None because CI might have OPENAI_API_KEY set;
        // just verify no panics.
        let _ = config;
    }

    #[test]
    fn cloud_llm_judge_persist_context_default() {
        let ctx = CloudJudgePersistContext::default();
        assert!(ctx.contract_id.is_none());
        assert!(ctx.task_id.is_none());
        assert!(ctx.subtask_id.is_none());
        assert!(ctx.session_id.is_none());
    }

    // ─── Learning Bridge Integration Tests ──────────────────────────────────

    /// A mock learning bridge that records all calls for assertion.
    struct RecordingLearningBridge {
        verification_calls: std::sync::Mutex<Vec<VerificationLearningSignal>>,
        outcome_calls: std::sync::Mutex<Vec<TaskOutcomeSignal>>,
        template_calls: std::sync::Mutex<Vec<String>>,
    }

    impl RecordingLearningBridge {
        fn new() -> Self {
            Self {
                verification_calls: std::sync::Mutex::new(Vec::new()),
                outcome_calls: std::sync::Mutex::new(Vec::new()),
                template_calls: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl TaskLearningBridge for RecordingLearningBridge {
        async fn learn_from_task_outcome(&self, signal: &TaskOutcomeSignal) -> Result<(), String> {
            self.outcome_calls.lock().unwrap().push(signal.clone());
            Ok(())
        }
        async fn extract_template(
            &self,
            contract: &TaskContract,
            _report: &TaskDeliveryReport,
        ) -> Result<Option<String>, String> {
            self.template_calls
                .lock()
                .unwrap()
                .push(contract.goal.clone());
            Ok(Some("mock-template".into()))
        }
        async fn suggest_tools(
            &self,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
        ) -> Result<Vec<String>, String> {
            Ok(vec![])
        }
        async fn task_pattern_stats(&self, _: &str) -> Result<Option<TaskPatternStats>, String> {
            Ok(None)
        }
        async fn learn_from_verification(
            &self,
            signal: &VerificationLearningSignal,
        ) -> Result<(), String> {
            self.verification_calls.lock().unwrap().push(signal.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn verify_subtask_invokes_learning_bridge() {
        let tmp = tempfile::TempDir::new().unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let mut svc = LocalDurableTaskLifecycle::new(tmp.path().join("data"), work.clone());

        let recorder = std::sync::Arc::new(RecordingLearningBridge::new());
        svc.set_learning_bridge(recorder.clone());

        // Create contract with a FileExists criterion
        let plan = make_test_plan();
        let mut contract = svc
            .create_contract("u", "s", "test learning", &plan, TaskScope::default())
            .await
            .unwrap();

        // Add a criterion that will pass
        let target = work.join("output.txt");
        std::fs::write(&target, "hello").unwrap();
        // Also create test.txt — now generated from acceptance_checks
        std::fs::write(work.join("test.txt"), "").unwrap();
        contract.subtasks[0].criteria.push(VerificationCriterion {
            id: "file-check".into(),
            description: "output.txt exists".into(),
            verifier: VerifierKind::FileExists {
                paths: vec![target.to_string_lossy().to_string()],
            },
            required: true,
            timeout_sec: 30,
            global_only: false,
        });
        svc.save_local(&contract).unwrap();

        // Execute subtask lifecycle
        svc.begin_subtask(&contract.task_id, "sub-1").await.unwrap();
        svc.complete_subtask_execution(&contract.task_id, "sub-1")
            .await
            .unwrap();

        // Verify should trigger learning
        let report = svc
            .verify_subtask(&contract.task_id, "sub-1")
            .await
            .unwrap();
        assert!(report.all_required_passed);

        // Check that learning bridge was called
        let calls = recorder.verification_calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "learning bridge should be called once");
        assert_eq!(calls[0].subtask_id, "sub-1");
        assert!(calls[0].all_passed);
        assert_eq!(calls[0].attempt, 1);
        assert!(!calls[0].criteria_results.is_empty());
    }

    #[tokio::test]
    async fn deliver_task_invokes_learning_bridge() {
        let tmp = tempfile::TempDir::new().unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let mut svc = LocalDurableTaskLifecycle::new(tmp.path().join("data"), work.clone());

        let recorder = std::sync::Arc::new(RecordingLearningBridge::new());
        svc.set_learning_bridge(recorder.clone());

        let plan = make_test_plan();
        let contract = svc
            .create_contract(
                "u",
                "s",
                "deliver learning test",
                &plan,
                TaskScope::default(),
            )
            .await
            .unwrap();

        // Create test.txt so sub-1's FileExists criterion passes
        std::fs::write(work.join("test.txt"), "").unwrap();

        // Complete sub-1 and verify (has criteria from acceptance_checks)
        svc.begin_subtask(&contract.task_id, "sub-1").await.unwrap();
        svc.complete_subtask_execution(&contract.task_id, "sub-1")
            .await
            .unwrap();
        let _ = svc
            .verify_subtask(&contract.task_id, "sub-1")
            .await
            .unwrap();

        // Complete sub-2 (no criteria → auto-verified)
        svc.begin_subtask(&contract.task_id, "sub-2").await.unwrap();
        svc.complete_subtask_execution(&contract.task_id, "sub-2")
            .await
            .unwrap();

        let report = svc.deliver_task(&contract.task_id).await.unwrap();
        assert_eq!(report.goal, "deliver learning test");

        // Check that outcome learning was called
        let outcome_calls = recorder.outcome_calls.lock().unwrap();
        assert_eq!(
            outcome_calls.len(),
            1,
            "learn_from_task_outcome should be called"
        );
        assert!(outcome_calls[0].success);
        assert_eq!(outcome_calls[0].goal, "deliver learning test");

        // Check that template extraction was called
        let template_calls = recorder.template_calls.lock().unwrap();
        assert_eq!(
            template_calls.len(),
            1,
            "extract_template should be called for completed contracts"
        );
    }

    // ─── parse_test_output tests ────────────────────────────────────────────

    #[test]
    fn parse_test_output_rust_cargo() {
        let output = r#"
running 42 tests
test foo ... ok
test bar ... ok
test baz ... FAILED

test result: ok. 41 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out
"#;
        let (p, f) = parse_test_output(output).unwrap();
        assert_eq!(p, 41);
        assert_eq!(f, 1);
    }

    #[test]
    fn parse_test_output_pytest() {
        let output = "===== 10 passed, 2 failed in 1.23s =====";
        let (p, f) = parse_test_output(output).unwrap();
        assert_eq!(p, 10);
        assert_eq!(f, 2);
    }

    #[test]
    fn parse_test_output_pytest_all_pass() {
        let output = "===== 15 passed in 0.50s =====";
        let (p, f) = parse_test_output(output).unwrap();
        assert_eq!(p, 15);
        assert_eq!(f, 0);
    }

    #[test]
    fn parse_test_output_jest() {
        let output = r#"
Test Suites: 1 failed, 4 passed, 5 total
Tests:       2 failed, 10 passed, 12 total
Snapshots:   0 total
Time:        3.456 s
"#;
        let (p, f) = parse_test_output(output).unwrap();
        assert_eq!(p, 10);
        assert_eq!(f, 2);
    }

    #[test]
    fn parse_test_output_junit_maven() {
        let output = "Tests run: 25, Failures: 3, Errors: 1, Skipped: 2";
        let (p, f) = parse_test_output(output).unwrap();
        assert_eq!(p, 21); // 25 - 3 - 1
        assert_eq!(f, 4); // 3 + 1
    }

    #[test]
    fn parse_test_output_mocha_passing_failing() {
        let output = "  8 passing (120ms)\n  1 failing";
        let (p, f) = parse_test_output(output).unwrap();
        assert_eq!(p, 8);
        assert_eq!(f, 1);
    }

    #[test]
    fn parse_test_output_unrecognized_returns_none() {
        let output = "Hello world, this is not test output.";
        assert!(parse_test_output(output).is_none());
    }

    // ─── BuildPass verifier tests ───────────────────────────────────────────

    #[tokio::test]
    async fn buildpass_verifier_success() {
        let tmp = tempfile::TempDir::new().unwrap();
        let runner = VerificationRunner::new(tmp.path().to_path_buf());
        let criterion = VerificationCriterion {
            id: "build-ok".into(),
            description: "build passes".into(),
            verifier: VerifierKind::BuildPass {
                cmd: "echo 'build ok'".into(),
            },
            required: true,
            timeout_sec: 10,
            global_only: false,
        };
        let result = runner.run_criterion(&criterion).await;
        assert!(result.passed);
        assert_eq!(result.expected, "exit code == 0");
    }

    #[tokio::test]
    async fn buildpass_verifier_failure() {
        let tmp = tempfile::TempDir::new().unwrap();
        let runner = VerificationRunner::new(tmp.path().to_path_buf());
        let criterion = VerificationCriterion {
            id: "build-fail".into(),
            description: "build fails".into(),
            verifier: VerifierKind::BuildPass {
                cmd: "echo 'error: something wrong' >&2; exit 1".into(),
            },
            required: true,
            timeout_sec: 10,
            global_only: false,
        };
        let result = runner.run_criterion(&criterion).await;
        assert!(!result.passed);
        assert!(result.evidence.contains("error: something wrong"));
    }

    // ─── TestPass verifier tests ────────────────────────────────────────────

    #[tokio::test]
    async fn testpass_verifier_uses_parsed_rate_not_just_exit_code() {
        // This is the P0 bug scenario: exit code 0 but low pass rate
        let tmp = tempfile::TempDir::new().unwrap();
        let runner = VerificationRunner::new(tmp.path().to_path_buf());
        let criterion = VerificationCriterion {
            id: "test-rate".into(),
            description: "tests with pass rate".into(),
            verifier: VerifierKind::TestPass {
                // Exit 0 but report 5 passed, 5 failed → 50% pass rate
                cmd: "echo 'test result: ok. 5 passed; 5 failed; 0 ignored'; exit 0".into(),
                min_pass_rate: 0.9, // require 90%
            },
            required: true,
            timeout_sec: 10,
            global_only: false,
        };
        let result = runner.run_criterion(&criterion).await;
        // Despite exit 0, should FAIL because 50% < 90%
        assert!(
            !result.passed,
            "TestPass should fail when pass rate (50%) < min_pass_rate (90%), even if exit code is 0"
        );
        assert!(result.evidence.contains("50%"));
    }

    #[tokio::test]
    async fn testpass_verifier_passes_when_rate_met() {
        let tmp = tempfile::TempDir::new().unwrap();
        let runner = VerificationRunner::new(tmp.path().to_path_buf());
        let criterion = VerificationCriterion {
            id: "test-ok".into(),
            description: "all tests pass".into(),
            verifier: VerifierKind::TestPass {
                cmd: "echo 'test result: ok. 10 passed; 0 failed; 0 ignored'".into(),
                min_pass_rate: 1.0,
            },
            required: true,
            timeout_sec: 10,
            global_only: false,
        };
        let result = runner.run_criterion(&criterion).await;
        assert!(result.passed);
        assert!(result.evidence.contains("100%"));
    }

    #[tokio::test]
    async fn testpass_verifier_fallback_to_exit_code() {
        // When output can't be parsed, fall back to exit code
        let tmp = tempfile::TempDir::new().unwrap();
        let runner = VerificationRunner::new(tmp.path().to_path_buf());
        let criterion = VerificationCriterion {
            id: "test-fallback".into(),
            description: "unparseable output".into(),
            verifier: VerifierKind::TestPass {
                cmd: "echo 'all good'; exit 0".into(),
                min_pass_rate: 1.0,
            },
            required: true,
            timeout_sec: 10,
            global_only: false,
        };
        let result = runner.run_criterion(&criterion).await;
        assert!(result.passed, "should pass via exit code fallback");
        assert!(result.evidence.contains("could not parse test counts"));
    }

    #[tokio::test]
    async fn testpass_verifier_fallback_exit_nonzero() {
        let tmp = tempfile::TempDir::new().unwrap();
        let runner = VerificationRunner::new(tmp.path().to_path_buf());
        let criterion = VerificationCriterion {
            id: "test-fallback-fail".into(),
            description: "unparseable + exit 1".into(),
            verifier: VerifierKind::TestPass {
                cmd: "echo 'kaboom'; exit 1".into(),
                min_pass_rate: 1.0,
            },
            required: true,
            timeout_sec: 10,
            global_only: false,
        };
        let result = runner.run_criterion(&criterion).await;
        assert!(!result.passed, "should fail via exit code fallback");
    }

    // ─── Composite verifier tests ───────────────────────────────────────────

    #[tokio::test]
    async fn composite_all_pass() {
        let tmp = tempfile::TempDir::new().unwrap();
        let runner = VerificationRunner::new(tmp.path().to_path_buf());
        let criterion = VerificationCriterion {
            id: "comp-all".into(),
            description: "all must pass".into(),
            verifier: VerifierKind::Composite {
                criteria: vec![
                    VerificationCriterion {
                        id: "a".into(),
                        description: "a".into(),
                        verifier: VerifierKind::Command {
                            cmd: "true".into(),
                            expected_exit: 0,
                        },
                        required: true,
                        timeout_sec: 10,
                        global_only: false,
                    },
                    VerificationCriterion {
                        id: "b".into(),
                        description: "b".into(),
                        verifier: VerifierKind::Command {
                            cmd: "true".into(),
                            expected_exit: 0,
                        },
                        required: true,
                        timeout_sec: 10,
                        global_only: false,
                    },
                ],
                require_all: true,
            },
            required: true,
            timeout_sec: 30,
            global_only: false,
        };
        let result = runner.run_criterion(&criterion).await;
        assert!(result.passed);
        assert!(result.evidence.contains("a: ✓"));
        assert!(result.evidence.contains("b: ✓"));
    }

    #[tokio::test]
    async fn composite_all_one_fails() {
        let tmp = tempfile::TempDir::new().unwrap();
        let runner = VerificationRunner::new(tmp.path().to_path_buf());
        let criterion = VerificationCriterion {
            id: "comp-all-fail".into(),
            description: "one fails in ALL mode".into(),
            verifier: VerifierKind::Composite {
                criteria: vec![
                    VerificationCriterion {
                        id: "ok".into(),
                        description: "ok".into(),
                        verifier: VerifierKind::Command {
                            cmd: "true".into(),
                            expected_exit: 0,
                        },
                        required: true,
                        timeout_sec: 10,
                        global_only: false,
                    },
                    VerificationCriterion {
                        id: "fail".into(),
                        description: "fail".into(),
                        verifier: VerifierKind::Command {
                            cmd: "false".into(),
                            expected_exit: 0,
                        },
                        required: true,
                        timeout_sec: 10,
                        global_only: false,
                    },
                ],
                require_all: true,
            },
            required: true,
            timeout_sec: 30,
            global_only: false,
        };
        let result = runner.run_criterion(&criterion).await;
        assert!(
            !result.passed,
            "ALL mode should fail if any sub-criterion fails"
        );
        assert!(result.evidence.contains("ok: ✓"));
        assert!(result.evidence.contains("fail: ✗"));
    }

    #[tokio::test]
    async fn composite_any_one_passes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let runner = VerificationRunner::new(tmp.path().to_path_buf());
        let criterion = VerificationCriterion {
            id: "comp-any".into(),
            description: "one passes in ANY mode".into(),
            verifier: VerifierKind::Composite {
                criteria: vec![
                    VerificationCriterion {
                        id: "fail1".into(),
                        description: "fail".into(),
                        verifier: VerifierKind::Command {
                            cmd: "false".into(),
                            expected_exit: 0,
                        },
                        required: true,
                        timeout_sec: 10,
                        global_only: false,
                    },
                    VerificationCriterion {
                        id: "ok1".into(),
                        description: "ok".into(),
                        verifier: VerifierKind::Command {
                            cmd: "true".into(),
                            expected_exit: 0,
                        },
                        required: true,
                        timeout_sec: 10,
                        global_only: false,
                    },
                ],
                require_all: false,
            },
            required: true,
            timeout_sec: 30,
            global_only: false,
        };
        let result = runner.run_criterion(&criterion).await;
        assert!(
            result.passed,
            "ANY mode should pass if at least one sub-criterion passes"
        );
    }

    #[tokio::test]
    async fn composite_any_all_fail() {
        let tmp = tempfile::TempDir::new().unwrap();
        let runner = VerificationRunner::new(tmp.path().to_path_buf());
        let criterion = VerificationCriterion {
            id: "comp-any-fail".into(),
            description: "all fail in ANY mode".into(),
            verifier: VerifierKind::Composite {
                criteria: vec![
                    VerificationCriterion {
                        id: "f1".into(),
                        description: "f".into(),
                        verifier: VerifierKind::Command {
                            cmd: "false".into(),
                            expected_exit: 0,
                        },
                        required: true,
                        timeout_sec: 10,
                        global_only: false,
                    },
                    VerificationCriterion {
                        id: "f2".into(),
                        description: "f".into(),
                        verifier: VerifierKind::Command {
                            cmd: "false".into(),
                            expected_exit: 0,
                        },
                        required: true,
                        timeout_sec: 10,
                        global_only: false,
                    },
                ],
                require_all: false,
            },
            required: true,
            timeout_sec: 30,
            global_only: false,
        };
        let result = runner.run_criterion(&criterion).await;
        assert!(
            !result.passed,
            "ANY mode should fail if all sub-criteria fail"
        );
    }

    // ─── SubtaskStage state machine tests ───────────────────────────────────

    #[test]
    fn subtask_stage_terminal_states() {
        assert!(SubtaskStage::Completed.is_terminal());
        assert!(
            SubtaskStage::Skipped {
                reason: "n/a".into()
            }
            .is_terminal()
        );
        assert!(
            SubtaskStage::Abandoned {
                reason: "n/a".into()
            }
            .is_terminal()
        );

        // Non-terminal
        assert!(!SubtaskStage::Pending.is_terminal());
        assert!(!SubtaskStage::Executing.is_terminal());
        assert!(!SubtaskStage::Verified.is_terminal());
        assert!(!SubtaskStage::AwaitingVerification.is_terminal());
        assert!(!SubtaskStage::Verifying.is_terminal());
        assert!(!SubtaskStage::VerificationFailed { results: vec![] }.is_terminal());
    }

    #[test]
    fn subtask_stage_success_states() {
        assert!(SubtaskStage::Completed.is_success());
        assert!(SubtaskStage::Verified.is_success());

        assert!(!SubtaskStage::Pending.is_success());
        assert!(!SubtaskStage::Executing.is_success());
        assert!(!SubtaskStage::Abandoned { reason: "x".into() }.is_success());
        assert!(!SubtaskStage::Skipped { reason: "x".into() }.is_success());
    }

    #[test]
    fn subtask_stage_can_start() {
        assert!(SubtaskStage::Pending.can_start());
        assert!(SubtaskStage::VerificationFailed { results: vec![] }.can_start());

        assert!(!SubtaskStage::Executing.can_start());
        assert!(!SubtaskStage::Completed.can_start());
        assert!(!SubtaskStage::Verified.can_start());
        assert!(!SubtaskStage::Blocked { reason: "x".into() }.can_start());
    }

    #[test]
    fn subtask_stage_as_str_roundtrip() {
        let stages = vec![
            SubtaskStage::Pending,
            SubtaskStage::Blocked {
                reason: "dep".into(),
            },
            SubtaskStage::Executing,
            SubtaskStage::ExecutionFailed {
                error: "err".into(),
            },
            SubtaskStage::AwaitingVerification,
            SubtaskStage::Verifying,
            SubtaskStage::VerificationFailed { results: vec![] },
            SubtaskStage::Verified,
            SubtaskStage::Completed,
            SubtaskStage::Skipped {
                reason: "skip".into(),
            },
            SubtaskStage::Abandoned {
                reason: "give up".into(),
            },
        ];
        let expected_strs = vec![
            "pending",
            "blocked",
            "executing",
            "execution_failed",
            "awaiting_verification",
            "verifying",
            "verification_failed",
            "verified",
            "completed",
            "skipped",
            "abandoned",
        ];
        for (stage, expected) in stages.iter().zip(expected_strs.iter()) {
            assert_eq!(stage.as_str(), *expected, "as_str() for {:?}", stage);
        }
    }

    #[test]
    fn subtask_stage_serde_roundtrip() {
        let stage = SubtaskStage::VerificationFailed {
            results: vec![VerificationResult {
                criterion_id: "c1".into(),
                passed: false,
                evidence: "nope".into(),
                expected: "yes".into(),
                duration_ms: 100,
                error: None,
            }],
        };
        let json = serde_json::to_string(&stage).unwrap();
        let parsed: SubtaskStage = serde_json::from_str(&json).unwrap();
        assert_eq!(stage, parsed);
    }

    // ─── validate_snapshot_name tests ───────────────────────────────────────

    #[test]
    fn validate_snapshot_name_accepts_valid() {
        assert!(validate_snapshot_name("task_123_sub1_v1").is_ok());
        assert!(validate_snapshot_name("abc").is_ok());
        assert!(validate_snapshot_name("A_B_C").is_ok());
    }

    #[test]
    fn validate_snapshot_name_rejects_invalid() {
        assert!(validate_snapshot_name("").is_err());
        assert!(validate_snapshot_name("has spaces").is_err());
        assert!(validate_snapshot_name("has-dashes").is_err());
        assert!(validate_snapshot_name("path/sep").is_err());
        assert!(validate_snapshot_name("special!char").is_err());
    }

    // ─── extract_paths_from_text tests ──────────────────────────────────────

    #[test]
    fn extract_paths_finds_source_files() {
        let text = "Modified `src/main.rs` and tests/test.py for the feature";
        let paths = extract_paths_from_text(text);
        assert!(paths.contains(&"src/main.rs".to_string()));
        assert!(paths.contains(&"tests/test.py".to_string()));
    }

    #[test]
    fn extract_paths_finds_config_files() {
        let text = "Updated Cargo.toml, config.yaml and schema.json";
        let paths = extract_paths_from_text(text);
        assert!(paths.contains(&"Cargo.toml".to_string()));
        assert!(paths.contains(&"config.yaml".to_string()));
        assert!(paths.contains(&"schema.json".to_string()));
    }

    #[test]
    fn extract_paths_returns_empty_for_no_paths() {
        let text = "This is just a plain description with no file references";
        let paths = extract_paths_from_text(text);
        assert!(paths.is_empty());
    }

    // ─── build_outcome_signal tests ─────────────────────────────────────────

    #[test]
    fn build_outcome_signal_basic() {
        let contract = TaskContract {
            contract_id: "c1".into(),
            task_id: "t1".into(),
            goal: "Implement auth".into(),
            scope: TaskScope::default(),
            subtasks: vec![
                DurableSubtask {
                    id: "s1".into(),
                    title: "Auth module".into(),
                    stage: SubtaskStage::Completed,
                    criteria: vec![VerificationCriterion {
                        id: "cr1".into(),
                        description: "build passes".into(),
                        verifier: VerifierKind::BuildPass { cmd: "true".into() },
                        required: true,
                        timeout_sec: 30,
                        global_only: false,
                    }],
                    files: vec!["src/auth.rs".into()],
                    ..DurableSubtask::default()
                },
                DurableSubtask {
                    id: "s2".into(),
                    title: "Tests".into(),
                    stage: SubtaskStage::Completed,
                    criteria: vec![],
                    ..DurableSubtask::default()
                },
            ],
            global_verification: vec![],
            version: 1,
            status: ContractStatus::Completed,
            created_at: "now".into(),
            updated_at: "now".into(),
            domain_hint: None,
            task_type: None,
            last_global_results: Vec::new(),
        };
        let report = TaskDeliveryReport {
            task_id: "t1".into(),
            contract_id: "c1".into(),
            goal: "Implement auth".into(),
            subtask_summaries: vec![
                SubtaskDeliverySummary {
                    id: "s1".into(),
                    title: "Auth module".into(),
                    stage: "completed".into(),
                    criteria_passed: 1,
                    criteria_total: 1,
                    retry_count: 0,
                },
                SubtaskDeliverySummary {
                    id: "s2".into(),
                    title: "Tests".into(),
                    stage: "completed".into(),
                    criteria_passed: 0,
                    criteria_total: 0,
                    retry_count: 0,
                },
            ],
            global_verification: vec![],
            total_turns: 5,
            total_tokens: 10000,
            total_verifications: 1,
            risks: vec![],
            timestamp: "now".into(),
        };

        let signal =
            build_outcome_signal(&contract, &report, vec!["bash".into()], None, None, None);
        assert!(signal.success);
        assert_eq!(signal.goal, "Implement auth");
        assert_eq!(signal.subtask_outcomes.len(), 2);
        assert_eq!(signal.tools_used, vec!["bash"]);
        assert_eq!(signal.total_turns, 5);

        // s1 has 1 criterion, all passed → pass_rate = 1.0
        let s1 = &signal.subtask_outcomes[0];
        assert!(s1.success);
        assert!((s1.verification_pass_rate.unwrap() - 1.0).abs() < 0.001);
        assert_eq!(s1.files_modified, vec!["src/auth.rs"]);

        // s2 has no criteria → pass_rate = None
        let s2 = &signal.subtask_outcomes[1];
        assert!(s2.verification_pass_rate.is_none());
    }

    #[test]
    fn build_outcome_signal_failure_case() {
        let contract = TaskContract {
            contract_id: "c2".into(),
            task_id: "t2".into(),
            goal: "Fix bug".into(),
            scope: TaskScope::default(),
            subtasks: vec![DurableSubtask {
                id: "s1".into(),
                title: "Fix".into(),
                stage: SubtaskStage::Abandoned {
                    reason: "too hard".into(),
                },
                retry_count: 2,
                criteria: vec![
                    VerificationCriterion {
                        id: "cr1".into(),
                        description: "build".into(),
                        verifier: VerifierKind::BuildPass { cmd: "true".into() },
                        required: true,
                        timeout_sec: 30,
                        global_only: false,
                    },
                    VerificationCriterion {
                        id: "cr2".into(),
                        description: "test".into(),
                        verifier: VerifierKind::TestPass {
                            cmd: "true".into(),
                            min_pass_rate: 1.0,
                        },
                        required: true,
                        timeout_sec: 30,
                        global_only: false,
                    },
                ],
                ..DurableSubtask::default()
            }],
            global_verification: vec![],
            version: 1,
            status: ContractStatus::Active,
            created_at: "now".into(),
            updated_at: "now".into(),
            domain_hint: None,
            task_type: None,
            last_global_results: Vec::new(),
        };
        let report = TaskDeliveryReport {
            task_id: "t2".into(),
            contract_id: "c2".into(),
            goal: "Fix bug".into(),
            subtask_summaries: vec![SubtaskDeliverySummary {
                id: "s1".into(),
                title: "Fix".into(),
                stage: "abandoned".into(),
                criteria_passed: 1,
                criteria_total: 2,
                retry_count: 2,
            }],
            global_verification: vec![],
            total_turns: 10,
            total_tokens: 20000,
            total_verifications: 3,
            risks: vec![],
            timestamp: "now".into(),
        };

        let signal = build_outcome_signal(
            &contract,
            &report,
            vec![],
            Some(30),
            Some("code".into()),
            None,
        );
        assert!(!signal.success, "abandoned task should not be success");
        assert_eq!(signal.total_retries, 2);
        assert_eq!(signal.user_rating, Some(30));
        assert_eq!(signal.domain_hint, Some("code".into()));

        let s1 = &signal.subtask_outcomes[0];
        assert!(!s1.success);
        assert_eq!(s1.retry_count, 2);
        // 1 passed out of 2 criteria → 0.5
        assert!((s1.verification_pass_rate.unwrap() - 0.5).abs() < 0.001);
    }

    // ─── truncate tests ─────────────────────────────────────────────────────

    #[test]
    fn truncate_short_string_unchanged() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_string_cut() {
        let result = truncate("hello world", 5);
        assert!(result.starts_with("hello"));
        assert!(result.contains("[truncated]"));
    }

    // ─── Snapshot exclusion tests ─────────────────────────────────────────────

    #[test]
    fn snapshot_excludes_mo_session_dir() {
        let tmp = std::env::temp_dir().join(format!("snap-excl-{}", uuid::Uuid::new_v4()));
        let src = tmp.join("src");
        std::fs::create_dir_all(src.join(".mo-session/contracts/deep")).unwrap();
        std::fs::write(src.join(".mo-session/contracts/deep/data.json"), "{}").unwrap();
        std::fs::create_dir_all(src.join(".git/objects")).unwrap();
        std::fs::write(src.join(".git/objects/abc"), "blob").unwrap();
        std::fs::create_dir_all(src.join("target/debug")).unwrap();
        std::fs::write(src.join("target/debug/bin"), "elf").unwrap();
        std::fs::write(src.join("main.py"), "print('hi')").unwrap();
        std::fs::create_dir_all(src.join("lib")).unwrap();
        std::fs::write(src.join("lib/util.py"), "x=1").unwrap();

        let dst = tmp.join("snapshot");
        copy_dir_recursive(&src, &dst).unwrap();

        // Source files ARE copied
        assert!(dst.join("main.py").exists());
        assert!(dst.join("lib/util.py").exists());
        // Excluded dirs are NOT copied
        assert!(
            !dst.join(".mo-session").exists(),
            ".mo-session should be excluded"
        );
        assert!(!dst.join(".git").exists(), ".git should be excluded");
        assert!(!dst.join("target").exists(), "target should be excluded");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn snapshot_exclusion_helper() {
        assert!(is_snapshot_excluded(std::ffi::OsStr::new(".mo-session")));
        assert!(is_snapshot_excluded(std::ffi::OsStr::new(".git")));
        assert!(is_snapshot_excluded(std::ffi::OsStr::new("target")));
        assert!(is_snapshot_excluded(std::ffi::OsStr::new("node_modules")));
        assert!(is_snapshot_excluded(std::ffi::OsStr::new("__pycache__")));
        assert!(!is_snapshot_excluded(std::ffi::OsStr::new("src")));
        assert!(!is_snapshot_excluded(std::ffi::OsStr::new("lib")));
        assert!(!is_snapshot_excluded(std::ffi::OsStr::new("main.py")));
    }

    // -----------------------------------------------------------------------
    // Unhappy-path / edge-case tests
    // -----------------------------------------------------------------------

    #[test]
    fn contract_status_parse_unknown_defaults_to_draft() {
        assert_eq!(ContractStatus::parse("unknown"), ContractStatus::Draft);
        assert_eq!(ContractStatus::parse(""), ContractStatus::Draft);
        assert_eq!(ContractStatus::parse("ACTIVE"), ContractStatus::Draft); // case-sensitive
    }

    #[test]
    fn contract_status_roundtrip_all_variants() {
        for status in [
            ContractStatus::Draft,
            ContractStatus::Active,
            ContractStatus::Amended,
            ContractStatus::Completed,
            ContractStatus::Abandoned,
        ] {
            let s = status.as_str();
            assert_eq!(ContractStatus::parse(s), status);
        }
    }

    #[test]
    fn contract_status_serde_roundtrip() {
        for status in [
            ContractStatus::Draft,
            ContractStatus::Active,
            ContractStatus::Amended,
            ContractStatus::Completed,
            ContractStatus::Abandoned,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let restored: ContractStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(restored, status);
        }
    }

    #[test]
    fn contract_status_invalid_json_fails() {
        let result: Result<ContractStatus, _> = serde_json::from_str(r#""unknown_status""#);
        assert!(result.is_err());
    }

    #[test]
    fn verification_criterion_defaults() {
        let json =
            r#"{"id":"c1","description":"test","verifier":{"kind":"command","cmd":"echo ok"}}"#;
        let c: VerificationCriterion = serde_json::from_str(json).unwrap();
        assert!(c.required); // default_true
        assert_eq!(c.timeout_sec, 120); // default_timeout
        assert!(!c.global_only); // default false
    }

    #[test]
    fn verification_criterion_all_verifier_kinds_deserialize() {
        let cases = vec![
            r#"{"id":"1","description":"d","verifier":{"kind":"command","cmd":"echo"}}"#,
            r#"{"id":"2","description":"d","verifier":{"kind":"command_output","cmd":"echo","contains":["ok"]}}"#,
            r#"{"id":"3","description":"d","verifier":{"kind":"file_exists","paths":["a.rs"]}}"#,
            r#"{"id":"4","description":"d","verifier":{"kind":"grep_check","file":"a.rs","pattern":"fn main"}}"#,
            r#"{"id":"5","description":"d","verifier":{"kind":"build_pass","cmd":"cargo build"}}"#,
            r#"{"id":"6","description":"d","verifier":{"kind":"test_pass","cmd":"cargo test"}}"#,
            r#"{"id":"7","description":"d","verifier":{"kind":"llm_judge","prompt":"Is it good?"}}"#,
        ];
        for json in cases {
            let c: VerificationCriterion = serde_json::from_str(json).unwrap();
            assert!(!c.id.is_empty());
        }
    }

    #[test]
    fn verification_criterion_invalid_verifier_kind_fails() {
        let json = r#"{"id":"1","description":"d","verifier":{"kind":"nonexistent"}}"#;
        let result: Result<VerificationCriterion, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn verification_result_with_error() {
        let r = VerificationResult {
            criterion_id: "c1".into(),
            passed: false,
            evidence: "".into(),
            expected: "exit 0".into(),
            duration_ms: 500,
            error: Some("command not found".into()),
        };
        let json = serde_json::to_string(&r).unwrap();
        let loaded: VerificationResult = serde_json::from_str(&json).unwrap();
        assert!(!loaded.passed);
        assert_eq!(loaded.error.as_deref(), Some("command not found"));
    }

    #[test]
    fn verification_result_without_error_skips_field() {
        let r = VerificationResult {
            criterion_id: "c1".into(),
            passed: true,
            evidence: "ok".into(),
            expected: "ok".into(),
            duration_ms: 100,
            error: None,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("error"));
    }

    #[test]
    fn task_scope_default_empty() {
        let s = TaskScope::default();
        assert!(s.in_scope.is_empty());
        assert!(s.out_of_scope.is_empty());
        assert!(s.assumptions.is_empty());
    }

    #[test]
    fn task_scope_roundtrip() {
        let s = TaskScope {
            in_scope: vec!["auth module".into()],
            out_of_scope: vec!["UI".into()],
            assumptions: vec!["PostgreSQL available".into()],
        };
        let json = serde_json::to_string(&s).unwrap();
        let loaded: TaskScope = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.in_scope, s.in_scope);
        assert_eq!(loaded.out_of_scope, s.out_of_scope);
    }

    #[test]
    fn subtask_verification_report_roundtrip() {
        let report = SubtaskVerificationReport {
            subtask_id: "st-1".into(),
            all_required_passed: false,
            results: vec![VerificationResult {
                criterion_id: "c1".into(),
                passed: false,
                evidence: "exit 1".into(),
                expected: "exit 0".into(),
                duration_ms: 1000,
                error: Some("test failed".into()),
            }],
            timestamp: "2024-01-01T00:00:00Z".into(),
        };
        let json = serde_json::to_string(&report).unwrap();
        let loaded: SubtaskVerificationReport = serde_json::from_str(&json).unwrap();
        assert!(!loaded.all_required_passed);
        assert_eq!(loaded.results.len(), 1);
        assert_eq!(loaded.results[0].criterion_id, "c1");
    }

    #[test]
    fn verifier_kind_command_defaults() {
        let json = r#"{"kind":"command","cmd":"echo hello"}"#;
        let v: VerifierKind = serde_json::from_str(json).unwrap();
        match v {
            VerifierKind::Command { cmd, expected_exit } => {
                assert_eq!(cmd, "echo hello");
                assert_eq!(expected_exit, 0); // default
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn verifier_kind_test_pass_default_rate() {
        let json = r#"{"kind":"test_pass","cmd":"cargo test"}"#;
        let v: VerifierKind = serde_json::from_str(json).unwrap();
        match v {
            VerifierKind::TestPass { min_pass_rate, .. } => {
                assert!((min_pass_rate - 1.0).abs() < f64::EPSILON);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn verifier_kind_llm_judge_default_threshold() {
        let json = r#"{"kind":"llm_judge","prompt":"check quality"}"#;
        let v: VerifierKind = serde_json::from_str(json).unwrap();
        match v {
            VerifierKind::LlmJudge { pass_threshold, .. } => {
                assert!((pass_threshold - 0.7).abs() < f64::EPSILON);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn verifier_kind_composite_roundtrip() {
        let json = r#"{"kind":"composite","criteria":[
            {"id":"c1","description":"d","verifier":{"kind":"command","cmd":"echo"}}
        ],"require_all":false}"#;
        let v: VerifierKind = serde_json::from_str(json).unwrap();
        match v {
            VerifierKind::Composite {
                criteria,
                require_all,
            } => {
                assert_eq!(criteria.len(), 1);
                assert!(!require_all);
            }
            _ => panic!("wrong variant"),
        }
    }
}

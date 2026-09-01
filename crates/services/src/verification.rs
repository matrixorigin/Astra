//! Verification contracts and execution shared by Work, skills, and harnesses.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::{path::Path, sync::Arc};

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

/// Semantic verifier supplied by a trusted model boundary.
#[async_trait]
pub trait LlmJudge: Send + Sync {
    async fn evaluate(&self, prompt: &str, context: &str) -> Result<f64, String>;
}

/// Executes one typed verification criterion inside a fixed work directory.
pub struct VerificationRunner {
    pub work_dir: std::path::PathBuf,
    pub llm_judge: Option<Arc<dyn LlmJudge>>,
}

impl VerificationRunner {
    pub fn new(work_dir: std::path::PathBuf) -> Self {
        Self {
            work_dir,
            llm_judge: None,
        }
    }

    pub fn with_llm_judge(work_dir: std::path::PathBuf, judge: Arc<dyn LlmJudge>) -> Self {
        Self {
            work_dir,
            llm_judge: Some(judge),
        }
    }

    pub async fn run_criterion(&self, criterion: &VerificationCriterion) -> VerificationResult {
        let started = std::time::Instant::now();
        let execution = tokio::time::timeout(
            std::time::Duration::from_secs(criterion.timeout_sec as u64),
            self.execute(&criterion.verifier),
        )
        .await;
        let duration_ms = started.elapsed().as_millis() as u64;

        match execution {
            Ok(Ok((passed, evidence, expected))) => VerificationResult {
                criterion_id: criterion.id.clone(),
                passed,
                evidence,
                expected,
                duration_ms,
                error: None,
            },
            Ok(Err(error)) => VerificationResult {
                criterion_id: criterion.id.clone(),
                passed: false,
                evidence: String::new(),
                expected: String::new(),
                duration_ms,
                error: Some(error),
            },
            Err(_) => VerificationResult {
                criterion_id: criterion.id.clone(),
                passed: false,
                evidence: String::new(),
                expected: format!("completed within {}s", criterion.timeout_sec),
                duration_ms,
                error: Some("verification timed out".to_string()),
            },
        }
    }

    async fn execute(&self, verifier: &VerifierKind) -> Result<(bool, String, String), String> {
        match verifier {
            VerifierKind::Command { cmd, expected_exit } => {
                let (code, stdout, stderr) = run_command(cmd, &self.work_dir).await?;
                let evidence = combined_output(&stdout, &stderr);
                Ok((
                    code == *expected_exit,
                    truncate(&evidence, 4096),
                    format!("exit code == {expected_exit}"),
                ))
            }
            VerifierKind::CommandOutput {
                cmd,
                contains,
                not_contains,
            } => {
                let (_, stdout, _) = run_command(cmd, &self.work_dir).await?;
                let passed = contains.iter().all(|value| stdout.contains(value))
                    && not_contains.iter().all(|value| !stdout.contains(value));
                Ok((
                    passed,
                    truncate(&stdout, 4096),
                    format!("contains: {contains:?}, not_contains: {not_contains:?}"),
                ))
            }
            VerifierKind::FileExists { paths } => {
                let missing = paths
                    .iter()
                    .filter(|path| resolve_existing_path(&self.work_dir, path).is_none())
                    .cloned()
                    .collect::<Vec<_>>();
                Ok((
                    missing.is_empty(),
                    if missing.is_empty() {
                        format!("all {} files exist", paths.len())
                    } else {
                        format!("missing: {missing:?}")
                    },
                    format!("files exist: {paths:?}"),
                ))
            }
            VerifierKind::GrepCheck {
                file,
                pattern,
                should_match,
            } => {
                let path = resolve_existing_path(&self.work_dir, file)
                    .ok_or_else(|| format!("read {file}: No such file or directory"))?;
                let content = std::fs::read_to_string(path)
                    .map_err(|error| format!("read {file}: {error}"))?;
                let found = regex::Regex::new(pattern)
                    .map(|regex| regex.is_match(&content))
                    .unwrap_or_else(|_| content.contains(pattern));
                Ok((
                    found == *should_match,
                    format!(
                        "pattern '{pattern}' {} in {file}",
                        if found { "found" } else { "not found" }
                    ),
                    format!("pattern match == {should_match}"),
                ))
            }
            VerifierKind::ReadFileContains {
                path,
                contains,
                not_contains,
            } => {
                let path = resolve_existing_path(&self.work_dir, path)
                    .ok_or_else(|| format!("read {path}: No such file or directory"))?;
                let content = std::fs::read_to_string(&path)
                    .map_err(|error| format!("read {}: {error}", path.display()))?;
                let passed = contains.iter().all(|value| content.contains(value))
                    && not_contains.iter().all(|value| !content.contains(value));
                Ok((
                    passed,
                    format!(
                        "file {} content checks {}",
                        path.display(),
                        if passed { "passed" } else { "failed" }
                    ),
                    format!("contains: {contains:?}, not_contains: {not_contains:?}"),
                ))
            }
            VerifierKind::BuildPass { cmd } => {
                let (code, _, stderr) = run_command(cmd, &self.work_dir).await?;
                Ok((
                    code == 0,
                    truncate(&stderr, 4096),
                    "exit code == 0".to_string(),
                ))
            }
            VerifierKind::TestPass { cmd, min_pass_rate } => {
                let (code, stdout, stderr) = run_command(cmd, &self.work_dir).await?;
                let output = combined_output(&stdout, &stderr);
                let (passed, evidence) = match parse_test_counts(&output) {
                    Some((passed_count, failed_count)) => {
                        let total = passed_count + failed_count;
                        let ratio = if total == 0 {
                            0.0
                        } else {
                            passed_count as f64 / total as f64
                        };
                        (
                            ratio >= *min_pass_rate,
                            format!(
                                "{passed_count} passed, {failed_count} failed ({:.1}%)",
                                ratio * 100.0
                            ),
                        )
                    }
                    None => (
                        code == 0,
                        format!("exit code {code}; test counts unavailable"),
                    ),
                };
                Ok((
                    passed,
                    format!("{evidence}\n{}", truncate(&output, 3800)),
                    format!("pass rate >= {:.1}%", min_pass_rate * 100.0),
                ))
            }
            VerifierKind::LlmJudge {
                prompt,
                pass_threshold,
            } => {
                let Some(judge) = &self.llm_judge else {
                    return Err("LLM judge not configured".to_string());
                };
                let score = judge.evaluate(prompt, &self.judge_context()).await?;
                if !score.is_finite() || !(0.0..=1.0).contains(&score) {
                    return Err("LLM judge returned an invalid score".to_string());
                }
                Ok((
                    score >= *pass_threshold,
                    format!("LLM score: {score:.2}"),
                    format!("score >= {pass_threshold:.2}"),
                ))
            }
            VerifierKind::Composite {
                criteria,
                require_all,
            } => {
                let mut results = Vec::with_capacity(criteria.len());
                for criterion in criteria {
                    results.push(Box::pin(self.run_criterion(criterion)).await);
                }
                let passed = if *require_all {
                    results.iter().all(|result| result.passed)
                } else {
                    results.iter().any(|result| result.passed)
                };
                Ok((
                    passed,
                    results
                        .iter()
                        .map(|result| format!("{}={}", result.criterion_id, result.passed))
                        .collect::<Vec<_>>()
                        .join(", "),
                    format!(
                        "{} of {} criteria",
                        if *require_all { "all" } else { "any" },
                        criteria.len()
                    ),
                ))
            }
        }
    }

    fn judge_context(&self) -> String {
        let entries = std::fs::read_dir(&self.work_dir)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .take(30)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        format!(
            "Work directory: {}\nEntries: {}",
            self.work_dir.display(),
            entries.join(", ")
        )
    }
}

async fn run_command(cmd: &str, work_dir: &Path) -> Result<(i32, String, String), String> {
    let cmd = cmd.to_string();
    let work_dir = work_dir.to_path_buf();
    let task = tokio::task::spawn_blocking(move || {
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(work_dir)
            .output()
            .map_err(|error| format!("command failed: {error}"))?;
        Ok::<_, String>((
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ))
    });
    tokio::time::timeout(std::time::Duration::from_secs(600), task)
        .await
        .map_err(|_| "command timed out after 600s".to_string())?
        .map_err(|error| format!("command task failed: {error}"))?
}

fn resolve_existing_path(root: &Path, value: &str) -> Option<std::path::PathBuf> {
    let requested = Path::new(value);
    let root = root.canonicalize().ok()?;
    if requested.is_absolute() {
        let canonical = requested.canonicalize().ok()?;
        return canonical.starts_with(&root).then_some(canonical);
    }
    let direct = root.join(requested);
    if direct.exists() {
        let canonical = direct.canonicalize().ok()?;
        return canonical.starts_with(&root).then_some(canonical);
    }
    let file_name = requested.file_name()?;
    let mut pending = vec![root.clone()];
    let mut visited = 0usize;
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory).ok()?.flatten() {
            visited += 1;
            if visited > 5_000 {
                return None;
            }
            let path = entry.path();
            if path.is_dir() {
                if !matches!(
                    entry.file_name().to_str(),
                    Some(".git" | "target" | "node_modules" | "dist" | "build")
                ) {
                    pending.push(path);
                }
            } else if entry.file_name() == file_name {
                let canonical = path.canonicalize().ok()?;
                if canonical.starts_with(&root) {
                    return Some(canonical);
                }
            }
        }
    }
    None
}

fn combined_output(stdout: &str, stderr: &str) -> String {
    if stderr.is_empty() {
        stdout.to_string()
    } else {
        format!("{stdout}\n--- stderr ---\n{stderr}")
    }
}

fn parse_test_counts(output: &str) -> Option<(u64, u64)> {
    output.lines().rev().find_map(|line| {
        let passed = number_before(line, "passed");
        let failed = number_before(line, "failed");
        (passed.is_some() || failed.is_some()).then_some((passed.unwrap_or(0), failed.unwrap_or(0)))
    })
}

fn number_before(line: &str, marker: &str) -> Option<u64> {
    let prefix = line.split(marker).next()?;
    prefix
        .split(|character: char| !character.is_ascii_digit())
        .rfind(|part| !part.is_empty())?
        .parse()
        .ok()
}

fn truncate(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        value.to_string()
    } else {
        format!(
            "{}…[truncated]",
            value.chars().take(max_chars).collect::<String>()
        )
    }
}

#[cfg(test)]
mod runner_tests {
    use super::*;

    struct InvalidJudge;

    #[async_trait]
    impl LlmJudge for InvalidJudge {
        async fn evaluate(&self, _prompt: &str, _context: &str) -> Result<f64, String> {
            Ok(1.5)
        }
    }

    fn criterion(verifier: VerifierKind) -> VerificationCriterion {
        VerificationCriterion {
            id: "gate".to_string(),
            description: "gate".to_string(),
            verifier,
            required: true,
            timeout_sec: 5,
            global_only: false,
        }
    }

    #[tokio::test]
    async fn file_evidence_cannot_escape_the_bound_work_directory() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        let runner = VerificationRunner::new(root.path().to_path_buf());
        let result = runner
            .run_criterion(&criterion(VerifierKind::ReadFileContains {
                path: outside.path().display().to_string(),
                contains: Vec::new(),
                not_contains: Vec::new(),
            }))
            .await;

        assert!(!result.passed);
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn semantic_judge_score_must_be_finite_probability() {
        let root = tempfile::tempdir().unwrap();
        let runner =
            VerificationRunner::with_llm_judge(root.path().to_path_buf(), Arc::new(InvalidJudge));
        let result = runner
            .run_criterion(&criterion(VerifierKind::LlmJudge {
                prompt: "verify".to_string(),
                pass_threshold: 0.9,
            }))
            .await;

        assert!(!result.passed);
        assert_eq!(
            result.error.as_deref(),
            Some("LLM judge returned an invalid score")
        );
    }
}

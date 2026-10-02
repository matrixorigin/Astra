//! Acceptance-check contracts and the canonical skill verification runner.

use serde::{Deserialize, Serialize};
use std::path::Path;

// ─── Verification Criterion ─────────────────────────────────────────────────

/// Machine-executable acceptance criterion.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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

/// Executes one typed verification criterion inside a fixed work directory.
pub struct VerificationRunner {
    pub work_dir: std::path::PathBuf,
}

impl VerificationRunner {
    pub fn new(work_dir: std::path::PathBuf) -> Self {
        Self { work_dir }
    }

    /// Parse the complete batch before observing files. Command criteria must
    /// run through an authorized tool provider, never through this runner.
    pub async fn run_criteria(
        &self,
        values: &[serde_json::Value],
    ) -> (bool, Vec<VerificationResult>) {
        let criteria: Vec<VerificationCriterion> = match values
            .iter()
            .map(|value| serde_json::from_value(value.clone()))
            .collect::<Result<_, _>>()
        {
            Ok(criteria) => criteria,
            Err(error) => {
                return (
                    false,
                    vec![VerificationResult {
                        criterion_id: "invalid_criteria".into(),
                        passed: false,
                        evidence: String::new(),
                        expected: "valid verification criteria".into(),
                        duration_ms: 0,
                        error: Some(error.to_string()),
                    }],
                );
            }
        };
        let mut results = Vec::with_capacity(criteria.len());
        for criterion in &criteria {
            if verifier_requires_shell(&criterion.verifier) {
                results.push(VerificationResult {
                    criterion_id: criterion.id.clone(),
                    passed: false,
                    evidence: String::new(),
                    expected: "verification without shell execution".into(),
                    duration_ms: 0,
                    error: Some(
                        "command-backed verification requires an authorized tool provider".into(),
                    ),
                });
            } else {
                results.push(self.run_criterion(criterion).await);
            }
        }
        let passed = criteria
            .iter()
            .zip(&results)
            .all(|(criterion, result)| !criterion.required || result.passed);
        (passed, results)
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
            VerifierKind::Command { .. }
            | VerifierKind::CommandOutput { .. }
            | VerifierKind::BuildPass { .. }
            | VerifierKind::TestPass { .. } => {
                Err("command-backed verification requires an authorized tool provider".into())
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
}

fn verifier_requires_shell(verifier: &VerifierKind) -> bool {
    match verifier {
        VerifierKind::Command { .. }
        | VerifierKind::CommandOutput { .. }
        | VerifierKind::BuildPass { .. }
        | VerifierKind::TestPass { .. } => true,
        VerifierKind::Composite { criteria, .. } => criteria
            .iter()
            .any(|criterion| verifier_requires_shell(&criterion.verifier)),
        VerifierKind::FileExists { .. }
        | VerifierKind::GrepCheck { .. }
        | VerifierKind::ReadFileContains { .. } => false,
    }
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

#[cfg(test)]
mod runner_tests {
    use super::*;

    fn criterion(verifier: VerifierKind) -> VerificationCriterion {
        VerificationCriterion {
            id: "gate".to_string(),
            description: "gate".to_string(),
            verifier,
            required: true,
            timeout_sec: 5,
        }
    }

    #[tokio::test]
    async fn verification_batches_preserve_required_checks_without_shell_execution() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("output.txt"), "ready").unwrap();
        let runner = VerificationRunner::new(root.path().to_path_buf());
        {
            assert_eq!(runner.run_criteria(&[]).await, (true, Vec::new()));
            for (path, required, expected) in [
                ("output.txt", true, true),
                ("missing", true, false),
                ("missing", false, true),
            ] {
                let mut check = criterion(VerifierKind::FileExists {
                    paths: vec![path.into()],
                });
                check.required = required;
                let (passed, results) = runner
                    .run_criteria(&[serde_json::to_value(check).unwrap()])
                    .await;
                assert_eq!(passed, expected);
                assert_eq!(results.len(), 1);
                assert_eq!(results[0].passed, path == "output.txt");
            }
        }
        for verifier in [
            VerifierKind::Command {
                cmd: "touch marker".into(),
                expected_exit: 0,
            },
            VerifierKind::Composite {
                criteria: vec![criterion(VerifierKind::Command {
                    cmd: "touch marker".into(),
                    expected_exit: 0,
                })],
                require_all: true,
            },
        ] {
            let (passed, results) = runner
                .run_criteria(&[serde_json::to_value(criterion(verifier)).unwrap()])
                .await;
            assert!(!passed);
            assert!(results[0].error.is_some());
            assert!(!root.path().join("marker").exists());
        }
        let check = criterion(VerifierKind::CommandOutput {
            cmd: "printf ready".into(),
            contains: vec!["ready".into()],
            not_contains: vec!["failed".into()],
        });
        let (passed, results) = runner
            .run_criteria(&[serde_json::to_value(check).unwrap()])
            .await;
        assert!(!passed && results[0].error.is_some());
    }

    #[tokio::test]
    async fn malformed_batch_cannot_skip_validation_and_execute_a_valid_prefix() {
        let root = tempfile::tempdir().unwrap();
        let runner = VerificationRunner::new(root.path().to_path_buf());
        let valid = serde_json::to_value(criterion(VerifierKind::FileExists {
            paths: vec!["missing".into()],
        }))
        .unwrap();
        for invalid in [
            serde_json::Value::Null,
            serde_json::json!({"id": "unknown", "description": "unsupported verifier", "verifier": {"kind": "unknown"}}),
        ] {
            {
                let (passed, results) =
                    runner.run_criteria(&[valid.clone(), invalid.clone()]).await;
                assert!(!passed);
                assert!(
                    results
                        .iter()
                        .any(|result| result.error.is_some() && !result.passed)
                );
                assert_eq!(results.len(), 1);
                assert_eq!(results[0].criterion_id, "invalid_criteria");
            }
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
}

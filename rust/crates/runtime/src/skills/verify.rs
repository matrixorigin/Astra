//! Skill-level verification — runs success criteria after skill execution.
//!
//! Wraps the existing `VerificationRunner` from `astra_services::durable_task`
//! to provide skill-specific verification without duplicating any verification logic.

use std::path::PathBuf;
use std::sync::Arc;

use astra_services::{VerificationCriterion, VerificationResult, VerificationRunner, VerifierKind};

use super::manifest::SkillManifest;

/// Runs verification criteria declared in a skill manifest.
///
/// Reuses `VerificationRunner` from durable tasks — same 8 verifier kinds,
/// same timeout handling, same LLM judge integration.
pub struct SkillVerifier {
    runner: VerificationRunner,
}

impl SkillVerifier {
    /// Create a verifier for the given working directory.
    pub fn new(work_dir: PathBuf) -> Self {
        Self {
            runner: VerificationRunner::new(work_dir),
        }
    }

    /// Create a verifier with LLM judge support for semantic checks.
    pub fn with_llm_judge(work_dir: PathBuf, judge: Arc<dyn astra_services::LlmJudge>) -> Self {
        Self {
            runner: VerificationRunner::with_llm_judge(work_dir, judge),
        }
    }

    /// Run all success criteria declared in the skill manifest.
    ///
    /// Returns `(all_required_passed, results)`.
    /// If the manifest has no criteria, returns `(true, [])`.
    pub async fn verify(&self, manifest: &SkillManifest) -> (bool, Vec<VerificationResult>) {
        if manifest.success_criteria.is_empty() {
            return (true, Vec::new());
        }
        self.verify_criteria(&manifest.success_criteria).await
    }

    /// Run a specific set of criteria.
    ///
    /// Returns `(all_required_passed, results)`.
    pub async fn verify_criteria(
        &self,
        criteria: &[VerificationCriterion],
    ) -> (bool, Vec<VerificationResult>) {
        let mut results = Vec::with_capacity(criteria.len());

        for criterion in criteria {
            // Skip LlmJudge if no judge is configured
            if matches!(criterion.verifier, VerifierKind::LlmJudge { .. })
                && self.runner.llm_judge.is_none()
            {
                results.push(VerificationResult {
                    criterion_id: criterion.id.clone(),
                    passed: !criterion.required, // skip advisory, fail required
                    evidence: String::new(),
                    expected: "LLM judge evaluation".to_string(),
                    duration_ms: 0,
                    error: Some("LLM judge not configured, skipped".to_string()),
                });
                continue;
            }
            results.push(self.runner.run_criterion(criterion).await);
        }

        let all_required_passed = criteria
            .iter()
            .zip(results.iter())
            .all(|(c, r)| !c.required || r.passed);

        (all_required_passed, results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_empty_criteria_passes() {
        let manifest = SkillManifest::default();
        let verifier = SkillVerifier::new(PathBuf::from("/tmp"));
        let (passed, results) = verifier.verify(&manifest).await;
        assert!(passed);
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_file_exists_criterion() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("output.txt");
        std::fs::File::create(&file_path)
            .unwrap()
            .write_all(b"hello")
            .unwrap();

        let mut manifest = SkillManifest::default();
        manifest.success_criteria.push(VerificationCriterion {
            id: "output-exists".to_string(),
            description: "Output file must exist".to_string(),
            verifier: VerifierKind::FileExists {
                paths: vec![file_path.to_string_lossy().to_string()],
            },
            required: true,
            timeout_sec: 10,
            global_only: false,
        });

        let verifier = SkillVerifier::new(dir.path().to_path_buf());
        let (passed, results) = verifier.verify(&manifest).await;
        assert!(passed);
        assert_eq!(results.len(), 1);
        assert!(results[0].passed);
    }

    #[tokio::test]
    async fn test_required_criterion_fails() {
        let dir = TempDir::new().unwrap();

        let mut manifest = SkillManifest::default();
        manifest.success_criteria.push(VerificationCriterion {
            id: "missing-file".to_string(),
            description: "File must exist".to_string(),
            verifier: VerifierKind::FileExists {
                paths: vec![
                    dir.path()
                        .join("nonexistent.txt")
                        .to_string_lossy()
                        .to_string(),
                ],
            },
            required: true,
            timeout_sec: 10,
            global_only: false,
        });

        let verifier = SkillVerifier::new(dir.path().to_path_buf());
        let (passed, results) = verifier.verify(&manifest).await;
        assert!(!passed);
        assert!(!results[0].passed);
    }

    #[tokio::test]
    async fn test_advisory_criterion_doesnt_block() {
        let dir = TempDir::new().unwrap();

        let mut manifest = SkillManifest::default();
        manifest.success_criteria.push(VerificationCriterion {
            id: "advisory-check".to_string(),
            description: "Nice to have".to_string(),
            verifier: VerifierKind::FileExists {
                paths: vec![
                    dir.path()
                        .join("optional.txt")
                        .to_string_lossy()
                        .to_string(),
                ],
            },
            required: false, // advisory
            timeout_sec: 10,
            global_only: false,
        });

        let verifier = SkillVerifier::new(dir.path().to_path_buf());
        let (passed, results) = verifier.verify(&manifest).await;
        assert!(passed); // advisory failure doesn't block
        assert!(!results[0].passed); // but it's still recorded as failed
    }

    #[tokio::test]
    async fn test_command_output_criterion() {
        let dir = TempDir::new().unwrap();

        let mut manifest = SkillManifest::default();
        manifest.success_criteria.push(VerificationCriterion {
            id: "echo-check".to_string(),
            description: "Echo contains expected text".to_string(),
            verifier: VerifierKind::CommandOutput {
                cmd: "echo 'all tests passed'".to_string(),
                contains: vec!["tests passed".to_string()],
                not_contains: vec![],
            },
            required: true,
            timeout_sec: 10,
            global_only: false,
        });

        let verifier = SkillVerifier::new(dir.path().to_path_buf());
        let (passed, results) = verifier.verify(&manifest).await;
        assert!(passed);
        assert!(results[0].passed);
    }
}

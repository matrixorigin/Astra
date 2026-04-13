//! Contract Generator — converts TaskPlan → TaskContract.
//!
//! SubtaskPlan now carries structured `acceptance_checks: Vec<VerifierKind>` directly
//! from the LLM decomposition. This module wraps them in `VerificationCriterion` with
//! sensible defaults (required, timeout) and adds project-level global checks
//! (build, test, lint).
//!
//! # Usage
//!
//! ```ignore
//! let cg = ContractGenerator::new(project_root);
//! let contract = cg.generate("Implement auth API", &plan, None)?;
//! ```

use uuid::Uuid;

use crate::durable_task::{
    ContractStatus, DurableSubtask, SubtaskStage, TaskContract, TaskScope, VerificationCriterion,
    VerifierKind,
};
use crate::task_orchestrator::{SubtaskPlan, TaskPlan};

// ─── Project Detection ──────────────────────────────────────────────────────

/// Minimal project context needed for command detection.
/// Accepts data from runtime's ProjectContext or from simple file-existence checks.
#[derive(Debug, Clone, Default)]
pub struct ProjectDetection {
    pub has_cargo_toml: bool,
    pub has_package_json: bool,
    pub has_pyproject_toml: bool,
    pub has_setup_py: bool,
    pub has_go_mod: bool,
    pub has_makefile: bool,
    pub has_jest_config: bool,
    /// True when `test_*.py` or `*_test.py` files exist (bare Python project)
    pub has_test_py: bool,
    /// Override: if set, used as-is
    pub build_cmd_override: Option<String>,
    /// Override: if set, used as-is
    pub test_cmd_override: Option<String>,
    /// Override: if set, used as-is
    pub lint_cmd_override: Option<String>,
}

impl ProjectDetection {
    /// Auto-detect from filesystem at given root.
    pub fn detect(root: &std::path::Path) -> Self {
        let has_test_py = root
            .read_dir()
            .ok()
            .map(|entries| {
                entries.filter_map(|e| e.ok()).any(|e| {
                    let n = e.file_name();
                    let name = n.to_string_lossy();
                    (name.starts_with("test_") || name.ends_with("_test.py"))
                        && name.ends_with(".py")
                })
            })
            .unwrap_or(false);
        Self {
            has_cargo_toml: root.join("Cargo.toml").exists(),
            has_package_json: root.join("package.json").exists(),
            has_pyproject_toml: root.join("pyproject.toml").exists(),
            has_setup_py: root.join("setup.py").exists(),
            has_go_mod: root.join("go.mod").exists(),
            has_makefile: root.join("Makefile").exists(),
            has_jest_config: root.join("jest.config.js").exists()
                || root.join("jest.config.ts").exists(),
            has_test_py,
            build_cmd_override: None,
            test_cmd_override: None,
            lint_cmd_override: None,
        }
    }
}

/// Infer the project build command from detection results.
pub fn detect_build_command(det: &ProjectDetection) -> Option<String> {
    if let Some(ref cmd) = det.build_cmd_override {
        return Some(cmd.clone());
    }
    if det.has_cargo_toml {
        Some("cargo build".into())
    } else if det.has_package_json {
        Some("npm run build".into())
    } else if det.has_go_mod {
        Some("go build ./...".into())
    } else if det.has_makefile {
        Some("make".into())
    } else {
        None
    }
}

/// Infer the project test command from detection results.
pub fn detect_test_command(det: &ProjectDetection) -> Option<String> {
    if let Some(ref cmd) = det.test_cmd_override {
        return Some(cmd.clone());
    }
    if det.has_cargo_toml {
        Some("cargo test --workspace".into())
    } else if det.has_package_json {
        if det.has_jest_config {
            Some("npx jest".into())
        } else {
            Some("npm test".into())
        }
    } else if det.has_pyproject_toml || det.has_setup_py || det.has_test_py {
        Some("pytest".into())
    } else if det.has_go_mod {
        Some("go test ./...".into())
    } else {
        None
    }
}

/// Infer the project lint command from detection results.
pub fn detect_lint_command(det: &ProjectDetection) -> Option<String> {
    if let Some(ref cmd) = det.lint_cmd_override {
        return Some(cmd.clone());
    }
    if det.has_cargo_toml {
        Some("cargo clippy --workspace -- -D warnings".into())
    } else if det.has_package_json {
        Some("npx eslint .".into())
    } else if det.has_pyproject_toml || det.has_setup_py {
        Some("ruff check .".into())
    } else if det.has_go_mod {
        Some("golangci-lint run".into())
    } else {
        None
    }
}

// ─── VerifierKind → VerificationCriterion ───────────────────────────────────

/// Convert `acceptance_checks` from a subtask plan into `VerificationCriterion`s.
///
/// Filters out `Command`/`CommandOutput` verifiers (security: prevent arbitrary
/// command execution from LLM output).  When `checks` is empty, falls back to a
/// `FileExists` criterion if `files` is non-empty.
pub fn acceptance_checks_to_criteria(
    subtask_id: &str,
    checks: &[VerifierKind],
    files: &[String],
) -> Vec<VerificationCriterion> {
    if checks.is_empty() {
        if files.is_empty() {
            Vec::new()
        } else {
            vec![VerificationCriterion {
                id: format!("{subtask_id}-files"),
                description: format!("Modified files exist: {}", files.join(", ")),
                verifier: VerifierKind::FileExists {
                    paths: files.iter().map(|p| sanitize_criteria_path(p)).collect(),
                },
                required: true,
                timeout_sec: 10,
                global_only: false,
            }]
        }
    } else {
        checks
            .iter()
            .filter(|vk| {
                !matches!(
                    vk,
                    VerifierKind::Command { .. } | VerifierKind::CommandOutput { .. }
                )
            })
            .enumerate()
            .map(|(i, vk)| {
                wrap_verifier(
                    format!("{subtask_id}-ac{i}"),
                    sanitize_verifier_paths(vk.clone()),
                )
            })
            .collect()
    }
}

/// Strip `tmp/`, `/tmp/`, and leading `/` from paths in criteria to keep them
/// project-relative. LLMs frequently hallucinate these prefixes.
fn sanitize_criteria_path(path: &str) -> String {
    let p = path
        .strip_prefix("/tmp/")
        .or_else(|| path.strip_prefix("tmp/"))
        .unwrap_or(path);
    // Also strip leading `/` to make absolute paths relative
    p.strip_prefix('/').unwrap_or(p).to_string()
}

/// Apply path sanitization to all path fields inside a `VerifierKind`.
fn sanitize_verifier_paths(vk: VerifierKind) -> VerifierKind {
    match vk {
        VerifierKind::FileExists { paths } => VerifierKind::FileExists {
            paths: paths.iter().map(|p| sanitize_criteria_path(p)).collect(),
        },
        VerifierKind::GrepCheck {
            file,
            pattern,
            should_match,
        } => VerifierKind::GrepCheck {
            file: sanitize_criteria_path(&file),
            pattern,
            should_match,
        },
        VerifierKind::ReadFileContains {
            path,
            contains,
            not_contains,
        } => VerifierKind::ReadFileContains {
            path: sanitize_criteria_path(&path),
            contains,
            not_contains,
        },
        other => other,
    }
}

/// Wrap a `VerifierKind` into a `VerificationCriterion` with sensible defaults.
fn wrap_verifier(id: String, verifier: VerifierKind) -> VerificationCriterion {
    let global_only = matches!(
        verifier,
        VerifierKind::BuildPass { .. }
            | VerifierKind::TestPass { .. }
            | VerifierKind::LlmJudge { .. }
    );
    let timeout_sec = match &verifier {
        VerifierKind::BuildPass { .. } | VerifierKind::TestPass { .. } => 600,
        VerifierKind::Command { .. } | VerifierKind::CommandOutput { .. } => 120,
        _ => 30,
    };
    let description = describe_verifier(&verifier);
    VerificationCriterion {
        id,
        description,
        verifier,
        required: true,
        timeout_sec,
        global_only,
    }
}

fn describe_verifier(v: &VerifierKind) -> String {
    match v {
        VerifierKind::FileExists { paths } => format!("Files exist: {}", paths.join(", ")),
        VerifierKind::ReadFileContains { path, contains, .. } => {
            format!("{path} contains {:?}", contains)
        }
        VerifierKind::GrepCheck { file, pattern, .. } => format!("grep '{pattern}' in {file}"),
        VerifierKind::Command { cmd, .. } => format!("Command succeeds: {cmd}"),
        VerifierKind::CommandOutput { cmd, contains, .. } => {
            format!("{cmd} output contains {:?}", contains)
        }
        VerifierKind::BuildPass { cmd } => format!("Build passes: {cmd}"),
        VerifierKind::TestPass { cmd, .. } => format!("Tests pass: {cmd}"),
        VerifierKind::LlmJudge { prompt, .. } => prompt.clone(),
        VerifierKind::Composite { .. } => "Composite check".into(),
    }
}

// ─── Contract Generator ─────────────────────────────────────────────────────

/// Generates a [`TaskContract`] from a [`TaskPlan`] with auto-detected verification.
pub struct ContractGenerator {
    detection: ProjectDetection,
}

impl ContractGenerator {
    /// Create from auto-detected project settings.
    pub fn from_path(root: &std::path::Path) -> Self {
        Self {
            detection: ProjectDetection::detect(root),
        }
    }

    /// Create with explicit detection settings.
    pub fn new(detection: ProjectDetection) -> Self {
        Self { detection }
    }

    /// Generate a TaskContract from a TaskPlan.
    ///
    /// - `goal`: the user's original goal text
    /// - `plan`: the decomposed plan
    /// - `scope`: optional explicit scope; if None, inferred from subtask titles
    pub fn generate(
        &self,
        goal: &str,
        plan: &TaskPlan,
        scope: Option<TaskScope>,
    ) -> Result<TaskContract, String> {
        let contract_id = Uuid::new_v4().to_string();
        let task_id = Uuid::new_v4().to_string();
        let now = chrono::Utc::now().to_rfc3339();

        let subtasks: Vec<DurableSubtask> = plan
            .subtasks
            .iter()
            .map(|sp| self.convert_subtask(sp))
            .collect();

        let scope = scope.unwrap_or_else(|| self.infer_scope(goal, &subtasks));
        let global_verification = self.generate_global_criteria();

        Ok(TaskContract {
            contract_id,
            task_id,
            goal: goal.to_string(),
            scope,
            subtasks,
            global_verification,
            version: 1,
            status: ContractStatus::Draft,
            created_at: now.clone(),
            updated_at: now,
            domain_hint: None,
            task_type: None,
            last_global_results: Vec::new(),
        })
    }

    /// Convert a SubtaskPlan into a DurableSubtask.
    ///
    /// Wraps each `VerifierKind` from `acceptance_checks` into a `VerificationCriterion`.
    /// If no checks are provided but `files` are listed, adds a `FileExists` check.
    fn convert_subtask(&self, sp: &SubtaskPlan) -> DurableSubtask {
        let criteria = acceptance_checks_to_criteria(&sp.id, &sp.acceptance_checks, &sp.files);

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
    }

    /// Infer task scope from goal and subtasks.
    fn infer_scope(&self, goal: &str, subtasks: &[DurableSubtask]) -> TaskScope {
        let in_scope: Vec<String> = subtasks.iter().map(|s| s.title.clone()).collect();

        let mut out_of_scope = Vec::new();
        let goal_lower = goal.to_lowercase();
        if !goal_lower.contains("deploy") {
            out_of_scope.push("Deployment and CI/CD changes".into());
        }
        if !goal_lower.contains("doc") && !goal_lower.contains("readme") {
            out_of_scope.push("Documentation updates".into());
        }

        let mut assumptions = vec!["Project builds successfully before changes".into()];
        if self.detection.has_cargo_toml {
            assumptions.push("Rust toolchain is installed and working".into());
        }
        if self.detection.has_package_json {
            assumptions.push("Node.js/npm is installed and dependencies are available".into());
        }

        TaskScope {
            in_scope,
            out_of_scope,
            assumptions,
        }
    }

    /// Generate standard global verification criteria based on the project type.
    fn generate_global_criteria(&self) -> Vec<VerificationCriterion> {
        let mut criteria = Vec::new();

        if let Some(cmd) = detect_build_command(&self.detection) {
            criteria.push(VerificationCriterion {
                id: "global-build".into(),
                description: "Full project builds without errors".into(),
                verifier: VerifierKind::BuildPass { cmd },
                required: true,
                timeout_sec: 600,
                global_only: true,
            });
        }

        if let Some(cmd) = detect_test_command(&self.detection) {
            criteria.push(VerificationCriterion {
                id: "global-test".into(),
                description: "All existing tests pass".into(),
                verifier: VerifierKind::TestPass {
                    cmd,
                    min_pass_rate: 1.0,
                },
                required: true,
                timeout_sec: 600,
                global_only: true,
            });
        }

        if let Some(cmd) = detect_lint_command(&self.detection) {
            criteria.push(VerificationCriterion {
                id: "global-lint".into(),
                description: "No new lint errors introduced".into(),
                verifier: VerifierKind::Command {
                    cmd,
                    expected_exit: 0,
                },
                required: false,
                timeout_sec: 300,
                global_only: true,
            });
        }

        criteria
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_orchestrator::TaskStatus;

    fn rust_detection() -> ProjectDetection {
        ProjectDetection {
            has_cargo_toml: true,
            ..Default::default()
        }
    }

    fn node_detection() -> ProjectDetection {
        ProjectDetection {
            has_package_json: true,
            has_jest_config: true,
            ..Default::default()
        }
    }

    fn make_plan() -> TaskPlan {
        TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "auth-module".into(),
                    title: "Create auth module".into(),
                    description: Some("JWT-based auth".into()),
                    depends_on: vec![],
                    status: TaskStatus::Pending,
                    effort: Some("medium".into()),
                    files: vec!["src/auth.rs".into()],
                    acceptance_checks: vec![
                        VerifierKind::TestPass {
                            cmd: "cargo test --workspace".into(),
                            min_pass_rate: 1.0,
                        },
                        VerifierKind::FileExists {
                            paths: vec!["src/auth.rs".into()],
                        },
                    ],
                },
                SubtaskPlan {
                    id: "api-routes".into(),
                    title: "Add API routes".into(),
                    description: Some("REST endpoints for login/logout".into()),
                    depends_on: vec!["auth-module".into()],
                    status: TaskStatus::Pending,
                    effort: Some("large".into()),
                    files: vec!["src/routes.rs".into()],
                    acceptance_checks: vec![VerifierKind::BuildPass {
                        cmd: "cargo build".into(),
                    }],
                },
            ],
            notes: Some("Use JWT for auth".into()),
        }
    }

    // ─── Project detection tests ────────────────────────────────────────────

    #[test]
    fn detect_build_cmd_rust() {
        let det = rust_detection();
        assert_eq!(detect_build_command(&det), Some("cargo build".into()));
        assert_eq!(
            detect_test_command(&det),
            Some("cargo test --workspace".into())
        );
        assert_eq!(
            detect_lint_command(&det),
            Some("cargo clippy --workspace -- -D warnings".into())
        );
    }

    #[test]
    fn detect_build_cmd_node() {
        let det = node_detection();
        assert_eq!(detect_build_command(&det), Some("npm run build".into()));
        assert_eq!(detect_test_command(&det), Some("npx jest".into()));
        assert_eq!(detect_lint_command(&det), Some("npx eslint .".into()));
    }

    #[test]
    fn detect_build_cmd_override() {
        let det = ProjectDetection {
            has_cargo_toml: true,
            build_cmd_override: Some("make build".into()),
            test_cmd_override: Some("make test".into()),
            ..Default::default()
        };
        assert_eq!(detect_build_command(&det), Some("make build".into()));
        assert_eq!(detect_test_command(&det), Some("make test".into()));
    }

    #[test]
    fn detect_build_cmd_python() {
        let det = ProjectDetection {
            has_pyproject_toml: true,
            ..Default::default()
        };
        assert_eq!(detect_build_command(&det), None);
        assert_eq!(detect_test_command(&det), Some("pytest".into()));
        assert_eq!(detect_lint_command(&det), Some("ruff check .".into()));
    }

    #[test]
    fn detect_test_cmd_bare_python() {
        let det = ProjectDetection {
            has_test_py: true,
            ..Default::default()
        };
        assert_eq!(detect_build_command(&det), None);
        assert_eq!(detect_test_command(&det), Some("pytest".into()));
    }

    #[test]
    fn detect_bare_python_from_filesystem() {
        let tmp = std::env::temp_dir().join(format!("detect-py-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("main.py"), "def add(a,b): return a+b\n").unwrap();
        std::fs::write(tmp.join("test_main.py"), "def test_add(): pass\n").unwrap();
        let det = ProjectDetection::detect(&tmp);
        assert!(det.has_test_py, "should detect test_*.py files");
        assert_eq!(detect_test_command(&det), Some("pytest".into()));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn detect_build_cmd_go() {
        let det = ProjectDetection {
            has_go_mod: true,
            ..Default::default()
        };
        assert_eq!(detect_build_command(&det), Some("go build ./...".into()));
        assert_eq!(detect_test_command(&det), Some("go test ./...".into()));
        assert_eq!(detect_lint_command(&det), Some("golangci-lint run".into()));
    }

    // ─── Contract generation tests ──────────────────────────────────────────

    #[test]
    fn generate_contract_from_plan() {
        let cg = ContractGenerator::new(rust_detection());
        let plan = make_plan();
        let contract = cg.generate("Implement user auth API", &plan, None).unwrap();

        assert_eq!(contract.goal, "Implement user auth API");
        assert_eq!(contract.subtasks.len(), 2);
        assert_eq!(contract.status, ContractStatus::Draft);
        assert_eq!(contract.version, 1);

        let s0 = &contract.subtasks[0];
        assert_eq!(s0.id, "auth-module");
        assert_eq!(s0.stage, SubtaskStage::Pending);
        assert_eq!(s0.criteria.len(), 2);

        let has_test_verifier = s0
            .criteria
            .iter()
            .any(|c| matches!(c.verifier, VerifierKind::TestPass { .. }));
        assert!(has_test_verifier, "should have TestPass verifier");

        let has_file_verifier = s0.criteria.iter().any(|c| {
            matches!(&c.verifier, VerifierKind::FileExists { paths } if paths.contains(&"src/auth.rs".to_string()))
        });
        assert!(has_file_verifier, "should have FileExists verifier");

        let s1 = &contract.subtasks[1];
        assert_eq!(s1.depends_on, vec!["auth-module"]);
        let has_build_verifier = s1
            .criteria
            .iter()
            .any(|c| matches!(c.verifier, VerifierKind::BuildPass { .. }));
        assert!(has_build_verifier, "should have BuildPass verifier");

        assert!(contract.global_verification.len() >= 2);
        let global_ids: Vec<&str> = contract
            .global_verification
            .iter()
            .map(|c| c.id.as_str())
            .collect();
        assert!(global_ids.contains(&"global-build"));
        assert!(global_ids.contains(&"global-test"));
    }

    #[test]
    fn generate_contract_with_explicit_scope() {
        let cg = ContractGenerator::new(rust_detection());
        let plan = make_plan();
        let scope = TaskScope {
            in_scope: vec!["Auth only".into()],
            out_of_scope: vec!["Rate limiting".into()],
            assumptions: vec!["DB is running".into()],
        };
        let contract = cg.generate("Auth", &plan, Some(scope)).unwrap();
        assert_eq!(contract.scope.in_scope, vec!["Auth only"]);
        assert_eq!(contract.scope.out_of_scope, vec!["Rate limiting"]);
    }

    #[test]
    fn generate_contract_no_checks_with_files() {
        let cg = ContractGenerator::new(rust_detection());
        let plan = TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "s1".into(),
                title: "Do thing".into(),
                files: vec!["src/thing.rs".into()],
                ..Default::default()
            }],
            notes: None,
        };
        let contract = cg.generate("Do thing", &plan, None).unwrap();
        let s0 = &contract.subtasks[0];
        assert_eq!(s0.criteria.len(), 1);
        assert!(matches!(
            s0.criteria[0].verifier,
            VerifierKind::FileExists { .. }
        ));
    }

    #[test]
    fn generate_contract_no_checks_no_files() {
        let cg = ContractGenerator::new(rust_detection());
        let plan = TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "s1".into(),
                title: "Do thing".into(),
                ..Default::default()
            }],
            notes: None,
        };
        let contract = cg.generate("Do thing", &plan, None).unwrap();
        let s0 = &contract.subtasks[0];
        assert!(s0.criteria.is_empty());
    }

    #[test]
    fn generate_contract_empty_plan() {
        let cg = ContractGenerator::new(rust_detection());
        let plan = TaskPlan {
            subtasks: vec![],
            notes: None,
        };
        let contract = cg.generate("Empty", &plan, None).unwrap();
        assert!(contract.subtasks.is_empty());
        assert!(!contract.global_verification.is_empty());
    }

    // ─── wrap_verifier tests ────────────────────────────────────────────────

    #[test]
    fn wrap_verifier_sets_global_only_for_build_and_test() {
        let c = wrap_verifier(
            "t1".into(),
            VerifierKind::BuildPass {
                cmd: "cargo build".into(),
            },
        );
        assert!(c.global_only);
        assert_eq!(c.timeout_sec, 600);

        let c = wrap_verifier(
            "t2".into(),
            VerifierKind::TestPass {
                cmd: "cargo test".into(),
                min_pass_rate: 1.0,
            },
        );
        assert!(c.global_only);
    }

    #[test]
    fn wrap_verifier_local_for_file_checks() {
        let c = wrap_verifier(
            "t1".into(),
            VerifierKind::FileExists {
                paths: vec!["a.rs".into()],
            },
        );
        assert!(!c.global_only);
        assert_eq!(c.timeout_sec, 30);

        let c = wrap_verifier(
            "t2".into(),
            VerifierKind::GrepCheck {
                file: "a.rs".into(),
                pattern: "fn main".into(),
                should_match: true,
            },
        );
        assert!(!c.global_only);
    }

    #[test]
    fn wrap_verifier_command_gets_120s_timeout() {
        let c = wrap_verifier(
            "t1".into(),
            VerifierKind::Command {
                cmd: "echo ok".into(),
                expected_exit: 0,
            },
        );
        assert_eq!(c.timeout_sec, 120);
        assert!(!c.global_only);
    }

    // ─── Scope inference tests ──────────────────────────────────────────────

    #[test]
    fn inferred_scope_excludes_deploy_and_docs() {
        let cg = ContractGenerator::new(rust_detection());
        let subtasks = vec![DurableSubtask {
            id: "s1".into(),
            title: "Add feature".into(),
            ..Default::default()
        }];
        let scope = cg.infer_scope("add a feature", &subtasks);
        assert!(scope.out_of_scope.iter().any(|s| s.contains("Deployment")));
        assert!(
            scope
                .out_of_scope
                .iter()
                .any(|s| s.contains("Documentation"))
        );
        assert!(
            scope
                .assumptions
                .iter()
                .any(|s| s.contains("Rust toolchain"))
        );
    }

    #[test]
    fn inferred_scope_includes_deploy_when_goal_mentions_it() {
        let cg = ContractGenerator::new(rust_detection());
        let subtasks = vec![];
        let scope = cg.infer_scope("deploy the service", &subtasks);
        assert!(!scope.out_of_scope.iter().any(|s| s.contains("Deployment")));
    }

    // ─── describe_verifier tests ────────────────────────────────────────────

    #[test]
    fn convert_subtask_filters_command_variants() {
        let cg = ContractGenerator::new(rust_detection());
        let plan = TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "s1".into(),
                title: "Do thing".into(),
                acceptance_checks: vec![
                    VerifierKind::FileExists {
                        paths: vec!["a.rs".into()],
                    },
                    VerifierKind::Command {
                        cmd: "rm -rf /".into(),
                        expected_exit: 0,
                    },
                    VerifierKind::CommandOutput {
                        cmd: "cat /etc/passwd".into(),
                        contains: vec!["root".into()],
                        not_contains: vec![],
                    },
                    VerifierKind::GrepCheck {
                        file: "a.rs".into(),
                        pattern: "fn main".into(),
                        should_match: true,
                    },
                ],
                ..Default::default()
            }],
            notes: None,
        };
        let contract = cg.generate("Do thing", &plan, None).unwrap();
        let s0 = &contract.subtasks[0];
        assert_eq!(
            s0.criteria.len(),
            2,
            "Command and CommandOutput should be filtered"
        );
        assert!(matches!(
            s0.criteria[0].verifier,
            VerifierKind::FileExists { .. }
        ));
        assert!(matches!(
            s0.criteria[1].verifier,
            VerifierKind::GrepCheck { .. }
        ));
    }

    #[test]
    fn verifier_kind_serde_roundtrip() {
        let checks = vec![
            VerifierKind::FileExists {
                paths: vec!["src/lib.rs".into()],
            },
            VerifierKind::ReadFileContains {
                path: "src/lib.rs".into(),
                contains: vec!["pub fn".into()],
                not_contains: vec![],
            },
            VerifierKind::GrepCheck {
                file: "src/lib.rs".into(),
                pattern: "fn main".into(),
                should_match: true,
            },
            VerifierKind::BuildPass {
                cmd: "cargo build".into(),
            },
            VerifierKind::TestPass {
                cmd: "cargo test".into(),
                min_pass_rate: 1.0,
            },
        ];
        for vk in &checks {
            let json = serde_json::to_string(vk).unwrap();
            let roundtripped: VerifierKind = serde_json::from_str(&json).unwrap();
            let json2 = serde_json::to_string(&roundtripped).unwrap();
            assert_eq!(json, json2, "roundtrip failed for {json}");
        }
    }

    #[test]
    fn describe_verifier_coverage() {
        let d = describe_verifier(&VerifierKind::FileExists {
            paths: vec!["a.rs".into()],
        });
        assert!(d.contains("a.rs"));

        let d = describe_verifier(&VerifierKind::GrepCheck {
            file: "b.rs".into(),
            pattern: "fn main".into(),
            should_match: true,
        });
        assert!(d.contains("fn main") && d.contains("b.rs"));

        let d = describe_verifier(&VerifierKind::ReadFileContains {
            path: "c.rs".into(),
            contains: vec!["hello".into()],
            not_contains: vec![],
        });
        assert!(d.contains("c.rs") && d.contains("hello"));
    }

    #[test]
    fn sanitize_criteria_path_strips_tmp_prefix() {
        assert_eq!(sanitize_criteria_path("tmp/app.js"), "app.js");
        assert_eq!(sanitize_criteria_path("/tmp/app.js"), "app.js");
        assert_eq!(sanitize_criteria_path("/tmp/src/main.rs"), "src/main.rs");
        assert_eq!(sanitize_criteria_path("src/app.js"), "src/app.js");
        assert_eq!(sanitize_criteria_path("/usr/local/bin"), "usr/local/bin");
    }

    #[test]
    fn sanitize_verifier_paths_cleans_all_variants() {
        let vk = VerifierKind::GrepCheck {
            file: "tmp/index.html".into(),
            pattern: "hello".into(),
            should_match: true,
        };
        let cleaned = sanitize_verifier_paths(vk);
        match cleaned {
            VerifierKind::GrepCheck { file, .. } => assert_eq!(file, "index.html"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn sanitize_verifier_paths_passthrough_non_path_variants() {
        let vk = VerifierKind::BuildPass {
            cmd: "cargo build".into(),
        };
        let cleaned = sanitize_verifier_paths(vk.clone());
        assert!(matches!(cleaned, VerifierKind::BuildPass { .. }));
    }

    #[test]
    fn acceptance_checks_to_criteria_sanitizes_file_paths() {
        let criteria = acceptance_checks_to_criteria(
            "s1",
            &[],
            &["tmp/app.js".into(), "/tmp/index.html".into()],
        );
        assert_eq!(criteria.len(), 1);
        match &criteria[0].verifier {
            VerifierKind::FileExists { paths } => {
                assert_eq!(paths, &["app.js", "index.html"]);
            }
            _ => panic!("expected FileExists"),
        }
    }

    #[test]
    fn acceptance_checks_to_criteria_sanitizes_check_paths() {
        let checks = vec![VerifierKind::GrepCheck {
            file: "/tmp/src/main.rs".into(),
            pattern: "fn main".into(),
            should_match: true,
        }];
        let criteria = acceptance_checks_to_criteria("s1", &checks, &[]);
        match &criteria[0].verifier {
            VerifierKind::GrepCheck { file, .. } => {
                assert_eq!(file, "src/main.rs");
            }
            _ => panic!("expected GrepCheck"),
        }
    }
}

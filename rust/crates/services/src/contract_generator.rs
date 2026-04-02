//! Contract Generator — converts TaskPlan → TaskContract with auto-detected verification.
//!
//! The generator bridges the gap between the human-oriented plan (SubtaskPlan with string
//! acceptance criteria) and the machine-executable contract (DurableSubtask with structured
//! VerificationCriterion + VerifierKind).
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
        Self {
            has_cargo_toml: root.join("Cargo.toml").exists(),
            has_package_json: root.join("package.json").exists(),
            has_pyproject_toml: root.join("pyproject.toml").exists(),
            has_setup_py: root.join("setup.py").exists(),
            has_go_mod: root.join("go.mod").exists(),
            has_makefile: root.join("Makefile").exists(),
            has_jest_config: root.join("jest.config.js").exists()
                || root.join("jest.config.ts").exists(),
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
    } else if det.has_pyproject_toml || det.has_setup_py {
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

// ─── Acceptance Parsing ─────────────────────────────────────────────────────

/// Parse a human-written acceptance string into structured verification criteria.
///
/// Supports several patterns:
/// - "tests pass" → TestPass verifier
/// - "builds successfully" → BuildPass verifier
/// - "file X exists" → FileExists verifier
/// - "output contains X" → CommandOutput verifier
/// - Anything else → LlmJudge (semantic check)
fn parse_acceptance_to_criteria(
    acceptance: &str,
    subtask_id: &str,
    det: &ProjectDetection,
) -> Vec<VerificationCriterion> {
    let mut criteria = Vec::new();
    let lower = acceptance.to_lowercase();

    // Split by common delimiters: newlines, semicolons, numbered items
    let parts: Vec<&str> = acceptance
        .split('\n')
        .flat_map(|line| line.split(';'))
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();

    for (idx, part) in parts.iter().enumerate() {
        let part_lower = part.to_lowercase();
        let crit_id = format!("{subtask_id}-ac{idx}");

        if let Some(c) = try_parse_test_criterion(&crit_id, part, &part_lower, det) {
            criteria.push(c);
        } else if let Some(c) = try_parse_build_criterion(&crit_id, part, &part_lower, det) {
            criteria.push(c);
        } else if let Some(c) = try_parse_no_warnings_criterion(&crit_id, part, &part_lower, det) {
            criteria.push(c);
        } else if let Some(c) = try_parse_grep_criterion(&crit_id, part, &part_lower) {
            criteria.push(c);
        } else if let Some(c) = try_parse_function_exists_criterion(&crit_id, part, &part_lower) {
            criteria.push(c);
        } else if let Some(c) = try_parse_file_exists_criterion(&crit_id, part, &part_lower) {
            criteria.push(c);
        } else if let Some(c) = try_parse_command_output_criterion(&crit_id, part, &part_lower) {
            criteria.push(c);
        } else if let Some(c) = try_parse_permission_criterion(&crit_id, part, &part_lower) {
            criteria.push(c);
        } else if let Some(c) = try_parse_flexible_grep_criterion(&crit_id, part, &part_lower) {
            criteria.push(c);
        } else {
            // Fallback: LLM judge for semantic criteria (deferred — not yet implemented)
            criteria.push(VerificationCriterion {
                id: crit_id,
                description: part.to_string(),
                verifier: VerifierKind::LlmJudge {
                    prompt: format!(
                        "Evaluate whether the following acceptance criterion is met: \"{part}\". \
                         Respond with a score from 0.0 to 1.0."
                    ),
                    pass_threshold: 0.7,
                },
                required: !lower.contains("optional"),
                timeout_sec: 60,
                global_only: true, // LlmJudge not yet implemented; skip in per-subtask
            });
        }
    }

    // If nothing parsed (empty acceptance), add a generic LLM check
    if criteria.is_empty() && !acceptance.trim().is_empty() {
        criteria.push(VerificationCriterion {
            id: format!("{subtask_id}-ac0"),
            description: acceptance.to_string(),
            verifier: VerifierKind::LlmJudge {
                prompt: format!(
                    "Evaluate whether the following criterion is met: \"{acceptance}\". \
                     Score 0.0 to 1.0."
                ),
                pass_threshold: 0.7,
            },
            required: true,
            timeout_sec: 60,
            global_only: true, // LlmJudge not yet implemented; skip in per-subtask
        });
    }

    criteria
}

fn try_parse_test_criterion(
    id: &str,
    desc: &str,
    lower: &str,
    det: &ProjectDetection,
) -> Option<VerificationCriterion> {
    let is_test = lower.contains("test")
        && (lower.contains("pass") || lower.contains("succeed") || lower.contains("green"));
    if !is_test {
        return None;
    }
    let cmd = detect_test_command(det)?;
    Some(VerificationCriterion {
        id: id.to_string(),
        description: desc.to_string(),
        verifier: VerifierKind::TestPass {
            cmd,
            min_pass_rate: 1.0,
        },
        required: true,
        timeout_sec: 300,
        global_only: true, // Expensive: only run during global verification
    })
}

fn try_parse_build_criterion(
    id: &str,
    desc: &str,
    lower: &str,
    det: &ProjectDetection,
) -> Option<VerificationCriterion> {
    let is_build = (lower.contains("build") || lower.contains("compile"))
        && (lower.contains("pass")
            || lower.contains("succeed")
            || lower.contains("success")
            || lower.contains("without error"));
    if !is_build {
        return None;
    }
    let cmd = detect_build_command(det)?;
    Some(VerificationCriterion {
        id: id.to_string(),
        description: desc.to_string(),
        verifier: VerifierKind::BuildPass { cmd },
        required: true,
        timeout_sec: 300,
        global_only: true, // Expensive: only run during global verification
    })
}

fn try_parse_file_exists_criterion(
    id: &str,
    desc: &str,
    lower: &str,
) -> Option<VerificationCriterion> {
    // Match patterns like "file src/auth.rs exists" or "create src/auth.rs"
    if !lower.contains("file") && !lower.contains("create") && !lower.contains("exist") {
        return None;
    }

    let paths = extract_file_paths(desc);
    if paths.is_empty() {
        return None;
    }

    Some(VerificationCriterion {
        id: id.to_string(),
        description: desc.to_string(),
        verifier: VerifierKind::FileExists { paths },
        required: true,
        timeout_sec: 10,
        global_only: false, // lightweight — run per-subtask
    })
}

fn try_parse_grep_criterion(
    id: &str,
    desc: &str,
    lower: &str,
) -> Option<VerificationCriterion> {
    // Match "contains X in file Y" or "file Y should contain X"
    let has_contain = lower.contains("contain") || lower.contains("include");
    let has_file_ref = lower.contains("in file") || lower.contains("in the file");
    if !has_contain || !has_file_ref {
        return None;
    }

    // Best-effort extraction: look for quoted strings and file paths
    let paths = extract_file_paths(desc);
    let quoted = extract_quoted_strings(desc);

    if paths.is_empty() || quoted.is_empty() {
        return None;
    }

    Some(VerificationCriterion {
        id: id.to_string(),
        description: desc.to_string(),
        verifier: VerifierKind::GrepCheck {
            file: paths[0].clone(),
            pattern: quoted[0].clone(),
            should_match: !lower.contains("not contain") && !lower.contains("should not"),
        },
        required: true,
        timeout_sec: 10,
        global_only: false, // lightweight — run per-subtask
    })
}

/// Match "Command X outputs 'Y'" or "Running X produces 'Y'" patterns.
///
/// Generates a `CommandOutput` verifier that runs the command and checks stdout
/// contains the expected string. Covers acceptance criteria like:
/// - "Command /tmp/hellosh outputs 'hello china'"
/// - "Running the script produces 'hello world!' output"
/// - "/tmp/foo prints 'bar'"
fn try_parse_command_output_criterion(
    id: &str,
    desc: &str,
    lower: &str,
) -> Option<VerificationCriterion> {
    let has_output_keyword = lower.contains("output")
        || lower.contains("produce")
        || lower.contains("print")
        || lower.contains("return");
    let has_run_keyword = lower.contains("run")
        || lower.contains("execut")
        || lower.contains("command")
        || lower.contains("script");

    if !has_output_keyword && !has_run_keyword {
        return None;
    }

    // Need at least a file path (the command) OR a quoted expected output
    let paths = extract_file_paths(desc);
    let quoted = extract_quoted_strings(desc);

    if paths.is_empty() && quoted.is_empty() {
        return None;
    }

    // Build the command: prefer file path, fallback to first word after "run"/"execute"
    let cmd = if !paths.is_empty() {
        paths[0].clone()
    } else {
        return None; // Can't determine what command to run
    };

    // Build expected output check — exclude the command path itself from expected output
    let contains: Vec<String> = quoted
        .iter()
        .filter(|q| *q != &cmd && !paths.contains(q))
        .cloned()
        .collect();
    let not_contains = if lower.contains("without error") || lower.contains("no error") {
        vec!["error".to_string()]
    } else {
        vec![]
    };

    // Only create verifier if we have something to check
    if contains.is_empty() && not_contains.is_empty() {
        return None;
    }

    Some(VerificationCriterion {
        id: id.to_string(),
        description: desc.to_string(),
        verifier: VerifierKind::CommandOutput {
            cmd,
            contains,
            not_contains,
        },
        required: true,
        timeout_sec: 30,
        global_only: false, // lightweight — run per-subtask
    })
}

/// Match permission/executable criteria.
///
/// Generates a `Command` verifier using `test -x <file>` (exit 0 = executable).
/// Covers acceptance criteria like:
/// - "Script has executable permissions (ls -l shows x bits)"
/// - "ls -l shows executable permissions"
/// - "File is executable"
fn try_parse_permission_criterion(
    id: &str,
    desc: &str,
    lower: &str,
) -> Option<VerificationCriterion> {
    let has_perm_keyword = lower.contains("permission")
        || lower.contains("executable")
        || lower.contains("chmod")
        || lower.contains("x bit");

    if !has_perm_keyword {
        return None;
    }

    let paths = extract_file_paths(desc);
    if paths.is_empty() {
        return None;
    }

    Some(VerificationCriterion {
        id: id.to_string(),
        description: desc.to_string(),
        verifier: VerifierKind::Command {
            cmd: format!("test -x {}", paths[0]),
            expected_exit: 0,
        },
        required: true,
        timeout_sec: 10,
        global_only: false, // lightweight — run per-subtask
    })
}

/// Match "no warnings" / "zero warnings" / "clean compile" patterns.
///
/// Uses the project's build command with a pipe to `grep -c warning` or similar.
/// Covers acceptance criteria like:
/// - "No compiler warnings"
/// - "Code compiles with zero warnings"
/// - "Clean build (no warnings)"
fn try_parse_no_warnings_criterion(
    id: &str,
    desc: &str,
    lower: &str,
    det: &ProjectDetection,
) -> Option<VerificationCriterion> {
    let has_warning = lower.contains("warning");
    let has_negative = lower.contains("no ")
        || lower.contains("zero")
        || lower.contains("0 ")
        || lower.contains("without")
        || lower.contains("clean");

    if !has_warning || !has_negative {
        return None;
    }

    let build_cmd = detect_build_command(det)?;

    // Build a command that fails if warnings are present in stderr.
    // Redirect stderr to stdout so we can grep both streams.
    let cmd = format!("{build_cmd} 2>&1 | grep -ci 'warning' | grep -q '^0$'");

    Some(VerificationCriterion {
        id: id.to_string(),
        description: desc.to_string(),
        verifier: VerifierKind::Command {
            cmd,
            expected_exit: 0,
        },
        required: true,
        timeout_sec: 300,
        global_only: true, // build is expensive — global only
    })
}

/// Match "Function X exists in file Y" or "Module exports X" patterns.
///
/// Generates a `GrepCheck` that searches for the symbol name in the specified file.
/// Covers acceptance criteria like:
/// - "Function authenticate exists in src/auth.rs"
/// - "src/config.ts exports createUser"
/// - "File src/models.py defines class User"
fn try_parse_function_exists_criterion(
    id: &str,
    desc: &str,
    lower: &str,
) -> Option<VerificationCriterion> {
    let has_symbol_kind = lower.contains("function")
        || lower.contains("class")
        || lower.contains("struct")
        || lower.contains("enum")
        || lower.contains("trait")
        || lower.contains("interface")
        || lower.contains("const ")
        || lower.contains("export");
    let has_existence = lower.contains("exist")
        || lower.contains("define")
        || lower.contains("declare")
        || lower.contains("export")
        || lower.contains("has a ");

    if !has_symbol_kind || !has_existence {
        return None;
    }

    let paths = extract_file_paths(desc);
    if paths.is_empty() {
        return None;
    }

    // Try to find the symbol name: first from quoted strings, then from words
    // immediately following symbol keywords.
    let quoted = extract_quoted_strings(desc);
    let symbol = if !quoted.is_empty() {
        quoted[0].clone()
    } else {
        // Look for the word right after "function"/"class"/"struct" etc.
        let keywords = [
            "function ", "class ", "struct ", "enum ", "trait ", "interface ", "const ",
        ];
        let mut found = None;
        for kw in &keywords {
            if let Some(pos) = lower.find(kw) {
                let after = &desc[pos + kw.len()..];
                if let Some(word) = after.split_whitespace().next() {
                    let clean = word.trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
                    if !clean.is_empty() {
                        found = Some(clean.to_string());
                        break;
                    }
                }
            }
        }
        found?
    };

    Some(VerificationCriterion {
        id: id.to_string(),
        description: desc.to_string(),
        verifier: VerifierKind::GrepCheck {
            file: paths[0].clone(),
            pattern: symbol,
            should_match: true,
        },
        required: true,
        timeout_sec: 10,
        global_only: false,
    })
}

/// Flexible grep: matches "file X should have Y", "X includes Y", "Y in X" etc.
///
/// This is a looser variant of `try_parse_grep_criterion` that doesn't require
/// the exact "in file" / "contain" keywords. It catches patterns like:
/// - "src/config.ts includes the string 'API_KEY'"
/// - "Makefile should have a 'test' target"
/// - "File src/models.py should have 'class User'"
fn try_parse_flexible_grep_criterion(
    id: &str,
    desc: &str,
    lower: &str,
) -> Option<VerificationCriterion> {
    let paths = extract_file_paths(desc);
    let quoted = extract_quoted_strings(desc);

    // Need both a file and a pattern to search for
    if paths.is_empty() || quoted.is_empty() {
        return None;
    }

    // Must have some verb indicating containment/presence
    let has_verb = lower.contains("has ")
        || lower.contains("have ")
        || lower.contains("include")
        || lower.contains("contain")
        || lower.contains("should")
        || lower.contains("with ");

    if !has_verb {
        return None;
    }

    let should_match = !lower.contains("not have")
        && !lower.contains("should not")
        && !lower.contains("shouldn't")
        && !lower.contains("not contain")
        && !lower.contains("not include");

    // Filter out quoted strings that are themselves file paths (avoid using the path as a grep pattern)
    let pattern_candidates: Vec<&String> = quoted.iter().filter(|q| !paths.contains(q)).collect();
    if pattern_candidates.is_empty() {
        return None;
    }

    Some(VerificationCriterion {
        id: id.to_string(),
        description: desc.to_string(),
        verifier: VerifierKind::GrepCheck {
            file: paths[0].clone(),
            pattern: pattern_candidates[0].clone(),
            should_match,
        },
        required: true,
        timeout_sec: 10,
        global_only: false,
    })
}

/// Extract file paths from a string (heuristic: words containing '/' or known extensions/names).
fn extract_file_paths(text: &str) -> Vec<String> {
    let extensions = [
        ".rs", ".py", ".ts", ".tsx", ".js", ".jsx", ".go", ".java", ".rb", ".cpp", ".c", ".h",
        ".toml", ".yaml", ".yml", ".json", ".sql", ".md", ".txt", ".sh",
    ];
    // Well-known filenames without extensions
    let known_names = [
        "Makefile", "Dockerfile", "Vagrantfile", "Gemfile", "Rakefile", "Procfile",
        "CMakeLists", "Justfile",
    ];

    text.split_whitespace()
        .map(|w| w.trim_matches(|c: char| c == '`' || c == '\'' || c == '"' || c == ','))
        .filter(|w| {
            w.contains('/')
                || w.contains('\\')
                || extensions.iter().any(|ext| w.ends_with(ext))
                || known_names.iter().any(|name| *w == *name)
        })
        .map(String::from)
        .collect()
}

/// Extract quoted strings (single or double quotes).
fn extract_quoted_strings(text: &str) -> Vec<String> {
    let mut results = Vec::new();
    for delim in ['"', '\'', '`'] {
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            if c == delim {
                let s: String = chars.by_ref().take_while(|&ch| ch != delim).collect();
                if !s.is_empty() {
                    results.push(s);
                }
            }
        }
    }
    results
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

        // Convert SubtaskPlan → DurableSubtask
        let subtasks: Vec<DurableSubtask> = plan
            .subtasks
            .iter()
            .map(|sp| self.convert_subtask(sp))
            .collect();

        // Build scope
        let scope = scope.unwrap_or_else(|| self.infer_scope(goal, &subtasks));

        // Generate global verification criteria
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
        })
    }

    /// Convert a SubtaskPlan into a DurableSubtask with structured verification.
    fn convert_subtask(&self, sp: &SubtaskPlan) -> DurableSubtask {
        let criteria = match &sp.acceptance {
            Some(acc) if !acc.trim().is_empty() => {
                parse_acceptance_to_criteria(acc, &sp.id, &self.detection)
            }
            _ => {
                // No acceptance string: add file-based criteria if files are specified
                let mut c = Vec::new();
                if !sp.files.is_empty() {
                    c.push(VerificationCriterion {
                        id: format!("{}-files", sp.id),
                        description: format!(
                            "Modified files exist: {}",
                            sp.files.join(", ")
                        ),
                        verifier: VerifierKind::FileExists {
                            paths: sp.files.clone(),
                        },
                        required: true,
                        timeout_sec: 10,
                        global_only: false, // lightweight — run per-subtask
                    });
                }
                c
            }
        };

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
        }
    }

    /// Infer task scope from goal and subtasks.
    fn infer_scope(&self, goal: &str, subtasks: &[DurableSubtask]) -> TaskScope {
        let in_scope: Vec<String> = subtasks.iter().map(|s| s.title.clone()).collect();

        let mut out_of_scope = Vec::new();
        let goal_lower = goal.to_lowercase();
        // Common things people might expect but aren't in the plan
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

        // Global build check
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

        // Global test check
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

        // Global lint check (non-blocking)
        if let Some(cmd) = detect_lint_command(&self.detection) {
            criteria.push(VerificationCriterion {
                id: "global-lint".into(),
                description: "No new lint errors introduced".into(),
                verifier: VerifierKind::Command {
                    cmd,
                    expected_exit: 0,
                },
                required: false, // advisory, not blocking
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
                    acceptance: Some("tests pass; file src/auth.rs exists".into()),
                },
                SubtaskPlan {
                    id: "api-routes".into(),
                    title: "Add API routes".into(),
                    description: Some("REST endpoints for login/logout".into()),
                    depends_on: vec!["auth-module".into()],
                    status: TaskStatus::Pending,
                    effort: Some("large".into()),
                    files: vec!["src/routes.rs".into()],
                    acceptance: Some("build succeeds; endpoint responds correctly".into()),
                },
            ],
            notes: Some("Use JWT for auth".into()),
        }
    }

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
    fn detect_build_cmd_go() {
        let det = ProjectDetection {
            has_go_mod: true,
            ..Default::default()
        };
        assert_eq!(detect_build_command(&det), Some("go build ./...".into()));
        assert_eq!(detect_test_command(&det), Some("go test ./...".into()));
        assert_eq!(
            detect_lint_command(&det),
            Some("golangci-lint run".into())
        );
    }

    #[test]
    fn generate_contract_from_plan() {
        let cg = ContractGenerator::new(rust_detection());
        let plan = make_plan();
        let contract = cg.generate("Implement user auth API", &plan, None).unwrap();

        assert_eq!(contract.goal, "Implement user auth API");
        assert_eq!(contract.subtasks.len(), 2);
        assert_eq!(contract.status, ContractStatus::Draft);
        assert_eq!(contract.version, 1);

        // First subtask should have parsed acceptance criteria
        let s0 = &contract.subtasks[0];
        assert_eq!(s0.id, "auth-module");
        assert_eq!(s0.stage, SubtaskStage::Pending);
        assert!(!s0.criteria.is_empty(), "should have acceptance criteria");

        // Check that "tests pass" was parsed into TestPass verifier
        let has_test_verifier = s0.criteria.iter().any(|c| {
            matches!(c.verifier, VerifierKind::TestPass { .. })
        });
        assert!(
            has_test_verifier,
            "should parse 'tests pass' into TestPass verifier"
        );

        // Check that "file src/auth.rs exists" was parsed into FileExists
        let has_file_verifier = s0.criteria.iter().any(|c| {
            matches!(&c.verifier, VerifierKind::FileExists { paths } if paths.contains(&"src/auth.rs".to_string()))
        });
        assert!(
            has_file_verifier,
            "should parse 'file exists' into FileExists verifier"
        );

        // Second subtask: "build succeeds" → BuildPass
        let s1 = &contract.subtasks[1];
        assert_eq!(s1.depends_on, vec!["auth-module"]);
        let has_build_verifier = s1.criteria.iter().any(|c| {
            matches!(c.verifier, VerifierKind::BuildPass { .. })
        });
        assert!(
            has_build_verifier,
            "should parse 'build succeeds' into BuildPass verifier"
        );

        // Global verification should include build + test + lint
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
    fn generate_contract_no_acceptance() {
        let cg = ContractGenerator::new(rust_detection());
        let plan = TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "s1".into(),
                title: "Do thing".into(),
                files: vec!["src/thing.rs".into()],
                acceptance: None,
                ..Default::default()
            }],
            notes: None,
        };
        let contract = cg.generate("Do thing", &plan, None).unwrap();
        // With no acceptance but files listed, should get FileExists criterion
        let s0 = &contract.subtasks[0];
        assert_eq!(s0.criteria.len(), 1);
        assert!(matches!(
            s0.criteria[0].verifier,
            VerifierKind::FileExists { .. }
        ));
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

    #[test]
    fn parse_acceptance_test_patterns() {
        let det = rust_detection();
        let criteria = parse_acceptance_to_criteria("all tests pass", "s1", &det);
        assert_eq!(criteria.len(), 1);
        assert!(matches!(
            criteria[0].verifier,
            VerifierKind::TestPass { .. }
        ));
    }

    #[test]
    fn parse_acceptance_build_patterns() {
        let det = rust_detection();
        let criteria = parse_acceptance_to_criteria("build succeeds without errors", "s1", &det);
        assert_eq!(criteria.len(), 1);
        assert!(matches!(
            criteria[0].verifier,
            VerifierKind::BuildPass { .. }
        ));
    }

    #[test]
    fn parse_acceptance_file_exists_pattern() {
        let det = rust_detection();
        let criteria = parse_acceptance_to_criteria("file src/auth.rs exists", "s1", &det);
        assert_eq!(criteria.len(), 1);
        assert!(matches!(
            &criteria[0].verifier,
            VerifierKind::FileExists { paths } if paths == &["src/auth.rs"]
        ));
    }

    #[test]
    fn parse_acceptance_multi_criterion() {
        let det = rust_detection();
        let criteria = parse_acceptance_to_criteria(
            "tests pass; build succeeds; file src/new.rs exists",
            "s1",
            &det,
        );
        assert_eq!(criteria.len(), 3);
        assert!(matches!(criteria[0].verifier, VerifierKind::TestPass { .. }));
        assert!(matches!(
            criteria[1].verifier,
            VerifierKind::BuildPass { .. }
        ));
        assert!(matches!(
            criteria[2].verifier,
            VerifierKind::FileExists { .. }
        ));
    }

    #[test]
    fn parse_acceptance_fallback_to_llm_judge() {
        let det = rust_detection();
        let criteria =
            parse_acceptance_to_criteria("code follows existing patterns", "s1", &det);
        assert_eq!(criteria.len(), 1);
        assert!(matches!(
            criteria[0].verifier,
            VerifierKind::LlmJudge { .. }
        ));
    }

    #[test]
    fn parse_acceptance_grep_pattern() {
        let det = rust_detection();
        let criteria = parse_acceptance_to_criteria(
            "should contain 'pub fn authenticate' in file src/auth.rs",
            "s1",
            &det,
        );
        // Should detect grep pattern
        assert!(!criteria.is_empty());
        let has_grep = criteria
            .iter()
            .any(|c| matches!(c.verifier, VerifierKind::GrepCheck { .. }));
        assert!(has_grep, "should parse grep pattern");
    }

    #[test]
    fn extract_file_paths_works() {
        let paths = extract_file_paths("modify src/auth.rs and tests/auth_test.rs");
        assert_eq!(paths, vec!["src/auth.rs", "tests/auth_test.rs"]);
    }

    #[test]
    fn extract_file_paths_with_quotes() {
        let paths = extract_file_paths("create `src/new.rs` file");
        assert_eq!(paths, vec!["src/new.rs"]);
    }

    #[test]
    fn extract_quoted_strings_works() {
        let quoted = extract_quoted_strings("contains 'hello world' in file");
        assert_eq!(quoted, vec!["hello world"]);
    }

    #[test]
    fn inferred_scope_excludes_deploy_and_docs() {
        let cg = ContractGenerator::new(rust_detection());
        let subtasks = vec![DurableSubtask {
            id: "s1".into(),
            title: "Add feature".into(),
            ..Default::default()
        }];
        let scope = cg.infer_scope("add a feature", &subtasks);
        assert!(scope
            .out_of_scope
            .iter()
            .any(|s| s.contains("Deployment")));
        assert!(scope
            .out_of_scope
            .iter()
            .any(|s| s.contains("Documentation")));
        assert!(scope
            .assumptions
            .iter()
            .any(|s| s.contains("Rust toolchain")));
    }

    #[test]
    fn inferred_scope_includes_deploy_when_goal_mentions_it() {
        let cg = ContractGenerator::new(rust_detection());
        let subtasks = vec![];
        let scope = cg.infer_scope("deploy the service", &subtasks);
        assert!(!scope
            .out_of_scope
            .iter()
            .any(|s| s.contains("Deployment")));
    }

    #[test]
    fn parse_helloworld_acceptance_text() {
        let det = ProjectDetection::default();
        // Subtask 1: "File exists at /tmp/helloworld.sh containing ..."
        let c1 = parse_acceptance_to_criteria(
            "File exists at /tmp/helloworld.sh containing 'echo \"hello world!\"' or similar",
            "create-script",
            &det,
        );
        assert!(
            !c1.is_empty(),
            "should parse at least one criterion for 'File exists ...'"
        );
        let has_file = c1
            .iter()
            .any(|c| matches!(&c.verifier, VerifierKind::FileExists { .. }));
        assert!(has_file, "should detect FileExists verifier for acceptance text with 'file exists'");

        // Subtask 2: "ls -l shows executable permissions" → Permission verifier
        let c2 = parse_acceptance_to_criteria(
            "Script has executable permissions (ls -l shows x bits) on /tmp/helloworld.sh",
            "make-exec",
            &det,
        );
        assert!(!c2.is_empty(), "should have at least one criterion");
        let has_perm = c2
            .iter()
            .any(|c| matches!(&c.verifier, VerifierKind::Command { .. }));
        assert!(has_perm, "should detect Command verifier for permission check");
        assert!(!c2[0].global_only, "permission check should run per-subtask");

        // Subtask 3: "Running the script produces 'hello world!'" → CommandOutput
        let c3 = parse_acceptance_to_criteria(
            "Running /tmp/helloworld.sh produces 'hello world!' output without errors",
            "verify-exec",
            &det,
        );
        assert!(!c3.is_empty(), "should have at least one criterion");
        let has_cmd_output = c3
            .iter()
            .any(|c| matches!(&c.verifier, VerifierKind::CommandOutput { .. }));
        assert!(has_cmd_output, "should detect CommandOutput verifier for script execution check");
        assert!(!c3[0].global_only, "command output check should run per-subtask");
    }

    #[test]
    fn parse_command_output_criterion() {
        let det = ProjectDetection::default();

        // "Command /tmp/foo outputs 'bar'"
        let c = parse_acceptance_to_criteria(
            "Command /tmp/foo outputs 'bar'",
            "s1",
            &det,
        );
        assert_eq!(c.len(), 1);
        match &c[0].verifier {
            VerifierKind::CommandOutput { cmd, contains, .. } => {
                assert_eq!(cmd, "/tmp/foo");
                assert_eq!(contains, &["bar"]);
            }
            other => panic!("expected CommandOutput, got {:?}", other),
        }

        // "/tmp/script prints 'hello world'"
        let c = parse_acceptance_to_criteria(
            "/tmp/script prints 'hello world'",
            "s2",
            &det,
        );
        assert_eq!(c.len(), 1);
        assert!(matches!(&c[0].verifier, VerifierKind::CommandOutput { .. }));

        // "execute /tmp/run and output should contain 'done'"
        let c = parse_acceptance_to_criteria(
            "execute /tmp/run and output should contain 'done'",
            "s3",
            &det,
        );
        assert!(!c.is_empty());
        let has_cmd = c.iter().any(|c| matches!(&c.verifier, VerifierKind::CommandOutput { .. }));
        assert!(has_cmd, "should detect command output pattern");

        // Backtick-quoted command path should NOT appear in contains list
        let c = parse_acceptance_to_criteria(
            "Command `/tmp/hiworld` outputs exactly 'hi world' and exits with code 0",
            "s4",
            &det,
        );
        assert_eq!(c.len(), 1);
        match &c[0].verifier {
            VerifierKind::CommandOutput { cmd, contains, .. } => {
                assert_eq!(cmd, "/tmp/hiworld");
                assert_eq!(contains, &["hi world"], "path should not be in contains list");
            }
            other => panic!("expected CommandOutput, got {:?}", other),
        }
    }

    #[test]
    fn parse_permission_criterion() {
        let det = ProjectDetection::default();

        // "Script has executable permissions on /tmp/foo.sh"
        let c = parse_acceptance_to_criteria(
            "Script has executable permissions on /tmp/foo.sh",
            "s1",
            &det,
        );
        assert_eq!(c.len(), 1);
        match &c[0].verifier {
            VerifierKind::Command { cmd, expected_exit } => {
                assert!(cmd.contains("test -x"), "cmd should use test -x");
                assert!(cmd.contains("/tmp/foo.sh"));
                assert_eq!(*expected_exit, 0);
            }
            other => panic!("expected Command, got {:?}", other),
        }

        // "chmod +x applied, x bit set on /tmp/bar"
        let c = parse_acceptance_to_criteria(
            "chmod +x applied, x bit set on /tmp/bar",
            "s2",
            &det,
        );
        assert!(!c.is_empty());
        let has_perm = c.iter().any(|c| matches!(&c.verifier, VerifierKind::Command { .. }));
        assert!(has_perm, "should detect permission pattern with chmod keyword");
    }

    #[test]
    fn parse_no_warnings_criterion() {
        let det = rust_detection();

        // "No compiler warnings"
        let c = parse_acceptance_to_criteria("No compiler warnings", "s1", &det);
        assert_eq!(c.len(), 1);
        match &c[0].verifier {
            VerifierKind::Command { cmd, expected_exit } => {
                assert!(cmd.contains("cargo build"), "should use project build cmd");
                assert!(cmd.contains("warning"), "should grep for warning");
                assert_eq!(*expected_exit, 0);
            }
            other => panic!("expected Command, got {:?}", other),
        }
        assert!(c[0].global_only, "no-warnings check should be global_only");

        // "Zero warnings in build output"
        let c = parse_acceptance_to_criteria("Zero warnings in build output", "s2", &det);
        assert!(!c.is_empty());
        assert!(c.iter().any(|c| matches!(&c.verifier, VerifierKind::Command { .. })));

        // "Code compiles with clean output without warnings"
        let c = parse_acceptance_to_criteria(
            "Code compiles with clean output without warnings",
            "s3",
            &det,
        );
        assert!(!c.is_empty());
        assert!(c.iter().any(|c| matches!(&c.verifier, VerifierKind::Command { .. })));
    }

    #[test]
    fn parse_no_warnings_needs_project_detection() {
        // Without project detection, no build command → fallback to LlmJudge
        let det = ProjectDetection::default();
        let c = parse_acceptance_to_criteria("No compiler warnings", "s1", &det);
        assert_eq!(c.len(), 1);
        assert!(
            matches!(&c[0].verifier, VerifierKind::LlmJudge { .. }),
            "without build cmd, should fall to LlmJudge"
        );
    }

    #[test]
    fn parse_function_exists_criterion() {
        let det = ProjectDetection::default();

        // "Function authenticate exists in src/auth.rs"
        let c = parse_acceptance_to_criteria(
            "Function authenticate exists in src/auth.rs",
            "s1",
            &det,
        );
        assert_eq!(c.len(), 1);
        match &c[0].verifier {
            VerifierKind::GrepCheck {
                file,
                pattern,
                should_match,
            } => {
                assert_eq!(file, "src/auth.rs");
                assert_eq!(pattern, "authenticate");
                assert!(*should_match);
            }
            other => panic!("expected GrepCheck, got {:?}", other),
        }

        // "src/models.py defines class User"
        let c = parse_acceptance_to_criteria(
            "src/models.py defines class User",
            "s2",
            &det,
        );
        assert!(!c.is_empty());
        let has_grep = c
            .iter()
            .any(|c| matches!(&c.verifier, VerifierKind::GrepCheck { .. }));
        assert!(has_grep, "should detect class existence as grep");

        // "Module src/config.ts exports function createUser"
        let c = parse_acceptance_to_criteria(
            "Module src/config.ts exports function createUser",
            "s3",
            &det,
        );
        assert!(!c.is_empty());
        let has_grep = c
            .iter()
            .any(|c| matches!(&c.verifier, VerifierKind::GrepCheck { .. }));
        assert!(has_grep, "should detect export existence as grep");
    }

    #[test]
    fn parse_flexible_grep_criterion() {
        let det = ProjectDetection::default();

        // "src/config.ts includes the string 'API_KEY'"
        let c = parse_acceptance_to_criteria(
            "src/config.ts includes the string 'API_KEY'",
            "s1",
            &det,
        );
        assert_eq!(c.len(), 1);
        match &c[0].verifier {
            VerifierKind::GrepCheck {
                file,
                pattern,
                should_match,
            } => {
                assert_eq!(file, "src/config.ts");
                assert_eq!(pattern, "API_KEY");
                assert!(*should_match);
            }
            other => panic!("expected GrepCheck, got {:?}", other),
        }

        // "Makefile should have a 'test' target" — "makefile" contains "file",
        // so file_exists parser catches it first. Use a path instead:
        let c = parse_acceptance_to_criteria(
            "build/config.yaml should have 'debug: true'",
            "s2",
            &det,
        );
        assert!(!c.is_empty());
        match &c[0].verifier {
            VerifierKind::GrepCheck {
                file,
                pattern,
                should_match,
            } => {
                assert_eq!(file, "build/config.yaml");
                assert_eq!(pattern, "debug: true");
                assert!(*should_match);
            }
            other => panic!("expected GrepCheck, got {:?}", other),
        }

        // Negative grep with "should not"
        let c = parse_acceptance_to_criteria(
            "src/auth.rs should not have 'unwrap()'",
            "s3",
            &det,
        );
        assert!(!c.is_empty());
        let grep = c
            .iter()
            .find(|c| matches!(&c.verifier, VerifierKind::GrepCheck { .. }));
        assert!(grep.is_some(), "should detect negative flexible grep");
        if let VerifierKind::GrepCheck { should_match, .. } = &grep.unwrap().verifier {
            assert!(!should_match, "should be negated");
        }
    }
}

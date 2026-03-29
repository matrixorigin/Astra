//! Plan Decomposition Engine — automatically break down complex goals into subtasks.
//!
//! # Architecture
//!
//! ```text
//! User Goal → analyze_codebase() → generate_plan() → TaskPlan { subtasks, deps }
//! ```
//!
//! The decomposer:
//! 1. Scans the project structure for context
//! 2. Calls the LLM to break down the goal into subtasks
//! 3. Returns a TaskPlan with dependencies

#![allow(dead_code)] // parse_plan_response and related structs are used in tests

use serde::{Deserialize, Serialize};
use std::path::Path;

// Re-export task types from services
pub use mo_agent_services::task_orchestrator::{SubtaskPlan, TaskPlan, TaskStatus};

/// Project context gathered for plan decomposition.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectContext {
    /// Project root directory.
    pub root: String,
    /// Key files detected (Cargo.toml, package.json, etc.).
    pub entry_points: Vec<String>,
    /// Languages detected in the project.
    pub languages: Vec<String>,
    /// Directory structure summary.
    pub structure_summary: String,
    /// Number of source files.
    pub source_file_count: usize,
    /// Key source modules with line counts: ("src/main.rs", 150)
    pub key_modules: Vec<(String, usize)>,
    /// Git branch name if in a repo.
    pub git_branch: Option<String>,
    /// Test framework detected (e.g., "cargo test", "pytest", "jest").
    pub test_framework: Option<String>,
}

/// Scan project root to gather context for planning.
pub fn analyze_project(root: &Path) -> ProjectContext {
    let mut ctx = ProjectContext {
        root: root.display().to_string(),
        ..Default::default()
    };

    // Detect entry points
    let entry_point_files = [
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "setup.py",
        "go.mod",
        "pom.xml",
        "build.gradle",
        "Makefile",
        "CMakeLists.txt",
        ".git",
    ];

    for f in &entry_point_files {
        if root.join(f).exists() {
            ctx.entry_points.push(f.to_string());
        }
    }

    // Detect languages from file extensions
    let mut lang_set = std::collections::HashSet::new();
    let ext_to_lang = [
        ("rs", "Rust"),
        ("py", "Python"),
        ("ts", "TypeScript"),
        ("tsx", "TypeScript"),
        ("js", "JavaScript"),
        ("jsx", "JavaScript"),
        ("go", "Go"),
        ("java", "Java"),
        ("rb", "Ruby"),
        ("cpp", "C++"),
        ("c", "C"),
        ("cs", "C#"),
        ("swift", "Swift"),
        ("kt", "Kotlin"),
    ];

    let mut source_count = 0;

    // Simple recursive scan (limited depth)
    fn scan_dir(
        dir: &Path,
        depth: usize,
        max_depth: usize,
        count: &mut usize,
        lang_set: &mut std::collections::HashSet<&'static str>,
        ext_to_lang: &[(&str, &'static str)],
    ) {
        if depth > max_depth || *count > 1000 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();

            // Skip hidden and common non-source dirs
            if name_str.starts_with('.')
                || matches!(
                    name_str.as_ref(),
                    "node_modules" | "target" | "venv" | "__pycache__" | "dist" | "build"
                )
            {
                continue;
            }

            let path = entry.path();
            if path.is_dir() {
                scan_dir(&path, depth + 1, max_depth, count, lang_set, ext_to_lang);
            } else if path.is_file() {
                *count += 1;
                if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                    for (e, lang) in ext_to_lang {
                        if ext == *e {
                            lang_set.insert(*lang);
                            break;
                        }
                    }
                }
            }
        }
    }

    scan_dir(root, 0, 4, &mut source_count, &mut lang_set, &ext_to_lang);

    ctx.languages = lang_set.into_iter().map(String::from).collect();
    ctx.languages.sort();
    ctx.source_file_count = source_count;

    // Collect key modules: largest source files (by line count), top 15
    let mut source_files: Vec<(String, usize)> = Vec::new();
    collect_source_files(root, root, 0, 4, &mut source_files);
    source_files.sort_by(|a, b| b.1.cmp(&a.1));
    source_files.truncate(15);
    ctx.key_modules = source_files;

    // Detect test framework
    if root.join("Cargo.toml").exists() {
        ctx.test_framework = Some("cargo test".into());
    } else if root.join("package.json").exists() {
        if root.join("jest.config.js").exists() || root.join("jest.config.ts").exists() {
            ctx.test_framework = Some("jest".into());
        } else {
            ctx.test_framework = Some("npm test".into());
        }
    } else if root.join("pyproject.toml").exists() || root.join("setup.py").exists() {
        ctx.test_framework = Some("pytest".into());
    } else if root.join("go.mod").exists() {
        ctx.test_framework = Some("go test".into());
    }

    // Detect git branch
    let head_file = root.join(".git").join("HEAD");
    if let Ok(head) = std::fs::read_to_string(&head_file) {
        if let Some(branch) = head.trim().strip_prefix("ref: refs/heads/") {
            ctx.git_branch = Some(branch.to_string());
        }
    }

    // Build structure summary
    let mut dirs = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let name = entry.file_name().to_string_lossy().to_string();
                if !name.starts_with('.') {
                    dirs.push(name);
                }
            }
        }
    }
    dirs.sort();
    ctx.structure_summary = if dirs.is_empty() {
        "(flat project)".to_string()
    } else {
        format!("Top-level dirs: {}", dirs.join(", "))
    };

    ctx
}

/// Collect source files with line counts for key module analysis.
fn collect_source_files(
    root: &Path,
    dir: &Path,
    depth: usize,
    max_depth: usize,
    out: &mut Vec<(String, usize)>,
) {
    if depth > max_depth || out.len() > 200 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let source_exts = [
        "rs", "py", "ts", "tsx", "js", "jsx", "go", "java", "rb", "cpp", "c",
    ];
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.')
            || matches!(
                name_str.as_ref(),
                "node_modules" | "target" | "venv" | "__pycache__" | "dist" | "build"
            )
        {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            collect_source_files(root, &path, depth + 1, max_depth, out);
        } else if path.is_file() {
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                if source_exts.contains(&ext) {
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        let lines = content.lines().count();
                        if lines > 20 {
                            let rel = path
                                .strip_prefix(root)
                                .unwrap_or(&path)
                                .display()
                                .to_string();
                            out.push((rel, lines));
                        }
                    }
                }
            }
        }
    }
}

/// Prompt template for plan decomposition.
pub fn decomposition_prompt(goal: &str, context: &ProjectContext) -> String {
    let mut prompt = String::with_capacity(2048);

    prompt.push_str("You are a senior software architect creating an execution plan.\n\n");
    prompt.push_str("## Project Context\n");
    prompt.push_str(&format!("- Root: {}\n", context.root));
    prompt.push_str(&format!(
        "- Build system: {}\n",
        if context.entry_points.is_empty() {
            "(none detected)".to_string()
        } else {
            context.entry_points.join(", ")
        }
    ));
    prompt.push_str(&format!(
        "- Languages: {}\n",
        if context.languages.is_empty() {
            "(unknown)".to_string()
        } else {
            context.languages.join(", ")
        }
    ));
    prompt.push_str(&format!("- Structure: {}\n", context.structure_summary));
    prompt.push_str(&format!("- Source files: ~{}\n", context.source_file_count));
    if let Some(ref branch) = context.git_branch {
        prompt.push_str(&format!("- Git branch: {branch}\n"));
    }
    if let Some(ref test_fw) = context.test_framework {
        prompt.push_str(&format!("- Test framework: {test_fw}\n"));
    }

    // Key modules — the LLM needs this to suggest which files to modify
    if !context.key_modules.is_empty() {
        prompt.push_str("\n## Key Modules (by size)\n");
        for (path, lines) in &context.key_modules {
            prompt.push_str(&format!("- {path} ({lines} lines)\n"));
        }
    }

    prompt.push_str(&format!("\n## Goal\n{goal}\n"));

    prompt.push_str(r#"
## Instructions
Decompose this goal into 3-8 concrete subtasks. For EACH subtask, provide:

1. **id**: short kebab-case ID (e.g., "add-auth", "fix-parser", "write-tests")
2. **title**: one-line summary
3. **description**: what specifically needs to be done
4. **depends_on**: IDs of subtasks that must finish first (empty array if none)
5. **effort**: estimated scope — "small" (<30 lines changed), "medium" (30-100), or "large" (100+)
6. **files**: list of files likely to be modified (relative paths; best guess from project structure)
7. **acceptance**: how to verify this subtask is done (e.g., "tests pass", "endpoint returns 200")

Guidelines:
- Order subtasks so dependencies come first
- Each subtask should be completable in ONE focused session
- Always include a testing subtask
- If the goal involves refactoring, add a "verify no regression" final subtask

Return ONLY this JSON:
```json
{
  "subtasks": [
    {
      "id": "unique-id",
      "title": "Short title",
      "description": "What needs to be done",
      "depends_on": [],
      "effort": "small|medium|large",
      "files": ["src/foo.rs", "tests/test_foo.rs"],
      "acceptance": "How to verify completion"
    }
  ],
  "notes": "High-level approach and risk considerations"
}
```"#);

    prompt
}

/// Parse LLM response into a TaskPlan.
pub fn parse_plan_response(response: &str) -> Result<TaskPlan, String> {
    // Try to extract JSON from the response (may be wrapped in markdown)
    let json_str = extract_json(response);

    // Parse the JSON
    let parsed: PlanResponse =
        serde_json::from_str(&json_str).map_err(|e| format!("Invalid plan JSON: {e}"))?;

    // Convert to TaskPlan
    let subtasks = parsed
        .subtasks
        .into_iter()
        .map(|st| SubtaskPlan {
            id: st.id,
            title: st.title,
            description: st.description,
            depends_on: st.depends_on.unwrap_or_default(),
            status: TaskStatus::Pending,
            effort: st.effort,
            files: st.files.unwrap_or_default(),
            acceptance: st.acceptance,
        })
        .collect();

    Ok(TaskPlan {
        subtasks,
        notes: parsed.notes,
    })
}

#[derive(Deserialize)]
struct PlanResponse {
    subtasks: Vec<SubtaskResponse>,
    notes: Option<String>,
}

#[derive(Deserialize)]
struct SubtaskResponse {
    id: String,
    title: String,
    description: Option<String>,
    depends_on: Option<Vec<String>>,
    effort: Option<String>,
    files: Option<Vec<String>>,
    acceptance: Option<String>,
}

/// Extract JSON from a response that may include markdown code blocks.
fn extract_json(response: &str) -> String {
    // Try to find JSON in markdown code block
    if let Some(start) = response.find("```json") {
        let after_start = &response[start + 7..];
        if let Some(end) = after_start.find("```") {
            return after_start[..end].trim().to_string();
        }
    }

    // Try plain ``` block
    if let Some(start) = response.find("```") {
        let after_start = &response[start + 3..];
        // Skip language identifier if present
        let content = if let Some(newline) = after_start.find('\n') {
            &after_start[newline + 1..]
        } else {
            after_start
        };
        if let Some(end) = content.find("```") {
            return content[..end].trim().to_string();
        }
    }

    // Look for raw JSON object
    if let Some(start) = response.find('{') {
        if let Some(end) = response.rfind('}') {
            return response[start..=end].to_string();
        }
    }

    response.to_string()
}

/// Format a TaskPlan for display.
pub fn format_plan(plan: &TaskPlan) -> String {
    let mut out = String::new();

    out.push_str("┌── Plan ──────────────────────────────────────────\n");

    if let Some(ref notes) = plan.notes {
        out.push_str("│ ");
        out.push_str(notes);
        out.push('\n');
        out.push_str("│\n");
    }

    // Build dependency graph visualization
    let ready = plan.ready_subtasks();
    let ready_ids: std::collections::HashSet<_> = ready.iter().map(|st| st.id.as_str()).collect();

    for (i, st) in plan.subtasks.iter().enumerate() {
        let status_icon = match st.status {
            TaskStatus::Completed => "✓",
            TaskStatus::InProgress => "▶",
            TaskStatus::Failed => "✗",
            TaskStatus::Paused => "⏸",
            _ if ready_ids.contains(st.id.as_str()) => "○",
            _ => "·",
        };

        // Effort badge
        let effort_badge = match st.effort.as_deref() {
            Some("small") => " [S]",
            Some("medium") => " [M]",
            Some("large") => " [L]",
            _ => "",
        };

        out.push_str(&format!(
            "│ {} {}{} {}\n",
            status_icon, st.id, effort_badge, st.title
        ));

        if let Some(ref desc) = st.description {
            out.push_str(&format!("│     └─ {}\n", desc));
        }

        if !st.files.is_empty() {
            out.push_str(&format!("│     📁 {}\n", st.files.join(", ")));
        }

        if let Some(ref acc) = st.acceptance {
            out.push_str(&format!("│     ✅ {}\n", acc));
        }

        if !st.depends_on.is_empty() {
            out.push_str(&format!("│     deps: {}\n", st.depends_on.join(", ")));
        }

        if i < plan.subtasks.len() - 1 {
            out.push_str("│\n");
        }
    }

    out.push_str("└─────────────────────────────────────────────────\n");

    // Effort summary
    let small = plan
        .subtasks
        .iter()
        .filter(|s| s.effort.as_deref() == Some("small"))
        .count();
    let medium = plan
        .subtasks
        .iter()
        .filter(|s| s.effort.as_deref() == Some("medium"))
        .count();
    let large = plan
        .subtasks
        .iter()
        .filter(|s| s.effort.as_deref() == Some("large"))
        .count();
    if small + medium + large > 0 {
        out.push_str(&format!(
            "  Effort: {} small, {} medium, {} large\n",
            small, medium, large
        ));
    }

    out.push_str(&format!(
        "  Progress: {}% ({}/{})\n",
        plan.progress_pct(),
        plan.items_done(),
        plan.subtasks.len()
    ));

    if !ready.is_empty() {
        out.push_str(&format!(
            "  Ready: {}\n",
            ready
                .iter()
                .map(|st| st.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    out
}

// ─── Plan Mode State ─────────────────────────────────────────────────────────

/// State for interactive Plan Mode (plan> prompt).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlanModeState {
    /// The original goal from user
    pub goal: String,
    /// Current plan being edited
    pub plan: TaskPlan,
    /// Project context at time of entering plan mode
    pub context: ProjectContext,
    /// Conversation history within plan mode (user_msg, assistant_msg)
    pub history: Vec<(String, String)>,
    /// Whether the plan has been modified since generation
    pub modified: bool,
    /// Version history for plan rollback and diffing
    #[serde(default)]
    pub version_history: PlanVersionHistory,
}

impl PlanModeState {
    /// Create a new plan mode state with initial goal and context
    pub fn new(goal: String, context: ProjectContext) -> Self {
        Self {
            goal,
            context,
            plan: TaskPlan::default(),
            history: Vec::new(),
            modified: false,
            version_history: PlanVersionHistory::default(),
        }
    }

    /// Set the plan (after LLM generation) and record a version.
    pub fn set_plan(&mut self, plan: TaskPlan) {
        let summary = if self.version_history.versions.is_empty() {
            "Initial plan".to_string()
        } else {
            "Plan updated".to_string()
        };
        self.version_history.record(&plan, &summary);
        self.plan = plan;
    }

    /// Update the plan with a change description (for manual modifications).
    pub fn update_plan(&mut self, plan: TaskPlan, change_summary: &str) {
        self.version_history.record(&plan, change_summary);
        self.plan = plan;
        self.modified = true;
    }

    /// Rollback to a specific plan version.
    pub fn rollback_to_version(&mut self, version: u32) -> Result<String, String> {
        let v = self.version_history.get_version(version)
            .ok_or_else(|| format!("Version {} not found", version))?;
        let plan = v.plan.clone();
        let summary = format!("Rollback to v{}", version);
        self.version_history.record(&plan, &summary);
        self.plan = plan;
        self.modified = true;
        Ok(summary)
    }

    /// Mark a subtask as completed by ID (prefix match).
    /// Returns Ok(title) on success, Err(msg) on failure.
    pub fn complete_subtask(&mut self, id_prefix: &str) -> Result<String, String> {
        let matches: Vec<usize> = self
            .plan
            .subtasks
            .iter()
            .enumerate()
            .filter(|(_, st)| st.id.starts_with(id_prefix))
            .map(|(i, _)| i)
            .collect();
        match matches.len() {
            0 => Err(format!("No subtask matching '{id_prefix}'")),
            1 => {
                let st = &mut self.plan.subtasks[matches[0]];
                st.status = TaskStatus::Completed;
                self.modified = true;
                Ok(st.title.clone())
            }
            _ => Err(format!(
                "Ambiguous: {} subtasks match '{id_prefix}'",
                matches.len()
            )),
        }
    }

    /// Add a conversation turn to history
    pub fn add_turn(&mut self, user_msg: &str, assistant_msg: &str) {
        self.history
            .push((user_msg.to_string(), assistant_msg.to_string()));
    }

    /// Generate the plan mode prompt for LLM interactions
    pub fn plan_mode_prompt(&self, user_message: &str) -> String {
        let mut prompt = String::new();

        prompt.push_str("You are in PLAN MODE, helping the user refine a plan.\n\n");
        prompt.push_str(&format!("## Original Goal\n{}\n\n", self.goal));

        if !self.plan.subtasks.is_empty() {
            prompt.push_str("## Current Plan\n");
            prompt.push_str(&serde_json::to_string_pretty(&self.plan).unwrap_or_default());
            prompt.push_str("\n\n");
        }

        // Include recent history
        if !self.history.is_empty() {
            prompt.push_str("## Recent Discussion\n");
            for (i, (u, a)) in self.history.iter().rev().take(3).rev().enumerate() {
                prompt.push_str(&format!("User {}: {}\n", i + 1, u));
                prompt.push_str(&format!("Assistant {}: {}\n", i + 1, a));
            }
            prompt.push_str("\n");
        }

        prompt.push_str(&format!("## User Request\n{}\n\n", user_message));

        prompt.push_str(
            r#"## Instructions
Based on the user's request, respond in ONE of these ways:

1. **If modifying the plan**: Output the updated plan as JSON with format:
```json
{
  "subtasks": [{"id": "...", "title": "...", "description": "...", "depends_on": [...]}],
  "notes": "..."
}
```

2. **If answering a question**: Respond naturally, no JSON needed.

Keep responses concise. The plan JSON must be valid if provided."#,
        );

        prompt
    }

    /// Check if user input is an execute command
    pub fn is_execute_command(input: &str) -> bool {
        let lower = input.trim().to_lowercase();
        matches!(
            lower.as_str(),
            "execute" | "go" | "start" | "done" | "run" | "开始" | "执行" | "运行"
        )
    }

    /// Memory protocol content for storing the active plan
    pub fn to_memory_content(&self) -> String {
        format!(
            "[plan:active] Goal: {}\n\n{}",
            self.goal,
            serde_json::to_string_pretty(&self.plan).unwrap_or_default()
        )
    }

    /// Memory protocol content for a completed plan
    pub fn to_completed_memory(&self) -> String {
        format!(
            "[plan:completed] Goal: {}\nStatus: {} subtasks\n\n{}",
            self.goal,
            self.plan.subtasks.len(),
            serde_json::to_string_pretty(&self.plan).unwrap_or_default()
        )
    }

    /// Save plan mode state to a file for session recovery.
    pub fn save_to_file(&self, path: &Path) -> Result<(), String> {
        let json =
            serde_json::to_string_pretty(self).map_err(|e| format!("serialize plan state: {e}"))?;
        std::fs::write(path, json).map_err(|e| format!("write plan state: {e}"))?;
        Ok(())
    }

    /// Load plan mode state from a file.
    pub fn load_from_file(path: &Path) -> Result<Self, String> {
        let data = std::fs::read_to_string(path).map_err(|e| format!("read plan state: {e}"))?;
        serde_json::from_str(&data).map_err(|e| format!("parse plan state: {e}"))
    }

    /// Default path for plan state persistence.
    pub fn state_path() -> std::path::PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        std::path::PathBuf::from(home)
            .join(".mo-agent")
            .join("plan_state.json")
    }

    /// Remove the saved state file.
    pub fn clear_saved_state() {
        let path = Self::state_path();
        let _ = std::fs::remove_file(path);
    }
}

/// Format the plan mode prompt (for display)
pub fn format_plan_mode_prompt() -> &'static str {
    "plan> "
}

/// Generate a plan modification prompt for LLM
pub fn plan_modification_prompt(state: &PlanModeState, user_request: &str) -> String {
    state.plan_mode_prompt(user_request)
}

/// Check if user input is a resume command for paused plan execution.
pub fn is_resume_command(input: &str) -> bool {
    let trimmed = input.trim().to_lowercase();
    matches!(
        trimmed.as_str(),
        "continue" | "resume" | "继续" | "go" | "next"
    )
}

/// Format a subtask as a rich prompt for the LLM to execute.
///
/// Includes the task title, description, files to modify, and acceptance criteria.
/// Used by the plan auto-execution loop to convert subtasks into actionable LLM prompts.
pub fn format_subtask_prompt(subtask: &SubtaskPlan) -> String {
    let mut prompt = format!("Execute this subtask: {}\n", subtask.title);

    if let Some(ref desc) = subtask.description {
        prompt.push_str(&format!("\nDescription: {}\n", desc));
    }

    if !subtask.files.is_empty() {
        prompt.push_str(&format!(
            "\nFiles to modify: {}\n",
            subtask.files.join(", ")
        ));
    }

    if let Some(ref acceptance) = subtask.acceptance {
        prompt.push_str(&format!("\nAcceptance criteria: {}\n", acceptance));
    }

    prompt.push_str(
        "\nPlease implement this change. Read the relevant files first, \
         make the changes, and verify they compile/pass tests.",
    );

    prompt
}

// ─── Plan Execution Config & Preview ─────────────────────────────────────────

/// Configuration for plan execution behavior.
#[derive(Debug, Clone, Default)]
pub struct PlanExecutionConfig {
    /// If true, prompt user for confirmation before executing each subtask.
    pub step_by_step: bool,
    /// If true, auto-execute immediately after plan decomposition (skip explicit "execute").
    pub auto_execute: bool,
}

/// Result of a plan execution for summary purposes.
#[derive(Debug, Clone, Default)]
pub struct PlanExecutionSummary {
    pub goal: String,
    pub total_subtasks: usize,
    pub completed: usize,
    pub failed: usize,
    pub paused: usize,
    /// Subtask IDs that were completed, in execution order.
    pub execution_order: Vec<String>,
    /// Number of parallel groups that were executed.
    pub parallel_rounds: usize,
}

impl PlanExecutionSummary {
    /// Build a summary from a completed (or paused) plan.
    pub fn from_plan(plan: &TaskPlan, goal: &str, parallel_rounds: usize) -> Self {
        let completed = plan
            .subtasks
            .iter()
            .filter(|s| s.status == TaskStatus::Completed)
            .count();
        let failed = plan
            .subtasks
            .iter()
            .filter(|s| s.status == TaskStatus::Failed)
            .count();
        let paused = plan
            .subtasks
            .iter()
            .filter(|s| s.status == TaskStatus::Paused || s.status == TaskStatus::InProgress)
            .count();

        Self {
            goal: goal.to_string(),
            total_subtasks: plan.subtasks.len(),
            completed,
            failed,
            paused,
            execution_order: plan
                .subtasks
                .iter()
                .filter(|s| s.status == TaskStatus::Completed)
                .map(|s| s.id.clone())
                .collect(),
            parallel_rounds,
        }
    }

    /// Format the summary for display.
    pub fn format(&self) -> String {
        let mut out = String::new();
        out.push_str("┌── Execution Summary ────────────────────────────\n");
        out.push_str(&format!("│ Goal: {}\n", self.goal));
        out.push_str("│\n");

        let status = if self.completed == self.total_subtasks {
            "✓ Complete"
        } else if self.failed > 0 {
            "✗ Partial (failures)"
        } else if self.paused > 0 {
            "⏸ Paused"
        } else {
            "· Incomplete"
        };
        out.push_str(&format!("│ Status:    {}\n", status));
        out.push_str(&format!(
            "│ Subtasks:  {}/{} completed\n",
            self.completed, self.total_subtasks
        ));
        if self.failed > 0 {
            out.push_str(&format!("│ Failed:    {}\n", self.failed));
        }
        if self.parallel_rounds > 0 {
            out.push_str(&format!("│ Rounds:    {} (parallel-aware)\n", self.parallel_rounds));
        }
        if !self.execution_order.is_empty() {
            out.push_str(&format!(
                "│ Order:     {}\n",
                self.execution_order.join(" → ")
            ));
        }
        out.push_str("└─────────────────────────────────────────────────\n");
        out
    }
}

/// Format a pre-execution preview showing parallel analysis and execution order.
///
/// Displayed before execution starts to give the user insight into how
/// the plan will be executed. Shows parallel groups, file conflicts, and
/// estimated round count.
pub fn format_execution_preview(plan: &TaskPlan) -> String {
    let analysis = analyze_parallelism(plan);
    let ready = plan.ready_subtasks();

    let mut out = String::new();
    out.push_str("┌── Execution Preview ────────────────────────────\n");
    out.push_str(&format!(
        "│ {} subtasks, {} ready now\n",
        plan.subtasks.len(),
        ready.len()
    ));

    // Show parallel groups
    if analysis.groups.len() > 1 || analysis.groups.first().map(|g| g.len()).unwrap_or(0) > 1 {
        out.push_str("│\n");
        out.push_str(&format!("│ Parallel Groups ({} rounds):\n", analysis.groups.len()));
        for (i, group) in analysis.groups.iter().enumerate() {
            let names: Vec<_> = group
                .iter()
                .filter_map(|id| plan.subtasks.iter().find(|s| &s.id == id))
                .map(|s| format!("[{}] {}", s.id, s.title))
                .collect();
            let parallel_marker = if group.len() > 1 { " ║" } else { "  " };
            out.push_str(&format!(
                "│   Round {}{}: {}\n",
                i + 1,
                parallel_marker,
                names.join(", ")
            ));
        }
    }

    // Show file conflicts
    if !analysis.conflicts.is_empty() {
        out.push_str("│\n");
        out.push_str(&format!(
            "│ ⚠ {} file conflict(s):\n",
            analysis.conflicts.len()
        ));
        for c in &analysis.conflicts {
            out.push_str(&format!(
                "│   {} ↔ {} ({})\n",
                c.subtask_a, c.subtask_b, c.shared_files.join(", ")
            ));
        }
    }

    // Effort estimate
    let total_effort: usize = plan
        .subtasks
        .iter()
        .map(|s| match s.effort.as_deref() {
            Some("large") => 3,
            Some("medium") => 2,
            _ => 1,
        })
        .sum();
    let effort_label = match total_effort {
        0..=3 => "Low",
        4..=8 => "Medium",
        _ => "High",
    };
    out.push_str("│\n");
    out.push_str(&format!(
        "│ Estimated effort: {} ({} units)\n",
        effort_label, total_effort
    ));

    out.push_str("└─────────────────────────────────────────────────\n");
    out
}

/// User confirmation result after viewing execution preview.
#[derive(Debug, Clone, PartialEq)]
pub enum ExecutionConfirmation {
    /// User confirmed — proceed with execution.
    Execute,
    /// User chose step-by-step mode.
    StepByStep,
    /// User wants to edit the plan first.
    Edit,
    /// User cancelled execution.
    Cancel,
}

/// Parse user response to the execution confirmation prompt.
pub fn parse_execution_confirmation(input: &str) -> ExecutionConfirmation {
    let lower = input.trim().to_lowercase();
    match lower.as_str() {
        "y" | "yes" | "go" | "execute" | "run" | "确认" | "是" => ExecutionConfirmation::Execute,
        "s" | "step" | "step-by-step" | "逐步" => ExecutionConfirmation::StepByStep,
        "e" | "edit" | "modify" | "编辑" | "修改" => ExecutionConfirmation::Edit,
        _ => ExecutionConfirmation::Cancel,
    }
}

/// Format the step-by-step confirmation prompt shown before each subtask.
pub fn format_subtask_confirmation(subtask: &SubtaskPlan, idx: usize, total: usize) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "\n┌── Subtask {}/{}: [{}] {}\n",
        idx + 1,
        total,
        subtask.id,
        subtask.title
    ));
    if let Some(ref desc) = subtask.description {
        out.push_str(&format!("│ {}\n", desc));
    }
    if !subtask.files.is_empty() {
        out.push_str(&format!("│ Files: {}\n", subtask.files.join(", ")));
    }
    out.push_str("└ Execute? (y)es / (s)kip / (q)uit: ");
    out
}

/// Parse the per-subtask confirmation response.
#[derive(Debug, Clone, PartialEq)]
pub enum SubtaskConfirmation {
    Execute,
    Skip,
    Quit,
}

pub fn parse_subtask_confirmation(input: &str) -> SubtaskConfirmation {
    let lower = input.trim().to_lowercase();
    match lower.as_str() {
        "y" | "yes" | "" => SubtaskConfirmation::Execute,
        "s" | "skip" | "跳过" => SubtaskConfirmation::Skip,
        _ => SubtaskConfirmation::Quit,
    }
}

// ─── Plan Versioning ─────────────────────────────────────────────────────────

/// A versioned snapshot of a plan, recording the full plan at a point in time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanVersion {
    /// Monotonically increasing version number
    pub version: u32,
    /// What changed in this version (user-driven or auto-generated)
    pub change_summary: String,
    /// Snapshot of the plan at this version
    pub plan: TaskPlan,
    /// Timestamp (ISO 8601)
    pub timestamp: String,
}

/// Version history tracker embedded in PlanModeState.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlanVersionHistory {
    /// All recorded versions, ordered by version number.
    pub versions: Vec<PlanVersion>,
    /// Current version number (starts at 0).
    pub current_version: u32,
}

impl PlanVersionHistory {
    /// Record a new version. Returns the version number.
    pub fn record(&mut self, plan: &TaskPlan, change_summary: &str) -> u32 {
        self.current_version += 1;
        let version = PlanVersion {
            version: self.current_version,
            change_summary: change_summary.to_string(),
            plan: plan.clone(),
            timestamp: chrono_now_iso(),
        };
        self.versions.push(version);
        self.current_version
    }

    /// Get a specific version by number.
    pub fn get_version(&self, version: u32) -> Option<&PlanVersion> {
        self.versions.iter().find(|v| v.version == version)
    }

    /// Show a compact diff between two versions (by listing changed subtask IDs).
    pub fn diff_versions(&self, from: u32, to: u32) -> Result<PlanDiff, String> {
        let v_from = self.get_version(from)
            .ok_or_else(|| format!("Version {} not found", from))?;
        let v_to = self.get_version(to)
            .ok_or_else(|| format!("Version {} not found", to))?;

        let old_ids: std::collections::HashSet<&str> = v_from.plan.subtasks.iter()
            .map(|s| s.id.as_str()).collect();
        let new_ids: std::collections::HashSet<&str> = v_to.plan.subtasks.iter()
            .map(|s| s.id.as_str()).collect();

        let added: Vec<String> = new_ids.difference(&old_ids).map(|s| s.to_string()).collect();
        let removed: Vec<String> = old_ids.difference(&new_ids).map(|s| s.to_string()).collect();

        // Detect modified (same ID but different title/description/deps)
        let mut modified = Vec::new();
        for st_new in &v_to.plan.subtasks {
            if let Some(st_old) = v_from.plan.subtasks.iter().find(|s| s.id == st_new.id) {
                if st_new.title != st_old.title
                    || st_new.description != st_old.description
                    || st_new.depends_on != st_old.depends_on
                    || st_new.effort != st_old.effort
                    || st_new.files != st_old.files
                {
                    modified.push(st_new.id.clone());
                }
            }
        }

        Ok(PlanDiff { from_version: from, to_version: to, added, removed, modified })
    }

    /// Format a compact version log for display.
    pub fn format_log(&self) -> String {
        if self.versions.is_empty() {
            return "  No version history yet.\n".to_string();
        }
        let mut out = String::new();
        for v in self.versions.iter().rev().take(10) {
            out.push_str(&format!("  v{}: {} ({} subtasks) — {}\n",
                v.version, v.change_summary, v.plan.subtasks.len(), v.timestamp));
        }
        out
    }
}

/// Result of diffing two plan versions.
#[derive(Debug, Clone)]
pub struct PlanDiff {
    pub from_version: u32,
    pub to_version: u32,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub modified: Vec<String>,
}

impl PlanDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.modified.is_empty()
    }

    pub fn format(&self) -> String {
        let mut out = format!("  Plan diff v{} → v{}:\n", self.from_version, self.to_version);
        for id in &self.added {
            out.push_str(&format!("    + {}\n", id));
        }
        for id in &self.removed {
            out.push_str(&format!("    - {}\n", id));
        }
        for id in &self.modified {
            out.push_str(&format!("    ~ {}\n", id));
        }
        if self.is_empty() {
            out.push_str("    (no changes)\n");
        }
        out
    }
}

fn chrono_now_iso() -> String {
    // Use system time for a simple ISO 8601 timestamp
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Simple format without external crate
    format!("{}", now)
}

// ─── Parallel Subtask Detection & File Conflict ─────────────────────────────

/// Analysis of which subtasks can run in parallel.
#[derive(Debug, Clone)]
pub struct ParallelGroups {
    /// Groups of subtask IDs that can execute concurrently.
    /// Each group contains subtasks that are all ready and have no file conflicts.
    pub groups: Vec<Vec<String>>,
    /// File conflicts detected: (subtask_a, subtask_b, shared_files).
    pub conflicts: Vec<FileConflict>,
}

/// Two subtasks targeting overlapping files.
#[derive(Debug, Clone)]
pub struct FileConflict {
    pub subtask_a: String,
    pub subtask_b: String,
    pub shared_files: Vec<String>,
}

/// Analyze a plan to find parallelizable subtask groups and file conflicts.
pub fn analyze_parallelism(plan: &TaskPlan) -> ParallelGroups {
    let ready = plan.ready_subtasks();
    if ready.len() <= 1 {
        return ParallelGroups {
            groups: if ready.is_empty() { vec![] } else { vec![vec![ready[0].id.clone()]] },
            conflicts: vec![],
        };
    }

    // Detect file conflicts between all pairs of ready subtasks
    let mut conflicts = Vec::new();
    for i in 0..ready.len() {
        for j in (i + 1)..ready.len() {
            let shared: Vec<String> = ready[i].files.iter()
                .filter(|f| ready[j].files.contains(f))
                .cloned()
                .collect();
            if !shared.is_empty() {
                conflicts.push(FileConflict {
                    subtask_a: ready[i].id.clone(),
                    subtask_b: ready[j].id.clone(),
                    shared_files: shared,
                });
            }
        }
    }

    // Build groups: use a simple greedy coloring approach
    // conflicting subtasks can't be in the same group
    let conflict_pairs: std::collections::HashSet<(String, String)> = conflicts.iter()
        .flat_map(|c| vec![
            (c.subtask_a.clone(), c.subtask_b.clone()),
            (c.subtask_b.clone(), c.subtask_a.clone()),
        ])
        .collect();

    let mut groups: Vec<Vec<String>> = Vec::new();
    for st in &ready {
        let mut placed = false;
        for group in groups.iter_mut() {
            let has_conflict = group.iter().any(|g_id| {
                conflict_pairs.contains(&(g_id.clone(), st.id.clone()))
            });
            if !has_conflict {
                group.push(st.id.clone());
                placed = true;
                break;
            }
        }
        if !placed {
            groups.push(vec![st.id.clone()]);
        }
    }

    ParallelGroups { groups, conflicts }
}

/// Format parallelism analysis for display.
pub fn format_parallelism(analysis: &ParallelGroups) -> String {
    let mut out = String::new();

    if analysis.groups.len() <= 1 && analysis.conflicts.is_empty() {
        if analysis.groups.is_empty() {
            out.push_str("  No ready subtasks.\n");
        } else {
            out.push_str(&format!("  Sequential: {}\n", analysis.groups[0].join(", ")));
        }
        return out;
    }

    out.push_str("  ┌── Parallel Execution Groups ──\n");
    for (i, group) in analysis.groups.iter().enumerate() {
        let label = if group.len() > 1 { "║" } else { "│" };
        out.push_str(&format!("  {} Group {}: {}\n", label, i + 1, group.join(" + ")));
    }
    out.push_str("  └────────────────────────────────\n");

    if !analysis.conflicts.is_empty() {
        out.push_str("  ⚠ File conflicts:\n");
        for c in &analysis.conflicts {
            out.push_str(&format!("    {} ↔ {} on: {}\n",
                c.subtask_a, c.subtask_b, c.shared_files.join(", ")));
        }
    }

    out
}

// ─── Plan Templates ─────────────────────────────────────────────────────────

/// A reusable plan template.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanTemplate {
    /// Template name (e.g., "rust-feature", "ts-api-endpoint")
    pub name: String,
    /// Description of when to use this template
    pub description: String,
    /// Languages this template applies to
    pub languages: Vec<String>,
    /// Template subtasks (with placeholder descriptions)
    pub subtasks: Vec<SubtaskPlan>,
    /// Notes to include in generated plans
    pub notes: Option<String>,
}

/// Built-in templates for common coding tasks.
pub fn builtin_templates() -> Vec<PlanTemplate> {
    vec![
        PlanTemplate {
            name: "rust-feature".to_string(),
            description: "Add a new feature to a Rust project".to_string(),
            languages: vec!["Rust".to_string()],
            subtasks: vec![
                SubtaskPlan {
                    id: "design".into(), title: "Design the API surface".into(),
                    description: Some("Define public types, traits, and function signatures".into()),
                    effort: Some("small".into()),
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "implement".into(), title: "Implement core logic".into(),
                    description: Some("Write the implementation, handle error cases".into()),
                    depends_on: vec!["design".into()],
                    effort: Some("large".into()),
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "tests".into(), title: "Add tests".into(),
                    description: Some("Unit tests for core logic, integration tests for API".into()),
                    depends_on: vec!["implement".into()],
                    effort: Some("medium".into()),
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "docs".into(), title: "Update documentation".into(),
                    description: Some("Doc comments, README updates if needed".into()),
                    depends_on: vec!["implement".into()],
                    effort: Some("small".into()),
                    ..Default::default()
                },
            ],
            notes: Some("Follow existing code conventions. Run cargo test --workspace before committing.".into()),
        },
        PlanTemplate {
            name: "ts-api-endpoint".to_string(),
            description: "Add a new API endpoint to a TypeScript/Node.js project".to_string(),
            languages: vec!["TypeScript".to_string(), "JavaScript".to_string()],
            subtasks: vec![
                SubtaskPlan {
                    id: "types".into(), title: "Define types/interfaces".into(),
                    description: Some("Request/response types, validation schemas".into()),
                    effort: Some("small".into()),
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "route".into(), title: "Add route handler".into(),
                    description: Some("Implement the endpoint with proper error handling".into()),
                    depends_on: vec!["types".into()],
                    effort: Some("medium".into()),
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "test".into(), title: "Add endpoint tests".into(),
                    description: Some("Unit + integration tests with mocked dependencies".into()),
                    depends_on: vec!["route".into()],
                    effort: Some("medium".into()),
                    ..Default::default()
                },
            ],
            notes: Some("Use existing middleware patterns. Add OpenAPI annotations if available.".into()),
        },
        PlanTemplate {
            name: "bug-fix".to_string(),
            description: "Fix a bug with proper regression testing".to_string(),
            languages: vec![],
            subtasks: vec![
                SubtaskPlan {
                    id: "reproduce".into(), title: "Reproduce the bug".into(),
                    description: Some("Write a failing test that demonstrates the bug".into()),
                    effort: Some("small".into()),
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "root-cause".into(), title: "Identify root cause".into(),
                    description: Some("Trace the issue through the code, identify the exact location".into()),
                    depends_on: vec!["reproduce".into()],
                    effort: Some("medium".into()),
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "fix".into(), title: "Implement the fix".into(),
                    description: Some("Make minimal, targeted changes to fix the issue".into()),
                    depends_on: vec!["root-cause".into()],
                    effort: Some("small".into()),
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "verify".into(), title: "Verify fix and test".into(),
                    description: Some("Ensure the failing test passes and no regressions".into()),
                    depends_on: vec!["fix".into()],
                    effort: Some("small".into()),
                    ..Default::default()
                },
            ],
            notes: Some("Test-first approach: write the failing test BEFORE fixing. Run full test suite after.".into()),
        },
        PlanTemplate {
            name: "refactor".to_string(),
            description: "Refactor code with safety nets".to_string(),
            languages: vec![],
            subtasks: vec![
                SubtaskPlan {
                    id: "baseline".into(), title: "Establish baseline".into(),
                    description: Some("Ensure all tests pass, document current behavior".into()),
                    effort: Some("small".into()),
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "refactor".into(), title: "Apply refactoring".into(),
                    description: Some("Make structural changes while preserving behavior".into()),
                    depends_on: vec!["baseline".into()],
                    effort: Some("large".into()),
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "verify".into(), title: "Verify no regressions".into(),
                    description: Some("Run full test suite, check for behavioral changes".into()),
                    depends_on: vec!["refactor".into()],
                    effort: Some("small".into()),
                    ..Default::default()
                },
            ],
            notes: Some("Make each refactoring step small and independently verifiable. Commit frequently.".into()),
        },
    ]
}

/// Find matching templates for a project context.
pub fn suggest_templates(context: &ProjectContext, goal: &str) -> Vec<&'static str> {
    let templates = builtin_templates();
    let goal_lower = goal.to_lowercase();
    let mut matches = Vec::new();

    for t in &templates {
        // Language match
        let lang_match = t.languages.is_empty() || t.languages.iter().any(|l|
            context.languages.iter().any(|cl| cl.eq_ignore_ascii_case(l))
        );
        if !lang_match {
            continue;
        }

        // Goal keyword match
        let name_match = goal_lower.contains(&t.name.replace('-', " "))
            || goal_lower.contains(&t.name);
        let desc_match = t.description.split_whitespace()
            .any(|w| w.len() > 3 && goal_lower.contains(&w.to_lowercase()));

        if name_match || desc_match {
            matches.push(t.name.as_str());
        }
    }

    // Static lifetime trick: return names that match the builtin list
    let builtin = builtin_templates();
    let mut result = Vec::new();
    for m in matches {
        for bt in &builtin {
            if bt.name == m {
                // Leak the string for static lifetime (these are a fixed small set)
                result.push(&*Box::leak(bt.name.clone().into_boxed_str()));
            }
        }
    }
    result
}

/// Instantiate a template by name, customizing the goal.
pub fn instantiate_template(name: &str, goal: &str) -> Option<TaskPlan> {
    let templates = builtin_templates();
    let template = templates.iter().find(|t| t.name == name)?;

    let mut plan = TaskPlan {
        subtasks: template.subtasks.clone(),
        notes: template.notes.clone(),
    };

    // Reset all statuses and add goal context to notes
    for st in &mut plan.subtasks {
        st.status = TaskStatus::Pending;
    }

    if let Some(ref mut notes) = plan.notes {
        *notes = format!("Goal: {}\n{}", goal, notes);
    } else {
        plan.notes = Some(format!("Goal: {}", goal));
    }

    Some(plan)
}

// ─── Plan Listing ───────────────────────────────────────────────────────────

/// List all saved plan state files.
pub fn list_saved_plans() -> Vec<SavedPlanInfo> {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let plans_dir = std::path::PathBuf::from(&home).join(".mo-agent");

    let mut result = Vec::new();

    // Check for active plan state
    let state_path = plans_dir.join("plan_state.json");
    if state_path.exists() {
        if let Ok(state) = PlanModeState::load_from_file(&state_path) {
            result.push(SavedPlanInfo {
                name: "active".to_string(),
                goal: state.goal,
                progress_pct: state.plan.progress_pct(),
                subtask_count: state.plan.subtasks.len(),
                status: if state.plan.progress_pct() == 100 { "completed" } else { "active" }.to_string(),
            });
        }
    }

    // Check for plan templates in templates dir
    let templates_dir = plans_dir.join("plan_templates");
    if templates_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(&templates_dir) {
            for entry in entries.flatten() {
                if let Some(ext) = entry.path().extension() {
                    if ext == "json" {
                        if let Ok(data) = std::fs::read_to_string(entry.path()) {
                            if let Ok(tmpl) = serde_json::from_str::<PlanTemplate>(&data) {
                                result.push(SavedPlanInfo {
                                    name: tmpl.name,
                                    goal: tmpl.description,
                                    progress_pct: 0,
                                    subtask_count: tmpl.subtasks.len(),
                                    status: "template".to_string(),
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    result
}

/// Summary info for a saved plan.
#[derive(Debug, Clone)]
pub struct SavedPlanInfo {
    pub name: String,
    pub goal: String,
    pub progress_pct: u32,
    pub subtask_count: usize,
    pub status: String,
}

/// Format saved plans for display.
pub fn format_plan_list(plans: &[SavedPlanInfo]) -> String {
    if plans.is_empty() {
        return "  No saved plans. Use /plan enter <goal> to create one.\n".to_string();
    }
    let mut out = String::new();
    out.push_str("  ┌── Saved Plans ──\n");
    for p in plans {
        let status_icon = match p.status.as_str() {
            "active" => "▶",
            "completed" => "✓",
            "template" => "📋",
            _ => "·",
        };
        out.push_str(&format!("  {} {} — {} ({}%, {}/{} subtasks)\n",
            status_icon, p.name, p.goal, p.progress_pct, 
            (p.subtask_count as u32 * p.progress_pct / 100.max(1)),
            p.subtask_count));
    }
    out.push_str("  └────────────────\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analyze_project_detects_cargo_toml() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let ctx = analyze_project(root);
        assert!(
            ctx.entry_points.contains(&"Cargo.toml".to_string()),
            "should detect Cargo.toml: {:?}",
            ctx.entry_points
        );
        assert!(
            ctx.languages.contains(&"Rust".to_string()),
            "should detect Rust: {:?}",
            ctx.languages
        );
    }

    #[test]
    fn parse_plan_response_json_block() {
        let response = r#"Here's the plan:
```json
{
  "subtasks": [
    {"id": "setup", "title": "Setup project", "description": "Init deps"}
  ],
  "notes": "Simple plan"
}
```
Done!"#;

        let plan = parse_plan_response(response).unwrap();
        assert_eq!(plan.subtasks.len(), 1);
        assert_eq!(plan.subtasks[0].id, "setup");
        assert_eq!(plan.notes, Some("Simple plan".to_string()));
    }

    #[test]
    fn parse_plan_response_raw_json() {
        let response = r#"{"subtasks": [{"id": "t1", "title": "Task 1"}]}"#;
        let plan = parse_plan_response(response).unwrap();
        assert_eq!(plan.subtasks.len(), 1);
    }

    #[test]
    fn format_plan_shows_progress() {
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "done".to_string(),
                    title: "Completed task".to_string(),
                    description: None,
                    depends_on: vec![],
                    status: TaskStatus::Completed,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "pending".to_string(),
                    title: "Pending task".to_string(),
                    description: Some("Needs work".to_string()),
                    depends_on: vec!["done".to_string()],
                    status: TaskStatus::Pending,
                    ..Default::default()
                },
            ],
            notes: Some("Test plan".to_string()),
        };

        let formatted = format_plan(&plan);
        assert!(
            formatted.contains("✓"),
            "should show completed: {formatted}"
        );
        assert!(formatted.contains("50%"), "should show 50%: {formatted}");
        assert!(
            formatted.contains("pending"),
            "should be ready: {formatted}"
        );
    }

    #[test]
    fn decomposition_prompt_includes_context() {
        let ctx = ProjectContext {
            root: "/test".to_string(),
            entry_points: vec!["Cargo.toml".to_string()],
            languages: vec!["Rust".to_string()],
            structure_summary: "src, tests".to_string(),
            source_file_count: 42,
            ..Default::default()
        };

        let prompt = decomposition_prompt("Add logging", &ctx);
        assert!(prompt.contains("Rust"), "should include language: {prompt}");
        assert!(
            prompt.contains("Cargo.toml"),
            "should include entry point: {prompt}"
        );
        assert!(
            prompt.contains("Add logging"),
            "should include goal: {prompt}"
        );
    }

    #[test]
    fn ready_subtasks_respects_dependencies() {
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "a".to_string(),
                    title: "First".to_string(),
                    description: None,
                    depends_on: vec![],
                    status: TaskStatus::Pending,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "b".to_string(),
                    title: "Second".to_string(),
                    description: None,
                    depends_on: vec!["a".to_string()],
                    status: TaskStatus::Pending,
                    ..Default::default()
                },
            ],
            notes: None,
        };

        let ready = plan.ready_subtasks();
        assert_eq!(ready.len(), 1, "only 'a' should be ready");
        assert_eq!(ready[0].id, "a");
    }

    // ─── New tests for Plan Mode gaps ───────────────────────────────────

    #[test]
    fn is_execute_command_detects_user_input() {
        assert!(PlanModeState::is_execute_command("execute"));
        assert!(PlanModeState::is_execute_command("Execute"));
        assert!(PlanModeState::is_execute_command("go"));
        assert!(PlanModeState::is_execute_command("start"));
        assert!(PlanModeState::is_execute_command("done"));
        assert!(PlanModeState::is_execute_command("run"));
        assert!(PlanModeState::is_execute_command("开始"));
        assert!(PlanModeState::is_execute_command("执行"));
        assert!(PlanModeState::is_execute_command("运行"));
        // Negatives
        assert!(!PlanModeState::is_execute_command("add a task"));
        assert!(!PlanModeState::is_execute_command("simplify the plan"));
        assert!(!PlanModeState::is_execute_command("[PLAN_EXECUTE]"));
    }

    #[test]
    fn is_execute_command_trims_whitespace() {
        assert!(PlanModeState::is_execute_command("  go  "));
        assert!(PlanModeState::is_execute_command(" Execute "));
    }

    #[test]
    fn complete_subtask_by_prefix() {
        let ctx = ProjectContext::default();
        let mut ps = PlanModeState::new("test".into(), ctx);
        ps.set_plan(TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "setup-deps".into(),
                    title: "Install deps".into(),
                    description: None,
                    depends_on: vec![],
                    status: TaskStatus::Pending,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "write-code".into(),
                    title: "Write code".into(),
                    description: None,
                    depends_on: vec!["setup-deps".into()],
                    status: TaskStatus::Pending,
                    ..Default::default()
                },
            ],
            notes: None,
        });

        // Complete by prefix
        let result = ps.complete_subtask("setup");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "Install deps");
        assert_eq!(ps.plan.subtasks[0].status, TaskStatus::Completed);
        assert!(ps.modified);

        // Now "write-code" should be ready
        let ready = ps.plan.ready_subtasks();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "write-code");
    }

    #[test]
    fn complete_subtask_not_found() {
        let ctx = ProjectContext::default();
        let mut ps = PlanModeState::new("test".into(), ctx);
        ps.set_plan(TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "alpha".into(),
                title: "A".into(),
                description: None,
                depends_on: vec![],
                status: TaskStatus::Pending,
                ..Default::default()
            }],
            notes: None,
        });

        let result = ps.complete_subtask("beta");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("No subtask"));
    }

    #[test]
    fn complete_subtask_ambiguous() {
        let ctx = ProjectContext::default();
        let mut ps = PlanModeState::new("test".into(), ctx);
        ps.set_plan(TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "test-unit".into(),
                    title: "Unit tests".into(),
                    description: None,
                    depends_on: vec![],
                    status: TaskStatus::Pending,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "test-integration".into(),
                    title: "Integration tests".into(),
                    description: None,
                    depends_on: vec![],
                    status: TaskStatus::Pending,
                    ..Default::default()
                },
            ],
            notes: None,
        });

        let result = ps.complete_subtask("test");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Ambiguous"));
    }

    #[test]
    fn save_and_load_plan_state() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

        let ctx = ProjectContext {
            root: "/test".into(),
            languages: vec!["Rust".into()],
            ..Default::default()
        };
        let mut ps = PlanModeState::new("Build a thing".into(), ctx);
        ps.set_plan(TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "step1".into(),
                title: "First step".into(),
                description: Some("Do it".into()),
                depends_on: vec![],
                status: TaskStatus::Pending,
                ..Default::default()
            }],
            notes: Some("my notes".into()),
        });
        ps.add_turn("simplify", "OK done");

        // Save
        ps.save_to_file(&path).unwrap();

        // Load
        let loaded = PlanModeState::load_from_file(&path).unwrap();
        assert_eq!(loaded.goal, "Build a thing");
        assert_eq!(loaded.plan.subtasks.len(), 1);
        assert_eq!(loaded.plan.subtasks[0].id, "step1");
        assert_eq!(loaded.plan.notes, Some("my notes".into()));
        assert_eq!(loaded.history.len(), 1);
        assert_eq!(loaded.context.languages, vec!["Rust".to_string()]);
    }

    #[test]
    fn load_from_missing_file_errors() {
        let result = PlanModeState::load_from_file(std::path::Path::new("/nonexistent/plan.json"));
        assert!(result.is_err());
    }

    #[test]
    fn plan_mode_prompt_includes_plan_and_goal() {
        let ctx = ProjectContext::default();
        let mut ps = PlanModeState::new("Add auth".into(), ctx);
        ps.set_plan(TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "jwt".into(),
                title: "Add JWT".into(),
                description: None,
                depends_on: vec![],
                status: TaskStatus::Pending,
                ..Default::default()
            }],
            notes: None,
        });

        let prompt = ps.plan_mode_prompt("make it simpler");
        assert!(prompt.contains("Add auth"), "should contain goal");
        assert!(prompt.contains("jwt"), "should contain plan subtask");
        assert!(
            prompt.contains("make it simpler"),
            "should contain user request"
        );
        assert!(
            prompt.contains("PLAN MODE"),
            "should contain mode indicator"
        );
    }

    #[test]
    fn plan_mode_prompt_includes_history() {
        let ctx = ProjectContext::default();
        let mut ps = PlanModeState::new("goal".into(), ctx);
        ps.add_turn("q1", "a1");
        ps.add_turn("q2", "a2");

        let prompt = ps.plan_mode_prompt("q3");
        assert!(prompt.contains("Recent Discussion"), "should have history");
        assert!(prompt.contains("q1"), "should contain first turn");
    }

    #[test]
    fn to_memory_content_format() {
        let ctx = ProjectContext::default();
        let mut ps = PlanModeState::new("Deploy app".into(), ctx);
        ps.set_plan(TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "s1".into(),
                title: "Step 1".into(),
                description: None,
                depends_on: vec![],
                status: TaskStatus::Pending,
                ..Default::default()
            }],
            notes: None,
        });

        let content = ps.to_memory_content();
        assert!(content.starts_with("[plan:active]"));
        assert!(content.contains("Deploy app"));

        let completed = ps.to_completed_memory();
        assert!(completed.starts_with("[plan:completed]"));
        assert!(completed.contains("1 subtasks"));
    }

    #[test]
    fn parse_plan_response_invalid_json() {
        let result = parse_plan_response("not json at all");
        assert!(result.is_err());
    }

    #[test]
    fn parse_plan_response_missing_subtasks() {
        let result = parse_plan_response(r#"{"notes": "just notes"}"#);
        // Should fail because subtasks field is required
        assert!(result.is_err());
    }

    #[test]
    fn progress_tracking_after_completion() {
        let ctx = ProjectContext::default();
        let mut ps = PlanModeState::new("test".into(), ctx);
        ps.set_plan(TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "a".into(),
                    title: "A".into(),
                    description: None,
                    depends_on: vec![],
                    status: TaskStatus::Pending,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "b".into(),
                    title: "B".into(),
                    description: None,
                    depends_on: vec![],
                    status: TaskStatus::Pending,
                    ..Default::default()
                },
            ],
            notes: None,
        });

        assert_eq!(ps.plan.progress_pct(), 0);
        assert_eq!(ps.plan.items_done(), 0);

        ps.complete_subtask("a").unwrap();
        assert_eq!(ps.plan.progress_pct(), 50);
        assert_eq!(ps.plan.items_done(), 1);

        ps.complete_subtask("b").unwrap();
        assert_eq!(ps.plan.progress_pct(), 100);
        assert_eq!(ps.plan.items_done(), 2);
    }

    // ─── Enhanced plan features tests ───────────────────────────────────

    #[test]
    fn analyze_project_detects_key_modules() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let ctx = analyze_project(root);
        assert!(
            !ctx.key_modules.is_empty(),
            "should find key modules in Rust project"
        );
        // Largest module should have a decent line count
        let (_, lines) = &ctx.key_modules[0];
        assert!(
            *lines > 50,
            "largest module should be >50 lines, got {lines}"
        );
    }

    #[test]
    fn analyze_project_detects_test_framework() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let ctx = analyze_project(root);
        assert_eq!(ctx.test_framework.as_deref(), Some("cargo test"));
    }

    #[test]
    fn analyze_project_detects_git_branch() {
        // This test runs in a git repo
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .expect("should find repo root");
        let ctx = analyze_project(root);
        assert!(ctx.git_branch.is_some(), "should detect git branch");
    }

    #[test]
    fn decomposition_prompt_includes_new_fields() {
        let ctx = ProjectContext {
            root: "/myapp".to_string(),
            entry_points: vec!["package.json".to_string()],
            languages: vec!["TypeScript".to_string()],
            structure_summary: "src, tests".to_string(),
            source_file_count: 100,
            key_modules: vec![
                ("src/api.ts".to_string(), 500),
                ("src/db.ts".to_string(), 300),
            ],
            git_branch: Some("feature/auth".to_string()),
            test_framework: Some("jest".to_string()),
        };

        let prompt = decomposition_prompt("Add user authentication", &ctx);
        assert!(
            prompt.contains("Key Modules"),
            "should include modules section: {prompt}"
        );
        assert!(
            prompt.contains("src/api.ts"),
            "should list key module: {prompt}"
        );
        assert!(
            prompt.contains("500 lines"),
            "should show line count: {prompt}"
        );
        assert!(
            prompt.contains("feature/auth"),
            "should include branch: {prompt}"
        );
        assert!(
            prompt.contains("jest"),
            "should include test framework: {prompt}"
        );
        assert!(prompt.contains("effort"), "should ask for effort: {prompt}");
        assert!(prompt.contains("files"), "should ask for files: {prompt}");
        assert!(
            prompt.contains("acceptance"),
            "should ask for acceptance: {prompt}"
        );
    }

    #[test]
    fn parse_plan_response_with_effort_and_files() {
        let response = r#"```json
{
  "subtasks": [
    {
      "id": "add-model",
      "title": "Add User model",
      "description": "Create user schema",
      "depends_on": [],
      "effort": "small",
      "files": ["src/models/user.ts", "src/db/schema.ts"],
      "acceptance": "User model compiles and has tests"
    },
    {
      "id": "add-api",
      "title": "Add auth endpoints",
      "depends_on": ["add-model"],
      "effort": "large",
      "files": ["src/routes/auth.ts"]
    }
  ],
  "notes": "Use JWT for stateless auth"
}
```"#;

        let plan = parse_plan_response(response).unwrap();
        assert_eq!(plan.subtasks.len(), 2);

        let s0 = &plan.subtasks[0];
        assert_eq!(s0.effort.as_deref(), Some("small"));
        assert_eq!(s0.files, vec!["src/models/user.ts", "src/db/schema.ts"]);
        assert_eq!(
            s0.acceptance.as_deref(),
            Some("User model compiles and has tests")
        );

        let s1 = &plan.subtasks[1];
        assert_eq!(s1.effort.as_deref(), Some("large"));
        assert!(s1.acceptance.is_none(), "optional field should be None");
    }

    #[test]
    fn format_plan_shows_effort_and_files() {
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "impl".into(),
                    title: "Implement feature".into(),
                    description: Some("Do the thing".into()),
                    depends_on: vec![],
                    status: TaskStatus::Pending,
                    effort: Some("medium".into()),
                    files: vec!["src/feat.rs".into(), "tests/feat_test.rs".into()],
                    acceptance: Some("cargo test passes".into()),
                },
                SubtaskPlan {
                    id: "docs".into(),
                    title: "Write docs".into(),
                    description: None,
                    depends_on: vec!["impl".into()],
                    status: TaskStatus::Pending,
                    effort: Some("small".into()),
                    ..Default::default()
                },
            ],
            notes: None,
        };

        let output = format_plan(&plan);
        assert!(
            output.contains("[M]"),
            "should show medium effort badge: {output}"
        );
        assert!(
            output.contains("[S]"),
            "should show small effort badge: {output}"
        );
        assert!(output.contains("📁"), "should show files icon: {output}");
        assert!(output.contains("src/feat.rs"), "should list file: {output}");
        assert!(output.contains("✅"), "should show acceptance: {output}");
        assert!(
            output.contains("cargo test"),
            "should show acceptance criteria: {output}"
        );
        assert!(
            output.contains("Effort:"),
            "should show effort summary: {output}"
        );
    }

    #[test]
    fn parse_plan_response_backward_compatible() {
        // Old format without effort/files/acceptance should still parse
        let response = r#"{"subtasks": [{"id": "t1", "title": "Do thing"}]}"#;
        let plan = parse_plan_response(response).unwrap();
        assert_eq!(plan.subtasks[0].effort, None);
        assert!(plan.subtasks[0].files.is_empty());
        assert!(plan.subtasks[0].acceptance.is_none());
    }

    // ═══════════════════════════ Auto-Execution Tests ═══════════════════════

    #[test]
    fn format_subtask_prompt_minimal() {
        let st = SubtaskPlan {
            id: "t1".into(),
            title: "Add login page".into(),
            ..Default::default()
        };
        let prompt = format_subtask_prompt(&st);
        assert!(prompt.contains("Add login page"));
        assert!(prompt.contains("implement this change"));
        // No description, files, or acceptance → those sections omitted
        assert!(!prompt.contains("Description:"));
        assert!(!prompt.contains("Files to modify:"));
        assert!(!prompt.contains("Acceptance criteria:"));
    }

    #[test]
    fn format_subtask_prompt_full() {
        let st = SubtaskPlan {
            id: "t2".into(),
            title: "Add auth middleware".into(),
            description: Some("JWT token validation for all /api routes".into()),
            files: vec!["src/middleware.rs".into(), "src/auth.rs".into()],
            acceptance: Some("All /api routes return 401 without valid token".into()),
            ..Default::default()
        };
        let prompt = format_subtask_prompt(&st);
        assert!(prompt.contains("Add auth middleware"));
        assert!(prompt.contains("JWT token validation"));
        assert!(prompt.contains("src/middleware.rs, src/auth.rs"));
        assert!(prompt.contains("401 without valid token"));
    }

    #[test]
    fn format_subtask_prompt_preserves_description_detail() {
        let st = SubtaskPlan {
            id: "t3".into(),
            title: "Refactor DB layer".into(),
            description: Some(
                "Extract connection pooling into a separate module.\nAdd retry logic.".into(),
            ),
            ..Default::default()
        };
        let prompt = format_subtask_prompt(&st);
        assert!(prompt.contains("Extract connection pooling"));
        assert!(prompt.contains("retry logic"));
    }

    #[test]
    fn plan_auto_execution_dependency_ordering() {
        // Verify ready_subtasks respects dependencies
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "setup".into(),
                    title: "Setup deps".into(),
                    status: TaskStatus::Pending,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "impl".into(),
                    title: "Implement feature".into(),
                    depends_on: vec!["setup".into()],
                    status: TaskStatus::Pending,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "test".into(),
                    title: "Add tests".into(),
                    depends_on: vec!["impl".into()],
                    status: TaskStatus::Pending,
                    ..Default::default()
                },
            ],
            notes: None,
        };

        // Only "setup" should be ready initially
        let ready = plan.ready_subtasks();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "setup");
    }

    #[test]
    fn plan_auto_execution_unblocks_after_completion() {
        let mut plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "setup".into(),
                    title: "Setup deps".into(),
                    status: TaskStatus::Completed,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "impl".into(),
                    title: "Implement feature".into(),
                    depends_on: vec!["setup".into()],
                    status: TaskStatus::Pending,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "test".into(),
                    title: "Add tests".into(),
                    depends_on: vec!["impl".into()],
                    status: TaskStatus::Pending,
                    ..Default::default()
                },
            ],
            notes: None,
        };

        // After "setup" completes, "impl" should be ready
        let ready = plan.ready_subtasks();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "impl");

        // "test" still blocked
        assert!(!ready.iter().any(|s| s.id == "test"));

        // Complete "impl" too
        plan.subtasks[1].status = TaskStatus::Completed;
        let ready = plan.ready_subtasks();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "test");
    }

    #[test]
    fn plan_progress_tracking() {
        let mut plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "a".into(),
                    title: "A".into(),
                    status: TaskStatus::Completed,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "b".into(),
                    title: "B".into(),
                    status: TaskStatus::InProgress,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "c".into(),
                    title: "C".into(),
                    status: TaskStatus::Pending,
                    ..Default::default()
                },
            ],
            notes: None,
        };

        assert_eq!(plan.progress_pct(), 33); // 1/3
        assert_eq!(plan.items_done(), 1);

        plan.subtasks[1].status = TaskStatus::Completed;
        assert_eq!(plan.progress_pct(), 66); // 2/3
        assert_eq!(plan.items_done(), 2);

        plan.subtasks[2].status = TaskStatus::Completed;
        assert_eq!(plan.progress_pct(), 100);
        assert_eq!(plan.items_done(), 3);
    }

    #[test]
    fn plan_parallel_subtasks_all_ready() {
        // Multiple subtasks with no deps should all be ready at once
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "a".into(),
                    title: "A".into(),
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "b".into(),
                    title: "B".into(),
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "c".into(),
                    title: "C".into(),
                    ..Default::default()
                },
            ],
            notes: None,
        };
        let ready = plan.ready_subtasks();
        assert_eq!(ready.len(), 3);
    }

    #[test]
    fn plan_blocked_by_incomplete_dep() {
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "a".into(),
                    title: "A".into(),
                    status: TaskStatus::InProgress,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "b".into(),
                    title: "B".into(),
                    depends_on: vec!["a".into()],
                    ..Default::default()
                },
            ],
            notes: None,
        };
        // "a" is in-progress (not completed), so "b" is blocked
        let ready = plan.ready_subtasks();
        assert!(
            ready.is_empty(),
            "b should be blocked while a is in-progress"
        );
    }

    #[test]
    fn plan_execution_simulates_full_run() {
        // Simulate the auto-execution loop logic without the async chat call
        let mut plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "a".into(),
                    title: "Step A".into(),
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "b".into(),
                    title: "Step B".into(),
                    depends_on: vec!["a".into()],
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "c".into(),
                    title: "Step C".into(),
                    depends_on: vec!["b".into()],
                    ..Default::default()
                },
            ],
            notes: None,
        };

        let mut executed_order = Vec::new();

        // Simulate the execution loop
        loop {
            // Mark any in-progress as completed
            for st in plan.subtasks.iter_mut() {
                if st.status == TaskStatus::InProgress {
                    st.status = TaskStatus::Completed;
                    break;
                }
            }

            // Find next ready
            let next = plan.ready_subtasks().first().map(|s| s.id.clone());
            match next {
                Some(id) => {
                    let st = plan.subtasks.iter_mut().find(|s| s.id == id).unwrap();
                    st.status = TaskStatus::InProgress;
                    executed_order.push(id);
                }
                None => break,
            }
        }

        assert_eq!(executed_order, vec!["a", "b", "c"]);
        assert_eq!(plan.progress_pct(), 100);
    }

    #[test]
    fn plan_execution_pause_preserves_state() {
        // Simulate Ctrl+C pause: in-progress subtask stays in-progress
        let mut plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "a".into(),
                    title: "Step A".into(),
                    status: TaskStatus::Completed,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "b".into(),
                    title: "Step B".into(),
                    depends_on: vec!["a".into()],
                    status: TaskStatus::InProgress,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "c".into(),
                    title: "Step C".into(),
                    depends_on: vec!["b".into()],
                    ..Default::default()
                },
            ],
            notes: None,
        };

        // After pause: b is still in-progress, c is still pending
        assert_eq!(plan.progress_pct(), 33); // only a is completed
        let remaining = plan
            .subtasks
            .iter()
            .filter(|s| s.status == TaskStatus::Pending || s.status == TaskStatus::InProgress)
            .count();
        assert_eq!(remaining, 2);

        // Resume: complete b, then c should become ready
        plan.subtasks[1].status = TaskStatus::Completed;
        let ready = plan.ready_subtasks();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "c");
    }

    #[test]
    fn is_resume_command_detects_keywords() {
        assert!(is_resume_command("continue"));
        assert!(is_resume_command("Continue"));
        assert!(is_resume_command("resume"));
        assert!(is_resume_command("go"));
        assert!(is_resume_command("next"));
        assert!(is_resume_command("继续"));
        // Whitespace
        assert!(is_resume_command("  continue  "));
        assert!(is_resume_command(" RESUME "));
    }

    #[test]
    fn is_resume_command_rejects_non_resume() {
        assert!(!is_resume_command("hello"));
        assert!(!is_resume_command("fix the bug"));
        assert!(!is_resume_command("continue with something else"));
        assert!(!is_resume_command(""));
    }

    // ═══════════════════════════ Plan Versioning Tests ════════════════════════

    #[test]
    fn version_history_record_and_retrieve() {
        let mut history = PlanVersionHistory::default();
        let plan = TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "a".into(), title: "A".into(), ..Default::default()
            }],
            notes: None,
        };

        let v1 = history.record(&plan, "Initial plan");
        assert_eq!(v1, 1);
        assert_eq!(history.versions.len(), 1);

        let v2 = history.record(&plan, "Added subtask");
        assert_eq!(v2, 2);
        assert_eq!(history.current_version, 2);

        let retrieved = history.get_version(1).unwrap();
        assert_eq!(retrieved.change_summary, "Initial plan");
    }

    #[test]
    fn version_diff_detects_changes() {
        let mut history = PlanVersionHistory::default();

        let plan_v1 = TaskPlan {
            subtasks: vec![
                SubtaskPlan { id: "a".into(), title: "A".into(), ..Default::default() },
                SubtaskPlan { id: "b".into(), title: "B".into(), ..Default::default() },
            ],
            notes: None,
        };
        history.record(&plan_v1, "v1");

        let plan_v2 = TaskPlan {
            subtasks: vec![
                SubtaskPlan { id: "a".into(), title: "A modified".into(), ..Default::default() },
                SubtaskPlan { id: "c".into(), title: "C new".into(), ..Default::default() },
            ],
            notes: None,
        };
        history.record(&plan_v2, "v2");

        let diff = history.diff_versions(1, 2).unwrap();
        assert!(diff.added.contains(&"c".to_string()), "c should be added");
        assert!(diff.removed.contains(&"b".to_string()), "b should be removed");
        assert!(diff.modified.contains(&"a".to_string()), "a should be modified");
    }

    #[test]
    fn version_diff_no_changes() {
        let mut history = PlanVersionHistory::default();
        let plan = TaskPlan {
            subtasks: vec![SubtaskPlan { id: "a".into(), title: "A".into(), ..Default::default() }],
            notes: None,
        };
        history.record(&plan, "v1");
        history.record(&plan, "v2");

        let diff = history.diff_versions(1, 2).unwrap();
        assert!(diff.is_empty());
    }

    #[test]
    fn version_diff_invalid_version() {
        let history = PlanVersionHistory::default();
        assert!(history.diff_versions(1, 2).is_err());
    }

    #[test]
    fn version_log_format() {
        let mut history = PlanVersionHistory::default();
        let plan = TaskPlan { subtasks: vec![], notes: None };
        history.record(&plan, "Created");
        history.record(&plan, "Added tasks");

        let log = history.format_log();
        assert!(log.contains("v1"));
        assert!(log.contains("v2"));
        assert!(log.contains("Created"));
        assert!(log.contains("Added tasks"));
    }

    // ═══════════════════════════ Parallel Subtask Tests ══════════════════════

    #[test]
    fn parallel_groups_no_deps_all_parallel() {
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan { id: "a".into(), title: "A".into(), ..Default::default() },
                SubtaskPlan { id: "b".into(), title: "B".into(), ..Default::default() },
                SubtaskPlan { id: "c".into(), title: "C".into(), ..Default::default() },
            ],
            notes: None,
        };

        let analysis = analyze_parallelism(&plan);
        assert_eq!(analysis.groups.len(), 1, "all should be in one group");
        assert_eq!(analysis.groups[0].len(), 3);
        assert!(analysis.conflicts.is_empty());
    }

    #[test]
    fn parallel_groups_with_file_conflict() {
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "a".into(), title: "A".into(),
                    files: vec!["src/main.rs".into()],
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "b".into(), title: "B".into(),
                    files: vec!["src/main.rs".into(), "src/lib.rs".into()],
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "c".into(), title: "C".into(),
                    files: vec!["src/other.rs".into()],
                    ..Default::default()
                },
            ],
            notes: None,
        };

        let analysis = analyze_parallelism(&plan);
        assert!(analysis.conflicts.len() >= 1, "should detect a-b conflict");
        assert!(analysis.conflicts[0].shared_files.contains(&"src/main.rs".to_string()));

        // a and b should be in different groups, c can go with either
        assert!(analysis.groups.len() >= 2, "should split conflicting subtasks: {:?}", analysis.groups);
    }

    #[test]
    fn parallel_groups_single_subtask() {
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan { id: "only".into(), title: "Only one".into(), ..Default::default() },
            ],
            notes: None,
        };

        let analysis = analyze_parallelism(&plan);
        assert_eq!(analysis.groups.len(), 1);
        assert_eq!(analysis.groups[0], vec!["only"]);
        assert!(analysis.conflicts.is_empty());
    }

    #[test]
    fn parallel_groups_respects_dependency_filter() {
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan { id: "a".into(), title: "A".into(), ..Default::default() },
                SubtaskPlan {
                    id: "b".into(), title: "B".into(),
                    depends_on: vec!["a".into()],
                    ..Default::default()
                },
            ],
            notes: None,
        };

        let analysis = analyze_parallelism(&plan);
        // Only "a" is ready, "b" depends on "a"
        assert_eq!(analysis.groups.len(), 1);
        assert_eq!(analysis.groups[0], vec!["a"]);
    }

    #[test]
    fn format_parallelism_display() {
        let analysis = ParallelGroups {
            groups: vec![
                vec!["a".into(), "c".into()],
                vec!["b".into()],
            ],
            conflicts: vec![FileConflict {
                subtask_a: "a".into(),
                subtask_b: "b".into(),
                shared_files: vec!["src/main.rs".into()],
            }],
        };

        let output = format_parallelism(&analysis);
        assert!(output.contains("Group 1"), "should show groups");
        assert!(output.contains("a + c"), "group 1 should have a and c");
        assert!(output.contains("Group 2"), "should have second group");
        assert!(output.contains("⚠"), "should show conflict warning");
        assert!(output.contains("src/main.rs"), "should show conflicting file");
    }

    // ═══════════════════════════ Plan Template Tests ═════════════════════════

    #[test]
    fn builtin_templates_exist() {
        let templates = builtin_templates();
        assert!(templates.len() >= 3, "should have at least 3 templates");

        let names: Vec<&str> = templates.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"rust-feature"));
        assert!(names.contains(&"bug-fix"));
        assert!(names.contains(&"refactor"));
    }

    #[test]
    fn builtin_templates_have_valid_deps() {
        for template in builtin_templates() {
            let ids: Vec<&str> = template.subtasks.iter().map(|s| s.id.as_str()).collect();
            for st in &template.subtasks {
                for dep in &st.depends_on {
                    assert!(ids.contains(&dep.as_str()),
                        "Template '{}': subtask '{}' depends on '{}' which doesn't exist",
                        template.name, st.id, dep);
                }
            }
        }
    }

    #[test]
    fn instantiate_template_customizes_goal() {
        let plan = instantiate_template("bug-fix", "Fix login timeout").unwrap();
        assert!(!plan.subtasks.is_empty());
        assert!(plan.notes.as_ref().unwrap().contains("Fix login timeout"));

        // All statuses should be Pending
        for st in &plan.subtasks {
            assert_eq!(st.status, TaskStatus::Pending);
        }
    }

    #[test]
    fn instantiate_template_unknown_returns_none() {
        assert!(instantiate_template("nonexistent-template", "goal").is_none());
    }

    #[test]
    fn instantiate_template_rust_feature() {
        let plan = instantiate_template("rust-feature", "Add user auth").unwrap();
        let ids: Vec<&str> = plan.subtasks.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(&"design"));
        assert!(ids.contains(&"implement"));
        assert!(ids.contains(&"tests"));
    }

    // ═══════════════════════════ Plan List Tests ═════════════════════════════

    #[test]
    fn format_plan_list_empty() {
        let output = format_plan_list(&[]);
        assert!(output.contains("No saved plans"));
    }

    #[test]
    fn format_plan_list_with_entries() {
        let plans = vec![
            SavedPlanInfo {
                name: "active".to_string(),
                goal: "Build auth system".to_string(),
                progress_pct: 50,
                subtask_count: 4,
                status: "active".to_string(),
            },
            SavedPlanInfo {
                name: "rust-feature".to_string(),
                goal: "Add a feature template".to_string(),
                progress_pct: 0,
                subtask_count: 3,
                status: "template".to_string(),
            },
        ];

        let output = format_plan_list(&plans);
        assert!(output.contains("▶"), "active plan should have play icon");
        assert!(output.contains("📋"), "template should have clipboard icon");
        assert!(output.contains("Build auth system"));
    }

    // ═══════════════════════════ PlanDiff Format Tests ═══════════════════════

    #[test]
    fn plan_diff_format_shows_changes() {
        let diff = PlanDiff {
            from_version: 1,
            to_version: 2,
            added: vec!["new-task".into()],
            removed: vec!["old-task".into()],
            modified: vec!["changed-task".into()],
        };

        let output = diff.format();
        assert!(output.contains("+ new-task"));
        assert!(output.contains("- old-task"));
        assert!(output.contains("~ changed-task"));
        assert!(output.contains("v1 → v2"));
    }

    #[test]
    fn plan_diff_empty_format() {
        let diff = PlanDiff {
            from_version: 1, to_version: 2,
            added: vec![], removed: vec![], modified: vec![],
        };
        assert!(diff.is_empty());
        assert!(diff.format().contains("no changes"));
    }

    // ═══════════════════════ Parallel Execution Simulation ═══════════════════

    #[test]
    fn parallel_execution_simulation_groups() {
        // Simulate the parallel-group-aware execution loop
        let mut plan = TaskPlan {
            subtasks: vec![
                // Group 1: a and b are independent, can run in parallel
                SubtaskPlan {
                    id: "a".into(), title: "Step A".into(),
                    files: vec!["src/a.rs".into()],
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "b".into(), title: "Step B".into(),
                    files: vec!["src/b.rs".into()],
                    ..Default::default()
                },
                // Group 2: c depends on a, d depends on b
                SubtaskPlan {
                    id: "c".into(), title: "Step C".into(),
                    depends_on: vec!["a".into()],
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "d".into(), title: "Step D".into(),
                    depends_on: vec!["b".into()],
                    ..Default::default()
                },
                // Group 3: e depends on c and d
                SubtaskPlan {
                    id: "e".into(), title: "Step E".into(),
                    depends_on: vec!["c".into(), "d".into()],
                    ..Default::default()
                },
            ],
            notes: None,
        };

        let mut execution_rounds: Vec<Vec<String>> = Vec::new();

        loop {
            let analysis = analyze_parallelism(&plan);
            let group = match analysis.groups.first() {
                Some(g) if !g.is_empty() => g.clone(),
                _ => break,
            };

            let mut round = Vec::new();
            for id in &group {
                let st = plan.subtasks.iter_mut().find(|s| s.id == *id).unwrap();
                st.status = TaskStatus::InProgress;
                round.push(id.clone());
            }
            // Simulate completion
            for id in &round {
                let st = plan.subtasks.iter_mut().find(|s| s.id == *id).unwrap();
                st.status = TaskStatus::Completed;
            }
            execution_rounds.push(round);
        }

        assert_eq!(execution_rounds.len(), 3, "should have 3 rounds: {:?}", execution_rounds);
        // Round 1: a and b (no conflicts, no deps)
        assert!(execution_rounds[0].contains(&"a".to_string()));
        assert!(execution_rounds[0].contains(&"b".to_string()));
        // Round 2: c and d (unblocked after a and b)
        assert!(execution_rounds[1].contains(&"c".to_string()));
        assert!(execution_rounds[1].contains(&"d".to_string()));
        // Round 3: e (depends on c and d)
        assert_eq!(execution_rounds[2], vec!["e"]);
        assert_eq!(plan.progress_pct(), 100);
    }

    #[test]
    fn parallel_execution_with_file_conflicts_splits_groups() {
        let mut plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "a".into(), title: "A".into(),
                    files: vec!["shared.rs".into()],
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "b".into(), title: "B".into(),
                    files: vec!["shared.rs".into()],
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "c".into(), title: "C".into(),
                    files: vec!["other.rs".into()],
                    ..Default::default()
                },
            ],
            notes: None,
        };

        let analysis = analyze_parallelism(&plan);
        // a and b conflict on shared.rs, so they should be in different groups
        assert!(analysis.groups.len() >= 2, "conflicting tasks should split: {:?}", analysis.groups);
        assert!(!analysis.conflicts.is_empty());

        // Simulate group-by-group execution
        let mut rounds = Vec::new();
        loop {
            let analysis = analyze_parallelism(&plan);
            let group = match analysis.groups.first() {
                Some(g) if !g.is_empty() => g.clone(),
                _ => break,
            };
            for id in &group {
                let st = plan.subtasks.iter_mut().find(|s| s.id == *id).unwrap();
                st.status = TaskStatus::Completed;
            }
            rounds.push(group);
        }

        // All 3 tasks should complete, but in at least 2 rounds due to conflict
        assert!(rounds.len() >= 2, "file conflict should force multiple rounds: {:?}", rounds);
        assert_eq!(plan.progress_pct(), 100);
    }

    #[test]
    fn parallel_execution_single_chain_is_sequential() {
        // Linear dependency chain: a → b → c
        let mut plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan { id: "a".into(), title: "A".into(), ..Default::default() },
                SubtaskPlan { id: "b".into(), title: "B".into(),
                    depends_on: vec!["a".into()], ..Default::default() },
                SubtaskPlan { id: "c".into(), title: "C".into(),
                    depends_on: vec!["b".into()], ..Default::default() },
            ],
            notes: None,
        };

        let mut rounds = Vec::new();
        loop {
            let analysis = analyze_parallelism(&plan);
            let group = match analysis.groups.first() {
                Some(g) if !g.is_empty() => g.clone(),
                _ => break,
            };
            assert_eq!(group.len(), 1, "sequential chain should yield groups of 1");
            for id in &group {
                let st = plan.subtasks.iter_mut().find(|s| s.id == *id).unwrap();
                st.status = TaskStatus::Completed;
            }
            rounds.push(group);
        }

        assert_eq!(rounds.len(), 3, "should be 3 sequential rounds");
        assert_eq!(rounds[0], vec!["a"]);
        assert_eq!(rounds[1], vec!["b"]);
        assert_eq!(rounds[2], vec!["c"]);
    }

    #[test]
    fn version_history_in_plan_mode_state() {
        let ctx = ProjectContext::default();
        let mut ps = PlanModeState::new("test".into(), ctx);

        // set_plan should auto-record version
        ps.set_plan(TaskPlan {
            subtasks: vec![SubtaskPlan { id: "a".into(), title: "A".into(), ..Default::default() }],
            notes: None,
        });
        assert_eq!(ps.version_history.current_version, 1);

        // update_plan should also record
        ps.update_plan(TaskPlan {
            subtasks: vec![
                SubtaskPlan { id: "a".into(), title: "A".into(), ..Default::default() },
                SubtaskPlan { id: "b".into(), title: "B".into(), ..Default::default() },
            ],
            notes: None,
        }, "Added subtask b");
        assert_eq!(ps.version_history.current_version, 2);

        // Rollback should work
        let result = ps.rollback_to_version(1);
        assert!(result.is_ok());
        assert_eq!(ps.plan.subtasks.len(), 1, "should rollback to v1 with 1 subtask");
        assert_eq!(ps.version_history.current_version, 3, "rollback creates new version");
    }

    #[test]
    fn version_history_persists_through_save_load() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

        let ctx = ProjectContext::default();
        let mut ps = PlanModeState::new("test".into(), ctx);
        ps.set_plan(TaskPlan {
            subtasks: vec![SubtaskPlan { id: "a".into(), title: "A".into(), ..Default::default() }],
            notes: None,
        });
        ps.save_to_file(&path).unwrap();

        let loaded = PlanModeState::load_from_file(&path).unwrap();
        assert_eq!(loaded.version_history.current_version, 1);
        assert_eq!(loaded.version_history.versions.len(), 1);
    }

    // ═══════════════════════ Execution Config & Preview Tests ════════════════

    #[test]
    fn execution_preview_format_basic() {
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan { id: "a".into(), title: "A".into(),
                    effort: Some("small".into()), ..Default::default() },
                SubtaskPlan { id: "b".into(), title: "B".into(),
                    effort: Some("medium".into()), ..Default::default() },
            ],
            notes: None,
        };
        let preview = format_execution_preview(&plan);
        assert!(preview.contains("Execution Preview"));
        assert!(preview.contains("2 subtasks"));
        assert!(preview.contains("2 ready now"));
        assert!(preview.contains("Estimated effort"));
    }

    #[test]
    fn execution_preview_shows_parallel_groups() {
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan { id: "a".into(), title: "A".into(),
                    files: vec!["a.rs".into()], ..Default::default() },
                SubtaskPlan { id: "b".into(), title: "B".into(),
                    files: vec!["b.rs".into()], ..Default::default() },
                SubtaskPlan { id: "c".into(), title: "C".into(),
                    depends_on: vec!["a".into()], ..Default::default() },
            ],
            notes: None,
        };
        let preview = format_execution_preview(&plan);
        assert!(preview.contains("Parallel Groups"));
        assert!(preview.contains("Round"));
    }

    #[test]
    fn execution_preview_shows_file_conflicts() {
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan { id: "a".into(), title: "A".into(),
                    files: vec!["shared.rs".into()], ..Default::default() },
                SubtaskPlan { id: "b".into(), title: "B".into(),
                    files: vec!["shared.rs".into()], ..Default::default() },
            ],
            notes: None,
        };
        let preview = format_execution_preview(&plan);
        assert!(preview.contains("file conflict"));
        assert!(preview.contains("shared.rs"));
    }

    #[test]
    fn execution_summary_complete_plan() {
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan { id: "a".into(), title: "A".into(),
                    status: TaskStatus::Completed, ..Default::default() },
                SubtaskPlan { id: "b".into(), title: "B".into(),
                    status: TaskStatus::Completed, ..Default::default() },
            ],
            notes: None,
        };
        let summary = PlanExecutionSummary::from_plan(&plan, "Test goal", 2);
        assert_eq!(summary.completed, 2);
        assert_eq!(summary.total_subtasks, 2);
        assert_eq!(summary.parallel_rounds, 2);
        let formatted = summary.format();
        assert!(formatted.contains("Execution Summary"));
        assert!(formatted.contains("Test goal"));
        assert!(formatted.contains("Complete"));
        assert!(formatted.contains("2/2"));
    }

    #[test]
    fn execution_summary_partial_with_failures() {
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan { id: "a".into(), title: "A".into(),
                    status: TaskStatus::Completed, ..Default::default() },
                SubtaskPlan { id: "b".into(), title: "B".into(),
                    status: TaskStatus::Failed, ..Default::default() },
                SubtaskPlan { id: "c".into(), title: "C".into(),
                    status: TaskStatus::Pending, ..Default::default() },
            ],
            notes: None,
        };
        let summary = PlanExecutionSummary::from_plan(&plan, "Failing goal", 1);
        assert_eq!(summary.completed, 1);
        assert_eq!(summary.failed, 1);
        let formatted = summary.format();
        assert!(formatted.contains("Partial (failures)"));
        assert!(formatted.contains("Failed:"));
    }

    #[test]
    fn execution_summary_paused() {
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan { id: "a".into(), title: "A".into(),
                    status: TaskStatus::Completed, ..Default::default() },
                SubtaskPlan { id: "b".into(), title: "B".into(),
                    status: TaskStatus::InProgress, ..Default::default() },
            ],
            notes: None,
        };
        let summary = PlanExecutionSummary::from_plan(&plan, "Paused goal", 1);
        assert_eq!(summary.paused, 1);
        assert!(summary.format().contains("Paused"));
    }

    #[test]
    fn parse_execution_confirmation_variants() {
        assert_eq!(parse_execution_confirmation("y"), ExecutionConfirmation::Execute);
        assert_eq!(parse_execution_confirmation("yes"), ExecutionConfirmation::Execute);
        assert_eq!(parse_execution_confirmation("go"), ExecutionConfirmation::Execute);
        assert_eq!(parse_execution_confirmation("确认"), ExecutionConfirmation::Execute);
        assert_eq!(parse_execution_confirmation("s"), ExecutionConfirmation::StepByStep);
        assert_eq!(parse_execution_confirmation("step"), ExecutionConfirmation::StepByStep);
        assert_eq!(parse_execution_confirmation("e"), ExecutionConfirmation::Edit);
        assert_eq!(parse_execution_confirmation("edit"), ExecutionConfirmation::Edit);
        assert_eq!(parse_execution_confirmation("n"), ExecutionConfirmation::Cancel);
        assert_eq!(parse_execution_confirmation("no"), ExecutionConfirmation::Cancel);
        assert_eq!(parse_execution_confirmation(""), ExecutionConfirmation::Cancel);
    }

    #[test]
    fn parse_subtask_confirmation_variants() {
        assert_eq!(parse_subtask_confirmation("y"), SubtaskConfirmation::Execute);
        assert_eq!(parse_subtask_confirmation("yes"), SubtaskConfirmation::Execute);
        assert_eq!(parse_subtask_confirmation(""), SubtaskConfirmation::Execute); // default = yes
        assert_eq!(parse_subtask_confirmation("s"), SubtaskConfirmation::Skip);
        assert_eq!(parse_subtask_confirmation("skip"), SubtaskConfirmation::Skip);
        assert_eq!(parse_subtask_confirmation("q"), SubtaskConfirmation::Quit);
        assert_eq!(parse_subtask_confirmation("quit"), SubtaskConfirmation::Quit);
    }

    #[test]
    fn format_subtask_confirmation_shows_details() {
        let st = SubtaskPlan {
            id: "add-tests".into(),
            title: "Add unit tests".into(),
            description: Some("Write tests for the parser module".into()),
            files: vec!["src/parser.rs".into(), "tests/parser_test.rs".into()],
            ..Default::default()
        };
        let formatted = format_subtask_confirmation(&st, 2, 5);
        assert!(formatted.contains("Subtask 3/5")); // 0-indexed → display as 3
        assert!(formatted.contains("add-tests"));
        assert!(formatted.contains("Add unit tests"));
        assert!(formatted.contains("parser module"));
        assert!(formatted.contains("parser.rs"));
        assert!(formatted.contains("Execute?"));
    }

    #[test]
    fn plan_execution_config_defaults() {
        let config = PlanExecutionConfig::default();
        assert!(!config.step_by_step);
        assert!(!config.auto_execute);
    }

    #[test]
    fn execution_summary_order_tracks_completed() {
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan { id: "x".into(), title: "X".into(),
                    status: TaskStatus::Completed, ..Default::default() },
                SubtaskPlan { id: "y".into(), title: "Y".into(),
                    status: TaskStatus::Pending, ..Default::default() },
                SubtaskPlan { id: "z".into(), title: "Z".into(),
                    status: TaskStatus::Completed, ..Default::default() },
            ],
            notes: None,
        };
        let summary = PlanExecutionSummary::from_plan(&plan, "test", 0);
        assert_eq!(summary.execution_order, vec!["x", "z"]);
        assert!(summary.format().contains("x → z"));
    }

    #[test]
    fn execution_preview_effort_levels() {
        // All large = High effort
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan { id: "a".into(), title: "A".into(),
                    effort: Some("large".into()), ..Default::default() },
                SubtaskPlan { id: "b".into(), title: "B".into(),
                    effort: Some("large".into()), ..Default::default() },
                SubtaskPlan { id: "c".into(), title: "C".into(),
                    effort: Some("large".into()), ..Default::default() },
            ],
            notes: None,
        };
        let preview = format_execution_preview(&plan);
        assert!(preview.contains("High"));

        // All small = Low effort
        let plan2 = TaskPlan {
            subtasks: vec![
                SubtaskPlan { id: "a".into(), title: "A".into(),
                    effort: Some("small".into()), ..Default::default() },
            ],
            notes: None,
        };
        let preview2 = format_execution_preview(&plan2);
        assert!(preview2.contains("Low"));
    }
}

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

/// Prompt template for plan decomposition.
pub fn decomposition_prompt(goal: &str, context: &ProjectContext) -> String {
    format!(
        r#"You are a senior software architect analyzing a project and creating an execution plan.

## Project Context
- Root: {root}
- Entry points: {entry_points}
- Languages: {languages}
- Structure: {structure}
- Source files: ~{count}

## User Goal
{goal}

## Instructions
Break down this goal into 3-8 concrete subtasks. For each subtask:
1. Give it a short ID (e.g., "setup", "impl-api", "tests")
2. Provide a clear title
3. List any dependencies (subtask IDs that must complete first)
4. Keep scope manageable (each subtask should be completable in one focused session)

Return a JSON object with this exact structure:
```json
{{
  "subtasks": [
    {{
      "id": "unique-id",
      "title": "Short title",
      "description": "What needs to be done",
      "depends_on": ["id-of-dependency"]
    }}
  ],
  "notes": "Optional high-level approach notes"
}}
```

Only return the JSON, no other text."#,
        root = context.root,
        entry_points = if context.entry_points.is_empty() {
            "(none detected)".to_string()
        } else {
            context.entry_points.join(", ")
        },
        languages = if context.languages.is_empty() {
            "(unknown)".to_string()
        } else {
            context.languages.join(", ")
        },
        structure = context.structure_summary,
        count = context.source_file_count,
        goal = goal
    )
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
            _ if ready_ids.contains(st.id.as_str()) => "○", // ready to execute
            _ => "·", // blocked
        };

        out.push_str(&format!("│ {} {} {}\n", status_icon, st.id, st.title));

        if let Some(ref desc) = st.description {
            out.push_str(&format!("│     └─ {}\n", desc));
        }

        if !st.depends_on.is_empty() {
            out.push_str(&format!("│     deps: {}\n", st.depends_on.join(", ")));
        }

        if i < plan.subtasks.len() - 1 {
            out.push_str("│\n");
        }
    }

    out.push_str("└─────────────────────────────────────────────────\n");
    out.push_str(&format!(
        "  Progress: {}% ({}/{})\n",
        plan.progress_pct(),
        plan.items_done(),
        plan.subtasks.len()
    ));

    if !ready.is_empty() {
        out.push_str(&format!(
            "  Ready to execute: {}\n",
            ready.iter().map(|st| st.id.as_str()).collect::<Vec<_>>().join(", ")
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
        }
    }
    
    /// Set the plan (after LLM generation)
    pub fn set_plan(&mut self, plan: TaskPlan) {
        self.plan = plan;
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
        self.history.push((user_msg.to_string(), assistant_msg.to_string()));
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
        
        prompt.push_str(r#"## Instructions
Based on the user's request, respond in ONE of these ways:

1. **If modifying the plan**: Output the updated plan as JSON with format:
```json
{
  "subtasks": [{"id": "...", "title": "...", "description": "...", "depends_on": [...]}],
  "notes": "..."
}
```

2. **If answering a question**: Respond naturally, no JSON needed.

Keep responses concise. The plan JSON must be valid if provided."#);
        
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
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| format!("serialize plan state: {e}"))?;
        std::fs::write(path, json).map_err(|e| format!("write plan state: {e}"))?;
        Ok(())
    }

    /// Load plan mode state from a file.
    pub fn load_from_file(path: &Path) -> Result<Self, String> {
        let data = std::fs::read_to_string(path)
            .map_err(|e| format!("read plan state: {e}"))?;
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
                },
                SubtaskPlan {
                    id: "pending".to_string(),
                    title: "Pending task".to_string(),
                    description: Some("Needs work".to_string()),
                    depends_on: vec!["done".to_string()],
                    status: TaskStatus::Pending,
                },
            ],
            notes: Some("Test plan".to_string()),
        };

        let formatted = format_plan(&plan);
        assert!(formatted.contains("✓"), "should show completed: {formatted}");
        assert!(formatted.contains("50%"), "should show 50%: {formatted}");
        assert!(formatted.contains("pending"), "should be ready: {formatted}");
    }

    #[test]
    fn decomposition_prompt_includes_context() {
        let ctx = ProjectContext {
            root: "/test".to_string(),
            entry_points: vec!["Cargo.toml".to_string()],
            languages: vec!["Rust".to_string()],
            structure_summary: "src, tests".to_string(),
            source_file_count: 42,
        };

        let prompt = decomposition_prompt("Add logging", &ctx);
        assert!(prompt.contains("Rust"), "should include language: {prompt}");
        assert!(
            prompt.contains("Cargo.toml"),
            "should include entry point: {prompt}"
        );
        assert!(prompt.contains("Add logging"), "should include goal: {prompt}");
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
                },
                SubtaskPlan {
                    id: "b".to_string(),
                    title: "Second".to_string(),
                    description: None,
                    depends_on: vec!["a".to_string()],
                    status: TaskStatus::Pending,
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
                },
                SubtaskPlan {
                    id: "write-code".into(),
                    title: "Write code".into(),
                    description: None,
                    depends_on: vec!["setup-deps".into()],
                    status: TaskStatus::Pending,
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
                },
                SubtaskPlan {
                    id: "test-integration".into(),
                    title: "Integration tests".into(),
                    description: None,
                    depends_on: vec![],
                    status: TaskStatus::Pending,
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
            }],
            notes: None,
        });

        let prompt = ps.plan_mode_prompt("make it simpler");
        assert!(prompt.contains("Add auth"), "should contain goal");
        assert!(prompt.contains("jwt"), "should contain plan subtask");
        assert!(prompt.contains("make it simpler"), "should contain user request");
        assert!(prompt.contains("PLAN MODE"), "should contain mode indicator");
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
                },
                SubtaskPlan {
                    id: "b".into(),
                    title: "B".into(),
                    description: None,
                    depends_on: vec![],
                    status: TaskStatus::Pending,
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
}

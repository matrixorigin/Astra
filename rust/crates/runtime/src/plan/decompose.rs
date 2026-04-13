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
use std::time::Instant;

// Re-export task types from services
pub use astra_services::task_orchestrator::{SubtaskPlan, TaskPlan, TaskStatus};

/// First `messages[]` row (`role: system`) when the CLI enables **plan-only chat** (`/plan on`).
/// Edge tools are omitted from the payload; the model should reason and answer with a plan only.
pub const CHAT_PLAN_ONLY_SYSTEM: &str = "You are in **plan-only** mode.\n\n\
Rules:\n\
- Produce a clear, actionable plan: ordered steps, dependencies, risks, and verification.\n\
- Do **not** assume any tools or shell commands will run. Do not offer to read files, search the repo, or run builds unless the user leaves plan-only mode.\n\
- If critical information is missing, ask concise questions before the plan.\n\
- Prefer sections: Goal, Assumptions, Steps, Verification.\n";

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
    /// Number of test files detected.
    #[serde(default)]
    pub test_file_count: usize,
    /// Git status: number of modified/untracked files.
    #[serde(default)]
    pub git_dirty_count: usize,
    /// Whether there are uncommitted changes.
    #[serde(default)]
    pub has_uncommitted_changes: bool,
    /// Key directories detected (src/, tests/, lib/, etc.)
    #[serde(default)]
    pub key_directories: Vec<String>,
    /// Successful plan templates from prior tasks (injected into LLM prompt).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prior_templates: Vec<PlanTemplateHint>,
}

/// A lightweight hint from a previously successful plan template.
///
/// Injected into the decomposition prompt so the LLM can leverage learned patterns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanTemplateHint {
    /// Original goal pattern that was successfully completed.
    pub goal_pattern: String,
    /// Subtask titles from the successful plan.
    pub subtask_titles: Vec<String>,
    /// Success rate (0.0–1.0).
    pub success_rate: f64,
    /// Number of times this template was used.
    pub use_count: u32,
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

    // Detect git branch and status (supports worktrees)
    let git_dir = resolve_git_dir(root);
    if let Some(ref gd) = git_dir {
        let head_file = gd.join("HEAD");
        if let Ok(head) = std::fs::read_to_string(&head_file)
            && let Some(branch) = head.trim().strip_prefix("ref: refs/heads/")
        {
            ctx.git_branch = Some(branch.to_string());
        }

        // Simple heuristic: check if index file exists
        let index_file = gd.join("index");
        ctx.has_uncommitted_changes = index_file.exists();

        // Count dirty files by checking worktree against index (simplified)
        ctx.git_dirty_count = count_dirty_files(root);
    }

    // Count test files
    ctx.test_file_count = count_test_files(root);

    // Build structure summary and key directories
    let mut dirs = Vec::new();
    let key_dir_names = [
        "src", "lib", "tests", "test", "spec", "examples", "docs", "benches",
    ];
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let name = entry.file_name().to_string_lossy().to_string();
                if !name.starts_with('.') {
                    dirs.push(name.clone());
                    if key_dir_names.contains(&name.as_str()) {
                        ctx.key_directories.push(name);
                    }
                }
            }
        }
    }
    dirs.sort();
    ctx.key_directories.sort();
    ctx.structure_summary = if dirs.is_empty() {
        "(flat project)".to_string()
    } else {
        format!("Top-level dirs: {}", dirs.join(", "))
    };

    ctx
}

/// Count test files in a project (simplified heuristic).
fn count_test_files(root: &Path) -> usize {
    let mut count = 0;

    // Common test directories
    let test_dirs = ["tests", "test", "spec", "__tests__"];
    for dir in &test_dirs {
        let test_path = root.join(dir);
        if test_path.is_dir() {
            count += count_files_in_dir(&test_path, 3);
        }
    }

    // Also count files matching test patterns in src
    let src_path = root.join("src");
    if src_path.is_dir() {
        count += count_test_files_in_src(&src_path, 4);
    }

    count
}

/// Count files in a directory up to max_depth.
fn count_files_in_dir(dir: &Path, max_depth: usize) -> usize {
    fn inner(dir: &Path, depth: usize, max_depth: usize) -> usize {
        if depth > max_depth {
            return 0;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };

        let mut count = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                count += 1;
            } else if path.is_dir() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if !name_str.starts_with('.')
                    && !matches!(
                        name_str.as_ref(),
                        "node_modules" | "target" | "venv" | "__pycache__"
                    )
                {
                    count += inner(&path, depth + 1, max_depth);
                }
            }
        }
        count
    }
    inner(dir, 0, max_depth)
}

/// Count test files in src directory (files matching *_test.rs, test_*.py, etc.)
fn count_test_files_in_src(dir: &Path, max_depth: usize) -> usize {
    fn inner(dir: &Path, depth: usize, max_depth: usize) -> usize {
        if depth > max_depth {
            return 0;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };

        let mut count = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                let name = entry.file_name().to_string_lossy().to_lowercase();
                // Common test file patterns
                if name.contains("_test.")
                    || name.contains(".test.")
                    || name.contains("_spec.")
                    || name.contains(".spec.")
                    || name.starts_with("test_")
                {
                    count += 1;
                }
            } else if path.is_dir() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if !name_str.starts_with('.') {
                    count += inner(&path, depth + 1, max_depth);
                }
            }
        }
        count
    }
    inner(dir, 0, max_depth)
}

/// Resolve the actual git directory path.
/// Handles both regular repos (.git is a directory) and worktrees (.git is a file pointing to the actual git dir).
/// Returns None if no valid git directory is found.
fn resolve_git_dir(root: &Path) -> Option<std::path::PathBuf> {
    let git_path = root.join(".git");
    if git_path.is_dir() {
        return Some(git_path);
    }
    // Worktree: .git is a file containing "gitdir: /path/to/.git/worktrees/name"
    if git_path.is_file()
        && let Ok(content) = std::fs::read_to_string(&git_path)
        && let Some(gd) = content.trim().strip_prefix("gitdir: ")
    {
        let path = std::path::PathBuf::from(gd);
        if path.exists() && path.join("HEAD").exists() {
            return Some(path);
        }
    }
    None
}

/// Simple heuristic to count dirty files (not as accurate as git status).
fn count_dirty_files(root: &Path) -> usize {
    // This is a very rough approximation
    // In a real implementation, we'd use gix or shell out to git
    // For now, just return 0 as a placeholder
    let _ = root;
    0
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
        } else if path.is_file()
            && let Some(ext) = path.extension().and_then(|e| e.to_str())
            && source_exts.contains(&ext)
            && let Ok(content) = std::fs::read_to_string(&path)
        {
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

    // Prior successful templates — helps the LLM leverage learned patterns
    if !context.prior_templates.is_empty() {
        prompt.push_str("\n## Learned Patterns from Similar Past Tasks\n");
        prompt.push_str("The following plans were successfully completed for similar goals. Use them as reference (adapt, don't copy blindly):\n\n");
        for (i, tmpl) in context.prior_templates.iter().enumerate() {
            prompt.push_str(&format!(
                "### Pattern {} (success rate: {:.0}%, used {} time{})\n",
                i + 1,
                tmpl.success_rate * 100.0,
                tmpl.use_count,
                if tmpl.use_count == 1 { "" } else { "s" }
            ));
            prompt.push_str(&format!("- Goal: {}\n", tmpl.goal_pattern));
            prompt.push_str("- Subtask structure:\n");
            for title in &tmpl.subtask_titles {
                prompt.push_str(&format!("  - {title}\n"));
            }
            prompt.push('\n');
        }
    }

    prompt.push_str(&format!("\n## Goal\n{goal}\n"));

    prompt.push_str(
        "\n## Decomposition phase (read carefully)\n\
You are in **plan-decomposition** mode: you do **not** have shell, git, read_file, or other tools in this request.\n\
**Never refuse** the goal because tools are unavailable. Assume a human or the main agent **will** run commands later.\n\
- For goals like \"review the latest commit\", \"check CI\", or \"what changed\": emit subtasks whose descriptions name the exact commands to run (e.g. `git log -1 --stat`, `git show HEAD`) and what to look for in the output.\n\
- Your reply must be **only** the JSON specified below (or the clarification JSON array). **Do not** end with conversational refusals such as \"I don't have tools\" without including valid JSON.\n\n",
    );

    prompt.push_str(r#"
## Instructions
First, assess if you have enough information to create a precise plan. If the goal is ambiguous or you need clarification, ask 1-3 focused questions INSTEAD of generating a plan.

### If You Need Clarification
Return ONLY this JSON when information is missing:
```json
[
  {
    "question": "What is the scope of the authentication feature?",
    "options": ["JWT-based API auth", "Session-based web auth", "Both"],
    "default": 0,
    "category": "scope"
  }
]
```

Question categories: "scope" (features to include), "approach" (implementation strategy), "behavior" (edge cases), "technical" (specific tech choices), "confirmation" (yes/no).

### If You Have Enough Information
Decompose this goal into 3-8 concrete subtasks. For EACH subtask, provide:

1. **id**: short kebab-case ID (e.g., "add-auth", "fix-parser", "write-tests")
2. **title**: one-line summary
3. **description**: what specifically needs to be done
4. **depends_on**: IDs of subtasks that must finish first (empty array if none)
5. **effort**: estimated scope — "small" (<30 lines changed), "medium" (30-100), or "large" (100+)
6. **files**: list of files likely to be modified (relative paths; best guess from project structure)
7. **acceptance_checks**: a JSON array of structured verification checks. Each element is an object with a `"kind"` field. Available kinds:
   - `{"kind": "file_exists", "paths": ["src/foo.rs"]}` — files must exist
   - `{"kind": "read_file_contains", "path": "src/foo.rs", "contains": ["expected text"], "not_contains": []}` — file content check (safe, no shell)
   - `{"kind": "grep_check", "file": "src/foo.rs", "pattern": "pub fn new_feature", "should_match": true}` — grep pattern in file
   - `{"kind": "command", "cmd": "chmod +x /tmp/script.sh && test -x /tmp/script.sh", "expected_exit": 0}` — shell command exit code
   - `{"kind": "build_pass", "cmd": "cargo build"}` — project builds
   - `{"kind": "test_pass", "cmd": "cargo test --workspace", "min_pass_rate": 1.0}` — tests pass
   Every subtask MUST have at least one check. Prefer `read_file_contains` / `grep_check` over shell commands when verifying file content.

Guidelines:
- Order subtasks so dependencies come first
- Each subtask should be completable in ONE focused session
- **All file paths must be relative to the project root** (the current working directory). Never use `/tmp/`, `tmp/`, or any absolute path. If the goal creates new files, place them in the project root or a sensible subdirectory (e.g. `src/`, `index.html`, `app.js`).
- **Paths in `acceptance_checks` must match `files` exactly** — mismatched paths cause false verification failures.
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
      "acceptance_checks": [
        {"kind": "file_exists", "paths": ["src/foo.rs"]},
        {"kind": "grep_check", "file": "src/foo.rs", "pattern": "pub fn new_feature", "should_match": true}
      ]
    }
  ],
  "notes": "High-level approach and risk considerations"
}
```"#);

    prompt
}

/// Query the `plan_templates` table for templates whose goal matches the given goal.
///
/// Uses keyword-based matching: extracts significant words from the goal and searches
/// for templates containing any of them. Returns up to `limit` results sorted by
/// relevance (success_rate × use_count).
pub async fn query_similar_templates(
    pool: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    goal: &str,
    limit: usize,
) -> Vec<PlanTemplateHint> {
    // Extract keywords from goal (skip short words)
    let keywords: Vec<String> = goal
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
        .filter(|w| w.len() >= 3)
        .map(String::from)
        .collect();

    if keywords.is_empty() {
        return Vec::new();
    }

    // Build LIKE conditions for keyword matching
    let like_clauses: Vec<String> = keywords
        .iter()
        .take(6) // cap at 6 keywords to avoid huge queries
        .map(|kw| format!("goal_pattern LIKE '%{kw}%'"))
        .collect();
    let where_clause = like_clauses.join(" OR ");

    let query = format!(
        "SELECT goal_pattern, template_json, success_rate, use_count \
         FROM plan_templates \
         WHERE user_id = ? AND ({where_clause}) \
         ORDER BY (success_rate * use_count) DESC \
         LIMIT ?",
    );

    let rows = match sqlx::query(&query)
        .bind(user_id)
        .bind(limit as i64)
        .fetch_all(pool)
        .await
    {
        Ok(rows) => rows,
        Err(_) => return Vec::new(),
    };

    let mut hints = Vec::new();
    for row in &rows {
        let goal_pattern: String = match sqlx::Row::try_get(row, "goal_pattern") {
            Ok(v) => v,
            Err(_) => continue,
        };
        let template_json: String = match sqlx::Row::try_get(row, "template_json") {
            Ok(v) => v,
            Err(_) => continue,
        };
        let success_rate: f32 = sqlx::Row::try_get(row, "success_rate").unwrap_or(0.0);
        let use_count: i32 = sqlx::Row::try_get(row, "use_count").unwrap_or(0);

        // Parse subtask_titles from template_json
        let subtask_titles =
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&template_json) {
                json.get("subtask_titles")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default()
            } else {
                Vec::new()
            };

        if subtask_titles.is_empty() {
            continue;
        }

        hints.push(PlanTemplateHint {
            goal_pattern,
            subtask_titles,
            success_rate: success_rate as f64,
            use_count: use_count as u32,
        });
    }

    hints
}
pub fn plan_response_parse_error_preview(
    response: &str,
    max_lines: usize,
    max_chars: usize,
) -> String {
    let t = response.trim();
    if t.is_empty() {
        return String::new();
    }
    let capped: String = t.chars().take(max_chars).collect();
    let excerpt = capped
        .lines()
        .take(max_lines)
        .collect::<Vec<_>>()
        .join("\n");
    let truncated = t.chars().count() > max_chars || t.lines().count() > max_lines;
    if truncated && !excerpt.is_empty() {
        format!("{excerpt}\n…")
    } else {
        excerpt
    }
}

/// Parse LLM response into a TaskPlan.
pub fn parse_plan_response(response: &str) -> Result<TaskPlan, String> {
    // Try to extract JSON from the response (may be wrapped in markdown)
    let json_str = extract_json(response);

    // First, try to parse as generic JSON to provide better error messages
    let parsed_value: serde_json::Value = match serde_json::from_str(&json_str) {
        Ok(v) => v,
        Err(e) => {
            // Check if the response looks like it contains no valid JSON
            if !json_str.contains('{') && !json_str.contains('[') {
                return Err(
                    "No JSON found in response. Model returned plain text instead of JSON plan."
                        .to_string(),
                );
            }
            return Err(format!("Invalid JSON syntax: {e}"));
        }
    };

    // Check if it's an object with the expected structure
    if let Some(obj) = parsed_value.as_object() {
        if !obj.contains_key("subtasks") {
            // Provide helpful message about what's missing
            let keys: Vec<_> = obj.keys().take(5).collect();
            return Err(format!(
                "JSON object missing 'subtasks' field. Found keys: {:?}. \
                 Expected format: {{\"subtasks\": [...], \"notes\": \"...\"}}",
                keys
            ));
        }
        if !obj.get("subtasks").is_some_and(|v| v.is_array()) {
            return Err(
                "'subtasks' must be an array. Expected format: {\"subtasks\": [...]}".to_string(),
            );
        }
    } else if parsed_value.is_array() {
        // This might be a clarification question array - caller should handle this
        return Err(
            "Response is a JSON array (likely clarification questions). Use parse_clarification_response() instead."
                .to_string(),
        );
    } else {
        return Err(format!(
            "Expected JSON object with 'subtasks' field, got: {}",
            match &parsed_value {
                serde_json::Value::Null => "null",
                serde_json::Value::Bool(_) => "boolean",
                serde_json::Value::Number(_) => "number",
                serde_json::Value::String(_) => "string",
                _ => "unknown",
            }
        ));
    }

    // Now parse with the proper type
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
            acceptance_checks: parse_acceptance_checks(st.acceptance_checks),
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
    #[serde(default)]
    acceptance_checks: Vec<serde_json::Value>,
}

/// Try-parse each raw JSON value into a `VerifierKind`, skipping entries that
/// fail (unknown/hallucinated kind) and filtering out shell-execution variants
/// (`Command`, `CommandOutput`) that could be an RCE vector from LLM output.
fn parse_acceptance_checks(
    raw: Vec<serde_json::Value>,
) -> Vec<astra_services::durable_task::VerifierKind> {
    use astra_services::durable_task::VerifierKind;
    raw.into_iter()
        .filter_map(
            |v| match serde_json::from_value::<VerifierKind>(v.clone()) {
                Ok(vk) => {
                    if matches!(
                        vk,
                        VerifierKind::Command { .. } | VerifierKind::CommandOutput { .. }
                    ) {
                        None
                    } else {
                        Some(vk)
                    }
                }
                Err(_e) => None,
            },
        )
        .collect()
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

    // Raw top-level JSON array (e.g. clarification questions) — must run before `{`…`}` slice,
    // otherwise we would clip the first `{` inside the array and break parsing.
    let trim = response.trim();
    if trim.starts_with('[')
        && serde_json::from_str::<serde_json::Value>(trim)
            .ok()
            .is_some_and(|v| v.is_array())
    {
        return trim.to_string();
    }

    // Look for raw JSON object
    if let Some(start) = response.find('{')
        && let Some(end) = response.rfind('}')
    {
        return response[start..=end].to_string();
    }

    response.to_string()
}

/// Format a TaskPlan for display.
pub fn format_plan(plan: &TaskPlan) -> String {
    let mut out = String::new();
    let wrap_width = 70; // max content width inside the plan box

    if let Some(ref notes) = plan.notes {
        for line in wrap_text(notes, wrap_width) {
            out.push_str(&format!("  {}\n", line));
        }
        out.push('\n');
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

        // Step header: icon + id + effort + title
        let header = format!("{}{} {}", st.id, effort_badge, st.title);
        let header_lines = wrap_text(&header, wrap_width - 4);
        for (j, line) in header_lines.iter().enumerate() {
            if j == 0 {
                out.push_str(&format!("  {} {}\n", status_icon, line));
            } else {
                out.push_str(&format!("    {}\n", line));
            }
        }

        // Description — wrapped and indented
        if let Some(ref desc) = st.description {
            for line in wrap_text(desc, wrap_width - 6) {
                out.push_str(&format!("      {}\n", line));
            }
        }

        // Files
        if !st.files.is_empty() {
            let files_str = st.files.join(", ");
            for line in wrap_text(&format!("📁 {}", files_str), wrap_width - 6) {
                out.push_str(&format!("      {}\n", line));
            }
        }

        // Acceptance checks
        if !st.acceptance_checks.is_empty() {
            let count = st.acceptance_checks.len();
            let line = format!(
                "✅ {count} verification check{}",
                if count == 1 { "" } else { "s" }
            );
            for l in wrap_text(&line, wrap_width - 6) {
                out.push_str(&format!("      {}\n", l));
            }
        }

        // Dependencies
        if !st.depends_on.is_empty() {
            out.push_str(&format!("      deps: {}\n", st.depends_on.join(", ")));
        }

        if i < plan.subtasks.len() - 1 {
            out.push('\n');
        }
    }

    out.push_str("  ─────────────────────────────────────────\n");

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

/// Format a TaskPlan as markdown suitable for terminal rendering via `StreamingMarkdown`.
pub fn format_plan_markdown(plan: &TaskPlan, goal: Option<&str>) -> String {
    let mut out = String::new();

    if let Some(g) = goal {
        out.push_str(&format!("**Plan:** {g}\n\n"));
    }

    if let Some(ref notes) = plan.notes {
        out.push_str(&format!("{notes}\n\n"));
    }

    for (i, st) in plan.subtasks.iter().enumerate() {
        let status_icon = match st.status {
            TaskStatus::Completed => "✓",
            TaskStatus::InProgress => "▶",
            TaskStatus::Failed => "✗",
            TaskStatus::Paused => "⏸",
            _ => "○",
        };

        let effort_badge = match st.effort.as_deref() {
            Some("small") => " `S`",
            Some("medium") => " `M`",
            Some("large") => " `L`",
            _ => "",
        };

        out.push_str(&format!(
            "{}. {} **{}**{} — {}\n",
            i + 1,
            status_icon,
            st.id,
            effort_badge,
            st.title,
        ));

        if let Some(ref desc) = st.description {
            out.push_str(&format!("   {desc}\n"));
        }

        if !st.files.is_empty() {
            let files: Vec<_> = st.files.iter().map(|f| format!("`{f}`")).collect();
            out.push_str(&format!("   Files: {}\n", files.join(", ")));
        }

        if !st.acceptance_checks.is_empty() {
            let checks: Vec<_> = st
                .acceptance_checks
                .iter()
                .map(|vk| match vk {
                    astra_services::durable_task::VerifierKind::FileExists { paths } => {
                        format!("`file_exists: {}`", paths.join(", "))
                    }
                    astra_services::durable_task::VerifierKind::ReadFileContains {
                        path, ..
                    } => format!("`read_file: {path}`"),
                    astra_services::durable_task::VerifierKind::GrepCheck {
                        file, pattern, ..
                    } => format!("`grep '{pattern}' {file}`"),
                    astra_services::durable_task::VerifierKind::Command { cmd, .. } => {
                        format!("`{cmd}`")
                    }
                    astra_services::durable_task::VerifierKind::BuildPass { cmd } => {
                        format!("`build: {cmd}`")
                    }
                    astra_services::durable_task::VerifierKind::TestPass { cmd, .. } => {
                        format!("`test: {cmd}`")
                    }
                    _ => "`check`".into(),
                })
                .collect();
            out.push_str(&format!("   Verify: {}\n", checks.join(", ")));
        }

        if !st.depends_on.is_empty() {
            out.push_str(&format!(
                "   _(depends on: {})_\n",
                st.depends_on.join(", ")
            ));
        }

        out.push('\n');
    }

    out.push_str("---\n");

    let mut summary_parts = Vec::new();
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
        let mut effort_parts = Vec::new();
        if small > 0 {
            effort_parts.push(format!("{small} small"));
        }
        if medium > 0 {
            effort_parts.push(format!("{medium} medium"));
        }
        if large > 0 {
            effort_parts.push(format!("{large} large"));
        }
        summary_parts.push(effort_parts.join(", "));
    }
    summary_parts.push(format!(
        "{}% ({}/{})",
        plan.progress_pct(),
        plan.items_done(),
        plan.subtasks.len(),
    ));
    out.push_str(&summary_parts.join(" | "));
    out.push('\n');

    out
}

/// Wrap text to fit within a given width, breaking at word boundaries.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        if paragraph.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut current = String::new();
        for word in paragraph.split_whitespace() {
            if current.is_empty() {
                current = word.to_string();
            } else if current.chars().count() + 1 + word.chars().count() <= width {
                current.push(' ');
                current.push_str(word);
            } else {
                lines.push(current);
                current = word.to_string();
            }
        }
        if !current.is_empty() {
            lines.push(current);
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

// ─── Plan Mode State ─────────────────────────────────────────────────────────

/// Typed errors for plan persistence operations.
#[derive(Debug, Clone)]
pub enum PlanLoadError {
    /// Plan ID contains illegal characters (path traversal, etc.)
    InvalidId(String),
    /// Plan file does not exist on disk.
    NotFound(String),
    /// Plan file exists but is corrupted or unreadable.
    Corrupt(String),
    /// I/O or other unexpected error.
    Internal(String),
}

impl std::fmt::Display for PlanLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidId(msg) => write!(f, "invalid plan ID: {msg}"),
            Self::NotFound(msg) => write!(f, "plan not found: {msg}"),
            Self::Corrupt(msg) => write!(f, "plan corrupted: {msg}"),
            Self::Internal(msg) => write!(f, "plan error: {msg}"),
        }
    }
}

fn default_version() -> u64 {
    1
}

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
    /// Pending clarification questions from LLM
    #[serde(default)]
    pub pending_clarifications: Option<PendingClarifications>,
    /// Execution timeline tracking all events
    #[serde(default)]
    pub timeline: ExecutionTimeline,
    /// Monotonic version counter for optimistic concurrency control.
    /// Incremented on every save; checked on update to detect lost writes.
    #[serde(default = "default_version")]
    pub version: u64,
    /// User who created this plan (for ownership filtering).
    #[serde(default)]
    pub created_by: Option<String>,
    /// Wall-clock origin for CLI "Assembling plan · Ns" (plan> session; not serialized).
    #[serde(skip)]
    pub assemble_wall_start: Option<Instant>,
    /// Legacy field — kept for deserialization compat with older plan state files.
    #[serde(default, skip_serializing)]
    #[allow(dead_code)]
    _background_execution: bool,
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
            pending_clarifications: None,
            timeline: ExecutionTimeline::default(),
            version: 1,
            created_by: None,
            assemble_wall_start: Some(Instant::now()),
            _background_execution: false,
        }
    }

    /// Create a new plan with an owner user ID.
    pub fn new_with_owner(goal: String, context: ProjectContext, user_id: String) -> Self {
        let mut state = Self::new(goal, context);
        state.created_by = Some(user_id);
        state
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
        let v = self
            .version_history
            .get_version(version)
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
            prompt.push('\n');
        }

        prompt.push_str(&format!("## User Request\n{}\n\n", user_message));

        // Detect completed plan — tailor instructions for follow-up edits
        let all_done = !self.plan.subtasks.is_empty()
            && self
                .plan
                .subtasks
                .iter()
                .all(|s| s.status == TaskStatus::Completed);

        if all_done {
            prompt.push_str(
                r#"## Instructions
The plan above is ALREADY COMPLETED. The user is requesting a follow-up change.

Assess the scope of the request:
- **Small tweak** (move files, rename, minor edit): Add ONLY the new subtask(s) needed. Keep all existing completed subtasks as-is with their current status.
- **Significant rework** (restructure, rewrite): You may replace subtasks, but preserve completed ones that are still valid.

Output the FULL plan as JSON (including existing completed subtasks unchanged) with the new subtask(s) appended:
```json
{
  "subtasks": [
    {"id": "existing-1", "title": "...", "status": "completed", ...},
    {"id": "followup-1", "title": "...", "description": "...", "depends_on": [...]}
  ],
  "notes": "..."
}
```

Rules:
- New subtasks get status "pending" (or omit status field).
- Do NOT reset completed subtasks to pending unless the user explicitly asks to redo them.
- Keep the JSON valid and concise."#,
            );
        } else {
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
        }

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
}

// ─── Auto Plan Detection ─────────────────────────────────────────────────────

/// Heuristics to detect if user input likely needs a plan.
/// Returns Some(reason) if plan mode should be suggested.
pub fn should_suggest_plan_mode(input: &str) -> Option<&'static str> {
    let lower = input.to_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();

    // Skip if input is very short (likely a question or simple command)
    // For Chinese text, count characters instead of words
    let is_short = if input
        .chars()
        .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
    {
        // Chinese: count non-whitespace characters
        input.chars().filter(|c| !c.is_whitespace()).count() < 6
    } else {
        // English: count words
        words.len() < 4
    };

    if is_short {
        return None;
    }

    // Multi-step indicators
    let has_multi_step = lower.contains(" and ")
        || lower.contains(" then ")
        || lower.contains("，然后")
        || lower.contains("然后")
        || lower.contains("并且")
        || lower.contains("同时");

    // Large scope indicators
    let has_large_scope = lower.contains("refactor")
        || lower.contains("重构")
        || lower.contains("migrate")
        || lower.contains("迁移")
        || lower.contains("implement")
        || lower.contains("实现")
        || lower.contains("build")
        || lower.contains("构建")
        || lower.contains("create")
        || lower.contains("创建");

    // Feature-level work
    let has_feature_keywords = lower.contains("feature")
        || lower.contains("功能")
        || lower.contains("module")
        || lower.contains("模块")
        || lower.contains("system")
        || lower.contains("系统")
        || lower.contains("service")  // singular
        || lower.contains("services") // plural
        || lower.contains("服务");

    // Test + implementation pattern
    let mentions_tests = lower.contains("test") || lower.contains("测试");
    let mentions_impl = lower.contains("implement")
        || lower.contains("add")
        || lower.contains("create")
        || lower.contains("添加");

    // Complexity indicators
    let mentions_multiple = lower.contains("multiple")
        || lower.contains("several")
        || lower.contains("all ")  // "all files", "all services"
        || lower.contains("多个")
        || lower.contains("一些")
        || lower.contains("files")
        || lower.contains("文件");

    // Decision logic
    if has_multi_step && (has_large_scope || has_feature_keywords) {
        return Some("Multi-step task with significant scope detected");
    }

    if has_multi_step && mentions_tests {
        return Some("Multi-step task with testing detected");
    }

    if mentions_tests && mentions_impl {
        return Some("Implementation + testing workflow detected");
    }

    if has_large_scope && mentions_multiple {
        return Some("Large-scale change affecting multiple files");
    }

    if has_large_scope && has_feature_keywords {
        return Some("Feature-level change detected");
    }

    // Long input with action verbs often indicates complex task
    let is_long = if input
        .chars()
        .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
    {
        input.chars().filter(|c| !c.is_whitespace()).count() >= 20
    } else {
        words.len() >= 15
    };

    if is_long && (has_large_scope || has_feature_keywords) {
        return Some("Complex task description detected");
    }

    None
}

impl PlanModeState {
    /// Memory protocol content for storing the active plan
    pub fn to_memory_content(&self) -> String {
        format!(
            "[@plan/active] Goal: {}\n\n{}",
            self.goal,
            serde_json::to_string_pretty(&self.plan).unwrap_or_default()
        )
    }

    /// Memory protocol content for a completed plan
    pub fn to_completed_memory(&self) -> String {
        format!(
            "[@plan/completed] Goal: {}\nStatus: {} subtasks\n\n{}",
            self.goal,
            self.plan.subtasks.len(),
            serde_json::to_string_pretty(&self.plan).unwrap_or_default()
        )
    }

    /// Save plan mode state to a file for session recovery.
    ///
    /// Uses atomic write (write to temp file, then rename) to prevent data loss
    /// from partial writes or crashes mid-save. A CRC32 checksum is embedded in
    /// the JSON for corruption detection on load.
    pub fn save_to_file(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create plan directory: {e}"))?;
        }
        // Serialize via Value so that save and load produce identical JSON bytes.
        // Direct struct serialization preserves field declaration order, but
        // loading re-serializes from Value (which uses sorted keys), causing
        // a checksum mismatch. Going through Value on both paths avoids this.
        let data_value =
            serde_json::to_value(self).map_err(|e| format!("serialize plan state: {e}"))?;
        let data_json = data_value.to_string();

        let checksum = crc32_hash(data_json.as_bytes());
        let wrapper = format!("{{\"_checksum\":\"{checksum:08x}\",\"data\":{data_json}}}");

        let tmp_path = path.with_extension("json.tmp");
        std::fs::write(&tmp_path, &wrapper).map_err(|e| format!("write temp plan state: {e}"))?;
        std::fs::rename(&tmp_path, path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp_path);
            format!("rename plan state: {e}")
        })?;
        Ok(())
    }

    /// Load plan mode state from a file.
    ///
    /// Supports both checksummed format (produced by atomic save) and legacy
    /// plain JSON. On checksum mismatch, returns `PlanLoadError::Corrupt`.
    pub fn load_from_file(path: &Path) -> Result<Self, PlanLoadError> {
        let raw = std::fs::read_to_string(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                PlanLoadError::NotFound(path.display().to_string())
            } else {
                PlanLoadError::Internal(format!("read plan state: {e}"))
            }
        })?;

        if let Ok(wrapper) = serde_json::from_str::<serde_json::Value>(&raw) {
            if let (Some(checksum_str), Some(inner)) = (
                wrapper.get("_checksum").and_then(|v| v.as_str()),
                wrapper.get("data"),
            ) {
                let inner_json = inner.to_string();
                let expected = u32::from_str_radix(checksum_str, 16).unwrap_or(0);
                let actual = crc32_hash(inner_json.as_bytes());

                if expected != actual {
                    return Err(PlanLoadError::Corrupt(format!(
                        "checksum mismatch ({expected:08x} vs {actual:08x})"
                    )));
                }

                return serde_json::from_value(inner.clone())
                    .map_err(|e| PlanLoadError::Corrupt(format!("parse plan state: {e}")));
            }
        }

        serde_json::from_str(&raw)
            .map_err(|e| PlanLoadError::Corrupt(format!("parse plan state: {e}")))
    }

    /// Default path for plan state persistence.
    pub fn state_path() -> std::path::PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        std::path::PathBuf::from(home)
            .join(".astra")
            .join("plan_state.json")
    }

    /// Load plan state with recovery — falls back to last good version
    /// if primary state file is corrupted.
    pub fn load_with_recovery(path: &Path) -> Result<Self, PlanLoadError> {
        match Self::load_from_file(path) {
            Ok(state) => Ok(state),
            Err(primary_err) => {
                let backup = path.with_extension("json.bak");
                if backup.exists() {
                    eprintln!("  ⚠ Primary state corrupted, loading backup: {primary_err}");
                    if let Ok(state) = Self::load_from_file(&backup) {
                        let _ = state.save_to_file(path);
                        return Ok(state);
                    }
                }

                let plans_dir = Self::plans_dir();
                if plans_dir.exists() {
                    let mut candidates: Vec<_> = Self::list_saved_plans().into_iter().collect();
                    if !candidates.is_empty() {
                        candidates.sort_by(|a, b| b.progress_pct.cmp(&a.progress_pct));
                        let best = &candidates[0];
                        if let Ok(state) = Self::load_from_plans_dir(&best.name) {
                            eprintln!("  ⚠ Recovered plan '{}' from plans directory", best.goal);
                            return Ok(state);
                        }
                    }
                }

                Err(PlanLoadError::Corrupt(format!(
                    "no recovery possible: {primary_err}"
                )))
            }
        }
    }

    /// Save with backup — keeps one backup copy for recovery.
    pub fn save_with_backup(&self, path: &Path) -> Result<(), String> {
        if path.exists() {
            let backup = path.with_extension("json.bak");
            let _ = std::fs::copy(path, &backup);
        }
        self.save_to_file(path)
    }

    /// Remove the saved state file.
    pub fn clear_saved_state() {
        let path = Self::state_path();
        let _ = std::fs::remove_file(&path);
        let backup = path.with_extension("bak");
        let _ = std::fs::remove_file(backup);
    }

    /// Directory for storing all plans.
    pub fn plans_dir() -> std::path::PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        std::path::PathBuf::from(home).join(".astra").join("plans")
    }

    /// Generate a unique plan ID from the goal.
    pub fn generate_plan_id(goal: &str) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        // Create a slug from the goal (first few words)
        let slug: String = goal
            .split_whitespace()
            .take(3)
            .collect::<Vec<_>>()
            .join("-")
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '-')
            .take(30)
            .collect();

        // Add a short hash for uniqueness
        let mut hasher = DefaultHasher::new();
        goal.hash(&mut hasher);
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        ts.hash(&mut hasher);
        let hash = hasher.finish();

        if slug.is_empty() {
            format!("plan-{:08x}", hash as u32)
        } else {
            format!("{}-{:04x}", slug.to_lowercase(), (hash & 0xFFFF) as u16)
        }
    }

    /// Save this plan to the plans directory with an explicit ID.
    /// Bumps the version counter for optimistic concurrency control.
    pub fn save_to_plans_dir_with_id(&mut self, plan_id: &str) -> Result<(), String> {
        Self::validate_plan_id(plan_id).map_err(|e| e.to_string())?;
        self.version += 1;
        let plans_dir = Self::plans_dir();
        std::fs::create_dir_all(&plans_dir).map_err(|e| format!("create plans dir: {e}"))?;
        let path = plans_dir.join(format!("{plan_id}.json"));
        self.save_to_file(&path)
    }

    /// Save this plan to the plans directory with a generated ID.
    pub fn save_to_plans_dir(&mut self) -> Result<String, String> {
        let plan_id = Self::generate_plan_id(&self.goal);
        self.save_to_plans_dir_with_id(&plan_id)?;
        Ok(plan_id)
    }

    /// Load a plan from the plans directory by ID.
    /// Returns typed `PlanLoadError` for proper HTTP status mapping.
    pub fn load_from_plans_dir(plan_id: &str) -> Result<Self, PlanLoadError> {
        Self::validate_plan_id(plan_id)?;
        let path = Self::plans_dir().join(format!("{plan_id}.json"));
        Self::load_from_file(&path)
    }

    /// List all saved plans in the plans directory.
    pub fn list_saved_plans() -> Vec<SavedPlanInfo> {
        Self::list_saved_plans_filtered(None)
    }

    /// List saved plans owned by the given user. Plans without an owner are included
    /// for backward compatibility with pre-ownership plans.
    pub fn list_saved_plans_for_user(user_id: &str) -> Vec<SavedPlanInfo> {
        Self::list_saved_plans_filtered(Some(user_id))
    }

    fn list_saved_plans_filtered(user_filter: Option<&str>) -> Vec<SavedPlanInfo> {
        let plans_dir = Self::plans_dir();
        let Ok(entries) = std::fs::read_dir(&plans_dir) else {
            return Vec::new();
        };

        let mut plans = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|n| n.to_str()) else {
                continue;
            };

            if let Ok(state) = Self::load_from_file(&path) {
                if let Some(uid) = user_filter {
                    let owner = state.created_by.as_deref().unwrap_or(uid);
                    if owner != uid {
                        continue;
                    }
                }

                let status = if state.plan.progress_pct() == 100 {
                    "completed"
                } else if state.plan.items_done() > 0 {
                    "in_progress"
                } else {
                    "pending"
                };

                plans.push(SavedPlanInfo {
                    name: name.to_string(),
                    goal: state.goal,
                    progress_pct: state.plan.progress_pct(),
                    subtask_count: state.plan.subtasks.len(),
                    status: status.to_string(),
                });
            }
        }

        // Sort by progress (in-progress first, then pending, then completed)
        plans.sort_by(|a, b| {
            let order = |s: &str| match s {
                "in_progress" => 0,
                "pending" => 1,
                "completed" => 2,
                _ => 3,
            };
            order(&a.status)
                .cmp(&order(&b.status))
                .then_with(|| b.progress_pct.cmp(&a.progress_pct))
        });

        plans
    }

    /// Delete a saved plan by ID.
    pub fn delete_saved_plan(plan_id: &str) -> Result<(), PlanLoadError> {
        Self::validate_plan_id(plan_id)?;
        let path = Self::plans_dir().join(format!("{plan_id}.json"));
        std::fs::remove_file(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                PlanLoadError::NotFound(plan_id.to_string())
            } else {
                PlanLoadError::Internal(format!("delete plan: {e}"))
            }
        })
    }

    /// Validate that a plan_id is safe for filesystem use (no path traversal).
    pub fn validate_plan_id(plan_id: &str) -> Result<(), PlanLoadError> {
        if plan_id.is_empty() {
            return Err(PlanLoadError::InvalidId("plan ID must not be empty".into()));
        }
        if !plan_id
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
        {
            return Err(PlanLoadError::InvalidId(format!(
                "'{plan_id}': only alphanumeric, dash, and underscore allowed"
            )));
        }
        Ok(())
    }
}

/// Format the plan mode prompt (for display)
pub fn format_plan_mode_prompt() -> &'static str {
    "plan> "
}

// ─── Plan Mode Entry Card ────────────────────────────────────────────────────

/// Format the plan mode entry card shown when user enters /plan.
///
/// Shows:
/// - Active plan status if any
/// - Smart options based on state
pub fn format_plan_entry_card(
    active_plan: Option<&PlanModeState>,
    paused_plan: Option<&TaskPlan>,
) -> String {
    let mut out = String::new();

    if let Some(ps) = active_plan {
        let pct = ps.plan.progress_pct();
        let goal_display = if ps.goal.is_empty() {
            "(no goal set)".to_string()
        } else {
            truncate_str(&ps.goal, 50)
        };

        out.push_str(&format!(
            "  📋 Plan Mode — {} ({}% done)\n",
            goal_display, pct
        ));
        out.push('\n');

        for st in ps.plan.subtasks.iter().take(4) {
            let icon = match st.status {
                TaskStatus::Completed => "✓",
                TaskStatus::InProgress => "→",
                _ => "○",
            };
            out.push_str(&format!("     {} {}\n", icon, truncate_str(&st.title, 50)));
        }
        if ps.plan.subtasks.len() > 4 {
            out.push_str(&format!("     … and {} more\n", ps.plan.subtasks.len() - 4));
        }

        out.push('\n');
        out.push_str("  [1] continue  [2] restart  [3] new  [4] exit\n");
    } else if let Some(paused) = paused_plan {
        let pct = paused.progress_pct();
        out.push_str(&format!("  📋 Plan Mode — ⏸ paused ({}% done)\n", pct));
        out.push('\n');
        out.push_str("  [1] resume  [2] new  [3] exit\n");
    } else {
        out.push_str("  📋 Plan Mode\n");
        out.push('\n');
        out.push_str("  Describe what you want to do:\n");
    }

    out
}

/// Parse the user's entry choice (number or text).
#[derive(Debug, Clone, PartialEq)]
pub enum PlanEntryChoice {
    Continue,
    Resume,
    Restart,
    New(String),
    Exit,
    Goal(String),
}

/// Parse user input at plan mode entry.
pub fn parse_plan_entry_choice(input: &str, has_active: bool, has_paused: bool) -> PlanEntryChoice {
    let trimmed = input.trim();
    let lower = trimmed.to_lowercase();

    // Check for explicit commands
    if lower == "exit" || lower == "quit" || lower == "4" {
        return PlanEntryChoice::Exit;
    }

    if has_active {
        // [1] continue [2] restart [3] new [4] exit
        match trimmed {
            "1" | "continue" | "继续" => return PlanEntryChoice::Continue,
            "2" | "restart" | "重新开始" => return PlanEntryChoice::Restart,
            "3" | "new" | "新建" => return PlanEntryChoice::New(String::new()),
            _ => {}
        }
    } else if has_paused {
        // [1] resume [2] new [3] exit
        match trimmed {
            "1" | "resume" | "恢复" => return PlanEntryChoice::Resume,
            "2" | "new" | "新建" => return PlanEntryChoice::New(String::new()),
            "3" => return PlanEntryChoice::Exit,
            _ => {}
        }
    }

    // Anything else is a goal description
    PlanEntryChoice::Goal(trimmed.to_string())
}

/// Truncate to at most `max_len` Unicode scalar values (`.chars()`), appending `"..."` if truncated.
fn truncate_str(s: &str, max_len: usize) -> String {
    let n = s.chars().count();
    if n <= max_len {
        s.to_string()
    } else {
        let take_n = max_len.saturating_sub(3).max(1);
        let truncated: String = s.chars().take(take_n).collect();
        format!("{truncated}...")
    }
}

// ─── Clarification Questions ─────────────────────────────────────────────────

/// A clarification question with multiple choice options.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClarificationQuestion {
    /// The question text
    pub question: String,
    /// Available options (1-indexed for user)
    pub options: Vec<String>,
    /// Optional default option (0-indexed)
    pub default: Option<usize>,
    /// Category of clarification (scope, approach, behavior, etc.)
    pub category: ClarificationCategory,
}

/// Category of clarification to help UI formatting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ClarificationCategory {
    /// Scope: what features to include/exclude
    Scope,
    /// Approach: implementation strategy choice
    Approach,
    /// Behavior: how should X behave in edge cases
    Behavior,
    /// Technical: specific technical decisions
    #[default]
    Technical,
    /// Confirmation: yes/no confirmation
    Confirmation,
}

/// Format a clarification question for CLI display.
pub fn format_clarification_question(q: &ClarificationQuestion) -> String {
    let mut out = String::new();

    // Category icon
    let icon = match q.category {
        ClarificationCategory::Scope => "📦",
        ClarificationCategory::Approach => "🛤️ ",
        ClarificationCategory::Behavior => "⚙️ ",
        ClarificationCategory::Technical => "🔧",
        ClarificationCategory::Confirmation => "❓",
    };

    out.push_str(&format!("  {} {}\n", icon, q.question));
    out.push('\n');

    for (i, opt) in q.options.iter().enumerate() {
        let num = i + 1;
        let is_default = q.default == Some(i);
        let marker = if is_default { "→" } else { " " };
        let suffix = if is_default { " (default)" } else { "" };
        out.push_str(&format!("  {} [{}] {}{}\n", marker, num, opt, suffix));
    }

    out.push_str("\n  Enter number or describe alternative: ");
    out
}

/// Parse user's response to a clarification question.
pub fn parse_clarification_response(
    input: &str,
    question: &ClarificationQuestion,
) -> ClarificationAnswer {
    let trimmed = input.trim();

    // Empty input with default
    if trimmed.is_empty() {
        if let Some(default_idx) = question.default {
            return ClarificationAnswer::Selected(default_idx);
        }
        return ClarificationAnswer::Invalid(
            "Please enter a number or describe your choice".to_string(),
        );
    }

    // Try to parse as number
    if let Ok(num) = trimmed.parse::<usize>() {
        if num >= 1 && num <= question.options.len() {
            return ClarificationAnswer::Selected(num - 1);
        }
        return ClarificationAnswer::Invalid(format!("Please enter 1-{}", question.options.len()));
    }

    // Treat as freeform answer
    ClarificationAnswer::Freeform(trimmed.to_string())
}

/// User's answer to a clarification question.
#[derive(Debug, Clone, PartialEq)]
pub enum ClarificationAnswer {
    /// Selected one of the provided options (0-indexed)
    Selected(usize),
    /// Provided a freeform alternative
    Freeform(String),
    /// Invalid input (with error message)
    Invalid(String),
}

/// Pending clarifications during plan generation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PendingClarifications {
    /// Questions waiting for answers
    pub questions: Vec<ClarificationQuestion>,
    /// Answers collected so far (parallel to questions)
    pub answers: Vec<String>,
}

impl PendingClarifications {
    /// Get the next unanswered question, if any.
    pub fn next_question(&self) -> Option<&ClarificationQuestion> {
        self.questions.get(self.answers.len())
    }

    /// Record an answer and return true if all questions answered.
    pub fn record_answer(&mut self, answer: String) -> bool {
        self.answers.push(answer);
        self.answers.len() >= self.questions.len()
    }

    /// Check if all questions have been answered.
    pub fn is_complete(&self) -> bool {
        self.answers.len() >= self.questions.len()
    }

    /// Format all Q&A pairs for inclusion in a prompt.
    pub fn format_for_prompt(&self) -> String {
        let mut out = String::new();
        for (q, a) in self.questions.iter().zip(self.answers.iter()) {
            out.push_str(&format!("Q: {}\nA: {}\n\n", q.question, a));
        }
        out
    }
}

/// Detect if LLM response contains clarification questions.
/// Returns parsed questions if found, None otherwise.
pub fn detect_clarification_questions(llm_text: &str) -> Option<Vec<ClarificationQuestion>> {
    // JSON array in ```json``` / ``` fences, or raw array — same extraction as plan JSON
    let extracted = extract_json(llm_text);
    let t = extracted.trim();
    if t.starts_with('[')
        && let Ok(questions) = serde_json::from_str::<Vec<ClarificationQuestion>>(t)
        && !questions.is_empty()
    {
        return Some(questions);
    }

    // Whole message is only a JSON array (no fence)
    let trim_full = llm_text.trim();
    if trim_full.starts_with('[')
        && trim_full != t
        && let Ok(questions) = serde_json::from_str::<Vec<ClarificationQuestion>>(trim_full)
        && !questions.is_empty()
    {
        return Some(questions);
    }

    // Legacy: compact `[{"question"` without newlines (old detector)
    if let Some(start) = llm_text.find("[{\"question\"")
        && let Some(end) = llm_text[start..].rfind(']')
    {
        let json_str = &llm_text[start..start + end + 1];
        if let Ok(questions) = serde_json::from_str::<Vec<ClarificationQuestion>>(json_str)
            && !questions.is_empty()
        {
            return Some(questions);
        }
    }

    // Try CLARIFICATION: marker format
    if llm_text.contains("CLARIFICATION:") || llm_text.contains("QUESTION:") {
        let mut questions = Vec::new();

        for line in llm_text.lines() {
            let line = line.trim();
            if let Some(q_text) = line
                .strip_prefix("CLARIFICATION:")
                .or_else(|| line.strip_prefix("QUESTION:"))
            {
                // Simple question without options - treat as yes/no
                questions.push(ClarificationQuestion {
                    question: q_text.trim().to_string(),
                    options: vec!["Yes".to_string(), "No".to_string()],
                    default: Some(0),
                    category: ClarificationCategory::Confirmation,
                });
            }
        }

        if !questions.is_empty() {
            return Some(questions);
        }
    }

    None
}

// ─── Project Context Display ─────────────────────────────────────────────────

/// Format project context for display in plan mode.
pub fn format_project_context(ctx: &ProjectContext) -> String {
    let stack = if ctx.entry_points.contains(&"Cargo.toml".to_string()) {
        "Rust"
    } else if ctx.entry_points.contains(&"package.json".to_string()) {
        "Node.js"
    } else if ctx.entry_points.contains(&"pyproject.toml".to_string())
        || ctx.entry_points.contains(&"setup.py".to_string())
    {
        "Python"
    } else if ctx.entry_points.contains(&"go.mod".to_string()) {
        "Go"
    } else if !ctx.languages.is_empty() {
        // Use detected languages as fallback
        return format_project_context_line(
            &ctx.languages.join("/"),
            ctx.source_file_count,
            ctx.git_branch.as_deref(),
        );
    } else {
        "unknown"
    };

    format_project_context_line(stack, ctx.source_file_count, ctx.git_branch.as_deref())
}

fn format_project_context_line(stack: &str, file_count: usize, branch: Option<&str>) -> String {
    let mut parts = vec![stack.to_string(), format!("{file_count} files")];
    if let Some(b) = branch {
        parts.push(b.to_string());
    }
    format!("  {}", parts.join(" · "))
}

// ─── Plan Tree Progress Display ──────────────────────────────────────────────

/// Compact tree view options.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TreeViewMode {
    /// Compact: one line per task with status icon
    #[default]
    Compact,
    /// Detailed: shows descriptions and files
    Detailed,
    /// Progress: shows progress bars
    Progress,
}

/// Format a plan as a compact tree with progress indicators.
pub fn format_plan_tree(plan: &TaskPlan, mode: TreeViewMode) -> String {
    let mut out = String::new();

    let done = plan.items_done() as usize;
    let total = plan.subtasks.len();
    let pct = plan.progress_pct();

    // Header with progress bar
    out.push_str(&format_progress_header(pct, done, total));
    out.push('\n');

    // Task tree
    let ready: std::collections::HashSet<_> = plan
        .ready_subtasks()
        .iter()
        .map(|st| st.id.clone())
        .collect();

    for (i, st) in plan.subtasks.iter().enumerate() {
        let is_last = i == plan.subtasks.len() - 1;
        let prefix = if is_last { "└─" } else { "├─" };
        let icon = status_icon(&st.status, ready.contains(&st.id));
        let effort = effort_badge(&st.effort);

        match mode {
            TreeViewMode::Compact => {
                out.push_str(&format!("  {} {} {}{}\n", prefix, icon, st.title, effort));
            }
            TreeViewMode::Detailed => {
                out.push_str(&format!(
                    "  {} {} {} [{}]{}\n",
                    prefix, icon, st.title, st.id, effort
                ));
                let cont_prefix = if is_last { "   " } else { "│  " };
                if let Some(ref desc) = st.description {
                    out.push_str(&format!(
                        "  {}   └─ {}\n",
                        cont_prefix,
                        truncate_str(desc, 50)
                    ));
                }
                if !st.files.is_empty() {
                    out.push_str(&format!("  {}   📁 {}\n", cont_prefix, st.files.join(", ")));
                }
            }
            TreeViewMode::Progress => {
                let task_pct = if st.status == TaskStatus::Completed {
                    100
                } else {
                    0
                };
                let bar = mini_progress_bar(task_pct, 10);
                out.push_str(&format!(
                    "  {} {} {} {}{}\n",
                    prefix, icon, bar, st.title, effort
                ));
            }
        }
    }

    out
}

/// Format progress header with bar.
fn format_progress_header(pct: u32, done: usize, total: usize) -> String {
    let bar = progress_bar(pct, 20);
    format!("  📋 Progress {} {}% ({}/{})", bar, pct, done, total)
}

/// Filled and empty segment lengths for a fixed-width bar (`filled` clamped to `width`).
pub fn progress_bar_segments(pct: u32, width: usize) -> (usize, usize) {
    let filled = (pct as usize * width / 100).min(width);
    let empty = width.saturating_sub(filled);
    (filled, empty)
}

/// Generate a progress bar string.
fn progress_bar(pct: u32, width: usize) -> String {
    let (filled, empty) = progress_bar_segments(pct, width);
    format!("[{}{}]", "█".repeat(filled), "░".repeat(empty))
}

/// Generate a mini progress bar.
fn mini_progress_bar(pct: u32, width: usize) -> String {
    let (filled, empty) = progress_bar_segments(pct, width);
    format!("[{}{}]", "▓".repeat(filled), "░".repeat(empty))
}

/// Get status icon for a task.
fn status_icon(status: &TaskStatus, is_ready: bool) -> &'static str {
    match status {
        TaskStatus::Completed => "✓",
        TaskStatus::InProgress => "▶",
        TaskStatus::Failed => "✗",
        TaskStatus::Paused => "⏸",
        TaskStatus::Cancelled => "⊘",
        TaskStatus::Pending if is_ready => "○",
        TaskStatus::Pending => "·",
    }
}

/// Get effort badge.
fn effort_badge(effort: &Option<String>) -> &'static str {
    match effort.as_deref() {
        Some("small") => " [S]",
        Some("medium") => " [M]",
        Some("large") => " [L]",
        _ => "",
    }
}

/// Format compact status line (for inline display).
pub fn format_status_line(plan: &TaskPlan) -> String {
    let done = plan.items_done();
    let total = plan.subtasks.len();
    let pct = plan.progress_pct();
    let bar = mini_progress_bar(pct, 8);
    format!("{} {}% ({}/{})", bar, pct, done, total)
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

// ── Paused plan: operator corrections + rewind ─────────────────────────────

/// Case-insensitive ASCII prefix strip (`prefix` must be ASCII).
/// CRC32 hash for plan state integrity checking.
/// Uses the standard CRC32 polynomial (IEEE 802.3 / ITU-T V.42).
fn crc32_hash(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    crc ^ 0xFFFF_FFFF
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let s = s.trim_start();
    if s.len() < prefix.len() {
        return None;
    }
    let head = s.get(..prefix.len())?;
    if !head.eq_ignore_ascii_case(prefix) {
        return None;
    }
    Some(s.get(prefix.len()..)?.trim_start())
}

/// Where to rewind when re-running part of a plan (1-based index or id prefix).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanRewindAnchor {
    /// 1-based index in `plan.subtasks` order (as shown during execution).
    OneBased(usize),
    /// Exact id or unique prefix of `SubtaskPlan.id`.
    IdPrefix(String),
}

/// Lines typed at `⏸>` (paused execution) that adjust the plan without abandoning it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanPausedUserAction {
    /// Stack guidance to inject into upcoming subtask prompts.
    Correction(String),
    /// Drop all stacked guidance.
    ClearCorrections,
    /// Mark this subtask and all following ones pending again.
    Rewind(PlanRewindAnchor),
}

fn parse_rewind_anchor_rest(rest: &str) -> Option<PlanRewindAnchor> {
    let rest = rest.trim();
    if rest.is_empty() {
        return None;
    }
    if let Ok(n) = rest.parse::<usize>() {
        if n == 0 {
            return None;
        }
        return Some(PlanRewindAnchor::OneBased(n));
    }
    Some(PlanRewindAnchor::IdPrefix(rest.to_string()))
}

/// Parse `correct …`, `rewind N`, `restart from …`, etc. Returns `None` for normal chat.
pub fn parse_plan_paused_user_line(line: &str) -> Option<PlanPausedUserAction> {
    let t = line.trim();
    if t.is_empty() {
        return None;
    }
    let tl = t.to_ascii_lowercase();
    if matches!(tl.as_str(), "correct clear" | "note clear" | "adjust clear") {
        return Some(PlanPausedUserAction::ClearCorrections);
    }

    if let Some(rest) = strip_prefix_ci(t, "rewind ") {
        return parse_rewind_anchor_rest(rest).map(PlanPausedUserAction::Rewind);
    }
    if let Some(rest) = strip_prefix_ci(t, "restart from ") {
        return parse_rewind_anchor_rest(rest).map(PlanPausedUserAction::Rewind);
    }
    if let Some(rest) = strip_prefix_ci(t, "redo from ") {
        return parse_rewind_anchor_rest(rest).map(PlanPausedUserAction::Rewind);
    }
    if let Some(rest) = strip_prefix_ci(t, "restart ") {
        return parse_rewind_anchor_rest(rest).map(PlanPausedUserAction::Rewind);
    }
    if let Some(rest) = strip_prefix_ci(t, "redo ") {
        return parse_rewind_anchor_rest(rest).map(PlanPausedUserAction::Rewind);
    }

    for p in ["correct ", "note ", "adjust "] {
        if let Some(rest) = strip_prefix_ci(t, p) {
            if rest.is_empty() {
                return None;
            }
            return Some(PlanPausedUserAction::Correction(rest.to_string()));
        }
    }

    None
}

pub fn resolve_rewind_start_index(
    plan: &TaskPlan,
    anchor: &PlanRewindAnchor,
) -> Result<usize, String> {
    match anchor {
        PlanRewindAnchor::OneBased(n) => {
            if *n == 0 || *n > plan.subtasks.len() {
                return Err(format!("subtask index must be 1..={}", plan.subtasks.len()));
            }
            Ok(*n - 1)
        }
        PlanRewindAnchor::IdPrefix(s) => {
            let q = s.trim();
            if q.is_empty() {
                return Err("empty subtask id".into());
            }
            let matches: Vec<usize> = plan
                .subtasks
                .iter()
                .enumerate()
                .filter(|(_, st)| st.id == q || st.id.starts_with(q))
                .map(|(i, _)| i)
                .collect();
            match matches.len() {
                0 => Err(format!("no subtask id matches {q:?}")),
                1 => Ok(matches[0]),
                _ => Err(format!(
                    "ambiguous id {:?} ({} matches); use a longer prefix or `rewind N` (1-based)",
                    q,
                    matches.len()
                )),
            }
        }
    }
}

/// Set `start_idx` and all following subtasks (in plan order) to pending if they were in progress.
/// Returns how many subtasks were reset.
pub fn rewind_plan_from_subtask(plan: &mut TaskPlan, start_idx: usize) -> usize {
    let mut n = 0usize;
    for st in plan.subtasks.iter_mut().skip(start_idx) {
        if matches!(
            st.status,
            TaskStatus::Completed
                | TaskStatus::InProgress
                | TaskStatus::Paused
                | TaskStatus::Failed
        ) {
            st.status = TaskStatus::Pending;
            n += 1;
        }
    }
    n
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

    if !subtask.acceptance_checks.is_empty() {
        prompt.push_str("\nAcceptance checks (automated verification will run these):\n");
        for (i, vk) in subtask.acceptance_checks.iter().enumerate() {
            let desc = match vk {
                astra_services::durable_task::VerifierKind::FileExists { paths } => {
                    format!("Files exist: {}", paths.join(", "))
                }
                astra_services::durable_task::VerifierKind::ReadFileContains {
                    path,
                    contains,
                    ..
                } => {
                    format!("{path} contains {:?}", contains)
                }
                astra_services::durable_task::VerifierKind::GrepCheck {
                    file,
                    pattern,
                    should_match,
                } => {
                    if *should_match {
                        format!("grep '{pattern}' matches in {file}")
                    } else {
                        format!("grep '{pattern}' must NOT match in {file}")
                    }
                }
                astra_services::durable_task::VerifierKind::Command { cmd, .. } => {
                    format!("Command succeeds: {cmd}")
                }
                astra_services::durable_task::VerifierKind::CommandOutput {
                    cmd, contains, ..
                } => {
                    format!("{cmd} output contains {:?}", contains)
                }
                astra_services::durable_task::VerifierKind::BuildPass { cmd } => {
                    format!("Build: {cmd}")
                }
                astra_services::durable_task::VerifierKind::TestPass { cmd, .. } => {
                    format!("Test: {cmd}")
                }
                _ => "Automated check".into(),
            };
            prompt.push_str(&format!("  {}. {}\n", i + 1, desc));
        }
    }

    prompt.push_str(
        "\nPlease implement this change. Read the relevant files first, \
         make the changes, and verify they compile/pass tests.",
    );

    prompt
}

/// Same as [`format_subtask_prompt`] but prefixes stacked operator notes (pause / correct …).
pub fn format_subtask_prompt_with_operator_notes(
    subtask: &SubtaskPlan,
    operator_notes: &[String],
) -> String {
    let body = format_subtask_prompt(subtask);
    if operator_notes.is_empty() {
        return body;
    }
    let mut block = String::from(
        "[Operator guidance — follow for this subtask unless unsafe; reconcile with the task text.]\n",
    );
    for (i, note) in operator_notes.iter().enumerate() {
        block.push_str(&format!("{}. {}\n", i + 1, note));
    }
    format!("{block}\n{body}")
}

// ─── Plan Execution Config & Preview ─────────────────────────────────────────

/// Configuration for plan execution behavior.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlanExecutionConfig {
    /// If true, prompt user for confirmation before executing each subtask.
    pub step_by_step: bool,
    /// If true, auto-execute immediately after plan decomposition (skip explicit "execute").
    pub auto_execute: bool,
}

/// Result of a plan execution for summary purposes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
        if self.paused > 0 {
            out.push_str(&format!("│ Paused:    {}\n", self.paused));
        }
        if self.parallel_rounds > 0 {
            out.push_str(&format!(
                "│ Rounds:    {} (parallel-aware)\n",
                self.parallel_rounds
            ));
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
    out.push_str(&format!(
        "Execution: {} subtasks, {} ready\n",
        plan.subtasks.len(),
        ready.len()
    ));

    if analysis.groups.len() > 1 || analysis.groups.first().map(|g| g.len()).unwrap_or(0) > 1 {
        for (i, group) in analysis.groups.iter().enumerate() {
            let ids: Vec<_> = group.iter().map(|id| id.as_str()).collect();
            let parallel = if group.len() > 1 { " (parallel)" } else { "" };
            out.push_str(&format!(
                "  Round {}{}: {}\n",
                i + 1,
                parallel,
                ids.join(", ")
            ));
        }
    }

    if !analysis.conflicts.is_empty() {
        out.push_str(&format!(
            "  ⚠ {} file conflict(s): ",
            analysis.conflicts.len()
        ));
        let conflict_strs: Vec<_> = analysis
            .conflicts
            .iter()
            .map(|c| {
                format!(
                    "{} ↔ {} ({})",
                    c.subtask_a,
                    c.subtask_b,
                    c.shared_files.join(", ")
                )
            })
            .collect();
        out.push_str(&conflict_strs.join(", "));
        out.push('\n');
    }

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
        0..=3 => "low",
        4..=8 => "medium",
        _ => "high",
    };
    out.push_str(&format!(
        "  Effort: {effort_label} ({total_effort} units)\n"
    ));

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
        let v_from = self
            .get_version(from)
            .ok_or_else(|| format!("Version {} not found", from))?;
        let v_to = self
            .get_version(to)
            .ok_or_else(|| format!("Version {} not found", to))?;

        let old_ids: std::collections::HashSet<&str> =
            v_from.plan.subtasks.iter().map(|s| s.id.as_str()).collect();
        let new_ids: std::collections::HashSet<&str> =
            v_to.plan.subtasks.iter().map(|s| s.id.as_str()).collect();

        let added: Vec<String> = new_ids
            .difference(&old_ids)
            .map(|s| s.to_string())
            .collect();
        let removed: Vec<String> = old_ids
            .difference(&new_ids)
            .map(|s| s.to_string())
            .collect();

        // Detect modified (same ID but different title/description/deps)
        let mut modified = Vec::new();
        for st_new in &v_to.plan.subtasks {
            if let Some(st_old) = v_from.plan.subtasks.iter().find(|s| s.id == st_new.id)
                && (st_new.title != st_old.title
                    || st_new.description != st_old.description
                    || st_new.depends_on != st_old.depends_on
                    || st_new.effort != st_old.effort
                    || st_new.files != st_old.files)
            {
                modified.push(st_new.id.clone());
            }
        }

        Ok(PlanDiff {
            from_version: from,
            to_version: to,
            added,
            removed,
            modified,
        })
    }

    /// Format a compact version log for display.
    pub fn format_log(&self) -> String {
        if self.versions.is_empty() {
            return "  No version history yet.\n".to_string();
        }
        let mut out = String::new();
        for v in self.versions.iter().rev().take(10) {
            out.push_str(&format!(
                "  v{}: {} ({} subtasks) — {}\n",
                v.version,
                v.change_summary,
                v.plan.subtasks.len(),
                v.timestamp
            ));
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
        let mut out = format!(
            "  Plan diff v{} → v{}:\n",
            self.from_version, self.to_version
        );
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

// ─── Execution Timeline ─────────────────────────────────────────────────────

/// Types of events that can occur during plan execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TimelineEventKind {
    /// Plan was created
    PlanCreated { subtask_count: usize },
    /// Execution started (auto or step mode)
    ExecutionStarted { mode: String },
    /// A subtask started
    SubtaskStarted { subtask_id: String, title: String },
    /// A subtask completed successfully
    SubtaskCompleted {
        subtask_id: String,
        title: String,
        duration_sec: u64,
    },
    /// A subtask failed
    SubtaskFailed {
        subtask_id: String,
        title: String,
        error: String,
    },
    /// A subtask was skipped
    SubtaskSkipped {
        subtask_id: String,
        title: String,
        reason: String,
    },
    /// Plan was modified/replanned
    Replan { reason: String, changes: String },
    /// User provided feedback/rating
    UserRating { rating: u8 },
    /// Execution was paused
    ExecutionPaused { reason: String },
    /// Execution was resumed
    ExecutionResumed,
    /// Plan completed (all subtasks done)
    PlanCompleted { success: bool, duration_sec: u64 },
    /// A discovery/observation during execution
    Discovery { message: String },
    /// Git commit associated with changes
    GitCommit {
        commit_hash: String,
        message: String,
    },
}

/// A single event in the execution timeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineEvent {
    /// ISO 8601 timestamp
    pub timestamp: String,
    /// Human-readable time (e.g., "14:23")
    pub time_display: String,
    /// The event details
    pub event: TimelineEventKind,
}

impl TimelineEvent {
    /// Create a new timeline event with current timestamp.
    pub fn new(event: TimelineEventKind) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Convert to HH:MM format (simplified - assumes local time)
        let hours = (now / 3600) % 24;
        let minutes = (now / 60) % 60;

        Self {
            timestamp: now.to_string(),
            time_display: format!("{:02}:{:02}", hours, minutes),
            event,
        }
    }

    /// Format event for display
    pub fn format_display(&self) -> String {
        let icon = match &self.event {
            TimelineEventKind::PlanCreated { .. } => "📋",
            TimelineEventKind::ExecutionStarted { .. } => "▶",
            TimelineEventKind::SubtaskStarted { .. } => "→",
            TimelineEventKind::SubtaskCompleted { .. } => "✓",
            TimelineEventKind::SubtaskFailed { .. } => "✗",
            TimelineEventKind::SubtaskSkipped { .. } => "⏭",
            TimelineEventKind::Replan { .. } => "🔄",
            TimelineEventKind::UserRating { .. } => "⭐",
            TimelineEventKind::ExecutionPaused { .. } => "⏸",
            TimelineEventKind::ExecutionResumed => "▶",
            TimelineEventKind::PlanCompleted { success, .. } => {
                if *success {
                    "✅"
                } else {
                    "❌"
                }
            }
            TimelineEventKind::Discovery { .. } => "⚠",
            TimelineEventKind::GitCommit { .. } => "📦",
        };

        let desc = match &self.event {
            TimelineEventKind::PlanCreated { subtask_count } => {
                format!("Plan created ({} subtasks)", subtask_count)
            }
            TimelineEventKind::ExecutionStarted { mode } => format!("Started {} execution", mode),
            TimelineEventKind::SubtaskStarted { title, .. } => format!("Started: {}", title),
            TimelineEventKind::SubtaskCompleted {
                title,
                duration_sec,
                ..
            } => format!("{} ({} sec)", title, duration_sec),
            TimelineEventKind::SubtaskFailed { title, error, .. } => {
                format!("{} - {}", title, error)
            }
            TimelineEventKind::SubtaskSkipped { title, reason, .. } => {
                format!("Skipped: {} ({})", title, reason)
            }
            TimelineEventKind::Replan { reason, .. } => format!("Replan: {}", reason),
            TimelineEventKind::UserRating { rating } => format!("Rating: {}/5", rating),
            TimelineEventKind::ExecutionPaused { reason } => format!("Paused: {}", reason),
            TimelineEventKind::ExecutionResumed => "Resumed".to_string(),
            TimelineEventKind::PlanCompleted {
                success,
                duration_sec,
            } => {
                if *success {
                    format!("Completed ({} sec total)", duration_sec)
                } else {
                    format!("Failed ({} sec total)", duration_sec)
                }
            }
            TimelineEventKind::Discovery { message } => format!("Discovered: {}", message),
            TimelineEventKind::GitCommit {
                commit_hash,
                message,
            } => format!(
                "Commit {}: {}",
                &commit_hash[..7.min(commit_hash.len())],
                message
            ),
        };

        format!("{}  {} {}", self.time_display, icon, desc)
    }
}

/// Execution timeline tracking all events during plan execution.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecutionTimeline {
    /// All recorded events, in chronological order.
    pub events: Vec<TimelineEvent>,
    /// Plan creation timestamp (for duration calculation).
    pub start_timestamp: Option<String>,
    /// Plan completion timestamp.
    pub end_timestamp: Option<String>,
}

impl ExecutionTimeline {
    /// Record a new event.
    pub fn record(&mut self, kind: TimelineEventKind) {
        let event = TimelineEvent::new(kind);

        // Track start/end timestamps
        match &event.event {
            TimelineEventKind::PlanCreated { .. } => {
                self.start_timestamp = Some(event.timestamp.clone());
            }
            TimelineEventKind::PlanCompleted { .. } => {
                self.end_timestamp = Some(event.timestamp.clone());
            }
            _ => {}
        }

        self.events.push(event);
    }

    /// Record plan creation.
    pub fn plan_created(&mut self, subtask_count: usize) {
        self.record(TimelineEventKind::PlanCreated { subtask_count });
    }

    /// Record execution start.
    pub fn execution_started(&mut self, auto_mode: bool) {
        let mode = if auto_mode {
            "auto".to_string()
        } else {
            "step".to_string()
        };
        self.record(TimelineEventKind::ExecutionStarted { mode });
    }

    /// Record subtask start.
    pub fn subtask_started(&mut self, subtask_id: &str, title: &str) {
        self.record(TimelineEventKind::SubtaskStarted {
            subtask_id: subtask_id.to_string(),
            title: title.to_string(),
        });
    }

    /// Record subtask completion.
    pub fn subtask_completed(&mut self, subtask_id: &str, title: &str, duration_sec: u64) {
        self.record(TimelineEventKind::SubtaskCompleted {
            subtask_id: subtask_id.to_string(),
            title: title.to_string(),
            duration_sec,
        });
    }

    /// Record subtask failure.
    pub fn subtask_failed(&mut self, subtask_id: &str, title: &str, error: &str) {
        self.record(TimelineEventKind::SubtaskFailed {
            subtask_id: subtask_id.to_string(),
            title: title.to_string(),
            error: error.to_string(),
        });
    }

    /// Record a discovery/observation.
    pub fn discovery(&mut self, message: &str) {
        self.record(TimelineEventKind::Discovery {
            message: message.to_string(),
        });
    }

    /// Record a git commit.
    pub fn git_commit(&mut self, commit_hash: &str, message: &str) {
        self.record(TimelineEventKind::GitCommit {
            commit_hash: commit_hash.to_string(),
            message: message.to_string(),
        });
    }

    /// Record plan completion.
    pub fn plan_completed(&mut self, success: bool) {
        let duration_sec = self
            .start_timestamp
            .as_ref()
            .and_then(|s| s.parse::<u64>().ok())
            .map(|start| {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                now.saturating_sub(start)
            })
            .unwrap_or(0);

        self.record(TimelineEventKind::PlanCompleted {
            success,
            duration_sec,
        });
    }

    /// Format timeline for display.
    pub fn format_display(&self) -> String {
        if self.events.is_empty() {
            return "  (no events recorded)".to_string();
        }

        let mut out = String::new();
        for event in &self.events {
            out.push_str("  ");
            out.push_str(&event.format_display());
            out.push('\n');
        }
        out
    }

    /// Get total duration in seconds (if completed).
    pub fn total_duration_sec(&self) -> Option<u64> {
        match (self.start_timestamp.as_ref(), self.end_timestamp.as_ref()) {
            (Some(start), Some(end)) => {
                let start_sec = start.parse::<u64>().ok()?;
                let end_sec = end.parse::<u64>().ok()?;
                Some(end_sec.saturating_sub(start_sec))
            }
            _ => None,
        }
    }

    /// Count completed subtasks.
    pub fn completed_subtask_count(&self) -> usize {
        self.events
            .iter()
            .filter(|e| matches!(e.event, TimelineEventKind::SubtaskCompleted { .. }))
            .count()
    }

    /// Count failed subtasks.
    pub fn failed_subtask_count(&self) -> usize {
        self.events
            .iter()
            .filter(|e| matches!(e.event, TimelineEventKind::SubtaskFailed { .. }))
            .count()
    }

    /// Count replans.
    pub fn replan_count(&self) -> usize {
        self.events
            .iter()
            .filter(|e| matches!(e.event, TimelineEventKind::Replan { .. }))
            .count()
    }
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
            groups: if ready.is_empty() {
                vec![]
            } else {
                vec![vec![ready[0].id.clone()]]
            },
            conflicts: vec![],
        };
    }

    // Detect file conflicts between all pairs of ready subtasks
    let mut conflicts = Vec::new();
    for i in 0..ready.len() {
        for j in (i + 1)..ready.len() {
            let shared: Vec<String> = ready[i]
                .files
                .iter()
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
    let conflict_pairs: std::collections::HashSet<(String, String)> = conflicts
        .iter()
        .flat_map(|c| {
            vec![
                (c.subtask_a.clone(), c.subtask_b.clone()),
                (c.subtask_b.clone(), c.subtask_a.clone()),
            ]
        })
        .collect();

    let mut groups: Vec<Vec<String>> = Vec::new();
    for st in &ready {
        let mut placed = false;
        for group in groups.iter_mut() {
            let has_conflict = group
                .iter()
                .any(|g_id| conflict_pairs.contains(&(g_id.clone(), st.id.clone())));
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
            out.push_str(&format!(
                "  Sequential: {}\n",
                analysis.groups[0].join(", ")
            ));
        }
        return out;
    }

    out.push_str("  ┌── Parallel Execution Groups ──\n");
    for (i, group) in analysis.groups.iter().enumerate() {
        let label = if group.len() > 1 { "║" } else { "│" };
        out.push_str(&format!(
            "  {} Group {}: {}\n",
            label,
            i + 1,
            group.join(" + ")
        ));
    }
    out.push_str("  └────────────────────────────────\n");

    if !analysis.conflicts.is_empty() {
        out.push_str("  ⚠ File conflicts:\n");
        for c in &analysis.conflicts {
            out.push_str(&format!(
                "    {} ↔ {} on: {}\n",
                c.subtask_a,
                c.subtask_b,
                c.shared_files.join(", ")
            ));
        }
    }

    out
}

// ─── Replan Detection ───────────────────────────────────────────────────────

/// Reasons for suggesting a replan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplanReason {
    /// Subtask execution failed
    SubtaskFailed { subtask_id: String, error: String },
    /// Dependencies are deadlocked (circular or missing)
    DependencyDeadlock { blocked_ids: Vec<String> },
    /// Too many file conflicts blocking parallel execution
    FileConflicts { conflict_count: usize },
    /// Execution taking longer than expected
    SlowExecution { rounds: usize, expected: usize },
    /// User explicitly requested replan
    UserRequest,
}

impl ReplanReason {
    pub fn format(&self) -> String {
        match self {
            Self::SubtaskFailed { subtask_id, error } => {
                format!("Subtask '{}' failed: {}", subtask_id, error)
            }
            Self::DependencyDeadlock { blocked_ids } => {
                format!(
                    "Dependency deadlock: {} subtasks blocked",
                    blocked_ids.len()
                )
            }
            Self::FileConflicts { conflict_count } => {
                format!(
                    "{} file conflicts preventing parallel execution",
                    conflict_count
                )
            }
            Self::SlowExecution { rounds, expected } => {
                format!("Execution slow: {} rounds (expected {})", rounds, expected)
            }
            Self::UserRequest => "User requested replan".to_string(),
        }
    }
}

/// Replan suggestion with reason and proposed action.
#[derive(Debug, Clone)]
pub struct ReplanSuggestion {
    pub reason: ReplanReason,
    pub suggested_action: String,
    pub auto_applicable: bool, // Can be applied without user confirmation
}

/// Detect if a replan is needed based on current plan state.
pub fn detect_replan_needed(
    plan: &TaskPlan,
    execution_rounds: usize,
    failed_subtasks: &[(&str, &str)], // (subtask_id, error_message)
) -> Option<ReplanSuggestion> {
    use astra_services::task_orchestrator::TaskStatus;

    // 1. Check for failed subtasks
    if let Some((id, error)) = failed_subtasks.first() {
        return Some(ReplanSuggestion {
            reason: ReplanReason::SubtaskFailed {
                subtask_id: id.to_string(),
                error: error.to_string(),
            },
            suggested_action: format!(
                "Retry subtask '{}' or modify dependencies to work around failure",
                id
            ),
            auto_applicable: false,
        });
    }

    // 2. Check for dependency deadlock
    let pending: Vec<&str> = plan
        .subtasks
        .iter()
        .filter(|s| s.status == TaskStatus::Pending)
        .map(|s| s.id.as_str())
        .collect();

    let ready = plan.ready_subtasks();

    // If there are pending subtasks but none are ready, we have a deadlock
    if !pending.is_empty() && ready.is_empty() {
        let blocked_ids: Vec<String> = pending.iter().map(|s| s.to_string()).collect();
        return Some(ReplanSuggestion {
            reason: ReplanReason::DependencyDeadlock { blocked_ids },
            suggested_action: "Review dependencies and remove or reorder blocked subtasks"
                .to_string(),
            auto_applicable: false,
        });
    }

    // 3. Check for excessive file conflicts
    let analysis = analyze_parallelism(plan);
    if analysis.conflicts.len() >= 3 {
        return Some(ReplanSuggestion {
            reason: ReplanReason::FileConflicts {
                conflict_count: analysis.conflicts.len(),
            },
            suggested_action: "Split subtasks to reduce file overlap or merge related subtasks"
                .to_string(),
            auto_applicable: false,
        });
    }

    // 4. Check for slow execution
    let expected_rounds = plan.subtasks.len();
    if execution_rounds > expected_rounds * 2 && execution_rounds >= 6 {
        return Some(ReplanSuggestion {
            reason: ReplanReason::SlowExecution {
                rounds: execution_rounds,
                expected: expected_rounds,
            },
            suggested_action: "Review subtask complexity or split into smaller tasks".to_string(),
            auto_applicable: false,
        });
    }

    None
}

/// Generate a replan prompt for the LLM.
pub fn generate_replan_prompt(
    original_goal: &str,
    current_plan: &TaskPlan,
    reason: &ReplanReason,
    context: &ProjectContext,
) -> String {
    use astra_services::task_orchestrator::TaskStatus;

    let mut prompt = String::with_capacity(2048);

    prompt.push_str("You are replanning a task that encountered issues during execution.\n\n");

    prompt.push_str("## Original Goal\n");
    prompt.push_str(original_goal);
    prompt.push_str("\n\n");

    prompt.push_str("## Current Plan Status\n");
    for st in &current_plan.subtasks {
        let status_icon = match st.status {
            TaskStatus::Completed => "✅",
            TaskStatus::Failed => "❌",
            TaskStatus::InProgress => "🔄",
            TaskStatus::Pending => "⏳",
            _ => "○",
        };
        prompt.push_str(&format!(
            "- {} [{}] {} (deps: {:?})\n",
            status_icon, st.id, st.title, st.depends_on
        ));
    }
    prompt.push('\n');

    prompt.push_str("## Problem Encountered\n");
    prompt.push_str(&reason.format());
    prompt.push_str("\n\n");

    prompt.push_str("## Project Context\n");
    prompt.push_str(&format!("- Languages: {}\n", context.languages.join(", ")));
    if let Some(ref branch) = context.git_branch {
        prompt.push_str(&format!("- Git branch: {}\n", branch));
    }
    prompt.push('\n');

    prompt.push_str(
        r#"## Instructions
Generate a revised plan that:
1. Keeps completed subtasks as-is (do NOT modify them)
2. Addresses the problem by modifying pending/failed subtasks
3. May add new subtasks if needed
4. May remove blocked subtasks if they're no longer relevant
5. Updates dependencies to resolve deadlocks

Return JSON in the same format as the original plan:
```json
{
  "subtasks": [...],
  "notes": "Explanation of changes made"
}
```
"#,
    );

    prompt
}

/// Format replan suggestion for CLI display.
pub fn format_replan_suggestion(suggestion: &ReplanSuggestion) -> String {
    let mut out = String::new();
    out.push('\n');
    out.push_str("  ⚠️ Replan Suggested\n");
    out.push_str(&format!("  Reason: {}\n", suggestion.reason.format()));
    out.push_str(&format!("  Action: {}\n", suggestion.suggested_action));
    out.push('\n');
    out.push_str("  Type '/plan replan' to regenerate the plan\n");
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
        let lang_match = t.languages.is_empty()
            || t.languages.iter().any(|l| {
                context
                    .languages
                    .iter()
                    .any(|cl| cl.eq_ignore_ascii_case(l))
            });
        if !lang_match {
            continue;
        }

        // Goal keyword match
        let name_match =
            goal_lower.contains(&t.name.replace('-', " ")) || goal_lower.contains(&t.name);
        let desc_match = t
            .description
            .split_whitespace()
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
    let plans_dir = std::path::PathBuf::from(&home).join(".astra");

    let mut result = Vec::new();

    // Check for active plan state
    let state_path = plans_dir.join("plan_state.json");
    if state_path.exists()
        && let Ok(state) = PlanModeState::load_from_file(&state_path)
    {
        result.push(SavedPlanInfo {
            name: "active".to_string(),
            goal: state.goal,
            progress_pct: state.plan.progress_pct(),
            subtask_count: state.plan.subtasks.len(),
            status: if state.plan.progress_pct() == 100 {
                "completed"
            } else {
                "active"
            }
            .to_string(),
        });
    }

    // Check for plan templates in templates dir
    let templates_dir = plans_dir.join("plan_templates");
    if templates_dir.exists()
        && let Ok(entries) = std::fs::read_dir(&templates_dir)
    {
        for entry in entries.flatten() {
            if let Some(ext) = entry.path().extension()
                && ext == "json"
                && let Ok(data) = std::fs::read_to_string(entry.path())
                && let Ok(tmpl) = serde_json::from_str::<PlanTemplate>(&data)
            {
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
            "active" | "in_progress" => "▶",
            "completed" => "✓",
            "pending" => "○",
            "template" => "📋",
            _ => "·",
        };
        let done_count = (p.subtask_count as u32 * p.progress_pct / 100) as usize;
        out.push_str(&format!(
            "  {} {} — {} ({}%, {}/{} subtasks)\n",
            status_icon,
            p.name,
            truncate_str(&p.goal, 40),
            p.progress_pct,
            done_count,
            p.subtask_count
        ));
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
    fn truncate_str_respects_utf8_char_boundaries() {
        let s = "在/tmp 下面构建一个js的网页，用户输入，展示绚丽的动态效果";
        let t = truncate_str(s, 20);
        assert!(t.ends_with("..."));
        assert!(
            t.chars().count() <= 20,
            "got {} chars: {t:?}",
            t.chars().count()
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
    fn progress_bar_segments_clamps_fill() {
        assert_eq!(progress_bar_segments(0, 10), (0, 10));
        assert_eq!(progress_bar_segments(50, 10), (5, 5));
        assert_eq!(progress_bar_segments(100, 10), (10, 0));
        assert_eq!(progress_bar_segments(100, 3), (3, 0));
        assert_eq!(progress_bar_segments(200, 5), (5, 0));
        let (f, e) = progress_bar_segments(33, 3);
        assert_eq!(f + e, 3);
    }

    #[test]
    fn format_plan_markdown_includes_goal_and_summary() {
        let plan = TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "t1".into(),
                title: "First".into(),
                description: Some("Desc".into()),
                depends_on: vec![],
                status: TaskStatus::Completed,
                effort: Some("small".into()),
                files: vec!["a.rs".into()],
                acceptance_checks: vec![astra_services::durable_task::VerifierKind::FileExists {
                    paths: vec!["a.rs".into()],
                }],
            }],
            notes: Some("Note line".into()),
        };
        let md = format_plan_markdown(&plan, Some("Ship it"));
        assert!(md.contains("Ship it"));
        assert!(md.contains("t1"));
        assert!(md.contains("First"));
        assert!(md.contains("Note line"));
        assert!(md.contains("100%") || md.contains("1/1"));
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
        assert!(
            prompt.contains("Never refuse") && prompt.contains("plan-decomposition"),
            "should require JSON despite no tools in this phase: {prompt}"
        );
    }

    #[test]
    fn plan_response_parse_error_preview_truncates() {
        let s = "a\nb\nc\nd\ne\nf";
        let p = plan_response_parse_error_preview(s, 3, 100);
        assert!(p.contains('a') && p.contains('b') && p.contains('c'));
        assert!(p.contains('…'));
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
    fn plan_mode_state_legacy_background_execution_deserializes() {
        let mut ps = PlanModeState::new("goal".into(), ProjectContext::default());
        ps.set_plan(TaskPlan {
            subtasks: vec![],
            notes: None,
        });
        // Older plan files had a `background_execution` key — ensure it doesn't break deserialization.
        let mut v = serde_json::to_value(&ps).expect("serialize");
        let obj = v.as_object_mut().expect("object");
        obj.insert(
            "background_execution".to_string(),
            serde_json::Value::Bool(true),
        );
        let _loaded: PlanModeState =
            serde_json::from_value(v).expect("deserialize with legacy field");
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
    fn plan_mode_prompt_uses_followup_instructions_when_all_completed() {
        let ctx = ProjectContext::default();
        let mut ps = PlanModeState::new("build login".into(), ctx);
        ps.set_plan(TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "html".into(),
                title: "Create HTML".into(),
                status: TaskStatus::Completed,
                ..Default::default()
            }],
            notes: None,
        });

        let prompt = ps.plan_mode_prompt("move files into a directory");
        assert!(
            prompt.contains("ALREADY COMPLETED"),
            "should use follow-up instructions"
        );
        assert!(
            prompt.contains("Small tweak"),
            "should mention scope assessment"
        );
        assert!(
            !prompt.contains("If answering a question"),
            "should NOT use default instructions"
        );
    }

    #[test]
    fn plan_mode_prompt_uses_default_instructions_when_not_all_completed() {
        let ctx = ProjectContext::default();
        let mut ps = PlanModeState::new("build login".into(), ctx);
        ps.set_plan(TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "html".into(),
                    title: "Create HTML".into(),
                    status: TaskStatus::Completed,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "css".into(),
                    title: "Add CSS".into(),
                    status: TaskStatus::Pending,
                    ..Default::default()
                },
            ],
            notes: None,
        });

        let prompt = ps.plan_mode_prompt("change approach");
        assert!(
            !prompt.contains("ALREADY COMPLETED"),
            "should NOT use follow-up instructions"
        );
        assert!(
            prompt.contains("If answering a question"),
            "should use default instructions"
        );
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
        assert!(content.starts_with("[@plan/active]"));
        assert!(content.contains("Deploy app"));

        let completed = ps.to_completed_memory();
        assert!(completed.starts_with("[@plan/completed]"));
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
            test_file_count: 15,
            git_dirty_count: 2,
            has_uncommitted_changes: true,
            key_directories: vec!["src".to_string(), "tests".to_string()],
            key_modules: vec![
                ("src/api.ts".to_string(), 500),
                ("src/db.ts".to_string(), 300),
            ],
            git_branch: Some("feature/auth".to_string()),
            test_framework: Some("jest".to_string()),
            prior_templates: vec![],
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
            prompt.contains("acceptance_checks"),
            "should ask for acceptance_checks: {prompt}"
        );
    }

    #[test]
    fn decomposition_prompt_includes_prior_templates() {
        let ctx = ProjectContext {
            root: "/test".into(),
            languages: vec!["Rust".into()],
            prior_templates: vec![PlanTemplateHint {
                goal_pattern: "add jwt authentication".into(),
                subtask_titles: vec![
                    "Create auth module".into(),
                    "Add middleware".into(),
                    "Write tests".into(),
                ],
                success_rate: 0.95,
                use_count: 3,
            }],
            ..Default::default()
        };

        let prompt = decomposition_prompt("Add OAuth login", &ctx);
        assert!(
            prompt.contains("Learned Patterns"),
            "should include templates section"
        );
        assert!(
            prompt.contains("add jwt authentication"),
            "should include template goal"
        );
        assert!(
            prompt.contains("Create auth module"),
            "should include subtask titles"
        );
        assert!(prompt.contains("95%"), "should include success rate");
        assert!(prompt.contains("3 times"), "should include use count");
    }

    #[test]
    fn decomposition_prompt_omits_empty_templates() {
        let ctx = ProjectContext {
            root: "/test".into(),
            languages: vec!["Rust".into()],
            prior_templates: vec![],
            ..Default::default()
        };

        let prompt = decomposition_prompt("Add feature", &ctx);
        assert!(
            !prompt.contains("Learned Patterns"),
            "should not include templates section when empty"
        );
    }

    #[test]
    fn plan_template_hint_serde_roundtrip() {
        let hint = PlanTemplateHint {
            goal_pattern: "test goal".into(),
            subtask_titles: vec!["step 1".into(), "step 2".into()],
            success_rate: 0.8,
            use_count: 5,
        };
        let json = serde_json::to_string(&hint).unwrap();
        let deserialized: PlanTemplateHint = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.goal_pattern, "test goal");
        assert_eq!(deserialized.subtask_titles.len(), 2);
        assert!((deserialized.success_rate - 0.8).abs() < 0.001);
        assert_eq!(deserialized.use_count, 5);
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
      "acceptance_checks": [
        {"kind": "file_exists", "paths": ["src/models/user.ts"]},
        {"kind": "test_pass", "cmd": "npm test", "min_pass_rate": 1.0}
      ]
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
        assert_eq!(s0.acceptance_checks.len(), 2);
        assert!(matches!(
            &s0.acceptance_checks[0],
            astra_services::durable_task::VerifierKind::FileExists { paths }
            if paths == &["src/models/user.ts"]
        ));

        let s1 = &plan.subtasks[1];
        assert_eq!(s1.effort.as_deref(), Some("large"));
        assert!(
            s1.acceptance_checks.is_empty(),
            "missing field should default to empty"
        );
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
                    acceptance_checks: vec![astra_services::durable_task::VerifierKind::TestPass {
                        cmd: "cargo test".into(),
                        min_pass_rate: 1.0,
                    }],
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
        assert!(output.contains("✅"), "should show check count: {output}");
        assert!(
            output.contains("Effort:"),
            "should show effort summary: {output}"
        );
    }

    #[test]
    fn parse_plan_response_backward_compatible() {
        // Old format without effort/files/acceptance_checks should still parse
        let response = r#"{"subtasks": [{"id": "t1", "title": "Do thing"}]}"#;
        let plan = parse_plan_response(response).unwrap();
        assert_eq!(plan.subtasks[0].effort, None);
        assert!(plan.subtasks[0].files.is_empty());
        assert!(plan.subtasks[0].acceptance_checks.is_empty());
    }

    #[test]
    fn parse_plan_response_skips_unknown_verifier_kind() {
        let response = r#"{"subtasks": [{
            "id": "t1", "title": "Do thing",
            "acceptance_checks": [
                {"kind": "file_exists", "paths": ["a.rs"]},
                {"kind": "quantum_entanglement_check", "qubit": 42},
                {"kind": "grep_check", "file": "b.rs", "pattern": "fn main"}
            ]
        }]}"#;
        let plan = parse_plan_response(response).unwrap();
        assert_eq!(
            plan.subtasks[0].acceptance_checks.len(),
            2,
            "unknown kind should be skipped"
        );
        assert!(matches!(
            &plan.subtasks[0].acceptance_checks[0],
            astra_services::durable_task::VerifierKind::FileExists { paths } if paths == &["a.rs"]
        ));
        assert!(matches!(
            &plan.subtasks[0].acceptance_checks[1],
            astra_services::durable_task::VerifierKind::GrepCheck { file, .. } if file == "b.rs"
        ));
    }

    #[test]
    fn parse_plan_response_filters_command_variants() {
        let response = r#"{"subtasks": [{
            "id": "t1", "title": "Do thing",
            "acceptance_checks": [
                {"kind": "file_exists", "paths": ["a.rs"]},
                {"kind": "command", "cmd": "rm -rf /", "expected_exit": 0},
                {"kind": "command_output", "cmd": "cat /etc/passwd", "contains": ["root"]},
                {"kind": "read_file_contains", "path": "a.rs", "contains": ["fn main"]}
            ]
        }]}"#;
        let plan = parse_plan_response(response).unwrap();
        assert_eq!(
            plan.subtasks[0].acceptance_checks.len(),
            2,
            "Command and CommandOutput should be filtered out"
        );
        assert!(matches!(
            &plan.subtasks[0].acceptance_checks[0],
            astra_services::durable_task::VerifierKind::FileExists { .. }
        ));
        assert!(matches!(
            &plan.subtasks[0].acceptance_checks[1],
            astra_services::durable_task::VerifierKind::ReadFileContains { .. }
        ));
    }

    #[test]
    fn parse_plan_response_all_unknown_yields_empty_checks() {
        let response = r#"{"subtasks": [{
            "id": "t1", "title": "Do thing",
            "acceptance_checks": [
                {"kind": "nonexistent_check", "foo": "bar"}
            ]
        }]}"#;
        let plan = parse_plan_response(response).unwrap();
        assert!(plan.subtasks[0].acceptance_checks.is_empty());
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
        assert!(!prompt.contains("Description:"));
        assert!(!prompt.contains("Files to modify:"));
        assert!(!prompt.contains("Acceptance checks"));
    }

    #[test]
    fn format_subtask_prompt_full() {
        let st = SubtaskPlan {
            id: "t2".into(),
            title: "Add auth middleware".into(),
            description: Some("JWT token validation for all /api routes".into()),
            files: vec!["src/middleware.rs".into(), "src/auth.rs".into()],
            acceptance_checks: vec![astra_services::durable_task::VerifierKind::GrepCheck {
                file: "src/middleware.rs".into(),
                pattern: "401".into(),
                should_match: true,
            }],
            ..Default::default()
        };
        let prompt = format_subtask_prompt(&st);
        assert!(prompt.contains("Add auth middleware"));
        assert!(prompt.contains("JWT token validation"));
        assert!(prompt.contains("src/middleware.rs, src/auth.rs"));
        assert!(
            prompt.contains("401"),
            "should mention 401 from acceptance checks"
        );
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

    #[test]
    fn parse_plan_paused_corrections_and_rewind() {
        assert_eq!(
            parse_plan_paused_user_line("correct skip tests for now"),
            Some(PlanPausedUserAction::Correction(
                "skip tests for now".into()
            ))
        );
        assert_eq!(
            parse_plan_paused_user_line("note use feature flags"),
            Some(PlanPausedUserAction::Correction("use feature flags".into()))
        );
        assert_eq!(
            parse_plan_paused_user_line("correct clear"),
            Some(PlanPausedUserAction::ClearCorrections)
        );
        assert_eq!(
            parse_plan_paused_user_line("rewind 2"),
            Some(PlanPausedUserAction::Rewind(PlanRewindAnchor::OneBased(2)))
        );
        assert_eq!(
            parse_plan_paused_user_line("restart from st-a"),
            Some(PlanPausedUserAction::Rewind(PlanRewindAnchor::IdPrefix(
                "st-a".into()
            )))
        );
        assert!(parse_plan_paused_user_line("hello").is_none());
    }

    #[test]
    fn rewind_plan_resets_suffix() {
        let mut plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "1".into(),
                    title: "a".into(),
                    status: TaskStatus::Completed,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "2".into(),
                    title: "b".into(),
                    status: TaskStatus::Completed,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "3".into(),
                    title: "c".into(),
                    status: TaskStatus::InProgress,
                    ..Default::default()
                },
            ],
            notes: None,
        };
        let idx = resolve_rewind_start_index(&plan, &PlanRewindAnchor::OneBased(2)).unwrap();
        assert_eq!(idx, 1);
        let n = rewind_plan_from_subtask(&mut plan, idx);
        assert_eq!(n, 2);
        assert_eq!(plan.subtasks[0].status, TaskStatus::Completed);
        assert_eq!(plan.subtasks[1].status, TaskStatus::Pending);
        assert_eq!(plan.subtasks[2].status, TaskStatus::Pending);
    }

    // ═══════════════════════════ Plan Versioning Tests ════════════════════════

    #[test]
    fn version_history_record_and_retrieve() {
        let mut history = PlanVersionHistory::default();
        let plan = TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "a".into(),
                title: "A".into(),
                ..Default::default()
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
            ],
            notes: None,
        };
        history.record(&plan_v1, "v1");

        let plan_v2 = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "a".into(),
                    title: "A modified".into(),
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "c".into(),
                    title: "C new".into(),
                    ..Default::default()
                },
            ],
            notes: None,
        };
        history.record(&plan_v2, "v2");

        let diff = history.diff_versions(1, 2).unwrap();
        assert!(diff.added.contains(&"c".to_string()), "c should be added");
        assert!(
            diff.removed.contains(&"b".to_string()),
            "b should be removed"
        );
        assert!(
            diff.modified.contains(&"a".to_string()),
            "a should be modified"
        );
    }

    #[test]
    fn version_diff_no_changes() {
        let mut history = PlanVersionHistory::default();
        let plan = TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "a".into(),
                title: "A".into(),
                ..Default::default()
            }],
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
        let plan = TaskPlan {
            subtasks: vec![],
            notes: None,
        };
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
                    id: "a".into(),
                    title: "A".into(),
                    files: vec!["src/main.rs".into()],
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "b".into(),
                    title: "B".into(),
                    files: vec!["src/main.rs".into(), "src/lib.rs".into()],
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "c".into(),
                    title: "C".into(),
                    files: vec!["src/other.rs".into()],
                    ..Default::default()
                },
            ],
            notes: None,
        };

        let analysis = analyze_parallelism(&plan);
        assert!(!analysis.conflicts.is_empty(), "should detect a-b conflict");
        assert!(
            analysis.conflicts[0]
                .shared_files
                .contains(&"src/main.rs".to_string())
        );

        // a and b should be in different groups, c can go with either
        assert!(
            analysis.groups.len() >= 2,
            "should split conflicting subtasks: {:?}",
            analysis.groups
        );
    }

    #[test]
    fn parallel_groups_single_subtask() {
        let plan = TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "only".into(),
                title: "Only one".into(),
                ..Default::default()
            }],
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
                SubtaskPlan {
                    id: "a".into(),
                    title: "A".into(),
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

        let analysis = analyze_parallelism(&plan);
        // Only "a" is ready, "b" depends on "a"
        assert_eq!(analysis.groups.len(), 1);
        assert_eq!(analysis.groups[0], vec!["a"]);
    }

    #[test]
    fn format_parallelism_display() {
        let analysis = ParallelGroups {
            groups: vec![vec!["a".into(), "c".into()], vec!["b".into()]],
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
        assert!(
            output.contains("src/main.rs"),
            "should show conflicting file"
        );
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
                    assert!(
                        ids.contains(&dep.as_str()),
                        "Template '{}': subtask '{}' depends on '{}' which doesn't exist",
                        template.name,
                        st.id,
                        dep
                    );
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
            from_version: 1,
            to_version: 2,
            added: vec![],
            removed: vec![],
            modified: vec![],
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
                    id: "a".into(),
                    title: "Step A".into(),
                    files: vec!["src/a.rs".into()],
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "b".into(),
                    title: "Step B".into(),
                    files: vec!["src/b.rs".into()],
                    ..Default::default()
                },
                // Group 2: c depends on a, d depends on b
                SubtaskPlan {
                    id: "c".into(),
                    title: "Step C".into(),
                    depends_on: vec!["a".into()],
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "d".into(),
                    title: "Step D".into(),
                    depends_on: vec!["b".into()],
                    ..Default::default()
                },
                // Group 3: e depends on c and d
                SubtaskPlan {
                    id: "e".into(),
                    title: "Step E".into(),
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

        assert_eq!(
            execution_rounds.len(),
            3,
            "should have 3 rounds: {:?}",
            execution_rounds
        );
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
                    id: "a".into(),
                    title: "A".into(),
                    files: vec!["shared.rs".into()],
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "b".into(),
                    title: "B".into(),
                    files: vec!["shared.rs".into()],
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "c".into(),
                    title: "C".into(),
                    files: vec!["other.rs".into()],
                    ..Default::default()
                },
            ],
            notes: None,
        };

        let analysis = analyze_parallelism(&plan);
        // a and b conflict on shared.rs, so they should be in different groups
        assert!(
            analysis.groups.len() >= 2,
            "conflicting tasks should split: {:?}",
            analysis.groups
        );
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
        assert!(
            rounds.len() >= 2,
            "file conflict should force multiple rounds: {:?}",
            rounds
        );
        assert_eq!(plan.progress_pct(), 100);
    }

    #[test]
    fn parallel_execution_single_chain_is_sequential() {
        // Linear dependency chain: a → b → c
        let mut plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "a".into(),
                    title: "A".into(),
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "b".into(),
                    title: "B".into(),
                    depends_on: vec!["a".into()],
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "c".into(),
                    title: "C".into(),
                    depends_on: vec!["b".into()],
                    ..Default::default()
                },
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
            subtasks: vec![SubtaskPlan {
                id: "a".into(),
                title: "A".into(),
                ..Default::default()
            }],
            notes: None,
        });
        assert_eq!(ps.version_history.current_version, 1);

        // update_plan should also record
        ps.update_plan(
            TaskPlan {
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
                ],
                notes: None,
            },
            "Added subtask b",
        );
        assert_eq!(ps.version_history.current_version, 2);

        // Rollback should work
        let result = ps.rollback_to_version(1);
        assert!(result.is_ok());
        assert_eq!(
            ps.plan.subtasks.len(),
            1,
            "should rollback to v1 with 1 subtask"
        );
        assert_eq!(
            ps.version_history.current_version, 3,
            "rollback creates new version"
        );
    }

    #[test]
    fn version_history_persists_through_save_load() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

        let ctx = ProjectContext::default();
        let mut ps = PlanModeState::new("test".into(), ctx);
        ps.set_plan(TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "a".into(),
                title: "A".into(),
                ..Default::default()
            }],
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
                SubtaskPlan {
                    id: "a".into(),
                    title: "A".into(),
                    effort: Some("small".into()),
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "b".into(),
                    title: "B".into(),
                    effort: Some("medium".into()),
                    ..Default::default()
                },
            ],
            notes: None,
        };
        let preview = format_execution_preview(&plan);
        assert!(preview.contains("2 subtasks"));
        assert!(preview.contains("2 ready"));
        assert!(preview.contains("Effort:"));
    }

    #[test]
    fn execution_preview_shows_parallel_groups() {
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "a".into(),
                    title: "A".into(),
                    files: vec!["a.rs".into()],
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "b".into(),
                    title: "B".into(),
                    files: vec!["b.rs".into()],
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "c".into(),
                    title: "C".into(),
                    depends_on: vec!["a".into()],
                    ..Default::default()
                },
            ],
            notes: None,
        };
        let preview = format_execution_preview(&plan);
        assert!(preview.contains("Round"));
        assert!(
            preview.contains("parallel")
                || preview.contains("Round 1")
                || preview.contains("Round 2")
        );
    }

    #[test]
    fn execution_preview_shows_file_conflicts() {
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "a".into(),
                    title: "A".into(),
                    files: vec!["shared.rs".into()],
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "b".into(),
                    title: "B".into(),
                    files: vec!["shared.rs".into()],
                    ..Default::default()
                },
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
                SubtaskPlan {
                    id: "a".into(),
                    title: "A".into(),
                    status: TaskStatus::Completed,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "b".into(),
                    title: "B".into(),
                    status: TaskStatus::Completed,
                    ..Default::default()
                },
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
                SubtaskPlan {
                    id: "a".into(),
                    title: "A".into(),
                    status: TaskStatus::Completed,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "b".into(),
                    title: "B".into(),
                    status: TaskStatus::Failed,
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
            ],
            notes: None,
        };
        let summary = PlanExecutionSummary::from_plan(&plan, "Paused goal", 1);
        assert_eq!(summary.paused, 1);
        assert!(summary.format().contains("Paused"));
    }

    #[test]
    fn parse_execution_confirmation_variants() {
        assert_eq!(
            parse_execution_confirmation("y"),
            ExecutionConfirmation::Execute
        );
        assert_eq!(
            parse_execution_confirmation("yes"),
            ExecutionConfirmation::Execute
        );
        assert_eq!(
            parse_execution_confirmation("go"),
            ExecutionConfirmation::Execute
        );
        assert_eq!(
            parse_execution_confirmation("确认"),
            ExecutionConfirmation::Execute
        );
        assert_eq!(
            parse_execution_confirmation("s"),
            ExecutionConfirmation::StepByStep
        );
        assert_eq!(
            parse_execution_confirmation("step"),
            ExecutionConfirmation::StepByStep
        );
        assert_eq!(
            parse_execution_confirmation("e"),
            ExecutionConfirmation::Edit
        );
        assert_eq!(
            parse_execution_confirmation("edit"),
            ExecutionConfirmation::Edit
        );
        assert_eq!(
            parse_execution_confirmation("n"),
            ExecutionConfirmation::Cancel
        );
        assert_eq!(
            parse_execution_confirmation("no"),
            ExecutionConfirmation::Cancel
        );
        assert_eq!(
            parse_execution_confirmation(""),
            ExecutionConfirmation::Cancel
        );
    }

    #[test]
    fn parse_subtask_confirmation_variants() {
        assert_eq!(
            parse_subtask_confirmation("y"),
            SubtaskConfirmation::Execute
        );
        assert_eq!(
            parse_subtask_confirmation("yes"),
            SubtaskConfirmation::Execute
        );
        assert_eq!(parse_subtask_confirmation(""), SubtaskConfirmation::Execute); // default = yes
        assert_eq!(parse_subtask_confirmation("s"), SubtaskConfirmation::Skip);
        assert_eq!(
            parse_subtask_confirmation("skip"),
            SubtaskConfirmation::Skip
        );
        assert_eq!(parse_subtask_confirmation("q"), SubtaskConfirmation::Quit);
        assert_eq!(
            parse_subtask_confirmation("quit"),
            SubtaskConfirmation::Quit
        );
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
                SubtaskPlan {
                    id: "x".into(),
                    title: "X".into(),
                    status: TaskStatus::Completed,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "y".into(),
                    title: "Y".into(),
                    status: TaskStatus::Pending,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "z".into(),
                    title: "Z".into(),
                    status: TaskStatus::Completed,
                    ..Default::default()
                },
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
                SubtaskPlan {
                    id: "a".into(),
                    title: "A".into(),
                    effort: Some("large".into()),
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "b".into(),
                    title: "B".into(),
                    effort: Some("large".into()),
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "c".into(),
                    title: "C".into(),
                    effort: Some("large".into()),
                    ..Default::default()
                },
            ],
            notes: None,
        };
        let preview = format_execution_preview(&plan);
        assert!(preview.contains("high"));

        // All small = Low effort
        let plan2 = TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "a".into(),
                title: "A".into(),
                effort: Some("small".into()),
                ..Default::default()
            }],
            notes: None,
        };
        let preview2 = format_execution_preview(&plan2);
        assert!(preview2.contains("low"));
    }

    // ═══════════════════════ Plan Entry Card Tests ═══════════════════════════

    #[test]
    fn plan_entry_card_shows_active_plan() {
        let ctx = ProjectContext::default();
        let mut ps = PlanModeState::new("Add authentication".into(), ctx);
        ps.set_plan(TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "1".into(),
                    title: "User model".into(),
                    status: TaskStatus::Completed,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "2".into(),
                    title: "JWT middleware".into(),
                    status: TaskStatus::InProgress,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "3".into(),
                    title: "Tests".into(),
                    ..Default::default()
                },
            ],
            notes: None,
        });

        let card = format_plan_entry_card(Some(&ps), None);
        assert!(card.contains("Plan Mode"));
        assert!(card.contains("Add authentication"));
        assert!(card.contains("continue"));
    }

    #[test]
    fn plan_entry_card_no_active_plan() {
        let card = format_plan_entry_card(None, None);
        assert!(card.contains("Plan Mode"));
        assert!(card.contains("Describe what you want"));
    }

    #[test]
    fn parse_plan_entry_choice_with_active() {
        assert_eq!(
            parse_plan_entry_choice("1", true, false),
            PlanEntryChoice::Continue
        );
        assert_eq!(
            parse_plan_entry_choice("continue", true, false),
            PlanEntryChoice::Continue
        );
        assert_eq!(
            parse_plan_entry_choice("2", true, false),
            PlanEntryChoice::Restart
        );
        assert_eq!(
            parse_plan_entry_choice("exit", true, false),
            PlanEntryChoice::Exit
        );

        // Unrecognized input becomes a goal
        let choice = parse_plan_entry_choice("add user auth", true, false);
        match choice {
            PlanEntryChoice::Goal(g) => assert_eq!(g, "add user auth"),
            _ => panic!("Expected Goal variant"),
        }
    }

    #[test]
    fn parse_plan_entry_choice_without_active() {
        // Without active plan, most inputs become goals
        let choice = parse_plan_entry_choice("add caching", false, false);
        match choice {
            PlanEntryChoice::Goal(g) => assert_eq!(g, "add caching"),
            _ => panic!("Expected Goal variant"),
        }

        // Exit still works
        assert_eq!(
            parse_plan_entry_choice("exit", false, false),
            PlanEntryChoice::Exit
        );
    }

    #[test]
    fn format_project_context_rust_project() {
        let ctx = ProjectContext {
            root: "/project".into(),
            entry_points: vec!["Cargo.toml".into()],
            languages: vec!["Rust".into()],
            structure_summary: "Top-level dirs: src, tests".into(),
            source_file_count: 42,
            test_framework: Some("cargo test".into()),
            git_branch: Some("main".into()),
            ..Default::default()
        };

        let formatted = format_project_context(&ctx);
        assert!(formatted.contains("Rust"));
        assert!(formatted.contains("42 files"));
        assert!(formatted.contains("main"));
    }

    // ═══════════════════════ Clarification Questions Tests ═══════════════════

    #[test]
    fn clarification_question_format_basic() {
        let q = ClarificationQuestion {
            question: "Which database should we use?".into(),
            options: vec!["PostgreSQL".into(), "MySQL".into(), "SQLite".into()],
            default: Some(0),
            category: ClarificationCategory::Technical,
        };

        let formatted = format_clarification_question(&q);
        assert!(formatted.contains("Which database"));
        assert!(formatted.contains("[1] PostgreSQL"));
        assert!(formatted.contains("[2] MySQL"));
        assert!(formatted.contains("(default)"));
        assert!(formatted.contains("🔧")); // Technical icon
    }

    #[test]
    fn clarification_question_categories() {
        let scope = ClarificationQuestion {
            question: "test".into(),
            options: vec![],
            default: None,
            category: ClarificationCategory::Scope,
        };
        assert!(format_clarification_question(&scope).contains("📦"));

        let approach = ClarificationQuestion {
            question: "test".into(),
            options: vec![],
            default: None,
            category: ClarificationCategory::Approach,
        };
        assert!(format_clarification_question(&approach).contains("🛤️"));
    }

    #[test]
    fn parse_clarification_response_number() {
        let q = ClarificationQuestion {
            question: "test".into(),
            options: vec!["A".into(), "B".into(), "C".into()],
            default: None,
            category: ClarificationCategory::Technical,
        };

        assert_eq!(
            parse_clarification_response("1", &q),
            ClarificationAnswer::Selected(0)
        );
        assert_eq!(
            parse_clarification_response("2", &q),
            ClarificationAnswer::Selected(1)
        );
        assert_eq!(
            parse_clarification_response("3", &q),
            ClarificationAnswer::Selected(2)
        );

        // Out of range
        match parse_clarification_response("4", &q) {
            ClarificationAnswer::Invalid(_) => {}
            other => panic!("Expected Invalid, got {:?}", other),
        }
    }

    #[test]
    fn parse_clarification_response_default() {
        let q = ClarificationQuestion {
            question: "test".into(),
            options: vec!["A".into(), "B".into()],
            default: Some(1),
            category: ClarificationCategory::Technical,
        };

        // Empty input uses default
        assert_eq!(
            parse_clarification_response("", &q),
            ClarificationAnswer::Selected(1)
        );
    }

    #[test]
    fn parse_clarification_response_freeform() {
        let q = ClarificationQuestion {
            question: "test".into(),
            options: vec!["A".into(), "B".into()],
            default: None,
            category: ClarificationCategory::Technical,
        };

        // Non-numeric input is freeform
        assert_eq!(
            parse_clarification_response("use redis instead", &q),
            ClarificationAnswer::Freeform("use redis instead".into())
        );
    }

    #[test]
    fn pending_clarifications_workflow() {
        let mut pc = PendingClarifications {
            questions: vec![
                ClarificationQuestion {
                    question: "Q1?".into(),
                    options: vec!["A".into()],
                    default: None,
                    category: ClarificationCategory::Technical,
                },
                ClarificationQuestion {
                    question: "Q2?".into(),
                    options: vec!["B".into()],
                    default: None,
                    category: ClarificationCategory::Technical,
                },
            ],
            answers: vec![],
        };

        assert!(!pc.is_complete());
        assert_eq!(
            pc.next_question().map(|q| &q.question),
            Some(&"Q1?".to_string())
        );

        assert!(!pc.record_answer("Answer 1".into()));
        assert_eq!(
            pc.next_question().map(|q| &q.question),
            Some(&"Q2?".to_string())
        );

        assert!(pc.record_answer("Answer 2".into()));
        assert!(pc.is_complete());
        assert!(pc.next_question().is_none());
    }

    #[test]
    fn pending_clarifications_format_for_prompt() {
        let pc = PendingClarifications {
            questions: vec![ClarificationQuestion {
                question: "Database?".into(),
                options: vec![],
                default: None,
                category: ClarificationCategory::Technical,
            }],
            answers: vec!["PostgreSQL".into()],
        };

        let formatted = pc.format_for_prompt();
        assert!(formatted.contains("Q: Database?"));
        assert!(formatted.contains("A: PostgreSQL"));
    }

    #[test]
    fn detect_clarification_json_format() {
        let llm_text = r#"I need some clarification:
[{"question":"Which auth method?","options":["JWT","Session"],"default":0,"category":"technical"}]
"#;

        let questions = detect_clarification_questions(llm_text);
        assert!(questions.is_some());
        let qs = questions.unwrap();
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].question, "Which auth method?");
        assert_eq!(qs[0].options, vec!["JWT", "Session"]);
    }

    /// Detects clarification when the model wraps questions in a pretty-printed JSON array
    /// inside a markdown code fence labeled `json` (common in plan mode).
    #[test]
    fn detect_clarification_pretty_json_in_fence() {
        let llm_text = r#"Here are a few questions:

```json
[
  {
    "question": "用户输入的类型是什么？",
    "options": ["文本输入", "鼠标/触摸交互", "两者都要"],
    "default": 2,
    "category": "scope"
  },
  {
    "question": "期望的动态效果风格是什么？",
    "options": ["粒子特效", "流体/波浪动画"],
    "default": 0,
    "category": "approach"
  }
]
```
"#;

        let questions = detect_clarification_questions(llm_text);
        assert!(questions.is_some());
        let qs = questions.unwrap();
        assert_eq!(qs.len(), 2);
        assert!(qs[0].question.contains("用户输入"));
        assert_eq!(qs[0].default, Some(2));
        assert_eq!(qs[1].category, ClarificationCategory::Approach);
    }

    #[test]
    fn detect_clarification_marker_format() {
        let llm_text = "Before proceeding, I need to know:\nCLARIFICATION: Should we support SSO?";

        let questions = detect_clarification_questions(llm_text);
        assert!(questions.is_some());
        let qs = questions.unwrap();
        assert_eq!(qs.len(), 1);
        assert!(qs[0].question.contains("SSO"));
        // Marker format creates yes/no options
        assert_eq!(qs[0].options, vec!["Yes", "No"]);
    }

    #[test]
    fn detect_clarification_no_questions() {
        let llm_text = "Here's the plan:\n{\"subtasks\":[]}";
        assert!(detect_clarification_questions(llm_text).is_none());
    }

    // ═══════════════════════ Plan Persistence Tests ══════════════════════════

    #[test]
    fn generate_plan_id_from_goal() {
        let id1 = PlanModeState::generate_plan_id("Add user authentication");
        assert!(id1.starts_with("add-user-authentication-"), "got: {}", id1);
        assert!(
            id1.len() > 25 && id1.len() < 40,
            "reasonable length: {}",
            id1
        );

        // Empty goal
        let id2 = PlanModeState::generate_plan_id("");
        assert!(id2.starts_with("plan-"), "empty goal: {}", id2);

        // Special characters get filtered
        let id3 = PlanModeState::generate_plan_id("Fix bug #123 & add tests!");
        assert!(!id3.contains('#'));
        assert!(!id3.contains('&'));
        assert!(!id3.contains('!'));
    }

    #[test]
    fn save_and_load_plan_to_temp_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("test-plan.json");

        let ctx = ProjectContext {
            root: "/test".into(),
            entry_points: vec!["Cargo.toml".into()],
            ..Default::default()
        };
        let mut state = PlanModeState::new("Test goal".into(), ctx);
        state.set_plan(TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "a".into(),
                    title: "A".into(),
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "b".into(),
                    title: "B".into(),
                    depends_on: vec!["a".into()],
                    ..Default::default()
                },
            ],
            notes: Some("Test notes".into()),
        });

        // Save
        state.save_to_file(&path).expect("save should succeed");
        assert!(path.exists());

        // Load
        let loaded = PlanModeState::load_from_file(&path).expect("load should succeed");
        assert_eq!(loaded.goal, "Test goal");
        assert_eq!(loaded.plan.subtasks.len(), 2);
        assert_eq!(loaded.plan.notes, Some("Test notes".into()));
    }

    #[test]
    fn list_saved_plans_returns_sorted() {
        // This test is more of a sanity check - actual file listing depends on temp dir
        let plans = vec![
            SavedPlanInfo {
                name: "plan-completed".into(),
                goal: "Completed".into(),
                progress_pct: 100,
                subtask_count: 3,
                status: "completed".into(),
            },
            SavedPlanInfo {
                name: "plan-in-progress".into(),
                goal: "In Progress".into(),
                progress_pct: 50,
                subtask_count: 4,
                status: "in_progress".into(),
            },
            SavedPlanInfo {
                name: "plan-pending".into(),
                goal: "Pending".into(),
                progress_pct: 0,
                subtask_count: 2,
                status: "pending".into(),
            },
        ];

        let formatted = format_plan_list(&plans);
        // In progress should appear with ▶
        assert!(formatted.contains("▶ plan-in-progress"));
        // Completed should appear with ✓
        assert!(formatted.contains("✓ plan-completed"));
        // Pending should appear with ○
        assert!(formatted.contains("○ plan-pending"));
    }

    // ═══════════════════════ Tree Progress Display Tests ═════════════════════

    #[test]
    fn format_plan_tree_compact() {
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "a".into(),
                    title: "Task A".into(),
                    status: TaskStatus::Completed,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "b".into(),
                    title: "Task B".into(),
                    status: TaskStatus::InProgress,
                    effort: Some("medium".into()),
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "c".into(),
                    title: "Task C".into(),
                    depends_on: vec!["b".into()],
                    ..Default::default()
                },
            ],
            notes: None,
        };

        let tree = format_plan_tree(&plan, TreeViewMode::Compact);
        assert!(tree.contains("Progress"), "should have progress header");
        assert!(tree.contains("33%"), "should show 33% (1/3)");
        assert!(tree.contains("✓ Task A"), "completed task");
        assert!(tree.contains("▶ Task B"), "in progress task");
        assert!(tree.contains("[M]"), "effort badge");
        assert!(tree.contains("Task C"), "pending task");
    }

    #[test]
    fn format_plan_tree_detailed() {
        let plan = TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "setup".into(),
                title: "Setup database".into(),
                description: Some("Configure PostgreSQL connection".into()),
                files: vec!["src/db.rs".into()],
                ..Default::default()
            }],
            notes: None,
        };

        let tree = format_plan_tree(&plan, TreeViewMode::Detailed);
        assert!(tree.contains("[setup]"), "should show ID");
        assert!(tree.contains("PostgreSQL"), "should show description");
        assert!(tree.contains("📁 src/db.rs"), "should show files");
    }

    #[test]
    fn format_plan_tree_progress_bars() {
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "a".into(),
                    title: "Done".into(),
                    status: TaskStatus::Completed,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "b".into(),
                    title: "Pending".into(),
                    ..Default::default()
                },
            ],
            notes: None,
        };

        let tree = format_plan_tree(&plan, TreeViewMode::Progress);
        // Progress mode shows mini bars per task
        assert!(tree.contains("▓"), "completed task should have filled bar");
        assert!(tree.contains("░"), "pending task should have empty bar");
    }

    #[test]
    fn format_status_line_compact() {
        let plan = TaskPlan {
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
                    status: TaskStatus::Completed,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "c".into(),
                    title: "C".into(),
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "d".into(),
                    title: "D".into(),
                    ..Default::default()
                },
            ],
            notes: None,
        };

        let line = format_status_line(&plan);
        assert!(line.contains("50%"));
        assert!(line.contains("2/4"));
    }

    #[test]
    fn progress_bar_rendering() {
        // 0%
        let bar0 = progress_bar(0, 10);
        assert_eq!(bar0, "[░░░░░░░░░░]");

        // 50%
        let bar50 = progress_bar(50, 10);
        assert_eq!(bar50, "[█████░░░░░]");

        // 100%
        let bar100 = progress_bar(100, 10);
        assert_eq!(bar100, "[██████████]");
    }

    #[test]
    fn should_suggest_plan_mode_multi_step() {
        // Multi-step with large scope
        assert!(should_suggest_plan_mode("implement authentication and then add tests").is_some());
        assert!(
            should_suggest_plan_mode("refactor the module and then migrate the database").is_some()
        );
        assert!(should_suggest_plan_mode("重构代码，然后添加测试").is_some());
    }

    #[test]
    fn should_suggest_plan_mode_impl_and_test() {
        // Implementation + testing pattern
        assert!(
            should_suggest_plan_mode("implement the API endpoint and write tests for it").is_some()
        );
        assert!(
            should_suggest_plan_mode("add user authentication with comprehensive tests").is_some()
        );
    }

    #[test]
    fn should_suggest_plan_mode_large_scope() {
        // Large scope with multiple files
        assert!(should_suggest_plan_mode("refactor multiple files in the auth module").is_some());
        assert!(
            should_suggest_plan_mode("migrate all services to the new database schema").is_some()
        );
    }

    #[test]
    fn should_suggest_plan_mode_short_input_skipped() {
        // Short inputs should not trigger plan mode
        assert!(should_suggest_plan_mode("fix bug").is_none());
        assert!(should_suggest_plan_mode("add test").is_none());
        assert!(should_suggest_plan_mode("what is this").is_none());
    }

    #[test]
    fn should_suggest_plan_mode_simple_questions_skipped() {
        // Simple questions should not trigger
        assert!(should_suggest_plan_mode("how does this work").is_none());
        assert!(should_suggest_plan_mode("explain the code").is_none());
    }

    #[test]
    fn detect_replan_deadlock() {
        use astra_services::task_orchestrator::{SubtaskPlan, TaskPlan, TaskStatus};

        // Create plan with circular dependency (a -> b -> a)
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "a".to_string(),
                    title: "Task A".to_string(),
                    depends_on: vec!["b".to_string()],
                    status: TaskStatus::Pending,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "b".to_string(),
                    title: "Task B".to_string(),
                    depends_on: vec!["a".to_string()],
                    status: TaskStatus::Pending,
                    ..Default::default()
                },
            ],
            notes: None,
        };

        let result = detect_replan_needed(&plan, 0, &[]);
        assert!(result.is_some());
        let suggestion = result.unwrap();
        assert!(matches!(
            suggestion.reason,
            ReplanReason::DependencyDeadlock { .. }
        ));
    }

    #[test]
    fn detect_replan_failed_subtask() {
        use astra_services::task_orchestrator::{SubtaskPlan, TaskPlan, TaskStatus};

        let plan = TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "test".to_string(),
                title: "Test task".to_string(),
                status: TaskStatus::Pending,
                ..Default::default()
            }],
            notes: None,
        };

        let failed = vec![("test", "compilation error")];
        let result = detect_replan_needed(&plan, 0, &failed);
        assert!(result.is_some());
        let suggestion = result.unwrap();
        assert!(matches!(
            suggestion.reason,
            ReplanReason::SubtaskFailed { .. }
        ));
    }

    #[test]
    fn detect_replan_none_for_healthy_plan() {
        use astra_services::task_orchestrator::{SubtaskPlan, TaskPlan, TaskStatus};

        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "first".to_string(),
                    title: "First task".to_string(),
                    depends_on: vec![],
                    status: TaskStatus::Pending,
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "second".to_string(),
                    title: "Second task".to_string(),
                    depends_on: vec!["first".to_string()],
                    status: TaskStatus::Pending,
                    ..Default::default()
                },
            ],
            notes: None,
        };

        // Healthy plan with no issues
        let result = detect_replan_needed(&plan, 1, &[]);
        assert!(result.is_none());
    }

    // ═══════════════════════ Execution Timeline Tests ════════════════════════

    #[test]
    fn timeline_records_events() {
        let mut timeline = ExecutionTimeline::default();

        timeline.plan_created(3);
        timeline.execution_started(true);
        timeline.subtask_started("s1", "Setup");
        timeline.subtask_completed("s1", "Setup", 60);
        timeline.discovery("Found existing config");

        assert_eq!(timeline.events.len(), 5);
        assert_eq!(timeline.completed_subtask_count(), 1);
        assert!(timeline.start_timestamp.is_some());
    }

    #[test]
    fn timeline_format_display() {
        let mut timeline = ExecutionTimeline::default();

        timeline.plan_created(2);
        timeline.subtask_completed("s1", "First task", 30);
        timeline.subtask_failed("s2", "Second task", "timeout");

        let display = timeline.format_display();
        assert!(display.contains("📋"), "should have plan created icon");
        assert!(display.contains("✓"), "should have completed icon");
        assert!(display.contains("✗"), "should have failed icon");
        assert!(display.contains("First task"));
        assert!(display.contains("Second task"));
        assert!(display.contains("timeout"));
    }

    #[test]
    fn timeline_counts() {
        let mut timeline = ExecutionTimeline::default();

        timeline.subtask_completed("s1", "A", 10);
        timeline.subtask_completed("s2", "B", 20);
        timeline.subtask_failed("s3", "C", "error");
        timeline.record(TimelineEventKind::Replan {
            reason: "user request".into(),
            changes: "added task".into(),
        });
        timeline.record(TimelineEventKind::Replan {
            reason: "conflict".into(),
            changes: "modified".into(),
        });

        assert_eq!(timeline.completed_subtask_count(), 2);
        assert_eq!(timeline.failed_subtask_count(), 1);
        assert_eq!(timeline.replan_count(), 2);
    }

    #[test]
    fn timeline_git_commit() {
        let mut timeline = ExecutionTimeline::default();

        timeline.git_commit("abc1234567890", "feat: add auth");

        let display = timeline.format_display();
        assert!(display.contains("📦"), "should have commit icon");
        assert!(display.contains("abc1234"), "should have short hash");
        assert!(display.contains("feat: add auth"));
    }

    #[test]
    fn timeline_plan_completed() {
        let mut timeline = ExecutionTimeline::default();

        timeline.plan_created(2);
        // Simulate some delay (we can't actually delay in tests, but the logic works)
        timeline.plan_completed(true);

        assert!(timeline.end_timestamp.is_some());

        let display = timeline.format_display();
        assert!(display.contains("✅"), "should have success icon");
    }

    // ═══════════════════════ extract_json Tests ════════════════════════

    #[test]
    fn extract_json_from_json_block() {
        let input = "Here:\n```json\n{\"key\": \"val\"}\n```\nDone.";
        assert_eq!(extract_json(input), r#"{"key": "val"}"#);
    }

    #[test]
    fn extract_json_from_plain_block() {
        let input = "Here:\n```\n{\"key\": 1}\n```\nDone.";
        assert_eq!(extract_json(input), r#"{"key": 1}"#);
    }

    #[test]
    fn extract_json_from_plain_block_with_lang() {
        let input = "Here:\n```rust\n{\"key\": 1}\n```\nDone.";
        assert_eq!(extract_json(input), r#"{"key": 1}"#);
    }

    #[test]
    fn extract_json_raw_array() {
        let input = r#"[{"q": "how?"}, {"q": "what?"}]"#;
        assert_eq!(extract_json(input), input);
    }

    #[test]
    fn extract_json_raw_object() {
        let input = "Some text {\"key\": \"val\"} more text";
        assert_eq!(extract_json(input), r#"{"key": "val"}"#);
    }

    #[test]
    fn extract_json_no_json() {
        let input = "Just some text";
        assert_eq!(extract_json(input), input);
    }

    #[test]
    fn extract_json_empty_string() {
        assert_eq!(extract_json(""), "");
    }

    // ═══════════════════════ parse_execution_confirmation Tests ════════════════════════

    #[test]
    fn execution_confirmation_yes_variants() {
        assert_eq!(
            parse_execution_confirmation("y"),
            ExecutionConfirmation::Execute
        );
        assert_eq!(
            parse_execution_confirmation("yes"),
            ExecutionConfirmation::Execute
        );
        assert_eq!(
            parse_execution_confirmation("go"),
            ExecutionConfirmation::Execute
        );
        assert_eq!(
            parse_execution_confirmation("execute"),
            ExecutionConfirmation::Execute
        );
        assert_eq!(
            parse_execution_confirmation("run"),
            ExecutionConfirmation::Execute
        );
    }

    #[test]
    fn execution_confirmation_chinese() {
        assert_eq!(
            parse_execution_confirmation("确认"),
            ExecutionConfirmation::Execute
        );
        assert_eq!(
            parse_execution_confirmation("是"),
            ExecutionConfirmation::Execute
        );
        assert_eq!(
            parse_execution_confirmation("逐步"),
            ExecutionConfirmation::StepByStep
        );
        assert_eq!(
            parse_execution_confirmation("编辑"),
            ExecutionConfirmation::Edit
        );
        assert_eq!(
            parse_execution_confirmation("修改"),
            ExecutionConfirmation::Edit
        );
    }

    #[test]
    fn execution_confirmation_step_by_step() {
        assert_eq!(
            parse_execution_confirmation("s"),
            ExecutionConfirmation::StepByStep
        );
        assert_eq!(
            parse_execution_confirmation("step"),
            ExecutionConfirmation::StepByStep
        );
        assert_eq!(
            parse_execution_confirmation("step-by-step"),
            ExecutionConfirmation::StepByStep
        );
    }

    #[test]
    fn execution_confirmation_edit() {
        assert_eq!(
            parse_execution_confirmation("e"),
            ExecutionConfirmation::Edit
        );
        assert_eq!(
            parse_execution_confirmation("edit"),
            ExecutionConfirmation::Edit
        );
        assert_eq!(
            parse_execution_confirmation("modify"),
            ExecutionConfirmation::Edit
        );
    }

    #[test]
    fn execution_confirmation_cancel_fallback() {
        assert_eq!(
            parse_execution_confirmation("n"),
            ExecutionConfirmation::Cancel
        );
        assert_eq!(
            parse_execution_confirmation("anything else"),
            ExecutionConfirmation::Cancel
        );
        assert_eq!(
            parse_execution_confirmation(""),
            ExecutionConfirmation::Cancel
        );
    }

    #[test]
    fn execution_confirmation_case_insensitive() {
        assert_eq!(
            parse_execution_confirmation("YES"),
            ExecutionConfirmation::Execute
        );
        assert_eq!(
            parse_execution_confirmation("  Go  "),
            ExecutionConfirmation::Execute
        );
    }

    // ═══════════════════════ parse_subtask_confirmation Tests ════════════════════════

    #[test]
    fn subtask_confirmation_execute() {
        assert_eq!(
            parse_subtask_confirmation("y"),
            SubtaskConfirmation::Execute
        );
        assert_eq!(
            parse_subtask_confirmation("yes"),
            SubtaskConfirmation::Execute
        );
        assert_eq!(parse_subtask_confirmation(""), SubtaskConfirmation::Execute);
    }

    #[test]
    fn subtask_confirmation_skip() {
        assert_eq!(parse_subtask_confirmation("s"), SubtaskConfirmation::Skip);
        assert_eq!(
            parse_subtask_confirmation("skip"),
            SubtaskConfirmation::Skip
        );
        assert_eq!(
            parse_subtask_confirmation("跳过"),
            SubtaskConfirmation::Skip
        );
    }

    #[test]
    fn subtask_confirmation_quit_fallback() {
        assert_eq!(parse_subtask_confirmation("q"), SubtaskConfirmation::Quit);
        assert_eq!(
            parse_subtask_confirmation("quit"),
            SubtaskConfirmation::Quit
        );
        assert_eq!(
            parse_subtask_confirmation("anything"),
            SubtaskConfirmation::Quit
        );
    }

    // ═══════════════════════ parse_plan_paused_user_line Tests ════════════════════════

    #[test]
    fn paused_line_empty() {
        assert_eq!(parse_plan_paused_user_line(""), None);
        assert_eq!(parse_plan_paused_user_line("   "), None);
    }

    #[test]
    fn paused_line_correction() {
        let r = parse_plan_paused_user_line("correct Fix the import order");
        assert_eq!(
            r,
            Some(PlanPausedUserAction::Correction(
                "Fix the import order".to_string()
            ))
        );
    }

    #[test]
    fn paused_line_note_correction() {
        let r = parse_plan_paused_user_line("note use async version");
        assert_eq!(
            r,
            Some(PlanPausedUserAction::Correction(
                "use async version".to_string()
            ))
        );
    }

    #[test]
    fn paused_line_adjust_correction() {
        let r = parse_plan_paused_user_line("adjust something");
        assert_eq!(
            r,
            Some(PlanPausedUserAction::Correction("something".to_string()))
        );
    }

    #[test]
    fn paused_line_clear_corrections() {
        assert_eq!(
            parse_plan_paused_user_line("correct clear"),
            Some(PlanPausedUserAction::ClearCorrections)
        );
        assert_eq!(
            parse_plan_paused_user_line("note clear"),
            Some(PlanPausedUserAction::ClearCorrections)
        );
        assert_eq!(
            parse_plan_paused_user_line("adjust clear"),
            Some(PlanPausedUserAction::ClearCorrections)
        );
    }

    #[test]
    fn paused_line_rewind_numeric() {
        let r = parse_plan_paused_user_line("rewind 3");
        assert_eq!(
            r,
            Some(PlanPausedUserAction::Rewind(PlanRewindAnchor::OneBased(3)))
        );
    }

    #[test]
    fn paused_line_rewind_id_prefix() {
        let r = parse_plan_paused_user_line("rewind setup");
        assert_eq!(
            r,
            Some(PlanPausedUserAction::Rewind(PlanRewindAnchor::IdPrefix(
                "setup".to_string()
            )))
        );
    }

    #[test]
    fn paused_line_restart_from() {
        let r = parse_plan_paused_user_line("restart from 2");
        assert_eq!(
            r,
            Some(PlanPausedUserAction::Rewind(PlanRewindAnchor::OneBased(2)))
        );
    }

    #[test]
    fn paused_line_redo_from() {
        let r = parse_plan_paused_user_line("redo from auth");
        assert_eq!(
            r,
            Some(PlanPausedUserAction::Rewind(PlanRewindAnchor::IdPrefix(
                "auth".to_string()
            )))
        );
    }

    #[test]
    fn paused_line_rewind_zero_ignored() {
        assert_eq!(parse_plan_paused_user_line("rewind 0"), None);
    }

    #[test]
    fn paused_line_rewind_empty_rest() {
        assert_eq!(parse_plan_paused_user_line("rewind "), None);
    }

    #[test]
    fn paused_line_correction_empty_rest() {
        assert_eq!(parse_plan_paused_user_line("correct "), None);
    }

    #[test]
    fn paused_line_normal_text_returns_none() {
        assert_eq!(parse_plan_paused_user_line("hello world"), None);
    }

    // ═══════════════════════ resolve_rewind_start_index Tests ════════════════════════

    #[test]
    fn rewind_index_one_based_valid() {
        use astra_services::task_orchestrator::{SubtaskPlan, TaskPlan};
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
            ],
            notes: None,
        };
        assert_eq!(
            resolve_rewind_start_index(&plan, &PlanRewindAnchor::OneBased(1)),
            Ok(0)
        );
        assert_eq!(
            resolve_rewind_start_index(&plan, &PlanRewindAnchor::OneBased(2)),
            Ok(1)
        );
    }

    #[test]
    fn rewind_index_one_based_out_of_range() {
        use astra_services::task_orchestrator::{SubtaskPlan, TaskPlan};
        let plan = TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "a".into(),
                title: "A".into(),
                ..Default::default()
            }],
            notes: None,
        };
        assert!(resolve_rewind_start_index(&plan, &PlanRewindAnchor::OneBased(0)).is_err());
        assert!(resolve_rewind_start_index(&plan, &PlanRewindAnchor::OneBased(5)).is_err());
    }

    #[test]
    fn rewind_index_id_prefix_exact_match() {
        use astra_services::task_orchestrator::{SubtaskPlan, TaskPlan};
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "setup".into(),
                    title: "Setup".into(),
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "build".into(),
                    title: "Build".into(),
                    ..Default::default()
                },
            ],
            notes: None,
        };
        assert_eq!(
            resolve_rewind_start_index(&plan, &PlanRewindAnchor::IdPrefix("build".into())),
            Ok(1)
        );
    }

    #[test]
    fn rewind_index_id_prefix_ambiguous() {
        use astra_services::task_orchestrator::{SubtaskPlan, TaskPlan};
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "test-unit".into(),
                    title: "Unit".into(),
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "test-integration".into(),
                    title: "Integ".into(),
                    ..Default::default()
                },
            ],
            notes: None,
        };
        let r = resolve_rewind_start_index(&plan, &PlanRewindAnchor::IdPrefix("test".into()));
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("ambiguous"));
    }

    #[test]
    fn rewind_index_id_prefix_no_match() {
        use astra_services::task_orchestrator::{SubtaskPlan, TaskPlan};
        let plan = TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "a".into(),
                title: "A".into(),
                ..Default::default()
            }],
            notes: None,
        };
        assert!(
            resolve_rewind_start_index(&plan, &PlanRewindAnchor::IdPrefix("zzz".into())).is_err()
        );
    }

    #[test]
    fn rewind_index_id_prefix_empty() {
        use astra_services::task_orchestrator::{SubtaskPlan, TaskPlan};
        let plan = TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "a".into(),
                title: "A".into(),
                ..Default::default()
            }],
            notes: None,
        };
        assert!(resolve_rewind_start_index(&plan, &PlanRewindAnchor::IdPrefix("".into())).is_err());
    }

    // ═══════════════════════ rewind_plan_from_subtask Tests ════════════════════════

    #[test]
    fn rewind_resets_from_start_index() {
        use astra_services::task_orchestrator::{SubtaskPlan, TaskPlan, TaskStatus};
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
        let n = rewind_plan_from_subtask(&mut plan, 1);
        assert_eq!(n, 1); // only "b" was reset (c was already pending)
        assert_eq!(plan.subtasks[0].status, TaskStatus::Completed); // unchanged
        assert_eq!(plan.subtasks[1].status, TaskStatus::Pending);
        assert_eq!(plan.subtasks[2].status, TaskStatus::Pending);
    }

    #[test]
    fn rewind_all_from_zero() {
        use astra_services::task_orchestrator::{SubtaskPlan, TaskPlan, TaskStatus};
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
                    status: TaskStatus::Failed,
                    ..Default::default()
                },
            ],
            notes: None,
        };
        let n = rewind_plan_from_subtask(&mut plan, 0);
        assert_eq!(n, 2);
    }

    #[test]
    fn rewind_past_end_resets_nothing() {
        use astra_services::task_orchestrator::{SubtaskPlan, TaskPlan, TaskStatus};
        let mut plan = TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "a".into(),
                title: "A".into(),
                status: TaskStatus::Completed,
                ..Default::default()
            }],
            notes: None,
        };
        let n = rewind_plan_from_subtask(&mut plan, 10);
        assert_eq!(n, 0);
    }

    // ═══════════════════════ analyze_parallelism Tests ════════════════════════

    #[test]
    fn parallelism_no_ready_subtasks() {
        use astra_services::task_orchestrator::{SubtaskPlan, TaskPlan, TaskStatus};
        let plan = TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "a".into(),
                title: "A".into(),
                status: TaskStatus::Completed,
                ..Default::default()
            }],
            notes: None,
        };
        let r = analyze_parallelism(&plan);
        assert!(r.groups.is_empty());
        assert!(r.conflicts.is_empty());
    }

    #[test]
    fn parallelism_single_ready() {
        use astra_services::task_orchestrator::{SubtaskPlan, TaskPlan, TaskStatus};
        let plan = TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "a".into(),
                title: "A".into(),
                status: TaskStatus::Pending,
                ..Default::default()
            }],
            notes: None,
        };
        let r = analyze_parallelism(&plan);
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.groups[0], vec!["a".to_string()]);
    }

    #[test]
    fn parallelism_no_conflicts() {
        use astra_services::task_orchestrator::{SubtaskPlan, TaskPlan, TaskStatus};
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "a".into(),
                    title: "A".into(),
                    status: TaskStatus::Pending,
                    files: vec!["a.rs".into()],
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "b".into(),
                    title: "B".into(),
                    status: TaskStatus::Pending,
                    files: vec!["b.rs".into()],
                    ..Default::default()
                },
            ],
            notes: None,
        };
        let r = analyze_parallelism(&plan);
        assert_eq!(r.groups.len(), 1); // all in one group
        assert_eq!(r.conflicts.len(), 0);
    }

    #[test]
    fn parallelism_with_file_conflict() {
        use astra_services::task_orchestrator::{SubtaskPlan, TaskPlan, TaskStatus};
        let plan = TaskPlan {
            subtasks: vec![
                SubtaskPlan {
                    id: "a".into(),
                    title: "A".into(),
                    status: TaskStatus::Pending,
                    files: vec!["shared.rs".into()],
                    ..Default::default()
                },
                SubtaskPlan {
                    id: "b".into(),
                    title: "B".into(),
                    status: TaskStatus::Pending,
                    files: vec!["shared.rs".into()],
                    ..Default::default()
                },
            ],
            notes: None,
        };
        let r = analyze_parallelism(&plan);
        assert_eq!(r.conflicts.len(), 1);
        assert_eq!(r.conflicts[0].shared_files, vec!["shared.rs".to_string()]);
        assert_eq!(r.groups.len(), 2); // separated into different groups
    }

    // ── Atomic save / load tests ────────────────────────────────────────

    #[test]
    fn atomic_save_creates_checksummed_wrapper() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

        let ps = PlanModeState::new("Checksum test".into(), ProjectContext::default());
        ps.save_to_file(&path).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let wrapper: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(wrapper.get("_checksum").is_some());
        assert!(wrapper.get("data").is_some());
    }

    #[test]
    fn atomic_save_load_roundtrip_with_checksum() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

        let mut ps = PlanModeState::new("Roundtrip".into(), ProjectContext::default());
        ps.set_plan(TaskPlan {
            subtasks: vec![SubtaskPlan {
                id: "s1".into(),
                title: "Step one".into(),
                ..Default::default()
            }],
            notes: None,
        });
        ps.save_to_file(&path).unwrap();

        let loaded = PlanModeState::load_from_file(&path).unwrap();
        assert_eq!(loaded.goal, "Roundtrip");
        assert_eq!(loaded.plan.subtasks.len(), 1);
    }

    #[test]
    fn load_legacy_format_still_works() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

        let ps = PlanModeState::new("Legacy".into(), ProjectContext::default());
        let json = serde_json::to_string_pretty(&ps).unwrap();
        std::fs::write(&path, json).unwrap();

        let loaded = PlanModeState::load_from_file(&path).unwrap();
        assert_eq!(loaded.goal, "Legacy");
    }

    #[test]
    fn save_with_backup_creates_bak_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("plan.json");

        let ps1 = PlanModeState::new("Version 1".into(), ProjectContext::default());
        ps1.save_to_file(&path).unwrap();

        let ps2 = PlanModeState::new("Version 2".into(), ProjectContext::default());
        ps2.save_with_backup(&path).unwrap();

        let backup = path.with_extension("json.bak");
        assert!(backup.exists());

        let loaded_main = PlanModeState::load_from_file(&path).unwrap();
        assert_eq!(loaded_main.goal, "Version 2");
    }

    #[test]
    fn crc32_hash_consistent() {
        let data = b"hello world";
        let h1 = crc32_hash(data);
        let h2 = crc32_hash(data);
        assert_eq!(h1, h2);
        assert_ne!(h1, 0);
    }

    #[test]
    fn crc32_hash_different_for_different_data() {
        let h1 = crc32_hash(b"hello");
        let h2 = crc32_hash(b"world");
        assert_ne!(h1, h2);
    }

    // ── validate_plan_id security regression tests ──────────────────────────

    #[test]
    fn validate_plan_id_rejects_empty() {
        assert!(PlanModeState::validate_plan_id("").is_err());
    }

    #[test]
    fn validate_plan_id_rejects_path_traversal() {
        let malicious = ["../etc/passwd", "../../secret", "foo/../bar", ".."];
        for id in &malicious {
            let err = PlanModeState::validate_plan_id(id).unwrap_err();
            assert!(
                matches!(err, PlanLoadError::InvalidId(_)),
                "should reject {id}: {err}"
            );
        }
    }

    #[test]
    fn validate_plan_id_rejects_slashes_and_special_chars() {
        let bad = [
            "foo/bar",
            "foo\\bar",
            "plan.json",
            "id with space",
            "a;b",
            "a&b",
        ];
        for id in &bad {
            assert!(
                PlanModeState::validate_plan_id(id).is_err(),
                "should reject {id}"
            );
        }
    }

    #[test]
    fn validate_plan_id_accepts_valid_ids() {
        let good = ["abc", "plan-123", "my_plan_v2", "ABC-xyz_01"];
        for id in &good {
            assert!(
                PlanModeState::validate_plan_id(id).is_ok(),
                "should accept {id}"
            );
        }
    }

    // ── CRC32 save/load round-trip regression test ──────────────────────────

    #[test]
    fn save_load_roundtrip_crc32_matches() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("roundtrip.json");

        let ps = PlanModeState::new("CRC32 roundtrip test".into(), ProjectContext::default());
        ps.save_to_file(&path).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let wrapper: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let checksum_str = wrapper["_checksum"].as_str().unwrap();
        let inner = &wrapper["data"];

        let expected = u32::from_str_radix(checksum_str, 16).unwrap();
        let actual = crc32_hash(inner.to_string().as_bytes());
        assert_eq!(
            expected, actual,
            "CRC32 mismatch: save used different serialization than load would"
        );

        let loaded = PlanModeState::load_from_file(&path).unwrap();
        assert_eq!(loaded.goal, "CRC32 roundtrip test");
    }

    #[test]
    fn save_load_roundtrip_with_complex_plan() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("complex.json");

        let mut ps = PlanModeState::new(
            "Plan with unicode 中文 & special chars: <>&\"".into(),
            ProjectContext::default(),
        );
        ps.add_turn("user input", "response with\nnewlines");

        ps.save_to_file(&path).unwrap();
        let loaded = PlanModeState::load_from_file(&path).unwrap();
        assert_eq!(loaded.goal, ps.goal);
        assert_eq!(loaded.history.len(), 1);
    }
}

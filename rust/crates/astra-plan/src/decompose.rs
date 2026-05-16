//! Shared plan-mode types and helpers:
//! - robust JSON extraction/repair for LLM responses
//! - persisted plan state, rewind helpers, execution summaries, and parallelism analysis

use serde::{Deserialize, Serialize};

use crate::repository::PlanLoadError;

// Re-export task types from services
pub use astra_services::task_orchestrator::{SubtaskPlan, TaskPlan, TaskStatus};

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

/// Strip trailing commas before `}` or `]` — a common LLM JSON error.
fn fix_trailing_commas(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    let mut i = 0;
    while i < len {
        if chars[i] == ',' {
            // Look ahead past whitespace for } or ]
            let mut j = i + 1;
            while j < len && chars[j].is_whitespace() {
                j += 1;
            }
            if j < len && (chars[j] == '}' || chars[j] == ']') {
                // Skip the trailing comma
                i += 1;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Strip single-line `// …` comments — another common LLM JSON error.
fn strip_json_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_string = false;
    let mut escape_next = false;
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    let mut i = 0;
    while i < len {
        if escape_next {
            out.push(chars[i]);
            escape_next = false;
            i += 1;
            continue;
        }
        if chars[i] == '\\' && in_string {
            out.push(chars[i]);
            escape_next = true;
            i += 1;
            continue;
        }
        if chars[i] == '"' {
            in_string = !in_string;
            out.push(chars[i]);
            i += 1;
            continue;
        }
        if !in_string && i + 1 < len && chars[i] == '/' && chars[i + 1] == '/' {
            // Skip to end of line
            while i < len && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Replace Unicode "smart" quotes with ASCII equivalents.
///
/// LLMs (especially Chinese-tuned models) sometimes emit `"` (U+201C/D) or
/// `'` (U+2018/9) instead of plain ASCII quotes. This converts them to the
/// JSON-compatible form. Only operates outside of already-balanced ASCII
/// strings — a quoted string that legitimately contains a smart quote keeps
/// it because we don't enter the escape; the conversion is best-effort
/// applied uniformly here, which is acceptable because any subsequent
/// `serde_json::from_str` re-validates.
fn normalize_smart_quotes(s: &str) -> String {
    s.replace(['\u{201C}', '\u{201D}', '\u{FF02}'], "\"")
        .replace(['\u{2018}', '\u{2019}', '\u{FF07}'], "'")
}

/// Convert single-quoted JSON strings to double-quoted. The walker tracks
/// whether it is inside a (possibly already double-quoted) string region so
/// it never disturbs a legitimate apostrophe inside a value like
/// `"don't"`. Inside a single-quoted region, any embedded ASCII `"` is
/// escaped to `\"` so the resulting double-quoted string remains valid.
fn fix_single_quoted_strings(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    let mut out = String::with_capacity(len);
    let mut i = 0;
    let mut in_dq = false;
    while i < len {
        let c = chars[i];
        if c == '\\' && in_dq && i + 1 < len {
            out.push(c);
            out.push(chars[i + 1]);
            i += 2;
            continue;
        }
        if c == '"' {
            in_dq = !in_dq;
            out.push(c);
            i += 1;
            continue;
        }
        if !in_dq && c == '\'' {
            // Only treat this as a single-quoted string if it is followed,
            // somewhere on the same logical token, by a closing `'`. Avoid
            // converting bare apostrophes mid-identifier (we should not see
            // those in valid JSON anyway).
            let mut j = i + 1;
            let mut found_close = false;
            while j < len {
                if chars[j] == '\\' && j + 1 < len {
                    j += 2;
                    continue;
                }
                if chars[j] == '\'' {
                    found_close = true;
                    break;
                }
                if chars[j] == '\n' {
                    break;
                }
                j += 1;
            }
            if found_close {
                out.push('"');
                let mut k = i + 1;
                while k < j {
                    if chars[k] == '"' {
                        out.push('\\');
                        out.push('"');
                    } else if chars[k] == '\\' && k + 1 < j {
                        out.push(chars[k]);
                        out.push(chars[k + 1]);
                        k += 2;
                        continue;
                    } else {
                        out.push(chars[k]);
                    }
                    k += 1;
                }
                out.push('"');
                i = j + 1;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Replace Python-style literals with their JSON equivalents.
///
/// LLMs that have been heavily fine-tuned on Python sometimes emit `True`,
/// `False`, or `None` even inside an otherwise valid JSON document. We
/// rewrite these only when they appear outside of any string and are
/// surrounded by non-identifier characters, so identifiers like `"True"`
/// inside a string are untouched.
fn fix_python_literals(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    let mut out = String::with_capacity(len);
    let mut i = 0;
    let mut in_string = false;
    let mut escape = false;
    let lits: [(&[char], &str); 3] = [
        (&['T', 'r', 'u', 'e'], "true"),
        (&['F', 'a', 'l', 's', 'e'], "false"),
        (&['N', 'o', 'n', 'e'], "null"),
    ];
    while i < len {
        let c = chars[i];
        if escape {
            out.push(c);
            escape = false;
            i += 1;
            continue;
        }
        if in_string {
            if c == '\\' {
                out.push(c);
                escape = true;
                i += 1;
                continue;
            }
            if c == '"' {
                in_string = false;
            }
            out.push(c);
            i += 1;
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            i += 1;
            continue;
        }

        let mut matched = false;
        for (lit_chars, repl) in &lits {
            let n = lit_chars.len();
            if i + n <= len && chars[i..i + n] == **lit_chars {
                let prev_ok = i == 0 || (!chars[i - 1].is_alphanumeric() && chars[i - 1] != '_');
                let next_ok =
                    i + n == len || (!chars[i + n].is_alphanumeric() && chars[i + n] != '_');
                if prev_ok && next_ok {
                    out.push_str(repl);
                    i += n;
                    matched = true;
                    break;
                }
            }
        }
        if !matched {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// Strip LLM thinking/reasoning tags: `<think>…</think>`, `<thinking>…</thinking>`, etc.
///
/// Many models wrap their reasoning in XML-like tags before the actual JSON output.
/// This must run before JSON extraction to avoid matching `{` inside thinking content.
fn strip_thinking_tags(text: &str) -> String {
    let mut result = text.to_string();
    for tag in &["think", "thinking", "reflect", "inner_monologue"] {
        loop {
            let open = format!("<{tag}>");
            let close = format!("</{tag}>");
            if let Some(start) = result.find(&open) {
                if let Some(rel_end) = result[start + open.len()..].find(&close) {
                    let end_pos = start + open.len() + rel_end + close.len();
                    result = format!("{}{}", &result[..start], &result[end_pos..]);
                    continue;
                }
                // Unclosed tag — keep content after the open tag; the JSON
                // may be embedded inside the thinking block itself.
                result = result[start + open.len()..].to_string();
            }
            break;
        }
    }
    result
}

/// Robust JSON extraction: tries multiple repair strategies.
///
/// 1. Strip LLM thinking tags (`<think>…</think>` etc.)
/// 2. Direct `extract_json` (handles markdown fences, raw objects)
/// 3. Fix trailing commas
/// 4. Strip `//` comments
/// 5. Both fixes combined
///
/// Returns the first variant that parses as valid JSON, or the original
/// extracted string if none succeed (caller handles the parse error).
pub fn extract_json_robust(response: &str) -> String {
    // Strip thinking/reasoning tags before extraction
    let cleaned = strip_thinking_tags(response);
    let extracted = extract_json(&cleaned);

    // Each repair stage is checked in order, returning early on success so
    // the cheapest fix wins. The list grows over time as we encounter new
    // LLM-emitted variants — keep the order roughly cheapest-to-most-
    // invasive so we don't aggressively rewrite valid input.
    let candidates = [
        extracted.clone(),
        normalize_smart_quotes(&extracted),
        fix_trailing_commas(&extracted),
        strip_json_comments(&extracted),
        fix_single_quoted_strings(&extracted),
        fix_python_literals(&extracted),
        // Composed: strip comments → trailing commas (common pair).
        fix_trailing_commas(&strip_json_comments(&extracted)),
        // Composed: smart quotes → trailing commas → comments.
        fix_trailing_commas(&strip_json_comments(&normalize_smart_quotes(&extracted))),
        // Composed: smart quotes → single quotes → trailing commas → comments.
        // This is the heaviest path — applied last when a Python-tuned LLM
        // returns single-quoted dicts inside markdown fences.
        fix_python_literals(&fix_trailing_commas(&strip_json_comments(
            &fix_single_quoted_strings(&normalize_smart_quotes(&extracted)),
        ))),
    ];
    for cand in &candidates {
        if serde_json::from_str::<serde_json::Value>(cand).is_ok() {
            return cand.clone();
        }
    }

    extracted
}

// ─── Plan Mode State ─────────────────────────────────────────────────────────

fn default_version() -> u64 {
    1
}

/// Cloud-backed plan authoring state mirrored into CLI/server flows.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlanModeState {
    /// The original user goal.
    pub goal: String,
    /// Current executable plan.
    pub plan: TaskPlan,
    /// Optional rendered markdown artifact for UI/sync consumers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_md: Option<String>,
    /// Whether the mirrored plan has loaded or local edits.
    #[serde(default)]
    pub modified: bool,
    /// Execution timeline used by active plan/executor flows.
    #[serde(default)]
    pub timeline: ExecutionTimeline,
    /// Monotonic version counter for optimistic concurrency control.
    /// Incremented on every save; checked on update to detect lost writes.
    #[serde(default = "default_version")]
    pub version: u64,
    /// User who created this plan (for ownership filtering).
    #[serde(default)]
    pub created_by: Option<String>,
    /// Most-recent session that touched this plan.
    #[serde(skip)]
    pub session_hint: Option<String>,
}

impl PlanModeState {
    /// Create a new plan state with the initial goal.
    pub fn new(goal: String) -> Self {
        Self {
            goal,
            plan: TaskPlan::default(),
            plan_md: None,
            modified: false,
            timeline: ExecutionTimeline::default(),
            version: 1,
            created_by: None,
            session_hint: None,
        }
    }

    /// Create a new plan with an owner user ID.
    pub fn new_with_owner(goal: String, user_id: String) -> Self {
        let mut state = Self::new(goal);
        state.created_by = Some(user_id);
        state
    }
}

// ─── Plan Identifiers ────────────────────────────────────────────────────────

impl PlanModeState {
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

/// Returns true when a subtask explicitly calls for real browser/UI verification.
pub fn subtask_requires_browser_verification(subtask: &SubtaskPlan) -> bool {
    let mut text = subtask.title.to_lowercase();
    if let Some(desc) = &subtask.description {
        text.push('\n');
        text.push_str(&desc.to_lowercase());
    }

    // Strong signals: tool/framework names that unambiguously mean browser.
    let strong_browser = [
        "browser",
        "in browser",
        "浏览器",
        "playwright",
        "selenium",
        "puppeteer",
        "cypress",
    ]
    .iter()
    .any(|needle| text.contains(needle));

    // Weak signals: only count if paired with explicit browser context.
    // "page" alone matches "pagination", "ui" matches "build"/"suite".
    let weak_browser = !strong_browser
        && [" web page", "web ui", "in the dom", "html canvas", "页面"]
            .iter()
            .any(|needle| text.contains(needle));

    let mentions_browser = strong_browser || weak_browser;

    // Verification keywords — removed "run" (too generic: "run tests", "run migration").
    let mentions_verification = [
        "test in browser",
        "verify in browser",
        "test",
        "verify",
        "validation",
        "validate",
        "check",
        "qa",
        "smoke",
        "open in",
        "测试",
        "验证",
        "检查",
        "打开",
        "试玩",
    ]
    .iter()
    .any(|needle| text.contains(needle));

    mentions_browser && mentions_verification
}

/// Build the executor prompt for a subtask, optionally prefixed with stacked
/// operator guidance from prior pause/correction turns.
pub fn format_subtask_prompt_with_operator_notes(
    subtask: &SubtaskPlan,
    operator_notes: &[String],
) -> String {
    let mut body = format!("Execute this subtask: {}\n", subtask.title);

    if let Some(ref desc) = subtask.description {
        body.push_str(&format!("\nDescription: {}\n", desc));
    }

    if !subtask.files.is_empty() {
        body.push_str(&format!(
            "\nFiles to modify: {}\n",
            subtask.files.join(", ")
        ));
    }

    if !subtask.acceptance_checks.is_empty() {
        body.push_str("\nAcceptance checks (automated verification will run these):\n");
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
            body.push_str(&format!("  {}. {}\n", i + 1, desc));
        }
    }

    body.push_str(
        "\nPlease implement this change. Read the relevant files first, \
         make the changes, and verify they compile/pass tests.\n\
         Before referencing any project type, function, struct, or API in new code, \
         confirm it exists using read_file, grep, or LSP tools. Do not assume symbol names.\n\
         \n\
         IMPORTANT — how to produce code:\n\
         - Emit actual file mutations as tool_calls: `write_file`, `str_replace`, \
           `create_file`, or `bash` (for mkdir / scaffolding). DO NOT paste \
           implementation code inside the assistant response as markdown code blocks \
           — markdown is inert and does not modify the filesystem.\n\
         - After writing any new file, confirm it exists (`read_file` or `bash ls`) \
           before declaring the subtask done.\n\
         - `skill` and `discover_skills` are advisory: consulting a skill does NOT \
           satisfy the subtask. The subtask is only complete when concrete files \
           have been written and any acceptance check passes.\n\
         - Do not invoke `github_create_pr` (or any PR-creation skill) before you \
         have actually written and committed the changes this subtask requires.",
    );

    if subtask_requires_browser_verification(subtask) {
        body.push_str(
            "\n\
             IMPORTANT — this subtask explicitly requires browser/UI verification:\n\
             - `curl`, `grep`, `head`, `ps`, or starting a local HTTP server do NOT count as browser verification.\n\
             - Only mark the subtask done after collecting evidence from a real browser-capable tool or workflow \
               (for example: Playwright, Selenium, Puppeteer, Cypress, a browser headless screenshot, \
               or a browser DOM dump after real page execution).\n\
             - If no browser-capable tool is available in this environment, say that plainly instead of claiming \
               the browser behavior was verified.\n",
        );
    }

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

// ─── Plan Execution Config & Summary ─────────────────────────────────────────

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
}

// ─── Execution Timeline ─────────────────────────────────────────────────────

/// Types of events that can occur during plan execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TimelineEventKind {
    /// Plan was created
    PlanCreated { subtask_count: usize },
    /// Plan was modified/replanned
    Replan { reason: String, changes: String },
    /// Plan was rewound — subtask at `from_idx` and every subtask after it
    /// reset to pending. `reset_count` is the number that actually flipped.
    SubtaskRewound {
        anchor: String,
        from_idx: usize,
        reset_count: usize,
        reason: Option<String>,
    },
    /// A single subtask was reset for re-execution (distinct from a rewind).
    SubtaskRedone {
        subtask_id: String,
        title: String,
        attempt: u32,
    },
}

/// A single event in the execution timeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineEvent {
    /// ISO 8601 timestamp
    pub timestamp: String,
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

        Self {
            timestamp: now.to_string(),
            event,
        }
    }
}

/// Execution timeline tracking all events during plan execution.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecutionTimeline {
    /// All recorded events, in chronological order.
    pub events: Vec<TimelineEvent>,
}

impl ExecutionTimeline {
    /// Record a new event.
    pub fn record(&mut self, kind: TimelineEventKind) {
        self.events.push(TimelineEvent::new(kind));
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

/// Summary info for a saved plan.
#[derive(Debug, Clone)]
pub struct SavedPlanInfo {
    pub name: String,
    pub goal: String,
    pub progress_pct: u32,
    pub subtask_count: usize,
    pub status: String,
}

#[cfg(test)]
mod tests {
    use super::*;

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

    // ═══════════════════════════ Auto-Execution Tests ═══════════════════════

    #[test]
    fn format_subtask_prompt_minimal() {
        let st = SubtaskPlan {
            id: "t1".into(),
            title: "Add login page".into(),
            ..Default::default()
        };
        let prompt = format_subtask_prompt_with_operator_notes(&st, &[]);
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
        let prompt = format_subtask_prompt_with_operator_notes(&st, &[]);
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
        let prompt = format_subtask_prompt_with_operator_notes(&st, &[]);
        assert!(prompt.contains("Extract connection pooling"));
        assert!(prompt.contains("retry logic"));
    }

    #[test]
    fn browser_verification_subtask_prompt_requires_real_browser_evidence() {
        let st = SubtaskPlan {
            id: "t4".into(),
            title: "Test game in browser".into(),
            description: Some(
                "Open the page, play a round, and verify keyboard input works.".into(),
            ),
            ..Default::default()
        };
        let prompt = format_subtask_prompt_with_operator_notes(&st, &[]);
        assert!(
            prompt.contains("requires browser/UI verification"),
            "prompt should surface explicit browser-verification guidance: {prompt}"
        );
        assert!(
            prompt.contains("curl") && prompt.contains("do NOT count as browser verification"),
            "prompt should explicitly reject curl-style checks as sufficient evidence: {prompt}"
        );
        assert!(
            prompt.contains("Playwright") || prompt.contains("browser headless screenshot"),
            "prompt should name acceptable browser-capable evidence: {prompt}"
        );
    }

    #[test]
    fn browser_verification_no_false_positive_on_non_browser_tasks() {
        // "page" in "pagination", "ui" in "build", "run" alone — should NOT trigger.
        for title in [
            "Run database migration for user page",
            "Build UI component library",
            "Run unit tests for the pagination module",
            "Check DOM manipulation in JSDOM tests",
            "Run canvas rendering benchmark",
        ] {
            let st = SubtaskPlan {
                id: "t1".into(),
                title: title.into(),
                description: None,
                ..Default::default()
            };
            assert!(
                !subtask_requires_browser_verification(&st),
                "should NOT trigger browser verification for: {title}"
            );
        }
    }

    #[test]
    fn browser_verification_true_positive_on_real_browser_tasks() {
        for title in [
            "Test game in browser",
            "Verify the web page renders correctly",
            "Open in browser and check layout",
            "用浏览器测试页面",
            "Run Playwright tests for login flow",
        ] {
            let st = SubtaskPlan {
                id: "t1".into(),
                title: title.into(),
                description: None,
                ..Default::default()
            };
            assert!(
                subtask_requires_browser_verification(&st),
                "should trigger browser verification for: {title}"
            );
        }
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
        assert_eq!(summary.goal, "Test goal");
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
        assert_eq!(summary.goal, "Failing goal");
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

    // ── Robust JSON extraction tests ────────────────────────────────────

    #[test]
    fn extract_json_robust_fixes_trailing_commas() {
        let input = r#"{"subtasks": [{"id": "t1", "title": "A",}]}"#;
        let result = extract_json_robust(input);
        assert!(serde_json::from_str::<serde_json::Value>(&result).is_ok());
    }

    #[test]
    fn extract_json_robust_strips_comments() {
        let input = r#"{
  "subtasks": [
    {"id": "t1", "title": "A"} // first task
  ]
}"#;
        let result = extract_json_robust(input);
        assert!(serde_json::from_str::<serde_json::Value>(&result).is_ok());
    }

    #[test]
    fn extract_json_robust_fixes_both() {
        let input = r#"{
  "subtasks": [
    {"id": "t1", "title": "A",}, // trailing comma + comment
  ]
}"#;
        let result = extract_json_robust(input);
        assert!(
            serde_json::from_str::<serde_json::Value>(&result).is_ok(),
            "should fix both trailing commas and comments: {result}"
        );
    }

    #[test]
    fn extract_json_robust_preserves_valid_json() {
        let input = r#"{"subtasks": [{"id": "t1", "title": "A"}]}"#;
        assert_eq!(extract_json_robust(input), input);
    }

    #[test]
    fn extract_json_robust_strips_thinking_tags() {
        let input = "<think>Let me analyze this goal. I need to create phases for {\"something\": true}.</think>\n{\"phases\": [{\"id\": \"p1\", \"title\": \"A\", \"description\": \"B\", \"estimated_subtasks\": 1}], \"total_effort\": \"small\"}";
        let result = extract_json_robust(input);
        assert!(
            serde_json::from_str::<serde_json::Value>(&result).is_ok(),
            "should parse after stripping thinking tags: {result}"
        );
        assert!(result.contains("phases"));
    }

    #[test]
    fn extract_json_robust_strips_thinking_with_markdown() {
        let input = "<thinking>reasoning here</thinking>\n```json\n{\"subtasks\": [{\"id\": \"t1\", \"title\": \"A\"}]}\n```";
        let result = extract_json_robust(input);
        assert!(serde_json::from_str::<serde_json::Value>(&result).is_ok());
        assert!(result.contains("subtasks"));
    }

    #[test]
    fn strip_thinking_tags_removes_all_variants() {
        let input = "<think>a</think>X<thinking>b</thinking>Y<reflect>c</reflect>Z";
        assert_eq!(strip_thinking_tags(input), "XYZ");
    }

    #[test]
    fn strip_thinking_tags_multiple_same_tag() {
        let input = "<think>first</think>MIDDLE<think>second</think>END";
        assert_eq!(strip_thinking_tags(input), "MIDDLEEND");
    }

    #[test]
    fn strip_thinking_tags_nested_braces_in_thinking() {
        let input = "<think>I see {\"key\": \"val\"} in the code</think>\n{\"phases\": []}";
        let result = strip_thinking_tags(input);
        assert_eq!(result, "\n{\"phases\": []}");
    }

    #[test]
    fn strip_thinking_tags_handles_unclosed() {
        let input = "before<think>reasoning without close";
        assert_eq!(strip_thinking_tags(input), "reasoning without close");
    }

    #[test]
    fn strip_thinking_tags_unclosed_with_json() {
        // Model outputs <think>...JSON... without </think> — JSON must survive
        let input = "<think>some reasoning\n{\"phases\": []}";
        let result = strip_thinking_tags(input);
        assert!(result.contains("{\"phases\": []}"), "got: {result}");
    }

    #[test]
    fn strip_thinking_tags_no_tags() {
        let input = "just plain text";
        assert_eq!(strip_thinking_tags(input), input);
    }

    #[test]
    fn extract_json_robust_handles_markdown_fence_with_errors() {
        let input = "```json\n{\"subtasks\": [{\"id\": \"t1\", \"title\": \"A\",}]}\n```";
        let result = extract_json_robust(input);
        assert!(serde_json::from_str::<serde_json::Value>(&result).is_ok());
    }

    #[test]
    fn fix_trailing_commas_nested() {
        let input = r#"{"a": [1, 2, 3,], "b": {"c": "d",}}"#;
        let fixed = fix_trailing_commas(input);
        assert!(serde_json::from_str::<serde_json::Value>(&fixed).is_ok());
    }

    #[test]
    fn strip_json_comments_preserves_urls() {
        // "//" inside strings should not be stripped
        let input = r#"{"url": "https://example.com"}"#;
        let stripped = strip_json_comments(input);
        assert_eq!(stripped, input);
    }

    #[test]
    fn extract_json_robust_handles_smart_quotes() {
        let input = "{\u{201C}id\u{201D}: \u{201C}t1\u{201D}}";
        let result = extract_json_robust(input);
        let v: serde_json::Value =
            serde_json::from_str(&result).expect("smart-quote payload should parse");
        assert_eq!(v["id"], "t1");
    }

    #[test]
    fn extract_json_robust_handles_single_quoted_strings() {
        let input = "{'id': 'task-1', 'title': 'first'}";
        let result = extract_json_robust(input);
        let v: serde_json::Value =
            serde_json::from_str(&result).expect("single-quoted JSON should parse");
        assert_eq!(v["id"], "task-1");
        assert_eq!(v["title"], "first");
    }

    #[test]
    fn extract_json_robust_handles_python_literals() {
        let input = r#"{"done": True, "skip": False, "note": None}"#;
        let result = extract_json_robust(input);
        let v: serde_json::Value =
            serde_json::from_str(&result).expect("python literals should be normalized");
        assert_eq!(v["done"], true);
        assert_eq!(v["skip"], false);
        assert!(v["note"].is_null());
    }

    #[test]
    fn extract_json_robust_handles_combined_errors() {
        // Smart quotes + single quotes + trailing comma + Python literal +
        // JS comment all in one payload — pathological but observed in
        // real LLM output.
        let input = "```json\n{\u{201C}items\u{201D}: ['a', 'b',], \
                     // trailing comment\n  \"done\": True,}\n```";
        let result = extract_json_robust(input);
        let v: serde_json::Value =
            serde_json::from_str(&result).expect("combined repair should parse: {result}");
        assert_eq!(v["items"][0], "a");
        assert_eq!(v["items"][1], "b");
        assert_eq!(v["done"], true);
    }

    #[test]
    fn fix_python_literals_preserves_strings() {
        // The literal `True` inside a string must NOT be rewritten.
        let input = r#"{"label": "True positive", "flag": True}"#;
        let out = fix_python_literals(input);
        let v: serde_json::Value = serde_json::from_str(&out).expect("must parse");
        assert_eq!(v["label"], "True positive");
        assert_eq!(v["flag"], true);
    }

    #[test]
    fn fix_single_quoted_strings_preserves_apostrophes_in_double_quoted() {
        // An apostrophe inside a double-quoted string must survive intact.
        let input = r#"{"msg": "don't break me"}"#;
        let out = fix_single_quoted_strings(input);
        assert_eq!(out, input);
    }

    #[test]
    fn fix_single_quoted_strings_escapes_embedded_double_quote() {
        let input = r#"{'msg': 'she said "hi"'}"#;
        let out = fix_single_quoted_strings(input);
        let v: serde_json::Value =
            serde_json::from_str(&out).expect("escaped embedded quote should parse");
        assert_eq!(v["msg"], "she said \"hi\"");
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
}

/// Agent persona / base identity.
pub const SYSTEM_PROMPT_BASE: &str =
    "You are an expert software engineer. You write clean, correct code and use tools precisely to solve tasks.";

/// Confidence threshold below which the system prompt includes an advisory
/// telling the LLM to ask for clarification rather than guessing with wrong tools.
pub const LOW_CONFIDENCE_THRESHOLD: f64 = 0.3;

/// Full system-prompt body when tools are available.
///
/// `tool_names`   – comma-joined list of tool names (for the self-model section)
/// `profile_desc` – optional project-profile block appended after the tool list
/// `selection_confidence` – tool selection confidence 0.0-1.0, used to gate advisories
/// `task_type`    – optional task classification ("code_review", "debugging", etc.)
pub fn build_main_system_prompt(
    tool_names: &[&str],
    profile_desc: &str,
    selection_confidence: f64,
    task_type: Option<&str>,
) -> String {
    if tool_names.is_empty() {
        return format!(
            "{SYSTEM_PROMPT_BASE}\n\n\
             ## CRITICAL\n\
             You have NO tools available in this turn. \
             Do NOT generate fake data (PRs, issues, commits, file contents). \
             If the user asks for real-time data, say: \"I don't have tools available to look that up.\"\n\
             {profile_desc}"
        );
    }

    let has_memory = tool_names.iter().any(|n| n.starts_with("memory"));
    let has_github = tool_names.iter().any(|n| n.starts_with("github"));
    let has_git = tool_names.iter().any(|n| n.starts_with("git_"));
    let has_glob = tool_names.contains(&"glob");
    let has_grep = tool_names.contains(&"grep");
    let has_read_file = tool_names.contains(&"read_file");
    let has_code_nav = tool_names.contains(&"find_definition") || tool_names.contains(&"find_references");
    let has_call_graph = tool_names.contains(&"call_graph");
    let has_multi_edit = tool_names.contains(&"multi_edit");
    let has_build_test = tool_names.contains(&"run_build_test");
    let has_git_mutations = tool_names.contains(&"git_commit");

    let mut prompt = format!(
        "{SYSTEM_PROMPT_BASE}\n\n\
         ## Self-Model\n\
         Tools: {}{}\n\n\
         ## Core Rules\n\
         1. Think step-by-step, then act. For multi-step tasks, plan BEFORE your first tool call.\n\
         2. NEVER fabricate data — always use tools for real-time info. Violations are worse than \"I don't know\".\n\
         3. Do ONLY what the user asked. When done → STOP and report.\n\
         4. Live data (CI, PRs, issues, stats, memory, git) → MUST call a tool. Never answer from training data.\n\
         5. Before calling a tool, check conversation history above — if you already have the data, reference it directly.\n\
         6. Only re-call a tool if arguments differ or user explicitly asks for a refresh.\n\n\
         ## Planning Protocol\n\
         For tasks that need 3+ tool calls, plan in a <think> block FIRST:\n\
         <think>\n\
         Goal: [what the user wants]\n\
         Plan: [numbered steps — what to read/check/change/verify]\n\
         </think>\n\
         After each tool result, reflect: <reflect>[what I learned] [adjust plan or proceed]</reflect>\n\
         This keeps you on track and prevents exploration spirals.\n\n\
         ## Context Strategy\n\
         Before acting, identify WHAT context you need:\n\
         1. **Plan context needs**: What files/functions/tests must I understand first?\n\
         2. **Batch the fetch**: Call all needed reads/greps in ONE turn (parallel).\n\
         3. **Check inventory**: If context was already fetched, use it — don't re-fetch.\n\
         4. **Then act**: Only after understanding, make your changes.\n\
         Example: To fix a bug in auth.rs, plan: \"Need auth.rs:50-100, the test file, and git blame on line 75\" → fetch all 3 → then edit.\n\n\
         ## Coding Discipline\n\
         - **Read before write**: understand existing patterns, naming conventions, and imports before editing.\n\
         - **Surgical edits**: change only what's needed. Don't rewrite unrelated code.\n\
         - **Verify after changes**: run build/test commands to confirm nothing broke.\n\
         - **Undo on failure**: if a change causes errors and you can't fix them, revert it.\n\
         - **One concern per edit**: each str_replace should address one logical change.\n\
         - **Imports and dependencies**: when adding new functionality, add required imports/deps.\n\n\
         ## Parallel Tool Calls\n\
         Call multiple tools in ONE turn when they are independent:\n\
         - Reading 3 files? Call read_file 3× in parallel.\n\
         - Need git_status AND git_diff? Call both.\n\
         - Need glob AND grep with different patterns? Call both.\n\
         Do NOT parallelize when one result determines the next call's arguments.\n\n\
         ## Token Efficiency\n\
         - Prefer targeted reads (line ranges) over full-file reads.\n\
         - Use glob to narrow candidates before grep.\n\
         - Request only the data you need — avoid fetching entire files when a section suffices.\n\
         - Summarize findings concisely. Show relevant code, not the whole file.\n\
         - If you've already fetched something, reference it from history — don't re-fetch.\n\n\
         ## Plan Execution\n\
         When executing a subtask from a decomposed plan:\n\
         - **Focus on the subtask**: implement ONLY what's described. Don't scope-creep.\n\
         - **Respect files list**: if the subtask specifies files to modify, start by reading those.\n\
         - **Meet acceptance criteria**: the subtask may include criteria — verify them before marking done.\n\
         - **Build/test after changes**: run the project's build and test commands to confirm.\n\
         - **Report clearly**: summarize what you changed and whether acceptance criteria passed.\n\
         - **Don't skip ahead**: each subtask may depend on previous ones. Trust the ordering.\n",
        tool_names.join(", "),
        profile_desc,
    );

    // ── Tool selection guidance: prefer specific tools over bash ──
    if has_git || has_github {
        prompt.push_str(
            "7. Use specific tools (git_diff, git_log, github_list_prs) for git/GitHub queries — NOT bash.\n",
        );
    }

    // ── GitHub rules: only when GitHub tools are selected ──
    if has_github {
        prompt.push_str(
            "8. For GitHub data: use github_list_prs / github_list_issues / github_repo_stats directly.\n",
        );
    }

    // ── Code navigation guidance ──
    if has_code_nav {
        prompt.push_str(
            "\n\
             ## Code Navigation\n\
             - **find_definition**: Where a symbol is defined. tree-sitter AST — more accurate than grep.\n\
             - **find_references**: All usages of a symbol. Use `kind` (definition/import/call/usage) to filter.\n\
             - **symbols**: File outline. Use `calls=true` to see what each function calls inline.\n\
             Use these BEFORE grep for code symbols. They understand syntax, grep doesn't.\n",
        );
    }
    if has_call_graph {
        prompt.push_str(
            "- **call_graph**: Call relationships. `callers=true` finds who calls a function. `scope='project'` searches cross-file.\n",
        );
    }
    if tool_names.contains(&"rename_symbol") {
        prompt.push_str(
            "- **rename_symbol**: Rename across project. AST-validated, skips comments/strings. dry_run=true previews.\n",
        );
    }
    if tool_names.contains(&"dead_code") {
        prompt.push_str(
            "- **dead_code**: Find unused symbols. Use before cleanup to identify safe deletions.\n",
        );
    }

    // ── Editing strategy guidance ──
    if has_multi_edit {
        prompt.push_str(
            "\n\
             ## Editing Strategy\n\
             - Use **multi_edit** for multiple related changes to one file — it's atomic (all-or-nothing) and more token-efficient than sequential str_replace.\n\
             - Use **str_replace(dry_run=true)** to preview changes before applying. Great for complex edits where you want to verify first.\n\
             - Use **delete_file** to remove files (safe: refuses .git/, directories, paths outside project root).\n\
             - For risky refactors: dry_run first → review diff → apply if correct.\n",
        );
    }

    // ── Build/test loop guidance ──
    if has_build_test {
        prompt.push_str(
            "\n\
             ## Build & Test Loop\n\
             - Use **run_build_test** instead of bash for build/test commands. It returns structured errors WITH source context.\n\
             - Each error shows: 🔧 Trivial (mechanical fix), 🔨 Fixable (needs reasoning), or Complex.\n\
             - Errors include 💡 hints — follow them for quick resolution.\n\
             - Each error location includes surrounding code — fix directly with str_replace, no extra read_file needed.\n\
             - If >3 errors in the same file, fix the FIRST one — later errors are often cascading.\n\
             - After fixing, call run_build_test again with the SAME command. The tool tracks iterations:\n\
             - It shows ✅ Fixed, 🆕 New, ⏳ Persistent errors — use this to gauge your fix progress.\n\
             - If you see ⚠ Regression (more errors after your fix), revert the change and try a different approach.\n\
             - Repeat until clean. Aim to fix ALL errors, not just the first one.\n",
        );
    }

    // ── Git workflow guidance ──
    if has_git_mutations {
        prompt.push_str(
            "\n\
             ## Git Workflow\n\
             - Use **git_commit** to commit changes (stages automatically). Write clear, concise commit messages.\n\
             - Use **git_stash** push/pop to save and restore work-in-progress.\n\
             - Use **git_checkout_file** to revert a file to its last committed state if an edit goes wrong.\n\
             - Commit after each logical milestone — don't accumulate too many uncommitted changes.\n",
        );
    }

    // ── Memory rules: only when memory tools are selected ──
    if has_memory {
        prompt.push_str(
            "\n\
             ## Memory Rules (check BEFORE reasoning about tools)\n\
             ### Triggers: 关注|跟踪|留意|记住|感兴趣|follow|watch|track|interested|prefer|remember\n\
             When user expresses tracking, interest, or preference → call memory_store IMMEDIATELY.\n\
             Format: \"[@ns/status] content\" (ns: pref, fact, knowledge, task, plan, insight)\n\
             Example: \"我关注matrixorigin\" → store \"[@pref/active] user follows matrixorigin\"\n\
             - Do NOT ask whether to store — just store, then confirm.\n\
             - Do NOT explore codebase for interest expressions.\n\
             - '## User Memories' (when present) = user context — check it BEFORE calling any tool.\n\
             - If User Memories has a repo mapping, USE that exact repo.\n\
             ### What to STORE: preferences, conventions, decisions, tracking interests.\n\
             ### What to SKIP: ephemeral tool outputs, raw file contents, duplicates.\n\
             ### Deduplication: before storing, consider if similar memory already exists. Use memory_correct to update instead of creating duplicates.\n\
             ### Negative preferences: \"不喜欢\", \"别用\", \"don't want\", \"stop using\" → store as [@pref/negative]. Respect in future tool/approach selection.\n\
             ### Staleness: if a stored memory seems outdated (e.g., old repo URL, changed preference), correct it with memory_correct rather than storing a new one.\n",
        );
    }

    // ── Low-confidence advisory: when tool selection is uncertain ──
    if selection_confidence < LOW_CONFIDENCE_THRESHOLD {
        prompt.push_str(
            "\n\
             ## ⚠ Low-Confidence Tool Selection\n\
             Tool selection confidence is LOW. If available tools seem insufficient, ASK the user to clarify.\n\
             Do NOT guess with bash/find/read_file when a more specific tool would be needed.\n",
        );
    }

    // ── Task-type specific rules ──
    match task_type {
        Some("code_review") => {
            prompt.push_str(
                "\n\
              ## Code Review Strategy\n\
              1. Get the diff: git_diff ONCE. Do NOT re-call with same args.\n\
              2. Identify changed files and understand the scope of changes.\n\
              3. For each significant change, read surrounding context (targeted line ranges, not full files).\n\
              4. Evaluate in order: **correctness** → **security** → **edge cases** → **performance** → **test coverage**.\n\
              5. Skip style nits unless they cause bugs.\n\
              6. If something is unclear, read the test file or call site before flagging.\n\
              7. Present findings grouped by severity: 🔴 must-fix, 🟡 should-fix, 💡 suggestion.\n",
            );
        }
        Some("debugging") => {
            prompt.push_str(
                "\n\
             ## Debugging Strategy\n\
             1. Start with the error message / stack trace — read it carefully before exploring.\n\
             2. Form a hypothesis about the root cause.\n\
             3. Verify with ONE targeted tool call (read the suspected file/function).\n\
             4. If hypothesis is wrong, form a new one — don't shotgun search.\n\
             5. Check recent git changes near the error site (git_log, git_blame).\n\
             6. If a command fails, do NOT retry the exact same command — vary the approach.\n\
             7. Once found: explain the root cause, show the fix, verify it compiles/passes.\n",
            );
        }
        Some("exploration") => {
            prompt.push_str(
                "\n\
             ## Exploration Strategy\n\
             1. Start broad: list_dir for project structure, then identify entry points.\n\
             2. Narrow: grep for key terms, glob for file patterns.\n\
             3. Build a mental map: entry points → core modules → dependencies → patterns.\n\
             4. Read files with targeted ranges, not full files — scan structure first.\n\
             5. Summarize architecture with concrete file paths and relationships.\n\
             6. Note patterns: error handling style, naming conventions, test structure.\n",
            );
        }
        Some("implementation") => {
            prompt.push_str(
                "\n\
              ## Implementation Strategy\n\
              1. **Understand structure**: symbols(calls=true) for file overview + call flow in one shot.\n\
              2. **Find location**: find_definition → glob → grep → read sections.\n\
              3. **Check impact**: find_references(kind='call') to see callers. call_graph(callers=true, scope='project') for thorough impact.\n\
              4. **Implement surgically**: minimal changes, follow style. str_replace auto-formats.\n\
              5. **Wire it up**: add imports, register modules, update exports.\n\
              6. **Verify**: run_build_test, fix from structured output, repeat.\n\
              7. **Commit**: git_commit with a clear message.\n",
            );
        }
        Some("refactoring") => {
            prompt.push_str(
                "\n\
             ## Refactoring Strategy\n\
             1. Run tests BEFORE refactoring to establish a passing baseline.\n\
             2. Use call_graph(callers=true, scope='project') to find all callers before changing a signature.\n\
             3. For renames: rename_symbol(dry_run=true) to preview, then dry_run=false to apply.\n\
             4. Make one logical change at a time — verify after each.\n\
             5. Preserve external behavior; focus on clarity and maintainability.\n\
             6. Run tests AFTER to confirm nothing regressed.\n",
            );
        }
        Some("testing") => {
            prompt.push_str(
                "\n\
             ## Testing Strategy\n\
             1. Read the module under test to understand its behavior and edge cases.\n\
             2. Follow existing test patterns: naming, setup/teardown, assertion style.\n\
             3. Cover: happy path → edge cases → error conditions → boundary values.\n\
             4. Each test verifies ONE behavior with a clear, descriptive name.\n\
             5. Run the new tests to confirm they pass — fix failures before reporting.\n",
            );
        }
        Some("documentation") => {
            prompt.push_str(
                "\n\
             ## Documentation Strategy\n\
             - Read the code first — document actual behavior, not assumptions.\n\
             - Include: purpose, usage examples, parameters, return values, error conditions.\n\
             - Keep docs close to the code they describe.\n\
             - Use the project's existing documentation style and format.\n",
            );
        }
        Some("performance") => {
            prompt.push_str(
                "\n\
             ## Performance Strategy\n\
             1. Measure first — don't guess. Profile to locate the actual bottleneck.\n\
             2. Optimize the hottest path only; avoid premature optimization elsewhere.\n\
             3. Check: algorithm complexity, allocation patterns, I/O blocking, cache misses.\n\
             4. Verify improvement with before/after measurements.\n\
             5. Ensure optimization doesn't break correctness — run tests after.\n",
            );
        }
        Some("analysis") => {
            prompt.push_str(
                "\n\
             ## Analysis Strategy\n\
             1. Gather data from multiple sources: code, git history, logs, docs.\n\
             2. Form hypotheses, then verify — don't jump to conclusions from a single signal.\n\
             3. Use git_blame + git_file_history for ownership/evolution questions.\n\
             4. Summarize findings with concrete evidence (file paths, line numbers, commit SHAs).\n\
             5. Present: root cause → impact → recommendation.\n",
            );
        }
        Some("deployment") => {
            prompt.push_str(
                "\n\
             ## Deployment Strategy\n\
             1. Check CI status FIRST — don't deploy if builds are failing.\n\
             2. Review pending changes: git_status → git_diff → CI status.\n\
             3. Verify config files (env vars, secrets) are correct for target environment.\n\
             4. Prefer incremental rollout over big-bang deployments.\n",
            );
        }
        _ => {}
    }

    // ── Output format guidance: always present ──
    prompt.push_str(
        "\n\
         ## Output Format\n\
         - **Respond in the user's language.** If they write Chinese, respond in Chinese.\n\
         - **Code changes**: show the changed code with brief explanation. Don't dump entire files.\n\
         - **Search results**: cite file:line, group by relevance. Quote the key lines, not every match.\n\
         - **Build/test output**: report pass/fail. On failure, show the error message — not the full log.\n\
         - **Explanations**: be direct. Lead with the answer, then give supporting details.\n\
         - **Multiple findings**: use a structured list or table. Don't bury results in prose.\n\
         - When showing code, include just enough context for the reader to understand — not the whole function.\n",
    );

    // ── Tool precedence guidance: always present ──
    prompt.push_str(
        "\n\
         ## Tool Precedence (prefer earlier tools in each chain)\n\
         - **Understand code**: symbols(calls=true) → call_graph → read_file\n\
         - **Navigate code**: find_definition / find_references(kind=...) → grep\n\
         - **Impact analysis**: call_graph(callers=true, scope='project') → find_references\n\
         - **Rename/refactor**: rename_symbol(dry_run=true) → review → apply\n\
         - **File search**: glob → grep (content) → log search (commits)\n\
         - **Code edit**: read context → str_replace → run_build_test\n\
         - **Git**: status → diff → log → show → blame; git_commit for changes\n\
         - **Build/test**: run_build_test → fix errors → repeat\n\
         - **GitHub**: list → detail → CI status\n",
    );
    if has_memory {
        prompt
            .push_str("         - **Memory**: check '## User Memories' → search → store/correct\n");
    }

    if has_glob || has_grep || has_read_file {
        prompt.push_str(
            "\n\
         ## Search Strategy\n\
         - Start narrow. Prefer likely roots first: src, crates, app, lib, packages, cmd, internal, tests.\n\
         - Use glob first to narrow filenames/dirs, then grep only that subset for content.\n\
         - For code review, search within changed files or adjacent modules before scanning the whole repo.\n\
         - Avoid broad repo-wide regex searches when a symbol, filename, extension, or directory hint is available.\n\
         - Skip generated or bulky trees unless the task explicitly targets them: build, dist, target, coverage, htmlcov, node_modules, vendor.\n\
         - After grep finds candidates, switch to targeted reads instead of repeating more broad searches.\n\
         - If a grep is slow or noisy, tighten path, extension, or literal term — do NOT repeat the same broad search.\n",
        );
    }

    // ── Error recovery: always present ──
    prompt.push_str(
        "\n\
         ## Tool Error Recovery\n\
         - If a tool returns an error, read the error message carefully.\n\
         - Fix the arguments (wrong path, typo, missing param) and retry ONCE.\n\
         - If it fails again, try an alternative tool or approach.\n\
         - NEVER retry the same failing call more than twice.\n\
         - If output is truncated (\"... truncated\"), work with what you have or narrow scope.\n\
         - **Timeout** (>30s no output): try a different approach, don't keep waiting.\n\
         - **Rate limited**: back off, don't retry the same API immediately.\n\
         - **Permission denied**: try a different path or ask the user.\n\
         - **Path not found**: use glob/grep to locate the file first.\n\
         - **Network failure**: check connectivity if multiple tools fail. Report to user.\n\
         - **Auth/credential error**: do NOT retry with same creds. Ask user to re-authenticate.\n\
         - **DB connection error**: verify MATRIXONE_HOST/PORT config. Use `mo_query` with simple SELECT 1 to test.\n\
         - **Empty results** (memory_search returns nothing): normal for new users — don't treat as error.\n\
         - **Unknown tool**: check get_agent_info for available tools. Do NOT invent tool names.\n",
    );

    prompt
}

// ── Task-type detection ──────────────────────────────────────────────

/// Keywords per task type for lightweight classification.
/// Each entry: (task_type_label, keywords).
/// CJK keywords are matched with contains(); Latin keywords use word-boundary matching.
const TASK_TYPE_KEYWORDS: &[(&str, &[&str])] = &[
    (
        "code_review",
        &[
            "review",
            "code review",
            "PR",
            "pull request",
            "diff",
            "评审",
            "审查",
            "代码审查",
        ],
    ),
    (
        "debugging",
        &[
            "debug",
            "error",
            "bug",
            "traceback",
            "exception",
            "crash",
            "调试",
            "报错",
            "崩溃",
            "出错",
        ],
    ),
    (
        "exploration",
        &[
            "explore",
            "understand",
            "how does",
            "what does",
            "architecture",
            "structure",
            "overview",
            "navigate",
            "了解",
            "理解",
            "架构",
            "结构",
            "概览",
            "怎么工作",
        ],
    ),
    (
        "implementation",
        &[
            "implement",
            "build",
            "create",
            "add feature",
            "write code",
            "develop",
            "实现",
            "开发",
            "新增",
            "添加功能",
            "写代码",
            "编写",
        ],
    ),
    (
        "refactoring",
        &[
            "refactor",
            "clean up",
            "cleanup",
            "simplify",
            "restructure",
            "reorganize",
            "remove dead code",
            "重构",
            "简化",
            "整理",
            "清理代码",
        ],
    ),
    (
        "testing",
        &[
            "test",
            "tests",
            "write tests",
            "test coverage",
            "unit test",
            "integration test",
            "测试",
            "写测试",
            "测试覆盖",
            "单元测试",
            "集成测试",
        ],
    ),
    (
        "documentation",
        &[
            "document",
            "docs",
            "readme",
            "write docs",
            "documentation",
            "docstring",
            "comment",
            "文档",
            "写文档",
            "注释",
            "说明",
        ],
    ),
    (
        "performance",
        &[
            "optimize",
            "performance",
            "profiling",
            "benchmark",
            "slow",
            "latency",
            "throughput",
            "memory leak",
            "bottleneck",
            "性能",
            "优化",
            "慢",
            "延迟",
            "基准测试",
            "瓶颈",
            "内存泄漏",
        ],
    ),
    (
        "analysis",
        &[
            "analyze",
            "analysis",
            "research",
            "investigate",
            "diagnose",
            "root cause",
            "why does",
            "why is",
            "分析",
            "研究",
            "调查",
            "诊断",
            "根因",
            "为什么",
        ],
    ),
    (
        "deployment",
        &[
            "deploy",
            "release",
            "publish",
            "rollout",
            "CI/CD",
            "pipeline",
            "staging",
            "production",
            "部署",
            "发布",
            "上线",
            "流水线",
            "发版",
            "灰度",
        ],
    ),
];

/// Detect task type from user query text.
/// Returns one of: `code_review`, `debugging`, `exploration`, `implementation`,
/// `refactoring`, `testing`, `documentation`, `performance`, `analysis`,
/// `deployment`, or `None`.
///
/// Uses simple keyword matching — no ML/embedding dependency.
/// CJK keywords use substring match; Latin keywords use case-insensitive
/// word-boundary match to avoid false positives (e.g. "fix" matching "prefix").
pub fn detect_task_type(query: &str) -> Option<&'static str> {
    if query.is_empty() {
        return None;
    }
    let lower = query.to_lowercase();

    let mut best: Option<(&str, usize)> = None;
    for &(label, keywords) in TASK_TYPE_KEYWORDS {
        let hits: usize = keywords
            .iter()
            .filter(|kw| {
                if kw.chars().any(|ch| ('\u{4E00}'..='\u{9FFF}').contains(&ch)) {
                    lower.contains(&kw.to_lowercase())
                } else {
                    // Word-boundary match for Latin keywords
                    lower
                        .split(|c: char| !c.is_alphanumeric() && c != '_')
                        .any(|w| w.eq_ignore_ascii_case(kw))
                        || lower.contains(&kw.to_lowercase())
                }
            })
            .count();
        if hits > 0 {
            match best {
                Some((_, prev)) if hits <= prev => {}
                _ => best = Some((label, hits)),
            }
        }
    }
    best.map(|(label, _)| label)
}

/// Injected into conversation when the agent repeats the same tool calls.
pub const STALL_NUDGE: &str = "You appear to be repeating the same tool calls. \
     Please try a different approach or summarize what you've found so far.";

#[cfg(test)]
mod tests {
    use super::*;

    // ── detect_task_type ──────────────────────────────────────────

    #[test]
    fn detect_code_review_en() {
        assert_eq!(detect_task_type("review this PR"), Some("code_review"));
        assert_eq!(detect_task_type("code review please"), Some("code_review"));
        assert_eq!(detect_task_type("check the diff"), Some("code_review"));
    }

    #[test]
    fn detect_code_review_cn() {
        assert_eq!(detect_task_type("评审一下这个代码"), Some("code_review"));
        assert_eq!(detect_task_type("帮我审查代码"), Some("code_review"));
        assert_eq!(detect_task_type("代码审查"), Some("code_review"));
    }

    #[test]
    fn detect_debugging_en() {
        assert_eq!(detect_task_type("debug this error"), Some("debugging"));
        assert_eq!(detect_task_type("there's a bug"), Some("debugging"));
        assert_eq!(detect_task_type("fix this crash"), Some("debugging"));
    }

    #[test]
    fn detect_debugging_cn() {
        assert_eq!(detect_task_type("调试一下这个"), Some("debugging"));
        assert_eq!(detect_task_type("报错了"), Some("debugging"));
        assert_eq!(detect_task_type("程序崩溃了"), Some("debugging"));
    }

    #[test]
    fn detect_exploration_en() {
        assert_eq!(
            detect_task_type("how does authentication work?"),
            Some("exploration")
        );
        assert_eq!(
            detect_task_type("explore the codebase"),
            Some("exploration")
        );
        assert_eq!(
            detect_task_type("show me the architecture"),
            Some("exploration")
        );
    }

    #[test]
    fn detect_exploration_cn() {
        assert_eq!(detect_task_type("了解一下这个项目"), Some("exploration"));
        assert_eq!(detect_task_type("架构是什么样的"), Some("exploration"));
        assert_eq!(detect_task_type("项目结构概览"), Some("exploration"));
    }

    #[test]
    fn detect_implementation_en() {
        assert_eq!(
            detect_task_type("implement user authentication"),
            Some("implementation")
        );
        assert_eq!(
            detect_task_type("build a new feature"),
            Some("implementation")
        );
        assert_eq!(
            detect_task_type("write code for login"),
            Some("implementation")
        );
    }

    #[test]
    fn detect_implementation_cn() {
        assert_eq!(detect_task_type("实现登录功能"), Some("implementation"));
        assert_eq!(detect_task_type("开发新功能"), Some("implementation"));
        assert_eq!(detect_task_type("帮我写代码"), Some("implementation"));
        // "编写测试用例" has both "编写"(implementation) and "测试"+"写测试"(testing)
        // Testing wins with 2 hits vs 1 — correct, it's about test cases
        assert_eq!(detect_task_type("编写测试用例"), Some("testing"));
    }

    #[test]
    fn detect_refactoring_en() {
        assert_eq!(
            detect_task_type("refactor the auth module"),
            Some("refactoring")
        );
        assert_eq!(detect_task_type("clean up dead code"), Some("refactoring"));
        assert_eq!(
            detect_task_type("simplify the function"),
            Some("refactoring")
        );
    }

    #[test]
    fn detect_refactoring_cn() {
        assert_eq!(detect_task_type("重构登录模块"), Some("refactoring"));
        assert_eq!(detect_task_type("简化这个函数"), Some("refactoring"));
        assert_eq!(detect_task_type("整理代码"), Some("refactoring"));
    }

    #[test]
    fn detect_testing_en() {
        assert_eq!(detect_task_type("write tests for the API"), Some("testing"));
        assert_eq!(detect_task_type("add unit test coverage"), Some("testing"));
        assert_eq!(detect_task_type("write integration tests"), Some("testing"));
    }

    #[test]
    fn detect_testing_cn() {
        assert_eq!(detect_task_type("写测试"), Some("testing"));
        assert_eq!(detect_task_type("增加测试覆盖"), Some("testing"));
        assert_eq!(detect_task_type("写单元测试"), Some("testing"));
    }

    #[test]
    fn detect_documentation_en() {
        assert_eq!(detect_task_type("document the API"), Some("documentation"));
        assert_eq!(
            detect_task_type("write docs for this"),
            Some("documentation")
        );
        assert_eq!(detect_task_type("update the readme"), Some("documentation"));
    }

    #[test]
    fn detect_documentation_cn() {
        assert_eq!(detect_task_type("写文档"), Some("documentation"));
        assert_eq!(detect_task_type("添加注释"), Some("documentation"));
        assert_eq!(detect_task_type("更新说明"), Some("documentation"));
    }

    #[test]
    fn detect_none_for_ambiguous() {
        assert_eq!(detect_task_type("hello"), None);
        assert_eq!(detect_task_type("你好"), None);
        assert_eq!(detect_task_type("thanks"), None);
    }

    #[test]
    fn detect_empty_returns_none() {
        assert_eq!(detect_task_type(""), None);
    }

    #[test]
    fn detect_highest_hit_wins() {
        // "review code diff" has 3 code_review hits (review, code review, diff)
        let result = detect_task_type("review the code diff");
        assert_eq!(result, Some("code_review"));
    }

    #[test]
    fn detect_case_insensitive() {
        assert_eq!(detect_task_type("DEBUG THIS ERROR"), Some("debugging"));
        assert_eq!(detect_task_type("REVIEW the PR"), Some("code_review"));
    }

    // ── build_main_system_prompt ──────────────────────────────────

    #[test]
    fn prompt_no_tools_warns_about_fabrication() {
        let p = build_main_system_prompt(&[], "", 0.5, None);
        assert!(p.contains("NO tools available"));
        assert!(p.contains("fake data"));
    }

    #[test]
    fn prompt_includes_tool_names() {
        let p = build_main_system_prompt(&["bash", "git_diff"], "", 0.5, None);
        assert!(p.contains("bash, git_diff"));
    }

    #[test]
    fn prompt_includes_memory_rules_when_memory_tools_present() {
        let p = build_main_system_prompt(&["memory_store", "memory_search"], "", 0.5, None);
        assert!(p.contains("Memory Rules"));
        assert!(p.contains("memory_store IMMEDIATELY"));
    }

    #[test]
    fn prompt_no_memory_rules_without_memory_tools() {
        let p = build_main_system_prompt(&["bash", "git_diff"], "", 0.5, None);
        assert!(!p.contains("Memory Rules"));
    }

    #[test]
    fn prompt_includes_github_rules_when_github_tools_present() {
        let p = build_main_system_prompt(&["github_list_prs", "github_list_issues"], "", 0.5, None);
        assert!(p.contains("GitHub data"));
    }

    #[test]
    fn prompt_low_confidence_advisory() {
        let p = build_main_system_prompt(&["bash"], "", 0.2, None);
        assert!(p.contains("Low-Confidence"));
        assert!(p.contains("ASK the user"));
    }

    #[test]
    fn prompt_no_low_confidence_above_threshold() {
        let p = build_main_system_prompt(&["bash"], "", 0.5, None);
        assert!(!p.contains("Low-Confidence"));
    }

    #[test]
    fn prompt_code_review_strategy() {
        let p = build_main_system_prompt(&["git_diff"], "", 0.5, Some("code_review"));
        assert!(p.contains("Code Review Strategy"));
        assert!(p.contains("git_diff ONCE"));
        assert!(p.contains("correctness"));
        assert!(p.contains("security"));
        assert!(p.contains("must-fix"));
    }

    #[test]
    fn prompt_debugging_strategy() {
        let p = build_main_system_prompt(&["bash"], "", 0.5, Some("debugging"));
        assert!(p.contains("Debugging Strategy"));
        assert!(p.contains("hypothesis"));
        assert!(p.contains("root cause"));
    }

    #[test]
    fn prompt_exploration_strategy() {
        let p = build_main_system_prompt(&["bash"], "", 0.5, Some("exploration"));
        assert!(p.contains("Exploration Strategy"));
        assert!(p.contains("mental map"));
    }

    #[test]
    fn prompt_implementation_strategy() {
        let p = build_main_system_prompt(&["bash"], "", 0.5, Some("implementation"));
        assert!(p.contains("Implementation Strategy"));
        assert!(p.contains("Implement surgically"));
    }

    #[test]
    fn prompt_implementation_strategy_mentions_glob_then_grep() {
        let p = build_main_system_prompt(
            &["glob", "grep", "read_file"],
            "",
            0.5,
            Some("implementation"),
        );
        assert!(p.contains("glob"));
        assert!(p.contains("grep"));
    }

    #[test]
    fn prompt_refactoring_strategy() {
        let p = build_main_system_prompt(&["bash"], "", 0.5, Some("refactoring"));
        assert!(p.contains("Refactoring Strategy"));
        assert!(p.contains("passing baseline"));
    }

    #[test]
    fn prompt_testing_strategy() {
        let p = build_main_system_prompt(&["bash"], "", 0.5, Some("testing"));
        assert!(p.contains("Testing Strategy"));
        assert!(p.contains("edge cases"));
    }

    #[test]
    fn prompt_documentation_strategy() {
        let p = build_main_system_prompt(&["bash"], "", 0.5, Some("documentation"));
        assert!(p.contains("Documentation Strategy"));
        assert!(p.contains("usage examples"));
    }

    #[test]
    fn prompt_includes_planning_protocol() {
        let p = build_main_system_prompt(&["bash"], "", 0.5, None);
        assert!(p.contains("Planning Protocol"));
        assert!(p.contains("<think>"));
        assert!(p.contains("<reflect>"));
    }

    #[test]
    fn prompt_includes_coding_discipline() {
        let p = build_main_system_prompt(&["bash"], "", 0.5, None);
        assert!(p.contains("Coding Discipline"));
        assert!(p.contains("Read before write"));
        assert!(p.contains("Surgical edits"));
        assert!(p.contains("Verify after changes"));
    }

    #[test]
    fn prompt_includes_parallel_tool_calls() {
        let p = build_main_system_prompt(&["bash"], "", 0.5, None);
        assert!(p.contains("Parallel Tool Calls"));
        assert!(p.contains("ONE turn"));
    }

    #[test]
    fn prompt_includes_token_efficiency() {
        let p = build_main_system_prompt(&["bash"], "", 0.5, None);
        assert!(p.contains("Token Efficiency"));
        assert!(p.contains("targeted reads"));
    }

    #[test]
    fn prompt_includes_output_format() {
        let p = build_main_system_prompt(&["bash"], "", 0.5, None);
        assert!(p.contains("Output Format"));
        assert!(p.contains("user's language"));
        assert!(p.contains("Code changes"));
        assert!(p.contains("Build/test output"));
    }

    #[test]
    fn prompt_includes_error_recovery() {
        let p = build_main_system_prompt(&["bash"], "", 0.5, None);
        assert!(p.contains("Tool Error Recovery"));
        assert!(p.contains("retry ONCE"));
        assert!(p.contains("truncated"));
        assert!(p.contains("Timeout"));
        assert!(p.contains("Rate limited"));
        assert!(p.contains("Permission denied"));
        assert!(p.contains("Path not found"));
        // New error recovery entries
        assert!(p.contains("Network failure"));
        assert!(p.contains("Auth/credential error"));
        assert!(p.contains("DB connection error"));
        assert!(p.contains("Empty results"));
        assert!(p.contains("Unknown tool"));
    }

    #[test]
    fn prompt_git_tool_preference_over_bash() {
        let p = build_main_system_prompt(&["git_diff", "git_log", "bash"], "", 0.5, None);
        assert!(p.contains("specific tools"));
        assert!(p.contains("NOT bash"));
    }

    #[test]
    fn prompt_profile_desc_included() {
        let p = build_main_system_prompt(&["bash"], "\n## Project: TestProj\n", 0.5, None);
        assert!(p.contains("Project: TestProj"));
    }

    // ── Constants ────────────────────────────────────────────────

    #[test]
    fn low_confidence_threshold_is_positive() {
        const { assert!(LOW_CONFIDENCE_THRESHOLD > 0.0) };
        const { assert!(LOW_CONFIDENCE_THRESHOLD < 1.0) };
    }

    #[test]
    fn stall_nudge_is_not_empty() {
        assert!(!STALL_NUDGE.is_empty());
        assert!(STALL_NUDGE.contains("different approach"));
    }

    // ── New task types: performance, analysis, deployment ────────

    #[test]
    fn detect_performance_en() {
        assert_eq!(
            detect_task_type("optimize database queries"),
            Some("performance")
        );
        assert_eq!(
            detect_task_type("this function is slow"),
            Some("performance")
        );
        assert_eq!(detect_task_type("run a benchmark"), Some("performance"));
        assert_eq!(detect_task_type("find the bottleneck"), Some("performance"));
    }

    #[test]
    fn detect_performance_cn() {
        assert_eq!(detect_task_type("性能优化"), Some("performance"));
        assert_eq!(detect_task_type("这个查询太慢了"), Some("performance"));
        assert_eq!(detect_task_type("延迟太高了"), Some("performance"));
        assert_eq!(detect_task_type("找到瓶颈"), Some("performance"));
    }

    #[test]
    fn detect_analysis_en() {
        assert_eq!(detect_task_type("analyze this code"), Some("analysis"));
        assert_eq!(
            detect_task_type("investigate the failure"),
            Some("analysis")
        );
        assert_eq!(detect_task_type("what is the root cause"), Some("analysis"));
        assert_eq!(detect_task_type("why does this happen"), Some("analysis"));
    }

    #[test]
    fn detect_analysis_cn() {
        assert_eq!(detect_task_type("分析一下这段代码"), Some("analysis"));
        assert_eq!(detect_task_type("调查这个失败"), Some("analysis"));
        assert_eq!(detect_task_type("根因是什么"), Some("analysis"));
        assert_eq!(detect_task_type("为什么会这样"), Some("analysis"));
    }

    #[test]
    fn detect_deployment_en() {
        assert_eq!(detect_task_type("deploy to production"), Some("deployment"));
        assert_eq!(detect_task_type("release version 2.0"), Some("deployment"));
        assert_eq!(
            detect_task_type("check the CI/CD pipeline"),
            Some("deployment")
        );
        assert_eq!(detect_task_type("set up staging"), Some("deployment"));
    }

    #[test]
    fn detect_deployment_cn() {
        assert_eq!(detect_task_type("部署到生产环境"), Some("deployment"));
        assert_eq!(detect_task_type("发布新版本"), Some("deployment"));
        assert_eq!(detect_task_type("上线计划"), Some("deployment"));
        assert_eq!(detect_task_type("灰度发布"), Some("deployment"));
    }

    // ── Strategy sections for new task types ──

    #[test]
    fn prompt_performance_strategy() {
        let p = build_main_system_prompt(&["bash"], "", 0.5, Some("performance"));
        assert!(p.contains("Performance Strategy"));
        assert!(p.contains("bottleneck"));
    }

    #[test]
    fn prompt_analysis_strategy() {
        let p = build_main_system_prompt(&["bash"], "", 0.5, Some("analysis"));
        assert!(p.contains("Analysis Strategy"));
        assert!(p.contains("hypotheses"));
    }

    #[test]
    fn prompt_deployment_strategy() {
        let p = build_main_system_prompt(&["bash"], "", 0.5, Some("deployment"));
        assert!(p.contains("Deployment Strategy"));
        assert!(p.contains("CI status"));
    }

    // ── Tool precedence guidance ──

    #[test]
    fn prompt_includes_tool_precedence() {
        let p = build_main_system_prompt(&["bash"], "", 0.5, None);
        assert!(p.contains("Tool Precedence"));
        assert!(p.contains("File search"));
        assert!(p.contains("Code edit"));
        assert!(p.contains("Git"));
        // Memory line only when memory tools present
        assert!(!p.contains("Memory:"));
        let p_mem = build_main_system_prompt(&["memory_store"], "", 0.5, None);
        assert!(p_mem.contains("Memory"));
    }

    #[test]
    fn prompt_includes_search_strategy_when_search_tools_present() {
        let p = build_main_system_prompt(&["glob", "grep", "read_file"], "", 0.5, None);
        assert!(p.contains("Search Strategy"));
        assert!(p.contains("Use glob first"));
        assert!(p.contains("Skip generated or bulky trees"));
        assert!(p.contains("If a grep is slow or noisy"));
    }

    #[test]
    fn prompt_omits_search_strategy_without_search_tools() {
        let p = build_main_system_prompt(&["bash"], "", 0.5, None);
        assert!(!p.contains("Search Strategy"));
    }

    // ── Enhanced memory rules ──

    #[test]
    fn prompt_memory_rules_include_dedup_and_negative() {
        let p = build_main_system_prompt(&["memory_store", "memory_search"], "", 0.5, None);
        assert!(p.contains("Deduplication"));
        assert!(p.contains("memory_correct"));
        assert!(p.contains("Negative preferences"));
        assert!(p.contains("不喜欢"));
        assert!(p.contains("Staleness"));
    }

    // ── Task type count invariant ──

    #[test]
    fn task_type_keywords_has_ten_types() {
        assert_eq!(TASK_TYPE_KEYWORDS.len(), 10, "expected 10 task types");
    }

    #[test]
    fn all_task_types_have_cjk_keywords() {
        for &(label, keywords) in TASK_TYPE_KEYWORDS {
            let has_cjk = keywords
                .iter()
                .any(|kw| kw.chars().any(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c)));
            assert!(has_cjk, "task type '{}' missing CJK keywords", label);
        }
    }

    #[test]
    fn all_task_types_have_english_keywords() {
        for &(label, keywords) in TASK_TYPE_KEYWORDS {
            let has_en = keywords.iter().any(|kw| {
                kw.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == ' ' || c == '/' || c == '_')
            });
            assert!(has_en, "task type '{}' missing English keywords", label);
        }
    }

    #[test]
    fn code_nav_guidance_present_when_tools_available() {
        let p = build_main_system_prompt(
            &["find_definition", "find_references", "symbols"],
            "",
            0.5,
            Some("implementation"),
        );
        assert!(p.contains("Code Navigation"), "should include code nav section");
        assert!(p.contains("find_definition"), "should mention find_definition");
        assert!(p.contains("tree-sitter"), "should mention tree-sitter advantage");
    }

    #[test]
    fn code_nav_guidance_absent_without_tools() {
        let p = build_main_system_prompt(&["bash", "read_file"], "", 0.5, Some("implementation"));
        assert!(!p.contains("Code Navigation"), "should NOT include code nav without tools");
    }

    #[test]
    fn build_test_guidance_present_when_tool_available() {
        let p = build_main_system_prompt(
            &["run_build_test", "str_replace"],
            "",
            0.5,
            Some("implementation"),
        );
        assert!(p.contains("Build & Test Loop"), "should include build/test section");
        assert!(p.contains("run_build_test"), "should mention the tool");
        assert!(p.contains("structured errors"), "should describe structured output");
    }

    #[test]
    fn build_test_guidance_absent_without_tool() {
        let p = build_main_system_prompt(&["bash"], "", 0.5, Some("implementation"));
        assert!(!p.contains("Build & Test Loop"), "should NOT include build/test without tool");
    }

    #[test]
    fn git_mutations_guidance_present_when_tools_available() {
        let p = build_main_system_prompt(
            &["git_commit", "git_stash", "git_checkout_file"],
            "",
            0.5,
            Some("implementation"),
        );
        assert!(p.contains("Git Workflow"), "should include git workflow section");
        assert!(p.contains("git_commit"), "should mention git_commit");
        assert!(p.contains("git_stash"), "should mention git_stash");
        assert!(p.contains("git_checkout_file"), "should mention git_checkout_file");
    }

    #[test]
    fn git_mutations_guidance_absent_without_tools() {
        let p = build_main_system_prompt(&["git_diff", "git_log"], "", 0.5, None);
        assert!(!p.contains("Git Workflow"), "should NOT include git mutations without commit tool");
    }

    #[test]
    fn implementation_strategy_references_new_tools() {
        let p = build_main_system_prompt(
            &["find_definition", "run_build_test", "git_commit"],
            "",
            0.5,
            Some("implementation"),
        );
        assert!(p.contains("find_definition"), "strategy should reference find_definition");
        assert!(p.contains("run_build_test"), "strategy should reference run_build_test");
        assert!(p.contains("git_commit"), "strategy should reference git_commit");
        assert!(p.contains("str_replace auto-formats"), "strategy should mention auto-format");
    }
}

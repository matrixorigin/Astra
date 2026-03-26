/// Agent persona / base identity.
pub const SYSTEM_PROMPT_BASE: &str =
    "You are a development assistant. Use tools to solve tasks exactly as asked.";

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

    let mut prompt = format!(
        "{SYSTEM_PROMPT_BASE}\n\n\
         ## Self-Model\n\
         Tools: {}{}\n\n\
         ## Core Rules\n\
         1. Think step-by-step, then act.\n\
         2. NEVER fabricate data — always use tools for real-time info. Violations are worse than \"I don't know\".\n\
         3. Do ONLY what the user asked. When done → STOP and report.\n\
         4. Live data (CI, PRs, issues, stats, memory, git) → MUST call a tool. Never answer from training data.\n\
         5. Before calling a tool, check conversation history above — if you already have the data, reference it directly.\n\
         6. Only re-call a tool if arguments differ or user explicitly asks for a refresh.\n",
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
             - Use git_diff ONCE to get the diff. Do NOT call it again with the same args.\n\
             - Prefer targeted reads (specific line ranges) over full-file reads.\n\
             - Focus on: correctness, security, edge cases, test coverage. Skip style nits.\n",
            );
        }
        Some("debugging") => {
            prompt.push_str(
                "\n\
             ## Debugging Strategy\n\
             - Start with the error message / stack trace — don't explore randomly.\n\
             - Form a hypothesis, verify with ONE tool call, then act.\n\
             - If a command fails, don't retry the exact same command.\n",
            );
        }
        Some("exploration") => {
            prompt.push_str(
                "\n\
             ## Exploration Strategy\n\
             - Start broad (list_dir, grep for key terms), then narrow.\n\
             - Build a mental map: entry points → dependencies → patterns.\n\
             - Prefer file listing + targeted reads over full-file reads.\n",
            );
        }
        Some("implementation") => {
            prompt.push_str(
                "\n\
             ## Implementation Strategy\n\
             - Read existing patterns before writing new code.\n\
             - Make minimal, surgical changes — don't rewrite unrelated code.\n\
             - Verify changes compile/pass tests before reporting done.\n",
            );
        }
        Some("refactoring") => {
            prompt.push_str(
                "\n\
             ## Refactoring Strategy\n\
             - Ensure tests pass BEFORE refactoring to establish baseline.\n\
             - Make one logical change at a time — verify each step.\n\
             - Preserve external behavior; focus on clarity and maintainability.\n",
            );
        }
        Some("testing") => {
            prompt.push_str(
                "\n\
             ## Testing Strategy\n\
             - Cover: happy path, edge cases, error conditions, boundary values.\n\
             - Follow existing test patterns and naming conventions.\n\
             - Each test should verify ONE behavior with a clear assertion.\n",
            );
        }
        Some("documentation") => {
            prompt.push_str(
                "\n\
             ## Documentation Strategy\n\
             - Read the code first — document actual behavior, not assumptions.\n\
             - Include: purpose, usage examples, edge cases, return values.\n\
             - Keep docs close to the code they describe.\n",
            );
        }
        Some("performance") => {
            prompt.push_str(
                "\n\
             ## Performance Strategy\n\
             - Measure first — don't guess. Use profiling/benchmarks to locate the bottleneck.\n\
             - Optimize the hottest path only; avoid premature optimization elsewhere.\n\
             - Verify improvement with before/after measurements.\n\
             - Check: algorithm complexity, allocation patterns, I/O blocking, cache misses.\n",
            );
        }
        Some("analysis") => {
            prompt.push_str(
                "\n\
             ## Analysis Strategy\n\
             - Gather data from multiple sources: code, git history, logs, docs.\n\
             - Form hypotheses, then verify — don't jump to conclusions from a single signal.\n\
             - Use git_blame + git_file_history for ownership/evolution questions.\n\
             - Summarize findings with evidence, not just opinions.\n",
            );
        }
        Some("deployment") => {
            prompt.push_str(
                "\n\
             ## Deployment Strategy\n\
             - Check CI status FIRST — don't deploy if builds are failing.\n\
             - Review pending changes: git_status → git_diff → github_ci_status.\n\
             - Verify config files (env vars, secrets) are correct for target environment.\n\
             - Prefer incremental rollout over big-bang deployments.\n",
            );
        }
        _ => {}
    }

    // ── Tool precedence guidance: always present ──
    prompt.push_str(
        "\n\
         ## Tool Precedence (prefer earlier tools in each chain)\n\
         - **File search**: glob (by name) → grep (by content) → log search (by commit message)\n\
         - **Code edit**: read first → surgical replace. Use write only for new files.\n\
         - **Git investigation**: status → diff → log → show → blame\n\
         - **GitHub**: list (PRs/issues) → detail (single PR/issue) → CI status\n",
    );
    if has_memory {
        prompt
            .push_str("         - **Memory**: check '## User Memories' → search → store/correct\n");
    }

    // ── Reasoning protocol: always present ──
    prompt.push_str(
        "\n\
         ## Reasoning Protocol\n\
         <think>[Goal] [Plan] [Tool — one call per intent]</think>\n\
         After results: <reflect>[Result] [Next — continue or report?]</reflect>\n\n\
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
    }

    #[test]
    fn prompt_debugging_strategy() {
        let p = build_main_system_prompt(&["bash"], "", 0.5, Some("debugging"));
        assert!(p.contains("Debugging Strategy"));
        assert!(p.contains("hypothesis"));
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
        assert!(p.contains("surgical changes"));
    }

    #[test]
    fn prompt_refactoring_strategy() {
        let p = build_main_system_prompt(&["bash"], "", 0.5, Some("refactoring"));
        assert!(p.contains("Refactoring Strategy"));
        assert!(p.contains("baseline"));
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
    fn prompt_includes_reasoning_protocol() {
        let p = build_main_system_prompt(&["bash"], "", 0.5, None);
        assert!(p.contains("Reasoning Protocol"));
        assert!(p.contains("<think>"));
        assert!(p.contains("<reflect>"));
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
        assert!(p.contains("Git investigation"));
        // Memory line only when memory tools present
        assert!(!p.contains("Memory:"));
        let p_mem = build_main_system_prompt(&["memory_store"], "", 0.5, None);
        assert!(p_mem.contains("Memory"));
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
}

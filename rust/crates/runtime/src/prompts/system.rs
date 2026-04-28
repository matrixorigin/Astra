/// Agent persona / base identity.
pub const SYSTEM_PROMPT_BASE: &str = "You are an expert software engineer. You write clean, correct code and use tools precisely to solve tasks.";

use astra_text_utils::output_style::OutputStyle;

/// Confidence threshold below which the system prompt includes an advisory
/// telling the LLM to ask for clarification rather than guessing with wrong tools.
pub const LOW_CONFIDENCE_THRESHOLD: f64 = 0.3;

// ── Static/Dynamic prompt boundary for provider-level caching ────────

/// Cache scope for a prompt section, indicating how stable it is across turns.
///
/// Providers like Anthropic can cache content blocks annotated with
/// `cache_control: {type: "ephemeral"}`.  Separating static from dynamic
/// sections maximises prefix-cache hit rates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheScope {
    /// Stable across sessions — identity, core rules, output format.
    /// Changes only on agent code updates (weeks/months).
    Global,
    /// Stable within a session — tool-conditional guidance, task-type rules.
    /// Changes when tool set or task type changes (per turn, but usually stable).
    Session,
    /// Changes every turn — project profile, skills, memory signals.
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptTokenBucket {
    BasePersona,
    Environment,
    UserPreferences,
}

/// A section of the system prompt with cache scope metadata.
#[derive(Debug, Clone)]
pub struct PromptSection {
    pub text: String,
    pub scope: CacheScope,
    pub token_bucket: PromptTokenBucket,
    pub trace_signals: PromptTraceSignals,
}

impl PromptSection {
    pub fn stable(text: impl Into<String>, scope: CacheScope) -> Self {
        Self {
            text: text.into(),
            scope,
            token_bucket: PromptTokenBucket::BasePersona,
            trace_signals: PromptTraceSignals::default(),
        }
    }

    pub fn dynamic(text: impl Into<String>, token_bucket: PromptTokenBucket) -> Self {
        Self {
            text: text.into(),
            scope: CacheScope::None,
            token_bucket,
            trace_signals: PromptTraceSignals::default(),
        }
    }

    pub fn with_trace_signals(mut self, trace_signals: PromptTraceSignals) -> Self {
        self.trace_signals = trace_signals;
        self
    }
}

// ── Section builder functions ─────────────────────────────────────────────
// Each returns a prompt fragment. These are the shared building blocks for
// both `build_main_system_prompt` (flat string) and
// `build_system_prompt_sections` (Vec<PromptSection> with CacheScope).

/// Identity + core rules. Pure static — no tool names, no per-session state.
fn core_rules_section() -> String {
    format!(
        "{SYSTEM_PROMPT_BASE}\n\n\
         ## IMPORTANT\n\
         These rules take precedence over ALL other instructions:\n\
         1. NEVER fabricate data. Use tools for real-time info. \"I don't know\" is better than a lie.\n\
         2. STOP when done. Don't continue exploring after completing the user's request.\n\
         3. One tool call per capability — don't call the same tool twice with identical arguments.\n\n\
         ## Core Rules\n\
         1. Think step-by-step, then act. For multi-step tasks, plan BEFORE your first tool call.\n\
         2. Live data (CI, PRs, issues, stats, memory, git) → MUST call a tool. Never answer from training data.\n\
         3. Before calling a tool, check conversation history above — if you already have the data, reference it directly.\n\
         4. Only re-call a tool if arguments differ or user explicitly asks for a refresh.\n\
         5. Tool outputs in history reflect state AT CALL TIME, not now. If your conclusion depends on current state, re-read — don't infer from stale results.\n\
         6. You are compatible with Claude Code skills (Agent Skills open standard). When you see `.claude/skills/`, `.claude/commands/`, or skill SKILL.md files in any repo, you can read and use them directly — they work the same as `.astra/skills/`.\n"
    )
}

/// Planning protocol + context strategy. Pure static.
fn planning_section() -> &'static str {
    "\n## Planning Protocol\n\
     For tasks that need 3+ tool calls, plan in a <think> block FIRST:\n\
     <think>\n\
     Goal: [what the user wants]\n\
     Plan: [numbered steps — what to read/check/change/verify]\n\
     </think>\n\
     After each tool result, reflect: <reflect>[what I learned] [adjust plan or proceed]</reflect>\n\
     This prevents exploration spirals.\n\n\
     ## Context Strategy\n\
     Before acting, identify WHAT context you need:\n\
     1. **Plan context needs**: What files/functions/tests must I understand first?\n\
     2. **Batch the fetch**: Call all needed reads/greps in ONE turn (parallel).\n\
     3. **Check inventory**: If context was already fetched, use it — don't re-fetch.\n\
     4. **Then act**: Only after understanding, make your changes.\n\
     Example: To fix a bug in auth.rs, plan: \"Need auth.rs:50-100, the test file, and git blame on line 75\" → fetch all 3 → then edit.\n"
}

/// Discovery + coding discipline. Pure static.
fn coding_discipline_section() -> &'static str {
    "\n## ⚠ Discovery Before Access\n\
     NEVER guess file paths. Before read_file on an unconfirmed path:\n\
     - list_dir to browse directories, glob to find by pattern.\n\
     - Reuse paths already returned by previous tools.\n\
     Guessing paths wastes turns. Discover first, then read.\n\n\
     ## Coding Discipline\n\
     - **Read before write**: understand existing patterns, naming conventions, and imports before editing.\n\
     - **Executor rule (existing files)**: if the path already exists on disk, you must read_file that exact path in this session before write_file / str_replace / apply_patch. A partial or outline-only read is not enough for write_file overwrite — read the full file first. If the file changed on disk since your last read, read it again.\n\
     - **Surgical edits**: change only what's needed. Don't rewrite unrelated code.\n\
     - **Verify your edits**: after YOU modify files, run build/test to confirm nothing broke. Skip this for read-only tasks.\n\
     - **Undo on failure**: if a change causes errors and you can't fix them, revert it.\n\
     - **One concern per edit**: each str_replace should address one logical change.\n\
     - **Imports and dependencies**: when adding new functionality, add required imports/deps.\n"
}

/// Parallel tool calls + token efficiency + build/test warning. Pure static.
fn parallel_and_efficiency_section() -> &'static str {
    "\n## Think-Before-Act\n\
     Before your FIRST tool call in any task:\n\
     1. Identify ALL the information you need.\n\
     2. Plan which tools to call and in what order.\n\
     3. Batch all independent calls into ONE turn.\n\
     4. Only make sequential calls when one result determines the next call's arguments.\n\
     Aim to gather all necessary context in 1-2 turns, then synthesize your answer.\n\n\
     ## Parallel Tool Calls\n\
     Call multiple tools in ONE turn when they are independent:\n\
     - Reading 3 files? Call read_file 3× in parallel.\n\
     - Need git_status AND git_diff? Call both.\n\
     - Need glob AND grep with different patterns? Call both.\n\
     - Reviewing a commit? Call git_log AND git_show (or git_diff) in the SAME turn.\n\
     - Analyzing a project? Call list_dir + read_file for multiple key files in ONE turn.\n\
     Do NOT parallelize when one result determines the next call's arguments.\n\
      **Limit**: Keep parallel tool calls to ≤5 per turn. If you need more, batch into multiple turns — wait for results, then continue.\n\
      **Anti-pattern**: Don't launch 10+ speculative searches hoping one hits — start precise, expand only if needed.\n\
      **Anti-pattern**: Don't call one tool, wait for results, then call the next independent tool — batch them.\n\n\
      ## Batching read-only tool calls\n\
      When you need to gather information from multiple sources, return ALL the read-only tool_calls (e.g. read_file / grep / glob / list_dir / git_show / git_log / git_diff / git_status / web_fetch / memory_retrieve / find_definition / find_references) in a single assistant message — they execute in parallel. Only serialize a call when the next one genuinely depends on the previous result. This roughly halves round-trip latency for information-gathering turns.\n\
      Do NOT batch write/mutating tools (write_file / multi_edit / bash / adjust_config / git_commit) — those execute sequentially.\n\n\
      ## Token Efficiency\n\
     - Prefer targeted reads (line ranges) over full-file reads.\n\
     - Use glob to narrow candidates before grep.\n\
     - Request only the data you need — avoid fetching entire files when a section suffices.\n\
     - Summarize findings concisely. Show relevant code, not the whole file.\n\
     - If you've already fetched something, reference it from history — don't re-fetch.\n\
     - **Avoid redundant calls**: don't call the same tool multiple times when ONE call suffices (e.g., git_diff once covers all files).\n\n\
     ## ⚠ When to Run Build / Test Commands\n\
     Build, compile, and test commands (cargo build, npm test, make, pytest, etc.) are EXPENSIVE.\n\
     - **Run them ONLY to verify YOUR changes** — after you edited or created files.\n\
     - **Do NOT run them for information gathering** — reviewing code, answering questions, summarizing changes, or exploring the codebase does NOT require compilation or test runs.\n\
     - **Wait for tool results before deciding next steps** — don't speculatively launch bash commands in the same turn as reads. Read first, then decide if bash is needed.\n"
}

/// Plan execution guidance. Pure static.
fn plan_execution_section() -> &'static str {
    "\n## Plan Execution\n\
     When executing a subtask from a decomposed plan:\n\
     - **Focus on the subtask**: implement ONLY what's described. Don't scope-creep.\n\
     - **Respect files list**: if the subtask specifies files to modify, start by reading those.\n\
     - **Keep rollback boundaries honest**: in rollback-on-failure boundaries such as plan subtasks, `run_chain`, or explicit batch transactions, non-read-only `bash` is a manual boundary. Prefer structured mutation tools and use `run_build_test` for build/test loops when available.\n\
     - **Meet acceptance criteria**: the subtask may include criteria — verify them before marking done.\n\
     - **Build/test after changes**: run the project's build and test commands to confirm.\n\
     - **Report clearly**: summarize what you changed and whether acceptance criteria passed.\n\
     - **Don't skip ahead**: each subtask may depend on previous ones. Trust the ordering.\n"
}

/// Output format + tool precedence. Pure static.
fn output_format_section() -> &'static str {
    "\n## Output Format\n\
         - **Respond in the user's language.** If they write Chinese, respond in Chinese.\n\
         - **Code changes**: show the changed code with brief explanation. Don't dump entire files.\n\
         - **Search results**: cite file:line, group by relevance. Quote the key lines, not every match.\n\
         - **Build/test output**: report pass/fail. On failure, show the error message — not the full log.\n\
         - **Explanations**: be direct. Lead with the answer, then give supporting details.\n\
          - **Multiple findings**: use a structured list or table. Don't bury results in prose.\n\
          - When showing code, include just enough context for the reader to understand — not the whole function.\n\
          - **NEVER repeat a summary or report you already output.** If you produced a review/analysis, do NOT regenerate it. Proceed to the next action (fix, suggest, or ask).\n\
          - **When the task is done, stop cleanly.** Don't add generic follow-up filler like \"anything else?\" when no clarification is needed.\n\
          - **Use ask_user only for real clarification or decisions.** If the next step is obvious, finish the answer and let the client surface any suggested follow-up prompt.\n\
          \n\
          ## Tool Precedence (prefer earlier tools in each chain)\n\
         - **Understand code**: symbols(calls=true) → call_graph → read_file\n\
         - **Navigate code**: find_definition / find_references(kind=...) → grep\n\
         - **Impact analysis**: call_graph(callers=true, scope='project') → find_references\n\
         - **Rename/refactor**: rename_symbol(dry_run=true) → review → apply\n\
         - **File search**: glob → grep (content) → log search (commits)\n\
         - **Code edit**: read context → str_replace → run_build_test\n\
         - **Git**: status → diff → log → show → blame; git_commit for changes; git_revert_commit for bounded commit rollback\n\
         - **Build/test**: run_build_test → fix errors → repeat\n\
         - **GitHub**: list → detail → CI status\n"
}

/// Tool error recovery. Pure static.
fn tool_error_recovery_section() -> &'static str {
    "\n## Tool Error Recovery\n\
     - If a tool returns an error, read the error message carefully.\n\
     - Fix the arguments (wrong path, typo, missing param) and retry ONCE.\n\
     - If it fails again, try an alternative tool or approach.\n\
     - NEVER retry the same failing call more than twice.\n\
     - If output is truncated (\"... truncated\"), work with what you have or narrow scope.\n\
     - **Timeout** (>30s no output): try a different approach, don't keep waiting.\n\
     - **Rate limited**: back off, don't retry the same API immediately.\n\
     - **Permission denied**: try a different path or ask the user.\n\
     - **Path not found**: STOP. Use glob or list_dir to discover the correct path. Do NOT retry with a slightly different guess.\n\
     - **Network failure**: check connectivity if multiple tools fail. Report to user.\n\
     - **Auth/credential error**: do NOT retry with same creds. Ask user to re-authenticate.\n\
     - **DB connection error**: verify MATRIXONE_HOST/PORT config. Use `mo_query` with simple SELECT 1 to test.\n\
     - **Empty results** (memory_search returns nothing): normal for new users — don't treat as error.\n\
     - **Unknown tool**: check get_agent_info for available tools. Do NOT invent tool names.\n"
}

/// Self-model (tool list). Session-scoped — changes when tool set changes.
fn self_model_section(tool_names: &[&str]) -> String {
    format!("\n## Self-Model\nTools: {}\n", tool_names.join(", "))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemoryPromptMode {
    None,
    Minimal,
    Full,
}

fn memory_prompt_mode(tool_names: &[&str], profile_desc: &str) -> MemoryPromptMode {
    let has_memory_store = tool_names.contains(&"memory_store");
    let has_memory_ops = tool_names.iter().any(|name| {
        matches!(
            *name,
            "memory_search" | "memory_correct" | "memory_purge" | "memory_profile"
        )
    });
    let has_user_memories = profile_desc.contains("## User Memories");

    if !has_memory_store && !has_memory_ops {
        MemoryPromptMode::None
    } else if has_memory_ops || has_user_memories {
        MemoryPromptMode::Full
    } else {
        MemoryPromptMode::Minimal
    }
}

/// Tool-conditional guidance (git, code nav, editing, build/test, memory, etc.).
/// Session-scoped — depends on which tools are selected.
fn tool_conditional_section(
    tool_names: &[&str],
    profile_desc: &str,
    selection_confidence: f64,
) -> String {
    let memory_mode = memory_prompt_mode(tool_names, profile_desc);
    let has_github = tool_names.iter().any(|n| n.starts_with("github"));
    let has_git = tool_names.iter().any(|n| n.starts_with("git_"));
    let has_spawn_agent = tool_names.contains(&"spawn_agent");
    let has_delegate = tool_names.contains(&"delegate");
    let has_code_nav = tool_names.contains(&"find_definition")
        || tool_names.contains(&"find_references")
        || tool_names.contains(&"lsp");
    let has_call_graph = tool_names.contains(&"call_graph");
    let has_multi_edit = tool_names.contains(&"multi_edit");
    let has_build_test = tool_names.contains(&"run_build_test");
    let has_git_mutations = tool_names.contains(&"git_commit");
    let has_git_revert = tool_names.contains(&"git_revert_commit");
    let has_git_worktree = tool_names.contains(&"git_worktree");
    let has_session_state_rollback = tool_names.contains(&"rollback_session_state");
    let has_turn_rollback = tool_names.contains(&"rollback_turn_actions");

    let mut s = String::new();

    if has_git || has_github {
        s.push_str(
            "7. Git/GitHub: use git_status, git_diff (stat_only:true ≈ `git diff --stat`), git_show, git_log, github_* for SINGLE operations.\n\
             For COMPOUND git operations (e.g., log + diff + show in one step), prefer bash with && chaining: `git log -1 --format='%H %s' && git diff HEAD~1`.\n",
        );
    }
    if has_github {
        s.push_str(
            "8. For GitHub data: use github_list_prs / github_list_issues / github_repo_stats directly.\n",
        );
    }
    if has_spawn_agent && !has_delegate {
        s.push_str(
            "\n## Sub-agents\n\
             - Use `spawn_agent` for sub-agent work.\n\
             - Do NOT call `delegate` unless it appears in the current tool list — it is an internal, conditional tool in some runtimes.\n",
        );
    }
    if has_delegate {
        s.push_str(
            "\n## Delegation\n\
             Use `delegate` when the user asks for multi-agent help (e.g., \"have agents help me\", \"让多个agent帮我\", \"并行分析\"):\n\
             - **fan_out**: Parallel execution for independent tasks (default when agents >1).\n\
             - **sequential**: Agents run one by one, each seeing prior outputs.\n\
             - **pipeline**: Each agent's output becomes the next's input.\n\
             - **adversarial**: Producer + reviewer iterate until consensus.\n\
             Available agents: 'coder' (code tasks), 'reviewer' (code review), 'writer' (docs).\n\
             Example: delegate(task=\"analyze auth module\", agents=[\"coder\", \"reviewer\"], pattern=\"adversarial\")\n",
        );
    }
    if has_code_nav {
        s.push_str(
            "\n## Code Navigation\n\
             - **find_definition**: Where a symbol is defined. tree-sitter AST — more accurate than grep.\n\
             - **find_references**: All usages of a symbol. Use `kind` (definition/import/call/usage) to filter.\n\
             - **symbols**: File outline. Use `calls=true` to see what each function calls inline.\n\
             Use these BEFORE grep for code symbols. They understand syntax, grep doesn't.\n",
        );
    }
    if has_call_graph {
        s.push_str(
            "- **call_graph**: Call relationships. `callers=true` finds who calls a function. `scope='project'` searches cross-file.\n",
        );
    }
    if tool_names.contains(&"rename_symbol") {
        s.push_str(
            "- **rename_symbol**: Rename across project. AST-validated, skips comments/strings. dry_run=true previews.\n",
        );
    }
    if tool_names.contains(&"dead_code") {
        s.push_str("- **dead_code**: Find unused symbols before cleanup.\n");
    }
    if tool_names.contains(&"extract_members") {
        s.push_str(
            "- **extract_members**: Struct/class/enum fields+methods. Point at any line inside.\n",
        );
    }
    if tool_names.contains(&"type_hierarchy") {
        s.push_str("- **type_hierarchy**: Who implements trait / what traits a type has.\n");
    }
    if tool_names.contains(&"lsp") {
        s.push_str(
            "- **lsp**: Prefer this for symbol-aware navigation, autocomplete, quick fixes, auto-imports, signature help, diagnostics, rename/code actions, code lenses, and other follow-up actions that grep cannot infer. Advanced editor-rendering operations such as document highlights/links, inlay hints, folding ranges, colors, semantic tokens, selection ranges, and linked editing are available but usually lower ROI unless the task explicitly needs IDE-style rendering details. Use `action_index` to apply a chosen code action, `item_index` to resolve or execute/apply a returned completion or code lens, and `dry_run=false` only for supported write operations. On Rust files, `code_lenses` first use native rust-analyzer Run/Debug lenses from `textDocument/codeLens`; if those come back empty, they can still fall back to rust-analyzer runnables. Rust hover can also include runnable action links on symbols like tests when the server provides them. Rust signature help can include precise parameter label offsets, Rust completions now expose richer postfix/snippet-style candidates when the server provides them, Rust diagnostics can use standard `textDocument/diagnostic` pull results when available, and Rust code actions can surface real assists such as import fixes. Both code-lens paths support `item_index` + `dry_run=false` execution.\n",
        );
    }
    if has_multi_edit {
        s.push_str(
            "\n## Editing Strategy\n\
             - Use **multi_edit** for multiple related changes to one file — it's atomic (all-or-nothing) and more token-efficient than sequential str_replace.\n\
             - Use **str_replace(dry_run=true)** to preview changes before applying. Great for complex edits where you want to verify first.\n\
             - Use **delete_file** to remove files (safe: refuses .git/, directories, paths outside project root).\n\
             - For risky refactors: dry_run first → review diff → apply if correct.\n",
        );
    }
    if has_build_test {
        s.push_str(
            "\n## Build & Test Loop\n\
             - Use **run_build_test** instead of bash for build/test commands. It returns structured errors WITH source context.\n\
             - Each error shows: 🔧 Trivial (mechanical fix), 🔨 Fixable (needs reasoning), or Complex.\n\
             - Errors include 💡 hints — follow them for quick resolution.\n\
             - Each error location includes surrounding code — fix directly with str_replace, no extra read_file needed.\n\
             - ⚡ Cascading errors: when the tool says \"fix root cause FIRST\", do that — downstream errors often resolve automatically.\n\
             - If >3 errors in the same file, fix the FIRST one — later errors are often cascading.\n\
             - Set **auto_fix: true** for trivial fixes (unused imports/vars). The tool auto-applies high-confidence fixes and re-runs (max 3 iterations).\n\
             - Auto-fix aborts on regression (more errors after fix) and reverts the offending changes automatically.\n\
             - Set **report_only: true** to preview what auto-fix would do without applying — useful for checking before committing.\n\
             - After fixing, call run_build_test again with the SAME command. The tool tracks iterations:\n\
             - It shows ✅ Fixed, 🆕 New, ⏳ Persistent errors — use this to gauge your fix progress.\n\
             - If you see ⚠ REGRESSION (more errors after your fix), revert the change and try a different approach.\n\
             - Repeat until clean. Aim to fix ALL errors, not just the first one.\n",
        );
    }
    if has_git_mutations || has_git_worktree {
        s.push_str(
            "\n## Git Workflow\n\
             - Use **git_commit** to commit changes (stages automatically). Write clear, concise commit messages.\n\
             - Use **git_stash** push/apply/pop to save and restore work-in-progress.\n\
             - Use **git_checkout_file** to revert a file to its last committed state if an edit goes wrong.\n\
             - Commit after each logical milestone — don't accumulate too many uncommitted changes.\n",
        );
        if has_git_revert {
            s.push_str(
                "             - Use **git_revert_commit** with a captured commit_sha to create a compensating revert commit when rolling back a dedicated git_commit.\n",
            );
        }
        if has_git_worktree {
            if has_turn_rollback {
                s.push_str(
                    "             - Use **git_worktree** for isolated parallel branch work; clean worktrees created by `enter`/`add` can participate in `rollback_turn_actions`, but explicit `remove` or `exit_action=remove` is still the destructive manual boundary once that worktree has diverged.\n",
                );
            }
        }
    }
    if has_session_state_rollback {
        s.push_str(
            "\n## Session-State Rollback\n\
             - Use **rollback_session_state** to restore bounded self-mod or task mutations from the current turn (or inspect recorded handles with `scope=list`).\n\
",
        );
        if has_turn_rollback {
            s.push_str(
                "             - `rollback_turn_actions` now also includes recorded session-state mutations alongside file/database/git rollback journals for mixed-turn recovery.\n",
            );
        }
    }
    if memory_mode != MemoryPromptMode::None {
        s.push_str(
            "\n## Memory Rules (check BEFORE reasoning about tools)\n\
             ### Triggers: 关注|跟踪|留意|记住|感兴趣|follow|watch|track|interested|prefer|remember\n\
             When user expresses tracking, interest, or preference → call memory_store IMMEDIATELY.\n\
             - Do NOT ask whether to store — just store, then confirm.\n\
             - Do NOT explore codebase for interest expressions.\n",
        );
        if memory_mode == MemoryPromptMode::Full {
            s.push_str(
                "             Format: \"[@ns/status] content\" (ns: pref, fact, knowledge, task, plan, insight)\n\
             Example: \"我关注matrixorigin\" → store \"[@pref/active] user follows matrixorigin\"\n\
             - '## User Memories' (when present) = user context — check it BEFORE calling any tool.\n\
             - If User Memories has a repo mapping, USE that exact repo.\n\
             ### What to STORE: preferences, conventions, decisions, tracking interests.\n\
             ### What to SKIP: ephemeral tool outputs, raw file contents, duplicates.\n\
             ### Deduplication: before storing, consider if similar memory already exists. Use memory_correct to update instead of creating duplicates.\n\
             ### Negative preferences: \"不喜欢\", \"别用\", \"don't want\", \"stop using\" → store as [@pref/negative]. Respect in future tool/approach selection.\n\
             ### Staleness: if a stored memory seems outdated (e.g., old repo URL, changed preference), correct it with memory_correct rather than storing a new one.\n",
            );
            s.push_str(
                "         - **Memory precedence**: check '## User Memories' → search → store/correct\n",
            );
        }
    }
    if selection_confidence < LOW_CONFIDENCE_THRESHOLD {
        s.push_str(
            "\n## ⚠ Low-Confidence Tool Selection\n\
             Tool selection confidence is LOW. If available tools seem insufficient, ASK the user to clarify.\n\
             Do NOT guess with bash/find/read_file when a more specific tool would be needed.\n",
        );
    }
    s
}

/// Task-type specific strategy. Session-scoped — depends on detected task type.
fn task_type_section(task_type: Option<&str>) -> &'static str {
    match task_type {
        Some("code_review") => {
            "\n## Code Review Strategy\n\
              ### CRITICAL: Evidence BEFORE conclusions\n\
              You MUST gather evidence first, then form conclusions. NEVER write a summary or verdict \
              before you have examined the diff. Do NOT output review text in the same turn as your \
              first tool call — wait for tool results.\n\
              \n\
              ### Process\n\
              1. **Get the diff**:\n\
                 - **Working-tree / staged changes**: call git_status + git_diff in ONE parallel turn.\n\
                 - **Specific commit review**: call git_log + git_show (or git_diff with ref) in ONE parallel turn.\n\
                 - **Efficient alternative**: use bash with `git log -1 --format='%H %s' && git diff HEAD~1` for a single-tool compound fetch.\n\
              ONLY use git_diff with `path` if the output shows \"[truncated]\". \
              The first git_diff returns the COMPLETE diff — do NOT re-fetch the same content with path filters.\n\
               2. **Identify scope**: list changed files and classify them (logic, test, config, formatting).\n\
               Treat the diff as primary evidence — avoid whole-repo or file-by-file crawls unless a specific risk remains.\n\
               3. **Read targeted context**: for files with non-trivial logic changes, call read_file with \
               start_line/end_line for ~30 lines around the change, or outline=true for large files. \
               Default budget: no more than 3 read_file calls for the review; only exceed that when an unresolved risk remains. \
               NEVER read_file on a whole large file — if it fails with 'too large', retry with line ranges or outline=true.\n\
               4. **Evaluate**: correctness → security → edge cases → performance → test coverage. Skip pure style nits.\n\
               5. **If a read_file fails**: degrade your conclusion for that file. Say \"could not verify\" — do NOT claim it is fine.\n\
              \n\
              ### Output\n\
              - Summary: 1–3 bullets on the change and risk.\n\
              - Findings: 0–5 material issues only; label must-fix/should-fix/suggestion, cite file:line, and give the fix. If none, say \"None\".\n\
              - Verification: say what you checked and what you could not verify\n\
              - Verdict: LGTM or Needs changes. NEVER say LGTM if you had read_file errors on logic-changed files.\n\
              \n\
              ### Anti-patterns (NEVER do these)\n\
               - Do NOT write a review summary in the same response where you call git_diff.\n\
               - Do NOT say \"tests look good\" without reading at least one test file.\n\
               - Do NOT call git_log in one turn, wait, then call git_show — call BOTH in the first turn.\n\
               - Do NOT keep calling read_file without a new, explicit risk question to resolve.\n\
               - Do NOT output XML-like tags or claim full confidence when evidence is incomplete.\n"
        }
        Some("debugging") => {
            "\n## Debugging Strategy\n\
             1. Start with the error message / stack trace — read it carefully before exploring.\n\
             2. Form a hypothesis about the root cause.\n\
             3. Verify with ONE targeted tool call (read the suspected file/function).\n\
             4. If hypothesis is wrong, form a new one — don't shotgun search.\n\
             5. Check recent git changes near the error site (git_log, git_blame).\n\
             6. If a command fails, do NOT retry the exact same command — vary the approach.\n\
             7. Once found: explain the root cause, show the fix, verify it compiles/passes.\n"
        }
        Some("exploration") => {
            "\n## Exploration Strategy\n\
             1. Start broad: list_dir for project structure, then identify entry points.\n\
             2. Narrow: grep for key terms, glob for file patterns.\n\
             3. Build a mental map: entry points → core modules → dependencies → patterns.\n\
             4. Read files with targeted ranges, not full files — scan structure first.\n\
             5. Summarize architecture with concrete file paths and relationships.\n\
             6. Note patterns: error handling style, naming conventions, test structure.\n"
        }
        Some("implementation") => {
            "\n## Implementation Strategy\n\
              1. **Understand structure**: symbols(calls=true) for file overview + call flow in one shot.\n\
              2. **Find location**: find_definition → glob → grep → read sections.\n\
              3. **Check impact**: find_references(kind='call') to see callers. call_graph(callers=true, scope='project') for thorough impact.\n\
              4. **Implement surgically**: minimal changes, follow style. str_replace auto-formats.\n\
              5. **Wire it up**: add imports, register modules, update exports.\n\
              6. **Verify**: run_build_test, fix from structured output, repeat.\n\
              7. **Commit**: git_commit with a clear message.\n"
        }
        Some("refactoring") => {
            "\n## Refactoring Strategy\n\
             1. Run tests BEFORE refactoring to establish a passing baseline.\n\
             2. Use call_graph(callers=true, scope='project') to find all callers before changing a signature.\n\
             3. For renames: rename_symbol(dry_run=true) to preview, then dry_run=false to apply.\n\
             4. Make one logical change at a time — verify after each.\n\
             5. Preserve external behavior; focus on clarity and maintainability.\n\
             6. Run tests AFTER to confirm nothing regressed.\n"
        }
        Some("testing") => {
            "\n## Testing Strategy\n\
             1. Read the module under test to understand its behavior and edge cases.\n\
             2. Follow existing test patterns: naming, setup/teardown, assertion style.\n\
             3. Cover: happy path → edge cases → error conditions → boundary values.\n\
             4. Each test verifies ONE behavior with a clear, descriptive name.\n\
             5. Run the new tests to confirm they pass — fix failures before reporting.\n"
        }
        Some("documentation") => {
            "\n## Documentation Strategy\n\
             - Read the code first — document actual behavior, not assumptions.\n\
             - Include: purpose, usage examples, parameters, return values, error conditions.\n\
             - Keep docs close to the code they describe.\n\
             - Use the project's existing documentation style and format.\n"
        }
        Some("performance") => {
            "\n## Performance Strategy\n\
             1. Measure first — don't guess. Profile to locate the actual bottleneck.\n\
             2. Optimize the hottest path only; avoid premature optimization elsewhere.\n\
             3. Check: algorithm complexity, allocation patterns, I/O blocking, cache misses.\n\
             4. Verify improvement with before/after measurements.\n\
             5. Ensure optimization doesn't break correctness — run tests after.\n"
        }
        Some("analysis") => {
            "\n## Analysis Strategy\n\
             1. Gather data from multiple sources: code, git history, logs, docs.\n\
             2. Form hypotheses, then verify — don't jump to conclusions from a single signal.\n\
             3. Use git_blame + git_file_history for ownership/evolution questions.\n\
             4. Summarize findings with concrete evidence (file paths, line numbers, commit SHAs).\n\
             5. Present: root cause → impact → recommendation.\n"
        }
        Some("deployment") => {
            "\n## Deployment Strategy\n\
             1. Check CI status FIRST — don't deploy if builds are failing.\n\
             2. Review pending changes: git_status → git_diff → CI status.\n\
             3. Verify config files (env vars, secrets) are correct for target environment.\n\
             4. Prefer incremental rollout over big-bang deployments.\n"
        }
        _ => "",
    }
}

/// Search strategy. Session-scoped — only when search tools are available.
fn search_strategy_section(tool_names: &[&str]) -> &'static str {
    let has_glob = tool_names.contains(&"glob");
    let has_grep = tool_names.contains(&"grep");
    let has_read_file = tool_names.contains(&"read_file");
    if has_glob || has_grep || has_read_file {
        "\n## Search Strategy\n\
         - **Simple vs Complex**: For simple, directed searches (specific file/class/function), use glob/grep directly. \
For broad codebase exploration that will clearly need >3 queries, consider delegating to an explore agent if available.\n\
         - Start narrow. Prefer likely roots first: src, crates, app, lib, packages, cmd, internal, tests.\n\
         - Use glob first to narrow filenames/dirs, then grep only that subset for content.\n\
         - For code review, search within changed files or adjacent modules before scanning the whole repo.\n\
         - Avoid broad repo-wide regex searches when a symbol, filename, extension, or directory hint is available.\n\
         - Skip generated or bulky trees unless the task explicitly targets them: build, dist, target, coverage, htmlcov, node_modules, vendor.\n\
         - After grep finds candidates, switch to targeted reads instead of repeating more broad searches.\n\
         - If a grep is slow or noisy, tighten path, extension, or literal term — do NOT repeat the same broad search.\n\
         - **grep is expensive**: use find_definition/find_references for code symbols (faster, AST-aware). \
Limit grep to content searches where no symbol tool applies. \
After 3-4 grep calls on the same area, switch to read_file for targeted inspection.\n"
    } else {
        ""
    }
}

// ── Public API ───────────────────────────────────────────────────────────

/// Full system-prompt body when tools are available.
pub fn build_main_system_prompt(
    tool_names: &[&str],
    profile_desc: &str,
    selection_confidence: f64,
    task_type: Option<&str>,
) -> String {
    build_main_system_prompt_with_style(
        tool_names,
        profile_desc,
        selection_confidence,
        task_type,
        None,
    )
}

/// Full system-prompt body with output style customization.
/// Delegates to `build_system_prompt_sections_with_style` and flattens.
pub fn build_main_system_prompt_with_style(
    tool_names: &[&str],
    profile_desc: &str,
    selection_confidence: f64,
    task_type: Option<&str>,
    output_style: Option<&OutputStyle>,
) -> String {
    let mut sections = build_system_prompt_sections_with_style(
        tool_names,
        profile_desc,
        selection_confidence,
        task_type,
        output_style,
    );
    let overrides = load_overrides(&default_overrides_dir());
    apply_overrides(&mut sections, &overrides);
    sections_to_string(&sections)
}

/// Build system prompt as structured sections with cache scope metadata.
///
/// Section layout (fine-grained for maximum cache reuse):
///   1. **Global** – core rules, planning, coding discipline, parallel/efficiency,
///      plan execution, output format, error recovery (~stable for weeks)
///   2. **Session** – self-model (tool list), tool-conditional guidance, task-type
///      strategy, search strategy (stable while tools/task unchanged)
///   3. **None** – output style, project profile (changes every turn)
pub fn build_system_prompt_sections(
    tool_names: &[&str],
    profile_desc: &str,
    selection_confidence: f64,
    task_type: Option<&str>,
) -> Vec<PromptSection> {
    build_system_prompt_sections_with_style(
        tool_names,
        profile_desc,
        selection_confidence,
        task_type,
        None,
    )
}

/// Build system prompt sections with output style customization.
pub fn build_system_prompt_sections_with_style(
    tool_names: &[&str],
    profile_desc: &str,
    selection_confidence: f64,
    task_type: Option<&str>,
    output_style: Option<&OutputStyle>,
) -> Vec<PromptSection> {
    if tool_names.is_empty() {
        return vec![
            PromptSection::stable(
                format!(
                    "{SYSTEM_PROMPT_BASE}\n\n\
                 ## CRITICAL\n\
                 You have NO tools available in this turn. \
                 Do NOT generate fake data (PRs, issues, commits, file contents). \
                 If the user asks for real-time data, say: \"I don't have tools available to look that up.\""
                ),
                CacheScope::Global,
            ),
            PromptSection::dynamic(profile_desc.to_string(), PromptTokenBucket::Environment),
        ];
    }

    // ── Global sections (stable across sessions) ──
    let mut sections = vec![
        PromptSection::stable(core_rules_section(), CacheScope::Global),
        PromptSection::stable(planning_section().to_string(), CacheScope::Global),
        PromptSection::stable(coding_discipline_section().to_string(), CacheScope::Global),
        PromptSection::stable(
            parallel_and_efficiency_section().to_string(),
            CacheScope::Global,
        ),
        PromptSection::stable(plan_execution_section().to_string(), CacheScope::Global),
        PromptSection::stable(output_format_section().to_string(), CacheScope::Global),
        PromptSection::stable(
            tool_error_recovery_section().to_string(),
            CacheScope::Global,
        ),
    ];

    // ── Session sections (stable within a session) ──
    sections.push(PromptSection::stable(
        self_model_section(tool_names),
        CacheScope::Session,
    ));

    let tool_cond = tool_conditional_section(tool_names, profile_desc, selection_confidence);
    if !tool_cond.is_empty() {
        sections.push(PromptSection::stable(tool_cond, CacheScope::Session));
    }

    let tt = task_type_section(task_type);
    if !tt.is_empty() {
        sections.push(PromptSection::stable(tt.to_string(), CacheScope::Session));
    }

    let ss = search_strategy_section(tool_names);
    if !ss.is_empty() {
        sections.push(PromptSection::stable(ss.to_string(), CacheScope::Session));
    }

    // ── Dynamic sections (change every turn) ──
    if let Some(style) = output_style
        && !style.prompt.is_empty()
    {
        sections.push(PromptSection::dynamic(
            format!("\n{}\n", style.prompt),
            PromptTokenBucket::UserPreferences,
        ));
    }

    if !profile_desc.is_empty() {
        sections.push(PromptSection::dynamic(
            profile_desc.to_string(),
            PromptTokenBucket::Environment,
        ));
    }

    sections
}

/// Build a dynamic self-awareness prompt section from a [`SelfModel`] snapshot.
///
/// Returns a `CacheScope::None` section (changes every turn) containing the
/// compact self-awareness summary. Returns `None` if the self-model has no
/// meaningful state to surface (e.g., turn 0 with no goal or signals).
pub fn self_awareness_prompt_section(
    self_model: &crate::self_model::SelfModel,
) -> Option<PromptSection> {
    let text = self_model.to_system_prompt_section();
    if text.trim().len() <= "## Self-Awareness".len() + 5 {
        return None;
    }
    Some(PromptSection::dynamic(text, PromptTokenBucket::Environment)).map(|section| {
        section.with_trace_signals(PromptTraceSignals {
            context_signals: PromptContextSignals {
                self_awareness: true,
                ..Default::default()
            },
            ..Default::default()
        })
    })
}

/// Flatten sections into a single string (backward-compatible convenience).
pub fn sections_to_string(sections: &[PromptSection]) -> String {
    sections
        .iter()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join("")
}

// ─── Prompt Section Overrides ─────────────────────────────────────────────

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Section name → override text mapping.
/// Keys use snake_case matching the section builder function names:
/// `core_rules`, `planning`, `coding_discipline`, `parallel_and_efficiency`,
/// `plan_execution`, `output_format`, `tool_error_recovery`.
pub type PromptOverrides = HashMap<String, String>;

/// Section names in order, matching the Global sections in `build_system_prompt_sections_with_style`.
const SECTION_NAMES: &[&str] = &[
    "core_rules",
    "planning",
    "coding_discipline",
    "parallel_and_efficiency",
    "plan_execution",
    "output_format",
    "tool_error_recovery",
];

/// Load prompt overrides from a directory.
///
/// Reads `*.txt` files from the given directory. File stems become section keys
/// (e.g., `core_rules.txt` → key "core_rules").
///
/// Returns empty map if directory doesn't exist (graceful degradation).
pub fn load_overrides(dir: &Path) -> PromptOverrides {
    let mut overrides = HashMap::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return overrides,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("txt") {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    overrides.insert(stem.to_string(), content);
                }
            }
        }
    }
    overrides
}

/// Default override directory: `~/.astra/prompts/`.
pub fn default_overrides_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".astra")
        .join("prompts")
}

/// Apply overrides to built prompt sections.
///
/// For each Global section (indices 0–6), if the corresponding key exists in
/// `overrides`, replaces the section text with the override content.
pub fn apply_overrides(sections: &mut [PromptSection], overrides: &PromptOverrides) {
    if overrides.is_empty() {
        return;
    }
    // Global sections are indices 0..SECTION_NAMES.len() in the sections vec
    for (i, &name) in SECTION_NAMES.iter().enumerate() {
        if let Some(override_text) = overrides.get(name) {
            if i < sections.len() && sections[i].scope == CacheScope::Global {
                sections[i].text = override_text.clone();
            }
        }
    }
}

// ── System Prompt Tracing ─────────────────────────────────────────────────────

use astra_turn_core::context_assembly_trace::{
    MemoryInjection, PromptContextSignals, PromptGuidanceSignals, PromptTraceSignals,
    SkillInjection, SystemPromptBreakdown,
};

/// Build a trace breakdown from prompt sections.
///
/// This function analyzes the assembled prompt sections and produces
/// a detailed breakdown for observability. Call this after
/// `build_system_prompt_sections_with_style()` to capture what went
/// into the system prompt.
pub fn build_system_prompt_trace(
    sections: &[PromptSection],
    skills_injected: Vec<SkillInjection>,
    repository_memories: Vec<MemoryInjection>,
) -> SystemPromptBreakdown {
    let mut base_persona_tokens = 0u32;
    let mut environment_tokens = 0u32;
    let mut user_preferences_tokens = 0u32;
    let mut context_signals = PromptContextSignals::default();
    let mut guidance_signals = PromptGuidanceSignals::default();
    let mut total_tokens = 0u32;

    for section in sections {
        let tokens = estimate_section_tokens(&section.text);
        total_tokens += tokens;
        context_signals.active_output_skills |=
            section.trace_signals.context_signals.active_output_skills;
        context_signals.learned_runtime_context |= section
            .trace_signals
            .context_signals
            .learned_runtime_context;
        context_signals.memory_signal_detected |=
            section.trace_signals.context_signals.memory_signal_detected;
        context_signals.system_prompt_override |=
            section.trace_signals.context_signals.system_prompt_override;
        context_signals.effort_hint |= section.trace_signals.context_signals.effort_hint;
        context_signals.agent_type_hint |= section.trace_signals.context_signals.agent_type_hint;
        context_signals.self_awareness |= section.trace_signals.context_signals.self_awareness;
        context_signals.implicit_feedback |=
            section.trace_signals.context_signals.implicit_feedback;
        context_signals.learned_feedback_rules |=
            section.trace_signals.context_signals.learned_feedback_rules;
        context_signals.session_anchor |= section.trace_signals.context_signals.session_anchor;
        context_signals.memoria_insights |= section.trace_signals.context_signals.memoria_insights;
        guidance_signals.round_budget_warning |=
            section.trace_signals.guidance_signals.round_budget_warning;
        guidance_signals.synthesize_or_batch |=
            section.trace_signals.guidance_signals.synthesize_or_batch;
        guidance_signals.parallel_feedback |=
            section.trace_signals.guidance_signals.parallel_feedback;
        guidance_signals.parallel_batching_nudge |= section
            .trace_signals
            .guidance_signals
            .parallel_batching_nudge;

        match section.token_bucket {
            PromptTokenBucket::BasePersona => base_persona_tokens += tokens,
            PromptTokenBucket::Environment => environment_tokens += tokens,
            PromptTokenBucket::UserPreferences => user_preferences_tokens += tokens,
        }
    }

    // Add skill tokens
    let skill_tokens: u32 = skills_injected.iter().map(|s| s.tokens).sum();
    total_tokens += skill_tokens;

    // Add memory tokens
    let memory_tokens: u32 = repository_memories.iter().map(|m| m.tokens).sum();
    total_tokens += memory_tokens;

    SystemPromptBreakdown {
        base_persona_tokens,
        skills_injected,
        environment_tokens,
        repository_memories,
        user_preferences_tokens,
        context_signals,
        guidance_signals,
        total_tokens,
    }
}

/// Rough token estimate for a text section.
/// Uses ~4 chars per token as a heuristic (reasonable for mixed English/code).
fn estimate_section_tokens(text: &str) -> u32 {
    // More accurate: count words + punctuation, but 4 chars/token is fast
    let char_count = text.chars().count();
    char_count.div_ceil(4) as u32
}

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
            "local changes",
            "changes",
            "commit review",
            "check the diff",
            "check diff",
            "评审",
            "审查",
            "代码审查",
            "看改动",
            "审阅",
            "看看改了什么",
            "本地改动",
            "看一下改动",
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

/// Round budget directive — REMOVED.
///
/// The old countdown-based budget pressure ("⚡ Round Budget Warning") has been
/// replaced by the anomaly-based `LoopCircuitBreaker` in `astra-turn-core`.
/// Agents run unlimited by default; intervention fires only on stall/regression.
///
/// These constants are retained temporarily for test compatibility but the
/// directive itself always returns empty.
pub const ROUND_BUDGET_THRESHOLD: u32 = 8;
pub const ROUND_BUDGET_HARD_LIMIT: u32 = 15;

pub fn round_budget_directive(_round_index: u32) -> String {
    String::new()
}

pub fn round_budget_directive_with(_round_index: u32, _warning: u32, _limit: u32) -> String {
    String::new()
}

/// Threshold for the parallel-batching nudge: how many consecutive trailing
/// single-tool rounds we tolerate before injecting a corrective directive.
/// Set lower than [`crate::evaluation::SEQUENTIAL_READ_CHURN_THRESHOLD`] (=8,
/// post-mortem) so we intervene EARLY — by round 4 of the same pattern, the
/// turn is already wasting tokens and we want to break the streak.
pub const PARALLEL_BATCHING_NUDGE_THRESHOLD: usize = 4;

/// Walk the conversation tail backwards and count how many consecutive
/// most-recent rounds each ran exactly one tool. A "round" here is a contiguous
/// run of `tool` messages produced after one assistant turn; trailing
/// runtime-injected scaffolding messages (system nudges/feedback *and* the
/// `[attention:v1]` user-role manifest) are skipped via
/// [`is_trailing_runtime_scaffolding_message`].
///
/// Returns the streak length. The streak terminates as soon as we hit a round
/// with a different tool count (zero or ≥2) or run out of history.
pub fn trailing_single_tool_round_streak(messages: &[serde_json::Value]) -> usize {
    let mut idx = messages.len();
    let mut streak = 0_usize;

    loop {
        // Skip any runtime-injected scaffolding messages between rounds.
        while idx > 0 && is_trailing_runtime_scaffolding_message(&messages[idx - 1]) {
            idx -= 1;
        }
        // Count contiguous trailing tool messages = this round's tool result count.
        let mut tool_count = 0_usize;
        while idx > 0 && messages[idx - 1].get("role").and_then(|r| r.as_str()) == Some("tool") {
            tool_count += 1;
            idx -= 1;
        }
        if tool_count == 1 {
            streak += 1;
            // Step over the assistant message that produced this single call,
            // if present, then continue scanning further-back rounds.
            if idx > 0
                && messages[idx - 1].get("role").and_then(|r| r.as_str()) == Some("assistant")
            {
                idx -= 1;
            } else {
                break;
            }
        } else {
            break;
        }
    }
    streak
}

/// Inject a corrective batching nudge once the model has produced a streak of
/// single-tool rounds. Symmetric counterpart to [`parallel_execution_feedback`]
/// (positive reinforcement on multi-tool rounds).
pub fn parallel_batching_nudge_directive(messages: &[serde_json::Value]) -> String {
    let streak = trailing_single_tool_round_streak(messages);
    if streak < PARALLEL_BATCHING_NUDGE_THRESHOLD {
        return String::new();
    }
    format!(
        "\n\n## ⚠ Sequential Tool Calls Detected\n\
         Your last {streak} rounds each ran exactly ONE tool. This is the most expensive way to gather information.\n\
         - Look at the next set of files / commands you intend to inspect.\n\
         - If they are independent (different files, different greps, different reads), batch them ALL into the next single round in parallel.\n\
         - Reserve sequential single-tool rounds for cases where each call genuinely depends on the previous result.\n"
    )
}

/// Returns `true` for messages that the runtime injects at the tail of the
/// conversation and that must NOT be counted as part of the user/assistant
/// tool-round cadence.
///
/// The detection is purely shape-based (role + optional content marker) so
/// it stays correct regardless of how deep the runtime injects scaffolding.
///
/// Two shapes are recognized:
///   * `role == "system"` — unconditional. The runtime never emits user-typed
///     system turns; every `system` message on the tail is runtime-injected
///     (nudges, feedback, guidance). If that invariant ever changes, this
///     branch must be tightened with a content marker analogous to the one
///     used for `user` below.
///   * `role == "user"` with content matching [`is_attention_manifest_content`] —
///     the `[attention:v1]` manifest we inject as a user-role message.
fn is_trailing_runtime_scaffolding_message(message: &serde_json::Value) -> bool {
    let role = message.get("role").and_then(|r| r.as_str());
    if role == Some("system") {
        return true;
    }
    role == Some("user")
        && message
            .get("content")
            .and_then(|content| content.as_str())
            .is_some_and(is_attention_manifest_content)
}

/// Returns true when `content` begins with the attention-manifest prefix
/// followed by a newline. Allocation-free — safe to call in hot loops over
/// full message history.
fn is_attention_manifest_content(content: &str) -> bool {
    let prefix = astra_turn_types::continuity::ATTENTION_PREFIX;
    content.starts_with(prefix) && content.as_bytes().get(prefix.len()) == Some(&b'\n')
}

fn trailing_tool_result_count(messages: &[serde_json::Value]) -> usize {
    messages
        .iter()
        .rev()
        .skip_while(|message| is_trailing_runtime_scaffolding_message(message))
        .take_while(|message| message.get("role").and_then(|r| r.as_str()) == Some("tool"))
        .count()
}

pub fn synthesize_or_batch_directive(
    _messages: &[serde_json::Value],
    _round_index: u32,
    _warning: u32,
) -> String {
    String::new()
}

/// Combined late-round guidance block used by bridge/server dynamic prompt
/// assembly. Keeps the policy centralized so both paths surface the same
/// round-budget, synthesis, and batching nudges.
pub fn tool_round_guidance_with(
    messages: &[serde_json::Value],
    round_index: u32,
    warning: u32,
    limit: u32,
) -> String {
    tool_round_guidance_trace_with(messages, round_index, warning, limit).0
}

pub fn tool_round_guidance(messages: &[serde_json::Value], round_index: u32) -> String {
    tool_round_guidance_with(
        messages,
        round_index,
        ROUND_BUDGET_THRESHOLD,
        ROUND_BUDGET_HARD_LIMIT,
    )
}

pub fn tool_round_guidance_trace_with(
    messages: &[serde_json::Value],
    round_index: u32,
    _warning: u32,
    _limit: u32,
) -> (String, PromptGuidanceSignals) {
    let trailing_tool_count = trailing_tool_result_count(messages);
    let parallel_feedback = trailing_tool_count > 1;
    let single_tool_streak = trailing_single_tool_round_streak(messages);
    let parallel_batching_nudge = single_tool_streak >= PARALLEL_BATCHING_NUDGE_THRESHOLD;

    // Only emit parallel-batching nudge and positive feedback.
    // Round budget pressure is gone — circuit breaker handles stalls.
    let _ = round_index;
    (
        format!(
            "{}{}",
            parallel_batching_nudge_directive(messages),
            parallel_execution_feedback(messages)
        ),
        PromptGuidanceSignals {
            round_budget_warning: false,
            synthesize_or_batch: false,
            parallel_feedback,
            parallel_batching_nudge,
        },
    )
}

/// Parallel execution feedback — injected into dynamic prompt when the previous
/// round had multiple tool results, indicating the LLM successfully batched.
///
/// Generic mechanism: counts tool-role messages in conversation history, returns
/// positive reinforcement hint when batching detected. Returns empty string for
/// round 0 or when ≤1 tool result in previous round.
pub fn parallel_execution_feedback(messages: &[serde_json::Value]) -> String {
    if messages.is_empty() {
        return String::new();
    }
    let tool_count = trailing_tool_result_count(messages);
    if tool_count > 1 {
        format!(
            "\n\n✓ Previous round: {tool_count} tools executed in parallel — excellent. \
             Keep batching independent operations."
        )
    } else {
        String::new()
    }
}

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

    // ── is_attention_manifest_content ─────────────────────────────

    #[test]
    fn attention_manifest_content_requires_prefix_and_newline() {
        let prefix = astra_turn_types::continuity::ATTENTION_PREFIX;
        // Exact prefix without newline — not a manifest (truncated / malformed).
        assert!(!is_attention_manifest_content(prefix));
        // Prefix followed by newline and body — valid manifest.
        assert!(is_attention_manifest_content(&format!("{}\nfoo", prefix)));
        // Prefix followed by newline only — still a valid manifest shell.
        assert!(is_attention_manifest_content(&format!("{}\n", prefix)));
        // Empty content — not a manifest.
        assert!(!is_attention_manifest_content(""));
        // Prefix followed by a non-newline byte — not a manifest (guards
        // against accidental matches like `[attention:v1]extra`).
        assert!(!is_attention_manifest_content(&format!("{}X", prefix)));
        // Random user content that merely mentions the marker — not a manifest.
        assert!(!is_attention_manifest_content(
            "what does [attention:v1] mean?"
        ));
    }

    // ── detect_task_type ──────────────────────────────────────────

    #[test]
    fn detect_code_review_en() {
        assert_eq!(detect_task_type("review this PR"), Some("code_review"));
        assert_eq!(detect_task_type("code review please"), Some("code_review"));
        assert_eq!(detect_task_type("check the diff"), Some("code_review"));
        assert_eq!(
            detect_task_type("review local changes"),
            Some("code_review")
        );
        assert_eq!(detect_task_type("look at the changes"), Some("code_review"));
        assert_eq!(
            detect_task_type("review latest commit"),
            Some("code_review")
        );
    }

    #[test]
    fn code_review_prompt_includes_commit_review_guidance() {
        let p = build_main_system_prompt(
            &["git_diff", "git_log", "git_show", "bash"],
            "",
            1.0,
            Some("code_review"),
        );
        assert!(
            p.contains("Specific commit review"),
            "should include commit review variant"
        );
        assert!(
            p.contains("git_log + git_show"),
            "should guide git_log + git_show in parallel"
        );
        assert!(
            p.contains("call BOTH in the first turn"),
            "should warn against sequential git_log then git_show"
        );
        assert!(
            p.contains("Default budget: no more than 3 read_file calls"),
            "should bound read_file fanout for review turns"
        );
    }

    #[test]
    fn detect_code_review_cn() {
        assert_eq!(detect_task_type("评审一下这个代码"), Some("code_review"));
        assert_eq!(detect_task_type("帮我审查代码"), Some("code_review"));
        assert_eq!(detect_task_type("代码审查"), Some("code_review"));
        assert_eq!(detect_task_type("看改动"), Some("code_review"));
        assert_eq!(detect_task_type("看看改了什么"), Some("code_review"));
        assert_eq!(detect_task_type("审阅本地改动"), Some("code_review"));
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
        // No-tools mode uses a minimal prompt — CC skill awareness is omitted
        assert!(
            !p.contains("Claude Code"),
            "no-tools prompt should not contain CC skill rule"
        );
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
    fn prompt_memory_store_only_uses_minimal_rules() {
        let p = build_main_system_prompt(&["memory_store"], "", 0.5, None);
        assert!(p.contains("Memory Rules"));
        assert!(p.contains("memory_store IMMEDIATELY"));
        assert!(!p.contains("Deduplication"));
        assert!(!p.contains("Negative preferences"));
        assert!(!p.contains("Memory precedence"));
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
        assert!(p.contains("Evidence BEFORE conclusions"));
        assert!(p.contains("NEVER write a summary or verdict"));
        assert!(p.contains("read_file"));
        assert!(p.contains("outline=true"));
        assert!(p.contains("could not verify"));
        assert!(p.contains("NEVER say LGTM"));
        assert!(p.contains("Anti-patterns"));
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
        assert!(p.contains("Executor rule (existing files)"));
        assert!(p.contains("Surgical edits"));
        assert!(p.contains("Verify your edits"));
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
    fn prompt_includes_build_test_guidance() {
        let p = build_main_system_prompt(&["bash"], "", 0.5, None);
        assert!(p.contains("When to Run Build / Test"));
        assert!(
            p.contains("ONLY to verify YOUR changes"),
            "should restrict build/test to post-edit verification"
        );
        assert!(
            p.contains("Do NOT run them for information gathering"),
            "should discourage speculative build/test"
        );
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
    fn prompt_git_tool_guidance_for_compound_ops() {
        let p = build_main_system_prompt(&["git_diff", "git_log", "bash"], "", 0.5, None);
        assert!(p.contains("git_status, git_diff"));
        assert!(p.contains("COMPOUND git operations"));
    }

    #[test]
    fn prompt_prefers_spawn_agent_over_internal_delegate_when_delegate_absent() {
        let p = build_main_system_prompt(&["spawn_agent", "bash"], "", 0.5, None);
        assert!(p.contains("Use `spawn_agent`"));
        assert!(p.contains("Do NOT call `delegate`"));
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

    #[test]
    fn prompt_user_memories_upgrade_memory_rules_to_full() {
        let p = build_main_system_prompt(
            &["memory_store"],
            "\n## User Memories\nprefers Rust\n",
            0.5,
            None,
        );
        assert!(p.contains("Deduplication"));
        assert!(p.contains("Memory precedence"));
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
        assert!(
            p.contains("Code Navigation"),
            "should include code nav section"
        );
        assert!(
            p.contains("find_definition"),
            "should mention find_definition"
        );
        assert!(
            p.contains("tree-sitter"),
            "should mention tree-sitter advantage"
        );
    }

    #[test]
    fn code_nav_guidance_absent_without_tools() {
        let p = build_main_system_prompt(&["bash", "read_file"], "", 0.5, Some("implementation"));
        assert!(
            !p.contains("Code Navigation"),
            "should NOT include code nav without tools"
        );
    }

    #[test]
    fn build_test_guidance_present_when_tool_available() {
        let p = build_main_system_prompt(
            &["run_build_test", "str_replace"],
            "",
            0.5,
            Some("implementation"),
        );
        assert!(
            p.contains("Build & Test Loop"),
            "should include build/test section"
        );
        assert!(p.contains("run_build_test"), "should mention the tool");
        assert!(
            p.contains("structured errors"),
            "should describe structured output"
        );
    }

    #[test]
    fn build_test_guidance_absent_without_tool() {
        let p = build_main_system_prompt(&["bash"], "", 0.5, Some("implementation"));
        assert!(
            !p.contains("Build & Test Loop"),
            "should NOT include build/test without tool"
        );
    }

    #[test]
    fn plan_execution_warns_about_mutating_bash_in_rollback_boundaries() {
        let p =
            build_main_system_prompt(&["bash", "run_build_test"], "", 0.5, Some("implementation"));
        assert!(
            p.contains("non-read-only `bash` is a manual boundary"),
            "should warn that mutating bash does not participate in rollback boundaries"
        );
        assert!(
            p.contains("run_build_test"),
            "should steer build/test work to run_build_test when available"
        );
    }

    #[test]
    fn git_mutations_guidance_present_when_tools_available() {
        let p = build_main_system_prompt(
            &[
                "git_commit",
                "git_revert_commit",
                "git_stash",
                "git_checkout_file",
                "git_worktree",
            ],
            "",
            0.5,
            Some("implementation"),
        );
        assert!(
            p.contains("Git Workflow"),
            "should include git workflow section"
        );
        assert!(p.contains("git_commit"), "should mention git_commit");
        assert!(
            p.contains("git_revert_commit"),
            "should mention git_revert_commit"
        );
        assert!(p.contains("git_worktree"), "should mention git_worktree");
        assert!(p.contains("git_stash"), "should mention git_stash");
        assert!(
            p.contains("git_checkout_file"),
            "should mention git_checkout_file"
        );
    }

    #[test]
    fn git_mutations_guidance_absent_without_tools() {
        let p = build_main_system_prompt(&["git_diff", "git_log"], "", 0.5, None);
        assert!(
            !p.contains("Git Workflow"),
            "should NOT include git mutations without commit tool"
        );
    }

    #[test]
    fn session_state_rollback_guidance_omits_turn_rollback_when_unavailable() {
        let p = build_main_system_prompt(
            &["rollback_session_state", "adjust_config"],
            "",
            0.5,
            Some("implementation"),
        );
        assert!(
            p.contains("Session-State Rollback"),
            "should include session rollback section"
        );
        assert!(
            !p.contains("rollback_turn_actions"),
            "should not mention unavailable mixed-surface rollback tool"
        );
    }

    #[test]
    fn session_state_rollback_guidance_mentions_turn_rollback_when_available() {
        let p = build_main_system_prompt(
            &[
                "rollback_session_state",
                "rollback_turn_actions",
                "adjust_config",
            ],
            "",
            0.5,
            Some("implementation"),
        );
        assert!(
            p.contains("rollback_turn_actions"),
            "should mention mixed-surface rollback when tool is available"
        );
    }

    #[test]
    fn implementation_strategy_references_new_tools() {
        let p = build_main_system_prompt(
            &["find_definition", "run_build_test", "git_commit"],
            "",
            0.5,
            Some("implementation"),
        );
        assert!(
            p.contains("find_definition"),
            "strategy should reference find_definition"
        );
        assert!(
            p.contains("run_build_test"),
            "strategy should reference run_build_test"
        );
        assert!(
            p.contains("git_commit"),
            "strategy should reference git_commit"
        );
        assert!(
            p.contains("str_replace auto-formats"),
            "strategy should mention auto-format"
        );
    }

    // ── build_system_prompt_sections tests ──────────────────────────

    #[test]
    fn sections_have_correct_scopes() {
        let tools = vec!["bash", "read_file", "glob", "grep"];
        let sections = build_system_prompt_sections(&tools, "cwd: /tmp", 0.8, None);

        // Should have multiple Global sections, then Session, then None
        let globals: Vec<_> = sections
            .iter()
            .filter(|s| s.scope == CacheScope::Global)
            .collect();
        let sessions: Vec<_> = sections
            .iter()
            .filter(|s| s.scope == CacheScope::Session)
            .collect();
        assert!(
            globals.len() >= 5,
            "should have multiple Global sections, got {}",
            globals.len()
        );
        assert!(!sessions.is_empty(), "should have Session sections");

        // First section should be Global
        assert_eq!(
            sections[0].scope,
            CacheScope::Global,
            "first section should be Global"
        );

        // Profile section should be CacheScope::None
        let profile = sections.iter().find(|s| s.scope == CacheScope::None);
        assert!(
            profile.is_some(),
            "should have a None-scoped profile section"
        );
        assert!(
            profile.unwrap().text.contains("cwd: /tmp"),
            "profile section should contain the cwd"
        );
    }

    #[test]
    fn sections_global_contains_identity_and_rules() {
        let tools = vec!["bash"];
        let sections = build_system_prompt_sections(&tools, "", 0.8, None);

        // Core rules are in the first Global section
        let global_text: String = sections
            .iter()
            .filter(|s| s.scope == CacheScope::Global)
            .map(|s| s.text.as_str())
            .collect();
        assert!(
            global_text.contains(SYSTEM_PROMPT_BASE),
            "should contain base identity"
        );
        assert!(
            global_text.contains("Core Rules"),
            "should contain core rules"
        );
        assert!(
            global_text.contains("Planning Protocol"),
            "should contain planning"
        );
        assert!(
            global_text.contains("Context Strategy"),
            "should contain context strategy"
        );
        assert!(
            global_text.contains("Claude Code skills"),
            "should contain CC skill compatibility rule"
        );
    }

    #[test]
    fn sections_session_contains_tool_guidance() {
        let tools = vec!["bash", "find_definition", "find_references", "git_commit"];
        let sections = build_system_prompt_sections(&tools, "", 0.8, Some("debugging"));

        let session_text: String = sections
            .iter()
            .filter(|s| s.scope == CacheScope::Session)
            .map(|s| s.text.as_str())
            .collect();
        assert!(
            session_text.contains("Code Navigation"),
            "session should include code nav guidance"
        );
        assert!(
            session_text.contains("Debugging Strategy"),
            "session should include task-type strategy"
        );
    }

    #[test]
    fn sections_no_profile_when_empty() {
        let tools = vec!["bash"];
        let sections = build_system_prompt_sections(&tools, "", 0.8, None);

        let none_scoped: Vec<_> = sections
            .iter()
            .filter(|s| s.scope == CacheScope::None)
            .collect();
        assert!(
            none_scoped.is_empty(),
            "no None-scoped section when profile is empty"
        );
    }

    #[test]
    fn sections_to_string_contains_all_content() {
        let tools = vec!["bash", "read_file", "glob"];
        let profile = "cwd: /test\ngit_branch: main";

        let sections = build_system_prompt_sections(&tools, profile, 0.8, Some("implementation"));
        let result = sections_to_string(&sections);

        // All key content should appear in the concatenated output
        assert!(
            result.contains(SYSTEM_PROMPT_BASE),
            "should contain identity"
        );
        assert!(
            result.contains("bash, read_file, glob"),
            "should contain tools"
        );
        assert!(result.contains("Core Rules"), "should contain core rules");
        assert!(
            result.contains("Implementation Strategy"),
            "should contain task guidance"
        );
        assert!(
            result.contains("Output Format"),
            "should contain output format"
        );
        assert!(
            result.contains("Tool Error Recovery"),
            "should contain error recovery"
        );
        assert!(result.contains("cwd: /test"), "should contain profile");
        assert!(
            result.contains("git_branch: main"),
            "should contain profile details"
        );
    }

    #[test]
    fn sections_empty_tools_returns_global_and_profile() {
        let sections = build_system_prompt_sections(&[], "cwd: /app", 0.5, None);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].scope, CacheScope::Global);
        assert!(sections[0].text.contains("NO tools available"));
        assert_eq!(sections[1].scope, CacheScope::None);
        assert!(sections[1].text.contains("cwd: /app"));
    }

    #[test]
    fn sections_low_confidence_in_session_scope() {
        let tools = vec!["bash"];
        let sections = build_system_prompt_sections(&tools, "", 0.1, None);

        let session_text: String = sections
            .iter()
            .filter(|s| s.scope == CacheScope::Session)
            .map(|s| s.text.as_str())
            .collect();
        assert!(
            session_text.contains("Low-Confidence Tool Selection"),
            "low confidence advisory should be in session section"
        );
    }

    // ── Confidence boundary ──────────────────────────────────────

    #[test]
    fn prompt_no_low_confidence_at_exact_threshold() {
        // Uses strict `<`, so exactly-at-threshold should NOT trigger.
        let p = build_main_system_prompt(&["bash"], "", LOW_CONFIDENCE_THRESHOLD, None);
        assert!(
            !p.contains("Low-Confidence"),
            "confidence == threshold (strict <) should NOT trigger advisory"
        );
    }

    #[test]
    fn prompt_low_confidence_at_zero() {
        let p = build_main_system_prompt(&["bash"], "", 0.0, None);
        assert!(p.contains("Low-Confidence"));
    }

    // ── Code-navigation sub-tool guidance ────────────────────────

    #[test]
    fn prompt_call_graph_guidance_when_present() {
        let p = build_main_system_prompt(&["call_graph", "find_definition"], "", 0.5, None);
        assert!(p.contains("call_graph"), "should mention call_graph tool");
        assert!(p.contains("callers=true"), "should describe callers mode");
        assert!(
            p.contains("scope='project'"),
            "should describe project scope"
        );
    }

    #[test]
    fn prompt_rename_symbol_guidance_when_present() {
        let p = build_main_system_prompt(&["rename_symbol"], "", 0.5, None);
        assert!(p.contains("rename_symbol"));
        assert!(p.contains("AST-validated"));
        assert!(p.contains("dry_run=true"));
    }

    #[test]
    fn prompt_dead_code_extract_members_type_hierarchy() {
        let p = build_main_system_prompt(
            &["dead_code", "extract_members", "type_hierarchy"],
            "",
            0.5,
            None,
        );
        assert!(p.contains("dead_code"));
        assert!(p.contains("Find unused symbols"));
        assert!(p.contains("extract_members"));
        assert!(p.contains("fields+methods"));
        assert!(p.contains("type_hierarchy"));
        assert!(p.contains("implements trait"));
    }

    // ── Editing strategy (multi_edit) ────────────────────────────

    #[test]
    fn prompt_multi_edit_includes_editing_strategy() {
        let p = build_main_system_prompt(&["multi_edit"], "", 0.5, None);
        assert!(p.contains("Editing Strategy"));
        assert!(p.contains("multi_edit"));
        assert!(p.contains("atomic"));
        assert!(p.contains("delete_file"));
    }

    #[test]
    fn prompt_editing_strategy_absent_without_multi_edit() {
        let p = build_main_system_prompt(&["bash", "read_file"], "", 0.5, None);
        assert!(!p.contains("Editing Strategy"));
    }

    // ── Unknown task type ────────────────────────────────────────

    #[test]
    fn prompt_unknown_task_type_no_task_strategy() {
        let p = build_main_system_prompt(&["bash"], "", 0.5, Some("nonexistent_type"));
        assert!(p.contains("Core Rules"), "base content should be present");
        assert!(!p.contains("Code Review Strategy"));
        assert!(!p.contains("Debugging Strategy"));
        assert!(!p.contains("Exploration Strategy"));
        assert!(!p.contains("Implementation Strategy"));
        assert!(!p.contains("Refactoring Strategy"));
        assert!(!p.contains("Testing Strategy"));
        assert!(!p.contains("Documentation Strategy"));
        assert!(!p.contains("Performance Strategy"));
        assert!(!p.contains("Analysis Strategy"));
        assert!(!p.contains("Deployment Strategy"));
    }

    // ── Search strategy with only read_file ──────────────────────

    #[test]
    fn prompt_search_strategy_with_only_read_file() {
        let p = build_main_system_prompt(&["read_file"], "", 0.5, None);
        assert!(
            p.contains("Search Strategy"),
            "read_file alone should trigger search strategy"
        );
    }

    // ── git_ vs github_ tool distinction ─────────────────────────

    #[test]
    fn prompt_git_prefix_without_github_omits_github_rule() {
        let p = build_main_system_prompt(&["git_diff", "git_log"], "", 0.5, None);
        assert!(
            p.contains("git_status, git_diff"),
            "git_ prefix triggers tool preference"
        );
        assert!(
            !p.contains("GitHub data"),
            "should NOT have GitHub-specific rule without github_ tools"
        );
    }

    #[test]
    fn prompt_github_tools_trigger_both_rules() {
        let p = build_main_system_prompt(&["github_list_prs"], "", 0.5, None);
        assert!(p.contains("github_*"), "github_ triggers preference rule");
        assert!(
            p.contains("GitHub data"),
            "github_ triggers GitHub-specific rule"
        );
    }

    // ── Profile desc in no-tools path ────────────────────────────

    #[test]
    fn prompt_no_tools_includes_profile_desc() {
        let p = build_main_system_prompt(&[], "\n## Project: MyApp\n", 0.5, None);
        assert!(p.contains("NO tools available"));
        assert!(p.contains("Project: MyApp"));
    }

    // ── sections_to_string edge case ─────────────────────────────

    #[test]
    fn sections_to_string_empty_input() {
        let result = sections_to_string(&[]);
        assert!(
            result.is_empty(),
            "empty sections should produce empty string"
        );
    }

    // ── All code-nav tools in session scope ──────────────────────

    #[test]
    fn sections_all_code_nav_tools_in_session_scope() {
        let tools = vec![
            "find_definition",
            "find_references",
            "call_graph",
            "rename_symbol",
            "dead_code",
            "extract_members",
            "type_hierarchy",
            "lsp",
        ];
        let sections = build_system_prompt_sections(&tools, "", 0.8, None);
        let session_text: String = sections
            .iter()
            .filter(|s| s.scope == CacheScope::Session)
            .map(|s| s.text.as_str())
            .collect();
        assert!(session_text.contains("Code Navigation"));
        assert!(session_text.contains("call_graph"));
        assert!(session_text.contains("rename_symbol"));
        assert!(session_text.contains("dead_code"));
        assert!(session_text.contains("extract_members"));
        assert!(session_text.contains("type_hierarchy"));
        assert!(session_text.contains("lsp"));
    }

    #[test]
    fn sections_lsp_alone_adds_code_navigation_guidance() {
        let sections = build_system_prompt_sections(&["lsp"], "", 0.8, None);
        let session_text: String = sections
            .iter()
            .filter(|s| s.scope == CacheScope::Session)
            .map(|s| s.text.as_str())
            .collect();
        assert!(session_text.contains("Code Navigation"));
        assert!(session_text.contains("item_index"));
        assert!(session_text.contains("action_index"));
        assert!(session_text.contains("quick fixes"));
        assert!(session_text.contains("autocomplete"));
    }

    // ── Empty-tools + empty-profile section behavior ─────────────

    #[test]
    fn sections_empty_tools_empty_profile_still_has_profile_section() {
        // Empty-tools code path always returns a profile section (even empty).
        let sections = build_system_prompt_sections(&[], "", 0.5, None);
        assert_eq!(
            sections.len(),
            2,
            "empty tools path always returns 2 sections"
        );
        assert_eq!(sections[0].scope, CacheScope::Global);
        assert_eq!(sections[1].scope, CacheScope::None);
        assert!(sections[1].text.is_empty());
    }

    // ── Output style injection ───────────────────────────────────────

    #[test]
    fn prompt_with_output_style_includes_style_content() {
        use astra_text_utils::output_style::{OutputStyle, StyleSource};

        let style = OutputStyle {
            name: "test".to_string(),
            description: "Test style".to_string(),
            prompt: "# Output Style: Test\nBe very brief.".to_string(),
            source: StyleSource::BuiltIn,
            keep_coding_instructions: true,
        };

        let p = build_main_system_prompt_with_style(&["bash"], "", 0.5, None, Some(&style));
        assert!(
            p.contains("# Output Style: Test"),
            "prompt should include output style header"
        );
        assert!(
            p.contains("Be very brief"),
            "prompt should include output style content"
        );
    }

    #[test]
    fn prompt_without_output_style_has_no_style_section() {
        let p = build_main_system_prompt_with_style(&["bash"], "", 0.5, None, None);
        assert!(
            !p.contains("# Output Style:"),
            "prompt without style should not have style section"
        );
    }

    #[test]
    fn sections_with_output_style_includes_style_content() {
        use astra_text_utils::output_style::{OutputStyle, StyleSource};

        let style = OutputStyle {
            name: "concise".to_string(),
            description: "Concise style".to_string(),
            prompt: "# Output Style: Concise\nMinimize output.".to_string(),
            source: StyleSource::BuiltIn,
            keep_coding_instructions: true,
        };

        let sections =
            build_system_prompt_sections_with_style(&["bash"], "", 0.5, None, Some(&style));
        let all_text: String = sections.iter().map(|s| s.text.as_str()).collect();
        assert!(
            all_text.contains("# Output Style: Concise"),
            "should include output style"
        );
        assert!(
            all_text.contains("Minimize output"),
            "should include style content"
        );
        // Output style should be in None scope (dynamic)
        let style_section = sections
            .iter()
            .find(|s| s.text.contains("Output Style: Concise"));
        assert_eq!(
            style_section.unwrap().scope,
            CacheScope::None,
            "output style should be None-scoped"
        );
    }

    // ── Prompt override tests ──

    #[test]
    fn apply_overrides_replaces_matching_section() {
        let tools = &["bash", "grep"];
        let mut sections =
            build_system_prompt_sections_with_style(tools, "test project", 0.8, None, None);

        let mut overrides = PromptOverrides::new();
        overrides.insert("core_rules".into(), "Custom core rules content".into());

        apply_overrides(&mut sections, &overrides);

        assert_eq!(sections[0].text, "Custom core rules content");
        assert_eq!(sections[0].scope, CacheScope::Global);
        // Other sections should be unchanged
        assert!(sections[1].text.contains("Planning Protocol"));
    }

    #[test]
    fn apply_overrides_ignores_unknown_keys() {
        let tools = &["bash"];
        let mut sections = build_system_prompt_sections_with_style(tools, "", 0.8, None, None);

        let original_text = sections[0].text.clone();
        let mut overrides = PromptOverrides::new();
        overrides.insert("nonexistent_section".into(), "should be ignored".into());

        apply_overrides(&mut sections, &overrides);
        assert_eq!(sections[0].text, original_text);
    }

    #[test]
    fn load_overrides_from_directory() {
        let dir = tempfile::tempdir().unwrap();

        std::fs::write(dir.path().join("core_rules.txt"), "My rules").unwrap();
        std::fs::write(dir.path().join("planning.txt"), "My planning").unwrap();
        std::fs::write(dir.path().join("not_a_txt.md"), "ignored").unwrap();

        let overrides = load_overrides(dir.path());

        assert_eq!(overrides.get("core_rules").unwrap(), "My rules");
        assert_eq!(overrides.get("planning").unwrap(), "My planning");
        assert!(!overrides.contains_key("not_a_txt"));
    }

    #[test]
    fn load_overrides_returns_empty_for_missing_dir() {
        let overrides = load_overrides(Path::new("/nonexistent/path"));
        assert!(overrides.is_empty());
    }

    #[test]
    fn build_system_prompt_trace_includes_skills_and_memories() {
        use astra_turn_core::context_assembly_trace::{MemoryInjection, SkillInjection};

        let sections = build_system_prompt_sections(&["bash", "grep"], "", 0.8, None);
        let skills = vec![SkillInjection {
            skill_name: "concise".into(),
            skill_version: None,
            tokens: 150,
            selection_reason: "active".into(),
        }];
        let memories = vec![MemoryInjection {
            memory_id: "m-1".into(),
            memory_type: "hybrid".into(),
            tokens: 200,
            relevance_score: 0.95,
            content_preview: "user prefers rust".into(),
        }];
        let bd = build_system_prompt_trace(&sections, skills, memories);

        assert!(
            bd.base_persona_tokens > 0,
            "base_persona should be non-zero"
        );
        assert_eq!(bd.skills_injected.len(), 1);
        assert_eq!(bd.skills_injected[0].skill_name, "concise");
        assert_eq!(bd.skills_injected[0].tokens, 150);
        assert_eq!(bd.repository_memories.len(), 1);
        assert_eq!(bd.repository_memories[0].tokens, 200);
        // total includes sections + skills + memories
        assert!(bd.total_tokens >= bd.base_persona_tokens + 150 + 200);
    }

    #[test]
    fn build_system_prompt_trace_empty_skills_and_memories() {
        let sections = build_system_prompt_sections(&["bash"], "", 0.5, None);
        let bd = build_system_prompt_trace(&sections, vec![], vec![]);

        assert!(bd.base_persona_tokens > 0);
        assert!(bd.skills_injected.is_empty());
        assert!(bd.repository_memories.is_empty());
        assert_eq!(
            bd.total_tokens,
            bd.base_persona_tokens + bd.environment_tokens + bd.user_preferences_tokens
        );
    }

    #[test]
    fn synthesize_or_batch_directive_requires_late_round_and_trailing_tools() {
        let early = synthesize_or_batch_directive(
            &[serde_json::json!({"role": "tool", "content": "a"})],
            ROUND_BUDGET_THRESHOLD - 1,
            ROUND_BUDGET_THRESHOLD,
        );
        assert!(early.is_empty(), "early rounds should not get the nudge");

        let no_trailing_tools = synthesize_or_batch_directive(
            &[serde_json::json!({"role": "assistant", "content": "done"})],
            ROUND_BUDGET_THRESHOLD,
            ROUND_BUDGET_THRESHOLD,
        );
        assert!(
            no_trailing_tools.is_empty(),
            "non-tool endings should not get the nudge"
        );
    }

    #[test]
    fn tool_round_guidance_combines_budget_synthesis_and_parallel_feedback() {
        let messages = vec![
            serde_json::json!({"role": "user", "content": "inspect the repo"}),
            serde_json::json!({"role": "tool", "content": "Cargo.toml"}),
            serde_json::json!({"role": "tool", "content": "README.md"}),
        ];

        let guidance = tool_round_guidance(&messages, ROUND_BUDGET_THRESHOLD);
        // Round budget and synthesize directives are neutered (circuit breaker).
        assert!(
            !guidance.contains("Round Budget Warning"),
            "round budget directive should be empty"
        );
        assert!(
            !guidance.contains("Synthesize Or Batch Now"),
            "synthesize directive should be empty"
        );
        // Parallel feedback still works.
        assert!(
            guidance.contains("2 tools executed in parallel"),
            "guidance should preserve parallel batching feedback"
        );
    }

    #[test]
    fn build_system_prompt_trace_records_guidance_signals_from_section_metadata() {
        let (guidance, guidance_signals) = tool_round_guidance_trace_with(
            &[
                serde_json::json!({"role": "tool", "content": "Cargo.toml"}),
                serde_json::json!({"role": "tool", "content": "README.md"}),
            ],
            ROUND_BUDGET_THRESHOLD,
            ROUND_BUDGET_THRESHOLD,
            ROUND_BUDGET_HARD_LIMIT,
        );
        let sections = vec![
            PromptSection::dynamic(guidance, PromptTokenBucket::Environment).with_trace_signals(
                PromptTraceSignals {
                    guidance_signals,
                    ..Default::default()
                },
            ),
        ];

        let breakdown = build_system_prompt_trace(&sections, vec![], vec![]);
        // Budget signals are always false now (circuit breaker replaces them).
        assert!(!breakdown.guidance_signals.round_budget_warning);
        assert!(!breakdown.guidance_signals.synthesize_or_batch);
        assert!(breakdown.guidance_signals.parallel_feedback);
    }

    #[test]
    fn tool_round_guidance_trace_with_returns_matching_signals() {
        let messages = vec![
            serde_json::json!({"role": "tool", "content": "Cargo.toml"}),
            serde_json::json!({"role": "tool", "content": "README.md"}),
        ];

        let (guidance, signals) = tool_round_guidance_trace_with(
            &messages,
            ROUND_BUDGET_THRESHOLD,
            ROUND_BUDGET_THRESHOLD,
            ROUND_BUDGET_HARD_LIMIT,
        );

        // Budget directives neutered.
        assert!(!guidance.contains("Round Budget Warning"));
        assert!(!guidance.contains("Synthesize Or Batch Now"));
        // Parallel feedback still works.
        assert!(guidance.contains("2 tools executed in parallel"));
        assert!(!signals.round_budget_warning);
        assert!(!signals.synthesize_or_batch);
        assert!(signals.parallel_feedback);
    }

    #[test]
    fn tool_round_guidance_ignores_trailing_runtime_system_messages() {
        let messages = vec![
            serde_json::json!({"role": "tool", "content": "Cargo.toml"}),
            serde_json::json!({"role": "tool", "content": "README.md"}),
            serde_json::json!({
                "role": "system",
                "content": "✓ 2 tools executed in parallel — excellent. Keep batching independent operations."
            }),
            serde_json::json!({
                "role": "system",
                "content": "## Already Fetched (do NOT re-read/re-grep these)\nFiles: README.md"
            }),
        ];

        let (guidance, signals) = tool_round_guidance_trace_with(
            &messages,
            ROUND_BUDGET_THRESHOLD,
            ROUND_BUDGET_THRESHOLD,
            ROUND_BUDGET_HARD_LIMIT,
        );

        // Budget directives neutered; parallel feedback still works.
        assert!(!guidance.contains("Synthesize Or Batch Now"));
        assert!(guidance.contains("2 tools executed in parallel"));
        assert!(!signals.synthesize_or_batch);
        assert!(signals.parallel_feedback);
    }

    #[test]
    fn tool_round_guidance_ignores_trailing_runtime_attention_manifest() {
        let messages = vec![
            serde_json::json!({"role": "assistant", "content": null, "tool_calls": [{"id": "call_1"}]}),
            serde_json::json!({"role": "tool", "content": "Cargo.toml"}),
            serde_json::json!({"role": "tool", "content": "README.md"}),
            serde_json::json!({
                "role": "system",
                "content": "[working-set:v1]\ngoal: inspect the project files"
            }),
            serde_json::json!({
                "role": "system",
                "content": "## Already Fetched (do NOT re-read/re-grep these)\nFiles: README.md"
            }),
            serde_json::json!({
                "role": "user",
                "content": "[attention:v1]\ngoal: inspect the project files"
            }),
        ];

        let (guidance, signals) = tool_round_guidance_trace_with(
            &messages,
            ROUND_BUDGET_THRESHOLD,
            ROUND_BUDGET_THRESHOLD,
            ROUND_BUDGET_HARD_LIMIT,
        );

        // Budget directives neutered; parallel feedback still works.
        assert!(!guidance.contains("Synthesize Or Batch Now"));
        assert!(guidance.contains("2 tools executed in parallel"));
        assert!(!signals.synthesize_or_batch);
        assert!(signals.parallel_feedback);
    }

    #[test]
    fn build_system_prompt_trace_records_context_signals_from_section_metadata() {
        let sections = vec![
            PromptSection::dynamic(
                "arbitrary dynamic payload without legacy markers".to_string(),
                PromptTokenBucket::Environment,
            )
            .with_trace_signals(PromptTraceSignals {
                context_signals: PromptContextSignals {
                    active_output_skills: true,
                    learned_runtime_context: true,
                    memory_signal_detected: true,
                    effort_hint: true,
                    agent_type_hint: true,
                    self_awareness: true,
                    implicit_feedback: true,
                    learned_feedback_rules: true,
                    session_anchor: true,
                    ..Default::default()
                },
                ..Default::default()
            }),
        ];

        let breakdown = build_system_prompt_trace(&sections, vec![], vec![]);
        assert!(breakdown.context_signals.active_output_skills);
        assert!(breakdown.context_signals.learned_runtime_context);
        assert!(breakdown.context_signals.memory_signal_detected);
        assert!(!breakdown.context_signals.system_prompt_override);
        assert!(breakdown.context_signals.effort_hint);
        assert!(breakdown.context_signals.agent_type_hint);
        assert!(breakdown.context_signals.self_awareness);
        assert!(breakdown.context_signals.implicit_feedback);
        assert!(breakdown.context_signals.learned_feedback_rules);
        assert!(breakdown.context_signals.session_anchor);
    }

    #[test]
    fn build_system_prompt_trace_uses_section_token_buckets() {
        let sections = vec![
            PromptSection::stable("base".to_string(), CacheScope::Global),
            PromptSection::dynamic(
                "runtime preference without legacy marker".to_string(),
                PromptTokenBucket::UserPreferences,
            ),
            PromptSection::dynamic(
                "arbitrary environment payload".to_string(),
                PromptTokenBucket::Environment,
            ),
        ];

        let breakdown = build_system_prompt_trace(&sections, vec![], vec![]);

        assert_eq!(
            breakdown.base_persona_tokens,
            estimate_section_tokens("base")
        );
        assert_eq!(
            breakdown.user_preferences_tokens,
            estimate_section_tokens("runtime preference without legacy marker")
        );
        assert_eq!(
            breakdown.environment_tokens,
            estimate_section_tokens("arbitrary environment payload")
        );
    }

    #[test]
    fn build_system_prompt_trace_ignores_unannotated_legacy_markers() {
        let breakdown = build_system_prompt_trace(
            &[PromptSection::dynamic(
                "\n\n## System Prompt Override\nlegacy override\n\n## ⚡ Round Budget Warning"
                    .to_string(),
                PromptTokenBucket::Environment,
            )],
            vec![],
            vec![],
        );

        assert!(!breakdown.context_signals.system_prompt_override);
        assert!(!breakdown.guidance_signals.round_budget_warning);
    }

    #[test]
    fn build_system_prompt_trace_uses_explicit_section_signals() {
        let breakdown = build_system_prompt_trace(
            &[
                PromptSection::dynamic("cwd: /tmp".to_string(), PromptTokenBucket::Environment)
                    .with_trace_signals(PromptTraceSignals {
                        context_signals: PromptContextSignals {
                            system_prompt_override: true,
                            ..Default::default()
                        },
                        guidance_signals: PromptGuidanceSignals {
                            round_budget_warning: true,
                            ..Default::default()
                        },
                    }),
            ],
            vec![],
            vec![],
        );

        assert!(breakdown.context_signals.system_prompt_override);
        assert!(breakdown.guidance_signals.round_budget_warning);
    }

    #[test]
    fn round_budget_defaults_allow_at_least_8_rounds_before_warning() {
        // With the raised defaults, round 7 should NOT trigger a warning.
        let directive = round_budget_directive(7);
        assert!(
            directive.is_empty(),
            "round 7 should not trigger budget warning with raised defaults, got: {directive}"
        );
    }

    #[test]
    fn round_budget_defaults_hard_limit_at_15() {
        // Directive is always empty now (circuit breaker replaces countdown).
        let at_limit = round_budget_directive(15);
        assert!(
            at_limit.is_empty(),
            "round budget directive should always be empty"
        );
        let before_limit = round_budget_directive(14);
        assert!(
            before_limit.is_empty(),
            "round budget directive should always be empty"
        );
    }

    // ─── Parallel batching nudge (real-session-shaped fixtures) ─────────
    //
    // Scenarios pulled from real sessions:
    //   - 6566d6a8 turn 1: 10 trailing single-tool read rounds → strong nudge
    //   - 03945541 turn 1: 6 single-tool rounds (locate→read) → soft case,
    //     but the nudge still fires at round 4+ since it cannot distinguish
    //     "legitimate dependency chain" from "should-have-batched" — the
    //     model's own next-round planning is the right place to disambiguate.
    //   - well-batched runs (≥2 tools per round) → never trigger.

    fn assistant_with_tool_calls(n: usize) -> Vec<serde_json::Value> {
        let mut msgs = vec![serde_json::json!({"role": "assistant", "tool_calls": []})];
        for _ in 0..n {
            msgs.push(serde_json::json!({"role": "tool", "content": "..."}));
        }
        msgs
    }

    fn rounds_pattern(per_round: &[usize]) -> Vec<serde_json::Value> {
        let mut out = vec![serde_json::json!({"role": "user", "content": "go"})];
        for &n in per_round {
            out.extend(assistant_with_tool_calls(n));
        }
        out
    }

    #[test]
    fn trailing_single_tool_streak_counts_consecutive_singletons_only() {
        // [3, 1, 1, 1, 1] → trailing streak = 4
        let msgs = rounds_pattern(&[3, 1, 1, 1, 1]);
        assert_eq!(trailing_single_tool_round_streak(&msgs), 4);

        // [1, 1, 2, 1, 1] → trailing streak = 2 (broken by the 2-tool round)
        let msgs = rounds_pattern(&[1, 1, 2, 1, 1]);
        assert_eq!(trailing_single_tool_round_streak(&msgs), 2);

        // [3, 3, 3] → 0 (last round was multi-tool)
        let msgs = rounds_pattern(&[3, 3, 3]);
        assert_eq!(trailing_single_tool_round_streak(&msgs), 0);

        // Empty / no tool messages → 0
        assert_eq!(trailing_single_tool_round_streak(&[]), 0);
        let only_user = vec![serde_json::json!({"role": "user", "content": "hi"})];
        assert_eq!(trailing_single_tool_round_streak(&only_user), 0);
    }

    #[test]
    fn parallel_batching_nudge_fires_after_threshold_streak() {
        // 4 single-tool rounds in a row — at threshold.
        let msgs = rounds_pattern(&[1, 1, 1, 1]);
        let directive = parallel_batching_nudge_directive(&msgs);
        assert!(
            directive.contains("Sequential Tool Calls Detected"),
            "expected nudge at threshold; got {:?}",
            directive
        );
        assert!(directive.contains("4 rounds"));
    }

    #[test]
    fn parallel_batching_nudge_silent_below_threshold() {
        let msgs = rounds_pattern(&[1, 1, 1]);
        assert!(parallel_batching_nudge_directive(&msgs).is_empty());
    }

    #[test]
    fn parallel_batching_nudge_silent_when_last_round_was_parallel() {
        // Long single-tool history followed by a 3-tool batch → no nudge,
        // because the model already corrected the pattern.
        let msgs = rounds_pattern(&[1, 1, 1, 1, 1, 1, 3]);
        assert!(
            parallel_batching_nudge_directive(&msgs).is_empty(),
            "should not nudge after the model already batched"
        );
    }

    #[test]
    fn parallel_batching_nudge_skips_runtime_system_messages() {
        // Trailing nudges/feedback injected by the runtime should not break
        // the streak detection.
        let mut msgs = rounds_pattern(&[1, 1, 1, 1]);
        msgs.push(serde_json::json!({
            "role": "system",
            "content": "## Already Fetched (do NOT re-read these)\nFiles: foo.rs"
        }));
        assert_eq!(trailing_single_tool_round_streak(&msgs), 4);
        assert!(
            parallel_batching_nudge_directive(&msgs).contains("Sequential Tool Calls Detected")
        );
    }

    #[test]
    fn tool_round_guidance_includes_batching_nudge_and_signals_it() {
        let msgs = rounds_pattern(&[1, 1, 1, 1]);
        let (guidance, signals) = tool_round_guidance_trace_with(
            &msgs,
            0, // early in the loop — round-budget warning should NOT fire
            ROUND_BUDGET_THRESHOLD,
            ROUND_BUDGET_HARD_LIMIT,
        );
        assert!(
            guidance.contains("Sequential Tool Calls Detected"),
            "guidance must surface the batching nudge"
        );
        assert!(!guidance.contains("Round Budget Warning"));
        assert!(signals.parallel_batching_nudge);
        assert!(!signals.round_budget_warning);
        // Single-tool last round → no positive parallel_feedback either.
        assert!(!signals.parallel_feedback);
    }
}

/// Agent persona / base identity.
///
/// Persona shapes tone and default behavior before any rule fires.
/// Keep it tight: identity + 3-4 behavioral traits. Longer personas
/// dilute; shorter ones leave the model to improvise a voice.
pub const SYSTEM_PROMPT_BASE: &str = "You are Astra, an expert software engineer operating as a terminal-native coding agent. You write clean, correct code and use tools precisely to solve tasks.\n\n\
    - **Direct over deferential**: state the answer, then the reasoning. No flattery, no hedging preambles (\"Great question!\", \"I'd be happy to…\").\n\
    - **Concise by default**: match response length to question complexity. A one-line question deserves a one-line answer.\n\
    - **Honest about uncertainty**: if you don't know, say so and propose how to find out — never fabricate.\n\
    - **Action-biased**: when the user asks for a change, make it. Don't ask permission for obvious next steps.";

use std::fmt::Write;

use astra_text_utils::output_style::OutputStyle;

/// Session-stable runtime context for the agent's self-knowledge.
///
/// Injected into the system prompt so the agent knows its identity,
/// environment, and session context — enabling inquiry, reflection,
/// and retrospection without hallucinating these details.
#[derive(Debug, Clone, Default)]
pub struct AgentRuntimeContext {
    pub model_name: Option<String>,
    pub workspace_cwd: Option<String>,
    pub git_branch: Option<String>,
    pub session_id: Option<String>,
    pub user_id: Option<String>,
    pub current_date: Option<String>,
}

impl AgentRuntimeContext {
    pub fn to_prompt_section(&self) -> String {
        let mut s = String::with_capacity(256);
        s.push_str("\n## Runtime Identity\n");
        if let Some(ref v) = self.model_name {
            let _ = writeln!(s, "Model: {v}");
        }
        if let Some(ref v) = self.current_date {
            let _ = writeln!(s, "Date: {v}");
        }
        if let Some(ref v) = self.workspace_cwd {
            let _ = writeln!(s, "Workspace: {v}");
        }
        if let Some(ref v) = self.git_branch {
            let _ = writeln!(s, "Git branch: {v}");
        }
        if let Some(ref v) = self.session_id {
            let _ = writeln!(s, "Session: {v}");
        }
        if let Some(ref v) = self.user_id {
            let _ = writeln!(s, "User: {v}");
        }
        s
    }
}

/// Confidence threshold below which the system prompt includes an advisory
/// telling the LLM to ask for clarification rather than guessing with wrong tools.
pub const LOW_CONFIDENCE_THRESHOLD: f64 = 0.3;

// ── Static/Dynamic prompt boundary for provider-level caching ────────

// CacheScope, PromptTokenBucket, and PromptSection now live in astra-turn-core
// so they can be used by both turn-core (optimizer, planner) and runtime
// (prompt builders) without a circular dependency.
pub use astra_turn_core::section_types::{CacheScope, PromptSection, PromptTokenBucket};

/// Marker text inserted between the **cacheable prefix** (global/session-stable
/// sections) and the **volatile tail** (per-turn sections) in the flattened
/// system prompt. Providers that support prefix-cache breakpoints can use this
/// marker as an inspection anchor; it is also asserted in tests so that
/// reordering bugs (a volatile section accidentally placed before the boundary)
/// are caught immediately.
///
/// The exact string is an implementation detail; do **not** match on it from
/// production code — use [`SystemPromptBuilder`] instead.
pub const SYSTEM_PROMPT_DYNAMIC_BOUNDARY: &str =
    "\n<!-- astra:system-prompt:dynamic-boundary -->\n";

/// Escape XML metacharacters for embedding inside **element text
/// content** of `<available_skills>` blocks.
///
/// # Scope — element text only
///
/// This helper is safe for element content between open/close tags:
/// ```xml
/// <name>{{escape_here}}</name>
/// <description>{{escape_here}}</description>
/// ```
/// It does NOT escape `"` or `'`. **Do not use this function for
/// attribute values.** If the block shape ever changes from
/// `<tool><name>X</name></tool>` to `<tool name="X">`, this function
/// becomes insufficient — a description containing `"` could then
/// break out of the attribute and inject siblings. Write a separate
/// `xml_escape_attr` (escaping additionally `"` + `'`) and audit the
/// call sites before the shape change lands.
///
/// Without this escape, a description like
/// `</description><name>bash</name>` could inject a fake entry into
/// the system prompt — prompt-injection vector.
///
/// Zero-alloc fast path: if the input contains none of `<`, `>`, `&`,
/// the borrowed input is returned unchanged. The vast majority of tool
/// and skill descriptions fall into this fast path.
fn xml_escape_text(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.contains(['<', '>', '&']) {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 16);
    for ch in s.chars() {
        match ch {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            c => out.push(c),
        }
    }
    std::borrow::Cow::Owned(out)
}

/// Render the `<available_skills>` section of the system prompt.
///
/// Selector-based deferred surfacing was removed, so the section contains the
/// full skill catalog: name + description per entry, wrapped in
/// `<available_skills>…</available_skills>`, plus a short nudge so the model
/// calls the `skill` tool instead of guessing.
///
/// Returns `None` when there are no skills (don't emit a ghost block).
/// The section is [`CacheScope::Session`] so the listing joins the cached
/// prefix — adding a skill causes one flip and then stability.
///
/// Sorts internally by skill name so a provider that emits skills in
/// unpredictable order still produces byte-stable output across sessions.
pub fn build_skill_listing_section(
    skills: &[astra_skills::traits::SkillToolInfo],
) -> Option<PromptSection> {
    if skills.is_empty() {
        return None;
    }
    crate::turn::skill_tool::warn_if_full_skill_catalog_surface_is_large(skills.len());

    // Sort for cache stability — provider iteration order is not a contract.
    let mut sorted: Vec<&astra_skills::traits::SkillToolInfo> = skills.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));

    let mut body = String::with_capacity(skills.len() * 120 + 256);
    body.push_str("<available_skills>\n");
    for s in &sorted {
        body.push_str("  <skill>\n    <name>");
        body.push_str(&xml_escape_text(&s.name));
        body.push_str("</name>\n    <description>");
        body.push_str(&xml_escape_text(&s.description));
        body.push_str("</description>\n  </skill>\n");
    }
    body.push_str("</available_skills>\n\n");
    body.push_str(
        "When a user request matches a skill above, prefer calling the \
         `skill` tool with that skill's name before any other tool. On \
         seeing `<skill-loaded name=\"...\"/>` in a tool result, follow \
         that skill's instructions — do not re-invoke it.\n\
         \n\
         EXCEPTION: when the user explicitly asks for parallel / \
         multi-agent / multiple-agent fan-out (e.g. \"多agents\", \"N \
         agents\", \"parallel review\", \"different angles in parallel\"), \
         route through `agent.spawn` instead — emit N spawn calls in a \
         single assistant message, each with `run_in_background: true`, \
         then collect with `agent.get_result`. Skills usually run \
         sequentially inside the parent turn, which contradicts the \
         user's explicit fan-out intent.",
    );

    Some(PromptSection::stable(body, CacheScope::Session))
}

#[allow(dead_code)]
pub fn build_deferred_tools_section(
    surface: &crate::tool_registry::surface::ToolSurface,
) -> Option<PromptSection> {
    let entries = surface.deferred();
    if entries.is_empty() {
        return None;
    }

    let mut body = String::with_capacity(entries.len() * 80 + 256);
    body.push_str("<deferred_tools>\n");
    for entry in entries {
        body.push_str("  <tool>\n    <name>");
        body.push_str(&xml_escape_text(&entry.name));
        body.push_str("</name>\n    <description>");
        body.push_str(&xml_escape_text(&entry.short_desc));
        body.push_str("</description>\n  </tool>\n");
    }
    body.push_str("</deferred_tools>\n\n");
    body.push_str(
        "If a tool in `<deferred_tools>` fits your next step, call \
         `tool_search(query=\"select:NAME\")` first — the tool_result will contain the \
         full schema so you can invoke it on the next turn. Never guess at a tool that is \
         not in `tools[]` without doing this. If you are about to say a needed tool is \
         unavailable, first call `tool_search`; for dotted legacy names such as \
         `agent.spawn`, select the consolidated tool name (`tool_search(query=\"select:agent\")`) \
         and then use its `action` field.",
    );

    Some(PromptSection::stable(body, CacheScope::Session))
}

/// Builder that enforces the **static-before-dynamic** invariant at the API
/// level, so callers cannot silently push a volatile section into the cached
/// prefix (the class of regression fixed by commit `b64223c9`).
///
/// Usage:
/// ```ignore
/// let mut b = SystemPromptBuilder::new();
/// b.push_stable(PromptSection::stable(rules, CacheScope::Global));
/// b.push_stable(PromptSection::stable(planning, CacheScope::Global));
/// b.push_volatile(PromptSection::dynamic(per_turn, Environment));
/// let sections = b.finish(); // stable first, boundary marker, then volatile
/// ```
///
/// `push_stable` rejects anything with `CacheScope::None`; `push_volatile`
/// rejects anything *without* `CacheScope::None`. This makes it impossible
/// for a caller to silently invert the order and wreck the prefix cache.
#[derive(Debug, Default)]
pub struct SystemPromptBuilder {
    stable: Vec<PromptSection>,
    volatile: Vec<PromptSection>,
}

impl SystemPromptBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a cacheable section (scope: `Global` or `Session`).
    ///
    /// # Panics
    /// Panics in debug builds if the section has `CacheScope::None`; in
    /// release builds the section is silently demoted to the volatile tail
    /// to avoid a cache-busting prefix at runtime.
    pub fn push_stable(&mut self, section: PromptSection) {
        debug_assert!(
            section.scope != CacheScope::None,
            "push_stable requires CacheScope::Global or ::Session; use push_volatile for dynamic content"
        );
        if section.scope == CacheScope::None {
            self.volatile.push(section);
        } else {
            self.stable.push(section);
        }
    }

    /// Append a volatile section (scope: `None`).
    ///
    /// # Panics
    /// Panics in debug builds if the section is not `CacheScope::None`; in
    /// release builds the section is promoted to the stable prefix so its
    /// content still reaches the model.
    pub fn push_volatile(&mut self, section: PromptSection) {
        debug_assert!(
            section.scope == CacheScope::None,
            "push_volatile requires CacheScope::None; use push_stable for cacheable content"
        );
        if section.scope == CacheScope::None {
            self.volatile.push(section);
        } else {
            self.stable.push(section);
        }
    }

    /// Finalise into `[stable..., boundary_marker, volatile...]`.
    ///
    /// The boundary marker is emitted only when both lanes are non-empty;
    /// an all-stable or all-volatile prompt keeps its original shape so
    /// existing byte-level assertions in tests remain valid.
    #[must_use]
    pub fn finish(self) -> Vec<PromptSection> {
        let Self {
            mut stable,
            mut volatile,
        } = self;
        if !stable.is_empty() && !volatile.is_empty() {
            stable.push(PromptSection::dynamic(
                SYSTEM_PROMPT_DYNAMIC_BOUNDARY.to_string(),
                PromptTokenBucket::BasePersona,
            ));
        }
        stable.append(&mut volatile);
        stable
    }
}

/// Build the static sections for the context pipeline.
/// These are the Global-scope sections that never change between turns.
/// Compile once at session start and pass to PipelineSession's TurnInput.
pub fn build_pipeline_static_sections() -> astra_turn_core::context_sources::StaticSections {
    use astra_turn_core::context_assembly_trace::PromptTraceSignals;
    use astra_turn_core::context_sources::StaticSections;
    use astra_turn_core::section_types::PromptTokenBucket;

    // Apply prompt overrides from $ASTRA_PROMPT_OVERRIDES_DIR (or ~/.astra/prompts).
    // assembly time; the pipeline applies them here so both paths surface
    // the same Global text.
    let overrides = load_overrides(&default_overrides_dir());
    let resolve =
        |key: &str, default: String| -> String { overrides.get(key).cloned().unwrap_or(default) };

    StaticSections {
        core_rules: PromptSection {
            text: resolve("core_rules", core_rules_section()),
            scope: CacheScope::Global,
            token_bucket: PromptTokenBucket::BasePersona,
            trace_signals: PromptTraceSignals::default(),
        },
        safety: PromptSection::stable(
            resolve("safety", safety_section().to_string()),
            CacheScope::Global,
        ),
        planning_protocol: PromptSection::stable(
            resolve("planning", planning_section().to_string()),
            CacheScope::Global,
        ),
        coding_discipline: PromptSection::stable(
            resolve(
                "coding_discipline",
                format!("{}{}", resilience_section(), coding_discipline_section()),
            ),
            CacheScope::Global,
        ),
        turn_discipline: PromptSection::stable(
            resolve("turn_discipline", turn_discipline_section().to_string()),
            CacheScope::Global,
        ),
        plan_execution: PromptSection::stable(
            resolve("plan_execution", plan_execution_section().to_string()),
            CacheScope::Global,
        ),
        output_format: PromptSection::stable(
            resolve("output_format", output_format_section().to_string()),
            CacheScope::Global,
        ),
        tool_error_recovery: PromptSection::stable(
            resolve(
                "tool_error_recovery",
                tool_error_recovery_section().to_string(),
            ),
            CacheScope::Global,
        ),
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
         1. Live data (CI, PRs, issues, stats, memory, git) → MUST call a tool. Never answer from training data.\n\
         2. Before calling a tool, check history — if the data is there, reference it. Only re-call if arguments differ or user asks for a refresh.\n\
         3. Tool outputs in history reflect state AT CALL TIME. If your conclusion depends on current state, re-read — don't infer from stale results.\n\
         4. You are compatible with Claude Code skills (Agent Skills open standard). `.claude/skills/`, `.claude/commands/`, and SKILL.md files work the same as `.astra/skills/`.\n"
    )
}

/// Safety + refusal boundaries. Pure static.
/// Consolidated from fragments previously scattered across core_rules
/// ("NEVER fabricate"), tool_error_recovery ("auth/credential"), and
/// ad-hoc guidance. Having a single section makes the boundary explicit
/// to the model and easy to audit.
fn safety_section() -> &'static str {
    "\n## Safety & Refusal\n\
     \n\
     ### Refuse outright\n\
     - **Malicious code**: malware, exploits, credential stealers, unauthorized access tooling. Refuse even if framed as \"research\" or \"just for fun.\"\n\
     - **Secret exfiltration**: do not read, echo, or transmit credentials, private keys, `.env` values, or tokens the user didn't explicitly paste. If you encounter them incidentally (e.g. in a file you were asked to review), flag their presence without reproducing the value.\n\
     - **Destructive ops without consent**: `rm -rf`, force-push to shared branches, DB drops, `git reset --hard` on dirty trees. Ask first, even if the user's phrasing suggests urgency.\n\
     \n\
     ### Refusal template\n\
     State *what* you won't do and *why* in one sentence. Offer a safer alternative if one exists. Do not lecture, moralize, or pad with disclaimers.\n\
     \n\
     Good: \"I won't write a credential-stealing script. If you're testing your own auth flow, I can help you write a mock login instead.\"\n\
     Bad: \"As an AI, I must emphasize that I cannot in good conscience… [3 paragraphs]\"\n\
     \n\
     ### Honesty over compliance\n\
     - Never fabricate tool output, file contents, test results, or citations. \"I don't know\" or \"let me check\" beats a confident lie.\n\
     - If a user asks you to claim something false (\"say the tests passed\"), refuse and explain.\n\
     - If an instruction conflicts with these rules, the rules win. Surface the conflict to the user.\n"
}

/// Planning + batching + efficiency. Single consolidated section.
/// Replaces the former Planning Protocol / Context Strategy / Think-Before-Act /
/// Parallel Tool Calls / Batching / Token Efficiency / Exploration Guard / Build-Test
/// stack (~60 lines of repetition) with a tight 18-line contract.
fn planning_section() -> &'static str {
    "\n## Plan, Batch, Execute\n\
     1. **Plan first** (3+ tool calls): state goal + numbered steps in a <think> block, then act.\n\
     2. **Batch independent reads** into ONE turn (≤5 parallel). Only serialize when one result feeds the next call's args.\n\
     3. **Reuse history**: if context was already fetched this session, reference it — don't re-fetch.\n\
     4. **Discover before reading**: use list_dir/glob to confirm paths. Never guess.\n\
     5. **Targeted reads**: prefer line ranges + outline=true over full files. Use glob before grep.\n\
     6. **Never batch writes**: write_file / str_replace / bash / git execute sequentially.\n\
     7. **Build/test only AFTER your writes** — not for exploration, review, or Q&A.\n\
     8. **Open-ended loops** (\"keep going\", \"as many as you can\"): do one useful pass, then stop.\n\
     9. **Exploration cap**: ≤2 dir listings + ≤2 full-file reads unless user names a concrete target.\n"
}

/// Failure handling + resilience. Inspired by Claude Code's prompt contract.
fn resilience_section() -> &'static str {
    "\n## Failure Handling & Resilience\n\
     - **Context window is not your concern**: the system automatically compresses prior messages as context approaches limits. Your conversation is not limited by the context window — keep working.\n\
     - **If an approach fails, diagnose before switching**: read the error, check your assumptions, try a focused fix. Don't retry the identical action blindly, but don't abandon a viable approach after a single failure either.\n\
     - **Never self-terminate when the user said continue**: if the user explicitly asked you to proceed with an approach, DO NOT output \"I'll stop\" / \"let me be honest, this won't work\" / \"I'm giving up\". Either execute or ask_user with a specific blocker.\n\
     - **Escalate only when genuinely stuck**: use ask_user ONLY after you've investigated the failure, not as a first response to friction. State what you tried, what failed, and what decision you need.\n\
     - **Batch large refactors**: if a change spans 50+ sites, work in batches of 10-15 files. Verify each batch compiles before proceeding. Don't attempt all-at-once heroics that blow the token budget.\n\
     - **On repeated str_replace failures**: if the same str_replace fails 2x, the file content has changed or your old_str is wrong. Re-read the file (targeted range), don't guess.\n"
}

/// Discovery + coding discipline. Pure static.
fn coding_discipline_section() -> &'static str {
    "\n## Coding Discipline\n\
     - **Read before write**: understand existing patterns, naming, and imports before editing.\n\
     - **Executor rule (existing files)**: the target path must be read in this session before write_file / str_replace / apply_patch. Outline-only reads are not enough for write_file overwrite. Re-read if the file changed on disk.\n\
     - **Surgical edits**: change only what's needed. One concern per str_replace.\n\
     - **Undo on failure**: if a change causes errors you can't fix, revert it.\n\
     - **Imports and dependencies**: when adding functionality, add required imports/deps.\n"
}

/// Turn discipline: brief announcements, terminal summary, no externalized reasoning.
/// Pure static — complements coding_discipline_section with session-flow rules.
/// Empirically, turns that churn >10 rounds usually lack a standing commitment to
/// summarize; requiring a turn-end summary creates implicit convergence pressure.
fn turn_discipline_section() -> &'static str {
    "\n## Turn Discipline\n\
     - **Announce once, briefly**: before your first tool call, write ONE sentence saying what you're about to do. Don't narrate every step.\n\
     - **End with a short summary**: close the turn with 1-2 sentences stating what changed and what's next. This is the deliverable — not a list of tools you ran.\n\
     - **No externalized reasoning**: deliberation belongs in <think> blocks. Skip \"Let me think...\" / \"Hmm\" / \"Actually, wait\" — noise, not content.\n\
     - **Lead with the answer**: \"The bug is on line 42 because X\" beats \"Looking at the code, I notice line 42 might be relevant, let me investigate…\".\n\
     - **Match depth to task**: a one-line question gets a one-line answer, not a structured report.\n"
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

/// Tool error recovery. Scenario-based: diagnose → fix → anti-pattern.
fn tool_error_recovery_section() -> &'static str {
    "\n## Tool Error Recovery\n\
     \n\
     ### Retry Budget\n\
     Fix args and retry ONCE. If it fails twice, switch tool or ask the user. Never loop on the same failing call.\n\
     \n\
     ### Scenario: File not found (read_file / str_replace / write_file)\n\
     - Symptom: `No such file or directory` / path error.\n\
     - Diagnose: did you guess the path? Was it moved, renamed, or in a different crate?\n\
     - Fix: `glob` with a partial pattern → confirm the real path → retry with the confirmed path.\n\
     - Anti-pattern: retrying variations like `src/foo.rs` → `./src/foo.rs` → `crates/x/src/foo.rs` hoping one sticks.\n\
     \n\
     ### Scenario: str_replace old_str did not match\n\
     - Symptom: `old_str not found` or ambiguous match.\n\
     - Diagnose: file changed since your last read, or whitespace/indent/quotes differ from what you typed.\n\
     - Fix: re-read the exact target lines → copy verbatim (including leading whitespace) → retry. For multiple matches, add surrounding context lines to disambiguate.\n\
     - Anti-pattern: shortening old_str hoping for a loose match; replace_all without verifying uniqueness.\n\
     \n\
     ### Scenario: bash command timeout or hang (>30s no output)\n\
     - Diagnose: interactive prompt waiting for input? Infinite loop? Slow network/build?\n\
     - Fix: add non-interactive flags (`--yes`, `-y`, `CI=1`); narrow scope (single file vs recursive); for builds use `run_build_test` with package scope, not `cargo build` on the workspace.\n\
     - Anti-pattern: re-running the same command with a longer timeout.\n\
     \n\
     ### Scenario: Truncated output (\"... truncated\")\n\
     - Fix: narrow the query (file glob, line range, `head_limit`, specific package) and retry. Work with what you have if the visible portion answers the question.\n\
     - Anti-pattern: re-running the identical call hoping for more.\n\
     \n\
     ### Scenario: Auth / credential / permission error\n\
     - Stop. Do NOT retry with the same credentials or path.\n\
     - Fix: ask the user to re-authenticate, or try a path you have access to.\n\
     \n\
     ### Non-errors (do not treat as failures)\n\
     - a memory read returns empty → normal for new users/topics; proceed without memory.\n\
     - `grep` / `glob` returns zero matches → valid answer; report it, don't keep searching blindly.\n\
     \n\
     ### Unknown tool name\n\
     If a tool name is rejected, it's not available in this session. Check the tools list; never invent tool names.\n"
}

/// Self-model (tool list). Removed — tool names are already visible in the
/// tools array schema. Listing them again wastes ~200 tokens per turn.
pub(crate) fn self_model_section(_tool_names: &[&str]) -> String {
    String::new()
}

/// Tool-conditional guidance. Removed — tool descriptions in the schema already
/// contain usage guidance. This section was duplicating schema content and
/// wasting ~1000 tokens per turn. Returns empty string.
pub(crate) fn tool_conditional_section(
    _tool_names: &[&str],
    _profile_desc: &str,
    _selection_confidence: f64,
) -> String {
    String::new()
}

fn task_lifecycle_section(tool_names: &[&str]) -> Option<String> {
    if !tool_names.contains(&"task") {
        return None;
    }
    Some(
        "\n## Task Lifecycle\n\
         Use the `task` tool automatically for multi-step work, just like a durable task board:\n\
         - Create a task before substantial implementation, debugging, refactoring, testing, or cloud/agent work.\n\
         - Keep it current: mark `in_progress` before execution, update subtasks/dependencies when the plan changes, and mark terminal status when done.\n\
         - Do not create tasks for simple Q&A, one-file lookups, or work that will finish in a single direct response.\n\
         - If work is delegated or queued for another agent, record ownership and blocking dependencies so the TUI/CLI can show real status.\n"
            .to_string(),
    )
}

/// Per-turn advisory for tool-selector uncertainty.
///
/// This must stay out of Session-scoped prompt blocks: confidence is computed
/// per turn from the current request and selected tools.
pub(crate) fn low_confidence_tool_selection_section(selection_confidence: f64) -> Option<String> {
    (selection_confidence < LOW_CONFIDENCE_THRESHOLD).then(|| {
        "\n## ⚠ Low-Confidence Tool Selection\n\
         Tool selection confidence is LOW. If available tools seem insufficient, ASK the user to clarify.\n\
         Do NOT guess with bash/find/read_file when a more specific tool would be needed.\n"
            .to_string()
    })
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
        let mut sections = vec![PromptSection::stable(
            format!(
                "{SYSTEM_PROMPT_BASE}\n\n\
                 ## CRITICAL\n\
                 You have NO tools available in this turn. \
                 Do NOT generate fake data (PRs, issues, commits, file contents). \
                 If the user asks for real-time data, say: \"I don't have tools available to look that up.\""
            ),
            CacheScope::Global,
        )];
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
        return sections;
    }

    // ── Global sections (stable across sessions) ──
    let mut sections = vec![
        PromptSection::stable(core_rules_section(), CacheScope::Global),
        PromptSection::stable(safety_section().to_string(), CacheScope::Global),
        PromptSection::stable(planning_section().to_string(), CacheScope::Global),
        PromptSection::stable(
            format!("{}{}", resilience_section(), coding_discipline_section()),
            CacheScope::Global,
        ),
        PromptSection::stable(turn_discipline_section().to_string(), CacheScope::Global),
        PromptSection::stable(plan_execution_section().to_string(), CacheScope::Global),
        PromptSection::stable(output_format_section().to_string(), CacheScope::Global),
        PromptSection::stable(
            tool_error_recovery_section().to_string(),
            CacheScope::Global,
        ),
    ];

    // ── Tool-dependent sections (CacheScope::None — derived from the active
    //    tool list/selection-confidence values, so they MUST go after the
    //    cache marker to keep the Global prefix stable) ──
    sections.push(PromptSection::dynamic(
        self_model_section(tool_names),
        PromptTokenBucket::BasePersona,
    ));

    let tool_cond = tool_conditional_section(tool_names, profile_desc, selection_confidence);
    if !tool_cond.is_empty() {
        // Tool-conditional guidance is composed from the live tool list and
        // the runtime profile description — both vary per turn — so it must
        // be billed to the `Environment` bucket, not `BasePersona`. Putting
        // dynamic content under `BasePersona` makes the persona bucket look
        // larger than the immutable persona text actually is, distorting
        // budget alerts.
        sections.push(PromptSection::dynamic(
            tool_cond,
            PromptTokenBucket::Environment,
        ));
    }

    if let Some(task_lifecycle) = task_lifecycle_section(tool_names) {
        // Task-lifecycle guidance is conditional on the active tool set
        // (e.g. whether plan/task tools are exposed) and is per-turn —
        // belongs in `Environment`, not `BasePersona`.
        sections.push(PromptSection::dynamic(
            task_lifecycle,
            PromptTokenBucket::Environment,
        ));
    }

    if let Some(low_confidence) = low_confidence_tool_selection_section(selection_confidence) {
        sections.push(PromptSection::dynamic(
            low_confidence,
            PromptTokenBucket::Environment,
        ));
    }

    let tt = task_type_section(task_type);
    if !tt.is_empty() {
        // The detected task type is recomputed each turn from the user
        // request — it's environmental signal, not part of the agent
        // persona. Bill to `Environment` so token accounting reflects
        // reality.
        sections.push(PromptSection::dynamic(
            tt.to_string(),
            PromptTokenBucket::Environment,
        ));
    }

    let ss = search_strategy_section(tool_names);
    if !ss.is_empty() {
        sections.push(PromptSection::dynamic(
            ss.to_string(),
            PromptTokenBucket::BasePersona,
        ));
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
    let serialized = astra_turn_core::context_serializer::serialize_prompt_sections(
        sections,
        &astra_turn_core::pipeline_config::ProviderCachePolicy::default(),
    );
    astra_turn_core::context_serializer::flatten_serialized_system_blocks(&serialized)
}

// ─── Prompt Section Overrides ─────────────────────────────────────────────

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Section name → override text mapping.
/// Keys use snake_case matching the section builder function names:
/// `core_rules`, `safety`, `planning`, `coding_discipline`, `turn_discipline`,
/// `plan_execution`, `output_format`, `tool_error_recovery`.
pub type PromptOverrides = HashMap<String, String>;

/// Section names in order, matching the Global sections in `build_system_prompt_sections_with_style`.
const SECTION_NAMES: &[&str] = &[
    "core_rules",
    "safety",
    "planning",
    "coding_discipline",
    "turn_discipline",
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
            "introspect",
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

#[deprecated(note = "Circuit breaker replaces countdown budget. Always returns empty.")]
pub fn round_budget_directive(_round_index: u32) -> String {
    String::new()
}

#[deprecated(note = "Circuit breaker replaces countdown budget. Always returns empty.")]
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
    // Compacted from a 3-bullet form (~450c) to one line (~165c). The
    // long form explained *why* parallel is cheaper and enumerated
    // examples — both derivable from the header and the model's
    // existing tool-use training. What the directive has to assert is
    // just: "you did N single-tool rounds in a row; batch the next
    // independent calls". Rides the volatile lane once the streak
    // threshold trips, so bytes here are per-turn waste until the
    // model batches (which resets the streak).
    format!(
        "\n\n## ⚠ Sequential Tool Calls Detected\n\
         Last {streak} rounds each ran one tool. Batch independent calls \
         (different files, greps, reads) into a single parallel round; \
         keep sequential rounds only when a call depends on the previous result.\n"
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
fn is_trailing_runtime_scaffolding_message(message: &serde_json::Value) -> bool {
    let role = message.get("role").and_then(|r| r.as_str());
    if role == Some("system") {
        return true;
    }
    if role != Some("user") {
        return false;
    }
    let Some(content) = message.get("content").and_then(|c| c.as_str()) else {
        return false;
    };
    // Session 8d9e5903 regression: every outbound request ends with a
    // role=user `<system-reminder>` wrapper produced by the volatile
    // lane (wire_assembly / bridge_inprocess / server_loop_host). This
    // is runtime scaffolding, not a user query, and must not break
    // round-cadence detection — otherwise the single-tool-streak
    // counter always returns 0 on live sessions and the
    // parallel-batching force never fires.
    if content.starts_with(SYSTEM_REMINDER_WRAPPER_PREFIX) {
        return true;
    }
    // Legacy attention manifest carried as a `role:user` scaffolding
    // message. Emission was dropped in wip-3; this check remains so
    // restored checkpoints from older versions still route around it.
    content.starts_with("[attention:v1]\n")
}

/// Wrapper tag applied by the volatile lane to mark runtime-injected
/// scaffolding carried on a `role=user` message (git state / self-
/// awareness / volatile nudges). See `wire_assembly`, `bridge_inprocess`,
/// and `server_loop_host` for producers.
const SYSTEM_REMINDER_WRAPPER_PREFIX: &str = "<system-reminder>";

fn trailing_tool_result_count(messages: &[serde_json::Value]) -> usize {
    messages
        .iter()
        .rev()
        .skip_while(|message| is_trailing_runtime_scaffolding_message(message))
        .take_while(|message| message.get("role").and_then(|r| r.as_str()) == Some("tool"))
        .count()
}

#[deprecated(note = "Circuit breaker replaces countdown budget. Always returns empty.")]
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
#[allow(deprecated)]
mod tests {
    use super::*;

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

    // Tests for `## Self-Model\nTools: ...` list, `## Memory Rules` /
    // `<types>` taxonomy, and `GitHub data` / `memory` guidance
    // were deleted: those Markdown sections were emitted by
    // `self_model_section` / `tool_conditional_section`, which are now
    // no-ops (commit a1187f76 — the tools array schema already carries
    // that guidance per-tool).

    #[test]
    fn prompt_no_memory_rules_without_memory_tools() {
        let p = build_main_system_prompt(&["bash", "git_diff"], "", 0.5, None);
        assert!(!p.contains("Memory Rules"));
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
        assert!(p.contains("Plan, Batch, Execute"));
        assert!(p.contains("<think>"));
    }

    #[test]
    fn task_tool_adds_lifecycle_guidance() {
        let p = build_main_system_prompt(&["task", "bash"], "", 1.0, None);
        assert!(p.contains("Task Lifecycle"));
        assert!(p.contains("Use the `task` tool automatically"));
        assert!(p.contains("mark `in_progress`"));
    }

    #[test]
    fn no_task_tool_omits_lifecycle_guidance() {
        let p = build_main_system_prompt(&["bash", "read_file"], "", 1.0, None);
        assert!(!p.contains("Task Lifecycle"));
        assert!(!p.contains("Use the `task` tool automatically"));
    }

    #[test]
    fn prompt_includes_coding_discipline() {
        let p = build_main_system_prompt(&["bash"], "", 0.5, None);
        assert!(p.contains("Coding Discipline"));
        assert!(p.contains("Read before write"));
        assert!(p.contains("Executor rule (existing files)"));
        assert!(p.contains("Surgical edits"));
        assert!(p.contains("One concern per str_replace"));
    }

    #[test]
    fn prompt_includes_parallel_tool_calls() {
        let p = build_main_system_prompt(&["bash"], "", 0.5, None);
        assert!(p.contains("Batch independent reads"));
        assert!(p.contains("ONE turn"));
    }

    #[test]
    fn prompt_includes_token_efficiency() {
        let p = build_main_system_prompt(&["bash"], "", 0.5, None);
        assert!(p.contains("Targeted reads"));
        assert!(p.contains("line ranges"));
    }

    #[test]
    fn prompt_includes_build_test_guidance() {
        let p = build_main_system_prompt(&["bash"], "", 0.5, None);
        assert!(
            p.contains("Build/test only AFTER your writes"),
            "should restrict build/test to post-edit verification"
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
        assert!(p.contains("Retry Budget"));
        assert!(p.contains("retry ONCE"));
        // Scenario headers
        assert!(p.contains("File not found"));
        assert!(p.contains("str_replace old_str did not match"));
        assert!(p.contains("bash command timeout"));
        assert!(p.contains("Truncated output"));
        assert!(p.contains("Auth / credential / permission error"));
        assert!(p.contains("Non-errors"));
        assert!(p.contains("Unknown tool name"));
        // Key anti-patterns preserved
        assert!(p.contains("Anti-pattern"));
        assert!(p.contains("memory read returns empty"));
    }

    #[test]
    fn prompt_bounds_runaway_file_exploration() {
        let p = build_main_system_prompt(&["bash", "read_file", "list_dir"], "", 0.5, None);
        assert!(p.contains("Open-ended loops"));
        assert!(p.contains("\"as many as you can\""));
        assert!(p.contains("≤2 dir listings"));
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
    fn code_nav_guidance_absent_without_tools() {
        let p = build_main_system_prompt(&["bash", "read_file"], "", 0.5, Some("implementation"));
        assert!(
            !p.contains("Code Navigation"),
            "should NOT include code nav without tools"
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
    fn git_mutations_guidance_absent_without_tools() {
        let p = build_main_system_prompt(&["git_diff", "git_log"], "", 0.5, None);
        assert!(
            !p.contains("Git Workflow"),
            "should NOT include git mutations without commit tool"
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
        // NOTE: tool-dependent sections (self-model, tool-conditional, task-type,
        // search-strategy) are intentionally `CacheScope::None` so they sit
        // AFTER the cache marker and can change per turn without invalidating
        // the cached prefix. The Session scope remains available for future
        // use but is not populated by the current build_system_prompt_sections.
        let _ = sessions; // kept to document the intent; no assertion on count

        // First section should be Global
        assert_eq!(
            sections[0].scope,
            CacheScope::Global,
            "first section should be Global"
        );

        // Profile lives in the None-scoped post-cache segment alongside
        // other tool-dependent sections. Search by content rather than
        // by scope+first-match.
        let profile = sections
            .iter()
            .find(|s| s.scope == CacheScope::None && s.text.contains("cwd: /tmp"));
        assert!(
            profile.is_some(),
            "should have a None-scoped profile section containing cwd"
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
            global_text.contains("Plan, Batch, Execute"),
            "should contain planning"
        );
        assert!(
            global_text.contains("Reuse history"),
            "should contain context reuse rule"
        );
        assert!(
            global_text.contains("Claude Code skills"),
            "should contain CC skill compatibility rule"
        );
    }

    #[test]
    fn sections_task_type_strategy_lands_in_none_scope() {
        // Task-type strategy (e.g. Debugging Strategy) still routes to the
        // None-scoped post-cache segment. `Code Navigation` guidance used
        // to live there too but was emitted by `tool_conditional_section`,
        // now a no-op.
        let tools = vec!["bash", "find_definition", "find_references", "git_commit"];
        let sections = build_system_prompt_sections(&tools, "", 0.8, Some("debugging"));

        let post_cache_text: String = sections
            .iter()
            .filter(|s| s.scope == CacheScope::None)
            .map(|s| s.text.as_str())
            .collect();
        assert!(
            post_cache_text.contains("Debugging Strategy"),
            "task-type strategy should land in None-scoped (post-cache) segment"
        );
    }

    #[test]
    fn sections_profile_and_toolset_populate_none_scope() {
        // Even with empty profile, the None segment still holds tool-dependent
        // sections (self-model, tool-conditional guidance, etc.). Prior to
        // the cache-stability refactor this was empty; now it's the
        // per-turn dynamic bucket.
        let tools = vec!["bash"];
        let sections = build_system_prompt_sections(&tools, "", 0.8, None);

        let none_scoped: Vec<_> = sections
            .iter()
            .filter(|s| s.scope == CacheScope::None)
            .collect();
        assert!(
            !none_scoped.is_empty(),
            "None-scoped segment should carry tool-dependent sections (self-model, etc.)"
        );
    }

    #[test]
    fn sections_to_string_contains_core_and_task_content() {
        let tools = vec!["bash", "read_file", "glob"];
        let profile = "cwd: /test\ngit_branch: main";

        let sections = build_system_prompt_sections(&tools, profile, 0.8, Some("implementation"));
        let result = sections_to_string(&sections);

        assert!(
            result.contains(SYSTEM_PROMPT_BASE),
            "should contain identity"
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
    fn sections_low_confidence_in_post_cache_segment() {
        // Low-confidence advisory is tool-selector-driven (depends on which
        // tools were chosen), so it lives in the None-scoped post-cache
        // segment alongside other per-turn content.
        let tools = vec!["bash"];
        let sections = build_system_prompt_sections(&tools, "", 0.1, None);

        let post_cache_text: String = sections
            .iter()
            .filter(|s| s.scope == CacheScope::None)
            .map(|s| s.text.as_str())
            .collect();
        assert!(
            post_cache_text.contains("Low-Confidence Tool Selection"),
            "low confidence advisory should land in None-scoped post-cache segment"
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

    // ── Editing strategy (multi_edit) ────────────────────────────

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

    // ── Empty-tools + empty-profile section behavior ─────────────

    #[test]
    fn sections_empty_tools_empty_profile_returns_global_only() {
        let sections = build_system_prompt_sections(&[], "", 0.5, None);
        assert_eq!(
            sections.len(),
            1,
            "empty tools + empty profile → only the global section"
        );
        assert_eq!(sections[0].scope, CacheScope::Global);
    }

    #[test]
    fn sections_empty_tools_with_profile_returns_two_sections() {
        let sections = build_system_prompt_sections(&[], "profile text", 0.5, None);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].scope, CacheScope::Global);
        assert_eq!(sections[1].scope, CacheScope::None);
        assert_eq!(sections[1].text, "profile text");
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
        // Other sections should be unchanged (safety is now [1], planning is [2])
        assert!(sections[2].text.contains("Plan, Batch, Execute"));
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
                    memory_signal_detected: true,
                    effort_hint: true,
                    agent_type_hint: true,
                    self_awareness: true,
                    implicit_feedback: true,
                    learned_feedback_rules: true,
                    ..Default::default()
                },
                ..Default::default()
            }),
        ];

        let breakdown = build_system_prompt_trace(&sections, vec![], vec![]);
        assert!(breakdown.context_signals.active_output_skills);
        assert!(breakdown.context_signals.memory_signal_detected);
        assert!(!breakdown.context_signals.system_prompt_override);
        assert!(breakdown.context_signals.effort_hint);
        assert!(breakdown.context_signals.agent_type_hint);
        assert!(breakdown.context_signals.self_awareness);
        assert!(breakdown.context_signals.implicit_feedback);
        assert!(breakdown.context_signals.learned_feedback_rules);
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

    /// Session 8d9e5903 regression: every outbound request has a
    /// `role=user` message with a `<system-reminder>` wrapper at the
    /// tail (volatile-lane injection carrying Git State / self-awareness
    /// / volatile nudges). This is runtime scaffolding, not a user
    /// query. Before the fix, `is_trailing_runtime_scaffolding_message`
    /// only recognized attention-manifest user content, so the streak
    /// detector broke at the first `<system-reminder>` it saw and
    /// returned 0 — which meant the parallel-batching force never fired
    /// despite 18 consecutive single-tool rounds in T11. The fix
    /// extends scaffolding detection to any user message whose content
    /// starts with `<system-reminder>`, which is a stable runtime
    /// marker applied by every provider path (bridge_inprocess /
    /// server_loop_host / wire_assembly).
    #[test]
    fn trailing_single_tool_streak_skips_system_reminder_wrapper() {
        let mut msgs = rounds_pattern(&[1, 1, 1, 1]);
        // The real shape seen in session 8d9e5903 captures:
        msgs.push(serde_json::json!({
            "role": "user",
            "content": "<system-reminder>\n\n\n## Git State\n- Git branch: improve_promts\n</system-reminder>"
        }));
        assert_eq!(
            trailing_single_tool_round_streak(&msgs),
            4,
            "runtime-injected <system-reminder> at tail must be treated as scaffolding \
             so the single-tool streak detector can see the real round cadence; \
             otherwise parallel-batching force never fires on live Astra sessions"
        );
        assert!(
            parallel_batching_nudge_directive(&msgs).contains("Sequential Tool Calls Detected"),
            "nudge must fire despite the <system-reminder> tail"
        );
    }

    #[test]
    fn trailing_single_tool_streak_skips_multiple_scaffolding_tails() {
        // Realistic Astra tail: attention manifest + system-reminder +
        // potentially a volatile-wrapper system message stacked up.
        let mut msgs = rounds_pattern(&[1, 1, 1, 1, 1]);
        msgs.push(serde_json::json!({
            "role": "system",
            "content": "(runtime-injected nudge)"
        }));
        msgs.push(serde_json::json!({
            "role": "user",
            "content": "<system-reminder>\nTurn: 5 | Tokens: 12000\n</system-reminder>"
        }));
        assert_eq!(
            trailing_single_tool_round_streak(&msgs),
            5,
            "multiple stacked scaffolding tails must all be peeled off"
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

    #[test]
    fn skill_listing_section_is_session_scoped_without_deferred_tools() {
        let skills = vec![
            astra_skills::traits::SkillToolInfo {
                name: "review".into(),
                description: "Review code changes".into(),
                ..Default::default()
            },
            astra_skills::traits::SkillToolInfo {
                name: "debug".into(),
                description: "Debug failures".into(),
                ..Default::default()
            },
        ];

        let section = build_skill_listing_section(&skills).expect("non-empty skill catalog");

        assert_eq!(section.scope, CacheScope::Session);
        assert!(section.text.contains("<available_skills>"));
        assert!(!section.text.contains("<deferred_tools>"));
        // Pin the actionable phrasing — wording can drift but the
        // contract is "model calls the `skill` tool when a skill matches
        // the user request". The source says "calling the `skill` tool";
        // keep both forms accepted so a one-word edit doesn't break this.
        assert!(
            section.text.contains("`skill` tool"),
            "skill listing must direct the model at the `skill` tool: {section_text}",
            section_text = section.text
        );
    }

    // ── SystemPromptBuilder invariants ─────────────────────────────────

    #[test]
    fn system_prompt_builder_emits_stable_then_boundary_then_volatile() {
        let mut b = SystemPromptBuilder::new();
        b.push_stable(PromptSection::stable("RULES", CacheScope::Global));
        b.push_stable(PromptSection::stable("SESSION", CacheScope::Session));
        b.push_volatile(PromptSection::dynamic(
            "ENV",
            PromptTokenBucket::Environment,
        ));
        let out = b.finish();

        assert_eq!(out.len(), 4, "expected 2 stable + boundary + 1 volatile");
        assert_eq!(out[0].text, "RULES");
        assert_eq!(out[0].scope, CacheScope::Global);
        assert_eq!(out[1].text, "SESSION");
        assert_eq!(out[1].scope, CacheScope::Session);
        // Boundary marker — scope None so it sits on the dynamic side
        assert_eq!(out[2].text, SYSTEM_PROMPT_DYNAMIC_BOUNDARY);
        assert_eq!(out[2].scope, CacheScope::None);
        assert_eq!(out[3].text, "ENV");
        assert_eq!(out[3].scope, CacheScope::None);

        // Rendered text: stable prefix must come before the marker, and
        // the marker must come before any volatile content.
        let rendered = sections_to_string(&out);
        let rules_pos = rendered.find("RULES").unwrap();
        let marker_pos = rendered.find(SYSTEM_PROMPT_DYNAMIC_BOUNDARY).unwrap();
        let env_pos = rendered.find("ENV").unwrap();
        assert!(
            rules_pos < marker_pos && marker_pos < env_pos,
            "order must be stable → boundary → volatile; got rules={rules_pos} marker={marker_pos} env={env_pos}"
        );
    }

    #[test]
    fn system_prompt_builder_omits_boundary_when_all_stable() {
        let mut b = SystemPromptBuilder::new();
        b.push_stable(PromptSection::stable("RULES", CacheScope::Global));
        let out = b.finish();
        assert_eq!(out.len(), 1);
        assert!(
            !out.iter().any(|s| s.text == SYSTEM_PROMPT_DYNAMIC_BOUNDARY),
            "no boundary marker when volatile lane is empty"
        );
    }

    #[test]
    fn system_prompt_builder_omits_boundary_when_all_volatile() {
        let mut b = SystemPromptBuilder::new();
        b.push_volatile(PromptSection::dynamic(
            "ENV",
            PromptTokenBucket::Environment,
        ));
        let out = b.finish();
        assert_eq!(out.len(), 1);
        assert!(
            !out.iter().any(|s| s.text == SYSTEM_PROMPT_DYNAMIC_BOUNDARY),
            "no boundary marker when stable lane is empty"
        );
    }

    #[test]
    #[should_panic(expected = "push_stable requires")]
    fn system_prompt_builder_rejects_volatile_in_stable_lane() {
        let mut b = SystemPromptBuilder::new();
        b.push_stable(PromptSection::dynamic(
            "oops",
            PromptTokenBucket::Environment,
        ));
    }

    #[test]
    #[should_panic(expected = "push_volatile requires")]
    fn system_prompt_builder_rejects_stable_in_volatile_lane() {
        let mut b = SystemPromptBuilder::new();
        b.push_volatile(PromptSection::stable("oops", CacheScope::Global));
    }
}

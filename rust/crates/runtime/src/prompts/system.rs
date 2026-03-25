/// Agent persona / base identity.
#[allow(dead_code)]
pub const SYSTEM_PROMPT_BASE: &str =
    "You are a development assistant. Use tools to solve tasks exactly as asked.";

/// Full system-prompt body when tools are available.
///
/// `tool_names`   – comma-joined list of tool names (for the self-model section)
/// `profile_desc` – optional project-profile block appended after the tool list
#[allow(dead_code)]
pub fn build_main_system_prompt(tool_names: &[&str], profile_desc: &str) -> String {
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

    // Detect which tool categories are available so the prompt only references
    // tools the LLM can actually call.  This enforces the invariant:
    //   "system prompt mentions tool X  ⟹  tool X is in the selection"
    // Violating this causes the LLM to *describe* calling a tool in text
    // instead of generating a function_call.
    let has_memory = tool_names.iter().any(|n| n.starts_with("memory"));
    let has_github = tool_names.iter().any(|n| n.starts_with("github"));

    let mut prompt = format!(
        "{SYSTEM_PROMPT_BASE}\n\n\
         ## Self-Model\n\
         Tools: {}{}\n\n\
         ## Core Rules\n\
         1. Think step-by-step, then act.\n\
         2. NEVER fabricate data. Your training data is stale — always use tools for real-time info.\n\
         3. Do ONLY what the user asked. When done → STOP and report.\n\
         4. If the answer requires current/live data → MUST call a tool. Do NOT answer from training data.\n\
         5. If the answer is already in the current conversation history → answer directly.\n",
        tool_names.join(", "),
        profile_desc,
    );

    // ── GitHub rules: only when GitHub tools are selected ──
    if has_github {
        prompt.push_str(
            "\
         6. For GitHub data (PRs, issues, commits): use github_list_prs / github_list_issues tools directly.\n",
        );
    }

    // ── Anti-hallucination: always present ──
    prompt.push_str(
        "\n\
         ## ANTI-HALLUCINATION (CRITICAL)\n\
         - NEVER generate fake PR numbers, issue numbers, commit SHAs, dates, or contributor names.\n\
         - If you cannot call a tool to verify data, say \"I need to look this up\" and call the tool.\n\
         - If a tool call fails, report the failure — do NOT substitute made-up data.\n\
         - Violations: generating realistic-looking but fabricated GitHub data is WORSE than saying \"I don't know\".\n",
    );

    // ── Memory rules: only when memory tools are selected ──
    // memory_store and memory_search are pinned, so this section is present in
    // almost every turn.  If a future configuration un-pins them, the prompt
    // gracefully omits these rules and avoids the "describe instead of call" bug.
    if has_memory {
        prompt.push_str(
            "\n\
         ## Memory\n\
         7. Memory rules:\n\
            - '## User Memories' (when present above) contains relevant user context — ALWAYS check it before calling tools.\n\
            - If User Memories contains a repo mapping (e.g. 'memoria = matrixorigin/Memoria'), USE that exact repo — do NOT search for alternatives.\n\
            - STORE to memory_store when user states a preference, convention, decision, or important fact.\n\
            - When storing: use tag format '[@ns/status] content' (ns: pref, fact, knowledge, task, plan, insight).\n\
            - DO NOT store: ephemeral tool outputs, raw file contents, or things already in memory.\n",
        );
    }

    // ── Reasoning protocol: always present ──
    prompt.push_str(
        "\n\
         ## Reasoning Protocol\n\
         Use this structure for non-trivial tasks:\n\
         <think>\n\
         [Goal] What does the user want?\n\
         [Plan] What steps are needed?\n\
         [Tool] Which tool fits best? One call per intent.\n\
         </think>\n\
         Then act. After tool results:\n\
         <reflect>\n\
         [Result] Did it work? [Next] Continue or report?\n\
         </reflect>",
    );

    prompt
}

/// Injected into conversation when the agent repeats the same tool calls.
pub const STALL_NUDGE: &str = "You appear to be repeating the same tool calls. \
     Please try a different approach or summarize what you've found so far.";

pub fn skill_content() -> String {
    format!(
        r#"---
name: batch
description: "Research, plan, and execute a large-scale change in parallel across isolated sub-agents, each producing a verified commit"
version: "2.0.0"
allowed_tools:
  - delegate
  - bash
  - read_file
  - write_file
triggers:
  - batch
  - parallel
  - "for each"
  - bulk
  - "across all"
when_to_use: "When the user explicitly wants to make a sweeping, mechanical change across many files (migrations, refactors, bulk renames) that can be decomposed into independent parallel units"
category: automation
arguments:
  - name: INSTRUCTION
    description: "Description of the batch change to make"
    required: true
tags:
  - parallel
  - automation
---
# Batch: Parallel Work Orchestration

You are orchestrating a large, parallelizable change across this codebase.

**Working directory**: ${{CTX_WORK_DIR}}

## Prerequisite

This skill requires a git repository. If this is not a git repo, tell the user and stop.

## Phase 1: Research and Plan

**Success criteria**: A concrete plan with numbered work units, each independently implementable and verifiable.

1. **Understand the scope.** Use the `delegate` tool to launch one or more sub-agents (foreground — you need their results) to deeply research what the instruction touches. Find all the files, patterns, and call sites that need to change. Understand existing conventions so the change is consistent.

2. **Decompose into independent units.** Break the work into 5–15 self-contained units. Each unit must:
   - Be independently implementable (no shared state with sibling units)
   - Be mergeable on its own without depending on another unit landing first
   - Be roughly uniform in size — split large units, merge trivial ones
   
   Scale the count to the actual work. Prefer per-directory or per-module slicing over arbitrary file lists.
   
   Each unit should take 5–15 minutes for a worker to execute independently. If a unit would take <2 min, merge it; >30 min, split it. For truly massive changes, go up to 30 units.

3. **Determine the verification recipe.** Figure out how a worker can verify its change actually works:
   - An existing test suite the worker can run
   - A build command that must succeed
   - A dev-server + curl pattern for API changes
   
   If you cannot find a concrete verification path, ask the user. Unit tests are a minimum baseline — do not skip verification entirely.

4. **Write the plan.** Include:
   - Summary of what you found during research
   - Numbered list of work units — each with: title, file list, one-line description
   - The verification recipe
   - The exact worker instructions (shared template)

5. **Present the plan for approval.** Do NOT proceed without user confirmation.

## Phase 2: Spawn Workers

**Success criteria**: All worker agents launched in parallel, each with a fully self-contained prompt.

Once the plan is approved, spawn one background agent per work unit using the `delegate` tool. **Launch them all in a single message so they run in parallel.**

For each agent, the prompt must be fully self-contained. Include:
- The overall goal (the user's instruction)
- This unit's specific task (title, file list, change description — copied from your plan)
- Codebase conventions discovered in Phase 1
- The verification recipe

Worker post-implementation steps (include verbatim):
```
1. Run the project's test suite. If tests fail, fix them.
2. Follow the verification recipe from the coordinator's plan.
3. Commit all changes with a clear message.
4. Report: end with a summary of what was changed and verified.
```

## Phase 3: Track Progress

**Success criteria**: Final status table showing all units completed or with clear failure reasons.

After launching all workers, render a status table:

| # | Unit | Status | Result |
|---|------|--------|--------|
| 1 | <title> | running | — |

As agents complete, update the table. When all agents have reported, render the final table and a one-line summary (e.g., "12/15 units completed successfully").

If a worker fails: note the failure reason in the table. Do NOT auto-retry — report the failure and let the user decide whether to retry, fix manually, or skip.

## Rules
- **Worker prompts must be self-contained** — workers have no access to your context. Include everything they need.
- **No file may be modified by multiple units** — if you detect this during planning, restructure the units.
- **Prefer many small units over few large ones** — easier to verify and retry.
"#,
    )
}

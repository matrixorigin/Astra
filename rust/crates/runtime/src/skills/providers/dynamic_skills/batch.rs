pub fn skill_content() -> String {
    format!(
        r#"---
name: batch
description: "Research, plan, and execute a large-scale change in parallel across isolated sub-agents, each producing a verified commit"
version: "1.0.0"
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
**Git branch**: ${{CTX_GIT_BRANCH}}
**Project type**: ${{CTX_PROJECT_TYPE}}

## Prerequisite

This skill requires a git repository. If this is not a git repo, tell the user and stop.

## Phase 1: Research and Plan

1. **Understand the scope.** Use `delegate` to launch sub-agents that research what the instruction touches — find all files, patterns, and call sites. Understand existing conventions.

2. **Decompose into independent units** (5–15, up to 30 for massive changes). Each unit must:
   - Be independently implementable (no shared state with siblings)
   - Be mergeable on its own
   - Take 5–15 minutes for a worker (<2 min → merge, >30 min → split)
   - Prefer per-directory or per-module slicing

3. **Determine verification recipe** based on project type:
   - Rust → `cargo test`, `cargo clippy`
   - Node → `npm test`, `npx tsc --noEmit`
   - Go → `go test ./...`, `go vet`
   - Python → `pytest`, `ruff check`
   - Or the project's Makefile `check`/`test` target

4. **Write the plan**: summary, numbered work units (title, file list, description), verification recipe, worker instructions.

5. **Present for approval.** Do NOT proceed without user confirmation.

## Phase 2: Spawn Workers

Launch one background agent per unit via `delegate` — all in a single message for parallel execution.

Each worker prompt must be self-contained:
- Overall goal + this unit's specific task
- Codebase conventions from Phase 1
- Verification recipe
- Post-implementation steps: run tests, verify, commit, report summary

## Phase 3: Track Progress

Render a status table as agents complete:

| # | Unit | Status | Result |
|---|------|--------|--------|

When all done, render final table + one-line summary. If a worker fails, report the failure — do NOT auto-retry.

## Rules
- Worker prompts must be self-contained — workers have no access to your context
- No file may be modified by multiple units
- Prefer many small units over few large ones
"#,
    )
}

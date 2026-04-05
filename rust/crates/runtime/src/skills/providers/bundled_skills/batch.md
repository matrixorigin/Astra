---
name: batch
description: "Research, plan, and execute a large-scale change in parallel across isolated sub-agents, each producing a verified commit"
version: "1.1.0"
context: fork
triggers:
  - batch
  - parallel
  - "for each"
  - bulk
  - "across all"
  - migration
when_to_use: "When the user wants to make a sweeping, mechanical change across many files (migrations, refactors, bulk renames) that can be decomposed into independent parallel units"
category: automation
arguments:
  - name: INSTRUCTION
    description: "Description of the batch change to make"
    required: true
tags:
  - parallel
  - automation
  - batch
---
# Batch: Parallel Work Orchestration

You are orchestrating a large, parallelizable change across the codebase.

## User Instruction

$ARGUMENTS

If `$ARGUMENTS` is empty, ask the user what change they want to make. Do NOT proceed without a clear instruction.

## Prerequisite

This skill requires a git repository. If you're not in one, tell the user and stop.

## Phase 1: Research and Plan

1. **Understand the scope.** Launch sub-agents (foreground — you need their results) to deeply research what the instruction touches. Find all the files, patterns, and call sites that need to change. Understand existing conventions so the migration is consistent.

2. **Decompose into independent units.** Break the work into 5–30 self-contained units. Each unit must:
   - Be independently implementable (no shared state with sibling units)
   - Be mergeable on its own without depending on another unit landing first
   - Be roughly uniform in size (split large units, merge trivial ones)

   Scale the count to the actual work: few files → closer to 5; hundreds of files → closer to 30. Prefer per-directory or per-module slicing over arbitrary file lists.

3. **Determine the verification recipe.** Figure out how a worker can verify its change works end-to-end — not just that unit tests pass:
   - A dev-server + curl pattern (for API changes)
   - An existing e2e/integration test suite
   - Manual smoke test instructions
   - "Unit tests are sufficient" (for pure refactors)

   If you cannot find a concrete verification path, ask the user. Do not skip this — the workers cannot ask the user themselves.

4. **Write the plan.** Include:
   - Summary of what you found during research
   - Numbered list of work units — for each: a short title, the list of files/directories it covers, and a one-line description of the change
   - The verification recipe (or "skip e2e because …" with justification)
   - The worker instructions template

Present the plan for user approval before proceeding.

## Phase 2: Execute Workers

After the plan is approved, spawn one sub-agent per work unit. Launch them all at once so they run in parallel.

Each worker's prompt must be fully self-contained. Include:
- The overall goal (the user's instruction)
- This unit's specific task (title, file list, change description — copied verbatim from your plan)
- Any codebase conventions you discovered
- The verification recipe from your plan

**Worker post-implementation steps** (include verbatim in each worker prompt):
1. **Simplify** — Review and clean up your changes for code quality.
2. **Run unit tests** — Run the project's test suite. If tests fail, fix them.
3. **Verify end-to-end** — Follow the verification recipe from the plan. If the recipe says to skip for this unit, skip it.
4. **Commit** — Commit all changes with a clear message describing what was changed and why.
5. **Report** — End with a status line so the coordinator can track progress.

## Phase 3: Track and Report

Render an initial status table when workers launch:

| # | Unit | Status | Details |
|---|------|--------|---------|
| 1 | <title> | running | — |
| 2 | <title> | running | — |

As workers complete, update the table with `done` or `failed` and details. When all workers have reported, render the final table and a one-line summary: "N/M units completed successfully."

## Rules
- Never modify the same file in two parallel units
- Each unit must be verifiable independently
- If the request can't be parallelized, explain why and execute sequentially
- Keep unit scope small — better many small units than few large ones
- If a unit fails, continue with others — don't abort the batch
- Worker prompts must be self-contained (workers have no access to your context)

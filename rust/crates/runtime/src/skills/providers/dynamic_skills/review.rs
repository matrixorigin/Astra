pub fn skill_content() -> String {
    format!(
        r#"---
name: review
description: "Review changed code for reuse, quality, and efficiency via three parallel review agents — then fix issues found"
version: "2.0.0"
allowed_tools:
  - delegate
  - bash
  - read_file
  - write_file
triggers:
  - review
  - simplify
  - "clean up code"
when_to_use: "When the user wants their recent code changes reviewed for unnecessary complexity, readability issues, or refactoring opportunities"
category: code-review
tags:
  - review
  - quality
---
# Review: Code Review and Cleanup

Review all changed files for reuse, quality, and efficiency. Fix any issues found.

**Working directory**: ${{CTX_WORK_DIR}}
**Git branch**: ${{CTX_GIT_BRANCH}}

## Phase 1: Identify Changes

**Success criteria**: You have a clear list of changed files and their diffs.

Run `git diff` (or `git diff HEAD` if there are staged changes) to see what changed. If there are no git changes, review the most recently modified files that the user mentioned or that you edited earlier in this conversation.

## Phase 2: Launch Three Review Agents in Parallel

**Success criteria**: Three agents launched concurrently, each with the full diff.

Use the `delegate` tool to launch all three agents concurrently in a single message. Pass each agent the full diff so it has the complete context.

### Agent 1: Code Reuse Review

For each change:

1. **Search for existing utilities and helpers** that could replace newly written code. Look for similar patterns elsewhere in the codebase — common locations are utility directories, shared modules, and files adjacent to the changed ones.
2. **Flag any new function that duplicates existing functionality.** Suggest the existing function to use instead.
3. **Flag any inline logic that could use an existing utility** — hand-rolled string manipulation, manual path handling, custom environment checks, ad-hoc type guards, and similar patterns.

### Agent 2: Code Quality Review

Review the same changes for hacky patterns:

1. **Redundant state**: state that duplicates existing state, cached values that could be derived
2. **Parameter sprawl**: adding new parameters instead of generalizing or restructuring
3. **Copy-paste with slight variation**: near-duplicate code blocks that should be unified
4. **Leaky abstractions**: exposing internal details that should be encapsulated
5. **Stringly-typed code**: using raw strings where constants, enums, or branded types already exist
6. **Unnecessary comments**: comments explaining WHAT the code does (well-named identifiers already do that) — keep only non-obvious WHY (hidden constraints, subtle invariants, workarounds)

### Agent 3: Efficiency Review

Review the same changes for efficiency:

1. **Unnecessary work**: redundant computations, repeated file reads, duplicate API calls, N+1 patterns
2. **Missed concurrency**: independent operations run sequentially when they could run in parallel
3. **Hot-path bloat**: new blocking work added to startup or per-request hot paths
4. **Recurring no-op updates**: state updates that fire unconditionally — add a change-detection guard
5. **Unnecessary existence checks**: pre-checking before operating (TOCTOU) — operate directly and handle the error
6. **Memory**: unbounded data structures, missing cleanup, event listener leaks
7. **Overly broad operations**: reading entire files when only a portion is needed

## Phase 3: Fix Issues

**Success criteria**: All valid findings addressed; false positives noted and skipped.

Wait for all three agents to complete. Aggregate their findings. For each finding:
- If valid and worth fixing: fix it directly
- If a false positive or not worth addressing: note it and move on

Prioritize fixes in large batches rather than one-at-a-time to stay within context limits. For conflicts between agents, use this priority: (1) correctness, (2) performance, (3) reusability.

When done, briefly summarize what was fixed (or confirm the code was already clean).
"#,
    )
}

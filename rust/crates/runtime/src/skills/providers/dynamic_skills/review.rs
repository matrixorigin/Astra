pub fn skill_content() -> String {
    format!(
        r#"---
name: review
description: "Review changed code for reuse, quality, and efficiency via three parallel review agents — then fix issues found"
version: "1.0.0"
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
**Project type**: ${{CTX_PROJECT_TYPE}}

## Phase 1: Identify Changes

Run `git diff` (or `git diff HEAD` for staged changes) to get the full diff. If no git changes, review the most recently modified files the user mentioned or you edited earlier.

## Phase 2: Launch Three Review Agents in Parallel

Use `delegate` to launch all three concurrently in a single message. Pass each the full diff.

### Agent 1: Code Reuse
- Search for existing utilities/helpers that could replace newly written code
- Flag new functions that duplicate existing functionality
- Flag inline logic that could use an existing utility (hand-rolled string manipulation, manual path handling, ad-hoc type guards)

### Agent 2: Code Quality
- Redundant state, parameter sprawl, copy-paste with slight variation
- Leaky abstractions, stringly-typed code where enums/constants exist
- Unnecessary comments explaining WHAT (keep only non-obvious WHY)

### Agent 3: Efficiency
- Redundant computations, repeated file reads, N+1 patterns
- Independent operations run sequentially (missed concurrency)
- Unbounded data structures, missing cleanup
- Overly broad operations (reading entire files when only a portion is needed)

## Phase 3: Fix Issues

Wait for all three agents. For each finding:
- Valid and worth fixing → fix it directly
- False positive → note and skip

Priority for conflicts: (1) correctness, (2) performance, (3) reusability.

Summarize what was fixed or confirm the code was already clean.
"#,
    )
}

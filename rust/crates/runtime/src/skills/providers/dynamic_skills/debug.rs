pub fn skill_content() -> String {
    format!(
        r#"---
name: debug
description: "Diagnose and fix bugs, test failures, and unexpected behavior systematically"
version: "2.0.0"
triggers:
  - debug
  - diagnose
  - troubleshoot
  - "why is"
  - "not working"
  - broken
  - error
when_to_use: "When the user reports a bug, test failure, error, or unexpected behavior that needs systematic diagnosis"
category: diagnostics
arguments:
  - name: ISSUE
    description: "Description of the issue to debug"
    required: false
tags:
  - debugging
---
# Debug: Systematic Diagnosis

Help the user diagnose and fix a bug, test failure, or unexpected behavior.

## Goal
Restore correct behavior with minimal changes. Do not refactor unrelated code.

## Step 1: Reproduce

**Success criteria**: You can trigger the bug on demand.

- Get the exact error message, stack trace, or unexpected output
- Find or write a minimal reproduction (test case, command, input)
- For compiled languages (Rust, C++, Go): reproduce in release mode first — debug builds have different optimization behavior and can mask real issues
- If the user can't reproduce: check recent changes (`git log --oneline -20`), environment differences, and intermittent triggers (race conditions, timing)

## Step 2: Gather Context

**Success criteria**: You understand the code path from input to failure.

- Read the failing code and its immediate callers
- Check recent changes to the affected area: `git log --oneline -10 -- <file>`
- Look for related test files that might show expected behavior
- Check environment: config files, env vars, feature flags that affect the code path

## Step 3: Hypothesize and Investigate

**Success criteria**: You have a specific hypothesis about the root cause.

Form hypotheses in order of likelihood:

1. **Recent change broke it** — diff the last few commits touching this area
2. **Wrong assumption** — input validation, type confusion, off-by-one
3. **State corruption** — stale cache, race condition, leaked resource
4. **Environment issue** — wrong dependency version, missing config, permissions
5. **Edge case** — empty input, unicode, large values, concurrent access

For each hypothesis: find evidence that confirms or refutes it before moving on.

## Step 4: Fix

**Success criteria**: The reproduction from Step 1 now passes.

- Make the smallest change that fixes the root cause
- If the fix is non-obvious, add a comment explaining WHY
- Do NOT fix unrelated issues you happen to notice

## Step 5: Verify

**Success criteria**: All existing tests pass AND the new case is covered.

- Run the reproduction from Step 1 — it must pass
- Run the full test suite for the affected module
- If no test existed for this bug, add one
- Check for similar patterns elsewhere that might have the same bug

## When to Stop

If the first two hypotheses don't pan out, switch to a fundamentally different diagnostic approach:
- **Binary search**: `git bisect` to find the breaking commit
- **Minimal repro**: strip away code until only the bug remains
- **Read the source**: of dependencies, not just your code
- **Rubber duck**: explain the problem step by step from scratch

If still stuck after exhausting alternatives, recommend the user use the `/stuck` skill.
"#,
    )
}

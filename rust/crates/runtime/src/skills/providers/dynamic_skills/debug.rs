pub fn skill_content() -> String {
    format!(
        r#"---
name: debug
description: "Diagnose and fix bugs, test failures, and unexpected behavior systematically"
version: "1.0.0"
allowed_tools:
  - bash
  - read_file
  - write_file
  - delegate
triggers:
  - debug
  - diagnose
  - troubleshoot
  - "why does this fail"
  - broken
when_to_use: "When the user reports a specific bug, test failure, or unexpected behavior that needs systematic diagnosis — not for general questions about error handling"
category: diagnostics
arguments:
  - name: ISSUE
    description: "Description of the issue to debug"
    required: false
tags:
  - debugging
composition:
  composable: true
  idempotent: false
  side_effects:
    - filesystem
  max_duration_sec: 900
---
# Debug: Systematic Diagnosis

Help the user diagnose and fix a bug, test failure, or unexpected behavior.

**Working directory**: ${{CTX_WORK_DIR}}
**Project type**: ${{CTX_PROJECT_TYPE}}
**Git branch**: ${{CTX_GIT_BRANCH}}

## Goal
Restore correct behavior with minimal changes. Do not refactor unrelated code.

## Step 1: Reproduce

If the error message or stack trace is already in the conversation, use it directly — don't re-run just to reproduce.

Otherwise:
- Get the exact error message, stack trace, or unexpected output
- Find or write a minimal reproduction (test case, command, input)
- If the user can't reproduce: check recent changes (`git log --oneline -20`), environment differences, and intermittent triggers (race conditions, timing)

## Step 2: Gather Context

- Read the failing code and its immediate callers
- Check recent changes to the affected area: `git log --oneline -10 -- <file>`
- Look for related test files that show expected behavior

## Step 3: Hypothesize and Investigate

Form hypotheses in order of likelihood:

1. **Recent change broke it** — diff the last few commits touching this area
2. **Wrong assumption** — input validation, type confusion, off-by-one
3. **State corruption** — stale cache, race condition, leaked resource
4. **Environment issue** — wrong dependency version, missing config, permissions
5. **Edge case** — empty input, unicode, large values, concurrent access

For each: find evidence that confirms or refutes it before moving on.

## Step 4: Fix

- Make the smallest change that fixes the root cause
- If the fix is non-obvious, add a comment explaining WHY
- Do NOT fix unrelated issues you happen to notice

## Step 5: Verify

- Run the reproduction from Step 1 — it must pass
- Run the full test suite for the affected module
- If no test existed for this bug, add one

## When to Stop

If the first two hypotheses don't pan out:
- `git bisect` to find the breaking commit
- Strip away code until only the bug remains
- Read the dependency's source, not just your code

If still stuck, invoke the `stuck` skill.
"#,
    )
}

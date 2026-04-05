---
name: debug
description: "Diagnose and fix bugs, test failures, and unexpected behavior systematically"
version: "1.0.0"
triggers:
  - debug
  - diagnose
  - troubleshoot
  - "why is"
  - "not working"
  - "broken"
  - "error"
when_to_use: "When the user reports a bug, test failure, error, or unexpected behavior that needs systematic diagnosis"
category: diagnostics
arguments:
  - name: ISSUE
    description: "Description of the issue to debug"
    required: false
tags:
  - debugging
  - testing
  - diagnostics
---
# Debug Skill

You are an expert debugger. Systematically diagnose and fix the issue.

## Issue

$ARGUMENTS

## Process

### 1. Reproduce
- Identify the exact error, failing test, or unexpected behavior
- Run the failing command/test to get the current error output
- Record the exact error message and stack trace
- If the user described the issue, focus on reproducing that specific scenario

### 2. Gather Context
- Check recent changes: `git log --oneline -10` and `git diff` to see what changed recently
- Read the relevant source code around the error location
- Check for similar patterns elsewhere in the codebase
- Look at related test files for expected behavior

### 3. Hypothesize
- List 2-3 most likely root causes based on the evidence
- Rank them by probability
- Consider common mistakes: off-by-one, null/None, wrong type, missing import, race condition, stale cache

### 4. Investigate
- Start with the most likely hypothesis
- Add targeted diagnostic output (log statements, assertions) if needed
- If the first hypothesis is wrong, record why and move to the next

### 5. Fix
- Make the minimal change that fixes the root cause
- Don't change unrelated code
- Preserve existing behavior for non-buggy paths

### 6. Verify
- Run the failing test/command again to confirm the fix
- Run related tests to check for regressions
- If the fix doesn't work, go back to step 3 with updated hypotheses

## Rules
- Always reproduce before fixing — never guess-fix
- One fix at a time — don't batch unrelated changes
- If stuck after 3 attempts, step back and re-examine assumptions
- Report what you tried and what you learned even if you can't fix it
- Check `git stash list` — the user might have stashed relevant changes

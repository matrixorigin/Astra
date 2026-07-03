---
name: review-code
description: "Test-quality review skill for unhappy paths, real assertions, DB/E2E coverage, lifecycle edges, and production-grade verification gaps."
user_invocable: true
when_to_use: "When the user asks for test quality, missing tests, unhappy path coverage, E2E coverage, DB assertions, or whether tests prove the change."
arguments:
  - name: TARGET
    description: "staged, unstaged, branch:<name>, commit:<sha>, file paths, or omitted for uncommitted changes."
    required: false
  - name: FOCUS
    description: "tests, unhappy, e2e, db, lifecycle, or all. Default: all."
    required: false
allowed_tools:
  - bash
  - read_file
  - grep
  - glob
  - git
---

# Review Code: Test Quality

This is not a general code review. Answer one question: do the tests prove the
changed behavior, especially when things go wrong?

## Task

$ARGUMENTS

## Step 1: Resolve Changed Behavior

```bash
git status --short
git diff --stat
git diff --name-only
```

For each changed behavior, identify:

- public entrypoint or caller;
- state/persistence side effect;
- error/cancellation path;
- existing test file, if any.

## Step 2: Look For Required Test Evidence

| Change signal | Required evidence |
| --- | --- |
| Public function/API/CLI command | Happy path plus at least one invalid input/error path |
| DB write or projection | Test asserts persisted state, not just `Ok(())` |
| Restore/sync/checkpoint | Test covers missing, stale, duplicate, or partial data |
| Auth/permission/capability | Denied case and allowed case |
| State machine/lifecycle | Out-of-order, double-submit, retry, cancellation, terminal-state behavior |
| Async task/channel/lock | Cancellation/timeout/cleanup or bounded queue behavior |
| Prompt/tool/skill selection | Test proves the selection rule, not just string presence |

Use `rg` to find tests before reading them:

```bash
rg -n "<function|type|route|event_name>" rust/crates tests web packages --glob '!target/**'
```

## Step 3: Classify Gaps

Missing test:

- A reachable changed behavior has no test at the owning layer.
- A persistence mutation has no state assertion.
- An unhappy path is the main risk and only happy paths are tested.

Weak test:

- Only checks `is_ok()` / `is_err()` with no effect assertion.
- Mocks away the behavior being changed.
- Tests a helper while the bug lives at the integration boundary.
- Snapshot/string test is brittle and does not assert the contract.

Covered:

- Test reaches the public path, asserts the relevant state/output, and includes at least one failure/lifecycle edge when that edge is material.

## Output Contract

```text
Missing Tests:
- <file:line> <behavior not proven, consequence>

Weak Tests:
- <test file:line> <why it can pass while behavior is broken>

Covered:
- <behavior> proven by <test>

Suggested Checks:
- <exact command(s), usually cd rust && cargo test -p <crate> <filter>>
```

If coverage is adequate, say so and name the tests that prove it.

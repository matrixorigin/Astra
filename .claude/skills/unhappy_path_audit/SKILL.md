---
name: unhappy-path-audit
description: "Reachability-first unhappy-path audit for changed Astra code paths: dead paths, error propagation, state consistency, resource leaks, hung waits, and unbounded accumulation."
user_invocable: true
when_to_use: "When the user explicitly asks to audit failure behavior: unhappy path, failure path, error handling audit, reachability audit, dead code audit, resource leak audit, hung/OOM audit, or resource safety audit."
arguments:
  - name: TARGET
    description: "Module, crate, file, branch range, or default uncommitted diff."
    required: false
  - name: FOCUS
    description: "reachability, classification, error-propagation, state, resource, or all. Default: all."
    required: false
allowed_tools:
  - bash
  - read_file
  - grep
  - glob
  - git
---

# Unhappy Path Audit

Audit failure behavior in order. Reachability comes before severity.

## Task

$ARGUMENTS

## First Principles

| Gate | Question | Common failure |
| --- | --- | --- |
| R0: changed path | What code path changed? | Auditing stale or unrelated code |
| R1: reachability | Can the path execute under current config/capabilities/modes? | Filing dead code as a bug |
| R2: correctness | When reached, does error/state/resource handling do the right thing? | Misclassification, dropped error, leak, hang |
| R3: consequence | What concrete behavior breaks? | Wrong severity |

Resource safety is a subset of R2:

| Check | Proposition |
| --- | --- |
| Q1 | Every created resource has a guaranteed cleanup path |
| Q2 | Every blocking wait has a guaranteed release/cancel/timeout path |
| Q3 | Every accumulation has a bound, backpressure, or recycling point |

Only apply Q1-Q3 to resources touched by the change.

## Step 1: Establish R0

```bash
git status --short
git diff --stat
git diff --name-only
```

For branch or commit targets, use the requested range and inspect the full diff.

Group changed paths by owner:

- Runtime/server/delegation/run lifecycle.
- Services/storage/journal/tasks/restore/sync.
- Turn/tool/prompt/skill selection.
- CLI/TUI/SDK/frontend.
- Tests and docs.

## Step 2: Prove R1 Reachability

For every candidate finding, trace:

```text
entrypoint -> caller -> changed branch -> config/capability/feature/mode gate -> execution path
```

Astra gates to check:

- capability surface and tool visibility;
- runtime config/env defaults;
- feature flags and mode switches;
- session/run/task status;
- delegation pause/cancel/retry state;
- cloud/local/offline DB availability;
- skill source visibility (`.claude`, `.agent`, server HOME, `skills_registry`).

If no current entrypoint reaches the code, report it as a dead-path note, not a bug.

## Step 3: Audit R2 Correctness

Classification:

- Are error categories preserved across boundaries?
- Are retryable, user-fixable, and fatal errors distinguished?
- Does a fallback hide the original cause?

Propagation:

- Does the caller see enough context to recover or report accurately?
- Are journal events, task status, and HTTP/CLI status consistent?

State:

- Are transitions valid from every previous state?
- Are duplicate, stale, out-of-order, and cancellation cases handled?
- Is persistent state written atomically enough for restore/sync?
- Can any consumer derive terminal state from a child/transport event, lookup
  miss, timeout, or cached projection instead of the producer-owned lifecycle?
- For grouped work, can one slot transition trigger parent analysis before the
  fixed-size group settles, or can several slot transitions schedule duplicate
  wakes?

Resource:

- Are tasks joined/cancelled?
- Are channels bounded or drained?
- Are locks held across await points?
- Is accumulation bounded by size, count, time, or compaction?

## Step 4: Assign R3 Severity

Use severity only after R1 and R2 are proven.

| Severity | Bar |
| --- | --- |
| Critical | Reachable in default/common config and causes incorrect behavior, data loss, security issue, hang, or unrecoverable task/session state |
| Important | Reachable under a real configuration and causes user-visible failure or missing recovery evidence |
| Low / note | Reachable but minor, or unreachable design debt worth tracking |

Group causally related symptoms into one finding. Do not split one root cause into multiple findings.

## Output Contract

```text
Critical:
- <file:line> <reachable path> -> <failure> -> <consequence>

Important:
- <file:line> ...

Low / Notes:
- <dead path or low-impact issue>

Verified OK:
- <risky path checked and why it is safe>

Unknowns:
- <missing evidence, if any>
```

Every finding must include the reachability chain or it does not belong in the report.

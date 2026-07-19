pub fn skill_content() -> String {
    r#"---
name: review
description: "Evidence-driven review of changed code for correctness, unhappy paths, reuse, quality, and efficiency"
version: "1.0.0"
allowed_tools:
  - bash
  - read_file
  - write_file
when_to_use: "User wants recently changed code reviewed for reuse, quality, or efficiency — typically before commit or after a feature lands"
category: code-review
tags:
  - review
  - quality
composition:
  composable: true
  idempotent: false
  side_effects:
    - filesystem
  max_duration_sec: 1200
---
# Review: Code Review and Cleanup

Review the changed behavior, prove material findings, and fix confirmed issues when the user authorized changes.

**Working directory**: ${{CTX_WORK_DIR}}
**Git branch**: ${{CTX_GIT_BRANCH}}
**Project type**: ${{CTX_PROJECT_TYPE}}

## Phase 1: Identify Changes

Establish the requested base/head and inspect the full diff. Read each changed file with enough adjacent production and test context to understand ownership, lifecycle, and callers. If there is no working-tree diff, use the branch or commit range named by the user; do not guess from modification times.

## Phase 2: Review Independent Failure Angles

Review all of these angles. Parallel agents are optional orchestration owned by the calling surface, not a requirement of this skill.

### Correctness and unhappy paths
- Trace each new state transition and async boundary through success, cancellation, timeout, disconnect, retry, partial failure, and recovery.
- Identify the canonical producer and consumer for every lifecycle fact. Flag dual truth, silent fallback, lost wakeups, leaked work, and terminal state that can be revived.
- Inspect the actual tests before making any claim about coverage.

### Reuse and design quality
- Search for existing owners, policies, enums, and helpers before proposing a new mechanism.
- Flag duplicate state, string-inferred protocol, leaky abstractions, parameter sprawl, and copy-paste variants.
- Prefer removing obsolete paths over preserving multiple partially overlapping systems.

### Efficiency and operability
- Redundant computations, repeated file reads, N+1 patterns
- Independent operations run sequentially (missed concurrency)
- Unbounded data structures, missing cleanup
- Overly broad operations (reading entire files when only a portion is needed)
- Missing diagnostics or repair signals on degraded paths

## Phase 3: Report and Fix

For every material finding, include severity, the concrete failure sequence, and a file/line reference. Distinguish verified defects from hypotheses and state what evidence would settle a hypothesis.

For each finding:
- Valid and worth fixing → fix it directly when changes are in scope
- False positive or already protected → cite the protecting code/test and skip it

Priority for conflicts: (1) correctness, (2) performance, (3) reusability.

Run focused tests that exercise the claimed causal boundary, including at least one relevant unhappy path. A compile-only pass or a source-text assertion is not evidence that runtime behavior works. Summarize confirmed fixes, verification, and any residual risk; only report the review clean when the inspected evidence supports it.
"#.to_string()
}

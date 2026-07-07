---
name: verify-task
description: "Verify completed Astra work with evidence. Uses durable task verification criteria when present; otherwise derives targeted gates from the diff and changed behavior."
user_invocable: true
when_to_use: "When the user wants to verify that a completed task actually works, run focused tests/lint/build checks, or produce a delivery report."
arguments:
  - name: TASK
    description: "last, task ID, or natural language description of what should work."
    required: false
  - name: SCOPE
    description: "quick, full, or custom. Default: full."
    required: false
allowed_tools:
  - bash
  - read_file
  - grep
  - glob
---

# Verify Task

Verification is evidence, not optimism. Use the strongest existing contract; if no
contract exists, derive checks from the diff and changed behavior.

## Task

$ARGUMENTS

## Phase 1: Determine Verification Source

1. Check current work:

```bash
git status --short
git diff --stat
git diff --name-only
```

2. If the task is durable/contract-backed, use the service types:

| Concept | Source |
| --- | --- |
| Criteria schema | `crates/services/src/verification.rs` |
| Durable task lifecycle | `crates/services/src/durable_task.rs` |
| CLI display/report bridge | `crates/astra-cli/src/cli/durable_bridge.rs` |
| Verification journal event | `crates/services/src/session_journal.rs` |

Current verifier kinds are `command`, `command_output`, `file_exists`, `grep_check`,
`build_pass`, `test_pass`, `read_file_contains`, `llm_judge`, and `composite`.

3. If there is no contract, create acceptance criteria from changed behavior:

- What user/system behavior changed?
- What state or output proves it?
- What failure path matters?
- Which crate/module owns it?

## Phase 2: Select Gates

Run only gates that can be affected by the change.

| Change | Required gate |
| --- | --- |
| Skill/docs only | Frontmatter parse, metadata JSON parse, stale path scan, `.claude`/`.agent` sync; no Rust build |
| Rust formatting only | `cargo fmt --check` |
| Single Rust crate | `cargo check -p <crate>` plus focused tests |
| Shared Rust API | `cargo check --workspace --all-targets` plus affected tests |
| Runtime/server lifecycle | Focused runtime tests, then `cargo check -p astra-runtime` |
| Turn/tool/prompt behavior | Focused turn/runtime tests plus prompt/tool surface assertions |
| Services/storage/MatrixOne | Focused services tests; online DB check only when configured and relevant |
| CLI/TUI | Focused `astra-cli` tests or command dry run |
| Frontend/SDK | Relevant package script or existing make target |
| Shell/deployment | Exact owning make/script dry run |

Do not run root-level raw cargo commands. The Rust workspace is under `rust/`.

## Phase 3: Execute And Interpret

For each criterion record:

- command or inspection performed;
- pass/fail/inconclusive;
- evidence line or output summary;
- skipped reason if not run.

If a command fails, stop broadening and diagnose the failed gate first. A later broad
test cannot make an earlier required failure irrelevant.

## Phase 4: Delivery Report

Use this shape:

```text
Verdict: verified | failed | inconclusive | verified with warnings

Criteria:
- PASS <criterion> - <evidence>
- FAIL <criterion> - <evidence and likely owner>
- SKIP <criterion> - <reason>

Commands:
- <command> -> <result>

Residual Risk:
- <only real gaps, such as online DB not available>
```

Verdict rules:

| Evidence | Verdict |
| --- | --- |
| All required criteria pass | verified |
| Required pass, optional/skipped checks have justified residual risk | verified with warnings |
| Any required criterion fails | failed |
| Verification could not run enough evidence to judge | inconclusive |

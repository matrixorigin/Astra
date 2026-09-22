---
name: verify-task
description: "Verify completed Astra work with evidence. Uses canonical Work criteria and checks when present; otherwise derives targeted gates from the diff and changed behavior."
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

2. If the change is backed by durable Work, use the canonical service types:

| Concept | Source |
| --- | --- |
| Generic command verifier schema | `crates/services/src/verification.rs` |
| Durable Work domain and lifecycle | `crates/services/src/work.rs` |
| Work checks and acceptance | `crates/services/src/work/acceptance.rs`, `crates/services/src/work/repository.rs` |
| CLI Work projection | `crates/astra-cli/src/cli/work_command.rs` |
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
| Services/storage/MatrixOne | Focused tests plus real DB/public-caller verification for changed schema/query/transaction/bootstrap behavior; record unavailable required DB evidence as missing and apply the verdict rules below |
| CLI/TUI | Focused `astra-cli` tests or command dry run |
| Frontend/SDK | Relevant package script or existing make target |
| Shell/deployment | Exact owning make/script dry run |

Run raw cargo commands from the repository root; `Cargo.toml` is the workspace manifest.

Keep checks bounded and relevant; skill/docs changes do not need database tests
or a Rust build. Reuse valid evidence for unchanged code and isolate failed cases
instead of repeatedly launching long suites. Do not raise timeouts, lower load,
weaken assertions, or rerun until green as a substitute for a fix.

## Phase 3: Execute And Interpret

For each criterion record:

- command or inspection performed;
- pass/fail/inconclusive;
- evidence line or output summary;
- skipped reason if not run.

If a command fails, stop broadening and diagnose the failed gate first. A later broad
test cannot make an earlier required failure irrelevant.

For optimization claims, compare the same workload and state assumptions before
and after. Record actual concurrency, new/existing sessions, cache state, I/O and
transaction counts, throughput, latency distribution, failures, and resource use
where relevant. Separate setup from steady-state time without hiding setup failures.
Report untested scale as unknown, not extrapolated capacity.

For model-cost/observation changes, reconcile primary, child, auxiliary, and retry
calls against the canonical ledger; distinguish estimates, missing usage, and
provider-reported totals. Check cache percentage denominator/coverage and avoid
double-counting cache/reasoning subsets or overlapping time spans. Cost estimates
must identify pricing source/date, cache pricing, currency and any exchange-rate
assumption. A cheaper judgment does not prove lower total task cost or equal quality.

Before an authorized commit, check independent review evidence for the current
diff using the model rules in [review-changes](../review_changes/SKILL.md).
Do not substitute self-review or stale approval. Report CI as pending/failed/passed;
do not wait for CI unless asked, and never report pending CI as a completed gate.

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

Change and benefit:
- <behavior, removed/replaced implementations/tests, measured gains and tradeoffs>
- <reproduction command or manual verification steps; scope and evidence limits>
```

Apply the first matching verdict rule below. Missing evidence never overrides a
known required failure.

| Evidence | Verdict |
| --- | --- |
| Any required criterion fails | failed |
| No required criterion fails, but a required check lacks evidence, including DB or independent review when required | inconclusive |
| All required criteria pass, only optional checks are skipped with justified residual risk | verified with warnings |
| All required criteria pass | verified |

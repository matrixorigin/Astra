---
name: astra-dev
description: "Programmatic Astra engineering workflow for non-trivial code changes. Use when touching Rust crates under rust/, runtime/server lifecycle, turn/tool/prompt behavior, skills, delegation, session journal, MatrixOne state, CLI, deployment scripts, or when debugging Astra tests."
user_invocable: true
when_to_use: "When developing, debugging, or refactoring Astra code; especially Rust workspace changes, runtime/server lifecycle work, tool or prompt behavior, skill discovery, session journal/debugging, cloud sync, delegation, CLI behavior, deployment scripts, or test failures."
arguments:
  - name: TASK
    description: "Bug, feature, refactor, or diagnostic task to perform."
    required: false
  - name: FILES
    description: "Optional comma-separated focus files or crates."
    required: false
allowed_tools:
  - bash
  - read_file
  - grep
  - glob
  - git
---

# Astra Dev

Use this as an execution workflow, not as a project encyclopedia. The goal is to
identify the owning crate, preserve the intent of existing edits, change the
smallest correct surface, and verify with the narrowest gate that proves the behavior.

## Task

$ARGUMENTS

## Phase 1: Understand Task (finish in about 3 minutes)

1. Run `git status --short`.
2. If focus files are known, run `git log --oneline -10 -- <FILES>` and inspect the current diff for those files.
3. Read only the owning module and its immediate caller/callee. Use the Ownership Map below.
4. State the task location in one line:

```text
Task: <behavior to change>
Owner: <crate/module>
Boundary: <caller -> owner -> persistence/tool/prompt/test surface>
Prior intent: <what the current uncommitted diff appears to be preserving>
```

Rules:

- Treat uncommitted changes as intentional until disproven.
- If you cannot explain the intent of a previous edit, inspect more before replacing it.
- Do not read every design document up front; load context only when it changes a decision.

## Phase 2: Plan Only When It Pays

Create an explicit plan before editing when the task touches more than 3 files, more than 1 crate, runtime plus services, persistence schema, prompt/tool visibility, delegation, or any user-visible workflow.

If the host supports plan mode, enter it for those cases. Otherwise write a short checklist and update it as work completes.

Skip a formal plan for narrow one-file fixes once Phase 1 has identified the owner.

## Phase 3: Execute

1. Read the target region immediately before editing.
2. Edit the owning module first. Change callers only when the contract changes.
3. For Rust code, after each logical edit batch run a narrow compile gate:

```bash
cd rust && cargo check -p <crate>
```

4. Add or update tests at the same layer as the behavior:

- Pure helper behavior: unit test in the owning crate.
- Runtime/turn behavior: focused runtime or turn-core test.
- Persistence behavior: service/storage test plus DB state assertion where applicable.
- CLI behavior: CLI parser/rendering test or focused command test.
- Skill/docs change: frontmatter/path/stale-reference validation, not a Rust build.

5. If compatibility is explicitly out of scope, delete the obsolete path instead of layering a second model.

## Phase 4: Failure Triage

Use the first matching row before widening the search.

| Failure | First check |
| --- | --- |
| Compile error | Visibility/import/type contract drift |
| Test assertion | Whether the test still describes the intended model |
| New behavior never runs | Capability, config, feature, or mode gate |
| Flake | Shared env/global state, time, async task leakage, DB IDs |
| Hang/leak | Spawned task ownership, cancellation token, channel capacity, cleanup path |
| Bad persisted state | Journal event, DB projection, status transition, request_id/session_id chain |
| Prompt/tool regression | Tool surface tier, capability source, selected skill list, budget pressure |

## Phase 5: Verification Gates

Run from repo root for `make` targets and from `rust/` for raw cargo.

| Change | Gate |
| --- | --- |
| Rust formatting only | `cd rust && cargo fmt --check` |
| Single Rust crate | `cd rust && cargo check -p <crate>` plus focused tests |
| Shared Rust API | `cd rust && cargo check --workspace --all-targets` plus affected crate tests |
| Runtime/server lifecycle | Focused runtime tests, then `cd rust && cargo check -p astra-runtime` |
| Turn behavior | `cd rust && cargo check -p astra-turn-core` plus focused turn tests |
| Services/storage/MatrixOne | Focused service/storage tests; note if online DB checks were skipped |
| Skills/docs only | Validate frontmatter, metadata JSON, path references, and `.claude`/`.agent` sync |
| Shell/deployment | Run the exact make/script/config dry-run that owns the behavior |
| Frontend/SDK | Use the relevant `package.json` script or existing make target |

Useful focused runtime tests:

| Area | Test target |
| --- | --- |
| Run lifecycle | `cd rust && cargo test -p astra-runtime server::run::lifecycle::tests -- --nocapture` |
| Server loop host | `cd rust && cargo test -p astra-runtime server::server_loop_host::tests -- --nocapture` |
| Capabilities | `cd rust && cargo test -p astra-runtime capabilities::tests -- --nocapture` |
| Delegation | `cd rust && cargo test -p astra-runtime server::delegation -- --nocapture` |
| Skills | `cd rust && cargo test -p astra-skills` |

## Delivery Report

End with:

- Behavior changed, not just files changed.
- Why the final model is simpler or more correct.
- Verification commands and outcomes.
- Skipped checks and residual risk, if any.

## Appendix: Ownership Map

| Area | Owner / anchor |
| --- | --- |
| HTTP server, run lifecycle, delegation engine | `astra-runtime` |
| Main server loop and tool host behavior | `rust/crates/runtime/src/server/server_loop_host.rs` |
| Run lifecycle startup/admission/projection | `rust/crates/runtime/src/server/run/lifecycle/` |
| Delegation sub-runs, retries, pause/resume | `rust/crates/runtime/src/server/delegation/` |
| Capability-gated tool visibility | `rust/crates/runtime/src/capabilities.rs`, `astra-turn-core::tool_surface` |
| Built-in tools and schemas | `rust/crates/astra-tools/` |
| Turn execution/finalization | `rust/crates/astra-turn-core/`, `rust/crates/astra-turn-types/` |
| Prompt blocks and context pressure | `rust/crates/astra-prompts/`, `rust/crates/astra-pipeline/`, `rust/crates/runtime/src/prompts/` |
| Journals, durable tasks, sync, coordination types | `rust/crates/services/` |
| Skill parsing/discovery/invocation | `rust/crates/astra-skills/`, `.claude/skills/`, `.agent/skills/` |
| Thin HTTP/SSE client | `rust/crates/astra-thin-client/` |
| Declarative harnesses | `rust/crates/astra-test-harness/`, `rust/crates/astra-harness/` |
| Frontend/admin surfaces | `web/`, `packages/sdk/` |

## Appendix: Astra Invariants

| Domain | Rule |
| --- | --- |
| Cargo | Workspace lives under `rust/`; raw cargo commands run there or use `--manifest-path rust/Cargo.toml`. |
| Errors | Library code uses `thiserror`; include operation, entity ID, and source context. Log at HTTP/CLI boundaries. |
| Persistent state | State transitions are explicit status fields and traceable through session/run/event IDs. |
| MatrixOne | Use `astra_core::resolve_database_name`; avoid hardcoded DB names and JSON-column WHERE filters. |
| Tools | Visibility is surface plus capability requirements. Do not add parallel allowlists when catalog metadata can express the rule. |
| Delegation | Parent/child runs preserve request constraints, cancellation, pause flags, mailbox routing, and journal/progress evidence. |
| Tests | Tests run in parallel. Own IDs/env, reset global state, and never rely on order. |
| Skills | A skill must add non-obvious workflow. Delete command wrappers and stale README-style docs. |

## Appendix: Sibling Skills

| Need | Skill |
| --- | --- |
| Review an uncommitted/branch/commit diff | `review_changes` |
| Audit failure paths, reachability, resource leaks, hung waits | `unhappy_path_audit` |
| Verify completion evidence after implementation | `verify_task` |
| Diagnose session stalls, tool failures, token usage | `analyze_session` |
| Reduce prompt/tool/skill context bloat | `optimize_prompt` |
| Audit edge/cloud sync integrity | `audit_cloud_sync` |
| Trace delegated agents/sub-runs | `trace_delegation` |

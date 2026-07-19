---
name: astra-dev
description: "Programmatic Astra engineering workflow for non-trivial code changes. Use when touching Rust crates under crates/, runtime/server lifecycle, turn/tool/prompt behavior, skills, delegation, session journal, MatrixOne state, CLI, deployment scripts, or when debugging Astra tests."
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

Use this as an engineering state machine, not as a project encyclopedia.
The loop is:

```text
Explore -> Integrate -> Execute -> Verify
             ^            |
             |            v
        Re-open only on explicit invalidation
```

The goal is to identify the owning crate, preserve the intent of existing edits,
convert known facts into an executable checklist, change the smallest correct
surface, and verify with the narrowest gate that proves the behavior.

## Hard Rule: Structured Control Flow

Do not add production control flow based on natural-language text matching.
Safety, admission, routing, blocking, retry, recovery, evaluation, and state
transitions must use typed facts: enums, `ErrorKind`, `result_class`,
`exit_semantics`, structured tool-result JSON, protocol parsers, AST/token
parsers, or exact machine-owned sentinel fields.

Allowed text matching is rare: UI display/search, tests of rendered text, or
legacy protocol fallback named with a `fallback` suffix. A fallback must not be
the primary safety/admission/blocking/evaluation decision.

## Hard Rule: One Owner, One Wired Path

Before adding a type, service, registry, state machine, observer, table, parser,
or compatibility layer:

1. Search for the existing owner of the fact or lifecycle and name it in the
   execution record.
2. Trace the real product entrypoint to that owner. Unit tests and constructors
   called only by tests are not wiring evidence.
3. Extend the owner when possible. A new owner is justified only by a distinct
   authority, deployment, security, or resource-lifecycle boundary.
4. If replacing a path, migrate its callers and delete the old implementation,
   old state/table/shim, and tests that only exercised the old island in the
   same change. Do not keep code for unspecified "future extensibility".
5. Report the complexity delta: implementations, status vocabularies, writers,
   tables, compatibility paths, and net code. Added tests do not cancel out a
   second source of truth.

Lifecycle status is owned by the producer that performs the transition. Put
the canonical typed projection on that producer and make CLI, server, UI,
persistence, and wake logic consume it. Consumers must not independently
re-count child states or translate transport events into lifecycle truth. A
fixed-size fanout is one work unit: individual child events are progress, and
only the canonical group terminal transition may authorize parent synthesis.

For persistence, a mock-only test is insufficient. Prove schema bootstrap,
query/transaction semantics, failure behavior, and the public caller against
the real configured database whenever the change crosses that boundary.

## Task

$ARGUMENTS

## Phase 1: Explore Task (finish in about 3 minutes)

1. Run `git status --short`.
2. If focus files are known, inspect both staged and unstaged diffs for them, then run `git log --oneline -10 -- <FILES>`.
3. Read only the owning module and the immediate caller/callee needed to explain the change. Use the Ownership Map below.
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
- Stop exploring when you can name the owner, contract boundary, and intended behavior.

## Phase 2: Integrate Findings

Before editing, convert discoveries into a compact execution record. Do this even
when the task is small; keep it brief for one-file fixes.

```text
Facts:
- <session-stable facts: owner, files, functions, invariants, review findings>

Assumptions:
- <claims that tests or compilation must confirm>

Edit checklist:
- [ ] <file>: <region/function>; <change>; <invariant preserved>

Verification:
- <narrowest command(s) proving the behavior>

Re-open conditions:
- <what would justify returning to exploration>
```

Use an explicit plan before editing when the task touches more than 3 files, more
than 1 crate, runtime plus services, persistence schema, prompt/tool visibility,
delegation, or any user-visible workflow. If the host supports plan mode, enter
it for those cases. Otherwise keep the checklist current.

Information durability:

- Session-stable: user goal, prior edit intent, owning file/module, function names,
  contracts, review findings, and invariants until an invalidation occurs.
- Volatile: line numbers after edits, test output, command output, branch status,
  diff context, runtime state, and anything another process may have changed.
- Do not re-grep a session-stable fact just because it came from a previous review
  or checklist.

Re-open exploration only when one of these happens:

- Target text or local context no longer matches.
- A patch/edit fails.
- Compile or test output contradicts the model.
- The user changes the goal or scope.
- The worktree changes outside your edits.
- A contract boundary is unknown and affects the edit.

## Phase 3: Execute Checklist

1. Work checklist items in dependency order.
2. Immediately before each edit, read the smallest complete local context needed
   to patch safely. Usually this is 20-80 lines around the target, not a whole file.
3. Do not use broad `grep`/`glob` during execution unless it tests a specific
   hypothesis from the checklist or a re-open condition fired.
4. Edit the owning module first. Change callers only when the contract changes.
5. For Rust code, after each logical edit batch run a narrow compile gate:

```bash
cargo check -p <crate>
```

6. Add or update tests at the same layer as the behavior:

- Pure helper behavior: unit test in the owning crate.
- Runtime/turn behavior: focused runtime or turn-core test.
- Persistence behavior: service/storage test plus DB state assertion where applicable.
- CLI behavior: CLI parser/rendering test or focused command test.
- Skill/docs change: frontmatter/path/stale-reference validation, not a Rust build.

7. If compatibility is explicitly out of scope, delete the obsolete path instead of layering a second model.

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

Run `make` targets and raw cargo commands from the repository root.

| Change | Gate |
| --- | --- |
| Rust formatting only | `cargo fmt --check` |
| Single Rust crate | `cargo check -p <crate>` plus focused tests |
| Shared Rust API | `cargo check --workspace --all-targets` plus affected crate tests |
| Runtime/server lifecycle | Focused runtime tests, then `cargo check -p astra-runtime` |
| Turn behavior | `cargo check -p astra-turn-core` plus focused turn tests |
| Services/storage/MatrixOne | Focused service/storage tests; note if online DB checks were skipped |
| Skills/docs only | Validate frontmatter, metadata JSON, path references, and `.claude`/`.agent` sync |
| Shell/deployment | Run the exact make/script/config dry-run that owns the behavior |
| Frontend/SDK | Use the relevant `package.json` script or existing make target |

Useful focused runtime tests:

| Area | Test target |
| --- | --- |
| Run lifecycle | `cargo test -p astra-runtime server::run::lifecycle::tests -- --nocapture` |
| Server loop host | `cargo test -p astra-runtime server::server_loop_host::tests -- --nocapture` |
| Capabilities | `cargo test -p astra-runtime capabilities::tests -- --nocapture` |
| Delegation | `cargo test -p astra-runtime server::delegation -- --nocapture` |
| Skills | `cargo test -p astra-skills` |

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
| Main server loop and tool host behavior | `crates/runtime/src/server/server_loop_host.rs` |
| Run lifecycle startup/admission/projection | `crates/runtime/src/server/run/lifecycle/` |
| Delegation sub-runs, retries, pause/resume | `crates/runtime/src/server/delegation/` |
| Capability-gated tool visibility | `crates/runtime/src/capabilities.rs`, `astra-turn-core::tool_surface` |
| Built-in tools and schemas | `crates/astra-tools/` |
| Turn execution/finalization | `crates/astra-turn-core/`, `crates/astra-turn-types/` |
| Prompt blocks and context pressure | `crates/astra-prompts/`, `crates/astra-pipeline/`, `crates/runtime/src/prompts/` |
| Journals, durable tasks, sync, coordination types | `crates/services/` |
| Skill parsing/discovery/invocation | `crates/astra-skills/`, `.claude/skills/`, `.agent/skills/` |
| Thin HTTP/SSE client | `crates/astra-thin-client/` |
| Declarative harnesses | `crates/astra-test-harness/`, `crates/astra-harness/` |
| Frontend/admin surfaces | `web/`, `packages/sdk/` |

## Appendix: Astra Invariants

| Domain | Rule |
| --- | --- |
| Cargo | Workspace lives at the repository root; raw cargo commands run from there. |
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

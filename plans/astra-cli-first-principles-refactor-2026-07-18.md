# Astra CLI first-principles refactor

Status: active

## Objective

Make `astra-cli` a thin, reliable client/edge host rather than a second runtime whose behavior is encoded in strings and duplicated state. The refactor should reduce compile/test cost, remove unfinished product surfaces, preserve typed evidence across boundaries, and make CLI-only, CLI+Server, and Edge+Server behavior converge on the same contracts.

This work does not preserve obsolete internal APIs or historical error-text protocols. Compatibility is only justified at an actual external versioned boundary.

## Current reality

- `crates/astra-cli/src` is about 324K lines of Rust.
- The largest files mix orchestration, transport, state mutation, rendering, and tests: `tui/event_loop.rs`, `cli/stream/stream_render.rs`, `edge_tools.rs`, `cli/slash/slash_session.rs`, `tui/chat_widget/mod.rs`, and `cli/permission_manager.rs`.
- The crate has roughly 5.1K unit tests. Many are valuable behavior/snapshot tests, but inline test bodies make already-large production modules harder to navigate and make every test target expensive to compile.
- Before this refactor, `lib.rs` and `main.rs` each declared the complete module graph. The same implementation and tests were compiled as both `astra-cli` and `astra-cli::bin/astra`.
- Tool execution still has a stringly core: many handlers return `String`; callers later infer success, failure, sandbox denial, retryability, and work-surface state from prefixes or prose.
- Several “compatibility” paths are not external protocol support. They are local aliases, dual state projections, old restore flows, or tests that keep both paths alive.

## Non-negotiable design rules

1. One semantic fact has one typed source of truth.
2. Human-readable text is presentation, never a control-plane protocol.
3. A hard boundary is reserved for real safety, permission, capability, budget, or durable-state invariants.
4. Runtime evidence stays structured until final LLM/UI rendering.
5. CLI-only, CLI+Server, and Edge+Server may use different transports, but must consume the same state/result contracts.
6. Stable prompt history contains real conversation/tool history; volatile runtime evidence stays in typed dynamic lanes.
7. Tests assert public behavior, payloads, state transitions, persistence, and rendering geometry. Tests do not search source text or pin incidental helper calls.
8. Delete unfinished surfaces instead of advertising a feature that always fails. Reintroduce them only with an end-to-end contract and tests.
9. Split modules after semantic boundaries are clear; do not create crates or traits merely to reduce line counts.
10. Each batch must be independently buildable, behavior-tested, and reversible through Git.

## Findings and priority

### P0: typed execution result boundary

`ToolExecutor::execute_raw` returns plain text. `execute_run` then calls `classify_tool_result_status`, and Git/worktree/database helpers contain additional `starts_with("Error:")` checks. Sandbox producers emit `SANDBOX_DENIED: ...`; adapters parse that prefix back into `error_kind=sandbox_denied`.

Target contract:

```text
Tool handler
  -> ToolExecutionOutcome { status, evidence, output, metadata }
  -> permission/retry/work-surface projection
  -> human/LLM rendering
```

There must be no reverse arrow from rendered prose to status/evidence.

The sandbox result should carry a typed denial containing the denial kind, requested path or command evidence, and an optional concrete approvable scope. Approval code should consume that scope directly. It must not extract quoted paths from an English message.

### P0: typed transport/auth failures

Several paths format `ThinClientError` into a string too early, then search for words such as `timeout`, `invalid token`, or `authentication failed`. This can clear credentials or choose recovery based on unrelated provider/user text.

Transport, HTTP status, Astra authentication, provider authentication, invalid payload, and durable-state failures need distinct variants. Presentation hints should be selected from variants/status codes.

### P0: one executable/module graph

Completed in the first batch. `src/main.rs` is now a thin call into the library entrypoint. External test modules live at the library crate root. The bin target reports zero duplicate unit tests.

### P1: lifecycle and durable-state boundaries

The session start/resume/recovery/commit code contains local fallbacks and old restore paths. Audit each fallback by failure class:

- recoverable transport degradation;
- durable journal/checkpoint recovery;
- obsolete local compatibility;
- silent data loss masked as success.

Only the first two categories should remain, and they must be observable and behavior-tested.

### P1: event and projection ownership

`tui/event_loop.rs`, `stream_render.rs`, and `chat_widget/mod.rs` own overlapping live state and projections. Refactor around typed events and explicit owners:

- runtime/edge event ingestion;
- durable transcript/session projection;
- UI view model;
- rendering and interaction.

The event loop should schedule and route events, not infer domain state from rendered output.

### P1: slash-command product surface

Every registered command needs a user journey, capability precondition, result contract, and visible outcome. Remove dead aliases and commands that only print placeholders. Command handlers should return a typed result (`view`, `mutation`, `deferred work`, `error`) instead of mutating unrelated UI/session fields ad hoc.

### P2: module decomposition and test placement

After P0/P1 contracts stabilize:

- split `edge_tools.rs` by execution contract and provider, not only by tool name;
- split `stream_render.rs` into event reduction, execution coordination, and render projection;
- split `event_loop.rs` into input, scheduling, runtime event reduction, and view navigation;
- move large inline test suites into sibling test modules while keeping private behavior accessible through narrow test fixtures;
- keep snapshot tests for visual contracts, but eliminate snapshots that only mirror implementation text.

## First completed implementation batch

- Replaced the duplicate lib/bin module graph with one library entrypoint and a thin binary.
- Removed an empty reserved TUI layout module and an unused context-panel compatibility alias.
- Removed the advertised but permanently unimplemented `@url` reference. Network retrieval belongs to the capability/tool layer.
- Changed file-history diff reads from silent `.ok()` degradation to visible I/O errors.
- Replaced the hand-written greedy “LCS approximation” with a maintained line edit script and exact behavior tests, including repeated-line movement and corrupt checkpoint/current-state failures.
- Changed model-catalog loading to retain typed HTTP/transport/payload failures. Authentication refresh is now selected from HTTP status, not body prose.

## Verification standard per batch

Minimum:

- `cargo fmt --all`
- `cargo clippy -p astra-cli --all-targets -- -D warnings`
- focused behavior tests for every changed boundary
- confirm `cargo test -p astra-cli --bin astra -- --list` has no duplicated library tests

Before a major phase commit:

- `make check`
- `make test-offline`
- relevant online tests when a server/DB/transport contract changed
- online MatrixOne tests for database behavior; never substitute mocks for the final DB contract

## Next implementation batches

1. Introduce the typed sandbox/tool-failure outcome at producers and delete `SANDBOX_DENIED_PREFIX` parsing and quoted-prose path extraction.
2. Replace remaining Astra auth/error text classification with structured transport and server error types.
3. Remove local legacy recovery/alias paths whose current typed replacement is already authoritative.
4. Extract state reducers from `stream_render.rs` and `event_loop.rs`, retaining UI snapshots plus event-sequence tests.
5. Audit and productize the slash-command registry, deleting commands and aliases without a complete user journey.

Each item should land as a coherent behavior change, not as a broad mechanical rewrite.

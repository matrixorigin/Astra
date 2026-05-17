# Web Agent Dynamic Multi-Agent Support

> Status: Phase 1 implementation contract, implemented in this branch.
> Scope: Enable `agent(action='spawn' | 'get_result')` in Web/server agent turns without regressing CLI multi-agent behavior.
> Audience: Runtime, Web agent, harness, and CLI maintainers.

This document is the guardrail for the implementation. If future code needs to
change any shared semantics below, update the shared runtime contract first and
then update CLI and Web/server adapters to consume that same contract.

## Problem

`astra` already has working dynamic multi-agent support in CLI: the model calls the consolidated `agent` tool with `action='spawn'`, the CLI wires a `DynamicAgentSpawner`, and `CliSpawnAgentExecutor` runs child agent loops.

Before this work, Web agent sessions exposed the same high-level `agent` schema through server-side tools, but server execution did not implement `spawn` or `get_result`. In Web chat, a model that tried to fan out child agents hit the server executor branch that reported spawn/get_result were not handled server-side.

That gap blocked two requirements:

- Web UI chat should support prompts such as "use multiple agents with different roles to review this PR".
- Harness built on Web agent should observe parent/child runs, tools, tokens, progress, and final synthesis without relying on CLI-only behavior.

## Goals

- Make dynamic multi-agent fan-out work in Web/server agent sessions.
- Preserve existing CLI behavior and CLI-specific execution semantics.
- Avoid duplicated spawn/get_result protocol logic across CLI and server.
- Keep execution environment-specific logic behind executor implementations.
- Make harness see child-agent activity as first-class runtime events/snapshots.
- Keep parent/child lineage explicit through `parent_run_id`, `agent_id`, and session journal/progress events.

## Non-Goals

- Do not simulate multi-agent behavior in Next.js by issuing multiple `/chat/stream` requests.
- Do not add permissive fallback behavior where Web silently pretends a child agent ran.
- Do not replace `CliSpawnAgentExecutor`.
- Do not make server child agents use CLI edge tools.
- Do not solve custom `.astra/teams` dynamic agent types in the first slice unless required by existing Web product behavior.

## Current State

### CLI path

CLI wires all required pieces:

- `astra-tools` defines the consolidated `agent` schema with `spawn`, `get_result`, `run_chain`, and `send_message`.
- CLI `ToolExecutor` receives a `SpawnAgentContext`.
- `agent(action='spawn')` is parsed by the shared runtime handler through the CLI compatibility wrapper.
- `DynamicAgentSpawner` creates and tracks the child run.
- `CliSpawnAgentExecutor` runs the child with local/edge tools.
- Parent gets the `agent_id`, later calls `get_result`, and synthesizes the final answer.

Important boundary: `DynamicAgentSpawner` owns lifecycle/state; `SpawnAgentExecutor` owns actual child execution.

### Web/server path

Web chat sends one request to runtime `/chat/stream`. Server-side runtime builds a `ServerToolExecutor` when no edge agent is connected.

Pre-change behavior:

- `agent(action='run_chain')` was delegated to the default executor.
- `agent(action='spawn')` and `agent(action='get_result')` returned unsupported action errors.

Implemented behavior in this branch:

- `agent(action='spawn')` and `agent(action='get_result')` route through the shared runtime handler.
- `agent(action='run_chain')` remains server-local and delegated to the default executor.
- `agent(action='send_message')` fails explicitly until mailbox-aware shared execution is added.

Server delegation already has a different sub-run executor path:

- `ServerSubRunExecutor` creates `ServerAgenticLoopHost` for delegated children.
- It wires server-side tools, workspace isolation, skill resolver, memory extraction, and observe-only harness sink.
- It is tied to the older `delegate` flow and `SubRunExecutor`, not dynamic `agent.spawn`.

The missing bridge is now `ServerSpawnAgentExecutor`: it adapts dynamic spawn
to the same server child-loop machinery rather than creating a separate
server-only agent lifecycle.

## Target Architecture

```
Parent agent loop
  |
  | tool call: agent(action='spawn', ...)
  v
Shared agent tool handler
  |
  | normalized SpawnAgentInput + AgentToolContext
  v
DynamicAgentSpawner
  |
  | SpawnRunConfig
  v
Environment-specific SpawnAgentExecutor
  |                         |
  | CLI                     | Web/server
  v                         v
CliSpawnAgentExecutor       ServerSpawnAgentExecutor
edge tools + TUI            ServerAgenticLoopHost + ServerToolExecutor + harness sink
```

The core rule: the protocol and lifecycle are shared; execution remains environment-specific.

## Core Principle

CLI and Web/server must share the agent runtime contract. They must not grow
parallel interpretations of what an agent spawn means.

Shared layers:

- Agent tool semantics.
- Agent input/output protocol.
- Agent lifecycle and state management.
- Parent/child lineage.
- Agent type definitions and constraints.
- Permission inheritance semantics.
- Progress and completion events.
- Mailbox and shared context semantics.
- Harness observation semantics.

Separated layers:

- Executor implementation.
- Tool backend.
- Credential/token resolution.
- Workspace provisioning details.
- UI rendering.

In short: share the agent runtime contract; split only the execution adapter.

## Phase 1 Lineage Decision

Web/server dynamic multi-agent should match the current CLI dynamic spawn model.

- Do not create separate user-visible Web chat sessions for child agents.
- Keep one parent session as the persistence, journal, progress, and harness container.
- Give every child an independent `run_id` and `agent_id`.
- Link children to the parent through `parent_run_id`.
- Use a child workspace/execution directory for isolation, but do not expose it as a separate chat.
- Let `get_result` address children by `agent_id`, exactly like CLI.

Rationale: CLI dynamic spawn is run-oriented, not chat-session-oriented. Matching that shape keeps parent/child management, result collection, and harness observation consistent across CLI and Web. Separate child sessions can be added later as a UI browsing feature, but they should not be required for the first server implementation.

## Shared Runtime Contract

The following behavior is authoritative and must be shared by CLI and Web/server.

### Tool Semantics

- The consolidated tool name is `agent`.
- Shared Phase 1 dynamic-agent actions are `spawn` and `get_result`.
- `run_chain` remains an existing environment-local action and is not part of dynamic-agent lifecycle management.
- `send_message` remains an advertised consolidated action, but server support requires a mailbox-aware executor refactor because current mailbox ownership lives in `AgenticLoopState`. Do not fake server support; unsupported mailbox wiring must fail explicitly.
- `spawn` launches exactly one child agent.
- Fan-out requires N separate `agent(action='spawn', run_in_background=true, ...)` tool calls in one assistant message.
- `get_result` addresses a child by `agent_id`.
- `run_in_background` is the canonical background flag.
- `background` remains a legacy alias and must normalize to `run_in_background`.
- `agents: [...]` batch payloads are invalid.
- `spawn: {...}` wrapper payloads are invalid.
- `task` as a deprecated alias for `prompt` must not be reintroduced unless the shared parser explicitly owns that compatibility decision.

### Spawn Input

Shared spawn input must resolve to one `SpawnAgentInput`.

Required semantic fields:

- `description`: short user-visible label.
- `prompt`: full task brief sent to the child.

Shared optional fields:

- `agent_type`
- `model`
- `run_in_background`
- `name`
- `max_turns`
- `complexity`
- `isolated`
- `allowed_tools`
- `inherit_prefix`

Defaulting rules must live in shared code:

- Existing CLI compatibility is preserved: if exactly one of `description` or `prompt` is supplied, shared code derives the missing one from the supplied value. Supplying neither is invalid.
- Missing `agent_type` defaults to `general-purpose`.
- Missing child `model` inherits the parent model through `AgentToolContext.current_model`.
- `complexity` scales the agent-type turn budget through the existing shared turn-budget resolver.

### Spawn Output

Shared `spawn` output must use the existing `SpawnAgentOutput` semantics:

- `completed`
- `cancelled`
- `waiting`
- `launched`
- `failed`

Background `spawn` returns `launched` with `agent_id`. Synchronous `spawn` returns the terminal child output.

### get_result Output

Shared `get_result` output must preserve the current CLI behavior:

- `completed`: child completed; includes `agent_id`, `result`, `finish_reason`, and `incomplete=false`.
- `interrupted`: child stopped with non-normal finish reason; includes `incomplete=true` and a hint.
- `failed`: child failed; includes `agent_id`, `error`, and `finish_reason`.
- `still_running`: child exists and is not terminal after the wait window; parent should call `get_result` again.
- `timeout`: child has no live or archived state after the wait window.

This distinction is part of the contract. Web/server must not collapse `still_running` into failure.

### Lifecycle and State

`DynamicAgentSpawner` is the shared owner of lifecycle state:

- `active_agents`
- `completed_agents`
- `background_tasks`
- `background_agent_ids`
- `completion_notifiers`
- `prefix_resolve_outcomes`
- `ProgressBroadcaster`
- `SharedContextCache`
- `AgentMailboxRouter`

No executor may keep a separate authoritative child-agent state table for dynamic spawn. Executors may keep local execution details, but lifecycle status must flow back through `DynamicAgentSpawner`.

### Lineage

Every child must have:

- independent `run_id`
- independent `agent_id`
- `parent_run_id` equal to the parent turn run id
- `agent_type`
- `description`

The parent session is the Phase 1 persistence container for Web/server, matching CLI. Child Web chat sessions are not part of the Phase 1 runtime contract.

### Agent Types and Constraints

Agent type definitions are shared runtime data:

- `explore`
- `code-review`
- `task`
- `general-purpose`

Their prompts, default models, tool allowlists, read-only flags, and default turn budgets must be resolved by the shared `AgentRegistry`. Web/server must not define a second copy of these semantics.

### Permission Semantics

Child permissions derive from parent permissions. Children may be narrower than parents, never broader.

Rules:

- Parent permission mode is inherited.
- Background status is reflected in inherited permissions.
- Read-only agent types must restrict mutation tools.
- If a child needs approval but cannot obtain it, it must return waiting/interrupted/failure according to shared runtime semantics. It must not fabricate success.

### Progress, Journal, and Harness

Progress and journal events must use the shared agent lifecycle:

- `agent_spawned`
- `agent_progress`
- `agent_completed`
- journal spawn/termination events when session journaling is configured

Harness must observe child runs through the same lineage and progress model. Phase 1 child harness mode is observe-only into the parent sink.

### Explicit Non-Contract

These are not shared runtime semantics:

- CLI TUI rendering.
- Web component rendering.
- Local MCP discovery.
- Server workspace path layout beyond the requirement that children are isolated under the parent session workspace.
- Token lookup implementation.
- Exact tool executor implementation.

## Design

### 1. Move the agent tool protocol handler into runtime

Shared runtime module:

`rust/crates/runtime/src/orchestration/agent_tool.rs`

Responsibilities:

- Parse the consolidated `agent` tool payload.
- Normalize aliases such as `run_in_background` and legacy `background`.
- Reject unsupported batch shapes like `agents: [...]` with a clear error.
- Convert `action='spawn'` into `SpawnAgentInput`.
- Convert `action='get_result'` into a `wait_for_agent` call.
- Return an explicit hard error for server-side `send_message` until mailbox-aware execution is shared; CLI may continue routing `send_message` through its existing mailbox executor wrapper.
- Render structured JSON results compatible with the current CLI tool output.
- Own all shared validation, defaulting, and output shaping listed in [Shared Runtime Contract](#shared-runtime-contract).

Authoritative API:

```rust
pub struct AgentToolContext {
    pub run_id: String,
    pub agent_id: String,
    pub current_model: Option<String>,
    pub recursion_depth: u8,
    pub working_dir: PathBuf,
    pub spawner: Arc<DynamicAgentSpawner>,
    pub inherited_permissions: InheritedPermissions,
    pub active_skills: Vec<String>,
    pub live_event_sink: Option<SharedAgentLiveEventSink>,
}

pub async fn handle_agent_tool(args: &Value, ctx: Option<&AgentToolContext>) -> String;
```

`handle_agent_tool` returns the stringified tool output because the existing CLI and server tool interfaces both consume string output. JSON construction still lives in the shared handler so wrappers do not own rendering semantics.

Compatibility note: keep `rust/crates/astra-cli/src/edge_tools/agent_spawning.rs` as a thin wrapper during migration, but remove its independent parsing logic. It should delegate to runtime.

Hard rule: `ServerToolExecutor` must not copy or reimplement spawn/get_result validation. It should call this shared module.

### 2. Add `ServerSpawnAgentExecutor`

Server executor implements:

```rust
#[async_trait]
impl SpawnAgentExecutor for ServerSpawnAgentExecutor {
    async fn execute(&self, config: SpawnRunConfig) -> Result<SpawnRunResult, String>;
}
```

The current implementation places it near `ServerSubRunExecutor` in:

`rust/crates/runtime/src/server/run_lifecycle.rs`

It can be moved later to a dedicated module if the file is split, but the
adapter boundary must stay the same:

Responsibilities:

- Build a `ServerAgenticLoopHost` for each spawned child.
- Use the child `config.agent_id`, `config.run_id`, `config.agent_type`, `config.task`, `config.system_prompt_addendum`, `config.model`, and `config.max_turns`.
- Wire `ServerToolExecutor` so child agents can use server-executable tools.
- Apply `config.allowed_tools` and `config.read_only` to the child state restrictions.
- Reuse server workspace provisioning. For dynamic spawns, prefer a child workspace under the parent session workspace:

```text
$ASTRA_SERVER_WORKSPACES/<session_id>/<child_run_id>/
```

- Thread cancellation token and pause flag where available.
- Thread `config.context_cache`, `config.mailbox`, and permission context.
- Emit `SpawnRunResult` with final output, token usage, tool count, permission telemetry, and finish reason.

Implementation should reuse code from `ServerSubRunExecutor` where possible. If reuse becomes large enough, extract a common builder:

```rust
ServerChildLoopBuilder
```

Inputs:

- `ChildLoopKind::Delegate | ChildLoopKind::Spawn`
- session/user/model/tool constraints
- workspace policy
- harness sink policy

This prevents the server delegate path and dynamic spawn path from drifting.

### 3. Add session-scoped server spawner registry

`DynamicAgentSpawner` should not be recreated per tool call, because `get_result` must see agents spawned earlier in the same parent turn/session.

Add a registry owned by `RunLifecycleService`:

```rust
server_agent_spawners: Arc<RwLock<HashMap<String, ServerAgentSpawnerEntry>>>
```

Key by `session_id`.

The registry provides:

```rust
async fn server_agent_spawner_for_session(&self, session_id: &str) -> ServerAgentSpawnerEntry;
```

The spawner must be built with:

- shared `ProgressBroadcaster`
- `ServerSpawnAgentExecutor`
- shared `AgentMailboxRouter`
- optional session journal persistence through `.with_session(session_id)`
- prefix store when fork-prefix is enabled

If a session can have multiple concurrent parent runs, keying the registry by `session_id` is still sufficient for result lookup because `agent_id` is globally unique, while `list_agents(parent_run_id)` preserves parent filtering. Do not key by child session in Phase 1 because child sessions are not part of the CLI-aligned dynamic spawn model.

Important concurrency rule: the session-scoped spawner must not imply a
session-scoped single mutable executor context. `ServerSpawnAgentExecutor`
stores runtime context by `parent_run_id`, and `SpawnRunConfig.parent_address`
selects the correct parent context at child execution time. A single
"latest context" slot is forbidden because concurrent parent runs in one
session could otherwise execute children under the wrong user/session/request
constraints.

### 4. Wire agent context into `ServerToolExecutor`

Extend `ServerToolExecutor` with:

```rust
agent_tool_context: Option<AgentToolContext>
```

Add:

```rust
pub fn set_agent_tool_context(&mut self, ctx: AgentToolContext);
```

In `RunLifecycleService::stream_chat` and non-streaming run creation, after `run_id`, `session_id`, `user_id`, workspace, and harness sink are known:

- fetch/create the session spawner
- build `AgentToolContext`
- pass it into `ServerToolExecutor`

Then replace the server executor's current unsupported `agent.spawn/get_result` branch with a call to the shared runtime handler.

No fallback: if `agent_tool_context` is absent, return a hard error saying server dynamic multi-agent is not wired for this session.

No duplicate parser: `ServerToolExecutor` may pattern-match only enough to keep
environment-local actions such as `run_chain` local. Validation and rendering
for `spawn`, `get_result`, rejected `agents: [...]`, and rejected `spawn: {...}`
must come from the shared runtime handler.

### 5. Progress and UI events

The existing `ProgressBroadcaster` already converts agent events into SSE-compatible values:

- `agent_spawned`
- `agent_progress`
- `agent_completed`

Wire the server spawner to the same broadcaster that `ServerAgenticLoopHost` subscribes to. The Web proxy can continue forwarding raw SSE frames. UI-specific rendering can be added later, but the runtime behavior should not depend on UI changes.

### 6. Harness integration

For the first slice, child spawns should use observe-only harness snapshots into the parent sink:

```rust
HarnessSlot::observe_only(parent_sink)
```

This matches the existing server sub-run behavior. It gives the harness:

- child turns
- child tool usage
- token counters
- parent/child lineage through run IDs and progress events
- final synthesis in the parent run

The parent run remains the enforcement point. That avoids double-enforcing budgets across parent and children until the harness policy explicitly supports per-child kernels.

Future extension:

- `harness.child_policy = observe_only | enforce_per_child | inherit_parent_limits`
- child snapshots include explicit `parent_run_id`, `agent_id`, and `agent_type` fields if the current snapshot schema is not enough for analysis.

### 7. Permission and safety model

Server child agents must never gain broader capability than the parent turn.

Rules:

- Child inherits parent permission mode through `InheritedPermissions`.
- `read_only` agent types must block write tools even if the server tool catalog contains them.
- Server child workspaces are isolated under session workspace.
- No CLI local filesystem tools are exposed in Web/server child runs.
- If approval/ask-user is needed, the behavior should match existing server-side ask-user semantics. If a background child cannot ask the user interactively, return a waiting/interrupted result rather than fabricating success.

### 8. CLI compatibility

CLI behavior should remain unchanged:

- `CliSpawnAgentExecutor` remains the executor for CLI spawns.
- CLI still owns token provider refresh, TUI live event sink, local project root, MCP tools, and edge tool executor.
- CLI still injects `AgentToolContext` into its `ToolExecutor`.
- Shared runtime handler only replaces duplicate parsing/wait/render logic.

Regression guard tests should assert:

- CLI `agent(action='spawn')` still calls `DynamicAgentSpawner` with `CliSpawnAgentExecutor`.
- CLI `get_result` still returns `still_running` for live incomplete children.
- CLI no-spawner context still fails explicitly.
- Server executor runtime context is keyed by `parent_run_id`, not by a mutable
  session-wide latest value.

## CLI Parity Contract

The Web/server implementation is correct only if it preserves these parity points with CLI:

- Same `agent` schema shape.
- Same accepted and rejected payload shapes.
- Same `SpawnAgentInput` normalization.
- Same `SpawnAgentOutput` JSON variants.
- Same `get_result` terminal and non-terminal JSON variants.
- Same `agent_id` addressing model.
- Same parent-run lineage semantics.
- Same built-in agent type definitions.
- Same read-only/write restriction semantics.
- Same shared context and mailbox semantics where those features are used.
- Same explicit failure when agent context is unavailable.

Allowed differences:

- CLI uses `CliSpawnAgentExecutor`; Web/server uses `ServerSpawnAgentExecutor`.
- CLI uses local edge tools and MCP; Web/server uses server-side tools.
- CLI token provider reads local credentials; Web/server uses server runtime token/model services.
- CLI emits TUI live events; Web emits SSE/progress events.
- CLI local workspace is the project root or child worktree; Web/server workspace is server-provisioned under the parent session workspace.

Any future change that modifies shared semantics must update the shared runtime module first, then let CLI and Web/server consume that behavior through the same API.

## Prohibited Implementation Patterns

Do not implement Web/server support using any of these shortcuts:

- Do not issue multiple Web `/chat/stream` requests from Next.js to simulate child agents.
- Do not add a second parser for `agent(action='spawn')` in `ServerToolExecutor`.
- Do not keep a separate server-only child-agent lifecycle map outside `DynamicAgentSpawner`.
- Do not return fake success for unsupported spawn/get_result.
- Do not silently degrade to `launched` when no executor exists in Web/server; missing executor must be a hard configuration error.
- Do not create child Web chat sessions in Phase 1.
- Do not bypass shared `AgentRegistry` definitions.
- Do not give Web/server children CLI-only tools.
- Do not let child permissions exceed parent permissions.

## Data Flow

### Web fan-out

1. User sends Web chat message.
2. Next.js posts one `/chat/stream` request to runtime.
3. Parent server agent decides to call multiple `agent(action='spawn', run_in_background=true, ...)` tools in one LLM round.
4. `ServerToolExecutor` routes each call to shared `handle_agent_tool`.
5. Shared handler calls session `DynamicAgentSpawner`.
6. Spawner registers child state and schedules `ServerSpawnAgentExecutor`.
7. Parent receives launched `agent_id`s.
8. Parent calls `agent(action='get_result', agent_id=...)`.
9. Shared handler waits on spawner completion notify.
10. Parent synthesizes results into the final answer.
11. Web receives normal text/progress SSE.

### Server child execution

1. `ServerSpawnAgentExecutor.execute(config)` builds child messages.
2. It builds `ServerAgenticLoopHost` with child model and server-side tools.
3. It builds `AgenticLoopState` with child run/session metadata.
4. It wires `ServerToolExecutor` for child tool calls.
5. It runs `run_agentic_loop_with_host`.
6. It returns `SpawnRunResult`.
7. Spawner updates status, journal, progress broadcaster, and completion notify.

## Testing Plan

### Unit tests

- Shared agent tool parser accepts `run_in_background` and legacy `background`.
- Shared parser rejects `agents: [...]` and `spawn: {...}` wrapper payloads.
- Shared `get_result` distinguishes `completed`, `failed`, `still_running`, and true timeout.
- Shared parser and renderer are used by both the CLI wrapper and server dispatch.
- Server executor maps `SpawnRunConfig` into child `AgenticLoopState` fields correctly.
- Read-only child agents restrict write tools server-side.
- Missing agent context returns the same explicit error shape in CLI and Web/server.

### Integration tests

- Web/server mock LLM emits `agent(action='spawn')` and final synthesis through the server executor.
- A follow-up Web/server test should cover two `agent(action='spawn', run_in_background=true)` calls in one round, two `get_result` calls, and final synthesis.
- SSE contains `agent_spawned` and `agent_completed` events.
- Harness snapshot sink receives parent and child snapshots.
- CLI multi-agent scenario remains unchanged.
- CLI and Web/server produce equivalent JSON for shared spawn/get_result cases.

### E2E harness test

Add a Web agent scenario:

```text
User: Use three agents with different roles to review this PR.
Expected:
- parent run emits 3 child runs
- each child has distinct description/agent_type
- parent calls get_result for each child
- final answer cites all child summaries
- harness records parent and child tool usage
```

Current committed regression guards:

- Shared runtime handler unit tests cover accepted/rejected payload shapes, model inheritance, `send_message` hard failure, and `get_result` status rendering.
- Server dispatch tests verify rejected wrapper shapes and missing agent context route through the shared handler.
- Server lifecycle tests verify child tool constraints and per-parent runtime context lookup.
- Web E2E verifies a server-side dynamic child spawn returns child output and emits `agent_spawned`/`agent_completed` progress events.
- CLI wrapper tests verify CLI uses the shared handler API instead of carrying an independent parser.

## Migration Plan

1. Add shared runtime `agent_tool` module while keeping CLI wrapper API stable.
2. Move CLI `agent_spawning` tests to shared module or duplicate only wrapper smoke tests.
3. Implement `ServerSpawnAgentExecutor` with the smallest clean reuse from `ServerSubRunExecutor`.
4. Add session spawner registry to `RunLifecycleService`.
5. Inject `AgentToolContext` into `ServerToolExecutor`.
6. Enable server `agent.spawn/get_result` dispatch.
7. Add Web/server integration tests.
8. Add harness E2E coverage.
9. Remove obsolete server error text that says spawn/get_result are CLI-only.

## Phase 1 Implementation Slices

Each slice should be independently reviewable.

### Slice 1: Shared Agent Tool Contract

- Add `runtime::orchestration::agent_tool`.
- Move parser, normalizer, spawn dispatch, get_result dispatch, and output rendering into it.
- Keep CLI wrapper API stable.
- Add shared unit tests for all accepted/rejected payload shapes and get_result states.
- Verify CLI tests still pass.

### Slice 2: Server Spawn Executor

- Add `ServerSpawnAgentExecutor`.
- Reuse or extract server child-loop construction from `ServerSubRunExecutor`.
- Wire server-side tools, workspace, model resolution, skill resolver, permissions, context cache, mailbox, progress emitter, and harness observe-only sink.
- Add executor unit tests for config-to-state mapping and read-only restrictions.

### Slice 3: Session Spawner Registry

- Add session-scoped spawner registry to `RunLifecycleService`.
- Build server spawners with `ServerSpawnAgentExecutor`, shared broadcaster, mailbox router, prefix store, context cache, and session journal.
- Ensure `get_result` can find children spawned earlier in the same session.

### Slice 4: Server Tool Dispatch

- Add `AgentToolContext` to `ServerToolExecutor`.
- Route `agent.spawn/get_result` through the shared runtime handler.
- Route `agent.send_message` to an explicit shared hard error until the mailbox-aware executor refactor lands; do not claim success.
- Remove the server-only unsupported spawn/get_result branch.
- Keep `run_chain` behavior either routed through shared action handling or explicitly documented if still delegated to the default executor.

### Slice 5: Web/Harness Integration Tests

- Add Web/server fan-out integration test.
- Add SSE progress assertions.
- Add harness observe-only child snapshot assertions.
- Add CLI parity regression tests.

## Open Questions

- Should background child agents survive server process restart? Current in-memory spawner does not provide durable restart recovery. Durable child execution should be a later slice using the existing run/task leasing model.
- Should child harness enforcement be observe-only or per-child? First slice should be observe-only to avoid double budget enforcement.
- Should custom team YAML agent types be supported in Web first slice? Built-in `explore`, `code-review`, `task`, and `general-purpose` are enough for initial Web multi-agent review.

## Acceptance Criteria

- Web chat can execute dynamic multi-agent fan-out with `agent.spawn/get_result`.
- CLI multi-agent tests continue to pass without behavior changes.
- No duplicated parser/normalizer for `agent.spawn/get_result`.
- Agent semantics, state management, input normalization, output rendering, lifecycle, and lineage are shared by CLI and Web/server.
- Server child agents execute with server tools, not CLI tools.
- Harness receives child activity for Web agent runs.
- Missing wiring fails explicitly; no fake success and no silent fallback.

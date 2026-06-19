# Edge Runtime Tool Boundary

Status: Design specification
Last Updated: 2026-06-19
Related: `edge-cloud-execution.md`, `tool-architecture-consolidation.md`, `web-agent-runner.md`

## Execution Topology

| Surface | UI | Agent loop owner | Runtime tool executor | Transport | Process requirement |
| --- | --- | --- | --- | --- | --- |
| Local CLI | User terminal | `astra-cli` | In-process CLI executor | CLI to server request stream | No `astra-edge` process |
| Web with server workspace | Browser | Server runtime | Server local runtime executor | In-process server call | No edge process |
| Web with user local workspace | Browser | Server runtime | `astra-edge` on user machine | Server to edge WebSocket or edge ledger | Edge process required |
| Thin/remote CLI | Remote terminal/client | Server runtime | Server or edge-selected runtime | Server transport policy | Depends on selected workspace |

`astra-edge` exists only for reverse execution into a user-local workspace. It is not part of the local CLI process model.

Local CLI owns local tool execution directly. Adding an `astra-edge` child process to local CLI would duplicate the CLI's own executor path without adding a communication capability.

Web cannot access paths such as `/home/user/project` or `/Users/user/project`. A resident edge agent is required when a server-controlled agent turn must execute runtime tools against those paths.

## Deployment Flexibility Objective

Astra runtime execution must be deployment-neutral.

The server must route tools by binding and capability, not by a hard-coded deployment shape.

The same tool-routing model must support:

| Deployment shape | Workspace binding | Executor binding | Runtime binding | Primary use |
| --- | --- | --- | --- | --- |
| Local CLI | `LocalFilesystem` | `LocalCli` | `HostProcess` | Developer terminal workflow |
| Web with local edge | `EdgeWorkspace` | `EdgeAgent` | `HostProcess` | Browser workflow over user-local files |
| Web with server sandbox | `ServerSandbox` | `ServerRuntime` or `ServerLocal` | `HostProcess` or container runtime | Hosted workspace workflow |
| Cloud workspace runtime | `CloudWorkspace` | `OrchestratorManaged` | `ProviderManaged` or container runtime | SaaS or private cloud workspaces |
| Request-scoped MCP | Any compatible workspace | `RequestScopedMcp` | MCP transport runtime | External tool integration |
| Gateway relay | Any compatible workspace | `OrchestratorManaged` | Remote runtime | Enterprise or provider-managed execution |

The routing layer must not assume that runtime execution means local process execution, edge WebSocket execution, or Kubernetes execution. Those are transport and lifecycle choices behind the same binding model.

## Operator Integration Boundary

This document does not design a Kubernetes operator.

This document does not define:

- custom resource definitions,
- reconciliation loops,
- controller ownership rules,
- namespace layout,
- pod templates,
- PVC layout,
- service mesh policy,
- image build or rollout strategy.

This document defines the runtime contract that an operator can implement.

A Kubernetes operator can integrate by managing runtime lifecycle and publishing runtime state into Astra's existing binding and advertisement model.

| Operator-managed concern | Astra contract surface |
| --- | --- |
| Runtime creation | `WorkspaceBinding`, `ExecutorBinding`, `RuntimeBinding` |
| Runtime readiness | executor status, runtime status, heartbeat, capability advertisement |
| Runtime policy | `PolicyIntent`, resource limits, filesystem authority, network policy, credentials policy |
| Runtime disposal | lease expiry, idle timeout, run completion, workspace retention policy |
| Runtime recovery | executor id stability, session/workspace rebinding, degraded/offline status |
| Tool support | executor manifest and sanitized runtime advertisement |

Tool calls remain data-plane RPC. They must not require a Kubernetes CRD per tool invocation.

Operator-style lifecycle management is control plane. Tool execution is runtime data plane.

## Platform-Neutral Runtime Contract

Every runtime executor must expose the same minimal contract, independent of deployment platform:

| Contract | Requirement |
| --- | --- |
| Identity | stable executor id for routing, audit, and reconnection |
| Workspace | declared workspace kind, cwd or workspace handle, read/write authority |
| Runtime | session manager, isolation backend, launch driver, status |
| Tool manifest | actual supported tool names derived from handlers |
| Capability advertisement | sanitized `RuntimeEnvironmentAdvertisement` |
| Policy | filesystem, network, credentials, approval, audit, and resource constraints |
| Heartbeat | liveness and degraded/offline transition signal |
| Cancellation | request-level cancellation for long-running and destructive-capable tools |
| Timeout | request-level timeout enforced by the executor, not only by the caller |
| Artifacts | explicit artifact publication path for generated files |

Kubernetes is one implementation target for this contract. Edge host processes, server-local sandboxes, provider-managed runtimes, and gateway relays are other implementation targets.

## Tool Ownership

Tool ownership is determined before transport selection.

| Owner | Examples | Execution location |
| --- | --- | --- |
| Control plane | `ask_user`, `enter_plan_mode`, `exit_plan_mode`, `task`, `notify`, `introspect` | Server or CLI host |
| Server service | `memory`, `mo`, `mo_query`, `rollback_database_snapshots`, server-side `tool_search` | Server runtime |
| Runtime executor | `read_file`, `write_file`, `str_replace`, `list_dir`, `grep`, `glob`, `bash`, `git`, `run_script`, `symbols` | Server sandbox, edge, or orchestrator runtime |
| Request-scoped MCP | `mcp__...` | MCP transport |
| Host-interactive sink | approval, `ask_user` UI, plan review, permission-mode change | Active UI host |

Control-plane tools must not route to edge transport.

Runtime tools route to edge only when the workspace binding is `EdgeWorkspace`.

Server-service tools must remain server-owned unless the registry explicitly splits them into server-service and runtime-executor variants.

## Current Code Boundaries

`astra-edge` executes tool requests through `astra_tools::executor::DefaultToolExecutor`.

`astra-cli/src/edge_tools.rs` is a CLI facade. It combines shared tool logic, CLI-only session state, rollback journals, background task integration, local MCP dispatch, LSP sessions, plan-mode sinks, and default delegation into `astra_tools`.

`astra_tools` contains the shared executor, schemas, file operations, shell operations, git operations, GitHub client logic, web fetch/search, memory protocol helpers, ask-user parsing, task management, and run-script RPC.

`astra_runtime_env::ToolRegistry::builtins()` is a capability and routing registry. It is not a proof that a specific executor binary has a handler for every listed tool.

`astra_tools::schemas::all_tool_schemas()` is the model-visible static schema catalog. It is not a proof that every executor path can execute every schema.

Actual execution support is defined by each executor's dispatch table or registered handler table.

## Required Invariant

For every model-visible tool schema in a turn:

```text
schema_visible(tool)
  => runtime_registry_knows(tool)
  => run_binding_admits(tool, args)
  => route_can_deliver(tool)
  => selected_executor_supports(tool)
```

For every edge-advertised tool:

```text
edge_advertises(tool)
  => server_registry_knows(tool)
  => edge_executor_manifest_contains(tool)
  => edge_handler_executes(tool) without "not available" fallback
```

For every runtime-executor tool in an `EdgeWorkspace` turn:

```text
visible_runtime_tool(tool)
  => selected_edge_advertisement_contains(tool)
```

Control-plane and server-service tools are excluded from the edge advertisement intersection because they are not edge-owned.

## Drift Sources

Three catalogs currently need explicit parity checks:

| Catalog | Source | Purpose |
| --- | --- | --- |
| Model schemas | `astra_tools::schemas::all_tool_schemas()` | LLM-visible function definitions |
| Runtime registry | `astra_runtime_env::ToolRegistry::builtins()` | capability checks and route ownership |
| Handlers | `DefaultToolExecutor::dispatch`, `ServerToolExecutor` handlers, CLI `edge_tools::ToolExecutor::execute` | actual execution |

Known drift classes:

- A registry entry can exist without a model schema.
- A model schema can exist without a handler on a specific transport.
- A CLI handler can exist without a shared schema.
- An edge advertisement can pass registry validation while the edge executor lacks the handler.
- A server runtime schema can be visible for an edge workspace before intersecting with edge-advertised runtime tools.

## Edge Advertisement

Edge authentication must include a structured runtime advertisement derived from the edge executor manifest.

The advertisement must not be derived only from `ToolRegistry::builtins()`.

The advertisement must include:

- workspace binding,
- executor binding,
- runtime binding,
- policy intent,
- effective capabilities,
- supported tool names,
- denials for unsupported or policy-blocked tools.

Server validation must:

- reject malformed advertisements,
- reject non-edge executor bindings,
- strip unknown registry names,
- strip names absent from the edge executor manifest,
- preserve control-plane and server-service ownership outside the edge runtime set,
- store sanitized capabilities in the live edge pool and edge registry.

## Host Interaction Boundary

`ask_user` parsing is shared logic.

`ask_user` execution is host-owned because it requires an interactive client connection.

Approval gates are host-owned because they require user policy state, UI rendering, audit events, and response channels.

Plan review is host-owned because it changes permission mode and exposes a UI-specific review surface.

Background task control is host-owned because task registries and detach slots are drained by the active UI host.

Edge runtime tools must not block on stdin or TTY prompts. A missing host sink must return a structured tool error.

## Cancellation and Timeout

Edge must honor per-request `timeout_secs`.

Edge must support `edge_tool_cancel` for cancellable tools.

Cancellation requirements:

- cancellation token per in-flight request,
- propagation into shell/process tools,
- best-effort process termination,
- no result delivery after server cancellation,
- no late destructive writes after timeout,
- request cleanup on disconnect.

Server-side timeout without edge-side cancellation is insufficient for write-capable runtime tools.

## Module Classification

### Already Shared or Low-Risk Runtime Tools

These are implemented in `astra_tools` or already have shared equivalents:

- `read_file`
- `write_file`
- `str_replace`
- `delete_file`
- `multi_edit`
- `list_dir`
- `grep`
- `glob`
- `bash`
- `git` common actions
- `github`
- `web_fetch`
- `web_search`
- `tool_search`
- `env`
- `config`
- `run_script` on Unix
- `ask_user` parser only

Edge support for these must still be gated by executor manifest, runtime policy, and cancellation behavior.

### Candidate Extraction Modules

These can move toward `astra_tools` after schema, registry, and handler parity are defined:

| Module | Target | Required extraction work |
| --- | --- | --- |
| `notebook_edit.rs` | Runtime executor | Add schema, registry entry, shared handler, file sandbox checks |
| `code_analysis.rs` | Runtime executor or code-intel package | Define public tool names, add schemas, add registry specs, remove CLI facade dependency |
| `lsp_stdio_session.rs` | Runtime executor support library | Keep process/session lifecycle headless; expose configuration-driven construction |
| `passive_lsp.rs` | Runtime executor support library | Move workspace and server configuration into explicit runtime config |
| `lsp_tools.rs` | Runtime executor | Add manifest entries per supported operation or keep consolidated `lsp` only |
| `worktree.rs` | Runtime executor | Add git permission boundaries and rollback journal interface |
| `mo_tools.rs` | Split owner | Keep server-service `mo` by default; add separate runtime-owned variant only for edge-local MatrixOne |

### Host-Context Modules

These require host-provided context before extraction:

| Module | Host dependency |
| --- | --- |
| `context_tools.rs` | shared context cache, task manager, agent identity |
| `context_analysis.rs` | observability session |
| `agent_spawning.rs` | dynamic agent spawner |
| `agent_messaging.rs` | mailbox/router context |
| `mcp_dispatch.rs` | MCP manager and server declarations |
| `session_state.rs` | session-state journal and persistence ports |
| `self_mod_tools.rs` | self-command persistence ports and mutation governors |
| `memoria.rs` | server URL, auth token, memory service ownership |

### Host-Only Behaviors

These stay outside edge runtime execution:

- approval prompt rendering,
- `ask_user` UI and response collection,
- plan-review UI,
- permission-mode changes,
- TUI background task registry,
- CLI progress sink rendering,
- terminal-specific formatting.

## Web Tool Surface Rule for Edge Workspaces

For `EdgeWorkspace`, tool schema resolution must combine:

1. server control-plane schemas,
2. server-service schemas,
3. request-scoped MCP schemas,
4. runtime-executor schemas intersected with selected edge advertised tools.

Server sandbox workspaces use server-local runtime support instead of edge advertisement.

`WorkspaceBindingKind::None` exposes only control-plane and server-service schemas.

Unknown or disconnected edge runtime removes runtime-executor schemas from the model-visible surface, or emits a blocked run state before model execution.

## Parity Tests

Required tests:

| Test | Assertion |
| --- | --- |
| Schema to registry parity | Every static schema name is registered or explicitly marked plugin/pass-through/internal |
| Registry to schema parity | Every model-facing registry entry has a schema or is marked non-model-facing |
| Registry to handler parity | Every routable runtime tool has at least one handler for each advertised transport |
| Edge advertisement parity | Edge advertised names equal edge executor manifest names after policy filtering |
| Edge no-ghost execution | Any edge-advertised tool executes without `DefaultToolExecutor` "not available" fallback |
| Web edge schema intersection | Edge workspace runtime schemas are hidden when the edge does not advertise them |
| Control-plane bypass | `ask_user`, approvals, plan lifecycle, and task control never route to edge |
| Cancel propagation | `edge_tool_cancel` aborts long-running shell/process requests |
| Timeout propagation | `timeout_secs` limits edge execution, not only server wait time |
| CLI topology | Local CLI runs without starting or depending on `astra-edge` |

## Implementation Sequence

### Phase 0: Manifest and Parity

- Add executor manifest API to `astra_tools`.
- Derive `DefaultToolExecutor` supported tool names from registered handlers or a single local table.
- Add schema/registry/handler parity tests.
- Build edge advertisement from executor manifest plus runtime policy.
- Sanitize edge advertisements against both server registry and edge manifest.

### Phase 1: Edge Transport Correctness

- Track in-flight edge requests by request id.
- Wire per-request cancellation tokens.
- Honor `timeout_secs`.
- Implement `edge_tool_cancel` for shell/process-backed tools.
- Drop late results after cancellation or timeout.

### Phase 2: Web Schema Intersection

- During Web tool surface assembly for `EdgeWorkspace`, intersect runtime-executor schemas with selected edge advertised tools.
- Keep server control-plane, server-service, and MCP schemas outside that intersection.
- Emit run-blocked metadata when an edge workspace has no connected capable runtime executor.

### Phase 3: Platform-Neutral Runtime Contract

- Keep routing decisions based on `WorkspaceBinding`, `ExecutorBinding`, `RuntimeBinding`, `PolicyIntent`, and executor manifest.
- Represent orchestrator-managed runtimes without adding Kubernetes-specific assumptions to the core routing layer.
- Keep tool execution on data-plane transports.
- Keep lifecycle state in runtime status, executor status, heartbeat, lease, and advertisement records.
- Do not introduce operator-specific CRDs or reconciliation behavior in this phase.

### Phase 4: Pure Module Extraction

- Move pure runtime logic from `astra-cli/src/edge_tools` into `astra_tools`.
- Replace `crate::cli` references with explicit traits.
- Keep host sinks and UI-specific behavior in CLI/server host layers.
- Add transport-specific tests before exposing new schemas.

### Phase 5: Ownership Splits

- Split ambiguous tools by owner where needed.
- Keep `memory` server-owned.
- Keep default `mo` server-owned.
- Add edge-local database tools only as separately declared runtime tools.
- Keep MCP transport declarations outside the core edge executor manifest unless the edge owns the MCP server process.

## Acceptance Criteria

- Local CLI does not require `astra-edge`.
- Web local-workspace execution requires a connected edge runtime with a valid advertisement.
- Model-visible runtime tools for edge workspaces are executable by the selected edge.
- No advertised edge tool returns a generic "not available in DefaultToolExecutor" result.
- Control-plane tools never route to edge.
- Host-interactive tools fail fast without a host sink.
- Cancellable edge tools stop on server cancellation or timeout.
- Schema, registry, and handler drift is caught in CI.
- Runtime routing does not depend on Kubernetes-specific types.
- An orchestrator-managed runtime can integrate through binding, manifest, heartbeat, policy, and advertisement records.
- No operator CRD or controller design is required to implement this document.

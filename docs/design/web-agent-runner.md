# Web-Agent Execution

> Status: Current design contract.
> Scope: Web agent execution binding across edge agents, MCP tools, and orchestrator-managed cloud runtimes.
> Audience: Product, runtime, edge, security, and observability maintainers.

This document replaces the older server-owned execution design. Web agent sessions keep cloud-native state, but execution is bound to an explicit executor/runtime pair:

- **Edge agent**: a user-owned `astra-edge` process connected to the cloud for local filesystem, private network, and specialized hardware access.
- **Orchestrator-managed runtime**: a cloud workspace runtime provisioned and selected by an external orchestration layer such as an operator or deployment controller.
- **MCP server**: a remote tool endpoint for API-shaped operations that do not need workspace process execution.

Astra records and routes these bindings; it does not select leases for cloud executors or call Kubernetes APIs directly.

## Intent

Web agent sessions need to work across private codebases, internal services, specialized hardware, and cloud-managed workspaces. The design separates durable state from execution:

- **State is cloud-only**: session history, traces, plans, memory, artifacts, and run events live in the cloud database.
- **Execution is externally owned**: edge agents and orchestrator-managed runtimes execute tools outside the runtime server process.
- **Binding is explicit**: every workspace execution path carries workspace, executor, runtime, transport, and fallback metadata.

## Execution Paradigms

| Paradigm | Mechanism | Use case |
| --- | --- | --- |
| Edge agent | `astra-edge` connects to the cloud and serves tool execution over edge transport | Local repositories, private networks, hardware attached to a user-controlled machine |
| Orchestrator-managed runtime | External orchestration provisions the workspace runtime and exposes a resident agent transport | Cloud workspaces, managed sandboxes, long-lived team workspaces |
| MCP tool | Remote MCP service exposes API-like operations | Database reads, monitoring queries, external service integration |

These can be composed in one session. For example, an agent may query production metadata through MCP, then execute repository tests in an orchestrator-managed cloud workspace.

## Goals

- Preserve Web agent convenience: multi-device access, persistent state, collaborative visibility, and replayable event streams.
- Keep execution routing auditable through `ExecutionBinding` metadata.
- Represent cloud workspace execution as `orchestrator_managed` plus `sandbox_resident_agent`.
- Let external orchestration own provisioning, scheduling, pod/session lifecycle, and runtime health.
- Keep edge agents as a separate execution binding, not as the cloud workspace scheduler.
- Use MCP for lightweight API tools without requiring workspace process execution.

## Non-Goals

- Astra runtime server does not schedule cloud executor leases.
- Astra runtime server does not implement Kubernetes pod, deployment, or operator logic.
- Astra runtime server does not expose a cloud executor registration or heartbeat surface.
- The old server-owned RPC execution model is not part of the current contract.
- This is not a CI/CD DAG system; agent tasks remain session-scoped.

## Current Architecture

```
Browser / mobile client
        |
        v
Cloud API server
  - auth, session state, run events
  - workspace records
  - execution binding projection
        |
        +--> Edge agent transport
        |
        +--> Sandbox resident agent transport
        |      (runtime provisioned by external orchestration)
        |
        +--> MCP HTTP transport
```

The runtime server can reject or block a run when the binding is unavailable, but it should not invent an implicit fallback executor for a cloud workspace whose policy disables fallback.

## Binding Contract

For cloud workspaces, the normal path is:

```json
{
  "workspace": {
    "kind": "cloud_workspace",
    "authority": "read_write",
    "fallback_policy": "disabled"
  },
  "executor": {
    "kind": "orchestrator_managed",
    "executor_id": "orchestrator-managed",
    "transport": "sandbox_resident_agent",
    "status": "online"
  },
  "runtime": {
    "session_manager": "provider_managed",
    "isolation_backend": "provider_managed",
    "launch_driver": "kubernetes"
  },
  "transport": "sandbox_resident_agent"
}
```

The `kubernetes` launch driver is runtime metadata. It says the runtime is expected to be provisioned through the orchestration layer; it does not mean Astra calls the Kubernetes API.

## Product UX

The Web UI should present execution choices in terms users can act on:

- **Cloud workspace**: managed runtime for zero local setup.
- **Edge agent**: connect a local or private environment.
- **MCP tools**: connect scoped remote APIs.

Cloud workspace setup should not ask users to manage executor daemons. Operational controls for runtime classes, cluster placement, idle cleanup, and quota belong in the external orchestration/deployment surface.

## Failure Semantics

When execution cannot be routed, events should carry enough metadata to explain the boundary:

- `workspace.kind`
- `executor.kind`
- `executor.status`
- `transport`
- `fallback_policy`
- `runtime.launch_driver` when available

For cloud workspace execution, user-facing errors should say that the workspace is not routed to an available orchestrator-managed executor transport. They should not suggest connecting a legacy cloud executor daemon.

## Audit Requirements

Every tool call routed through Web agent execution should preserve:

- workspace identity and authority
- executor kind and id
- runtime id and isolation metadata
- selected transport
- fallback policy
- blocked/waiting reason when routing fails

This keeps run replay and support diagnosis possible without giving Astra ownership of the underlying orchestration system.

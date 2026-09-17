# Capability system

> Status: target design contract.
> Last updated: 2026-07-07.

The capability system owns tools, skills, MCP, provider decisions, tool schema projection, admission, execution routing, fallback, and diagnostics.

This is the target contract for implementation. It is not a snapshot of the current tool code.

The protocol-independent provider snapshot, internal tool identity, invocation,
and typed outcome sub-contract is defined in
[capability-provider-runtime.md](capability-provider-runtime.md).

## Principle

```text
visible(tool) and execute(tool_call) must come from the same provider decision.
```

No host loop, schema builder, or executor should maintain a separate shadow allowlist.

## Concepts

| Concept | Meaning |
| --- | --- |
| Capability | Abstract ability such as file read, shell execute, web fetch, memory query, agent spawn. |
| Provider | Concrete supplier of capability such as Edge, Server, MCP, cloud workspace, request-scoped binding. |
| Tool | Model-facing callable schema backed by one or more capabilities. |
| Skill | Packaged higher-level capability that may expose tools, prompts, resources, and policy. |
| Decision | Deterministic routing/admission result for a tool or capability. |

## Decision shape

A decision should include:

```text
capability
tool_name
provider_type
provider_id
execution_owner
route
admission_status
runtime_binding_status
fallback_policy
fallback_from
degraded_reason
offline_reason
user_visible_message
trace_fields
```

## Provider priority

1. Explicit user/request binding.
2. Edge/CLI local provider.
3. Request-scoped MCP provider.
4. Server cloud provider.
5. Policy-approved fallback provider.

When Edge/CLI and Server both provide `web_fetch`, Edge/CLI wins by default. Server `web_fetch` is fallback or policy-selected cloud route.

## Admission states

| State | Meaning |
| --- | --- |
| Ready | Selected provider can execute now. |
| Hidden | Tool should not appear in the current surface. |
| PolicyBlocked | Current mode, permission, or deployment policy blocks the call. |
| MissingRuntimeBinding | Provider contract exists but no runtime binding is available. |
| ProviderOffline | Provider is known but offline. |
| Unsupported | No provider owns this capability. |
| Malformed | Call shape is invalid before provider routing. |

Unknown executor-gated capability defaults to `Unsupported`.

For workspace-bound shell execution, the workspace root is the capability and
observation boundary. A Bash invocation may select an existing directory
within that root through its call-scoped `workdir`; this changes only the
subprocess starting directory and never persists as session state. Resolution
and symlink confinement happen at the CLI/User Runner executor before spawn,
and, on Unix process paths with pinned-cwd support, a root-relative no-symlink
directory-handle walk pins the identity used by policy, source capture, cache,
execution, and evidence. Executors that cannot consume that pinned subdirectory
identity (including the current managed mount boundary and non-Unix process
path) fail closed instead of falling back to path-based execution.

## Routes

| Route | Meaning |
| --- | --- |
| ServerControlPlane | Cloud state/control operations. |
| ServerRuntime | Explicitly configured server-safe runtime tools. |
| RequestScopedMcp | MCP tools bound to the request/session. |
| EdgeBound | User-owned local provider. |
| GatewayRelay | External workspace or resident-agent transport. |
| Unsupported | No execution route. |

## Tool surface

Tool schema order must be deterministic:

1. stable catalog tools in catalog order;
2. provider dynamic tools in deterministic provider order;
3. deferred tools through the same decision pool.

Dynamic provider state should change compact availability facts, not invalidate the stable prompt prefix.

The built-in default always-load surface is a bounded first-request primitive
set, not a catalog of every useful workflow. Artifact-result recovery through
`introspect` and the ordinary persisted-causality entrypoint through `reflect`
remain eager as compact read-only observation operations. Their resident
projections expose only the ordinary call shape; advanced reflection fields
remain available through explicit typed `tool_search`/`invoke_tool` selection.
Fan-out, advanced memory operations, and graph-maintenance schemas remain
discoverable through the deferred surface. The resident compact-JSON schema
projection has an 8 KiB regression budget with a safety margin; explicit user-
pinned tools may exceed it. Ordinary workspace navigation (`read_file`,
`list_dir`, and `grep`) stays resident, while the specialized `glob` query is
deferred and remains fully reachable through explicit selection. Deferred
entries carry only compact discovery metadata and are activated through the
typed protocol, so a selected full schema never silently re-enters the
repeated provider `tools[]` prefix. This bounds the fixed provider prefix
without making cache reuse or capability reachability a correctness
dependency.

Projection must preserve argument meaning as well as structural validation.
The resident `start_work.tasks` field retains its canonical acceptance-unit
definition; procedural steps serving one outcome are not separate outcomes.
Its duplicate discovery-only annotation stays in the catalog, not the resident
wire schema. This does not change the schema byte budget or Work admission
authority, and preserving the definition is not proof of model compliance.

## Skills

Skills are capability packages. A skill may contribute:

- instructions;
- tools;
- MCP bindings;
- resources;
- policy requirements;
- version and compatibility metadata.

A skill does not bypass provider decision. Skill-provided tools still require explicit capability ownership and runtime binding.

## MCP

MCP is a provider type, not a separate tool universe.

- Request-scoped MCP should be represented as provider decisions.
- MCP discovery failures should be observable degraded states.
- Lock contention or transient discovery should not silently look like permanent tool absence.
- MCP write tools obey plan and permission policy.

## Diagnostics

Diagnostics must distinguish:

- unknown tool;
- unsupported capability;
- missing runtime binding;
- provider offline;
- policy blocked;
- malformed tool call;
- fallback selected.

## Test obligations

- Projection, admission, and execution route agree.
- Unknown executor-gated capability is denied.
- Edge/CLI capability outranks server fallback when both are present.
- Server default capacity does not expose local executor tools.
- Plan mode blocks mutations without destroying read/introspect visibility.
- Provider fallback is traced and user-visible.

## Migration roadmap

Capability migration should proceed in stages:

1. Define provider decision as the only output of capability resolution.
2. Make schema projection consume provider decisions.
3. Make admission consume the same decision.
4. Make execution route consume the same decision.
5. Remove host-loop provider special cases that duplicate routing logic.
6. Add diagnostics and tests for unsupported, missing binding, offline, policy blocked, malformed, and fallback states.

## Capability unhappy paths

| Path | Required behavior |
| --- | --- |
| Unknown executor-gated capability | Deny as unsupported. |
| MCP discovery contention | Report degraded/transient state, not silent disappearance. |
| Edge offline | ProviderOffline for Edge-bound tools. |
| Server fallback disabled | Block capability with policy reason. |
| Tool schema visible but execution route unavailable | Invalid state; projection/admission/execution decision must be fixed. |
| Plan mode mutation | PolicyBlocked with explanation. |
| Unknown tool name | Unknown/malformed, not missing runtime binding. |

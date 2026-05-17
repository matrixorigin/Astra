# Tool Architecture Consolidation

This design defines Astra's tool-visibility model. Tool schemas may be
implemented in different crates today, but what the model sees must be derived
from one rule:

```text
visible(tool) = surface_admits(tool.scope, surface)
              && capabilities.has_all(tool.requires)
```

## Phase 0: capability-driven tool surface

The current implementation lives in:

- `astra_turn_core::capability::{Capability, CapabilitySet}`
- `astra_turn_core::tool_surface::{Surface, resolve, resolve_with_diagnostics}`
- `astra_turn_core::tool_registry_meta::ToolMeta::requires`
- `astra_runtime::capabilities::{server_runtime_tool_schemas, cli_local_tool_schemas}`

`DEFAULT_EXECUTOR_TOOL_NAMES` and `SERVER_EXECUTOR_TOOL_NAMES` are removed.
There is no second allowlist to synchronize with `TOOL_CATALOG`.

## Surfaces

| Surface | Execution location | Tool source |
| --- | --- | --- |
| `Web` | API server | capability-resolved server catalog + server MCP/plugins |
| `CliRemote` | API server | same policy as `Web` |
| `CliLocal` | CLI process | capability-resolved local catalog + local MCP/plugins |

`Scope` describes where a tool runs relative to its executor. It does not mean
"CLI-only". For example, `read_file` is `Scope::Local`, but Web can still
execute it against the server-side workspace.

## Capabilities

Capabilities are session-invariant. They are fixed before the tool list and
capability-conditioned prompt text are rendered, preserving prompt-cache
stability within a session.

| Capability | Gates |
| --- | --- |
| `AgentSpawner` | `agent` spawn/result collection |
| `MemoryService` | `memory` |
| `Database` | `mo` |
| `SkillsCatalog` | dynamic `skill` schema |
| `GitHubAuth` | `github` |
| `LSPServer` | `lsp` |
| `PlanLifecycle` | `enter_plan_mode`, `exit_plan_mode` |

Production server/web-agent capabilities are derived from actual service wiring
and intentionally do **not** include `AgentSpawner` until server-side dispatch
for `agent(action='spawn'|'get_result')` is implemented. Tests that need the
entire catalog use the explicitly named `full_server_capabilities_for_tests()`.

CLI local capabilities include the local tool services it can route. The
`AgentSpawner` capability is included only when a `DynamicAgentSpawner` is
wired for the session.

## MCP and plugin tools

Static catalog tools are filtered by `ToolMeta::requires`. Runtime plugin/MCP
schemas whose names are not in `TOOL_CATALOG` pass through after catalog tools,
preserving deterministic catalog order and stable prompt-cache prefixes.

MCP tools should use the `mcp__<server>__<tool>` naming convention. If a plugin
schema collides with a catalog name, the catalog filter wins: a capability-gated
catalog tool cannot bypass filtering via plugin pass-through.

## Prompt and provider contract

`tools[]` ordering is deterministic:

1. Catalog tools in `TOOL_CATALOG` order when admitted by surface/capabilities.
2. Non-catalog plugin/MCP schemas in source order after dedupe.

For identical `(surface, CapabilitySet, schema_pool)`, `resolve` output must be
byte-stable. Capability changes mid-session are not allowed because they would
change both `tools[]` and capability-dependent system prompt text.

Providers that support many tools can receive the resolved list directly.
Deferred-tool flows (`tool_search(select:NAME)`) must search the same
post-capability pool plus plugin schemas, not a legacy static allowlist.

## Observability

`introspect(dimension="capability")` should explain:

- active and inactive capabilities,
- visible tools,
- catalog tools dropped because required capabilities are missing,
- plugin/MCP tools that passed through,
- the actual emitted tool count when a turn selection report is available.

## Guardrail tests

Required invariants:

- `agent` is absent when `AgentSpawner` is absent.
- production server/web-agent capabilities do not advertise `agent`.
- `resolve` is byte-stable for identical inputs.
- catalog ordering is stable.
- plugin pass-through does not bypass catalog capability filters.
- plan lifecycle tools require `PlanLifecycle`.
- prompt fan-out guidance is hidden when `AgentSpawner` is absent.

## Later phases

Phase 1 should move schema and metadata definitions into a single shared
definition type. Phase 2 should continue consolidating duplicated clients.
Phase 3 can replace parallel CLI/server dispatch match arms with registered
tool handlers once schema/metadata drift is fully eliminated.

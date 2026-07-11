# Astra documentation

This directory is the design, operation, and reference documentation for Astra.

Design docs are normative target contracts. They guide implementation; they are not summaries of whatever the current code happens to do today. Current implementation may satisfy only part of a design in a given branch.

Docs here should describe durable design, target behavior, public contracts, runbooks, and reference material. They should not be used as implementation diaries, PR status logs, verification transcripts, or historical scratchpads.

## Start here

| Document | Scope |
| --- | --- |
| [Architecture](design/ARCHITECTURE.md) | Current system overview and non-negotiable architecture principles. |
| [Design index](design/README.md) | Canonical design domains and ownership boundaries. |
| [Documentation architecture](design/documentation-architecture.md) | Rules for design ownership, document classes, and migration from historical docs. |
| [Agent backbone and capacity providers](design/agent-backbone-capacity-provider.md) | Shared agent semantics across Web, CLI, Edge, Server, MCP, and future providers. |
| [Runtime lifecycle](design/runtime-lifecycle.md) | Sessions, runs, turns, tasks, plan mode, cancel, resume, and recovery. |
| [Capability system](design/capability-system.md) | Tools, skills, MCP, provider routing, admission, fallback, and diagnostics. |
| [Tool result quality firewall](design/tool-result-quality-firewall.md) | Tool output validation and quality annotations before model reuse. |
| [Context and prompt](design/context-and-prompt.md) | Context assembly, prompt cache, dynamic state, and memory injection boundaries. |
| [Prompt lifecycle](design/prompt-lifecycle.md) | Prompt assembly, versioning, stable prefix, cache, and evolution boundary. |
| [Context window management](design/context-window-management.md) | Token budgets, compaction, and context preservation. |
| [Observation plane](design/observation-plane.md) | Trace, audit, introspect, reflect, status, and user-visible diagnostics. |
| [Introspect and reflect](design/introspect-and-reflect.md) | Agent self-observation, reflection boundaries, and introspection dimensions. |
| [Session observability](design/session-observability.md) | User/support visible status, stream projection, stuck diagnosis, reconnect. |
| [Edge-cloud execution](design/edge-cloud-execution.md) | Edge/CLI local capacity and server-safe cloud fallback. |
| [Cloud-edge sync](architecture/edge-cloud-sync-architecture.md) | Durable outbox, event facts, retention, repair, and sync status. |
| [Safety and permissions](design/safety-and-permissions.md) | Permission, sandbox, side-effect, policy, and trust boundaries. |
| [Permission sync](design/permission-sync.md) | Cross-surface scoped approvals, revocation, expiration, and audit. |
| [Trust and safety](design/trust-and-safety.md) | Evidence, claim support, trust levels, and audit obligations. |
| [Tuning jobs](design/tuning-jobs.md) | Controlled prompt/skill/routing/memory/model improvement workflows. |
| [Evaluation](design/evaluation.md) | Behavioral evaluation, replay modes, and regression gates. |

## Directory map

| Directory | Purpose |
| --- | --- |
| `design/` | Current design contracts and target behavior. |
| `architecture/` | Cross-domain architecture views. |
| `guides/` | Operational guides and runbooks. |
| `quickstart/` | Setup and first-run material. |
| `reference/` | API, CLI, configuration, command, and dependency reference. |
| `testing/` | Test strategy and coverage contracts. |

## Documentation rules

- One design domain has one canonical document.
- Avoid implementation chronology. Describe invariants, responsibilities, and failure semantics.
- Avoid duplicate source-of-truth documents. Merge or delete older versions.
- Keep stable contracts in `docs/`; keep transient planning in `plans/` only while actionable.
- Prefer concise current design over long historical documents.
- A doc should state goals, non-goals, ownership boundaries, data/state model, failure modes, and test obligations.

## Architecture principle

Astra has one agent backbone and multiple capacity providers.

Web, CLI, Edge, Server, MCP, and future providers share session/run/turn lifecycle, context assembly, trace, reflection, checkpoint, tool admission, failure semantics, and audit. Capability differences come from providers, not from separate agent implementations.

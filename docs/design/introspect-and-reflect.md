# Introspect and reflect

> Status: target design contract.
> Last updated: 2026-07-07.

Introspect and reflect are first-class backbone capabilities. They are not debug-only tools and not prompt decorations.

## Ownership

This document owns:

- agent self-observation contract;
- introspection dimensions and response shape;
- reflection boundaries;
- model-visible diagnostics for state, context, capability, trace, provider, sync, and task state;
- safety boundaries for what the agent may inspect.

It does not own:

- raw trace storage, owned by [observation-plane.md](observation-plane.md);
- provider routing, owned by [capability-system.md](capability-system.md);
- context assembly, owned by [context-and-prompt.md](context-and-prompt.md);
- lifecycle transitions, owned by [runtime-lifecycle.md](runtime-lifecycle.md).

## Principle

```text
Introspect reports system facts. Reflect reasons over those facts.
```

Introspection must be factual, structured, and bounded. Reflection may synthesize strategy, uncertainty, and next actions, but should not mutate state by itself.

## Goals

- Give the agent accurate self-awareness without exposing unsafe internals.
- Let the agent explain why a tool is unavailable, blocked, degraded, or hidden.
- Let the agent understand current run/session/task/sync/provider stage.
- Preserve Web/CLI/Edge parity at the backbone level.
- Avoid repeated exploration caused by missing state visibility.
- Keep prompt-cache stable by exposing dynamic state through compact structured introspection.

## Introspection dimensions

| Dimension | Answers |
| --- | --- |
| `state` | Current session/run/turn/task status, stage, terminal/resumable state. |
| `capability` | Available, hidden, blocked, offline, degraded, or unsupported capabilities. |
| `provider` | Provider bindings, selected routes, fallback policy, health, offline reason. |
| `tool` | Visible tools, why hidden/blocked, expected argument contract, last failures. |
| `context` | Loaded context blocks, compaction status, memory/artifact references. |
| `invocation lifecycle` | Prepared/dispatched/terminal counts, dispatch certainty, reconciliation, archive/reference ownership, maintenance progress. |
| `prompt_cache` | Stable prefix identity, dynamic block changes, cache-affecting differences. |
| `trace` | Recent causal events, tool lifecycle, retry/cache/provider decisions. |
| `sync` | Outbox/ack/degraded/poison/action-needed state. |
| `memory` | Retrieved memories, confidence, conflicts, provenance. |
| `plan` | Plan mode state, blocked mutation policy, pending plan tasks. |
| `safety` | Permission state, sandbox boundary, side-effect policy. |
| `budget` | Token, cost, retry, fanout, and time budget when available. |

## Response contract

An introspection response should be structured:

```text
dimension
status
summary
facts[]
blocked[]
degraded[]
next_actions[]
refs[]
```

Facts should be concise and attributable. Raw logs should not be returned by default.

The resident `reflect` schema accepts a concrete `question` for its default
summary view. For typed options such as `facet`, `depth`, or `horizon`, select
`reflect` with `tool_search(query="select:reflect")`, then call `invoke_tool`
with the selected contract. Selection preserves the resident schema and its
prompt-cache identity. A diagnostic succeeds only when its tool outcome
succeeds; requesting valid parameters alone does not prove observations were
obtained.

Routine self-diagnosis starts with a summary overview (or hint for a quick
check), reusing applicable observations. System guidance, tool descriptions,
and bundled workflows must agree on this default. A concrete evidence gap or
an explicit deep-audit request can justify deeper inspection; no fixed call
quota limits recovery. Snapshot claims must exclude later diagnostic calls,
and completed-turn totals must not be presented as totals for an ongoing turn.

Oversized structured introspection has a distinct bounded model projection;
the full durable report remains unchanged. This projection preserves complete
JSON and supporting evidence identities, prioritizes important observations,
and explicitly lists omitted fields and counts separately from the source
report's budget. It does not split JSON or duplicate the causal graph. Another
live introspection obtains a new snapshot, not a continuation of the old one;
omission alone is not a reason to request more evidence.

## Capability introspection

Capability introspection must distinguish:

- tool does not exist;
- no provider owns the capability;
- provider exists but offline;
- runtime binding missing;
- plan/policy blocks the call;
- argument shape is malformed;
- fallback is available;
- fallback was selected.

The agent should never have to infer these from generic tool errors.

## Context introspection

Context introspection should report:

- which context blocks were loaded;
- why they were loaded;
- what was compacted;
- unresolved constraints;
- memory conflicts;
- artifact references;
- provider/sync state included in prompt.

It should not dump the whole prompt unless explicit debug permission allows it.

## Reflection

Routine reflection defaults to `summary`. `hint` and `summary` return bounded
observations, supporting evidence, and prioritized actions without expanding
the causal graph. Omitted material is reported through the result budget;
retained observations and actions must not contain dangling evidence references.
Explicit `diagnostic` and `forensic` requests retain deeper evidence and graph
inspection. This is progressive disclosure, not a usage quota or tool disablement.
Server-backed and local-journal reflection share the same report projection.

Reflection may produce:

- uncertainty assessment;
- strategy adjustment;
- retry/fallback recommendation;
- request for user clarification;
- risk summary;
- next-step proposal.

Reflection must not directly execute tools, change tasks, alter provider bindings, or approve permissions. It may request those actions through normal lifecycle/capability paths.

## Plan mode

Plan mode should preserve introspection and reflection. Mutating tools may be blocked, but the agent still needs to know:

- what it would do outside plan mode;
- which provider would execute it;
- what approval or state transition is required.

## Prompt-cache interaction

Do not rewrite large system prompt sections to update introspection state. Keep the introspection protocol stable and put dynamic facts in compact blocks or tool responses.

## Test obligations

- Missing provider binding is visible through capability introspection.
- Plan mode reports policy blocks without hiding all tools.
- Edge offline is reported as provider state, not generic failure.
- Compacted context remains explainable.
- Reflection cannot mutate state directly.
- Introspection works in Web without Edge.

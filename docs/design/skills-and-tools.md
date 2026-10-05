# Skills and tools

> Status: target design contract.
> Last updated: 2026-07-19.

Skills and tools define model-facing capabilities. The capability system owns routing and admission; this document defines packaging and user/product semantics.

## Skills

A skill is a packaged capability with:

- instructions;
- examples;
- resources;
- optional tools or MCP bindings;
- input/output contract;
- permission requirements;
- evaluation cases;
- version metadata.

Skill maturity is progressive: prompt-only, structured prompt, tool-backed,
resource-backed, evaluated, then governed. A package must not claim a maturity
level unless its production discovery and activation path demonstrates it.

The unused source-public `SkillLifecycleEvent` enum has been retired. Real
skill invocation hooks and session-event dispatch remain the execution paths;
constructing an event enum never provided telemetry delivery.

## Tools

A tool is a callable schema. Tool visibility and execution are decided by the capability system.

`session(action=config)` accepts a JSON numeric `value`. Retrieval counts
require integers; compression thresholds accept fractional numbers. The
shared governor owns supported paths, ranges and drift limits; schemas and executor
admission must agree with that contract. Numeric strings are rejected.

Resident tools expose a small stable `tools[]` contract. `tool_search` selects
deferred invocation contracts into canonical conversation evidence; it does not
inject their schemas into later `tools[]` requests. Invoke selected tools through
the resident `invoke_tool` carrier and reuse the selection across turns while
its contract and capability remain current. Selection is knowledge, not a
permission grant: request projection and execution still enforce current provider
and policy admission. After compaction removes necessary argument knowledge,
rediscovery is legitimate; do not require or prohibit it merely by turn count.
Ordinary `agent` spawn, status (`list`), result collection (`get_result`), wait (`wait`), and
communication (`send_message`) share a compact resident contract when the
delegation capability is admitted. Its fields and per-action requirements and
allowlists come from the canonical schema; a field valid for one action does
not become valid for every action. The resident spawn contract includes
`agent_type` so the parent can select the appropriate persona directly.
Advanced fields use the canonical contract through `invoke_tool`; when that
contract is not already known, discover it with `tool_search`. Both routes share the same executor and
admission checks. If the final authorized surface omits `tool_search`, the
visible `agent` instead carries its full owner-authorized contract directly;
filtering discovery must not strand child messaging or result retrieval.
The resident projection must remain inside the fixed tool
schema budget, so ordinary delegation does not add a large repeated prompt.
Child completion notifications contain bounded previews, not execution failures
when output is truncated. The executing parent retains the complete terminal
result until settlement; `agent.get_result` reads that retained result before
durable reconciliation. Larger tool output uses the existing authorized
artifact reader when available. Output recovery must not rerun completed work.
`agent.wait` admits a current-run input wait with an optional bounded timeout.
The shared loop waits after sibling tools settle, releases execution capacity,
and applies child results, semantic messages, or user guidance through its
existing input boundary. It does not turn `get_result` into a wait or add a
database poller. Observation timeout preserves child execution; actual run
expiry and cancellation retain their original authority. The wait trace binds
the invoking tool call, and Explain distinguishes timeout from interruption.
Resident `introspect` exposes live facets, Explain selectors and artifact
pagination using the canonical summary/current-turn defaults. Select its full
contract for custom topic, depth, horizon, source policy, context inclusion or
format; these advanced options remain supported without repeating their schemas
in every request.

## Relationship

A skill may require tools, but it does not make those tools available by itself. Provider decision still controls whether a required capability can run in the current session.

## Skill lifecycle

```text
draft -> validated -> published -> activated -> deprecated -> archived
```

Activation may be scoped by user, workspace, agent, or policy.

CLI skill activation uses the registry-backed `$name` path. The retired console
built-in markdown/concise switch and its client-only selection state are removed;
Server and provider skill facts retain their canonical request and trace owners.
The unused session-local automatic `SKILL.md` rewrite proposal chain is retired.
Evaluated, approved tuning and activation remain owned by [tuning jobs](tuning-jobs.md).

The operating workflow is:

```text
author -> validate -> evaluate -> publish -> activate -> observe -> tune -> version
```

Governed skills declare their stable identity and version, required
capabilities, allowed providers, instructions/resources, input/output and
permission contracts, evaluation cases, compatibility, and rollback policy.

## Implementation constraints

- One shared owner parses manifests, resolves discovery paths, and validates
  packages. CLI, server, Web, and providers consume that owner instead of
  maintaining local loaders or compatibility-shaped copies.
- Before adding a registry, parser, provider, or lifecycle state, identify the
  current owner and production callers. A second implementation requires a real
  deployment or authority boundary, not convenience for one caller.
- A replacement migrates callers and removes the superseded implementation and
  its self-only tests in the same change. Temporary dual paths require an owner,
  expiry condition, and convergence test.
- Exported types and parser unit tests do not prove a usable skill. Tests must
  cover discovery, capability admission, activation, required tool/resource
  availability, failure diagnostics, and the user-visible outcome.
- Persistence changes require the real schema, query, transaction, migration,
  and rollback/failure path to be exercised against the supported database.

## Compatibility

Skill updates should declare:

- instruction-only change;
- schema-compatible change;
- schema-breaking change;
- provider requirement change;
- permission change.

Provider or permission changes require stronger review.

## Discovery

Skill discovery should be progressive:

- stable small index in prompt;
- deferred loading for full instructions/resources;
- capability-aware filtering;
- deterministic ordering;
- clear diagnostics for unavailable skill dependencies.

CLI admissions keep fork-skill dispatch on the Server-owned execution. The CLI
retains skill discovery and inline tool callbacks; it does not construct a second
fork-skill executor or invocation ledger.

## Observability

Automatic fork-skill acceptance checks use the shared verification runner and
validate the complete criterion batch before evaluating it. File observers are
confined to the declared verification directory. Command-backed criteria are
not automatically executed: they report a verification failure, including when
nested in a composite. A manifest is not authority to run shell commands on the
Server. Command execution belongs to the ordinary policy-authorized CLI or User
Runner tool provider, with its approval, cancellation and trace boundaries.

Track per skill version: invocation and success/failure counts, tool-call
validity, user correction rate, provider fallback/block rate, token cost, and
regression failures.

## Optional memory candidate judgment pilot

Memory relevance and explicit lesson dismissal use the existing selector boundary.
An optional server admin `judgment_offering_id` binds those judgments to a
registered Offering, including the TypeSafe System One adapter. Offering credentials
remain encrypted and server-owned. This binding never selects a memory extraction
model or changes agent/tool policy. See [the pilot guide](../guides/memory-judgment-pilot.md)
for configuration, fallback and validation limitations.

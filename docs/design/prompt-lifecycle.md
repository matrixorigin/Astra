# Prompt lifecycle

> Status: target design contract.
> Last updated: 2026-07-07.

Prompt lifecycle defines how Astra builds, versions, caches, inspects, and evolves prompts. It is distinct from context selection and tool routing, though it consumes both.

## Ownership

This document owns:

- prompt assembly phases;
- stable prefix and dynamic block boundary;
- prompt versioning;
- prompt-cache strategy;
- prompt introspection metadata;
- prompt evolution boundary.

Context selection belongs to [context-and-prompt.md](context-and-prompt.md). Provider decisions belong to [capability-system.md](capability-system.md).

## Assembly phases

```text
base contract
  -> agent profile
  -> provider/tool protocol
  -> safety policy
  -> stable examples
  -> dynamic run/session/context blocks
  -> tool schemas
  -> user turn
```

## Stable prefix

Stable prefix should contain:

- agent role and invariant behavior;
- provider decision schema;
- tool protocol;
- safety and permission contract;
- trace/introspection contract;
- stable response requirements.

It should not contain volatile provider health, long task lists, raw tool output, or sync counters.

## Dynamic blocks

Dynamic blocks should have stable keys and compact values:

```text
run_state
provider_state
task_projection
sync_state
context_summary
memory_recall
artifact_manifest
recent_trace
```

## Prompt version

A prompt version should identify:

```text
prompt_contract_version
agent_profile_version
skill_versions
safety_policy_version
provider_contract_version
tool_protocol_version
context_schema_version
```

## Prompt cache

Prompt cache goals:

- stable prefix reuse;
- minimal churn from provider state changes;
- deterministic tool ordering;
- compact dynamic state;
- no correctness dependency on cache artifacts.

ForkPrefix is a cache/diagnostic optimization, not restore correctness.

## Prompt introspection

The system should be able to explain:

- which prompt contract was used;
- which dynamic blocks changed;
- which tools were included and why;
- which memories/artifacts were included;
- whether cache should have hit or missed.

## Evolution

Prompt changes go through tuning/evaluation gates when they affect behavior. Emergency safety prompt updates may bypass normal rollout only under explicit policy and must be auditable.

# Agent backbone and capacity providers

> Status: target design contract.
> Last updated: 2026-07-07.

This document defines the current execution philosophy for Web, CLI, Edge, Server, MCP, cloud workspaces, and future tool providers.

It is a normative target. Implementation may be partial during migration.

## Principle

Astra has one agent backbone. Different environments contribute capacity through providers.

```text
Web Agent parity with CLI = same backbone semantics, not same local executor.
```

The Web Agent must not become weaker in context, trace, session stage, introspection, reflection, checkpoint, audit, or failure semantics when Edge is absent. It may have fewer local capabilities because those capabilities are provider-owned.

## Backbone responsibilities

The backbone owns:

- session/run/turn/task state;
- transcript and context assembly;
- prompt-cache stable prompt layout;
- checkpoint, resume, fork, and recovery;
- trace, audit, introspect, and reflect;
- tool protocol and result semantics;
- provider decision;
- safety, permission, consent, retention, and learning lineage.

These responsibilities must not be duplicated by Web, CLI, Edge, or Server implementations.

## Provider responsibilities

A capacity provider owns concrete capability supply:

| Provider | Owns | Does not imply |
| --- | --- | --- |
| Server cloud | cloud-safe platform capabilities, configured `web_fetch`, request-scoped MCP, cloud artifacts, admin/query/report services | default bash, arbitrary host file write, user workspace shell |
| Edge/CLI | user-owned workspace, shell, file, git, local network, local MCP, local browser/session identity | separate agent backbone |
| Request-scoped MCP | explicit remote API-shaped tools | workspace executor semantics |
| Cloud workspace runtime | externally provisioned workspace executor | server-owned scheduler unless explicitly designed |
| Future provider | declared capability under provider contract | implicit admission |

## Provider resolution

Resolution is deterministic and explainable.

1. User/request binding wins. If the user selected an Edge workspace or MCP server, that binding is authoritative unless policy rejects it.
2. Local identity wins over cloud fallback for the same capability. If Edge/CLI and Server both provide `web_fetch`, use Edge/CLI by default.
3. Server fallback is explicit. It requires policy permission and must record fallback reason.
4. Unknown executor-gated capability is denied by default until provider binding is implemented.
5. Projection, admission, and execution must share the same decision.

## Provider decision shape

A provider decision should carry:

```text
capability
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

The decision is emitted once and consumed by the tool surface, admission path, execution path, diagnostics, and audit events.

## Web Agent without Edge

Without Edge, Web Agent still has the backbone:

- durable session/run/turn state;
- transcript and context continuity;
- trace and audit facts;
- checkpoint and resume;
- introspect and reflect;
- server-safe tools and MCP;
- server-configured `web_fetch` when enabled;
- clear diagnostics for unavailable local capabilities.

It does not pretend to have local shell/file/git access unless a provider supplies those capabilities.

## Web Agent with Edge

With Edge, the same Web Agent gains extra provider capacity:

- local workspace file and shell operations;
- user-local network or browser context;
- local MCP servers;
- local permission prompts and policy enforcement;
- durable local journal and sync outbox target.

Edge is an extension of hands, not a second brain.

## Prompt cache rules

Prompt cache requires stable prompt structure:

- Keep provider contract and tool protocol in stable prefix.
- Put provider availability and run-stage details in structured dynamic context.
- Do not rewrite large tool instructions when a provider goes offline.
- Emit provider decisions as compact structured facts.
- Treat ForkPrefix as cache/diagnostic optimization, not restore correctness.

## State mode rules

Plan, cancel, pause, blocked, deleted, archived, and resumed are durable state machine concepts.

- Plan mode is a policy overlay: mutating tools may be blocked, but context/trace/introspection remain complete.
- Cancel should not leave immortal task-board entries.
- Deleted means hidden from active projection, not erased from audit lineage.
- Blocked must include provider/tool/policy reason and resumability information.

## Required invariants

- No surface-specific agent loop should own a different trace or context semantics.
- No server default capacity should expose local executor semantics.
- No tool should be visible without an explainable provider decision.
- No tool should pass admission through a different rule than execution.
- No provider fallback should be silent.
- No dynamic provider state should invalidate the stable prompt prefix unnecessarily.

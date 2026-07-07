# Astra architecture

> Status: target architecture baseline.
> Last updated: 2026-07-07.

Astra is an agent runtime built around one durable agent backbone and multiple capacity providers. It is not a collection of separate Web, CLI, Edge, and Server agents.

This document is normative. It defines the architecture Astra should converge to; a branch may implement only a stage of it.

## Core thesis

```text
Agent behavior = backbone semantics + context state + provider decisions + model output
```

The backbone must be shared across all surfaces:

- session, run, turn, and task lifecycle;
- context assembly and prompt-cache stable prompt structure;
- checkpoint, resume, fork, and recovery;
- trace, audit, introspect, and reflect;
- tool schema projection, tool admission, tool execution result semantics;
- safety, permission, retention, and learning lineage.

Capacity providers add capabilities to the backbone:

- Server cloud provider for safe cloud capabilities such as control-plane operations, cloud artifacts, configured `web_fetch`, request-scoped MCP, metadata queries, and platform services.
- Edge/CLI provider for user-owned local workspace capabilities such as shell, file, git, local browser/network, and local MCP.
- Request-scoped provider for capabilities explicitly bound to a single request or session.
- Future providers such as sandbox Python, remote browser, cloud workspace runtime, private data connector, and enterprise policy tool.

## Non-goals

- Do not create two agent loops for Web and CLI.
- Do not give the server default bash, arbitrary file write, or host executor semantics.
- Do not let UI mode decide runtime semantics.
- Do not encode dynamic runtime state by rewriting large prompt sections.
- Do not hard-stop a run when a narrower degraded state or tool-level block is sufficient.

## Runtime layers

```text
Client surfaces
  Web UI | CLI/TUI | API clients | Edge agent
        |
        v
Agent backbone
  session/run/turn/task lifecycle
  context assembly and prompt cache layout
  checkpoint/resume/fork
  trace/audit/introspect/reflect
  provider decision and tool lifecycle
        |
        v
Capacity providers
  server cloud | edge/cli local | MCP | request-scoped | cloud workspace
        |
        v
State and facts
  C0 control | C1 transcript | C2 audit facts | C3 trace facts | C4 debug bundle | C5 learning artifacts
```

## Provider decision

Every capability exposed to the model must come from a provider decision. The decision is the single source for:

- tool visibility;
- tool admission;
- execution route;
- fallback eligibility;
- LLM-visible diagnostic;
- audit and trace event fields.

A provider decision must include at least:

- capability and tool name;
- provider type and provider id;
- execution owner;
- route;
- admission status;
- runtime binding status;
- fallback policy;
- degraded/offline reason;
- trace fields.

If Edge/CLI and Server both provide the same capability, the user-bound Edge/CLI provider wins by default. Server fallback is allowed only when policy permits it and must be recorded as a fallback decision.

## State layers

| Layer | Purpose | Examples |
| --- | --- | --- |
| C0 control | Durable runtime control state | sessions, runs, tasks, checkpoints, leases, execution slots |
| C1 transcript | User-visible conversation state | messages, assistant output, transcript items |
| C2 audit facts | Durable facts for audit and replay | agent events, run events, permission decisions |
| C3 trace facts | Structured execution trace | model rounds, tool lifecycle, retry/cache/provider/sync decisions |
| C4 debug bundle | Explicit short-lived diagnostics | raw captures, manifests, downloadable support bundles |
| C5 learning artifacts | Consent-gated derived training/eval data | redacted examples, eval labels, quality signals |

C0-C3 are normal runtime facts. C4 is off by default and short-lived. C5 must be derived through consent, redaction, quality gate, lineage, and deletion propagation.

## Tool lifecycle

The tool lifecycle has four phases:

1. Projection: build the model-visible tool surface from provider decisions.
2. Admission: decide whether a concrete tool call is allowed now.
3. Execution: route the admitted call to the selected provider.
4. Result: persist result, failure, fallback, and degraded state in trace/audit facts.

Projection, admission, and execution must not each reimplement their own provider logic.

## Prompt and context lifecycle

Prompt-cache stability is achieved by stable structure, not by hiding runtime truth.

- Stable system rules, tool protocol, provider contract, and trace schema belong in the stable prefix.
- Dynamic state belongs in compact structured context blocks with stable keys.
- Provider online/offline state should change provider decisions, not rewrite large prompt text.
- Restore correctness depends on checkpoint, transcript, event log, and artifact manifest; ForkPrefix is cache/diagnostic optimization only.

## Failure semantics

Astra should prefer precise degradation over broad interruption.

- Missing provider binding blocks the affected tool or capability, not the entire backbone.
- Offline Edge should produce a provider-offline decision and recovery hint.
- Malformed trace/event data should be isolated and reported without poisoning the session.
- Cancel, pause, plan, blocked, deleted, and archived are durable state machine states, not UI-only flags.
- A user-visible stop must include structured reason and resumability information.

## Required evolution areas

The architecture is not complete until these areas are true as system properties:

- fully unified provider decision across schema projection, admission, and execution;
- Edge durable sync outbox with ack watermark, retry, poison isolation, and repair UX;
- normalized C3 events for provider, step, retry, cache, and sync decisions;
- agent event retention/archive and poison semantics;
- debug bundle product lifecycle;
- learning pipeline with consent, redaction, quality, lineage, and deletion propagation.

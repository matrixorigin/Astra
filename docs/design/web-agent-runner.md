# Web Agent runner

> Status: target design contract.
> Last updated: 2026-07-07.

The Web Agent runner is the Web surface over the shared agent backbone. It owns Web-specific session UX and streaming behavior, not separate agent semantics.

## Ownership

This document owns:

- Web session/run stream behavior;
- Web provider selection UX;
- Web-visible blocked/degraded states;
- browser disconnect/reconnect behavior;
- Web projection of tasks, sync, artifacts, and provider state.

It does not own:

- provider priority and admission, owned by [capability-system.md](capability-system.md);
- local runtime authority, owned by [edge-runtime-tool-boundary.md](edge-runtime-tool-boundary.md);
- lifecycle state machine, owned by [runtime-lifecycle.md](runtime-lifecycle.md);
- context/prompt semantics, owned by [context-and-prompt.md](context-and-prompt.md).

## Goals

- Preserve the same session/run/turn lifecycle as CLI and Edge.
- Preserve context, trace, introspect, reflect, checkpoint, resume, and audit semantics.
- Route tools through explicit capacity providers.
- Support Web-only operation through server-safe and request-scoped providers.
- Expand capability when Edge/CLI or cloud workspace providers are connected.

## Non-goals

- The server does not provide default bash, arbitrary file writes, git mutation, or host executor access.
- Web does not maintain a separate context pipeline from CLI.
- Web does not silently fall back to server tools when a user-bound Edge provider is offline unless policy allows fallback.
- Web UI state is not the source of truth for run/task/session state.

## Web-only operation

Without Edge, Web Agent still has:

- durable session/run/turn/task state;
- transcript and context continuity;
- trace and audit facts;
- checkpoint and resume;
- introspect and reflect;
- server-safe tools and request-scoped MCP;
- server-configured `web_fetch` when enabled;
- clear diagnostics for unavailable local capabilities.

It does not pretend to have local shell/file/git authority.

## Web with Edge or workspace provider

With a provider binding, Web can surface additional capacity:

- local workspace file tools;
- shell/git execution;
- local MCP;
- user-local browser/network context;
- cloud workspace runtime tools;
- local permission prompts through provider UX.

The UI should present this as provider capacity, not as a different agent mode.

## Stream behavior

The Web stream should carry:

- session/run identity;
- transcript deltas;
- tool call/result events;
- provider blocked/degraded events;
- task projection changes;
- artifact metadata;
- terminal outcome.

Malformed or out-of-order non-critical stream events should be isolated where possible. Critical identity or state corruption should fail closed with a structured error.

## Browser disconnect

Browser disconnect is not the same as run cancellation.

- If the user explicitly cancels, transition run state.
- If the network drops, preserve durable run state and allow reconnect.
- If backend dispatch is blocked, surface blocked provider state.
- If a stream cursor is malformed, do not corrupt active run projection.

## Web-visible diagnostics

A Web user should see:

- provider offline/reconnect required;
- capability blocked by policy;
- tool unavailable because no provider binding exists;
- fallback selected;
- sync degraded/action needed;
- whether the run can continue, retry, or needs user action.

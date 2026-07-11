# Client surfaces and deployment

> Status: target design contract.
> Last updated: 2026-07-07.

Client surfaces and deployment owns Web, CLI, TUI, Edge process, API clients, and deployment topology boundaries. It does not own agent semantics.

## Client surfaces

| Surface | Responsibility |
| --- | --- |
| Web | Multi-device UI, streamed run projection, provider selection, task/status display. |
| CLI/TUI | Local interactive interface, local provider control, terminal permission UX. |
| Edge agent | User-owned provider process for workspace/local capabilities. |
| API clients | Programmatic access to sessions, runs, events, and provider bindings. |

All surfaces consume the same backbone state and projections.

## Deployment responsibilities

Deployment may provide:

- cloud API server;
- MatrixOne/state store;
- artifact storage;
- queue/workers;
- Edge connectivity service;
- optional managed workspace runtime;
- observability stack.

Astra runtime server does not implicitly become a Kubernetes scheduler or a local executor just because it is deployed in cloud.

## Web integration

Web integrations should use runtime contracts, not private implementation assumptions:

- session/run APIs;
- SSE or stream events;
- provider selection APIs;
- task projection;
- artifact metadata/download;
- sync/provider status;
- auth and workspace authority.

## TUI/CLI

CLI/TUI owns local interactive ergonomics but not separate agent semantics. It should expose:

- provider health;
- permission prompts;
- sync status;
- task projection;
- local diagnostics;
- reconnect/resume.

## UI projection rules

- UI displays durable projection, not private local cache as truth.
- Task board is derived from task state.
- Sync state is derived from outbox/ack/degraded facts.
- Provider state is derived from provider decisions and health.
- Cancel/delete/archive must round-trip through durable state.

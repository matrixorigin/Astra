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

### Agent binding addressing

Agent Binding APIs and chat requests remain authenticated. Registration
idempotency is scoped to the authenticated registrant; subsequent read, chat
resolution and disable operations address the binding by ID and do not re-match
the caller's user or principal scope. Product authorization remains the
integrating application's responsibility. Session/run ownership and data/tool
authorization are unchanged. This is a transitional addressing contract; the
complete contract persists the registering provider and requires both provider
identity and binding ID for lookup, runtime use and disable operations.

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

The startup card reserves terminal width before styling and clips text by Unicode
display cells. Native MOI login uses a short `MOI` display label, not its internal
credential profile identifier. Narrow or short terminals use a static presentation;
animated frames must not wrap and invalidate cursor-up row accounting.

Inline terminal resize reconciles the viewport with the terminal's cursor
position before clearing and repainting. The existing crossterm input owner
pauses its reusable event stream with an acknowledged worker handoff for a
bounded cursor query on the blocking pool and preserves unrelated input. A
size watchdog recovers missed resize signals, and invalid cursor coordinates
use the missing-reply fallback. Cursor replies are associated with the queried dimensions; intervening
resizes invalidate them. Scheduled frames wait for this reconciliation and
cannot advance the remembered screen size; viewport growth erases
transient UI before scrolling. Resize must preserve native history and must
not purge scrollback. Terminals that do not answer cursor queries fall back to
height-clamping corrections; width-reflow recovery requires a cursor reply.

## MOI-managed local client updates

MOI-managed client distributions opt into `moi-client-update-v1` with an
executable-relative `installation.json` marker. Standalone Astra and
image-managed Edge deployments do not opt in. `astra update` delegates to the
paired moi-cli in the same immutable release directory; distribution metadata,
downloads, installation, and recovery have one owner in MOI, not a second Rust
updater. The command runs before application configuration or authentication.

CLI and local Edge retain a shared `runtime.lock` file lock for the process
lifetime. The updater needs an exclusive lock, never kills clients, and cannot
switch while a consumer is admitted. Under the lock, clients reject an
unfinished installation transaction or an executable no longer selected by
`current`. Protocol/version probes are offline; update checks never read UC,
Memoria, Genesis, or provider credentials. TUI startup may show cached notices
and launch a bounded anonymous metadata check; machine/helper commands remain
quiet. Hosted Runner updates remain image deployment operations.

Cached notices distinguish an installable update from
`CLI_UPDATE_COMPATIBILITY_CHANGE`: the latter advertises a new release that
requires a separate installation prefix and Skill search root, preserving the
existing installation and login data. It is not eligible for in-place update.
Notice renderers only display validated bundle identifiers and known reason
messages. Explicit update/check delegates bounded check-lock waiting to MOI;
runtime occupancy continues to fail immediately. Missing derived Skill copies
and same-prefix reinstall recovery are owned by MOI under its exclusive lock,
not by Rust startup or authentication code.

## UI projection rules

- UI displays durable projection, not private local cache as truth.
- Task board is derived from task state.
- Sync state is derived from outbox/ack/degraded facts.
- Provider state is derived from provider decisions and health.
- Cancel/delete/archive must round-trip through durable state.

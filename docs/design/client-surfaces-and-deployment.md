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

Transcript clients consume ordered items, stable source event identities,
`next_before_seq`, and `has_more`. Root conversation, run, and session scopes
remain distinct. Responses contain no physical page references or page hashes;
local fallback selection compares actual conversation coverage. Work views use
the committed transcript cursor and item commitment fields to assess publication.

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


A CLI turn consumes one Server-owned SSE execution stream. Edge callbacks run
inside that stream; their completion never grants the client another model
round. The client uses shared ingestion, context trace and continuity checkpoint
projections without constructing the runtime execution-loop state. Provider
recovery, action admission and completion policy remain with the Server.

Process-local runtime notifications remain with the turn's local control owner
until successful durable settlement. Authentication and session retries resend
uncommitted facts; notifications arriving after request admission remain queued.
Durable user guidance keeps its Server acceptance and disposition protocol.

The TUI composer exposes Enter for ordinary submission and Ctrl+Q for explicit
next-turn conversational input. The latter stays in the existing client-owned
FIFO and never becomes current-run guidance. Enqueueing alone does not close
guidance admission. During a run, ordinary messages stay in the existing
pending-intent list until that turn binds its Run, then are submitted in order.
An uncertain delivery is not retried as a new message, and unsent messages
remain editable if the turn cannot hand them off. The ordinary lifecycle
handoff, modal/draft gates, session-change restoration, and guidance custody
rules remain authoritative. Local controls retain their dispatcher semantics.
The agent navigator's message action targets the selected agent. A previous
message still being sent keeps a later draft editable; if its delivery is
unknown, the user may explicitly send the later message despite possible
reordering. A message the agent did not use stays available in the current
client as a target-scoped draft; Alt+U opens it for editing and retry, copying,
or explicit discard. An unconfirmed message is instead marked as possibly
delivered and is never retried automatically. These drafts stay with the same
account and session; they are neither sent to the main conversation nor
claimed durable.
Child guidance receipts normally arrive on the existing parent live channel.
If a receipt is delayed, the client performs one owner- and session-scoped
lookup by intent identity; it does not attach another Run stream or replay its
history. An accepted receipt remains pending if the child is paused or has
not yet processed the message. Reconnection or a failed lookup never turns an
unknown delivery into permission to resend it automatically.
A `/model` selection may be made while a turn is running. The active Run
rechecks the selected Offering and applies it at the next model-round
boundary it observes. The interface reports the request as pending until the
Run confirms application; an already started model request or tool continues
unchanged. Rejected or unconfirmed changes are not presented as applied.
The selected model is also used by this client for later turns, including
queued messages. `/model clear` removes the client's override for later turns.
Permission changes likewise remain visibly requested until the active Run
reports that its execution boundary applied them.
During an active turn, `/allow` and `/allow read_only` use that same permission
control, while the `/plan` workflow command waits for the turn to finish.
Other commands that need the mutable Session are queued with explicit feedback.
Live controls use the credentials admitted for the physical run. A static
gateway bearer is never replaced or refreshed using an unrelated saved profile.
Local notices and catalog errors remain visible during execution without
finishing an in-flight tool; live transcript deltas reuse unchanged history.
A typed client wake rechecks owner/Session attachment and input gates before
consuming one FIFO entry. Input remains in the FIFO until consumption; terminal
paste protection and popup handling apply to user gestures independently.
A guarded paste-tail Enter on an empty composer is consumed without creating a
new draft; explicit newlines and nonempty drafts retain their handoff gates.
This queue has process-local custody, not durable Server acceptance or recovery
across client restart; it does not add a runtime delivery class or scheduler.

The startup card reserves terminal width before styling and clips text by Unicode
display cells. Native MOI login uses a short `MOI` display label, not its internal
credential profile identifier. Narrow or short terminals use a static presentation;
animated frames must not wrap and invalidate cursor-up row accounting.

Mutable assistant replies use a bounded Markdown tail in both the main viewport
and the Ctrl+O transcript. When the byte window or rendered-row budget omits earlier
text, a preview-only notice occupies one row within that same budget; its wording
shortens to fit narrow terminals. The complete received source remains unchanged.
Once output settles, the complete reply becomes available in the transcript and
is queued for native scrollback through the existing background layout and bounded
drain, independently of the statistics footer. A still-live tool prevents premature
settlement. The notice does not imply a model context limit, and Ctrl+O does not
provide the complete reply while it is still streaming.

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

The agent navigator and typed run transcript are the active inspection surfaces.
The disconnected task-detail overlay and its TaskCell refresh hooks are retired;
local runtime snapshots update navigator status, while typed live events update
run conversations. An unused standalone plan spinner is also retired; active
terminal progress indicators keep their existing owners.

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
Memoria, Genesis, or provider credentials. TUI startup shows cached notices
below the composer/status strip and launches a bounded anonymous metadata check.
Its completion refreshes the notice in the same TUI session, including while a
turn is running. Notices remain visible in the compact chat surface until the
user exits (modal views keep their existing layout); narrow terminals wrap the
text. They never enter conversation history. Installable releases say
`Update available (<bundle>) · Exit Astra, then run astra update to upgrade.`
The paired moi-cli owns check throttling; this is a startup check, not periodic
polling or automatic installation. Machine/helper commands remain quiet.
Hosted Runner updates remain image deployment operations.

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

CLI skill discovery uses the current unified catalog. Skill execution outcomes
and quality ranking remain Server-owned; CLI turns do not load a separate
quality tracker or mirror discovery into session state. Existing workspace
skill history remains an informational field for self-inspection and is retained
when recovery and subsequent turn commits update other workspace facts.

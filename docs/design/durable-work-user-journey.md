# Durable Work across surfaces

> Status: target design contract; current support and verification limits are
> described in the final section.
> Last updated: 2026-09-16.

This document owns the user-visible journey for finding, observing, continuing,
and changing the execution provider for one durable Work across TUI, Web, and
Edge. Runtime lifecycle, provider routing, and synchronization mechanics remain
owned by their canonical contracts linked below.

## First principles

- The user follows an outcome. A Work is the stable thing they name and reopen;
  runtime ids are implementation details unless the user needs them to recover.
- A Work has canonical server facts and branch-specific conversation histories.
  Changing device or surface must not create a shadow Work or silently fork a
  branch's Session.
- Reading shared state does not grant control. Many clients may observe a Work;
  an authorized controller may submit through the existing Session/Run
  admission. The single-writer scope is the canonical Session/branch contract,
  not automatically the entire Work.
- Session control handoff, Edge provider selection, and Server Run-owner
  recovery are three independent transitions. Each has a different authority,
  durable state, and failure boundary.
- The Server owns the agent loop and canonical Session/Run lifecycle. An Edge
  process provides local capabilities; its connection generation only fences
  that connection. It is not the same identity as a logical workspace or the
  selected provider binding. Replacing an Edge must not create a new Run or
  imply moving the Server-owned agent loop.
- Execution capacity is replaceable; local workspace state is not magically
  portable. A provider change must prove what code and pending effects moved.
- A Git commit identifies committed repository content. It does not identify the
  conversation cursor, checkpoint, dirty files, credentials, or pending tool calls.
- A stable idempotency key deduplicates an admitted logical request; it cannot
  make arbitrary shell or remote side effects exactly-once. Ambiguous effects
  remain unresolved until the invocation ledger proves whether retry is safe.
- Failures must preserve the last proven state and say what action can recover
  it. A disconnected Edge, browser, or stream never proves completion or
  cancellation.
- Multi-user scale means bounded work and strict owner isolation. Sharing one
  user's Work with another principal requires an explicit membership contract;
  matching ids or URLs never grant access.

## The user's journey

## Delivery boundary

The supported boundary is a reliable read-and-continue journey plus a provider
switch between settled Runs. A TUI-created Work is discoverable in Web, remote
Run/activity freshness is explicit, branch-controller takeover is separate from
Edge movement, and an Edge switch requires a clean, pre-materialized target
workspace. The provider-switch path does not migrate an active Run or
automatically copy workspace changes. Automatic patch materialization and
Server Run-owner recovery require separate contracts and release gates.

### 1. Start or promote once

Web and TUI create Work through the same Server Work service. Web may create a
new Work. TUI may promote its current idle durable Session, preserving that
Session instead of creating a hidden second conversation. If TUI has no durable
Session yet, it explains the one prerequisite and offers the next action.

On success, both surfaces show the same `work_id` and selected `branch_id`.
TUI should expose a direct Web link when the profile has a configured Web origin;
otherwise it shows the Work id and the Web `Now` entry point. The public link
must not expose the opaque Session id.

### 2. Observe from any authorized device

The Work list is owner-scoped, recent-first, and paginated. Opening an item
shows the canonical goal, current branch, branch-specific conversation, task
graph, recent activity, run state, last update time, and execution/sync health
from bounded Server projections. Readers attach read-only and do not acquire
controller authority or the Run writer lease.

The page distinguishes `updated just now`, a known refresh delay, and stale or
unavailable data. It never presents a local cache as current truth. Opening a
Work from a second device must preserve its identity and branch; it is not a
fork unless the user explicitly chooses a fork action.

### 3. Continue safely

A reader becomes a controller only through the existing deliberate Work branch
control flow. That controller can then submit through the canonical Session/Run
admission. A competing controller receives a typed conflict and a clear action
such as refreshing or explicitly taking control. Concurrent readers remain
usable. Duplicate admission requests reuse an idempotency identity; unresolved
external effects are reconciled through the invocation ledger before any retry
that could repeat them.

### 4. Continue on another Edge

The product action is `Continue on another Edge`. It is separate from the
existing `Take control here` action, which transfers the client controller for
a Work branch. An Edge switch keeps the same Work, branch, and Server Session
identities while selecting a different provider for future local capabilities.

The first supported version changes Edge **between Runs**. The existing Run
must reach a terminal state and its side-effect obligations must be settled, or
the user must explicitly stop it and resolve any ambiguous invocation. A
terminal Run alone does not prove its external effects are settled. The Server
remains the agent and Session owner; the new Edge does not restore or own an
in-flight model loop. The next Work turn uses the same Server-owned Session,
branch, and task graph. Cross-Edge automatic recovery of an active Run is a
separate, later capability.

The first release supports **Edge-to-Edge** transfer only. The current source
must already be an authenticated first-party, unscoped Edge, and the target
must have a verified, clean materialization of the same logical workspace,
repository identity, ref, and committed tree. A Server binding cannot be
converted through this handoff action because there is no source Edge
materialization or attestation to prove. Provider-scoped Edge
registrations belong to the provider-authorized runtime path and are not exposed
as native Work handoff targets because their scope is request authority. The
user commits or saves changes on the source Edge before switching; Astra does
not claim that Git carries untracked files, credentials, environment, or pending
effects. Preflight checks owner/workspace authority, target connectivity,
required capabilities, and exact target tree. Automatic patch/commit transfer
is a separate capability with its own UX and tests.

The binding change needs one durable linearization point: after preflight, the
Server compare-and-swaps the selected Session/workspace provider binding from
its expected generation to the new generation. New Run admission and Edge tool
dispatch validate that generation, so they cannot race past the switch. Only
facts captured for this Work's relevant invocations and workspace revision
must be acknowledged; a busy shared Edge must not block on unrelated Sessions.
The prior binding is fenced for this selected Session/workspace scope, while
the same Edge may continue serving unrelated Sessions. Before the binding
commit, an abandoned preflight leaves the current provider unchanged. There is
no user-visible execution-switch cancel endpoint after the switch request is
admitted: a browser or Edge disconnect leaves the durable operation to recover
or retry. After commit, cancellation may block future execution, but it does
not restore the old binding. The target must confirm the logical workspace and
tree before a new Run is admitted.

Reuse the existing provider, workspace, Run, invocation, and Work branch
control ownership contracts; do not create a second Work-level or per-UI writer
lease. A transfer is complete only when the target confirms the new binding
generation and expected tree. If preflight fails, the current provider remains
authoritative. If the binding has advanced and target validation fails, the old
Edge remains fenced for this binding and Work is visible as `Needs attention`;
do not silently roll back or replay tools. Retrying uses the same transfer
request identity and reconciles invocation outcomes before any side effect can
run again.

The user-visible states are:

| State | User sees | Safe action |
| --- | --- | --- |
| Ready | Current provider and last verified workspace revision | Continue or choose a target |
| Run in progress | This Run must finish, or be explicitly stopped, before switching | Wait or stop this Run |
| Checking target | Identity, capabilities, repository, and workspace are being checked | Leave and retry; no durable provider change has been committed |
| Source preparation required | Commit changes on the current Edge and prepare a clean matching workspace on the target, then retry | Keep the current provider; do not start another Run |
| Switching | The provider binding generation is changing between Run generations | Wait for target confirmation |
| Ready on target | New provider binding and workspace hash are confirmed | Continue on this Work |
| Needs attention | Exact failed step, last confirmed provider, and saved evidence | Reconnect, retry, or resume manually |

## Ownership and identifiers

| Object | Stable identity | Authority |
| --- | --- | --- |
| Work | `work_id` | Server owns goal, criteria, branches, task graph, and Work events |
| Branch | `work_id + branch_id` | Server owns branch revisions and delivery selection |
| Session | opaque `session_id` | Server lifecycle owns transcript/context continuity; CLI may retain a local journal/cache |
| Run | `run_id + owner_generation` | Run lifecycle owns status, lease, checkpoint, pending obligations, and outcome |
| Controller/attachment | `attachment_id + controller basis` | Work branch control and Session handoff authorize a client to submit; a read attachment grants no control |
| Edge process | `registry_id + connection generation` | Edge registry owns connection liveness and advertised capabilities; `executor_id` is a selectable label |
| Provider binding | `Session/workspace + binding generation` | Canonical selection authorizes provider use at Run admission and invocation dispatch |
| Workspace materialization | persisted `materialization_id` plus authenticated canonical checkout root (bounded physical claim) and repo identity/ref/tree hash | The local Edge state identity survives reconnects and Edge-label changes, while independently materialized devices can use the same path without sharing a claim; a matching hash proves committed tree identity, not environment or external effects |

Client URLs and normal user-facing controls identify Work and branch. Session
and Run ids remain available in diagnostics and repair surfaces, not as the
primary navigation model.

## Performance and isolation contract

Initial reads remain bounded and revision-pinned: paginate the Work catalog,
load only the first task-graph page for first paint, and fetch later pages on
demand. Activity, transcript, branch control, provider health, and task graph
may advance on separate durable cursors/revisions; the UI must label freshness
per projection and recover retention gaps with a bounded snapshot. They must
not scan all events or rebuild the entire Work view for every task update. The
owner-scoped Server activity read discovers Runs started by other surfaces; a
local Web composer flag is not evidence that a Run is active. Each visible Work
detail page polls one exact branch about every 1.2 seconds so a quiet page can
discover a Run started elsewhere. The latest `/now` page refreshes its first
20-entry keyset page about every 10 seconds; older pages do not poll. Task graph
refresh remains bounded to its first page while a Run can change it. Hidden tabs
pause refresh. Reconnect resumes with bounded backoff and jitter, ignores
obsolete branch results, and never overlaps polls within a page.

The initial scale acceptance profile is 25 independent owner identities, four
Sessions per owner, 100 concurrent Work readers, and 25 actively changing Work
views. Candidate targets on a declared reference deployment are p95 first
projection under 1 second, p95 active update visibility under 2 seconds, and
p95 Work catalog reads under 500 ms. These are unmeasured targets until a load
lane records database, server, browser, and Edge counts plus dataset sizes.
Two-second polling cannot establish a sub-two-second commit-to-render target;
active discovery needs a change feed or a measured polling budget with latency
headroom. Query count must remain bounded by pages and active Work views, not by
total historical events or task count. Passing this profile is a tested
baseline, not a claim of unbounded capacity. At 100 readers, 1.2-second branch
discovery adds about 80 indexed activity reads per second before task-graph and
catalog refreshes. This is a workload estimate, not a capacity result; the load
lane must record connection-pool waits and query latency. Deployments above this
profile need measured fanout or event delivery instead of assuming the polling
rate is free.

The reproducible read lane is
`scripts/load/work_surface_capacity_probe.py --profile cross-surface-100`. It
uses Work and branch identifier templates plus one access token per owner,
records bounded projection latency and response sizes, and can pair every read
with a foreign-owner request that must return not-found. Run it against the
same deployment window as database, API, browser, and Edge metrics; a dry run
prints the 25-owner/four-Session mapping without contacting the service.

Each authenticated read and mutation is scoped by the resolved principal and
workspace authority. A foreign owner receives the same not-found behavior as a
missing Work. Runs in different Sessions must not share cursors, attachments,
leases, idempotency keys, or provider bindings. Independent Sessions may
progress concurrently. If they share a physical checkout, its workspace
mutation boundary must still serialize conflicting mutations or require
separate worktrees; independent Session leases alone do not make a shared
workspace safe.

## Unhappy paths

| Failure | Required behavior |
| --- | --- |
| No durable TUI Session | Explain the prerequisite; do not create a hidden replacement Session |
| Work creation retry or timeout | Resolve by idempotency identity and show the same Work, or an honest unknown result |
| Active Run blocks promotion | Return a typed conflict; preserve the original Session and allow retry after it settles |
| Missing/foreign Work or branch | Return not-found without disclosing whether another owner has it |
| Concurrent write | Preserve readers; use branch controller and Session admission contracts, and make the loser retryable |
| Stale controller after handoff | Fence its next mutation and keep its read view available when authorized |
| Web stream or browser disconnect | Keep execution unchanged; reconnect from a durable cursor |
| Target Edge offline or under-capable | Do not change provider; name the missing capability or reconnect action |
| Target Edge has no authenticated materialization identity/root | Refuse the switch and request a fresh authenticated registration; hostname, executor labels, and registry row ids are not proof of physical ownership |
| Dirty, untracked, or mismatched workspace | Reject the initial Edge switch; require a clean verified target tree and explain what must be re-created |
| Source Edge lost during a tool call | Preserve ambiguous side effects as unresolved; do not replay until the invocation ledger proves retry safety |
| Source stops before binding commit | Keep the old binding authoritative or show the last confirmed provider/revision |
| Target validation fails after binding commit | Keep the prior Edge fenced for this binding, preserve evidence, and show `Needs attention` |
| Disconnect or cancel races with transfer | Before binding commit, an abandoned preflight leaves the current provider unchanged; after commit, keep the new binding and admit no execution until resumed |
| Transfer races with a new Run | Serialize at canonical admission or binding compare-and-swap; exactly one transition wins |
| Different Sessions share a checkout | Serialize conflicting workspace mutations or require isolated worktrees |
| DB or event service degraded | Show last confirmed revision and staleness; retry with bounded backoff |
| Duplicate or reordered updates | Reconcile by durable cursor/revision; never regress status or synthesize twice |

## Verification gates

- Unit/property tests prove controller and provider-binding generation fencing,
  monotonic projections, idempotent operation recovery, cancellation boundaries,
  and conservative treatment of ambiguous side effects.
- Runtime/database integration tests cover exact Session preservation during
  TUI promotion, multiple read attachments, competing controllers, stale
  controller submissions, duplicate starts, owner isolation on every operation,
  and multiple independent Sessions progressing concurrently.
- A cross-surface E2E starts Work through the TUI public entrypoint, opens the
  same `work_id` in Web, observes Run discovery plus task/activity changes from
  their authoritative revisions, and continues without creating another
  Session or Work.
- An Edge handoff E2E switches between settled Runs to a pre-materialized clean
  target, rejects dirty or mismatched workspaces, and races switching against
  Run admission, cancellation, target loss, capability revocation, duplicate
  retries, and ambiguous invocation outcomes. It proves there is never a period
  with two admitted writers for the selected workspace binding.
- A load lane runs the minimum scale profile above while checking p95/p99
  latency, query/page counts, stream reconnect cost, and tenant isolation.
  Live database and real Edge tests stay in opt-in lanes; deterministic offline
  contract tests remain required in fork CI.

## Current implementation audit

- TUI `/work start` promotes its current durable Session through the Server Work
  binding API. It rejects a missing Session, reuses the Session on exact retry,
  and prints the Work id, `Ctrl+T` task-board hint, and the Web `/now` entry
  point. There is no configured Web deep link yet.
- TUI `/work execution` reads the current Work binding, authoritative provider
  generation, and a bounded target directory without blocking the render loop.
  It is a read-only diagnostic surface; provider switching and retry are
  initiated from the Web Work card (or an equivalent explicit API client).
  It captures the session identity at submission, cancels stale reads when a
  session is rebound, and keeps target-directory failure separate from the
  current placement so the user can still diagnose the next write.
- Web `/now` lists the first 20 Server Works and refreshes that bounded page
  while visible. Opening a Work loads bounded, revision-pinned Server
  projections, attaches a read-only branch view, and reads the selected
  branch's authoritative activity. Visible Work pages discover remote Run
  activity with an owner-scoped single-branch read about every 1.2 seconds;
  active task graphs refresh their first bounded page every 2 seconds even for
  TUI-originated Runs. Both
  loops pause in hidden tabs, add jitter, back off after errors, and avoid
  overlapping requests. Load targets remain unmeasured.
- Web chat history imports Server sessions tagged `source=web_v1`; a TUI Session
  is not automatically inserted into the Web chat list. The Work page is the
  current cross-surface entry point.
- Work now has a durable execution-selection record keyed by isolation domain,
  owner, Session, and branch. Authorized admission pins the canonical Server
  sandbox or records an authenticated Edge placement. A read never creates or
  rewrites the binding: an uninitialized Work reports that state and becomes
  initialized when its first provider is admitted. API callers cannot choose or
  override an existing selection.
  Run admission checks the selected generation while holding the canonical
  Session authority, and tool dispatch checks it again in the same transaction
  as Run action admission and the invocation claim. A selection change is
  rejected while writer/reservation authority, a Run slot, or a prepared,
  dispatched, or outcome-unknown invocation remains active. Exact composite-key
  reads and the existing owner/Session/state invocation index keep coordination
  scoped to the affected Session; dispatch does not lock the selection row, so
  the binding fence adds no serialization to parallel tool fan-out. Live
  MatrixOne coverage exists for owner/Session isolation, stale generations,
  switching-state admission, and busy-switch rejection, but this environment
  cannot execute those database tests. Dispatch verifies an existing physical
  workspace claim with a non-locking read; claim insertion and repair stay on
  binding admission, so parallel tool fan-out does not add a write/lock to the
  hot path. A physical identity is derived from the persisted materialization
  identity and authenticated canonical checkout root; hostname, executor
  labels, and connection registry ids are never used as a checkout fallback.
  The identity file lives in Edge local state rather than the repository, so
  attestation does not manufacture a dirty workspace.
  A checkout claim is an active single-writer fence rather than a permanent
  Session lock: an idle owner can be transferred atomically to a fresh Session
  after its live execution slot and unresolved invocation records are clear.
  Abandoned running slots are reclaimable only after both the owner lease and
  the shared stale window expire; manually paused or unresolved work remains
  fenced until an explicit recovery action settles it. A fresh Session is
  never redirected to `/resume` another Session merely because it shares the
  same checkout.
- Recovery points now have one shared typed manifest for Work, branch, Session
  cursor/context head, Run frontier, execution binding, Workspace snapshot,
  Artifact references, and environment requirements. The Workspace manifest
  validates the capture declaration shape, matching fingerprints, canonical
  paths, content aggregates, file/blob digests, symlink boundaries, and
  MatrixOne Git4Data source references. A server-authored safe-boundary
  publisher builds the manifest from canonical Work/Session facts, rejects an
  active writer, reservation, Run, or changing provider, and publishes it only
  after a locked re-check. Exact request retries are stable and changed request
  bodies conflict. `GET` recovery-point views expose a server-derived
  assessment: conversation and Work state can be verified while the current
  stage explicitly reports `workspace_not_captured`; published does not mean
  portable or automatically restorable. The lower-level preparing capture is
  retained for staging and is never treated as a published point. Recovery
  points are removed with branch cleanup. This is still not cross-Edge
  migration, workspace content restore, historical rollback, or Server
  Run-owner recovery.
- Work branch-control operations and Session handoff already implement
  authorized client-controller transfer with fencing and effect sealing. The
  Web force-takeover copy currently says `Moving this Work here`, which can be
  mistaken for Edge or Run-owner transfer; the UI must name it as taking control
  on this device.
- Work creation/promotion is owner-scoped and idempotent; foreign-owner reads
  return not-found. Runtime DB tests cover preserving the existing Session,
  multiple read attachments, and exactly one winner among concurrent writers.
  The public Work execution/turn regression also proves that a persisted Edge
  binding is carried into Web admission without exposing the internal Session
  id; it does not claim a live Edge relay or socket dispatch.
- Edge registration, capability advertisement, connected-provider routing, and
  the Edge-to-Edge switch API/Web flow exist. The switch is fail-closed when
  either materialization lacks an authenticated identity/root, when the source or
  target attestation changes, or when another Session claims the same physical
  checkout. Live database and real Edge cross-surface tests remain opt-in; the
  checked-in offline contracts cover request identity, target bounds, stale
  generations, and the TUI read path. A Server binding has no source Edge
  materialization, so Server-to-Edge conversion is outside this handoff
  contract. Automatic Server Run-owner recovery is still not a production
  consumer.

The current implementation status and owning contracts are tracked in the
[runtime lifecycle](runtime-lifecycle.md), [durable runs](durable-agent-runs.md),
[Edge-cloud execution](edge-cloud-execution.md), and
[client surfaces](client-surfaces-and-deployment.md). The between-Run Edge
provider transfer is implemented behind the durable binding and attestation
contracts; a deployment should still enable its live Edge/database lane before
calling the full cross-surface E2E verified. Activity polling and multi-user
capacity targets remain unmeasured. Server Run-owner crash recovery remains
unsupported until its production recovery consumer and separate lifecycle E2E
exist.

# Session observability

> Status: target design contract.
> Last updated: 2026-07-07.

Session observability defines how users, agents, and support tooling understand what is happening in a session without reading raw logs.

## Ownership

This document owns:

- user-visible session/run/task status;
- stream event projection semantics;
- stuck/degraded/blocked explanations;
- observability views for resume, cancel, and reconnect;
- support-grade status summaries.

It does not own raw trace schema, which belongs to [observation-plane.md](observation-plane.md).

## Principle

```text
A session should be explainable from durable projections and structured facts.
```

## Status model

A session status projection should include:

```text
session_id
active_run_id
run_status
stage
current_turn
active_task_summary
provider_status
sync_status
last_progress_at
waiting_for
blocked_reason
resumability
terminal_outcome
```

## Progress semantics

Progress should be derived from durable events:

- model round started/completed;
- tool call started/completed/failed;
- provider decision;
- retry decision;
- cache decision;
- sync status;
- task transition;
- checkpoint saved;
- stream cursor advanced.

The explicit CLI `journal digest` reads the complete authorized profile and
attached account journals. Terminal turn usage is the qualified accounting
source; `LlmRound` tokens are a separate diagnostic observation. Digest, diff,
and tree's root self totals share the same root `Turn`/`TurnError` accounting;
neither round replays nor child terminals add to those buckets. Tree still
rolls separately attributed child completions into its inclusive root total.
Tree's `tool_calls` likewise sums known root terminal execution counts and
attributed child completions. An explicit terminal zero is authoritative;
round mirrors and child terminals do not add to the root's own count. Missing
scalars contribute no known executions. Digest's outcome counts may also include
rejected or reused requests, so they need not equal tree's execution count.
Diff's `prompt` and `completion` are fresh input and output; its `total` is
their sum, excluding prompt-cache buckets. Known terminal totals do not
reconstruct missing usage or upgrade a historical last-round snapshot into
a complete attempt total. The JSON
`usage_coverage` and round-conflict fields identify unknown buckets and
excluded contradictions. Root-plus-child observed totals mix sources and are
never a bill or proof of complete session cost. No journal cursor grants
access to another owner's file. The explicit CLI `debug` inspector uses the same
attached profile/account source authorization. Each source retains its own turns,
checkpoints, correction timeline and exported owner identity; ingestion metadata
does not select a storage partition. Debug retains the CLI authentication check
but does not discover skill catalogs or connect MCP providers.

`journal diff` reports known executed tool calls from terminal `tool_count`,
with unowned per-call execution evidence retained separately. Its ordered tool
sequence includes executed records only; rejected, reused, suppressed and
deferred records are not executions. Aggregate-only turns and unresolved
executed observations without sufficient identity make the sequence incomplete.
Runtime failures preserve the available producer run scope. Reconciliation
requires explicit matching run and turn namespaces; call deduplication also
uses the producer-frozen canonical invocation identity (owner, session, run,
turn chain and invocation ID). A terminal summary does not relabel its embedded
records. Different chains within one run remain distinct; round numbers do not
identify invocations. Missing or malformed optional identity preserves the
observation without proving replay. A reused turn number, a sole surviving
call-ID witness or an identical unscoped payload does not establish ownership.
Unscoped rounds and errors remain separate observations, as do contradictory
execution facts. Incomplete totals combine terminal aggregates with retained
unproven evidence and may overcount overlapping or replayed observations; they
are not an exact physical invocation count. Unidentified executed terminal detail
cannot establish that an external observation is additional coverage; matching
the aggregate numerically does not resolve that possible overlap.
When canonical invocation identity establishes duplicate observations, round-internal call
order takes precedence over a standalone error's persistence position.
An executed canonical invocation shared by separately counted terminal buckets,
or by an orphan observation and a terminal bucket, prevents completeness.
Aggregate evidence totals and retained observations are not discarded.
Invocation identity alone does not supply a missing canonical position. When
relocation crosses terminal detail without a comparable position and reverses
terminal-relative order, ordering is incomplete even if all identities are known.
Order between independent concurrent producers remains the journal projection; it is
not a reconstructed physical execution timeline.

Legacy `llm_round` records without `session_id` can inherit a file's session
only when that same authorized source contains an explicit matching session ID
in the inspected records.
Every explicit event ID and conversation cursor must still match the requested
session. A primary file cannot bind an unattributed supplemental source; missing
IDs on other event types and unanchored round-only fragments remain invalid.

## Stuck detection

A run may appear stuck because of:

- model TTFB;
- provider offline;
- tool timeout;
- permission wait;
- plan-mode block;
- sync degraded;
- owner lease issue;
- stream disconnect;
- task waiting for child run.

The projection should expose the specific reason and next action.

The CLI/TUI publishes an interruption as a separate lifecycle warning even
when the model emitted no answer text. A notice-only fallback stored in the
final result is not evidence that a text delta was displayed. Completed tool
output and any partial model text remain visible; the warning does not become
a model-authored answer or change the canonical paused/recovery state.

A safe provider recovery publishes its scheduled attempt before waiting again,
without inventing task progress or adding an assistant-authored history message.
When recovery is exhausted, live and replayed interruption projections carry the
same provider cause, factual completed-call/checkpoint counts and explicit next
action. A zero-tool recovery does not erase preceding tool evidence. Selecting a
different model still requires ordinary authorization and admission.

Each `LlmRoundStarted` retains the exact durable invocation identity and logical
attempt index. It is closed by `LlmRoundCompleted` or typed `LlmRoundFailed`,
including pre-dispatch rejection, streaming interruption and cancellation. A
known completed provider response stays completed if its durable receipt later
fails; that separate settlement failure does not authorize its output. Failure
traces retain safe kind/stage, actual dispatch facts, latency and qualified usage
when available. Terminal trace usage is qualified from that invocation's canonical
attempt facts before emission, including pending receipt settlement. Earlier
logical continuations contribute to turn totals, not this invocation's trace.
Missing usage stays unknown. They do not store raw provider
errors, prompts or reasoning. Capture readers recognize failure events in both
raw and persisted artifact envelopes. Normal `StepIncomplete` on a tool-round
yield is not a failed task; final run errors close the actual current step.

## Stream events

Stream events are transport projections of durable or near-durable facts. They should carry enough information to repair UI state after reconnect.

Model output and reasoning carry `model_item_id`, the identity of their physical
model message, separately from run, request, and transcript receipt identities.
Live buffering, deferred delivery, durable projection, and replay preserve that
identity. It is observation metadata, not provider-facing prompt content.

A durable assistant item retires only live fragments with the same model item
identity and a represented part: output does not retire unpersisted reasoning.
Equal text from different model items remains distinct. Missing identity is not
permission to reconcile by text, completion status, or the latest commit cursor.
This reconciliation uses the existing transcript page; it requires no extra
database lookup. Parent and child views apply the same identity contract.

Tool terminal projections preserve execution facts independently of display
previews. `executed` describes this call: `false` means no execution, `true`
means execution occurred, and explicit `null` means execution cannot be
confirmed. An absent field supplies no execution fact. A `reused` disposition
can carry a receipt from an earlier execution without executing this call
again. Live delivery, replay, and size-bounded projections preserve these
distinctions. A shortened preview is not an authoritative receipt; control
identities remain exact or are explicitly omitted, never shortened into a
different identity.

Pure single-edit argument validation and invocation normalization author their
own no-execution fact before target binding or content reads. Invalid, mixed
single/batch, and identical replacements retain the original error/recovery
evidence, with `execution_started=false`, `execution_fact=not_executed` and
`disposition=rejected` through CLI and server execution, durable terminal
replay, and canonical callback journal recording. Such rejection does not
replenish provider recovery. A missing target, failed content match, unchanged workspace or
`ToolInvalidArgs` kind alone cannot establish that execution never began;
later failures retain their source-owned facts.

Malformed non-critical stream events should be isolated when possible. Identity or cursor corruption should fail closed with structured error and should not corrupt durable run state.

Edge callback HTTP failures are reported independently of tool execution and
durable run state. A generic HTTP rejection, including a proxy/WAF HTML block
page, is not evidence of a lifecycle contract violation. HTTP 409 alone, JSON
shape, and English detail text do not establish an invariant failure. The
Server projects typed outcomes into stable `error_code` values:
`edge_callback_payload_conflict` identifies divergent immutable callback
payloads and retains contract classification; `run_interaction_authority_lost`
and `run_interaction_superseded` identify recorded interactions that did not
resume execution and remain ordinary HTTP conflicts (`InvalidRequest`). Unknown
or uncoded 409 responses also remain ordinary conflicts. Neither recorded
outcome authorizes local execution; only the existing approval acknowledgement
gate can do that. CLI diagnostics retain the HTTP
status and available request ID, omit HTML/markup pages, and bound other response
previews. Live stderr, returned callback errors, and final detach messages use
the same API error formatter. Detachment is distinct from confirmed Server run
cancellation; existing surface-specific settlement and recovery remain
authoritative. An ambiguous acknowledgement must not be treated as proof of
rejection or used to justify re-executing a completed tool.

## Tool results and runtime guidance

Tool result documents and runtime-authored guidance are distinct evidence.
Recovery, quality, duplicate-observation, and pre-tool context annotations must
not be appended to `ToolCallRecord.result_full`: doing so can turn complete JSON
into an invalid document and break typed consumers or artifact recovery.

The journal retains `runtime_advisories` beside the result. Canonical tool
messages retain the same per-call guidance in `_astra_tool_result_advisories`,
including across continuation and compression. Executor-provided metadata cannot
author this reserved runtime field. Guidance is rendered only on disposable
provider/display projections; projection consumes the internal marker exactly
once and canonical-suffix checks use the same projection. Model-facing content
may contain explanatory prose; the canonical result document remains unchanged.
This adds no volatile singleton, global cache prefix, or separate replay state.

Oversized guidance uses the same immutable artifact store and bounded
`introspect(artifact, offset, max_bytes)` recovery as oversized output. Artifact
identity includes the document kind: ordinary result and runtime guidance for
the same run/call cannot overwrite one another. Ordinary result handles and
paths are unchanged. Only result-kind descriptors can authorize `result_full`
or result compaction. The journal retains `runtime_advisory_artifact` beside
the bounded guidance preview; full guidance remains in that verified artifact,
not in every future prompt or journal row.

## Resume and reconnect

On reconnect, the client should rebuild from:

- latest run status;
- transcript cursor;
- task projection;
- provider status;
- sync status;
- recent trace summary;
- artifact manifest.

Browser disconnect is not cancellation.
An SSE or WebSocket send failure, malformed observation payload, or exhausted
observer read retry closes that attachment without changing durable run status.
Only an explicit authorized cancel request may cancel the run; the client can
reattach using its durable event cursor.
An internal CLI stream failure does not cancel the Server run. The local
session execution lease covers local tool execution and canonical partial
commit; after those finish, it is released with the request. A still-active
remote run remains protected by Server session-slot admission. An unconfirmed
local execution or failed owner cleanup is not treated as a clean stream
failure and retains its local exclusion until its own authority settles.

Transcript progress is described by item sequence and committed cursor.
Physical page counts, page high watermarks, and page lag are not exposed.
The committed projection may honestly lag canonical context until its evidence
has been materialized and the contiguous cursor promoted.

## One-shot local content privacy

`astra --no-journal-content chat ...` (or `ASTRA_JOURNAL_CONTENT_REDACT=1`)
intentionally omits the local canonical conversation commit. A successfully
completed turn remains successful if its content-redacted terminal record is
durably settled. The record and final JSON report
`local_continuation: "omitted_by_policy"`; plain output warns that this turn
cannot be recovered or automatically resumed locally. This is distinct from a
canonical commit and does not grant a canonical cursor or claim recoverability.

The local terminal record retains redaction markers and operational counts,
without model/tool content or raw diagnostic sidecars. No CSL snapshot is
written and no profile last-session pointer is advanced. Local recovery and
automatic resume reject older journal/CSL/checkpoint state after that boundary.
File-edit undo entries remain in memory for the running process, but their
before/after content is not persisted to file checkpoints under this policy.
Previously written checkpoints are left unchanged.
Automatic cloud-list selection also skips locally marked sessions. A local
journal read failure cannot prove the absence of this boundary, so local
recovery and automatic selection fail closed rather than using an older
snapshot. A successor commit must also pass canonical replay before clearing
an earlier omission; a JSON-shaped record alone is insufficient. A legitimately
missing journal still permits existing recovery sources.
In one-shot chat, an explicitly selected, authorized Server canonical
ResumeBundle remains the Server's authority; a later complete canonical local
settlement can restore local recoverability. TUI restore of a locally marked
session is rejected before turn admission with guidance to use an explicit
one-shot Server resume. Ordinary unmarked TUI restores retain their existing
behavior. This switch controls local persistence, not Server
retention or the stdout/event output explicitly requested by the caller.

Privacy settlement uses the existing journal execution lease, owner checks,
base-cursor comparison, file-generation checks, fsync and exact-record readback.
Actual append/durability failures, uncertain settlement and invalid continuation
still fail closed. The stdio app-server requires durable local thread
continuation and rejects this policy before admitting a turn.

## User-facing diagnostics

A user should be able to answer:

- Is it running, blocked, waiting, cancelled, or complete?
- What is it waiting for?
- Is Edge/CLI/server/MCP provider available?
- Did sync finish?
- Can I resume, retry, reconnect, or cancel?
- What changed since last visible output?

## Test obligations

- Offline provider produces visible blocked/degraded status.
- Malformed stream cursor does not corrupt active run state.
- Browser disconnect does not imply cancellation.
- Cancelled tasks disappear from active task board projection.
- Resume reconstructs status without stale UI cache.

A failed canonical terminal commit emits a live-only, root-scoped
`run_error` with `error_code=canonical_persistence_failed`. It preserves partial
answer text and advises reopening the Session to inspect recorded state before
retrying. This notification is not a durable terminal or evidence that an
uncertain transaction committed or rolled back; the canonical recovery owner
still decides that state. No successful completion is published from this path.

Web keeps one genuine text/reasoning projection for the selected message in its
existing user-scoped chat state across observer connections. Reconnect initializes
the browser from that projection and resumes at its captured cursor; rendered
text and failure diagnostics never substitute for raw parser state. One observer
may write that projection at a time. Conflicting observers or missing/ahead
continuations return an explicit observation error without failing the Run or
replacing the saved answer. Replay-only observation remains independent. This
state has the same process lifetime as the Web chat projection; it adds no durable
or cross-server transcript guarantee.

The Web continuation cursor is the next event not represented by the selected
message's genuine projection. Backend status polling updates control status,
not parser progress; a durable event count is only a starting boundary when a
new queued message is admitted. Public control responses expose a continuation
cursor only for a matching Session, root Run and assistant projection. Resume
uses that server-selected assistant, including a completed paused message;
missing local bindings are refreshed once and reported as unavailable rather
than creating a browser-only assistant.

Web admits live Run observation only from the committed selected binding. Resume
and queued input publish that selection; the existing lifecycle owner registers
POST creation and admits GET observers using one chat/session/run/message lease.
Replacement and unmount retire only observation, fencing late callbacks, queued
patches and retries. Cursor and status progress do not replace a live observer.

Deferred-input conflicts refresh the selected observation only while the initiating
chat/session/run/message still owns it. A conflict never implicitly starts a new
Run. Unconfirmed submissions report an error and retain the Composer draft for
review and explicit resubmission; only accepted input receives queued feedback.

Plan-review approvals derive bounded correlation IDs from the Session and tool
invocation at the existing approval producer. Native invocation IDs are unchanged;
the review ID and its durable event keys fit the existing interaction columns.

Stop uses the same selected chat/Session/root/assistant binding as observation.
The request requires the initiating Session, root and nullable assistant; a missing
or malformed binding is rejected, and a changed binding is a conflict before any
cancellation is dispatched. A runtime-only root has an explicit null assistant
and never completes another message by fallback. After dispatch, cancellation
continues only against its captured root; delayed responses, reconciliation and
rollback cannot mutate a successor binding or newer root facts. Browser cleanup
belongs to the initiating operation, and Stop status reads never overlap.

When a committed Stop outcome restores the same attachable binding, live
observation recovers through the existing bounded GET retry owner. This includes
reconciliation after a pending cancellation is rejected, not only an HTTP error.
The handoff preserves the budget and cursor, never replays the turn, and leaves
paused/terminal outcomes to their existing lifecycle semantics.

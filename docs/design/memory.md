# Memory

> Status: target design contract.
> Last updated: 2026-07-07.

Memory owns durable knowledge across and within sessions. Context injection uses memory, but memory lifecycle, provenance, confidence, and deletion belong here.

## Memory classes

| Class | Meaning |
| --- | --- |
| Working memory | Current turn/session scratch state. |
| Episodic memory | Past session events and outcomes. |
| Semantic memory | Durable facts and concepts. |
| Procedural memory | Reusable procedures, preferences, and skills. |
| Project memory | Workspace/repository-specific facts. |

## Requirements

- Every memory item has provenance.
- Confidence and freshness are explicit.
- Conflicts are represented, not silently overwritten.
- User deletion propagates to derived memory.
- Memory used in a response is traceable.
- Memory injection is bounded by context budget and task relevance.

## User-requested retention

Acknowledging a fact supplied in the conversation does not require lookup or
persistence. A request to retain user facts or preferences requires a successful
memory write before the assistant claims storage. Conversation-only scope and
explicit tool bans remain authoritative; they do not grant permission to persist.
If persistence was requested but cannot be performed under those constraints,
the assistant must distinguish acknowledgment from an actual stored memory.
A write receipt proves storage, not a later-session recall that has not run.

## Loading policy

Memory loading is intent-driven:

- current task determines candidate memories;
- recent session facts outrank old memory on conflict;
- procedural memory should be loaded at point of use;
- low-confidence memory should be marked as uncertain;
- sensitive memory follows permission and redaction policy.

## Backend boundary

The design does not require one physical backend. Vector, fulltext, graph, tabular, or MCP-backed memory can coexist as long as they satisfy the same provenance, confidence, deletion, and trace contract.

## Credential authority and admission

Extraction and remote session-end maintenance are Server-owned and run at
explicit Session close/finalize boundaries. Ordinary model rounds neither recall nor refresh long-term memory, and do not
load the current-session memory snapshot. Explicit memory tools and HTTP recall
remain available. This removes implicit credential resolution and remote reads
from every model preparation; users request memory when the task needs it. CLI
exit hands off through `POST /sessions/{session_id}/memory/finalize` using the
selected profile's fresh access token; it preserves the resumable Session and
never falls back to a client-side purge if the handoff fails. Explicit session
close enqueues the same maintenance within the canonical close transaction.
Direct/in-process providers continue to use the same governance algorithm.
Without a bound memory provider and session identity, CLI exit skips maintenance.
The CLI memory inference adapter remains available for lesson relevance checks;
it does not introduce a second extraction service or lesson synthesizer.

The Server composes one per-user memory authority policy. Explicit `memory` tools, HTTP memory routes, compaction and session-end governance must use that same policy.

Hosted or browser-login deployments use the application-scoped credential resolver owned by authentication. Each operation resolves the current owner binding and generation; no master-key fallback is inferred by a generic pool builder.

- Missing binding or `none`: normal disabled state. Optional memory operations report `NotAttempted` and do not contact Memoria.
- `read_only`: recall is allowed; write-oriented extraction, reflection and session-end cleanup are not admitted.
- `read_write`: read and write operations are allowed. Transport checks remain in place to catch revocation or changes after admission.

Consent denials in explicit tools and HTTP routes point to Memoria Settings → Connected apps → Astra Cloud → Memory sharing settings. They distinguish disabled sharing from read-only write denial and do not prescribe `/login` or a deployment master key as a hosted-user remedy. Missing server wiring reports a configuration problem for the administrator; local execution retains its deployment-specific configuration guidance.

Trusted self-hosted deployments may explicitly enable `MEMORIA_SELF_HOSTED_MASTER_ACCESS=1` with `MEMORIA_MASTER_KEY` and no `MEMORIA_WEB_URL`. A persisted scoped binding always wins, preserving its owner namespace and consent. Only an active local password account with no scoped binding and no retained Memoria identity may select the owner-bound master port. Disconnect, account deactivation/deletion and lookup errors fail closed, including for runtime ports created before the lifecycle change. These data requests authenticate with Memoria's owner-scoped master scheme, which validates the deployment secret but removes administrator authority before routing to memory handlers. Every request projects the authenticated Astra user as the Memoria owner; an unbound or incompatible backend fails closed. Memoria must contain the `Memoria-Owner` support introduced by `matrixorigin/Memoria#250` (not present in v0.5.1).

The background coordinator may launch a lightweight admission task, but it checks consent before loading snapshots, resolving an LLM, generating memory, or scheduling persistence. See [authentication](authentication.md) for issuer, credential replacement and retention.

MOI native UC uses the same resolver and consumers with an explicitly enabled
product-owned authority, not the local-password fallback or a second Memoria
login. Current UC status and canonical issuer mapping are checked for every
operation. Its bounded owner namespace and server-only credential are described
in [MOI native login](../guides/moi-native-login.md#built-in-memory). Ordinary
model rounds do not initiate memory operations, even when this authority is enabled.

Optional reads report admission through the existing `MemoriaPort` read
operation, with one fresh authority resolution per operation and no discarded
read preflight. `Disabled` is quiet `NotAttempted`; `AuthorityUnavailable` and
`Failed` are `Unavailable`; successful empty reads are `Complete`. HTTP status
errors, including 401/403, are failed operations rather than inferred consent
denials. The shared absolute 750 ms prompt deadline covers authority and HTTP.
Snapshot reads quietly skip disabled memory; compaction takes its early local
fallback before summary generation for disabled or unavailable authority, while
transport failures retain downstream summary behavior. Cleanup maps every read
error to purge failure. Write admission before expensive work and fresh final
write checks remain required.

## Finalization, recovery and owner boundaries

Strict HTTP recall responses include a fresh `memory_scope` attestation with
`astra_user_id`, `memoria_owner_id` and `session_id`. Authentication authorizes the
Session using Astra identity; the proxy validates backend rows using the resolved
Memoria owner before overwriting any backend-supplied attestation. CLI validates
that attestation against its selected Astra account and requested Session, then
validates every row against the attested Memoria owner. UC and personal Memoria
namespaces need not equal `/auth/me.user_id`; an absent, malformed or mismatched
attestation fails closed. No owner or grant cache is shared between operations.

Fresh schema contract `2026-10-16-v104` includes `agent_session_memory_work`,
the durable journal for extraction and governance. Ordinary turn completion
stores conversation input only through the canonical Context coordinator: it
does not clone the transcript into a memory job, run extraction, or create a
second post-loop settlement worker. Close/finalize enqueues one durable task;
the existing recovery driver materializes the latest committed Context head
and freezes input only when that task executes. This intentionally trades
per-turn long-term-memory freshness for fewer writes and model calls. Runtime-only
per-turn facts are not checkpointed separately for memory; extraction derives
content from the committed conversation and turn coordinate. No in-process
snapshot or per-turn handoff is required for recovery.

Destructive stages verify the canonical completed-turn
watermark and wait for execution settlement. If freshness cannot be verified or
materialization fails, working memory is retained.

Worker selection uses a database lease (90 seconds), with owner/session-specific
fencing through the existing Session write fence. Short preparation/checkpoint transactions take that fence; only
destructive working cleanup holds it across its bounded HTTP call. Optional reads, extraction
writes and reflection do not hold Session admission open while waiting on the
backend. Before cleanup, the fence rechecks execution settlement, the canonical
watermark and the exact frozen plan. The lease does not grant memory authority:
every read/write resolves current owner and consent. Extraction retains its 20-second attempt
budget; finalization retains a 60-second attempt budget. Restart and lease expiry
recover unfinished work. Failed attempts back off from 5 seconds to 16 minutes;
the managed recovery worker polls every two seconds with concurrency two.

The journal freezes an uncertain working-write payload before HTTP dispatch,
then freezes the final snapshot and runtime facts before consolidation. The
working inventory follows owner/session-scoped cursor pages (at most 500 rows per
request), with a 4096-memory bound and a ten-second total read budget. Only an
explicit end of pagination permits freezing the plan; overflow, malformed or
repeated cursors, duplicate rows, and failed pages retain working memory. Journal
reads decode the native JSON column so inputs and frozen plans larger than 64KB
survive restart without a truncating character cast. Required
writes must return exact, nonempty, protocol-compatible receipts. Once a working
write is prepared, subsequent reads cannot replace its receipt identity; stale
snapshot cleanup remains best effort even if a deletion succeeds but its response
is lost. Reads before a new write may still record a reused existing snapshot.
Episode receipts, exact working IDs removed, reflection candidates and scene
receipts are checkpointed separately. A failed archive cannot authorize cleanup. Cleanup only
removes the captured owner/session working IDs and accepts an already absent ID
on retry. A failed read never grants an empty successful purge. Optional
reflection/scene retries preserve completed episode and cleanup stages.

The journal stores at most one pending input and one different active input.
Claiming moves input into active ownership rather than duplicating it. Successful
extraction retires its transcript and prepared write, retaining publication turn,
facts and fingerprints for canonical recovery and later finalization. Fenced
inspection validates the same authority without rewriting or rescheduling the
journal. Finishing combines the checkpoint and lease release in one transaction.
Only full consolidation and optional-stage success retire the frozen inventory,
receipts, purge IDs and reflection payload; failures retain exact replay inputs.
Completion flags and the status report remain available after retirement.

This bounds payload generations, not total retained sessions or arbitrary input
bytes. Permanently failing work remains recoverable and can retain large inputs;
Session deletion owns physical removal. More worker pods can process independent
sessions, but the shared due index and database pool remain
database capacity limits; this is not a claim of unlimited horizontal throughput.

CLI exit targets its idle generation; a follow-up turn invalidates that old
maintenance request. Explicit close settles all already-admitted extraction and
rejects writes after the closed generation is consolidated. The close transaction
creates its journal without requiring a per-turn snapshot, allowing restart recovery to discover canonical input. A previously completed
CLI-exit generation cannot complete an explicit close while actual Run execution,
a live owner lease, unresolved effect custody or explicit Controller authority
remains. The atomic Run terminal/context commit retires its exact execution
owner; there is no earlier derived-slot release to interpret as settlement.
Close invalidates the old maintenance plan under the same canonical Session/Run
fences and retains pending work for the admitted turn's late handoff or canonical
recovery. Destructive checkpoints wait for execution and actual authority to
settle, using database time for authority expiry.
Repeated close or exit requests on an idle, completed generation retain journal
progress instead of creating duplicate archives.
`GET /sessions/{session_id}/memory/status` exposes pending work, the settled turn,
required/optional completion, failure flags and retry timing to the Session owner;
it does not expose queued prompts, credentials or snapshot content.

Durable working/episode/scene writes require Memoria's dedicated
`POST /v1/memories/idempotent` protocol. Durable extraction and finalization
explicitly pin reads, writes and cleanup to Memoria `main`; a user checkout cannot
retarget this journal or change its expected receipt identity. The authenticated
owner, branch and stable key determine a receipt ID; the backend atomically arbitrates inserts and rejects
key reuse with another payload. Old servers reject the dedicated endpoint before
writing, so Astra retains working state and reports unavailable maintenance until
the matching backend is deployed. Similarity deduplication is not a substitute for
an idempotency receipt. The existing direct transports and explicit memory tools
retain their ordinary API and timeout contracts.

CLI keeps its five-second exit budget; that budget covers the durable handoff,
not remote completion. A queued acknowledgment is never reported as a confirmed
archive. Disconnect/revocation blocks future journal operations without erasing
remote memories. Deleting an Astra Session removes its queued journal inputs and
cancels later stages under the same Session fence; it does not promise remote
Memoria erasure. Memoria retention and explicit memory deletion still govern the
remote records. Successful consolidation is tracked durably per generation,
rather than relying on a process-local 15-minute debounce timer.

The interactive Server no longer runs session-start or semantic prompt prefetch.
The shared memory-read helpers remain available to explicit memory workflows;
current credential and owner checks belong to each actual operation. Compaction
may still return memory produced by that explicit context-maintenance boundary.
Web session switching and archiving do not imply memory finalization.

## Learning boundary

Memory is not training data by default. Learning artifacts require consent, redaction, quality gate, lineage, and deletion propagation.

A memory retention request alone does not establish a durable Work task. A turn
that prohibits tools must not create Work through automatic admission. Ordinary
conversation-only acknowledgment needs no lookup or storage caveat. The compact
resident memory surface supports remember/recall; select the full memory contract
and use its invocation carrier for operations such as forget or update.

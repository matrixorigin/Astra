# Data and storage

> Status: target design contract.
> Last updated: 2026-10-31.

Data and storage owns platform state layering, MatrixOne usage, retention, archival, versioning, and data correctness boundaries.

## State layers

| Layer | Purpose |
| --- | --- |
| C0 control | sessions, runs, tasks, checkpoints, leases. |
| C1 transcript | user-visible messages and transcript items. |
| C2 audit facts | durable events, permissions, provider decisions. |
| C3 trace facts | execution trace and diagnostic events. |
| C4 debug bundle | short-lived raw diagnostics and manifests. |
| C5 learning artifacts | redacted, consent-gated derived data. |

Delegation topology, status, retry lineage, usage, and terminal evidence are owned
by `agent_runs` and `agent_run_events`. Run admission and owner-fenced transitions
persist these canonical facts; there is no separate delegation table or
delegation-state mirror. Run display is rendered on read from canonical Run, events and checkpoints.
There is no display table, background mirror repair, or display hash in a
terminal receipt. Lost-ACK reconciliation validates the actual canonical facts.

The unused general authority-event query source API is retired; canonical
Session authority still persists owner-scoped audit facts. Handoff fencing
verification reads the exact rejected reservation, including its old lease and
expected root, rather than accepting an unrelated rejection event.

Execution bindings are initialized once as Ready at generation 1. Subsequent
provider switches use the controller-authorized durable begin/complete/retry
receipt contract; the unused general binding CAS source API is retired.
Generation fencing, quiescence checks, and owner-scoped binding reads remain
part of the canonical coordinator. Edge handoff requires verifiable source
and target materializations; it does not convert a Server binding into Edge.

Work graph dependencies are stored only in the immutable graph revision's edge
manifest. Graph validation and revision-pinned readers share that authority;
there is no relational edge projection. Criterion identity is owned by its
initial immutable revision. Revision 1 remains retained for the Work lifetime,
including after removal from an accepted set, so the existing owner/Work-scoped
primary key prevents identity reuse without a separate identity table.

## Persistence simplification boundary

The production schema inventory contains 112 table declarations. The earlier
retirement of diagnostic projections and separation of immutable Run openings
are already part of the baseline. This wave removes the five authoring and
adaptation tables listed below; it adds no replacement table.

| Journey | Boundary and disposition |
| --- | --- |
| Identity, device trust and policy | Preserve authorization, replay protection and credential/lease authority. |
| Session, Run and conversation | Preserve lifecycle, deletion fencing, canonical transcript and recovery. |
| Canonical Context, fork and handoff | Preserve heads, immutable nodes, pins, receipts and execution switching. |
| Event audit and other diagnostics | Preserve the event DAG, semantic/model evidence and identity-collision diagnostics. Generic Context/Decision captures and Server harness snapshot products are retired. |
| Inference and model routing | Preserve physical provider identity, admission, settlement, usage and route policy; metrics remain a derived projection. |
| Tools and artifacts | Preserve uncertain-effect recovery, durable reference edges, upload leases, reservations and sealed bytes. |
| Workspace, agents, Edge and messaging | Preserve selected provider ownership, dispatch and message coordination. |
| MCP and skills | Preserve installed capabilities, credentials and version/evaluation journeys. MCP discovery remains a projection. |
| Plan, session state and semantic reads | Preserve actual authoring gates, owner state and read observations; Work is not an automatic replacement for these callers. |
| Quotas, bootstrap and maintenance | Preserve admission limits, schema ownership and bounded maintenance progress. |
| Work roots, revisions, decisions and operations | Preserve branches, evidence, ordering, recovery and cross-Session operations; do not replace sequence high-water marks with retained-row MAX. |
| Work runtime-event delivery | Preserve the bounded outbox, coverage floor and projection cursor. |
| Work saved-progress capture | Preserve real capture, package/download and artifact/Context retention. |

The earlier wave removed prompt hash-diff diagnostics (two tables),
legacy database-versioning HTTP checkpoints (one), Work-specific patch
export/application/commit (three), and persisted Context manifest diagnostics
(two), plus the two duplicate Work representations described above. No new
projection, operation store, background worker or process cache replaces them.

Run projection drops two diagnostic SQL reads; Session state drops those two
plus one or two latest-manifest reads. Trace-enabled writes no longer maintain
those diagnostic histories. Production defaults already disabled these writes,
so this does not imply a write reduction on every default production exchange.
Actual provider usage/cache facts and explicit full captures remain independent.

Fresh schemas carry no diagnostic-only artifact manifest or state-item counter.
Retained durable reference kinds, citation counters, byte-content references and
upload protocols keep their existing transactions and retention deadlines.

This is a fresh empty database and same-version deployment contract. There is
no additional marker mechanism, migration, compatibility API or old-data cleanup.
The existing schema contract version advances to v117 and rejects earlier versions. Strict
request-context JSON decoding is not promised for historical payloads. Existing
engine snapshots and in-flight old Git operations are not reconciled by the new
build; stop old workers before handing their workspaces to a new deployment.

### Mutable Run control and cold payloads

`agent_runs` is the sole Run control owner. Its immutable opening bodies live
in exactly one `agent_run_openings` row per Run, including Runs with no opening.
The existing creation transaction writes both rows, initial events and admitted
Context together. Full readers verify the opening hashes and reject a missing
payload row; control/frames-only readers explicitly carry an unloaded body.
Control-only transactions use the existing admitted and locked control projection:
recovery ownership claims, cancellation, terminal/pause transitions, checkpoint
writes, ordinary generation-fenced event drains and user intent reconciliation.
They validate control references without loading or certifying the cold opening.
A recovery batch retains control records, rather than accumulating every
candidate's immutable opening while its locks are held. A claim establishes
custody and an authoritative frontier; it does not authorize execution.

Fresh execution resume, explicit running/waiting status transitions,
invocation-bound intents, Provider interactions and model-routing admission
retain complete opening validation. Approval and AskUser waits/resolutions use
control facts; resolving an existing live wait can return its executor to running
without granting a fresh executor. Every exact-session
locked read selects its projection explicitly, sharing the same Session admission,
lock order, generation/lease/cancellation fences and transaction owner. No cache,
additional SQL round trip or duplicate lifecycle state is introduced.
Conversation selection, watermark and admitted opening bytes share one statement
snapshot. Session deletion removes opening rows before Run rows.

Checkpoint bodies live only in `run_checkpoints`. The Run stores an all-null or
all-present `(current_checkpoint_id, checkpoint_version, current_checkpoint_hash)`
reference; the digest covers the exact stored bytes. Mutable ordinary identities
retain their original creation time, so saving A, B, then A makes A current while
B may still be newest by history creation order. Immutable handoff retries keep
the original stored bytes and hash. Owner/generation/lease/cancellation admission,
checkpoint persistence and the final Run CAS remain one transaction. Historical
handoff-reference validation retains its separate custody/tail rules.

The unused atomic-terminal write/replay API is removed. Runtime settlement uses
one canonical terminal receipt owner, with Context/transcript orchestration in
the lifecycle transaction. Shared control admission, physical control-event
receipts and hash validation remain because canonical receipt resolution uses
them. Out-of-tree Rust callers must use canonical settlement; no compatibility
adapter or in-flight legacy settlement resolver is retained. Historical events
remain readable through the ordinary event reader.

The bounded terminal receipt and error remain on the Run row to preserve atomic
terminal settlement without another write. Recency updates retain their existing
list/cursor semantics. Guidance reads lease liveness with its locked control
projection; exact commit reconciliation uses positional windows of at most 64
identities and 16 KiB of identity bytes. Ordinary append already batches its
idempotency preflight. These paths add no cache, scheduler or lifecycle owner. The Run binding/Offering indexes and Run-event event-ID index are removed: current queries do not filter by those keys; owner/recency, Work, sequence/idempotency, control, interaction, subject and supersession access paths retain their indexes.

The retired diagnostic products intentionally lose their separately submitted
Context/Decision payloads, Server live/durable harness snapshots, duplicate
activation mutation history and self-reported skill evaluation counts. Real
Context authority, semantic judgments, model receipts, event reflection, harness
verifier enforcement and skill authoring/publication/activation remain with their
existing owners. No migration, backfill, compatibility reader or new schema marker
is provided: this shape requires a fresh database and one matching binary version.

## MatrixOne role

MatrixOne is the platform state store and analytic substrate. It should support:

- durable runtime state;
- indexed transcript and event queries;
- audit and replay;
- retention and purge jobs;
- analytic/evaluation workloads;
- optional MatrixOne-native value such as time travel or hybrid search when appropriate.

SQLx TCP connections enable `TCP_NODELAY` before the MySQL handshake and TLS
negotiation. Consecutive small protocol messages, including prepared-statement
close and execute packets, must not incur Nagle/delayed-ACK stalls. This is a
transport setting; persistence and transaction boundaries remain unchanged.

Pool shutdown cancels an unfinished health check on a connection already being
returned to the pool. The connection is discarded and its permit released;
shutdown does not depend on a reply from that health check. A close listener is
registered before checking the closed flag so concurrent close cannot lose its
notification. Healthy returns still check and reuse their connections. This is
not a deadline for application-held connections or other graceful-close I/O;
callers must still stop admissions and drain the owning runtime first. Shared
pool limits and process-wide connection quota remain unchanged.

Settled execution continuation belongs to the Run checkpoint. Provider attempts
retain their exact wire hash, byte count and admission identity; uncertain
admission reconciles those facts before transport. The unused database canonical
transition head/WAL, Session recovery API and through-turn retirement protocol
are retired. In-flight model recovery remains unsupported.

## Schema bootstrap

The unused database CSL backend and its table declaration are retired. Server
continuation uses canonical Context and Run checkpoints; CLI continuation uses
its file CSL. Fresh schemas omit the database CSL table.

Bootstrap supports a fresh database or the current schema contract. Canonical
CREATE declarations define the complete table shape; startup validates it
without ALTER-based upgrades, historical backfills, or retired-table cleanup.
An older completion marker is rejected and requires a new database. Interrupted
fresh bootstrap can retry under the existing database lease, with readiness
published only after schema validation succeeds.

The standalone Agent configuration catalog and its CRUD service are retired.
Edge registration, heartbeat, AgentBindings and Run execution retain their
canonical owners; no replacement Agent metadata projection is stored.

## Session-memory recovery journal

Fresh schema contract `2026-10-16-v104` includes `agent_session_memory_work`, keyed
by canonical Astra owner and Session. It checkpoints the existing extraction
and governance algorithms: frozen request/write inputs, stage receipts, a
worker lease and bounded retry timing. Canonical Session status, execution
settlement and context heads remain authoritative; the journal does not grant
memory access or create another context lifecycle. Session deletion removes
its queued inputs through the existing deletion catalog.
Ordinary turns create no memory-journal row or input snapshot. Explicit Session
close/finalize enqueues maintenance; its worker materializes the latest canonical
Context through the existing coordinator before freezing extraction input.

Completed extraction drops its input transcript; publication coordinates and
facts survive independently. Only fully successful governance retires replay
inventories and receipts. Fenced inspections do not persist state, and finishing
commits state and lease release together. Uncertain/failed work keeps its exact
replay data. There remains one row per retained Session, not a global byte bound.

The startup shape guard requires all journal columns. An existing v102 database
is rejected under the repository's fresh-schema policy; deployment requires a
new/current-contract database rather than only changing the image. Bootstrap
does not silently add the journal to an old completion marker. Database
replacement and preservation of existing data require an operator-owned rollout;
this memory change performs no automatic reset or migration.

The CLI's composite checkpoint index is committed atomically with its local
checkpoint. It is not mirrored into `session_artifacts`: Server recovery uses
canonical Run checkpoints and conversation state, and CLI debug reads the local
index. Checkpoint finalization performs no remote index merge or duplicate
projection write.

## Transcript persistence

Fresh schema contract `2026-10-08-v101` stores transcript items and their
committed projection head. Physical page metadata and the unused source event
position column are removed. The run lookup index is `(user_id, run_id)`.
Context snapshot anchor, request, and response IDs share the canonical event ID
capacity, and the schema guard checks all three columns.
Model thinking observations use only the configuration-bound snapshot; obsolete
capability and error mirror columns are absent.
The retired tool-result projection decision/receipt tables and special event
field are absent. Agent provider observations are request-local; they create no
inference invocation/attempt rows, accepted/terminal context pairs or settlement
debts. Standalone completion and background accounting
retain their inference ledger. Durable tool custody remains independent.
The retired session-level physical-workspace claim, unexecuted workflow-trigger
and plan-step attempt tables are not part of this contract. The unused cloud
configuration-body mirror is also absent; configuration version changes retain
their canonical journal audit and local content-addressed bodies. Plans retain their
owner-scoped authoring state and session binding; execution and attempt evidence
remain with Work and Run. There is no migration, replacement table, page cache, or rebuild job;
an older database must be recreated from the current schema.

One append implementation requires canonical session admission in the same
transaction. Canonical/core event writers reuse their admission; standalone
append and evidence materialization acquire `admit_session_event_write(false)`,
including reasoning-only and cursor-only materialization. Empty append executes
no SQL; all-replay append does not allocate a sequence. New identities use one
lazy `MAX(item_seq)` under the owner/session lock. Membership and insert batches
are limited by bind count and payload bytes; an indivisible oversized item is
sent alone. Database column equality determines duplicate identities within
and across batches. First occurrence wins; equal content under distinct IDs
remains distinct. All chunks and enclosing event capture roll back together.

Item content hashes, per-item canonical commitment fields, contiguous committed
projection heads, and authoritative terminal replay verification remain
required. Accepted event replay can repair missing display material and enrich
reasoning; it cannot reconstruct arbitrary original item sequences/timestamps
or synthesize a committed head from the newest item. Server Run prompt history
comes only from the admitted canonical Context, including an empty head. It does
not probe CSL or raw transcript rows to reconstruct missing Context. Imported or
legacy-only transcripts remain readable as history; they do not silently become
model context. Explicit Run resume continues through its checkpoint custody owner.

Server Session resume materializes the owner-scoped canonical Context before
activating the Session. Both full hydration and continuation fail when that
state is missing or invalid; neither consults Server-local recovery files.
Full hydration retains diagnostic token totals, title and branch hints, but
these cannot authorize execution. CLI local journal/workspace and checkpoint
recovery remain explicit local operations. The unused cloud
`session_checkpoints` table and checkpoint-only Server fallback are retired;
Run checkpoint custody remains with `run_checkpoints`.

Fresh schemas also omit `skill_selection_events`, `infra_sandbox_metadata`
and `skill_metrics`. The separate skill-selection writer copied queries and
reported success without deriving it from execution. Actual skill resolution,
activation, tool outcomes, semantic decision evidence and observer dispatch retain their
existing owners. The metadata-only Sandbox CRUD API and its database-snapshot
alias are removed; execution isolation belongs to the selected capacity provider
and workspace. Marketplace report aggregation, global quality statistics and
ranked search are removed; registry visibility, installation, publishing and executable activation remain. No replacement projections or compatibility paths are added.
These cuts apply to fresh databases. They do not delete existing physical tables
or historical rows, and do not define a mixed-version deployment or data-retention
transition.

## Retention

Retention is a product contract:

- C0 follows account/session policy.
- C1 follows user history policy.
- C2 follows audit policy and may archive.
- C3 follows diagnosis window and may compact.
- C4 has short TTL and explicit enablement.
- C5 follows consent, lineage, and deletion propagation.

## Correctness

- Event ids should be stable and collision-resistant.
- Ingestion should be idempotent.
- Diagnostic event counters commit with their inserted diagnostic rows. Exact
  replay contributes no insertion delta. They count captured diagnostics, not
  conversation turns or user-visible messages.
- Fresh root admission commits one immutable Run opening containing the
  original input, admitted execution contract and Context base identity, plus
  its original execution lease. It does not borrow a Controller writer or reservation,
  or materialize a second raw input before execution. Exact opening identity
  resolves a lost acknowledgement. Resume reuses this original admission.
- Raw conversation settlement stores original input, applied user guidance and the
  actual terminal assistant answer with its model-item identity. An accepted Run
  that ends before execution can retain only its immutable opening. User-visible
  history combines that input with materialized raw history at the transcript
  owner through the bounded `scope=conversation` read. This replaces selected
  detail's owner-wide Run enumeration; idle/warm reads also refresh raw history.
  It adds no stored projection or synthetic transcript cursor. Root status and
  raw rows share one SQL statement snapshot; metadata and artifact previews keep
  their separate read boundaries. Historical opening inputs have a 256 KiB
  budget on both stored JSON bodies plus a conservative display envelope, applied
  in the same SQL before transfer/decoding. History retains a newest continuous
  prefix within 200 candidates. A reusable same-root continuation can conditionally
  omit history in that snapshot; a successor returns full bounded history. Explicit
  omission has no raw cursor or truncation assertion. The selected input is retained exactly
  under the existing opening-size limit. Display history is limited to 200 items.
  These bounds do not imply history-independent database scans. It does not
  copy tool starts/results: canonical Context owns model continuation and the
  invocation ledger owns governed outcomes and effect uncertainty. Run tool
  cards are optional live presentation. Paused output does not occupy the
  final-answer identity. Compaction cannot replace the original user input.
- Raw append validates database identity equality in bounded batches. Exact
  owner, nullable Run, role, content and immutable payload must match; the full
  stored content hash must also verify. Assistant reasoning/status are separate
  presentation enrichment. Unknown JSON fields remain immutable identity.
  Conflicts in any chunk roll back the entire transaction, including earlier
  siblings. Exact replay allocates no new sequence and preserves enrichment.
- Root execution commits canonical Context (one manifest/content row and head),
  original raw transcript, one bounded Run completion receipt, semantic usage
  and exact Run owner retirement together. It needs no Controller operation receipt,
  writer release, settlement markers or mirrored terminal event payloads. Context rejection leaves the Run active; Run rejection rolls back
  Context too. Lost acknowledgement resolves the exact generation-scoped Run
  receipt and verifies full raw identities and the exact immutable Context
  manifest metadata, indexed coordinates and its ordered inline contents. A later Session head or
  compaction does not invalidate a retained historical commit. Recovery
  cannot repair absent history or accept conflicting facts.
- Session list/GET/resumable recency is the later of explicit Session metadata
  activity and the established server/default-branch Context head's update time.
  Existing head writes, including Controller leases, renewal, authority cleanup
  and fork activation, may promote a Session; recency is not a commit-only clock.
  The shared list/GET/resumable query does not read immutable conversation nodes
  or aggregate Run history, and adds no activity writes or read repairs. Empty
  or missing heads fall back to Session metadata. Seek cursors and ordering use
  the same full-precision timestamp, with Session ID breaking ties. Static
  directories paginate stably; concurrent activity does not provide a snapshot
  or an exactly-once traversal. Listing may still scan and sort the owner's
  Session directory; it must not scan global conversation history.
- The resumable Session directory is one owner-scoped metadata/head query,
  limited to 20 active or paused Sessions. Its `turn_count` is the current
  canonical head's completed turn, or zero without an established head. Model,
  Git branch and permission mode are metadata hints only. It does not aggregate
  diagnostic events or hydrate workspace artifacts, tools or checkpoints per
  row. Usage/checkpoint counts are zero and detail fields remain empty because
  they are not loaded. The response structure is unchanged; selecting an ID
  still invokes the separate detailed restore and authority checks. Sessions
  with diagnostic or workspace history alone no longer qualify for automatic
  CLI selection based on a positive directory turn count; explicit ID selection
  still reaches detailed restore. Directory hints never authorize execution.
- Optional `agent_events` diagnostics neither authorize nor gate conversation
  admission, settlement or replay. Ordinary root/resumed execution and children
  do not write diagnostic copies during execution or terminal cleanup. Explicit
  root Explain uses the existing bounded ingestion owner; legacy diagnostic
  turn lists can be sparse and are not a conversation authority.
- Context display publication follows canonical terminal settlement in the
  existing tracked cleanup task. Workspace projections no longer block model
  completion; `context_trace_signal` uses the bounded ingestion worker and its
  batching, retry and shutdown drain instead of a synchronous event API
  transaction. Failed canonical settlement does not publish derived context.
- Trace capture and asynchronous ingestion share an all-inserted batch proof:
  exact affected-row count plus distinct, bounded identities avoids rereading
  newly inserted rows. Partial insertion, duplicate identities, replay and
  possible truncation retain owner-scoped hash/write-marker classification.
- The existing asynchronous ingestion worker combines eligible session heads
  into count- and charged-byte-bounded transactions. The aggregate event bound
  or earliest absolute flush deadline triggers dispatch; owner round-robin and
  one in-flight head per session preserve fairness and ordering. One transaction
  uses one shared ingestion permit and connection, retaining foreground pool
  headroom. It inserts event rows together and applies inserted-only counter,
  session-end and causal-edge effects through set-based writes. Session admission
  returns stored row identities; input spelling is not a second equality rule.
  The shared causal-edge writer bounds each statement to 10,922 six-parameter
  rows, preserving parent order across chunks within the caller's transaction.
  All facts and effects commit before durable delivery is acknowledged.
- Cross-session batching couples locks for one bounded database attempt. A
  failed/uncertain batch retains each session's exact retry head; retries use the
  same executor one group at a time. An authoritative session rejection rolls
  back the shared transaction, rejects only that session and retries its peers,
  including during drain. Database failure is not success, and ambiguous commit
  replay still requires canonical payload-hash classification. A batch bound of
  one retains independent-session execution. This writer does not batch the
  synchronous event API or control-plane admission transactions, and fewer SQL
  statements do not imply fewer stored rows or unlimited concurrent executions.
- Poison records should be isolated.
- Invalid records should not block later independent facts.
- Canonical Run lease recovery must preserve execution and effect custody without a derived slot or reclamation clock.
- Run-event append checks persisted idempotency identities in bounded windows
  (64 keys, 16 KiB of key bytes; an indivisible key is queried alone), under
  the existing owner/session/run transaction. Database column equality maps
  matches to input ordinals; Rust does not normalize persisted identities.
  Conflicting immutable payloads abort the entire batch, including across
  windows. Unkeyed events remain distinct. This reduces preflight round trips,
  not retained event rows or payload bytes; exact commit reconciliation retains
  its existing canonical owner.
- Run mutations retain the deletion-fence → Session → Run
  lock order. A caller's exact-owner/session/run locking read also establishes
  existence; it does not need a separate existence query in that transaction.
  Canonical owner-backed appends use their exact-session generation/lease
  UPDATE as the run fence; appends without execution authority still require
  explicit run-existence admission. Generic event appends retain the same
  locked metadata reader but omit the unused checkpoint body. These changes
  reduce SQL exchanges and returned bytes, not retained data or durability.
- Synchronous event creation uses the database's insert receipt for generated
  fields instead of reading the inserted row again. Session admission retains
  the same durable deletion fence.
- Run status snapshots omit checkpoint bodies and read their canonical binding
  and accounting facts in one bounded event query after the metadata lookup.
  A narrow type/index aggregate selects the earliest event of each binding type
  and the latest finalized fact, or latest finished fact only when none exists;
  the selected positions then join back to their owner-scoped payloads.
  At most four payloads are returned. Selection precedes decoding, so corruption
  of a selected fact remains an error rather than authorizing fallback. This is
  a read optimization, not a cache, new authority, retention change, or atomic
  snapshot across the two reads.
- Data deletion must propagate to derived artifacts.
- Decision audits and skill selections acquire canonical non-lazy session
  admission in the same transaction as INSERT. Decision reference checks and
  receipt decoding complete before commit; a receipt failure rolls back the
  insert. Skill catalog resolution precedes BEGIN and the first selected
  skill's resolved version is bound in INSERT, retaining the supplied version
  when unresolved. Neither path allocates sequences or changes event counters;
  an empty skill hook performs no database I/O. Their checked-out connection is
  released only after completed commit or rollback; cancellation closes that
  checkout. A lost commit acknowledgement still has an unknown outcome.

## Versioning

Versioning is used for reproducibility and experimentation, not as a substitute for audit facts.

A reconstructable decision needs:

- prompt/context version;
- memory snapshot or references;
- provider decision facts;
- transcript and trace facts;
- model and parameter metadata.

## Agent event retention

Agent events must have explicit retention class.

Suggested retention classes:

| Class | Examples | Policy |
| --- | --- | --- |
| control_audit | permission, provider decision, run transition | account/audit policy. |
| trace_default | tool lifecycle, retry/cache/sync facts | diagnosis window, then archive/compact. |
| quality_signal | tool quality, eval labels | learning/eval policy after redaction. |
| debug_ref | debug artifact references | follows debug bundle TTL. |
| ephemeral_metric | high-volume counters | aggregate then expire. |

Missing retention class should fail safe by applying a bounded default, not infinite retention.

## Poison and quarantine storage

Invalid or conflicting records should move to quarantine with:

```text
quarantine_id
source
session_id
run_id
event_id
payload_hash
reason
raw_ref_or_summary
repair_options
created_at
expires_at
```

Quarantine must be observable and repairable. It must not block independent valid facts.

## Archival and deletion

Archival should preserve allowed metadata while removing or redacting payloads according to policy. Deletion must propagate to:

- transcript-derived memory;
- learning datasets;
- debug bundles;
- artifact manifests;
- eval cases that include private payloads.

Compressed conversation history is an immutable `context_history_v1` session
artifact, referenced by `session_transcript` with the owning session ID. It
remains reachable until the existing owned-session deletion releases its
references. Server introspect reads bounded UTF-8 windows from the authenticated
artifact store; client-local file paths are not recovery authority.

The unused `storage::delete_agent_event_edges_for_owned_event_ids` source API
is retired. Event deletion keeps edge removal in the existing owner-scoped
transaction, and session deletion retains bounded owner/session cleanup.

The durable run retry counter is retired from the fresh schema and Rust record.
Verification retries retain their `retry_of` and `retry_scope` lineage; recovery
generations and provider-attempt retry accounting remain separate authorities.

## Agent request observations

Ordinary agent model requests and their auxiliary summaries reuse Run execution
authority. Wire identity, dispatch, terminal and usage observations live in the
request and feed existing trace projections. A model attempt is not a second
persistent task. Root startup and cleanup do not load or retire provider-attempt
WALs. Explicit handoff checkpoints carry the complete verified continuation;
Run/Context commits and tool-side-effect admission retain their durable owners.

Session metadata does not enable full provider request/response persistence.
Ordinary Server chat does not automatically publish `llm_capture` artifacts or
full-exchange local journal records. Explicit artifact publication and optional
trace capture keep their existing owners and do not govern Run completion.

Process failure can discard in-flight model output and diagnostics. An absent
usage observation remains unknown and cannot authorize a side effect or establish
a billing fact. Standalone accounting uses its existing ledger boundary.


Schema contract `2026-10-24-v107` retires `run_display_projections`. Fresh schema
creation and Session deletion do not reference it. Bootstrap never drops an
existing table, upgrades a populated schema, or backfills a replacement. Older
schema markers still fail the current-contract check. Runtime display reads
use the captured Run/event watermark and independently read checkpoint metadata;
a newer checkpoint summary during that read is accepted display inconsistency,
never execution or resume authority.

Server opening and terminal commits do not synthesize legacy `agent_events`
`user_query`/`llm_response` roots, including in Explain mode. Explain captures
optional runtime diagnostics; it does not create another conversation ledger.
Conversation history and resume use canonical Context and raw transcript, and
execution/usage use Run. Audit turn views over explicitly ingested client roots
can therefore be empty for Server-only sessions; they do not certify whether a
conversation or execution occurred.

The immutable Run opening uses two canonical `LONGTEXT` columns on its
`agent_run_openings` row: `opening_events_json` holds admission facts and
`opening_context_json` holds the original input and an optional admitted Context
base. Root and delegated Runs use the same input carrier. Generic creation
recaptures admission facts, preserves exact input (including an empty string),
and rejects prebound Context authority; only the Root creation transaction may
bind its captured Context grant. An input alone never authorizes Context access
or skips the existing binding checks. The opening uses one v2 schema and hash
framing, with no historical-data conversion or old-format read path.
Each part is stored once, in the same Run creation transaction. One opening hash covers
the complete logical value, with an aggregate 8 MiB UTF-8 limit including JSON
framing. Status, observation and opening replay read the event column directly;
they do not extract it from a large Context document. Compact metadata reads
explicitly omit both bodies and cannot prove full opening identity. Full reads
reject partial, malformed or hash-mismatched parts. Opening frames retain their
logical cursors without separate event-row writes; late binding and control
facts remain in their existing event owner.

Schema contract `2026-10-24-v108` makes non-null raw transcript source identity
unique within `(user_id, session_id)` using the database column collation. NULL
source identities remain available for imported history. Startup rejects a
non-unique replacement for this constraint. This fresh-schema prerequisite does
not authorize a Run or replace generic immutable-content validation; admitted
Run writes can omit source membership probes only after their complete opening
and terminal authority path is installed.


## Canonical Context content storage

Each immutable `conversation_manifest_nodes` row stores small `manifest_json`
metadata and one non-null `content_json LONGTEXT` containing only that node's
ordered message segments. Existing ordered segment hashes bind the body; no
second hash or mutable projection is stored. Ordinary append writes this turn's
content and advances the existing head in the same transaction. Fork prefixes
and pins continue to share their parent's rows. Repeated content in different
nodes is stored independently, so cross-node content deduplication is retired.

Nodes use the existing scope, compaction generation and conversation sequence
as their physical primary key. Range reads therefore visit only the captured
generation and sequence interval. The existing manifest hash binds the complete
scope and coordinates and has one unique index. Hash-only reads select that
index explicitly while retaining scope predicates; known-coordinate proofs use
the full primary key. These two access paths replace four redundant secondary
indexes without changing any cursor, authority or wire contract.

Common preparation limits a commit to 256 segments, 8 MiB per segment and
32 MiB for the encoded content frame, including JSON framing. Encoding happens
before the transaction. A post-inference resource failure preserves the full
answer, usage and effect disposition in the existing failed Run terminal while
leaving Context unchanged; it never claims success or retries an effect.
Integrity and authority failures retain their distinct rejection paths.

Full reads validate metadata, indexed coordinates and each ordered content
reference. Inherited reads follow the retained fork records, including deleted
ancestors, and enforce captured generation/sequence bounds before reading a body.
Metadata-only attach, fork and delta reads omit the body. Cold ACK
verification reads the exact immutable node, independently of a newer head.
Node deletion removes its body under the existing head/pin/lineage rules.
There are no separate segment or manifest-reference tables, upload staging,
background orphan-body cleanup, historical conversion or old-layout readers.
The non-frozen `segments:batchGet` transport takes a `manifest_root` locator;
standalone `segments:upload` is retired. Frozen MOI wire contracts remain intact.
These per-node bounds do not bound the size of a fully materialized Context.

## Ordinary conversation admission and finish

The model-facing `session` tool exposes configuration and sleep only. Its
optional global history paging, search and around-item actions are retired,
together with their database reader and CLI tool variants. Local CLI journal
commands and authenticated Run reads retain their existing owners.

Existing Server sessions are authenticated and checked for current status in
transactional Run admission. They do not require a preceding display-record
read. New-session quota creation and provider-authorized session resolution keep
their own boundaries. Unbound ordinary requests require binding absence during
canonical Context capture; bound sessions require explicit execution selection.

A new ordinary conversation does not automatically discover personal skills,
the skill catalog, Work bindings, pending Work operations or legacy plans.
Explicit skill selection, authenticated restrictions, explicit Work and recovered
operations still use their canonical owners. The new-Run opening ACK initializes
already-existing guidance and permission observers at its exact empty-control
frontier; a later wake remains pending and recovery hydrates actual history.
Cross-pod quiet startup uses the existing watcher cadence rather than another
synchronous control scan.

Opening and final persistence each use a short transaction. Model requests,
tool execution, history materialization and optional cleanup do not run while
those transactions hold Session locks. Final outcome, canonical Context and raw
conversation commit together so a completed response is available to the next
turn. Optional traces use the bounded asynchronous ingestion owner and its
shutdown drain. Moving work to that owner does not remove it from SQL budgets.

Only observations resolved against the selected canonical builtin implementation
can omit invocation custody. Agent action semantics have one owner in
`astra-turn-types`: `agent list` observes the in-memory child snapshot and does
not consume delivery or require a receive inbox. Argument validation, permission,
capability, cancellation and output governance still apply, including failures.
Other agent actions, provider/Runner calls and actual external effects retain
canonical prepare/dispatch/outcome custody. Public names and provider-authored
readonly metadata cannot grant observation authority.

The receive inbox is opened only for an executable invocation requiring it.
Observation-only turns do not start mailbox polling or cleanup. For a genuinely
used inbox, release transfers ownership immediately into the messaging owner's
existing cancellation-safe exact-token task. Its completion is tracked by the
application drain; final conversation persistence does not await its SQL. Pending
messages and ambiguous delivery custody are retained by that owner.

The public SQL-budget fixture measures the entire request, including BEGIN,
COMMIT and the application's post-stream drain. With an existing authenticated
Session and a scripted provider, a short ordinary turn uses 15 statements with
an empty Context and 17 with retained Context, in two transactions. One or three
readonly discovery/agent-list rounds add no framework statements. Holding the
provider until the real control watcher polls adds one observation statement;
it does not trigger redundant permission or input reads. These are measured
fixture counts, not a zero-SQL guarantee for new Session creation, approval,
remote tools, effects, recovery or opt-in Explain diagnostics.

Remaining historical reads are explicit limitations. Admission checks actual
Run status, live owners and unresolved tool custody under the existing fences;
the raw transcript writer still allocates its Session sequence with MAX. On
MatrixOne 4.2.5, a tenant-prefix predicate can bypass a selective secondary
index, including a forced index (matrixorigin/matrixone#31046). A generated-key
workaround was rejected because interleaved-history testing still scanned base
rows when its index returned nothing. No generated occupancy fields, new owner
state or transcript counters are added to mask that behavior. The current
schema therefore retains the existing admission and raw-history owners; this
PR does not claim all remaining queries have history-independent cost.

## Retired authoring and adaptation state

Fresh schema contract `2026-10-31-v117` removes `agent_agents`,
`user_skill_sources`, `user_skill_versions`, `session_state_items` and
`model_router_deployments`, and removes the state-item artifact pin counter.
Executable AgentBindings, Skills, Work graphs, explicit Memory, canonical Run
and Context records remain with their existing owners. Session state reads no
longer aggregate state-item summaries or user-anchor projections. Platform
snapshot reads no longer query the Agent configuration catalog. Configured Auto
routing no longer queries a deployment registry.

The existing bootstrap contract rejects earlier database versions before Server
state, listeners or sweepers start. This is a fresh-database cutover, with no
migration or compatibility path. It does not transfer existing users, Sessions,
Runs, Work or artifacts. Keep old databases and backups separate; stop old pods
and settle in-flight custody/effects before switching deployments. Never point
the new retention sweeper at an unbootstrapped old pool: direct library helpers
are not Server startup and do not perform contract admission themselves.

Artifact retention keeps citation counters and durable reference edges. The
retired `state_item` reference kind is rejected; retained kinds continue to pin
payloads until their owning lifecycle releases them.

# Data and storage

> Status: target design contract.
> Last updated: 2026-07-07.

Data and storage owns platform state layering, MatrixOne usage, retention, archival, versioning, and data correctness boundaries.

## State layers

| Layer | Purpose |
| --- | --- |
| C0 control | sessions, runs, tasks, checkpoints, leases, execution slots. |
| C1 transcript | user-visible messages and transcript items. |
| C2 audit facts | durable events, permissions, provider decisions. |
| C3 trace facts | execution trace and diagnostic events. |
| C4 debug bundle | short-lived raw diagnostics and manifests. |
| C5 learning artifacts | redacted, consent-gated derived data. |

Delegation topology, status, retry lineage, usage, and terminal evidence are owned
by `agent_runs` and `agent_run_events`. Run admission and owner-fenced transitions
persist these canonical facts; there is no separate delegation table or
delegation-state mirror. The run display projection and bounded terminal-receipt
repair remain derived from canonical run facts.

Session state projections remain owned by personal-skill activation and
user-anchor memory. The unused general state-item upsert API and generic mutation
validator are retired. Session lifecycle fencing, projection reads, artifact
reference accounting, and retention remain owned by the existing storage
boundaries.

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

## MatrixOne role

MatrixOne is the platform state store and analytic substrate. It should support:

- durable runtime state;
- indexed transcript and event queries;
- audit and replay;
- retention and purge jobs;
- analytic/evaluation workloads;
- optional MatrixOne-native value such as time travel or hybrid search when appropriate.

## Schema bootstrap

Bootstrap supports a fresh database or the current schema contract. Canonical
CREATE declarations define the complete table shape; startup validates it
without ALTER-based upgrades, historical backfills, or retired-table cleanup.
An older completion marker is rejected and requires a new database. Interrupted
fresh bootstrap can retry under the existing database lease, with readiness
published only after schema validation succeeds.

Agent metadata is persisted by the database service. Its unused alternative
in-memory CRUD implementation and self-tests are retired; the unconfigured
service remains explicit. The owner/name unique constraint is enforced by the
database, and create or rename conflicts now return HTTP 409 rather than 500.
Other SQL and malformed persisted-data errors retain their 500 behavior.

## Transcript persistence

Fresh schema contract `2026-10-07-v100` stores transcript items and their
committed projection head. Physical page metadata and the unused source event
position column are removed. The run lookup index is `(user_id, run_id)`.
Model thinking observations use only the configuration-bound snapshot; obsolete
capability and error mirror columns are absent.
The retired tool-result projection decision/receipt tables and special event
field are absent. Current provider-wire and accepted-to-terminal recovery
contracts remain unchanged.
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
or synthesize a committed head from the newest item. The bounded root-only
transcript restore fallback remains available when canonical history is absent.

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
- Session event counters commit in the same transaction as their inserted event
  rows, including terminal settlement and trace repair. Replay contributes no
  insertion delta; a lost commit acknowledgement must not trigger a separate
  counter update.
- Terminal capture batches core conversation events and LLM/tool trace events
  through the same writer, sharing session admission and insertion readback.
- Poison records should be isolated.
- Invalid records should not block later independent facts.
- Slot/lease semantics must avoid permanent deadlock through expiry or repair.
- Run mutations retain the deletion-fence → session → execution-slot → run
  lock order. A caller's exact-owner/session/run locking read also establishes
  existence; it does not need a separate existence query in that transaction.
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

## Router deployment state

Fresh schema contract `2026-09-27-v90` adds `model_router_deployments`, keyed by
owner, with a compare-and-swap revision and typed deployment/revocation JSON. This
is C0 tuning control state. Mutations and existing `auth_audit_logs` entries share
one transaction. Routing decisions and reviewed outcomes continue to use
`agent_run_events`; dashboard queries are bounded projections of that ledger.
As with the canonical bootstrap policy above, older schema markers are rejected;
this change does not implement an in-place migration or backfill.

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

# Data versioning

> Status: target design contract.
> Last updated: 2026-07-07.

Data versioning defines how Astra makes agent decisions reproducible across changing prompts, memory, provider state, tools, and user data.

The legacy sandbox restore endpoint is also retired: authenticated session access does not authorize restoring the configured service database. Database administration must use an explicit administrative boundary.

## Principle

Reproducibility requires versioned inputs and durable facts, not only a transcript.

```text
Decision = prompt version + context snapshot + memory refs + provider decisions + model params + tool facts
```

## Versioned inputs

Track versions or stable references for:

- prompt contract and dynamic context blocks;
- tool schema and capability decisions;
- skill package versions;
- memory records and retrieval query;
- transcript slice;
- artifacts and file references;
- model and parameters;
- policy and permission state.

## Snapshot types

| Snapshot | Purpose |
| --- | --- |
| Context snapshot | What the model saw. |
| Provider snapshot | What tools/capabilities were available and why. |
| Memory snapshot | Which memories were retrieved and with what scores. |
| Artifact manifest | Which external or large objects were referenced. |
| Policy snapshot | Permissions, plan mode, and safety policy. |

## Work branches and database administration

Work branches use the owner-scoped Work contracts and canonical recovery-point
capture. The legacy `/branches` create, diff, merge, delete, and cost-estimate
routes are retired. They are not database administration: snapshot restore must
never be exposed as an authenticated Work merge, and estimated costs require
actual model pricing and usage evidence.

## Branching and experimentation

Versioning enables safe experiments:

- prompt candidate replay;
- skill version comparison;
- memory loading strategy comparison;
- provider routing policy comparison;
- model routing comparison.

Experiments should not mutate production state until activated through evaluation gates.

## Replay contract

A replay should be able to reconstruct:

- input transcript;
- context blocks;
- provider decisions;
- tool result envelopes;
- model config;
- output and trace facts.

If an external provider cannot be replayed exactly, the replay must mark that dependency as simulated, unavailable, or substituted.

## Deletion and retention

Versioning must respect deletion:

- deleted user data invalidates derived snapshots or masks content;
- C4 debug data expires by TTL;
- C5 learning artifacts must preserve lineage for deletion propagation;
- audit facts may retain metadata according to policy without retaining raw private payloads.

## Execution-local database rollback

CLI and Server use the same SQL statement scanner, snapshot journal and rollback
planner. Quoted SQL and comments do not create statement boundaries. Mutating
statements, including `LOAD` and writes after a read statement, capture a snapshot
before execution; destructive-operation admission remains a separate check.
Connection selection and credentials remain on the selected execution adapter.

Capture, journal recording, query execution and rollback serialize on the existing
execution-local journal. A successful restore marks affected later snapshots for
cleanup only. Failed cleanup retains those records and retries `DROP`, never a
second `RESTORE`. Rollback results expose `restore_completed` so database recovery
is distinguishable from unfinished cleanup. This journal is execution-local and
does not promise recovery across a process restart.

## Event lineage

Lineage queries return owner-scoped persisted events and their canonical parent
relationships. They do not read process-local session files or synthesize a
contribution score from changed snapshot references. The former optional
`contribution_score` response field is retired; state-reference changes alone
do not establish an event's causal contribution. Checkpoint and snapshot
persistence retain their existing recovery responsibilities.

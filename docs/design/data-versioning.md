# Data versioning

> Status: target design contract.
> Last updated: 2026-07-07.

Data versioning defines how Astra makes agent decisions reproducible across changing prompts, memory, provider state, tools, and user data.

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

## Unified snapshot identity

A snapshot is an immutable manifest, not a timestamp. Astra addresses the
manifest with a UUIDv7 `snapshot_id`, which is useful for ordering, idempotent
requests, and cross-edge observation. The manifest also records the immutable
component references that make the state reproducible:

```text
workspace: git commit/tree hash
memory:    owner/trial Memoria branch + base revision
data:      MatrixOne snapshot/branch + base revision
context:   canonical input hash
policy:    model/provider/tool-policy hashes
```

The canonical manifest has a separate `snapshot_fingerprint` content hash for
equality and tamper detection. A `trial_id` identifies a case/arm/repetition
execution and points to the snapshot it actually used; it is not an alias for
the snapshot. Mutable components are branched or copy-on-write within the
owner/trial namespace. A timestamp remains metadata for ordering and
retention, never the sole identity or evidence of isolation.

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

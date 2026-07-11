# MatrixOne-native paradigm

> Status: target design contract.
> Last updated: 2026-07-07.

MatrixOne is not only a persistence backend. Astra should use it as a unified state, analytics, replay, and governance substrate when that produces simpler and more reliable agent infrastructure.

## Principles

- Use the database as a correctness boundary for durable facts.
- Prefer queryable facts over opaque blobs when the data needs audit, replay, or learning.
- Keep raw debug data out of normal fact tables.
- Separate platform state from user/workspace data authority.
- Treat advanced MatrixOne features as leverage, not as accidental coupling.

## Platform state

MatrixOne stores Astra-owned platform facts:

- identity and authorization metadata;
- sessions, runs, tasks, checkpoints, leases;
- transcript items;
- audit and trace events;
- artifacts metadata;
- provider decisions;
- eval and tuning metadata;
- retention and deletion lineage.

## Queryable facts over blobs

Queryable tables should be used when the system needs:

- replay;
- audit;
- support diagnosis;
- evaluation;
- retention policy;
- conflict detection;
- lineage.

Opaque artifacts should be used for:

- large raw payloads;
- debug bundles;
- binary captures;
- temporary diagnostics;
- private payloads that should not enter normal analytics.

## Native leverage

Astra may use MatrixOne-native capabilities for:

- hybrid search over memory and artifacts metadata;
- time-travel style reproducibility when available;
- analytic scans over trace/eval data;
- retention and purge jobs;
- versioned experiment comparisons;
- materialized projections for UI performance.

These capabilities should remain behind design contracts so the agent backbone is not tied to a single SQL trick.

## Data ownership

Platform state is not the same as user workspace data. A provider may expose user data to the agent through explicit authority, but that does not mean Astra owns or copies all user data into platform tables.

## Anti-patterns

- Storing everything as JSON because schema design is inconvenient.
- Storing raw debug output in audit facts.
- Treating analytics tables as source-of-truth control state.
- Letting DB feature experiments leak into tool/provider semantics.
- Using training datasets without deletion lineage.

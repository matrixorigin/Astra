# audit-cloud-sync

Audit edge-cloud synchronization in **astra**. Verifies data integrity between local
journal/checkpoints and MatrixOne cloud tables.

**Architecture reference (restore, composite snapshots, skill registry vs cloud):** [rust/docs/edge-cloud-sync-architecture.md](../../rust/docs/edge-cloud-sync-architecture.md) — especially §8–§8.5.

## Usage

```
/skill audit-cloud-sync
/skill audit-cloud-sync --aspect events
/skill audit-cloud-sync --aspect learning
/skill audit-cloud-sync <session-id> --aspect checkpoints
```

## What It Audits

| Aspect | Local Source | Cloud Target | Check |
|--------|-------------|-------------|-------|
| **Events** | `~/.astra/sessions/<id>.jsonl` | `agent_events` | Event count match, expansion, ingestion rate |
| **Learning** | `~/.astra/learning/<profile>.json` | `learning_snapshots` | Version match, delta sync, conflicts |
| **Checkpoints** | `~/.astra/sessions/<id>/step_checkpoints/*-heavy.json` (+ `checkpoints/` markdown) | `session_checkpoints` | Coverage, recoverability, consistency |
| **Tasks** | Journal PlanProgress events | `agent_tasks` | Active task sync, orphan detection |

## Sync Architecture

```
Edge writes locally FIRST (always) → async push to cloud
Cloud is authoritative for cross-session data (learning, preferences)
Event ingestion: batch INSERT IGNORE (idempotent, deduped by event_id)
Learning sync: versioned with optimistic locking, gzip+base64 compressed
```

## Key Metrics

- **Event ingestion rate**: Target >95% of local events in cloud
- **Learning version**: Local and cloud versions should match
- **Checkpoint coverage**: Target checkpoint every 2-3 turns
- **Sync success rate**: From `session_sync_log` audit trail

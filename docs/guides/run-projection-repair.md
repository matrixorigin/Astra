# Run Projection Repair Runbook

This runbook covers stale or missing durable run display projections.

## Model

`run_display_projections` is a derived read model. It exists to make run list
and run detail reads cheap, but it is not the source of truth.

Authoritative run facts are:

- `agent_runs` for the durable run status, owner, session, counters, and error.
- `agent_run_events` for ordered durable run events.
- `run_checkpoints` for the latest recoverable checkpoint.

Do not repair an incident by directly changing `run_display_projections` to a
desired status. Rebuild the projection from the durable facts instead.

## Impact

A failed projection refresh can make a run list or projection response look
stale while the durable run state is already correct. The terminal state must
not be rolled back or overwritten because the projection write failed.

Expected user-facing symptoms include:

- A run detail response has `observability.projection_lag_events > 0`.
- A session response has `observability.active_run_projection_lag_events > 0`.
- A list view shows a stale status while `GET /chat/runs/{run_id}` reports the
  correct durable status.

## Alert Signals

Alert on repeated warning logs containing any of these messages:

- `run status committed but display projection refresh failed`
- `run status CAS committed but display projection refresh failed`
- `run transition committed but display projection refresh failed`

Each warning includes `user_id`, `run_id`, and `error` fields.

Also alert when projection lag remains positive after a short retry window:

```bash
curl -sS \
  -H "Authorization: Bearer $ASTRA_TOKEN" \
  "$ASTRA_API_BASE/chat/runs/$RUN_ID/projection?recent_limit=20" \
  | jq '.observability'
```

For direct database triage, compare the durable event high watermark with the
projection index:

```sql
SELECT
  r.user_id,
  r.run_id,
  r.status AS durable_status,
  r.last_event_idx AS durable_event_idx,
  COALESCE(p.projection_event_idx, -1) AS projection_event_idx,
  r.last_event_idx - COALESCE(p.projection_event_idx, -1) AS lag_events,
  p.status AS projection_status,
  p.latest_event_type,
  p.updated_at AS projection_updated_at
FROM agent_runs r
LEFT JOIN run_display_projections p
  ON p.user_id = r.user_id
 AND p.run_id = r.run_id
WHERE r.user_id = ?
  AND r.run_id = ?;
```

`lag_events > 0` or a missing projection row means the read model is stale or
absent. The durable status in `agent_runs` remains authoritative.

## Repair

Use the repair endpoint. It verifies run ownership, rebuilds the projection from
durable run facts, and returns the repaired projection.

```bash
curl -sS -X POST \
  -H "Authorization: Bearer $ASTRA_TOKEN" \
  "$ASTRA_API_BASE/chat/runs/$RUN_ID/projection/repair?recent_limit=20" \
  | jq '{repaired, status: .projection.status, observability: .projection.observability}'
```

Expected successful response shape:

```json
{
  "repaired": true,
  "status": "failed",
  "observability": {
    "has_durable_projection": true,
    "projection_lag_events": 0
  }
}
```

After repair, re-read the projection and confirm:

- `observability.has_durable_projection == true`
- `observability.projection_lag_events == 0`
- `status`, `error_message`, `latest_event_type`, and checkpoint fields match
  the durable facts.

## Unhappy Paths

If repair returns `404`, the authenticated user does not own the run or the run
does not exist in durable storage.

If repair returns `503`, the rebuild query or projection upsert failed. Check
database connectivity and the related rows in `agent_runs`, `agent_run_events`,
and `run_checkpoints`.

If repair succeeds but lag becomes positive again, the write path is repeatedly
failing after durable commits. Treat this as a service or database incident, not
as data corruption in the durable run facts.

If the projection row is missing but `GET /chat/runs/{run_id}/projection` still
returns useful data, the response was synthesized from durable run facts. Run the
repair endpoint so list queries and indexed projection reads are restored.

## Verification

Fast HTTP tests:

```bash
cargo test --manifest-path rust/Cargo.toml -p astra-runtime \
  repair_run_projection_http_rebuilds_and_returns_projection --lib -- --nocapture
```

MatrixOne-backed repair test:

```bash
ASTRA_TEST_DB_IT=1 cargo test --manifest-path rust/Cargo.toml -p astra-runtime \
  repair_run_projection_http_repairs_real_database_projection --lib -- --ignored --nocapture
```

Service-level rebuild test:

```bash
cargo test --manifest-path rust/Cargo.toml -p astra-services \
  rebuild_run_projection_repairs_stale_projection_from_facts --lib -- --nocapture
```

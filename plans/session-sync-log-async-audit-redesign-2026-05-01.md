# session_sync_log: async audit with DB-level retention

Status: design
Date: 2026-05-01
Supersedes: `session-sync-log-prune-hotpath-2026-05-01.md` (Option A probabilistic prune)

## Root Cause

Audit writes (INSERT) and retention maintenance (DELETE) are synchronous in the push critical path. This is a design flaw — audit is observability, not business logic. It must never block or contend with the operation it observes.

## Design Principles

1. **Audit writes are fire-and-forget** — losing an audit row is acceptable; blocking a push is not.
2. **Retention is the DB's job** — application-level prune queries are unnecessary complexity.
3. **Zero prune queries in application code** — remove `build_sync_log_prune_query` entirely.

## Architecture

```
push_session_state_to_cloud()
  ├── INSERT session state (business) ← sync, must succeed
  └── tx.send(AuditEntry)            ← non-blocking, bounded channel

Background:
  AuditFlusher (tokio task)
    └── batch INSERT every 1s or 64 entries (whichever first)
    └── no prune — DB handles retention via TTL or scheduled cleanup

DB-level:
  storage::run_cleanup() already does time-based DELETE (sync_log_days policy)
  → that's the only retention mechanism needed
```

## Detailed Design

### 1. `AuditEntry` struct

```rust
pub(crate) struct SyncAuditEntry {
    pub user_id: String,
    pub session_id: String,
    pub sync_type: String,
    pub direction: SyncDirection,
    pub payload_size: usize,
    pub status: String,
    pub error_message: Option<String>,
}
```

### 2. `SyncAuditWriter`

```rust
pub(crate) struct SyncAuditWriter {
    tx: tokio::sync::mpsc::Sender<SyncAuditEntry>,
}

impl SyncAuditWriter {
    /// Non-blocking send. If channel is full, drop the entry (log a counter).
    pub fn log(&self, entry: SyncAuditEntry) {
        if self.tx.try_send(entry).is_err() {
            // Metric: audit_entries_dropped += 1
            tracing::debug!(target: "astra_services::audit", "sync audit channel full, entry dropped");
        }
    }
}
```

### 3. `SyncAuditFlusher` (background task)

```rust
pub(crate) async fn run_audit_flusher(
    mut rx: tokio::sync::mpsc::Receiver<SyncAuditEntry>,
    pool: sqlx::Pool<sqlx::MySql>,
) {
    let mut buf = Vec::with_capacity(64);
    loop {
        // Drain up to 64 entries or wait 1s timeout
        let deadline = tokio::time::sleep(Duration::from_secs(1));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                entry = rx.recv() => {
                    match entry {
                        Some(e) => {
                            buf.push(e);
                            if buf.len() >= 64 { break; }
                        }
                        None => {
                            // Channel closed, flush remaining and exit
                            flush_batch(&pool, &mut buf).await;
                            return;
                        }
                    }
                }
                _ = &mut deadline => { break; }
            }
        }
        flush_batch(&pool, &mut buf).await;
    }
}

async fn flush_batch(pool: &sqlx::Pool<sqlx::MySql>, buf: &mut Vec<SyncAuditEntry>) {
    if buf.is_empty() { return; }
    // Single multi-row INSERT for the batch
    // INSERT INTO session_sync_log (...) VALUES (?, ...), (?, ...), ...
    // On error: log warning, discard batch (audit is best-effort)
    buf.clear();
}
```

### 4. Integration into `MatrixOneSyncService`

```rust
pub struct MatrixOneSyncService {
    pool: sqlx::Pool<sqlx::MySql>,
    audit: SyncAuditWriter,
}

impl MatrixOneSyncService {
    pub fn new(pool: sqlx::Pool<sqlx::MySql>) -> (Self, tokio::task::JoinHandle<()>) {
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        let handle = tokio::spawn(run_audit_flusher(rx, pool.clone()));
        (Self { pool, audit: SyncAuditWriter { tx } }, handle)
    }

    // log_sync becomes:
    fn log_sync(&self, ...) {
        self.audit.log(SyncAuditEntry { ... });
    }
}
```

### 5. Integration into `session_restore.rs`

`log_session_sync` becomes a method on a shared `SyncAuditWriter` passed into the push functions, instead of a standalone async fn that takes a pool.

```rust
// Before: await log_session_sync(pool, ...).await
// After:  audit.log(SyncAuditEntry { ... })
```

No await. No error handling needed at call site. No Result return.

### 6. Remove all prune code

Delete entirely:
- `build_sync_log_prune_query()`
- `sync_log_retain_limit()`
- `should_prune_sync_log()`
- `SYNC_LOG_PRUNE_INVERSE_PROBABILITY`
- `SYNC_LOG_SUCCESS_RETAIN` / `SYNC_LOG_ERROR_RETAIN`
- `prune_sync_logs()` method

Retention is handled solely by `storage::run_cleanup()` which already does:
```sql
DELETE FROM session_sync_log WHERE created_at < DATE_SUB(NOW(6), INTERVAL ? DAY) LIMIT ?
```
This is the correct mechanism — time-based, batched, runs on a schedule (not per-write).

### 7. Graceful shutdown

The API server's shutdown sequence must:
1. Drop all `SyncAuditWriter` clones (closes channel sender)
2. Await the flusher `JoinHandle` (drains remaining buffer)

This ensures no audit entries are lost on clean shutdown.

## Changes Summary

| File | Change |
|------|--------|
| `state_sync.rs` | Add `SyncAuditEntry`, `SyncAuditWriter`, `run_audit_flusher`. Remove `build_sync_log_prune_query`, `sync_log_retain_limit`, `prune_sync_logs`, retain constants. Change `MatrixOneSyncService::new` signature. |
| `session_restore.rs` | Replace `log_session_sync` / `log_checkpoint_sync` async fns with `audit.log(...)` calls. Remove prune logic. |
| `storage.rs` | No change — `run_cleanup` already handles time-based retention. |
| `Cargo.toml` | Remove `fastrand` (no longer needed). |
| Tests | Update `session_sync_log_prune_partitions_by_sync_type_on_live_matrixone` — rewrite as a test of `run_audit_flusher` batch behavior. Remove prune-specific unit tests. |

## Performance Impact

| Metric | Before (current main) | After |
|--------|----------------------|-------|
| push_session_state latency | business INSERT + audit INSERT + prune DELETE | business INSERT only |
| Concurrent test contention | 22× DELETE on same partition | zero — audit is buffered |
| Audit write I/O | 1 INSERT per push (sync) | 1 batch INSERT per 64 pushes or 1s |
| Worst-case audit loss | none (sync) | up to 256 entries on crash (acceptable) |

## Testing Strategy

1. Unit test: `SyncAuditWriter::log` is non-blocking even when channel full
2. Unit test: `run_audit_flusher` flushes on batch-size threshold
3. Unit test: `run_audit_flusher` flushes on timeout (1s)
4. Unit test: graceful shutdown drains buffer
5. Integration test: after N push operations + flush, DB has N audit rows
6. Existing `run_cleanup` test covers time-based retention (unchanged)

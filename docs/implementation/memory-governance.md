# Memory Governance Implementation

> **Status**: Complete  
> **Last Updated**: 2026-02-20

---

## Overview

Memory governance is the automated enforcement of retention policies, confidence decay, and cleanup across all memory layers. It runs as scheduled background tasks with distributed locking for multi-instance safety.

## Architecture

### Components

```
MemoryGovernanceScheduler (façade)
    ├── SchedulerBackend (abstract)
    │   ├── AsyncIOBackend (default, dev/small deploy)
    │   └── (pluggable: Celery, Temporal, K8s CronJob, etc.)
    └── GovernanceTaskRunner
        ├── Acquires distributed lock (distributed_locks table)
        ├── Executes MemoryGovernanceEngine.run_*_tasks()
        └── Releases lock
```

### Task Schedule

| Task | Interval | Lock Name | Actions |
|---|---|---|---|
| **Hourly** | 3600s | `governance_hourly` | Archive closed working memory, purge sensory buffer |
| **Daily** | 86400s | `governance_daily` | Confidence decay, quarantine low entries, compress episodic |
| **Weekly** | 604800s | `governance_weekly` | T1 verification, contradiction scan, health reports |

### Distributed Locking

**Table Schema:**
```sql
CREATE TABLE distributed_locks (
  lock_name VARCHAR(64) PRIMARY KEY,
  instance_id VARCHAR(64) NOT NULL,
  acquired_at DATETIME NOT NULL,
  expires_at DATETIME NOT NULL,
  task_name VARCHAR(64) NOT NULL,
  INDEX idx_task_name (task_name)
);
```

**Lock Lifecycle:**
1. **Acquire**: Try INSERT new lock (lock_name is PK)
   - Success → execute task
   - Duplicate → check if expired
     - Expired → UPDATE to take over
     - Active → SKIP (another instance holds it)

2. **Release**: DELETE lock after task completes

3. **Expiry**: 5 minutes (LOCK_HEARTBEAT_TIMEOUT)
   - If instance crashes, lock auto-expires
   - Next instance can take over

**Guarantees:**
- Exactly-once execution per cycle across N instances
- No single point of failure (no external coordinator needed)
- Automatic recovery from crashed instances

## Usage

### Default (Single-Process, Dev)

```python
# In api/main.py lifespan:
from core.context import MemoryGovernanceScheduler

scheduler = MemoryGovernanceScheduler()
await scheduler.start()   # Spawns 3 asyncio tasks
...
await scheduler.stop()
```

### Custom Backend (Production)

```python
from core.context import (
    GovernanceTaskRunner,
    SchedulerBackend,
    MemoryGovernanceScheduler,
)
from api.database import get_db_context

# Implement your backend
class CeleryBackend(SchedulerBackend):
    def __init__(self, runner: GovernanceTaskRunner):
        self.runner = runner
    
    async def start(self, tasks: dict[str, int]):
        for name, interval in tasks.items():
            celery_app.conf.beat_schedule[name] = {
                'task': 'governance.run',
                'schedule': interval,
                'args': [name],
            }
    
    async def stop(self):
        pass

# Wire it up
runner = GovernanceTaskRunner(get_db_context)
backend = CeleryBackend(runner)
scheduler = MemoryGovernanceScheduler(backend=backend)
await scheduler.start()
```

## Task Details

### Hourly Tasks

**Archive Closed Working Memory**
- Query `AgentScratchpad` with `status == "completed"`
- Mark as archived (soft delete)
- Frees up active working memory for new tasks

### Daily Tasks

**Confidence Decay**
```python
confidence(t) = initial_confidence × 0.5^(days_since_validation / half_life)
```
- Recalculate for all knowledge entries
- Update `confidence` and `updated_at` columns
- Entries below 0.3 threshold queued for quarantine

**Quarantine Low Confidence**
- Find entries with `confidence < 0.3`
- Log for manual review or automated revalidation
- Excluded from retrieval until revalidated

**Compress Episodic Events**
- Find events older than 90 days
- Compress to session summaries (future: LLM-generated)
- Reduces storage, preserves audit trail

### Weekly Tasks

**T1 Auto-Verification**
- Find knowledge entries with `trust_tier == "T1"`
- Re-fetch source URLs/APIs
- Compare against stored content
- Flag contradictions

**Contradiction Scan**
- Group knowledge entries by (category, key_name)
- Find groups with multiple different values
- Log for manual review

**Health Reports**
- Per-user memory statistics:
  - Total entries
  - Average confidence
  - Low confidence count
  - Contradictions found
- Log as INFO for monitoring

## Configuration

### Environment Variables

```bash
# Lock heartbeat timeout (seconds)
GOVERNANCE_LOCK_TIMEOUT=300

# Task intervals (seconds)
GOVERNANCE_HOURLY_INTERVAL=3600
GOVERNANCE_DAILY_INTERVAL=86400
GOVERNANCE_WEEKLY_INTERVAL=604800
```

### Trust Tier Half-Lives

```python
TRUST_TIER_HALF_LIVES = {
    "T1": 365,   # Verified: official docs, verified APIs
    "T2": 180,   # Curated: human-reviewed, team knowledge
    "T3": 60,    # Inferred: agent-extracted, LLM summaries
    "T4": 30,    # Unverified: raw user input
}
```

## Monitoring & Observability

### Governance Stats (Acceptance Indicators)

```python
from core.context.lifecycle import MemoryGovernanceEngine

engine = MemoryGovernanceEngine(db)
stats = engine.governance_stats()
# {
#   "total_entries": 1200,
#   "avg_confidence": 0.72,
#   "min_confidence": 0.08,
#   "quarantined": 45,
#   "quarantine_pct": 3.8,
#   "tier_distribution": {"T1": 120, "T2": 400, "T3": 580, "T4": 100},
#   "contradictions": 2
# }
```

**Acceptance criteria:**
- `avg_confidence` should stay above 0.5 (decay is working but not over-aggressive)
- `quarantine_pct` < 10% (healthy knowledge base)
- `contradictions` trending toward 0 (weekly scan is resolving conflicts)

### Governance Run History (Trend Tracking)

Every `GovernanceTaskRunner.run()` persists results to `governance_runs`:

```sql
CREATE TABLE governance_runs (
  task_name VARCHAR(32) NOT NULL,
  result JSON NOT NULL,
  created_at DATETIME NOT NULL,
  INDEX idx_task_created (task_name, created_at)
);

-- Trend query: daily quarantine count over last 30 days
SELECT DATE(created_at) AS day,
       JSON_EXTRACT(result, '$.quarantined') AS quarantined
FROM governance_runs
WHERE task_name = 'daily'
  AND created_at > DATE_SUB(NOW(), INTERVAL 30 DAY)
ORDER BY day;
```

### Logs

All governance actions are logged:
```
INFO: Governance [hourly]: {'archived_notes': 5}
INFO: Governance [daily]: {'decayed_entries': 42, 'quarantined': 3, 'compressed_events': 128}
INFO: Governance [weekly]: {'contradictions_found': 1, 'health_reports': 12}
```

### Metrics

Track in your monitoring system:
- `governance_task_duration_ms` — execution time per task
- `governance_lock_contention` — how often locks are held
- `governance_entries_decayed` — confidence decay volume
- `governance_entries_quarantined` — low confidence entries
- `governance_contradictions_found` — data quality issues

### Health Checks

```python
# Query lock table to see which instance holds which lock
SELECT lock_name, instance_id, acquired_at, expires_at
FROM distributed_locks
WHERE expires_at > NOW();

# Check for stale locks (should be empty)
SELECT lock_name, instance_id, expires_at
FROM distributed_locks
WHERE expires_at < NOW();
```

## Testing

### Unit Tests

```python
# Test task runner with mocked DB
from core.context import GovernanceTaskRunner

runner = GovernanceTaskRunner(mock_db_context)
result = runner.run("hourly")
assert result is not None
assert "archived_notes" in result
```

### Integration Tests

```python
# Test with real DB
from core.context import MemoryGovernanceScheduler

scheduler = MemoryGovernanceScheduler()
await scheduler.start()
await asyncio.sleep(1)
await scheduler.stop()
```

### Distributed Lock Tests

```python
# Test lock acquisition
runner = GovernanceTaskRunner(get_db_context)
result1 = runner.run("hourly")  # Should succeed
result2 = runner.run("hourly")  # Should skip (lock held)
assert result1 is not None
assert result2 is None
```

## Troubleshooting

### Lock Stuck

**Symptom**: Tasks always skip, lock never releases

**Diagnosis**:
```sql
SELECT * FROM distributed_locks WHERE expires_at > NOW();
```

**Fix**: Delete stale lock
```sql
DELETE FROM distributed_locks WHERE lock_name = 'governance_hourly';
```

### Tasks Not Running

**Symptom**: No governance logs, memory not decaying

**Diagnosis**:
1. Check scheduler started: `await scheduler.start()` called?
2. Check logs for errors: `grep "Governance" app.log`
3. Check lock table: any locks acquired?

**Fix**:
- Verify `MemoryGovernanceScheduler` initialized in `api/main.py` lifespan
- Check database connectivity
- Verify `distributed_locks` table exists

### High Lock Contention

**Symptom**: Many instances, tasks frequently skip

**Diagnosis**:
```sql
SELECT lock_name, COUNT(*) as attempts
FROM governance_events
WHERE action = 'lock_skip'
GROUP BY lock_name;
```

**Fix**:
- Increase task interval (reduce frequency)
- Reduce number of instances (scale down)
- Implement custom backend with better scheduling (Celery, Temporal)

## Future Enhancements

- [ ] Metrics export (Prometheus)
- [ ] Configurable task intervals per environment
- [ ] Celery backend implementation
- [ ] Temporal backend implementation
- [ ] K8s CronJob backend implementation
- [ ] Governance event audit trail
- [ ] Per-tenant governance policies
- [ ] Adaptive decay based on usage patterns

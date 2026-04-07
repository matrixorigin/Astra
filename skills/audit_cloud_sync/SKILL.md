---
name: audit-cloud-sync
description: "Developer skill: audit edge-cloud synchronization in astra — event ingestion, learning sync, checkpoint recovery, conflict resolution, sync failures. Verifies data integrity between local journal and cloud tables."
user_invocable: true
when_to_use: "When the user wants to audit edge-cloud sync, check event ingestion, or diagnose sync failures"
arguments:
  - name: TARGET
    description: "Session ID, or 'last'. Omit for most recent session."
    required: false
  - name: ASPECT
    description: "Audit focus: 'events', 'learning', 'checkpoints', 'tasks', 'all' (default: all)"
    required: false
allowed_tools:
  - bash
  - read_file
  - grep
  - glob
---
# Audit Cloud Sync

Audit edge-cloud synchronization in astra. Verifies that local journal events, learning
snapshots, checkpoints, and task state are correctly synced to MatrixOne cloud tables.
Identifies sync gaps, conflicts, failures, and data integrity issues.

For **HybridRestoreService**, Step Protocol vs services `RestoredSession`, composite snapshots, and **skill** paths (registry vs `GET /skills` vs cloud prompt index), read **`rust/docs/edge-cloud-sync-architecture.md`** §8–§8.5 in the repo.

## Task

$ARGUMENTS

---

## Phase 1: Map the Sync Landscape

### 1.1 Astra's Local-First Architecture

```
Edge (Local)                         Cloud (MatrixOne)
────────────                         ──────────────────
~/.astra/sessions/<id>.jsonl    ──►  agent_events (batch INSERT IGNORE)
~/.astra/sessions/<id>/         ──►  session_checkpoints
~/.astra/learning/<profile>.json──►  learning_snapshots (versioned, gzip+base64)
~/.astra/user_prefs             ◄──  user_preferences (pull on start)
  (plan state in journal)       ──►  agent_tasks (task records)
  (session metadata)            ──►  agent_sessions (UPSERT)
                                ──►  session_sync_log (audit trail)
```

**Principle**: Edge writes locally first (always), then async pushes to cloud.
Cloud is authoritative for cross-session data (learning, preferences).

### 1.2 Locate Local Data

```bash
# Session journal
ls -la ~/.astra/sessions/*.jsonl 2>/dev/null | tail -5

# Learning snapshots
ls -la ~/.astra/learning/*.json 2>/dev/null

# Session checkpoints
ls -la ~/.astra/sessions/*/step_checkpoints/ 2>/dev/null | tail -10

# User preferences
cat ~/.astra/user_prefs 2>/dev/null | head -20
```

---

## Phase 2: Event Ingestion Audit

### 2.1 Event Ingestion Pipeline

```
JournalEvent (edge)
    │
    ▼ journal.write()  [sync, local JSONL]
    │
    ▼ pusher.enqueue()  [async channel, capacity=200]
    │
    ▼ EventIngestionWorker  [background tokio task]
    │  ├─ buffer accumulates events
    │  ├─ flush when: buffer ≥ 20 OR 5 seconds elapsed
    │  └─ retry: 3 attempts, exponential backoff (100ms → 2s)
    │
    ▼ INSERT IGNORE INTO agent_events  [idempotent, deduped by event_id]
```

### 2.2 Count Local vs Cloud Events

```bash
# Count local journal events
wc -l ~/.astra/sessions/<SESSION_ID>.jsonl

# Count local events by type
cat ~/.astra/sessions/<SESSION_ID>.jsonl | python3 -c "
import json, sys
from collections import Counter
types = Counter()
for line in sys.stdin:
    try:
        e = json.loads(line)
        types[e.get('type', 'unknown')] += 1
    except: pass
for t, c in types.most_common():
    print(f'  {t:30s}: {c}')
print(f'  {\"TOTAL\":30s}: {sum(types.values())}')
"
```

**Cloud comparison** (requires MatrixOne access):
```sql
SELECT event_type, COUNT(*) as cnt
FROM agent_events
WHERE session_id = '<SESSION_ID>'
GROUP BY event_type
ORDER BY cnt DESC;
```

### 2.3 Identify Missing Events

Compare local journal event_ids against cloud:

```bash
# Extract local event IDs (deterministic hash)
cat ~/.astra/sessions/<SESSION_ID>.jsonl | python3 -c "
import json, sys, hashlib
for line in sys.stdin:
    try:
        e = json.loads(line)
        # event_id = hash(session_id + turn + event_type + ts)
        components = f\"{e.get('session_id','')}{e.get('turn',0)}{e.get('type','')}{e.get('ts','')}\"
        eid = hashlib.sha256(components.encode()).hexdigest()[:16]
        print(eid)
    except: pass
" > /tmp/local_event_ids.txt
wc -l /tmp/local_event_ids.txt
```

Flag:
- 🔴 Cloud has <80% of local events → ingestion pipeline dropping events
- 🟡 Cloud has 80-95% of local events → some flush failures
- 🟢 Cloud has >95% of local events → healthy sync

### 2.4 Event Expansion Audit

Each Turn event should expand into 1 main event + N tool_call child events:

```bash
cat ~/.astra/sessions/<SESSION_ID>.jsonl | python3 -c "
import json, sys
for line in sys.stdin:
    e = json.loads(line)
    if e.get('type') == 'Turn':
        tc = e.get('tool_count', 0)
        calls = e.get('tool_calls', [])
        print(f'  Turn {e.get(\"turn\",\"?\")}: tool_count={tc}, tool_calls_recorded={len(calls)}')
        if tc != len(calls):
            print(f'    ⚠️ MISMATCH: tool_count != len(tool_calls)')
"
```

### 2.5 Ingestion Latency

Check `session_sync_log` for ingestion timing:

```sql
SELECT sync_type, sync_direction, status, 
       AVG(payload_size) as avg_payload,
       COUNT(*) as count
FROM session_sync_log
WHERE user_id = '<USER_ID>' AND session_id = '<SESSION_ID>'
GROUP BY sync_type, sync_direction, status;
```

---

## Phase 3: Learning Sync Audit

### 3.1 Learning Snapshot Format

Local: `~/.astra/learning/<profile>.json` — raw JSON
Cloud: `learning_snapshots` table — gzip + base64 encoded

```bash
# Check local learning snapshot
ls -la ~/.astra/learning/*.json 2>/dev/null
cat ~/.astra/learning/default.json 2>/dev/null | python3 -c "
import json, sys
data = json.load(sys.stdin)
entities = data.get('entities', {})
patterns = data.get('patterns', {})
cal = data.get('calibration', {})
print(f'Entities: {len(entities)}')
print(f'Patterns: {len(patterns)}')
print(f'Has calibration: {bool(cal)}')
print(f'Total size: {sys.stdin.seek(0,2)} bytes')
" 2>/dev/null || echo "No local learning snapshot found"
```

### 3.2 Version Consistency

Learning snapshots use optimistic locking (version numbers):

```sql
SELECT snapshot_id, profile_name, version, entity_count, pattern_count,
       has_calibration, updated_at
FROM learning_snapshots
WHERE user_id = '<USER_ID>'
ORDER BY updated_at DESC
LIMIT 5;
```

Check:
- Does local version match cloud version?
- Any version conflicts (local pushed but cloud had newer version)?

### 3.3 Delta Sync Efficiency

Astra supports delta snapshots for incremental learning sync:

```
Full snapshot: ~40 KB
Delta snapshot: 2-5 KB (85-90% reduction)
```

From sync log, check:
- Is delta sync being used? (look for small payload_size)
- Or is full snapshot pushed every time? (large payload_size)

### 3.4 Conflict Resolution

Learning merge strategy:
- **Entity observations**: Higher observation count wins
- **Patterns**: Union (combine all seen patterns)
- **Preferences**: Last-writer-wins (timestamp comparison)

Check sync log for conflict indicators:
```sql
SELECT * FROM session_sync_log
WHERE sync_type = 'learning' AND status = 'error'
ORDER BY created_at DESC LIMIT 10;
```

---

## Phase 4: Checkpoint Audit

### 4.1 Checkpoint Inventory

```bash
# List local checkpoints
ls -la ~/.astra/sessions/<SESSION_ID>/step_checkpoints/ 2>/dev/null

# Check checkpoint sizes
du -sh ~/.astra/sessions/<SESSION_ID>/step_checkpoints/*-heavy.json 2>/dev/null
```

### 4.2 Checkpoint Completeness

A heavy checkpoint should contain:
- Full message array (system + user + assistant + tool messages)
- Enough context to resume the session

```bash
# Validate checkpoint structure
for f in ~/.astra/sessions/<SESSION_ID>/step_checkpoints/*-heavy.json; do
  python3 -c "
import json, sys
msgs = json.load(open('$f'))
roles = {}
for m in msgs:
    r = m.get('role', '?')
    roles[r] = roles.get(r, 0) + 1
total_bytes = sum(len(json.dumps(m)) for m in msgs)
print(f'$(basename $f): {len(msgs)} msgs, {total_bytes:,} bytes, roles={roles}')
" 2>/dev/null
done
```

### 4.3 Checkpoint Recovery Simulation

Can a session be restored from the latest checkpoint?

```bash
# Check RestoredSession would have
cat ~/.astra/sessions/<SESSION_ID>/step_checkpoints/*-heavy.json 2>/dev/null | python3 -c "
import json, sys, glob, os

checkpoints = sorted(glob.glob(os.path.expanduser(
    '~/.astra/sessions/<SESSION_ID>/step_checkpoints/*-heavy.json'
)))

if not checkpoints:
    print('No checkpoints found!')
    sys.exit(0)

latest = checkpoints[-1]
msgs = json.load(open(latest))
print(f'Latest checkpoint: {os.path.basename(latest)}')
print(f'Messages: {len(msgs)}')
print(f'Has system message: {any(m.get(\"role\")==\"system\" for m in msgs)}')
print(f'Has user messages: {sum(1 for m in msgs if m.get(\"role\")==\"user\")}')
print(f'Has tool calls: {sum(1 for m in msgs if m.get(\"tool_calls\"))}')
print(f'Total size: {os.path.getsize(latest):,} bytes')
" 2>/dev/null
```

### 4.4 Checkpoint-Journal Consistency

The checkpoint should reflect the journal's state at that turn:

```bash
# Compare checkpoint turn count with journal turn count
journal_turns=$(grep -c '"Turn"' ~/.astra/sessions/<SESSION_ID>.jsonl 2>/dev/null)
checkpoint_count=$(ls ~/.astra/sessions/<SESSION_ID>/step_checkpoints/*-heavy.json 2>/dev/null | wc -l)
echo "Journal turns: $journal_turns"
echo "Checkpoints: $checkpoint_count"
```

Flag:
- 🔴 No checkpoints for session with >10 turns
- 🟡 Large gap between checkpoints (>5 turns)
- 🟢 Regular checkpoints every 2-3 turns

---

## Phase 5: Task State Sync Audit

### 5.1 Active Tasks

```sql
SELECT task_id, status, goal, subtask_count, 
       completed_subtasks, created_at, updated_at
FROM agent_tasks
WHERE user_id = '<USER_ID>' AND status IN ('active', 'paused')
ORDER BY updated_at DESC;
```

### 5.2 Task-Session Linkage

Every active task should be linked to a session:

```sql
SELECT t.task_id, t.goal, t.status,
       s.session_id, s.status as session_status
FROM agent_tasks t
LEFT JOIN agent_sessions s ON t.session_id = s.session_id
WHERE t.user_id = '<USER_ID>'
ORDER BY t.updated_at DESC LIMIT 10;
```

Flag:
- 🔴 Active task with no matching session (orphaned)
- 🟡 Active task with "ended" session (task not completed before session end)

---

## Phase 6: Sync Log Analysis

### 6.1 Sync Success Rate

```sql
SELECT sync_type, sync_direction,
       COUNT(CASE WHEN status='success' THEN 1 END) as success,
       COUNT(CASE WHEN status='error' THEN 1 END) as errors,
       COUNT(*) as total,
       ROUND(100.0 * COUNT(CASE WHEN status='success' THEN 1 END) / COUNT(*), 1) as success_pct
FROM session_sync_log
WHERE user_id = '<USER_ID>'
GROUP BY sync_type, sync_direction
ORDER BY total DESC;
```

### 6.2 Error Patterns

```sql
SELECT sync_type, error_message, COUNT(*) as cnt
FROM session_sync_log
WHERE user_id = '<USER_ID>' AND status = 'error'
GROUP BY sync_type, error_message
ORDER BY cnt DESC LIMIT 10;
```

Common retryable errors:
- `1040`: Too many connections
- `1205`: Lock wait timeout
- `1213`: Deadlock
- `2006`: MySQL server gone away
- `2013`: Lost connection

### 6.3 Sync Log Retention

Astra retains bounded sync logs per user:
- 200 success rows
- 50 error rows

Check if retention is working:
```sql
SELECT status, COUNT(*) as cnt
FROM session_sync_log
WHERE user_id = '<USER_ID>'
GROUP BY status;
```

---

## Phase 7: Sync Audit Report

```
╔══════════════════════════════════════════════════════════════╗
║  ☁️  Cloud Sync Audit                                        ║
║  Session: {session_id}                                       ║
║  User: {user_id}                                             ║
╠══════════════════════════════════════════════════════════════╣
║                                                              ║
║  📊 Sync Coverage                                            ║
║  ├─ Events:      {cloud}/{local} ({pct}%)  {status_icon}    ║
║  ├─ Learning:    v{local_ver} local / v{cloud_ver} cloud     ║
║  ├─ Checkpoints: {count} local / {cloud_count} cloud         ║
║  ├─ Tasks:       {synced}/{total} synced                     ║
║  └─ Preferences: {synced ? "✅" : "❌"}                      ║
║                                                              ║
║  📈 Sync Health                                              ║
║  ├─ Event ingestion rate:  {pct}%  {bar}                    ║
║  ├─ Learning sync:         {status}                          ║
║  ├─ Checkpoint coverage:   {pct}%  {bar}                    ║
║  └─ Overall sync success:  {pct}%  {bar}                    ║
║                                                              ║
║  🔴 Issues ({n})                                             ║
║  {list of sync problems}                                     ║
║                                                              ║
║  🟡 Warnings ({n})                                           ║
║  {list of sync concerns}                                     ║
║                                                              ║
║  💡 Recommendations                                          ║
║  {specific fixes}                                            ║
║                                                              ║
╚══════════════════════════════════════════════════════════════╝
```

---

## Common Sync Issues

| Issue | Symptom | Fix |
|-------|---------|-----|
| Event ingestion backlog | Channel at capacity (200) | Increase `channel_capacity` or reduce `flush_interval` |
| Learning version conflict | Push fails with version mismatch | Pull first, merge, then push |
| Checkpoint too large | Heavy checkpoint >5MB | Trim tool results before checkpointing |
| No checkpoints | Session restored without context | Verify checkpoint trigger fires every N turns |
| Duplicate key on INSERT | `1062` errors in sync log | Expected — INSERT IGNORE handles this |
| Connection pool exhaustion | `1040` errors | Reduce concurrent sync operations |
| Stale learning on cloud | Local has more entities than cloud | Force push learning snapshot |

---

## Reference: Key Source Files

| Component | File |
|-----------|------|
| Event ingestion | `rust/crates/services/src/event_ingestion.rs` |
| State sync (all types) | `rust/crates/services/src/state_sync.rs` |
| Session restore | `rust/crates/services/src/session_restore.rs` |
| Session journal | `rust/crates/services/src/session_journal.rs` |
| Checkpoint heavy save | `rust/crates/services/src/session_journal.rs` |
| Learning pipeline | `rust/crates/services/src/learning.rs`, `rust/crates/runtime/src/pipeline/learning.rs` |
| Sync log retention | `rust/crates/services/src/state_sync.rs` (SYNC_LOG_*_RETAIN) |

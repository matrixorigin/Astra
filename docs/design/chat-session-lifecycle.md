# Chat Session Lifecycle Design

## What Is a Session?

A session is a **persistent conversation thread** stored in the DB (`sessions` table).
It is NOT a connection resource — closing a network connection does not end a session.

Sessions serve three purposes:
1. **Context continuity** — events are linked to a session_id; the context manager
   retrieves relevant history across turns
2. **Audit trail** — every LLM call, tool use, and memory write is traceable to a session
3. **Replay** — any session can be replayed in a sandbox for regression testing

## Session States

```
active → zombie（超时）→ archived（老化）
```

| State | Meaning |
|---|---|
| `active` | Session exists and may be in use |
| `zombie` | No activity for > TTL; process likely crashed or was killed |
| `archived` | Events compressed into memory summaries; raw events deleted |

There is no `closed` state. Sessions are never explicitly closed — zombie
detection handles all cases, including normal exits.

## Lifecycle Rules

### Creation
- Created at the start of each `chat` invocation (or reused via `--session-id` / `--resume`)
- `status = active`, `updated_at = now()`

### Active Heartbeat
- No explicit heartbeat needed
- `agent_sessions.updated_at` has `onupdate=func.now()` — it refreshes automatically
  whenever `event_count` or any other field is updated (which happens on every turn)
- Zombie detection uses `updated_at` as the activity timestamp; `last_active_at` is
  redundant with `updated_at` and can be removed in a future cleanup

### No Explicit Close
- `close_session()` is not called on exit — zombie detection handles all cases
- Normal exit, `Ctrl-C`, and `kill -9` are all treated the same way
- `/clear` command only creates a new session; it does not close the old one

### Zombie Detection (background job)
- A background task runs periodically (suggested: every 5–15 minutes)
- Any session with `status = active` AND `updated_at < now() - ZOMBIE_TTL`
  is marked `zombie`
- Uses `agent_sessions.updated_at` (auto-refreshed by SQLAlchemy `onupdate`) —
  **no separate table or heartbeat column needed**
- Suggested `ZOMBIE_TTL`: 30 minutes (configurable via `infra_configs`)

```sql
UPDATE agent_sessions
SET status = 'zombie'
WHERE status = 'active'
  AND updated_at < NOW() - INTERVAL 30 MINUTE;
```

### Archival (memory governance integration)
- Sessions older than `ARCHIVE_TTL` (suggested: 30 days) are candidates for archival
- Archival = run `session_summary.py` to compress events into `mem_memories`,
  then delete raw events from `conversation_events`
- Session record itself is kept (for audit); only raw events are deleted

## What We Do NOT Do

- **No `closed` state** — zombie detection replaces explicit close entirely
- **No `close_session` on exit** — removed from CLI; no server-side equivalent needed
- **No delete on chat exit** — session records are permanent audit artifacts
- **No per-connection session** — one logical conversation = one session,
  regardless of how many times the user reconnects

## Comparison with Other Systems

| System | Storage | Cleanup |
|---|---|---|
| Cursor | Local SQLite | LRU eviction, manual clear |
| Claude.ai / ChatGPT | Server DB | User-initiated delete, data retention policy |
| **This system** | MatrixOne (server) | Zombie GC + archival via memory governance |

The key difference from Cursor: history survives across devices and process restarts.
The key difference from Claude.ai: old sessions are compressed into memories
(information preserved) rather than deleted (information lost).

## Resume Semantics and Race Conditions

`--resume` reads `last_session_id` from the local profile and reuses it without
validating the server-side session state. This creates several race conditions:

| Scenario | Current behavior | Correct behavior |
|---|---|---|
| Resume a zombie session | Used as-is; zombie GC job may concurrently mark it zombie | Re-activate (`status = active`) |
| Resume a deleted/non-existent session | First message fails with server error | Silently create new session |
| Resume a closed session | Used as-is (status inconsistency) | Re-activate |

**Fix (CLI side):** validate the candidate session before using it; fall back to
`create_session` if it doesn't exist or can't be re-activated.

**Fix (server side):** when a `chat` request arrives with an existing `session_id`,
re-activate it (`status = active`, `updated_at = now()`) regardless of its current
state, rather than rejecting it. A session being zombie/closed does not mean its
history is gone — it just means no one was using it.

```python
# cli/mo_agent_api.py — resume with validation
if resume and not session_id and _profile.get("last_session_id"):
    candidate = _profile["last_session_id"]
    try:
        s = client.get_session(candidate)
        if s:  # exists → reuse (server will re-activate)
            session_id = candidate
    except Exception:
        pass  # not found → fall through to create_session
```



```bash
# New session each time (default)
mo-agent chat --user-id alice

# Resume last session
mo-agent chat --user-id alice --resume

# Join a specific session
mo-agent chat --user-id alice --session-id <id>

# Script / pipe: multiple turns in one session
printf "turn 1\nturn 2\nturn 3\n" | mo-agent chat --user-id alice
```

## Implementation Checklist

- [x] `agent_sessions` table has `updated_at` (auto-refreshed via `onupdate=func.now()`)
- [x] `--resume` flag reads `last_session_id` from profile
- [x] `close_session()` calls removed from CLI (`chat` finally + `/clear`)
- [ ] Background zombie detection job (`core/events/session_manager.py`)
- [ ] Resume validation: `get_session` before reuse, fall back to `create_session` if missing
- [ ] Server-side: re-activate session on incoming chat request (set `status = active`)
- [ ] Archival pipeline integration with `core/memory/session_summary.py`
- [ ] `ZOMBIE_TTL` and `ARCHIVE_TTL` configurable via `infra_configs`
- [ ] Remove redundant `last_active_at` column (superseded by `updated_at`)

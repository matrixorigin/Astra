# Concurrency Model and Multi-tenancy Design

## Overview

This document describes how mo-dev-agent handles concurrent operations, multi-user scenarios, and isolation guarantees.

## Core Principles

1. **Event Immutability**: Once written, events are never modified
2. **Database-level Isolation**: Each sandbox is a separate database
3. **Read-Write Separation**: Time Machine is read-only, Sandbox is read-write
4. **No Global Locks**: Operations don't block each other

---

## 1. Concurrent Event Logging

### Scenario: Multiple Users Logging Events Simultaneously

**Architecture**:
```
User A → Session A → conversation_events (INSERT)
User B → Session B → conversation_events (INSERT)
User C → Session C → conversation_events (INSERT)
```

**Isolation Level**: Row-level
- Each INSERT is independent
- No conflicts (different event_ids via ULID)
- MatrixOne handles concurrent writes

**Guarantees**:
- ✅ No data loss
- ✅ No blocking
- ✅ ACID compliance

---

## 2. Time Machine Concurrency

### Scenario: User A Queries History While User B Writes New Events

**Architecture**:
```
User A: SELECT * FROM events {SNAPSHOT = 'checkpoint_1'}  (Read-only)
User B: INSERT INTO events ...                             (Write)
```

**Isolation**:
- Time Machine uses `{SNAPSHOT = 'name'}` syntax
- **Read-only operation**, no database state change
- User B's writes don't affect User A's historical query

**Guarantees**:
- ✅ User A sees consistent historical state
- ✅ User B's writes proceed normally
- ✅ Zero interference

**Performance**:
- No locks
- No blocking
- Snapshot queries are fast (MatrixOne optimization)

---

## 3. Sandbox Concurrency

### Scenario: Multiple Users Running Experiments Simultaneously

**Architecture**:
```
User A: sandbox_exp_a (separate database)
User B: sandbox_exp_b (separate database)
User C: sandbox_exp_c (separate database)
Main DB: dev_agent (unchanged)
```

**Isolation Level**: Database-level
- Each sandbox is a **separate database**
- Complete isolation (no shared state)
- Can run unlimited parallel sandboxes

**Guarantees**:
- ✅ User A's experiment doesn't affect User B
- ✅ No sandbox affects main database
- ✅ Can run 10+ sandboxes in parallel

**Resource Management**:
- Zero-copy clone (CoW) - minimal storage overhead
- Each sandbox has independent connection pool
- Cleanup is automatic (DROP DATABASE)

---

## 4. Checkpoint Creation Concurrency

### Scenario: User A Creates Checkpoint While User B Writes Events

**Architecture**:
```
User A: CREATE SNAPSHOT checkpoint_x FOR ACCOUNT sys
User B: INSERT INTO events ...
```

**Behavior**:
- Snapshot captures state at creation time
- User B's writes after snapshot creation are not included
- No blocking (snapshot is async in MatrixOne)

**Guarantees**:
- ✅ Consistent snapshot
- ✅ No write blocking
- ✅ Deterministic state

---

## 5. Multi-tenancy Model

### Current Implementation

**Tenant Isolation**:
- `user_id` field in all events
- `tenant_id` field in sessions (optional)
- Query filtering by user_id/tenant_id

**Sandbox Isolation**:
- Each user can create their own sandboxes
- Sandbox naming convention: `sandbox_{user_id}_{experiment_name}`
- No cross-user sandbox access

### Future Enhancements

**Tenant-level Isolation** (P2):
```python
# Each tenant gets their own database
tenant_a: dev_agent_tenant_a
tenant_b: dev_agent_tenant_b

# Sandboxes are tenant-scoped
sandbox: dev_agent_tenant_a_sandbox_exp1
```

---

## 6. Conflict Scenarios and Resolution

### Scenario 1: Concurrent Checkpoint Creation

**Problem**: Two users create checkpoints with same name

**Solution**:
- Use unique names (ULID or timestamp)
- MatrixOne will error if name exists
- Application handles error gracefully

### Scenario 2: Sandbox Name Collision

**Problem**: Two users try to create sandbox with same name

**Solution**:
- Include user_id in sandbox name
- Use timestamp for uniqueness
- `DROP IF EXISTS` before creation

### Scenario 3: Restore While Others Are Writing

**Problem**: User A restores checkpoint, affecting User B's writes

**Current Status**: ⚠️ **Not supported**
- `RESTORE ACCOUNT` is a global operation
- Would affect all users

**Solution**: **Don't use RESTORE for read operations**
- ✅ Time Machine uses read-only `{SNAPSHOT}` queries
- ✅ Sandbox uses `CLONE` (separate database)
- ❌ Never use `RESTORE ACCOUNT` in production

---

## 7. Performance Characteristics

### Time Machine

| Operation | Latency | Blocking | Scalability |
|-----------|---------|----------|-------------|
| Create checkpoint | ~100ms | No | Unlimited |
| Query at checkpoint | ~10-50ms | No | High |
| List checkpoints | ~5ms | No | High |

### Sandbox

| Operation | Latency | Blocking | Scalability |
|-----------|---------|----------|-------------|
| Create sandbox (CLONE) | ~1-5s | No | 10+ parallel |
| Query sandbox | ~10ms | No | High |
| Delete sandbox | ~1s | No | High |

### Event Logging

| Operation | Latency | Blocking | Scalability |
|-----------|---------|----------|-------------|
| Log event | ~5-10ms | No | 1000+ QPS |
| Query events | ~10-50ms | No | High |

---

## 8. Best Practices

### For Production Deployment

1. **Never use RESTORE ACCOUNT** - Use read-only queries instead
2. **Limit concurrent sandboxes** - Monitor resource usage
3. **Cleanup old checkpoints** - Use retention policies
4. **Use unique names** - Include user_id and timestamp

### For Development

1. **Use sandboxes for experiments** - Never test on main database
2. **Create checkpoints before risky operations** - Easy rollback
3. **Clean up after experiments** - Drop sandboxes when done

### For Multi-user Scenarios

1. **Namespace sandboxes by user** - `sandbox_{user_id}_{name}`
2. **Filter events by user_id** - Ensure data isolation
3. **Monitor resource usage** - Prevent abuse

---

## 9. Limitations and Trade-offs

### Current Limitations

1. **No cross-database transactions** - Each sandbox is independent
2. **No automatic merge** - Manual merge required (P1 feature)
3. **Storage overhead** - Each sandbox consumes storage (CoW mitigates)

### Trade-offs

| Aspect | Choice | Trade-off |
|--------|--------|-----------|
| Isolation | Database-level | Higher resource usage vs table-level |
| Snapshot frequency | Manual | User control vs automatic PITR |
| Cleanup | Manual | Explicit control vs automatic GC |

---

## 10. Future Enhancements

### P1 - Near-term

1. **Automatic sandbox expiry** - TTL-based cleanup
2. **Sandbox merge** - Merge changes back to main
3. **Resource quotas** - Limit sandboxes per user

### P2 - Long-term

1. **PITR integration** - Continuous time-travel
2. **Cross-branch queries** - Query multiple branches
3. **Branch permissions** - Fine-grained access control

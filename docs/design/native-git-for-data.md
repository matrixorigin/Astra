# Native Git for Data Implementation

## Overview

This document describes the **native** implementation using MatrixOne's built-in Git for Data capabilities, directly leveraging:

1. **Zero-copy CLONE** - Instant table/database duplication with no storage overhead
2. **data branch** - Git-like branching, diff, and merge for data
3. **Snapshot** - Point-in-time state capture

## Why Native Implementation?

### Previous Approach (Deprecated)
- Used `CREATE DATABASE ... CLONE` for sandboxes
- Manual metadata tracking
- Custom branch management logic

### Native Approach (Current)
- ✅ Uses MatrixOne's **native `data branch` syntax**
- ✅ Uses **zero-copy CLONE** for instant duplication
- ✅ Built-in diff/merge capabilities
- ✅ No custom metadata needed
- ✅ Better performance and reliability

---

## Core Capabilities

### 1. Zero-Copy CLONE

**Table Clone**:
```sql
-- Instant clone, no storage duplication
CREATE TABLE target_table CLONE source_table;

-- Clone from snapshot
CREATE TABLE target_table CLONE source_table {snapshot="snap_name"};
```

**Database Clone**:
```sql
-- Clone entire database
CREATE DATABASE target_db CLONE source_db;

-- Clone from snapshot
CREATE DATABASE target_db CLONE source_db {snapshot="snap_name"};
```

**Performance**:
- ⚡ Instant creation (1-5 seconds regardless of data size)
- 💾 Zero storage overhead (Copy-on-Write)
- 🔒 Full isolation (separate objects)

### 2. Data Branch

**Create Branch**:
```sql
-- Branch from current state
data branch create table branch_table from source_table;

-- Branch from snapshot
data branch create table branch_table from source_table {snapshot="snap_name"};

-- Branch entire database
data branch create database branch_db from source_db;
```

**Diff Branch**:
```sql
-- Show differences
data branch diff branch_table against main_table;

-- Count differences
data branch diff branch_table against main_table output count;

-- Export to file
data branch diff branch_table against main_table output file '/path/to/file';

-- Diff with snapshots
data branch diff table1{snapshot="snap1"} against table2{snapshot="snap2"};
```

**Merge Branch**:
```sql
-- Merge (error on conflict)
data branch merge source_table into target_table;

-- Skip conflicts
data branch merge source_table into target_table when conflict skip;

-- Accept source on conflict
data branch merge source_table into target_table when conflict accept;
```

**Delete Branch**:
```sql
-- Delete table branch
data branch delete table database_name.table_name;

-- Delete database branch
data branch delete database database_name;
```

### 3. Snapshot

```sql
-- Create snapshot
CREATE SNAPSHOT snap_name FOR TABLE database_name table_name;
CREATE SNAPSHOT snap_name FOR DATABASE database_name;
CREATE SNAPSHOT snap_name FOR ACCOUNT account_name;

-- List snapshots
SHOW SNAPSHOTS;

-- Drop snapshot
DROP SNAPSHOT snap_name;
```

---

## Python SDK

### NativeBranchManager

```python
from core.sandbox.native_branch import NativeBranchManager

mgr = NativeBranchManager()

# Create table branch
mgr.create_table_branch("exp_events", "conversation_events")

# Diff
diff = mgr.diff_tables("conversation_events", "exp_events")

# Merge
mgr.merge_tables("exp_events", "conversation_events", conflict_strategy="accept")

# Delete
mgr.delete_table_branch("exp_events")

# Zero-copy clone
mgr.clone_table("events_backup", "conversation_events")
```

---

## Use Cases

### 1. Experiment Workflow

```python
# Create experiment branch
mgr.create_table_branch("events_exp", "conversation_events")

# Run experiments (modify events_exp)
db.execute("insert into events_exp values (...)")

# Compare with main
diff = mgr.diff_tables("conversation_events", "events_exp")
print(f"Changes: {len(diff)}")

# Merge if successful
if experiment_successful:
    mgr.merge_tables("events_exp", "conversation_events", "accept")
else:
    mgr.delete_table_branch("events_exp")
```

### 2. Time-Travel Experiment

```python
from sdk.git_for_data import GitForData

git = GitForData()

# Create checkpoint
git.create_snapshot("before_experiment")

# Create branch from checkpoint
mgr.create_table_branch(
    "events_exp",
    "conversation_events",
    snapshot="before_experiment"
)

# Experiment in branch...

# Compare with current state
diff = mgr.diff_tables("conversation_events", "events_exp")
```

### 3. Zero-Copy Backup

```python
# Instant backup (no storage cost)
mgr.clone_table("events_backup", "conversation_events")

# Restore if needed
db.execute("drop table conversation_events")
db.execute("alter table events_backup rename to conversation_events")
```

### 4. A/B Testing

```python
# Create two experiment branches
mgr.create_table_branch("events_exp_a", "conversation_events")
mgr.create_table_branch("events_exp_b", "conversation_events")

# Run different experiments
# ... modify events_exp_a with strategy A
# ... modify events_exp_b with strategy B

# Compare results
diff_a = mgr.diff_tables("conversation_events", "events_exp_a")
diff_b = mgr.diff_tables("conversation_events", "events_exp_b")

# Merge winner
mgr.merge_tables("events_exp_a", "conversation_events", "accept")
```

---

## Architecture Comparison

### Old Approach (Deprecated)

```
Main DB (dev_agent)
  └── conversation_events

Sandbox (sandbox_exp1) - Separate DB
  ├── conversation_events (cloned)
  └── _sandbox_metadata (custom tracking)
```

**Issues**:
- Manual metadata management
- Custom branch logic
- Database-level isolation only

### Native Approach (Current)

```
Main DB (dev_agent)
  └── conversation_events

Branch Table (events_exp) - Same DB
  └── branched from conversation_events
  └── metadata in mo_catalog.mo_branch_metadata
```

**Advantages**:
- ✅ Native metadata in `mo_catalog.mo_branch_metadata`
- ✅ Built-in diff/merge
- ✅ Table-level or database-level branching
- ✅ Better performance

---

## Performance Characteristics

| Operation | Latency | Storage | Scalability |
|-----------|---------|---------|-------------|
| CLONE table | ~1-2s | Zero (CoW) | Unlimited |
| CLONE database | ~2-5s | Zero (CoW) | Unlimited |
| data branch create | ~1-2s | Zero (CoW) | Unlimited |
| data branch diff | ~10-100ms | N/A | High |
| data branch merge | ~100ms-1s | N/A | High |

**Key Insight**: All operations use Copy-on-Write (CoW), so:
- ⚡ Instant creation regardless of data size
- 💾 Zero storage overhead until modifications
- 🔄 Modifications only store deltas

---

## Migration Guide

### From Old Sandbox to Native Branch

**Old Code**:
```python
from core.sandbox.advanced_sandbox import AdvancedSandbox

sandbox = AdvancedSandbox()
sandbox.create_sandbox("exp1")
sandbox.clone_table_to_sandbox("exp1", "conversation_events")
# ... experiment
sandbox.delete_sandbox("exp1")
```

**New Code**:
```python
from core.sandbox.native_branch import NativeBranchManager

mgr = NativeBranchManager()
mgr.create_table_branch("events_exp", "conversation_events")
# ... experiment
mgr.delete_table_branch("events_exp")
```

**Benefits**:
- Simpler API
- Better performance
- Native diff/merge support

---

## Best Practices

### 1. Use CLONE for Backups
```python
# Before risky operation
mgr.clone_table("events_backup", "conversation_events")
```

### 2. Use data branch for Experiments
```python
# For experiments with diff/merge
mgr.create_table_branch("events_exp", "conversation_events")
```

### 3. Use Snapshots for Time-Travel
```python
# Create checkpoint before branching
git.create_snapshot("checkpoint_1")
mgr.create_table_branch("events_exp", "conversation_events", snapshot="checkpoint_1")
```

### 4. Clean Up Branches
```python
# Always clean up after experiments
try:
    # ... experiment
finally:
    mgr.delete_table_branch("events_exp")
```

---

## Limitations

1. **data branch** requires primary key on tables
2. **Merge conflicts** need manual resolution strategy
3. **Cross-database branches** not supported (use CLONE instead)
4. **Snapshot retention** - old snapshots should be cleaned up

---

## Future Enhancements

1. **Automatic conflict resolution** - AI-assisted merge strategies
2. **Branch visualization** - Show branch tree like `git log --graph`
3. **Branch permissions** - Fine-grained access control
4. **Automatic cleanup** - TTL-based branch expiry

---

## References

- MatrixOne Git for Data: `/home/xupeng/matrixone/test/distributed/cases/git4data/`
- Branch tests: `/home/xupeng/matrixone/test/distributed/cases/git4data/branch/`
- Clone tests: `/home/xupeng/matrixone/test/distributed/cases/git4data/clone/`

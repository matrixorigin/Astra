# Git for Data Features - Complete Design

## 1. Time Machine

### Core Capabilities

**Implemented** ✅:
- Create checkpoints
- Read-only time-travel queries (using `{SNAPSHOT = 'name'}` syntax)
- List all checkpoints
- Replay conversations (query historical state)

**To Be Implemented** ⏳:
- [ ] Compare differences between two time points (Diff)
- [ ] View change history between checkpoints
- [ ] Query events by time range
- [ ] Checkpoint tags and description management
- [ ] Checkpoint search and filtering
- [ ] Timeline visualization
- [ ] Export data at specific time point

### Use Cases

1. **Debugging and Auditing**: Review historical decision-making process
2. **A/B Testing Comparison**: Compare effects of different versions
3. **Data Recovery**: View data before accidental deletion
4. **Compliance Auditing**: Trace historical operation records

### API Design

```python
class TimeMachine:
    # Implemented
    def create_checkpoint(name, description) -> dict
    def list_checkpoints() -> list[dict]
    def get_events_at_checkpoint(checkpoint, session_id) -> list[Event]
    def replay_conversation(session_id, checkpoint) -> dict
    
    # To be implemented
    def diff_checkpoints(checkpoint1, checkpoint2) -> dict
    def search_checkpoints(query, tags) -> list[dict]
    def export_checkpoint(checkpoint, format) -> bytes
    def get_checkpoint_metadata(checkpoint) -> dict
    def update_checkpoint_description(checkpoint, description) -> None
```

---

## 2. Sandbox

### Core Capabilities

**Implemented** ✅:
- Zero-copy database clone (entire database)
- Clone from snapshot
- Fully isolated experimental environment
- Sandbox comparison (with main database)
- Automatic cleanup
- **Table-level clone** - Clone specific tables to sandbox
- **Incremental loading** - Add new tables/data to sandbox
- **Selective deletion** - Remove specific tables from sandbox
- **Sandbox management** - Metadata, tags, listing
- **Sandbox history** - Checkpoints within sandbox

**To Be Implemented** ⏳:
- [ ] **Sandbox merge** - Merge sandbox changes back to main (Merge)
- [ ] **Sandbox permissions** - Multi-user sandbox isolation
- [ ] **Sandbox lifecycle** - Automatic expiration and cleanup
- [ ] **Data sync** - Sync specific tables from main to sandbox

### Use Cases

1. **Safe Experimentation**: Test new features without affecting production
2. **Data Analysis**: Run complex queries on a copy
3. **Training and Testing**: Use production data copies
4. **Parallel Development**: Multiple teams work independently
5. **What-If Analysis**: Simulate different scenarios
6. **Data Repair**: Fix data in sandbox then merge back to main

### API Design (Complete)

```python
class AdvancedSandbox:
    # Implemented - Database level
    def create_clone_sandbox(name, from_snapshot) -> dict
    def drop_clone_sandbox(name) -> None
    def run_isolated_experiment(name, fn, cleanup) -> dict
    def compare_sandbox_with_main(sandbox, table) -> dict
    
    # Implemented - Table level operations
    def clone_table_to_sandbox(sandbox, table, new_name) -> dict
    def add_table_to_sandbox(sandbox, table, from_snapshot) -> dict
    def remove_table_from_sandbox(sandbox, table) -> None
    def list_sandbox_tables(sandbox) -> list[str]
    
    # Implemented - Sandbox management
    def list_sandboxes(prefix, include_metadata) -> list[dict]
    def get_sandbox_info(sandbox) -> dict
    def update_sandbox_metadata(sandbox, description, tags) -> None
    def get_sandbox_metadata(sandbox) -> dict
    
    # Implemented - Sandbox history
    def create_sandbox_checkpoint(sandbox, checkpoint_name, description) -> dict
    def list_sandbox_checkpoints(sandbox) -> list[dict]
    def restore_sandbox_to_checkpoint(sandbox, checkpoint) -> None
    
    # To be implemented - Data sync
    def apply_changes_to_sandbox(sandbox, changes) -> dict
    def merge_sandbox_to_main(sandbox, tables, strategy) -> dict
    def diff_sandbox_with_main(sandbox, table) -> dict
    def sync_table_from_main(sandbox, table) -> None
    def set_sandbox_expiry(sandbox, ttl_hours) -> None
```

---

## 3. PITR (Point-in-Time Recovery)

### Core Capabilities

**To Be Implemented** ⏳:
- [ ] Enable PITR (automatic history retention)
- [ ] Restore to any point in time
- [ ] Query data at any point in time
- [ ] PITR policy management (retention duration)
- [ ] PITR storage optimization

### Use Cases

1. **Continuous Time Travel**: No need to manually create checkpoints
2. **Precise Recovery**: Restore to second-level precision
3. **Audit Compliance**: Automatically retain historical records

### API Design

```python
class PITRManager:
    def enable_pitr(database, retention_hours) -> dict
    def disable_pitr(database) -> None
    def list_pitr_policies() -> list[dict]
    def query_at_timestamp(query, timestamp) -> list[dict]
    def restore_to_timestamp(database, timestamp) -> None
    def get_pitr_storage_usage(database) -> dict
```

---

## 4. Diff & Merge

### Core Capabilities

**To Be Implemented** ⏳:
- [ ] Data difference comparison
- [ ] Schema difference comparison
- [ ] Three-way merge
- [ ] Conflict detection and resolution
- [ ] Change preview

### Use Cases

1. **Code Review-style Data Review**: View data changes
2. **Safe Merge**: Merge sandbox modifications back to main
3. **Conflict Resolution**: Handle concurrent modification conflicts

### API Design

```python
class DiffMerge:
    def diff_databases(db1, db2, tables) -> dict
    def diff_tables(table1, table2) -> dict
    def preview_merge(source, target, strategy) -> dict
    def merge_with_strategy(source, target, strategy, conflict_resolution) -> dict
    def detect_conflicts(source, target) -> list[dict]
```

---

## 5. Feature Priority

### P0 - Immediate Implementation (Core Scenarios) ✅ Completed

1. **Sandbox Table-level Operations** ✅
   - `clone_table_to_sandbox()` - Clone only needed tables
   - `add_table_to_sandbox()` - Incrementally add tables
   - `remove_table_from_sandbox()` - Remove tables

2. **Sandbox Management** ✅
   - `list_sandboxes()` - View all sandboxes
   - `get_sandbox_info()` - Sandbox details
   - `update_sandbox_metadata()` - Description and tags

3. **Sandbox History** ✅
   - `create_sandbox_checkpoint()` - Checkpoints within sandbox
   - `list_sandbox_checkpoints()` - List checkpoints
   - `restore_sandbox_to_checkpoint()` - Restore to checkpoint

### P1 - Near-term Implementation (Enhanced Features)

4. **Time Machine Enhancement**
   - `diff_checkpoints()` - Compare checkpoints
   - `search_checkpoints()` - Search checkpoints

5. **Sandbox Merge**
   - `diff_sandbox_with_main()` - Difference comparison
   - `merge_sandbox_to_main()` - Merge back to main

### P2 - Long-term Planning (Advanced Features)

6. **PITR Integration**
7. **Visualization Tools**
8. **Permissions and Multi-tenancy**

---

## 6. MatrixOne Capability Mapping

### Fully Utilized ✅

1. **CLONE** - Zero-copy database clone
2. **SNAPSHOT** - Snapshot creation and queries
3. **{SNAPSHOT = 'name'}** - Time-travel queries
4. **Table-level CLONE** - Fine-grained cloning
5. **RESTORE DATABASE** - Database-level restore

### To Be Utilized ⏳

1. **PITR** - Continuous point-in-time recovery
2. **Diff/Merge** - If MatrixOne supports

---

## 7. Implementation Roadmap

### Phase 1: Table-level Sandbox ✅ (Completed)
- Implement table-level cloning
- Sandbox metadata management
- Sandbox listing and search

### Phase 2: Sandbox History ✅ (Completed)
- Checkpoints within sandbox
- Operation history tracking
- Sandbox restore

### Phase 3: Merge Capability (2 weeks)
- Difference comparison
- Merge strategies
- Conflict detection

### Phase 4: PITR Integration (1 month)
- Enable PITR
- Time-point queries
- Automatic history management

---

## 8. Current Status

### Implemented Features ✅

**Time Machine**:
- Checkpoint creation and management
- Read-only time-travel queries
- Conversation replay

**Sandbox**:
- Zero-copy database clone
- Table-level operations (clone/add/remove)
- Sandbox management (list/info/metadata)
- Sandbox history (checkpoint/restore)
- Full isolation

**Test Coverage**:
- 39 tests, 100% passing
- Unit tests + Integration tests
- Edge case coverage

### Production Readiness

- ✅ **Safety**: No dangerous global operations
- ✅ **Performance**: Zero-copy, second-level creation
- ✅ **Isolation**: Fully independent sandboxes
- ✅ **Manageability**: Metadata, tags, checkpoints
- ✅ **Testability**: 39 tests coverage
- ✅ **Documentation**: Design docs + API docs

# Git for Data Features - Complete Design

## 0. Why Git for Data?

Agent platforms face problems that traditional databases cannot solve:

| Production Problem | Required Data Capability | Why Traditional DBs Fail |
|---|---|---|
| "Why did the agent decide that 3 months ago?" | Query the exact data state at decision time | No time-travel queries; manual snapshots don't scale |
| "Will this prompt change break existing cases?" | Run regression tests on full production data instantly | Copying production DB takes hours and significant storage |
| "Which past answers broke when knowledge updated?" | Compare agent behavior across two data versions | No zero-copy branching; can't cheaply create two environments |
| "Is my training data contaminated?" | Trace data lineage across dataset versions | No native causal chain queries across snapshots |
| "Who accessed which version of the data?" | Bind permissions to data versions, not just tables | Access control is row/table scoped, not version-scoped |

Git for Data (time-travel queries, zero-copy branching, snapshots, PITR) solves these at the storage engine level. This is why it serves as the architectural spine — not as a feature showcase, but because the problems demand it.

Each agent decision binds to versioned inputs: `f(prompt@version, skill@version, context@snapshot, memory@state, llm_params)`. When 4 of 5 inputs are version-controlled, LLM non-determinism is constrained to a minimal, auditable range.

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

## 5. Hallucination Firewall

### Core Capabilities

**To Be Implemented** ⏳:
- [ ] Extract verifiable claims from LLM responses
- [ ] Query same snapshot the LLM saw for verification
- [ ] Annotate responses with verification status
- [ ] Block delivery if contradictions found
- [ ] Confidence scoring for claims

### Use Cases

1. **Fact Verification**: Verify LLM claims against known data
2. **Consistency Checking**: Ensure responses align with context
3. **Quality Assurance**: Block hallucinated responses before delivery
4. **Trust Scoring**: Build confidence metrics for LLM outputs

### API Design

```python
class HallucinationFirewall:
    def extract_claims(response_text) -> list[Claim]
    def verify_claims(claims, snapshot_id) -> list[VerificationResult]
    def annotate_response(response, verifications) -> AnnotatedResponse
    def should_block_response(verifications, threshold) -> bool
    def compute_confidence_score(verifications) -> float
```

---

## 6. Cost-Aware Branching

### Core Capabilities

**To Be Implemented** ⏳:
- [ ] Query historical LLM call costs via time-travel
- [ ] Predict costs before execution
- [ ] Suggest cheaper alternatives
- [ ] Enforce budget limits per branch
- [ ] Cost optimization recommendations

### Use Cases

1. **Budget Control**: Prevent expensive operations
2. **Cost Optimization**: Choose most cost-effective approaches
3. **Resource Planning**: Predict costs for experiments
4. **Alternative Suggestions**: Recommend cheaper models/approaches

### API Design

```python
class CostAwareBranching:
    def predict_cost(operation, model, context_size) -> CostEstimate
    def get_historical_costs(time_range, filters) -> list[CostRecord]
    def suggest_alternatives(operation, budget_limit) -> list[Alternative]
    def enforce_budget_limit(branch, budget) -> BudgetPolicy
    def optimize_for_cost(operation_plan) -> OptimizedPlan
```

---

## 7. Data-Versioned Prompt Evolution

### Core Capabilities

**To Be Implemented** ⏳:
- [ ] Create branch for each prompt change
- [ ] Write candidate prompts to branch
- [ ] Replay golden sessions on branch
- [ ] Compute quality delta vs baseline
- [ ] Merge only if improvement exceeds threshold

### Use Cases

1. **Prompt A/B Testing**: Compare prompt versions scientifically
2. **Quality Gates**: Only deploy improved prompts
3. **Rollback Safety**: Revert to previous prompt versions
4. **Performance Tracking**: Monitor prompt quality over time

### API Design

```python
class PromptEvolution:
    def create_prompt_branch(base_version, candidate_prompt) -> Branch
    def replay_golden_sessions(branch, session_ids) -> ReplayResults
    def compute_quality_delta(branch_results, baseline_results) -> QualityDelta
    def merge_if_improved(branch, threshold) -> MergeResult
    def rollback_to_version(prompt_id, version) -> None
```

---

## 8. Training Data Pipeline

### Core Capabilities

**To Be Implemented** ⏳:
- [ ] Build datasets from high-quality events
- [ ] Create snapshot as dataset version
- [ ] Build SFT pairs from causal chains
- [ ] Compare datasets across versions
- [ ] Detect contamination via lineage

### Use Cases

1. **Dataset Versioning**: Track training data evolution
2. **Quality Control**: Ensure high-quality training data
3. **Contamination Detection**: Prevent test data leakage
4. **Reproducible Training**: Recreate exact training conditions

### API Design

```python
class TrainingDataPipeline:
    def build_dataset_from_events(quality_filter, time_range) -> Dataset
    def create_dataset_snapshot(dataset, version_name) -> Snapshot
    def build_sft_pairs(causal_chains) -> list[SFTPair]
    def compare_dataset_versions(v1, v2) -> DatasetDiff
    def detect_contamination(train_snapshot, test_snapshot) -> ContaminationReport
```

---

## 9. Event Lineage Graph

### Core Capabilities

**To Be Implemented** ⏳:
- [ ] Full upstream/downstream traceability
- [ ] Recursive CTE queries on causal_chain_id/parent_event_id
- [ ] Contamination detection across datasets
- [ ] Impact analysis for data changes
- [ ] Lineage visualization

### Use Cases

1. **Root Cause Analysis**: Trace decisions to source data
2. **Impact Assessment**: Understand downstream effects
3. **Contamination Detection**: Find data leakage paths
4. **Audit Trails**: Complete decision provenance

### API Design

```python
class EventLineage:
    def trace_upstream(event_id) -> LineageGraph
    def trace_downstream(event_id) -> LineageGraph
    def detect_contamination_paths(source, target) -> list[Path]
    def analyze_impact(change_event_id) -> ImpactAnalysis
    def visualize_lineage(event_id, depth) -> LineageVisualization
```

---

## 10. Snapshot-Scoped Permissions

### Core Capabilities

**To Be Implemented** ⏳:
- [ ] Bind permissions to data versions
- [ ] Version-specific access control
- [ ] Permission inheritance across snapshots
- [ ] Audit permission changes
- [ ] Time-bounded access grants

### Use Cases

1. **Data Governance**: Control access to specific data versions
2. **Compliance**: Ensure proper data access controls
3. **Experiment Isolation**: Restrict access to experimental data
4. **Audit Requirements**: Track who accessed what version when

### API Design

```python
class SnapshotPermissions:
    def grant_snapshot_access(user, snapshot, permissions) -> Grant
    def revoke_snapshot_access(user, snapshot) -> None
    def check_snapshot_permission(user, snapshot, operation) -> bool
    def list_user_snapshots(user) -> list[SnapshotAccess]
    def audit_snapshot_access(snapshot, time_range) -> list[AccessLog]
```

---

## 11. Feature Priority

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

4. **Hallucination Firewall** ⏳
   - Verify LLM claims against snapshot data
   - Block contradictory responses

### P1 - Near-term Implementation (Enhanced Features)

5. **Time Machine Enhancement**
   - `diff_checkpoints()` - Compare checkpoints
   - `search_checkpoints()` - Search checkpoints

6. **Sandbox Merge**
   - `diff_sandbox_with_main()` - Difference comparison
   - `merge_sandbox_to_main()` - Merge back to main

7. **Cost-Aware Branching**
   - Historical cost queries
   - Budget enforcement

8. **Data-Versioned Prompt Evolution**
   - Prompt A/B testing via branches
   - Quality-gated merging

### P2 - Long-term Planning (Advanced Features)

9. **PITR Integration**
10. **Training Data Pipeline**
11. **Event Lineage Graph**
12. **Snapshot-Scoped Permissions**
13. **Visualization Tools**
14. **Multi-tenancy**

---

## 12. MatrixOne Capability Mapping

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

## 13. Implementation Roadmap

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

### Phase 4: Hallucination Firewall (3 weeks)
- Claim extraction from LLM responses
- Snapshot-consistent verification
- Response blocking logic

### Phase 5: Cost-Aware Branching (2 weeks)
- Historical cost queries
- Budget enforcement
- Alternative suggestions

### Phase 6: Prompt Evolution (4 weeks)
- Branch-based prompt testing
- Quality measurement
- Automated merging

### Phase 7: Training Data Pipeline (6 weeks)
- Dataset versioning
- SFT pair generation
- Contamination detection

### Phase 8: Event Lineage (4 weeks)
- Lineage graph construction
- Impact analysis
- Visualization

### Phase 9: PITR Integration (1 month)
- Enable PITR
- Time-point queries
- Automatic history management

### Phase 10: Snapshot Permissions (3 weeks)
- Version-scoped access control
- Permission inheritance
- Audit trails

---

## 14. Current Status

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

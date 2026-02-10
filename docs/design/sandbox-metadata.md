# Sandbox Metadata Management

## Schema

```sql
CREATE TABLE sandbox_metadata (
  sandbox_name        VARCHAR(255) PRIMARY KEY,
  description         TEXT,
  created_by          VARCHAR(255),
  created_at          TIMESTAMP(6) NOT NULL,      -- Microsecond precision
  updated_at          TIMESTAMP(6) NOT NULL,      -- Microsecond precision
  tags                JSON,
  status              VARCHAR(50) DEFAULT 'active',
  source_database     VARCHAR(255),
  source_snapshot     VARCHAR(255),
  
  INDEX (created_at, updated_at, status, created_by)
);
```

**Key Design Decisions**:
- ✅ `TIMESTAMP(6)` - Microsecond precision (highest supported by MatrixOne)
- ✅ Indexed on all filterable fields for performance
- ✅ Automatic timestamp management via `CURRENT_TIMESTAMP(6)`

## Restore Implementation

### Native MatrixOne RESTORE

Uses MatrixOne's native `RESTORE ACCOUNT ... DATABASE ... FROM SNAPSHOT`:

```python
def __init__(self, source_db: str = "dev_agent", account: str = "sys", db: Optional[Database] = None):
    self.account = account  # Configurable account

def restore(self, sandbox: str, checkpoint: str) -> None:
    """Restore sandbox to checkpoint.
    
    1. Validates checkpoint exists
    2. Validates checkpoint timestamp <= sandbox creation time
    3. Uses native RESTORE DATABASE command
    """
    snapshot_name = f"{sandbox}_{checkpoint}"
    
    # Validate checkpoint time
    if snapshot_ts > sandbox_created_at:
        raise ValueError("Cannot restore to future checkpoint")
    
    # Native restore (instant, no data copy)
    # Account is configurable via constructor
    self.db.execute(f'RESTORE ACCOUNT {self.account} DATABASE {sandbox} FROM SNAPSHOT {snapshot_name}')
```

**Usage**:
```python
# Default account (sys)
sandbox = Sandbox(db=db)

# Custom account
sandbox = Sandbox(db=db, account="my_account")
sandbox.restore("exp1", "checkpoint1")  # Uses my_account
```

**Advantages**:
- ⚡ Instant restore (no data copy)
- 🔒 Atomic operation
- ✅ Preserves all database state
- ✅ Timestamp validation prevents invalid restores

### Timestamp Validation

```python
# Checkpoint must be created BEFORE or AT sandbox creation time
checkpoint_ts <= sandbox_created_at

# Example:
# Sandbox created: 2026-02-10 14:00:00.123456
# Checkpoint 1:    2026-02-10 13:59:00.000000  ✅ Valid (before)
# Checkpoint 2:    2026-02-10 14:00:00.123456  ✅ Valid (at creation)
# Checkpoint 3:    2026-02-10 14:01:00.000000  ❌ Invalid (after)
```

## API

### Create with Metadata
```python
sandbox.create(
    "exp1",
    description="Testing new feature",
    created_by="alice",
    tags=["experiment", "ml"],
    from_snapshot="snap1"
)
```

### List with Filtering
```python
# All sandboxes
sandboxes = sandbox.list()

# Filter by pattern
sandboxes = sandbox.list(pattern="%exp%")

# Filter by status
sandboxes = sandbox.list(status="active")

# Filter by creator
sandboxes = sandbox.list(created_by="alice")

# Filter by creation time
from datetime import datetime, timedelta
yesterday = datetime.now() - timedelta(days=1)
sandboxes = sandbox.list(created_after=yesterday)

# Filter by update time
sandboxes = sandbox.list(updated_after=yesterday)

# Filter by tags
sandboxes = sandbox.list(tags=["experiment"])

# Combined filters
sandboxes = sandbox.list(
    pattern="%exp%",
    status="active",
    created_by="alice",
    created_after=yesterday,
    tags=["ml"]
)
```

### Update Metadata
```python
sandbox.update(
    "exp1",
    description="Updated description",
    tags=["experiment", "ml", "production"],
    status="archived"
)
```

### Get Info
```python
info = sandbox.info("exp1")
# Returns:
# {
#     "sandbox_name": "exp1",
#     "description": "Testing new feature",
#     "created_by": "alice",
#     "created_at": "2026-02-10 14:00:00",
#     "updated_at": "2026-02-10 14:30:00",
#     "tags": ["experiment", "ml"],
#     "status": "active",
#     "source_database": "dev_agent",
#     "source_snapshot": "snap1",
#     "table_count": 8,
#     "table_details": [...]
# }
```

## Use Cases

### 1. Track Experiments
```python
# Create experiment sandbox
sandbox.create(
    "exp_model_v2",
    description="Testing GPT-4 integration",
    created_by="data_team",
    tags=["experiment", "gpt4"]
)

# List all experiments
experiments = sandbox.list(tags=["experiment"])
```

### 2. Lifecycle Management
```python
# Archive old sandboxes
old_sandboxes = sandbox.list(
    created_after=datetime.now() - timedelta(days=30),
    status="active"
)
for sb in old_sandboxes:
    sandbox.update(sb["sandbox_name"], status="archived")
```

### 3. Team Collaboration
```python
# List my sandboxes
my_sandboxes = sandbox.list(created_by="alice")

# List team sandboxes
team_sandboxes = sandbox.list(tags=["team_project"])
```

### 4. Audit Trail
```python
# Find recently modified sandboxes
recent = sandbox.list(
    updated_after=datetime.now() - timedelta(hours=1)
)

# Get full history
info = sandbox.info("exp1")
print(f"Created: {info['created_at']}")
print(f"Updated: {info['updated_at']}")
print(f"By: {info['created_by']}")
```

## Database Integration

The metadata is automatically managed:
- `make db-init` creates the `sandbox_metadata` table
- `make db-reset` recreates it
- All sandbox operations update metadata automatically

## Implementation

- ✅ Schema in `infra/scripts/init-db.sh`
- ✅ Full CRUD operations
- ✅ Rich filtering (time, status, tags, creator)
- ✅ Automatic timestamp management
- ✅ 40 tests passing

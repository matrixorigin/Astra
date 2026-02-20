# Code Execution

> **Status**: Design — ready for implementation
> **Last Updated**: 2026-02-20
> **Dependencies**: [Data Versioning](data-versioning.md), [Trust and Safety](trust-and-safety.md), [Skills and Tools](skills-and-tools.md)

---

## 1. Problem

An agent that can only talk is a chatbot. An agent that can execute code is a worker.

The industry has converged on this: OpenAI Codex runs code in sandboxed VMs. Glean (Feb 2026) argues agents need sandboxes as "persistent short-term memory." The Plan-Code-Execute pattern shows agents creating their own tools at runtime via code generation.

**The question is no longer whether agents should execute code, but how.**

---

## 2. Industry Landscape (2026)

| Platform | Isolation | Data Access | Checkpoint | Working Memory |
|----------|-----------|-------------|------------|----------------|
| E2B | Firecracker microVM | None | None | Ephemeral |
| Sprites (Fly.io) | Firecracker | Files only | Filesystem (~1s) | Persistent VM |
| Daytona | Docker | None | None | Persistent |
| ragflow | Docker + gVisor | None | None | Container recycled |
| Glean | VPC-isolated | Enterprise Graph | None | File system |
| Databricks | Cluster | Full warehouse | None | Notebook state |

**Gap**: Nobody combines execution isolation with database-level data versioning. Sprites checkpoints filesystems. Databricks gives data access without isolation. Nobody does both.

---

## 3. Concepts

Four independent concerns:

```
┌──────────────────────────────────────────────────────────────┐
│                       CodeExecutor                            │
│                    (orchestration service)                     │
│                                                               │
│  ┌──────────────┐  ┌───────────────┐  ┌────────────────────┐│
│  │ SecurityGuard │  │  DataContext   │  │     Runtime        ││
│  │              │  │               │  │                    ││
│  │ Static       │  │ Lifecycle:    │  │ SubprocessRuntime  ││
│  │ analysis     │  │  Session or   │  │ DockerRuntime      ││
│  │ (pre-exec)   │  │  Execution    │  │ E2BRuntime         ││
│  │              │  │               │  │ ...                ││
│  │ Import ctrl  │  │ Wraps:        │  │                    ││
│  │ Call detect  │  │  Sandbox      │  │ Implements:        ││
│  │              │  │  (CLONE)      │  │  execute()         ││
│  │              │  │  Branch       │  │  health_check()    ││
│  │              │  │  (diff/merge) │  │                    ││
│  └──────────────┘  └───────────────┘  └────────────────────┘│
└──────────────────────────────────────────────────────────────┘
```

### Runtime

An isolated environment that executes code. This is what the industry calls a "sandbox" — E2B, Sprites, Daytona are all runtimes.

Pluggable via ABC. Default is `SubprocessRuntime` (zero dependencies). Production uses `DockerRuntime`. Cloud uses `E2BRuntime`.

A runtime knows nothing about data, security, or orchestration. It takes code + env vars + resource limits, runs it, returns stdout/stderr/exit_code.

### DataContext

Manages the data environment for code execution. Two lifecycle modes:

| Mode | Lifecycle | Use Case |
|------|-----------|----------|
| **Execution-scoped** | Created before execution, destroyed after | One-off queries, stateless analysis |
| **Session-scoped** | Created on first data access, destroyed on session end | Multi-step analysis, working memory |

A DataContext wraps the existing `Sandbox` class (CLONE/SNAPSHOT/RESTORE) and `Branch` class (diff/merge). It adds lifecycle management and access control — the sandbox DB is created with a **read-only** or **read-write** database user depending on `DataAccessLevel`.

### SecurityGuard

Pre-execution static analysis. Rejects code before it reaches any runtime. This is defense-in-depth — even if the runtime has its own isolation, we don't send obviously dangerous code to it.

Not a sandbox. Not a runtime. A gate.

### CodeExecutor

Orchestration service that composes the above three. Not a sandbox — it's a service. Callers (skills, ChatLoop) interact only with this.

---

## 4. Design

### 4.1 Runtime Interface

```python
@dataclass
class ResourceProfile:
    max_memory_mb: int = 256
    max_cpu_seconds: int = 30
    max_wall_seconds: int = 60
    max_output_bytes: int = 1_048_576   # 1MB stdout cap
    network_enabled: bool = False

@dataclass
class ExecutionResult:
    stdout: str
    stderr: str
    exit_code: int
    execution_time_ms: float
    truncated: bool = False             # True if stdout hit max_output_bytes

class Runtime(ABC):
    @abstractmethod
    def execute(self, code: str, language: str,
                resources: ResourceProfile,
                env: dict[str, str] | None = None) -> ExecutionResult: ...

    @abstractmethod
    def health_check(self) -> bool: ...

    @property
    @abstractmethod
    def supported_languages(self) -> list[str]: ...
```

Why `ResourceProfile` instead of individual params:
- Different use cases need different profiles (data analysis: 1GB/60s, simple calc: 64MB/5s)
- Profiles can be named and reused (`PROFILE_DATA_ANALYSIS`, `PROFILE_LIGHTWEIGHT`)
- Adding new resource dimensions doesn't change the interface

### 4.2 DataContext

```python
class DataAccessLevel(Enum):
    NONE = "none"       # No database access
    READ = "read"       # Clone with read-only DB user
    WRITE = "write"     # Clone with read-write DB user + auto-checkpoint

class DataContextScope(Enum):
    EXECUTION = "execution"   # Destroyed after single execution
    SESSION = "session"       # Persists across executions within session

class DataContext:
    def __init__(self, db: Session, sandbox: Sandbox,
                 scope: DataContextScope, access: DataAccessLevel): ...

    @property
    def dsn(self) -> str:
        """Connection string for the sandbox DB (read-only or read-write)."""

    @property
    def alive(self) -> bool:
        """Whether the sandbox DB still exists."""

    def ensure_created(self) -> None:
        """Create sandbox DB if not yet created (idempotent)."""

    def checkpoint(self, name: str = "pre_exec") -> None:
        """SNAPSHOT current state. Only valid for WRITE access."""

    def restore(self, name: str = "pre_exec") -> None:
        """RESTORE to checkpoint. Atomic rollback."""

    def diff(self, tables: list[str] | None = None) -> list[TableDiff]:
        """Compare sandbox vs source. Uses snapshot-based comparison."""

    def merge(self, tables: list[str] | None = None) -> MergeResult:
        """Apply sandbox changes back to source DB."""

    def destroy(self) -> None:
        """DROP sandbox database. Idempotent."""
```

**Access control via DB user** (not AST heuristics):
- `READ` → connects with a user that has `SELECT` only on the sandbox DB
- `WRITE` → connects with a user that has full DML on the sandbox DB
- Neither can access the source DB — the DSN points only to the clone

**Diff implementation**: Not brute-force `SELECT *` comparison. Uses MatrixOne snapshot comparison:
```sql
-- Rows in sandbox that differ from source snapshot
SELECT s.* FROM sandbox_db.sessions s
WHERE NOT EXISTS (
    SELECT 1 FROM source_db.sessions{SNAPSHOT='pre_exec'} src
    WHERE src.session_id = s.session_id
    AND src.cost_category <=> s.cost_category
    -- ... all columns
)
```
This is efficient because it leverages the MVCC snapshot that already exists from the checkpoint. No full table scan of both databases.

### 4.3 SecurityGuard

```python
@dataclass
class SecurityVerdict:
    safe: bool
    issues: list[SecurityIssue]

@dataclass
class SecurityIssue:
    category: str       # "dangerous_import", "dangerous_call", "sql_injection"
    description: str
    line: int

class SecurityGuard:
    def __init__(self, deny_imports: set[str] | None = None,
                 allow_imports: set[str] | None = None): ...

    def analyze(self, code: str, language: str,
                extra_allowed: list[str] | None = None) -> SecurityVerdict: ...
```

Default deny list (dangerous modules):
```
os, subprocess, sys, shutil, socket, ctypes, pickle,
multiprocessing, http, ftplib, telnetlib, signal, importlib
```

Default allow list (safe for data work):
```
json, math, datetime, re, collections, itertools, functools,
typing, dataclasses, decimal, statistics, csv, io, hashlib, uuid
```

Callers extend per-execution: `extra_allowed=["pandas", "numpy", "matplotlib"]`

**What SecurityGuard does NOT do**: SQL injection detection. That was a bad idea in v2 — AST can't reliably detect SQL injection patterns, and it's the wrong layer. Data safety is enforced by `DataContext` via DB user permissions (read-only user can't write, period).

### 4.4 CodeExecutor

```python
@dataclass
class CodeExecutionRequest:
    code: str
    language: str = "python"
    resources: ResourceProfile = field(default_factory=ResourceProfile)
    session_id: str | None = None
    data_access: DataAccessLevel = DataAccessLevel.NONE
    data_scope: DataContextScope = DataContextScope.EXECUTION
    allowed_imports: list[str] | None = None

@dataclass
class CodeExecutionResult:
    execution: ExecutionResult
    security: SecurityVerdict
    data_snapshot_id: str | None = None
    data_diff: list[TableDiff] | None = None   # Only for WRITE mode

class CodeExecutor:
    def __init__(self, runtime: Runtime, db: Session,
                 sandbox: Sandbox, security: SecurityGuard): ...

    def execute(self, request: CodeExecutionRequest) -> CodeExecutionResult: ...

    def get_or_create_data_context(
        self, session_id: str, access: DataAccessLevel,
        scope: DataContextScope
    ) -> DataContext: ...
```

Execution flow:

```
execute(request)
  │
  ├─ 1. GUARD
  │     security.analyze(code, allowed_imports)
  │     → If unsafe: return immediately with issues, no execution
  │
  ├─ 2. DATA (if data_access != NONE)
  │     ├─ get_or_create_data_context(session_id, access, scope)
  │     │   → EXECUTION scope: always create new
  │     │   → SESSION scope: reuse existing or create
  │     ├─ context.ensure_created()                    # CLONE
  │     ├─ If WRITE: context.checkpoint("pre_exec")   # SNAPSHOT
  │     └─ env["MO_DSN"] = context.dsn
  │
  ├─ 3. EXECUTE
  │     runtime.execute(code, language, resources, env)
  │     → ExecutionResult
  │
  ├─ 4. POST-EXECUTE (if data_access == WRITE)
  │     ├─ If exit_code != 0: context.restore("pre_exec")
  │     └─ If exit_code == 0: data_diff = context.diff()
  │
  ├─ 5. CLEANUP (if EXECUTION scope)
  │     context.destroy()
  │
  └─ 6. Return CodeExecutionResult
```

**Session-scoped DataContext management**: `CodeExecutor` holds a `dict[str, DataContext]` keyed by session_id. Session-scoped contexts are created on first use and destroyed when the session closes (via a cleanup hook on `SessionManager`).

---

## 5. Innovation

### 5.1 Data Pull Requests

Git does PRs for code. Sprites does checkpoint/restore for filesystems. **We do PRs for data.**

```
CLONE → EXECUTE → DIFF → MERGE or DISCARD
```

| Step | Implementation | Cost |
|------|---------------|------|
| CLONE | `CREATE DATABASE ... CLONE source` | Zero-cost (MatrixOne copy-on-write) |
| EXECUTE | Code runs against clone | Normal execution |
| DIFF | Snapshot-based comparison (see 4.2) | Reads only changed rows |
| MERGE | `INSERT/UPDATE/DELETE` from clone to source | Proportional to changes |
| DISCARD | `DROP DATABASE clone` | Instant |

**Why this is novel**: The diff is data-aware. Not "47 disk blocks changed" (Sprites) or "3 files modified" (Git). It's "3 rows in sessions table: cost_category changed from NULL to 'high_cost'." Humans can review this.

### 5.2 Database as Agent Working Memory

Glean (Feb 2026) identified that agents need sandboxes as persistent short-term memory. Their approach: file system. Our approach: **database tables**.

```
Execution 1: "Analyze 10,000 sessions"
  → CREATE TABLE working.session_stats AS SELECT ... GROUP BY ...
  → Returns: "Created summary table with 847 high-cost sessions"

Execution 2: "Which users have the most high-cost sessions?"
  → SELECT user_id, COUNT(*) FROM working.session_stats GROUP BY user_id
  → Returns: top 10 users (queries the table from execution 1)

Execution 3: "Export the top 50 to CSV"
  → SELECT ... FROM working.session_stats ORDER BY ... LIMIT 50
  → Returns: CSV content in stdout
```

Why database tables beat files as working memory:
- **Queryable**: SQL aggregation, filtering, joining — not just `cat file.csv`
- **Composable**: Execution 2 can JOIN results from execution 1 with other tables
- **Scalable**: Database handles 10M rows; files in context window don't
- **Auditable**: Every intermediate state is time-travel queryable

This is enabled by `DataContextScope.SESSION` — the sandbox DB persists across executions within a session.

### 5.3 Execution Time-Travel

Every execution binds to a `context_snapshot_id`. After the fact:
- Query the exact data the code saw: `SELECT ... {SNAPSHOT = 'pre_exec'}`
- Reproduce the execution with identical inputs
- Audit: what data → what code → what output → what data changes

This closes the audit loop from [Trust and Safety](trust-and-safety.md). No other code execution platform offers this because none have a time-travel-capable database.

---

## 6. Security Model

Three independent layers. Any single layer failing does not compromise the system.

| Layer | What | Enforced By |
|-------|------|-------------|
| **Static analysis** | Reject dangerous code patterns before execution | SecurityGuard (AST) |
| **Runtime isolation** | Process/container/VM boundary, resource limits | Runtime implementation |
| **Data isolation** | Code runs against clone, not production; access level via DB user | DataContext + MatrixOne |

**Key design decision**: Data safety is enforced by database permissions, not by code analysis. A `READ` DataContext connects with a DB user that literally cannot execute `INSERT/UPDATE/DELETE`. This is unforgeable — no amount of clever Python code can bypass database-level permissions.

**SubprocessRuntime** (dev/demo): `setrlimit` + `timeout` + `tmpdir`. Sufficient for trusted environments. Not sufficient for untrusted code — AST is the primary defense.

**DockerRuntime** (production): `--read-only --tmpfs /workspace --network none --memory 256m --user nobody`. Optional gVisor (`--runtime=runsc`). Defense in depth with AST.

---

## 7. Skill Integration

```python
class ExecuteCodeSkill(Skill):
    name = "execute_code"
    version = "1.0.0"
    description = "Execute Python code in isolated environment with optional database access"
    side_effect_profile = SideEffectProfile(category=SideEffectCategory.WRITE)

    async def execute(self, input: ExecuteCodeInput) -> ExecuteCodeOutput:
        result = self.code_executor.execute(CodeExecutionRequest(
            code=input.code,
            data_access=DataAccessLevel(input.data_access),
            data_scope=DataContextScope.SESSION if input.session_id else DataContextScope.EXECUTION,
            session_id=input.session_id,
            allowed_imports=input.allowed_imports,
        ))
        return ExecuteCodeOutput(
            success=result.execution.exit_code == 0,
            result=result.execution.stdout,
            error=result.execution.stderr if result.execution.exit_code != 0 else None,
            data_diff=result.data_diff,
        )
```

The Skill layer handles the translation between `CodeExecutionResult` (executor domain) and `SkillOutput` (ChatLoop domain). CodeExecutor never knows about skills.

---

## 8. File Structure

```
core/
  sandbox/                          # Data versioning (existing, unchanged)
    sandbox.py                      #   CLONE / SNAPSHOT / RESTORE
    branch.py                       #   Diff / Merge
  runtime/                          # Code execution isolation (new)
    __init__.py                     #   Runtime ABC, ExecutionResult, ResourceProfile
    subprocess_runtime.py           #   Default runtime
  code_executor/                    # Orchestration (new)
    __init__.py                     #   CodeExecutor, Request/Result types
    security.py                     #   SecurityGuard
    data_context.py                 #   DataContext lifecycle management
  skills/
    builtin.py                      #   + ExecuteCodeSkill
```

---

## 9. Resource Lifecycle

### Storage Cost Model

MatrixOne CLONE and SNAPSHOT are copy-on-write. Actual storage cost:

| Operation | Storage Cost |
|-----------|-------------|
| `CLONE` (create sandbox DB) | ~0 (metadata only, data shared with source) |
| `SNAPSHOT` (checkpoint) | ~0 (MVCC timestamp marker) |
| `SELECT` in sandbox | 0 (reads shared pages) |
| `INSERT/UPDATE/DELETE` in sandbox | Proportional to modified rows only |
| `DROP DATABASE` (cleanup) | Releases modified pages |

A typical READ execution costs effectively zero storage. A WRITE execution costs only the delta.

**The real risk is not storage but metadata accumulation** — thousands of sandbox databases and snapshot objects slow down MatrixOne's metadata catalog. Cleanup is mandatory.

### Cleanup Strategy

Three tiers, matching DataContext lifecycle:

```
┌─────────────────────────────────────────────────────────┐
│  Tier 1: EXECUTION-SCOPED (immediate)                    │
│  Destroyed in CodeExecutor.execute() finally block.      │
│  Sandbox DB lives for seconds. Zero leak risk.           │
├─────────────────────────────────────────────────────────┤
│  Tier 2: SESSION-SCOPED (session end + TTL)              │
│  Destroyed when session closes (SessionManager hook).    │
│  Safety net: TTL of 1 hour — background task drops any   │
│  sandbox DB older than TTL with no active session.       │
├─────────────────────────────────────────────────────────┤
│  Tier 3: DATA PR PENDING (explicit + TTL)                │
│  Kept alive until human merges or discards.              │
│  Safety net: TTL of 24 hours — auto-discard with         │
│  warning event logged. Configurable per deployment.      │
└─────────────────────────────────────────────────────────┘
```

Implementation: a periodic cleanup task (reuses existing `MemoryGovernanceEngine` scheduling):

```sql
-- Find orphaned sandbox databases
SELECT sandbox_name, created_at, status FROM sandbox_metadata
WHERE sandbox_name LIKE 'code_exec_%'
  AND status = 'active'
  AND updated_at < NOW() - INTERVAL 1 HOUR
  AND sandbox_name NOT IN (SELECT ... FROM active_sessions ...)
```

Then `DROP DATABASE` + delete metadata for each orphan.

### Snapshot Cleanup

Snapshots within sandbox databases are cleaned up when the sandbox DB is dropped (MatrixOne cascades). No separate snapshot cleanup needed.

For session-scoped contexts with multiple checkpoints (e.g., 5 executions = 5 snapshots), only the latest checkpoint is kept. Previous checkpoints are dropped after each successful execution.

---

## 10. Implementation Plan

### Phase 1: MVP
- `Runtime` ABC + `SubprocessRuntime`
- `SecurityGuard` with AST analysis
- `DataContext` wrapping existing Sandbox (execution-scoped only)
- `CodeExecutor` orchestration
- `ExecuteCodeSkill`
- Tests

### Phase 2: Production
- `DockerRuntime` with gVisor + container pool
- Session-scoped DataContext with cleanup hooks
- Data PR workflow (diff visualization, merge/discard in ChatLoop)
- Background cleanup task for orphaned sandbox DBs

### Phase 3: Advanced
- Cloud runtimes (E2B, Daytona)
- Multi-language (SQL direct, shell)
- Interactive REPL mode
- Execution cost estimation

# Code Execution

> **Status**: Implementing (Phase 1 MVP)
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
│  │ analysis     │  │  Session-     │  │ DockerRuntime      ││
│  │ (pre-exec)   │  │  scoped      │  │ E2BRuntime         ││
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

An isolated environment that executes code. Pluggable via ABC. Default is `SubprocessRuntime` (zero dependencies). Production uses `DockerRuntime`.

A runtime knows nothing about data, security, or orchestration. It takes code + env vars + resource limits, runs it, returns stdout/stderr/exit_code/started_at.

### DataContext

Manages the data environment for code execution. **Session-scoped only** — created on first data access, destroyed on session end.

Key design: **table-level dynamic clone**. Not whole-database clone. Agent declares which tables it needs, DataContext clones only those tables into the sandbox DB. This minimizes the data blocks pinned by the sandbox.

A DataContext wraps the existing `Sandbox` class (CLONE/SNAPSHOT/RESTORE) and `Branch` class (diff/merge). Access control via database user — `code_exec_ro` for READ, `code_exec_rw` for WRITE.

### SecurityGuard

Pre-execution static analysis. Rejects code before it reaches any runtime. Defense-in-depth — even if the runtime has its own isolation, we don't send obviously dangerous code to it.

### CodeExecutor

Orchestration service that composes the above three. Callers (skills, ChatLoop) interact only with this.

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
    started_at: datetime | None = None  # UTC timestamp when execution began (for PITR)
```

### 4.2 Data Access Model

Three levels, each with clear semantics:

```
NONE  — Pure computation, no database access
READ  — Direct connection to source DB with read-only user, no sandbox
WRITE — Session-scoped sandbox with table-level clone, time-travel, Data PR
```

**WRITE mode constraints** (documented, not hidden):
- **Single source DB**: one session binds to one source database, immutable after first WRITE
- **Non-transparent**: code receives sandbox DB connection, not source DB
- **Declarative tables**: agent declares which tables to access; only those are cloned
- **Session lifecycle**: sandbox lives for the session duration

### 4.3 DataContext (WRITE mode)

```python
class DataContext:
    """Session-scoped sandbox with table-level zero-copy branch."""

    def ensure_created(self) -> None:
        """Create empty sandbox DB if not exists (idempotent)."""

    def ensure_tables(self, tables: list[str]) -> None:
        """Branch declared tables into sandbox (zero-copy, idempotent per table).
        Uses `data branch create table` — kernel tracks LCA automatically."""

    def diff(self, tables: list[str] | None = None) -> list[TableDiff]:
        """Diff sandbox tables against source using native `data branch diff`.
        Three-way diff with automatic LCA detection by kernel."""

    def merge(self, tables: list[str] | None = None, on_conflict: str = "skip") -> MergeResult:
        """Merge sandbox changes back to source using native `data branch merge`.
        Conflict strategies: error, skip, accept."""

    def destroy(self) -> None:
        """Cleanup: `data branch delete` per table + DROP DATABASE."""

    @property
    def dsn(self) -> str:
        """Connection string with access-appropriate DB user."""
```

**No snapshots needed.** `data branch create` records LCA in kernel metadata. `data branch diff` uses LCA for three-way comparison. `data branch merge` handles conflicts natively.

**Cleanup**:
```sql
data branch delete table sandbox_s1.orders;
data branch delete table sandbox_s1.products;
DROP DATABASE IF EXISTS sandbox_s1;
```

### 4.4 SecurityGuard

```python
class SecurityGuard:
    def analyze(self, code: str, language: str,
                extra_allowed: list[str] | None = None) -> SecurityVerdict: ...
```

Default deny: `os, subprocess, sys, shutil, socket, ctypes, pickle, multiprocessing, http, signal, importlib`
Default allow: `json, math, datetime, re, collections, itertools, functools, typing, decimal, statistics, csv, io, hashlib, uuid`

Data safety is enforced by DB user permissions, not by code analysis.

### 4.5 CodeExecutor

```python
@dataclass
class CodeExecutionRequest:
    code: str
    language: str = "python"
    resources: ResourceProfile = field(default_factory=ResourceProfile)
    session_id: str | None = None
    data_access: DataAccessLevel = DataAccessLevel.NONE
    source_db: str | None = None          # Required for WRITE
    tables: list[str] | None = None       # Required for WRITE — declares accessed tables
    allowed_imports: list[str] | None = None

@dataclass
class TimeTravelInfo:
    started_at: datetime           # Execution start UTC (PITR within GC window)
    source_db: str                 # Source database name
    sandbox_db: str                # Sandbox database name
    pre_snapshot: str              # Pre-execution snapshot name

@dataclass
class CodeExecutionResult:
    execution: ExecutionResult
    security: SecurityVerdict
    data_diff: list[TableDiff] | None = None
    time_travel: TimeTravelInfo | None = None  # Only for WRITE mode
```

Execution flow:

```
execute(request)
  │
  ├─ 1. GUARD
  │     security.analyze(code, allowed_imports)
  │     → If unsafe: return immediately, no execution
  │
  ├─ 2. DATA (if WRITE)
  │     ├─ get_or_create session context
  │     ├─ context.ensure_created()              # CREATE DATABASE (once)
  │     ├─ context.ensure_tables(tables)         # data branch create (zero-copy, idempotent)
  │     └─ env["MO_DATABASE"] = context.sandbox_name
  │
  ├─ 2b. DATA (if READ)
  │     └─ env["MO_DATABASE"] = source_db (read-only user)
  │
  ├─ 3. EXECUTE
  │     runtime.execute(code, language, resources, env)
  │
  ├─ 4. POST-EXECUTE (if WRITE, exit_code == 0)
  │     └─ diff = context.diff(tables)  # native data branch diff
  │
  └─ 5. Return CodeExecutionResult(time_travel=TimeTravelInfo(...))
```

---

## 5. Innovation

### 5.1 Data Pull Requests

Git does PRs for code. **We do PRs for data.**

```
data branch create → EXECUTE → data branch diff → data branch merge or DROP
```

The diff is data-aware and three-way (kernel auto-detects LCA): "3 rows in sessions table: cost_category changed from NULL to 'high_cost'." Humans can review this. Conflict strategies: error, skip, accept.

### 5.2 Database as Agent Working Memory

Session-scoped sandbox persists across executions. Agent creates intermediate tables, queries them in later executions. Database tables beat files as working memory — queryable, composable, scalable, auditable.

### 5.3 Execution Time-Travel

Every WRITE execution records a `TimeTravelInfo`:

```python
@dataclass
class TimeTravelInfo:
    started_at: datetime    # Execution start UTC (PITR within GC window)
    source_db: str          # Source database name
    sandbox_db: str         # Sandbox database name
```

**Input audit** (what did the agent see?):
- `started_at` — PITR timestamp, queryable within MatrixOne GC window
- Sandbox branch tables share data blocks with source — the branch IS the point-in-time view

**Output audit** (what did the agent change?):
- `data branch diff` — native three-way diff between sandbox and source, available as long as sandbox exists
- Stored in `CodeExecutionResult.data_diff`

**Audit lifecycle**:

| Time window | Input reproducible? | Output reproducible? |
|-------------|--------------------|--------------------|
| During session | ✅ sandbox branch = point-in-time view | ✅ diff available live |
| After session, within GC window | ✅ PITR on source DB | ⚠️ only stored diff |
| After GC window | ⚠️ only stored metadata | ⚠️ only stored diff |

No snapshots needed. The branch itself is the audit artifact.

---

## 6. Security Model

Three independent layers:

| Layer | What | Enforced By |
|-------|------|-------------|
| **Static analysis** | Reject dangerous code patterns | SecurityGuard (AST) |
| **Runtime isolation** | Process/container boundary, resource limits | Runtime |
| **Data isolation** | DB user permissions (read-only can't write) | DataContext + MatrixOne |

---

## 7. Resource Lifecycle

### Storage Cost

| Operation | Storage Cost |
|-----------|-------------|
| `CREATE DATABASE` (empty sandbox) | ~0 |
| `data branch create table` (per table) | ~0 (zero-copy, shared data blocks via refcount) |
| `SELECT` in sandbox | 0 (reads shared pages) |
| `INSERT/UPDATE/DELETE` in sandbox | Proportional to modified rows |
| `data branch delete` + `DROP DATABASE` | Releases modified pages, decrements refcount |

**GC impact**: Branch tables pin data blocks in source tables via reference counting. While pinned, source table compaction (LSM tree) cannot reclaim old versions. Impact is proportional to sandbox lifetime × source table write rate.

**Mitigation**: Session-scoped lifecycle keeps sandbox short-lived (minutes to hours). No snapshots — branches are the only pinning mechanism.

### Cleanup Strategy

```
┌─────────────────────────────────────────────────────────┐
│  Tier 1: SESSION-SCOPED (session end)                    │
│  `data branch delete` per table + DROP DATABASE.         │
│  No snapshots to clean up.                               │
├─────────────────────────────────────────────────────────┤
│  Tier 2: SAFETY NET (TTL)                                │
│  Background task drops sandbox DBs older than TTL with   │
│  no active session. Catches abandoned sessions.          │
├─────────────────────────────────────────────────────────┤
│  Tier 3: DATA PR PENDING (explicit)                      │
│  Kept alive until human merges or discards.              │
│  TTL of 24 hours, auto-discard with warning.             │
└─────────────────────────────────────────────────────────┘
```

---

## 8. File Structure

```
core/
  sandbox/                          # Data versioning (existing)
    sandbox.py                      #   CLONE / SNAPSHOT / RESTORE (legacy)
    branch.py                       #   data branch create/diff/merge/delete (primary)
  runtime/                          # Code execution isolation
    __init__.py                     #   Runtime ABC, ExecutionResult, ResourceProfile
    subprocess_runtime.py           #   Default runtime
  code_executor/                    # Orchestration
    __init__.py                     #   CodeExecutor, Request/Result types, TimeTravelInfo
    security.py                     #   SecurityGuard
    data_context.py                 #   DataContext (session-scoped, table-level branch)
  skills/
    builtin.py                      #   + ExecuteCodeSkill
```

---

## 9. Implementation Plan

### Phase 1: MVP (current)
- ✅ `Runtime` ABC + `SubprocessRuntime`
- ✅ `SecurityGuard` with AST analysis
- ✅ `CodeExecutor` orchestration
- ✅ `ExecuteCodeSkill`
- 🔄 `DataContext` — session-scoped, table-level clone, time-travel, transactional cleanup

### Phase 2: Production
- `DockerRuntime` with gVisor + container pool
- Data PR workflow (diff visualization, merge/discard in ChatLoop)
- Background cleanup task for orphaned sandbox DBs

### Phase 3: Advanced
- Transparent sandbox (requires MatrixOne kernel: connection-level isolation)
- Multi-database sandbox support
- Cloud runtimes (E2B, Daytona)
- Interactive REPL mode

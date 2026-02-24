# Code Execution

> **Status**: Implementing (Phase 1 MVP)
> **Last Updated**: 2026-02-25
> **Dependencies**: [Data Versioning](data-versioning.md), [Trust and Safety](trust-and-safety.md), [Skills and Tools](skills-and-tools.md), [Edge-Cloud Execution](edge-cloud-execution.md)

---

## 1. Problem

An agent that can only talk is a chatbot. An agent that can execute code is a worker.

The industry has converged on this: OpenAI Codex runs code in sandboxed VMs. Glean (Feb 2026) argues agents need sandboxes as "persistent short-term memory." The Plan-Code-Execute pattern shows agents creating their own tools at runtime via code generation.

**The question is no longer whether agents should execute code, but how.**

### Execution Location in Edge-Cloud Architecture

Code execution is a **cloud skill**. Unlike file/shell/git tools that must run on the user's machine (edge), code sandbox requires isolation guarantees (Docker, resource limits, network control) that the edge cannot reliably provide. The cloud manages sandbox lifecycle, data context (CLONE/Branch), and security enforcement.

Edge agents invoke code execution via tool calls; the cloud intercepts, executes in sandbox, and returns results. See [Edge-Cloud Execution § Skill Classification](edge-cloud-execution.md).

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

An isolated environment that executes code. Pluggable via ABC with **self-describing capabilities**.

Each runtime declares its `RuntimeCapabilities`:

| Property | Meaning | SubprocessRuntime | DockerRuntime | FirecrackerRuntime |
|----------|---------|-------------------|---------------|-------------------|
| `isolation` | Host boundary strength | PROCESS | CONTAINER | MICROVM |
| `network_isolatable` | Can disable network per-execution | ❌ | ✅ | ✅ |
| `filesystem_isolated` | Code cannot access host FS | ❌ | ✅ | ✅ |
| `resource_limits` | Enforces memory/CPU limits | Linux only | ✅ | ✅ |
| `reproducible` | Same code+env → same result | ❌ | ✅ | ✅ |

Upper layers use capabilities for decisions:
- **CodeExecutor**: injects capabilities as `MO_RUNTIME_*` env vars so executed code can adapt
- **SecurityGuard**: could relax AST checks when isolation ≥ CONTAINER (defense-in-depth still applies)
- **create_runtime()**: factory selects the best available runtime matching caller's requirements

A runtime knows nothing about data, security, or orchestration. It takes code + env vars + resource limits, runs it, returns stdout/stderr/exit_code/started_at.

### DataContext

Manages the data environment for code execution. **Session-scoped only** — created on first data access, destroyed on session end.

Key design: **table-level dynamic clone**. Not whole-database clone. Agent declares which tables it needs, DataContext clones only those tables into the sandbox DB. This minimizes the data blocks pinned by the sandbox.

A DataContext wraps the existing `Sandbox` class (CLONE/SNAPSHOT/RESTORE) and `Branch` class (diff/merge). Access control is configured at deployment time — the DB credentials in the agent's DSN determine what operations are permitted. DataContext does not manage permissions at runtime.

### SecurityGuard

Pre-execution static analysis. Rejects code before it reaches any runtime. Defense-in-depth — even if the runtime has its own isolation, we don't send obviously dangerous code to it.

### CodeExecutor

Orchestration service that composes the above three. Callers (skills, ChatLoop) interact only with this.

---

## 4. Design

### 4.1 Runtime Interface

```python
class IsolationLevel(str, Enum):
    NONE = "none"            # No isolation (e.g. eval in-process)
    PROCESS = "process"      # Separate process, rlimit
    CONTAINER = "container"  # Docker container
    MICROVM = "microvm"      # Firecracker / gVisor

@dataclass(frozen=True)
class RuntimeCapabilities:
    isolation: IsolationLevel
    network_isolatable: bool
    filesystem_isolated: bool
    resource_limits: bool
    reproducible: bool

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

class Runtime(ABC):
    @property
    @abstractmethod
    def capabilities(self) -> RuntimeCapabilities: ...

    @abstractmethod
    def execute(self, code, language, resources, env) -> ExecutionResult: ...

    @abstractmethod
    def health_check(self) -> bool: ...

def create_runtime(
    *,
    min_isolation: IsolationLevel = IsolationLevel.PROCESS,
    require_network_isolation: bool = False,
    image: str | None = None,
) -> Runtime:
    """Factory: tries Docker → Subprocess, raises if nothing satisfies constraints."""
```

**Capability-aware code execution**: CodeExecutor injects runtime capabilities as environment variables (`MO_RUNTIME_ISOLATION`, `MO_RUNTIME_FS_ISOLATED`, etc.). Executed code can read these to adapt behavior — e.g., use in-memory buffers instead of temp files when filesystem is not isolated.

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

Data safety is enforced by sandbox isolation (code operates on a branch, not the source) and deployment-time DB user configuration, not by runtime GRANT or code analysis.

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

Three independent layers (defense-in-depth):

| Layer | What | Enforced By |
|-------|------|-------------|
| **Static analysis** | Reject dangerous code patterns | SecurityGuard (AST) |
| **Runtime isolation** | Process/container/microVM boundary, resource limits | Runtime |
| **Data isolation** | Sandbox boundary + deployment-time DB user config | DataContext + MatrixOne |

**Capability-aware security**: When `runtime.capabilities.isolation >= CONTAINER`, the runtime itself is the primary security boundary. AST analysis remains as defense-in-depth but could be relaxed for known-safe patterns. When isolation is PROCESS only, AST analysis is critical.

**Runtime selection per environment**:

| Environment | Runtime | Isolation | Why |
|-------------|---------|-----------|-----|
| Local dev / CLI | SubprocessRuntime | PROCESS | No Docker dependency, fast |
| API / staging | DockerRuntime | CONTAINER | Untrusted code, network isolation |
| Production | FirecrackerRuntime | MICROVM | Strongest isolation, sub-second boot |

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
│  Tier 0: SYNCHRONOUS (session close API)                 │
│  SessionService._cleanup_sandbox() queries               │
│  sandbox_metadata by session_id, deletes immediately.    │
├─────────────────────────────────────────────────────────┤
│  Tier 1: SESSION-SCOPED (session end callback)           │
│  SessionManager.close_session(on_close=...) triggers     │
│  CodeExecutor.cleanup_session() → DataContext.destroy()  │
│  `data branch delete` per table + DROP DATABASE.         │
├─────────────────────────────────────────────────────────┤
│  Tier 2: BACKGROUND SCAN (hourly, distributed lock)      │
│  SandboxCleaner.run() via GovernanceTaskRunner:          │
│  - Closed session sandboxes (Tier 0/1 missed)           │
│  - Zombie sessions (active but no activity > TTL)        │
│  - Expired unbound (no session_id, older than TTL)       │
│  - Orphan databases (no metadata entry at all)           │
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
  sandbox/                          # Data versioning
    sandbox.py                      #   Sandbox (branch-based: create/delete/add_table/snapshot/restore/diff/merge)
    branch.py                       #   Branch (data branch create/diff/merge/delete)
    cleanup.py                      #   SandboxCleaner (4-tier background cleanup)
  runtime/                          # Code execution isolation
    __init__.py                     #   Runtime ABC, RuntimeCapabilities, IsolationLevel, create_runtime()
    subprocess_runtime.py           #   SubprocessRuntime (dev/trusted, rlimit)
    docker_runtime.py               #   DockerRuntime (production, container isolation)
  code_executor/                    # Orchestration
    __init__.py                     #   CodeExecutor, Request/Result types, TimeTravelInfo
    security.py                     #   SecurityGuard (AST static analysis)
    data_context.py                 #   DataContext (session-scoped, table-level branch, metadata tracking)
  skills/
    builtin.py                      #   ExecuteCodeSkill (registered via register_builtin_skills)
  context/
    lifecycle.py                    #   MemoryGovernanceEngine.run_hourly_tasks() → SandboxCleaner
    scheduler.py                    #   GovernanceTaskRunner (distributed lock + heartbeat)
api/
  services/
    session_service.py              #   _cleanup_sandbox() on session close (Tier 0)
  routers/
    streaming.py                    #   ChatLoop + CodeExecutor wiring
```

---

## 9. Implementation Plan

### Phase 1: MVP ✅
- ✅ `Runtime` ABC + `SubprocessRuntime`
- ✅ `SecurityGuard` with AST analysis
- ✅ `CodeExecutor` orchestration
- ✅ `ExecuteCodeSkill` registered as ChatLoop tool
- ✅ `DataContext` — session-scoped, table-level branch, time-travel, metadata tracking

### Phase 2: Production ✅
- ✅ `DockerRuntime` — container isolation, cap_drop=ALL, no-new-privileges, network isolation
- ✅ `RuntimeCapabilities` — self-describing isolation/network/filesystem/resource properties
- ✅ `create_runtime()` factory — capability-based selection with fallback
- ✅ Capability injection — `MO_RUNTIME_*` env vars for code-level adaptation
- ✅ `SandboxCleaner` — 4-tier background cleanup via GovernanceTaskRunner
- ✅ Session close → sandbox cleanup (Tier 0)
- ✅ CLI + API wiring — `CodeExecutor` → `register_builtin_skills` → ChatLoop

### Phase 3: Advanced
- `FirecrackerRuntime` — microVM isolation (Firecracker is Apache 2.0 open source, requires Linux host)
- Data PR workflow (diff visualization, merge/discard in ChatLoop)
- Transparent sandbox (requires MatrixOne kernel: connection-level isolation)
- Multi-database sandbox support
- Container pool (pre-warmed containers for sub-100ms cold start)
- Interactive REPL mode

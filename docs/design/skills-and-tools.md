# Skills and Tools

> **Status**: Core Design — single source of truth for skill system, packaging, selection, and tool integration
> **Last Updated**: 2026-02-28
>
> 🔵 **Implementation Status**: `SkillManager` (install/uninstall/credential CRUD) and `SkillPipeline` (unified selection) are implemented.
> Marketplace discovery, publishing, RBAC, and MatrixOne Publication distribution are Design Targets.

---

## The Shift: From Functions to Stateful Packages

The industry is moving from "tools as function calls" to "skills as modular expertise packages." Anthropic's Agent Skills introduces three-tier progressive loading. ElizaOS pioneered plugin schemas (plugins declare DB tables, platform auto-migrates).

mo-agent-engine goes further with **Skill-as-Package**: skills are platform capabilities with platform-defined schemas and typed API layers. All skill tables live in the platform database with `sk_{skill}_{table}` naming convention. Users interact with skill data through skill APIs, not direct SQL. Each skill defines its own tables in `skills/{name}/models.py`.

---

## 1. Skill Architecture

### What a Skill Is

A skill is a **versioned, stateful capability package** with:

- **Identity**: name, version (semver), description
- **Schema**: database tables defined by platform (`sk_{skill}_{table}` in platform DB)
- **API layer**: typed interface for data access (users don't write direct SQL)
- **Credentials**: what secrets it needs (e.g. GitHub token, per-user encrypted)
- **Requirements**: what it needs (permissions, platform capabilities)
- **Side-effect profile**: read / write / destructive (see [Trust and Safety](trust-and-safety.md))
- **Progressive disclosure**: metadata → summary → full instructions
- **Execution logic**: the actual code
- **Audit trail**: every invocation recorded with version, params, result

### Core Insight: Skills Are Platform Capabilities

**Skills are platform capabilities, not user plugins.** Their table schemas are deterministic — defined by the platform, not by users. This is the same model as `knowledge_entries` and `conversation_events`: platform defines the schema, skill provides an API layer for CRUD operations.

All tables — core and skill — live in the **same platform database**. Skill tables are distinguished by naming convention (`sk_{skill_name}_`), not by physical database separation.

Users interact with skill data through **skill APIs**, not direct SQL:
```python
github.save_token(token)                    # → encrypted in user_credentials
github.add_repo("matrixorigin/matrixone")   # → INSERT INTO sk_github_repos
github.list_prs(repo, state="open")         # → GitHub API + cache
github.get_pr_checks(repo, pr_number)       # → GitHub API + cache
```

### Database Architecture

```
┌──────────────────────────────────────────────────────────────┐
│                     Platform Database                         │
│                  (single DB, managed by platform)             │
│                                                               │
│  Core tables (no prefix):                                     │
│    users, roles, sessions, conversation_events,               │
│    agents, model_registry                                     │
│                                                               │
│  Skill infrastructure tables (no prefix, part of core):       │
│    skills_registry, skill_permissions,                        │
│    skill_installations, user_credentials                      │
│                                                               │
│  Skill business tables (sk_{skill}_ prefix):                  │
│    sk_github_repos, sk_github_pr_cache                        │
│    sk_knowledge_entries, sk_knowledge_relations                │
│                                                               │
│  All tables share the same Base, same init_db().              │
│  Skill tables defined in skills/{name}/models.py              │
└──────────────────────────────────────────────────────────────┘
```

### Table Naming Convention

| Category | Prefix | Defined in | Examples |
|----------|--------|------------|----------|
| Core platform | (none) | `api/models/` | `users`, `roles`, `sessions`, `agents` |
| Skill infrastructure | (none) | `api/models/skill.py` | `skills_registry`, `skill_installations`, `skill_permissions`, `user_credentials` |
| Skill business data | `sk_{skill}_` | `skills/{skill}/models.py` | `sk_github_repos`, `sk_github_pr_cache` |

Rules:
- Skill infrastructure tables are part of core — they manage the skill lifecycle, not skill-specific data
- Skill business tables use `sk_{skill_name}_` prefix — matches `table_prefix` in manifest
- All tables use the same `Base` from `api/models.py`
- `init_db()` imports all skill models and calls `Base.metadata.create_all()`

### Why No BYOD for Skill Tables

Previous design (v2) had skill tables in a user-provided BYOD database. This was wrong because:

1. **Skill tables are platform-defined** — the platform controls the schema, not the user
2. **JOINs** — skill data needs to join with core tables (`users`, `conversation_events`) for audit, context, and retrieval
3. **Complexity** — BYOD adds DDL execution on install, cross-DB query limitations, connection pooling per user
4. **No actual need** — users don't need data sovereignty over PR cache or knowledge entries; they need the platform to manage it

BYOD remains a future option for users who want to bring their own **business data** (not skill data) — but that's a separate concern from skill-as-package.

### Skill Package Structure

A skill is a platform-managed Python package:

```
skills/
  github/
    __init__.py
    manifest.yaml          # metadata, dependencies, credentials
    models.py              # SQLAlchemy table definitions (uses Base from api/models.py)
    api.py                 # typed API layer for data access
    actions.py             # skill actions (what the agent calls)
```

#### Manifest

```yaml
# skills/github/manifest.yaml
name: github
version: "1.0.0"
description: "GitHub integration — PRs, issues, CI status, code search"
author: "mo-agent-engine"
table_prefix: sk_github

tables:
  - sk_github_repos
  - sk_github_pr_cache

credentials:
  - name: github_token
    type: secret
    description: "GitHub Personal Access Token or App token"
    required: true

requires:
  - http

depends_on: []
```

#### Schema (Platform-Defined)

```python
# skills/github/models.py
"""GitHub skill tables — platform-defined, lives in platform DB."""

from api.models import Base  # same Base as all other tables

class SkGithubRepo(Base):
    __tablename__ = "sk_github_repos"
    __table_args__ = (
        UniqueConstraint("owner", "name", name="uq_sk_github_repo_owner_name"),
    )

    repo_id = Column(String(36), primary_key=True)
    owner = Column(String(100), nullable=False)
    name = Column(String(100), nullable=False)
    full_name = Column(String(200), nullable=False)
    default_branch = Column(String(100), default="main")
    created_at = Column(DateTime, nullable=False)

class SkGithubPRCache(Base):
    __tablename__ = "sk_github_pr_cache"
    __table_args__ = (
        UniqueConstraint("repo_full_name", "pr_number", name="uq_sk_github_pr_repo_pr"),
        Index("ix_sk_github_pr_cache_repo_state", "repo_full_name", "state"),
    )

    cache_id = Column(String(36), primary_key=True)
    repo_full_name = Column(String(200), nullable=False)
    pr_number = Column(Integer, nullable=False)
    title = Column(String(500))
    state = Column(String(20))          # open / closed / merged
    author = Column(String(100))
    ci_status = Column(String(20))      # success / failure / pending
    ci_conclusion = Column(String(20))
    data = Column(JSON)                  # full PR payload
    fetched_at = Column(DateTime, nullable=False)
```

#### Skill API Layer

```python
# skills/github/api.py
"""GitHub skill API — typed interface for data access."""

class GitHubSkillAPI:
    """API layer for GitHub skill data. Uses platform DB session."""

    def __init__(self, db: Session, credentials: dict[str, str]):
        self._db = db  # platform DB session
        self._token = credentials.get("github_token")
        self._client = Github(auth=Auth.Token(self._token)) if self._token else None

    def add_repo(self, owner: str, name: str) -> dict:
        """Register a repository for tracking."""

    def list_repos(self) -> list[dict]:
        """List registered repositories."""

    def list_prs(self, repo: str, state: str = "open", limit: int = 10) -> list[dict]:
        """List PRs. Fetches from GitHub API, caches in sk_github_pr_cache."""

    def get_pr_checks(self, repo: str, pr_number: int) -> dict:
        """Get CI/check status for a specific PR."""
```

#### Skill Actions (What the Agent Calls)

```python
# skills/github/actions.py
"""GitHub skill actions — registered as tools for the agent."""

class ListPRsAction:
    name = "github_list_prs"
    description = "List open/closed PRs in a repository"

    async def execute(self, api: GitHubSkillAPI, repo: str, state: str = "open") -> dict:
        return api.list_prs(repo, state)

class GetPRChecksAction:
    name = "github_get_pr_checks"
    description = "Get CI/check status for a PR"

    async def execute(self, api: GitHubSkillAPI, repo: str, pr_number: int) -> dict:
        return api.get_pr_checks(repo, pr_number)
```

### Skill Versioning

```
Register → Store in skills_registry (with version, code_hash, git_commit_hash)
         → Keep active version in memory
         → Archive old versions (never delete)

Execute  → Record skill_name + skill_version in conversation_events
         → Result logged with full provenance

Replay   → Load exact version from event metadata
         → Verify code_hash matches current skill code
         → ⚠️ If mismatch: warn that skill code changed since original execution
         → Execute with current skill logic (historical code loading not yet supported)
         → Compare results for drift detection

Upgrade  → New version triggers regression gate (see trust-and-safety.md)
         → Gate passes → activate new version
         → Gate fails → reject, keep old version
```

> **Known limitation**: Replay currently uses the live skill code, not the historical version. `code_hash` and `git_commit_hash` are recorded for audit but not used to load historical code. If skill logic changed between executions, replay results may differ. Short-term mitigation: replay warns on `code_hash` mismatch. Long-term: skill code versioning via artifact registry or git tags.

### Declarative Skill Definition

```yaml
# skill.yaml — declarative skill definition
name: code_review
version: 2.1.0
description: Review code changes for quality, security, and style
table_prefix: code_review    # tables: code_review_{table_name}

credentials:
  - name: github_token
    type: secret
    description: GitHub Personal Access Token
    required: true

triggers:
  keywords: [review, PR, pull request, code quality]

requirements:
  access: read

parameters:
  pr_number:
    type: integer
    description: Pull request number to review
    required: true
  focus_areas:
    type: array
    items: {type: string, enum: [quality, security, performance, style]}
    description: Areas to focus the review on
    default: [quality, security]

side_effects:
  category: read
  external_apis: [github]

progressive_disclosure:
  index_tokens: 25       # embedding index (never in LLM context)
  full_schema_tokens: 500 # complete tool schema (budget-gated)

mcp_compatible: true
```

---

## 2. Execution Model

Skills execute in two locations depending on what they need access to. See [Edge-Cloud Execution](edge-cloud-execution.md) for the full design.

**Edge skills** (execute on user's machine):
- File ops, shell, git, grep, glob, MCP servers
- Need local filesystem — server doesn't have it
- Executed by edge's tool router; cloud returns `tool_calls`, edge runs them

**Cloud skills** (execute on server):
- Knowledge search, memory recall, session history, marketplace
- Need platform data in MatrixOne
- Executed server-side during `/chat/turn` processing

**Execution paths**:
1. **Edge Tool** → EdgeChatLoop receives `tool_call` from cloud → `tool_router.execute()` → result sent back in next `/chat/turn`
2. **Cloud Skill** → `AgentExecutor.execute_skill()` → `ToolMockingLayer.execute()` → `skill.execute()` (server-side, within `/chat/turn`)
3. **MCP Tool** → `MCPBridge.call_tool()` → MCP server (separate process via stdio/HTTP, on edge)
4. **Scratchpad** → in-memory, no external call (server-side)

Safety is NOT achieved through isolation, but through:
- `SideEffectCategory` (READ/WRITE/DESTRUCTIVE) → approval gates
- Edge permission system (allow/ask/deny) for local tools
- `ToolMockingLayer` → replay mode blocks destructive ops
- MCP tools → naturally process-isolated

For **heavy background workloads** (model training, data collection), see
[Deployment Architecture § Background Jobs](deployment-architecture.md#3-execution-model-tools-vs-background-jobs).
These are NOT skills — they are jobs submitted via `/jobs` API.

### Runtime Execution Flow

```python
async def execute_skill(user_id, skill_name, params):
    # 1. Verify skill is installed
    installation = db.query(SkillInstallation).filter_by(
        user_id=user_id, skill_name=skill_name, status="installed"
    ).one_or_none()
    if not installation:
        raise SkillNotInstalled(f"Install first: /skill install {skill_name}")

    # 2. Get credentials
    creds = get_decrypted_credentials(user_id, skill_name)

    # 3. Create skill API instance (uses platform DB session)
    api = GitHubSkillAPI(db=db, credentials=creds)

    # 4. Execute action
    action = skill.get_action(params["action_name"])
    return await action.execute(api, **params)
```

### Progressive Disclosure (Anthropic-Aligned)

Following Anthropic's Agent Skills pattern and RAG-MCP research (Gan & Sun, 2025), skills load in two tiers with **real token accounting** and **semantic retrieval**:

```
Index Tier: EMBEDDING INDEX (always available, never in LLM context)
  Embedding vector of name + description + triggers
  Used by semantic retriever to find candidates — LLM never sees this tier.
  Cost: 0 prompt tokens (lives in vector index only)

Schema Tier: FULL TOOL SCHEMA (injected for budget-available candidates)
  Complete OpenAI tool JSON schema (from Pydantic model or default)
  Includes: name, description, parameters, detailed_instructions, examples, edge_cases
  Token cost: measured per-skill via len(json) // 4
  LLM sees full schemas and selects via native function calling in a single pass.
```

Note: Anthropic's Agent Skills uses a three-tier model (name → summary → full). We collapse summary and full into one tier because OpenAI-style function calling requires full parameter schemas — a summary-only ranking pass would double LLM latency for marginal benefit. The schema's `name` + `description` fields serve as the implicit summary during LLM candidate ranking.

**Key design principles** (learned from industry):
- **Real token measurement, not constants**: Each skill's schema cost is computed from actual serialized size, not hardcoded estimates. Schema sizes vary 3-5× across skills.
- **Budget is a hard cap**: If a skill doesn't fit the remaining budget, it is **excluded entirely** — no empty stubs. An empty-parameter stub wastes tokens and confuses the LLM.
- **Semantic retrieval is mandatory at scale**: RAG-MCP (arXiv:2505.03275) empirically shows keyword matching collapses beyond ~30 tools. Embedding-based retrieval achieves 3.2× accuracy improvement.
- **Semantic retrieval replaces LLM ranking**: The vector index does the candidate filtering (zero LLM cost). The LLM only sees budget-capped full schemas and selects via native function calling in a single pass.

**Why this matters**: With 50+ skills, putting all details in context wastes attention budget. Index Tier embeddings let the retriever find candidates without any prompt tokens. Schema Tier full schemas are loaded only for budget-available skills, and the LLM selects directly.

### Skill Types

| Type | Side Effects | Approval | Examples |
|------|-------------|----------|----------|
| **Read** | None | Auto | code_read, ci_status, search_code |
| **Write** | External state change | Configurable | create_pr, merge_pr, create_issue |
| **Destructive** | Irreversible change | Always required | delete_repo, force_push |
| **Compute** | Internal only | Auto | summarize, analyze, generate_tests |

### Skill Lifecycle

```
Draft → Registered → Active → Deprecated → Archived

Draft:       Development, not available to agents
Registered:  In registry, not yet active (pending gate)
Active:      Available to agents, regression-tested
Deprecated:  Still works, but new selections discouraged
Archived:    Read-only, available for replay only
```

---

## 3. Skill Selection Pipeline

> **Implementation**: `core/skills/pipeline.py` — `SkillPipeline` is the single public interface.
> Internal components (`selector.py`, `modern_selector.py`, `self_improving_selector.py`) are implementation details — external code must use `SkillPipeline` only.

### The Problem

With 50+ skills, the LLM can't efficiently choose from a flat list. Selection must be fast, accurate, and auditable. Research shows keyword matching collapses beyond ~30 tools (RAG-MCP, 2025). Semantic retrieval is mandatory, not optional.

### Unified Pipeline: Retrieve → Audit → Feedback

Previously, five selector classes existed with overlapping responsibilities (`SkillSelector`, `ModernSkillSelector`, `AuditableSkillSelector`, `SelfImprovingSelector`, `AgentSkillSelector`). These have been unified into a single `SkillPipeline`:

```
┌─────────────────────────────────────────────────────────┐
│                     SkillPipeline                        │
│                                                          │
│  ┌──────────────────────────────────────────────────┐   │
│  │ Stage 1: RETRIEVE + RANK                         │   │
│  │  Semantic vector search (<50ms, 0 prompt tokens) │   │
│  │  → Budget-controlled full schema loading         │   │
│  │  → Apply learned corrections                     │   │
│  │  Output: tools_schema + candidate metadata       │   │
│  └──────────────────────────────────────────────────┘   │
│                         │                                │
│  ┌──────────────────────▼───────────────────────────┐   │
│  │ Stage 2: AUDIT                                   │   │
│  │  Snapshot context → Record selection event        │   │
│  │  Output: event_id (for feedback linkage)          │   │
│  └──────────────────────────────────────────────────┘   │
│                         │                                │
│  ┌──────────────────────▼───────────────────────────┐   │
│  │ Stage 3: FEEDBACK (post-execution, async)        │   │
│  │  Collect signals → Batch write                    │   │
│  │  Learning cycle runs periodically, not inline     │   │
│  └──────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────┘
```

**Key design decisions**:

1. **`get_tools_schema()` remains the primary interface** — ChatLoop needs tools schema for LLM function calling. The pipeline enriches this call with audit + learning, not replaces it.
2. **Learning is separate from selection** — `SkillPipeline.get_tools_schema()` applies learned corrections synchronously. Learning cycle (`learn()`) runs asynchronously via scheduler or API call.
3. **Feedback is batched** — `record_feedback()` writes to an in-memory buffer, flushed periodically. No synchronous DB write per tool execution.
4. **No internal implementation leaks** — Callers never see `ModernSkillSelector` or `SelfImprovingSelector`. The pipeline is the only public interface.

### Interface

```python
class SkillPipeline:
    """Unified skill selection: retrieve → audit → feedback."""

    def __init__(self, db, llm_client, *, audit=True, learning=True): ...

    def get_tools_schema(self, query, session_id, *, max_candidates=5) -> ToolsResult:
        """Select skills and return tools schema for LLM."""

    def record_feedback(self, event_id, signal, data) -> None:
        """Buffer a feedback signal (async flush)."""

    def learn(self, *, days=7) -> LearningResult:
        """Run learning cycle. Called by scheduler, not by ChatLoop."""

    def stats(self) -> dict: ...

@dataclass
class ToolsResult:
    tools: list[dict]       # OpenAI tools schema, ready for LLM
    event_id: str | None    # Audit event ID (None if audit disabled)
    candidates: int         # Number of candidates considered
```

### ChatLoop Integration

```python
result = self.pipeline.get_tools_schema(
    query=user_input, session_id=session_id, max_candidates=max_candidates,
)
tools_schema = result.tools

# After each tool execution:
self.pipeline.record_feedback(result.event_id, SignalType.EXECUTION_TIME, {"ms": elapsed})
```

### Multi-Stage Selection Detail

```
Stage 1: RETRIEVE (semantic vector search, <50ms, 0 prompt tokens)
  - Encode query into embedding vector
  - Cosine similarity against skill embedding index (SkillIndex)
  - Return top-k candidates (k = 2× max_candidates for headroom)
  - Fallback: keyword matching if vector index unavailable

Stage 2: LOAD (full schema, budget-controlled)
  - Build full OpenAI tool schema for each candidate
  - Measure real token cost per schema: len(json) // 4
  - Include only if within remaining context_budget
  - Skills that exceed budget are excluded entirely (no stubs)
  - LLM selects + extracts parameters in a single function-calling pass
```

**Why no separate LLM ranking pass?** OpenAI-style function calling requires full parameter schemas to generate valid calls. A two-pass approach (rank with summaries → load full schemas) doubles LLM latency for marginal benefit. Semantic retrieval (zero LLM cost) does the heavy filtering; the budget cap ensures only a controlled number of full schemas reach the LLM.

**Scaling behavior**:
| Skill count | Stage 1 method | Prompt tokens (5 candidates) |
|-------------|---------------|------------------------------|
| <20         | keyword OK    | ~500-2000 (budget-capped)    |
| 20-100      | semantic required | ~500-2000 (budget-capped)|
| 100+        | semantic + hierarchical | ~500-2000 (budget-capped) |

Prompt token cost stays **constant** regardless of total skill count — only the retrieval index grows.

### Auditable Selection

Every selection is recorded:

```json
{
  "event_id": "sel_01...",
  "session_id": "sess_01...",
  "user_query": "Review PR #123",
  "selected_skills": ["code_review", "summarize_pr"],
  "selection_method": "semantic",
  "created_at": "2026-02-20T12:00:00Z"
}
```

After selection, the executor enforces approval gates based on each skill's `SideEffectCategory` (Read/Write/Destructive). See [§1 Skill Types](#skill-types) for the approval matrix.

### Self-Improving Selection

The `SelfImprovingSelector` (internal to `SkillPipeline`) learns from historical failures via a closed-loop:

```
PRODUCTION USAGE:
  User query → SkillPipeline.get_tools_schema()
    → Stage 1: Semantic retrieval + LLM ranking
    → Stage 2: Apply learned corrections (boost/penalize skills by pattern)
    → Stage 3: Record audit event
  → Execution & feedback signals recorded

LEARNING CYCLE (triggered manually or scheduled):
  1. OBSERVE: Get recent failures from skill_selection_events
  2. DIAGNOSE: Extract patterns (query_pattern → wrong_skills → correct_skills)
  3. VALIDATE: Regression gate replays golden sessions, compares old vs new scores
  4. DEPLOY: If improvement confirmed, activate learnings (confidence-gated)
```

**Four signal types drive learning**:

| Signal | Trigger | Example |
|--------|---------|---------|
| `WRONG_SKILL` | User corrects skill choice | "I wanted create_pr, not list_prs" |
| `SLOW_EXECUTION` | Execution > 5000ms | Skill took too long |
| `HIGH_COST` | Cost > $0.10 | Expensive LLM calls in skill |
| `LOW_SATISFACTION` | User rating < 3 | Poor result quality |

**Multi-factor scoring** combines dimensions with configurable weights:

```
score = accuracy_weight × accuracy_score + speed_weight × speed_score
      + cost_weight × cost_score + satisfaction_weight × satisfaction_score
```

**Safety mechanisms**:
- **Regression Gate**: Tests on golden queries before deployment, requires improvement ≥ threshold
- **Confidence Threshold**: Only applies learnings with confidence ≥ 50 (increases with evidence, capped at 99)
- **Full Audit Trail**: Every selection, learning, and gate validation logged
- **Reversibility**: Can disable learning per pipeline, reset learnings in database

**Database tables**:
- `skill_selection_events` — every selection decision with query, selected skills, method
- `skill_selection_learnings` — learned correction rules with confidence and evidence count (capped at 200 active; lowest-confidence evicted)
- `skill_learning_signals` — raw feedback signals per execution
- `gate_results` — regression gate verdicts for learning changes

**Reliability**:
- Feedback buffer: in-memory with `max_buffer_size=10,000`, oldest signals evicted on overflow
- Retry limit: 3 retries per signal on flush failure, then dropped with warning
- Process crash = buffered signals lost (acceptable: feedback is optimization data, not correctness-critical)

**Performance**:
| Operation | Latency | Storage |
|-----------|---------|---------|
| `get_tools_schema()` | ~10ms | 1KB/event |
| `learn()` | 1-5s | 500B/learning |

For usage guide (weights configuration, selective learning, troubleshooting), see [Multi-Dimensional Learning Guide](../guides/multi-dimensional-learning-guide.md).

### Procedural Memory Bridge

`core/skills/procedural_memory.py` provides a type-layer adapter that converts `skill_selection_learnings` rows into `Memory` domain objects. This enables the Skill Selector to use memory-system APIs (governance, confidence decay, trust tiers) without duplicating data.

**Design boundary**: skill selection learnings are Skill Selector internal correction rules, NOT general-purpose procedural memory. The bridge is consumed only during skill selection — it is NOT injected into `MemoryRetriever`.

---

## 4. MCP / A2A Compatibility Layer

> **MCP Status**: ✅ Implemented — `core/skills/mcp_bridge.py` (`MCPBridge` class), supports stdio + streamable HTTP transports, namespaced tool names, integrated into ChatLoop (3 execution paths). 12 unit tests passing.

MCP and A2A are interoperability protocols — useful for ecosystem integration, but not architectural drivers. Our Skill abstraction is strictly more powerful (versioned, auditable, sandboxable). MCP/A2A are thin adapters on top.

### Why Support MCP (But Not Center On It)

- **Ecosystem access**: Connect to external MCP servers (filesystem, databases, SaaS tools) without writing custom skills
- **Exposure**: Let external agents (Claude Code, Cursor) call our skills
- **Not a differentiator**: MCP is a wire format. Our value is in what happens around the call — audit, versioning, isolation

### Integration Design

```
┌─────────────────────────────────────────────────────────────┐
│  mo-agent-engine Skill Registry                             │
│                                                             │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐     │
│  │ Built-in     │  │ User-defined │  │ MCP Server   │     │
│  │ Skills       │  │ Skills       │  │ Adapters     │     │
│  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘     │
│         │                  │                  │             │
│         └──────────────────┼──────────────────┘             │
│                            │                                │
│              Unified Skill Interface                        │
│              (name, version, params, execute)               │
└────────────────────────────┬────────────────────────────────┘
                             │
              ┌──────────────┼──────────────┐
              ▼              ▼              ▼
        MCP Client      MCP Client     Direct Call
        (external       (external      (built-in
         server A)       server B)      execution)
```

**Two directions**:

1. **MCP Client**: mo-agent-engine connects to external MCP servers. Each MCP tool is registered as a skill with auto-generated metadata, side-effect profile defaults to "write" (conservative).

2. **MCP Server**: mo-agent-engine exposes its skills as MCP tools. External agents (Claude Code, Cursor, etc.) can use our skills with full audit trail.

### A2A Gateway (Future)

Google's Agent-to-Agent protocol enables cross-platform agent collaboration. Our event blackboard coordination model maps naturally to A2A:

- **Agent Card** → AgentProfile (system_prompt, skills, model)
- **Task** → Delegation event with causal_chain_id
- **Message** → conversation_events with agent_id
- **Artifact** → Skill execution results

---

## 5. Skill Marketplace

> 🔵 **Design Target** — Marketplace discovery, publishing, and RBAC are not yet implemented.
> Current implementation: `SkillManager` supports install/uninstall/credential CRUD for
> platform-defined skills. The marketplace vision below describes the target architecture.

### The Vision: App Store for Agent Skills

Skills are publishable, discoverable, and installable — like an app store. Admin publishes skills to the marketplace, controls visibility per user/role, and users install skills to enable capabilities.

### Architecture: Publish → Authorize → Install → Use

```
ADMIN (platform operator)
  │
  ├── Publishes skill to marketplace (skills_registry table)
  ├── Grants access: per-user or per-role (skill_permissions table)
  └── Manages skill lifecycle: activate / deprecate / archive

USER
  │
  ├── Browses available skills (filtered by permissions)
  ├── Installs skill:
  │     → Permission check
  │     → User provides credentials if required (encrypted in platform DB)
  │     → Records in skill_installations
  ├── Uses skill in sessions (agent calls skill API, skill reads/writes platform DB)
  └── Uninstalls skill (marks uninstalled, deletes credentials)
```

### Platform Tables

#### skills_registry — unified skill catalog

> ORM model: `api/models/skill.py::SkillRegistry`. Service layer: `core/skills/catalog.py::SkillCatalog` (aliased as `SkillRegistry` in `core/skills/registry.py` for backward compatibility).

```python
class SkillRegistry(Base):
    __tablename__ = "skills_registry"

    skill_id = Column(String(255), primary_key=True)  # skill_name@version
    skill_name = Column(String(255), nullable=False, index=True)
    version = Column(String(32), nullable=False)
    description = Column(Text)
    skill_definition = Column(JSON)
    source = Column(String(20), default="builtin")  # builtin/marketplace/user
    manifest = Column(JSON)
    is_active = Column(SmallInteger, default=1)
    is_public = Column(SmallInteger, default=0)
    status = Column(String(20), default="active")
    created_by = Column(String(36))
    created_at = Column(DateTime, default=func.now())
    updated_at = Column(DateTime, default=func.now(), onupdate=func.now())
```

#### skill_installations — per-user install state

```python
class SkillInstallation(Base):
    __tablename__ = "skill_installations"
    __table_args__ = (
        UniqueConstraint("user_id", "skill_name", name="uq_user_skill"),
        Index("ix_install_user_status", "user_id", "status"),
    )

    installation_id = Column(String(36), primary_key=True)
    user_id = Column(String(36), nullable=False)
    skill_name = Column(String(100), nullable=False)
    skill_version = Column(String(20), nullable=False)
    status = Column(String(20), default="installed")  # installed | uninstalled
    installed_at = Column(DateTime, nullable=False)
    updated_at = Column(DateTime)
```

#### user_credentials — per-user encrypted secrets

```python
class UserCredential(Base):
    __tablename__ = "user_credentials"
    __table_args__ = (
        UniqueConstraint("user_id", "skill_name", "credential_name",
                         name="uq_user_skill_cred"),
    )

    credential_id = Column(String(36), primary_key=True)
    user_id = Column(String(36), nullable=False)
    skill_name = Column(String(100), nullable=False)
    credential_name = Column(String(100), nullable=False)
    value_encrypted = Column(Text, nullable=False)
    created_at = Column(DateTime, nullable=False)
    rotated_at = Column(DateTime)
```

#### skill_permissions — RBAC

```python
class SkillPermission(Base):
    __tablename__ = "skill_permissions"
    __table_args__ = (
        UniqueConstraint("skill_name", "grantee_type", "grantee_id",
                         name="uq_skill_grantee"),
    )

    permission_id = Column(String(36), primary_key=True)
    skill_name = Column(String(100), nullable=False)
    grantee_type = Column(String(10), nullable=False)  # "user" | "role"
    grantee_id = Column(String(36), nullable=False)
    permission_type = Column(String(10), nullable=False, default="install")  # "install" | "execute" | "admin"
    tenant_id = Column(String(36), nullable=True)  # NULL = platform-wide
    granted_by = Column(String(36), nullable=False)
    granted_at = Column(DateTime, nullable=False)
    expires_at = Column(DateTime, nullable=True)  # NULL = no expiry
```

> **Multi-tenancy**: When `tenant_id` is set, the permission is scoped to that tenant. `is_public=1` skills are visible to all tenants but still require per-tenant install permission. Platform-wide permissions (`tenant_id=NULL`) are admin-only.

### Install / Uninstall Lifecycle

**Install Flow**:
```
User: "install github skill"
  ├─ 1. Check permission → query skill_permissions
  ├─ 2. Check dependencies → query skill_installations
  ├─ 3. Prompt for credentials (if required) → encrypt and store in user_credentials
  └─ 4. Record installation → INSERT INTO skill_installations
```
No DDL execution. Tables already exist in platform DB (created by `init_db()`).

**Uninstall Flow**:
```
User: "uninstall github skill"
  ├─ 1. Check: any other skills depend on this?
  ├─ 2. Mark as uninstalled in skill_installations
  └─ 3. Delete credentials from user_credentials
```
No DROP TABLE. Skill data remains in platform DB.

**Upgrade Flow**:
```
Platform upgrades github skill v1.0.0 → v1.1.0
  ├─ Schema change? → Platform-level migration (same as any api/models.py change)
  └─ No schema change? → Update skill_installations.skill_version
```

### init_db() — Skill Table Discovery

```python
def init_db():
    """Create all tables — core + skill."""
    _import_skill_models()
    Base.metadata.create_all(bind=engine, checkfirst=True)

def _import_skill_models():
    """Auto-discover and import skills/*/models.py."""
    import importlib
    from pathlib import Path
    skills_dir = Path(__file__).parent.parent / "skills"
    for skill_dir in skills_dir.iterdir():
        if skill_dir.is_dir() and (skill_dir / "models.py").exists():
            importlib.import_module(f"skills.{skill_dir.name}.models")
```

### MatrixOne-Enhanced Distribution (Design Target, opt-in)

When both publisher and subscriber are on MatrixOne, skill distribution can leverage native Publication for zero-copy, auto-updating skill catalogs:

```
PUBLISHER (skill author account)
  ├── Develops skill, tests in sandbox (CREATE CLONE → replay gate)
  ├── Publishes: CREATE PUBLICATION skill_catalog_pub DATABASE skill_catalog TABLE ...;
  └── Updates skill → subscribers see changes immediately (zero-copy)

SUBSCRIBER (consumer account)
  ├── Subscribes: CREATE DATABASE marketplace FROM publisher_acct PUBLICATION skill_catalog_pub;
  ├── Agent loads skill definition from subscription DB
  ├── Executes skill in own account context (isolation)
  └── Invocation logged in own conversation_events (audit)
```

**Version pinning**: `CREATE CLONE pinned_skills FROM publisher_acct.skill_catalog;` for snapshot-in-time, vs subscribe for always-latest.

With MatrixOne: **Publication = distribution + auto-update. Clone = version pinning. Multi-Account = access control.** The entire marketplace infrastructure collapses into 3 SQL statements.

### What Moves Out of api/models.py

| Current Location | Becomes | Skill |
|-----------------|---------|-------|
| `Repo` in `api/models.py` | `sk_github_repos` in `skills/github/models.py` | github |
| `core/repos/` | `skills/github/api.py` | github |
| `core/skills/github_client.py` | `skills/github/api.py` | github |
| `KnowledgeEntry` in `api/models.py` | `sk_knowledge_entries` in `skills/knowledge/models.py` | knowledge |
| `KnowledgeRelation` in `api/models.py` | `sk_knowledge_relations` in `skills/knowledge/models.py` | knowledge |
| `core/context/knowledge.py` | `skills/knowledge/api.py` | knowledge |

### Skill Permission Model

```
Admin publishes skill "github" to marketplace
  ├─ Grant to role: all users with role "developer" can install
  ├─ Grant to user: only specific user can install
  └─ Public: all authenticated users can install
```

---

## 6. Tool Design Principles

Following Anthropic's guidance on writing tools for agents:

### 1. Token-Efficient Returns

```python
# Bad: return entire PR object (2000 tokens)
def get_pr(pr_number):
    return github.get_pr(pr_number)  # Everything

# Good: return only what's needed (200 tokens)
def get_pr(pr_number, fields=["title", "body", "files_changed", "status"]):
    pr = github.get_pr(pr_number)
    return {f: getattr(pr, f) for f in fields}
```

### 2. Self-Contained and Robust

Each tool handles its own errors, retries, and edge cases. The agent should never see raw HTTP errors or stack traces.

### 3. Minimal Overlap

If a human engineer can't definitively say which tool to use in a given situation, the agent can't either. Merge overlapping tools or add clear disambiguation in descriptions.

### 4. Descriptive Parameters

Parameter names and descriptions are part of the tool's "prompt." They must be unambiguous:

```python
# Bad
{"file": "string"}

# Good
{"file_path": "Absolute path to the file to read, e.g. /src/auth/login.py"}
```

---

## 7. Comparison with Industry

Comparison focuses on **stateful skill management** — persistent data, lifecycle, platform-level governance. All frameworks support tool/function schemas for LLM calling; that is table stakes and not compared here.

| Feature | ElizaOS | LangChain | MCP | **mo-agent-engine** |
|---------|---------|-----------|-----|---------------------|
| Platform-managed schema | ✅ plugin schema (plugin owns) | ❌ stateless | ❌ stateless | ✅ platform-defined |
| Table namespace | ❌ bare names | ❌ | ❌ | ✅ `sk_{skill}_{table}` |
| Install lifecycle | ❌ | ❌ | ❌ | ✅ |
| Skill API layer | ❌ direct SQL | ❌ | ❌ | ✅ typed API |
| Marketplace + RBAC | ❌ | ❌ | ❌ | ✅ |
| Per-user credentials | ❌ env vars | ❌ env vars | ❌ | ✅ encrypted |
| Skill-local models | ❌ | ❌ | ❌ | ✅ `skills/{name}/models.py` |
| Self-improving selection | ❌ | ❌ | ❌ | ✅ closed-loop learning |
| Unified selection pipeline | ❌ | ❌ | ❌ | ✅ retrieve → audit → feedback |

---

## 8. Open Questions

1. ~~**Cross-skill data access**: can knowledge skill read from github skill's tables?~~
   **Resolved** — see §9 Cross-Skill Data Access below.

2. **Schema evolution**: how to handle ALTER TABLE when platform upgrades a skill?
   - Same as any other migration — platform-level, applied by operator

3. **Skill cost → budget gate integration**: `SignalWeights.cost` (0.2) drives learning, but skill execution cost doesn't flow back to the budget control system in [Deployment Architecture](deployment-architecture.md). Need: execution cost recorded per-skill → aggregated per-session → checked against session budget before next skill call.

4. **Prompt evolution → skill regression gate**: `InputFaceLearner` can modify prompts, but prompt changes may alter which skills the LLM selects. Should prompt changes trigger the skill selection regression gate? Current answer: no (they are independent input faces). May need cross-face regression testing.

---

## 9. Cross-Skill Data Access

> **Status**: Design Decision — resolves Open Question #1

### Problem

Skills have isolated table namespaces (`sk_{skill}_`), but real workflows need cross-skill data. Example: knowledge skill needs to index GitHub PR summaries from `sk_github_pr_cache`.

### Decision: Manifest Explicit Dependency + Platform Cross-API

**Two mechanisms, layered**:

#### 1. Manifest `depends_on` — Declares Intent

```yaml
# skills/knowledge/manifest.yaml
name: knowledge
depends_on:
  - github          # declares: I need data from the github skill
  - code_execution  # declares: I need data from code_execution skill
```

`depends_on` is enforced at install time (`SkillManager.install()` already checks this) and at runtime (`SkillManager.require_executable()` verifies all dependencies are installed). This is already implemented.

#### 2. Platform Cross-Skill API — Controls Access

Skills do NOT import each other's modules or query each other's tables directly. Instead, the platform provides a typed cross-skill API:

```python
class SkillDataBridge:
    """Platform-provided cross-skill data access.

    Injected into skill API constructors. Skills call bridge methods
    instead of importing each other's models or writing raw SQL.
    """

    def __init__(self, db: Session, requesting_skill: str, user_id: str):
        self._db = db
        self._skill = requesting_skill
        self._user_id = user_id

    def query(self, target_skill: str, table: str, filters: dict, limit: int = 100) -> list[dict]:
        """Read rows from another skill's table.

        Validates: requesting_skill has target_skill in depends_on.
        Returns dicts (not ORM objects) to prevent schema coupling.
        """

    def count(self, target_skill: str, table: str, filters: dict) -> int:
        """Count rows in another skill's table."""
```

**Why not direct JOINs?** All tables are in the same DB, so JOINs are trivially possible. But direct SQL creates implicit coupling — skill A breaks when skill B changes its schema. The bridge provides:
- **Dependency validation**: only declared dependencies are accessible
- **Schema decoupling**: returns dicts, not ORM objects
- **Audit**: every cross-skill access is logged
- **Future-proof**: if skills move to separate databases, only the bridge changes

**Implementation priority**: Phase 2. Current skills (github, knowledge) don't yet need cross-skill access. When the first real use case appears, implement `SkillDataBridge`.

---

## 10. Low-Code Skill Template

> **Status**: Design Target

### Problem

Creating a new skill requires writing 4 files (manifest.yaml, models.py, api.py, actions.py) with boilerplate. This friction discourages skill creation and makes the system feel heavyweight for simple use cases.

### Solution: YAML Declaration → Auto-Generated Skeleton

A single `skill.yaml` declares everything. The platform generates the models/api/actions skeleton:

```yaml
# skill.yaml — complete low-code skill definition
name: jira
version: "1.0.0"
description: "Jira integration — issues, sprints, boards"
table_prefix: sk_jira

credentials:
  - name: jira_token
    type: secret
    required: true
  - name: jira_url
    type: string
    required: true

tables:
  issues:
    columns:
      issue_key: {type: string, max_length: 20, primary_key: true}
      summary: {type: string, max_length: 500}
      status: {type: string, max_length: 50}
      assignee: {type: string, max_length: 100, nullable: true}
      priority: {type: string, max_length: 20}
      data: {type: json}
      fetched_at: {type: datetime}
    indexes:
      - columns: [status, assignee]

actions:
  list_issues:
    description: "List Jira issues with filters"
    parameters:
      project: {type: string, required: true}
      status: {type: string, enum: [open, in_progress, done]}
    side_effect: read

  create_issue:
    description: "Create a new Jira issue"
    parameters:
      project: {type: string, required: true}
      summary: {type: string, required: true}
      issue_type: {type: string, default: "Task"}
    side_effect: write

depends_on: []
```

### CLI Command

```bash
mo-admin skill scaffold skill.yaml
# Generates:
#   skills/jira/
#     manifest.yaml      ← from skill.yaml metadata
#     models.py           ← SQLAlchemy models from tables section
#     api.py              ← typed API skeleton with CRUD methods
#     actions.py          ← action classes from actions section
#     __init__.py
```

### Type Mapping

| YAML type | SQLAlchemy | Python |
|-----------|-----------|--------|
| `string` | `String(max_length)` | `str` |
| `integer` | `Integer` | `int` |
| `float` | `Float` | `float` |
| `boolean` | `SmallInteger` | `bool` |
| `datetime` | `DateTime` | `datetime` |
| `json` | `JSON` | `dict` |
| `text` | `Text` | `str` |

### Design Principles

- **Scaffold, not runtime codegen**: generates real Python files that developers own and can customize
- **No magic**: generated code is readable, follows the same patterns as hand-written skills
- **Escape hatch**: after scaffolding, developers modify generated files freely
- **Validation**: `skill.yaml` is validated against a JSON Schema before generation

---

## 11. Skill Sandbox Mode

> **Status**: Design Target
> **Dependency**: [Code Execution Sandbox](code-sandbox.md)

### Problem

Cloud skills execute in the platform process. A buggy or malicious skill can access the full platform DB, consume unbounded resources, or crash the process.

### Solution: Optional Container-Isolated Execution

Skills can opt into sandbox mode via manifest:

```yaml
# skills/untrusted_analyzer/manifest.yaml
name: untrusted_analyzer
version: "1.0.0"
sandbox:
  enabled: true
  image: mo-skill-runtime:latest   # base image with Python + skill deps
  memory_limit: 512m
  cpu_limit: 1.0
  timeout_seconds: 30
  network: none                     # no network access
  volumes: []                       # no host mounts
```

### Execution Model

```
Normal skill:     ChatLoop → SkillExecutor → skill.execute() (in-process)
Sandboxed skill:  ChatLoop → SkillExecutor → SandboxRunner → Docker container
                                                  │
                                                  ├── Mount: skill code (read-only)
                                                  ├── Env: credentials (injected)
                                                  ├── Stdin: JSON request
                                                  └── Stdout: JSON response
```

### Security Tiers

| Tier | Isolation | Use Case | Default |
|------|-----------|----------|---------|
| **Trusted** | In-process | Platform-built skills (github, knowledge) | ✅ |
| **Sandboxed** | Docker container | Third-party / marketplace skills | |
| **Strict** | gVisor + no-network | User-uploaded skills | |

### When to Sandbox

- **Always sandbox**: skills from marketplace with `source != "builtin"`
- **Optional sandbox**: platform skills during development/testing
- **Never sandbox**: edge tools (they already run on user's machine)

### Data Access in Sandbox

Sandboxed skills cannot access the platform DB directly. Instead:

```
Container                          Platform
  │                                   │
  ├── stdin: {action, params}  ──────►│
  │                                   ├── Validate action against manifest
  │                                   ├── Execute DB query on behalf of skill
  │◄── stdout: {result}       ◄──────┤
  │                                   │
```

The platform acts as a proxy — the skill declares what tables it needs in the manifest, and the platform executes queries on its behalf. This prevents:
- Unauthorized table access (only declared tables)
- SQL injection (platform builds queries, not the skill)
- Resource abuse (timeout + memory limit enforced by Docker)

### Implementation Priority

Phase 3. Current skills are all platform-built (trusted). Sandbox mode becomes critical when:
1. Marketplace opens to third-party skill authors
2. Users can upload custom skills
3. Skills need to execute user-provided code

---

## 12. Dependency Management Enhancement

Current dependency management (see [Section 1](#1-skill-architecture) manifest format and [Section 3](#3-skill-selection-pipeline) resolution) only supports name-based matching (`depends_on: ["git"]`) with no version constraints, no Skill→Tool tracking, and no conflict detection at install time.

A comprehensive enhancement plan covering semantic versioning, tool dependencies, conflict resolution, and upgrade impact analysis is documented in:

**[Skill and Tool Dependency & Versioning Enhancement Plan](../../plans/skill-tool-dependency-versioning.md)**

The enhanced format will be backward compatible with the current list-based `depends_on` syntax.

---

## References

- [Anthropic: Equipping Agents with Agent Skills](https://www.anthropic.com/engineering/equipping-agents-for-the-real-world-with-agent-skills)
- [Anthropic: Writing Tools for AI Agents](https://www.anthropic.com/engineering/writing-tools-for-agents)
- [RAG-MCP: Mitigating Prompt Bloat in LLM Tool Selection (arXiv:2505.03275)](https://arxiv.org/abs/2505.03275) — 3.2× accuracy, ~50% token reduction via retrieval-based tool selection
- [Progressive Context Enrichment for LLMs (Inferable, 2025)](https://www.inferable.ai/blog/posts/llm-progressive-context-encrichment) — production validation of fetch-on-demand pattern
- [Claude Skills: Breaking LLM Memory Barriers (Developers Digest)](https://www.developersdigest.tech/blog/claude-skills-breaking-llm-memory-barriers) — ~30-50 tokens per skill until activation
- [MCP Specification](https://modelcontextprotocol.io/docs/getting-started/intro)
- [A2A Protocol Guide](https://a2aprotocol.ai/blog/2025-full-guide-a2a-protocol)
- [AI Agents 2026: Practical Architecture](https://www.andriifurmanets.com/blogs/ai-agents-2026-practical-architecture-tools-memory-evals-guardrails)
- [Vercel: Agent Skills — Creating, Installing, and Sharing](https://vercel.com/kb/guide/agent-skills-creating-installing-and-sharing-reusable-agent-context)
- [ElizaOS: Plugin Database Schema](https://docs.elizaos.ai/plugins/schemas) — plugin-declared schemas with auto-migration (closest industry precedent to Skill-as-Package)

Content was rephrased for compliance with licensing restrictions.

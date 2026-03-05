# Skills and Tools

> **Status**: Core Design — single source of truth for skill system, packaging, selection, and tool integration
> **Last Updated**: 2026-03-05
> **Related**: [agent-loop-reliability.md](agent-loop-reliability.md) (pre-LLM task routing and tool scoping)
>
> 🔵 **Implementation Status**: `SkillCatalog` (register/lifecycle/versioning) and `ToolRegistry` (unified tool selection with pinned/dynamic split) are implemented.
> Marketplace discovery, publishing, RBAC, and MatrixOne Publication distribution are Design Targets.
> Skill Configuration Center (§13) — P0 (ORM models, config_center core, manifest parsing, require_executable validation, migration), P1 (REST API + CLI commands) are implemented. P2 (tenant-scope admin, config change events) remains.
> Sandbox Mode (§11) promoted to P1 — mandatory for third-party skills before marketplace opens.
> Skill Table Registry (§14), Historical Code Replay (§2) are Design Targets.
>
> ⚠️ **2026-03-05 Update**: §3 Skill Selection Pipeline rewritten. The old `SkillPipeline`, `ModernSkillSelector`, `SelfImprovingSelector`, `selector.py`, `learning_signals.py`, `learning_config.py`, `learning_similarity.py` have been **deleted** and replaced by `ToolRegistry` (`core/skills/tool_registry.py`). ToolRegistry provides a unified pinned/dynamic tool split with embedding retrieval, intent filtering, and prefilter reordering. Context-Aware Pre-Filtering (§3.5) and Conversation State Signals (§3.6) remain. The self-improving learning loop (§3.9) is removed — the learning API endpoints are stubs.

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
- **Configuration**: settings, secrets, and per-resource bindings (see §13 Skill Configuration Center)
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
github.set_config(default_token=token)      # → encrypted in skill_settings (§13)
github.bind_repo("matrixorigin/matrixone",  # → skill_resource_bindings (§13)
    read_token=read_tok, write_token=write_tok)
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
│    skill_installations, skill_settings,                       │
│    skill_resource_bindings                                    │
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
| Skill infrastructure | (none) | `api/models/skill.py` | `skills_registry`, `skill_installations`, `skill_permissions`, `skill_settings`, `skill_resource_bindings` |
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

# ⚠️ Legacy format (deprecated — see §13 Skill Configuration Center for new format)
# New manifests should use settings: / secrets: / resources: sections instead.
credentials:
  - name: github_token
    type: secret
    description: "GitHub Personal Access Token or App token"
    required: true

requires:
  - http

depends_on: []
# New format with version constraints (also supported):
# depends_on:
#   - name: knowledge
#     version: ">=1.0,<2.0"
#     type: skill
# See docs/guides/skill-dependencies.md for full reference.
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

    def __init__(self, db: Session, config: "SkillConfig"):
        self._db = db  # platform DB session
        # Resource-specific token if available, else skill-level default (§13)
        token = (config.resource or {}).get("read_token") or config.secrets.get("default_token")
        self._base_url = config.settings.get("api_base_url", "https://api.github.com")
        self._client = Github(auth=Auth.Token(token), base_url=self._base_url) if token else None

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
         → Gate passes → validate dependency constraints (see §5 Lifecycle)
         → Constraint check passes → activate new version
         → Constraint check fails → reject, report which dependents would break
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

# Configuration — see §13 Skill Configuration Center for full spec
settings:
  - name: review_depth
    type: enum
    values: [thorough, quick, security-only]
    default: "thorough"

secrets:
  - name: github_token
    description: "GitHub Personal Access Token (fallback)"

resources:
  type: repo
  key_pattern: "{owner}/{name}"
  bindings:
    - name: token
      type: secret
      description: "Repo-specific GitHub token"
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

    # 2. Resolve configuration (§13 Skill Configuration Center)
    config = config_center.resolve_all(
        skill_name=skill_name, user_id=user_id,
        tenant_id=tenant_id, resource_key=params.get("resource_key")
    )
    errors = config_center.validate(skill_name, user_id, tenant_id, params.get("resource_key"))
    if errors:
        raise SkillConfigError(errors)

    # 3. Create skill API instance (receives resolved config)
    api = GitHubSkillAPI(config=config)

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

### Historical Code Replay

Replay requires executing the exact skill code that ran during the original session. The platform maintains a **skill artifact registry** to guarantee code-level reproducibility.

**Publish flow**: `mo-admin skill publish` packages the skill directory into a tarball, computes `code_hash` (SHA-256), and stores it in the artifact registry (local filesystem or object storage). The `skills_registry.code_hash` column records the hash for each published version.

**Replay flow**:
```
Replay engine reads session event → extracts skill_id + version
  ├─ Fetch tarball from artifact registry
  ├─ Verify SHA-256(tarball) == event.metadata.code_hash
  │     └─ Mismatch → reject replay with CodeIntegrityError (no silent fallback)
  ├─ Extract to temp directory
  └─ SandboxedExecutor loads skill from extracted path (read-only mount)
```

This ensures replay never silently runs newer code against historical sessions.

---

## 3. Skill Selection Pipeline

> **Implementation**: `core/skills/tool_registry.py` — `ToolRegistry` is the single public interface for tool selection.
> `core/skills/catalog.py` — `SkillCatalog` handles skill registration, lifecycle, and DB persistence.
> `core/skills/prefilter.py` — Context-aware pre-filtering (conversation state signals + skill tags).
>
> ⚠️ **2026-03-05 Update**: The old `SkillPipeline`, `selector.py`, `modern_selector.py`, `self_improving_selector.py`, `learning_signals.py`, `learning_config.py`, `learning_similarity.py` have been deleted. `ToolRegistry` replaces them with a simpler pinned/dynamic split + embedding retrieval model.

### 3.1 The Problem

With 50+ skills, the LLM can't efficiently choose from a flat list. Selection must be fast, accurate, and auditable. Research shows keyword matching collapses beyond ~30 tools (RAG-MCP, 2025). Semantic retrieval is mandatory, not optional.

**Two distinct failure modes**:

1. **Retrieval failure** — the correct skill isn't in the candidate set. Solved by semantic retrieval (implemented).
2. **Disambiguation failure** — multiple skills match semantically, but only one is correct for the user's actual intent. Example: "分析前一个上下文" matches both `introspection` (session metadata snapshot) and event analysis (historical event data). Semantic similarity alone cannot distinguish them because their descriptions overlap. **This is the harder problem and requires context-aware pre-filtering (§3.5).**

### 3.2 Unified Pipeline: ToolRegistry (Pinned + Dynamic)

```
┌──────────────────────────────────────────────────────────────┐
│                       ToolRegistry                           │
│                                                              │
│  ┌────────────────────────────────────────────────────────┐  │
│  │ PINNED TOOLS (always included)                         │  │
│  │  bash, read_file, write_file, grep, glob, list_dir     │  │
│  │  → Always in LLM context, no selection needed          │  │
│  └────────────────────────────────────────────────────────┘  │
│                           +                                  │
│  ┌────────────────────────────────────────────────────────┐  │
│  │ DYNAMIC TOOLS (selected per-request)                   │  │
│  │  Step 1: Intent filter (conversational → skip)         │  │
│  │  Step 2: Prefilter reorder by conversation state       │  │
│  │  Step 3: Embedding retrieval top-K (or truncate)       │  │
│  │  Step 4: Token budget enforcement                      │  │
│  └────────────────────────────────────────────────────────┘  │
│                           =                                  │
│  Final tool schemas for LLM                                  │
└──────────────────────────────────────────────────────────────┘
```

**Key design decisions**:

1. **Pinned/dynamic split** — Core coding tools (bash, grep, file ops) are always available. Cloud skills (GitHub, CI, etc.) are selected dynamically per request.
2. **`select()` is the single interface** — ChatLoop calls `registry.select(user_query, messages)` and gets back a list of OpenAI tool schemas.
3. **Pre-filtering is deterministic and zero-cost** — No LLM calls, no embedding computation. Uses structured tags on skills and rule-based conversation state signals.
4. **Token budget is the hard constraint** — Pinned tools are never dropped. Dynamic tools are included until the budget is exhausted.
5. **No learning loop** — The self-improving selector has been removed. Selection relies on embedding similarity + prefilter rules.

### 3.3 Interface

```python
class ToolRegistry:
    """Unified registry for all tools. Handles selection per request."""

    def __init__(
        self,
        pinned_names: frozenset[str] | None = None,
        max_dynamic: int = 8,
        max_tokens: int = 2500,
        embed_fn: Callable | None = None,
    ): ...

    def register_skill(self, skill, source: ToolSource, pinned: bool | None = None) -> None:
        """Register a Skill instance (has to_openai_schema())."""

    def register_schema(self, schema: dict, source: ToolSource) -> None:
        """Register a raw OpenAI tool schema dict."""

    def select(self, user_query: str = "", messages: list[dict] | None = None) -> list[dict]:
        """Select tools for one LLM request. Returns list of OpenAI tool schemas."""

    def get_all_schemas(self) -> list[dict]:
        """Return all tool schemas (no filtering). For backward compat."""
```

### 3.4 ChatLoop Integration

```python
# In ChatLoop.run_step_stream():
tools_schema = self._tool_registry.select(user_input, messages)

# Append scratchpad tools if enabled
if self.scratchpad:
    tools_schema = list(tools_schema) + _SCRATCHPAD_TOOLS

# Append MCP tools
if self.mcp_bridge and self.mcp_bridge.tool_count > 0:
    mcp_tools = await self.mcp_bridge.get_tools_schema()
    tools_schema = list(tools_schema) + mcp_tools
```

### 3.5 Context-Aware Pre-Filtering (NEW)

> **Status**: Design — addresses disambiguation failure mode identified in session 019cbb9e.
> **Principle**: Zero additional context tokens. All filtering happens before LLM sees anything.

#### The Problem Pre-Filtering Solves

Semantic retrieval finds skills whose descriptions are similar to the query. But when multiple skills have overlapping descriptions, retrieval returns all of them and the LLM picks based on surface similarity — which is often wrong.

**Real failure case** (session `019cbb9e-c0f7-7340-887b-dad94f773af3`):
- User: "分析一下前一个上下文的情况还有决策链评估"
- `introspection` skill description mentions "session analysis", "context"
- Event analysis capability also involves "context", "analysis"
- Semantic retrieval ranked `introspection` highest (description overlap)
- LLM selected `introspection` → returned session metadata snapshot
- **Correct answer**: needed historical event data from `conversation_events`, not a current session snapshot

The LLM cannot distinguish these because it only sees skill descriptions. The distinction requires understanding **what data source** each skill accesses and **whether the user is referencing history**.

#### Solution: Structured Skill Tags + Conversation State Signals

Two components, both deterministic, both zero-token:

**Component 1: Skill Tags** — structured metadata on each skill, stored in `skills_registry`, NOT included in LLM context.

```python
@dataclass
class SkillTags:
    """Structured tags for pre-filtering. Never sent to LLM."""
    scope: str              # "current_session" | "historical" | "cross_session" | "external"
    data_source: str        # "session_metadata" | "event_store" | "memory_store" | "external_api"
    intent_type: list[str]  # ["analytical", "fetch", "mutate", "introspect"]
    requires_history: bool  # True if skill needs access to past turns/events
```

Example tags:

| Skill | scope | data_source | intent_type | requires_history |
|-------|-------|-------------|-------------|-----------------|
| `introspection` | current_session | session_metadata | [introspect] | False |
| `event_reader` | historical | event_store | [analytical] | True |
| `list_prs` | external | external_api | [fetch] | False |
| `create_issue` | external | external_api | [mutate] | False |
| `memory_recall` | cross_session | memory_store | [analytical] | True |
| `reflect` | historical | event_store | [analytical, introspect] | True |

**Component 2: Conversation State Signals** — extracted from the current query + message history using deterministic rules (no LLM).

```python
@dataclass
class ConversationState:
    """Signals extracted from conversation context. Zero LLM cost."""
    references_history: bool    # "前一个", "上一轮", "刚才", "之前"
    is_analytical: bool         # "分析", "评估", "为什么", "怎么回事"
    is_fetch: bool              # "查看", "列出", "最新的", "情况"
    is_mutate: bool             # "创建", "修改", "删除"
    turn_count: int             # How many turns in this session
    has_tool_results: bool      # Previous turn had tool execution results
    previous_skill: str | None  # What skill was used in the previous turn
```

#### Pre-Filter Rules

Rules are deterministic `if/then` logic. They narrow the candidate pool, never expand it.

```python
def pre_filter(skills: list[SkillMetadata], state: ConversationState) -> list[SkillMetadata]:
    """Narrow skill candidates based on conversation state. Zero LLM cost.

    Conservative: returns full list if no rules match.
    """
    if not state:
        return skills  # No state → no filtering

    filtered = skills

    # Rule 1: History reference → prefer historical scope, deprioritize current-only
    if state.references_history and state.is_analytical:
        filtered = _prefer(filtered,
            include_tags={"scope": ["historical", "cross_session"]},
            deprioritize_tags={"scope": ["current_session"]},
        )

    # Rule 2: External data fetch → prefer external scope
    if state.is_fetch and not state.references_history:
        filtered = _prefer(filtered,
            include_tags={"data_source": ["external_api"]},
        )

    # Rule 3: Mutation intent → only mutate skills
    if state.is_mutate:
        filtered = _prefer(filtered,
            include_tags={"intent_type": ["mutate"]},
        )

    # Safety: never return empty — fall back to full list
    return filtered if filtered else skills
```

**Critical design constraint**: `_prefer()` does NOT remove skills. It reorders them so preferred skills come first. The vector retrieval `top_k` then naturally selects the preferred ones. This means pre-filtering can never cause a retrieval failure — it only influences ranking.

```python
def _prefer(
    skills: list[SkillMetadata],
    include_tags: dict[str, list[str]] | None = None,
    deprioritize_tags: dict[str, list[str]] | None = None,
) -> list[SkillMetadata]:
    """Reorder skills: matching include_tags first, matching deprioritize_tags last."""
    preferred = []
    normal = []
    deprioritized = []

    for skill in skills:
        tags = skill.tags  # SkillTags from registry
        if include_tags and _matches_any(tags, include_tags):
            preferred.append(skill)
        elif deprioritize_tags and _matches_any(tags, deprioritize_tags):
            deprioritized.append(skill)
        else:
            normal.append(skill)

    return preferred + normal + deprioritized
```

#### Token Cost Analysis

| Component | Prompt tokens | Storage | Compute |
|-----------|--------------|---------|---------|
| Skill tags | 0 (never in LLM context) | ~100 bytes/skill in DB | 0 |
| Conversation state extraction | 0 (rule-based) | 0 | <1ms (string matching) |
| Pre-filter rules | 0 | 0 | <1ms (list reorder) |
| **Total** | **0** | **negligible** | **<1ms** |

Compare with alternatives:
- Longer skill descriptions: +50-200 tokens per skill × N skills per call
- LLM disambiguation step: +500-1000 tokens per turn + latency
- Pre-filtering: **0 tokens, <1ms**

#### Database Schema Change

Add `tags` column to `skills_registry`:

```sql
ALTER TABLE skills_registry ADD COLUMN tags JSON;
-- Example value:
-- {"scope": "historical", "data_source": "event_store",
--  "intent_type": ["analytical"], "requires_history": true}
```

Tags are populated at skill registration time from the manifest:

```yaml
# skills/github/manifest.yaml (new section)
tags:
  scope: external
  data_source: external_api
  intent_type: [fetch, mutate]
  requires_history: false
```

For existing skills without manifest tags, `SkillCatalog` infers defaults:
- Skills with `category: "github"` → `scope: external, data_source: external_api`
- Skills with `category: "analysis"` → `scope: historical, data_source: event_store`
- Unknown → no tags → pre-filter passes them through unchanged

### 3.6 Conversation State Signals (NEW)

> **Status**: Design — extracted from conversation history without LLM.

#### Signal Extraction

```python
class ConversationState:
    """Extract conversation signals from messages. Pure string matching."""

    _HISTORY_MARKERS = {"前一个", "上一轮", "刚才", "之前", "previous", "last", "earlier", "before"}
    _ANALYTICAL_MARKERS = {"分析", "评估", "为什么", "怎么回事", "analyze", "evaluate", "why", "assess"}
    _FETCH_MARKERS = {"查看", "列出", "最新", "情况", "show", "list", "latest", "status", "get"}
    _MUTATE_MARKERS = {"创建", "修改", "删除", "新建", "create", "update", "delete", "modify"}

    @classmethod
    def from_messages(cls, messages: list[dict]) -> "ConversationState":
        """Extract signals from message history. O(n) string scan, no LLM."""
        last_user_msg = ""
        for msg in reversed(messages):
            if msg.get("role") == "user":
                last_user_msg = msg.get("content", "").lower()
                break

        return cls(
            references_history=any(m in last_user_msg for m in cls._HISTORY_MARKERS),
            is_analytical=any(m in last_user_msg for m in cls._ANALYTICAL_MARKERS),
            is_fetch=any(m in last_user_msg for m in cls._FETCH_MARKERS),
            is_mutate=any(m in last_user_msg for m in cls._MUTATE_MARKERS),
            turn_count=sum(1 for m in messages if m.get("role") == "user"),
            has_tool_results=any(m.get("role") == "tool" for m in messages[-3:]),
            previous_skill=cls._extract_previous_skill(messages),
        )

    @staticmethod
    def _extract_previous_skill(messages: list[dict]) -> str | None:
        """Find the skill used in the most recent assistant turn."""
        for msg in reversed(messages):
            if msg.get("role") == "assistant" and msg.get("tool_calls"):
                calls = msg["tool_calls"]
                if calls:
                    return calls[0].get("function", {}).get("name")
        return None
```

#### Why Not Use LLM for Intent Classification?

| Approach | Tokens | Latency | Accuracy | Failure mode |
|----------|--------|---------|----------|-------------|
| LLM intent classification | 200-500/turn | 200-500ms | High | Adds cost to every turn |
| Embedding similarity | 0 prompt, compute | 10-50ms | Medium | Can't distinguish semantic overlap |
| **Rule-based signals** | **0** | **<1ms** | **Medium-high for common patterns** | **Misses novel phrasings** |
| Rule-based + learning | 0 | <1ms | High (improves over time) | Cold start on new patterns |

We choose rule-based signals because:
1. **Zero token cost** — the primary constraint
2. **Deterministic** — same input always produces same signals, debuggable
3. **Improvable** — the self-improving selector can learn new signal patterns from misclassification feedback (see §3.8)
4. **Composable** — signals combine with existing semantic retrieval, not replace it

### 3.7 Multi-Stage Selection Detail

```
Stage 0: PRE-FILTER (deterministic, <1ms, 0 tokens)
  - Extract ConversationState from messages
  - Match state signals against skill tags
  - Reorder skill pool (preferred first, deprioritized last)
  - Fallback: if no signals match, pass full pool unchanged

Stage 1: RETRIEVE (semantic vector search, <50ms, 0 prompt tokens)
  - Encode query into embedding vector
  - Search against pre-filtered skill pool (SkillIndex)
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
| Skill count | Stage 0 | Stage 1 method | Prompt tokens (5 candidates) |
|-------------|---------|---------------|------------------------------|
| <20         | optional | keyword OK    | ~500-2000 (budget-capped)    |
| 20-100      | recommended | semantic required | ~500-2000 (budget-capped)|
| 100+        | required | semantic + hierarchical | ~500-2000 (budget-capped) |

Prompt token cost stays **constant** regardless of total skill count — only the retrieval index grows. Pre-filtering cost stays **constant** regardless of skill count — it's a list reorder, not a search.

### 3.8 Auditable Selection

Selection events are still recorded in `skill_selection_events` for audit purposes. The `ToolRegistry` itself does not write audit events — this is handled by the ChatLoop's event logger.

### ~~3.9 Self-Improving Selection~~ (REMOVED)

> **Deleted**: The `SelfImprovingSelector`, `learning_signals.py`, `learning_config.py`, `learning_similarity.py` modules have been removed. The learning API endpoints (`/learning/trigger`, `/learning/signals`, `/learning/stats`) are stubs returning empty data. The `submit_feedback` endpoint still works for recording user feedback on selection events.

### ~~3.10 Procedural Memory Bridge~~ (REMOVED)

> **Deleted**: `core/skills/procedural_memory.py` was removed along with the learning system.

### ~~3.11 Self-Learning Upgrade Path~~ (REMOVED)

> **Deleted**: Design target removed along with the learning system.

### 3.12 Relationship to RouteStage (agent-loop-reliability.md)

The `RouteStage` in the ChatLoop pipeline (see [agent-loop-reliability.md](agent-loop-reliability.md)) is a **post-filter** that operates on the tool schema list *after* `ToolRegistry` has selected candidates.

```
ToolRegistry.select() (pinned + intent filter + prefilter + embedding retrieval)
  → produces tools_schema
    → RouteStage (post-filter: intent-based tool scoping)
      → produces final tools_schema for LLM
```

### 3.13 Maintainability: Tag and Marker Lifecycle

Skill tags and conversation state markers are the two manual maintenance surfaces in pre-filtering. Left unmanaged, they drift and become unreliable. This section defines how they stay correct.

#### Skill Tags

**Who maintains**: Skill author at registration time. Tags are declared in `manifest.yaml` and stored in `skills_registry.tags`.

**How they stay correct**:

1. **Registration-time validation** — `SkillCatalog.register()` validates tags against a fixed enum of allowed values. Unknown `scope` or `data_source` values are rejected at registration, not discovered at runtime.

```python
VALID_SCOPES = {"current_session", "historical", "cross_session", "external"}
VALID_DATA_SOURCES = {"session_metadata", "event_store", "memory_store", "external_api"}
VALID_INTENT_TYPES = {"analytical", "fetch", "mutate", "introspect"}
```

2. **Default inference for untagged skills** — Skills registered without tags get defaults inferred from `category`:

| category | Inferred scope | Inferred data_source |
|----------|---------------|---------------------|
| github, jira, external | external | external_api |
| analysis, evaluation | historical | event_store |
| memory, knowledge | cross_session | memory_store |
| (unknown) | no tags → pre-filter passes through |

3. **Drift detection** — The learning cycle (§3.9) detects when a skill's tags don't match its actual usage pattern. If `WRONG_SKILL` signals consistently involve a skill whose tags should have prevented selection, the system logs a `tag_drift_warning` event with a suggested correction. Human reviews and updates the manifest.

**What this means**: tags are not a growing maintenance burden. They are set once per skill, validated at registration, and drift-detected automatically. The only manual action is reviewing drift warnings — which is the same workflow as reviewing any other learning signal.

#### Conversation State Markers

**Who maintains**: Platform developers. Markers are hardcoded string sets in `ConversationState`.

**How they stay correct**:

1. **Markers are intentionally small** — Each set has 5-10 entries. This is not a growing dictionary; it's a fixed set of high-signal patterns.

2. **Coverage monitoring** — The system reports `pre_filter_hit_rate`: the percentage of queries where at least one signal fired. If hit rate drops below 30%, markers may need expansion. If hit rate exceeds 80%, markers may be too aggressive.

3. **Learning-driven expansion** — When the learning cycle (§3.9) identifies `WRONG_SKILL` clusters that share a query pattern not covered by existing markers, it logs a `marker_gap` event. Platform developers review and add markers. This is a low-frequency event (new markers needed maybe once per quarter as usage patterns stabilize).

4. **Bilingual by default** — Markers include both Chinese and English variants. New markers must include both languages.

**What this means**: markers are a small, stable set that grows slowly via learning feedback. They are not a scaling concern.

### 3.14 Testing Strategy and Success Criteria

#### Unit Tests

**Pre-filter logic** (deterministic, fast):

```python
# Test: history reference + analytical intent → prefer historical skills
def test_prefilter_history_analytical():
    skills = [mock_skill("introspection", tags={"scope": "current_session"}),
              mock_skill("event_reader", tags={"scope": "historical"})]
    state = ConversationState(references_history=True, is_analytical=True)

    result = pre_filter(skills, state)

    # event_reader should be first (preferred), introspection last (deprioritized)
    assert result[0].name == "event_reader"
    assert result[1].name == "introspection"
    # Both still present — pre-filter never removes
    assert len(result) == 2

# Test: no signals → pass through unchanged
def test_prefilter_no_signals():
    skills = [mock_skill("a"), mock_skill("b")]
    state = ConversationState()  # all False

    result = pre_filter(skills, state)
    assert result == skills  # unchanged order

# Test: empty skills → empty result (no crash)
def test_prefilter_empty():
    assert pre_filter([], ConversationState()) == []
```

**Conversation state extraction**:

```python
# Test: Chinese history markers
def test_state_chinese_history():
    msgs = [{"role": "user", "content": "分析一下前一个上下文"}]
    state = ConversationState.from_messages(msgs)
    assert state.references_history is True
    assert state.is_analytical is True

# Test: English markers
def test_state_english():
    msgs = [{"role": "user", "content": "analyze the previous context"}]
    state = ConversationState.from_messages(msgs)
    assert state.references_history is True
    assert state.is_analytical is True

# Test: no markers → all False
def test_state_neutral():
    msgs = [{"role": "user", "content": "hello"}]
    state = ConversationState.from_messages(msgs)
    assert state.references_history is False
    assert state.is_analytical is False
```

**Tag validation at registration**:

```python
def test_register_skill_invalid_tag():
    with pytest.raises(ValueError, match="Invalid scope"):
        catalog.register(skill_def_with_tags({"scope": "invalid_value"}))

def test_register_skill_default_tags():
    skill = catalog.register(skill_def_no_tags(category="github"))
    assert skill.tags["scope"] == "external"
    assert skill.tags["data_source"] == "external_api"
```

#### Integration Tests

**End-to-end selection with pre-filtering** (real DB, real pipeline):

```python
def test_pipeline_prefilter_improves_selection(db_factory):
    """The actual failure case from session 019cbb9e."""
    registry = ToolRegistry(embed_fn=embed_fn, max_tokens=50000)

    # Register two skills with overlapping descriptions but different tags
    register_skill(db_factory, "introspection",
        description="Analyze session context and agent state",
        tags={"scope": "current_session", "data_source": "session_metadata"})
    register_skill(db_factory, "event_reader",
        description="Analyze historical events and decision chains",
        tags={"scope": "historical", "data_source": "event_store"})

    # Query that references history + is analytical
    result = pipeline.get_tools_schema(
        query="分析一下前一个上下文的情况还有决策链评估",
        session_id="test",
        conversation_state=ConversationState(references_history=True, is_analytical=True),
    )

    tool_names = [t["function"]["name"] for t in result.tools]
    # event_reader should rank higher than introspection
    assert tool_names.index("event_reader") < tool_names.index("introspection")
    assert result.pre_filter_applied is True
```

#### Success Criteria

| Metric | Target | How to measure |
|--------|--------|---------------|
| Pre-filter accuracy | ≥90% of `WRONG_SKILL` signals from disambiguation failures are preventable | Backtest against historical `skill_selection_events` |
| Zero retrieval regression | 0 cases where pre-filter causes correct skill to be missed | Pre-filter reorders, never removes — structurally guaranteed |
| Token overhead | 0 additional prompt tokens | Structural — tags never enter LLM context |
| Latency overhead | <1ms per selection | Benchmark `pre_filter()` on 100-skill pool |
| Pre-filter hit rate | 30-70% of queries trigger at least one signal | monitoring dashboard |
| Marker maintenance frequency | <1 update per quarter after stabilization | Track `marker_gap` events over time |

#### Backtest Validation (Before Deployment)

Before deploying pre-filtering, run a backtest against historical selection events:

```python
def backtest_prefilter(db_factory, days=30):
    """Replay historical selections with pre-filtering enabled.
    
    For each WRONG_SKILL event:
    1. Reconstruct ConversationState from the original query
    2. Run pre_filter() on the original candidate set
    3. Check if the correct skill would have ranked higher
    """
    wrong_skill_events = get_wrong_skill_events(db_factory, days)
    preventable = 0

    for event in wrong_skill_events:
        state = ConversationState.from_messages(
            reconstruct_messages(event.session_id, event.event_id))
        original_candidates = reconstruct_candidates(event)
        filtered = pre_filter(original_candidates, state)

        correct_skill = event.correction_suggestion
        if filtered and filtered[0].name == correct_skill:
            preventable += 1

    return {
        "total_wrong_skill": len(wrong_skill_events),
        "preventable_by_prefilter": preventable,
        "prevention_rate": preventable / len(wrong_skill_events) if wrong_skill_events else 0,
    }
```

Deploy only if `prevention_rate ≥ 0.5` (pre-filter prevents at least half of disambiguation failures). This is conservative — even 50% prevention with zero token cost is a clear win.

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

#### user_credentials — per-user encrypted secrets (deprecated → §13)

> ⚠️ **Deprecated**: Replaced by `skill_settings` (is_secret=1) + `skill_resource_bindings` in §13 Skill Configuration Center. The new design adds scope chain (global → tenant → user), per-resource bindings, and type validation. See §13 for migration path.

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

Every mutation to a user's installed skill set must preserve the **dependency invariant**: all version constraints declared by all installed skills are satisfied. `install()` establishes this invariant; `upgrade()`, `rollback()`, and `uninstall()` must not break it.

**Install Flow**:
```
User: "install github skill"
  ├─ 1. Check permission → query skill_permissions
  ├─ 2. Resolve dependency tree (DependencyResolver):
  │     ├─ Cycle detection (DFS)
  │     ├─ Missing dependency check (transitive)
  │     ├─ Version constraint validation (all constraints in tree)
  │     └─ Topological sort (install order)
  ├─ 3. Verify all skill-type deps are installed for this user
  ├─ 4. Parse manifest → extract settings/secrets/resources schema (§13)
  │     ├─ Prompt for required secrets without defaults
  │     ├─ Apply manifest defaults for settings
  │     ├─ Validate types and constraints
  │     └─ Store in skill_settings (scope=user)
  └─ 5. Record installation → INSERT INTO skill_installations
```
No DDL execution. Tables already exist in platform DB (created by `init_db()`).

**Upgrade Flow**:
```
User: "upgrade knowledge"  (knowledge currently 1.5.0, registry has 2.0.0)
  ├─ 1. Verify skill is installed and new version exists in registry
  ├─ 2. Reverse dependency check: find all installed skills that depend on this skill
  │     For each dependent, verify new version satisfies their version constraint
  │     If any constraint would break → reject with DependencyConflictError
  │     Example: skill_a requires knowledge ~=1.2 → upgrading to 2.0.0 rejected
  ├─ 3. Forward dependency check: if the new version has different depends_on,
  │     resolve the new dependency tree (same as install: cycles, missing, versions)
  ├─ 4. Update skill_installations.skill_version, record previous_version
  └─ 5. On failure: previous_version enables rollback
```

**Rollback Flow**:
```
User: "rollback knowledge"  (knowledge currently 2.0.0, previous was 1.5.0)
  ├─ 1. Verify skill is installed and has previous_version
  ├─ 2. Same validation as upgrade but targeting previous_version:
  │     ├─ Reverse dependency check (dependents still satisfied?)
  │     └─ Forward dependency check (old version's deps still available?)
  └─ 3. Swap skill_version ↔ previous_version
```

**Uninstall Flow**:
```
User: "uninstall github skill"
  ├─ 1. Reverse dependency check: find all installed skills that depend on this skill
  │     If any exist → reject with error listing dependents
  │     "Cannot uninstall 'github': required by 'code_review', 'pr_tracker'.
  │      Uninstall dependents first, or use --force to skip this check."
  ├─ 2. Mark as uninstalled in skill_installations
  └─ 3. Delete settings, secrets, and resource bindings (skill_settings + skill_resource_bindings)
```
No DROP TABLE. Skill data remains in platform DB.

**Runtime Enforcement** (`require_executable`):
```
Agent calls skill during session
  ├─ 1. Verify skill is installed and active
  ├─ 2. Verify all skill-type dependencies are installed
  ├─ 3. Verify installed dependency versions satisfy constraints
  │     (last line of defense if invariant was violated by a bug or --force)
  └─ 4. Verify user has permission
```

**Design rationale**: Version validation at every mutation point (install/upgrade/rollback/uninstall) is the primary safety mechanism. Runtime version checking in `require_executable` is a defense-in-depth backstop — it should never trigger if the mutation gates work correctly, but it catches edge cases like `--force` uninstalls or direct DB edits.

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
| Per-user credentials | ❌ env vars | ❌ env vars | ❌ | ✅ encrypted, scoped, per-resource (§13) |
| Skill-local models | ❌ | ❌ | ❌ | ✅ `skills/{name}/models.py` |
| Self-improving selection | ❌ | ❌ | ❌ | ~~Removed~~ |
| Unified selection pipeline | ❌ | ❌ | ❌ | ✅ ToolRegistry (pinned/dynamic + embedding) |
| Context-aware pre-filtering | ❌ | ❌ | ❌ | ✅ skill tags + conversation state (§3.5) |
| Learning rollback | ❌ | ❌ | ❌ | ~~Removed~~ |
| Regression gate for learnings | ❌ | ❌ | ❌ | ~~Removed~~ (regression gate still exists for other uses) |

---

## 8. Open Questions

1. ~~**Cross-skill data access**: can knowledge skill read from github skill's tables?~~
   **Resolved** — see §9 Cross-Skill Data Access below.

2. **Schema evolution**: how to handle ALTER TABLE when platform upgrades a skill?
   - Same as any other migration — platform-level, applied by operator

3. ~~**Skill cost → budget gate integration**: `SignalWeights.cost` (0.2) drives learning, but skill execution cost doesn't flow back to the budget control system in [Deployment Architecture](deployment-architecture.md). Need: execution cost recorded per-skill → aggregated per-session → checked against session budget before next skill call.~~
   **Resolved** — `record_feedback(HIGH_COST)` now carries `actual_tokens` + `actual_usd`; ChatLoop reads per-skill cost for budget gate enforcement. See §3 "Cost signal closed-loop".

4. **Prompt evolution → skill regression gate**: `InputFaceLearner` can modify prompts, but prompt changes may alter which skills the LLM selects. Should prompt changes trigger the skill selection regression gate? Current answer: no (they are independent input faces). May need cross-face regression testing.

5. ~~**Skill configuration center**: skills need non-secret parameters (URLs, timeouts, review styles) with scoped defaults and install-time validation.~~
   **Resolved** — see §13 Skill Configuration Center below.

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

### Direct JOIN Optimization (P2, opt-in)

For performance-sensitive queries, skills can declare `direct_join: true` in their manifest dependency:

```yaml
depends_on:
  - name: github
    direct_join: true   # opt-in: allow raw SQL JOINs against github tables
```

When enabled, `SkillDataBridge.query()` returns a SQLAlchemy `Subquery` instead of materialized dicts, allowing the caller to compose JOINs in a single DB round-trip. The bridge still validates the dependency declaration and logs the access. This is strictly opt-in — default remains dict-based to preserve schema decoupling.

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

settings:
  - name: default_project
    type: string
    description: "Default project key"

secrets:
  - name: api_token
    description: "Jira API token (fallback)"

resources:
  type: project
  key_pattern: "{project_key}"
  bindings:
    - name: api_token
      type: secret
      required: true
    - name: board_id
      type: integer

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

### Validation & Testing (P1)

`mo-admin skill validate <skill_dir>` performs post-scaffold consistency checks:
- Generated code matches manifest declarations (actions, tables, config keys)
- Type annotations are consistent between YAML types and Python signatures
- Required config keys have no missing defaults
- Action side-effect categories match manifest declarations

Optional: `--generate-tests` flag auto-generates a pytest skeleton per action (happy path + missing-required-config error case).

### Web YAML Editor (P2, Optional)

A lightweight browser-based YAML editor with:
- Live JSON Schema validation (red squiggles on invalid fields)
- Visual table/action builder (form → YAML, not the other way around)
- One-click "Scaffold & Download" that calls `mo-admin skill scaffold` server-side

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

**Promoted to P1.** Minimum viable sandbox ships before marketplace opens:

| Phase | Scope | Effort |
|-------|-------|--------|
| P1 | Two-tier runtime: Trusted (in-process) + Sandboxed (Docker + stdin/stdout proxy) | 3 days |
| P1 | Mandatory sandbox for `source != "builtin"` skills at install time | 0.5 day |
| P2 | Strict tier (gVisor + no-network) for user-uploaded skills | 2 days |
| P2 | Resource quota enforcement (memory/CPU/timeout) | 1 day |

**P1 gate**: No third-party skill may execute in-process. `SkillManager.install()` rejects marketplace skills that declare `sandbox.enabled: false`.

---

## 12. Dependency Management Enhancement

Dependency versioning is fully implemented across all mutation paths.

**Implemented**:
- Semantic versioning with pip-style constraints (`>=1.0,<2.0`, `~=1.2.3`, etc.)
- Typed dependencies: `Dependency(name, version_constraint, type=skill|tool)`
- Full dependency tree validation: cycles, missing, version conflicts, transitive deps
- Topological sort for install ordering
- Backward compatible with old `depends_on: ["name"]` format
- CLI `upgrade-check` command for impact analysis
- `upgrade()` — validates new version satisfies dependents' constraints + forward deps resolvable
- `rollback()` — validates old version satisfies dependents' constraints + forward deps installed
- `uninstall()` — rejects if reverse dependents exist (supports `force=True` to bypass)
- `require_executable()` — checks dependency existence and version compatibility (defense-in-depth)
- API layer (`marketplace.py`) — returns 409 CONFLICT for all dependency errors

See [Skill Dependencies Guide](../guides/skill-dependencies.md) for usage documentation.

---

## 13. Skill Configuration Center

### Problem

Skills need runtime parameters to function. The current system has three separate, incomplete mechanisms:

1. **`SkillUserCredential` table** — encrypted per-user secrets, but no type validation, no scoping beyond user, no install-time prompting, no pre-execution gate.
2. **`ScopeResolver` + `Token` table** — scope-chain resolution for LLM tokens (`repo > project > user > global`), but not connected to the skill system at all.
3. **`RepoRegistry`** — per-repo token linking, but only for repo-level access, not generalized to arbitrary skill resources.

These three systems solve overlapping problems with incompatible designs. Meanwhile, real-world skills need all of:

- **Skill-level settings** — `api_base_url`, `timeout`, `review_style` (plaintext, scoped)
- **Skill-level secrets** — `github_token`, `api_key` (encrypted, scoped)
- **Resource-bound credentials** — repo A uses token X with read access, repo B uses token Y with write access (encrypted, per-resource, per-user)

No current system handles all three. The result: skills either hardcode defaults, fail at runtime with missing config, or each skill reinvents its own config storage.

### Design: Unified Configuration with Resource Bindings

One system. Three value types (plaintext, secret, resource-bound secret). One scope chain. One resolution API. One validation gate.

#### Core Model

```
┌─────────────────────────────────────────────────────────────┐
│                    Skill Manifest                            │
│                                                             │
│  settings:          (plaintext, scoped)                     │
│    - api_base_url   string, default "https://api.github.com"│
│    - timeout        integer, default 30                     │
│                                                             │
│  secrets:           (encrypted, scoped)                     │
│    - default_token  "Fallback token for all resources"      │
│                                                             │
│  resources:         (per-resource credential bindings)      │
│    type: repo                                               │
│    key_pattern: "{owner}/{name}"                            │
│    bindings:                                                │
│      - name: read_token   type: secret, required: true      │
│      - name: write_token  type: secret, required: false     │
│      - name: branch       type: string, default: "main"     │
└─────────────────────────────────────────────────────────────┘
                           │
                    resolve(skill, user)
                           │
                           ▼
┌─────────────────────────────────────────────────────────────┐
│                  Effective Configuration                     │
│                                                             │
│  settings:                                                  │
│    api_base_url: "https://github.corp.com/api/v3"  (tenant) │
│    timeout: 30                              (manifest default)│
│                                                             │
│  secrets:                                                   │
│    default_token: "ghp_xxx..."              (user)          │
│                                                             │
│  resources:                                                 │
│    "matrixorigin/matrixone":                                │
│      read_token: "ghp_aaa..."               (user+resource) │
│      write_token: "ghp_bbb..."              (user+resource) │
│      branch: "main"                         (manifest default)│
│    "some-org/private":                                      │
│      read_token: "ghp_ccc..."               (user+resource) │
│      write_token: null                      (not set)       │
│      branch: "develop"                      (user+resource) │
└─────────────────────────────────────────────────────────────┘
```

#### Manifest Declaration

Three sections replace the old `credentials:` and add what was missing:

```yaml
# skills/github/manifest.yaml
name: github
version: "1.0.0"
description: "GitHub integration — PRs, issues, CI status, code search"

# ── Skill-level settings (plaintext, scoped) ──
settings:
  - name: api_base_url
    type: string
    description: "GitHub API base URL (for GitHub Enterprise)"
    default: "https://api.github.com"
  - name: request_timeout
    type: integer
    description: "HTTP request timeout in seconds"
    default: 30
    min: 5
    max: 300
  - name: pr_review_style
    type: enum
    values: [thorough, quick, security-only]
    description: "Default review depth"
    default: "thorough"
  - name: max_files_per_review
    type: integer
    description: "Max files to include in a single review"
    default: 50
    min: 1
    max: 500

# ── Skill-level secrets (encrypted, scoped) ──
secrets:
  - name: default_token
    description: "Fallback GitHub token when no resource-specific token is set"
    required: false

# ── Per-resource bindings (encrypted + plaintext, per resource instance) ──
resources:
  type: repo                          # resource type name
  key_pattern: "{owner}/{name}"       # how resource keys are formatted
  description: "GitHub repository"
  bindings:
    - name: read_token
      type: secret
      description: "Token with read access to this repo"
      required: true
    - name: write_token
      type: secret
      description: "Token with write access (PRs, issues, etc.)"
      required: false
    - name: default_branch
      type: string
      description: "Default branch for this repo"
      default: "main"
    - name: review_enabled
      type: boolean
      description: "Enable automatic PR review for this repo"
      default: true
```

**Why three sections, not one?**

| Section | Encrypted? | Scoped? | Per-resource? | Example |
|---------|-----------|---------|---------------|---------|
| `settings` | No | global → tenant → user | No | `api_base_url`, `timeout` |
| `secrets` | Yes | global → tenant → user | No | `default_token`, `api_key` |
| `resources` | Mixed | user only | Yes | repo tokens, channel webhooks |

Collapsing these into one flat list (the old `credentials:` approach) loses the semantic distinction. A `timeout` doesn't need encryption. A per-repo `write_token` doesn't inherit from tenant scope — it's bound to a specific resource instance.

#### Type System

```yaml
# Supported types for settings, secrets, and resource bindings
types:
  string:
    constraints: [min_length, max_length, pattern]
  integer:
    constraints: [min, max]
  float:
    constraints: [min, max]
  boolean:
    constraints: []
  enum:
    constraints: [values]  # required: list of allowed values
  secret:
    constraints: []        # always encrypted, no plaintext validation
  url:
    constraints: [schemes]  # e.g. [https, http]
    # Syntactic sugar for string + URL pattern validation

# Universal fields (apply to all entries in settings, secrets, and resources.bindings):
#   required: bool   — default false. If true, validation fails when value is missing.
#   default: Any     — default value from manifest (lowest priority in scope chain).
#   description: str — human-readable description for CLI prompts and API docs.
```

#### Database Schema

**Replace** `skill_user_credentials` with two tables:

```python
class SkillSetting(Base):
    """Skill configuration: settings (plaintext) and secrets (encrypted).
    
    Scope chain: user → tenant → global → manifest default.
    Secrets are encrypted via CredentialManager before storage.
    """
    __tablename__ = "skill_settings"
    __table_args__ = (
        UniqueConstraint(
            "skill_name", "setting_name", "scope_type", "scope_id",
            name="uq_skill_setting_scope"
        ),
    )

    setting_id = Column(String(36), primary_key=True, default=lambda: str(uuid4()))
    skill_name = Column(String(100), nullable=False, index=True)
    setting_name = Column(String(100), nullable=False)
    setting_value = Column(Text, nullable=False)       # plaintext or encrypted
    is_secret = Column(SmallInteger, nullable=False, default=0)  # 0=plaintext, 1=encrypted
    scope_type = Column(String(20), nullable=False)    # "global" | "tenant" | "user"
    scope_id = Column(String(36), nullable=True)       # NULL for global
    created_at = Column(DateTime, default=func.now(), nullable=False)
    updated_at = Column(DateTime, default=func.now(), onupdate=func.now())
    updated_by = Column(String(36), nullable=False)


class SkillResourceBinding(Base):
    """Per-resource credential and config bindings.
    
    Each row = one (user, skill, resource_key, binding_name) tuple.
    Example: user=alice, skill=github, resource_key=matrixorigin/matrixone,
             binding_name=read_token, value=<encrypted token>
    """
    __tablename__ = "skill_resource_bindings"
    __table_args__ = (
        UniqueConstraint(
            "user_id", "skill_name", "resource_key", "binding_name",
            name="uq_skill_resource_binding"
        ),
    )

    binding_id = Column(String(36), primary_key=True, default=lambda: str(uuid4()))
    user_id = Column(String(36), nullable=False, index=True)
    skill_name = Column(String(100), nullable=False, index=True)
    resource_type = Column(String(50), nullable=False)   # "repo", "channel", "project"
    # Denormalized from manifest — same for all rows with same (skill_name, resource_key).
    # Stored per-row for query convenience (filter by type without joining manifest).
    resource_key = Column(String(500), nullable=False)   # "matrixorigin/matrixone"
    binding_name = Column(String(100), nullable=False)   # "read_token", "write_token"
    binding_value = Column(Text, nullable=False)         # plaintext or encrypted
    is_secret = Column(SmallInteger, nullable=False, default=0)
    created_at = Column(DateTime, default=func.now(), nullable=False)
    updated_at = Column(DateTime, default=func.now(), onupdate=func.now())
```

**Why two tables instead of one?**

`skill_settings` has scope chain (global → tenant → user) with no resource dimension.
`skill_resource_bindings` has resource dimension with no scope chain (always per-user — you don't inherit someone else's repo token).

Merging them into one table would require nullable `resource_key` + nullable `scope_type` with a CHECK constraint to enforce mutual exclusivity. Two tables is cleaner and queryable.

**Migration from `skill_user_credentials`:**

```sql
-- One-time migration: move existing credentials to skill_settings
INSERT INTO skill_settings (setting_id, skill_name, setting_name, setting_value, is_secret, scope_type, scope_id, updated_by)
SELECT credential_id, skill_name, credential_name, value_encrypted, 1, 'user', user_id, user_id
FROM skill_user_credentials;

-- Then drop old table
DROP TABLE IF EXISTS skill_user_credentials;
```

#### Scope Resolution

```
                    skill_settings scope chain
                    ───────────────────────────
                    
user (scope_type="user", scope_id=alice)        ← highest priority
  ↓ fallback
tenant (scope_type="tenant", scope_id=acme)
  ↓ fallback
global (scope_type="global", scope_id=NULL)
  ↓ fallback
manifest default                                 ← lowest priority


                    skill_resource_bindings
                    ───────────────────────
                    
user + resource_key (exact match)                ← only level
  ↓ fallback
skill-level secret (from skill_settings)         ← e.g. default_token
  ↓ fallback
not set → validation error if required
```

**Example resolution for GitHub skill, user alice:**

```
api_base_url:
  skill_settings WHERE skill=github, name=api_base_url, scope=user, scope_id=alice  → miss
  skill_settings WHERE skill=github, name=api_base_url, scope=tenant, scope_id=acme → "https://github.corp.com/api/v3"
  ✓ resolved: "https://github.corp.com/api/v3" (from tenant)

read_token for matrixorigin/matrixone:
  skill_resource_bindings WHERE user=alice, skill=github, resource_key=matrixorigin/matrixone, name=read_token → "ghp_aaa..."
  ✓ resolved: "ghp_aaa..." (from resource binding)

read_token for unknown-org/new-repo:
  skill_resource_bindings WHERE user=alice, skill=github, resource_key=unknown-org/new-repo, name=read_token → miss
  skill_settings WHERE skill=github, name=default_token, scope=user, scope_id=alice → "ghp_fallback..."
  ✓ resolved: "ghp_fallback..." (fallback to skill-level default_token)

write_token for unknown-org/new-repo:
  skill_resource_bindings → miss
  skill_settings default_token → "ghp_fallback..." (same token for read/write)
  ✓ resolved: "ghp_fallback..." (fallback)
```

#### Core API

```python
class SkillConfigCenter:
    """Unified configuration center for skills.
    
    Handles settings (plaintext), secrets (encrypted), and resource bindings.
    Single entry point for all skill configuration needs.
    """

    def __init__(
        self,
        db_factory: Callable[[], Session],
        credential_mgr: CredentialManager,
        manifest_loader: Callable[[str], dict | None],
    ):
        self.db_factory = db_factory
        self.credential_mgr = credential_mgr
        self.manifest_loader = manifest_loader

    # ── Settings & Secrets (scoped) ──
    # Note: set_setting takes explicit (scope_type, scope_id) because the caller
    # chooses WHERE to write (user scope, tenant scope, or global).
    # get_setting takes (user_id, tenant_id) because it resolves the full chain
    # automatically: user → tenant → global → manifest default.

    def set_setting(
        self, skill_name: str, setting_name: str, value: Any,
        scope_type: str = "user", scope_id: str | None = None,
        updated_by: str = "",
    ) -> None:
        """Set a setting or secret at a specific scope level.
        Auto-encrypts if manifest declares it as secret."""
        ...

    def get_setting(
        self, skill_name: str, setting_name: str,
        user_id: str, tenant_id: str | None = None,
    ) -> Any:
        """Resolve effective setting value through scope chain + manifest default.
        Walks: user → tenant → global → manifest default, returns first hit."""
        ...

    # ── Resource Bindings ──

    def bind_resource(
        self, user_id: str, skill_name: str,
        resource_key: str, bindings: dict[str, Any],
    ) -> None:
        """Bind credentials/config to a specific resource.
        
        Example:
            center.bind_resource("alice", "github", "matrixorigin/matrixone", {
                "read_token": "ghp_aaa...",
                "write_token": "ghp_bbb...",
                "default_branch": "main",
            })
        """
        ...

    def get_resource_binding(
        self, user_id: str, skill_name: str,
        resource_key: str, binding_name: str,
    ) -> Any:
        """Get a specific resource binding, falling back to skill-level secret."""
        ...

    def get_resource_bindings(
        self, user_id: str, skill_name: str,
        resource_key: str,
    ) -> dict[str, Any]:
        """Get all bindings for a resource, with fallbacks applied."""
        ...

    def list_resources(
        self, user_id: str, skill_name: str,
    ) -> list[dict]:
        """List all resources the user has configured for a skill.
        
        Returns: [{"resource_key": "matrixorigin/matrixone", "resource_type": "repo", ...}]
        """
        ...

    def unbind_resource(
        self, user_id: str, skill_name: str, resource_key: str,
    ) -> None:
        """Remove all bindings for a resource."""
        ...

    # ── Bulk Resolution (for skill execution) ──

    def resolve_all(
        self, skill_name: str, user_id: str,
        tenant_id: str | None = None,
        resource_key: str | None = None,
    ) -> SkillConfig:
        """Resolve complete effective configuration for skill execution.
        
        Returns a SkillConfig with all settings, secrets, and resource bindings resolved.
        This is the single call made before skill execution.
        """
        ...

    # ── Validation ──

    def validate(
        self, skill_name: str, user_id: str,
        tenant_id: str | None = None,
        resource_key: str | None = None,
    ) -> list[ConfigValidationError]:
        """Validate all required config is present and well-typed.
        
        Returns empty list if valid, otherwise list of missing/invalid items.
        Called at install time and pre-execution.
        """
        ...

    def get_schema(self, skill_name: str) -> SkillConfigSchema:
        """Return the config schema from manifest (settings + secrets + resources)."""
        ...


@dataclass
class SkillConfig:
    """Resolved configuration passed to skill at execution time."""
    settings: dict[str, Any]           # {"api_base_url": "...", "timeout": 30}
    secrets: dict[str, str]            # {"default_token": "ghp_xxx"}
    resource: dict[str, Any] | None    # {"read_token": "ghp_aaa", "branch": "main"} if resource_key provided
    resource_type: str | None          # "repo", "project", etc. — from manifest, for multi-resource-type skills
    resource_key: str | None           # "matrixorigin/matrixone" — echo back for logging/debugging


@dataclass
class ConfigValidationError:
    """A missing or invalid configuration item."""
    section: str          # "settings" | "secrets" | "resources"
    name: str             # "read_token"
    resource_key: str | None  # "matrixorigin/matrixone" or None
    error: str            # "required but not set" | "invalid type: expected integer"
```

#### Skill Consumption Pattern

Skills receive a single `SkillConfig` object — they never touch the database or know about scopes:

```python
class GitHubSkillAPI:
    """GitHub skill receives resolved config. Zero knowledge of config storage."""

    def __init__(self, config: SkillConfig):
        self._base_url = config.settings.get("api_base_url", "https://api.github.com")
        self._timeout = config.settings.get("request_timeout", 30)
        # Resource-specific token if available, else skill-level default
        token = (config.resource or {}).get("read_token") or config.secrets.get("default_token")
        self._client = Github(auth=Auth.Token(token), base_url=self._base_url, timeout=self._timeout)

    def list_prs(self, repo: str, state: str = "open") -> list[dict]:
        ...
```

**Execution flow with resource context:**

```
Agent: "review PR #42 on matrixorigin/matrixone"
  │
  ├─ 1. Skill selector picks github skill
  ├─ 2. Extract resource_key from tool call args: "matrixorigin/matrixone"
  ├─ 3. config = config_center.resolve_all(
  │        skill_name="github",
  │        user_id="alice",
  │        tenant_id="acme",
  │        resource_key="matrixorigin/matrixone"
  │     )
  ├─ 4. errors = config_center.validate(...)
  │     If errors → return SkillConfigError to agent
  │     Agent can ask user: "I need a read token for matrixorigin/matrixone. Please configure it."
  ├─ 5. api = GitHubSkillAPI(config)
  └─ 6. result = api.review_pr(42)
```

#### Install-Time and Pre-Execution Validation

**Install flow** (replaces old step 4 "Prompt for credentials"):

```
User: "install github skill"
  ├─ 1. Check permission
  ├─ 2. Resolve dependency tree
  ├─ 3. Verify dependencies installed
  ├─ 4. Parse manifest → extract settings/secrets/resources schema
  ├─ 5. Prompt for required secrets without defaults
  │     "GitHub skill needs a default_token (optional). Provide now or later?"
  ├─ 6. Apply manifest defaults for settings
  ├─ 7. Store provided values in skill_settings (scope=user)
  ├─ 8. Record installation
  └─ 9. Show resource binding instructions:
        "To use GitHub skill with specific repos, run:
         mo-agent skill config github --resource matrixorigin/matrixone"
```

**Pre-execution gate** (replaces old `require_executable`):

```
Agent calls skill during session
  ├─ 1. Verify skill installed and active
  ├─ 2. Verify dependencies
  ├─ 3. Verify permission
  ├─ 4. Determine resource_key from tool call args (if applicable)
  ├─ 5. config_center.validate(skill, user, tenant, resource_key)
  │     ├─ Missing required setting → SkillConfigError
  │     ├─ Missing required secret → SkillConfigError
  │     ├─ Missing required resource binding → SkillConfigError
  │     └─ Type validation failure → SkillConfigError
  ├─ 6. config = config_center.resolve_all(skill, user, tenant, resource_key)
  └─ 7. Inject config into skill execution context
```

#### REST API

```
# ── Settings & Secrets ──
GET    /skills/{skill}/config                          # Effective config (resolved for current user)
GET    /skills/{skill}/config/schema                   # Config schema from manifest
PUT    /skills/{skill}/config/{name}                   # Set setting/secret (user scope)
DELETE /skills/{skill}/config/{name}                   # Reset to inherited/default
PUT    /skills/{skill}/config/{name}?scope=global      # Admin: set global default
PUT    /skills/{skill}/config/{name}?scope=tenant      # Admin: set tenant default

# ── Resource Bindings ──
GET    /skills/{skill}/resources                       # List configured resources
GET    /skills/{skill}/resources/{key}                 # Get bindings for a resource
PUT    /skills/{skill}/resources/{key}                 # Set/update resource bindings
DELETE /skills/{skill}/resources/{key}                 # Remove resource bindings

# ── Validation ──
GET    /skills/{skill}/config/validate                 # Validate all config present
GET    /skills/{skill}/config/validate?resource={key}  # Validate including resource
```

**Example API calls:**

```bash
# Set company-wide GitHub Enterprise URL (admin)
PUT /skills/github/config/api_base_url?scope=global
{"value": "https://github.corp.example.com/api/v3"}

# Set personal default token
PUT /skills/github/config/default_token
{"value": "ghp_personal_token_xxx"}

# Bind tokens to a specific repo
PUT /skills/github/resources/matrixorigin%2Fmatrixone
{
  "read_token": "ghp_read_only_xxx",
  "write_token": "ghp_write_xxx",
  "default_branch": "main"
}

# Check what's configured
GET /skills/github/config
→ {
    "settings": {"api_base_url": "https://github.corp.example.com/api/v3", "timeout": 30, ...},
    "secrets": {"default_token": "***"},
    "resources_configured": 2
  }

# Validate before execution
GET /skills/github/config/validate?resource=matrixorigin/matrixone
→ {"valid": true, "errors": []}

GET /skills/github/config/validate?resource=unknown-org/new-repo
→ {"valid": false, "errors": [{"section": "resources", "name": "read_token", "error": "required but not set"}]}
```

#### CLI Commands

```bash
# Interactive config setup
mo-agent skill config github
# → Shows current settings, prompts for missing required items

# Set a setting
mo-agent skill config github --set api_base_url=https://github.corp.com/api/v3

# Set a secret
mo-agent skill config github --secret default_token
# → Prompts for value (hidden input)

# Bind a resource
mo-agent skill config github --resource matrixorigin/matrixone
# → Prompts for read_token (required), write_token (optional), default_branch

# List resources
mo-agent skill config github --list-resources

# Validate
mo-agent skill config github --validate
```

#### Event Sourcing

All config mutations are events:

```python
# Setting changed (plaintext — value included)
event_type: "skill_config_changed"
content: {
    "skill": "github", "section": "settings",
    "name": "api_base_url", "scope": "tenant",
    "old_value": null, "new_value": "https://github.corp.com/api/v3"
}

# Secret changed (value NEVER logged — only the fact that it changed)
event_type: "skill_config_changed"
content: {
    "skill": "github", "section": "secrets",
    "name": "default_token", "scope": "user"
    # No old_value / new_value fields for secrets.
}

# Resource bound (binding names listed, secret values omitted)
event_type: "skill_resource_bound"
content: {
    "skill": "github", "resource_type": "repo",
    "resource_key": "matrixorigin/matrixone",
    "bindings_set": ["read_token", "write_token", "default_branch"]
    # Secret binding values are never logged. Non-secret values may be included.
}

# Resource unbound
event_type: "skill_resource_unbound"
content: {"skill": "github", "resource_key": "matrixorigin/matrixone"}
```

#### Relationship to Existing Systems

| Old Component | Disposition | Replacement |
|---------------|-------------|-------------|
| `SkillUserCredential` table | **DROP** | `skill_settings` (is_secret=1) |
| `CredentialManager` | **KEEP** | Still handles encrypt/decrypt |
| `SkillManager` credential CRUD | **REPLACE** | `SkillConfigCenter` |
| `ScopeResolver` | **REUSE pattern** | `SkillConfigCenter` implements same scope chain |
| `RepoRegistry.token_id` | **KEEP for LLM tokens** | Skill resource bindings handle skill-specific tokens |
| `SkillManager.require_executable()` | **EXTEND** | Calls `config_center.validate()` |

#### Another Skill Example: Jira

To show this isn't GitHub-specific:

```yaml
# skills/jira/manifest.yaml
name: jira
version: "1.0.0"

settings:
  - name: instance_url
    type: url
    schemes: [https]
    description: "Jira instance URL"
    required: true    # No default — must be configured
  - name: default_project
    type: string
    description: "Default project key for new issues"

secrets:
  - name: api_token
    description: "Jira API token (fallback for all projects)"
    required: false

resources:
  type: project
  key_pattern: "{project_key}"
  description: "Jira project"
  bindings:
    - name: api_token
      type: secret
      description: "Project-specific API token"
      required: true
    - name: board_id
      type: integer
      description: "Default board ID for this project"
    - name: issue_type
      type: enum
      values: [Bug, Story, Task, Epic]
      description: "Default issue type"
      default: "Task"
```

Resolution for Jira skill, user bob:
```
instance_url → skill_settings (tenant scope): "https://jira.corp.com"
api_token for PROJECT-A → skill_resource_bindings: "jira_token_for_project_a"
api_token for PROJECT-B → miss → fallback to skill_settings secret: "jira_default_token"
board_id for PROJECT-A → skill_resource_bindings: 42
```

#### Implementation Priority

| Phase | Scope | Effort | Status |
|-------|-------|--------|--------|
| P0 | `skill_settings` + `skill_resource_bindings` tables | 0.5 day | ✅ Done |
| P0 | `SkillConfigCenter` core (set/get/resolve/validate) | 1.5 days | ✅ Done |
| P0 | Manifest parsing (`settings:` / `secrets:` / `resources:`) | 0.5 day | ✅ Done |
| P0 | Pre-execution validation in `require_executable()` | 0.5 day | ✅ Done |
| P0 | Migration from `skill_user_credentials` | 0.5 day | ✅ Done |
| P1 | REST API endpoints (`api/routers/skill_config.py`) | 1 day | ✅ Done |
| P1 | CLI `mo-agent skill config` commands | 1 day | ✅ Done |
| P2 | Tenant-scope admin endpoints | 0.5 day | |
| P2 | Config change events | 0.5 day | |

---

## 14. Skill Table Registry

> **Status**: Design Target — P2

### Problem

All skill tables live in the same database schema with `sk_{skill}_` prefix convention. As skill count grows, this creates: (a) no enforcement that skills only touch their own tables, (b) no way to migrate a skill's tables independently, (c) no path to per-skill schema isolation if needed later.

### Solution: SkillTableRegistry

A platform-level registry that tracks which tables belong to which skill and enforces access boundaries:

```python
class SkillTableRegistry:
    """Tracks skill → table ownership. Enforces access at query time."""

    def tables_for(self, skill_name: str) -> list[str]:
        """Return table names owned by this skill."""

    def owner_of(self, table_name: str) -> str | None:
        """Return skill that owns this table, or None if platform table."""

    def validate_access(self, skill_name: str, table_name: str) -> bool:
        """True if skill is allowed to access this table (own tables + declared depends_on)."""
```

**Per-skill schema separation** (optional, MatrixOne-native): Skills can opt into a dedicated database (`CREATE DATABASE sk_github`) instead of shared prefix. The registry abstracts this — callers use logical table names, the registry resolves to physical `{db}.{table}`. Migration between shared-prefix and separate-database is a registry config change, not a code change.

**Integration points**:
- `SandboxRunner` uses `validate_access()` before proxying DB queries for sandboxed skills
- `SkillDataBridge` uses `tables_for()` to validate cross-skill access
- `init_db()` populates the registry from skill manifests at startup

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

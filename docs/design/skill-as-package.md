# Skill-as-Package: Stateful Skill Architecture with BYOD

> Design Document — 2026-02-22
> Status: Draft

## 1. Problem Statement

Current agent frameworks (LangChain, CrewAI, AutoGen, MCP) treat tools/skills as **stateless functions**. But real-world skills need persistent state:

- GitHub skill needs repos, PR cache, CI status cache
- Jira skill needs projects, issues, sprints
- Knowledge skill needs knowledge entries, relations, embeddings

Today in mo-agent-engine, these tables are mixed into `api/models.py` alongside core platform tables. There's no concept of "installing" a skill, no schema isolation, and no way for users to bring their own database.

## 2. Core Insight

**Skills are platform capabilities, not user plugins.** Their table schemas are deterministic — defined by the platform, not by users. This is the same model as `knowledge_entries` and `conversation_events`: platform defines the schema, skill provides an API layer for CRUD operations.

Users interact with skill data through **skill APIs**, not direct SQL:
```python
github.save_token(token)                    # → encrypted in user_credentials
github.add_repo("matrixorigin/matrixone")   # → INSERT INTO github_repos
github.list_prs(repo, state="open")         # → GitHub API + cache
github.get_pr_checks(repo, pr_number)       # → GitHub API + cache
```

## 3. Design Goals

1. **Platform-defined schema** — skill tables are defined in platform code, like any other model
2. **BYOD (Bring Your Own Database)** — users provide their own DB connection for skill data
3. **Install = DDL execution** — installing a skill runs platform-defined CREATE TABLE on user's DB
4. **Skill API layer** — skills expose typed APIs for data access, no direct SQL from users
5. **Admin-managed marketplace** — admin publishes skills, controls visibility per user/role
6. **Per-user credentials** — encrypted, managed through skill API

## 4. Architecture Overview

```
┌──────────────────────────────────────────────────────────────┐
│                     Platform Database                         │
│                  (managed by platform operator)               │
│                                                               │
│  Core tables (always present):                                │
│    users, roles, sessions, conversation_events,               │
│    agents, model_registry                                     │
│                                                               │
│  Skill management tables:                                     │
│    skill_definitions, skill_permissions,                      │
│    skill_installations, user_connections, user_credentials    │
│                                                               │
│  This DB stores: identity, audit trail, skill catalog,        │
│  user DB connections, encrypted credentials                   │
└──────────────────────────────────────────────────────────────┘
          │
          │  user registers DB connection
          │
          ▼
┌──────────────────────────────────────────────────────────────┐
│                  User Database (BYOD)                         │
│          (user's own MatrixOne / MySQL / etc.)                │
│                                                               │
│  Schema defined by platform, tables created on skill install  │
│  Database name: user-specified (e.g. "my_project")            │
│                                                               │
│  Skill tables (created on install via platform DDL):          │
│    github_repos, github_pr_cache                              │
│    knowledge_entries, knowledge_relations                     │
│    jira_projects, jira_issues                                 │
│                                                               │
│  Platform metadata (created on first install):                │
│    _agent_meta_installed_skills                               │
└──────────────────────────────────────────────────────────────┘
```

### Key Separation

| Concern | Where | Why |
|---------|-------|-----|
| User identity, auth, roles | Platform DB | Platform manages access control |
| Conversation events, sessions | Platform DB | Audit trail owned by platform |
| Skill catalog + permissions | Platform DB | Admin-managed marketplace |
| User's DB connection info | Platform DB (`user_connections`) | Platform needs to know how to connect |
| Skill credentials (tokens, keys) | Platform DB (`user_credentials`, encrypted) | Secure storage, not in user DB |
| Skill business data | User DB | Data sovereignty — user owns their data |

## 5. Skill Package Structure

A skill is a platform-managed Python package:

```
skills/
  github/
    __init__.py
    manifest.yaml          # metadata, dependencies, credentials
    models.py              # SQLAlchemy table definitions (platform-defined)
    api.py                 # typed API layer for data access
    actions.py             # skill actions (what the agent calls)
```

**No `migrations/` directory.** Schema is platform-defined. Version upgrades are handled by platform-level migration (same as `api/models.py` today).

### 5.1 Manifest

```yaml
# skills/github/manifest.yaml
name: github
version: "1.2.0"
description: "GitHub integration — PRs, issues, CI status, code search"
author: "mo-agent-engine"
table_prefix: github

# Tables this skill needs (platform-defined schema)
tables:
  - github_repos
  - github_pr_cache

# Required credentials (user provides via skill API)
credentials:
  - name: github_token
    type: secret
    description: "GitHub Personal Access Token or App token"
    required: true

# Platform capabilities needed
requires:
  - llm
  - http

depends_on: []

dialects:
  - mysql
```

### 5.2 Schema (Platform-Defined)

```python
# skills/github/models.py
"""GitHub skill tables — schema defined by platform, created in user BYOD."""

from sqlalchemy import Column, String, Integer, DateTime, JSON, Float
from sqlalchemy import UniqueConstraint, Index
from core.skills.schema_base import SkillTableBase

class GithubRepos(SkillTableBase):
    __tablename__ = "github_repos"
    __table_args__ = (
        UniqueConstraint("owner", "name", name="uq_github_repos_owner_name"),
    )

    repo_id = Column(String(36), primary_key=True)
    owner = Column(String(100), nullable=False)
    name = Column(String(100), nullable=False)
    full_name = Column(String(200), nullable=False)
    default_branch = Column(String(100), default="main")
    created_at = Column(DateTime, nullable=False)

class GithubPRCache(SkillTableBase):
    __tablename__ = "github_pr_cache"
    __table_args__ = (
        UniqueConstraint("repo_full_name", "pr_number", name="uq_github_pr_cache_repo_pr"),
        Index("ix_github_pr_cache_repo_state", "repo_full_name", "state"),
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

### 5.3 Skill API Layer

```python
# skills/github/api.py
"""GitHub skill API — typed interface for data access."""

class GitHubSkillAPI:
    """API layer for GitHub skill data. Users interact through this, not direct SQL."""

    def __init__(self, user_db: Session, credentials: dict[str, str]):
        self._db = user_db
        self._token = credentials.get("github_token")
        self._client = Github(auth=Auth.Token(self._token)) if self._token else None

    # --- Repo management ---

    def add_repo(self, owner: str, name: str) -> dict:
        """Register a repository for tracking."""
        # INSERT INTO github_repos ...

    def list_repos(self) -> list[dict]:
        """List registered repositories."""
        # SELECT * FROM github_repos ...

    # --- PR operations (API + cache) ---

    def list_prs(self, repo: str, state: str = "open", limit: int = 10) -> list[dict]:
        """List PRs. Fetches from GitHub API, caches in github_pr_cache."""
        # 1. Call GitHub API
        # 2. Upsert into github_pr_cache
        # 3. Return results

    def get_pr_checks(self, repo: str, pr_number: int) -> dict:
        """Get CI/check status for a specific PR."""
        # GitHub API: GET /repos/{owner}/{repo}/commits/{ref}/check-runs

    # --- Credential management ---

    def save_token(self, token: str) -> None:
        """Save/update GitHub token (encrypted in platform DB)."""
        # Handled by platform credential manager, not direct DB access
```

### 5.4 Skill Actions (What the Agent Calls)

```python
# skills/github/actions.py
"""GitHub skill actions — registered as tools for the agent."""

class ListPRsAction:
    name = "list_prs"
    description = "List open/closed PRs in a repository"

    async def execute(self, api: GitHubSkillAPI, repo: str, state: str = "open") -> dict:
        return api.list_prs(repo, state)

class GetPRChecksAction:
    name = "get_pr_checks"
    description = "Get CI/check status for a PR"

    async def execute(self, api: GitHubSkillAPI, repo: str, pr_number: int) -> dict:
        return api.get_pr_checks(repo, pr_number)
```

## 6. Platform Tables

These live in the platform database:

### 6.1 user_connections — BYOD registry

```python
class UserConnection(Base):
    __tablename__ = "user_connections"

    connection_id = Column(String(36), primary_key=True)
    user_id = Column(String(36), nullable=False, unique=True)
    dialect = Column(String(20), nullable=False)      # "mysql" | "matrixone"
    host = Column(String(255), nullable=False)
    port = Column(Integer, nullable=False)
    database = Column(String(100), nullable=False)    # user-chosen DB name
    username = Column(String(100), nullable=False)
    password_encrypted = Column(Text, nullable=False)  # encrypted at rest
    status = Column(String(20), default="active")
    created_at = Column(DateTime, nullable=False)
    verified_at = Column(DateTime)                     # last successful test
```

### 6.2 skill_definitions — marketplace catalog

```python
class SkillDefinition(Base):
    __tablename__ = "skill_definitions"

    skill_id = Column(String(36), primary_key=True)
    name = Column(String(100), nullable=False, unique=True)
    version = Column(String(20), nullable=False)
    description = Column(Text)
    manifest = Column(JSON, nullable=False)
    is_active = Column(Boolean, default=True)
    is_public = Column(Boolean, default=False)
    created_by = Column(String(36))
    created_at = Column(DateTime, nullable=False)
    updated_at = Column(DateTime)
```

### 6.3 skill_installations — per-user install state

```python
class SkillInstallation(Base):
    __tablename__ = "skill_installations"
    __table_args__ = (
        UniqueConstraint("user_id", "skill_name", name="uq_user_skill"),
    )

    installation_id = Column(String(36), primary_key=True)
    user_id = Column(String(36), nullable=False, index=True)
    skill_name = Column(String(100), nullable=False)
    skill_version = Column(String(20), nullable=False)
    status = Column(String(20), default="installed")  # installed | uninstalled
    installed_at = Column(DateTime, nullable=False)
    updated_at = Column(DateTime)
```

### 6.4 user_credentials — per-user encrypted secrets

```python
class UserCredential(Base):
    __tablename__ = "user_credentials"
    __table_args__ = (
        UniqueConstraint("user_id", "skill_name", "credential_name",
                         name="uq_user_skill_cred"),
    )

    credential_id = Column(String(36), primary_key=True)
    user_id = Column(String(36), nullable=False, index=True)
    skill_name = Column(String(100), nullable=False)
    credential_name = Column(String(100), nullable=False)
    value_encrypted = Column(Text, nullable=False)
    created_at = Column(DateTime, nullable=False)
    rotated_at = Column(DateTime)
```

### 6.5 skill_permissions — RBAC

```python
class SkillPermission(Base):
    __tablename__ = "skill_permissions"
    __table_args__ = (
        UniqueConstraint("skill_name", "grantee_type", "grantee_id",
                         name="uq_skill_grantee"),
    )

    permission_id = Column(String(36), primary_key=True)
    skill_name = Column(String(100), nullable=False, index=True)
    grantee_type = Column(String(10), nullable=False)  # "user" | "role"
    grantee_id = Column(String(36), nullable=False)
    granted_by = Column(String(36), nullable=False)
    granted_at = Column(DateTime, nullable=False)
```

## 7. Install / Uninstall Lifecycle

### 7.1 Install Flow

```
User: "install github skill"
  │
  ├─ 1. Check permission: user has access to this skill?
  │     → query skill_permissions
  │
  ├─ 2. Check connection: user has a BYOD connection?
  │     → query user_connections, test with SELECT 1
  │
  ├─ 3. Check dependencies: required skills installed?
  │     → query skill_installations
  │
  ├─ 4. Create tables on user's DB:
  │     → connect to user DB
  │     → CREATE TABLE github_repos (...)    ← platform-defined DDL
  │     → CREATE TABLE github_pr_cache (...)
  │     → CREATE TABLE _agent_meta_installed_skills IF NOT EXISTS
  │     → INSERT INTO _agent_meta_installed_skills
  │
  ├─ 5. Prompt for credentials:
  │     → "GitHub skill requires a GitHub token. Please provide:"
  │     → encrypt and store in user_credentials (platform DB)
  │
  └─ 6. Record installation:
        → INSERT INTO skill_installations (platform DB)
```

### 7.2 Uninstall Flow

```
User: "uninstall github skill"
  │
  ├─ 1. Check: any other skills depend on this?
  │
  ├─ 2. Option A: keep data (default)
  │     → mark as uninstalled in skill_installations
  │     → tables remain in user DB
  │
  └─ 3. Option B: clean uninstall (user explicitly requests)
        → DROP TABLE github_repos, github_pr_cache on user DB
        → DELETE credentials from user_credentials
        → mark as uninstalled
```

### 7.3 Upgrade Flow

```
Platform upgrades github skill v1.2.0 → v1.3.0
  │
  ├─ Schema change? (e.g. new column in github_pr_cache)
  │   → Platform-level ALTER TABLE on user DB (same as alembic migration)
  │   → Triggered on next use or admin push
  │
  └─ No schema change? (code-only update)
      → Update skill_installations.skill_version
      → Immediate, no DB changes needed
```

**Key difference from v1 design**: no per-skill migration files. Schema changes are platform-level migrations, same mechanism as `api/models.py` changes today.

## 8. Runtime: How Skills Execute

```python
# Pseudocode — runtime skill execution in ChatLoop

async def execute_skill(user_id, skill_name, params):
    # 1. Verify skill is installed
    installation = platform_db.query(SkillInstallation).filter_by(
        user_id=user_id, skill_name=skill_name, status="installed"
    ).one_or_none()
    if not installation:
        raise SkillNotInstalled(f"Install {skill_name} first: /skill install {skill_name}")

    # 2. Get user's DB connection + credentials
    conn_info = platform_db.query(UserConnection).filter_by(user_id=user_id).one()
    creds = get_decrypted_credentials(user_id, skill_name)

    # 3. Get user DB session (pooled)
    user_db = user_db_pool.get_session(user_id, conn_info)

    # 4. Create skill API instance
    api = GitHubSkillAPI(user_db=user_db, credentials=creds)

    # 5. Execute action through API
    try:
        action = skill.get_action(params["action_name"])
        return await action.execute(api, **params)
    finally:
        user_db.close()
```

### Connection Pooling

```python
class UserDBPool:
    """Per-user connection pool with lazy initialization."""

    def __init__(self):
        self._pools: dict[str, Engine] = {}

    def get_session(self, user_id: str, conn_info: UserConnection) -> Session:
        if user_id not in self._pools:
            self._pools[user_id] = create_engine(
                conn_info.to_url(),
                pool_size=3,
                max_overflow=2,
                pool_recycle=1800,
            )
        return Session(self._pools[user_id])

    def close_user(self, user_id: str):
        if user_id in self._pools:
            self._pools.pop(user_id).dispose()
```

## 9. Table Naming Convention

All skill tables: `{skill_name}_{table_name}`

```
github_repos, github_pr_cache
knowledge_entries, knowledge_relations
jira_projects, jira_issues
```

Platform metadata in user DB: `_agent_meta_` prefix (reserved).

## 10. Skill Permission Model

```
Admin publishes skill "github" to marketplace
  │
  ├─ Grant to role: all users with role "developer" can install
  ├─ Grant to user: only specific user can install
  └─ Public: all authenticated users can install
```

## 11. What Moves Out of Core

| Current Location | Becomes | Skill |
|-----------------|---------|-------|
| `Repo` in `api/models.py` | `github_repos` in `skills/github/models.py` | github |
| `core/repos/` | `skills/github/api.py` | github |
| `core/skills/github_client.py` | `skills/github/api.py` | github |
| `KnowledgeEntry` in `api/models.py` | `skills/knowledge/models.py` | knowledge |
| `KnowledgeRelation` in `api/models.py` | `skills/knowledge/models.py` | knowledge |
| `core/context/knowledge.py` | `skills/knowledge/api.py` | knowledge |
| `core/context/knowledge_graph.py` | `skills/knowledge/api.py` | knowledge |
| `core/context/hybrid_retrieval.py` | `skills/knowledge/api.py` | knowledge |

Tables that STAY in platform DB:
| Table | Reason |
|-------|--------|
| `users`, `roles` | Identity |
| `sessions`, `conversation_events` | Audit trail |
| `agents` | Agent definitions |
| `skill_definitions`, `skill_permissions` | Marketplace |
| `skill_selection_events` | Audit |
| `quality_assessments` | Quality tracking |
| `context_snapshots`, `decisions` | Audit |

## 12. Comparison with Industry

| Feature | ElizaOS | LangChain | MCP | **mo-agent-engine** |
|---------|---------|-----------|-----|---------------------|
| Skill has schema | ✅ plugin schema | ❌ | ❌ | ✅ platform-defined |
| Schema management | plugin owns | N/A | N/A | platform owns |
| BYOD | ❌ fixed PG | ❌ | ❌ | ✅ **unique** |
| Table namespace | ❌ bare names | ❌ | ❌ | ✅ `{skill}_{table}` |
| Install lifecycle | ❌ | ❌ | ❌ | ✅ |
| Skill API layer | ❌ direct SQL | ❌ | ❌ | ✅ typed API |
| Marketplace + RBAC | ❌ | ❌ | ❌ | ✅ |
| Per-user credentials | ❌ env vars | ❌ env vars | ❌ | ✅ encrypted |
| Multi-dialect | ❌ PG only | ❌ | ❌ | ✅ MySQL/MatrixOne |

**Key innovation**: platform-defined schema + BYOD + typed skill API. Skills are platform capabilities with deterministic schemas, not user-contributed plugins with arbitrary schemas. This is simpler, safer, and more consistent than the ElizaOS model.

## 13. Open Questions

1. **Cross-skill data access**: can knowledge skill read from github skill's tables?
   - Proposed: explicit dependency in manifest, platform provides cross-skill API

2. **Offline user DB**: what if user's DB is unreachable?
   - Proposed: graceful degradation, conversation continues with platform-only capabilities

3. **Schema evolution**: how to ALTER TABLE on user's DB when platform upgrades a skill?
   - Proposed: platform-level migration, same as alembic. Run on next use or admin push.

# Skill-as-Package: Stateful Skill Architecture

> Design Document — 2026-02-22
> Status: Draft (v3 — single DB, no BYOD for skill tables)

## 1. Problem Statement

Current agent frameworks (LangChain, CrewAI, AutoGen, MCP) treat tools/skills as **stateless functions**. But real-world skills need persistent state:

- GitHub skill needs repos, PR cache, CI status cache
- Jira skill needs projects, issues, sprints
- Knowledge skill needs knowledge entries, relations, embeddings

Today in mo-agent-engine, these tables are mixed into `api/models.py` alongside core platform tables. There's no concept of "installing" a skill, no schema isolation, and no naming convention to distinguish skill data from core data.

## 2. Core Insight

**Skills are platform capabilities, not user plugins.** Their table schemas are deterministic — defined by the platform, not by users. This is the same model as `knowledge_entries` and `conversation_events`: platform defines the schema, skill provides an API layer for CRUD operations.

All tables — core and skill — live in the **same platform database**. Skill tables are distinguished by naming convention (`sk_{skill_name}_`), not by physical database separation.

Users interact with skill data through **skill APIs**, not direct SQL:
```python
github.save_token(token)                    # → encrypted in user_credentials
github.add_repo("matrixorigin/matrixone")   # → INSERT INTO sk_github_repos
github.list_prs(repo, state="open")         # → GitHub API + cache
github.get_pr_checks(repo, pr_number)       # → GitHub API + cache
```

## 3. Design Goals

1. **Platform-defined schema** — skill tables are defined in platform code, like any other model
2. **Single database** — all tables in one DB, skill tables use `sk_` prefix for isolation
3. **Install = record** — installing a skill records in `skill_installations`, no DDL needed
4. **Skill API layer** — skills expose typed APIs for data access, no direct SQL from users
5. **Admin-managed marketplace** — admin publishes skills, controls visibility per user/role
6. **Per-user credentials** — encrypted, managed through skill API
7. **Skill-local models** — each skill defines its own tables in `skills/{name}/models.py`, not in `api/models.py`

## 4. Architecture Overview

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
│    skill_definitions, skill_permissions,                      │
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
| Core platform | (none) | `api/models.py` | `users`, `roles`, `sessions`, `agents` |
| Skill infrastructure | (none) | `api/models.py` | `skill_definitions`, `skill_installations`, `skill_permissions`, `user_credentials` |
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

## 5. Skill Package Structure

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

### 5.1 Manifest

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

### 5.2 Schema (Platform-Defined)

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

### 5.3 Skill API Layer

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

### 5.4 Skill Actions (What the Agent Calls)

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

## 6. Platform Tables

### 6.1 skill_definitions — marketplace catalog

> **Note**: `skill_definitions` is the marketplace catalog (what skills exist, their manifests).
> `skills_registry` (see `memory-and-context.md`) is the runtime registry tracking active versions
> and selection history. Both tables are needed — different purposes.

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

### 6.2 skill_installations — per-user install state

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

### 6.3 user_credentials — per-user encrypted secrets

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

### 6.4 skill_permissions — RBAC

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
  ├─ 2. Check dependencies: required skills installed?
  │     → query skill_installations
  │
  ├─ 3. Prompt for credentials (if required):
  │     → "GitHub skill requires a GitHub token. Please provide:"
  │     → encrypt and store in user_credentials
  │
  └─ 4. Record installation:
        → INSERT INTO skill_installations
```

No DDL execution. Tables already exist in platform DB (created by `init_db()`).

### 7.2 Uninstall Flow

```
User: "uninstall github skill"
  │
  ├─ 1. Check: any other skills depend on this?
  │
  ├─ 2. Mark as uninstalled in skill_installations
  │
  └─ 3. Delete credentials from user_credentials
```

No DROP TABLE. Skill data remains in platform DB. Admin can clean up orphaned data separately.

### 7.3 Upgrade Flow

```
Platform upgrades github skill v1.0.0 → v1.1.0
  │
  ├─ Schema change? (e.g. new column in sk_github_pr_cache)
  │   → Platform-level migration (same as any api/models.py change)
  │   → Applied by platform operator, not per-user
  │
  └─ No schema change? (code-only update)
      → Update skill_installations.skill_version
```

## 8. Runtime: How Skills Execute

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

No BYOD connection, no user DB pool. Skill API uses the same platform DB session.

## 9. Table Naming Convention

```
Core tables:           users, roles, sessions, conversation_events, agents
Skill infrastructure:  skill_definitions, skill_installations, skill_permissions, user_credentials
Skill business data:   sk_github_repos, sk_github_pr_cache
                       sk_knowledge_entries, sk_knowledge_relations
                       sk_jira_projects, sk_jira_issues
```

Pattern: `sk_{skill_name}_{table_name}`

Reserved prefix: `sk_` — only for skill business data tables.

## 10. Skill Permission Model

```
Admin publishes skill "github" to marketplace
  │
  ├─ Grant to role: all users with role "developer" can install
  ├─ Grant to user: only specific user can install
  └─ Public: all authenticated users can install
```

## 11. What Moves Out of api/models.py

| Current Location | Becomes | Skill |
|-----------------|---------|-------|
| `Repo` in `api/models.py` | `sk_github_repos` in `skills/github/models.py` | github |
| `core/repos/` | `skills/github/api.py` | github |
| `core/skills/github_client.py` | `skills/github/api.py` | github |
| `KnowledgeEntry` in `api/models.py` | `sk_knowledge_entries` in `skills/knowledge/models.py` | knowledge |
| `KnowledgeRelation` in `api/models.py` | `sk_knowledge_relations` in `skills/knowledge/models.py` | knowledge |
| `core/context/knowledge.py` | `skills/knowledge/api.py` | knowledge |

Tables that STAY in `api/models.py`:

| Table | Reason |
|-------|--------|
| `users`, `roles` | Identity (core) |
| `sessions`, `conversation_events` | Audit trail (core) |
| `agents` | Agent definitions (core) |
| `skill_definitions`, `skill_permissions`, `skill_installations`, `user_credentials` | Skill infrastructure (core) |
| `skill_selection_events` | Audit (core) |
| `context_snapshots`, `decisions` | Audit (core) |

## 12. init_db() — Skill Table Discovery

```python
# api/database.py

def init_db():
    """Create all tables — core + skill."""
    # Import skill models so they register with Base.metadata
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

## 13. Comparison with Industry

| Feature | ElizaOS | LangChain | MCP | **mo-agent-engine** |
|---------|---------|-----------|-----|---------------------|
| Skill has schema | ✅ plugin schema | ❌ | ❌ | ✅ platform-defined |
| Schema management | plugin owns | N/A | N/A | platform owns |
| Table namespace | ❌ bare names | ❌ | ❌ | ✅ `sk_{skill}_{table}` |
| Install lifecycle | ❌ | ❌ | ❌ | ✅ |
| Skill API layer | ❌ direct SQL | ❌ | ❌ | ✅ typed API |
| Marketplace + RBAC | ❌ | ❌ | ❌ | ✅ |
| Per-user credentials | ❌ env vars | ❌ env vars | ❌ | ✅ encrypted |
| Skill-local models | ❌ | ❌ | ❌ | ✅ `skills/{name}/models.py` |

## 14. Open Questions

1. **Cross-skill data access**: can knowledge skill read from github skill's tables?
   - Proposed: explicit dependency in manifest, platform provides cross-skill API
   - Since all tables are in the same DB, JOINs are trivial

2. **Schema evolution**: how to handle ALTER TABLE when platform upgrades a skill?
   - Same as any other migration — platform-level, applied by operator

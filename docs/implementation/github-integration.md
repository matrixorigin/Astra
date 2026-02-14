# GitHub Integration

## Purpose

Enable agents to perform GitHub operations (read code, review PRs, manage issues, check CI) as skills. All operations are logged as events for audit and replay.

## Architecture

```
Skills (PR Review, Issue Triage, CI Analysis, ...)
        │
        ▼
GitHubClient (unified interface)
  - Token resolution (priority fallback)
  - Rate limiting (per-token tracking)
  - Error handling (401 → deactivate token)
  - Audit logging (all API calls → events)
        │
        ▼
RepoRegistry + TokenResolver + PermissionManager
        │
        ▼
GitHub API (PyGithub)
```

Module: `core/repos/`

## Token Management

Tokens are resolved by priority:

1. Repo-specific token (most specific)
2. User-specific token
3. Account default token
4. Global default token

```python
# Token resolution
token = token_resolver.resolve(
    repo_url="https://github.com/org/repo",
    user_id="alice",
    operation="pull_request.read"
)
```

Tokens are stored encrypted in MatrixOne. Deactivated automatically on 401 responses.

## Repository Registry

```python
registry = RepoRegistry(db)
repo = registry.create(
    repo_url="https://github.com/org/repo",
    repo_type=RepoType.CODE,
    owner_id="alice",
    access_scope=AccessScope.WRITE,
    metadata={"default_branch": "main"}
)
```

## Operations

| Category | Operations |
|---|---|
| Repository | Clone, read files, search code, list branches |
| Pull Request | Create, review, comment, merge, list |
| Issues | Create, update, label, assign, close |
| CI/CD | Get workflow status, read logs, trigger re-run |
| Releases | Create, list, get assets |

All operations are implemented as skills with declared side-effect profiles. Read operations are safe for replay; write operations use recorded results during replay.

## Side-Effect Isolation

```python
# In production: real GitHub API call
# In replay: return recorded result (no real API call)
# In dry-run: validate parameters only
side_effect_profile = {
    "category": "write",
    "external_apis": ["github"],
    "mock_strategy": "recorded"
}
```

See [trust-and-safety.md §7](../design/trust-and-safety.md) for the full side-effect isolation design.

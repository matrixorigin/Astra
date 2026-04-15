# Authentication and Authorization

## Model

Two-layer separation:

1. **Application layer** (platform code): JWT authentication + resource ownership
2. **Database layer** (MatrixOne/user DB): database-native permissions for data operations

The platform does NOT implement RBAC. It does NOT query `mo_catalog.mo_user_grant`. Authorization is ownership-based: you can only operate on resources you created.

## Runtime Modes

- `ASTRA_AUTH_MODE=local_jwt` (default): astra issues and validates local JWTs via `/auth/login` and `/auth/refresh`.
- `ASTRA_AUTH_MODE=trusted_moi`: astra trusts externally issued Moi JWTs (`Authorization: Bearer ...`) and disables local auth endpoints (`/auth/register`, `/auth/login`, `/auth/refresh`, `/auth/logout` return 403).

## Authentication: JWT

```
Client → POST /auth/login → JWT (access_token + refresh_token)
Client → Authorization: Bearer <access_token> → API validates → extracts user_id
```

- Access token: 1 hour TTL
- Refresh token: 7 day TTL
- Stateless validation (no DB lookup per request)

## Authorization: Resource Ownership

Every resource has an owner:

```python
class Agent:
    owner_user_id: str

class Session:
    user_id: str

class Sandbox:
    user_id: str  # creator
```

Authorization check:

```python
def delete_agent(agent_id: str, user_id: str):
    agent = agent_repo.get(agent_id)
    if agent.owner_user_id != user_id:
        raise PermissionError("Can only delete your own agents")
    agent_repo.delete(agent_id)
```

No roles, no permission matrices, no RBAC queries.

## Database Layer Permissions

When the platform operates on a user's database (e.g., Sandbox creation via `CREATE CLONE`), the database's own permission system decides whether the operation is allowed:

```python
try:
    sandbox.create(name)  # → CREATE DATABASE ... CLONE ...
    audit.log(user_id, "sandbox_create", name, status="success")
except DatabaseError as e:
    # Database rejected — insufficient privileges
    audit.log(user_id, "sandbox_create", name, status="failed", error=str(e))
    raise
```

The platform does not pre-check database permissions. It executes and lets the database enforce. This avoids duplicating permission logic.

## Audit

All authentication and authorization events are logged:

- Login/logout/token refresh
- Resource creation/deletion
- Permission denials
- Database operation failures

Audit entries are events in `conversation_events` with `event_type = 'audit'`.

## What This Is NOT

- ❌ No MatrixOne **native** RBAC — roles live in app tables (`auth_roles`: `astra_admin`, `astra_user`)
- ❌ No role hierarchy
- ❌ No permission inheritance
- ❌ No cross-user resource sharing (future: team-level sharing via visibility flags)

The simplicity is intentional. Ownership-based authorization covers the current use cases. Role-based access can be added later if team/org features require it, but it should remain in the application layer, not coupled to database RBAC.

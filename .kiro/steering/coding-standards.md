---
inclusion: always
---

# Project-Specific Coding Standards

**Philosophy: Explicit is better than implicit. Testable is better than clever.**

---

## Type Annotations - 100% Required

**All functions must have type hints:**

```python
# ✅ Good: Full type annotations
def create_session(
    user_id: str,
    agent_id: str,
    metadata: dict[str, Any] | None = None
) -> Session:
    """Create a new session."""
    ...

# ❌ Bad: No type hints
def create_session(user_id, agent_id, metadata=None):
    ...

# ✅ Good: Complex types
from typing import Callable, Optional, Protocol

class SkillAPI(Protocol):
    def get_skill(self, skill_id: str) -> dict[str, Any]: ...

def install_skill(
    skill_id: str,
    api_client: SkillAPI,
    db_factory: Callable[[], Session]
) -> Skill:
    ...
```

**Type checking must pass:**
```bash
make type-check  # Must pass before commit
```

---

## Error Handling

### Use Specific Exception Types

```python
# ✅ Good: Custom exceptions for domain errors
class SkillNotFoundError(Exception):
    """Raised when skill does not exist."""
    pass

class PermissionDeniedError(Exception):
    """Raised when user lacks required permission."""
    pass

def install_skill(skill_id: str, user_id: str) -> Skill:
    if not skill_exists(skill_id):
        raise SkillNotFoundError(f"Skill {skill_id} not found")
    
    if not has_permission(user_id, "skill:install"):
        raise PermissionDeniedError(f"User {user_id} cannot install skills")
    
    return _do_install(skill_id)

# ❌ Bad: Generic exceptions
def install_skill(skill_id: str, user_id: str) -> Skill:
    if not skill_exists(skill_id):
        raise Exception("Not found")  # Too generic!
    
    if not has_permission(user_id, "skill:install"):
        raise Exception("No permission")  # Too generic!
```

### Never Swallow Exceptions

```python
# ❌ Bad: Silent failure
try:
    result = risky_operation()
except:
    pass  # Error lost!

# ❌ Bad: Logging without re-raising
try:
    result = risky_operation()
except Exception as e:
    logger.error(f"Error: {e}")
    # Error not propagated!

# ✅ Good: Log and re-raise
try:
    result = risky_operation()
except Exception as e:
    logger.error(f"Failed to perform operation: {e}", exc_info=True)
    raise  # Propagate error

# ✅ Good: Transform to domain exception
try:
    result = external_api_call()
except requests.HTTPError as e:
    raise SkillAPIError(f"Failed to fetch skill: {e}") from e
```

---

## Database Operations

### Always Use Context Managers

```python
# ✅ Good: Context manager ensures cleanup
def get_user(user_id: str) -> User | None:
    with SessionLocal() as db:
        return db.query(User).filter_by(id=user_id).first()

# ❌ Bad: Manual session management (leak risk)
def get_user(user_id: str) -> User | None:
    db = SessionLocal()
    user = db.query(User).filter_by(id=user_id).first()
    db.close()  # Might not execute if exception!
    return user
```

### Use Transactions for Multi-Step Operations

```python
# ✅ Good: Atomic transaction
def install_skill(skill_id: str, user_id: str) -> Skill:
    with SessionLocal() as db:
        try:
            # 1. Create installation record
            installation = SkillInstallation(skill_id=skill_id, user_id=user_id)
            db.add(installation)
            
            # 2. Update user permissions
            permission = SkillPermission(user_id=user_id, skill_id=skill_id)
            db.add(permission)
            
            # 3. Log event
            event = create_event(db, "skill_installed", ...)
            
            db.commit()
            return installation
        except Exception:
            db.rollback()
            raise

# ❌ Bad: No transaction, partial state on failure
def install_skill(skill_id: str, user_id: str) -> Skill:
    db = SessionLocal()
    installation = SkillInstallation(skill_id=skill_id, user_id=user_id)
    db.add(installation)
    db.commit()  # Committed!
    
    # If this fails, installation is already committed!
    permission = SkillPermission(user_id=user_id, skill_id=skill_id)
    db.add(permission)
    db.commit()
```

### Avoid N+1 Queries

```python
# ❌ Bad: N+1 query problem
def get_users_with_skills() -> list[dict]:
    users = db.query(User).all()
    result = []
    for user in users:  # 1 query
        skills = db.query(Skill).filter_by(user_id=user.id).all()  # N queries!
        result.append({"user": user, "skills": skills})
    return result

# ✅ Good: Single query with join
def get_users_with_skills() -> list[dict]:
    users = db.query(User).options(joinedload(User.skills)).all()
    return [{"user": user, "skills": user.skills} for user in users]
```

### MatrixOne-Specific: Don't Assume, Ask

**MatrixOne ≠ MySQL ≠ PostgreSQL. When something behaves unexpectedly:**
- Don't conclude "it's impossible" — check MatrixOne docs first
- Don't silently work around with weaker code — it might be a bug worth reporting
- Don't downgrade the design — if stuck, **ask the user** before compromising

---

## Event Sourcing - Mandatory Pattern

**All state changes must be logged as events:**

```python
# ✅ Good: Event-driven state change
def update_skill_status(
    skill_id: str,
    new_status: str,
    user_id: str,
    causal_chain_id: str
) -> Event:
    with SessionLocal() as db:
        # 1. Get current state
        skill = db.query(Skill).filter_by(id=skill_id).first()
        old_status = skill.status
        
        # 2. Log event BEFORE state change
        event = logger.create_event(
            event_type="skill_status_changed",
            skill_id=skill_id,
            old_status=old_status,
            new_status=new_status,
            user_id=user_id,
            causal_chain_id=causal_chain_id
        )
        
        # 3. Update state
        skill.status = new_status
        db.commit()
        
        return event

# ❌ Bad: Direct state change, no audit trail
def update_skill_status(skill_id: str, new_status: str) -> None:
    db.execute(f"UPDATE skills SET status = '{new_status}' WHERE id = '{skill_id}'")
    # No event logged! Audit trail broken!
```

**Causal chain must be propagated:**

```python
# ✅ Good: Propagate causal chain
def handle_user_request(user_id: str, message: str) -> Response:
    # Create root event
    user_event = logger.create_user_query(
        user_id=user_id,
        content=message,
        causal_chain_id=str(uuid.uuid4())  # New chain
    )
    
    # Propagate chain to child events
    llm_event = logger.create_llm_response(
        user_id=user_id,
        content=response_text,
        parent_event_id=user_event.event_id,
        causal_chain_id=user_event.causal_chain_id  # Same chain!
    )
    
    return Response(...)
```

---

## Dependency Injection - Required for Testability

```python
# ✅ Good: Dependencies injected
class SkillManager:
    def __init__(
        self,
        db_factory: Callable[[], Session],
        api_client: SkillAPI,
        logger: EventLogger
    ):
        self.db_factory = db_factory
        self.api_client = api_client
        self.logger = logger
    
    def install_skill(self, skill_id: str) -> Skill:
        # Fully testable with mock dependencies
        data = self.api_client.get_skill(skill_id)
        db = self.db_factory()
        ...

# ❌ Bad: Hardcoded dependencies
class SkillManager:
    def __init__(self):
        self.db = create_engine("postgresql://...")  # Hardcoded!
        self.api_client = HTTPClient()  # Can't mock!
    
    def install_skill(self, skill_id: str) -> Skill:
        # Impossible to test without real DB and API
        ...
```

---

## API Development

### Use Pydantic for Validation

```python
from pydantic import BaseModel, Field

# ✅ Good: Pydantic model with validation
class CreateSkillRequest(BaseModel):
    skill_id: str = Field(..., min_length=1, max_length=100)
    name: str = Field(..., min_length=1)
    version: str = Field(..., pattern=r"^\d+\.\d+\.\d+$")
    permissions: list[str] = Field(default_factory=list)

@app.post("/skills")
def create_skill(request: CreateSkillRequest) -> SkillResponse:
    # Validation automatic!
    skill = skill_manager.create_skill(
        skill_id=request.skill_id,
        name=request.name,
        version=request.version
    )
    return SkillResponse.from_orm(skill)

# ❌ Bad: Manual validation
@app.post("/skills")
def create_skill(request: dict) -> dict:
    # Manual validation, error-prone
    if not request.get("skill_id"):
        raise ValueError("skill_id required")
    if len(request["skill_id"]) > 100:
        raise ValueError("skill_id too long")
    ...
```

### Return Appropriate Status Codes

```python
from fastapi import HTTPException, status

# ✅ Good: Specific status codes
@app.get("/skills/{skill_id}")
def get_skill(skill_id: str) -> SkillResponse:
    try:
        skill = skill_manager.get_skill(skill_id)
        return SkillResponse.from_orm(skill)
    except SkillNotFoundError:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail=f"Skill {skill_id} not found"
        )
    except PermissionDeniedError:
        raise HTTPException(
            status_code=status.HTTP_403_FORBIDDEN,
            detail="Permission denied"
        )

# ❌ Bad: Generic 500 for everything
@app.get("/skills/{skill_id}")
def get_skill(skill_id: str) -> dict:
    skill = skill_manager.get_skill(skill_id)
    return skill  # Returns 500 on any error!
```

### Authentication Required

```python
from fastapi import Depends
from api.auth import get_current_user

# ✅ Good: Authentication enforced
@app.post("/skills")
def create_skill(
    request: CreateSkillRequest,
    current_user: User = Depends(get_current_user)  # Auth required!
) -> SkillResponse:
    skill = skill_manager.create_skill(
        skill_id=request.skill_id,
        user_id=current_user.id
    )
    return SkillResponse.from_orm(skill)

# ❌ Bad: No authentication
@app.post("/skills")
def create_skill(request: CreateSkillRequest) -> SkillResponse:
    # Anyone can create skills!
    ...
```

---

## Logging

### Structured Logging with Context

```python
from core.logging_config import get_logger

logger = get_logger(__name__)

# ✅ Good: Structured logging with context
def install_skill(skill_id: str, user_id: str) -> Skill:
    logger.info(
        "Installing skill",
        extra={
            "skill_id": skill_id,
            "user_id": user_id,
            "action": "skill_install"
        }
    )
    try:
        skill = _do_install(skill_id, user_id)
        logger.info(
            "Skill installed successfully",
            extra={
                "skill_id": skill_id,
                "user_id": user_id,
                "installation_id": skill.id
            }
        )
        return skill
    except Exception as e:
        logger.error(
            "Failed to install skill",
            extra={
                "skill_id": skill_id,
                "user_id": user_id,
                "error": str(e)
            },
            exc_info=True
        )
        raise

# ❌ Bad: Unstructured logging
def install_skill(skill_id: str, user_id: str) -> Skill:
    print(f"Installing {skill_id}")  # Don't use print!
    logger.info("Installing skill")  # No context!
```

### Never Log Sensitive Data

```python
# ❌ Bad: Logging sensitive data
logger.info(f"User login: {username}, password: {password}")
logger.info(f"API token: {api_token}")
logger.info(f"User email: {email}, SSN: {ssn}")

# ✅ Good: No sensitive data
logger.info(f"User login: {username}")
logger.info(f"API token: {api_token[:8]}...")  # Only prefix
logger.info(f"User authenticated: {user_id}")
```

---

## Documentation

### Docstrings for All Public Functions

```python
# ✅ Good: Complete docstring
def install_skill(
    skill_id: str,
    user_id: str,
    version: str | None = None
) -> Skill:
    """Install a skill for a user.
    
    Args:
        skill_id: Unique identifier of the skill to install
        user_id: ID of the user installing the skill
        version: Specific version to install (default: latest)
    
    Returns:
        Installed skill instance
    
    Raises:
        SkillNotFoundError: If skill does not exist
        PermissionDeniedError: If user lacks install permission
        VersionConflictError: If version conflicts with dependencies
    
    Example:
        >>> skill = install_skill("python-linter", "alice", "1.0.0")
        >>> print(skill.status)
        'installed'
    """
    ...

# ❌ Bad: No docstring
def install_skill(skill_id: str, user_id: str, version: str | None = None) -> Skill:
    ...
```

### Comment "Why", Not "What"

```python
# ✅ Good: Explains reasoning
def calculate_score(events: list[Event]) -> float:
    # Use exponential decay to prioritize recent events
    # Decay factor of 0.95 chosen empirically to balance recency vs history
    decay_factor = 0.95
    score = sum(event.value * (decay_factor ** i) for i, event in enumerate(events))
    return score

# ❌ Bad: States the obvious
def calculate_score(events: list[Event]) -> float:
    # Set decay factor to 0.95
    decay_factor = 0.95
    # Loop through events and calculate score
    score = sum(event.value * (decay_factor ** i) for i, event in enumerate(events))
    # Return the score
    return score
```

---

## Code Style

### Follow PEP 8

```bash
# Linting must pass
make lint

# Auto-fix formatting
make lint-fix
```

### Naming Conventions

```python
# Classes: PascalCase
class SkillManager:
    pass

# Functions/variables: snake_case
def install_skill(skill_id: str) -> Skill:
    user_permissions = get_permissions(user_id)
    ...

# Constants: UPPER_SNAKE_CASE
MAX_RETRIES = 3
DEFAULT_TIMEOUT = 30

# Private: prefix with _
class SkillManager:
    def _validate_permissions(self, user_id: str) -> bool:
        ...
```

### Import Order

```python
# 1. Standard library
import os
import sys
from datetime import datetime

# 2. Third-party
import pytest
from fastapi import FastAPI
from sqlalchemy import create_engine

# 3. Local
from api.models import User, Skill
from core.logging_config import get_logger
```

---

## Performance

### Use Generators for Large Datasets

```python
# ✅ Good: Generator (memory efficient)
def get_all_events() -> Generator[Event, None, None]:
    with SessionLocal() as db:
        for event in db.query(Event).yield_per(1000):
            yield event

# ❌ Bad: Load all into memory
def get_all_events() -> list[Event]:
    with SessionLocal() as db:
        return db.query(Event).all()  # OOM for large tables!
```

### Pagination for API Endpoints

```python
# ✅ Good: Paginated response
@app.get("/events")
def list_events(
    skip: int = 0,
    limit: int = 100
) -> list[EventResponse]:
    events = db.query(Event).offset(skip).limit(limit).all()
    return [EventResponse.from_orm(e) for e in events]

# ❌ Bad: Return all records
@app.get("/events")
def list_events() -> list[EventResponse]:
    events = db.query(Event).all()  # Could be millions!
    return [EventResponse.from_orm(e) for e in events]
```

---

## Security

### No SQL Injection

```python
# ✅ Good: ORM or parameterized queries
def get_user(user_id: str) -> User | None:
    return db.query(User).filter_by(id=user_id).first()

# ❌ Bad: String interpolation (SQL injection!)
def get_user(user_id: str) -> User | None:
    query = f"SELECT * FROM users WHERE id = '{user_id}'"
    return db.execute(query).first()
```

### Permission Checks

```python
# ✅ Good: Permission check before action
def delete_skill(skill_id: str, user_id: str) -> None:
    if not has_permission(user_id, "skill:delete"):
        raise PermissionDeniedError()
    
    db.delete(Skill, id=skill_id)

# ❌ Bad: No permission check
def delete_skill(skill_id: str) -> None:
    db.delete(Skill, id=skill_id)  # Anyone can delete!
```

---

## Pre-Commit Checklist

Before every commit:
- [ ] Type checking passes: `make type-check`
- [ ] Linting passes: `make lint`
- [ ] Tests pass: `pytest -n auto -W error`
- [ ] No debug print statements
- [ ] No commented-out code
- [ ] Docstrings added for public functions
- [ ] Type hints on all functions
- [ ] Error handling uses specific exceptions
- [ ] Database sessions use context managers
- [ ] State changes logged as events

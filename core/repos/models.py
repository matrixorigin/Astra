"""Repository models."""

from datetime import datetime
from enum import Enum
from typing import Any

from pydantic import BaseModel, Field


class RepoType(str, Enum):
    """Repository type."""

    CODE = "code"
    CI = "ci"
    TESTER = "tester"
    DOCS = "docs"


class AccessScope(str, Enum):
    """Access scope for repository."""

    READ = "read"
    WRITE = "write"
    ADMIN = "admin"


class OwnerType(str, Enum):
    """Owner type."""

    USER = "user"
    TENANT = "tenant"


class Repo(BaseModel):
    """Repository model."""

    repo_id: str
    repo_url: str
    repo_type: RepoType
    owner_id: str
    owner_type: OwnerType
    repo_group: str | None = None
    token_id: str | None = None
    access_scope: AccessScope
    metadata: dict[str, Any] = Field(default_factory=dict)
    is_active: bool = True
    created_at: datetime
    updated_at: datetime

    model_config = {"from_attributes": True}

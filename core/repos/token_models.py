"""Token management models."""

from datetime import datetime
from enum import Enum

from pydantic import BaseModel, Field


class TokenType(str, Enum):
    """Token type."""

    REPO = "repo"
    LLM = "llm"


class Token(BaseModel):
    """Token model."""

    token_id: str
    token_type: TokenType
    provider: str | None = None  # 'github', 'openai', 'groq'
    scope_user_id: str | None = None
    scope_tenant_id: str | None = None
    scope_repo: str | None = None
    secret_ref: str | None = None  # Vault path (preferred)
    encrypted_value: str | None = None  # Fallback
    is_active: bool = True
    expires_at: datetime | None = None
    created_at: datetime
    metadata: dict = Field(default_factory=dict)

    model_config = {"from_attributes": True}

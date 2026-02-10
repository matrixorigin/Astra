"""Token management models."""

from datetime import datetime
from enum import Enum
from typing import Optional

from pydantic import BaseModel, Field


class TokenType(str, Enum):
    """Token type."""

    REPO = "repo"
    LLM = "llm"


class Token(BaseModel):
    """Token model."""

    token_id: str
    token_type: TokenType
    provider: Optional[str] = None  # 'github', 'openai', 'groq'
    scope_user_id: Optional[str] = None
    scope_tenant_id: Optional[str] = None
    scope_repo: Optional[str] = None
    secret_ref: Optional[str] = None  # Vault path (preferred)
    encrypted_value: Optional[str] = None  # Fallback
    is_active: bool = True
    expires_at: Optional[datetime] = None
    created_at: datetime
    metadata: dict = Field(default_factory=dict)

    model_config = {"from_attributes": True}

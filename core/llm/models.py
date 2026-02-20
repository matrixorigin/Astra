"""LLM client models."""

from datetime import datetime
from enum import Enum
from typing import Any

from pydantic import BaseModel, Field


class LLMProvider(str, Enum):
    """LLM provider."""

    OPENAI = "openai"
    GROQ = "groq"
    ANTHROPIC = "anthropic"
    SELF_HOSTED = "self_hosted"


class LLMMessage(BaseModel):
    """LLM message."""

    role: str  # 'system' | 'user' | 'assistant' | 'tool'
    content: str | None = None
    tool_calls: list[dict[str, Any]] | None = None
    tool_call_id: str | None = None
    name: str | None = None


class LLMRequest(BaseModel):
    """LLM request."""

    messages: list[LLMMessage]
    model: str
    temperature: float = 0.7
    max_tokens: int | None = None
    metadata: dict[str, Any] = Field(default_factory=dict)


class LLMResponse(BaseModel):
    """LLM response."""

    content: str
    model: str
    provider: LLMProvider
    tokens_prompt: int
    tokens_completion: int
    tokens_total: int
    latency_ms: int
    cost_usd: float
    # Prompt caching stats (Anthropic cache_read/cache_creation; OpenAI cached_tokens)
    cache_read_tokens: int = 0
    cache_creation_tokens: int = 0
    metadata: dict[str, Any] = Field(default_factory=dict)


class LLMCallLog(BaseModel):
    """LLM call log."""

    log_id: str
    event_id: str
    user_id: str
    provider: LLMProvider
    model: str
    tokens_prompt: int
    tokens_completion: int
    tokens_total: int
    cost_usd: float
    latency_ms: int
    status: str  # 'success' | 'failed'
    error_message: str | None = None
    created_at: datetime
    metadata: dict[str, Any] = Field(default_factory=dict)

    model_config = {"from_attributes": True}

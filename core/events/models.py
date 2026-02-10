"""Event models and types.

Defines the data structures for conversation events following the event-centric design.
"""

from datetime import datetime, timezone
from enum import Enum
from typing import Any

from pydantic import BaseModel, Field


class EventType(str, Enum):
    """Event types in the conversation flow."""

    USER_QUERY = "user_query"
    LLM_REQUEST = "llm_request"
    LLM_RESPONSE = "llm_response"
    TOOL_CALL = "tool_call"
    TOOL_RESULT = "tool_result"
    SYSTEM_MESSAGE = "system_message"
    MULTI_AGENT_MESSAGE = "multi_agent_message"


class TokenUsage(BaseModel):
    """Token usage statistics."""

    prompt: int = Field(description="Prompt tokens")
    completion: int = Field(description="Completion tokens")
    total: int = Field(description="Total tokens")


class ContextSnapshot(BaseModel):
    """Context snapshot for reproducibility.

    Captures the state needed to reproduce an LLM call.
    """

    prompt_template_id: str | None = Field(
        default=None, description="Prompt template ID and version"
    )
    skills_used: list[str] | None = Field(
        default=None, description="Skills used in this interaction"
    )
    history_events: list[str] | None = Field(
        default=None, description="Event IDs included in context"
    )
    retrieved_chunks: list[str] | None = Field(default=None, description="RAG chunk IDs retrieved")


class ConversationEvent(BaseModel):
    """Conversation event model.

    Represents a single atomic event in the conversation flow.
    Based on the event-centric design in context-memory-session-and-tables.md §4.1
    """

    event_id: str = Field(description="ULID, globally unique and sortable")
    user_id: str = Field(description="User identifier")
    session_id: str = Field(description="Session identifier")
    agent_id: str = Field(description="Agent type (e.g., dev-agent)")
    agent_version: str = Field(description="Agent code/config version")
    event_type: EventType = Field(description="Event type")
    content: str = Field(description="Original content")
    desensitized_content: str | None = Field(
        default=None, description="Desensitized version for compliance"
    )
    metadata: dict[str, Any] | None = Field(
        default=None, description="Namespace convention: dev.*, chat.*, etc."
    )
    context_snapshot: ContextSnapshot | None = Field(
        default=None, description="Reproducibility snapshot"
    )
    token_usage: TokenUsage | None = Field(default=None, description="Token usage statistics")
    embedding_ref: str | None = Field(default=None, description="External vector store chunk ID")
    created_at: datetime = Field(default_factory=lambda: datetime.now(timezone.utc))
    prompt_template_id: str | None = Field(default=None, description="References prompt_templates")
    skills_snapshot: list[dict[str, Any]] | None = Field(
        default=None, description="Skills used with versions"
    )
    quality_score: float | None = Field(default=None, description="System pre-score (0-5)")
    is_flagged: bool = Field(default=False, description="Flagged for review")
    training_eligible: bool = Field(default=False, description="Eligible for training pipeline")
    parent_event_id: str | None = Field(
        default=None, description="Immediate prior event in causal chain"
    )
    causal_chain_id: str | None = Field(
        default=None, description="Groups one user query + full LLM/tool chain"
    )
    llm_model_used: str | None = Field(
        default=None, description="Model identifier at inference time"
    )
    llm_params: dict[str, Any] | None = Field(
        default=None, description="LLM parameters (temperature, max_tokens, etc.)"
    )

    model_config = {"use_enum_values": True}

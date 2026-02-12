"""Pydantic schemas for agents."""

from pydantic import BaseModel, Field


class AgentCreateRequest(BaseModel):
    """Agent creation request."""

    agent_name: str = Field(..., min_length=1, max_length=255)
    agent_type: str = Field(default="chatbot", pattern="^(chatbot|assistant|workflow|custom)$")
    config: dict | None = None


class AgentUpdateRequest(BaseModel):
    """Agent update request."""

    agent_name: str | None = Field(None, min_length=1, max_length=255)
    config: dict | None = None
    is_active: bool | None = None


class AgentResponse(BaseModel):
    """Agent response."""

    agent_id: str
    agent_name: str
    agent_type: str
    owner_user_id: str
    config: dict | None = None
    is_active: bool = True

"""Pydantic schemas for events."""

from datetime import datetime

from pydantic import BaseModel, Field


class EventCreateRequest(BaseModel):
    """Request to create an event."""

    session_id: str
    event_type: str = Field(..., description="Event type: user_query, llm_response, tool_call, etc.")
    content: str
    metadata: dict | None = None


class EventResponse(BaseModel):
    """Event response."""

    event_id: str
    session_id: str
    user_id: str
    event_type: str
    content: str
    created_at: datetime
    metadata: dict | None = None
    parent_event_id: str | None = None
    causal_chain_id: str | None = None


class EventListResponse(BaseModel):
    """List of events response."""

    events: list[EventResponse]
    total: int

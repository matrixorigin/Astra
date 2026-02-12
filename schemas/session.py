"""Pydantic schemas for sessions."""

from datetime import datetime

from pydantic import BaseModel, Field


class SessionCreateRequest(BaseModel):
    """Request to create a session."""

    metadata: dict | None = Field(default=None, description="Optional session metadata")


class SessionResponse(BaseModel):
    """Session response."""

    session_id: str
    user_id: str
    status: str
    event_count: int
    created_at: datetime
    last_active_at: datetime
    metadata: dict | None = None


class SessionListResponse(BaseModel):
    """List of sessions response."""

    sessions: list[SessionResponse]
    total: int

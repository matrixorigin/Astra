"""Session models and types.

Defines the data structures for session management.
"""

from datetime import datetime, timezone
from enum import Enum
from typing import Any

from pydantic import BaseModel, Field


class SessionStatus(str, Enum):
    """Session status."""

    ACTIVE = "active"
    IDLE = "idle"
    CLOSED = "closed"


class SummaryStatus(str, Enum):
    """Summary generation status."""

    PENDING = "pending"
    COMPLETED = "completed"
    FAILED = "failed"


class Session(BaseModel):
    """Session model.

    Represents a conversation session with lifecycle management.
    Based on design document §4.2
    """

    session_id: str = Field(description="Session identifier")
    user_id: str = Field(description="User identifier")
    created_at: datetime = Field(default_factory=lambda: datetime.now(timezone.utc))
    updated_at: datetime = Field(default_factory=lambda: datetime.now(timezone.utc))
    last_active_at: datetime | None = Field(default=None, description="Last activity timestamp")
    status: SessionStatus = Field(default=SessionStatus.ACTIVE)
    last_event_id: str | None = Field(default=None, description="Last event in session")
    event_count: int = Field(default=0, description="Number of events in session")
    summary_status: SummaryStatus | None = Field(
        default=None, description="Summary generation status"
    )
    summary_job_id: str | None = Field(default=None, description="Summary job identifier")
    vector_db_snapshot_id: str | None = Field(
        default=None, description="Vector store snapshot reference"
    )
    metadata: dict[str, Any] | None = Field(default=None, description="Additional metadata")

    model_config = {"use_enum_values": True}

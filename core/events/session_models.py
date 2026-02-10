"""Session models and types.

Defines the data structures for session management.
"""

from datetime import UTC, datetime
from enum import Enum
from typing import Any, Optional

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
    tenant_id: Optional[str] = Field(default=None, description="Tenant identifier")
    created_at: datetime = Field(default_factory=lambda: datetime.now(UTC))
    updated_at: datetime = Field(default_factory=lambda: datetime.now(UTC))
    last_active_at: Optional[datetime] = Field(
        default=None, description="Last activity timestamp"
    )
    status: SessionStatus = Field(default=SessionStatus.ACTIVE)
    last_event_id: Optional[str] = Field(
        default=None, description="Last event in session"
    )
    event_count: int = Field(default=0, description="Number of events in session")
    summary_status: Optional[SummaryStatus] = Field(
        default=None, description="Summary generation status"
    )
    summary_job_id: Optional[str] = Field(
        default=None, description="Summary job identifier"
    )
    vector_db_snapshot_id: Optional[str] = Field(
        default=None, description="Vector store snapshot reference"
    )
    metadata: Optional[dict[str, Any]] = Field(
        default=None, description="Additional metadata"
    )

    model_config = {"use_enum_values": True}

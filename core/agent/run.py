"""AgentRun — durable unit of agent work, persisted as events."""

import json
from datetime import datetime, timezone
from enum import Enum
from uuid import uuid4

from pydantic import BaseModel, Field


class RunStatus(str, Enum):
    PENDING = "pending"
    RUNNING = "running"
    WAITING = "waiting"
    COMPLETED = "completed"
    FAILED = "failed"
    CANCELLED = "cancelled"


class RunTrigger(str, Enum):
    USER_MESSAGE = "user_message"
    JOB_COMPLETED = "job_completed"
    CHILD_RUN_COMPLETED = "child_run_completed"
    WEBHOOK = "webhook"
    SCHEDULE = "schedule"


class AgentRun(BaseModel):
    """A durable unit of agent work. State is persisted as agent_events."""

    run_id: str = Field(default_factory=lambda: str(uuid4()))
    session_id: str
    user_id: str
    agent_id: str = "dev-agent"
    status: RunStatus = RunStatus.PENDING
    trigger: RunTrigger = RunTrigger.USER_MESSAGE
    trigger_event_id: str | None = None

    # Async wait state
    waiting_for: str | None = None  # "job:<id>" | "run:<id>" | "webhook:<id>"

    # Plan tracking
    plan_id: str | None = None
    current_step: str | None = None

    # Parent-child
    parent_run_id: str | None = None

    # Priority (0=highest, 10=lowest, default 5)
    priority: int = 5

    # Timing
    created_at: datetime = Field(default_factory=lambda: datetime.now(timezone.utc))
    completed_at: datetime | None = None

    # Input
    user_input: str = ""
    context: dict | None = None

    def to_event_content(self) -> str:
        return self.model_dump_json(exclude_none=True)

    @classmethod
    def from_event_content(cls, content: str) -> "AgentRun":
        return cls.model_validate_json(content)

"""Workflow, run, trigger, and streaming models."""

from sqlalchemy import (
    JSON, Column, DateTime, Integer, SmallInteger, String, Text, UniqueConstraint,
)
from sqlalchemy.sql import func

from api.base import Base


class WorkflowDefinition(Base):
    """Registered workflow templates — versioned, reusable."""
    __tablename__ = "workflow_definitions"
    workflow_id = Column(String(255), primary_key=True)
    name = Column(String(255), nullable=False, index=True)
    version = Column(String(32), nullable=False)
    description = Column(Text)
    definition = Column(JSON, nullable=False)
    created_by = Column(String(255))
    is_active = Column(SmallInteger, default=1)
    created_at = Column(DateTime, default=func.now())
    updated_at = Column(DateTime, default=func.now(), onupdate=func.now())


class WorkflowRun(Base):
    """Runtime state of a workflow execution."""
    __tablename__ = "workflow_runs"
    run_id = Column(String(255), primary_key=True)
    workflow_id = Column(String(255), nullable=False, index=True)
    agent_run_id = Column(String(255), index=True)
    status = Column(String(32), nullable=False, default="pending")
    waiting_for = Column(String(255))
    waiting_step_id = Column(String(255))
    current_step_idx = Column(Integer, default=0)
    step_results = Column(JSON, default=dict)
    inputs = Column(JSON, default=dict)
    error = Column(Text)
    created_by = Column(String(255))
    started_at = Column(DateTime, default=func.now())
    completed_at = Column(DateTime)
    updated_at = Column(DateTime, default=func.now(), onupdate=func.now())


class RunEvent(Base):
    """Persisted SSE events for cross-worker streaming."""
    __tablename__ = "run_events"
    __table_args__ = (
        UniqueConstraint("run_id", "idx", name="uq_run_event_run_idx"),
    )
    id = Column(Integer, primary_key=True, autoincrement=True)
    run_id = Column(String(255), nullable=False, index=True)
    idx = Column(Integer, nullable=False)
    event_type = Column(String(64), nullable=False)
    data = Column(JSON, nullable=False)
    event_id = Column(String(255))
    agent_id = Column(String(255))
    created_at = Column(DateTime, default=func.now())


class Trigger(Base):
    """Webhook or cron trigger that creates AgentRuns."""
    __tablename__ = "triggers"
    trigger_id = Column(String(255), primary_key=True)
    user_id = Column(String(255), nullable=False, index=True)
    agent_id = Column(String(255), nullable=False)
    trigger_type = Column(String(32), nullable=False)
    name = Column(String(255), nullable=False)
    user_input = Column(Text, nullable=False)
    context = Column(JSON)
    cron_expr = Column(String(128))
    secret = Column(String(255))
    session_id = Column(String(255))
    next_fire_at = Column(DateTime)
    is_active = Column(SmallInteger, default=1)
    created_at = Column(DateTime, default=func.now())

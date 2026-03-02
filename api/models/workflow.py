"""Workflow, trigger models."""

from sqlalchemy import (
    Column, DateTime, Integer, SmallInteger, String, Text,
)
from sqlalchemy.sql import func

from api.base import Base
from api.models._types import NullableJSON as JSON


class WorkflowDefinition(Base):
    __tablename__ = "wf_definitions"
    workflow_id = Column(String(255), primary_key=True)
    name = Column(String(255), nullable=False, index=True)
    version = Column(String(32), nullable=False)
    description = Column(Text)
    definition = Column(JSON, nullable=False)
    created_by = Column(String(255))
    is_active = Column(SmallInteger, default=1, server_default="1")
    created_at = Column(DateTime, default=func.now())
    updated_at = Column(DateTime, default=func.now(), onupdate=func.now())


class WorkflowRun(Base):
    __tablename__ = "wf_runs"
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


class Trigger(Base):
    __tablename__ = "wf_triggers"
    trigger_id = Column(String(255), primary_key=True)
    user_id = Column(String(36), nullable=False, index=True)
    agent_id = Column(String(64), nullable=False)
    trigger_type = Column(String(32), nullable=False)
    name = Column(String(255), nullable=False)
    user_input = Column(Text, nullable=False)
    context = Column(JSON)
    cron_expr = Column(String(128))
    secret = Column(String(255))
    session_id = Column(String(36))
    next_fire_at = Column(DateTime)
    is_active = Column(SmallInteger, default=1, server_default="1")
    created_at = Column(DateTime, default=func.now())

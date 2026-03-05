"""Agent, session, event, working-memory, and run-event models."""

from matrixone import VectorPrecision, VectorType
from matrixone.sqlalchemy_ext import FulltextIndex, FulltextParserType
from sqlalchemy import (
    Column, DateTime, Float, Index, Integer, SmallInteger, String, Text,
    UniqueConstraint,
)
from sqlalchemy.sql import func

from api.base import Base
from api.models._constants import EMBEDDING_DIM
from api.models._types import NullableJSON as JSON


class Agent(Base):
    __tablename__ = "agent_agents"
    agent_id = Column(String(36), primary_key=True)
    agent_name = Column(String(100), nullable=False)
    agent_type = Column(String(50), nullable=False)
    owner_user_id = Column(String(36), nullable=False, index=True)
    agent_config = Column("agent_config", JSON)
    data_source = Column(JSON)
    is_active = Column(SmallInteger, server_default="1", nullable=False)
    created_at = Column(DateTime, default=func.now(), nullable=False)
    updated_at = Column(DateTime, default=func.now(), onupdate=func.now())


class Session(Base):
    __tablename__ = "agent_sessions"
    __table_args__ = (
        Index("idx_sessions_user_status", "user_id", "status"),
    )

    session_id = Column(String(36), primary_key=True)
    user_id = Column(String(36), nullable=False)
    agent_id = Column(String(36), nullable=True, index=True)
    title = Column(String(255), nullable=True)
    status = Column(String(20), default="active", nullable=False)
    event_count = Column(Integer, default=0, nullable=False)
    last_event_id = Column(String(36), nullable=True)
    summary_status = Column(String(20), nullable=True)
    summary_job_id = Column(String(36), nullable=True)
    vector_db_snapshot_id = Column(String(64), nullable=True)
    created_at = Column(DateTime, default=func.now(), nullable=False)
    updated_at = Column(DateTime, default=func.now(), onupdate=func.now())
    ended_at = Column(DateTime, nullable=True)
    last_active_at = Column(DateTime, default=func.now(), nullable=False)
    session_metadata = Column("metadata", JSON)


class Event(Base):
    __tablename__ = "agent_events"
    __table_args__ = (
        FulltextIndex("ft_content_session", ["content", "session_id"], parser=FulltextParserType.NGRAM),
        Index("idx_events_user_type_time", "user_id", "event_type", "created_at"),
        Index("idx_events_session_time", "session_id", "created_at"),
        Index("idx_events_parent_type", "parent_event_id", "event_type"),
        Index("idx_events_chain_time", "causal_chain_id", "created_at"),
        Index("idx_events_run_type", "run_id", "event_type"),
    )

    event_id = Column(String(36), primary_key=True)
    session_id = Column(String(36), nullable=False)
    user_id = Column(String(36), nullable=False)
    agent_id = Column(String(36), nullable=False, default="system")
    agent_version = Column(String(32), nullable=False, default="1.0.0")
    event_type = Column(String(50), nullable=False)
    content = Column(Text, nullable=False)
    parent_event_id = Column(String(36), nullable=True)
    causal_chain_id = Column(String(36), nullable=False)
    desensitized_content = Column(Text)
    created_at = Column(DateTime, default=func.now(), nullable=False)
    event_metadata = Column("metadata", JSON)
    context_snapshot = Column(JSON)
    token_usage = Column(JSON)
    embedding_ref = Column(String(128))
    embedding = Column(VectorType(EMBEDDING_DIM, VectorPrecision.F32))
    prompt_template_id = Column(String(64))
    skills_snapshot = Column(JSON)
    quality_score = Column(Float)
    is_flagged = Column(SmallInteger, server_default="0")
    training_eligible = Column(SmallInteger, server_default="0")
    llm_model_used = Column(String(50))
    llm_params = Column(JSON)
    skill_name = Column(String(255))
    skill_version = Column(String(32))
    skill_result = Column(JSON)
    run_id = Column(String(36), index=True)
    parent_run_id = Column(String(36), index=True)
    waiting_for = Column(String(255), index=True)
    dedup_key = Column(String(255), index=True)


class AgentScratchpad(Base):
    """Working memory: structured notes for long-horizon tasks."""
    __tablename__ = "agent_scratchpads"
    __table_args__ = (
        Index('idx_scratchpad_session', 'session_id'),
        Index('idx_scratchpad_user', 'user_id'),
        Index('idx_scratchpad_type', 'note_type'),
    )

    note_id = Column(String(64), primary_key=True)
    session_id = Column(String(36), nullable=False)
    user_id = Column(String(36), nullable=False)
    agent_id = Column(String(64))
    note_type = Column(String(50), nullable=False)
    content = Column(Text, nullable=False)
    status = Column(String(20), default="active")
    related_event_ids = Column(JSON)
    related_note_ids = Column(JSON)
    created_at = Column(DateTime, default=func.now())
    updated_at = Column(DateTime, default=func.now(), onupdate=func.now())


class RunEvent(Base):
    """Persisted SSE events for cross-worker streaming."""
    __tablename__ = "agent_run_events"
    __table_args__ = (
        UniqueConstraint("run_id", "idx", name="uq_run_event_run_idx"),
    )
    id = Column(Integer, primary_key=True, autoincrement=True)
    run_id = Column(String(36), nullable=False, index=True)
    idx = Column(Integer, nullable=False)
    event_type = Column(String(64), nullable=False)
    data = Column(JSON, nullable=False)
    event_id = Column(String(36))
    agent_id = Column(String(64))
    created_at = Column(DateTime, default=func.now())

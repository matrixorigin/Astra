"""Evaluation, quality, feedback, and training models."""

from sqlalchemy import (
    Column, DateTime, Float, Index, Integer, SmallInteger, String, Text, UniqueConstraint,
)
from sqlalchemy.sql import func

from api.base import Base
from api.models._types import NullableJSON as JSON


class QualityAssessment(Base):
    __tablename__ = "eval_quality_assessments"
    __table_args__ = (
        UniqueConstraint("level", "target_id", name="uq_level_target"),
        Index("ix_qa_session_level", "session_id", "level"),
    )

    assessment_id = Column(String(36), primary_key=True)
    level = Column(String(10), nullable=False)
    target_id = Column(String(36), nullable=False)
    session_id = Column(String(36), nullable=False)
    score = Column(Float, nullable=False)
    step_count = Column(Integer, nullable=False, default=0)
    failure_count = Column(Integer, nullable=False, default=0)
    details = Column(JSON)
    created_at = Column(DateTime, default=func.now(), nullable=False)
    updated_at = Column(DateTime, default=func.now(), onupdate=func.now())


class GateResult(Base):
    __tablename__ = "eval_gate_results"

    gate_id = Column(String(36), primary_key=True)
    change_type = Column(String(20), nullable=False)
    change_id = Column(String(128), nullable=False)
    snapshot_used = Column(String(64))
    sessions_tested = Column(Integer, default=0)
    error_rate = Column(Float, default=0.0)
    score_delta = Column(Float, default=0.0)
    passed = Column(SmallInteger, nullable=False)
    metrics = Column(Text)
    created_at = Column(DateTime, default=func.now())


class LLMFeedback(Base):
    __tablename__ = "eval_llm_feedback"
    feedback_id = Column(String(36), primary_key=True)
    prompt_template_id = Column(String(64), nullable=False, index=True)
    prompt_version = Column(String(32), nullable=False)
    llm_request_id = Column(String(64), index=True)
    rating = Column(Integer)
    comment = Column(Text)
    feedback_metadata = Column("metadata", JSON)
    created_at = Column(DateTime, default=func.now())


class LLMCallLog(Base):
    __tablename__ = "eval_llm_call_logs"
    log_id = Column(String(36), primary_key=True)
    event_id = Column(String(36), index=True)
    user_id = Column(String(36), index=True)
    provider = Column(String(50))
    model = Column(String(50))
    tokens_prompt = Column(Integer)
    tokens_completion = Column(Integer)
    tokens_total = Column(Integer)
    cost_usd = Column(Float)
    latency_ms = Column(Integer)
    status = Column(String(20))
    error_message = Column(Text)
    created_at = Column(DateTime, default=func.now(), index=True)
    call_metadata = Column("metadata", JSON)


class UserFeedback(Base):
    __tablename__ = "eval_user_feedback"
    __table_args__ = (
        Index("ix_feedback_agent_created", "agent_id", "created_at"),
    )
    feedback_id = Column(String(64), primary_key=True)
    user_id = Column(String(36), nullable=False, index=True)
    agent_id = Column(String(64), index=True)
    session_id = Column(String(36))
    event_id = Column(String(64))
    rating = Column(Integer)
    feedback_type = Column(String(32))
    comment = Column(Text)
    created_at = Column(DateTime, default=func.now())


class TrainingData(Base):
    __tablename__ = "eval_training_data"
    data_id = Column(String(36), primary_key=True)
    session_id = Column(String(36), nullable=False, index=True)
    input_text = Column(Text, nullable=False)
    output_text = Column(Text, nullable=False)
    quality = Column(String(20), nullable=False)
    contamination_score = Column(Float, nullable=False, default=0.0)
    content_hash = Column(String(64), unique=True, nullable=False)
    created_at = Column(DateTime, default=func.now())

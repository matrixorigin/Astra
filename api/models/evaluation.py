"""Evaluation, quality, feedback, and training models."""

from sqlalchemy import (
    JSON, Column, DateTime, Float, Index, Integer, SmallInteger, String, Text, UniqueConstraint,
)
from sqlalchemy.sql import func

from api.base import Base


class QualityAssessment(Base):
    """Multi-level quality assessment (chain / session)."""
    __tablename__ = "quality_assessments"
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
    """Unified gate validation results for all change types."""
    __tablename__ = "gate_results"

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
    __tablename__ = "llm_feedback"
    feedback_id = Column(String(36), primary_key=True)
    prompt_template_id = Column(String(64), nullable=False, index=True)
    prompt_version = Column(String(32), nullable=False)
    llm_request_id = Column(String(64), index=True)
    rating = Column(Integer)
    comment = Column(Text)
    feedback_metadata = Column("metadata", JSON)
    created_at = Column(DateTime, default=func.now())


class LLMCallLog(Base):
    __tablename__ = "llm_call_logs"
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
    created_at = Column(DateTime, default=func.now())
    call_metadata = Column("metadata", JSON)


class UserFeedback(Base):
    __tablename__ = "user_feedback"
    __table_args__ = (
        Index("ix_feedback_agent_created", "agent_id", "created_at"),
    )
    feedback_id = Column(String(64), primary_key=True)
    user_id = Column(String(255), nullable=False, index=True)
    agent_id = Column(String(255), index=True)
    session_id = Column(String(64))
    event_id = Column(String(64))
    rating = Column(Integer)
    feedback_type = Column(String(32))
    comment = Column(Text)
    created_at = Column(DateTime, default=func.now())


class TrainingData(Base):
    """Training data extracted from sessions with quality filtering."""
    __tablename__ = "training_data"
    data_id = Column(String(36), primary_key=True)
    session_id = Column(String(36), nullable=False, index=True)
    input_text = Column(Text, nullable=False)
    output_text = Column(Text, nullable=False)
    quality = Column(String(20), nullable=False)
    contamination_score = Column(Float, nullable=False, default=0.0)
    content_hash = Column(String(64), unique=True, nullable=False)
    created_at = Column(DateTime, default=func.now())


class ModelQualityMetric(Base):
    """Model routing quality and cost tracking."""
    __tablename__ = "model_quality_metrics"
    metric_id = Column(String(36), primary_key=True)
    task_type = Column(String(50), nullable=False, index=True)
    model = Column(String(100), nullable=False, index=True)
    quality_score = Column(Float, nullable=False)
    cost = Column(Float, nullable=False)
    recorded_at = Column(DateTime, default=func.now())


class AdversarialAttack(Base):
    """Adversarial attack evaluation results."""
    __tablename__ = "adversarial_attacks"
    attack_id = Column(String(36), primary_key=True)
    agent_id = Column(String(36), nullable=False, index=True)
    attack_type = Column(String(50), nullable=False)
    success = Column(SmallInteger, nullable=False)
    severity = Column(String(20), nullable=False)
    evidence = Column(Text)
    recorded_at = Column(DateTime, default=func.now())


class ModelArtifact(Base):
    """Trained model artifacts with versioning and lifecycle."""
    __tablename__ = "model_artifacts"
    artifact_id = Column(String(36), primary_key=True)
    model_name = Column(String(128), nullable=False, index=True)
    version = Column(String(32), nullable=False)
    base_model = Column(String(128))
    artifact_path = Column(Text, nullable=False)
    artifact_format = Column(String(32), default="onnx")
    metrics = Column(JSON)
    training_config = Column(JSON)
    dataset_size = Column(Integer)
    is_active = Column(SmallInteger, default=0, index=True)
    created_by = Column(String(36))
    created_at = Column(DateTime, default=func.now())

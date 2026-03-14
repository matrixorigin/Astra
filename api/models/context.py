"""Context snapshot, decision audit, embedding, and prompt models."""

from matrixone import VectorPrecision, VectorType
from sqlalchemy import (
    Column,
    Float,
    Integer,
    SmallInteger,
    String,
    Text,
    UniqueConstraint,
)
from sqlalchemy.sql import func

from api.base import Base
from api.models._constants import EMBEDDING_DIM
from api.models._types import DateTime6, NullableJSON as JSON


class PromptFragment(Base):
    """Content-addressed storage for prompt sections.

    Fixed sections (identity, self_model, constraints) are stored once and
    referenced by hash. This deduplicates repeated content across turns/sessions.
    """

    __tablename__ = "ctx_prompt_fragments"
    fragment_hash = Column(String(64), primary_key=True)  # SHA256 prefix
    content = Column(Text, nullable=False)
    token_count = Column(Integer, nullable=False)
    fragment_type = Column(String(32), nullable=False, index=True)  # identity, self_model, etc.
    created_at = Column(DateTime6, default=func.now())


class ContextSnapshot(Base):
    __tablename__ = "ctx_snapshots"
    context_capture_id = Column(String(36), primary_key=True)
    session_id = Column(String(36), nullable=False, index=True)
    event_id = Column(String(36), nullable=False, index=True)
    system_prompt = Column(Text)
    skill_definitions = Column(JSON)
    selected_events = Column(JSON)
    retrieved_events = Column(JSON)
    code_context = Column(JSON)
    documentation = Column(JSON)
    token_budget = Column(JSON)
    total_tokens = Column(Integer)
    assembly_time_ms = Column(Integer)
    relevance_scores = Column(JSON)
    task_type = Column(String(50))
    skills_used = Column(JSON)
    llm_request_id = Column(String(64))
    llm_response_id = Column(String(64))
    created_at = Column(DateTime6, default=func.now())


class DecisionAudit(Base):
    __tablename__ = "ctx_decision_audits"
    decision_id = Column(String(36), primary_key=True)
    session_id = Column(String(36), nullable=False, index=True)
    event_id = Column(String(36), nullable=True, index=True)
    decision_type = Column(String(50), nullable=False, index=True)
    input_state = Column(JSON)
    decision_output = Column(JSON)
    reasoning = Column(Text)
    model_used = Column(String(50))
    model_params = Column(JSON)
    confidence_score = Column(String(10))
    created_at = Column(DateTime6, default=func.now())
    context_capture_id = Column(String(36))


class PromptFeedback(Base):
    __tablename__ = "ctx_prompt_feedback"
    feedback_id = Column(String(36), primary_key=True)
    llm_request_id = Column(String(36), nullable=False, index=True)
    user_comment = Column(Text)
    created_at = Column(DateTime6, default=func.now())


class EventEmbedding(Base):
    __tablename__ = "ctx_event_embeddings"
    event_id = Column(String(36), primary_key=True)
    embedding = Column(VectorType(EMBEDDING_DIM, VectorPrecision.F32))
    model_name = Column(String(50))
    model_version = Column(String(32))
    embedding_metadata = Column("metadata", JSON)
    created_at = Column(DateTime6, default=func.now())
    updated_at = Column(DateTime6, default=func.now(), onupdate=func.now())


class PromptTemplate(Base):
    __tablename__ = "ctx_prompt_templates"
    template_id = Column(String(64), primary_key=True)
    version = Column(String(32), nullable=False)
    content = Column(Text, nullable=False)
    input_variables = Column(JSON)
    description = Column(String(255))
    is_active = Column(SmallInteger, default=1, server_default="1")
    created_at = Column(DateTime6, default=func.now())
    updated_at = Column(DateTime6, default=func.now(), onupdate=func.now())


class PromptVariant(Base):
    __tablename__ = "ctx_prompt_variants"
    __table_args__ = (
        UniqueConstraint("prompt_template_id", "version", name="uq_template_version"),
    )
    variant_id = Column(String(64), primary_key=True)
    prompt_template_id = Column(String(64), nullable=False, index=True)
    version = Column(Integer, nullable=False)
    content = Column(Text, nullable=False)
    quality_score = Column(Float)
    description = Column(String(255))
    created_at = Column(DateTime6, default=func.now())

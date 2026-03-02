"""Skill registry, installation, permissions, selection, and learning models."""

from matrixone import VectorPrecision, VectorType
from sqlalchemy import (
    Column, DateTime, Float, Index, Integer, SmallInteger, String, Text, UniqueConstraint,
)
from sqlalchemy.sql import func

from api.base import Base
from api.models._constants import EMBEDDING_DIM
from api.models._types import NullableJSON as JSON


class SkillRegistry(Base):
    __tablename__ = "skills_registry"
    skill_id = Column(String(255), primary_key=True)  # skill_name@version
    skill_name = Column(String(255), nullable=False, index=True)
    version = Column(String(32), nullable=False)
    description = Column(Text)
    skill_definition = Column(JSON)
    code_hash = Column(String(64))
    git_commit_hash = Column(String(64))
    is_active = Column(SmallInteger, default=1)
    status = Column(String(20), default="active")
    category = Column(String(50))
    subcategory = Column(String(50))
    triggers = Column(JSON)
    dependencies = Column(JSON)
    priority = Column(Integer)
    cost_estimate = Column(String(20))
    side_effect_profile = Column(JSON)
    quality_schema = Column(JSON)  # Tool Result Quality Firewall — Tier 1 schema
    source = Column(String(20), default="builtin")
    manifest = Column(JSON)
    is_public = Column(SmallInteger, default=0)
    created_by = Column(String(36))
    created_at = Column(DateTime, default=func.now())
    updated_at = Column(DateTime, default=func.now(), onupdate=func.now())


class SkillInstallation(Base):
    __tablename__ = "skill_installations"
    __table_args__ = (
        UniqueConstraint("user_id", "skill_name", name="uq_user_skill"),
        Index("ix_install_user_status", "user_id", "status"),
    )

    installation_id = Column(String(36), primary_key=True)
    user_id = Column(String(36), nullable=False)
    skill_name = Column(String(100), nullable=False)
    skill_version = Column(String(20), nullable=False)
    previous_version = Column(String(20))
    status = Column(String(20), default="installed")
    installed_at = Column(DateTime, default=func.now(), nullable=False)
    updated_at = Column(DateTime, onupdate=func.now())


class SkillUserCredential(Base):
    __tablename__ = "skill_user_credentials"
    __table_args__ = (
        UniqueConstraint("user_id", "skill_name", "credential_name",
                         name="uq_user_skill_cred"),
    )

    credential_id = Column(String(36), primary_key=True)
    user_id = Column(String(36), nullable=False)
    skill_name = Column(String(100), nullable=False)
    credential_name = Column(String(100), nullable=False)
    value_encrypted = Column(Text, nullable=False)
    created_at = Column(DateTime, default=func.now(), nullable=False)
    rotated_at = Column(DateTime)


class SkillPermission(Base):
    __tablename__ = "skill_permissions"
    __table_args__ = (
        UniqueConstraint("skill_name", "grantee_type", "grantee_id", "permission_type",
                         name="uq_skill_grantee"),
    )

    permission_id = Column(String(36), primary_key=True)
    skill_name = Column(String(100), nullable=False)
    grantee_type = Column(String(10), nullable=False)
    grantee_id = Column(String(36), nullable=False)
    permission_type = Column(String(10), nullable=False, default="install")
    tenant_id = Column(String(36), nullable=True)
    granted_by = Column(String(36), nullable=False)
    granted_at = Column(DateTime, default=func.now(), nullable=False)
    expires_at = Column(DateTime, nullable=True)


class SkillSelectionEvent(Base):
    __tablename__ = "skill_selection_events"
    __table_args__ = (
        Index("ix_sse_skill_created", "skill_name", "created_at"),
        Index("ix_sse_session_created", "session_id", "created_at"),
    )
    event_id = Column(String(36), primary_key=True)
    session_id = Column(String(36))
    user_query = Column(Text)
    context_snapshot = Column(String(255))
    available_skills = Column(JSON)
    selected_skills = Column(JSON)
    skill_name = Column(String(128))
    skill_version = Column(String(32))
    selection_method = Column(String(50))
    selection_reasoning = Column(Text)
    candidate_scores = Column(JSON)
    execution_result = Column(JSON)
    execution_success = Column(SmallInteger)
    execution_time_ms = Column(Integer)
    execution_cost = Column(Float)
    user_feedback_score = Column(Integer)
    selection_correctness = Column(SmallInteger)
    correction_suggestion = Column(JSON)
    created_at = Column(DateTime, default=func.now(), index=True)


class SkillSelectionLearning(Base):
    __tablename__ = "skill_selection_learnings"
    learning_id = Column(String(36), primary_key=True)
    query_pattern = Column(String(255), nullable=False, index=True)
    query_embedding = Column(VectorType(EMBEDDING_DIM, VectorPrecision.F32))
    wrong_skills = Column(JSON)
    correct_skills = Column(JSON)
    improvement_score = Column(Float)
    confidence = Column(Float)
    evidence_count = Column(Integer, default=1)
    applied_count = Column(Integer, default=0)
    last_applied_at = Column(DateTime)
    signal_type = Column(String(50), default="wrong_skill", index=True)
    target_metrics = Column(JSON)
    context_features = Column(JSON)
    is_active = Column(SmallInteger, default=1, index=True)
    created_at = Column(DateTime, default=func.now())
    updated_at = Column(DateTime, default=func.now(), onupdate=func.now())


class SkillLearningSignal(Base):
    __tablename__ = "skill_learning_signals"
    signal_id = Column(String(36), primary_key=True)
    selection_event_id = Column(String(36), index=True, nullable=False)
    signal_type = Column(String(50), nullable=False, index=True)
    signal_data = Column(JSON, nullable=False)
    created_at = Column(DateTime, default=func.now(), index=True)


class SkillExecutionMetric(Base):
    __tablename__ = "skill_execution_metrics"
    metric_id = Column(String(36), primary_key=True)
    session_id = Column(String(36), index=True, nullable=False)
    skill_name = Column(String(255), index=True, nullable=False)
    execution_time_ms = Column(Integer, nullable=False)
    execution_cost = Column(Float, default=0.0)
    success = Column(SmallInteger, nullable=False)
    error_message = Column(Text)
    created_at = Column(DateTime, default=func.now(), index=True)

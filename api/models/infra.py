"""Infrastructure models: LLM, config, repo, sandbox, locks."""

from sqlalchemy import (
    JSON, Column, DateTime, Integer, SmallInteger, String, Text, UniqueConstraint,
)
from sqlalchemy.sql import func

from api.base import Base


class LLMModel(Base):
    __tablename__ = "infra_llm_models"
    __table_args__ = (
        UniqueConstraint('model_name', 'provider', name='uq_llm_model_name_provider'),
    )
    model_id = Column(String(36), primary_key=True)
    model_name = Column(String(100), nullable=False, index=True)
    provider = Column(String(50), nullable=False)
    api_key_encrypted = Column(String(512), nullable=False)
    base_url = Column(String(512), nullable=True)
    is_active = Column(SmallInteger, server_default="1", nullable=False)
    context_window = Column(Integer, default=128000)
    max_completion_tokens = Column(Integer, nullable=True)
    input_modalities = Column(JSON, default=lambda: ["text"])
    output_modalities = Column(JSON, default=lambda: ["text"])
    supported_parameters = Column(JSON, default=list)
    pricing = Column(JSON, default=dict)
    architecture = Column(String(50), nullable=True)
    tags = Column(JSON, default=list)
    created_by = Column(String(36), nullable=True)
    created_at = Column(DateTime, default=func.now())
    updated_at = Column(DateTime, default=func.now(), onupdate=func.now())


class Config(Base):
    __tablename__ = "infra_configs"
    __table_args__ = (
        UniqueConstraint('key_name', 'scope_type', 'scope_user_id', name='uq_config_scope'),
    )
    config_id = Column(String(64), primary_key=True)
    key_name = Column(String(255), nullable=False, index=True)
    scope_type = Column(String(32), nullable=False, index=True)
    scope_user_id = Column(String(64), nullable=True, index=True)
    value = Column(Text, nullable=False)
    description = Column(Text)
    created_at = Column(DateTime, default=func.now())
    updated_at = Column(DateTime, default=func.now(), onupdate=func.now())


class Repo(Base):
    __tablename__ = "infra_repos"
    repo_id = Column(String(36), primary_key=True)
    user_id = Column(String(36), nullable=False, index=True)
    repo_url = Column(String(255), nullable=False)
    repo_name = Column(String(100), nullable=False)
    repo_type = Column(String(50))
    token_id = Column(String(36))
    access_scope = Column(String(50), default="user")
    branch = Column(String(100), default="main")
    status = Column(String(20), default="active")
    last_synced_at = Column(DateTime)
    repo_metadata = Column("metadata", JSON)
    created_at = Column(DateTime, default=func.now())
    updated_at = Column(DateTime, default=func.now(), onupdate=func.now())


class SandboxMetadata(Base):
    __tablename__ = "infra_sandbox_metadata"
    sandbox_name = Column(String(64), primary_key=True)
    user_id = Column(String(36))
    data_source = Column(JSON)
    description = Column(String(255))
    created_by = Column(String(36))
    created_at = Column(DateTime(6))
    updated_at = Column(DateTime(6))
    tags = Column(JSON)
    source_database = Column(String(64))
    source_snapshot = Column(String(64))
    status = Column(String(20))
    session_id = Column(String(36), index=True, nullable=True)
    repo_id = Column(String(36), nullable=True)
    container_id = Column(String(64), nullable=True)
    terminated_at = Column(DateTime, nullable=True)


class DistributedLock(Base):
    __tablename__ = "infra_distributed_locks"
    lock_name = Column(String(64), primary_key=True)
    instance_id = Column(String(64), nullable=False)
    acquired_at = Column(DateTime, default=func.now(), nullable=False)
    expires_at = Column(DateTime, nullable=False)
    task_name = Column(String(64), nullable=False, index=True)

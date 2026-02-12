"""SQLAlchemy ORM models."""

from sqlalchemy import Column, DateTime, Integer, String, Text, JSON
from sqlalchemy.dialects.mysql import TINYINT
from sqlalchemy.orm import declarative_base
from sqlalchemy.sql import func

Base = declarative_base()


class User(Base):
    __tablename__ = "users"
    user_id = Column(String(36), primary_key=True)
    username = Column(String(50), unique=True, nullable=False, index=True)
    email = Column(String(255), unique=True, nullable=False, index=True)
    password_hash = Column(String(255), nullable=False)
    display_name = Column(String(100))
    is_active = Column(TINYINT(1), server_default="1", nullable=False)
    created_at = Column(DateTime, default=func.now(), nullable=False)
    last_login_at = Column(DateTime)


class Agent(Base):
    __tablename__ = "agents"
    agent_id = Column(String(36), primary_key=True)
    agent_name = Column(String(100), nullable=False)
    agent_type = Column(String(50), nullable=False)
    owner_user_id = Column(String(36), nullable=False, index=True)
    agent_config = Column("agent_config", JSON)
    data_source = Column(JSON)  # 新增：数据源配置
    is_active = Column(TINYINT(1), server_default="1", nullable=False)
    created_at = Column(DateTime, default=func.now(), nullable=False)
    updated_at = Column(DateTime, default=func.now(), onupdate=func.now())


class RefreshToken(Base):
    __tablename__ = "refresh_tokens"
    token_id = Column(String(36), primary_key=True)
    user_id = Column(String(36), nullable=False, index=True)
    token_hash = Column(String(255), nullable=False)
    expires_at = Column(DateTime, nullable=False)
    is_revoked = Column(TINYINT(1), default=0, nullable=False)
    created_at = Column(DateTime, default=func.now(), nullable=False)


class Session(Base):
    __tablename__ = "sessions"
    session_id = Column(String(36), primary_key=True)
    user_id = Column(String(36), nullable=False, index=True)
    agent_id = Column(String(36), nullable=True, index=True)  # 新增：关联的Agent
    title = Column(String(255), nullable=True)  # 新增：会话标题
    status = Column(String(20), default="active", nullable=False, index=True)
    event_count = Column(Integer, default=0, nullable=False)
    created_at = Column(DateTime, default=func.now(), nullable=False)
    updated_at = Column(DateTime, default=func.now(), onupdate=func.now())  # 新增：更新时间
    ended_at = Column(DateTime, nullable=True)  # 新增：结束时间
    last_active_at = Column(DateTime, default=func.now(), nullable=False)
    session_metadata = Column("metadata", JSON)


class Event(Base):
    __tablename__ = "conversation_events"
    event_id = Column(String(36), primary_key=True)
    session_id = Column(String(36), nullable=False, index=True)
    user_id = Column(String(36), nullable=False, index=True)
    agent_id = Column(String(36), nullable=False, default="system")
    agent_version = Column(String(32), nullable=False, default="1.0.0")
    event_type = Column(String(50), nullable=False, index=True)
    content = Column(Text, nullable=False)
    parent_event_id = Column(String(36), nullable=True, index=True)
    causal_chain_id = Column(String(36), nullable=False, index=True)
    desensitized_content = Column(Text)
    created_at = Column(DateTime, default=func.now(), nullable=False)
    event_metadata = Column("metadata", JSON)
    context_snapshot = Column(JSON)
    token_usage = Column(JSON)
    embedding_ref = Column(String(128))
    prompt_template_id = Column(String(64))
    skills_snapshot = Column(JSON)
    quality_score = Column(String(10))
    is_flagged = Column(TINYINT(1), server_default="0")
    training_eligible = Column(TINYINT(1), server_default="0")
    llm_model_used = Column(String(50))
    llm_params = Column(JSON)
    skill_name = Column(String(255))
    skill_version = Column(String(32))


class PromptTemplate(Base):
    __tablename__ = "prompt_templates"
    template_id = Column(String(64), primary_key=True)
    template_name = Column(String(255), nullable=False)
    template_content = Column(Text, nullable=False)
    version = Column(String(32), nullable=False)
    created_at = Column(DateTime, default=func.now(), nullable=False)
    is_active = Column(TINYINT(1), server_default="1")


class SkillRegistry(Base):
    __tablename__ = "skills_registry"
    skill_id = Column(String(64), primary_key=True)
    skill_name = Column(String(255), nullable=False)
    version = Column(String(32), nullable=False)
    skill_version = Column(String(32), nullable=False)
    skill_definition = Column(JSON)
    git_commit_hash = Column(String(64), index=True)
    created_at = Column(DateTime, default=func.now(), nullable=False)
    is_active = Column(TINYINT(1), server_default="1")


class ContextSnapshot(Base):
    __tablename__ = "context_snapshots"
    snapshot_id = Column(String(36), primary_key=True)
    event_id = Column(String(64), index=True)
    snapshot_data = Column(JSON, nullable=False)
    created_at = Column(DateTime, default=func.now(), nullable=False, index=True)


class DecisionAudit(Base):
    __tablename__ = "decision_audit"
    decision_id = Column(String(36), primary_key=True)
    session_id = Column(String(36), nullable=False, index=True)
    event_id = Column(String(36), nullable=False, index=True)
    snapshot_id = Column(String(36), nullable=False, index=True)
    decision_type = Column(String(50), nullable=False)
    decision_output = Column(JSON, nullable=False)
    model_params = Column(JSON)
    created_at = Column(DateTime, default=func.now(), nullable=False, index=True)


class EventEmbedding(Base):
    __tablename__ = "event_embeddings"
    embedding_id = Column(String(64), primary_key=True)
    event_id = Column(String(64), nullable=False, index=True)
    embedding_vector = Column(Text)
    created_at = Column(DateTime, default=func.now(), nullable=False)


class Repo(Base):
    __tablename__ = "repos"
    repo_id = Column(String(64), primary_key=True)
    repo_url = Column(String(512), nullable=False, unique=True)
    repo_type = Column(String(32), nullable=False)
    owner_id = Column(String(255), nullable=False, index=True)
    owner_type = Column(String(32), nullable=False)
    repo_group = Column(String(255))
    token_id = Column(String(64))
    access_scope = Column(String(32), nullable=False)
    repo_metadata = Column("metadata", JSON)
    created_at = Column(DateTime, default=func.now(), nullable=False)
    updated_at = Column(DateTime, default=func.now(), onupdate=func.now())
    is_active = Column(TINYINT(1), server_default="1", nullable=False)


class SandboxMetadata(Base):
    __tablename__ = "sandbox_metadata"
    sandbox_name = Column(String(255), primary_key=True)
    user_id = Column(String(36), nullable=False, index=True)  # 新增
    data_source = Column(JSON, nullable=False)  # 新增
    description = Column(Text)
    created_by = Column(String(255))
    source_database = Column(String(255))
    source_snapshot = Column(String(255))
    status = Column(String(32), default="active")
    created_at = Column(DateTime, default=func.now(), nullable=False)
    updated_at = Column(DateTime, default=func.now(), onupdate=func.now())
    expires_at = Column(DateTime)  # 新增
    deleted_at = Column(DateTime)  # 新增
    tables = Column(JSON)
    tags = Column(JSON)


class AuditLog(Base):
    __tablename__ = "audit_logs"
    log_id = Column(String(36), primary_key=True)
    user_id = Column(String(36), nullable=False, index=True)
    action = Column(String(100), nullable=False)
    resource_type = Column(String(50), nullable=False)
    resource_id = Column(String(255))
    details = Column(JSON)
    timestamp = Column(DateTime, default=func.now(), nullable=False)
    ip_address = Column(String(45))
    user_agent = Column(Text)

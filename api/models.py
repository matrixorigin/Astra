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


class Config(Base):
    __tablename__ = "configs"
    config_id = Column(String(36), primary_key=True)
    key_name = Column(String(100), nullable=False, index=True)
    value = Column(Text)
    scope_type = Column(String(20), default="global")  # global, tenant, user
    scope_tenant_id = Column(String(36), index=True)
    scope_user_id = Column(String(36), index=True)
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
    agent_id = Column(String(36), nullable=True, index=True)
    title = Column(String(255), nullable=True)
    status = Column(String(20), default="active", nullable=False, index=True)
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
    skill_id = Column(String(128), primary_key=True)
    skill_name = Column(String(255), nullable=False)
    version = Column(String(32), nullable=False)
    description = Column(Text)
    skill_definition = Column("requirements", JSON)
    code_hash = Column(String(64))
    is_active = Column(TINYINT(1), server_default="1")
    status = Column(String(32), server_default="active")
    category = Column(String(64))
    subcategory = Column(String(64))
    triggers = Column(JSON)
    dependencies = Column(JSON)
    priority = Column(Integer, server_default="50")
    cost_estimate = Column(String(32))
    side_effect_category = Column(String(32))
    created_at = Column(DateTime, default=func.now(), nullable=False)
    updated_at = Column(DateTime, default=func.now(), onupdate=func.now())
    git_commit_hash = Column(String(64), index=True)


class ContextSnapshot(Base):
    __tablename__ = "context_snapshots"
    snapshot_id = Column(String(64), primary_key=True)
    session_id = Column(String(36), nullable=False, index=True)
    event_id = Column(String(64), index=True)
    system_prompt = Column(Text)
    skill_definitions = Column(JSON)
    selected_events = Column(JSON)
    code_context = Column(JSON)
    documentation = Column(JSON)
    total_tokens = Column(Integer)
    token_budget = Column(JSON)
    assembly_time_ms = Column(Integer)
    relevance_scores = Column(JSON)
    task_type = Column(String(64))
    created_at = Column(DateTime, default=func.now(), index=True)
    skills_used = Column(JSON)
    llm_request_id = Column(String(100))
    llm_response_id = Column(String(100))


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
    event_id = Column(String(64), primary_key=True)  # Use event_id as primary key
    embedding = Column(Text, nullable=False)  # Store as JSON string
    model_name = Column(String(100), nullable=False)
    model_version = Column(String(20), nullable=False)
    embedding_metadata = Column("metadata", JSON)  # Use column mapping for reserved word
    created_at = Column(DateTime, default=func.now(), nullable=False)
    updated_at = Column(DateTime, default=func.now(), onupdate=func.now())


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


class Token(Base):
    __tablename__ = "tokens"
    token_id = Column(String(64), primary_key=True)
    type = Column(String(32), nullable=False)
    provider = Column(String(64))
    scope_user_id = Column(String(64))
    scope_tenant_id = Column(String(64))
    scope_repo = Column(String(255))
    secret_ref = Column(String(255))
    encrypted_value = Column(Text)
    is_active = Column(TINYINT(1), server_default="1")
    expires_at = Column(DateTime)
    created_at = Column(DateTime, default=func.now())
    token_metadata = Column("metadata", JSON)


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


class SkillSelectionEvent(Base):
    __tablename__ = "skill_selection_events"
    event_id = Column(String(36), primary_key=True)
    session_id = Column(String(36), nullable=False, index=True)
    user_query = Column(Text, nullable=False)
    context_snapshot = Column(String(100), nullable=False)
    available_skills = Column(JSON, nullable=False)
    selected_skills = Column(JSON, nullable=False)
    selection_method = Column(String(50), nullable=False)
    selection_reasoning = Column(Text)
    candidate_scores = Column(JSON)
    execution_result = Column(JSON)
    execution_success = Column(TINYINT(1))
    execution_time_ms = Column(Integer)
    execution_cost = Column(JSON)
    user_feedback_score = Column(Integer)
    selection_correctness = Column(TINYINT(1))
    correction_suggestion = Column(JSON)
    created_at = Column(DateTime, default=func.now(), nullable=False)


class SkillSelectionLearning(Base):
    __tablename__ = "skill_selection_learnings"
    learning_id = Column(String(36), primary_key=True)
    query_pattern = Column(String(255), nullable=False, index=True)
    wrong_skills = Column(JSON, nullable=False)
    correct_skills = Column(JSON, nullable=False)
    improvement_score = Column(Integer)
    evidence_count = Column(Integer, default=1)
    confidence = Column(Integer)
    learned_at = Column(DateTime, default=func.now())
    applied_count = Column(Integer, default=0)
    last_applied_at = Column(DateTime)

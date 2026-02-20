"""SQLAlchemy ORM models."""

from sqlalchemy import Column, DateTime, Integer, String, Text, JSON, ForeignKey, Float, Index
from sqlalchemy.dialects.mysql import TINYINT
from sqlalchemy.orm import declarative_base
from sqlalchemy.sql import func
from matrixone import VectorType, VectorPrecision

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


class SelectorGateResult(Base):
    __tablename__ = "selector_gate_results"
    gate_id = Column(String(36), primary_key=True)
    selector_version = Column(String(32))
    test_queries = Column(JSON)  # Store test queries used
    test_count = Column(Integer)  # Number of test queries
    verdict = Column(String(20), nullable=False)  # PASS/FAIL
    new_avg_score = Column(Float)
    old_avg_score = Column(Float)
    improvement_pct = Column(Float)
    learnings_applied = Column(Integer, default=0)  # Number of learnings applied
    details = Column(JSON)
    created_at = Column(DateTime, default=func.now())


class GateResult(Base):
    """Unified gate validation results for all change types."""
    __tablename__ = "gate_results"
    
    gate_id = Column(String(36), primary_key=True)
    change_type = Column(String(20), nullable=False)  # prompt/skill/config/selector
    change_id = Column(String(128), nullable=False)  # e.g., "code_review@v3"
    snapshot_used = Column(String(64))  # Snapshot ID used for validation
    sessions_tested = Column(Integer, default=0)  # Number of golden sessions tested
    error_rate = Column(Float, default=0.0)  # Error rate from replay
    score_delta = Column(Float, default=0.0)  # Quality score change
    passed = Column(TINYINT(1), nullable=False)  # Pass/fail verdict
    metrics = Column(Text)  # JSON metrics (error_rate, score_delta, latency, tokens)
    created_at = Column(DateTime, default=func.now())


class Config(Base):
    __tablename__ = "configs"
    config_id = Column(String(64))
    key_name = Column(String(255), primary_key=True, nullable=False)
    value = Column(Text, nullable=False)
    description = Column(Text)
    created_at = Column(DateTime, default=func.now())
    updated_at = Column(DateTime, default=func.now(), onupdate=func.now())


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
    embedding = Column(VectorType(1536, VectorPrecision.F32))  # Native VECF32 vector
    prompt_template_id = Column(String(64))
    skills_snapshot = Column(JSON)
    quality_score = Column(Float)  # Changed from String(10) to Float for aggregation
    is_flagged = Column(TINYINT(1), server_default="0")
    training_eligible = Column(TINYINT(1), server_default="0")
    llm_model_used = Column(String(50))
    llm_params = Column(JSON)
    skill_name = Column(String(255))
    skill_version = Column(String(32))
    skill_result = Column(JSON)


class Role(Base):
    __tablename__ = "roles"
    role_id = Column(String(36), primary_key=True)
    role_name = Column(String(50), unique=True, nullable=False)
    description = Column(String(255))
    created_at = Column(DateTime, default=func.now())


class UserRole(Base):
    __tablename__ = "user_roles"
    id = Column(Integer, primary_key=True, autoincrement=True)
    user_id = Column(String(36), ForeignKey("users.user_id"), nullable=False, index=True)
    role_id = Column(String(36), ForeignKey("roles.role_id"), nullable=False, index=True)
    created_at = Column(DateTime, default=func.now())


class SkillRegistry(Base):
    __tablename__ = "skills_registry"
    skill_id = Column(String(255), primary_key=True)  # skill_name@version
    skill_name = Column(String(255), nullable=False, index=True)
    version = Column(String(32), nullable=False)
    description = Column(Text)
    skill_definition = Column(JSON)
    code_hash = Column(String(64))
    git_commit_hash = Column(String(64))
    is_active = Column(TINYINT(1), default=1)
    category = Column(String(50))
    subcategory = Column(String(50))
    triggers = Column(JSON)
    dependencies = Column(JSON)
    priority = Column(Integer)
    cost_estimate = Column(String(20))
    side_effect_profile = Column(JSON)  # From replay-sandbox design
    created_at = Column(DateTime, default=func.now())
    updated_at = Column(DateTime, default=func.now(), onupdate=func.now())


class PromptTemplate(Base):
    __tablename__ = "prompt_templates"
    template_id = Column(String(64), primary_key=True)
    version = Column(String(32), nullable=False)
    content = Column(Text, nullable=False)
    input_variables = Column(JSON)
    description = Column(String(255))
    is_active = Column(TINYINT(1), default=1)
    created_at = Column(DateTime, default=func.now())
    updated_at = Column(DateTime, default=func.now(), onupdate=func.now())


class ContextSnapshot(Base):
    __tablename__ = "context_snapshots"
    snapshot_id = Column(String(36), primary_key=True)
    session_id = Column(String(36), nullable=False, index=True)
    event_id = Column(String(36), nullable=False, index=True)
    system_prompt = Column(Text)
    skill_definitions = Column(JSON)
    selected_events = Column(JSON)
    retrieved_events = Column(JSON)  # Raw retrieval results with scores for replay consistency
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
    created_at = Column(DateTime, default=func.now())


class DecisionAudit(Base):
    __tablename__ = "decision_audit"
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
    created_at = Column(DateTime, default=func.now())
    snapshot_id = Column(String(36))


class EventEmbedding(Base):
    __tablename__ = "event_embeddings"
    event_id = Column(String(36), primary_key=True)
    embedding = Column(VectorType(1536, VectorPrecision.F32))  # Native VECF32 vector
    model_name = Column(String(50))
    model_version = Column(String(32))
    embedding_metadata = Column("metadata", JSON)
    created_at = Column(DateTime, default=func.now())
    updated_at = Column(DateTime, default=func.now(), onupdate=func.now())


class Repo(Base):
    __tablename__ = "repos"
    repo_id = Column(String(36), primary_key=True)
    user_id = Column(String(36), nullable=False, index=True)
    repo_url = Column(String(255), nullable=False)
    repo_name = Column(String(100), nullable=False)
    repo_type = Column(String(50))
    token_id = Column(String(36))  # For TokenResolver
    access_scope = Column(String(50), default="user")
    branch = Column(String(100), default="main")
    status = Column(String(20), default="active")
    last_synced_at = Column(DateTime)
    repo_metadata = Column("metadata", JSON)
    created_at = Column(DateTime, default=func.now())
    updated_at = Column(DateTime, default=func.now(), onupdate=func.now())


class SandboxMetadata(Base):
    __tablename__ = "sandbox_metadata"
    # Matches core/sandbox/sandbox.py schema
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
    # Optional fields for container/session if needed
    session_id = Column(String(36), index=True, nullable=True)
    repo_id = Column(String(36), nullable=True)
    container_id = Column(String(64), nullable=True)
    terminated_at = Column(DateTime, nullable=True)


class AuditLog(Base):
    __tablename__ = "audit_logs"
    log_id = Column(String(36), primary_key=True)
    user_id = Column(String(36), nullable=False, index=True)
    action = Column(String(50), nullable=False)
    resource_type = Column(String(50))
    resource_id = Column(String(36))
    details = Column(JSON)
    ip_address = Column(String(45))
    created_at = Column(DateTime, default=func.now())


class Token(Base):
    __tablename__ = "tokens"
    token_id = Column(String(36), primary_key=True)
    type = Column(String(50), nullable=False)
    provider = Column(String(50), nullable=False)
    encrypted_value = Column(String(255), nullable=True)  # Nullable if using secret_ref
    secret_ref = Column(String(255))
    is_active = Column(TINYINT(1), default=1)
    scope_user_id = Column(String(36), index=True)
    scope_tenant_id = Column(String(36), index=True)
    scope_repo = Column(String(255), index=True)
    created_at = Column(DateTime, default=func.now())
    expires_at = Column(DateTime, nullable=True)
    token_metadata = Column("metadata", JSON)


class SkillSelectionEvent(Base):
    __tablename__ = "skill_selection_events"
    event_id = Column(String(36), primary_key=True)
    session_id = Column(String(36), index=True)
    user_query = Column(Text)
    context_snapshot = Column(String(255))
    available_skills = Column(JSON)
    selected_skills = Column(JSON)
    selection_method = Column(String(50))
    selection_reasoning = Column(Text)
    candidate_scores = Column(JSON)
    execution_result = Column(JSON)
    execution_success = Column(TINYINT(1))
    execution_time_ms = Column(Integer)
    execution_cost = Column(Float)
    user_feedback_score = Column(Integer)
    selection_correctness = Column(TINYINT(1))
    correction_suggestion = Column(JSON)
    created_at = Column(DateTime, default=func.now())


class SkillSelectionLearning(Base):
    __tablename__ = "skill_selection_learning"
    learning_id = Column(String(36), primary_key=True)
    query_pattern = Column(String(255), nullable=False, index=True)
    query_embedding = Column(VectorType(1536, VectorPrecision.F32))  # Native VECF32 vector
    wrong_skills = Column(JSON)
    correct_skills = Column(JSON)
    improvement_score = Column(Float)
    confidence = Column(Float)
    evidence_count = Column(Integer, default=1)
    applied_count = Column(Integer, default=0)
    last_applied_at = Column(DateTime)
    # Multi-dimensional learning fields
    signal_type = Column(String(50), default="wrong_skill", index=True)
    target_metrics = Column(JSON)  # {"time_ms": 500, "cost": 0.01, "satisfaction": 4}
    context_features = Column(JSON)  # Context features for better matching
    created_at = Column(DateTime, default=func.now())
    updated_at = Column(DateTime, default=func.now(), onupdate=func.now())


class SkillLearningSignal(Base):
    """Raw learning signals collected from skill selection events."""
    __tablename__ = "skill_learning_signals"
    signal_id = Column(String(36), primary_key=True)
    selection_event_id = Column(String(36), index=True, nullable=False)
    signal_type = Column(String(50), nullable=False, index=True)  # wrong_skill, slow_execution, high_cost, low_satisfaction
    signal_data = Column(JSON, nullable=False)  # Type-specific data
    created_at = Column(DateTime, default=func.now(), index=True)


class SkillExecutionMetric(Base):
    """Records execution metrics for skills to enable multi-dimensional learning."""
    __tablename__ = "skill_execution_metrics"
    metric_id = Column(String(36), primary_key=True)
    session_id = Column(String(36), index=True, nullable=False)
    skill_name = Column(String(255), index=True, nullable=False)
    execution_time_ms = Column(Integer, nullable=False)
    execution_cost = Column(Float, default=0.0)
    success = Column(TINYINT(1), nullable=False)
    error_message = Column(Text)
    created_at = Column(DateTime, default=func.now(), index=True)


class KnowledgeEntry(Base):
    """Semantic memory: extracted knowledge that persists across sessions."""
    __tablename__ = "knowledge_entries"
    entry_id = Column(String(64), primary_key=True)
    user_id = Column(String(64), nullable=False, index=True)
    agent_id = Column(String(64))
    
    # What
    category = Column(String(50), nullable=False, index=True)  # user_preference | codebase_pattern | domain_fact | tool_behavior | entity
    key_name = Column(String(255), nullable=False, index=True)
    value = Column(Text, nullable=False)
    
    # Provenance
    source_event_ids = Column(JSON, nullable=False)
    extraction_method = Column(String(50))  # llm_extraction | user_explicit | observation
    
    # Trust & Lifecycle
    trust_tier = Column(String(10), default="T3")  # T1/T2/T3/T4
    confidence = Column(Float, default=1.0)
    initial_confidence = Column(Float, default=1.0)
    last_validated_at = Column(DateTime, default=func.now())
    last_accessed_at = Column(DateTime)
    access_count = Column(Integer, default=0)
    
    # Versioning
    version = Column(Integer, default=1)
    superseded_by = Column(String(64))
    
    # Vector search (stored as text, converted to VECF64 in raw SQL)
    embedding = Column(VectorType(1536, VectorPrecision.F32))  # Native VECF32 vector
    
    created_at = Column(DateTime, default=func.now())
    updated_at = Column(DateTime, default=func.now(), onupdate=func.now())


class AgentScratchpad(Base):
    """Working memory: structured notes for long-horizon tasks."""
    __tablename__ = "agent_scratchpad"
    __table_args__ = (
        Index('idx_scratchpad_session', 'session_id'),
        Index('idx_scratchpad_user', 'user_id'),
        Index('idx_scratchpad_type', 'note_type'),
    )
    
    note_id = Column(String(64), primary_key=True)
    session_id = Column(String(64), nullable=False)
    user_id = Column(String(64), nullable=False)
    agent_id = Column(String(64))
    
    # Note content
    note_type = Column(String(50), nullable=False)  # plan | hypothesis | finding | todo | decision
    content = Column(Text, nullable=False)
    
    # Lifecycle
    status = Column(String(20), default="active")  # active | completed | superseded
    
    # Linkage
    related_event_ids = Column(JSON)
    related_note_ids = Column(JSON)
    
    created_at = Column(DateTime, default=func.now())
    updated_at = Column(DateTime, default=func.now(), onupdate=func.now())

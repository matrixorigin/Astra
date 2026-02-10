#!/bin/bash
# Database initialization script for mo-dev-agent
# Creates core tables based on design document

set -e

# Load environment variables
if [ -f .env ]; then
    export $(cat .env | grep -v '^#' | xargs)
fi

# Database connection parameters
DB_HOST=${MATRIXONE_HOST:-localhost}
DB_PORT=${MATRIXONE_PORT:-6001}
DB_USER=${MATRIXONE_USER:-root}
DB_PASS=${MATRIXONE_PASSWORD:-111}
DB_NAME=${MATRIXONE_DATABASE:-dev_agent}

echo "Initializing mo-dev-agent database..."
echo "Host: $DB_HOST:$DB_PORT"
echo "Database: $DB_NAME"
echo ""

# MySQL connection options (try multiple SSL variants)
MYSQL_BASE="-h$DB_HOST -P$DB_PORT -u$DB_USER -p$DB_PASS"

# Try to create database (try different SSL options)
if mysql $MYSQL_BASE -e "CREATE DATABASE IF NOT EXISTS $DB_NAME;" 2>/dev/null; then
    MYSQL_OPTS="$MYSQL_BASE"
elif mysql $MYSQL_BASE --skip-ssl -e "CREATE DATABASE IF NOT EXISTS $DB_NAME;" 2>/dev/null; then
    echo "Using --skip-ssl"
    MYSQL_OPTS="$MYSQL_BASE --skip-ssl"
elif mysql $MYSQL_BASE --skip_ssl -e "CREATE DATABASE IF NOT EXISTS $DB_NAME;" 2>/dev/null; then
    echo "Using --skip_ssl (underscore)"
    MYSQL_OPTS="$MYSQL_BASE --skip_ssl"
else
    echo "❌ Failed to connect to database. Please check connection parameters."
    exit 1
fi

# Execute SQL schema
mysql $MYSQL_OPTS "$DB_NAME" <<'EOF'

-- ============================================================================
-- Core Tables for mo-dev-agent
-- Based on: docs/design/context-memory-session-and-tables.md
-- ============================================================================

-- conversation_events: Event-centric data model (single source of truth)
CREATE TABLE IF NOT EXISTS conversation_events (
  event_id            VARCHAR(64) PRIMARY KEY COMMENT 'ULID, globally unique and sortable',
  user_id             VARCHAR(64) NOT NULL COMMENT 'User identifier (cross-session key)',
  session_id          VARCHAR(64) NOT NULL COMMENT 'Session identifier',
  agent_id            VARCHAR(64) NOT NULL COMMENT 'Agent type (e.g., dev-agent, chat-agent)',
  agent_version       VARCHAR(32) NOT NULL COMMENT 'Agent code/config version',
  event_type          VARCHAR(24) NOT NULL COMMENT 'user_query | llm_request | llm_response | tool_call | tool_result | system_message | multi_agent_message',
  content             TEXT NOT NULL COMMENT 'Original content (for reproducibility)',
  desensitized_content TEXT COMMENT 'Optional: desensitized version for compliance',
  metadata            JSON COMMENT 'Namespace convention: dev.*, chat.*, etc.',
  context_snapshot    JSON COMMENT 'Reproducibility: prompt_template_id, skills_used, history_events, retrieved_chunks',
  token_usage         JSON COMMENT 'e.g., {"prompt":1200, "completion":300, "total":1500}',
  embedding_ref       VARCHAR(128) COMMENT 'External vector store chunk ID',
  created_at          TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  prompt_template_id  VARCHAR(64) COMMENT 'References prompt_templates (template_id@version)',
  skills_snapshot     JSON COMMENT 'e.g., [{"id":"review", "version":"v2", "used":true}]',
  quality_score       DECIMAL(3,2) COMMENT 'System pre-score (0-5)',
  is_flagged          BOOLEAN DEFAULT FALSE,
  training_eligible   BOOLEAN DEFAULT FALSE COMMENT 'Set by rule or evaluation for training pipeline',
  parent_event_id     VARCHAR(64) COMMENT 'Immediate prior event in causal chain',
  causal_chain_id     VARCHAR(64) COMMENT 'Groups one user query + full LLM/tool chain',
  llm_model_used      VARCHAR(50) COMMENT 'Model identifier at inference time',
  llm_params          JSON COMMENT 'e.g., {"temperature":0.7, "max_tokens":1024}',
  
  INDEX idx_ce_user_time (user_id, created_at),
  INDEX idx_ce_session (session_id, created_at DESC),
  INDEX idx_ce_training (training_eligible, quality_score DESC),
  INDEX idx_causal_chain (causal_chain_id, created_at)
) COMMENT='Event-centric conversation data (single source of truth)';

-- sessions: Conversation scope and lifecycle
CREATE TABLE IF NOT EXISTS sessions (
  session_id          VARCHAR(36) PRIMARY KEY,
  user_id             VARCHAR(255) NOT NULL,
  tenant_id           VARCHAR(255) COMMENT 'Optional: multi-tenant support',
  created_at          TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  updated_at          TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  last_active_at      TIMESTAMP COMMENT 'For recovery and idle timeout',
  status              VARCHAR(32) COMMENT 'active | idle | closed',
  last_event_id       VARCHAR(64) COMMENT 'App-maintained ref (no FK to avoid circular dependency)',
  event_count         INT DEFAULT 0 COMMENT 'For max_events enforcement',
  summary_status      VARCHAR(32) COMMENT 'pending | completed | failed',
  summary_job_id      VARCHAR(255),
  vector_db_snapshot_id VARCHAR(128) COMMENT 'For replay/sandbox: vector store snapshot ref',
  metadata            JSON,
  
  INDEX idx_sessions_user_status_active (user_id, status, last_active_at DESC)
) COMMENT='Session management and lifecycle';

-- prompt_templates: Versioned prompt templates
CREATE TABLE IF NOT EXISTS prompt_templates (
  template_id         VARCHAR(64) NOT NULL,
  version             VARCHAR(32) NOT NULL,
  content             TEXT NOT NULL COMMENT 'Template body (Markdown, placeholders)',
  effective_at        TIMESTAMP COMMENT 'When this version became effective',
  is_active           BOOLEAN DEFAULT TRUE,
  created_at          TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  
  PRIMARY KEY (template_id, version)
) COMMENT='Versioned prompt templates (anti-fragility)';

-- skills_registry: Versioned skill definitions
CREATE TABLE IF NOT EXISTS skills_registry (
  skill_id            VARCHAR(64) NOT NULL,
  version             VARCHAR(20) NOT NULL COMMENT 'Semantic versioning (v1.0.0)',
  git_commit_hash     VARCHAR(64) COMMENT 'MatrixOne Git for Data commit hash',
  description         TEXT NOT NULL,
  documentation       TEXT COMMENT 'Full Markdown docs (examples/params)',
  skill_code          TEXT COMMENT 'Python code (small skills) or NULL',
  code_ref            VARCHAR(255) COMMENT 'Large codebases: MatrixOne internal repo path',
  input_schema        JSON,
  output_schema       JSON,
  tools_required      JSON COMMENT 'Dependent tool IDs',
  safety_rules        JSON COMMENT '["no_pii", "max_tokens=500"]',
  tags                JSON COMMENT '["customer_service", "data_query"]',
  status              VARCHAR(20) DEFAULT 'active' COMMENT 'active | deprecated | experimental',
  created_at          TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  updated_at          TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  
  PRIMARY KEY (skill_id, version)
) COMMENT='Versioned skill definitions (first-class citizens)';

-- configs: Key-value configuration
CREATE TABLE IF NOT EXISTS configs (
  config_id           VARCHAR(64) PRIMARY KEY,
  scope_type          VARCHAR(32) COMMENT 'global | tenant | user',
  scope_id            VARCHAR(255) COMMENT 'Nullable (e.g., tenant_id, user_id)',
  key_name            VARCHAR(255) NOT NULL,
  value               TEXT,
  updated_at          TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  
  UNIQUE KEY idx_configs_scope_key (scope_type, scope_id, key_name)
) COMMENT='Key-value configuration (budgets, session limits, feature flags)';

-- tokens: Secret management
CREATE TABLE IF NOT EXISTS tokens (
  token_id            VARCHAR(64) PRIMARY KEY,
  type                VARCHAR(32) NOT NULL COMMENT 'repo | llm',
  provider            VARCHAR(64) COMMENT 'e.g., github, openai, groq',
  scope_user_id       VARCHAR(255),
  scope_tenant_id     VARCHAR(255),
  scope_repo          VARCHAR(255),
  secret_ref          VARCHAR(255) COMMENT 'Preferred: Vault path or secret manager ref',
  encrypted_value     TEXT COMMENT 'Alternative if no secret manager',
  created_at          TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  expires_at          TIMESTAMP,
  is_active           BOOLEAN DEFAULT TRUE,
  rotation_policy     VARCHAR(64) COMMENT 'manual | 90d',
  metadata            JSON,
  
  INDEX idx_tokens_scope_user (scope_user_id, type),
  INDEX idx_tokens_scope_tenant (scope_tenant_id, type)
) COMMENT='Secret management (repo and LLM tokens)';

-- event_evaluations: Feedback and evaluation
CREATE TABLE IF NOT EXISTS event_evaluations (
  eval_id             VARCHAR(64) PRIMARY KEY,
  event_id            VARCHAR(64) NOT NULL,
  evaluator_id        VARCHAR(64) COMMENT 'user or system',
  score               DECIMAL(3,2) COMMENT 'Overall score',
  dimensions          JSON COMMENT '{"relevance":5, "helpfulness":4, "safety":5}',
  feedback            TEXT,
  source              VARCHAR(20) COMMENT 'user_feedback | auto_metric | human_label',
  created_at          TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  
  INDEX idx_eval_event (event_id)
) COMMENT='User/system evaluations per event';

-- memory_index_queue: RAG indexing queue
CREATE TABLE IF NOT EXISTS memory_index_queue (
  queue_id            VARCHAR(36) PRIMARY KEY,
  event_id            VARCHAR(64) NOT NULL COMMENT 'Event to be indexed',
  status              VARCHAR(32) NOT NULL COMMENT 'pending | processing | completed | failed',
  retry_count         INT DEFAULT 0,
  created_at          TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  
  INDEX idx_queue_status (status, created_at)
) COMMENT='RAG indexing queue (async worker)';

EOF

echo ""
echo "✅ Database schema initialized successfully!"
echo ""
echo "Tables created:"
echo "  - conversation_events (event-centric data model)"
echo "  - sessions (conversation scope)"
echo "  - prompt_templates (versioned prompts)"
echo "  - skills_registry (versioned skills)"
echo "  - configs (key-value config)"
echo "  - tokens (secret management)"
echo "  - event_evaluations (feedback loop)"
echo "  - memory_index_queue (RAG pipeline)"
echo ""
echo "Next steps:"
echo "  1. Verify: make db-connect"
echo "  2. Test: make test-unit"

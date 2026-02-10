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
  skill_name          VARCHAR(255) COMMENT 'Skill name if this is a skill execution event',
  skill_version       VARCHAR(32) COMMENT 'Skill version for replay reproducibility',
  
  INDEX idx_ce_user_time (user_id, created_at),
  INDEX idx_ce_session (session_id, created_at DESC),
  INDEX idx_ce_training (training_eligible, quality_score DESC),
  INDEX idx_causal_chain (causal_chain_id, created_at),
  INDEX idx_skill_exec (skill_name, skill_version, created_at)
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
  skill_name          VARCHAR(255) NOT NULL COMMENT 'Human-readable skill name',
  version             VARCHAR(20) NOT NULL COMMENT 'Semantic versioning (1.0.0)',
  git_commit_hash     VARCHAR(64) COMMENT 'MatrixOne Git for Data commit hash',
  description         TEXT NOT NULL,
  documentation       TEXT COMMENT 'Full Markdown docs (examples/params)',
  requirements        JSON COMMENT 'Skill requirements: repo_types, min_access, llm_required',
  skill_code          TEXT COMMENT 'Python code (small skills) or NULL',
  code_ref            VARCHAR(255) COMMENT 'Large codebases: MatrixOne internal repo path',
  code_hash           VARCHAR(64) COMMENT 'SHA256 hash of skill code for verification',
  input_schema        JSON,
  output_schema       JSON,
  tools_required      JSON COMMENT 'Dependent tool IDs',
  safety_rules        JSON COMMENT '["no_pii", "max_tokens=500"]',
  tags                JSON COMMENT '["customer_service", "data_query"]',
  category            VARCHAR(64) DEFAULT 'general' COMMENT 'Skill category (github, code, docs, etc.)',
  subcategory         VARCHAR(64) DEFAULT 'default' COMMENT 'Skill subcategory',
  triggers            JSON COMMENT 'Keyword triggers for skill selection',
  dependencies        JSON COMMENT 'Dependent skill names',
  priority            INT DEFAULT 5 COMMENT 'Selection priority (1-10)',
  cost_estimate       VARCHAR(20) DEFAULT 'medium' COMMENT 'low | medium | high',
  is_active           BOOLEAN DEFAULT TRUE COMMENT 'Current active version',
  status              VARCHAR(20) DEFAULT 'active' COMMENT 'active | deprecated | experimental',
  created_at          TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  updated_at          TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  
  PRIMARY KEY (skill_id, version),
  UNIQUE KEY idx_skill_name_version (skill_name, version),
  INDEX idx_skill_active (skill_name, is_active),
  INDEX idx_skill_category (category, subcategory)
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

-- repos: Multi-repository management
CREATE TABLE IF NOT EXISTS repos (
  repo_id             VARCHAR(64) PRIMARY KEY,
  repo_url            VARCHAR(500) NOT NULL COMMENT 'Full GitHub URL',
  repo_type           VARCHAR(50) NOT NULL COMMENT 'code | ci | tester | docs',
  owner_id            VARCHAR(255) NOT NULL COMMENT 'user_id or tenant_id',
  owner_type          VARCHAR(50) NOT NULL COMMENT 'user | tenant',
  repo_group          VARCHAR(255) COMMENT 'Logical grouping (e.g., matrixone-project)',
  token_id            VARCHAR(64) COMMENT 'Reference to tokens table (NULL = use priority fallback)',
  access_scope        VARCHAR(50) NOT NULL COMMENT 'read | write | admin',
  metadata            JSON COMMENT '{"default_branch":"main", "ci_paths":[], "test_paths":[]}',
  is_active           BOOLEAN DEFAULT TRUE,
  created_at          TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  updated_at          TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  
  UNIQUE KEY idx_repo_url_owner (repo_url, owner_id),
  INDEX idx_owner (owner_id, owner_type),
  INDEX idx_repo_type (repo_type),
  INDEX idx_repo_group (repo_group),
  INDEX idx_token (token_id)
) COMMENT='Multi-repository management with per-repo tokens';

-- sandbox_metadata: Sandbox lifecycle and metadata
CREATE TABLE IF NOT EXISTS sandbox_metadata (
  sandbox_name        VARCHAR(255) PRIMARY KEY,
  description         TEXT,
  created_by          VARCHAR(255),
  created_at          TIMESTAMP(6) NOT NULL COMMENT 'Microsecond precision',
  updated_at          TIMESTAMP(6) NOT NULL COMMENT 'Microsecond precision',
  tags                JSON COMMENT 'e.g., ["experiment", "production"]',
  status              VARCHAR(50) DEFAULT 'active' COMMENT 'active | archived | expired',
  source_database     VARCHAR(255),
  source_snapshot     VARCHAR(255),
  
  INDEX idx_sandbox_created_at (created_at),
  INDEX idx_sandbox_updated_at (updated_at),
  INDEX idx_sandbox_status (status),
  INDEX idx_sandbox_created_by (created_by)
) COMMENT='Sandbox metadata and lifecycle management';

-- github_operations: GitHub API operation audit log
CREATE TABLE IF NOT EXISTS github_operations (
  operation_id        VARCHAR(64) PRIMARY KEY,
  event_id            VARCHAR(64) NOT NULL COMMENT 'Link to conversation_events',
  repo_id             VARCHAR(64) NOT NULL,
  operation_type      VARCHAR(100) NOT NULL COMMENT 'submit_pr_review | merge_pr | create_issue',
  operation_params    JSON NOT NULL COMMENT 'Operation parameters',
  token_id            VARCHAR(64) NOT NULL,
  is_dry_run          BOOLEAN DEFAULT FALSE COMMENT 'Distinguish simulation from real execution',
  status              VARCHAR(50) NOT NULL COMMENT 'success | failed | pending',
  response_code       INT,
  response_body       JSON,
  error_message       TEXT,
  executed_at         TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  duration_ms         INT,
  
  INDEX idx_event_id (event_id),
  INDEX idx_repo_id (repo_id, executed_at),
  INDEX idx_status (status),
  INDEX idx_dry_run (is_dry_run)
) COMMENT='GitHub API operation audit log for replay and compliance';

-- llm_call_logs: LLM API call logs for cost tracking
CREATE TABLE IF NOT EXISTS llm_call_logs (
  log_id              VARCHAR(64) PRIMARY KEY,
  event_id            VARCHAR(64) NOT NULL COMMENT 'Link to conversation_events',
  user_id             VARCHAR(255) NOT NULL,
  provider            VARCHAR(50) NOT NULL COMMENT 'openai | groq | anthropic',
  model               VARCHAR(100) NOT NULL,
  tokens_prompt       INT NOT NULL,
  tokens_completion   INT NOT NULL,
  tokens_total        INT NOT NULL,
  cost_usd            DECIMAL(10, 6) NOT NULL COMMENT 'Estimated cost in USD',
  latency_ms          INT NOT NULL,
  status              VARCHAR(50) NOT NULL COMMENT 'success | failed',
  error_message       TEXT,
  created_at          TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  metadata            JSON,
  
  INDEX idx_event_id (event_id),
  INDEX idx_user_id (user_id, created_at),
  INDEX idx_provider (provider),
  INDEX idx_status (status)
) COMMENT='LLM API call logs for cost tracking and analysis';

-- llm_pricing: Historical LLM pricing for accurate cost replay
CREATE TABLE IF NOT EXISTS llm_pricing (
  pricing_id          VARCHAR(64) PRIMARY KEY,
  provider            VARCHAR(50) NOT NULL COMMENT 'openai | groq | anthropic',
  model               VARCHAR(100) NOT NULL,
  price_per_1k_prompt DECIMAL(10, 6) NOT NULL COMMENT 'Price per 1K prompt tokens',
  price_per_1k_completion DECIMAL(10, 6) NOT NULL COMMENT 'Price per 1K completion tokens',
  effective_from      TIMESTAMP NOT NULL COMMENT 'Pricing effective start time',
  effective_to        TIMESTAMP COMMENT 'Pricing effective end time (NULL = current)',
  created_at          TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  
  INDEX idx_provider_model (provider, model),
  INDEX idx_effective (effective_from, effective_to)
) COMMENT='Historical LLM pricing for accurate cost calculation and replay';

-- context_snapshots: Store assembled context for debugging and replay
CREATE TABLE IF NOT EXISTS context_snapshots (
  snapshot_id         VARCHAR(36) PRIMARY KEY COMMENT 'ULID',
  session_id          VARCHAR(36) NOT NULL,
  event_id            VARCHAR(36) COMMENT 'Event that triggered this context',
  created_at          TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  
  -- Context content
  system_prompt       TEXT COMMENT 'System instructions',
  skill_definitions   JSON COMMENT 'Available skills',
  selected_events     JSON COMMENT 'Array of {event_id, score, reason}',
  code_context        JSON COMMENT 'Code files/functions included',
  documentation       JSON COMMENT 'Documentation sections included',
  
  -- Metadata
  total_tokens        INT COMMENT 'Total tokens in assembled context',
  token_budget        JSON COMMENT 'Budget breakdown by component',
  assembly_time_ms    INT COMMENT 'Time to assemble context',
  relevance_scores    JSON COMMENT 'Detailed scoring for each event',
  task_type           VARCHAR(50) COMMENT 'code_review | planning | debugging | general',
  
  -- LLM call linkage
  llm_request_id      VARCHAR(128) COMMENT 'LLM provider request ID',
  llm_response_id     VARCHAR(128) COMMENT 'LLM provider response ID',
  
  INDEX idx_session (session_id),
  INDEX idx_event (event_id),
  INDEX idx_created (created_at),
  INDEX idx_task_type (task_type)
) COMMENT='Context snapshots for debugging and replay';

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
echo "  - repos (multi-repository management)"
echo "  - sandbox_metadata (sandbox lifecycle)"
echo "  - github_operations (GitHub API audit log)"
echo "  - llm_call_logs (LLM cost tracking)"
echo "  - llm_pricing (historical LLM pricing)"
echo "  - context_snapshots (context debugging and replay)"
echo ""
echo "Next steps:"
echo "  1. Verify: make db-connect"
echo "  2. Test: make test-unit"

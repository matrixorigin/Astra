-- Agent Configuration Database Initialization
-- This script creates tables for scope-based configuration management

-- Create agent_config database
CREATE DATABASE IF NOT EXISTS agent_config;

-- Model registry with extensible scope support
CREATE TABLE IF NOT EXISTS agent_config.model_registry (
  config_id           VARCHAR(64) PRIMARY KEY,
  scope_type          VARCHAR(32) NOT NULL COMMENT 'global|account|user|project|repo|region|environment...',
  scope_id            VARCHAR(255) COMMENT 'NULL for global, or specific ID',
  model_name          VARCHAR(255) NOT NULL,
  provider            VARCHAR(64) NOT NULL COMMENT 'openai|anthropic|groq',
  config_json         TEXT NOT NULL COMMENT 'Model configuration',
  created_by          VARCHAR(255) NOT NULL,
  created_at          TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  updated_at          TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  
  UNIQUE KEY idx_scope_model (scope_type, scope_id, model_name),
  INDEX idx_scope (scope_type, scope_id),
  INDEX idx_created_by (created_by)
) COMMENT='Model registry with scope-based access control';

-- Skills registry with extensible scope support
CREATE TABLE IF NOT EXISTS agent_config.skills_registry (
  skill_id            VARCHAR(64) PRIMARY KEY,
  skill_name          VARCHAR(255) NOT NULL,
  scope_type          VARCHAR(32) NOT NULL COMMENT 'global|account|user',
  scope_id            VARCHAR(255) COMMENT 'NULL for global, account_id or user_id',
  skill_type          VARCHAR(64) NOT NULL COMMENT 'builtin|python|sql|external',
  description         TEXT,
  parameters_schema   TEXT COMMENT 'JSON Schema for parameters',
  implementation      TEXT COMMENT 'Code or reference',
  is_active           BOOLEAN DEFAULT TRUE,
  created_by          VARCHAR(255) NOT NULL,
  created_at          TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  updated_at          TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  
  UNIQUE KEY idx_scope_skill (scope_type, scope_id, skill_name),
  INDEX idx_scope (scope_type, scope_id),
  INDEX idx_created_by (created_by),
  INDEX idx_active (is_active)
) COMMENT='Skills registry with scope-based access control';

-- API tokens with extensible scope support
CREATE TABLE IF NOT EXISTS agent_config.api_tokens (
  token_id            VARCHAR(64) PRIMARY KEY,
  token_type          VARCHAR(32) NOT NULL COMMENT 'llm|repo',
  provider            VARCHAR(64) NOT NULL COMMENT 'openai|anthropic|groq|github',
  scope_type          VARCHAR(32) NOT NULL COMMENT 'global|account|user|project|repo...',
  scope_id            VARCHAR(255) COMMENT 'NULL for global, or specific ID',
  encrypted_value     TEXT NOT NULL COMMENT 'Encrypted API key',
  is_active           BOOLEAN DEFAULT TRUE,
  created_by          VARCHAR(255) NOT NULL,
  created_at          TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  expires_at          TIMESTAMP COMMENT 'Token expiration time',
  
  INDEX idx_scope (scope_type, scope_id, token_type),
  INDEX idx_provider (provider, is_active),
  INDEX idx_created_by (created_by)
) COMMENT='API tokens with scope-based access control';

-- Audit logs for all administrative operations
CREATE TABLE IF NOT EXISTS agent_config.audit_logs (
  log_id              VARCHAR(64) PRIMARY KEY,
  user_id             VARCHAR(255) NOT NULL,
  action              VARCHAR(64) NOT NULL COMMENT 'create_model|update_model|delete_model|create_skill...',
  resource_type       VARCHAR(64) NOT NULL COMMENT 'model|token|skill|user|role',
  resource_id         VARCHAR(255),
  scope_type          VARCHAR(32),
  scope_id            VARCHAR(255),
  old_value           TEXT COMMENT 'Before change',
  new_value           TEXT COMMENT 'After change',
  status              VARCHAR(32) DEFAULT 'success' COMMENT 'success|failed|denied',
  error_message       TEXT,
  created_at          TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  
  INDEX idx_user_time (user_id, created_at),
  INDEX idx_action (action, created_at),
  INDEX idx_resource (resource_type, resource_id),
  INDEX idx_status (status, created_at)
) COMMENT='Audit trail for all administrative operations';

-- Insert default global models
INSERT INTO agent_config.model_registry (config_id, scope_type, scope_id, model_name, provider, config_json, created_by) VALUES
('model_global_gpt4o', 'global', NULL, 'gpt-4o', 'openai', 
 '{"context_window": 128000, "price_per_1k_prompt": 0.0025, "price_per_1k_completion": 0.01, "rpm_limit": 500, "tpm_limit": 150000}', 
 'system'),
('model_global_gpt4o_mini', 'global', NULL, 'gpt-4o-mini', 'openai',
 '{"context_window": 128000, "price_per_1k_prompt": 0.00015, "price_per_1k_completion": 0.0006, "rpm_limit": 1000, "tpm_limit": 200000}',
 'system'),
('model_global_claude35', 'global', NULL, 'claude-3-5-sonnet-20241022', 'anthropic',
 '{"context_window": 200000, "price_per_1k_prompt": 0.003, "price_per_1k_completion": 0.015, "rpm_limit": 50, "tpm_limit": 40000}',
 'system');

-- Note: GRANT statements should be run after roles are created
-- See init-rbac.sql for role creation and privilege grants

SELECT 'Agent configuration database initialized successfully' AS status;

"""Database schema initialization for agent engine."""

-- Users table
CREATE TABLE IF NOT EXISTS users (
  user_id           VARCHAR(64) PRIMARY KEY,
  username          VARCHAR(255) UNIQUE NOT NULL,
  email             VARCHAR(255) UNIQUE NOT NULL,
  password_hash     VARCHAR(255) NOT NULL,
  display_name      VARCHAR(255),
  is_active         BOOLEAN DEFAULT TRUE,
  is_verified       BOOLEAN DEFAULT FALSE,
  created_at        TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  updated_at        TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  
  INDEX idx_username (username),
  INDEX idx_email (email),
  INDEX idx_active (is_active)
) COMMENT='Application users';

-- Refresh tokens table
CREATE TABLE IF NOT EXISTS refresh_tokens (
  token_id          VARCHAR(64) PRIMARY KEY,
  user_id           VARCHAR(64) NOT NULL,
  token_hash        VARCHAR(255) NOT NULL,
  expires_at        TIMESTAMP NOT NULL,
  is_revoked        BOOLEAN DEFAULT FALSE,
  created_at        TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  
  INDEX idx_user (user_id),
  INDEX idx_expires (expires_at),
  INDEX idx_revoked (is_revoked),
  FOREIGN KEY (user_id) REFERENCES users(user_id) ON DELETE CASCADE
) COMMENT='Refresh tokens for authentication';

-- Agents table
CREATE TABLE IF NOT EXISTS agents (
  agent_id          VARCHAR(64) PRIMARY KEY,
  agent_name        VARCHAR(255) NOT NULL,
  agent_type        VARCHAR(64) DEFAULT 'chatbot',
  owner_user_id     VARCHAR(64) NOT NULL,
  config            JSON,
  is_active         BOOLEAN DEFAULT TRUE,
  created_at        TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  updated_at        TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  
  INDEX idx_owner (owner_user_id),
  INDEX idx_type (agent_type),
  INDEX idx_active (is_active),
  FOREIGN KEY (owner_user_id) REFERENCES users(user_id) ON DELETE CASCADE
) COMMENT='Agent registry';

-- Sessions table
CREATE TABLE IF NOT EXISTS sessions (
  session_id        VARCHAR(64) PRIMARY KEY,
  agent_id          VARCHAR(64) NOT NULL,
  user_id           VARCHAR(64) NOT NULL,
  status            VARCHAR(32) DEFAULT 'active',
  metadata          JSON,
  created_at        TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  updated_at        TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  
  INDEX idx_agent (agent_id),
  INDEX idx_user (user_id),
  INDEX idx_status (status),
  FOREIGN KEY (agent_id) REFERENCES agents(agent_id) ON DELETE CASCADE,
  FOREIGN KEY (user_id) REFERENCES users(user_id) ON DELETE CASCADE
) COMMENT='Chat sessions';

-- Events table
CREATE TABLE IF NOT EXISTS events (
  event_id          VARCHAR(64) PRIMARY KEY,
  agent_id          VARCHAR(64) NOT NULL,
  user_id           VARCHAR(64) NOT NULL,
  session_id        VARCHAR(64) NOT NULL,
  event_type        VARCHAR(32) NOT NULL,
  content           TEXT,
  metadata          JSON,
  created_at        TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  
  INDEX idx_agent_created (agent_id, created_at),
  INDEX idx_session_created (session_id, created_at),
  INDEX idx_user_created (user_id, created_at),
  INDEX idx_type (event_type),
  FOREIGN KEY (agent_id) REFERENCES agents(agent_id) ON DELETE CASCADE,
  FOREIGN KEY (user_id) REFERENCES users(user_id) ON DELETE CASCADE,
  FOREIGN KEY (session_id) REFERENCES sessions(session_id) ON DELETE CASCADE
) COMMENT='All agent events';

-- Skills table
CREATE TABLE IF NOT EXISTS skills (
  skill_id          VARCHAR(64) PRIMARY KEY,
  skill_name        VARCHAR(255) NOT NULL,
  owner_user_id     VARCHAR(64),
  skill_type        VARCHAR(64),
  definition        JSON NOT NULL,
  is_active         BOOLEAN DEFAULT TRUE,
  created_at        TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  updated_at        TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  
  INDEX idx_owner (owner_user_id),
  INDEX idx_type (skill_type),
  INDEX idx_active (is_active),
  FOREIGN KEY (owner_user_id) REFERENCES users(user_id) ON DELETE SET NULL
) COMMENT='Skill library';

-- Context snapshots table
CREATE TABLE IF NOT EXISTS context_snapshots (
  snapshot_id       VARCHAR(64) PRIMARY KEY,
  session_id        VARCHAR(64) NOT NULL,
  agent_id          VARCHAR(64) NOT NULL,
  snapshot_data     JSON NOT NULL,
  created_at        TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  
  INDEX idx_session_created (session_id, created_at),
  INDEX idx_agent_created (agent_id, created_at),
  FOREIGN KEY (session_id) REFERENCES sessions(session_id) ON DELETE CASCADE,
  FOREIGN KEY (agent_id) REFERENCES agents(agent_id) ON DELETE CASCADE
) COMMENT='Context snapshots for debugging';

# Authentication and Authorization System Design

## Overview

This document describes the production-grade authentication and authorization system for mo-agent-engine. The system is designed to work **without HTTP server** - CLI directly connects to MatrixOne database with built-in multi-tenancy support.

## Design Principles

1. **No HTTP Server Required**: CLI directly connects to database
2. **Multi-Tenancy Native**: Leverage MatrixOne's tenant isolation
3. **Shared Platform + Private Data**: Core platform in sys tenant, agent-specific data in separate tenants
4. **Standard Auth**: API Keys for CLI, JWT for future web UI
5. **Fine-grained Permissions**: Role-based + resource-based access control
6. **Audit Trail**: All auth events logged for compliance

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│  Client Layer                                               │
│  - CLI (API Key in ~/.mo-agent/config.json)                │
│  - Future: Web UI (JWT in cookie/localStorage)             │
│  - Future: SDK (API Key in code)                           │
└─────────────────────┬───────────────────────────────────────┘
                      │ Direct Database Connection
┌─────────────────────▼───────────────────────────────────────┐
│  MatrixOne Cluster                                          │
│                                                             │
│  ┌─────────────────────────────────────────────────────┐   │
│  │ sys Tenant (Platform Core)                          │   │
│  │  - app_users (all users)                            │   │
│  │  - app_roles (roles & permissions)                  │   │
│  │  - api_keys (authentication)                        │   │
│  │  - conversation_events (shared events)              │   │
│  │  - skills (shared skill library)                    │   │
│  │  - context_snapshots (shared context)               │   │
│  │  - models, tokens, configs (platform config)        │   │
│  └─────────────────────────────────────────────────────┘   │
│                                                             │
│  ┌─────────────────────────────────────────────────────┐   │
│  │ agent_alice Tenant (Alice's Private Space)         │   │
│  │  - private_data (Alice's private data)              │   │
│  │  - custom_skills (Alice's custom skills)            │   │
│  │  - experiments (sandbox branches)                   │   │
│  └─────────────────────────────────────────────────────┘   │
│                                                             │
│  ┌─────────────────────────────────────────────────────┐   │
│  │ agent_bob Tenant (Bob's Private Space)             │   │
│  │  - private_data                                     │   │
│  │  - experiments                                      │   │
│  └─────────────────────────────────────────────────────┘   │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

## Multi-Tenancy Model

### Tenant Types

1. **Platform Tenant (sys)**
   - Stores all shared platform data
   - Users, roles, permissions
   - Shared events, skills, context
   - Platform configuration

2. **Agent Tenant (agent_xxx)**
   - Each agent can have dedicated tenant
   - Private data isolation
   - Independent experiments (sandbox/branch)
   - Full MatrixOne capabilities (HTAP, Git for Data)

### Data Sharing Patterns

```sql
-- Pattern 1: Shared Platform Data (in sys tenant)
-- All agents read/write to sys.conversation_events
INSERT INTO sys.conversation_events (event_id, user_id, content, ...)
VALUES (...);

-- Pattern 2: Private Agent Data (in agent tenant)
-- Alice's private experiments
CREATE DATABASE agent_alice.experiments;
CREATE SNAPSHOT agent_alice.exp_v1 FOR DATABASE experiments;

-- Pattern 3: Cross-Tenant Pub/Sub (future)
-- Agent A publishes event, Agent B subscribes
PUBLISH TO sys.event_stream VALUES (...);
SUBSCRIBE FROM sys.event_stream WHERE user_id = 'bob';

-- Pattern 4: Git for Data
-- Alice branches for experiment
CREATE DATABASE agent_alice.exp_branch FROM agent_alice.main;
-- Success -> merge back
-- Failure -> drop branch
```

## Authentication Flow (No HTTP Server)

### CLI Authentication

```
1. User runs: mo-agent login --username alice
2. CLI prompts for password
3. CLI connects to sys tenant database
4. CLI validates password against sys.app_users
5. CLI generates API key and stores in sys.api_keys
6. CLI saves API key to ~/.mo-agent/config.json
7. Future commands read API key from config file
8. Each command validates API key against sys.api_keys
```

### API Key Validation

```python
# In every CLI command
def authenticate():
    # 1. Read API key from config
    config = load_config("~/.mo-agent/config.json")
    api_key = config.get("api_key")
    
    # 2. Connect to sys tenant
    db = Database(tenant="sys")
    
    # 3. Validate API key
    key_hash = hashlib.sha256(api_key.encode()).hexdigest()
    row = db.fetchone(
        "SELECT user_id, scopes FROM api_keys WHERE key_hash = %s AND is_active = TRUE",
        (key_hash,)
    )
    
    if not row:
        raise AuthenticationError("Invalid API key")
    
    # 4. Return user context
    return {
        "user_id": row["user_id"],
        "scopes": json.loads(row["scopes"])
    }
```

## Data Model

### Core Tables (in sys tenant)

```sql
-- Users: Application-level user accounts
CREATE TABLE app_users (
  user_id           VARCHAR(64) PRIMARY KEY,
  username          VARCHAR(255) UNIQUE NOT NULL,
  email             VARCHAR(255) UNIQUE,
  password_hash     VARCHAR(255),           -- bcrypt hash
  display_name      VARCHAR(255),
  avatar_url        VARCHAR(500),
  tenant_id         VARCHAR(64),            -- Multi-tenancy support
  is_active         BOOLEAN DEFAULT TRUE,
  is_verified       BOOLEAN DEFAULT FALSE,
  last_login_at     TIMESTAMP,
  created_at        TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  updated_at        TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  metadata          JSON,                   -- Extensible user attributes
  
  INDEX idx_tenant (tenant_id),
  INDEX idx_email (email),
  INDEX idx_active (is_active)
) COMMENT='Application users';

-- Roles: Permission templates
CREATE TABLE app_roles (
  role_id           VARCHAR(64) PRIMARY KEY,
  role_name         VARCHAR(64) UNIQUE NOT NULL,
  display_name      VARCHAR(255),
  description       TEXT,
  permissions       JSON NOT NULL,          -- Array of permission strings
  is_system         BOOLEAN DEFAULT FALSE,  -- System roles cannot be deleted
  tenant_id         VARCHAR(64),            -- NULL = global role
  created_at        TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  updated_at        TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  
  INDEX idx_tenant (tenant_id),
  UNIQUE KEY uk_role_tenant (role_name, tenant_id)
) COMMENT='Roles with permissions';

-- User-Role assignments
CREATE TABLE app_user_roles (
  user_id           VARCHAR(64) NOT NULL,
  role_id           VARCHAR(64) NOT NULL,
  scope_type        VARCHAR(32),            -- 'global' | 'tenant' | 'project' | 'repo'
  scope_id          VARCHAR(255),           -- NULL for global
  granted_by        VARCHAR(64),
  granted_at        TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  expires_at        TIMESTAMP,              -- Optional expiration
  
  PRIMARY KEY (user_id, role_id, scope_type, scope_id),
  INDEX idx_user (user_id),
  INDEX idx_role (role_id),
  INDEX idx_scope (scope_type, scope_id)
) COMMENT='User role assignments with scope';

-- API Keys: For CLI and programmatic access
CREATE TABLE api_keys (
  key_id            VARCHAR(64) PRIMARY KEY,
  user_id           VARCHAR(64) NOT NULL,
  key_prefix        VARCHAR(16) NOT NULL,   -- First 8 chars for identification
  key_hash          VARCHAR(255) NOT NULL,  -- SHA-256 hash of full key
  name              VARCHAR(255),           -- User-friendly name
  scopes            JSON,                   -- Restricted permissions
  rate_limit        INT,                    -- Requests per minute
  last_used_at      TIMESTAMP,
  expires_at        TIMESTAMP,
  is_active         BOOLEAN DEFAULT TRUE,
  created_at        TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  
  INDEX idx_user (user_id),
  INDEX idx_prefix (key_prefix),
  INDEX idx_active (is_active)
) COMMENT='API keys for authentication';

-- Sessions: For web UI (optional, can use stateless JWT)
CREATE TABLE app_sessions (
  session_id        VARCHAR(64) PRIMARY KEY,
  user_id           VARCHAR(64) NOT NULL,
  token_hash        VARCHAR(255) NOT NULL,
  ip_address        VARCHAR(45),
  user_agent        VARCHAR(500),
  expires_at        TIMESTAMP NOT NULL,
  created_at        TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  last_activity_at  TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  
  INDEX idx_user (user_id),
  INDEX idx_expires (expires_at)
) COMMENT='User sessions';

-- Auth Events: Audit trail
CREATE TABLE auth_events (
  event_id          VARCHAR(64) PRIMARY KEY,
  event_type        VARCHAR(32) NOT NULL,   -- 'login' | 'logout' | 'token_created' | 'permission_denied'
  user_id           VARCHAR(64),
  username          VARCHAR(255),           -- For failed login attempts
  ip_address        VARCHAR(45),
  user_agent        VARCHAR(500),
  success           BOOLEAN,
  failure_reason    VARCHAR(255),
  metadata          JSON,
  created_at        TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  
  INDEX idx_user (user_id, created_at),
  INDEX idx_type (event_type, created_at),
  INDEX idx_ip (ip_address, created_at)
) COMMENT='Authentication and authorization audit log';
```

## Multi-Tenancy Benefits

### 1. Shared Platform Core (sys tenant)

**Advantages**:
- All agents share events, skills, context
- Real-time collaboration via pub/sub
- Unified analytics and monitoring
- Single source of truth

**Use Cases**:
- Agent A learns from Agent B's conversations
- Shared skill library across all agents
- Cross-agent knowledge graph
- Platform-wide audit trail

### 2. Private Agent Tenants

**Advantages**:
- Complete data isolation
- Independent experiments (sandbox/branch)
- Full MatrixOne capabilities per agent
- No interference between agents

**Use Cases**:
- Alice experiments with new prompts (branch → test → merge/drop)
- Bob's private customer data (GDPR compliance)
- Sandbox validation before production
- Per-agent cost tracking

### 3. Git for Data Workflows

```sql
-- Alice wants to test a new skill
-- 1. Create branch
CREATE DATABASE agent_alice.exp_new_skill FROM agent_alice.main;

-- 2. Test in branch
USE agent_alice.exp_new_skill;
-- ... run experiments ...

-- 3. Success? Merge back
-- (MatrixOne native merge support)

-- 4. Failure? Drop branch
DROP DATABASE agent_alice.exp_new_skill;
```

### 4. HTAP Analytics

```sql
-- Real-time analytics across all agents
SELECT 
  user_id,
  COUNT(*) as total_queries,
  AVG(response_time_ms) as avg_latency
FROM sys.conversation_events
WHERE created_at > NOW() - INTERVAL 1 HOUR
GROUP BY user_id;

-- Cross-agent knowledge graph
SELECT 
  e1.user_id as agent_a,
  e2.user_id as agent_b,
  COUNT(*) as shared_topics
FROM sys.conversation_events e1
JOIN sys.conversation_events e2 
  ON e1.topic = e2.topic
WHERE e1.user_id != e2.user_id
GROUP BY e1.user_id, e2.user_id;
```

## Tenant Management

### Creating Agent Tenant

```sql
-- Admin creates tenant for new agent
CREATE ACCOUNT agent_alice ADMIN_NAME 'alice' IDENTIFIED BY 'password';

-- Grant access to shared platform data
GRANT SELECT, INSERT ON sys.conversation_events TO agent_alice;
GRANT SELECT ON sys.skills TO agent_alice;
GRANT SELECT ON sys.models TO agent_alice;

-- Agent creates private databases
USE agent_alice;
CREATE DATABASE private_data;
CREATE DATABASE experiments;
```

### Tenant Isolation

```python
class Database:
    def __init__(self, user_id: str = None):
        # 1. Authenticate user
        auth = authenticate()
        
        # 2. Determine tenant
        if auth["user_id"] in PLATFORM_ADMINS:
            # Admin can access sys tenant
            self.tenant = "sys"
        else:
            # Regular user uses agent tenant
            self.tenant = f"agent_{auth['user_id']}"
        
        # 3. Connect to appropriate tenant
        self.conn = connect(
            host="localhost",
            port=6001,
            user=self.tenant,
            password=get_tenant_password(self.tenant),
            database=self.tenant
        )
    
    def query_shared_events(self):
        # Access shared platform data
        return self.execute("SELECT * FROM sys.conversation_events")
    
    def query_private_data(self):
        # Access private tenant data
        return self.execute(f"SELECT * FROM {self.tenant}.private_data")
```

## Permission Model (Updated)

### Permission Format

```
<resource>:<action>:<scope>:<tenant>
```

Examples:
- `event:create:global:sys` - Create events in sys tenant
- `skill:read:tenant:agent_alice` - Read skills in Alice's tenant
- `data:write:private:agent_alice` - Write to Alice's private data
- `*:*:*:*` - Super admin (all permissions, all tenants)

### Tenant-Aware Permissions

```python
def check_permission(user_id: str, permission: str, target_tenant: str) -> bool:
    """
    Check if user has permission for resource in target tenant.
    
    Args:
        user_id: User ID
        permission: e.g., "event:create:global:sys"
        target_tenant: e.g., "sys" or "agent_alice"
    
    Returns:
        True if authorized
    """
    # 1. Get user's tenant
    user_tenant = f"agent_{user_id}"
    
    # 2. Check if accessing own tenant
    if target_tenant == user_tenant:
        # Users have full access to their own tenant
        return True
    
    # 3. Check if accessing sys tenant (shared platform)
    if target_tenant == "sys":
        # Check specific permission
        return has_permission(user_id, permission)
    
    # 4. Cross-tenant access requires explicit permission
    return has_permission(user_id, f"{permission}:{target_tenant}")
```

## CLI Configuration

### Config File Structure

```json
{
  "api_key": "moa_abc123...",
  "user_id": "alice",
  "tenant": "agent_alice",
  "platform_url": "localhost:6001",
  "default_database": "agent_alice.main"
}
```

### CLI Commands

```bash
# Login (creates API key, saves to config)
mo-agent login --username alice

# Use shared platform data
mo-agent chat --user-id alice
# → Reads from sys.conversation_events
# → Writes to sys.conversation_events

# Use private tenant data
mo-agent experiment --branch new_feature
# → Creates agent_alice.exp_new_feature
# → Isolated from main

# Cross-tenant collaboration (if permitted)
mo-agent share-skill --to bob --skill-id skill_123
# → Copies skill from agent_alice to agent_bob
```

## Data Flow Examples

### Example 1: Normal Chat

```
1. User: mo-agent chat "Hello"
2. CLI reads API key from ~/.mo-agent/config.json
3. CLI connects to sys tenant
4. CLI validates API key → gets user_id = "alice"
5. CLI writes event to sys.conversation_events
6. CLI reads context from sys.context_snapshots
7. CLI calls LLM
8. CLI writes response to sys.conversation_events
9. All agents can see this conversation (shared)
```

### Example 2: Private Experiment

```
1. User: mo-agent experiment --branch test_prompt
2. CLI creates agent_alice.exp_test_prompt
3. CLI copies relevant data to branch
4. User tests new prompt in branch
5. Success? CLI merges back to agent_alice.main
6. Failure? CLI drops branch
7. No impact on other agents
```

### Example 3: Cross-Agent Learning

```
1. Agent Alice has successful conversation
2. Event stored in sys.conversation_events
3. Agent Bob queries sys.conversation_events
4. Bob learns from Alice's patterns
5. Bob improves own responses
6. Real-time knowledge sharing via pub/sub
```

### Permission Format

```
<resource>:<action>:<scope>
```

Examples:
- `model:create:global` - Create global models
- `model:read:tenant` - Read tenant models
- `token:manage:user` - Manage own tokens
- `chat:execute:*` - Execute chat in any scope
- `*:*:*` - Super admin (all permissions)

### Built-in Roles

```json
{
  "super_admin": {
    "permissions": ["*:*:*"],
    "description": "Full system access"
  },
  "tenant_admin": {
    "permissions": [
      "model:*:tenant",
      "token:*:tenant",
      "user:*:tenant",
      "chat:*:tenant"
    ],
    "description": "Tenant administrator"
  },
  "developer": {
    "permissions": [
      "model:read:tenant",
      "token:manage:user",
      "chat:execute:*",
      "skill:execute:*"
    ],
    "description": "Developer with chat access"
  },
  "viewer": {
    "permissions": [
      "model:read:tenant",
      "chat:read:tenant"
    ],
    "description": "Read-only access"
  }
}
```

## Authentication Flow

### 1. Password Login (Web UI)

```
1. User submits username + password
2. Backend validates credentials
3. Generate JWT with claims:
   {
     "sub": "user_id",
     "username": "alice",
     "tenant_id": "acme",
     "roles": ["developer"],
     "exp": 1234567890
   }
4. Return JWT to client
5. Client includes JWT in Authorization header
```

### 2. API Key Authentication (CLI)

```
1. User creates API key via web UI or CLI
2. System generates key: "moa_" + random(32) + checksum
3. Store key_prefix (first 8 chars) and key_hash (SHA-256)
4. Return full key to user (only shown once)
5. User stores key in ~/.mo-agent/config.json
6. CLI includes key in X-API-Key header
7. Backend validates by:
   - Extract key_prefix
   - Lookup key_hash
   - Compare SHA-256(provided_key) == stored_hash
```

### 3. Service Account (Internal)

```
1. System creates service account with long-lived token
2. Used for background jobs, webhooks, etc.
3. Stored in environment variables or secret manager
```

## Authorization Flow

```python
def check_permission(user_id: str, permission: str, resource_scope: dict) -> bool:
    """
    Check if user has permission for resource.
    
    Args:
        user_id: User ID
        permission: e.g., "model:create:global"
        resource_scope: e.g., {"type": "tenant", "id": "acme"}
    
    Returns:
        True if authorized
    """
    # 1. Load user roles with scope
    roles = get_user_roles(user_id, resource_scope)
    
    # 2. Collect all permissions from roles
    all_permissions = []
    for role in roles:
        all_permissions.extend(role.permissions)
    
    # 3. Check if any permission matches
    for perm in all_permissions:
        if permission_matches(perm, permission, resource_scope):
            return True
    
    # 4. Log denial
    log_auth_event("permission_denied", user_id, permission)
    return False
```

## Scope Hierarchy

```
global
  └─ tenant (account)
      └─ project
          └─ repo
              └─ user
```

Permission inheritance:
- `global` scope grants access to all lower scopes
- `tenant` scope grants access to all projects/repos in tenant
- `user` scope only grants access to user's own resources

## Security Considerations

### Password Storage
- Use bcrypt with cost factor 12
- Never store plaintext passwords
- Implement password complexity requirements

### Token Security
- JWT: Sign with RS256 (asymmetric), rotate keys regularly
- API Keys: SHA-256 hash, never store plaintext
- Session tokens: Random 32-byte, hash before storage

### Rate Limiting
- Per user: 100 req/min (configurable)
- Per API key: Custom limits
- Failed login attempts: 5 attempts, then 15-minute lockout

### Audit Trail
- Log all authentication events
- Log all permission denials
- Retain logs for 90 days (configurable)

## Implementation Phases

### Phase 1: Core Auth + Multi-Tenancy (Week 1)
- [ ] Create sys tenant schema (app_users, app_roles, api_keys)
- [ ] Implement tenant management (create agent tenants)
- [ ] Implement API key authentication (no password for MVP)
- [ ] Update Database SDK to be tenant-aware
- [ ] Basic permission checking

### Phase 2: CLI Integration (Week 2)
- [ ] CLI login/logout commands
- [ ] Store API key in ~/.mo-agent/config.json
- [ ] Update all CLI commands to use API key
- [ ] Tenant-aware operations (sys vs agent tenant)

### Phase 3: RBAC + Permissions (Week 3)
- [ ] Role management (sys.app_roles)
- [ ] Tenant-aware permission checking
- [ ] Cross-tenant access control
- [ ] Built-in roles (super_admin, tenant_admin, developer, viewer)

### Phase 4: Git for Data Integration (Week 4)
- [ ] Experiment commands (create/merge/drop branches)
- [ ] Cross-tenant sharing (share-skill, share-context)
- [ ] Pub/Sub foundation for real-time collaboration
- [ ] Audit event logging
- [ ] Failed login protection
- [ ] Security headers

## Migration from Current System

### Step 1: Create new tables
```sql
-- Run migration script
source infra/scripts/init-auth.sql
```

### Step 2: Migrate existing users
```python
# Create app_users from existing user_ids
for user_id in existing_users:
    create_app_user(
        user_id=user_id,
        username=user_id,  # Use user_id as username initially
        tenant_id="default"
    )
```

### Step 3: Assign default roles
```python
# All existing users get "developer" role
for user in app_users:
    assign_role(user.user_id, "developer", scope="global")
```

### Step 4: Update PermissionChecker
```python
# Replace MatrixOne RBAC queries with app_roles queries
# See implementation in core/auth/permission_checker.py
```

### Step 5: Update CLI
```python
# Add API key authentication
# Store key in ~/.mo-agent/config.json
```

## API Endpoints

### Authentication
- `POST /auth/login` - Password login
- `POST /auth/logout` - Logout
- `POST /auth/refresh` - Refresh JWT
- `POST /auth/register` - User registration (if enabled)

### API Keys
- `POST /api-keys` - Create API key
- `GET /api-keys` - List user's API keys
- `DELETE /api-keys/{key_id}` - Revoke API key

### Users (Admin only)
- `GET /users` - List users
- `POST /users` - Create user
- `PUT /users/{user_id}` - Update user
- `DELETE /users/{user_id}` - Delete user

### Roles (Admin only)
- `GET /roles` - List roles
- `POST /roles` - Create role
- `PUT /roles/{role_id}` - Update role
- `DELETE /roles/{role_id}` - Delete role

### Permissions
- `GET /users/{user_id}/permissions` - Get user permissions
- `POST /users/{user_id}/roles` - Assign role to user
- `DELETE /users/{user_id}/roles/{role_id}` - Remove role from user

## Configuration

```yaml
# config/auth.yaml
auth:
  jwt:
    algorithm: RS256
    access_token_ttl: 3600      # 1 hour
    refresh_token_ttl: 604800   # 7 days
    issuer: mo-agent
    
  api_keys:
    prefix: moa_
    length: 32
    default_rate_limit: 100     # per minute
    
  password:
    min_length: 8
    require_uppercase: true
    require_lowercase: true
    require_digit: true
    require_special: false
    bcrypt_cost: 12
    
  session:
    ttl: 86400                  # 24 hours
    cleanup_interval: 3600      # 1 hour
    
  rate_limit:
    default: 100                # per minute
    login: 5                    # per 15 minutes
    
  audit:
    retention_days: 90
    log_success: true
    log_failure: true
```

## Testing Strategy

### Unit Tests
- Password hashing/verification
- JWT generation/validation
- API key generation/validation
- Permission matching logic

### Integration Tests
- Login flow
- API key authentication
- Permission checking with scopes
- Role assignment

### Security Tests
- SQL injection attempts
- JWT tampering
- Brute force protection
- Rate limiting

## Monitoring

### Metrics
- Login success/failure rate
- API key usage
- Permission denial rate
- Token expiration rate

### Alerts
- High failure rate (> 10% in 5 min)
- Brute force attempts detected
- Unusual API key usage pattern
- Token validation errors

## References

- [JWT Best Practices](https://tools.ietf.org/html/rfc8725)
- [OWASP Authentication Cheat Sheet](https://cheatsheetseries.owasp.org/cheatsheets/Authentication_Cheat_Sheet.html)
- [NIST Password Guidelines](https://pages.nist.gov/800-63-3/sp800-63b.html)

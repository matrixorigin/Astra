# Admin & User Management Design

**Status**: Draft  
**Version**: 1.1  
**Last Updated**: 2026-02-11

## Problem Statement

Current system has the following issues:

1. **Permission Confusion**: `mo-agent` CLI can directly modify global configurations (model registry, tokens), which should require admin privileges
2. **Missing Role System**: No clear admin/user/viewer role separation
3. **Missing Admin Tools**: No dedicated admin CLI or API for system configuration management
4. **Security Risk**: Regular users can modify configurations that affect all users

## Design Goals

1. **Separation of Concerns**: Agent CLI (users) vs Admin CLI (administrators)
2. **Principle of Least Privilege**: Users can only access resources within their scope
3. **Audit Trail**: All administrative operations logged for compliance
4. **Backward Compatibility**: Don't break existing functionality
5. **Leverage MatrixOne RBAC**: Use MatrixOne's built-in role and privilege system instead of reinventing the wheel
6. **Extensible Scoping**: Support diverse business dimensions (User, Project, Repo, Team) beyond just Tenant/User.

---

## 3. Architectural Decisions

Before diving into implementation details, we establish the following core architectural decisions that define the system's nature, boundaries, and security model.

### 3.1 Agent Architecture Taxonomy

We adopt a three-layer taxonomy to clarify the definition of "Agent" vs "Platform":

1.  **Platform Capabilities (The Kernel)**:
    *   **Definition**: The underlying "Operating System" capabilities provided by the core framework. These are passive APIs, not active agents.
    *   **Components**: Event Bus, Time Machine (Time Travel), Sandbox (Isolation), Memory Manager, Scope Resolver.
    *   **Role**: Provides the laws of physics (Time, Space, Memory) for agents to live in.

2.  **System Agents (The Daemons)**:
    *   **Definition**: Pre-installed, autonomous agents that maintain system health and perform background tasks. They run automatically based on triggers.
    *   **Components**: Regression Agent (auto-test on change), Audit Agent (security scan), Tuning Agent (prompt optimization).
    *   **Role**: Like system daemons (cron, logrotate), ensuring the platform remains stable and self-improving.

3.  **User Agents (The Apps)**:
    *   **Definition**: Business-specific agents triggered by user actions to solve domain problems.
    *   **Components**: Code Review Agent, CI Diagnosis Agent, Data Analysis Agent.
    *   **Role**: Like user applications, utilizing platform capabilities to deliver business value.

### 3.2 Service Model: Stateful Intelligence Service

*   **Definition**: mo-dev-agent is a **Stateful Intelligence Service**, not a SaaS ERP.
*   **Data Ownership**:
    *   **User Data**: (e.g., Code, Orders, Customer Lists) resides in the user's external databases or Git repositories. We do not own this.
    *   **Intelligence Metadata**: (e.g., Decision history, Skill execution logs, Context snapshots) resides in **MatrixOne**. We own this to enable "Intelligence" (Recall, Replay, Reasoning).
*   **Implication**: The service manages the *context* of work, not the *work product* itself.

### 3.3 Dual-Layer Security Architecture

We avoid reinventing RBAC by using a hybrid approach:

*   **Layer 1: Infrastructure Security (MatrixOne RBAC)**
    *   **Scope**: Database connection, Table access, SQL execution.
    *   **Mechanism**: MatrixOne native roles (`mo_agent_admin`, `mo_agent_user`).
    *   **Responsibility**: Prevents unauthorized data access at the physical level (e.g., "User A cannot DROP TABLE").
    *   **Managed By**: Platform Ops / MatrixOne.

*   **Layer 2: Business Scope Control (Open Scope Protocol)**
    *   **Scope**: Project visibility, Model usage quotas, Repo access.
    *   **Mechanism**: Application-level `scope_type` + `scope_id` filtering.
    *   **Responsibility**: Enforces business logic boundaries (e.g., "Dev Team A cannot use Marketing Team's GPT-4 quota").
    *   **Managed By**: Business Admins (via `mo-admin`).

### 3.4 MatrixOne Binding Strategy

We consciously choose a **strong binding** with MatrixOne at the kernel level for strategic advantages, while maintaining architectural decoupling via interfaces.

*   **Why Strong Binding?**
    *   **Time Travel**: MatrixOne's `SELECT ... AS OF TIMESTAMP` enables instant agent state restoration without complex event sourcing logic.
    *   **Zero-Copy Branching**: MatrixOne's `CREATE SNAPSHOT` enables instant Sandbox creation for safe agent experimentation.
    *   **Unified Storage**: Combining Vector, Relational, and Git-like capabilities in one engine reduces stack complexity.

*   **Future Compatibility**:
    *   While implementation is bound, we define **Interface Layers** (e.g., `TimeTravelProvider`, `SandboxProvider`) in code.
    *   Non-MatrixOne implementations (e.g., Log-based replay, Docker-based sandbox) can be added as alternative providers in the future if needed.

---

## 2. MatrixOne RBAC System

### 1.1 Built-in Roles

MatrixOne provides built-in roles with predefined privileges:

```sql
-- System built-in roles
moadmin    -- System administrator role (cannot be modified)
public     -- Default role for all users (cannot be modified)
accountadmin -- Account administrator role (per-tenant)
```

### 1.2 Role Management

```sql
-- Create custom roles
CREATE ROLE role_name;

-- Grant privileges to role
GRANT privilege_list ON object_type object_name TO role_name [WITH GRANT OPTION];

-- Grant role to user
GRANT role_name TO user_name;

-- Set active role
SET ROLE role_name;

-- Revoke privileges
REVOKE privilege_list ON object_type object_name FROM role_name;

-- Drop role
DROP ROLE role_name;
```

### 1.3 Privilege Types

| Privilege Level | Object Type | Privileges |
|----------------|-------------|------------|
| **Account Level** | `account *` | `CREATE ACCOUNT`, `DROP ACCOUNT`, `ALTER ACCOUNT`, `CREATE USER`, `DROP USER`, `ALTER USER`, `CREATE ROLE`, `DROP ROLE`, `CREATE DATABASE`, `DROP DATABASE`, `SHOW DATABASES`, `CONNECT`, `MANAGE GRANTS`, `ALL` |
| **Database Level** | `database db_name.*` | `SHOW TABLES`, `CREATE TABLE`, `DROP TABLE`, `ALTER TABLE`, `CREATE VIEW`, `DROP VIEW`, `ALTER VIEW`, `ALL`, `OWNERSHIP` |
| **Table Level** | `table db_name.table_name` | `SELECT`, `INSERT`, `UPDATE`, `DELETE`, `TRUNCATE`, `REFERENCE`, `INDEX`, `ALL`, `OWNERSHIP` |

---

## 2. Role Design for mo-dev-agent

### 2.1 Role Hierarchy

```
moadmin (System Admin)
  ├── accountadmin (Tenant Admin)
  │   ├── mo_agent_admin (Agent Admin Role)
  │   │   └── Users with admin privileges
  │   └── mo_agent_user (Agent User Role)
  │       └── Regular users
  └── public (All Users)
```

### 2.2 Custom Roles

```sql
-- Agent Admin Role: Can manage models, tokens, skills
CREATE ROLE mo_agent_admin;

-- Grant account-level privileges
GRANT CREATE DATABASE, DROP DATABASE, SHOW DATABASES ON account * TO mo_agent_admin;
GRANT CREATE USER, DROP USER, ALTER USER ON account * TO mo_agent_admin;
GRANT CREATE ROLE, DROP ROLE ON account * TO mo_agent_admin;

-- Grant database-level privileges for agent_config database
GRANT ALL ON database agent_config.* TO mo_agent_admin;

-- Agent User Role: Can only use agent features
CREATE ROLE mo_agent_user;

-- Grant minimal privileges
GRANT CONNECT ON account * TO mo_agent_user;
GRANT SELECT ON database agent_config.* TO mo_agent_user;
GRANT ALL ON database agent_sessions.* TO mo_agent_user; -- Own sessions only
```

### 2.3 Permission Matrix

| Operation | moadmin | accountadmin | mo_agent_admin | mo_agent_user | public |
|-----------|---------|--------------|----------------|---------------|--------|
| **Global Model Config** |
| Add/Update/Remove global models | ✅ | ❌ | ❌ | ❌ | ❌ |
| View global models | ✅ | ✅ | ✅ | ✅ | ✅ |
| **Tenant Model Config** |
| Add/Update/Remove tenant models | ✅ | ✅ | ✅ | ❌ | ❌ |
| View tenant models | ✅ | ✅ | ✅ | ✅ | ✅ |
| **User Model Config** |
| Add/Update/Remove user models | ✅ | ✅ | ✅ | ✅ (own) | ❌ |
| View user models | ✅ | ✅ | ✅ | ✅ (own) | ✅ (own) |
| **API Keys** |
| Manage global API keys | ✅ | ❌ | ❌ | ❌ | ❌ |
| Manage tenant API keys | ✅ | ✅ | ✅ | ❌ | ❌ |
| Manage user API keys | ✅ | ✅ | ✅ | ✅ (own) | ❌ |
| **Sessions** |
| View all sessions | ✅ | ✅ | ✅ | ❌ | ❌ |
| View own sessions | ✅ | ✅ | ✅ | ✅ | ✅ |
| Delete sessions | ✅ | ✅ | ✅ | ✅ (own) | ❌ |
| **Skills** |
| Register global skills | ✅ | ❌ | ❌ | ❌ | ❌ |
| Register tenant skills | ✅ | ✅ | ✅ | ❌ | ❌ |
| Use skills | ✅ | ✅ | ✅ | ✅ | ✅ |

---

## 3. Database Schema

### 3.1 Leverage MatrixOne System Tables

MatrixOne provides system tables for user and role management:

```sql
-- System catalog tables (read-only)
mo_catalog.mo_user        -- User accounts
mo_catalog.mo_role        -- Roles
mo_catalog.mo_role_privs  -- Role privileges
mo_catalog.mo_user_grant  -- User-role grants
mo_catalog.mo_role_grant  -- Role-role grants
mo_catalog.mo_account     -- Tenant accounts
```

### 3.2 Application Tables

We only need to add application-specific tables:

```sql
-- Agent configuration database
CREATE DATABASE IF NOT EXISTS agent_config;

-- Model registry (scope-based)
CREATE TABLE IF NOT EXISTS agent_config.model_registry (
  config_id           VARCHAR(64) PRIMARY KEY,
  scope_type          VARCHAR(32) NOT NULL,  -- 'global' | 'account' | 'user' | 'project' | 'repo' ... (extensible)
  scope_id            VARCHAR(255),          -- NULL for global, account_id, user_id, or project_id
  model_name          VARCHAR(255) NOT NULL,
  provider            VARCHAR(64) NOT NULL,
  config_json         JSON NOT NULL,
  created_by          VARCHAR(255) NOT NULL,
  created_at          TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  updated_at          TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  
  UNIQUE KEY idx_scope_model (scope_type, scope_id, model_name),
  INDEX idx_scope (scope_type, scope_id)
);

-- API tokens (scope-based)
CREATE TABLE IF NOT EXISTS agent_config.api_tokens (
  token_id            VARCHAR(64) PRIMARY KEY,
  token_type          VARCHAR(32) NOT NULL,  -- 'llm' | 'repo'
  provider            VARCHAR(64) NOT NULL,
  scope_type          VARCHAR(32) NOT NULL,  -- Standard: 'global'|'account'|'user'; Custom: 'repo'|'project'|'team'
  scope_id            VARCHAR(255),
  encrypted_value     TEXT NOT NULL,
  is_active           BOOLEAN DEFAULT TRUE,
  created_by          VARCHAR(255) NOT NULL,
  created_at          TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  expires_at          TIMESTAMP,
  
  INDEX idx_scope (scope_type, scope_id, token_type)
);

-- Audit logs
CREATE TABLE IF NOT EXISTS agent_config.audit_logs (
  log_id              VARCHAR(64) PRIMARY KEY,
  user_id             VARCHAR(255) NOT NULL,
  action              VARCHAR(64) NOT NULL,
  resource_type       VARCHAR(64) NOT NULL,
  resource_id         VARCHAR(255),
  scope_type          VARCHAR(32),
  scope_id            VARCHAR(255),
  old_value           JSON,
  new_value           JSON,
  status              VARCHAR(32) DEFAULT 'success',
  error_message       TEXT,
  created_at          TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  
  INDEX idx_user_time (user_id, created_at),
  INDEX idx_action (action, created_at)
);
```

### 3.3 Extensible Scope Strategy (Open Scope Protocol)

To support diverse business scenarios (e.g., "Project" for Dev Agents, "Region" for Sales Agents) without changing the database schema, we adopt an **Open Scope Protocol**.

#### 1. Scope Definition
`scope_type` is an extensible string field. We define standard scopes and allow business-specific extensions.

*   **Standard Scopes** (Built-in):
    *   `global`: Applies to the entire system installation. `scope_id` is NULL.
    *   `account`: Applies to a MatrixOne Tenant. `scope_id` is `account_id`.
    *   `user`: Applies to a specific user. `scope_id` is `user_id`.

*   **Extended Scopes** (Business Specific):
    *   `repo`: Specific to a Git repository (e.g., for repository-specific tokens).
    *   `project`: Specific to a project group.
    *   `team`: Specific to a department or team.
    *   `region`: Specific to a geographical region (for other agent types).

#### 2. Resolution Priority (Client-Driven)
The database does not enforce a fixed hierarchy. Instead, the **Agent (Client)** determines the resolution priority by requesting a list of scopes in order of preference.

**Example: Resolving a GitHub Token for a Dev Agent**
When the agent needs to access a repository, it queries `api_tokens` with the following priority:

1.  **Repo Scope** (Most specific): Is there a token specifically for `repo:github.com/org/project`?
2.  **Project Scope**: Is there a token for `project:backend-team`?
3.  **User Scope**: Has the current user `user:alice` provided a personal token?
4.  **Account Scope**: Is there a shared tenant-level token?
5.  **Global Scope**: Is there a system-wide fallback token?

**Implementation Logic (Pseudo-code):**

```python
def resolve_config(key, context):
    """
    context = {
        'repo_id': '...',
        'project_id': '...',
        'user_id': '...',
        'account_id': '...'
    }
    """
    # Define priority list (Specific -> General)
    priority_scopes = [
        ('repo', context.get('repo_id')),
        ('project', context.get('project_id')),
        ('user', context.get('user_id')),
        ('account', context.get('account_id')),
        ('global', None)
    ]
    
    for scope_type, scope_id in priority_scopes:
        if not scope_id and scope_type != 'global': continue
        
        val = db.query("SELECT * FROM configs WHERE key=? AND scope_type=? AND scope_id=?", 
                       key, scope_type, scope_id)
        if val: return val
        
    return default_value
```

---

## 5. CLI Architecture

### 4.1 Separation of Concerns

```
mo-agent (User CLI)          mo-admin (Admin CLI)
├── chat                     ├── user
├── session                  │   ├── create
│   ├── list                 │   ├── list
│   └── show                 │   ├── grant-role
├── skill                    │   └── revoke-role
│   └── list                 ├── role
└── model                    │   ├── create
    ├── list (own scope)     │   ├── list
    └── show                 │   └── grant-privilege
                             ├── model
                             │   ├── list (all scopes)
                             │   ├── add (global/account/project...)
                             │   ├── update
                             │   └── remove
                             ├── token
                             │   ├── create
                             │   ├── list
                             │   └── revoke
                             └── audit
                                 └── logs
```

### 4.2 mo-agent (User CLI) - Restricted

```bash
# ✅ Allowed: View available models
mo-agent model list

# ✅ Allowed: View model details
mo-agent model show gpt-4

# ❌ Removed: Add/Update/Remove models
# mo-agent model add ...     # Command removed
# mo-agent model update ...  # Command removed
# mo-agent model remove ...  # Command removed

# ✅ Allowed: Manage own sessions
mo-agent session list
mo-agent session show <session_id>

# ✅ Allowed: Chat
mo-agent chat
```

### 4.3 mo-admin (Admin CLI) - Full Control

```bash
# User management (uses MatrixOne RBAC)
mo-admin user create alice --password '***' --role mo_agent_user
mo-admin user list
mo-admin user grant-role alice mo_agent_admin
mo-admin user revoke-role alice mo_agent_user

# Role management (uses MatrixOne RBAC)
mo-admin role create custom_role
mo-admin role grant-privilege custom_role "SELECT ON database agent_config.*"
mo-admin role list

# Model management (application-level)
mo-admin model add gpt-4 openai --scope global
mo-admin model add claude-3 anthropic --scope account --account-id acc_123

# Extended Scope Example: Project-specific model
mo-admin model add deepseek-coder --scope project --scope-id backend_v2

# Token management
mo-admin token create --type llm --provider openai --scope user --user-id alice
mo-admin token list --user alice
mo-admin token revoke <token_id>

# Audit logs
mo-admin audit logs --user alice --action create_model --since 2026-02-01
```

---

## 6. Permission Checker Implementation

See [permission-checker.md](./admin-user-management/permission-checker.md) for detailed implementation.

---

## 6. Implementation Plan

### Phase 1: Database & Infrastructure (Week 1)
- [ ] Create `agent_config` database and tables
- [ ] Create MatrixOne roles: `mo_agent_admin`, `mo_agent_user`
- [ ] Implement `PermissionChecker` using MatrixOne RBAC
- [ ] Implement `AuditLogger`
- [ ] Add tests

### Phase 2: mo-admin CLI (Week 2)
- [ ] Create `mo-admin` CLI entry point
- [ ] Implement user management commands (wrapper around MatrixOne)
- [ ] Implement role management commands (wrapper around MatrixOne)
- [ ] Implement model management commands
- [ ] Implement token management commands
- [ ] Implement audit commands

### Phase 3: mo-agent Permission Restrictions (Week 3)
- [x] Remove `mo-agent model add/update/remove` commands
- [x] Add permission checks to all operations
- [x] Create `PermissionChecker` using MatrixOne RBAC
- [x] Create `AuditLogger` for tracking operations
- [x] Implement `mo-admin` CLI with model/token/audit commands
- [x] Update documentation
- [x] Add tests (13 new tests, 116 total passing)

### Phase 4: Testing & Documentation (Week 4)
- [ ] Integration tests
- [ ] Security tests
- [ ] User documentation
- [ ] Admin documentation

---

## 7. Backward Compatibility

### 7.1 Migration Strategy

1. **Default Roles**: Create default roles on first startup
2. **Existing Configs**: Automatically migrate to global scope
3. **Gradual Migration**: Mark `mo-agent` admin commands as deprecated, remove in next version

### 7.2 Initialization Script

```sql
-- Run on first startup
-- Create custom roles
CREATE ROLE IF NOT EXISTS mo_agent_admin;
CREATE ROLE IF NOT EXISTS mo_agent_user;

-- Grant privileges to mo_agent_admin
GRANT CREATE DATABASE, DROP DATABASE, SHOW DATABASES ON account * TO mo_agent_admin;
GRANT ALL ON database agent_config.* TO mo_agent_admin;

-- Grant privileges to mo_agent_user
GRANT CONNECT ON account * TO mo_agent_user;
GRANT SELECT ON database agent_config.* TO mo_agent_user;

-- Create default admin user
CREATE USER IF NOT EXISTS admin IDENTIFIED BY '<random_password>';
GRANT mo_agent_admin TO admin;
GRANT moadmin TO admin; -- Optional: full system access
```

---

## 8. Security Considerations

### 8.1 Authentication
- CLI uses MatrixOne user credentials
- Credentials stored in `~/.mo-agent/credentials`
- Support for connection strings

### 8.2 Authorization
- All operations checked via `PermissionChecker`
- Leverages MatrixOne's `mo_role_privs` table
- Principle of least privilege

### 8.3 Audit
- All CRUD operations logged to `audit_logs`
- Includes before/after values
- Support audit log export

---

## 10. Example Scenarios

### Scenario 1: Admin Adds Global Model

```bash
# 1. Admin adds global model
mo-admin model add gpt-4-turbo openai \
  --scope global \
  --price-prompt 0.01 \
  --price-completion 0.03

# 2. Audit log automatically recorded
# 3. All users can immediately see it
mo-agent model list  # Shows gpt-4-turbo
```

### Scenario 2: Tenant Admin Configures Team Models

```bash
# 1. Tenant admin adds team-specific model
mo-admin model add premium-gpt-4 openai \
  --scope account \
  --account-id team_a \
  --price-prompt 0.005

# 2. Only team_a users can see it
mo-agent model list --user alice  # alice in team_a, sees it
mo-agent model list --user bob    # bob in team_b, doesn't see it
```

### Scenario 3: Regular User Tries to Modify Config

```bash
# User tries to add model
mo-agent model add my-model openai

# Output:
# ❌ Error: 'model add' command is not available in mo-agent
# 💡 Tip: Contact your administrator to add models
# 📖 See: mo-agent model list (to view available models)
```

---

## 11. Summary

### Core Principles
1. **Leverage MatrixOne RBAC**: Use built-in role and privilege system
2. **Separation of Concerns**: mo-agent (users) vs mo-admin (administrators)
3. **Least Privilege**: Users can only access resources within their scope
4. **Audit Trail**: All administrative operations are traceable
5. **Security First**: Permission checks + audit logs

### Key Changes
- ✅ Use MatrixOne's `CREATE ROLE`, `GRANT`, `REVOKE` commands
- ✅ Create custom roles: `mo_agent_admin`, `mo_agent_user`
- ✅ Add application tables: `model_registry`, `api_tokens`, `audit_logs`
- ✅ Create new `mo-admin` CLI
- ✅ Remove admin commands from `mo-agent`
- ✅ Add permission checks to all operations
- ✅ Log all administrative operations

### Next Steps
1. Review design document
2. Implement Phase 1 (Database + Infrastructure)
3. Implement Phase 2 (mo-admin CLI)
4. Implement Phase 3 (mo-agent restrictions)

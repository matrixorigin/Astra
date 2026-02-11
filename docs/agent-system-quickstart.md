# Agent System Quick Start

## Prerequisites

- MatrixOne running on localhost:6001
- Python 3.11+
- Dependencies installed (`make install`)

## Step 1: Initialize Agent System

```bash
# Initialize database tables and RBAC roles
make db-init-agent

# Or manually:
python3 infra/scripts/init_agent_system.py
```

This creates:
- ✅ `agent_config` database
- ✅ Tables: `model_registry`, `skills_registry`, `api_tokens`, `audit_logs`
- ✅ Roles: `mo_agent_admin`, `mo_agent_user`

## Step 2: Create Users and Grant Roles

```sql
-- Connect to MatrixOne
mysql -h127.0.0.1 -P6001 -uroot -p111

-- Create users
CREATE USER alice IDENTIFIED BY 'password123';
CREATE USER admin IDENTIFIED BY 'admin123';

-- Grant roles
GRANT mo_agent_user TO alice;
GRANT mo_agent_admin TO admin;

-- Verify
SHOW GRANTS FOR alice;
SHOW GRANTS FOR admin;
```

## Step 3: Configure Models (Admin)

```bash
# Add global model (requires mo_agent_admin role)
mo-admin model add gpt-4o openai \
  --scope global \
  --context-window 128000 \
  --price-prompt 0.0025 \
  --price-completion 0.01

# Add account-specific model
mo-admin model add premium-gpt-4 openai \
  --scope account \
  --account-id acme \
  --price-prompt 0.002
```

## Step 4: Use Agent (User)

```bash
# Start chat as regular user
mo-agent chat --user-id alice

# List available models
mo-agent model list

# Register personal skill
mo-agent skill register my_parser.py --name parse_logs
```

## Scope-Based Configuration

### Example 1: Project-Specific Model

```sql
-- Admin configures cheaper model for experimental project
INSERT INTO agent_config.model_registry VALUES (
  'model_project_exp_gpt4mini',
  'project',
  'experimental',
  'gpt-4o-mini',
  'openai',
  '{"context_window": 128000, "price_per_1k_prompt": 0.00015}',
  'admin',
  NOW()
);
```

```python
# User working on experimental project
client = LLMClient(
    db=db,
    user_id='alice',
    tenant_id='acme',
    scope_context={'project': 'experimental'}
)

# Will use cheaper gpt-4o-mini for this project
```

### Example 2: Repo-Specific API Key

```sql
-- Admin configures repo-specific token with higher rate limit
INSERT INTO agent_config.api_tokens VALUES (
  'token_repo_matrixone',
  'llm',
  'openai',
  'repo',
  'matrixone',
  'sk-special-key-with-higher-limit',
  TRUE,
  'admin',
  NOW(),
  NULL
);
```

```python
# User working on matrixone repo
client = LLMClient(
    db=db,
    user_id='alice',
    tenant_id='acme',
    scope_context={'repo': 'matrixone'}
)

# Will use repo-specific token with higher rate limit
```

## Troubleshooting

### Tables not found
```bash
# Re-run initialization
make db-init-agent
```

### Permission denied
```sql
-- Check user roles
SHOW GRANTS FOR alice;

-- Grant missing role
GRANT mo_agent_user TO alice;
```

### Import errors
```bash
# Reinstall dependencies
make install
```

## Next Steps

- See [Admin & User Management Design](../docs/design/admin-user-management.md) for architecture
- See [Scope-Based Configuration](../docs/scope-based-configuration.md) for advanced usage

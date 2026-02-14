# Scope-Based Configuration Usage Guide

## Overview

The Scope-Based Configuration system implements the **Open Scope Protocol**, allowing flexible configuration management across different business dimensions (repo, project, region, environment, etc.) without changing the database schema.

## Quick Start

### 1. Basic Usage with LLMClient

```python
from core.llm.client import LLMClient
from sdk import Database

db = Database()

# Scenario 1: Dev Agent working on a specific project
client = LLMClient(
    db=db,
    user_id='alice',
    tenant_id='acme',
    scope_context={'project': 'backend'}
)

# API keys and model configs will be resolved with priority:
# project > user > account > global

# Scenario 2: Dev Agent working on a specific repo
client = LLMClient(
    db=db,
    user_id='alice',
    tenant_id='acme',
    scope_context={
        'repo': 'matrixone',
        'project': 'backend'
    }
)

# Priority: repo > project > user > account > global
```

### 2. Dynamic Context Updates

```python
# Start without specific context
client = LLMClient(db=db, user_id='alice', tenant_id='acme')

# Later, update context when user switches to a different project
client.set_user_context(
    user_id='alice',
    tenant_id='acme',
    scope_context={'project': 'frontend'}
)

# Configuration will be re-resolved with new context
```

### 3. Direct Use of ScopeResolver

```python
from core.config.scope_resolver import ScopeResolver, ScopeChainBuilder

# Build scope chain for Dev Agent
chain = ScopeChainBuilder.dev_agent(
    user_id='alice',
    account_id='acme',
    repo='matrixone',
    project='backend'
)

resolver = ScopeResolver(db, chain)

# Resolve specific model config
model = resolver.resolve_model('gpt-4')
# Returns: {'model_name': 'gpt-4', 'scope_type': 'repo', ...}

# Resolve API token
token = resolver.resolve_token('llm', 'openai')
# Returns: {'token_type': 'llm', 'provider': 'openai', 'encrypted_value': '...'}

# List all accessible models
models = resolver.list_models()
# Returns: [model1, model2, ...] with more specific scopes overriding general ones

# List all accessible skills
skills = resolver.list_skills()
```

## Scope Chain Builders

### Dev Agent (Default)

```python
chain = ScopeChainBuilder.dev_agent(
    user_id='alice',
    account_id='acme',
    repo='matrixone',      # Optional
    project='backend'      # Optional
)

# Priority: repo > project > user > account > global
```

### Sales Agent

```python
chain = ScopeChainBuilder.sales_agent(
    user_id='bob',
    account_id='sales_corp',
    region='us-west',      # Optional
    sales_group='enterprise'  # Optional
)

# Priority: region > sales_group > user > account > global
```

### Deploy Agent

```python
chain = ScopeChainBuilder.deploy_agent(
    user_id='ops',
    account_id='devops_team',
    environment='production',  # Optional
    project='api-gateway'      # Optional
)

# Priority: environment > project > account > global
```

### Custom Scope Chain

```python
chain = ScopeChainBuilder.custom(
    user_id='alice',
    account_id='acme',
    custom_scopes=[
        ('branch', 'feature-x'),
        ('environment', 'staging'),
        ('repo', 'backend')
    ]
)

# Priority: branch > environment > repo > user > account > global
```

## Database Setup

### 1. Create Tables

```sql
-- Model registry with extensible scope
CREATE TABLE IF NOT EXISTS agent_config.model_registry (
  config_id           VARCHAR(64) PRIMARY KEY,
  scope_type          VARCHAR(32) NOT NULL,  -- 'global' | 'account' | 'user' | 'project' | 'repo' | ...
  scope_id            VARCHAR(255),          -- NULL for global, or specific ID
  model_name          VARCHAR(255) NOT NULL,
  provider            VARCHAR(64) NOT NULL,
  config_json         JSON NOT NULL,
  created_by          VARCHAR(255) NOT NULL,
  created_at          TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  
  UNIQUE KEY idx_scope_model (scope_type, scope_id, model_name),
  INDEX idx_scope (scope_type, scope_id)
);

-- API tokens with extensible scope
CREATE TABLE IF NOT EXISTS agent_config.api_tokens (
  token_id            VARCHAR(64) PRIMARY KEY,
  token_type          VARCHAR(32) NOT NULL,  -- 'llm' | 'repo'
  provider            VARCHAR(64) NOT NULL,
  scope_type          VARCHAR(32) NOT NULL,  -- Extensible
  scope_id            VARCHAR(255),
  encrypted_value     TEXT NOT NULL,
  is_active           BOOLEAN DEFAULT TRUE,
  created_by          VARCHAR(255) NOT NULL,
  created_at          TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  
  INDEX idx_scope (scope_type, scope_id, token_type)
);
```

### 2. Insert Sample Data

```sql
-- Global model config (fallback for all users)
INSERT INTO agent_config.model_registry VALUES (
  'model_global_gpt4',
  'global',
  NULL,
  'gpt-4',
  'openai',
  '{"context_window": 8192, "price_per_1k_prompt": 0.03}',
  'admin',
  NOW()
);

-- Project-specific model config (overrides global)
INSERT INTO agent_config.model_registry VALUES (
  'model_project_backend_gpt4',
  'project',
  'backend',
  'gpt-4',
  'openai',
  '{"context_window": 8192, "price_per_1k_prompt": 0.025}',  -- Discounted price
  'alice',
  NOW()
);

-- Repo-specific API token (most specific)
INSERT INTO agent_config.api_tokens VALUES (
  'token_repo_matrixone',
  'llm',
  'openai',
  'repo',
  'matrixone',
  'sk-repo-specific-key',
  TRUE,
  'alice',
  NOW()
);
```

## Resolution Priority

When resolving a configuration, the system queries scopes from **most specific to most general**:

```
repo (matrixone)
  ↓ (not found)
project (backend)
  ↓ (not found)
user (alice)
  ↓ (not found)
account (acme)
  ↓ (not found)
global
  ↓ (found!)
```

The **first match** is returned.

## List Operations

When listing configurations (e.g., `list_models()`), the system:

1. Queries **all scopes** in the chain
2. Merges results with **more specific scopes overriding general ones**
3. Returns the final merged list

Example:
- Global: `gpt-4` (price: $0.03)
- Project: `gpt-4` (price: $0.025)
- Result: `gpt-4` (price: $0.025) ← Project config wins

## Best Practices

### 1. Use Appropriate Scope Chain Builder

```python
# For development work
chain = ScopeChainBuilder.dev_agent(...)

# For sales operations
chain = ScopeChainBuilder.sales_agent(...)

# For deployment operations
chain = ScopeChainBuilder.deploy_agent(...)
```

### 2. Provide Context When Available

```python
# Good: Provide specific context
client = LLMClient(
    db=db,
    user_id='alice',
    tenant_id='acme',
    scope_context={'repo': 'matrixone', 'project': 'backend'}
)

# Acceptable: Minimal context
client = LLMClient(db=db, user_id='alice', tenant_id='acme')
```

### 3. Update Context Dynamically

```python
# When user switches context (e.g., changes repo)
client.set_user_context(
    user_id='alice',
    tenant_id='acme',
    scope_context={'repo': 'new-repo'}
)
```

### 4. Use Custom Scopes for Special Cases

```python
# For feature branch specific config
chain = ScopeChainBuilder.custom(
    user_id='alice',
    account_id='acme',
    custom_scopes=[
        ('branch', 'feature-ai-integration'),
        ('repo', 'backend')
    ]
)
```

## Testing

See `tests/unit/test_scope_resolver.py` and `tests/unit/test_scope_integration.py` for comprehensive examples.

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│                      LLMClient                          │
│  ┌───────────────────────────────────────────────────┐ │
│  │            ScopeResolver                          │ │
│  │  ┌─────────────────────────────────────────────┐ │ │
│  │  │         Scope Chain                         │ │ │
│  │  │  repo > project > user > account > global   │ │ │
│  │  └─────────────────────────────────────────────┘ │ │
│  │                      ↓                            │ │
│  │  ┌─────────────────────────────────────────────┐ │ │
│  │  │         Database Queries                    │ │ │
│  │  │  SELECT * FROM model_registry               │ │ │
│  │  │  WHERE scope_type=? AND scope_id=?          │ │ │
│  │  └─────────────────────────────────────────────┘ │ │
│  └───────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────┘
```

## See Also

- [Authentication](authentication.md)
- [LLM Integration](llm-integration.md)

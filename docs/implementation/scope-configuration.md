# Scope-Based Configuration Usage Guide

## Overview

The Scope-Based Configuration system implements the **Open Scope Protocol**, allowing flexible configuration management across different business dimensions (repo, project, region, environment, etc.) without changing the database schema.

## Quick Start

### 1. Basic Usage with LLMClient

```python
from core.llm.client import LLMClient

# Scenario 1: Dev Agent working on a specific project
client = LLMClient(
    db=db,
    user_id='alice',
    scope_context={'project': 'backend'}
)
# Priority: project > user > global

# Scenario 2: Dev Agent working on a specific repo
client = LLMClient(
    db=db,
    user_id='alice',
    scope_context={'repo': 'matrixone', 'project': 'backend'}
)
# Priority: repo > project > user > global
```

### 2. Dynamic Context Updates

```python
client = LLMClient(db=db, user_id='alice')

# Update context when user switches project
client.set_user_context(
    user_id='alice',
    scope_context={'project': 'frontend'}
)
```

### 3. Direct Use of ScopeResolver

```python
from core.scope.scope_resolver import ScopeResolver, ScopeChainBuilder

chain = ScopeChainBuilder.dev_agent(
    user_id='alice',
    repo='matrixone',
    project='backend'
)

resolver = ScopeResolver(db, chain)
token = resolver.resolve_token('llm', 'openai')
config = resolver.resolve_config('max_context_tokens')
```

## Scope Chain Builders

### Dev Agent

```python
chain = ScopeChainBuilder.dev_agent(user_id='alice', repo='matrixone', project='backend')
# Priority: repo > project > user > global
```

### Sales Agent

```python
chain = ScopeChainBuilder.sales_agent(user_id='bob', region='us-west', sales_group='enterprise')
# Priority: region > sales_group > user > global
```

### Deploy Agent

```python
chain = ScopeChainBuilder.deploy_agent(environment='production', project='api-gateway')
# Priority: environment > project > global
```

### Custom Scope Chain

```python
chain = ScopeChainBuilder.custom(
    user_id='alice',
    custom_scopes=[('branch', 'feature-x'), ('environment', 'staging')]
)
# Priority: branch > environment > user > global
```

## Resolution Priority

```
repo (matrixone)
  ↓ (not found)
project (backend)
  ↓ (not found)
user (alice)
  ↓ (not found)
global
  ↓ (found!)
```

The **first match** is returned.

## Testing

See `tests/unit/test_scope_resolver.py` and `tests/unit/test_scope_integration.py`.

## See Also

- [Authentication](authentication.md)
- [LLM Integration](llm-integration.md)

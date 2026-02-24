# Per-Agent Model Override

## Overview

Enable each agent to specify its preferred LLM model, allowing cost optimization and capability matching in multi-agent workflows.

## Motivation

In multi-agent systems, different agents have different computational needs:

| Agent Role | Task Complexity | Optimal Model | Cost/1M tokens |
|------------|-----------------|---------------|----------------|
| Orchestrator | High (planning, coordination) | claude-3-opus | $15.00 |
| Code Generator | Medium (implementation) | claude-3-sonnet | $3.00 |
| Test Runner | Low (execution, validation) | claude-3-haiku | $0.25 |
| Reviewer | Medium-High (analysis) | claude-3-sonnet | $3.00 |

Without per-agent model override:
- All agents use the same model → either overpaying for simple tasks or under-powering complex ones
- No way to optimize cost/quality tradeoff per agent role

## Design

### 1. Agent Config Schema Extension

```python
# agents.agent_config JSON schema
{
    "system_prompt": "You are a code review specialist...",
    "model": "claude-3-sonnet-20241022",  # NEW: preferred model
    "model_constraints": {                 # NEW: optional constraints
        "fallback": "claude-3-haiku-20241022",
        "max_cost_per_call": 0.10,
        "require_reasoning": false
    },
    "allowed_tools": ["read_file", "search_code"],
    "max_tokens": 4096
}
```

### 2. Model Resolution Priority

When determining which model to use for an agent run:

```
1. Explicit request.model (user override)     ← Highest priority
   ↓
2. run.context["model"] (programmatic)
   ↓
3. agent_config["model"] (agent default)      ← NEW
   ↓
4. SLO escalation (performance-based)
   ↓
5. Global default (llm_config.model)          ← Lowest priority
```

### 3. Implementation Points

#### 3.1 RunEngine: Load agent model into context

```python
# run_engine.py: create_child_run()
def _load_agent_config(self, agent_id: str) -> dict | None:
    # Existing: loads system_prompt, allowed_tools
    # NEW: also loads model, model_constraints
    ...

async def create_child_run(self, parent_run_id, agent_id, task, context):
    ctx = dict(context or {})
    config = self._load_agent_config(agent_id)
    if config:
        # Existing
        if config.get("system_prompt"):
            ctx["system_prompt"] = config["system_prompt"]
        # NEW: propagate agent's preferred model
        if config.get("model") and "model" not in ctx:
            ctx["model"] = config["model"]
        if config.get("model_constraints"):
            ctx["model_constraints"] = config["model_constraints"]
```

#### 3.2 ChatLoop: Respect agent model

```python
# chat_loop.py: run_step() / run_step_stream()
def _resolve_model(self, context: dict | None) -> str | None:
    """Resolve model with full priority chain."""
    ctx = context or {}
    
    # 1. Explicit context override (from request or parent)
    if ctx.get("model"):
        return ctx["model"]
    
    # 2. Agent config model (loaded by RunEngine)
    # Already in context if set
    
    # 3. SLO escalation
    # ... existing logic
    
    return None  # Use LLMClient default
```

#### 3.3 Cost Constraint Enforcement

```python
# chat_loop.py or llm/client.py
def _check_model_constraints(self, model: str, constraints: dict) -> str:
    """Apply model constraints, potentially downgrading."""
    if not constraints:
        return model
    
    max_cost = constraints.get("max_cost_per_call")
    if max_cost:
        model_config = self.router.get(model)
        estimated_cost = model_config.pricing.prompt * 4  # ~4K tokens estimate
        if estimated_cost > max_cost:
            fallback = constraints.get("fallback")
            if fallback:
                logger.info(f"Model {model} exceeds cost limit, using {fallback}")
                return fallback
    
    return model
```

### 4. API Changes

#### 4.1 Agent Create/Update

```python
# POST /agents, PUT /agents/{agent_id}
class AgentConfig(BaseModel):
    system_prompt: str | None = None
    model: str | None = None  # NEW
    model_constraints: ModelConstraints | None = None  # NEW
    allowed_tools: list[str] | None = None
    max_tokens: int | None = None

class ModelConstraints(BaseModel):
    fallback: str | None = None
    max_cost_per_call: float | None = None
    require_reasoning: bool = False
```

#### 4.2 Agent Response

```python
# GET /agents/{agent_id}
{
    "agent_id": "code-reviewer",
    "agent_name": "Code Reviewer",
    "agent_config": {
        "system_prompt": "...",
        "model": "claude-3-sonnet-20241022",  # Visible in response
        "model_constraints": {
            "fallback": "claude-3-haiku-20241022"
        }
    }
}
```

### 5. Multi-Agent Workflow Example

```python
# Orchestrator delegates to specialized agents
workflow = {
    "orchestrator": {
        "agent_id": "lead-agent",
        "model": "claude-3-opus-20241022",  # Deep reasoning for planning
    },
    "workers": [
        {
            "agent_id": "implementer",
            "model": "claude-3-sonnet-20241022",  # Good for code
        },
        {
            "agent_id": "tester", 
            "model": "claude-3-haiku-20241022",  # Fast, cheap for tests
        },
    ]
}

# Cost breakdown for a typical task:
# - Orchestrator: 1 call × $0.015 = $0.015
# - Implementer: 3 calls × $0.003 = $0.009
# - Tester: 5 calls × $0.00025 = $0.00125
# Total: $0.025 vs $0.075 (all Opus) = 67% savings
```

### 6. Audit Trail

Every model selection is logged:

```python
# conversation_events
{
    "event_type": "model_selected",
    "metadata": {
        "agent_id": "implementer",
        "requested_model": null,
        "agent_model": "claude-3-sonnet-20241022",
        "resolved_model": "claude-3-sonnet-20241022",
        "resolution_reason": "agent_config",
        "constraints_applied": false
    }
}
```

### 7. Backward Compatibility

- Agents without `model` in config continue to use global default
- Existing API contracts unchanged
- No migration required — `model` field is optional

## Testing Strategy

1. **Unit tests**: Model resolution priority chain
2. **Integration tests**: Agent with model config → correct model used
3. **E2E tests**: Multi-agent workflow with different models per agent
4. **Cost verification**: Actual API calls use specified models

## Implementation Checklist

- [x] Extend `_load_agent_config()` to include model fields
- [x] Update `create_child_run()` to propagate agent model to context
- [x] Update ChatLoop to use context model (already done, verify)
- [x] Update agent API schemas
- [x] Write integration tests
- [x] Write E2E tests with real model calls
- [ ] Add model selection audit event (`model_selected` in conversation_events)

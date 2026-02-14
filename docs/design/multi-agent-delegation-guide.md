# Multi-Agent Delegation Quick Reference

## Overview

The multi-agent delegation system enables orchestrator agents to coordinate work across multiple specialist agents with full auditability and parallel execution support.

## Key Concepts

### Agent Tiers

- **User Agents**: Domain-specific problem solvers (e.g., code_reviewer, data_analyst)
- **System Agents**: Platform maintenance (e.g., regression_agent, audit_agent) - can auto-trigger
- **Orchestrator Agents**: Coordinate multiple agents, can delegate tasks

### AgentProfile

```python
from core.agent.agent_registry import AgentProfile, AgentRegistry

# User agent (default)
user_agent = AgentProfile(
    agent_id="code_reviewer",
    system_prompt="You are an expert code reviewer",
    skill_filter=["read_file", "search_code"],  # Optional: limit skills
    model="gpt-4",  # Optional: override default model
)

# Orchestrator agent
orchestrator = AgentProfile(
    agent_id="lead_agent",
    system_prompt="Coordinate code review tasks",
    tier="orchestrator",
    can_delegate=True,
    delegate_to=["code_reviewer", "security_reviewer"],  # Whitelist (empty = all)
)

# System agent
system_agent = AgentProfile(
    agent_id="regression_agent",
    system_prompt="Run regression tests",
    tier="system",
    can_delegate=True,
    triggers=["skill_change", "prompt_change"],  # Auto-trigger events
)
```

### Registration

```python
registry = AgentRegistry()

# Register target agents first (for validation)
registry.register(user_agent)
registry.register(orchestrator)

# Check delegation permissions
can_delegate = registry.can_delegate("lead_agent", "code_reviewer")  # True
```

## Coordination Patterns

### 1. Fan-Out / Fan-In (Parallel)

Execute multiple tasks concurrently and synthesize results:

```python
from core.agent.coordination import CoordinationPatterns, Task

patterns = CoordinationPatterns(delegation_skill)

# Define parallel tasks
tasks = [
    Task(agent_id="code_agent", description="Review code quality"),
    Task(agent_id="security_agent", description="Check for vulnerabilities"),
    Task(agent_id="perf_agent", description="Analyze performance"),
]

# Execute in parallel
results = await patterns.fan_out(tasks, session_id="s1", user_id="u1")

# Synthesize results
summary = patterns.fan_in(results)
print(summary)
# Output:
# ✅ 3 tasks completed successfully:
#   [code_agent]: Found 2 issues...
#   [security_agent]: No vulnerabilities...
#   [perf_agent]: Identified bottleneck...
```

### 2. Pipeline (Sequential)

Execute tasks in sequence, passing output to next step:

```python
steps = [
    Task(agent_id="analyzer", description="Analyze codebase"),
    Task(agent_id="fixer", description="Fix identified issues"),
    Task(agent_id="tester", description="Run tests"),
]

result = await patterns.pipeline(steps, session_id="s1", user_id="u1")

if result.success:
    print(f"Pipeline complete: {result.output}")
else:
    print(f"Pipeline failed at: {result.agent_id}")
```

**Features**:
- Each step receives previous step's output in `context["previous_output"]`
- Early termination on first failure
- Full audit trail of sequential execution

### 3. Adversarial Review (Iterative Refinement)

Iterative proposal-review-revise loop:

```python
result = await patterns.adversarial_review(
    proposal="Initial design document",
    proposer_agent="designer",
    reviewer_agent="senior_architect",
    session_id="s1",
    user_id="u1",
    max_rounds=3,
)

if result.success:
    print(f"Approved after {round_count} rounds")
    print(result.output)  # Final approved proposal
```

**Features**:
- Automatic approval detection (looks for "approved" or "lgtm")
- Revision loop with configurable max rounds
- Returns final proposal even if max rounds reached

## Direct Delegation

For simple one-off delegation:

```python
from core.skills.delegation import DelegateTaskInput, DelegateTaskSkill

delegate_skill = DelegateTaskSkill(registry, chat_loop_factory)

input_data = DelegateTaskInput(
    agent_id="code_reviewer",
    task="Review auth.py for security issues",
    context="PR #123 changes authentication flow",
    session_id="s1",
    user_id="u1",
)

output = await delegate_skill.execute(input_data)
print(output.result)
```

### Parallel Delegation

```python
inputs = [
    DelegateTaskInput(agent_id="agent1", task="Task 1", ...),
    DelegateTaskInput(agent_id="agent2", task="Task 2", ...),
    DelegateTaskInput(agent_id="agent3", task="Task 3", ...),
]

outputs = await delegate_skill.execute_parallel(inputs)
# Returns list in same order as inputs
```

## Stream Multiplexing

All stream events include `agent_id` for multi-agent UI rendering:

```python
async for event in chat_loop.run_step_stream(...):
    print(f"[{event.agent_id}] {event.event_type}: {event.data}")
    
# Output:
# [orchestrator] run_started: {"query": "Review PR"}
# [code_agent] text_delta: {"chunk": "Analyzing..."}
# [security_agent] text_delta: {"chunk": "Checking..."}
# [orchestrator] text_delta: {"chunk": "Based on reviews..."}
```

## Event Chain Linking

All delegated events inherit parent's `causal_chain_id`:

```python
# Query events by causal chain
events = db.query(ConversationEvent).filter(
    ConversationEvent.causal_chain_id == chain_id
).all()

# Full audit trail:
# 1. [orchestrator] user_query: "Review PR"
# 2. [orchestrator] tool_call: delegate_task(agent="code_agent")
# 3. [code_agent] llm_response: "Found 2 issues..."
# 4. [orchestrator] tool_result: "Found 2 issues..."
# 5. [orchestrator] llm_response: "Based on review..."
```

## Testing

```python
# Unit test example
@pytest.mark.asyncio
async def test_fan_out():
    registry = AgentRegistry()
    registry.register(AgentProfile(agent_id="agent1", system_prompt="Agent 1"))
    
    # Mock delegation skill
    mock_delegate = MagicMock()
    mock_delegate.execute = AsyncMock(return_value=DelegateTaskOutput(...))
    
    patterns = CoordinationPatterns(mock_delegate)
    tasks = [Task(agent_id="agent1", description="Task 1")]
    
    results = await patterns.fan_out(tasks, "s1", "u1")
    assert len(results) == 1
    assert results[0].success
```

## Best Practices

1. **Register target agents before orchestrators** - Validation requires targets to exist
2. **Use empty delegate_to for flexibility** - Allows delegation to any agent
3. **Set skill_filter for security** - Limit what agents can do
4. **Use coordination patterns over raw delegation** - Built-in error handling and synthesis
5. **Check can_delegate() before delegation** - Enforce permissions at runtime
6. **Monitor causal chains for debugging** - Full audit trail of multi-agent workflows

## Architecture Notes

- **No new execution engine**: ChatLoop handles everything
- **Event blackboard coordination**: All communication through conversation_events
- **Time-travel queryable**: Inspect any point in multi-agent workflow
- **Replay compatible**: Re-execute multi-agent workflows from events
- **Stream multiplexing**: Real-time multi-agent progress in UI

## Future Enhancements

- System agent auto-triggers (on skill/prompt changes)
- Model routing per agent tier
- Cost-aware scheduling with budget convergence
- Conflict resolution for competing agent proposals
- Dynamic team formation based on task requirements

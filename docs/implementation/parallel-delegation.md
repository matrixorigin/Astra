# Parallel Delegation in ChatLoop

## Overview

ChatLoop now supports **automatic parallel delegation** when the LLM returns multiple `delegate_task` tool calls. This enables efficient fan-out/fan-in patterns for multi-agent collaboration.

## How It Works

### 1. Detection

When ChatLoop receives tool calls from the LLM, it checks if there are multiple `delegate_task` calls:

```python
delegation_calls = [tc for tc in tool_calls if tc["function"]["name"] == "delegate_task"]

if len(delegation_calls) > 1:
    # Parallel delegation path
else:
    # Sequential tool execution path
```

### 2. Fan-Out

ChatLoop constructs `DelegateTaskInput` objects for all delegations and calls `execute_parallel_stream()`:

```python
inputs = []
for tc in delegation_calls:
    params = json.loads(tc["function"]["arguments"])
    inputs.append(DelegateTaskInput(
        agent_id=params.get("agent_id"),
        task=params.get("task"),
        context=params.get("context"),
        session_id=session_id,
        user_id=user_id,
    ))

async for event in skill.execute_parallel_stream(inputs):
    yield event  # Forward to orchestrator's stream
```

### 3. Stream Multiplexing

`execute_parallel_stream()` uses `asyncio.Queue` to multiplex events from all delegated agents:

- All agents start in parallel
- Events are tagged with `agent_id`
- Events are yielded in arrival order (non-deterministic)
- Completion is tracked via sentinel pattern

### 4. Result Collection

ChatLoop collects results from `TEXT_DONE` events and tracks errors:

```python
results = {}  # call_id -> result_text
agent_to_call = {}  # agent_id -> call_id mapping
completed_agents = set()  # Track completion

try:
    async for event in skill.execute_parallel_stream(inputs):
        yield event
        
        if event.event_type == StreamEventType.TEXT_DONE:
            agent_id = event.agent_id
            result_text = event.data.get("text", "")
            call_id = agent_to_call.get(agent_id)
            if call_id:
                results[call_id] = result_text
        
        elif event.event_type == StreamEventType.AGENT_COMPLETED:
            completed_agents.add(event.agent_id)
            # Fallback for agents with no text output
            call_id = agent_to_call.get(event.agent_id)
            if call_id and call_id not in results:
                results[call_id] = f"Agent '{event.agent_id}' completed with no text output"
        
        elif event.event_type == StreamEventType.RUN_ERROR:
            # Track errors
            call_id = agent_to_call.get(event.agent_id)
            if call_id:
                results[call_id] = f"Error: {event.data.get('error', 'Unknown error')}"

except Exception as e:
    logger.error(f"Error in parallel delegation: {e}", exc_info=True)
    # Mark incomplete delegations as failed
    for tc in delegation_calls:
        if tc["id"] not in results:
            results[tc["id"]] = f"Error: {str(e)}"
```

**Error Handling**:
- Individual agent failures are captured via `RUN_ERROR` events
- Stream-level exceptions are caught and logged
- Incomplete delegations are marked as failed
- LLM receives all results (including errors) for synthesis

### 5. Fan-In

After all agents complete, ChatLoop emits `TOOL_RESULT` events for each delegation:

```python
for tc in delegation_calls:
    result_str = results.get(tc["id"], "No result")
    yield StreamEvent(
        event_type=StreamEventType.TOOL_RESULT,
        data={"call_id": tc["id"], "result": result_str[:500]},
        ...
    )
    messages.append({"role": "tool", "tool_call_id": tc["id"], "content": result_str})
```

The LLM then receives all results and can synthesize a final response.

## Event Flow

```
LLM returns 3 delegate_task calls
    ↓
ChatLoop detects parallel delegation
    ↓
TOOL_CALL_START × 3 (one per delegation)
    ↓
execute_parallel_stream() starts
    ↓
AGENT_DELEGATED × 3 (fan-out)
    ↓
RUN_STARTED × 3 (parallel execution)
    ↓
TEXT_DELTA × N (multiplexed streaming)
    ↓
TEXT_DONE × 3 (results collected)
    ↓
AGENT_COMPLETED × 3 (completion markers)
    ↓
TOOL_RESULT × 3 (fan-in)
    ↓
LLM synthesizes final response
```

## Example LLM Response

```json
{
  "role": "assistant",
  "content": "I'll coordinate a comprehensive code review.",
  "tool_calls": [
    {
      "id": "call_1",
      "function": {
        "name": "delegate_task",
        "arguments": "{\"agent_id\": \"code_agent\", \"task\": \"Review code quality\"}"
      }
    },
    {
      "id": "call_2",
      "function": {
        "name": "delegate_task",
        "arguments": "{\"agent_id\": \"security_agent\", \"task\": \"Review security\"}"
      }
    },
    {
      "id": "call_3",
      "function": {
        "name": "delegate_task",
        "arguments": "{\"agent_id\": \"perf_agent\", \"task\": \"Review performance\"}"
      }
    }
  ]
}
```

ChatLoop automatically detects this as parallel delegation and executes all 3 agents concurrently.

## Benefits

1. **Automatic**: No special syntax needed - LLM just returns multiple tool calls
2. **Efficient**: True parallel execution with asyncio
3. **Streaming**: All agents stream in real-time, multiplexed to orchestrator
4. **Auditable**: Every event is tagged with `agent_id` for tracing
5. **Backward Compatible**: Single delegation still works as before (uses sequential path)
6. **Error Resilient**: Individual agent failures don't block other agents

## Testing

See `tests/integration/test_chatloop_parallel_delegation.py` for integration tests demonstrating:

- Parallel delegation detection
- Tool call batching
- Event multiplexing
- Result collection

Run tests:
```bash
pytest tests/integration/test_chatloop_parallel_delegation.py -v
```

## Example

See `examples/multi_agent_delegation.py` for a working example of parallel code review with 3 specialist agents.

Run example:
```bash
python examples/multi_agent_delegation.py
```

## Implementation Files

- `core/agent/chat_loop.py` - Parallel delegation detection and fan-in logic
- `core/skills/delegation.py` - `execute_parallel_stream()` with Queue-based multiplexing
- `core/agent/executor.py` - `execute_skill_stream()` for streaming skill execution

## Future Enhancements

- **Aggregation Strategies**: Custom result aggregation (voting, consensus, etc.)
- **Partial Failure Handling**: Continue even if some agents fail
- **Progress Tracking**: Real-time progress bars for each agent
- **Cost Tracking**: Per-agent cost attribution in parallel execution

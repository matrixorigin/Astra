# Real-Time Streaming Output

**Status**: Design  
**Created**: 2026-02-11  
**Phase**: 5

---

## The Problem

Current agent interaction is request-response: user sends message → waits → gets complete response. This creates three pain points:

1. **Perceived latency** — An agent that takes 15 seconds to think, call 3 tools, and compose a response feels broken. The user sees nothing for 15 seconds, then a wall of text. The same 15 seconds with streaming feels responsive — the user sees progress in real-time.

2. **No visibility into agent reasoning** — When the agent is calling tools, the user doesn't know what's happening. Is it stuck? Is it doing something dangerous? Streaming tool calls and intermediate results gives the user a chance to intervene before the agent finishes.

3. **Multi-agent workflows are opaque** — When an orchestrator delegates to 3 agents in parallel, the user sees nothing until all 3 complete. Streaming shows each agent's progress as it happens.

These aren't cosmetic issues. In production, users abort "stuck" agents that are actually working. Users can't catch mistakes mid-execution. And long-running workflows feel like black boxes.

---

## Design Principles

1. **AG-UI protocol alignment** — We adopt the AG-UI (Agent-User Interaction) event types as our streaming vocabulary. AG-UI is the emerging industry standard (adopted by CopilotKit, TanStack, Microsoft Agent Framework). Aligning now means future UI integrations are plug-and-play.

2. **Events are the stream** — We already log everything as events. Streaming is just **real-time event delivery**. No new data model — the same `conversation_events` that power audit and replay also power streaming. The stream is the event log, delivered live.

3. **Transport-agnostic** — The streaming protocol works over SSE (Server-Sent Events) for HTTP, WebSocket for persistent connections, and stdout for CLI. The event format is the same regardless of transport.

4. **Streaming doesn't break auditability** — Every streamed chunk is also a logged event. Replay produces the same stream. This is unique — most streaming implementations are fire-and-forget with no audit trail.

---

## Architecture

### Event Stream Types

Following AG-UI conventions, adapted to our event-centric architecture:

```python
class StreamEventType(str, Enum):
    # Lifecycle
    RUN_STARTED = "run_started"          # Agent begins processing
    RUN_FINISHED = "run_finished"        # Agent completes
    RUN_ERROR = "run_error"              # Agent encounters error
    
    # Text generation
    TEXT_DELTA = "text_delta"            # Incremental text chunk
    TEXT_DONE = "text_done"             # Text generation complete
    
    # Thinking / reasoning (for reasoning models)
    THINKING_DELTA = "thinking_delta"    # Chain-of-thought chunk
    THINKING_DONE = "thinking_done"      # Thinking complete
    
    # Tool use
    TOOL_CALL_START = "tool_call_start"  # Agent decides to call a tool
    TOOL_CALL_ARGS = "tool_call_args"    # Tool arguments (streamed)
    TOOL_CALL_END = "tool_call_end"      # Tool call complete
    TOOL_RESULT = "tool_result"          # Tool execution result
    
    # Planning (connects to autonomous planning)
    PLAN_CREATED = "plan_created"        # Agent generated a plan
    PLAN_STEP_START = "plan_step_start"  # Starting a plan step
    PLAN_STEP_DONE = "plan_step_done"    # Plan step completed
    PLAN_REVISED = "plan_revised"        # Plan was revised
    
    # Multi-agent (connects to multi-agent collaboration)
    AGENT_DELEGATED = "agent_delegated"  # Task delegated to another agent
    AGENT_PROGRESS = "agent_progress"    # Delegated agent progress update
    AGENT_COMPLETED = "agent_completed"  # Delegated agent finished
    
    # State
    STATE_DELTA = "state_delta"          # Shared state update
```

### Stream Event Format

```python
@dataclass
class StreamEvent:
    event_type: StreamEventType
    event_id: str                        # Same as conversation_event.event_id
    timestamp: datetime
    data: dict                           # Type-specific payload
    
    # Audit linkage
    causal_chain_id: str                 # Links to the conversation chain
    parent_event_id: str | None          # Parent in the causal chain
    agent_id: str | None                 # Which agent produced this
```

Example stream for a multi-turn tool use interaction:

```
→ {"event_type": "run_started", "data": {"query": "Why is CI failing?"}}
→ {"event_type": "thinking_delta", "data": {"chunk": "I need to check the CI logs..."}}
→ {"event_type": "thinking_done", "data": {}}
→ {"event_type": "tool_call_start", "data": {"tool": "ci_get_logs", "call_id": "tc_1"}}
→ {"event_type": "tool_call_args", "data": {"call_id": "tc_1", "args": "{\"repo\": \"mo\", \"run_id\": 456}"}}
→ {"event_type": "tool_call_end", "data": {"call_id": "tc_1"}}
→ {"event_type": "tool_result", "data": {"call_id": "tc_1", "result": "Test X failed: assertion error..."}}
→ {"event_type": "text_delta", "data": {"chunk": "The CI failure is caused by "}}
→ {"event_type": "text_delta", "data": {"chunk": "a broken assertion in test_user.py..."}}
→ {"event_type": "text_done", "data": {"full_text": "The CI failure is caused by..."}}
→ {"event_type": "run_finished", "data": {"usage": {"prompt_tokens": 1200, "completion_tokens": 350}}}
```

### Integration with ChatLoop

The existing `ChatLoop.run_step` returns a final string. We add a streaming variant:

```python
class ChatLoop:
    async def run_step_stream(
        self,
        user_input: str,
        session_id: str,
        user_id: str,
        context: dict | None = None,
    ) -> AsyncIterator[StreamEvent]:
        """Stream events as the agent processes a request."""
        
        yield StreamEvent(event_type=StreamEventType.RUN_STARTED, ...)
        
        # Multi-turn tool use loop (same logic as run_step)
        for _round in range(MAX_TOOL_ROUNDS):
            # Stream LLM response chunks
            async for chunk in self.llm.chat_with_tools_stream(messages, tools):
                if chunk.type == "thinking":
                    yield StreamEvent(event_type=StreamEventType.THINKING_DELTA, ...)
                elif chunk.type == "tool_call":
                    yield StreamEvent(event_type=StreamEventType.TOOL_CALL_START, ...)
                elif chunk.type == "text":
                    yield StreamEvent(event_type=StreamEventType.TEXT_DELTA, ...)
            
            # Execute tools and stream results
            for tc in tool_calls:
                yield StreamEvent(event_type=StreamEventType.TOOL_CALL_END, ...)
                result = self.executor.execute_skill(...)
                yield StreamEvent(event_type=StreamEventType.TOOL_RESULT, ...)
        
        yield StreamEvent(event_type=StreamEventType.RUN_FINISHED, ...)
```

`run_step` becomes a thin wrapper that collects the stream into a final response. Streaming is the primary path; non-streaming is derived from it.

### Transport Layer

```
┌─────────────┐     ┌──────────────────┐     ┌─────────────┐
│   CLI        │◄────│  StreamEvent     │────▶│   Web UI    │
│  (stdout)    │     │  AsyncIterator   │     │  (SSE/WS)   │
└─────────────┘     └──────────────────┘     └─────────────┘
                            │
                            ▼
                    ┌──────────────────┐
                    │ conversation_    │
                    │ events (logged)  │
                    └──────────────────┘
```

**CLI transport**: Print stream events to stdout with formatting:
```
Agent> [thinking] I need to check the CI logs...
Agent> [tool] ci_get_logs(repo="mo", run_id=456)
Agent> [result] Test X failed: assertion error in test_user.py:42
Agent> The CI failure is caused by a broken assertion in test_user.py...
```

**HTTP transport**: Server-Sent Events (SSE) endpoint:
```
GET /api/v1/chat/stream?session_id=xxx
Accept: text/event-stream

data: {"event_type": "run_started", ...}
data: {"event_type": "text_delta", "data": {"chunk": "The CI..."}}
...
```

**WebSocket transport**: For bidirectional communication (user can cancel mid-stream):
```
ws://host/api/v1/chat/ws?session_id=xxx
← {"event_type": "text_delta", ...}
→ {"action": "cancel"}  // User cancels
← {"event_type": "run_error", "data": {"reason": "cancelled_by_user"}}
```

### Streaming + Audit Trail

Every `StreamEvent` is simultaneously:
1. **Delivered to the client** in real-time
2. **Logged to `conversation_events`** for audit

This means:
- **Replay produces the same stream.** Replay a session and you get the same sequence of stream events, with the same timing metadata.
- **Time-travel on streams.** Query what the user saw at any point during a streaming response.
- **Stream forensics.** If a user reports "the agent said something wrong mid-stream then corrected itself," you can query the exact stream events to see what happened.

```sql
-- Reconstruct the stream for a specific interaction
SELECT event_type, content, created_at
FROM conversation_events
WHERE causal_chain_id = @chain_id
  AND event_type IN ('text_delta', 'tool_call_start', 'tool_result')
ORDER BY created_at
```

### Streaming + Multi-Agent

When the orchestrator delegates to parallel agents, the stream multiplexes:

```
→ {"event_type": "agent_delegated", "data": {"agent": "code_agent", "task": "Review code"}}
→ {"event_type": "agent_delegated", "data": {"agent": "security_agent", "task": "Review security"}}
→ {"event_type": "agent_progress", "data": {"agent": "code_agent", "chunk": "Analyzing file..."}}
→ {"event_type": "agent_progress", "data": {"agent": "security_agent", "chunk": "Checking deps..."}}
→ {"event_type": "agent_completed", "data": {"agent": "security_agent", "result": "No issues"}}
→ {"event_type": "agent_completed", "data": {"agent": "code_agent", "result": "Found 2 issues"}}
→ {"event_type": "text_delta", "data": {"chunk": "Based on the reviews..."}}
```

The client can render this as parallel progress indicators — each agent's progress shown separately.

### User Intervention

Streaming enables mid-execution intervention:

1. **Cancel** — User sends cancel signal. Agent stops after current step. Partial results are preserved as events.
2. **Redirect** — User sends new instruction mid-stream. Agent incorporates it into the current plan (connects to autonomous planning).
3. **Approve/Reject** — For gated operations, the stream pauses and waits for user input before continuing.

```python
class StreamControl:
    CANCEL = "cancel"           # Stop execution
    REDIRECT = "redirect"       # Change direction
    APPROVE = "approve"         # Approve gated action
    REJECT = "reject"           # Reject gated action
```

---

## Implementation Priority

**P0**: Text streaming for CLI (`text_delta` events over stdout)
**P1**: Tool call streaming (show tool calls and results in real-time)
**P2**: SSE transport for HTTP API
**P3**: Multi-agent stream multiplexing
**P4**: User intervention (cancel, redirect)
**P5**: WebSocket transport with bidirectional control

---

## What This Is NOT

- **Not just "print tokens as they arrive."** That's the trivial case. This is a structured event stream that covers tool calls, planning, multi-agent delegation, and user intervention — all auditable.
- **Not a separate system from events.** The stream IS the event log, delivered in real-time. No dual-write, no sync issues, no "streaming said X but the log says Y."
- **Not fire-and-forget.** Every streamed chunk is persisted. This is what makes streaming compatible with our audit and replay guarantees.

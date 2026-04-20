# Session Observability System Design

## Problem

Debugging astra sessions currently requires:
1. Reading raw JSONL journal + step_events + heavy checkpoints manually
2. Adding ad-hoc `tracing::debug!` logs and rebuilding
3. Guessing what happened between events because the data flow has gaps

The goal: **any session issue should be diagnosable from persisted session data alone, without adding logs or rebuilding.**

## Current State

### What exists

| Artifact | Location | Content | Gaps |
|----------|----------|---------|------|
| **Journal JSONL** | `~/.astra/sessions/{id}.jsonl` | `session_start`, `turn`, `llm_round`, `stall_detected`, `turn_guard_verdict`, `context_assembly_recorded`, `turn_evaluation`, `adaptive_*` | No per-round tool execution details; `turn` only has final aggregates |
| **Step events** | `{id}/step_events.jsonl` | `StepCreated/Started/Completed/Retried`, `ToolCallStarted/Completed/Failed`, `StallDetected` | No record of what was sent to LLM; no record of SSE events received |
| **Heavy checkpoints** | `{id}/step_checkpoints/NNN-heavy.json` | Full OpenAI messages array at each step boundary | Messages have empty `function.name` due to format bugs; no tool execution output |
| **Journal digest** | `astra journal digest` | Aggregated metrics per turn | Derived from journal; same gaps |
| **Diagnostic log** | `ASTRA_LOG_FILE` / `ASTRA_DIAGNOSTIC_LOG` | `tracing` output at runtime | Ephemeral; not persisted with session; requires rebuild to add new log points |

### What's missing for full runtime reconstruction

1. **LLM request/response per round**: what messages + tools were sent, what the LLM returned (tool_calls with names and arguments)
2. **Tool execution per call**: tool name, args, output (or truncated preview), duration, error, which path (edge vs headless vs server fallback)
3. **SSE event flow**: which `tool_request` events were emitted by bridge, which were accepted/dropped by CLI
4. **Skill interception**: which tool_calls were intercepted, what skill was resolved, what was surgically removed
5. **Stall/guard state transitions**: nudge content, escalation level changes, tool restrictions applied

## Design

### Principle: Event Store, not Logs

Every significant runtime action writes a **structured event** to the session's event store. Events are:
- **Append-only**: never modified after write
- **Causally linked**: each event references its parent event(s)
- **Self-describing**: contains enough context to understand without external state
- **Leveled**: events have a verbosity level (L0=always, L1=verbose, L2=debug)

### Event Schema

```rust
struct SessionEvent {
    ts: Instant,           // monotonic offset from session start
    event_type: String,    // e.g. "llm_request", "tool_exec", "sse_tool_request"
    turn: u32,
    round: u32,            // LLM round within turn (0-based)
    level: EventLevel,     // L0, L1, L2
    payload: Value,        // event-specific structured data
    parent_event_id: Option<String>,
}

enum EventLevel {
    L0,  // Always recorded: turn boundaries, tool results, errors, stalls
    L1,  // Verbose (/verbose): LLM request/response summaries, SSE flow, skill interception
    L2,  // Debug (ASTRA_SESSION_DEBUG=1): full message arrays, raw SSE events, checkpoint diffs
}
```

### Event Catalog

#### L0 — Always recorded (current + new)

| Event | Trigger | Payload |
|-------|---------|---------|
| `turn_start` | User message received | `{user_input, model, turn_index}` |
| `turn_end` | Turn complete | `{tokens_in, tokens_out, duration_ms, tool_calls_ok, tool_calls_fail}` |
| `llm_round` | Each LLM call returns | `{round, tokens_in, tokens_out, ttft_ms, tool_calls_returned, tool_call_names}` |
| `tool_exec` | Tool execution complete | `{tool, args_preview, output_preview, ok, ms, path (edge/headless/server)}` |
| `tool_error` | Tool execution failed | `{tool, error, error_category}` |
| `stall_detected` | Stall detector fires | `{stall_type, nudge_content_preview}` |
| `turn_guard_verdict` | Guard escalation | `{level, restricted_tools, reason}` |

#### L1 — Verbose (`/verbose` or `--verbose`)

| Event | Trigger | Payload |
|-------|---------|---------|
| `llm_request_summary` | Before LLM call | `{message_count, system_tokens, history_tokens, tool_schema_count, tool_schema_tokens, budget_pressure}` |
| `llm_response_summary` | After LLM response | `{finish_reason, text_preview(100), tool_calls: [{name, args_keys}], reasoning_preview(100)}` |
| `sse_tool_request` | Bridge emits tool_request | `{tool, request_id, args_keys}` |
| `sse_tool_request_dropped` | CLI drops tool_request | `{tool, request_id, reason}` |
| `skill_intercept` | Skill call intercepted | `{skill_name, resolved, surgically_removed_ids}` |
| `context_assembly` | Prompt assembled | `{tools_selected, memories_count, history_turns_retained, budget_pressure}` |
| `headless_match` | Tool output matched | `{tool, match_source (edge/by_sig/protocol_error), edge_round_len}` |

#### L2 — Debug (development only)

| Event | Trigger | Payload |
|-------|---------|---------|
| `llm_request_full` | Before LLM call | `{messages: [...], tools: [...]}` (full payload) |
| `llm_response_full` | After LLM response | `{raw_tool_calls: [...], full_text}` |
| `sse_raw` | Each SSE event | `{event_type, data_preview}` |
| `checkpoint_diff` | Step boundary | `{messages_added, messages_removed}` |

### Storage

```
~/.astra/sessions/{session_id}/
├── {session_id}.jsonl          # Journal (L0 events, backward compatible)
├── step_events.jsonl           # Step lifecycle events (existing)
├── step_checkpoints/           # Heavy/light checkpoints (existing)
├── turn_events.jsonl           # NEW: L0+L1 events (when verbose)
├── session-memory.md           # Session memory
└── workspace.yaml              # Workspace state
```

- **L0 events**: always written to journal JSONL (backward compatible with `astra journal digest`)
- **L1 events**: written to `turn_events.jsonl` when verbose mode is active
- **L2 events**: written to `turn_events.jsonl` only when `ASTRA_SESSION_DEBUG=1`
- **No separate file per level** — single `turn_events.jsonl` with `level` field for filtering

### Runtime Integration

```
┌─────────────────────────────────────────────────────────┐
│ TurnEventBuffer (per-turn, in-memory)                   │
│                                                         │
│  Collects events during a turn, flushes on turn_end.    │
│  Verbosity level set by /verbose toggle or --verbose.   │
│                                                         │
│  write_event(type, level, payload)                      │
│    → if level <= active_level: buffer.push(event)       │
│                                                         │
│  flush() → append to turn_events.jsonl                  │
│         → merge L0 events into journal JSONL             │
└─────────────────────────────────────────────────────────┘
```

Instrumentation points (where `write_event` is called):

| Location | Events |
|----------|--------|
| `agentic_loop_tool_phase.rs` | `turn_start`, `llm_round`, `turn_end` |
| `headless_tool_pipeline/execute.rs` | `tool_exec`, `tool_error`, `headless_match` |
| `bridge_llm_stream.rs` | `llm_request_summary`, `llm_response_summary` |
| `bridge_inprocess.rs` | `sse_tool_request` |
| `chat_turn_sse_dispatch.rs` | `sse_tool_request_dropped` |
| `agentic_tool_interception.rs` | `skill_intercept` |
| `stall.rs` / `turn_guard.rs` | `stall_detected`, `turn_guard_verdict` |

### CLI Interface

```bash
# Toggle verbose during session
> /verbose              # Enable L1 events for this session
> /verbose off          # Disable

# Start with verbose
astra chat --verbose

# Analyze session with full timeline
astra journal trace <session_id>
# Output:
# 00:00.000 [turn_start] user="review local changes" model=qwen3.6-plus
# 00:00.190 [context_assembly] tools=20 memories=3 budget=18%
# 00:02.557 [llm_round] round=0 tokens_in=9102 ttft=1939ms tool_calls=[skill]
# 00:02.558 [sse_tool_request] tool=skill request_id=call_a326...
# 00:02.560 [skill_intercept] skill_name="" resolved=false reason="empty skill_name"
# 00:02.561 [tool_exec] tool=skill ok=true ms=0 output="Failed to load skill ''"
# 00:05.100 [llm_round] round=1 tokens_in=12000 tool_calls=[skill]
# ...
# 00:15.513 [stall_detected] type=intent_drift
# 00:19.542 [llm_round] round=4 tool_calls=[git_status, git_diff]
# 00:19.560 [tool_exec] tool=git_status ok=true ms=18 path=edge
# 00:19.568 [tool_exec] tool=git_diff ok=true ms=8 path=edge
# 00:46.031 [turn_end] tokens_in=70863 tokens_out=1517 tools_ok=4 tools_fail=1

# Debug level (full payloads)
ASTRA_SESSION_DEBUG=1 astra chat --verbose
```

### Migration Path

1. **Phase 1** (this PR): Add `TurnEventBuffer` with L0 events. Wire into existing journal write path. No new files.
2. **Phase 2**: Add `turn_events.jsonl` for L1 events. Wire `/verbose` toggle. Add `astra journal trace`.
3. **Phase 3**: Add L2 events. Wire `ASTRA_SESSION_DEBUG`. Add `astra journal trace --debug`.

### Design Decisions

1. **Why not just use tracing?** Tracing is ephemeral and requires `RUST_LOG` at startup. Session events are persisted with the session and can be analyzed days later.

2. **Why not extend the journal JSONL?** Journal is the stable contract for `astra journal digest`. L1/L2 events would bloat it. Separate `turn_events.jsonl` keeps the journal lean.

3. **Why a buffer, not direct writes?** Buffering per-turn allows atomic flush (no partial turn data) and avoids I/O on every event.

4. **Why levels, not categories?** Levels are simpler to reason about: "I want more detail" → increase level. Categories require knowing which categories exist.

5. **Backward compatibility**: L0 events are a superset of current journal events. Existing `astra journal digest` continues to work unchanged.

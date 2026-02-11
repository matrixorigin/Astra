# Multi-Agent Collaboration

**Status**: Design  
**Created**: 2026-02-11  
**Phase**: 5

---

## The Problem

Single-agent architectures hit a wall when tasks require:

1. **Parallel specialization** — "Analyze this PR's code quality, security implications, and performance impact simultaneously." One agent doing all three sequentially is slow. Three specialists in parallel is fast, but they need to share findings.

2. **Cross-domain reasoning** — A bug triage task needs a code agent (reads source), a CI agent (reads logs), and a data agent (queries metrics). Each has different tool access and context. Forcing one agent to hold all context exceeds token limits and degrades quality.

3. **Adversarial verification** — An agent generates a fix. Who checks it? Having the same agent self-check is unreliable. A separate reviewer agent with different instructions catches errors the author misses.

4. **Long-running workflows** — "Monitor this deployment for 2 hours, escalate if error rate exceeds 5%." This requires an agent that delegates, waits, and reacts — not a single synchronous call.

These are real production needs, not theoretical. Teams using single-agent systems work around them with manual orchestration, which defeats the purpose.

---

## Design Principles

1. **Event-sourced coordination** — All inter-agent communication flows through `conversation_events`. No hidden channels. Every message, delegation, and result is an auditable event with causal chain linkage.

2. **Shared-nothing execution, shared-everything data** — Agents don't share memory or state directly. They share a database. Each agent reads/writes events. This eliminates concurrency bugs and makes the system naturally replayable.

3. **Skill-scoped agents** — Each agent is defined by its skill set, not by custom code. A "security reviewer" agent is just an agent with security-related skills and a security-focused system prompt. This reuses the existing skill infrastructure.

4. **Auditable delegation** — When Agent A delegates to Agent B, the delegation is an event. When B returns results, that's an event. The full delegation chain is traceable via `causal_chain_id`. This is the same audit trail as single-agent, extended to multi-agent.

---

## Architecture

### Coordination Model: Event Blackboard

We use an **event blackboard** pattern — agents coordinate through shared events in the database, not through direct message passing.

```
┌──────────────┐     ┌──────────────┐     ┌──────────────┐
│  Code Agent  │     │   CI Agent   │     │  Data Agent  │
│  (skills:    │     │  (skills:    │     │  (skills:    │
│   code_read, │     │   ci_logs,   │     │   sql_query, │
│   code_fix)  │     │   ci_trigger)│     │   metrics)   │
└──────┬───────┘     └──────┬───────┘     └──────┬───────┘
       │                    │                    │
       ▼                    ▼                    ▼
┌─────────────────────────────────────────────────────────┐
│              conversation_events (MatrixOne)             │
│                                                         │
│  event: delegate(from=orchestrator, to=code_agent, ...) │
│  event: skill_result(agent=code_agent, ...)             │
│  event: delegate(from=orchestrator, to=ci_agent, ...)   │
│  event: skill_result(agent=ci_agent, ...)               │
│  event: synthesize(from=orchestrator, inputs=[...])     │
└─────────────────────────────────────────────────────────┘
       ▲
       │
┌──────┴───────┐
│ Orchestrator │  ← Plans, delegates, synthesizes
│   Agent      │
└──────────────┘
```

**Why event blackboard instead of direct messaging:**

- **Replay**: Replay a multi-agent workflow by replaying events. Each agent re-executes against the same event stream.
- **Audit**: Complete visibility into who said what to whom and why.
- **Fault tolerance**: If an agent crashes, its last event is in the database. Another instance can pick up from there.
- **Time-travel debugging**: Use MatrixOne's time-travel queries to inspect the blackboard state at any point during a multi-agent workflow.

### Agent Definition

An agent is not a new abstraction — it's a **ChatLoop instance with a specific configuration**:

```python
@dataclass
class AgentConfig:
    agent_id: str                    # e.g., "code_reviewer"
    system_prompt: str               # Role-specific instructions
    skill_ids: list[str]             # Which skills this agent can use
    max_tool_rounds: int = 10        # Per-turn tool use limit
    can_delegate: bool = False       # Can this agent delegate to others?
    delegate_to: list[str] = []      # Which agents it can delegate to
```

No new execution engine. The existing `ChatLoop` handles tool use. Delegation is just another skill.

### Delegation as a Skill

```python
# Delegation is a skill like any other
{
    "skill_id": "delegate_task",
    "name": "Delegate Task to Specialist",
    "parameters": {
        "target_agent": {"type": "string", "description": "Agent ID to delegate to"},
        "task": {"type": "string", "description": "Task description"},
        "context": {"type": "string", "description": "Relevant context to pass"},
        "wait_for_result": {"type": "boolean", "default": True}
    }
}
```

When the orchestrator's LLM decides to delegate, it calls `delegate_task` like any other tool. The skill executor:
1. Creates a delegation event (parent = current event, same causal chain)
2. Spawns the target agent's ChatLoop with the task
3. Target agent executes, logs events, returns result
4. Result is logged as an event and returned to the orchestrator

### Coordination Patterns

**Pattern 1: Fan-out / Fan-in (Parallel)**

```
Orchestrator receives: "Full review of PR #123"
  → delegate(code_agent, "Review code quality")     ─┐
  → delegate(security_agent, "Review security")      ├─ parallel
  → delegate(perf_agent, "Review performance")       ─┘
  ← collect all results
  → synthesize final review
```

Each delegate runs in its own ChatLoop. Results are collected as events. The orchestrator synthesizes after all complete.

**Why MatrixOne matters here**: Each parallel agent can operate in its own zero-copy branch if it needs to modify data (e.g., run test queries). Branches are instant, free, and isolated. No agent can corrupt another's workspace.

**Pattern 2: Pipeline (Sequential)**

```
Orchestrator receives: "Fix the failing test in CI"
  → delegate(ci_agent, "Get failure details")
  ← ci_agent returns: "Test X fails with error Y"
  → delegate(code_agent, "Fix error Y in file Z")
  ← code_agent returns: patch
  → delegate(ci_agent, "Run test with patch")
  ← ci_agent returns: pass/fail
```

Each step's output becomes the next step's input. The orchestrator decides the next step based on results.

**Pattern 3: Adversarial Review**

```
Orchestrator receives: "Generate and verify a code fix"
  → delegate(code_agent, "Generate fix for issue #456")
  ← code_agent returns: proposed fix
  → delegate(reviewer_agent, "Review this fix: {proposed_fix}")
  ← reviewer_agent returns: approval or objections
  → if objections: delegate(code_agent, "Revise fix based on: {objections}")
  → loop until approved or max iterations
```

The reviewer agent has a different system prompt emphasizing criticism. This catches errors that self-review misses.

**Pattern 4: Supervisor with Escalation**

```
Supervisor agent monitors:
  → delegate(monitor_agent, "Watch deployment metrics for 1 hour")
  ← monitor_agent streams: periodic status events
  ← monitor_agent escalates: "Error rate exceeded 5%"
  → delegate(triage_agent, "Diagnose error spike")
  ← triage_agent returns: root cause
  → delegate(code_agent, "Generate hotfix")
  → human approval gate
```

### Multi-Agent Replay

The key innovation: **multi-agent workflows are replayable with the same guarantees as single-agent**.

Because all coordination flows through events:
1. Capture all events during production execution
2. To replay: re-execute each agent against the same event inputs
3. Side-effect isolation (mock mode) prevents real actions
4. Each agent can replay in its own sandbox branch

**Time-travel debugging for multi-agent**:
```sql
-- What did the code_agent see when it generated the fix?
SELECT * FROM conversation_events
  {SNAPSHOT = 'workflow_123_step_3'}
WHERE causal_chain_id = @chain_id
  AND agent_id = 'code_agent'
ORDER BY created_at
```

This is only possible because the event blackboard is in MatrixOne with time-travel support.

### Conflict Resolution

When parallel agents produce conflicting results:

1. **Orchestrator decides** — The orchestrator LLM sees all results and resolves conflicts. This is the default.
2. **Voting** — For adversarial patterns, multiple reviewers vote. Majority wins.
3. **Human escalation** — If confidence is low, escalate to human. The escalation is an event, so the human's decision is also auditable.

---

## Data Model

```sql
-- Agent registry
CREATE TABLE agent_configs (
    agent_id        VARCHAR(100) PRIMARY KEY,
    system_prompt   TEXT NOT NULL,
    skill_ids       JSON NOT NULL,          -- ["code_read", "code_fix"]
    max_tool_rounds INT DEFAULT 10,
    can_delegate    BOOLEAN DEFAULT FALSE,
    delegate_to     JSON DEFAULT '[]',      -- ["code_agent", "ci_agent"]
    created_at      TIMESTAMP DEFAULT NOW(),
    updated_at      TIMESTAMP DEFAULT NOW()
);

-- Delegation tracking (extends conversation_events)
-- No new table needed — delegation events use existing event schema:
--   event_type = 'agent_delegation'
--   content = task description
--   metadata = {"target_agent": "...", "source_agent": "...", "pattern": "fan_out"}
--   parent_event_id = delegating event
--   causal_chain_id = shared across entire workflow
```

---

## Implementation Priority

**P0**: Single orchestrator + fan-out/fan-in (covers 80% of use cases)
**P1**: Pipeline pattern + adversarial review
**P2**: Long-running supervisor with escalation
**P3**: Dynamic agent spawning (orchestrator creates new agent configs on the fly)

---

## What This Is NOT

- **Not a multi-agent chat room.** Agents don't have free-form conversations with each other. The orchestrator delegates specific tasks and collects results. This is deliberate — unconstrained agent-to-agent chat is unpredictable and hard to audit.
- **Not a new execution engine.** Multi-agent reuses ChatLoop, skills, events, and the existing executor. The only new concept is delegation-as-a-skill.
- **Not autonomous swarm intelligence.** Agents don't self-organize. The orchestrator plans and delegates. This is a practical choice — autonomous swarms are research-grade, not production-grade.

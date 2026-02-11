# Multi-Agent Collaboration

**Status**: Design (Updated 2026-02-11)  
**Created**: 2026-02-11  
**Phase**: 5

---

## The Problem

Single-agent architectures hit a wall when tasks require:

1. **Parallel specialization** — "Analyze this PR's code quality, security implications, and performance impact simultaneously." One agent doing all three sequentially is slow. Three specialists in parallel is fast, but they need to share findings.

2. **Cross-domain reasoning** — A bug triage task needs a code agent (reads source), a CI agent (reads logs), and a data agent (queries metrics). Each has different tool access and context. Forcing one agent to hold all context exceeds token limits and degrades quality.

3. **Adversarial verification** — An agent generates a fix. Who checks it? Having the same agent self-check is unreliable. A separate reviewer agent with different instructions catches errors the author misses.

4. **Long-running workflows** — "Monitor this deployment for 2 hours, escalate if error rate exceeds 5%." This requires an agent that delegates, waits, and reacts — not a single synchronous call.

5. **Platform self-maintenance** — The platform itself needs agents: regression testing on every change, periodic audit of past decisions, prompt optimization from quality signals. These are "System Agents" — same execution model, different trigger and permissions.

These are real production needs, not theoretical. Teams using single-agent systems work around them with manual orchestration, which defeats the purpose.

---

## Design Principles

1. **Event-sourced coordination** — All inter-agent communication flows through `conversation_events`. No hidden channels. Every message, delegation, and result is an auditable event with causal chain linkage.

2. **Shared-nothing execution, shared-everything data** — Agents don't share memory or state directly. They share a database. Each agent reads/writes events. This eliminates concurrency bugs and makes the system naturally replayable.

3. **Skill-scoped agents** — Each agent is defined by its skill set, not by custom code. A "security reviewer" agent is just an agent with security-related skills and a security-focused system prompt. This reuses the existing skill infrastructure.

4. **Auditable delegation** — When Agent A delegates to Agent B, the delegation is an event. When B returns results, that's an event. The full delegation chain is traceable via `causal_chain_id`. This is the same audit trail as single-agent, extended to multi-agent.

5. **Three-tier agent taxonomy** — Platform Capabilities (kernel APIs) → System Agents (daemons) → User Agents (apps). All agents use the same ChatLoop; the difference is permissions, triggers, and purpose.

---

## Architecture

### Agent Taxonomy

```
┌─────────────────────────────────────────────────────────────┐
│  USER AGENTS                                                │
│  Defined by users/developers. Solve domain problems.        │
│                                                             │
│  ┌─────────────┐ ┌─────────────┐ ┌──────────────────┐      │
│  │ Code Review  │ │ CI Diagnosis│ │ Data Analysis    │      │
│  │ Agent        │ │ Agent       │ │ Agent            │      │
│  └─────────────┘ └─────────────┘ └──────────────────┘      │
├─────────────────────────────────────────────────────────────┤
│  SYSTEM AGENTS                                              │
│  Pre-installed. Maintain platform health. Auto-triggered.   │
│                                                             │
│  ┌─────────────┐ ┌─────────────┐ ┌──────────────────┐      │
│  │ Regression   │ │ Audit       │ │ Tuning           │      │
│  │ Agent        │ │ Agent       │ │ Agent            │      │
│  └─────────────┘ └─────────────┘ └──────────────────┘      │
├─────────────────────────────────────────────────────────────┤
│  PLATFORM CAPABILITIES (not agents — APIs)                  │
│  Event Bus · Sandbox · Time Travel · LLM · Streaming ·     │
│  Skill Registry · Planning Engine · Regression Gate ·       │
│  Hallucination Firewall · Cost Control                      │
└─────────────────────────────────────────────────────────────┘
```

**Key distinction**: Hallucination Firewall is a Platform Capability (API). Audit Agent is a System Agent that *calls* the Firewall API. Code Review Agent is a User Agent that benefits from the Firewall transparently.

### Coordination Model: Event Blackboard

Agents coordinate through shared events in the database, not through direct message passing.

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

### End-to-End Execution Flow

This is how a complex user request actually flows through the system:

```
User: "Fix the failing CI and make sure it doesn't break anything else"
                    │
                    ▼
Step 1: CLASSIFY ──────────────────────────────────────────────
    Orchestrator receives request.
    _needs_planning() → True (multi-step, cross-domain)
    Enter PAOR loop.

Step 2: PLAN ──────────────────────────────────────────────────
    Planner generates:
      Plan v1:
        step 1: delegate(ci_agent, "Get CI failure details")
        step 2: delegate(code_agent, "Fix the root cause")  [depends: step 1]
        step 3: delegate(ci_agent, "Re-run CI with fix")    [depends: step 2]
        step 4: delegate(regression_agent, "Run regression") [depends: step 3]
    
    → event: plan_created (causal_chain_id=chain_001)
    → stream: PLAN_CREATED to user

Step 3: ACT (step 1) ─────────────────────────────────────────
    Spawn child ChatLoop for ci_agent:
      - system_prompt: "You diagnose CI failures..."
      - skills: [ci_get_logs, ci_get_config, ci_list_runs]
      - model: gpt-4o-mini (fast, cheap — this is a read task)
    
    ci_agent runs its own tool loop:
      → calls ci_get_logs(run_id=latest)
      → calls ci_get_config()
      → returns: "Test test_auth.py fails: missing env var DB_HOST"
    
    → event: agent_delegation (target=ci_agent, chain=chain_001)
    → event: tool_call (agent=ci_agent, skill=ci_get_logs, chain=chain_001)
    → event: agent_completed (agent=ci_agent, result=..., chain=chain_001)
    → stream: AGENT_DELEGATED, AGENT_PROGRESS, AGENT_COMPLETED

Step 4: OBSERVE + REFLECT ─────────────────────────────────────
    Orchestrator reads ci_agent result from events.
    Reflect: "Root cause identified. Plan still valid. Continue."
    
    → event: plan_reflection (chain=chain_001)

Step 5: ACT (step 2) ─────────────────────────────────────────
    Spawn child ChatLoop for code_agent:
      - system_prompt: "You fix code issues..."
      - skills: [code_read, code_write, code_search]
      - model: gpt-4o (complex reasoning task)
      - context: ci_agent's findings (passed via delegation event)
    
    code_agent runs its own tool loop:
      → calls code_search("DB_HOST", "test_auth.py")
      → calls code_read("ci/config.yml")
      → calls code_write("ci/config.yml", patch)
      → returns: patch + explanation
    
    All events logged with same chain_001.

Step 6: OBSERVE + REFLECT ─────────────────────────────────────
    Orchestrator reads code_agent result.
    Reflect: "Fix looks reasonable. But step 3 (re-run CI) might fail
              if the fix is wrong. Plan still valid. Continue."

Step 7: ACT (step 3) — re-run CI ─────────────────────────────
    ci_agent spawned again, triggers CI run, waits for result.
    
    → CI passes ✅

Step 8: OBSERVE + REFLECT ─────────────────────────────────────
    Reflect: "CI passes. Now run regression to make sure nothing
              else broke. Continue to step 4."

Step 9: ACT (step 4) — regression ─────────────────────────────
    Spawn regression_agent (System Agent):
      - Creates sandbox from current snapshot
      - Replays golden sessions in sandbox
      - Computes quality delta
      - Returns: pass/fail + metrics
    
    This is a System Agent using Platform Capabilities (Sandbox,
    Replay, Regression Gate) — same ChatLoop, elevated permissions.

Step 10: SYNTHESIZE ───────────────────────────────────────────
    All steps complete. Orchestrator synthesizes:
    "Fixed CI failure: added DB_HOST to test env config.
     CI now passes. Regression gate: 47/47 golden sessions pass,
     0 regressions detected."
    
    → event: llm_response (chain=chain_001)
    → stream: TEXT_DELTA... TEXT_DONE, RUN_FINISHED
```

**What the user sees** (via streaming multiplexer):
```
Agent> [plan] 4 steps: diagnose CI → fix code → verify → regression test
Agent> [ci_agent] Checking CI logs...
Agent> [ci_agent] Found: test_auth.py fails — missing env var DB_HOST
Agent> [code_agent] Searching for DB_HOST references...
Agent> [code_agent] Found issue in ci/config.yml. Applying fix...
Agent> [ci_agent] Re-running CI... ✅ passed
Agent> [regression] Running 47 golden sessions in sandbox... ✅ 0 regressions
Agent> Fixed CI failure: added DB_HOST to test env config. All checks pass.
```

### Agent Definition

An agent is not a new abstraction — it's a **ChatLoop instance with a specific configuration**:

```python
@dataclass
class AgentProfile:
    agent_id: str                    # e.g., "code_reviewer"
    system_prompt: str               # Role-specific instructions
    skill_filter: list[str] | None   # Which skills this agent can use
    model: str | None                # Optional model override
    can_delegate: bool = False       # Can this agent delegate to others?
    delegate_to: list[str] = []      # Which agents it can delegate to
    tier: str = "user"               # "user" | "system" | "orchestrator"
    triggers: list[str] = []         # Auto-trigger events (system agents only)
```

No new execution engine. The existing `ChatLoop` handles tool use. Delegation is just another skill.

### System Agent Definitions

```python
# Pre-installed system agents
SYSTEM_AGENTS = [
    AgentProfile(
        agent_id="regression_agent",
        system_prompt="You are a regression testing agent. When triggered, "
                      "create a sandbox, replay golden sessions against the "
                      "change, and report quality delta.",
        skill_filter=["create_sandbox", "replay_session", "compute_quality_delta",
                       "drop_sandbox"],
        tier="system",
        triggers=["skill_version_changed", "prompt_template_changed"],
    ),
    AgentProfile(
        agent_id="audit_agent",
        system_prompt="You are an audit agent. Verify past decisions against "
                      "current knowledge. Use time-travel to see what the agent "
                      "saw, then check if the answer is still valid.",
        skill_filter=["time_travel_query", "verify_claims", "flag_inconsistency"],
        tier="system",
        triggers=["periodic_24h", "knowledge_base_updated"],
    ),
    AgentProfile(
        agent_id="tuning_agent",
        system_prompt="You are a prompt tuning agent. Analyze low-scoring "
                      "interactions, identify patterns, and propose prompt "
                      "improvements. Test improvements in sandbox before "
                      "recommending.",
        skill_filter=["query_low_scores", "analyze_patterns", "create_sandbox",
                       "test_prompt_variant", "propose_improvement"],
        tier="system",
        triggers=["quality_score_below_threshold"],
    ),
]
```

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

### How Planning and Multi-Agent Compose

Planning (PAOR) and multi-agent are orthogonal but composable:

```
                    ┌─────────────────────────────────┐
                    │  Orchestrator PAOR Loop          │
                    │                                  │
                    │  Plan step 1: skill call ────────┼──→ direct execution
                    │  Plan step 2: delegate ──────────┼──→ child agent A
                    │  Plan step 3: delegate ──────────┼──→ child agent B (parallel with A)
                    │  Plan step 4: skill call ────────┼──→ direct execution (after A,B done)
                    │                                  │
                    │  Reflect after each step.        │
                    │  Revise plan if needed.           │
                    └─────────────────────────────────┘
                                    │
                    ┌───────────────┼───────────────┐
                    ▼                               ▼
            ┌──────────────┐                ┌──────────────┐
            │ Child Agent A │                │ Child Agent B │
            │ (may also     │                │ (simple task, │
            │  plan its own │                │  no planning) │
            │  sub-steps)   │                │               │
            └──────────────┘                └──────────────┘
```

- The orchestrator's PAOR loop generates a plan where steps can be skill calls OR delegations.
- A child agent can itself enter a PAOR loop if its task is complex enough.
- Depth is bounded by `PlanConstraints.max_steps` at each level.
- Each agent's events share the same `causal_chain_id` — the entire tree is one auditable workflow.

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

**Pattern 5: System Agent Auto-Trigger**

```
Event: skill_version_changed(skill_id="code_review", v1.2 → v1.3)
  → Platform detects trigger, spawns regression_agent
  → regression_agent:
      1. create_sandbox("regression_code_review_v1.3")
      2. replay_session(golden_sessions, sandbox)
      3. compute_quality_delta(baseline, current)
      4. if delta < threshold: block_deployment(skill_id, reason)
      5. drop_sandbox()
  → event: gate_result(passed=True/False, metrics={...})
```

System Agents are triggered by platform events, not user requests. They use the same ChatLoop + Skills, but have access to platform-level skills (sandbox, replay, time-travel).

### Streaming Integration

When multiple agents run, the streaming multiplexer merges their outputs:

```
┌──────────────┐  stream  ┌──────────────────────┐  merged   ┌────────┐
│  Code Agent  │─────────▶│                      │──────────▶│        │
└──────────────┘          │  Streaming            │           │  User  │
┌──────────────┐  stream  │  Multiplexer          │           │        │
│  CI Agent    │─────────▶│                      │           │        │
└──────────────┘          │  Tags each event with │           │        │
┌──────────────┐  stream  │  agent_id for UI      │           │        │
│  Orchestrator│─────────▶│  rendering            │           │        │
└──────────────┘          └──────────────────────┘           └────────┘
```

Each `StreamEvent` carries `agent_id`:
```python
StreamEvent(event_type=AGENT_PROGRESS, agent_id="code_agent", data={"chunk": "Reading file..."})
StreamEvent(event_type=AGENT_PROGRESS, agent_id="ci_agent", data={"chunk": "Fetching logs..."})
StreamEvent(event_type=AGENT_COMPLETED, agent_id="ci_agent", data={"result": "..."})
```

The CLI renders this as:
```
[code_agent] Reading file...
[ci_agent]   Fetching logs...
[ci_agent]   ✅ Done: Test X fails with error Y
[code_agent] Applying fix...
[code_agent] ✅ Done: Patched ci/config.yml
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
-- Agent registry (extends current AgentProfile)
CREATE TABLE agent_configs (
    agent_id        VARCHAR(100) PRIMARY KEY,
    system_prompt   TEXT NOT NULL,
    skill_ids       JSON NOT NULL,          -- ["code_read", "code_fix"]
    model           VARCHAR(100),           -- Optional model override
    max_tool_rounds INT DEFAULT 10,
    can_delegate    BOOLEAN DEFAULT FALSE,
    delegate_to     JSON DEFAULT '[]',      -- ["code_agent", "ci_agent"]
    tier            VARCHAR(20) DEFAULT 'user',  -- "user" | "system" | "orchestrator"
    triggers        JSON DEFAULT '[]',      -- ["skill_version_changed"] (system agents)
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
--   agent_id = which agent produced this event
```

---

## Control Plane / Data Plane (Future: Phase 8)

When the platform scales to multi-tenant, the agent system splits:

| Plane | Contains | Storage |
|---|---|---|
| **Control Plane** (our service) | agent_configs, skills_registry, prompt_templates, model_registry, gate_results | Lightweight DB (or MO) |
| **Data Plane** (user's MO) | conversation_events, llm_call_logs, sandboxes, snapshots | User's MatrixOne instance |

The Control Plane defines "what agents exist and what they can do." The Data Plane stores "what agents did and what they remember." This separation enables: user data stays in user's MO, we only manage configuration.

Not implemented now — noted here as a design constraint for future architecture decisions.

---

## Implementation Priority

**P0**: Delegation skill + child ChatLoop spawning (the core mechanism)
**P1**: Fan-out/fan-in with stream multiplexing (covers 80% of use cases)
**P2**: Pipeline pattern + adversarial review
**P3**: System Agent auto-trigger framework
**P4**: Long-running supervisor with escalation
**P5**: Dynamic agent spawning (orchestrator creates new agent configs on the fly)

---

## What This Is NOT

- **Not a multi-agent chat room.** Agents don't have free-form conversations with each other. The orchestrator delegates specific tasks and collects results. This is deliberate — unconstrained agent-to-agent chat is unpredictable and hard to audit.
- **Not a new execution engine.** Multi-agent reuses ChatLoop, skills, events, and the existing executor. The only new concept is delegation-as-a-skill.
- **Not autonomous swarm intelligence.** Agents don't self-organize. The orchestrator plans and delegates. This is a practical choice — autonomous swarms are research-grade, not production-grade.
- **Not a separate system from planning.** Planning (PAOR) and multi-agent are orthogonal. A plan step can delegate. A child agent can plan. They compose naturally because both use the same event system.
- **Not a microservice architecture.** All agents run in the same process (for now). "Spawning a child ChatLoop" is an in-process call, not an RPC. This keeps things simple and debuggable. Process isolation is a Phase 8 concern.

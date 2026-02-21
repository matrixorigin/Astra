# Agents and Orchestration

> **Status**: Core Design — single source of truth for agent execution, planning, and multi-agent coordination  
> **Last Updated**: 2026-02-14

---

## 1. What an Agent Is

An agent is a **ChatLoop instance with a specific configuration**:

```python
AgentProfile = {
    agent_id: str,           # "code_reviewer"
    system_prompt: str,      # Role-specific instructions
    skill_filter: list[str], # Which skills this agent can use
    model: str,              # Model override (optional)
    can_delegate: bool,      # Can delegate to other agents?
    delegate_to: list[str],  # Which agents it can delegate to
    tier: str,               # "user" | "system" | "orchestrator"
    triggers: list[str],     # Auto-trigger events (system agents only)
}
```

No new execution engine. No special runtime. The existing ChatLoop handles everything. Delegation is just another skill.

### Agent Taxonomy

```
USER AGENTS — Solve domain problems. Defined by users.
  Code Review, CI Diagnosis, Data Analysis, Security Audit

SYSTEM AGENTS — Maintain platform health. Auto-triggered.
  Regression Agent, Audit Agent, Tuning Agent, Eval Agent

PLATFORM CAPABILITIES — Not agents. APIs that agents call.
  Memory, Context, Sandbox, Time Travel, LLM, Streaming, Skills, Planning
```

**Key distinction**: Hallucination Firewall is a Platform Capability (API). Audit Agent is a System Agent that calls the Firewall API. Code Review Agent is a User Agent that benefits from the Firewall transparently.

---

## 2. The Execution Model: ChatLoop

> **See also**: [Durable Agent Runs](durable-agent-runs.md) — how ChatLoop is wrapped in a durable, resumable AgentRun for complex tasks that outlive a single HTTP request.

Every agent — user or system — runs the same loop:

```
User Input
    │
    ▼
┌─────────────────────────────────────────────────────────────┐
│  ChatLoop                                                   │
│                                                             │
│  1. Build context (see memory-and-context.md)               │
│  2. Call LLM with tools                                     │
│  3. If LLM returns tool_calls:                              │
│     a. Execute each tool (with side-effect enforcement)     │
│     b. Append tool results to messages                      │
│     c. Go to step 2 (multi-turn tool use)                   │
│  4. If LLM returns text: deliver response                   │
│  5. Log all events with causal_chain_id                     │
│                                                             │
│  Max rounds: configurable (default 10)                      │
│  Every step: streamed to client in real-time                │
└─────────────────────────────────────────────────────────────┘
```

### Streaming

Following AG-UI protocol conventions:

```
RUN_STARTED → THINKING_DELTA* → TOOL_CALL_START → TOOL_CALL_ARGS → 
TOOL_CALL_END → TOOL_RESULT → TEXT_DELTA* → TEXT_DONE → RUN_FINISHED
```

Every streamed chunk is simultaneously:
1. Delivered to the client in real-time
2. Logged to `conversation_events` for audit

Replay produces the same stream. Time-travel on streams. Stream forensics.

### Planning Integration

Before entering the tool loop, ChatLoop checks if the task needs planning:

```
Simple task ("What's the CI status?") → Direct skill execution
Complex task ("Fix the failing CI and run regression") → Enter PAOR loop
```

---

## 3. Autonomous Planning: PAOR Loop

### Plan-Act-Observe-Reflect

```
     ┌──────────────────────────────────────────┐
     │                                          │
     ▼                                          │
┌─────────┐     ┌─────────┐     ┌──────────┐   │
│  PLAN   │────▶│   ACT   │────▶│ OBSERVE  │   │
│         │     │         │     │          │   │
│ Generate│     │ Execute │     │ Check    │   │
│ or      │     │ next    │     │ result   │   │
│ revise  │     │ step    │     │ against  │   │
│ plan    │     │         │     │ expected │   │
└─────────┘     └─────────┘     └────┬─────┘   │
                                     │         │
                                     ▼         │
                                ┌──────────┐   │
                                │ REFLECT  │───┘
                                │          │
                                │ Continue?│
                                │ Revise?  │──▶ Back to PLAN
                                │ Done?    │──▶ Final Response
                                │ Escalate?│──▶ Human
                                └──────────┘
```

### Plans Are Data

Plans are stored as events — no new tables, no special runtime:

```
event_type = "plan_created"    → content = JSON plan structure        ✅ Implemented
event_type = "plan_revised"    → content = revised plan, revision_of  ✅ Implemented
event_type = "plan_step_start" → content = step description           ✅ Implemented
event_type = "plan_step_done"  → content = outcome + reflection       ✅ Implemented
event_type = "plan_completed"  → content = final summary              ✅ Implemented
event_type = "plan_failed"     → content = reason                     ✅ Implemented
```

All linked by `causal_chain_id`. All replayable. All auditable.

### Plan Versioning

```
Plan v1: [A → B → C → D]
          ↓ (B fails)
Plan v2: [A → B' → C → D]    revision_of = v1
          ↓ (C reveals new requirement)
Plan v3: [A → B' → C → E → D]  revision_of = v2
```

Every revision is queryable. Time-travel to any plan state. ✅ Revision events persisted via `Planner.log_plan_revised()`.

### Safety Boundaries

```python
PlanConstraints = {
    max_steps: 20,           # No runaway plans          ✅ Enforced
    max_revisions: 5,        # Don't revise forever      ✅ Enforced (was reading from llm.config)
    max_cost_budget: 10.0,   # Dollar limit
    requires_approval: [...], # Skills needing human OK
    timeout_minutes: 30,     # Wall-clock limit
    sandbox_required: False,  # Force sandbox execution
}
```

### Cross-Session Plans

Long-horizon goals persist in the database. ✅ Implemented via `restore_plan_from_events()`:

```sql
-- Resume a long-running plan
SELECT * FROM conversation_events
WHERE event_type IN ('plan_created', 'plan_revised', 'plan_step_done')
  AND metadata->>'goal_id' = @goal_id
ORDER BY created_at DESC LIMIT 1
```

When the user returns, the agent loads the latest plan state and continues.

---

## 4. Multi-Agent Collaboration

### Coordination: Event Blackboard

Agents coordinate through shared events in the database. No hidden channels. No shared memory. No direct message passing.

```
┌──────────────┐     ┌──────────────┐     ┌──────────────┐
│  Code Agent  │     │   CI Agent   │     │  Data Agent  │
└──────┬───────┘     └──────┬───────┘     └──────┬───────┘
       │                    │                    │
       ▼                    ▼                    ▼
┌─────────────────────────────────────────────────────────┐
│              conversation_events (MatrixOne)             │
│  All delegation, results, reflections are events        │
│  Linked by causal_chain_id → full audit trail           │
│  Time-travel queryable → debug any point                │
└─────────────────────────────────────────────────────────┘
```

**Why event blackboard**:
- **Replay**: Re-execute multi-agent workflow by replaying events
- **Audit**: Complete visibility into who said what to whom
- **Fault tolerance**: Agent crashes → last event in DB → another instance picks up
- **Time-travel**: Inspect blackboard state at any point during workflow

### Delegation as a Skill

```python
# Delegation is a tool like any other
delegate_task(
    target_agent="code_agent",
    task="Fix the auth bug in login.py",
    context="CI shows test_auth.py failing with missing DB_HOST",
    wait_for_result=True
)
```

When the orchestrator's LLM decides to delegate, it calls `delegate_task`. The skill executor:
1. Creates a delegation event (same causal chain)
2. Spawns the target agent's ChatLoop
3. Target agent executes, logs events, returns result
4. Result logged as event, returned to orchestrator

### Coordination Patterns

**Fan-out / Fan-in** (parallel):
```
Orchestrator → delegate(code_agent, "Review code")     ─┐
             → delegate(security_agent, "Review security") ├─ parallel
             → delegate(perf_agent, "Review performance")  ─┘
             ← collect all results → synthesize
```

**Pipeline** (sequential):
```
Orchestrator → delegate(ci_agent, "Get failure details")
             ← "Test X fails with error Y"
             → delegate(code_agent, "Fix error Y")
             ← patch
             → delegate(ci_agent, "Run test with patch")
```

**Adversarial Review**:
```
Orchestrator → delegate(code_agent, "Generate fix")
             ← proposed fix
             → delegate(reviewer_agent, "Review this fix")
             ← objections
             → delegate(code_agent, "Revise based on objections")
             → loop until approved or max iterations
```

### Stream Multiplexing

When multiple agents run in parallel, the streaming multiplexer merges their outputs:

```
[code_agent]     Reading file...
[security_agent] Checking dependencies...
[security_agent] ✅ No vulnerabilities found
[code_agent]     Found 2 issues in auth.py
[orchestrator]   Based on the reviews: ...
```

Each `StreamEvent` carries `agent_id` so the UI can render per-agent progress.

### System Agents

Pre-installed agents that maintain platform health. Same ChatLoop, elevated permissions, auto-triggered:

| Agent | Trigger | What It Does |
|-------|---------|-------------|
| **Regression Agent** | skill/prompt change | Replay golden sessions in sandbox, compute quality delta |
| **Audit Agent** | periodic / on-demand | Verify past decisions against current knowledge |
| **Tuning Agent** | quality_score < threshold | Analyze low-scoring interactions, propose prompt improvements |
| **Eval Agent** | new training data batch | Validate dataset quality, detect contamination |

System Agents are defined the same way as User Agents — `AgentProfile` with `system_prompt` + `skill_filter`. The only difference is access to platform-level skills (`create_sandbox`, `replay_session`, `time_travel_query`).

---

## 5. Agent Teams

### The Industry Shift (Feb 2026)

Anthropic's Agent Teams (Opus 4.6) proved that multi-agent parallel coordination is production-ready. Their C compiler experiment: 16 parallel agents, 2000 sessions, 100K lines of code, compiled Linux kernel. The architecture: team lead coordinates, teammates work in independent context windows, shared task board, peer-to-peer messaging.

This is the direction. Not single-agent with tools, but **teams of specialized agents with coordination protocols**.

### Team Architecture

```
┌─────────────────────────────────────────────────────────────┐
│  TEAM DEFINITION                                            │
│                                                             │
│  team_id: "code_quality_team"                               │
│  lead: "orchestrator_agent"                                 │
│  members:                                                   │
│    - {agent_id: "code_agent", role: "implementer"}          │
│    - {agent_id: "review_agent", role: "reviewer"}           │
│    - {agent_id: "security_agent", role: "security"}         │
│    - {agent_id: "test_agent", role: "tester"}               │
│  coordination: "task_board"                                 │
│  max_parallel: 4                                            │
│  budget: {max_cost: 50.0, max_tokens: 1M}                  │
└─────────────────────────────────────────────────────────────┘
```

### Task Board (Event Blackboard via conversation_events)

Teams coordinate through the existing event system — no new tables needed:

```python
# Lead agent creates tasks as events
create_event(type="team_task", content="Review auth.py", 
             metadata={"team_id": "quality_team", "status": "open", "assigned_to": None})

# Member agent claims a task by creating a child event
create_event(type="team_task_claimed", parent_event_id=task_event_id,
             metadata={"claimed_by": "code_agent"})

# Member completes → result event
create_event(type="team_task_done", parent_event_id=task_event_id,
             content="Found 2 issues in auth.py", metadata={"claimed_by": "code_agent"})
```

All task coordination flows through `conversation_events` — same causal chain tracking, same audit trail, same replay capability. No separate task board system needed.

**Why this is better than a separate task board**: Every task claim, every result, every status change is an auditable event. Replay a team workflow = replay the events. Debug a team failure = inspect the causal chain. Time-travel to see task board state at any point.

### Team Execution Flow

```
1. User request arrives → Lead agent decomposes into tasks
2. Tasks written to task_board (each as an event)
3. Member agents poll task_board, claim open tasks (lock)
4. Each member runs in independent ChatLoop + context window
5. Member completes → writes result_summary → unlocks task
6. Lead agent monitors progress, handles dependencies
7. When all tasks done → Lead synthesizes final response
8. All coordination visible in conversation_events
```

### Peer-to-Peer Messaging

Agents can message each other through events:

```python
# Agent A sends message to Agent B
send_message(
    to_agent="review_agent",
    content="I've pushed the fix to auth.py, please re-review",
    causal_chain_id=current_chain
)
# → Creates event_type="agent_message" in conversation_events
# → review_agent picks it up on next context build
```

### Dynamic Team Formation

Not all tasks need pre-defined teams. The lead agent can dynamically recruit:

```python
# Lead agent decides it needs a specialist
recruit_agent(
    role="database_expert",
    reason="Task requires complex SQL optimization",
    from_pool="available_agents",
    selection_criteria="skill_match"
)
```

The platform selects the best-fit agent based on skill overlap, current load, and historical performance on similar tasks.

---

## 6. Multi-Agent Conflict Resolution and Consensus

### The Problem

Agent Teams (§5) describe coordination — who does what. But they don't address what happens when agents **disagree**. A code agent says "refactor this function," a performance agent says "don't touch it, it's hot-path optimized," and a security agent says "rewrite it entirely, it has a vulnerability." Three valid perspectives, three incompatible actions. Without a conflict resolution mechanism, the lead agent either picks arbitrarily or deadlocks.

### Conflict Detection

Conflicts are detected structurally, not heuristically. When multiple agents produce results for related tasks, the framework checks for incompatibility:

```
Agent A result: "Modify function X → version A'"
Agent B result: "Modify function X → version B'"
Agent C result: "Do not modify function X"
  │
  ▼
Conflict detector:
  - Same target artifact (function X) → potential conflict
  - Actions are mutually exclusive (modify vs don't modify) → confirmed conflict
  - Log conflict_detected event with all competing proposals
```

Conflict detection is an event — it enters the causal chain and is auditable.

### Resolution Strategies

The lead agent selects a strategy based on team configuration and conflict type:

```
┌─────────────────────────────────────────────────────────────┐
│  RESOLUTION STRATEGIES                                      │
│                                                             │
│  1. AUTHORITY                                               │
│     Pre-assigned priority: security > correctness > perf    │
│     Highest-priority agent's proposal wins automatically    │
│     Use when: clear domain hierarchy exists                 │
│                                                             │
│  2. EVIDENCE-BASED ARBITRATION                              │
│     Each agent provides evidence (test results, metrics,    │
│     references) alongside its proposal                      │
│     Lead agent (or dedicated arbiter) evaluates evidence    │
│     Use when: proposals can be objectively compared         │
│                                                             │
│  3. SYNTHESIS                                               │
│     Lead agent receives all proposals + reasoning           │
│     Generates a merged solution that satisfies constraints  │
│     from all parties (e.g., rewrite for security BUT       │
│     preserve hot-path optimization)                         │
│     Use when: proposals are partially compatible            │
│                                                             │
│  4. SANDBOX TOURNAMENT                                      │
│     Each proposal executed in its own clone                 │
│     Run evaluation suite against each clone                 │
│     Highest-scoring clone wins                              │
│     Use when: objective quality metric exists               │
│                                                             │
│  5. HUMAN ESCALATION                                        │
│     Conflict + all proposals surfaced to human              │
│     Human decides (via HITL policy from trust-and-safety §9)│
│     Use when: stakes too high for automated resolution      │
└─────────────────────────────────────────────────────────────┘
```

### Consensus Protocol for Critical Decisions

For high-stakes actions (production deployment, data deletion, security-sensitive changes), simple majority isn't enough. The team uses a structured consensus:

```
Lead proposes action
  │
  ▼
All relevant members vote: APPROVE / OBJECT (with reason)
  │
  ├── Unanimous APPROVE → execute
  │
  ├── Any OBJECT with severity=blocking
  │   → Objection + reason injected into lead's context
  │   → Lead must address objection (revise proposal or override with justification)
  │   → Re-vote on revised proposal
  │   → Max 3 rounds, then escalate to human
  │
  └── Non-blocking objections → execute + log objections for post-hoc review
```

Every vote is an event. The full deliberation is replayable.

### Configuration

```python
TeamConfig = {
    "conflict_resolution": {
        "default_strategy": "evidence_based",
        "priority_order": ["security_agent", "code_agent", "perf_agent"],
        "consensus_required_for": ["production_deploy", "data_migration", "access_change"],
        "max_resolution_rounds": 3,
        "escalation_target": "human"  # or "senior_agent"
    }
}
```

### Why This Matters

Without explicit conflict resolution, multi-agent systems degrade to "last writer wins" or "loudest agent wins." Both are invisible failure modes — the system appears to work but silently drops valid perspectives. Making conflict resolution a first-class protocol means disagreements are **visible, auditable, and systematically resolved**.

---

## 7. Intelligent Model Routing

### The Problem

Not every task needs the most expensive model. A simple "what's the CI status?" doesn't need Opus. But a complex multi-file refactor does. Static model assignment wastes money or quality.

### Cost-Quality Router

```
User Request
    │
    ▼
┌─────────────────────────────────────────────────────────────┐
│  MODEL ROUTER                                               │
│                                                             │
│  1. Classify task complexity (rule-based + lightweight LLM) │
│     - simple: keyword lookup, status check → Haiku          │
│     - medium: single-file edit, explanation → Sonnet         │
│     - complex: multi-file refactor, architecture → Opus     │
│     - critical: security review, production deploy → Opus   │
│                                                             │
│  2. Check historical data                                   │
│     - Similar past tasks: which model scored highest?        │
│     - Cost efficiency: quality_score / cost ratio            │
│                                                             │
│  3. Apply constraints                                       │
│     - Budget remaining for this scope                    │
│     - Latency requirements                                  │
│     - Compliance requirements (some tasks require specific   │
│       models for audit reasons)                             │
│                                                             │
│  4. Route decision logged as event (auditable)              │
└─────────────────────────────────────────────────────────────┘
```

### Self-Improving Routing

```sql
-- Learn from historical quality vs cost
SELECT llm_model_used,
  AVG(quality_score) as avg_quality,
  AVG(CAST(metadata->>'$.total_cost' AS DECIMAL)) as avg_cost,
  AVG(quality_score) / AVG(CAST(metadata->>'$.total_cost' AS DECIMAL)) as efficiency
FROM conversation_events
WHERE event_type = 'llm_response'
  AND metadata->>'$.task_type' = @current_task_type
GROUP BY llm_model_used
ORDER BY efficiency DESC;
```

The router learns: "for code_review tasks, Sonnet achieves 4.2/5 quality at $0.003/task while Opus achieves 4.5/5 at $0.03/task — 10x cost for 7% quality gain." Route to Sonnet unless the user explicitly requests premium quality.

### Per-Agent Model Override

Teams can mix models:

```python
TeamConfig = {
    "lead": {"model": "opus", "reason": "coordination requires deep reasoning"},
    "implementer": {"model": "sonnet", "reason": "code generation, good enough"},
    "tester": {"model": "haiku", "reason": "test execution, simple tasks"},
}
```

---

## 8. Sub-Agent Architecture for Long-Horizon Tasks

Following Anthropic's multi-agent research system pattern:

```
Lead Agent (coordinator, high-level plan)
  │
  ├── Sub-agent A (deep technical exploration, 50K tokens)
  │   └── Returns: condensed summary (1-2K tokens)
  │
  ├── Sub-agent B (parallel research, 30K tokens)
  │   └── Returns: condensed summary (1-2K tokens)
  │
  └── Lead Agent synthesizes summaries → final response
```

**Key insight**: Each sub-agent can explore extensively with a clean context window. The lead agent never sees the raw exploration — only distilled results. This achieves separation of concerns and prevents context pollution.

This composes naturally with PAOR planning: a plan step can delegate to a sub-agent. A sub-agent can itself plan. Depth is bounded by `PlanConstraints.max_steps` at each level.

---

## 9. Agent Scheduling and Resource Management

### The Problem

Multi-agent parallelism (§5) creates resource contention: N agents competing for LLM API rate limits, database connections, and compute budget. Without scheduling, you get thundering herds, budget overruns, and priority inversions (a low-priority background task starves a user-facing request).

### Scheduling Model

```
Incoming agent tasks
  │
  ▼
┌─────────────────────────────────────────────────────────┐
│  SCHEDULER                                              │
│                                                         │
│  Priority Queues (preemptive):                          │
│    P0: User-facing interactive (< 2s latency target)    │
│    P1: User-initiated background (< 30s)                │
│    P2: System-initiated (evaluation, training, cleanup) │
│    P3: Speculative (parallel exploration, pre-warming)  │
│                                                         │
│  Resource Pools:                                        │
│    LLM tokens/min: allocated per priority tier          │
│    DB connections: bounded pool per scope                │
│    Clone slots: max concurrent clones per scope          │
│                                                         │
│  Admission Control:                                     │
│    - Estimate cost BEFORE scheduling (from history)     │
│    - Reject if budget remaining < estimated cost        │
│    - Reject if resource pool exhausted → queue or shed  │
└─────────────────────────────────────────────────────────┘
```

### Cost Convergence

The scheduler doesn't just limit spend — it **converges** toward a budget target:

```python
@dataclass
class BudgetPolicy:
    scope_id: str          # user, team, or account — deployment determines granularity
    daily_budget: float
    current_spend: float
    remaining_hours: float

    @property
    def burn_rate_target(self) -> float:
        """Target $/hour to stay within budget."""
        remaining = self.daily_budget - self.current_spend
        return remaining / max(self.remaining_hours, 1)

    def should_downgrade_model(self, estimated_cost: float) -> bool:
        """Switch to cheaper model if burn rate exceeds target."""
        return estimated_cost > self.burn_rate_target * 0.5
```

When burn rate exceeds target: automatically downgrade non-critical tasks to cheaper models. When under budget: allow quality upgrades. The system self-balances.

### Load Shedding

When all resource pools are saturated:

| Priority | Behavior |
|---|---|
| P0 (interactive) | Never shed. Preempt P2/P3 tasks if needed. |
| P1 (background) | Queue with timeout. Notify user if delayed. |
| P2 (system) | Defer to off-peak. Batch where possible. |
| P3 (speculative) | Shed immediately. These are optional by definition. |

For the full data flow architecture under multi-agent load — write batching, read path optimization, HTAP separation, and backpressure mechanisms — see [ARCHITECTURE.md § Data Flow Architecture](ARCHITECTURE.md#data-flow-architecture-throughput-under-multi-agent-load).

---

## 10. Cross-Model Consistency and Provider Resilience

### The Problem

The model router (§7) routes tasks to different models. But models disagree: Opus and Sonnet may produce structurally different outputs for the same prompt. And providers go down: if OpenAI is unavailable, can we failover to Anthropic without breaking the session? More fundamentally, even the **same model** is non-deterministic — the same input can produce different outputs across calls.

### Non-Determinism Budget

Not all non-determinism is equal. The framework assigns a **tolerance class** to each task type:

| Tolerance Class | Acceptable Variation | Example Tasks | Enforcement |
|---|---|---|---|
| **Strict** | Structural identity (same tool calls, same schema) | Production deploys, data migrations, financial calculations | Retry up to 3x if output structure differs from expected |
| **Semantic** | Same conclusion, different wording allowed | Code review, Q&A, explanations | Verify key assertions match via lightweight judge |
| **Relaxed** | Different approaches acceptable if quality maintained | Brainstorming, exploration, creative writing | Quality score check only |

Configuration per skill:

```python
class SkillConsistencyPolicy:
    tolerance: Literal["strict", "semantic", "relaxed"]
    verification_model: str | None  # cheaper model for consistency checks; None = skip
    max_retries: int = 2            # retries on consistency failure
    reference_output: str | None    # golden output for strict comparison (optional)
```

### Consistency Verification Mechanism

```
Agent produces output with Model A
  │
  ▼
Step 1: STRUCTURAL CHECK (fast, no LLM call)
  - Does output match expected schema? (tool_call format, JSON structure)
  - Does output contain required fields?
  - Are tool call parameters within declared bounds?
  │
  ├── Fail → retry with same model (malformed output, transient)
  │
  ▼
Step 2: SEMANTIC CHECK (only for strict/semantic tolerance)
  - Lightweight judge model compares output against:
    a) Prior turns in this session (contradiction detection)
    b) Reference output if available (semantic equivalence)
    c) Known invariants for this task type
  │
  ├── Contradiction detected → log consistency_violation event
  │   → If mid-session model switch caused it: mark model pair as incompatible for this task type
  │   → Feed into router: "Model B is not a safe fallback for task type X"
  │
  ▼
Step 3: CROSS-MODEL EQUIVALENCE TEST (offline, batch)
  - Periodically replay golden sessions across all routable models
  - Build compatibility matrix:

    | Task Type      | Opus→Sonnet | Opus→GPT-4 | Sonnet→Haiku |
    |----------------|-------------|------------|--------------|
    | code_review    | 94% compat  | 87% compat | 72% compat   |
    | deploy         | 99% compat  | 91% compat | N/A (blocked)|
    | explanation    | 98% compat  | 96% compat | 93% compat   |

  - Router uses this matrix for failover decisions
  - Matrix auto-updates as new replay data accumulates
```

### Provider Failover

```
Primary provider unavailable
  │
  ▼
Failover decision (informed by compatibility matrix):
  │
  ├── Check compatibility matrix for current task type
  │   ├── Compatible fallback exists (>90%) → failover immediately
  │   ├── Marginal compatibility (70-90%) → failover + run consistency check on first response
  │   └── Low compatibility (<70%) → queue and wait for primary (with timeout)
  │
  ├── Mid-session: prefer same-family fallback (Opus → Sonnet, not Opus → GPT-4)
  │   to minimize behavioral discontinuity
  │
  └── Post-failover:
      - Log provider_failover event with original_model, fallback_model, compatibility_score
      - If session continues on fallback: flag for post-hoc review
      - When primary recovers: optionally switch back (configurable)
```

### Replay Consistency Across Models

A session recorded with Model A must be replayable even if Model A is no longer available. The replay system handles this:

```
Replay session (originally Model A, now using Model B)
  │
  ▼
For each LLM call in the session:
  - Input: identical (from context snapshot)
  - Output: Model B's response (will differ from original)
  │
  ▼
Comparison (using task's tolerance class):
  - Strict: same tool calls, same schema? → PASS/FAIL
  - Semantic: same conclusion, different wording? → PASS/FAIL
  - Relaxed: quality score within range? → PASS/FAIL
  │
  ▼
Report: "Session replay with Model B: 47/50 steps consistent, 3 flagged"
  → Flagged steps feed back into compatibility matrix
  → Every replay automatically improves cross-model knowledge
```

---

## References

- [Anthropic: Building a C Compiler with Agent Teams](https://www.anthropic.com/engineering/building-c-compiler)
- [Anthropic: Building Effective AI Agents](https://www.anthropic.com/research/building-effective-agents)
- [Anthropic: How We Built Our Multi-Agent Research System](https://www.anthropic.com/engineering/multi-agent-research-system)
- [Anthropic: Effective Context Engineering](https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents)
- [RouteMoA: Dynamic Routing for Mixture-of-Agents](https://huggingface.co/papers/2601.18130)

Content was rephrased for compliance with licensing restrictions.

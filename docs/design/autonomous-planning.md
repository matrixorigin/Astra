# Autonomous Planning

**Status**: Design  
**Created**: 2026-02-11  
**Phase**: 5

---

## The Problem

Current agent behavior is reactive: user says something → agent selects a skill → executes → responds. This breaks down for complex tasks:

1. **Multi-step tasks with dependencies** — "Set up a new microservice with CI/CD, database migration, and monitoring." This requires 10+ steps in a specific order. The user shouldn't have to guide each step manually.

2. **Tasks that require adaptation** — "Fix the failing CI pipeline." The agent doesn't know upfront whether it's a test failure, a config issue, or a dependency problem. It needs to investigate, form a hypothesis, act, observe results, and revise the plan.

3. **Long-horizon goals** — "Reduce test flakiness by 50% over the next sprint." This requires sustained effort across multiple sessions: identify flaky tests, analyze patterns, propose fixes, verify improvements. No single conversation covers this.

4. **Recovery from failure** — Agent tries approach A, it fails. Without planning, it either gives up or retries the same thing. With planning, it can backtrack and try approach B.

The industry is moving from "chatbot that executes commands" to "teammate that takes ownership of goals." Planning is the missing capability.

---

## Design Principles

1. **Plans are data, not code** — Plans are stored in the database as structured events. They are queryable, versionable, and replayable. A plan is not a hardcoded workflow — it's a data artifact the LLM generates and the system executes.

2. **Plan-Act-Observe-Reflect loop** — Inspired by ReAct but extended with explicit reflection. After each action, the agent observes the result and decides: continue plan, revise plan, or escalate. This is the core execution loop.

3. **Hierarchical decomposition** — Complex goals decompose into sub-goals, which decompose into tasks, which decompose into skill calls. Each level is a plan node. This mirrors how humans tackle complex work.

4. **Plans are auditable** — Every plan, revision, and execution step is an event in the causal chain. You can time-travel to see "what was the plan at step 3?" and "why did the agent revise it at step 5?"

---

## Architecture

### Plan-Act-Observe-Reflect (PAOR) Loop

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
    │ plan    │     │ (skill) │     │ expected │   │
    └─────────┘     └─────────┘     └────┬─────┘   │
                                         │         │
                                         ▼         │
                                    ┌──────────┐   │
                                    │ REFLECT  │   │
                                    │          │   │
                                    │ Continue?│───┘
                                    │ Revise?  │
                                    │ Done?    │──────▶ Final Response
                                    │ Escalate?│──────▶ Human
                                    └──────────┘
```

This is not ReAct (which is Act-Observe only). The explicit **Plan** phase generates a structured plan before acting. The explicit **Reflect** phase evaluates progress against the plan, not just the last action.

### Plan Structure

```python
@dataclass
class PlanNode:
    node_id: str
    goal: str                          # What this node aims to achieve
    strategy: str                      # How (high-level approach)
    steps: list[PlanStep]              # Ordered steps to execute
    status: Literal["pending", "active", "completed", "failed", "revised"]
    parent_node_id: str | None         # For hierarchical decomposition
    revision_of: str | None            # If this revises a previous plan
    created_at: datetime
    
@dataclass
class PlanStep:
    step_id: str
    description: str                   # Human-readable description
    skill_id: str | None               # Which skill to call (None = sub-plan)
    params: dict | None                # Skill parameters
    depends_on: list[str]              # Step IDs this depends on
    expected_outcome: str              # What success looks like
    status: Literal["pending", "active", "completed", "failed", "skipped"]
    actual_outcome: str | None         # What actually happened
    reflection: str | None             # Agent's assessment after execution
```

### Hierarchical Decomposition

```
Goal: "Set up microservice with CI/CD"
├── Sub-goal: "Create service scaffold"
│   ├── Step: create_repo(name="user-service")
│   ├── Step: generate_code(template="fastapi")
│   └── Step: commit_and_push()
├── Sub-goal: "Set up CI/CD"
│   ├── Step: create_github_actions(tests=True, deploy=True)
│   ├── Step: configure_secrets(env="staging")
│   └── Step: trigger_first_build()
├── Sub-goal: "Database migration"
│   ├── Step: create_migration(schema="users")
│   └── Step: run_migration(env="staging")
└── Sub-goal: "Monitoring"
    ├── Step: add_health_endpoint()
    └── Step: configure_alerts(error_rate=">5%")
```

Each sub-goal can be delegated to a specialist agent (connects to multi-agent collaboration). Each step maps to a skill call.

### Reflection and Adaptation

After each step, the agent reflects:

```python
reflection_prompt = """
Plan: {plan_summary}
Just completed: Step {step_id} - {step_description}
Expected outcome: {expected_outcome}
Actual outcome: {actual_outcome}

Assess:
1. Did this step succeed? (yes/partial/no)
2. Does the remaining plan still make sense? (yes/needs_revision)
3. Are there new risks or blockers? (describe)
4. Next action: (continue/revise_plan/escalate/done)
"""
```

This is where the agent catches problems early. If step 3 reveals that the CI config is wrong, the agent revises the plan instead of blindly continuing.

**Key innovation: Reflection against versioned state.** The agent's reflection includes not just the action result, but the current data state. Using MatrixOne's snapshot, we can capture the exact state at each reflection point. If the plan goes wrong, we can time-travel to any reflection point and understand exactly what the agent knew when it decided to continue or revise.

### Plan Versioning

Plans evolve. The original plan might be revised 3 times during execution. Each revision is a new `PlanNode` with `revision_of` pointing to the previous version.

```
Plan v1: [A → B → C → D]
          ↓ (B fails)
Plan v2: [A → B' → C → D]    revision_of = v1
          ↓ (C reveals new requirement)
Plan v3: [A → B' → C → E → D]  revision_of = v2
```

All versions are stored as events. You can query the full plan evolution:

```sql
-- Full plan history for a goal
SELECT * FROM conversation_events
WHERE causal_chain_id = @chain_id
  AND event_type = 'plan_created' OR event_type = 'plan_revised'
ORDER BY created_at
```

**Why MatrixOne matters**: Plan revisions combined with data snapshots mean you can answer: "At plan v2, what data did the agent see that made it revise step B?" This is time-travel debugging for planning.

### Cross-Session Planning

Long-horizon goals span multiple sessions. The plan persists in the database:

```sql
-- Resume a long-running plan
SELECT * FROM conversation_events
WHERE event_type IN ('plan_created', 'plan_revised', 'plan_step_completed')
  AND metadata->>'goal_id' = @goal_id
ORDER BY created_at DESC
LIMIT 1  -- Get latest plan state
```

When the user returns: "How's the test flakiness project going?", the agent:
1. Loads the latest plan state from events
2. Checks which steps are completed
3. Resumes from the next pending step
4. Or reflects on overall progress and revises if needed

### Safety: Plan Boundaries

Plans have guardrails:

```python
@dataclass
class PlanConstraints:
    max_steps: int = 20                # No runaway plans
    max_revisions: int = 5             # Don't revise forever
    max_cost_budget: float = 10.0      # Dollar limit for LLM calls
    requires_approval: list[str] = []  # Skills that need human OK
    timeout_minutes: int = 30          # Wall-clock limit
    sandbox_required: bool = False     # Force execution in sandbox
```

When `sandbox_required = True`, the entire plan executes in a MatrixOne zero-copy branch. If the plan fails or produces bad results, drop the branch. No production impact.

---

## Integration with Existing Systems

### Plans as Events

Plans use the existing event system — no new tables:

```
event_type = "plan_created"     → content = JSON plan structure
event_type = "plan_revised"     → content = revised plan, metadata.revision_of = previous
event_type = "plan_step_start"  → content = step description
event_type = "plan_step_done"   → content = outcome + reflection
event_type = "plan_completed"   → content = final summary
event_type = "plan_failed"      → content = failure reason + what was tried
```

All linked by `causal_chain_id`. All replayable. All auditable.

### Plans + Multi-Agent

The orchestrator agent can generate a plan and delegate sub-goals to specialist agents:

```
Orchestrator generates plan:
  Sub-goal 1 → delegate to code_agent
  Sub-goal 2 → delegate to ci_agent
  Sub-goal 3 → delegate to data_agent (parallel with sub-goal 2)
  
Orchestrator monitors progress via events.
Orchestrator reflects after each sub-goal completes.
Orchestrator revises plan if needed.
```

### Plans + Regression Gate

Before executing a plan in production, optionally run it in a sandbox first:

```
1. Create zero-copy branch of production data
2. Execute full plan in branch
3. Inspect results
4. If good → execute in production
5. If bad → revise plan, try again in new branch
```

This is "dry-run for plans" — only possible with zero-cost branching.

---

## Data Model

No new tables. Plans are events:

```sql
-- Plan events use conversation_events with structured metadata:
-- event_type: 'plan_created' | 'plan_revised' | 'plan_step_start' | 
--             'plan_step_done' | 'plan_completed' | 'plan_failed'
-- content: JSON plan/step structure
-- metadata: {
--   "goal_id": "...",
--   "plan_version": 1,
--   "revision_of": null | "previous_event_id",
--   "step_index": 3,
--   "constraints": {...}
-- }
-- causal_chain_id: links all events in the plan
-- parent_event_id: links step events to plan event
```

---

## Implementation Priority

**P0**: PAOR loop for single-session, single-agent plans (the core loop)
**P1**: Hierarchical decomposition (sub-goals)
**P2**: Cross-session plan persistence and resumption
**P3**: Plan dry-run in sandbox branches
**P4**: Integration with multi-agent delegation

---

## What This Is NOT

- **Not a workflow engine.** Plans are generated by the LLM, not defined by developers. There's no YAML workflow definition. The agent decides the plan based on the goal and context.
- **Not unbounded autonomy.** Plans have hard limits (max steps, cost budget, timeout). The agent operates within constraints, not with unlimited freedom.
- **Not speculative execution.** The agent doesn't explore multiple plans in parallel (that's MCTS/Tree-of-Thought). It commits to one plan, reflects, and revises. This is simpler, cheaper, and sufficient for engineering tasks.

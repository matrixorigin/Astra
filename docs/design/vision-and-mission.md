# Vision and Mission

## The Problems We Solve

AI agents are entering production across engineering, data, and operations teams. But as they move from demos to real workloads, five problems consistently block adoption:

### 1. Agent Decisions Are Black Boxes

An agent recommends a code fix, triages a bug, or merges a PR. Three months later, someone asks: "Why did the agent do that?" No one can answer. The data the agent saw has changed. The prompt has been updated. The context window is gone. There is no way to reconstruct the decision.

This is a dealbreaker for regulated industries (finance, healthcare, legal) and any team that needs to trust agent outputs.

### 2. Prompt and Skill Iteration Is Guesswork

Teams change prompts and deploy. Did it break existing cases? No one knows until users complain. There's no regression testing for prompt changes — no way to replay past conversations against a new prompt and measure quality delta before shipping.

The result: teams either iterate too slowly (afraid to break things) or too recklessly (ship and pray).

### 3. Knowledge Updates Silently Invalidate Past Answers

RAG systems retrieve from a knowledge base. The knowledge base gets updated. But answers generated from the old version are still cached, still referenced, still trusted. No mechanism exists to detect which past outputs are now inconsistent with current knowledge.

This is the "silent rot" problem — the system degrades without anyone noticing.

### 4. Experimentation on Real Data Is Prohibitively Expensive

To test a new agent strategy on production data, you need to copy the database. For large datasets, this takes hours and significant storage. Most teams skip it and test on toy data instead, then discover problems in production.

### 5. Training Data Lineage Is Missing

Teams extract fine-tuning datasets from agent interactions. But they can't answer: "Was the source data correct when this training example was generated?" or "Does my test set overlap with training data from three versions ago?" Data quality is unverifiable.

---

## Vision

**An intelligent agent platform where decisions are reproducible, iterations are safe, and data quality is provable.**

We build agents that engineering and data teams can actually trust in production — not because the LLM is perfect, but because every decision is traceable, every change is testable, and every data dependency is versioned.

```
Agent Decision = f(prompt@version, skill@version, context@snapshot, memory@state, llm_params)

Control the inputs → constrain the non-determinism → audit the outputs.
```

## Mission

Build an agentic platform that solves the five adoption blockers:

| Problem | Solution | How |
|---|---|---|
| Decisions are black boxes | **Decision Audit Trail** | Every decision binds to a data snapshot; reconstruct the exact input state at any future time |
| Prompt iteration is guesswork | **Regression Gate** | Replay past sessions against new prompts in isolated environments; merge only when quality improves |
| Knowledge rot is invisible | **Knowledge Regression Detection** | When knowledge changes, automatically identify and re-evaluate affected past outputs |
| Experimentation is expensive | **Zero-Cost Branching** | Create full production data copies in milliseconds with zero storage overhead |
| Training data lineage is missing | **Versioned Data Pipeline** | Every training example traces back to its source interaction and the data state at that time |

### Why This Requires a Different Data Layer

These solutions share a common requirement: **the ability to query historical data states, branch data cheaply, and track causal relationships natively**. This is not achievable by bolting features onto a traditional database:

- Time-travel queries require storage engine support (not log replay)
- Zero-copy branching requires copy-on-write at the storage layer
- Causal event chains require HTAP (transactional writes + analytical queries on the same data)

MatrixOne provides all three natively through its Git for Data capabilities. This is why we build on MatrixOne — not as a branding exercise, but because the problems we solve are architecturally impossible on a Postgres + Pinecone + S3 stack.

---

## Core Architecture

### The Agent OS Model

mo-agent-engine is not an agent framework — it is an **Agent Operating System**. The distinction matters:

- A **framework** provides libraries for building agents (LangChain, CrewAI)
- An **OS** provides infrastructure that makes all agents on it inherently more capable

Every agent running on this OS automatically gets: auditable decisions, safe experimentation, time-travel debugging, and cost control. These aren't features the agent developer implements — they're platform guarantees.

### Three-Layer Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                      USER AGENTS (Apps)                         │
│  Code Review Agent · CI Diagnosis Agent · Data Analysis Agent   │
│  ─ User-facing functionality                                    │
│  ─ Defined by: system_prompt + skill_set + model                │
│  ─ Developers build these                                       │
├─────────────────────────────────────────────────────────────────┤
│                    SYSTEM AGENTS (Daemons)                       │
│  Regression Agent · Audit Agent · Tuning Agent · Eval Agent     │
│  ─ Platform health & quality maintenance                        │
│  ─ Pre-installed, run automatically on triggers                 │
│  ─ Same execution model as User Agents (ChatLoop + Skills)      │
│  ─ Have elevated permissions (access Time Travel, Sandbox APIs) │
├─────────────────────────────────────────────────────────────────┤
│                  PLATFORM CAPABILITIES (Kernel)                  │
│  Event Bus · Context/Memory · Sandbox · Time Travel · LLM       │
│  Client · Streaming · Skill Registry · Planning Engine          │
│  ─ NOT agents — these are APIs that agents call                 │
│  ─ Powered by MatrixOne (intentional tight coupling)            │
│  ─ Interface extraction deferred until multi-tenant deployment  │
└─────────────────────────────────────────────────────────────────┘
                              │
                    ┌─────────┴─────────┐
                    │    MatrixOne      │
                    │  (Data Layer)     │
                    │  Time Travel ·    │
                    │  Zero-Copy Branch │
                    │  HTAP · RBAC      │
                    └───────────────────┘
```

**Why this layering matters**:
- "Hallucination Firewall" is a **Platform Capability** (verification API), not an agent. But "Audit Agent" is a **System Agent** that calls the firewall API to audit past decisions.
- "Regression Gate" is a **Platform Capability** (sandbox + replay API). But "Regression Agent" is a **System Agent** that triggers the gate on every skill/prompt change.
- "Code Review" is a **User Agent** — it uses Platform Capabilities (context, memory, streaming) but doesn't know about sandbox or time-travel.

This separation means: adding a new User Agent requires zero platform code. You define a system prompt, pick skills, and register it.

### Unified Execution Model: How Agents Connect

Every agent — user or system — runs the same execution loop. The question is how they compose:

```
User Request
    │
    ▼
┌──────────────────────────────────────────────────────────────┐
│  Orchestrator Agent (ChatLoop)                               │
│                                                              │
│  1. Classify: simple task or complex goal?                   │
│     ├─ Simple → direct skill execution (single ChatLoop)     │
│     └─ Complex → enter PAOR loop ↓                           │
│                                                              │
│  2. PLAN: generate structured plan                           │
│     └─ Each step is either:                                  │
│        ├─ Skill call (execute directly)                      │
│        └─ Delegation (spawn child agent) ──────────────┐     │
│                                                        │     │
│  3. ACT: execute next step(s)                          │     │
│     ├─ Direct steps: call skill via AgentExecutor      │     │
│     └─ Delegated steps: spawn child ChatLoop(s) ───────┤     │
│                                                        │     │
│  4. OBSERVE: check results (from events)               │     │
│                                                        │     │
│  5. REFLECT: continue / revise / escalate / done       │     │
│     └─ If revise → back to PLAN                        │     │
│                                                        │     │
│  6. SYNTHESIZE: combine all results → final response   │     │
└──────────────────────────────────────────────────────────┘
                         │                           │
              ┌──────────┘                           │
              ▼                                      ▼
    ┌──────────────────┐                ┌──────────────────┐
    │  Code Agent       │                │  CI Agent         │
    │  (child ChatLoop) │                │  (child ChatLoop) │
    │  own skills,      │                │  own skills,      │
    │  own streaming,   │                │  own streaming,   │
    │  own sandbox      │                │  own sandbox      │
    └────────┬─────────┘                └────────┬─────────┘
             │                                   │
             ▼                                   ▼
    ┌─────────────────────────────────────────────────────┐
    │          conversation_events (Event Blackboard)      │
    │  All delegation, results, reflections are events     │
    │  Linked by causal_chain_id → full audit trail        │
    │  Time-travel queryable → debug any point             │
    └─────────────────────────────────────────────────────┘
             │
             ▼
    ┌─────────────────────────────────────────────────────┐
    │          Streaming Multiplexer                        │
    │  Merges all agent streams → single output to user    │
    │  Each chunk tagged with agent_id                     │
    │  User sees parallel progress in real-time            │
    └─────────────────────────────────────────────────────┘
```

**Key design decisions**:

1. **Delegation = spawning a child ChatLoop**, not a function call. The child agent has its own system prompt, skills, model, and streaming. It's a full agent, not a subroutine.

2. **The Event Blackboard is the only coordination mechanism.** Parent and child agents never share memory. The orchestrator reads child results from `conversation_events`. This makes the entire workflow replayable.

3. **Streaming is multiplexed, not sequential.** When 3 agents run in parallel, the user sees all 3 progressing. Each `StreamEvent` carries `agent_id` so the UI can render per-agent progress.

4. **Planning and multi-agent are orthogonal but composable.** A plan step can delegate to an agent. An agent can itself plan. This is recursive — an orchestrator plans, delegates to a code agent, which plans its own sub-steps. Depth is bounded by `PlanConstraints`.

5. **System Agents use the same execution model.** The Regression Agent is just a ChatLoop with `system_prompt="You are a regression testing agent..."`, skills `[replay_session, create_sandbox, compute_quality_delta]`, and a trigger (skill/prompt change event). No special runtime.

### Deterministic Boundary Control

LLM outputs are inherently non-deterministic. But agent *decisions* don't have to be black boxes. If we version-control every input to the decision:

- **Prompt** → versioned in `prompt_templates` table
- **Skill** → versioned with semantic versioning
- **Context snapshot** → bound to a database snapshot timestamp
- **Memory state** → queryable at any historical point
- **LLM parameters** → recorded per invocation

Then for any past decision, we can reconstruct: "Given these exact inputs, the LLM produced this output." The LLM itself is the only uncontrolled variable — and that's a much smaller audit surface than "we have no idea what happened."

### Event-Centric Design

Every interaction flows through `conversation_events` with causal chain tracking:

```
user_query → skill_selection → skill_execution → llm_response
     ↑              ↑                ↑                ↑
  event_id    parent_event_id   causal_chain_id   snapshot_ts
```

This enables:
- **Replay**: Re-execute any conversation with original or modified inputs
- **Lineage**: Trace any output back to its data origins
- **Audit**: Complete provenance for every agent action

In multi-agent workflows, the causal chain extends across agents:
```
user_query → orchestrator_plan → delegation(code_agent) → code_agent_tool_call → code_agent_result → orchestrator_synthesis
     ↑              ↑                    ↑                        ↑                      ↑                    ↑
  chain_001     chain_001            chain_001                chain_001              chain_001            chain_001
```

One `causal_chain_id` links the entire workflow. Time-travel to any point.

### Skills as Versioned Capabilities

Skills are not functions — they are **versioned, declarative capabilities** with:
- Declared requirements (repo type, permissions, parameters)
- Framework-enforced safety (permissions checked before execution)
- Full lifecycle management (register, version, deprecate)
- Side-effect isolation (mock mode for replay, sandbox for testing)

### Three-Layer Context Model

```
Memory (infinite, persistent) → Selection → Prompt (finite, curated) → LLM → Context (active, ephemeral)
```

The intelligence is in selection: choosing what to show the LLM from potentially years of accumulated data, within a fixed token budget.

### MatrixOne: Intentional Tight Coupling

MatrixOne is not a pluggable database choice — it is the architectural foundation. The core value propositions (time-travel, zero-copy branching, snapshot-consistent verification) are **architecturally impossible** on Postgres + Pinecone + S3.

Current stance:
- **Code directly calls MatrixOne SQL** — no abstraction layer
- **Interface extraction deferred** until multi-tenant deployment requires separating "our MO" from "user's MO"
- **This is a strategic advantage**, not technical debt — the tight coupling enables capabilities no other agent platform can offer

---

## Platform Capabilities (Kernel)

These are APIs available to all agents. They are NOT agents themselves.

| Capability | What It Provides | Powered By |
|---|---|---|
| **Event Bus** | Atomic event logging, causal chains, cross-session queries | `conversation_events` table |
| **Context/Memory** | Three-layer selection (Memory→Prompt→Context) | MatrixOne + embedding refs |
| **Sandbox** | Zero-copy isolated environments for experimentation | MatrixOne CLONE/SNAPSHOT |
| **Time Travel** | Query any historical data state | MatrixOne `{SNAPSHOT = ...}` |
| **LLM Client** | Multi-provider routing, circuit breaker, budget control | OpenAI/Groq/Anthropic adapters |
| **Streaming** | AG-UI protocol event stream, transport-agnostic | SSE/WebSocket/stdout |
| **Skill Registry** | Versioned skill management, side-effect profiles | `skills_registry` table |
| **Planning Engine** | PAOR loop, hierarchical decomposition, plan versioning | Planner + ChatLoop |
| **Hallucination Firewall** | Snapshot-consistent claim verification | Time Travel + LLM |
| **Regression Gate** | Automated quality gate for changes | Sandbox + Replay |
| **Cost Control** | Pre-call estimation, budget enforcement | LLM Client + `llm_call_logs` |

## System Agents (Daemons)

Pre-installed agents that maintain platform health. Same execution model as user agents (ChatLoop + Skills), but with elevated permissions and automatic triggers.

| Agent | Trigger | What It Does | Platform APIs Used |
|---|---|---|---|
| **Regression Agent** | Skill/prompt change event | Replay golden sessions in sandbox, compute quality delta, pass/fail gate | Sandbox, Replay, Regression Gate |
| **Audit Agent** | Periodic / on-demand | Verify past decisions against current knowledge, flag inconsistencies | Time Travel, Hallucination Firewall |
| **Tuning Agent** | Quality score threshold | Analyze low-scoring interactions, propose prompt improvements | Memory, Context, Prompt Evolution |
| **Eval Agent** | New training data batch | Validate dataset quality, detect contamination, compute metrics | Time Travel, Training Pipeline |

System Agents are defined the same way as User Agents — `AgentProfile` with `system_prompt` + `skill_filter`. The only difference is they have access to platform-level skills (e.g., `create_sandbox`, `replay_session`) that User Agents don't.

## User Agents (Apps)

User-facing agents that solve domain problems. They use Platform Capabilities transparently — every decision is automatically auditable, every interaction is replayable, without the agent developer doing anything special.

Examples:
- **Code Review Agent**: skills = `[code_read, code_diff, code_comment]`
- **CI Diagnosis Agent**: skills = `[ci_get_logs, ci_trigger, code_search]`
- **Data Analysis Agent**: skills = `[sql_query, chart_generate, data_export]`
- **Security Audit Agent**: skills = `[dep_scan, code_search, cve_lookup]`

Adding a new User Agent = define `AgentProfile` (system_prompt + skills + model). Zero platform code changes.

---

## Innovation Layer

These are Platform Capabilities that go beyond standard agent frameworks. Each addresses a real production need:

### Hallucination Firewall

**Problem**: LLM generates claims that contradict the data it was given.
**Solution**: Extract verifiable claims from responses, verify against the same data snapshot the LLM saw, block delivery if contradictions found.
**Why it needs data versioning**: Verification must use the *same* data state as generation. If data changed between generation and verification, you get false positives/negatives.

### Regression Gate (Sandbox-as-CI)

**Problem**: Prompt/skill changes might break existing good behavior.
**Solution**: Before any change is deployed, automatically replay golden sessions in a snapshot-isolated environment. Compute quality delta. Reject if regression exceeds threshold.
**Why it needs zero-copy branching**: Running regression tests on full production data must be instant and free, otherwise teams won't do it.

### Knowledge Regression Detection

**Problem**: Knowledge base updates silently invalidate past answers.
**Solution**: When knowledge changes, identify past decisions that depended on the old version. Re-evaluate in a branch with updated knowledge. Flag regressions.
**Why it needs time-travel + branching**: You need to query "what did the agent see then" (time-travel) and "what would it see now" (branch), then compare.

### Prompt Evolution Pipeline

**Problem**: Prompt engineering is trial-and-error with no scientific method.
**Solution**: Every prompt change creates a data branch. Run experiments in isolation. Measure quality. Merge only when improvement is statistically significant.
**Why it needs branching**: Each experiment needs full production data without interfering with production or other experiments.

### Training Data Pipeline

**Problem**: Fine-tuning data quality is unverifiable.
**Solution**: Build datasets from high-quality events. Every dataset is a named snapshot. Trace each example to its source. Detect contamination across versions.
**Why it needs snapshots + lineage**: Reproducible training requires exact dataset versioning. Contamination detection requires cross-version lineage queries.

### Cost-Aware Execution

**Problem**: LLM costs are unpredictable and can spike.
**Solution**: Query historical cost data to predict execution cost before spending. Block or suggest alternatives when budget would be exceeded.
**Why it needs historical queries**: Accurate cost prediction requires querying actual historical cost patterns, not estimates.

---

## Capabilities by Audience

### For Engineering Teams
- Intelligent agent with multi-turn tool use and conversation memory
- Monitor, summarize, and triage across multiple repositories
- CI/CD failure diagnosis, regression detection, benchmark tracking
- Modular skill system — extend with custom skills

### For Data Engineers
- Versioned prompts, skills, knowledge, and training data — all as data, not files
- Dataset lineage from training example back to source interaction
- Contamination detection across dataset versions
- Reproducible training pipelines via exact snapshot references

### For Platform Teams
- Zero-overhead experimentation on production data
- Automated regression gates for every prompt/skill change
- Cost prediction and budget enforcement
- Multi-tenant isolation with shared skill libraries

### For Compliance & Audit
- Decision reconstruction via data snapshot time-travel
- Immutable event trail with causal chains
- Snapshot-scoped access control
- Full provenance for every agent output

---

## Evolution Roadmap

### Phase 1: Foundation ✅ (Current)
Event system, session management, skill framework, basic sandbox, side-effect isolation.

### Phase 2: Decision Trust
- Decision audit trail (snapshot_ts binding on every decision event)
- Hallucination firewall (snapshot-consistent claim verification)
- Multi-turn tool use with full message chain preservation

### Phase 3: Safe Iteration
- Regression gate (zero-copy branch → replay golden sessions → quality delta → merge/reject)
- Prompt evolution pipeline (branch-based A/B testing with quality gates)
- Knowledge regression detection

### Phase 4: Real-Time Experience ✅ (Streaming Implemented)
- ✅ Streaming output (AG-UI protocol, structured event stream)
- ✅ Multi-turn tool call streaming with accumulation
- User intervention mid-execution (cancel, redirect, approve/reject gates)

### Phase 5: Autonomous Agents ✅ (Planning + Multi-Agent Skeleton Implemented)
- ✅ PAOR loop for single-session planning
- ✅ Agent registry and profile system
- Multi-agent orchestration (delegation-as-skill, fan-out/fan-in)
- Stream multiplexing across parallel agents
- Plan dry-run in sandbox branches
- Cross-session plan persistence

### Phase 6: System Agents
- Regression Agent (auto-trigger on skill/prompt change)
- Audit Agent (periodic decision verification)
- Tuning Agent (prompt optimization from quality signals)
- Eval Agent (training data validation)

### Phase 7: Data Intelligence
- Training data pipeline with versioned snapshots and lineage
- Event lineage graph with contamination detection
- Cost-aware execution with historical prediction

### Phase 8: Platform Scale
- Control Plane / Data Plane separation
- Multi-tenant agent instances (MatrixOne account-level isolation)
- Skill/prompt marketplace with branch-based trial
- Snapshot-scoped permissions
- Enterprise deployment (RBAC, row-level security, monitoring)

---

## In Short

We don't compete on "smarter LLM" or "more tools." We compete on **trust infrastructure for AI agents**:

- Every decision is **auditable** — reconstruct what the agent saw, not just what it did
- Every change is **testable** — regression gates before deployment, not after complaints
- Every data dependency is **versioned** — from knowledge base to training data to prompts

This is an **Agent Operating System**, not a framework:
- **Platform Capabilities** (kernel): Event Bus, Sandbox, Time Travel, Streaming, Planning — APIs that all agents inherit
- **System Agents** (daemons): Regression, Audit, Tuning — maintain platform health automatically
- **User Agents** (apps): Code Review, CI Diagnosis, Data Analysis — user-facing, zero platform code to add

The tight coupling with MatrixOne is the strategic moat. Time-travel, zero-copy branching, HTAP, and causal event chains are architecturally impossible to retrofit onto traditional database stacks. The platform turns these data capabilities into agent capabilities that no other platform can offer.

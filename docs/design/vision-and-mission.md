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

---

## Innovation Layer

These capabilities go beyond standard agent frameworks. Each addresses a real production need:

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

### Phase 4: Real-Time Experience
- Streaming output (AG-UI protocol, structured event stream over SSE/WebSocket/stdout)
- User intervention mid-execution (cancel, redirect, approve/reject gates)
- Streaming audit trail (every streamed chunk is a persisted, replayable event)

### Phase 5: Autonomous Agents
- Autonomous planning (Plan-Act-Observe-Reflect loop with hierarchical decomposition)
- Plan versioning and cross-session persistence
- Multi-agent collaboration (event blackboard coordination, delegation-as-skill)
- Parallel fan-out, pipeline, and adversarial review patterns
- Plan dry-run in sandbox branches

### Phase 6: Data Intelligence
- Training data pipeline with versioned snapshots and lineage
- Event lineage graph with contamination detection
- Cost-aware execution with historical prediction

### Phase 7: Platform Scale
- Multi-tenant agent instances (MatrixOne account-level isolation)
- Skill/prompt marketplace with branch-based trial
- Snapshot-scoped permissions
- Visual workflow editor (TODO — design pending)
- Enterprise deployment (RBAC, row-level security, monitoring)

---

## In Short

We don't compete on "smarter LLM" or "more tools." We compete on **trust infrastructure for AI agents**:

- Every decision is **auditable** — reconstruct what the agent saw, not just what it did
- Every change is **testable** — regression gates before deployment, not after complaints
- Every data dependency is **versioned** — from knowledge base to training data to prompts

This requires data capabilities (time-travel, zero-copy branching, HTAP, causal event chains) that are native to MatrixOne and architecturally impossible to retrofit onto traditional database stacks. The platform we build turns these data capabilities into agent capabilities that solve real production adoption blockers.

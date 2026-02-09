# Deployment Architecture Proposal

**Project**: mo-dev-agent  
**Date**: 2026-02-10  
**Status**: Proposal for Review  
**Alignment**: Based on [vision-and-mission.md](./vision-and-mission.md) and [context-memory-session-and-tables.md](./context-memory-session-and-tables.md)

---

## Executive Summary

This proposal defines the deployment architecture for mo-dev-agent, aligning with the core design principle: **event-centric memory system** that treats every interaction as traceable, analyzable, and trainable data assets. The architecture prioritizes:

1. **Event-first design**: `conversation_events` as the central data structure
2. **Reproducibility**: "Ten years from now we can still precisely reproduce today's decision"
3. **Data asset evolution**: Every interaction generates high-quality training data
4. **MatrixOne integration**: Single persistence layer with Git for Data support

---

## Core Architecture Principles

### 1. Event-Centric vs Agent-Centric

**Design Philosophy Shift**:

```
❌ Traditional Agent Framework:
   Agent Roles → Skills → Memory → Context

✅ mo-dev-agent (Event-Centric):
   Events → Context Assembly → Execution → New Events → Training Loop
```

**Key Difference**:
- **Events are first-class citizens**: All state flows through `conversation_events`
- **Agents are consumers**: Business logic that uses event/context/memory capabilities
- **Data is the asset**: Every interaction is versioned, traceable, and exportable

### 2. Three-Layer Model (Memory–Prompt–Context)

| Layer | Role | Storage | Evolution |
|-------|------|---------|-----------|
| **Memory** | Persistent knowledge | conversation_events + vector store refs | Short/medium/long-term hierarchy |
| **Prompt** | Versioned behavior | prompt_templates (versioned) | A/B testing, Git for Data |
| **Context** | Single inference input | context_snapshot (per call) | Token Budget Manager |

### 3. Core Capabilities ("Operating System Level")

1. **Conversation Replay ("对话时光机")**: Reproduce any past decision via `causal_chain_id`
2. **Time-Point Sandbox ("平行宇宙实验台")**: Test new prompts/skills on historical data with zero production impact
3. **Continuous Evolution**: Feedback → Evaluation → Training → Improved Models

---

## Project Structure

```
mo-dev-agent/                  # Project root
├── 📁 infra/                  # Infrastructure layer
│   ├── docker-compose.yml     # MatrixOne + Chroma + Redis (local dev)
│   ├── Makefile               # Global commands (setup/dev/test/deploy)
│   ├── scripts/
│   │   ├── init-db.sh         # Execute table creation SQL (§4.5)
│   │   ├── wait-for-db.sh
│   │   └── seed-test-data.sh  # Generate test causal chains
│   └── k8s/                   # Production deployment (Helm charts)
│
├── 📁 core/                   # 【CORE ENGINE】Shared "OS" for all agents
│   ├── __init__.py
│   ├── config/
│   │   ├── settings.py        # MATRIXONE_HOST, LLM_PROVIDER, etc.
│   │   └── constants.py       # Event types, state machine constants
│   │
│   ├── events/                # 【CRITICAL】Event system (design §0.1)
│   │   ├── __init__.py
│   │   ├── event_logger.py    # Write to conversation_events
│   │   ├── event_types.py     # user_query, llm_request, llm_response, tool_call, etc.
│   │   ├── causal_chain.py    # Manage causal_chain_id and parent_event_id
│   │   └── event_reader.py    # Query events by session/user/chain
│   │
│   ├── context/               # Context assembly (design §1)
│   │   ├── __init__.py
│   │   ├── builder.py         # build_context() - stable interface
│   │   ├── token_budget.py    # Token Budget Manager (§1.2)
│   │   ├── snapshot.py        # Generate context_snapshot for reproducibility
│   │   └── skill_filter.py    # Dynamic skill filtering (§1.4)
│   │
│   ├── memory/                # Memory layers (design §2)
│   │   ├── __init__.py
│   │   ├── short_term.py      # Load recent events from conversation_events
│   │   ├── medium_term.py     # Session summaries (session_summaries table)
│   │   ├── long_term.py       # RAG: embedding_ref + external vector store
│   │   └── index_queue.py     # memory_index_queue management
│   │
│   ├── prompt/                # Prompt asset management (design §1.3)
│   │   ├── __init__.py
│   │   ├── template_registry.py  # Load versioned prompt_templates
│   │   ├── version_router.py     # A/B testing, routing logic
│   │   └── validator.py          # Optional pre-injection validation
│   │
│   ├── replay/                # 【KEY INNOVATION】Conversation replay (design §2.6)
│   │   ├── __init__.py
│   │   ├── chain_loader.py    # Load full causal chain
│   │   ├── replayer.py        # Reproduce historical decisions
│   │   └── comparator.py      # Compare original vs replayed outputs
│   │
│   ├── sandbox/               # 【KEY INNOVATION】Time-point sandbox (design §3.5)
│   │   ├── __init__.py
│   │   ├── branch_manager.py  # Git for Data branch/clone at T1
│   │   ├── experiment.py      # Run experiments in isolated branch
│   │   ├── evaluator.py       # Evaluate sandbox results
│   │   └── regression_gate.py # Auto-replay N chains before merge
│   │
│   └── tools/                 # Tool registry (design §6)
│       ├── __init__.py
│       ├── base_tool.py
│       ├── registry.py        # Tool discovery and permission control
│       ├── llm_client.py      # LLM as a tool (not core abstraction)
│       └── sandbox_executor.py # Tool call sandbox (prevent privilege escalation)
│
├── 📁 agents/                 # 【BUSINESS LAYER】Virtual employees
│   ├── __init__.py
│   ├── base_agent.py          # Base: perceive → decide → execute → remember
│   ├── orchestrator.py        # Multi-agent coordination (decentralized handoff)
│   └── examples/              # Example agents (not core)
│       ├── echo_agent.py      # Minimal agent for testing event flow
│       ├── triage_agent.py    # Customer service triage
│       └── query_agent.py     # SQL generation agent
│
├── 📁 analytics/              # 【OPTIMIZATION LOOP】Make system smarter
│   ├── events_analytics/      # Event-level analysis
│   │   ├── quality_scorer.py  # Auto-score events (quality_score)
│   │   ├── chain_analyzer.py  # Analyze causal chains
│   │   └── reporter.py        # Generate quality reports
│   │
│   ├── feedback/              # Feedback collection (design §4.7)
│   │   ├── collector.py       # Collect user_rating
│   │   └── processor.py       # Write to event_evaluations
│   │
│   ├── training/              # Training pipeline (design §4.7)
│   │   ├── dataset_builder.py # Export from conversation_events
│   │   ├── export_pipeline.py # data_export_jobs
│   │   └── fine_tune.py       # LoRA fine-tuning script
│   │
│   └── summary/               # Conversation summarization
│       └── generator.py       # Generate session summaries
│
├── 📁 api/                    # 【ACCESS LAYER】Multi-endpoint service
│   ├── main.py                # FastAPI application
│   ├── endpoints/
│   │   ├── chat.py            # /chat endpoint
│   │   ├── sessions.py        # /sessions/latest (design §3.2)
│   │   ├── agents.py          # /agents management
│   │   ├── replay.py          # /replay endpoint (replay causal chains)
│   │   └── analytics.py       # /analytics queries
│   └── middleware/            # Security guardrails
│       ├── content_filter.py
│       └── rate_limiter.py
│
├── 📁 tests/                  # Full test coverage
│   ├── unit/
│   │   ├── test_token_budget.py      # Token Budget Manager
│   │   ├── test_event_logger.py      # Event persistence
│   │   ├── test_context_builder.py   # Context assembly
│   │   └── test_prompt_versioning.py # Prompt version routing
│   ├── integration/
│   │   ├── test_causal_chain.py      # Verify causal chain integrity
│   │   ├── test_context_snapshot.py  # Snapshot reproducibility
│   │   └── test_agent_memory.py      # Agent + Memory integration
│   └── e2e/
│       ├── test_replay.py            # "Ten years later reproduce decision"
│       ├── test_sandbox.py           # Sandbox experiment workflow
│       └── test_training_loop.py     # Feedback → Export → Training
│
├── 📁 examples/               # Quick start examples
│   ├── simple_chat.py         # 5-line chat startup
│   ├── replay_demo.py         # Demonstrate "conversation time machine"
│   └── sandbox_demo.py        # Demonstrate "parallel universe experiment"
│
├── .env.example
├── requirements.txt
├── pyproject.toml             # Project metadata (pip install -e .)
└── README.md                  # "5-minute startup guide"
```

---

## Key Design Decisions

### 1. Why `core/events/` is Top-Level

**Rationale**: Events are the **single source of truth** for all system state.

```python
# Every interaction flows through events:
User Query → event_logger.write(event_type='user_query')
           → context/builder.py reads recent events
           → LLM call → event_logger.write(event_type='llm_response')
           → Tool call → event_logger.write(event_type='tool_call')
           → analytics reads events for training
```

**Design Document Reference**: §0.1 "Event-Centric Data Asset"

### 2. Why `core/replay/` and `core/sandbox/` are Critical

**Rationale**: These are **operating-system-level capabilities**, not optional features.

| Capability | Value | Design Doc |
|------------|-------|------------|
| **Replay** | Reproduce any past decision; fault attribution; model A/B testing | §2.6 |
| **Sandbox** | Test new prompts/skills on historical data with zero production risk | §3.5 |

**Use Case Example**:
```
User reports: "Yesterday's answer was wrong"
→ Load causal_chain_id from that timestamp
→ core/replay/replayer.py reconstructs context_snapshot
→ Re-run with same model/params
→ Compare outputs → identify root cause (prompt? memory? model?)
```

### 3. Why LLM is in `core/tools/` (Not Top-Level)

**Rationale**: LLM is **one tool among many**, not the core abstraction.

```
Design Philosophy:
- Core = Events + Context + Memory + Replay
- Tools = Execution layer (LLM, GitHub API, SQL, etc.)
- Agents = Business logic that orchestrates tools
```

**Design Document Reference**: §1 "Context Design" - LLM is invoked after context assembly

### 4. Why `agents/` is Simplified

**Rationale**: Avoid premature complexity in agent roles.

```
❌ Over-engineered:
agents/roles/customer_service/triage_agent.py
agents/roles/customer_service/order_agent.py
agents/roles/data_analyst/query_agent.py
agents/roles/data_analyst/insight_agent.py

✅ Start simple:
agents/base_agent.py          # Core agent logic
agents/orchestrator.py         # Multi-agent coordination
agents/examples/               # Example implementations
```

**Phase 0-1**: Implement `echo_agent.py` to validate event flow  
**Phase 2+**: Add domain-specific agents as needed

---

## Data Flow: End-to-End

```
┌─────────────────────────────────────────────────────────────────┐
│ 1. User Request (HTTP/CLI)                                      │
└────────────────────┬────────────────────────────────────────────┘
                     │
                     ▼
┌─────────────────────────────────────────────────────────────────┐
│ 2. Session Resolution (api/endpoints/sessions.py)               │
│    → GET /sessions/latest?user_id=U123                          │
│    → Load or create session                                     │
└────────────────────┬────────────────────────────────────────────┘
                     │
                     ▼
┌─────────────────────────────────────────────────────────────────┐
│ 3. Event Persistence (core/events/event_logger.py)              │
│    → Write event_type='user_query' to conversation_events       │
│    → Set causal_chain_id = event_id (chain starts here)         │
└────────────────────┬────────────────────────────────────────────┘
                     │
                     ▼
┌─────────────────────────────────────────────────────────────────┐
│ 4. Context Assembly (core/context/builder.py)                   │
│    → Token Budget Manager allocates per-section caps            │
│    → Prompt version routing (A/B test or active_latest)         │
│    → Load recent events (short-term memory)                     │
│    → Optional: RAG retrieval (long-term memory)                 │
│    → Dynamic skill filtering                                    │
│    → Render prompt with sections                                │
└────────────────────┬────────────────────────────────────────────┘
                     │
                     ▼
┌─────────────────────────────────────────────────────────────────┐
│ 5. Snapshot Persistence (core/context/snapshot.py)              │
│    → Write event_type='llm_request' with context_snapshot       │
│    → context_snapshot = {                                       │
│        prompt_template_id, skills_used, history_events,         │
│        retrieved_chunks, section_tokens, routing_reason         │
│      }                                                           │
└────────────────────┬────────────────────────────────────────────┘
                     │
                     ▼
┌─────────────────────────────────────────────────────────────────┐
│ 6. LLM Call (core/tools/llm_client.py)                          │
│    → Resolve token (tokens table, priority order)               │
│    → Call LLM API                                               │
│    → Log to token_usage_log                                     │
└────────────────────┬────────────────────────────────────────────┘
                     │
                     ▼
┌─────────────────────────────────────────────────────────────────┐
│ 7. Response Persistence (core/events/event_logger.py)           │
│    → Write event_type='llm_response' with token_usage           │
│    → If tool_calls: loop (tool_call → tool_result events)       │
└────────────────────┬────────────────────────────────────────────┘
                     │
                     ▼
┌─────────────────────────────────────────────────────────────────┐
│ 8. Post-Chain Hooks (async, non-blocking)                       │
│    → Quality scoring (analytics/events_analytics/)              │
│    → Memory extraction (core/memory/index_queue.py)             │
│    → Prompt signals (analytics/feedback/processor.py)           │
└────────────────────┬────────────────────────────────────────────┘
                     │
                     ▼
              Return response to user
```

**Tables Touched** (typical request, no tool calls):
- Read: `sessions`, `configs`, `prompt_templates`, `skills_registry`, `conversation_events`, `tokens`
- Write: `sessions`, `conversation_events` (3 events: user_query, llm_request, llm_response), `token_usage_log`

---

## Implementation Roadmap

### Phase 0: Foundation (Week 1-2)

**Goal**: Database schema + event system

**Deliverables**:
- [ ] Create tables: `conversation_events`, `sessions`, `prompt_templates`, `skills_registry`, `configs`, `tokens`, `repos`, `token_usage_log`, `memory_index_queue`
- [ ] Implement `core/events/event_logger.py` (write events)
- [ ] Implement `core/events/event_reader.py` (query by session/user/chain)
- [ ] Implement `core/config/settings.py` (load from env)
- [ ] Docker Compose: MatrixOne + Redis
- [ ] `infra/scripts/init-db.sh` (execute CREATE statements)

**Test**: Write and read events; verify `causal_chain_id` integrity

### Phase 1: MVP Core (Week 3-4)

**Goal**: End-to-end conversation flow

**Deliverables**:
- [ ] Implement `core/context/builder.py` (build_context with Token Budget Manager)
- [ ] Implement `core/context/token_budget.py` (allocation algorithm from design §1.2)
- [ ] Implement `core/context/snapshot.py` (generate context_snapshot)
- [ ] Implement `core/prompt/template_registry.py` (load versioned templates)
- [ ] Implement `core/prompt/version_router.py` (active_latest routing)
- [ ] Implement `core/tools/llm_client.py` (OpenAI/Anthropic client)
- [ ] Implement `agents/base_agent.py` + `agents/examples/echo_agent.py`
- [ ] Implement `api/endpoints/chat.py` (POST /chat)
- [ ] Implement `api/endpoints/sessions.py` (GET /sessions/latest)

**Test**: 
- [ ] `tests/unit/test_token_budget.py` (verify allocation algorithm)
- [ ] `tests/unit/test_context_builder.py` (verify truncation)
- [ ] `tests/integration/test_causal_chain.py` (verify chain integrity)
- [ ] `tests/e2e/test_simple_chat.py` (user query → LLM response)

### Phase 2: Observability + Evaluation (Week 5-6)

**Goal**: Metrics and feedback loop

**Deliverables**:
- [ ] Create tables: `event_evaluations`, `training_annotations`, `data_export_jobs`
- [ ] Implement `analytics/events_analytics/quality_scorer.py` (auto-score events)
- [ ] Implement `analytics/feedback/collector.py` (user thumbs up/down)
- [ ] Implement `analytics/feedback/processor.py` (write to event_evaluations)
- [ ] Add Prometheus metrics (context tokens, session count, retrieval latency)
- [ ] Implement `api/endpoints/analytics.py` (query quality scores)

**Test**:
- [ ] `tests/integration/test_feedback_loop.py` (user feedback → event_evaluations → training_eligible)

### Phase 3: Intelligence + Training Loop (Week 7-9)

**Goal**: RAG + training pipeline

**Deliverables**:
- [ ] Implement `core/memory/long_term.py` (RAG with external vector store)
- [ ] Implement `core/memory/index_queue.py` (memory_index_queue management)
- [ ] Implement async worker for vector indexing
- [ ] Implement `analytics/training/dataset_builder.py` (export from conversation_events)
- [ ] Implement `analytics/training/export_pipeline.py` (data_export_jobs)
- [ ] Implement `analytics/summary/generator.py` (session summaries)
- [ ] Create table: `session_summaries`

**Test**:
- [ ] `tests/integration/test_rag.py` (retrieval timeout, fallback)
- [ ] `tests/e2e/test_training_loop.py` (feedback → export → training data)

### Phase 4: Experience + Analytics (Week 10-11)

**Goal**: Production-ready features

**Deliverables**:
- [ ] Implement `core/context/skill_filter.py` (dynamic skill filtering)
- [ ] Implement session lifecycle (idle timeout, max_events enforcement)
- [ ] Implement `core/prompt/version_router.py` A/B testing
- [ ] Create table: `agent_configs` (versioned)
- [ ] Implement pre-aggregated views (user_daily_stats)
- [ ] Implement retention policy (archive old events)

**Test**:
- [ ] `tests/unit/test_prompt_versioning.py` (A/B routing)
- [ ] `tests/integration/test_session_lifecycle.py` (idle, max_events)

### Phase 5: Replay + Sandbox (Week 12-14)

**Goal**: "Operating system level" capabilities

**Deliverables**:
- [ ] Implement `core/replay/chain_loader.py` (load full causal chain)
- [ ] Implement `core/replay/replayer.py` (reproduce historical decisions)
- [ ] Implement `core/replay/comparator.py` (compare outputs)
- [ ] Implement `core/sandbox/branch_manager.py` (Git for Data integration)
- [ ] Implement `core/sandbox/experiment.py` (run experiments in sandbox)
- [ ] Implement `core/sandbox/evaluator.py` (evaluate sandbox results)
- [ ] Implement `core/sandbox/regression_gate.py` (auto-replay before merge)
- [ ] Implement `api/endpoints/replay.py` (POST /replay)

**Test**:
- [ ] `tests/e2e/test_replay.py` ("Ten years later reproduce decision")
- [ ] `tests/e2e/test_sandbox.py` (sandbox experiment workflow)
- [ ] `tests/integration/test_context_snapshot.py` (snapshot reproducibility)

---

## Critical Success Metrics

| Metric | Target | Measurement |
|--------|--------|-------------|
| **Reproducibility Rate** | >99% | Replay matches original when same config/model |
| **Context Utilization** | <80% typical | Fraction of token budget used (leave headroom) |
| **Training Loop Cycle** | <1 week | Evaluate → Export → Train (when automated) |
| **Orphan Event Rate** | <1% | Events with no parent and not user_query |
| **Sandbox Experiment Coverage** | Review periodically | % of prompt changes tested in sandbox before merge |

---

## Alignment with Design Documents

| Design Doc Section | Implementation |
|--------------------|----------------|
| §0.1 Event-Centric Data Asset | `core/events/` + `conversation_events` table |
| §1 Context Design | `core/context/builder.py` + Token Budget Manager |
| §1.2 Token Budget Manager | `core/context/token_budget.py` |
| §1.3 Context Assembly Flow | `core/context/builder.py` (stable interface) |
| §1.4 Dynamic Skill Filtering | `core/context/skill_filter.py` |
| §1.6 Memory–Prompt–Context | `core/memory/`, `core/prompt/`, `core/context/` |
| §2 Memory Design | `core/memory/short_term.py`, `medium_term.py`, `long_term.py` |
| §2.6 Conversation Replay | `core/replay/` + `causal_chain_id` |
| §3 Session Management | `api/endpoints/sessions.py` + `sessions` table |
| §3.5 Time-Point Sandbox | `core/sandbox/` + Git for Data |
| §4 Table Design | `infra/scripts/init-db.sh` (all tables) |
| §4.7 Evaluation → Training Loop | `analytics/feedback/`, `analytics/training/` |
| §5 Token Management | `tokens` table + resolution priority |
| §6 Observability | Prometheus metrics in `api/main.py` |

---

## Risk Mitigation

| Risk | Mitigation |
|------|------------|
| Context over token limit | Token Budget Manager enforces truncation; log warning |
| Event write hotspot | Index on (session_id, created_at); monitor write rate; app-level sharding plan |
| RAG latency spikes | Retrieval timeout (800ms); degrade gracefully; record metric |
| Causal chain break | Events written in order; monitor orphan events; alert |
| Sandbox vector rebuild cost | Fallback: use current vector DB (not historically exact); document limitation |
| MatrixOne Git for Data not ready | Fallback: table clone + time-point query (`AS OF TIMESTAMP`) |

---

## Open Questions for Review

1. **MatrixOne Git for Data availability**: When will this feature be production-ready? Impacts Phase 5 timeline.
2. **Vector store selection**: Chroma (local dev) vs Pinecone (production)? Snapshot/replay capability required.
3. **Token budget defaults**: Confirm `context_max_tokens=8000` is appropriate for target models (GPT-4, Claude).
4. **Agent complexity**: Should Phase 1 include `orchestrator.py` or defer to Phase 4?
5. **Compliance (desensitization)**: Is `desensitized_content` column needed for MVP, or defer to later phase?

---

## Conclusion

This architecture proposal:

1. **Aligns with design documents**: Every module traces to specific sections in vision-and-mission.md and context-memory-session-and-tables.md
2. **Prioritizes core innovations**: Event-centric design, replay, sandbox, Token Budget Manager
3. **Simplifies premature complexity**: Minimal agent roles in Phase 0-1; expand as needed
4. **Enables data asset evolution**: Every interaction is traceable, analyzable, and trainable
5. **Provides clear roadmap**: 5 phases over 14 weeks, with testable deliverables

**Next Steps**:
1. Review and approve this proposal
2. Begin Phase 0 implementation (database schema + event system)
3. Validate with `echo_agent.py` before expanding agent capabilities

**Approval Required From**:
- [ ] Architecture Lead
- [ ] Data Engineering Lead
- [ ] ML/Training Lead
- [ ] Product Owner

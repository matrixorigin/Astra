# Deployment Architecture Proposal

**Project**: mo-agent-engine  
**Date**: 2026-02-10  
**Status**: Proposal for Review  
**Alignment**: Based on [vision-and-mission.md](./vision-and-mission.md) and [context-memory-session-and-tables.md](./context-memory-session-and-tables.md)

---

## Executive Summary

This proposal defines the deployment architecture for mo-agent-engine, aligning with the core design principle: **event-centric memory system** that treats every interaction as traceable, analyzable, and trainable data assets. The architecture prioritizes:

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

✅ mo-agent-engine (Event-Centric):
   Events → Context Assembly → Execution → New Events → Training Loop
```

**Key Difference**:
- **Events are first-class citizens**: All state flows through `conversation_events`
- **Agents are consumers**: Business logic that uses event/context/memory capabilities
- **Data is the asset**: Every interaction is versioned, traceable, and exportable

### 2. MatrixOne as "System Memory" (Not Just Database)

**Core Philosophy**: "Everything persists in MatrixOne" is not a technology choice — it's the architectural soul.

| Principle | Implementation | Why Critical |
|-----------|----------------|--------------|
| **Single Source of Truth** | All data assets (events/skills/prompts/docs) only in MatrixOne | Avoid dual-write inconsistency (DB + filesystem) |
| **Time-Point Traceability** | All tables enable MatrixOne Git for Data (AS OF queries) | Replay precisely restores "then" skill code/prompt templates |
| **Cache as Performance Only** | Local cache (memory/file) must be disposable and rebuildable | Service restart doesn't affect data integrity |
| **Metadata as Asset** | Skills docs, prompt descriptions, workflow YAML all in DB | Satisfies audit/compliance/knowledge requirements |
| **Deterministic Boundary Control** | Agent Decision = f(versioned_prompt, versioned_skill, versioned_context, versioned_memory, fixed_params); Git for Data controls 4 of 5 inputs | LLM non-determinism constrained to auditable range |

**Design Mantra**:
> "When you say 'skill documentation in MatrixOne', you're not storing data — you're injecting traceable memory into the system."
> 
> "When you say 'cache is disposable', you're not compromising performance — you're defending the single source of truth."

### 3. Three-Layer Model (Memory–Prompt–Context)

| Layer | Role | Storage | Evolution |
|-------|------|---------|-----------|
| **Memory** | Persistent knowledge | conversation_events + vector store refs | Short/medium/long-term hierarchy |
| **Prompt** | Versioned behavior | prompt_templates (versioned) | A/B testing, Git for Data |
| **Context** | Single inference input | context_snapshot (per call) | Token Budget Manager |

### 3. Core Capabilities ("Operating System Level")

1. **Conversation Replay ("对话时光机")**: Reproduce any past decision via `causal_chain_id`
2. **Time-Point Sandbox ("平行宇宙实验台")**: Test new prompts/skills on historical data with zero production impact
3. **Continuous Evolution**: Feedback → Evaluation → Training → Improved Models
4. **Extensible Skills System**: Skills as first-class citizens with versioning, composition, and marketplace support
5. **Hallucination Firewall**: Verify LLM claims against versioned data snapshots before delivery
6. **Cost-Aware Branching**: Predict execution cost from historical data; block or suggest alternatives when budget exceeded
7. **Regression Gate (Sandbox-as-CI)**: Automated quality gates for every skill/prompt change using snapshot-isolated testing
8. **Training Data Pipeline**: Versioned datasets built from high-quality events with full lineage tracking

### 4. Architectural Growth Principles

**Core Minimalism**: Core engine handles only event flow, context assembly, and extension dispatch. All business logic lives in extensions.

**Explicit Extensions**: New practices must go through:
- `extensions/` directory placement
- `ExtensionManager` registration
- Event hooks or config-driven integration

**Traceable Evolution**: Every extension has independent versioning + changelog; database records extension usage; answer "Why was this skill deprecated on 2026-03-01?"

**Mechanized Deprecation**: Avoid "technical debt snowball" with explicit deprecation policies and migration guides.

### 5. Cache Strategy: Performance Optimization, Not Data Source

**Design Principle**: Cache must be **disposable and rebuildable** from MatrixOne at any time.

| Scenario | Cache Usage | Rationale |
|----------|-------------|-----------|
| **Online Service** | ✅ LRU cache for hot skills/prompts | Reduce DB load (>95% hit rate target) |
| **Replay** | ❌ Force bypass cache | Precise historical restoration; cache causes distortion |
| **Sandbox Experiment** | ❌ Query MatrixOne AS OF | Experiments need isolated historical state |
| **Service Restart** | ✅ Cache auto-rebuilds | No data loss risk (all data in MatrixOne) |
| **Hallucination Check** | ❌ Force use snapshot queries | Verification must see exact data state LLM saw; cache causes false results |
| **Regression Gate** | ❌ Query snapshot directly | Gate must test against consistent historical state |
| **Training Pipeline** | ❌ Query snapshot directly | Dataset must be reproducible from snapshot |

**Implementation Guideline**:
- Cache invalidation on write: When skill/prompt updated, clear cache entry
- Replay queries use `as_of_timestamp` or `as_of_commit` parameters to bypass cache
- Cache layer is **transparent**: Application logic never depends on cache existence

---

## Project Structure

### Innovation Layer Architecture

```
┌─────────────────────────────────────────────────┐
│  Innovation Layer                               │
│  ├─ Hallucination Firewall (snapshot-consistent)  │
│  ├─ Cost-Aware Branching (predictive budget)      │
│  ├─ Regression Gate (sandbox-as-CI)               │
│  ├─ Prompt Evolution Pipeline (branch-based)      │
│  ├─ Training Data Pipeline (versioned datasets)   │
│  └─ Event Lineage Tracker (contamination detect)  │
└─────────────────────────────────────────────────┘
```

### Directory Structure

```
mo-agent-engine/                  # Project root
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
│   ├── skills/                # 【KEY INNOVATION】Skills as first-class citizens
│   │   ├── __init__.py
│   │   ├── skill.py           # Skill base class (input/output/version/safety)
│   │   ├── registry.py        # Load from skills_registry table
│   │   ├── composer.py        # Skill composition (chain/parallel/conditional)
│   │   ├── filter.py          # Enhanced dynamic filtering (LLM-assisted)
│   │   └── validator.py       # Pre-call validation (permissions/params)
│   │
│   ├── extensions/            # Extension management system
│   │   ├── __init__.py
│   │   ├── manager.py         # ExtensionManager (load/register/hooks)
│   │   └── base.py            # Extension base class
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
├── 📁 extensions/             # 【EXTENSIBILITY】All extension practices
│   ├── README.md              # Extension integration guide
│   ├── registry.json          # Extension registry (metadata)
│   ├── TEMPLATE.md            # New extension template
│   │
│   ├── skills/                # Community/custom skills
│   │   ├── github_issue_skill/
│   │   │   ├── skill.yaml     # Metadata (depends on: [github_api, llm])
│   │   │   ├── implementation.py
│   │   │   └── test_skill.py
│   │   └── sql_query_skill/
│   │       ├── skill.yaml
│   │       └── implementation.py
│   │
│   ├── workflows/             # Pre-built workflows (skill compositions)
│   │   └── customer_refund_flow.yaml  # triage → order → payment
│   │
│   ├── analytics/             # New analytics practices
│   │   └── skill_effectiveness.py  # Skill call success rate analysis
│   │
│   └── guardrails/            # New safety practices
│       └── pii_detector.py    # PII detection guardrail
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

## Skills System & Extension Architecture

### Why Skills Need First-Class Status

**Current Design's Implicit Skills**:

| Design Element | Implicit Skills Capability | Limitation |
|----------------|---------------------------|------------|
| `core/tools/` | Tools as skills (LLM/GitHub API) | Only low-level execution units, lacks business semantics |
| `skills_registry` table (design §4) | Skill metadata storage | No runtime skill composition/filtering logic |
| `core/context/skill_filter.py` | Dynamic skill filtering | Skills not explicitly modeled as first-class citizens |

**Industry Best Practices (2024)**:

| Practice | Representative Solution | Core Value | Integration Point |
|----------|------------------------|------------|-------------------|
| **Skills as Code** | Semantic Kernel | Skills = versionable code units | `core/skills/` + Git management |
| **Skill Composition** | LangChain LCEL | Chain/parallel/conditional composition | `core/skills/composer.py` |
| **Skill Marketplace** | Dify Plugin Hub | Community skill reuse | `extensions/skills/` |
| **Skill Versioning** | Microsoft AutoGen | A/B test skill effectiveness | Same level as `prompt_templates` |
| **Skill Safety** | NVIDIA NeMo Guardrails | Skill call guardrails | Enhanced `core/tools/sandbox_executor.py` |
| **Skill Discovery** | IBM Watsonx Orchestrate | LLM auto-recommends skills | Enhanced `core/skills/filter.py` |
| **Multi-Modal Skills** | Google Vertex AI Agents | Text/image/audio skills | Skill metadata extension |

### Skills Architecture

**Enhanced Database Design** (`skills_registry` table):

The `skills_registry` table is the **single source of truth** for all skill definitions, including documentation and code.

**Key Design Principles**:
1. **Documentation as Data Asset**: Full Markdown documentation stored in `documentation` TEXT field
2. **Code as Data Asset**: Small skills store code directly in `skill_code` TEXT field; large skills reference MatrixOne internal repo via `code_ref`
3. **Git for Data Integration**: Every skill write generates `git_commit_hash` for precise time-point queries
4. **Version Coexistence**: Primary key `(skill_id, version)` allows multiple versions to coexist

**Schema** (conceptual, see design doc for full SQL):

```sql
CREATE TABLE skills_registry (
  skill_id VARCHAR(64) NOT NULL,
  version VARCHAR(20) NOT NULL,          -- Semantic versioning (v1.0.0)
  git_commit_hash VARCHAR(64),           -- MatrixOne Git for Data commit hash
  description TEXT NOT NULL,
  documentation TEXT,                    -- Full Markdown docs (examples/params)
  skill_code TEXT,                       -- Python code (small skills) or NULL
  code_ref VARCHAR(255),                 -- Large codebases: MatrixOne internal repo path
  input_schema JSON,
  output_schema JSON,
  tools_required JSON,                   -- Dependent tool IDs
  safety_rules JSON,                     -- ["no_pii", "max_tokens=500"]
  tags JSON,                             -- ["customer_service", "data_query"]
  status VARCHAR(20) DEFAULT 'active',   -- active/deprecated/experimental
  created_at TIMESTAMP,
  updated_at TIMESTAMP,
  PRIMARY KEY (skill_id, version)
);
```

**New Table**: `skills_repos` (for large skill codebases)

```sql
CREATE TABLE skills_repos (
  repo_id VARCHAR(64) PRIMARY KEY,
  repo_name VARCHAR(100) NOT NULL,
  repo_path VARCHAR(255) NOT NULL,       -- MatrixOne internal path "mo://skills/..."
  description TEXT,
  git_branch VARCHAR(100) DEFAULT 'main',
  last_sync_at TIMESTAMP,
  created_at TIMESTAMP
);
```

**Value**: Large skill libraries (e.g., entire GitHub issue handling module) stored as MatrixOne internal repos; replay uses `repo_path + git_commit_hash` to restore exact code snapshot.

**Enhanced Event Recording** (`conversation_events` table):

```sql
ALTER TABLE conversation_events 
ADD COLUMN skill_used JSON COMMENT 'Replay key: [{"skill_id", "version", "git_commit_hash"}]';
```

**Example value**:
```json
[{"skill_id": "github_issue_create", "version": "1.2.0", "git_commit_hash": "a1b2c3d"}]
```

**Why Critical**: Replay reads `git_commit_hash` from event to 100% precisely restore skill code at that moment, avoiding "skill updated causing replay distortion".

**Skill Base Class** (`core/skills/skill.py`):

```python
from pydantic import BaseModel
from typing import List, Dict, Optional

class Skill(BaseModel):
    """First-class skill representation"""
    skill_id: str
    version: str
    git_commit_hash: Optional[str]  # For replay traceability
    description: str
    documentation: Optional[str]    # Markdown docs from DB
    skill_code: Optional[str]       # Python code from DB or None
    code_ref: Optional[str]         # MatrixOne repo path for large skills
    input_schema: Dict  # JSON Schema
    output_schema: Dict
    tools: List[str]    # Dependent tool IDs (links to tools_registry)
    tags: List[str]     # Business tags ("customer_service", "data_query")
    safety_rules: List[str]  # Safety rules ("no_pii", "max_tokens=500")
    status: str = "active"  # active/deprecated/experimental
```

**Skill Registry** (`core/skills/registry.py`):

**Design Principle**: MatrixOne is the **single source of truth**; cache is **disposable performance optimization**.

```python
class SkillRegistry:
    def __init__(self, db_client: MatrixOneClient):
        self.db = db_client
        self.cache = LRUCache(maxsize=100)  # Cache only latest active versions
    
    def get_skill(self, skill_id: str, version: str = None, 
                  as_of_timestamp: datetime = None,
                  as_of_commit: str = None) -> Skill:
        """
        Core: All reads must go through MatrixOne; cache only accelerates.
        
        Args:
            skill_id: Skill identifier
            version: Specific version or None for latest
            as_of_timestamp: For replay - query historical state
            as_of_commit: For replay - query by git commit hash
        """
        # Replay scenario: MUST bypass cache for precise historical query
        if as_of_timestamp or as_of_commit:
            return self._query_historical(skill_id, as_of_timestamp, as_of_commit)
        
        # Online service: Try cache (but auto-fallback to DB on miss)
        cache_key = f"{skill_id}:{version or 'latest'}"
        if cache_key in self.cache:
            return self.cache[cache_key]
        
        skill = self._query_from_db(skill_id, version)
        self.cache[cache_key] = skill
        return skill
    
    def _query_historical(self, skill_id: str, 
                         ts: datetime = None, 
                         commit: str = None) -> Skill:
        """
        Key: Leverage MatrixOne AS OF queries for precise time-point restoration.
        
        MatrixOne Git for Data supports:
        - AS OF TIMESTAMP 'YYYY-MM-DD HH:MM:SS'
        - AS OF COMMIT 'commit_hash'
        """
        if commit:
            # Precise commit-based query (from event.skill_used.git_commit_hash)
            sql = f"""
            SELECT * FROM skills_registry 
            AS OF COMMIT '{commit}' 
            WHERE skill_id = '{skill_id}'
            """
        else:
            # Timestamp-based query
            sql = f"""
            SELECT * FROM skills_registry 
            AS OF TIMESTAMP '{ts.isoformat()}' 
            WHERE skill_id = '{skill_id}' 
            ORDER BY version DESC LIMIT 1
            """
        return self.db.query_one(sql)  # Precisely restore historical version
    
    def register_skill(self, skill_def: dict) -> str:
        """
        Write to MatrixOne + trigger Git for Data commit.
        
        Returns:
            git_commit_hash: For recording in conversation_events
        """
        commit_hash = self.db.insert_with_git_commit(
            table="skills_registry",
            data=skill_def,
            message=f"Register skill {skill_def['skill_id']} v{skill_def['version']}"
        )
        # Invalidate cache
        self.cache.pop(f"{skill_def['skill_id']}:latest", None)
        return commit_hash
```

**Design Essence**:
- **Cache is disposable**: Service restart clears cache, but functionality remains intact (all data rebuilds from MatrixOne)
- **Replay forces cache bypass**: `as_of_timestamp` or `as_of_commit` parameters ensure 100% precise historical restoration
- **Write is versioned**: `insert_with_git_commit` wraps MatrixOne Git for Data API

**Skill Composition** (`core/skills/composer.py`):

```python
class SkillComposer:
    """Compose skills into workflows"""
    
    def compose(self, skills: List[Skill], logic: str = "sequential"):
        """
        Compose skills into workflow
        
        Args:
            skills: List of skills to compose
            logic: "sequential" | "parallel" | "conditional"
        
        Returns:
            Workflow object with nodes and edges
        """
        pass
```

**Integration with Context Builder**:

```python
# core/context/builder.py modification
def build_context(session_id: str, current_request: str) -> Context:
    # Load available skills
    skills = skill_registry.get_available_skills(session_context)
    
    # Filter skills (LLM-assisted or rule-based)
    filtered_skills = skill_filter.filter(skills, current_request)
    
    # Inject into context_snapshot for reproducibility
    context_snapshot["skills_used"] = [
        {
            "skill_id": s.skill_id, 
            "version": s.version,
            "git_commit_hash": s.git_commit_hash  # CRITICAL for replay
        } 
        for s in filtered_skills
    ]
    
    return Context(prompt=rendered, skills=filtered_skills)
```

**Replay Integration** (`core/replay/replayer.py`):

**Design Goal**: "Ten years later, reproduce today's decision" = Use `git_commit_hash` from event → MatrixOne AS OF COMMIT → Precisely restore skill code → Sandbox execution → Output identical to original.

```python
def replay_event_chain(causal_chain_id: str):
    """
    Replay full causal chain with precise skill restoration.
    
    Key: Use git_commit_hash from events to restore exact historical skill code.
    """
    events = event_reader.get_chain(causal_chain_id)
    
    for event in events:
        if event.type == "skill_call":
            # Extract historical skill snapshot identifier from event
            skill_ref = event.skill_used[0]  # {"skill_id":..., "git_commit_hash":...}
            
            # CRITICAL: Use git_commit_hash to query MatrixOne historical version
            skill = skill_registry.get_skill(
                skill_id=skill_ref["skill_id"],
                as_of_commit=skill_ref["git_commit_hash"]  # MatrixOne AS OF COMMIT
            )
            
            # Execute historical skill code (in sandbox)
            result = sandbox_executor.run(skill.skill_code, event.input)
            
            # Verify: result should match event.output (byte-level identical)
            assert result == event.output, "Replay divergence detected"
```

**Why This Satisfies Design Requirements**:

| Requirement | Implementation | Verification |
|-------------|----------------|--------------|
| Everything in MatrixOne | ✅ Skills docs/code/events/prompts all in DB | `SELECT documentation FROM skills_registry WHERE skill_id='x'` |
| Precise Replay | ✅ Events record `git_commit_hash` + AS OF COMMIT queries | Replay output byte-level identical to original |
| Version Control | ✅ All tables enable Git for Data | `SHOW VERSIONS FOR TABLE skills_registry` |
| Cache Safety | ✅ Replay forces cache bypass | Service restart doesn't affect replay results |
| Audit Compliance | ✅ All changes tracked (who/when/what) | `SELECT * FROM skills_registry VERSION BETWEEN ...` |
| "Ten Years Later" | ✅ Event+skill+prompt+context full-chain snapshot | Use 2026 event to replay, output identical to 2026 |

### Extension System Architecture

**Extension Manager** (`core/extensions/manager.py`):

```python
class ExtensionManager:
    """Manage all extensions (skills/workflows/analytics/guardrails)"""
    
    def load_extension(self, path: str):
        """
        Dynamically load extension (no service restart)
        
        Supports:
        - .yaml: Skill definitions
        - .py: Plugin implementations
        """
        if path.endswith(".yaml"):
            self._load_skill_from_yaml(path)
        elif path.endswith(".py"):
            self._load_plugin(path)
    
    def register_hook(self, event_type: str, callback: Callable):
        """
        Register event hooks for extensions
        
        Example: When event_type='skill_call', trigger PII detection
        """
        self._hooks[event_type].append(callback)
    
    def trigger_hooks(self, event_type: str, event: Event):
        """Trigger all registered hooks for event type"""
        for callback in self._hooks.get(event_type, []):
            callback(event)
```

**Extension Registry** (`extensions/registry.json`):

```json
{
  "version": "1.0",
  "extensions": [
    {
      "id": "github_issue_skill",
      "type": "skill",
      "version": "1.0.0",
      "path": "extensions/skills/github_issue_skill",
      "dependencies": ["github_api", "llm"],
      "status": "active",
      "created_at": "2026-02-10"
    },
    {
      "id": "pii_detector",
      "type": "guardrail",
      "version": "1.0.0",
      "path": "extensions/guardrails/pii_detector.py",
      "hooks": ["pre_llm_call", "post_tool_call"],
      "status": "active"
    }
  ]
}
```

**Extension Template** (`extensions/TEMPLATE.md`):

```markdown
# New Extension Integration Guide

## 1. Placement
- Skills: `extensions/skills/your_skill/`
- Workflows: `extensions/workflows/your_workflow.yaml`
- Analytics: `extensions/analytics/your_analyzer.py`
- Guardrails: `extensions/guardrails/your_guardrail.py`

## 2. Required Files
- `README.md`: Description, usage, examples
- `implementation.py` or `skill.yaml`: Core logic
- `test_*.py`: Unit tests
- Entry in `extensions/registry.json`

## 3. Registration
```json
{
  "id": "your_extension",
  "type": "skill|workflow|analytics|guardrail",
  "version": "1.0.0",
  "path": "extensions/.../",
  "dependencies": ["tool1", "tool2"],
  "status": "active"
}
```

## 4. Validation Workflow
```bash
make test-extension path=extensions/skills/your_skill
make sandbox-validate extension=your_extension
```

## 5. Deprecation Policy
When deprecating, add to implementation:
```python
@deprecated(since="v1.3", removal="v2.0", alternative="new_skill_v2")
def old_skill(...):
    pass
```
```

### Extension Evolution Workflow

```
┌─────────────────────────────────────────────────────────────────┐
│ 1. Discover New Practice (community/team)                       │
└────────────────────┬────────────────────────────────────────────┘
                     │
                     ▼
┌─────────────────────────────────────────────────────────────────┐
│ 2. Evaluate                                                      │
│    High value → Create Extension PR                             │
│    Experimental → Place in extensions/experimental/             │
└────────────────────┬────────────────────────────────────────────┘
                     │
                     ▼
┌─────────────────────────────────────────────────────────────────┐
│ 3. Automated Testing                                            │
│    → Unit tests (test_*.py)                                     │
│    → Integration tests                                          │
└────────────────────┬────────────────────────────────────────────┘
                     │
                     ▼
┌─────────────────────────────────────────────────────────────────┐
│ 4. Sandbox Validation                                           │
│    → Replay historical chains with new extension                │
│    → Measure quality delta                                      │
│    → Check regression threshold                                 │
└────────────────────┬────────────────────────────────────────────┘
                     │
                     ├─ Pass → Merge to extensions/
                     └─ Fail → Rollback + Archive
                     │
                     ▼
┌─────────────────────────────────────────────────────────────────┐
│ 5. Documentation + Examples                                     │
│    → Update README.md                                           │
│    → Add usage examples                                         │
└────────────────────┬────────────────────────────────────────────┘
                     │
                     ▼
┌─────────────────────────────────────────────────────────────────┐
│ 6. Monitor Effectiveness                                        │
│    → Track: call count, success rate, user satisfaction        │
│    → Data meets bar → Promote to core module                   │
│    → Data poor → Mark deprecated                               │
└─────────────────────────────────────────────────────────────────┘
```

**Key Mechanisms**:

| Mechanism | Purpose | Tool Support |
|-----------|---------|--------------|
| **Extension Registry** | Unified management of all extensions | `extensions/registry.json` |
| **Sandbox Validation** | Zero-risk validation of new practices | Reuse `core/sandbox/` capability |
| **Deprecation Policy** | Graceful retirement of old practices | `@deprecated` decorator + migration guide |
| **Practice Scorecard** | Quantify practice value | Call count / user satisfaction / error rate |

### Skill Effectiveness Closed Loop

**New Table**: `skill_evaluations`

```sql
CREATE TABLE skill_evaluations (
  evaluation_id VARCHAR(64) PRIMARY KEY,
  skill_id VARCHAR(64) NOT NULL,
  event_id VARCHAR(64) NOT NULL,  -- Links to conversation_events
  success BOOLEAN,
  latency_ms INT,
  user_feedback TEXT,
  created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX idx_skill_eval ON skill_evaluations(skill_id, success);
```

**Analytics**: `extensions/analytics/skill_effectiveness.py`
- Generate skill heatmap (success rate by skill)
- Auto-downgrade low-performing skills (integrate with `skill_filter.py`)

### Multi-Modal Skills Support

**Skill Metadata Extension** (`extensions/skills/image_analysis/skill.yaml`):

```yaml
skill_id: image_analysis
version: 1.0.0
description: Analyze images and extract insights
input_types:
  - text
  - image_url
output_types:
  - text
  - bounding_boxes
tools:
  - gpt4v_client
  - claude35_vision
tags:
  - vision
  - multimodal
safety_rules:
  - no_pii_in_images
  - max_image_size=10MB
```

**Tool Enhancement**: `core/tools/multimodal_client.py` (supports GPT-4V/Claude 3.5)

### Defense Against "Practice Sprawl"

| Risk | Mitigation Strategy |
|------|---------------------|
| **Extension Fragmentation** | Mandatory Extension Registry + tag classification (skills/workflows/guardrails) |
| **Core Bloat** | Core engine only provides "extension points"; all practices default to `extensions/` |
| **Version Conflicts** | Each extension declares dependency versions (`requires: core>=1.2.0`) |
| **Security Vulnerabilities** | Extension sandbox execution (resource isolation + permission whitelist) |
| **Documentation Gaps** | PR template enforces: README.md + examples + test cases |

---

## Global Data Flow: MatrixOne as System Memory

**Design Philosophy**: MatrixOne is not just a "database" — it's the **system memory substrate**. All state flows through it; cache is transparent performance optimization.

```
┌─────────────────────────────────────────────────────────────────┐
│                    Skill Developer                               │
└────────────────────┬────────────────────────────────────────────┘
                     │ POST /skills (with documentation + code)
                     ▼
┌─────────────────────────────────────────────────────────────────┐
│              API: skills_registry endpoint                       │
└────────────────────┬────────────────────────────────────────────┘
                     │
                     ▼
┌─────────────────────────────────────────────────────────────────┐
│  MatrixOne: Write to skills_registry + Git for Data commit      │
│  → Generates git_commit_hash                                    │
└────────────────────┬────────────────────────────────────────────┘
                     │
                     ├─→ Return git_commit_hash to API
                     │
                     └─→ (Optional) Update LRU Cache
                          [Dotted line: Cache is disposable]
                     
┌─────────────────────────────────────────────────────────────────┐
│                    User Conversation                             │
└────────────────────┬────────────────────────────────────────────┘
                     │ POST /chat
                     ▼
┌─────────────────────────────────────────────────────────────────┐
│              Context Builder (core/context/)                     │
│  → Load skills from SkillRegistry                               │
│  → Filter skills for current task                               │
└────────────────────┬────────────────────────────────────────────┘
                     │
                     ▼
┌─────────────────────────────────────────────────────────────────┐
│              Event Logger (core/events/)                         │
│  → Write conversation_events with:                              │
│    skill_used = [{"skill_id", "version", "git_commit_hash"}]    │
└────────────────────┬────────────────────────────────────────────┘
                     │
                     ▼
                MatrixOne (Single Source of Truth)
                     │
                     ▼
┌─────────────────────────────────────────────────────────────────┐
│                    Replay Request                                │
│  POST /replay?causal_chain_id=xyz                               │
└────────────────────┬────────────────────────────────────────────┘
                     │
                     ▼
┌─────────────────────────────────────────────────────────────────┐
│              Replayer (core/replay/)                             │
│  1. Read events by causal_chain_id                              │
│  2. Extract git_commit_hash from event.skill_used               │
└────────────────────┬────────────────────────────────────────────┘
                     │
                     ▼
┌─────────────────────────────────────────────────────────────────┐
│  MatrixOne: AS OF COMMIT query                                  │
│  SELECT * FROM skills_registry                                  │
│  AS OF COMMIT 'git_commit_hash'                                 │
│  → Precisely restore historical skill code                      │
└────────────────────┬────────────────────────────────────────────┘
                     │ [BYPASS CACHE - Critical for replay]
                     ▼
┌─────────────────────────────────────────────────────────────────┐
│              Sandbox Executor                                    │
│  → Execute historical skill code                                │
│  → Output byte-level identical to original                      │
└─────────────────────────────────────────────────────────────────┘
```

**Key Design Points**:
1. **Solid lines**: Data flow through MatrixOne (mandatory)
2. **Dotted lines**: Cache layer (optional, disposable)
3. **Replay path**: Completely bypasses cache, queries MatrixOne AS OF COMMIT
4. **Single source of truth**: All persistent state (skills, events, prompts) only in MatrixOne

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
│    → Skill filtering (core/skills/filter.py)                    │
│    → Load recent events (short-term memory)                     │
│    → Optional: RAG retrieval (long-term memory)                 │
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

**Goal**: Database schema + event system + MatrixOne Git for Data

**Deliverables**:
- [ ] Create tables: `conversation_events`, `sessions`, `prompt_templates`, `skills_registry`, `skills_repos`, `configs`, `tokens`, `repos`, `token_usage_log`, `memory_index_queue`
- [ ] **Enable MatrixOne Git for Data** on all core tables:
  - [ ] `skills_registry` (for skill code/docs versioning)
  - [ ] `prompt_templates` (for prompt versioning)
  - [ ] `conversation_events` (for event history)
  - [ ] `agent_configs` (for agent config versioning)
- [ ] Add `git_commit_hash` column to `skills_registry`
- [ ] Add `skill_used` JSON column to `conversation_events`
- [ ] Implement `core/events/event_logger.py` (write events)
- [ ] Implement `core/events/event_reader.py` (query by session/user/chain)
- [ ] Implement `core/config/settings.py` (load from env)
- [ ] Docker Compose: MatrixOne + Redis
- [ ] `infra/scripts/init-db.sh` (execute CREATE statements + enable Git for Data)

**Test**: 
- [ ] Write and read events; verify `causal_chain_id` integrity
- [ ] Verify MatrixOne AS OF TIMESTAMP queries work on test data
- [ ] Verify Git for Data commit generation on skill registration

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

### Phase 1.5: Skills System (Week 5)

**Goal**: Elevate skills to first-class citizens with MatrixOne-first design

**Deliverables**:
- [ ] Implement `core/skills/skill.py` (Skill base class with `git_commit_hash`)
- [ ] Implement `core/skills/registry.py` with:
  - [ ] `get_skill(as_of_timestamp=...)` for time-point queries
  - [ ] `get_skill(as_of_commit=...)` for commit-based queries
  - [ ] `register_skill()` with Git for Data commit generation
  - [ ] LRU cache with explicit invalidation on writes
- [ ] Implement `core/skills/composer.py` (skill composition: chain/parallel/conditional)
- [ ] Implement `core/skills/filter.py` (enhanced dynamic filtering with LLM-assisted recommendation)
- [ ] Implement `core/skills/validator.py` (pre-call validation: permissions/params)
- [ ] Implement `core/extensions/manager.py` (ExtensionManager: load/register/hooks)
- [ ] Implement `core/extensions/base.py` (Extension base class)
- [ ] Create `extensions/` directory structure
- [ ] Create `extensions/registry.json` (extension registry)
- [ ] Create `extensions/TEMPLATE.md` (new extension template)
- [ ] Modify `core/context/builder.py` to inject `skills_used` (with `git_commit_hash`) into `context_snapshot`

**Test**:
- [ ] `tests/unit/test_skill_composer.py` (verify skill composition logic)
- [ ] `tests/unit/test_skill_filter.py` (verify filtering with/without LLM)
- [ ] `tests/unit/test_skill_registry_cache.py` (verify cache bypass on replay queries)
- [ ] `tests/integration/test_extension_loading.py` (load extension from yaml/py)
- [ ] `tests/integration/test_skill_versioning.py` (verify AS OF COMMIT queries return correct historical skill)
- [ ] Implement `core/extensions/manager.py` (ExtensionManager: load/register/hooks)
- [ ] Implement `core/extensions/base.py` (Extension base class)
- [ ] Create `extensions/` directory structure
- [ ] Create `extensions/registry.json` (extension registry)
- [ ] Create `extensions/TEMPLATE.md` (new extension template)
- [ ] Modify `core/context/builder.py` to inject `skills_used` into `context_snapshot`

**Test**:
- [ ] `tests/unit/test_skill_composer.py` (verify skill composition logic)
- [ ] `tests/unit/test_skill_filter.py` (verify filtering with/without LLM)
- [ ] `tests/integration/test_extension_loading.py` (load extension from yaml/py)

### Phase 2: Observability + Evaluation (Week 6-7)

**Goal**: Metrics and feedback loop

**Deliverables**:
- [ ] Create tables: `event_evaluations`, `training_annotations`, `data_export_jobs`, `skill_evaluations`
- [ ] Implement `analytics/events_analytics/quality_scorer.py` (auto-score events)
- [ ] Implement `analytics/feedback/collector.py` (user thumbs up/down)
- [ ] Implement `analytics/feedback/processor.py` (write to event_evaluations)
- [ ] Add Prometheus metrics (context tokens, session count, retrieval latency, skill success rate)
- [ ] Implement `api/endpoints/analytics.py` (query quality scores)
- [ ] Implement `extensions/analytics/skill_effectiveness.py` (skill heatmap)

**Test**:
- [ ] `tests/integration/test_feedback_loop.py` (user feedback → event_evaluations → training_eligible)
- [ ] `tests/integration/test_skill_evaluation.py` (skill call → skill_evaluations)

### Phase 3: Intelligence + Training Loop (Week 8-10)

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

### Phase 4: Experience + Analytics (Week 11-12)

**Goal**: Production-ready features

**Deliverables**:
- [ ] Implement session lifecycle (idle timeout, max_events enforcement)
- [ ] Implement `core/prompt/version_router.py` A/B testing
- [ ] Create table: `agent_configs` (versioned)
- [ ] Implement pre-aggregated views (user_daily_stats)
- [ ] Implement retention policy (archive old events)
- [ ] Create example extensions:
  - [ ] `extensions/skills/github_issue_skill/` (with skill.yaml + implementation.py)
  - [ ] `extensions/workflows/customer_refund_flow.yaml`
  - [ ] `extensions/guardrails/pii_detector.py`
- [ ] Implement Makefile commands: `make test-extension`, `make sandbox-validate`

**Test**:
- [ ] `tests/unit/test_prompt_versioning.py` (A/B routing)
- [ ] `tests/integration/test_session_lifecycle.py` (idle, max_events)
- [ ] `tests/integration/test_extension_workflow.py` (load → validate → deploy extension)

### Phase 5: Replay + Sandbox (Week 13-15)

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
| **Sandbox Experiment Coverage** | Review periodically | % of prompt/skill changes tested in sandbox before merge |
| **Skill Success Rate** | >90% | Successful skill calls / total skill calls |
| **Extension Adoption Rate** | Track monthly | New extensions added vs deprecated |

---

## Alignment with Design Documents

| Design Doc Section | Implementation |
|--------------------|----------------|
| §0.1 Event-Centric Data Asset | `core/events/` + `conversation_events` table |
| §1 Context Design | `core/context/builder.py` + Token Budget Manager |
| §1.2 Token Budget Manager | `core/context/token_budget.py` |
| §1.3 Context Assembly Flow | `core/context/builder.py` (stable interface) |
| §1.4 Dynamic Skill Filtering | `core/skills/filter.py` (enhanced) |
| §1.6 Memory–Prompt–Context | `core/memory/`, `core/prompt/`, `core/context/` |
| §2 Memory Design | `core/memory/short_term.py`, `medium_term.py`, `long_term.py` |
| §2.6 Conversation Replay | `core/replay/` + `causal_chain_id` |
| §3 Session Management | `api/endpoints/sessions.py` + `sessions` table |
| §3.5 Time-Point Sandbox | `core/sandbox/` + Git for Data |
| §4 Table Design | `infra/scripts/init-db.sh` (all tables including `skill_evaluations`) |
| §4.7 Evaluation → Training Loop | `analytics/feedback/`, `analytics/training/` |
| §5 Token Management | `tokens` table + resolution priority |
| §6 Observability | Prometheus metrics in `api/main.py` |
| **Skills as First-Class** | `core/skills/` + `skills_registry` table + `extensions/` |
| **Extension System** | `core/extensions/manager.py` + `extensions/registry.json` |
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
| **Extension fragmentation** | Mandatory Extension Registry + tag classification; enforce PR template |
| **Core bloat from extensions** | Core engine only provides extension points; all practices default to `extensions/` |
| **Skill version conflicts** | Each skill declares dependency versions (`requires: core>=1.2.0`) |
| **Extension security vulnerabilities** | Extension sandbox execution (resource isolation + permission whitelist) |

---

## Open Questions for Review

1. **MatrixOne Git for Data availability**: When will this feature be production-ready? Impacts Phase 5 timeline.
2. **Vector store selection**: Chroma (local dev) vs Pinecone (production)? Snapshot/replay capability required.
3. **Token budget defaults**: Confirm `context_max_tokens=8000` is appropriate for target models (GPT-4, Claude).
4. **Agent complexity**: Should Phase 1 include `orchestrator.py` or defer to Phase 4?
5. **Compliance (desensitization)**: Is `desensitized_content` column needed for MVP, or defer to later phase?
6. **Extension governance**: Who approves new extensions? What's the review process for community contributions?
7. **Skill marketplace**: Should we plan for external skill marketplace integration (e.g., Dify Plugin Hub)?

---

## Conclusion

This architecture proposal:

1. **Aligns with design documents**: Every module traces to specific sections in vision-and-mission.md and context-memory-session-and-tables.md
2. **Prioritizes core innovations**: 
   - Event-centric design
   - **MatrixOne as "System Memory"** (not just database)
   - Conversation replay with byte-level precision
   - Time-point sandbox
   - Token Budget Manager
   - **Skills as first-class data assets** (with Git for Data versioning)
3. **Simplifies premature complexity**: Minimal agent roles in Phase 0-1; expand as needed
4. **Enables data asset evolution**: Every interaction is traceable, analyzable, and trainable
5. **Provides clear roadmap**: 5 phases over 15 weeks, with testable deliverables
6. **Ensures architectural growth**: Extension system enables continuous absorption of industry best practices without core refactoring

**Core Architectural Philosophies**:

> **"MatrixOne is not a database — it's the system memory substrate."**
> 
> When you say "skill documentation in MatrixOne", you're not storing data — you're injecting traceable memory into the system.

> **"Cache is disposable; data truth is singular."**
> 
> When you say "cache is disposable", you're not compromising performance — you're defending the single source of truth.

> **"Ten years later, reproduce today's decision."**
> 
> Event + `git_commit_hash` → MatrixOne AS OF COMMIT → Precise skill restoration → Sandbox execution → Output byte-level identical to original.

> **"When tomorrow brings a disruptive practice, integrate it in 1 day, not refactor the entire system."**
> 
> Extension system transforms mo-agent-engine from "excellent system" into **"industry practice incubator"**.

**Design Verification Matrix**:

| Design Requirement | Implementation | Verification Method |
|--------------------|----------------|---------------------|
| Everything in MatrixOne | ✅ Skills docs/code/events/prompts all in DB | `SELECT documentation FROM skills_registry WHERE skill_id='x'` |
| Precise Replay | ✅ Events record `git_commit_hash` + AS OF COMMIT queries | Replay output byte-level identical to original |
| Version Control | ✅ All tables enable Git for Data | `SHOW VERSIONS FOR TABLE skills_registry` |
| Cache Safety | ✅ Replay forces cache bypass | Service restart doesn't affect replay results |
| Audit Compliance | ✅ All changes tracked (who/when/what) | `SELECT * FROM skills_registry VERSION BETWEEN ...` |
| "Ten Years Later" | ✅ Event+skill+prompt+context full-chain snapshot | Use 2026 event to replay, output identical to 2026 |

**Next Steps**:
1. Review and approve this proposal
2. Begin Phase 0 implementation:
   - Database schema creation
   - **Enable MatrixOne Git for Data** on core tables
   - Event system with `causal_chain_id` and `skill_used` tracking
3. Validate with `echo_agent.py` before expanding agent capabilities
4. Create first example skill in Phase 1.5 to validate:
   - Skill registration with Git commit generation
   - Event recording with `git_commit_hash`
   - Replay with AS OF COMMIT query

**Approval Required From**:
- [ ] Architecture Lead (verify MatrixOne-first design)
- [ ] Data Engineering Lead (verify Git for Data integration)
- [ ] ML/Training Lead (verify training pipeline design)
- [ ] Product Owner (verify roadmap alignment)
- [ ] DevOps/Platform Lead (verify extension security + cache strategy)
